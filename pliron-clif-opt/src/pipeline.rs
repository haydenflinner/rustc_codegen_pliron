//! The per-function CLIF optimization pipeline as data.
//!
//! `DEFAULT` lists the steps in their baked order; `PLIRON_PIPELINE=a,b,c`
//! overrides selection and order by step `name` (bisect still applies —
//! env gates are bypassed since listing a step is the explicit request).
//! `Gate::Always` steps are correctness lowerings, not optimizations:
//! they run even under a custom pipeline, appended in table order.

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, FuncRef, Function, Inst, Value, ValueDef};
use cranelift_codegen::isa::{TargetFrontendConfig, TargetIsa};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use cranelift_module::FuncId;
use pliron::basic_block::BasicBlock;
use pliron::context::Ptr;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::config::{bisect, bisect_count, config, env_set, reads_begin, reads_take, trace};
use crate::*;

/// Per-CGU pass categories, mirroring the `State::*` flags in the
/// rustc-coupled crate (set from `PLIRON_*` env at CGU start, forced off
/// at `-O0`). Steps gate on these to preserve that behavior.
#[derive(Default, Clone, Copy)]
pub struct FnFlags {
    pub jumpthread: bool,
    pub unreach: bool,
    pub peep: bool,
    pub tailmerge: bool,
    pub taildup: bool,
    pub tailrec: bool,
    pub loadfwd: bool,
    pub slot_dse: bool,
    pub dse: bool,
    pub indvars: bool,
    pub licm: bool,
    pub loopidiom: bool,
    pub loopvec: bool,
    pub bcheck: bool,
    pub slp: bool,
}

#[derive(Clone, Copy)]
pub enum FlagId {
    Jumpthread,
    Unreach,
    Peep,
    Tailmerge,
    Taildup,
    Tailrec,
    Loadfwd,
    SlotDse,
    Dse,
    Indvars,
    Licm,
    Loopidiom,
    Loopvec,
    Bcheck,
    Slp,
}

impl FnFlags {
    fn get(&self, id: FlagId) -> bool {
        match id {
            FlagId::Jumpthread => self.jumpthread,
            FlagId::Unreach => self.unreach,
            FlagId::Peep => self.peep,
            FlagId::Tailmerge => self.tailmerge,
            FlagId::Taildup => self.taildup,
            FlagId::Tailrec => self.tailrec,
            FlagId::Loadfwd => self.loadfwd,
            FlagId::SlotDse => self.slot_dse,
            FlagId::Dse => self.dse,
            FlagId::Indvars => self.indvars,
            FlagId::Licm => self.licm,
            FlagId::Loopidiom => self.loopidiom,
            FlagId::Loopvec => self.loopvec,
            FlagId::Bcheck => self.bcheck,
            FlagId::Slp => self.slp,
        }
    }
}

/// Counts accumulated across functions for the `PLIRON_STATS` summary.
#[derive(Default, Clone)]
pub struct Stats {
    pub threaded: usize,
    pub dead_params: usize,
    pub dominated_folds: usize,
    pub unreached: usize,
    pub peeped: usize,
    pub merged: usize,
    pub dupd: usize,
    pub forwarded: usize,
    pub slot_dse: usize,
    /// Wall time per step name, summed over functions; printed under
    /// `PLIRON_TIMING`.
    pub timing: FxHashMap<&'static str, std::time::Duration>,
}

impl Stats {
    pub fn merge(&mut self, o: &Self) {
        self.threaded += o.threaded;
        self.dead_params += o.dead_params;
        self.dominated_folds += o.dominated_folds;
        self.unreached += o.unreached;
        self.peeped += o.peeped;
        self.merged += o.merged;
        self.dupd += o.dupd;
        self.forwarded += o.forwarded;
        self.slot_dse += o.slot_dse;
        for (n, d) in &o.timing {
            *self.timing.entry(n).or_default() += *d;
        }
    }
}

/// Everything a step needs beyond `&mut Function`.
pub struct Pipe<'a> {
    pub clctx: &'a mut Context,
    pub isa: &'a dyn TargetIsa,
    pub fname: &'a str,
    pub func_id: FuncId,
    pub nonnull: &'a mut FxHashSet<Value>,
    pub derived: &'a mut FxHashMap<Value, Value>,
    pub frozen: &'a mut FxHashMap<Value, u64>,
    pub noalias: &'a mut FxHashSet<Value>,
    pub deref: &'a mut FxHashMap<Value, (u64, bool)>,
    pub nowrite: &'a FxHashMap<FuncRef, bool>,
    pub call_frozen: &'a FxHashMap<FuncRef, u64>,
    pub scoped: &'a FxHashMap<Inst, Ptr<BasicBlock>>,
    pub invariant: &'a FxHashSet<Inst>,
    pub operand_frozen: &'a mut FxHashMap<Value, Vec<(u64, Ptr<BasicBlock>)>>,
    pub block_region: &'a FxHashMap<Block, Ptr<BasicBlock>>,
    /// PGO: CLIF block -> executed count (empty without PLIRON_PGO_USE).
    /// Loop passes scale budgets off it; `block_count` is the read path.
    pub prof: &'a FxHashMap<Block, u64>,
    pub region_parent: &'a FxHashMap<Ptr<BasicBlock>, Ptr<BasicBlock>>,
    pub lifetimes: &'a mut Vec<(Value, Option<Inst>, Block, bool)>,
    pub nocap: &'a FxHashSet<(Inst, u32)>,
    pub tcfg: TargetFrontendConfig,
    pub simd: bool,
    pub x64: bool,
    pub cmp64: bool,
    pub has_eh: bool,
    pub flags: &'a FnFlags,
    /// `PLIRON_CLIF=<substr>` matched this function's name.
    pub dump: bool,
    /// `PLIRON_VERIFY=full` (or the verify-failure retry pass).
    pub verify_full: bool,
    pub stats: &'a mut Stats,
    /// Every `(d, w)` pair `carry_one` applied, in order — the
    /// stepcache's fact-delta record (replaying the pairs reproduces
    /// the inserts and lifetime rewrites exactly).
    pub carry_log: Vec<(Value, Value)>,
    /// Step currently executing, for debug attribution prints.
    pub cur_step: &'static str,
    /// Self-profile hook wired by `src/` to rustc's `sess.prof`: calling
    /// it starts a `generic_activity` event for the step; the returned
    /// guard records the interval when it drops. `None` in tests.
    /// Type-erased so this crate stays free of `rustc_private`.
    pub prof_hook: Option<&'a (dyn Fn(&'static str) -> Box<dyn Send + 'a> + 'a)>,
}

impl Pipe<'_> {
    pub(crate) fn dump(&self, tag: &str) {
        if self.dump {
            eprintln!(
                "==== clif {} after {tag} ====\n{}",
                self.fname,
                self.clctx.func.display()
            );
        }
    }
    fn log(&self, label: &str, k: usize) {
        if k > 0 && env_set("PLIRON_STATS") {
            eprintln!("{label} {k} {}", self.fname);
        }
    }
}

#[derive(Clone, Copy)]
pub enum Gate {
    /// Correctness lowering, not a knob — always runs.
    Always,
    /// `PLIRON_<V>` unset or `!= "0"`.
    Env(&'static str),
    /// `PLIRON_<V> == "1"` — opt-in.
    OptIn(&'static str),
    /// Per-CGU `State::*` category flag.
    Flag(FlagId),
    /// All must hold.
    All(&'static [Gate]),
}

/// Cheap function-shape facts used to skip steps that cannot apply to
/// the current function. One pass over the instructions computes all of
/// them; `Analysis::shape_bits` caches the result per IR version. Each
/// bit is a *necessary* condition for the steps that require it — never
/// sufficient, so the bits may over-report (a false `LOOPS` just runs a
/// pass that finds nothing).
pub mod shape {
    /// At least one CFG edge — a lone block with a self-edge counts:
    /// its edge still carries block-param args `sameargs` can fold.
    pub const MULTI: u32 = 1 << 0;
    /// A cycle exists — in any block linearization, every cycle contains
    /// an edge to a block at an earlier-or-equal position.
    pub const LOOPS: u32 = 1 << 1;
    /// A conditional terminator exists (`brif` or `br_table`) — required
    /// by anything that consumes branch conditions.
    pub const CONDBR: u32 = 1 << 2;
    /// A `br_table` exists specifically — the switch-shape passes.
    pub const SWITCH: u32 = 1 << 3;
    /// Any memory access: load/store, stack, atomic ops.
    pub const MEM: u32 = 1 << 4;
    /// Any call.
    pub const CALL: u32 = 1 << 5;
    /// `slp` seeds: a block with ≥2 notrap scalar stores of the same
    /// element size to one resolved base (every pack path — the
    /// consecutive-offset runs and xpack's `plan_store` — merges only
    /// same-base adjacent stores), or ≥2 same-opcode vectorizable insts
    /// (the xpack op-pair shape).
    pub const SLP: u32 = 1 << 6;
    /// Any integer div/rem — `divconst` rewrites the constant-divisor
    /// cases and loopvec treats the opcodes as side effects.
    pub const DIVREM: u32 = 1 << 7;
}

/// Computes the `shape` bits in one pass over the function.
fn shape_of(func: &Function) -> u32 {
    use cranelift_codegen::ir::Opcode as Op;
    let mut bits = 0u32;
    // Layout-order index per block for the back-edge test.
    let mut idx: FxHashMap<Block, u32> = FxHashMap::default();
    for (i, b) in func.layout.blocks().enumerate() {
        idx.insert(b, i as u32);
    }
    for (i, b) in func.layout.blocks().enumerate() {
        // (base, elem bytes) → store count: a pair in one group is the
        // cheap necessary shape for every store-pack path.
        let mut stores: FxHashMap<(Value, u32), u32> = FxHashMap::default();
        // Same-op float-inst count → xpack op-pair seed. Integer dups are
        // excluded on purpose: two `iadd`s in a block is ambient pointer
        // arithmetic (every address computation), while float dups are
        // the unrolled-math shape xpack actually packs.
        let mut vecops: FxHashMap<Op, u32> = FxHashMap::default();
        for inst in func.layout.block_insts(b) {
            let op = func.dfg.insts[inst].opcode();
            // `stack_addr` reports neither load nor store but marks a
            // slot the memory passes walk (slotprom/slotmerge).
            if op.can_load() || op.can_store() || op == Op::StackAddr {
                bits |= shape::MEM;
            }
            if op.is_call() {
                bits |= shape::CALL;
            }
            match op {
                Op::Brif => bits |= shape::CONDBR,
                Op::BrTable => bits |= shape::CONDBR | shape::SWITCH,
                Op::Sdiv | Op::Udiv | Op::Srem | Op::Urem => bits |= shape::DIVREM,
                Op::Store => {
                    let fl = func.dfg.mem_flags[func.dfg.insts[inst].memflags().unwrap()];
                    let args = func.dfg.inst_args(inst);
                    let val = func.dfg.resolve_aliases(args[0]);
                    let ty = func.dfg.value_type(val);
                    if fl.notrap() && !ty.is_vector() && matches!(ty.bytes(), 1 | 2 | 4 | 8) {
                        let base = crate::slp::base_off(func, args[1]).0;
                        let c = stores.entry((base, ty.bytes())).or_insert(0);
                        *c += 1;
                        if *c >= 2 {
                            bits |= shape::SLP;
                        }
                    }
                }
                _ => {}
            }
            match op {
                Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv | Op::Fmin | Op::Fmax | Op::Fneg
                | Op::Fabs | Op::Sqrt => {
                    let c = vecops.entry(op).or_insert(0);
                    *c += 1;
                    if *c >= 2 {
                        bits |= shape::SLP;
                    }
                }
                _ => {}
            }
            // Vector-typed results (a function loopvec already rewrote)
            // leave scalar leftovers worth packing.
            if func
                .dfg
                .inst_results(inst)
                .first()
                .is_some_and(|&r| func.dfg.value_type(r).is_vector())
            {
                bits |= shape::SLP;
            }
            if op.is_terminator() {
                let pool = &func.dfg.value_lists;
                for bc in func.dfg.insts[inst]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    bits |= shape::MULTI;
                    if idx[&bc.block(pool)] <= i as u32 {
                        bits |= shape::LOOPS;
                    }
                }
            }
        }
    }
    bits
}

impl Gate {
    fn ok(&self, p: &Pipe) -> bool {
        match self {
            Gate::Always => true,
            Gate::Env(v) => config().enabled(v),
            Gate::OptIn(v) => config().is(v, "1"),
            Gate::Flag(id) => p.flags.get(*id),
            Gate::All(gs) => gs.iter().all(|g| g.ok(p)),
        }
    }
}

/// Version-keyed cache of the analyses passes share. Owned by
/// `run_pipeline` (outside `Pipe`) so an `&Analysis` result and
/// `&mut Function` can be borrowed at once.
///
/// Entries are dropped when the pipeline bumps `ver` after a mutating
/// step, and passes that mutate mid-step call `invalidate()` at the same
/// places they previously recomputed. Accessors also check a cheap shape
/// fingerprint — a read after an un-`invalidate`d structural mutation
/// panics instead of silently serving stale data.
#[derive(Default)]
pub struct Analysis {
    ver: u64,
    shape: (usize, usize),
    cfg: Option<ControlFlowGraph>,
    dt: Option<DominatorTree>,
    la: Option<LoopAnalysis>,
    /// Step-cache hash chain: the previous step's output hash is this
    /// step's input hash, so cache hits never re-hash the function.
    /// `fnh` hashes the FnJob input maps once per function. `pk` is
    /// the function's file key; `stepfile` is its per-step cache file,
    /// loaded lazily and written back once if `stepfile_dirty`.
    pub(crate) clif_hash: Option<[u8; 32]>,
    pub(crate) fnh: Option<[u8; 32]>,
    pub(crate) pk: Option<[u8; 32]>,
    pub(crate) stepfile: Option<Vec<crate::stepcache::StepEntry>>,
    pub(crate) stepfile_dirty: bool,
    /// `shape_of` result and the `ver` it was computed at; recomputed
    /// lazily after any mutating step.
    fshape: Option<(u64, u32)>,
    /// Alias pairs already carried onto their targets; facts are inputs
    /// and aliases are permanent, so a pair never needs carrying twice.
    carried: FxHashSet<(Value, Value)>,
}

/// One borrow of the three structural analyses (`&self` tuple).
pub struct CfgDtLa<'a> {
    pub cfg: &'a ControlFlowGraph,
    pub dt: &'a DominatorTree,
    pub la: &'a LoopAnalysis,
}

impl Analysis {
    fn clear(&mut self) {
        self.cfg = None;
        self.dt = None;
        self.la = None;
    }

    /// The pipeline advanced the IR version (a step declared `mutates`).
    pub fn bump(&mut self) {
        self.ver += 1;
        self.clear();
    }

    /// A pass mutated the function mid-step; drop everything computed
    /// against the old shape.
    pub fn invalidate(&mut self) {
        self.clear();
    }

    fn shape(func: &Function) -> (usize, usize) {
        (func.dfg.num_insts(), func.layout.blocks().count())
    }

    fn materialize(&mut self, func: &Function) {
        let shape = Self::shape(func);
        if self.cfg.is_some() {
            debug_assert_eq!(
                self.shape, shape,
                "analysis read after IR mutation without Analysis::invalidate()"
            );
        } else {
            self.shape = shape;
            self.cfg = Some(ControlFlowGraph::with_function(func));
        }
        if self.dt.is_none() {
            self.dt = Some(DominatorTree::with_function(
                func,
                self.cfg.as_ref().unwrap(),
            ));
        }
        if self.la.is_none() {
            let mut la = LoopAnalysis::new();
            la.compute(func, self.cfg.as_ref().unwrap(), self.dt.as_ref().unwrap());
            self.la = Some(la);
        }
    }

    /// Recompute whatever is missing, then borrow all three.
    pub fn cfg_dt_la<'a>(&'a mut self, func: &Function) -> CfgDtLa<'a> {
        self.materialize(func);
        CfgDtLa {
            cfg: self.cfg.as_ref().unwrap(),
            dt: self.dt.as_ref().unwrap(),
            la: self.la.as_ref().unwrap(),
        }
    }

    /// Recompute whatever is missing, then borrow cfg+domtree.
    pub fn cfg_dt<'a>(&'a mut self, func: &Function) -> (&'a ControlFlowGraph, &'a DominatorTree) {
        self.materialize(func);
        (self.cfg.as_ref().unwrap(), self.dt.as_ref().unwrap())
    }

    pub fn cfg<'a>(&'a mut self, func: &Function) -> &'a ControlFlowGraph {
        self.materialize(func);
        self.cfg.as_ref().unwrap()
    }

    /// `ver` counter — bumping it is how the driver invalidates the
    /// whole cache between steps.
    pub fn version(&self) -> u64 {
        self.ver
    }

    /// The function's `shape` bits, computed once and held until the
    /// next `bump()`.
    pub fn shape_bits(&mut self, func: &Function) -> u32 {
        match self.fshape {
            Some((v, s)) if v == self.ver => s,
            _ => {
                let s = shape_of(func);
                self.fshape = Some((self.ver, s));
                s
            }
        }
    }
}

pub struct Step {
    /// `PLIRON_PIPELINE` name (unique per entry).
    pub name: &'static str,
    /// Counter name consumed by `PLIRON_OPT_BISECT`; `None` for steps
    /// that never drew a bisect unit (keeps numbering stable).
    pub bisect: Option<&'static str>,
    /// `PLIRON_STATS` label, or "" for no per-step stats line.
    pub stat: &'static str,
    /// Run the verifier after this step under `verify_full`.
    pub verify: bool,
    /// The step may rewrite the function — the driver bumps the
    /// analysis version after it runs. Pure steps keep the cache.
    pub mutates: bool,
    /// Worth a per-step disk entry under `PLIRON_STEPCACHE`: the
    /// expensive passes. Cheap steps recompute on a partial miss —
    /// a file read + stencil deserialize costs more than they do.
    pub cached: bool,
    /// `shape` bits the function must have for this step to possibly
    /// apply. Checked per function at run time (not selection time):
    /// earlier steps can create the shape a later step needs.
    pub shape: u32,
    pub gate: Gate,
    pub run: fn(&mut Pipe, &mut Analysis) -> usize,
}

impl Step {
    /// `shape` requirement, builder-style: `step(..).needs(shape::LOOPS)`.
    const fn needs(mut self, mask: u32) -> Step {
        self.shape |= mask;
        self
    }
}

const fn step(
    name: &'static str,
    stat: &'static str,
    gate: Gate,
    run: fn(&mut Pipe, &mut Analysis) -> usize,
) -> Step {
    Step {
        name,
        bisect: Some(name),
        stat,
        verify: true,
        mutates: true,
        cached: false,
        shape: 0,
        gate,
        run,
    }
}

/// `step` for the passes heavy enough that replaying a cached result
/// beats recomputing (`PLIRON_TIMING` shows the top of the list).
const fn cstep(
    name: &'static str,
    stat: &'static str,
    gate: Gate,
    run: fn(&mut Pipe, &mut Analysis) -> usize,
) -> Step {
    Step {
        name,
        bisect: Some(name),
        stat,
        verify: true,
        mutates: true,
        cached: true,
        shape: 0,
        gate,
        run,
    }
}

pub fn run_pipeline(p: &mut Pipe) {
    match crate::diffcheck::mode_for(p.fname) {
        None => {
            run_steps(p, false);
        }
        Some(crate::diffcheck::Mode::PerStep) => {
            run_steps(p, true);
        }
        Some(crate::diffcheck::Mode::Whole) => run_whole_checked(p),
    }
}

/// The steps this run selects, in order: `PLIRON_PIPELINE` if set (plus
/// the `Gate::Always` lowerings), else every default step whose gate
/// holds. Gate decisions are pure functions of `p.flags` + the env
/// snapshot — `p.flags` is fixed for the run — so the list is identical
/// before, during, and after the pipeline. `pipeline_lookup`
/// re-evaluates it: a gate flip that changes *which* steps run must
/// miss even when every recorded env read happens to match. `quiet`
/// suppresses the unknown-name diagnostic for that read-only
/// evaluation.
pub(crate) fn selected(p: &Pipe, quiet: bool) -> Vec<&'static Step> {
    if let Some(list) = &config().pipeline {
        let mut out = Vec::new();
        for name in list {
            match DEFAULT
                .iter()
                .find(|s| s.name == name.as_str() && !matches!(s.gate, Gate::Always))
            {
                Some(s) => out.push(s),
                None if !quiet => eprintln!("PLIRON_PIPELINE: unknown step `{name}`"),
                None => {}
            }
        }
        out.extend(DEFAULT.iter().filter(|s| matches!(s.gate, Gate::Always)));
        out
    } else {
        DEFAULT.iter().filter(|s| s.gate.ok(p)).collect()
    }
}

/// Runs the selected steps. With `check_each`, every step that changes
/// the function is differentially checked against its input; returns
/// whether any check reported a mismatch. The step-cache only engages
/// on the normal path — a checked run must see the pipeline actually
/// execute.
fn run_steps(p: &mut Pipe, check_each: bool) -> bool {
    let mut an = Analysis::default();
    let pk = if check_each {
        None
    } else {
        match crate::stepcache::pipeline_lookup(p, &mut an) {
            crate::stepcache::PipeLookup::Hit => return false,
            crate::stepcache::PipeLookup::Miss(k) => Some(k),
            crate::stepcache::PipeLookup::Off => None,
        }
    };
    // The outer read window accumulates the union of every step's env
    // reads (frames nest); `draws` records the bisect units consumed so
    // a cache hit can replay them and keep bisect numbering identical.
    // `pk` is only `Some` when the cache is on — off means zero
    // recorder overhead for the normal path.
    let stats0 = pk.is_some().then(|| p.stats.clone());
    let armed = pk.is_some() && reads_begin();
    let draws0 = bisect_count();
    let sel = selected(p, false);
    let mut found = false;
    for &s in &sel {
        if !check_each {
            run_step(p, s, &mut an);
            continue;
        }
        let before = p.clctx.func.clone();
        let before_text = before.display().to_string();
        run_step(p, s, &mut an);
        if p.clctx.func.display().to_string() != before_text {
            found |= crate::diffcheck::check_and_report(
                p.fname,
                s.name,
                &before,
                &p.clctx.func,
                &facts_of(p),
            );
        }
    }
    let reads = reads_take();
    let draws = bisect_count() - draws0;
    if let (Some(k), true) = (pk, armed) {
        let ran = sel.iter().map(|s| s.name);
        crate::stepcache::pipeline_store(&k, p, stats0.as_ref().unwrap(), reads, draws, ran);
    }
    crate::stepcache::flush(&mut an);
    found
}

fn facts_of<'a>(p: &'a Pipe) -> crate::diffcheck::Facts<'a> {
    crate::diffcheck::Facts {
        nonnull: Some(p.nonnull),
        noalias: Some(p.noalias),
        deref: Some(p.deref),
        frozen: Some(p.frozen),
        nowrite: Some(p.nowrite),
        // `cranelift_module` names a declared function `u0:<FuncId>`.
        self_name: Some(format!("u0:{}", p.func_id.as_u32())),
        derived: Some(&*p.derived),
    }
}

/// `PLIRON_DIFFCHECK=1`: check the whole pipeline once per function, and
/// only on a mismatch replay it step by step to name the guilty step.
fn run_whole_checked(p: &mut Pipe) {
    let initial = p.clctx.func.clone();
    let derived0 = p.derived.clone();
    run_steps(p, false);
    let facts = facts_of(p);
    let m = match crate::diffcheck::check(
        &initial,
        &p.clctx.func,
        &facts,
        &crate::diffcheck::config_from_env(),
    ) {
        Ok(sum) => {
            if crate::config::env_is("PLIRON_DIFFCHECK_VERBOSE", "1") {
                eprintln!(
                    "diffcheck {} compared={} inconclusive={} {}",
                    p.fname,
                    sum.compared,
                    sum.inconclusive,
                    sum.why.unwrap_or_default().lines().next().unwrap_or("")
                );
            }
            return;
        }
        Err(m) => m,
    };
    // Replay from the start with per-step checks, then put the real
    // result back so the build carries on exactly as without checking.
    let final_func = std::mem::replace(&mut p.clctx.func, initial.clone());
    let final_derived = std::mem::replace(p.derived, derived0);
    let real_stats = std::mem::take(p.stats);
    let localized = run_steps(p, true);
    *p.stats = real_stats;
    p.clctx.func = final_func;
    *p.derived = final_derived;
    if !localized {
        crate::diffcheck::report(
            p.fname,
            "<whole pipeline>",
            &initial,
            &p.clctx.func,
            &facts_of(p),
            &m,
        );
    }
}

/// Names of the steps this run selects, in order (for `clif-check`).
pub fn selected_names(p: &Pipe) -> Vec<&'static str> {
    selected(p, true).iter().map(|s| s.name).collect()
}

/// Runs one step by name regardless of its gate (for `clif-check`).
pub fn run_named(p: &mut Pipe, name: &str) -> Option<usize> {
    let s = DEFAULT.iter().find(|s| s.name == name)?;
    Some((s.run)(p, &mut Analysis::default()))
}

fn run_step(p: &mut Pipe, s: &Step, an: &mut Analysis) {
    if let Some(b) = s.bisect
        && !crate::config::bisect_in(b, p.fname)
    {
        return;
    }
    // `PLIRON_TRACE=pliron::step` prints `time.busy` per step×function on
    // span close; the step's real name is the span name (see trace.rs).
    let _s = crate::trace::dyn_site("pliron::step", &["fn"], s.name)
        .span_with("fn", p.fname)
        .entered();
    let _pg = p.prof_hook.map(|h| h(s.name));
    let t0 = std::time::Instant::now();
    // A step whose required shape bits are absent cannot apply — skip it
    // before any cache lookup. Evaluated on the current state rather than
    // at selection time: earlier steps can create a later step's shape
    // (unrolled bodies seed `slp` pairs, rotations create back edges).
    // `PLIRON_NOSHAPE` disables the skip for A/B debugging;
    // `PLIRON_SHAPE_ONLY=<hex>` masks which bits gate (bisecting).
    let mask = config()
        .get("PLIRON_SHAPE_ONLY")
        .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        .unwrap_or(u32::MAX);
    let req = s.shape & mask;
    if req != 0 && !env_set("PLIRON_NOSHAPE") && an.shape_bits(&p.clctx.func) & req != req {
        if env_set("PLIRON_SHAPE_DEBUG") {
            eprintln!("shape skip: {} lacks {req:04x} on {}", s.name, p.fname);
        }
        // `PLIRON_SHAPE_CHECK`: run the step anyway and report if it
        // changed the function — the bits are supposed to be necessary
        // conditions, so any mutation means the gate is wrong. The
        // function is restored so the pipeline keeps gated semantics.
        if env_set("PLIRON_SHAPE_CHECK") {
            let before = p.clctx.func.clone();
            let before_text = before.display().to_string();
            let derived0 = p.derived.clone();
            let k = (s.run)(p, an);
            if k > 0 || p.clctx.func.display().to_string() != before_text {
                eprintln!("SHAPE-GATE VIOLATION: {} fired on {}", s.name, p.fname);
            }
            p.clctx.func = before;
            *p.derived = derived0;
            an.invalidate();
        }
        *p.stats.timing.entry(s.name).or_default() += t0.elapsed();
        return;
    }
    p.cur_step = s.name;
    let k = if s.cached {
        match crate::stepcache::lookup(p, s, an) {
            Some(k) => k,
            None => crate::stepcache::compute(p, s, an),
        }
    } else {
        let k = (s.run)(p, an);
        if s.mutates {
            // The next cached step's chain key must see this mutation.
            an.clif_hash = None;
        }
        k
    };
    *p.stats.timing.entry(s.name).or_default() += t0.elapsed();
    trace(s.name, t0.elapsed().as_secs_f64() * 1e3, p.fname);
    if s.mutates {
        // Not conditional on `k`: a pass that removes blocks but
        // reports 0 would leave cfg/domtree stale for the next step.
        an.bump();
        if k > 0 {
            // Alias targets can themselves be aliased later, so chains
            // grow a hop per re-alias — flatten before the next step's
            // per-inst resolves multiply by the depth.
            p.clctx.func.dfg.compact_aliases();
            carry_aliases(p, an);
        }
    }
    p.log(s.stat, k);
    if s.verify
        && p.verify_full
        && let Err(e) = cranelift_codegen::verify_function(&p.clctx.func, p.isa.flags())
    {
        panic!(
            "{} broke `{}`: {e}\n{}",
            s.name,
            p.fname,
            p.clctx.func.display()
        );
    }
}

// Late dispatch merge: `edgespec`/`bittab` leave a dispatch block whose
// preds already computed the discriminator it re-tests. Single-pred
// chains splice in via `merge_chains`; the multi-pred case is edgespec's
// own edge threading, which only becomes legal once later passes have
// rebundled the dispatch's uses behind edge args (the `select→icmp`
// discriminant in the pred then folds to `brif` on the raw flag via
// `brif_not`/`brif_argspec`, and `jtround` threads the decided edge).
fn s_mergechain(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let prof2 = crate::env_get("PLIRON_JT_PROF2").is_some();
    let mut tt = |name: &str, t0: std::time::Instant| {
        if prof2 {
            eprintln!("    mc-{name} {:.3}ms", t0.elapsed().as_secs_f64() * 1e3);
        }
    };
    let mut n = 0;
    if crate::pass_enabled("PLIRON_CHAINMERGE") {
        let t0 = std::time::Instant::now();
        jumpthread::merge_chains(&mut p.clctx.func);
        tt("chains", t0);
    }
    let mut cd: Option<(ControlFlowGraph, DominatorTree)> = None;
    if crate::pass_enabled("PLIRON_DISPATCHDUP") {
        let t0 = std::time::Instant::now();
        // Duplicating a dispatch into a jump pred rewires that pred's own
        // jump-preds onto the dispatch — run to fixpoint so multi-hop
        // chains (851/853/854 -> 388 -> 859) fully collapse.
        for _ in 0..6 {
            let k = {
                let (_, d) = cd.get_or_insert_with(|| {
                    let c = ControlFlowGraph::with_function(&p.clctx.func);
                    let d = DominatorTree::with_function(&p.clctx.func, &c);
                    (c, d)
                });
                jumpthread::dispatch_dup(&mut p.clctx.func, d)
            };
            n += k;
            if k == 0 {
                break;
            }
            cd = None;
        }
        tt("dup1", t0);
    }
    if crate::pass_enabled("PLIRON_EDGESPEC2") {
        cd = None;
        if crate::pass_enabled("PLIRON_REBIND") {
            let t0 = std::time::Instant::now();
            n += jumpthread::rebind_exports(&mut p.clctx.func);
            tt("rebind", t0);
        }
        let t0 = std::time::Instant::now();
        n += edgespec::run(&mut p.clctx.func);
        tt("espec", t0);
        if crate::pass_enabled("PLIRON_DISPATCHDUP") {
            let t0 = std::time::Instant::now();
            // Edgespec can leave fresh jump preds on small dispatch blocks.
            for _ in 0..6 {
                let k = {
                    let (_, d) = cd.get_or_insert_with(|| {
                        let c = ControlFlowGraph::with_function(&p.clctx.func);
                        let d = DominatorTree::with_function(&p.clctx.func, &c);
                        (c, d)
                    });
                    jumpthread::dispatch_dup(&mut p.clctx.func, d)
                };
                n += k;
                if k == 0 {
                    break;
                }
                cd = None;
            }
            tt("dup2", t0);
        }
    }
    let t0 = std::time::Instant::now();
    let k = clifpeep::brif_fold(&mut p.clctx.func);
    tt("brifold", t0);
    n += k;
    if n > 0 {
        let t0 = std::time::Instant::now();
        jumpthread::remove_unreachable_blocks(&mut p.clctx.func);
        edgespec::sweep_dead(&mut p.clctx.func);
        tt("sweep", t0);
    }
    if p.dump {
        p.dump("mergechain");
    }
    n
}

// Late single threading round: edge args turned constant by sameargs/celim
// decide merge-block brifs jumpthread's early fixpoint couldn't see.
fn s_jtround(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let mut total = 0;
    // One `round` can apply at most one export-repair per merge block
    // (the `seen`/`locked` guards), and every repair locks the region it
    // touched. Later dispatch-dup edges decided by constants (the
    // block388 -> block935 disc) need follow-up rounds.
    let mut cd: Option<(ControlFlowGraph, DominatorTree)> = None;
    for i in 0..8 {
        let nn = jumpthread::NonNull::new(&p.clctx.func, p.nonnull, p.derived);
        let n = {
            let (c, d) = cd.get_or_insert_with(|| {
                let c = ControlFlowGraph::with_function(&p.clctx.func);
                let d = DominatorTree::with_function(&p.clctx.func, &c);
                (c, d)
            });
            jumpthread::round(
                &mut p.clctx.func,
                &nn,
                crate::pass_enabled("PLIRON_DOMCOND"),
                c,
                d,
            )
        };
        if n > 0 {
            cd = None;
        }
        total += n;
        if p.dump {
            p.dump(&format!("jtr{i}"));
        }
        if n == 0 {
            break;
        }
    }
    if total > 0 {
        // Threaded edges strand the merge blocks they bypassed.
        jumpthread::remove_unreachable_blocks(&mut p.clctx.func);
        edgespec::sweep_dead(&mut p.clctx.func);
    }
    p.stats.threaded += total;
    total
}

fn s_jumpthread(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let (n, dead, folds, pmap) = jumpthread::run(&mut p.clctx.func, p.nonnull, p.derived);
    if !pmap.is_empty() {
        carry_pmap(p, &pmap);
    }
    p.stats.threaded += n;
    p.stats.dead_params += dead;
    p.stats.dominated_folds += folds;
    p.dump("jumpthread");
    n
}

// rustc emits bounds-check `brif`s whose conditions become constants
// once jumpthread folds dominated tests; left in place they make a
// loop read as a long side-exit chain to unroll/punroll/loopvec.
// Fold early so the loop passes see the canonical shape; the late
// `PLIRON_CONSTBR=1` run below still catches constants exposed by
// later passes.
fn s_constbr(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = jumpthread::fold_const_branches(&mut p.clctx.func);
    if k > 0 {
        jumpthread::remove_unreachable_blocks(&mut p.clctx.func);
    }
    k
}

fn s_slotprom(p: &mut Pipe, an: &mut Analysis) -> usize {
    let (cfg, _) = an.cfg_dt(&p.clctx.func);
    let k = slotprom::run(&mut p.clctx.func, cfg);
    p.dump("slotprom");
    k
}

fn s_unroll(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = unroll::run(&mut p.clctx.func, an, p.nonnull, p.derived, p.prof);
    // The skipped loop is now unreachable, and its trip count still
    // flows into dead params/branches; dropping them frees the slot
    // address arithmetic so slot DSE can see the slot doesn't escape.
    if k > 0 && config().enabled("PLIRON_UNROLL_CLEANUP") {
        let ub = jumpthread::remove_unreachable_blocks(&mut p.clctx.func);
        let (mut dp, mut di) = (0, 0);
        for _ in 0..4 {
            let p2 = jumpthread::remove_dead_params(&mut p.clctx.func)
                + jumpthread::fold_same_target_brifs(&mut p.clctx.func);
            let i = jumpthread::remove_dead_insts(&mut p.clctx.func, true);
            (dp, di) = (dp + p2, di + i);
            if p2 + i == 0 {
                break;
            }
        }
        if env_set("PLIRON_UNROLL_DEBUG") {
            eprintln!(
                "unroll cleanup {}: {ub} blocks, {dp} params, {di} insts",
                p.fname
            );
        }
    }
    if k > 0 && env_set("PLIRON_UNROLL_DEBUG") {
        eprintln!("unroll {k} {}", p.fname);
    }
    p.dump("unroll");
    k
}

fn s_unreach(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = unreach::run(&mut p.clctx.func);
    p.stats.unreached += k;
    k
}

fn s_clifpeep(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::run(&mut p.clctx.func, p.x64);
    p.stats.peeped += k;
    k
}

fn s_tailmerge(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = tailmerge::run(&mut p.clctx.func);
    p.stats.merged += k;
    k
}

fn s_taildup(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = taildup::run(&mut p.clctx.func);
    p.stats.dupd += k;
    k
}

// `f(a) + f(b)`-style recursion → accumulate the first call and
// loop on the second's args (LLVM TRE); before loadfwd/licm so
// they can still hoist out of the new back-edge.
fn s_tailrec(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = tailrec::run(&mut p.clctx.func, p.func_id);
    if k > 0 && p.dump {
        p.dump("tailrec");
    }
    k
}

fn s_loadfwd(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let t = std::time::Instant::now();
    p.stats.forwarded += loadfwd::run(
        &mut p.clctx.func,
        p.noalias,
        p.invariant,
        p.operand_frozen,
        p.block_region,
        p.region_parent,
        p.nowrite,
        p.call_frozen,
        p.nocap,
    );
    if crate::env_get("PLIRON_JT_PROF2").is_some() {
        eprintln!("    lf-run {:?}", t.elapsed());
    }
    if p.flags.slot_dse {
        let prof2 = crate::env_get("PLIRON_JT_PROF2").is_some();
        // A load whose only user was a removed dead store keeps its own
        // slot alive, so alternate the two until nothing changes.
        for i in 0..4 {
            let t = std::time::Instant::now();
            p.stats.slot_dse += loadfwd::dead_slot_stores(&mut p.clctx.func);
            if prof2 {
                eprintln!("    lf-dss[{i}] {:?}", t.elapsed());
            }
            let t = std::time::Instant::now();
            let n = if config().enabled("PLIRON_DEAD_LOADS") {
                jumpthread::remove_dead_insts(
                    &mut p.clctx.func,
                    config().enabled("PLIRON_DEAD_PURE"),
                )
            } else {
                0
            };
            if prof2 {
                eprintln!("    lf-rdi[{i}] {n} {:?}", t.elapsed());
            }
            if n == 0 {
                break;
            }
        }
    }
    p.dump("loadfwd");
    0
}

fn s_dse(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = dse::run(&mut p.clctx.func, p.noalias, p.nowrite, p.call_frozen);
    if k > 0 && p.dump {
        p.dump("dse");
    }
    k
}

// Upstream Cranelift's egraph pass: GVN + ISLE cprop/remat rules +
// redundant-load elimination + egraph LICM. Opt-in while its cost and
// wins vs. our own passes are measured.
// Upstream's egraph doesn't know `try_call` (it predates EH), so
// functions with invokes are skipped entirely.
fn s_egraph(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let has_try_call = p
        .clctx
        .func
        .layout
        .blocks()
        .flat_map(|b| p.clctx.func.layout.block_insts(b))
        .any(|i| p.clctx.func.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::TryCall);
    if has_try_call || !p.clctx.egraph_safe() {
        return 0;
    }
    let mut cp = ControlPlane::default();
    // The egraph indexes values directly; our passes leave
    // `Value` aliases it doesn't resolve.
    p.clctx.func.dfg.resolve_all_aliases();
    p.clctx.compute_cfg();
    p.clctx.compute_domtree();
    p.clctx.egraph_pass(p.isa, &mut cp).unwrap_or_else(|e| {
        panic!(
            "egraph broke `{}`: {e}\n{}",
            p.fname,
            p.clctx.func.display()
        )
    });
    // The egraph populated cfg/domtree; later passes don't maintain
    // them, so drop them rather than leave stale-but-"valid" state
    // that `compile`'s initial verify would check.
    p.clctx.cfg.clear();
    p.clctx.domtree.clear();
    p.dump("egraph");
    0
}

// Loop-carried load reuse: sibling to indvars since it grows
// stepped header params the same machinery produces.
fn s_ldrot(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = indvars::rotld(&mut p.clctx.func, an);
    // Rotation drops the carried load's address chain; its leftover
    // `base + (iv-1)*s` computation is then dead, and a second indvars
    // round can step the remaining address into a param and evict the
    // shared base from the loop.
    if k > 0 && p.flags.indvars && config().enabled("PLIRON_INDUCT2") && bisect("indvars2") {
        an.invalidate();
        indvars::run(&mut p.clctx.func, an, amode_scale(p.isa));
    }
    k
}

fn s_licm(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = licm::run(
        &mut p.clctx.func,
        an,
        p.noalias,
        p.deref,
        p.nowrite,
        p.call_frozen,
        p.scoped,
        p.block_region,
        p.region_parent,
        p.operand_frozen,
    );
    if k > 0 && p.dump {
        p.dump("licm");
    }
    k
}

// Rebase descending counted ivs (`(0..n).rev()`) to ascending so
// loopidiom/loopvec see the canonical `0..bound` count shape.
fn s_revnorm(p: &mut Pipe, an: &mut Analysis) -> usize {
    revnorm::run(&mut p.clctx.func, an)
}

fn s_loopidiom(p: &mut Pipe, an: &mut Analysis) -> usize {
    loopidiom::run(&mut p.clctx.func, an, p.noalias, p.tcfg, p.fname)
}

// A second ifconv round: loadfwd/egraph dedup re-loaded predicate
// operands that made jumpthread's arm blocks non-pure, leaving
// `filter(pred)` range-check diamonds folded only now that they're
// pure forwarders. Flattening them to `select` chains is what lets
// loopvec see the count-if/conditional-update shapes.
fn s_ifconv2(p: &mut Pipe, _an: &mut Analysis) -> usize {
    // Carried-constant params (`RangeInclusive`'s exhausted flag
    // — a self-edge param fed a literal at entry) fold here so
    // the dispatch arms they drive die before flattening
    // (`PLIRON_CPARM`).
    if config().enabled("PLIRON_CPARM") {
        let (k, pmap) = clifpeep::cparm(&mut p.clctx.func);
        if !pmap.is_empty() {
            carry_pmap(p, &pmap);
        }
        if k > 0 {
            // Const folds leave dead `iconst`s inside the arm
            // chains; `resolve_arm`'s speculation cap counts
            // them and the diamond then looks too wide.
            jumpthread::remove_dead_insts(&mut p.clctx.func, true);
        }
        if p.dump {
            eprintln!(
                "==== clif {} after cparm ({k}) ====\n{}",
                p.fname,
                p.clctx.func.display()
            );
        }
    }
    // Nested range-check diamonds (`is_ascii_alphabetic` &co) fold
    // leaf-up: repeat to fixpoint (bounded by diamond depth).
    let mut k = 0;
    for _ in 0..8 {
        let n2 = ifconv::run(&mut p.clctx.func, p.derived);
        k += n2;
        if n2 == 0 {
            break;
        }
    }
    if k > 0 {
        // Folded merges leave dead forwarders still edging into
        // the merge — remove them so the merge is single-pred,
        // then splice it into the header for a flat single-block
        // loop.
        jumpthread::remove_unreachable_blocks(&mut p.clctx.func);
        clifpeep::fusechains(&mut p.clctx.func);
        if p.dump {
            p.dump("ifconv2");
        }
    }
    k
}

// Unswitch invariant brifs before bcheck: the constant-folded
// copies (e.g. a `capacity == 0` early-out in HashMap::get) shrink
// each version's body and can expose more foldable checks.
fn s_unswitch(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = bcheck::unswitch(&mut p.clctx.func, an, p.fname);
    if k > 0 && p.dump {
        p.dump("unswitch");
    }
    k
}

// bcheck before loopvec: the versioned fast path has no per-iteration
// bounds branches, so it's vectorizable where the checked loop isn't.
fn s_bcheck(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = bcheck::run(&mut p.clctx.func, an, p.fname, p.has_eh);
    p.dump("bcheck");
    k
}

fn s_divconst(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = divconst::run(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("divconst");
    }
    k
}

fn s_loopvec(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = loopvec::run(
        &mut p.clctx.func,
        an,
        p.fname,
        p.noalias,
        p.tcfg,
        p.simd,
        p.x64,
        p.cmp64,
        p.prof,
    );
    // loopvec emits `base + iv*esize` addressing fresh each iteration;
    // a second indvars round turns them into stepped pointers.
    if k > 0 && p.flags.indvars && config().enabled("PLIRON_INDUCT2") && bisect("indvars2") {
        // loopvec rewrote the loop the cache describes.
        an.invalidate();
        indvars::run(&mut p.clctx.func, an, amode_scale(p.isa));
    }
    k
}

fn s_slp(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = slp::run(&mut p.clctx.func, an, p.noalias, p.simd, p.x64);
    if env_set("PLIRON_XPACK_CLIF") {
        eprintln!(
            "==== clif {} after slp ====\n{}",
            p.fname,
            p.clctx.func.display()
        );
    }
    k
}

fn s_switchmap(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = switchmap::run(&mut p.clctx.func);
    // The new bounds tests are often implied by a dominating check.
    let f =
        if k > 0 && config().enabled("PLIRON_DOMCOND") && config().enabled("PLIRON_SWITCHMAP_FOLD")
        {
            let folds = jumpthread::fold_dominated_conds(&mut p.clctx.func);
            p.stats.dominated_folds += folds;
            if folds > 0 {
                jumpthread::fold_const_branches(&mut p.clctx.func)
            } else {
                0
            }
        } else {
            0
        };
    if k > 0 && env_set("PLIRON_SWITCHMAP_DEBUG") {
        eprintln!("switchmap {k} fold {f} {}", p.fname);
    }
    0
}

fn s_constbr2(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = jumpthread::fold_const_branches(&mut p.clctx.func);
    if k > 0 && env_set("PLIRON_CONSTBR_DEBUG") {
        eprintln!("constbr {k} {}", p.fname);
    }
    k
}

fn s_punroll(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = punroll::run(&mut p.clctx.func, an, p.fname, p.prof);
    if k > 0 && p.dump {
        p.dump("punroll");
    }
    k
}

fn s_constdedup(p: &mut Pipe, an: &mut Analysis) -> usize {
    let (_, dt) = an.cfg_dt(&p.clctx.func);
    let k = constdedup::run(&mut p.clctx.func, dt);
    p.dump("constdedup");
    k
}

fn s_splink(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = splink::run(&mut p.clctx.func);
    p.dump("splink");
    k
}

fn s_frozen_loads(p: &mut Pipe, _an: &mut Analysis) -> usize {
    if p.frozen.is_empty() {
        return 0;
    }
    let k = clifpeep::frozen_loads(&mut p.clctx.func, p.frozen);
    if p.dump {
        eprintln!(
            "==== clif {}: {k} frozen loads, params {:?} ====\n{}",
            p.fname,
            p.frozen,
            p.clctx.func.display()
        );
    }
    k
}

// `isub x, k` → `iadd x, -k` so x64's `lea` lowering can write a
// call-arg/phi result non-destructively instead of `mov` + `sub`.
fn s_sublea(p: &mut Pipe, _an: &mut Analysis) -> usize {
    clifpeep::sublea(&mut p.clctx.func)
}

// Constant folding for insts the loop passes emit after `clifpeep`'s
// early round — see the `csweep2` step.
fn s_csweep2(p: &mut Pipe, _an: &mut Analysis) -> usize {
    clifpeep::const_sweep(&mut p.clctx.func)
}

// `brif (icmp x <u 0x80)` / `brif (icmp x <s 0)` sign-bit tests →
// `brif (band v, 1<<b)` — see `clifpeep::sgntest`.
fn s_sgntest(p: &mut Pipe, _an: &mut Analysis) -> usize {
    clifpeep::sgntest(&mut p.clctx.func)
}

// Two-compare range predicates (`x >= lo && x <= hi`) merged to
// `(x - lo) <u w` — see `clifpeep::rgmerge`.
fn s_rgmerge(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::rgmerge(&mut p.clctx.func);
    if k > 0 {
        p.dump("rgmerge");
    }
    k
}

// Complementary `load (select)` pairs → two speculative loads + value
// selects. The diamonds only collapse into selects late (fuse/
// sameargs), so this runs near the end rather than in `clifpeep`'s
// early round (which also runs it for anything already selected).
fn s_loadsel(p: &mut Pipe, _an: &mut Analysis) -> usize {
    clifpeep::loadsel(&mut p.clctx.func)
}

// `op multi_use, one_use` → `op one_use, multi_use` on commutative
// binops: the fewer-use operand takes two-address dst, so regalloc
// clobbers it in place instead of copying the live one (`movdqa`
// splat copies in matmul's `mulpd` loop).
fn s_commute(p: &mut Pipe, _an: &mut Analysis) -> usize {
    commute::run(&mut p.clctx.func)
}

// Scalar min/max wider than a register (i128) has no Cranelift
// lowering on any target; several passes can emit it.
fn s_wide_minmax(p: &mut Pipe, _an: &mut Analysis) -> usize {
    clifpeep::wide_minmax(&mut p.clctx.func)
}

// Rebind cold-call args defined in hot code so their ABI register
// pinning lands at the cold call site instead of constraining the
// whole hot loop's register allocation.
fn s_coldarg(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::coldargs(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("coldargs");
    }
    k
}

// Splice single-predecessor successors into their predecessor so
// hot loops broken into several blocks by cold-edge branches lose
// the per-edge register-allocation copy shuffle.
fn s_fuse(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::fusechains(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("fuse");
    }
    k
}

// Retarget edges past `jump`-only forwarder blocks, including
// try_call landing-pad and normal-return edges that the earlier
// jumpthread bypass cannot touch; each surviving forwarder emits a
// `b` in the final stream.
fn s_edgefwd(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = edgefwd::run(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("edgefwd");
    }
    k
}

// Fold multi-pred forwarders edgefwd leaves: `brif`-only blocks
// absorbed into `jump` preds, and `jump` forwarders with
// escaping params. Must run before coldedges so any hot->cold
// edges it creates still get cold adapters.
fn s_foldf(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::foldforwarders(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("foldf");
    }
    k
}

// Fold compares decided by dominating branch conditions (bounds
// checks implied by earlier checks) before the cold-edge split:
// a folded check's dead panic edge then never gets an adapter.
fn s_celim(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = celim::run(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("celim");
    }
    k
}

// Refold loop-carried affine params (i' = i+s on every back
// edge) onto one anchor IV: `p` becomes `bp + r*i` computed in
// the loop body instead of a regalloc parallel copy plus an
// increment on the back edge. Runs after celim (icmp folds can
// change which param is the counter) and before sameargs so
// newly-uniform params are still picked up.
//
// Load/store uses of dropped params get a fresh `bp + scaled(iv)`
// per use that folds into a base+index*scale amode, so no
// address math materializes; constant-trip counters fold their
// exit test onto the anchor's step value and die with deadrecs.
fn s_ivrefold(p: &mut Pipe, an: &mut Analysis) -> usize {
    let (k, pmap) = ivrefold::run(&mut p.clctx.func, an);
    if !pmap.is_empty() {
        carry_pmap(p, &pmap);
    }
    if k > 0 && p.dump {
        p.dump("ivrefold");
    }
    k
}

// Drop loop params whose only uses feed their own back-edge
// arg — dead recurrences (e.g. the i*4 byte-offset cursor
// indvars leaves when addressing moved to another param).
// Forward liveness can't see these: the param is "used" by
// its increment and the increment is "used" by the edge.
fn s_deadrec(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = ivrefold::deadrecs(&mut p.clctx.func, an);
    if k > 0 && p.dump {
        p.dump("deadrec");
    }
    k
}

// Per-block CSE of duplicate pure insts so redundant chains
// (the vectorizer's separate load/store address streams,
// rustc's repeated index math) collapse to one value before
// sameargs merges the now-identical edge args.
fn s_rangefold(p: &mut Pipe, _an: &mut Analysis) -> usize {
    rangefold::run(&mut p.clctx.func)
}

// A merge param's pure use-chain sunk onto the incoming edges (the
// inverse of arghoist): `count + flag` where flag merges as 1/0/ret
// becomes per-edge `count+1` / `count+ret0` — the flag's merge slot
// (a spill in regalloc) disappears.
fn s_paramsink(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let (k, moved) = paramsink::run(&mut p.clctx.func);
    if !moved.is_empty() {
        let pmap: FxHashMap<Value, Value> = moved.into_iter().collect();
        carry_pmap(p, &pmap);
    }
    if k > 0 {
        p.dump("paramsink");
    }
    k
}

// `icmp eq (select c, 0, ptr), 0` → `c` when `ptr` is non-null — the
// Option-niche test behind `Chars::next`'s bounds check; kills the
// `mov 0; cmp; csel; cmp` tail sequence.
fn s_niche(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let nn = jumpthread::NonNull::new(&p.clctx.func, p.nonnull, p.derived);
    let k =
        niche::run(&mut p.clctx.func, &nn) + jumpthread::remove_dead_insts(&mut p.clctx.func, true);
    if k > 0 {
        p.dump("niche");
    }
    k
}

// `brif c` edge args `select(c,a,b)` collapse to an arm — the loop-tail
// `csel` materializing `select(end?, 0, cursor)` on the edge where
// `end?` is already known-false.
fn s_selargs(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::selargs(&mut p.clctx.func)
        + jumpthread::remove_dead_insts(&mut p.clctx.func, true);
    if k > 0 {
        p.dump("selargs");
    }
    k
}

fn s_lcse(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::lcse(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("lcse");
    }
    k
}

// Dominator-scoped value numbering. lcse is per-block, so the
// duplicated increments looprot/punroll leave on sibling diamond
// edges (`i+1` recomputed per arm, then passed as merge params)
// never unify. dgvn aliases the dominated copies to the
// dominating def; sameargs then drops the now-uniform params and
// the increment materializes once above the branch.
fn s_dgvn(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::dgvn(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("dgvn");
    }
    k
}

// Drop block params that receive the same value on every
// incoming edge (and merge duplicate params): each surviving
// param costs a regalloc parallel copy per conditional pred —
// the `mov;b` edge-split blocks that dominate the block count.
// Runs before coldedges so adapters inherit smaller arg lists.
fn s_sameargs(p: &mut Pipe, _an: &mut Analysis) -> usize {
    if env_set("PLIRON_SA_DEBUG") || env_set("PLIRON_PHI_DEBUG") {
        eprintln!("--- sameargs {}", p.fname);
    }
    let (k, pmap) = clifpeep::sameargs(&mut p.clctx.func);
    if k > 0 {
        carry_pmap(p, &pmap);
        if p.dump {
            p.dump("sameargs");
        }
    }
    k
}

// `brif (icmp cc (select c, K, x), K)` niche-select dispatch →
// `brif c` + a middle block testing `x` — see
// `jumpthread::split_niche_tests`.
fn s_seltest(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = jumpthread::split_niche_tests(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("seltest");
    }
    k
}

// Second load-forwarding pass. sameargs' param drops (esp. affparm's
// `p_j = p_i + k` collapse) turn formerly opaque param addresses into
// `iadd` chains that `concrete` peels to exact (root, offset) locs —
// making stores/loads that were unprovable at the early run disjoint.
fn s_loadfwd2(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = loadfwd::run(
        &mut p.clctx.func,
        p.noalias,
        p.invariant,
        p.operand_frozen,
        p.block_region,
        p.region_parent,
        p.nowrite,
        p.call_frozen,
        p.nocap,
    );
    if k > 0 && p.dump {
        p.dump("loadfwd2");
    }
    k
}

// `band v, 1` on an already-0/1 scalar (rustc's i1 normalization
// residue around `icmp`/`uextend`/`select` booleans) — one `and`
// per element in scalar count_if loops. After sameargs: the merged
// predicate param is only alias-visible once it has dropped.
fn s_mask01(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::mask01(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("mask01");
    }
    k
}

// Thread edges through `brif` dispatch blocks decided by the supplied
// args (Option/enum sentinel selects, constant discriminants): the
// `select`/`ireduce`/`icmp eq k` cascades collapse to a `brif` on the
// flag or a direct edge. Runs before coldedges so any hot->cold edge
// it creates still gets an adapter.
fn s_edgespec(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = edgespec::run(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("edgespec");
    }
    k
}

// Sparse small-domain membership trees (is_whitespace/is_ascii_*
// `icmp`+`brif` chains) become one guarded `ushr` bitmask test when
// the accepted set fits a 64-bit window.
fn s_bittab(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = bittab::run(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("bittab");
    }
    k
}

// Move hot->cold block-param edge copies into cold adapter blocks
// so the moves only execute when the cold edge is actually taken.
// Runs after edgefwd: its bypass has no cold check and would
// retarget straight through the adapters, putting the parallel
// copy back in the hot predecessor tail.
fn s_coldedge(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::coldedges(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("coldedges");
    }
    k
}

// Second DSE: ifconv's store→load forwarding and the late edge
// transforms rewrite a slot's last readers to the stored value,
// leaving stores the early dse saw live (e.g. the `&key` slot
// materialized for `HashMap::get`, dead once `hash_one`'s byte
// loads forward from the key value itself).
fn s_dse2(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = dse::run(&mut p.clctx.func, p.noalias, p.nowrite, p.call_frozen);
    if k > 0 && p.dump {
        p.dump("dse2");
    }
    k
}

fn s_dse3(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = dse::run(&mut p.clctx.func, p.noalias, p.nowrite, p.call_frozen);
    if k > 0 && p.dump {
        p.dump("dse3");
    }
    k
}

// Edge-specialize a `jump` continuation whose param-bound loads a pred
// just stored: `entry(k).or_insert(0)`'s `*v += 1` continuation reads the
// `0` the fresh-insert edge wrote — clone the small block onto the edge
// and alias the load to the stored value so lcse/dse fold `store 0` +
// `load;+1;store` into `store 1`.
fn s_memspec(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = memspec::run(&mut p.clctx.func, p.noalias, p.nowrite, p.call_frozen);
    if k > 0 && p.dump {
        p.dump("memspec");
    }
    k
}

// Lane-by-lane constant stores into a stack slot that whole-vector
// loads read back (rustc's `Simd` const materialization, e.g.
// hashbrown's `u8x8` special-byte checks in `reserve_rehash`) fold
// to `iconst`+`bitcast`, and the lane stores go dead.
fn s_vecslot(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::vecslot(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("vecslot");
    }
    k
}

// `uextend.i64 (load.i32)` -> `uload32.i64` + `ireduce.i32`: x64
// emits movl either way, but dropping the extension vreg frees
// regalloc from coalescing it (a stray `movq` showed up in
// loop bodies). Last transform before codegen — earlier passes
// match `Opcode::Load` and must still see the load.
fn s_uload32(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::uloads32(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("uload32");
    }
    k
}

fn s_postinc(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = indvars::postinc(&mut p.clctx.func, an, 1024);
    if k > 0 && p.dump {
        p.dump("postinc");
    }
    k
}

// Carry `d`'s recorded facts onto `w` when `w` is `d`'s value
// everywhere — a merge carrier, an alias target, or a sunk/rebased
// replacement.
pub(crate) fn carry_one(p: &mut Pipe, d: Value, w: Value) {
    p.carry_log.push((d, w));
    // `nonnull` and `derived` are site facts — `!nonnull`/inbounds mark a
    // load result as non-null *or poison* at the point that justified it.
    // Carrying them to a value defined outside that site leaks the
    // guarantee into code the fact never covered: a `!nonnull` `Some`-arm
    // payload read unified with an unguarded `Option` niche load made
    // `jtround` fold a real null test (serde_derive `get_member`). Carry
    // only where `d`'s definition still covers `w`'s — same block, `d`
    // first (a param "def" is the block head).
    let covered = {
        let f = &p.clctx.func;
        // `try_value_def` skips reserved/stale values and doesn't chase
        // aliases (`value_def` resolves unchecked and panics on chains
        // ending at a tombstone); an alias def just falls through to
        // no-carry.
        let blk = |v: Value| match f.dfg.try_value_def(v) {
            Some(ValueDef::Result(i, _)) => f.layout.inst_block(i),
            Some(ValueDef::Param(b, _)) => Some(b),
            _ => None,
        };
        let pos = |v: Value| match f.dfg.try_value_def(v) {
            Some(ValueDef::Param(..)) => Some(0usize),
            Some(ValueDef::Result(i, _)) => {
                f.layout.block_insts(blk(v)?).position(|x| x == i)
            }
            _ => None,
        };
        blk(d).is_some()
            && blk(d) == blk(w)
            && matches!((pos(d), pos(w)), (Some(a), Some(b)) if a <= b)
    };
    if covered
        && let Some(&b) = p.derived.get(&d)
    {
        p.derived.insert(w, b);
    }
    if covered && p.nonnull.contains(&d) {
        if std::env::var_os("PLIRON_NNDBG").is_some() {
            let dd = p
                .clctx
                .func
                .dfg
                .value_def(d)
                .inst()
                .map(|i| p.clctx.func.dfg.display_inst(i).to_string());
            let wd = p
                .clctx
                .func
                .dfg
                .value_def(w)
                .inst()
                .map(|i| p.clctx.func.dfg.display_inst(i).to_string());
            eprintln!("nndbg carry {w} <- {d} [{}] w={wd:?} d={dd:?}", p.cur_step);
        }
        p.nonnull.insert(w);
    }
    if p.noalias.contains(&d) {
        p.noalias.insert(w);
    }
    if let Some(&f) = p.frozen.get(&d) {
        p.frozen.insert(w, f);
    }
    if let Some(&x) = p.deref.get(&d) {
        p.deref.insert(w, x);
    }
    if let Some(v) = p.operand_frozen.get(&d) {
        let v = v.clone();
        p.operand_frozen.insert(w, v);
    }
    for lt in p.lifetimes.iter_mut() {
        if lt.0 == d {
            lt.0 = w;
        }
    }
}

fn carry_pmap(p: &mut Pipe, pmap: &FxHashMap<Value, Value>) {
    let chase = |mut v: Value| {
        let mut n = pmap.len() + 1;
        while let Some(&w) = pmap.get(&v) {
            v = w;
            n -= 1;
            if n == 0 {
                break;
            }
        }
        v
    };
    // `x` derived from a dropped param is derived from its carrier, and
    // a dropped param's own base moves to the carrier too.
    let keys: Vec<Value> = p.derived.keys().copied().collect();
    for x in keys {
        if let Some(&b) = p.derived.get(&x) {
            let b = chase(b);
            p.derived.insert(x, b);
        }
    }
    // `pmap[d] = w` makes `w` `d`'s value on every path — the merge
    // invariant — so `d`'s recorded facts hold of `w` too, whatever the
    // merge kind (a phase-A carrier, a deduped sibling, or a
    // materialized `f(siblings)` tree).
    for (&d, &w) in pmap {
        carry_one(p, d, chase(w));
    }
}

/// `change_to_alias(d, w)` makes `w` `d`'s value everywhere, so facts
/// keyed on `d` transfer to `w`. Two dozen passes alias values; rather
/// than plumbing a merge map out of each, scan the alias table after
/// mutating steps — re-carrying an already-carried pair is idempotent,
/// and `an.carried` skips pairs already handled.
fn carry_aliases(p: &mut Pipe, an: &mut Analysis) {
    if p.derived.is_empty()
        && p.nonnull.is_empty()
        && p.noalias.is_empty()
        && p.frozen.is_empty()
        && p.deref.is_empty()
        && p.operand_frozen.is_empty()
        && p.lifetimes.is_empty()
    {
        return;
    }
    let func = &p.clctx.func;
    let pairs: Vec<(Value, Value)> = func
        .dfg
        .values()
        .filter_map(|v| {
            // Tombstone chains from deleted defs dead-end at reserved
            // targets — `resolve_aliases` panics there; just skip them.
            let r = func.dfg.try_resolve_aliases(v)?;
            (r != v && !an.carried.contains(&(v, r))).then_some((v, r))
        })
        .collect();
    for (d, w) in pairs {
        an.carried.insert((d, w));
        carry_one(p, d, w);
    }
}

fn s_webmerge(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let (k, pmap) = clifpeep::webmerge(&mut p.clctx.func);
    if k > 0 {
        carry_pmap(p, &pmap);
        if p.dump {
            p.dump("webmerge");
        }
    }
    k
}

// Loop count tests against a wide constant rematerialize it per
// iteration; compare `(a - j0) cc (K - j0)` instead — the bound is
// a non-constant preheader value codegen pins in a register. Runs
// late (after loop rotation) so latch-copied tests are covered.
fn s_boundcmp(p: &mut Pipe, an: &mut Analysis) -> usize {
    // x64 `cmp reg, imm32` sign-extends the immediate, so unsigned bounds
    // up to i32::MAX compare for free — the countdown only earns its
    // extra param and isub past that. Other targets keep the old floor.
    let kmin = if p.x64 { i32::MAX as i64 + 1 } else { 4096 };
    let k = indvars::boundcmp(&mut p.clctx.func, an, kmin);
    if k > 0 && p.dump {
        p.dump("boundcmp");
    }
    k
}

// Dead self-feed recurrences — boundcmp's countdown replaces the
// bound-tested iv with `rem`, leaving the iv's `+=` update feeding
// only its own edge arg. Runs after boundcmp/looprot.
fn s_selfarg(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::selfarg(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("selfarg");
    }
    k
}

// Small `br_table`s become `icmp eq`+`brif` chains: the table
// dispatch is ~8 instructions plus an indirect-branch prediction
// per execution, which dominates iterator state machines (e.g.
// `SplitWhitespace::next`'s per-char discriminant dispatch).
// Runs last so nothing un-does the chains and adapters/edge
// forwarding have already collapsed the surrounding shape.
fn s_brchain(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = brchain::run(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("brchain");
    }
    k
}

// Merge disjoint-lifetime explicit slots — last, so slot references
// cloned by earlier passes are all accounted for before sharing.
fn s_slotmerge(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let stats = env_set("PLIRON_STATS");
    let before: u64 = stats
        .then(|| {
            p.clctx
                .func
                .sized_stack_slots
                .values()
                .map(|d| d.size as u64)
                .sum()
        })
        .unwrap_or(0);
    let k = slotmerge::run(&mut p.clctx.func, p.nowrite, p.lifetimes, p.nocap);
    if stats {
        let after: u64 = p
            .clctx
            .func
            .sized_stack_slots
            .values()
            .map(|d| d.size as u64)
            .sum();
        eprintln!(
            "slotmerge {k} {before}->{after}B {} {}",
            p.nocap.len(),
            p.fname
        );
    }
    if k > 0 && p.dump {
        p.dump("slotmerge");
    }
    k
}

// `call %Mem{cpy,move,set}(dst, src, iconst n)` for small n becomes inline
// unaligned load/store chunks — rustc's element-wise copy intrinsics reach
// CLIF with a runtime len that only folds to `iconst` here.
fn s_memcall(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::memcall(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("memcall");
    }
    k
}

// Leaf `return` blocks that share a `brif` with a loop header are
// colder than the loop: mark them cold so the emitted RPO order puts
// them last and the loop arm falls through. Runs last — it's pure
// layout metadata, so nothing downstream needs to see it.
fn s_coldret(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = clifpeep::coldrets(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("coldrets");
    }
    k
}

/// The target folds `base + reg*{1,2,4,8}` into a memory addressing mode
/// (x86 SIB; aarch64 extended-register offset). wasm/riscv compute
/// addresses explicitly — stepped pointer IVs still pay off there.
fn amode_scale(isa: &dyn TargetIsa) -> bool {
    use target_lexicon::Architecture as A;
    matches!(isa.triple().architecture, A::X86_64 | A::Aarch64(_))
}

fn s_indvars(p: &mut Pipe, an: &mut Analysis) -> usize {
    indvars::run(&mut p.clctx.func, an, amode_scale(p.isa))
}

fn s_loopdel(p: &mut Pipe, an: &mut Analysis) -> usize {
    loopdel::run(&mut p.clctx.func, an)
}

fn s_looprot(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = looprot::run(&mut p.clctx.func, an, p.derived);
    p.dump("looprot");
    k
}

const JT: Gate = Gate::Flag(FlagId::Jumpthread);

pub static DEFAULT: &[Step] = &[
    Step {
        name: "jumpthread",
        bisect: Some("jumpthread"),
        stat: "",
        verify: true,
        mutates: true,
        // Ungated on shape: beyond edge threading it also merges
        // identical exit blocks, which needs no edges at all.
        cached: true,
        shape: 0,
        gate: JT,
        run: s_jumpthread,
    },
    cstep(
        "constbr",
        "constbr",
        Gate::All(&[JT, Gate::Env("PLIRON_CONSTBR")]),
        s_constbr,
    )
    .needs(shape::CONDBR),
    Step {
        name: "unroll",
        bisect: Some("unroll"),
        stat: "",
        verify: true,
        mutates: true,
        cached: true,
        shape: shape::LOOPS,
        gate: Gate::All(&[JT, Gate::Env("PLIRON_UNROLL")]),
        run: s_unroll,
    },
    step("unreach", "", Gate::Flag(FlagId::Unreach), s_unreach),
    Step {
        name: "clifpeep",
        bisect: Some("clifpeep"),
        stat: "",
        verify: false,
        mutates: true,
        cached: true,
        shape: 0,
        gate: Gate::Flag(FlagId::Peep),
        run: s_clifpeep,
    },
    step("tailmerge", "", Gate::Flag(FlagId::Tailmerge), s_tailmerge),
    step("taildup", "", Gate::Flag(FlagId::Taildup), s_taildup),
    step("tailrec", "tailrec", Gate::Flag(FlagId::Tailrec), s_tailrec).needs(shape::CALL),
    Step {
        name: "loadfwd",
        bisect: Some("loadfwd"),
        stat: "",
        verify: true,
        mutates: true,
        cached: true,
        // Fires on mem-free functions too (arg/result forwarding in
        // e.g. alloc shims), so no shape gate.
        shape: 0,
        gate: Gate::Flag(FlagId::Loadfwd),
        run: s_loadfwd,
    },
    // Stack-slot SSA promotion before the loop passes so the promoted
    // fields are loop-carried SSA values indvars/licm/bcheck can see.
    step(
        "slotprom",
        "slotprom",
        Gate::Env("PLIRON_SLOTPROM"),
        s_slotprom,
    )
    .needs(shape::MEM),
    cstep("dse", "dse", Gate::Flag(FlagId::Dse), s_dse).needs(shape::MEM),
    Step {
        name: "egraph",
        bisect: Some("egraph"),
        stat: "",
        verify: false,
        mutates: true,
        cached: true,
        shape: 0,
        gate: Gate::OptIn("PLIRON_EGRAPH"),
        run: s_egraph,
    },
    cstep("indvars", "indvars", Gate::Flag(FlagId::Indvars), s_indvars).needs(shape::LOOPS),
    step(
        "ldrot",
        "ldrot",
        Gate::All(&[Gate::Flag(FlagId::Indvars), Gate::Env("PLIRON_LDROT")]),
        s_ldrot,
    )
    .needs(shape::LOOPS | shape::MEM),
    cstep("licm", "licm", Gate::Flag(FlagId::Licm), s_licm).needs(shape::LOOPS),
    step("revnorm", "revnorm", Gate::Env("PLIRON_REVNORM"), s_revnorm).needs(shape::LOOPS),
    step(
        "loopidiom",
        "idiom",
        Gate::Flag(FlagId::Loopidiom),
        s_loopidiom,
    )
    .needs(shape::LOOPS | shape::MEM),
    cstep(
        "ifconv2",
        "ifconv2",
        Gate::All(&[Gate::Flag(FlagId::Loopvec), Gate::Env("PLIRON_IFCONV2")]),
        s_ifconv2,
    )
    .needs(shape::CONDBR),
    cstep(
        "unswitch",
        "unswitch",
        Gate::Env("PLIRON_UNSWITCH"),
        s_unswitch,
    )
    .needs(shape::LOOPS),
    cstep("bcheck", "bcheck", Gate::Flag(FlagId::Bcheck), s_bcheck).needs(shape::CONDBR),
    // `x / k` / `x % k` → magic-multiply + shift sequences. The div/rem
    // ops are `can_trap` side effects to loopvec, so expanding here is
    // what lets a `x % 3 != 0` filter predicate vectorize at all — and
    // the ~5-instr sequence beats a hardware `sdiv` in scalar code too.
    step(
        "divconst",
        "divconst",
        Gate::Env("PLIRON_DIVCONST"),
        s_divconst,
    )
    .needs(shape::DIVREM),
    cstep("loopvec", "loopvec", Gate::Flag(FlagId::Loopvec), s_loopvec).needs(shape::LOOPS),
    cstep("slp", "slp", Gate::Flag(FlagId::Slp), s_slp).needs(shape::SLP),
    // bcheck's per-loop version guards sit at the inner loop's entry —
    // their len/min/carry parts are invariant in every enclosing loop
    // but the only licm already ran. matmul's guard re-evaluates
    // `min(n-1, len)` and the offset chain ~n² times; hoist it.
    cstep("licm2", "licm", Gate::Flag(FlagId::Licm), s_licm).needs(shape::LOOPS),
    step("switchmap", "", Gate::Env("PLIRON_SWITCHMAP"), s_switchmap).needs(shape::SWITCH),
    Step {
        name: "constbr2",
        bisect: Some("constbr"),
        stat: "",
        verify: true,
        mutates: true,
        cached: false,
        shape: shape::CONDBR,
        gate: Gate::OptIn("PLIRON_CONSTBR"),
        run: s_constbr2,
    },
    cstep(
        "loopdel",
        "loopdel",
        Gate::All(&[JT, Gate::Env("PLIRON_LOOPDEL")]),
        s_loopdel,
    )
    .needs(shape::LOOPS),
    cstep("looprot", "", Gate::Env("PLIRON_LOOPROT"), s_looprot).needs(shape::LOOPS),
    // Scalar unroll of linear counted loops (e.g. ordered float
    // reductions that can't vectorize): K bodies per branch.
    cstep(
        "punroll",
        "punroll",
        Gate::All(&[JT, Gate::Env("PLIRON_PUNROLL")]),
        s_punroll,
    )
    .needs(shape::LOOPS),
    // Merge identical constants across dominated blocks — unroll/clone
    // passes stamp fresh `iconst`s per copy, and each one is a `mov`
    // downstream instead of a shared register (or `wzr`).
    step(
        "constdedup",
        "constdedup",
        Gate::Env("PLIRON_CSTDEDUP"),
        s_constdedup,
    ),
    Step {
        name: "frozen_loads",
        bisect: None,
        stat: "",
        verify: false,
        mutates: true,
        cached: false,
        shape: 0,
        gate: Gate::Always,
        run: s_frozen_loads,
    },
    cstep(
        "sublea",
        "sublea",
        Gate::All(&[Gate::Flag(FlagId::Peep), Gate::Env("PLIRON_SUBLEA")]),
        s_sublea,
    ),
    Step {
        name: "commute",
        bisect: Some("commute"),
        stat: "commute",
        verify: false,
        mutates: true,
        cached: false,
        shape: 0,
        gate: Gate::All(&[Gate::Flag(FlagId::Peep), Gate::Env("PLIRON_COMMUTE")]),
        run: s_commute,
    },
    Step {
        name: "wide_minmax",
        bisect: None,
        stat: "",
        verify: false,
        mutates: true,
        cached: false,
        shape: 0,
        gate: Gate::Always,
        run: s_wide_minmax,
    },
    Step {
        name: "coldarg",
        bisect: Some("coldarg"),
        stat: "coldarg",
        verify: false,
        mutates: true,
        cached: false,
        shape: 0,
        gate: Gate::Env("PLIRON_COLDARG"),
        run: s_coldarg,
    },
    step("fuse", "fuse", Gate::Env("PLIRON_FUSE"), s_fuse).needs(shape::MULTI),
    cstep("edgefwd", "edgefwd", Gate::Env("PLIRON_EDGEFWD"), s_edgefwd),
    step("foldf", "foldf", Gate::Env("PLIRON_FOLDF"), s_foldf),
    cstep("celim", "celim", Gate::Env("PLIRON_CELIM"), s_celim).needs(shape::CONDBR),
    cstep(
        "ivrefold",
        "ivrefold",
        Gate::Env("PLIRON_IVREFOLD"),
        s_ivrefold,
    )
    .needs(shape::LOOPS),
    cstep("deadrec", "deadrec", Gate::Env("PLIRON_DEADREC"), s_deadrec).needs(shape::LOOPS),
    cstep("lcse", "lcse", Gate::Env("PLIRON_LCSE"), s_lcse),
    cstep("dgvn", "dgvn", Gate::Env("PLIRON_DGVN"), s_dgvn),
    cstep(
        "sameargs",
        "sameargs",
        Gate::Env("PLIRON_SAMEARG"),
        s_sameargs,
    ),
    // `load (select)` speculation once the compare-exchange diamonds
    // are selects (fuse/sameargs ran) — early enough that loadfwd2 can
    // forward the network's own stores into the fresh arm loads.
    step("loadsel", "loadsel", Gate::Env("PLIRON_LDSEL"), s_loadsel).needs(shape::MEM),
    step(
        "loadfwd2",
        "loadfwd2",
        Gate::Flag(FlagId::Loadfwd),
        s_loadfwd2,
    )
    .needs(shape::MEM),
    step("mask01", "mask01", Gate::Env("PLIRON_MASK01"), s_mask01),
    cstep(
        "edgespec",
        "edgespec",
        Gate::Env("PLIRON_EDGESPEC"),
        s_edgespec,
    ),
    cstep("bittab", "bittab", Gate::Env("PLIRON_BITTAB"), s_bittab).needs(shape::CONDBR),
    step(
        "coldedge",
        "coldedge",
        Gate::Env("PLIRON_COLDEDGE"),
        s_coldedge,
    ),
    cstep("dse2", "dse2", Gate::Env("PLIRON_DSE2"), s_dse2).needs(shape::MEM),
    step("vecslot", "vecslot", Gate::Env("PLIRON_VECSLOT"), s_vecslot).needs(shape::MEM),
    // Late value numbering: the edge transforms and vecslot leave
    // identical insts in one block; also drives the mem-offset and
    // neg folds on late-introduced addressing.
    Step {
        name: "lcse2",
        bisect: Some("lcse"),
        stat: "lcse",
        verify: true,
        mutates: true,
        cached: false,
        shape: 0,
        gate: Gate::Env("PLIRON_LCSE"),
        run: s_lcse,
    },
    step("uload32", "uload32", Gate::Env("PLIRON_ULOAD32"), s_uload32).needs(shape::MEM),
    // `icmp p, K` count latches become `icmp p+s, K+s` — the test folds
    // into the already-computed step value (one-register `add;cmp;jcc`)
    // and leaves boundcmp's post-inc shape behind.
    step("postinc", "postinc", Gate::Env("PLIRON_POSTINC"), s_postinc).needs(shape::LOOPS),
    step("boundcmp", "boundcmp", Gate::Env("PLIRON_BCMP"), s_boundcmp).needs(shape::LOOPS),
    step("selfarg", "selfarg", Gate::Env("PLIRON_SELFARG"), s_selfarg),
    step("brchain", "brchain", Gate::Env("PLIRON_BRCHAIN"), s_brchain).needs(shape::SWITCH),
    // Sink `splat(const)` defs into their use blocks: a cross-region live
    // splat becomes a spill reload per loop iteration; a local one is a
    // single `movi`/`dup` at the use.
    step("splink", "splink", Gate::Env("PLIRON_SPLINK"), s_splink),
    // Flag-merge brif blocks (a bool merged through a param, then tested)
    // only settle after edgespec/celim — dup them onto const edges here too.
    step("taildup2", "", Gate::Flag(FlagId::Taildup), s_taildup).needs(shape::CONDBR),
    // Late dispatch-block duplication + edge re-spec: collapses the
    // `select -> ireduce -> icmp -> brif` dispatch tail onto a direct brif
    // on the flag (tokenize's block388/block859 discriminant chain —
    // only pays off once jtround can repair multi-pred targets, so this
    // sits right before the looping jtround).
    step("mergechain", "", Gate::Env("PLIRON_LATESPEC"), s_mergechain).needs(shape::CONDBR),
    step(
        "jtround",
        "threaded",
        Gate::Env("PLIRON_JTROUND"),
        s_jtround,
    )
    .needs(shape::CONDBR),
    cstep("slotmerge", "", Gate::Env("PLIRON_SLOTMERGE"), s_slotmerge).needs(shape::MEM),
    step("memcall", "memcall", Gate::Env("PLIRON_MEMCALL"), s_memcall).needs(shape::CALL),
    // Fold constants any earlier pass left behind — late value folds
    // (dgvn/sameargs) can materialize `iconst`s under `ineg`/`ushr`/
    // `ishl` chains the early clifpeep round never saw (matmul's
    // `((-(-216))>>3)<<6` vector-trip bound recomputed per iteration).
    step("csweep2", "", Gate::Env("PLIRON_CSWEEP2"), s_csweep2),
    // `jtround`/`mergechain` threading leaves `isub(isub e,a),(isub e,b)`
    // telescopes on the new edges (`end-front` vs `end-ptr'` lengths —
    // the token-len recomputes in tokenize's block303). lcse2 already
    // ran, so one more local pass here.
    Step {
        name: "lcse3",
        bisect: Some("lcse"),
        stat: "lcse",
        verify: true,
        mutates: true,
        cached: false,
        shape: 0,
        gate: Gate::Env("PLIRON_LCSE"),
        run: s_lcse,
    },
    // Late uniform-param drop: mergechain/jtround retarget edges past
    // shared join blocks, and the remaining edges often agree on an
    // arg that was non-uniform when sameargs first ran (chars_alpha's
    // Option<char> flag: exits retargeted away, all survivors pass 1).
    step(
        "sameargs2",
        "sameargs",
        Gate::All(&[Gate::Env("PLIRON_SAMEARG"), Gate::Env("PLIRON_SAMEARG2")]),
        s_sameargs,
    ),
    // (The old `arghoist` position was a second webmerge call right
    // after sameargs2's fixpoint — webmerge now hoists same-shape edge
    // args via phase-B `P`-keys, so nothing was left for it.)
    step(
        "paramsink",
        "paramsink",
        Gate::Env("PLIRON_PARAMSINK"),
        s_paramsink,
    ),
    // paramsink re-bases count latches to `icmp p, K` — re-point the
    // test at the `p + s` edge arg so the latch lowers to `add;cmp;jcc`
    // on one register.
    step(
        "postinc2",
        "postinc",
        Gate::Env("PLIRON_POSTINC"),
        s_postinc,
    )
    .needs(shape::LOOPS),
    // (`relparm`'s symbolic-gap pairs — tokenize's `pos` = `cur - base`
    // — are phase-D relational merges; the later sameargs3/webmerge
    // positions cover anything this extra call found.)
    // `icmp CC (select c,K,x), K` folds to `c`/`!c` when the live arm's
    // compare is provable — a pure fold, so it gets first crack at the
    // niche dispatch; `seltest` below handles the unprovable cases by
    // splitting the branch.
    step("nfold", "niche", Gate::Env("PLIRON_NFOLD"), s_niche),
    step("selargs", "selargs", Gate::Env("PLIRON_SELARGS"), s_selargs),
    // `brif (icmp (select c,K,x), K)` niche dispatch → `brif c` +
    // `icmp x,K` middle block — every edge knows `c`'s side, so the
    // `select c,·,·` args fold to arms and the cmov cluster dies
    // (`Chars::next`/`split_terminator` per-element cost). Runs late:
    // the select form only exists after ifconv/merge retargeting.
    step("seltest", "seltest", Gate::Env("PLIRON_SELTEST"), s_seltest).needs(shape::CONDBR),
    // `(x >= lo) && (x < hi)` merged to `(x - lo) <u w` — select/band
    // scalar forms plus loopvec's `bitselect(masked cmp, cmp, 0)` —
    // after sameargs folds the bounds to `iconst`/`splat(iconst)` the
    // merge becomes one `icmp` per element instead of cmp+cmp+select
    // (or two `pcmpgtd` + mask ops per SIMD group).
    step("rgmerge", "rgmerge", Gate::Env("PLIRON_RGPAIR"), s_rgmerge),
    // selargs/seltest resolve the niche selects in edge args, exposing
    // affine relations (`next = cur + 1` cursor pairs) that were opaque
    // to affparm at sameargs2 — one more round drops them, shrinking
    // the per-iteration edge copies in decode loops.
    step(
        "sameargs3",
        "sameargs",
        Gate::All(&[Gate::Env("PLIRON_SAMEARG"), Gate::Env("PLIRON_SAMEARG3")]),
        s_sameargs,
    ),
    // Late dead-param drop: sameargs/paramsink retargeting can close a
    // param-flow cycle that was live at jtround time (tokenize's token
    // (start,end) pair — `count()` discards it, so the four-block
    // carry chain is pure cargo for regalloc).
    Step {
        name: "deadp",
        bisect: Some("deadp"),
        stat: "deadp",
        verify: true,
        mutates: true,
        cached: false,
        shape: 0,
        gate: Gate::Env("PLIRON_DEADP"),
        run: |p, _an| jumpthread::remove_dead_params(&mut p.clctx.func),
    },
    // Phi merge through affine congruence: every in-edge of a param
    // carries the same `root + k` class (`proots::affine_nf` — Ascent),
    // so distinct SSA values that are transposed copies of one carrier
    // still merge. Catches the shapes `taildup`'s per-edge clones and
    // jumpthread's copied `iadd`s leave behind.
    step("webmerge", "webmerge", Gate::Env("PLIRON_WEBMERGE"), s_webmerge)
        .needs(shape::MULTI),
    // Interval-test chains `[lo,hi) → (x-lo u<w)` and same-width
    // range unions `(x|d)-k_hi u<w` — rustc's char classification
    // (a-z|A-Z) lands as a 4-branch chain; the offset form is one
    // branch each and the union merges the pair, LLVM's `c|0x20`.
    step(
        "rangefold",
        "rangefold",
        Gate::Env("PLIRON_RANGEFOLD"),
        s_rangefold,
    )
    .needs(shape::CONDBR),
    // One-bit compares feeding a `brif` — `byte <u 0x80` (the UTF-8
    // ASCII/continuation test in tokenize/chars_*), `x <s 0` — become
    // `brif (band v, 1<<b)` (targets swapped for the clear-polarity
    // case), which isel fuses into a single `tbnz`/`tbz` on aarch64 and
    // `test`+`jcc` on x64. Runs last: the `icmp` form is canonical for
    // every earlier pass.
    step("sgntest", "sgntest", Gate::Env("PLIRON_SGNTEST"), s_sgntest).needs(shape::CONDBR),
    // `jump`-edge continuations whose param-bound loads a pred just
    // stored (or_insert's `*v += 1` reading its own `0`) get cloned onto
    // the edge so lcse4 can fold the RMW to a direct store.
    step("memspec", "memspec", Gate::Env("PLIRON_MEMSPEC"), s_memspec).needs(shape::MEM),
    // sameargs3/paramsink substitute edge args into isub uses, leaving
    // `isub (iadd x, k), x` cursor deltas that only now have visible iadd
    // defs (tokenize's per-path byte lengths) — one more local fold.
    Step {
        name: "lcse4",
        bisect: Some("lcse"),
        stat: "lcse",
        verify: true,
        mutates: true,
        cached: false,
        shape: 0,
        gate: Gate::Env("PLIRON_LCSE"),
        run: s_lcse,
    },
    // Third DSE: the late edge/jtr cleanup can delete a slot's last
    // readers after `dse2` ran (SipHash's `&key` tail-byte loads —
    // hashmap's per-iteration `x` store dies only once the tail branch
    // structure resolves) — the orphaned store would otherwise reach
    // codegen as a stack write per loop iteration.
    cstep("dse3", "dse3", Gate::Env("PLIRON_DSE3"), s_dse3).needs(shape::MEM),
    // Loop-latch merges (`brif …, tail, …` where `tail` loops back) only
    // shrink under MAX_INSTS after the late folds — clone them last so the
    // loop's hot tail lands on the fallthrough edge instead of a `jmp`
    // per iteration (chars_*/tokenize dispatch).
    step("taildup3", "", Gate::Flag(FlagId::Taildup), s_taildup).needs(shape::CONDBR),
    // taildup's per-edge clones leave congruent `iadd`/`isub` args on a
    // shared successor — one more merge round after the last clone step.
    step(
        "webmerge2",
        "webmerge",
        Gate::Env("PLIRON_WEBMERGE"),
        s_webmerge,
    )
    .needs(shape::MULTI),
    Step {
        name: "coldret",
        bisect: Some("coldret"),
        stat: "coldret",
        verify: false,
        mutates: true,
        cached: false,
        shape: 0,
        gate: Gate::Env("PLIRON_COLDRET"),
        run: s_coldret,
    },
];


#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::isa::TargetIsa;
    use cranelift_codegen::settings;
    use rustc_hash::{FxHashMap, FxHashSet};

    fn host_isa() -> std::sync::Arc<dyn TargetIsa> {
        let flags = settings::Flags::new(settings::builder());
        cranelift_codegen::isa::lookup(target_lexicon::Triple::host())
            .expect("host ISA")
            .finish(flags)
            .expect("ISA flags")
    }

    /// `v1 = call f(v0)`; `block1` takes `v1` as its param — the shape a
    /// promoted load leaves behind when only one guarded path reaches
    /// it. `v2` is a second result for the dedup-carry case.
    const OPT_PTR: &str = "function %f(i64) -> i64 {
        sig0 = (i64) -> i64
        fn0 = %g sig0
    block0(v0: i64):
        v1 = call fn0(v0)
        v2 = iconst.i64 7
        v5 = iconst.i64 0
        v3 = icmp eq v0, v5
        brif v3, block0(v1), block1(v1)
    block1(v4: i64):
        return v4
    }";

    /// A block param's `nonnull` is true only on paths into its block;
    /// a uniform-feed merge (`pmap[param] = carrier`) makes the carrier
    /// the param's value *at those merge points*, not on paths that
    /// skip the block — so the fact must not spread to the carrier
    /// (`serde_derive`'s `Attr::get`: jtround folded the call result's
    /// null check and threaded `None` into `Some` code). Result→result
    /// dedups are universal equalities and still carry.
    #[test]
    fn carry_keeps_param_facts_scoped() {
        let func = cranelift_reader::parse_functions(OPT_PTR).unwrap().remove(0);
        let v = |n: u32| Value::from_u32(n);
        let (v1, v2, v4) = (v(1), v(2), v(4));
        assert!(matches!(func.dfg.value_def(v1), ValueDef::Result(..)));
        assert!(matches!(func.dfg.value_def(v2), ValueDef::Result(..)));
        assert!(matches!(func.dfg.value_def(v4), ValueDef::Param(..)));

        let isa = host_isa();
        let mut clctx = Context::for_function(func);
        let nowrite: FxHashMap<FuncRef, bool> = Default::default();
        let call_frozen: FxHashMap<FuncRef, u64> = Default::default();
        let scoped: FxHashMap<Inst, Ptr<BasicBlock>> = Default::default();
        let invariant: FxHashSet<Inst> = Default::default();
        let block_region: FxHashMap<Block, Ptr<BasicBlock>> = Default::default();
        let prof: FxHashMap<Block, u64> = Default::default();
        let region_parent: FxHashMap<Ptr<BasicBlock>, Ptr<BasicBlock>> = Default::default();
        let nocap: FxHashSet<(Inst, u32)> = Default::default();
        let mut nonnull = FxHashSet::default();
        nonnull.insert(v4);
        let mut derived = FxHashMap::default();
        let mut frozen = FxHashMap::default();
        let mut noalias = FxHashSet::default();
        let mut deref = FxHashMap::default();
        let mut operand_frozen = FxHashMap::default();
        let mut lifetimes = Vec::new();
        let mut stats = Stats::default();
        let flags = FnFlags::default();
        let mut p = Pipe {
            clctx: &mut clctx,
            isa: &*isa,
            fname: "carry_keeps_param_facts_scoped",
            func_id: FuncId::from_u32(0),
            nonnull: &mut nonnull,
            derived: &mut derived,
            frozen: &mut frozen,
            noalias: &mut noalias,
            deref: &mut deref,
            nowrite: &nowrite,
            call_frozen: &call_frozen,
            scoped: &scoped,
            invariant: &invariant,
            operand_frozen: &mut operand_frozen,
            block_region: &block_region,
            prof: &prof,
            region_parent: &region_parent,
            lifetimes: &mut lifetimes,
            nocap: &nocap,
            tcfg: isa.frontend_config(),
            simd: true,
            x64: false,
            cmp64: true,
            has_eh: false,
            flags: &flags,
            prof_hook: None,
            dump: false,
            verify_full: false,
            stats: &mut stats,
            carry_log: Vec::new(),
            cur_step: "",
        };
        // The buggy pair: a param's guarded nonnull onto the wider-scope
        // call result — must not transfer (different blocks: v4's site
        // doesn't cover v1's def).
        carry_one(&mut p, v4, v1);
        assert!(
            !p.nonnull.contains(&v1),
            "param-scoped nonnull poisoned the merge carrier"
        );
        // Dedup carries still work: a result's mark reaches its
        // later-defined twin in the same block.
        p.nonnull.insert(v1);
        carry_one(&mut p, v1, v2);
        assert!(
            p.nonnull.contains(&v2),
            "same-block result->result carry dropped a covered fact"
        );
    }
}
