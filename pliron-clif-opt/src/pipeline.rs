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
use cranelift_codegen::entity::{EntityRef, SecondaryMap};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, FuncRef, Function, Inst, StackSlot, Value, ValueDef};
use cranelift_codegen::isa::{TargetFrontendConfig, TargetIsa};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use cranelift_module::FuncId;
use pliron::basic_block::BasicBlock;
use pliron::context::Ptr;
use crate::vmap::VMap;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::config::{bisect, bisect_count, config, env_set, reads_begin, reads_take, trace};
use crate::*;

/// Widest x64 vector register class the backend may use (128/256/512).
/// Mirrors the vendored `isa::x64::WIDE_VECTOR_BITS` — that static lives
/// behind cranelift's `x86` feature, which this noarch crate doesn't
/// build. The backend stores the same value into both (src/lib.rs).
pub static X64_WIDE_VECTOR_BITS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(128);

pub fn x64_wide_bits() -> u32 {
    X64_WIDE_VECTOR_BITS.load(std::sync::atomic::Ordering::Relaxed)
}

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
    /// `-g -O0`: slotprom leaves variable (keyed) slots in memory.
    pub keep_dbg_slots: bool,
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
    /// How often the shared `Analysis` slot rebuilt each structural
    /// analysis — the granularity-split scoreboard.
    pub cfg_builds: usize,
    pub dt_builds: usize,
    pub la_builds: usize,
    pub cfg_ns: u64,
    pub dt_ns: u64,
    pub la_ns: u64,
}

/// PLIRON_MCPROF instrumentation: ns spent in stepcache pipeline
/// lookup/store across all threads (temporary diagnostics).
pub static MC_LOOKUP_T: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MC_STORE_T: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
        self.cfg_builds += o.cfg_builds;
        self.dt_builds += o.dt_builds;
        self.la_builds += o.la_builds;
        self.cfg_ns += o.cfg_ns;
        self.dt_ns += o.dt_ns;
        self.la_ns += o.la_ns;
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
    pub derived: &'a mut VMap<Value>,
    pub frozen: &'a mut FxHashMap<Value, u64>,
    pub noalias: &'a mut FxHashSet<Value>,
    /// The subset of `noalias` whose claim is region-scoped (clone args
    /// seeded from `region_frozen`/`region_noalias`): valid only inside
    /// the clone's region, so `isolated*` must not treat them as
    /// function-wide unreachable.
    pub scoped_na: &'a mut FxHashSet<Value>,
    pub deref: &'a mut FxHashMap<Value, (u64, bool)>,
    pub nowrite: &'a FxHashMap<FuncRef, bool>,
    pub call_frozen: &'a FxHashMap<FuncRef, u64>,
    /// Callees that can't unwind; any other plain call may unwind to the
    /// caller, exposing memory the caller can see (dse).
    pub nounwind: &'a FxHashSet<FuncRef>,
    pub scoped: &'a FxHashMap<Inst, Ptr<BasicBlock>>,
    pub invariant: &'a FxHashSet<Inst>,
    pub operand_frozen: &'a mut FxHashMap<Value, Vec<(u64, Ptr<BasicBlock>)>>,
    /// Dense "value may carry a fact" bitmask (`F_*`) — one
    /// `SecondaryMap` probe in `carry_one` gates the six hash-table
    /// lookups. Bits are only ever set, never cleared: a stale bit costs
    /// one wasted probe against a table that decides anyway.
    pub fact_bits: &'a mut SecondaryMap<Value, u8>,
    pub block_region: &'a FxHashMap<Block, Ptr<BasicBlock>>,
    /// PGO: CLIF block -> executed count (empty without PLIRON_PGO_USE).
    /// Loop passes scale budgets off it; `block_count` is the read path.
    pub prof: &'a FxHashMap<Block, u64>,
    pub region_parent: &'a pliron::cols::ColMap<Ptr<BasicBlock>, Ptr<BasicBlock>>,
    /// Each lowered instruction's region (from `block_region`), taken when
    /// the pipeline starts. Block merges keep instruction identity but not
    /// block identity, so consumers derive block regions from this
    /// (`live_block_region`) instead of the stale `block_region`.
    pub inst_region: FxHashMap<Inst, Ptr<BasicBlock>>,
    /// `llvm.lifetime.{start,end}` markers as (slots, anchor inst,
    /// block, is_start). The operand pointer is resolved to slots at
    /// lowering/`Facts` time — by slotmerge's point in the pipeline the
    /// operand `Value` is typically dead, so it can't ride along.
    pub lifetimes: &'a mut Vec<(Vec<StackSlot>, Option<Inst>, Block, bool)>,
    pub nocap: &'a FxHashSet<(Inst, u32)>,
    /// `-g`: variable slots' pieces (`dbglabel`), for slotprom.
    pub dbg_slots: &'a crate::dbglabel::DbgSlots,
    pub tcfg: TargetFrontendConfig,
    pub simd: bool,
    pub x64: bool,
    pub cmp64: bool,
    /// The code emitter folds `base + reg*{1,2,4,8}` into the load's
    /// addressing mode (x86 SIB, aarch64 extended-register). False on
    /// wasm, where the bookkeeping ISA is the host's but the waffle
    /// output computes every address explicitly — indvars then wants
    /// stepped pointer IVs rather than leaving `base + iv*s` inside.
    pub amode_scale: bool,
    /// Emit goes through waffle to wasm rather than a register-allocated
    /// ISA — passes whose only payoff is shaping code for regalloc
    /// (`coldarg`'s cold-site arg rebinding) have nothing to fix.
    pub wasm: bool,
    pub has_eh: bool,
    pub flags: &'a FnFlags,
    /// `PLIRON_CLIF=<substr>` matched this function's name.
    pub dump: bool,
    /// `PLIRON_VERIFY=full` (or the verify-failure retry pass).
    pub verify_full: bool,
    /// `State::dead_unopt` marked this fn: callerless `xcgu_copies`
    /// member — emit with only the `Gate::Always` correctness
    /// lowerings (same bail class as `giant_max`).
    pub unopt: bool,
    /// A mutating step reported edits since `iconst128`'s reap last
    /// ran. `post_sweep`'s whole-function DCE+fold walks are gated on
    /// this — with nothing mutated they can only re-scan clean IR.
    pub since_reap: bool,
    pub stats: &'a mut Stats,
    /// Every `(d, w)` pair `carry_one` applied, in order — the
    /// stepcache's fact-delta record (replaying the pairs reproduces
    /// the inserts and lifetime rewrites exactly).
    pub carry_log: Vec<(Value, Value)>,
    /// Module-local `FuncId` index -> declared symbol name, covering
    /// every `UserExternalName{namespace:0, index}` this fn's params
    /// reference. The step cache hashes those indices by symbol so
    /// identical bodies in different CGUs (different local numbering)
    /// share keys, and remaps a replayed variant into the consumer's
    /// numbering. Empty outside a module context (clif-check, unit
    /// tests) or when symbols can't be resolved — variants then carry
    /// no names and only apply where the numbering is shared.
    pub uen_syms: &'a FxHashMap<u32, String>,
    /// Step currently executing, for debug attribution prints.
    pub cur_step: &'static str,
    /// Self-profile hook wired by `src/` to rustc's `sess.prof`: calling
    /// it starts a `generic_activity` event for the step; the returned
    /// guard records the interval when it drops. `None` in tests.
    /// Type-erased so this crate stays free of `rustc_private`.
    pub prof_hook: Option<&'a (dyn Fn(&'static str) -> Box<dyn Send + 'a> + 'a)>,
    /// Module-wide FuncId-indexed symbol table, for `PLIRON_CORPUS` name
    /// maps. Empty outside the real backend.
    pub fn_names: &'a [String],
}

impl Pipe<'_> {
    pub(crate) fn dump(&self, tag: &str) {
        if self.dump {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::dump", "==== clif {} after {tag} ====\n{}",
                    self.fname,
                    self.clctx.func.display()
                )
            } else {
                // Non-rustc embedders (cubecl's JIT) install no tracing
                // subscriber — write the dump straight to stderr.
                eprintln!(
                    "==== clif {} after {tag} ====\n{}",
                    self.fname,
                    self.clctx.func.display()
                );
            }
        }
    }
    fn log(&self, label: &str, k: usize) {
        if k > 0
            && crate::config::tracing_on()
            && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
        {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::stats", "{label} {k} {}", self.fname)
            };
        }
    }
}

static CLIF_STEPS: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| pliron_cfg::env_is("PLIRON_CLIF_STEPS", "1"));

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
    /// A non-entry block declares at least one block param — the web/
    /// param transforms (`sameargs`/`webmerge`/`cparm`, jumpthread's
    /// `trivp`/`deadp`) only move edge args into and out of params; a
    /// function with none has nothing to merge or drop. Unreachable
    /// param'd blocks count: `deadp` cleans them too.
    pub const BPARAM: u32 = 1 << 8;
    /// An unconditional `jump` terminator exists — chain merging,
    /// forwarder bypass/folding, and the jump-plan paths only inspect
    /// `jump` predecessors.
    pub const JUMP: u32 = 1 << 9;
    /// At least one load-family inst — loadfwd's dataflow only ever
    /// produces forwards for loads; a load-free function can only have
    /// kills.
    pub const LOAD: u32 = 1 << 10;
    /// At least one store-family inst — DSE and dead-slot-store
    /// sweeps only remove stores.
    pub const STORE: u32 = 1 << 11;
    /// An `icmp` exists — `fold_null_tests`' null/sentinel folds all
    /// start from one (its `known()` operand test is finer, but the
    /// bit deliberately skips the per-arg walk).
    pub const TESTC: u32 = 1 << 12;
    /// More than one block — tail merge/dup, unreachable-block sweeps,
    /// and cross-block def-use passes need a second block to act on.
    pub const BLOCKS2: u32 = 1 << 13;
    /// A materialized constant exists (`iconst`/`f32const`/`f64const`/
    /// `f128const`/`vconst`) — `const_sweep`'s `eval`/`known` seeds and
    /// `constdedup`'s merge keys.
    pub const ICONST: u32 = 1 << 14;
    /// A stack slot is declared — `slotmerge` can merge slots no
    /// instruction references (dead decls carry no `MEM` inst).
    pub const SLOT: u32 = 1 << 15;
    /// The entry block has params (i.e. the function takes args) —
    /// `edgespec`'s `deadbits` zeroes demandless uses of any param,
    /// entry args included.
    pub const EPARAM: u32 = 1 << 16;

    /// Bits an ordinary mutating step may introduce where none existed:
    /// edge shape (const-branch folds create `jump`s, block clones and
    /// rewrites create edges/blocks/back-edges), block params (clone
    /// or substitution paths), and constants (`iconst` materialization
    /// — which can in turn make an existing `icmp` a `TESTC` case).
    /// The inst-kind bits (`LOAD`/`STORE`/`MEM`/`CALL`/`SLOT`/`SLP`/
    /// `DIVREM`/`SWITCH`) and `EPARAM` are absent: cloning or rewriting
    /// can't produce an instruction kind the function never had, and the
    /// signature is fixed. Steps that *do* materialize a new inst kind
    /// (`slotprom`'s stack accesses, `loopidiom`/`clifpeep`'s libcalls,
    /// vectorizers) declare `creates_all` instead.
    pub const CREATED: u32 = MULTI | LOOPS | CONDBR | BPARAM | JUMP | TESTC | BLOCKS2 | ICONST;
    /// Every settable bit — `shape_of` early-exits its scan once all
    /// are proven, which happens quickly on large functions.
    pub const ALL: u32 = MULTI
        | LOOPS
        | CONDBR
        | SWITCH
        | MEM
        | CALL
        | SLP
        | DIVREM
        | BPARAM
        | JUMP
        | LOAD
        | STORE
        | TESTC
        | BLOCKS2
        | ICONST
        | SLOT
        | EPARAM;
}

/// Computes the `shape` bits in one pass over the function.
pub(crate) fn shape_of(func: &Function) -> u32 {
    use cranelift_codegen::ir::Opcode as Op;
    let mut bits = 0u32;
    if func.layout.num_blocks() > 1 {
        bits |= shape::BLOCKS2;
    }
    if !func.sized_stack_slots.is_empty() || !func.dynamic_stack_slots.is_empty() {
        bits |= shape::SLOT;
    }
    let entry = func.layout.entry_block();
    if entry.is_some_and(|e| func.dfg.num_block_params(e) > 0) {
        bits |= shape::EPARAM;
    }
    // Layout-order index per block for the back-edge test — dense by
    // block index, not hashed.
    let mut idx = vec![0u32; func.dfg.num_blocks()];
    for (i, b) in func.layout.blocks().enumerate() {
        idx[b.index()] = i as u32;
    }
    for (i, b) in func.layout.blocks().enumerate() {
        if bits & shape::ALL == shape::ALL {
            break;
        }
        if Some(b) != entry && func.dfg.num_block_params(b) > 0 {
            bits |= shape::BPARAM;
        }
        // (base, elem bytes) → store count: a pair in one group is the
        // cheap necessary shape for every store-pack path. Lazy: most
        // blocks have no stores at all, and two hashmaps per block was
        // ~10k empty allocs on megafunctions.
        let mut stores: Option<FxHashMap<(Value, u32), u32>> = None;
        // Same-op float-inst count → xpack op-pair seed. Integer dups are
        // excluded on purpose: two `iadd`s in a block is ambient pointer
        // arithmetic (every address computation), while float dups are
        // the unrolled-math shape xpack actually packs.
        let mut vecops: Option<FxHashMap<Op, u32>> = None;
        for inst in func.layout.block_insts(b) {
            let op = func.dfg.insts[inst].opcode();
            // `stack_addr` reports neither load nor store but marks a
            // slot the memory passes walk (slotprom/slotmerge).
            if op.can_load() {
                bits |= shape::MEM | shape::LOAD;
            }
            if op.can_store() {
                bits |= shape::MEM | shape::STORE;
            }
            if op == Op::StackAddr {
                bits |= shape::MEM;
            }
            if op.is_call() {
                bits |= shape::CALL;
            }
            match op {
                Op::Brif => bits |= shape::CONDBR,
                Op::BrTable => bits |= shape::CONDBR | shape::SWITCH,
                Op::Jump => bits |= shape::JUMP,
                Op::Sdiv | Op::Udiv | Op::Srem | Op::Urem => bits |= shape::DIVREM,
                // Any materialized constant — `constdedup` keys on the
                // float forms too, so the bit covers all of them (a
                // wider bit only ever makes a `needs` gate more
                // conservative, never unsound).
                Op::Iconst | Op::F32const | Op::F64const | Op::F128const | Op::Vconst => {
                    bits |= shape::ICONST
                }
                // `fold_null_tests` fires only on `icmp` tests where
                // `known()` resolves an operand to an `iconst` — any
                // `icmp` over-reports (no per-arg walk in `shape_of`)
                // but stays a sound necessary condition.
                Op::Icmp => bits |= shape::TESTC,
                Op::Store if bits & shape::SLP == 0 => {
                    let fl = func.dfg.mem_flags[func.dfg.insts[inst].memflags().unwrap()];
                    let args = func.dfg.inst_args(inst);
                    let val = func.dfg.resolve_aliases(args[0]);
                    let ty = func.dfg.value_type(val);
                    if fl.notrap() && !ty.is_vector() && matches!(ty.bytes(), 1 | 2 | 4 | 8) {
                        let base = crate::slp::base_off(func, args[1]).0;
                        let c = stores
                            .get_or_insert_with(FxHashMap::default)
                            .entry((base, ty.bytes()))
                            .or_insert(0);
                        *c += 1;
                        if *c >= 2 {
                            bits |= shape::SLP;
                        }
                    }
                }
                _ => {}
            }
            match op {
                Op::Fadd
                | Op::Fsub
                | Op::Fmul
                | Op::Fdiv
                | Op::Fmin
                | Op::Fmax
                | Op::Fneg
                | Op::Fabs
                | Op::Sqrt
                    if bits & shape::SLP == 0 =>
                {
                    let c = vecops
                        .get_or_insert_with(FxHashMap::default)
                        .entry(op)
                        .or_insert(0);
                    *c += 1;
                    if *c >= 2 {
                        bits |= shape::SLP;
                    }
                }
                _ => {}
            }
            // Vector-typed results (a function loopvec already rewrote)
            // leave scalar leftovers worth packing.
            if bits & shape::SLP == 0
                && func
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
                    if idx[bc.block(pool).index()] <= i as u32 {
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

/// Hash of everything the structural analyses read: the layout block
/// sequence, each block's terminator identity, and its successors in
/// written order (jump-table and exception-table destinations
/// included). `O(blocks + edges)` to compute — far cheaper than the
/// bforest inserts `ControlFlowGraph::compute` pays per edge — so
/// `bump`/`invalidate` use it to decide whether a mutation changed the
/// CFG at all, keeping cfg/dt/la alive across value-only rewrites.
fn edge_fp(func: &Function) -> u64 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    h.write_usize(func.dfg.num_blocks());
    for b in func.layout.blocks() {
        h.write_usize(b.index());
        if let Some(inst) = func.layout.last_inst(b) {
            h.write_usize(inst.index());
            for bc in func.dfg.insts[inst]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                h.write_usize(bc.block(&func.dfg.value_lists).index());
            }
        }
    }
    h.finish()
}

/// Version-keyed cache of the analyses passes share. Owned by
/// `run_pipeline` (outside `Pipe`) so an `&Analysis` result and
/// `&mut Function` can be borrowed at once.
///
/// Entries are dropped when the pipeline bumps `ver` after a mutating
/// step, and passes that mutate mid-step call `invalidate()` at the same
/// places they previously recomputed. The drop is granular: cfg/dt/la
/// survive a bump whose `edge_fp` is unchanged (the step rewrote only
/// values/args), while the value-keyed caches always drop. Accessors
/// also assert the fingerprint — a read after an un-`invalidate`d
/// structural mutation panics instead of silently serving stale data.
#[derive(Default)]
pub struct Analysis {
    ver: u64,
    /// `edge_fp` of the function state `cfg` describes; `Some` iff
    /// `cfg` is `Some`. Replaces the old `num_insts` fingerprint:
    /// inst count changes on value-only rewrites that leave the CFG
    /// intact, while this catches every edge/block/terminator change.
    cfg_fp: Option<u64>,
    /// `cfg`/`dt`/`la` may describe a pre-bump IR state — the edge
    /// fingerprint is checked at the next read, not per `bump`, so a
    /// run of mutating steps between cfg consumers pays one walk total.
    cfg_dirty: bool,
    cfg: Option<ControlFlowGraph>,
    dt: Option<DominatorTree>,
    la: Option<LoopAnalysis>,
    /// Recycled analysis shells. `clear` moves the live instances here
    /// so the next rebuild's `compute` reuses their backing storage
    /// (bforest node pools, postorder/worklist Vecs) instead of
    /// regrowing from empty — ~60 CFG/DT rebuilds per corpus function
    /// was a measurable allocator leaf.
    spare_cfg: Option<ControlFlowGraph>,
    spare_dt: Option<DominatorTree>,
    spare_la: Option<LoopAnalysis>,
    /// Materialization counts, merged into `Stats` at pipeline end.
    pub(crate) cfg_builds: usize,
    pub(crate) dt_builds: usize,
    pub(crate) la_builds: usize,
    pub(crate) cfg_ns: u64,
    pub(crate) dt_ns: u64,
    pub(crate) la_ns: u64,
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
    /// `shape_of` result OR'd with the `creates` mask of every mutating
    /// step run since the scan — a superset of the current bits, so a
    /// clear bit still certifies absence while a set bit may be stale
    /// (over-admission: harmless). Never rescanned; the whole point is
    /// that gate checks stay O(1) after the first one.
    fshape: Option<u32>,
    /// Alias pairs already carried onto their targets; facts are inputs
    /// and aliases are permanent, so a pair never needs carrying twice.
    carried: FxHashSet<(Value, Value)>,
    /// Position into `dfg.alias_writes()` covered by the last
    /// `carry_aliases` scan — the list is append-only, so equal lengths
    /// mean no new aliases exist and `wm..len` is exactly the backlog.
    alias_wm: usize,
    /// `dfg.alias_epoch` when `compact_aliases` last ran. Chains only
    /// deepen when an alias's target is itself aliased — an epoch bump —
    /// so equal epochs mean every chain is already flat and the
    /// whole-`values()` scan is dead work.
    compact_epoch: u32,
    /// `proots::alias_map_cfg` for the current `ver` — the param-alias
    /// relation every memory pass needs (dse runs three times).
    amap: Option<VMap<Value>>,
    /// `proots::facts_cfg` for the current `ver` — the edge facts that
    /// feed `amap`, `loadfwd`'s `bounds`/`pmroots`, and any other
    /// proots consumer in this version. `amap` derives from it.
    facts: Option<crate::proots::Facts>,
    /// `loadfwd::isolated` (empty-`nocap` variant) for the current `ver`,
    /// paired with the `(noalias.len(), scoped_na.len())` it was computed
    /// against: `carry_aliases` can grow both sets inside a version, and
    /// they never shrink.
    iso: Option<((usize, usize), FxHashSet<crate::loadfwd::Root>)>,
    /// `Uses` index shared between passes within a version — ~10 steps
    /// need one and each used to rebuild it per fn. `Rc` so callers get
    /// the same owned-snapshot semantics as a local `Uses::new` (stale
    /// after the caller's own mutations until refetched). Keyed on
    /// `(ver, num_insts)` — inst count changes on nearly every
    /// structural mutation — and dropped by `clear()` like the rest.
    /// `RefCell` so the accessor can be `&self`.
    uses: std::cell::RefCell<Option<(u64, usize, std::rc::Rc<crate::uses::Uses>)>>,
}

/// One borrow of the three structural analyses (`&self` tuple).
pub struct CfgDtLa<'a> {
    pub cfg: &'a ControlFlowGraph,
    pub dt: &'a DominatorTree,
    pub la: &'a LoopAnalysis,
}

impl Analysis {
    fn clear(&mut self) {
        self.cfg_fp = None;
        self.cfg_dirty = false;
        self.spare_cfg = self.cfg.take().or_else(|| self.spare_cfg.take());
        self.spare_dt = self.dt.take().or_else(|| self.spare_dt.take());
        self.spare_la = self.la.take().or_else(|| self.spare_la.take());
        self.clear_values();
    }

    /// Drop only the value-keyed caches — used when a mutation left the
    /// CFG edge/block structure provably intact (`edge_fp` match).
    fn clear_values(&mut self) {
        self.amap = None;
        self.facts = None;
        self.iso = None;
        self.uses.get_mut().take();
    }

    /// The pipeline advanced the IR version (a step declared `mutates`).
    /// When the cached CFG's edge fingerprint still matches — the step
    /// rewrote only values/args — cfg/dt/la stay valid and only the
    /// value-keyed caches drop. `creates` (the step's `Step::creates`)
    /// is OR'd into `fshape`: the bits are a superset of the current
    /// shape, so a bit the step could have produced stops certifying
    /// absence from here on.
    pub fn bump(&mut self, func: &Function, creates: u32) {
        let _ = func;
        self.ver += 1;
        if let Some(s) = &mut self.fshape {
            *s |= creates;
        }
        if self.cfg.is_some() {
            self.cfg_dirty = true;
            self.clear_values();
        } else {
            self.clear();
        }
    }

    /// A pass mutated the function mid-step; drop everything computed
    /// against the old state. Same granularity as `bump`: a value-only
    /// rewrite keeps cfg/dt/la (deferred — see `cfg_dirty`).
    pub fn invalidate(&mut self, func: &Function) {
        let _ = func;
        if self.cfg.is_some() {
            self.cfg_dirty = true;
            self.clear_values();
        } else {
            self.clear();
        }
    }

    /// Lazily fills the CFG only — `dt`/`la` build in their own
    /// accessors, so a cfg-only consumer doesn't pay for domtree+loops.
    fn materialize(&mut self, func: &Function) {
        if self.cfg_dirty {
            self.cfg_dirty = false;
            // Resolve the deferred edge fingerprint once: a match keeps
            // cfg/dt/la (value-only rewrites), a miss drops them to the
            // spare shells for rebuild below.
            if self.cfg.is_some() && self.cfg_fp != Some(edge_fp(func)) {
                self.cfg_fp = None;
                self.spare_cfg = self.cfg.take().or_else(|| self.spare_cfg.take());
                self.spare_dt = self.dt.take().or_else(|| self.spare_dt.take());
                self.spare_la = self.la.take().or_else(|| self.spare_la.take());
            }
        }
        if self.cfg.is_some() {
            debug_assert_eq!(
                self.cfg_fp,
                Some(edge_fp(func)),
                "analysis read after IR mutation without Analysis::invalidate()"
            );
        } else {
            self.cfg_fp = Some(edge_fp(func));
            let t = std::time::Instant::now();
            let mut cfg = self.spare_cfg.take().unwrap_or_else(ControlFlowGraph::new);
            cfg.compute(func);
            self.cfg = Some(cfg);
            self.cfg_ns += t.elapsed().as_nanos() as u64;
            self.cfg_builds += 1;
        }
    }

    /// Hand the pipeline's cfg/domtree to the Context at pipeline end.
    /// Materialize first — under `cfg_dirty` the cached graph may be
    /// stale and must be checked before it is exported.
    pub fn take_cfg_dt(
        &mut self,
        func: &Function,
    ) -> Option<(ControlFlowGraph, DominatorTree)> {
        self.materialize(func);
        if self.cfg.is_some() && self.dt.is_some() {
            Some((self.cfg.take().unwrap(), self.dt.take().unwrap()))
        } else {
            None
        }
    }

    /// Recompute whatever is missing, then borrow all three.
    pub fn cfg_dt_la<'a>(&'a mut self, func: &Function) -> CfgDtLa<'a> {
        self.materialize(func);
        if self.dt.is_none() {
            let t = std::time::Instant::now();
            let mut dt = self.spare_dt.take().unwrap_or_else(DominatorTree::new);
            dt.compute(func, self.cfg.as_ref().unwrap());
            self.dt = Some(dt);
            self.dt_ns += t.elapsed().as_nanos() as u64;
            self.dt_builds += 1;
        }
        if self.la.is_none() {
            let t = std::time::Instant::now();
            let mut la = self.spare_la.take().unwrap_or_else(LoopAnalysis::new);
            la.compute(func, self.cfg.as_ref().unwrap(), self.dt.as_ref().unwrap());
            self.la = Some(la);
            self.la_ns += t.elapsed().as_nanos() as u64;
            self.la_builds += 1;
        }
        CfgDtLa {
            cfg: self.cfg.as_ref().unwrap(),
            dt: self.dt.as_ref().unwrap(),
            la: self.la.as_ref().unwrap(),
        }
    }

    /// The cached domtree under a shared borrow. Caller must have
    /// materialized it first (`cfg_dt`/`cfg_dt_la`); lets `&self`
    /// consumers like `uses()` run while the borrow is held.
    pub fn domtree(&self) -> &DominatorTree {
        self.dt.as_ref().expect("Analysis::domtree before cfg_dt")
    }

    /// Recompute whatever is missing, then borrow cfg+domtree.
    pub fn cfg_dt<'a>(&'a mut self, func: &Function) -> (&'a ControlFlowGraph, &'a DominatorTree) {
        self.materialize(func);
        if self.dt.is_none() {
            let t = std::time::Instant::now();
            let mut dt = self.spare_dt.take().unwrap_or_else(DominatorTree::new);
            dt.compute(func, self.cfg.as_ref().unwrap());
            self.dt = Some(dt);
            self.dt_ns += t.elapsed().as_nanos() as u64;
            self.dt_builds += 1;
        }
        (self.cfg.as_ref().unwrap(), self.dt.as_ref().unwrap())
    }

    pub fn cfg<'a>(&'a mut self, func: &Function) -> &'a ControlFlowGraph {
        self.materialize(func);
        self.cfg.as_ref().unwrap()
    }

    /// The `Uses` index for the current `(ver, num_insts)` — shared so
    /// the steps that want a use map build it once per IR version
    /// instead of once each. The returned `Rc` is an owned snapshot
    /// like a local `Uses::new`: it does not track later mutation —
    /// refetch (or `refresh_uses`) at the points a local map would be
    /// rebuilt.
    pub fn uses(&self, func: &Function) -> std::rc::Rc<crate::uses::Uses> {
        let mut slot = self.uses.borrow_mut();
        let shape = func.dfg.num_insts();
        if slot
            .as_ref()
            .is_none_or(|&(v, s, _)| v != self.ver || s != shape)
        {
            *slot = Some((
                self.ver,
                shape,
                std::rc::Rc::new(crate::uses::Uses::new(func)),
            ));
        }
        slot.as_ref().unwrap().2.clone()
    }

    /// Rebuild `uses` after in-step mutation (the keyed shape may not
    /// have changed for pure value edits).
    pub fn refresh_uses(&self, func: &Function) -> std::rc::Rc<crate::uses::Uses> {
        self.uses.borrow_mut().take();
        self.uses(func)
    }

    /// Everything `dse::run` reads before mutating — `(cfg, param-src
    /// alias map, isolated roots)` — with the alias map and isolation
    /// set cached per `ver` so dse's three pipeline runs compute them at
    /// most once per IR state. The returned durations are the
    /// (possibly cache-hit) `amap`/`iso` build times, reported by
    /// `dseprof` as `ps`/`iso`.
    ///
    /// `iso` keys on `(noalias.len(), scoped_na.len())` in addition to
    /// `ver`: `carry_aliases` can grow both `Pipe` sets inside a version,
    /// and they never shrink.
    pub fn dse_inputs<'a>(
        &'a mut self,
        func: &Function,
        noalias: &FxHashSet<Value>,
        scoped_na: &FxHashSet<Value>,
        cfrozen: &FxHashMap<FuncRef, u64>,
    ) -> (
        &'a ControlFlowGraph,
        &'a VMap<Value>,
        &'a FxHashSet<crate::loadfwd::Root>,
        std::time::Duration,
        std::time::Duration,
    ) {
        // `t_ps`/`t_iso` only feed `dseprof` log lines — skip the clock
        // reads unless `pliron::dse` tracing is live.
        let prof = crate::config::tracing_on()
            && tracing::enabled!(target: "pliron::dse", tracing::Level::INFO);
        let t = prof.then(std::time::Instant::now);
        self.materialize(func);
        if self.facts.is_none() {
            self.facts = Some(crate::proots::facts_cfg(func, self.cfg.as_ref().unwrap()));
        }
        if self.amap.is_none() {
            self.amap = Some(crate::proots::alias_of(self.facts.as_ref().unwrap()));
        }
        let t_ps = t.map_or(std::time::Duration::ZERO, |t| t.elapsed());
        let t = prof.then(std::time::Instant::now);
        if self
            .iso
            .as_ref()
            .is_none_or(|&(n, _)| n != (noalias.len(), scoped_na.len()))
        {
            self.iso = Some((
                (noalias.len(), scoped_na.len()),
                crate::loadfwd::isolated(
                    func,
                    noalias,
                    self.amap.as_ref().unwrap(),
                    cfrozen,
                    scoped_na,
                ),
            ));
        }
        (
            self.cfg.as_ref().unwrap(),
            self.amap.as_ref().unwrap(),
            &self.iso.as_ref().unwrap().1,
            t_ps,
            t.map_or(std::time::Duration::ZERO, |t| t.elapsed()),
        )
    }

    /// `ver` counter — bumping it is how the driver invalidates the
    /// whole cache between steps.
    pub fn version(&self) -> u64 {
        self.ver
    }

    /// `loadfwd`'s entry inputs — `(cfg, edge facts, param-alias map)` —
    /// each computed at most once per `ver` (the facts are shared with
    /// `dse_inputs`' `amap` source). Snapshots: callers treat them like
    /// locals built at this point — stale after their own mutations.
    pub fn lf_inputs<'a>(
        &'a mut self,
        func: &Function,
    ) -> (
        &'a ControlFlowGraph,
        &'a crate::proots::Facts,
        &'a VMap<Value>,
    ) {
        self.materialize(func);
        if self.facts.is_none() {
            self.facts = Some(crate::proots::facts_cfg(func, self.cfg.as_ref().unwrap()));
        }
        if self.amap.is_none() {
            self.amap = Some(crate::proots::alias_of(self.facts.as_ref().unwrap()));
        }
        (
            self.cfg.as_ref().unwrap(),
            self.facts.as_ref().unwrap(),
            self.amap.as_ref().unwrap(),
        )
    }

    /// The function's `shape` bits as a superset: scanned lazily once,
    /// then widened by each mutating step's `creates` (see `bump`).
    /// Callers must treat set bits as "may be present" — only a clear
    /// bit certifies absence.
    pub fn shape_bits(&mut self, func: &Function) -> u32 {
        match self.fshape {
            Some(s) => s,
            None => {
                let s = shape_of(func);
                self.fshape = Some(s);
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
    /// `shape` bits of which at least one must be present — for steps
    /// whose sub-transforms need different shapes (e.g. `mergechain`
    /// merges `jump` chains or rewrites `brif` dispatches). Skipped
    /// only when every bit is absent.
    pub any: u32,
    /// `shape` bits this step may introduce where none existed, OR'd
    /// into the cached superset after the step runs (`shape::CREATED`
    /// for ordinary rewrite passes; `creates_all` for steps that
    /// materialize a whole instruction kind — see the const's comment).
    /// Under-declaring only over-skips later gated steps (missed
    /// optimizations), never miscompiles — `PLIRON_SHAPE_CHECK` catches
    /// it on the corpus.
    pub creates: u32,
    pub gate: Gate,
    pub run: fn(&mut Pipe, &mut Analysis) -> usize,
    /// Per-step `pliron::step` callsite, resolved once — `dyn_site`
    /// takes a global mutex per call, which every worker hit per step
    /// per function.
    pub site: std::sync::OnceLock<&'static crate::trace::DynSite>,
}

impl Step {
    /// `shape` requirement, builder-style: `step(..).needs(shape::LOOPS)`.
    const fn needs(mut self, mask: u32) -> Step {
        self.shape |= mask;
        self
    }
    /// At-least-one `shape` requirement: `step(..).needs_any(a | b)`.
    const fn needs_any(mut self, mask: u32) -> Step {
        self.any |= mask;
        self
    }
    /// The step may materialize inst kinds the function never had —
    /// every shape bit becomes possibly-present after it mutates.
    const fn creates_all(mut self) -> Step {
        self.creates = u32::MAX;
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
        any: 0,
        creates: shape::CREATED,
        gate,
        run,
        site: std::sync::OnceLock::new(),
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
        creates: shape::CREATED,
        any: 0,
        gate,
        run,
        site: std::sync::OnceLock::new(),
    }
}

pub fn run_pipeline(p: &mut Pipe) {
    corpus_dump(p);
    // `PLIRON_LIVEREGION=0` falls back to the lowering-time block regions.
    if !p.block_region.is_empty() && crate::pass_enabled("PLIRON_LIVEREGION") {
        let func = &p.clctx.func;
        let mut m = FxHashMap::default();
        for b in func.layout.blocks() {
            if let Some(&r) = p.block_region.get(&b) {
                for i in func.layout.block_insts(b) {
                    m.insert(i, r);
                }
            }
        }
        p.inst_region = m;
    }
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

/// Block regions for the function as it is *now*: a block's region is the
/// deepest region containing every instruction it holds (by `inst_region`).
/// A block that a CLIF pass merged across a region boundary
/// (an inlined `&self` helper's block fused with the caller's continuation)
/// gets the outer region, so the helper's frozen-operand claim no longer
/// covers the caller's code (uiua `Shape::extend_from_slice`: loadfwd kept
/// a SmallVec capacity across `try_reserve`, 2026-10-22). A side-effecting
/// instruction with no recorded region (created by a pass) leaves its block
/// without one; new pure ones are ignored.
pub fn live_block_region(p: &Pipe) -> FxHashMap<Block, Ptr<BasicBlock>> {
    if p.inst_region.is_empty() {
        // Off (`PLIRON_LIVEREGION=0`) or no regions: the static map.
        return p.block_region.clone();
    }
    live_regions(&p.clctx.func, &p.inst_region, p.region_parent)
}

/// `live_block_region` over explicit inputs (see there).
pub fn live_regions(
    func: &Function,
    inst_region: &FxHashMap<Inst, Ptr<BasicBlock>>,
    rpar: &pliron::cols::ColMap<Ptr<BasicBlock>, Ptr<BasicBlock>>,
) -> FxHashMap<Block, Ptr<BasicBlock>> {
    let mut out = FxHashMap::default();
    let lca = |a: Ptr<BasicBlock>, b: Ptr<BasicBlock>| -> Option<Ptr<BasicBlock>> {
        let mut anc = Vec::new();
        let mut x = Some(a);
        while let Some(r) = x {
            if anc.len() > 64 {
                break;
            }
            anc.push(r);
            x = rpar.get(r).copied().filter(|&q| q != r);
        }
        let mut y = Some(b);
        let mut n = 0;
        while let Some(r) = y {
            if anc.contains(&r) {
                return Some(r);
            }
            n += 1;
            if n > 64 {
                break;
            }
            y = rpar.get(r).copied().filter(|&q| q != r);
        }
        None
    };
    'blk: for b in func.layout.blocks() {
        let mut reg: Option<Ptr<BasicBlock>> = None;
        for i in func.layout.block_insts(b) {
            let Some(&r) = inst_region.get(&i) else {
                let op = func.dfg.insts[i].opcode();
                if op.is_call() || op.can_store() || op.other_side_effects() {
                    continue 'blk;
                }
                continue;
            };
            reg = match reg {
                None => Some(r),
                Some(q) => match lca(q, r) {
                    Some(q) => Some(q),
                    None => continue 'blk,
                },
            };
        }
        if let Some(r) = reg {
            out.insert(b, r);
        }
    }
    out
}

/// `PLIRON_CORPUS=<dir>` dumps each function's pre-pipeline CLIF (facts
/// header + text body) as `<dir>/<name>.<n>.clif`, giving `clif-check
/// bench` a real-world corpus so pipeline perf work iterates without
/// rustc in the loop.
fn corpus_dump(p: &mut Pipe) {
    use std::sync::OnceLock;
    static DIR: OnceLock<Option<String>> = OnceLock::new();
    let Some(dir) = DIR
        .get_or_init(|| crate::config::env_get("PLIRON_CORPUS").map(str::to_string))
        .as_deref()
    else {
        return;
    };
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let facts = crate::diffcheck::Facts {
        nonnull: Some(&*p.nonnull),
        noalias: Some(&*p.noalias),
        scoped_na: Some(&*p.scoped_na),
        deref: Some(&*p.deref),
        frozen: Some(&*p.frozen),
        nowrite: Some(p.nowrite),
        self_name: Some(p.fname.to_string()),
        derived: Some(&*p.derived),
    };
    // Extra replay facts `diffcheck-facts` can't express: the per-callee
    // nocapture arg positions (a `(Inst, arg)` set on `Pipe`) re-keyed by
    // funcref so they survive the round trip, and the FuncId -> symbol
    // map so replays can report real callee names.
    let f = &p.clctx.func;
    let mut nc: Vec<String> = Vec::new();
    for &(i, a) in p.nocap.iter() {
        use cranelift_codegen::ir::instructions::CallInfo;
        let info = f.dfg.insts[i].analyze_call(&f.dfg.value_lists, &f.dfg.exception_tables);
        if let CallInfo::Direct(fr, _) | CallInfo::DirectWithSig(fr, _, _) = info {
            nc.push(format!("fn{}:{a}", fr.index()));
        }
    }
    nc.sort();
    nc.dedup();
    let mut names: Vec<String> = Vec::new();
    for fr in f.dfg.ext_funcs.keys() {
        if let cranelift_codegen::ir::ExternalName::User(u) = f.dfg.ext_funcs[fr].name {
            let un = &f.params.user_named_funcs()[u];
            if un.namespace == 0
                && let Some(s) = p.fn_names.get(un.index as usize).filter(|s| !s.is_empty())
            {
                names.push(format!("fn{}:{s}", fr.index()));
            }
        }
    }
    names.sort();
    // `lifetimes` anchors are `Inst`s, which have no textual name — emit
    // the anchor's ordinal in layout order; the reader re-derives it.
    let mut ord = FxHashMap::default();
    let mut k = 0u32;
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            ord.insert(i, k);
            k += 1;
        }
    }
    let mut bord = FxHashMap::default();
    for (k, b) in f.layout.blocks().enumerate() {
        bord.insert(b, k as u32);
    }
    let lts: Vec<String> = p
        .lifetimes
        .iter()
        .map(|(slots, a, b, s)| {
            let a = a
                .and_then(|i| ord.get(&i).copied())
                .map(|o| format!("i{o}"))
                .unwrap_or_default();
            let b = bord.get(b).map(|o| format!("b{o}")).unwrap_or_default();
            let ss = slots
                .iter()
                .map(|s| format!("s{}", s.index()))
                .collect::<Vec<_>>()
                .join("+");
            format!("{ss}:{a}:{b}:{}", u8::from(*s))
        })
        .collect();
    let text = format!(
        "{}; diffcheck-nocap: {}\n; diffcheck-names: {}\n; diffcheck-lifetimes: {}\n{}",
        crate::diffcheck::facts_header(&p.clctx.func, &facts),
        nc.join(","),
        names.join(","),
        lts.join(","),
        crate::diffcheck::clif_text(&p.clctx.func)
    );
    let name = crate::diffcheck::safe_name(p.fname);
    let path = std::path::Path::new(dir).join(format!("{name}.{n}.clif"));
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(path, text);
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
        gate_sel(p)
    }
}

/// `FnFlags` packed for the `SEL_MEMO` key — field order only.
fn flags_key(f: &FnFlags) -> u64 {
    (f.jumpthread as u64)
        | (f.unreach as u64) << 1
        | (f.peep as u64) << 2
        | (f.tailmerge as u64) << 3
        | (f.taildup as u64) << 4
        | (f.tailrec as u64) << 5
        | (f.loadfwd as u64) << 6
        | (f.slot_dse as u64) << 7
        | (f.dse as u64) << 8
        | (f.indvars as u64) << 9
        | (f.licm as u64) << 10
        | (f.loopidiom as u64) << 11
        | (f.loopvec as u64) << 12
        | (f.bcheck as u64) << 13
        | (f.slp as u64) << 14
}

fn gate_sel(p: &Pipe) -> Vec<&'static Step> {
    thread_local! {
        /// Env gates are process-fixed, so a flags key decides the same
        /// selection every call. Skipped while a stepcache read window
        /// is open — the window must see the gate vars actually consulted.
        static SEL_MEMO: std::cell::RefCell<FxHashMap<u64, Vec<&'static Step>>> =
            Default::default();
    }
    if !pliron_cfg::reads_open() {
        return SEL_MEMO.with(|m| {
            let mut m = m.borrow_mut();
            m.entry(flags_key(&p.flags))
                .or_insert_with(|| DEFAULT.iter().filter(|s| s.gate.ok(p)).collect())
                .clone()
        });
    }
    DEFAULT.iter().filter(|s| s.gate.ok(p)).collect()
}

/// `PLIRON_LT_PROBE=1` (or `=<symbol substring>`): after pipeline entry
/// and every step, print `ltprobe <fn> <step> <anchor gone> <lost>
/// <markers>` — lifetime markers whose anchor inst left the layout, and of
/// those the ones whose block did too (unresolvable for slotmerge). Diff
/// consecutive lines of one function to see which steps drop markers.
fn lt_probe(p: &Pipe, step: &str) {
    static ON: std::sync::LazyLock<Option<&'static str>> =
        std::sync::LazyLock::new(|| crate::env_get("PLIRON_LT_PROBE"));
    let Some(sel) = *ON else { return };
    if sel != "1" && !p.fname.contains(sel) {
        return;
    }
    let f = &p.clctx.func;
    let (mut gone, mut lost) = (0, 0);
    for (_, anc, blk, _) in p.lifetimes.iter() {
        if !anc.is_some_and(|i| f.layout.inst_block(i).is_some()) {
            gone += 1;
            lost += usize::from(!f.layout.is_block_inserted(*blk));
        }
    }
    eprintln!("ltprobe {} {step} {gone} {lost} {}", p.fname, p.lifetimes.len());
}

/// Runs the selected steps. With `check_each`, every step that changes
/// the function is differentially checked against its input; returns
/// whether any check reported a mismatch. The step-cache only engages
/// on the normal path — a checked run must see the pipeline actually
/// execute.
fn run_steps(p: &mut Pipe, check_each: bool) -> bool {
    lt_probe(p, "start");
    let mut an = Analysis::default();
    // `-g` value labels name this CGU's variable pieces, and cache keys
    // don't see them: a hit would replay another function's labels.
    let labelled = p.clctx.func.dfg.values_labels.is_some();
    let pk = if check_each || labelled {
        None
    } else {
        match crate::stepcache::pipeline_lookup(p, &mut an) {
            crate::stepcache::PipeLookup::Hit => return false,
            crate::stepcache::PipeLookup::Miss(c) => Some(c),
            crate::stepcache::PipeLookup::Off => None,
        }
    };
    // The outer read window accumulates the union of every step's env
    // reads (frames nest) — but a read set is only meaningful for the
    // disk tier, where a variant must re-validate its env reads in a
    // later process. The in-process tier shares one fixed `config()`,
    // so validation there is vacuous and the recorder stays off:
    // without this, arming a producer for its whole pipeline run
    // turned every `config()` read on that thread into map inserts
    // (~1.5M/compile) and bypassed all the env-value memoizations.
    // `pk` is only `Some` when some tier is on — off means zero
    // recorder overhead for the normal path.
    let stats0 = pk.is_some().then(|| p.stats.clone());
    let armed = pk.is_some() && crate::stepcache::enabled() && reads_begin();
    if armed {
        // The bisect budget changes which steps run but is read through
        // a process-global `LazyLock` — it lands in this window only on
        // the first function of a process. Record it explicitly so a
        // pipe variant made under one budget can never hit under another.
        let _ = config().get("PLIRON_OPT_BISECT");
    }
    let draws0 = bisect_count();
    let sel = selected(p, false);
    // Per-step wall time accumulates into a dense slot during the loop
    // and merges into `stats.timing` once — a `FxHashMap<&str>` probe
    // per step per function was a measurable leaf.
    let mut step_t = vec![std::time::Duration::ZERO; sel.len()];
    let mut found = false;
    for (i, &s) in sel.iter().enumerate() {
        if !check_each {
            run_step(p, s, &mut an, &mut step_t[i]);
            continue;
        }
        let before = p.clctx.func.clone();
        let before_text = before.display().to_string();
        run_step(p, s, &mut an, &mut step_t[i]);
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
    for (&s, d) in sel.iter().zip(step_t) {
        *p.stats.timing.entry(s.name).or_default() += d;
    }
    let reads = reads_take();
    let draws = bisect_count() - draws0;
    // `armed` only decides whether `reads` is a real read set (disk
    // tier) or empty (mem tier, where in-process validation is
    // vacuous) — the claim is published either way.
    if let Some(mut claim) = pk {
        let ran = sel.iter().map(|s| s.name);
        crate::stepcache::pipeline_store(&mut claim, p, stats0.as_ref().unwrap(), reads, draws, ran);
    }
    crate::stepcache::flush(&mut an);
    p.stats.cfg_builds += an.cfg_builds;
    p.stats.dt_builds += an.dt_builds;
    p.stats.cfg_ns += an.cfg_ns;
    p.stats.dt_ns += an.dt_ns;
    p.stats.la_ns += an.la_ns;
    p.stats.la_builds += an.la_builds;
    // Hand the pipeline's cfg/domtree to the Context — still exact for
    // the final IR state (`take_cfg_dt` resolves the deferred edge
    // fingerprint first), so `optimize()` skips its per-fn rebuild.
    if let Some((c, d)) = an.take_cfg_dt(&p.clctx.func) {
        p.clctx.cfg = c;
        p.clctx.domtree = d;
        tracing::debug!(target: "pliron::pipeline", "seeded cfg+dt for {}", p.fname);
    }
    found
}

fn facts_of<'a>(p: &'a Pipe) -> crate::diffcheck::Facts<'a> {
    crate::diffcheck::Facts {
        nonnull: Some(p.nonnull),
        noalias: Some(p.noalias),
        scoped_na: Some(p.scoped_na),
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
            if crate::config::tracing_on()
                && tracing::enabled!(target: "pliron::diffcheck", tracing::Level::INFO)
            {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::diffcheck", "diffcheck {} compared={} inconclusive={} {}",
                        p.fname,
                        sum.compared,
                        sum.inconclusive,
                        sum.why.unwrap_or_default().lines().next().unwrap_or("")
                    )
                };
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
    p.seed_derived_bits();
    let localized = run_steps(p, true);
    *p.stats = real_stats;
    p.clctx.func = final_func;
    *p.derived = final_derived;
    p.seed_derived_bits();
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

/// Max alias-chain depth and total hop count across the value table.
/// `resolve_aliases` walks chains without compression, so a deep chain
/// arriving at a pass turns its per-inst resolves quadratic; this is the
/// batch diagnostic for that (`PLIRON_CHAIN_PROF`). Memoized suffix
/// lengths keep the scan linear.
fn chain_stats(func: &Function) -> (usize, u64) {
    let mut memo: FxHashMap<Value, u32> = FxHashMap::default();
    let mut path: Vec<Value> = Vec::new();
    let (mut maxd, mut total) = (0usize, 0u64);
    for v in func.dfg.values() {
        path.clear();
        let mut w = v;
        let base = loop {
            if let Some(&md) = memo.get(&w) {
                break md;
            }
            if !func.dfg.value_is_valid(w) {
                break 0;
            }
            match func.dfg.value_alias_dest_for_serialization(w) {
                Some(o) => {
                    path.push(w);
                    w = o;
                }
                None => break 0,
            }
        };
        for (i, &x) in path.iter().enumerate() {
            memo.insert(x, base + (path.len() - i) as u32);
        }
        let d = base + path.len() as u32;
        total += d as u64;
        maxd = maxd.max(d as usize);
    }
    (maxd, total)
}

/// Instruction count at which optimization steps stop paying for
/// themselves (see `run_step`). `PLIRON_GIANT_MAX=<n>` overrides;
/// `=0` disables the bail entirely.
fn giant_max() -> usize {
    if pliron_cfg::reads_open() {
        return giant_max_env();
    }
    static MAX: std::sync::LazyLock<usize> = std::sync::LazyLock::new(giant_max_env);
    *MAX
}

fn giant_max_env() -> usize {
    match config()
        .get("PLIRON_GIANT_MAX")
        .and_then(|v| v.parse().ok())
        .unwrap_or(GIANT_MAX)
    {
        0 => usize::MAX,
        n => n,
    }
}
const GIANT_MAX: usize = 50_000;

/// `PLIRON_GIANT_STEPS=a,b,c` — experimental allowlist: steps named in
/// the list still run on over-`giant_max` functions. Lets A/B runs
/// isolate which steps carry giant-function quality.
fn giant_steps() -> Vec<String> {
    if pliron_cfg::reads_open() {
        return giant_steps_env();
    }
    static L: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(giant_steps_env);
    L.clone()
}

fn giant_steps_env() -> Vec<String> {
    config()
        .get("PLIRON_GIANT_STEPS")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn run_step(p: &mut Pipe, s: &Step, an: &mut Analysis, slot: &mut std::time::Duration) {
    if let Some(b) = s.bisect
        && !crate::config::bisect_in(b, p.fname)
    {
        return;
    }
    // Giant-function bail: generated matchers (Cranelift's ISLE
    // `constructor_simplify`) and pathologically unrolled bodies reach
    // 10⁵+ instructions, where the optimization steps are pure cost —
    // a single superlinear pass burns minutes for wins that are
    // marginal at that scale. Past `PLIRON_GIANT_MAX` instructions
    // only `Gate::Always` correctness lowerings still run; a skipped
    // step can only leave the function unoptimized, never wrong.
    // Checked per step rather than once at selection: a step can grow
    // the function past the limit mid-pipeline (unroll).
    if !matches!(s.gate, Gate::Always)
        && (p.unopt
            || (p.clctx.func.dfg.num_insts() > giant_max()
                && !giant_steps().iter().any(|n| n == s.name)))
    {
        return;
    }
    // `PLIRON_TRACE=pliron::step` prints `time.busy` per step×function on
    // span close; the step's real name is the span name (see trace.rs).
    let _s = crate::config::tracing_on().then(|| {
        s.site
            .get_or_init(|| crate::trace::dyn_site("pliron::step", &["fn"], s.name))
            .span_with("fn", p.fname)
            .entered()
    });
    let _pg = p.prof_hook.map(|h| h(s.name));
    let t0 = pliron_cfg::step_timing_on().then(std::time::Instant::now);
    // A step whose required shape bits are absent cannot apply — skip it
    // before any cache lookup. Evaluated on the current state rather than
    // at selection time: earlier steps can create a later step's shape
    // (unrolled bodies seed `slp` pairs, rotations create back edges).
    // `PLIRON_NOSHAPE` disables the skip for A/B debugging;
    // `PLIRON_SHAPE_ONLY=<hex>` masks which bits gate (bisecting).
    // These debug knobs are process-fixed; under a stepcache read window
    // they must go through `config()` so the reads get recorded.
    let (mask, noshape) = if pliron_cfg::reads_open() {
        (
            config()
                .get("PLIRON_SHAPE_ONLY")
                .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
                .unwrap_or(u32::MAX),
            env_set("PLIRON_NOSHAPE"),
        )
    } else {
        static SHAPE_ONLY: std::sync::LazyLock<u32> = std::sync::LazyLock::new(|| {
            config()
                .get("PLIRON_SHAPE_ONLY")
                .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
                .unwrap_or(u32::MAX)
        });
        static NOSHAPE: std::sync::LazyLock<bool> =
            std::sync::LazyLock::new(|| env_set("PLIRON_NOSHAPE"));
        (*SHAPE_ONLY, *NOSHAPE)
    };
    let req = s.shape & mask;
    let req_any = s.any & mask;
    // Only gated steps pay for the bits — `shape_bits` is version-cached,
    // but the probe itself isn't free either.
    let sbits = if req != 0 || req_any != 0 {
        an.shape_bits(&p.clctx.func)
    } else {
        0
    };
    if !noshape && ((req != 0 && sbits & req != req) || (req_any != 0 && sbits & req_any == 0)) {
        if crate::config::tracing_on()
            && tracing::enabled!(target: "pliron::pipeline", tracing::Level::DEBUG)
        {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::pipeline", "shape skip: {} lacks {req:04x} on {}", s.name, p.fname)
            };
        }
        // `PLIRON_SHAPE_CHECK`: run the step anyway and report if it
        // changed the function — the bits are supposed to be necessary
        // conditions, so any mutation means the gate is wrong. The
        // function is restored so the pipeline keeps gated semantics.
        let shape_check = if pliron_cfg::reads_open() {
            env_set("PLIRON_SHAPE_CHECK")
        } else {
            static SHAPE_CHECK: std::sync::LazyLock<bool> =
                std::sync::LazyLock::new(|| env_set("PLIRON_SHAPE_CHECK"));
            *SHAPE_CHECK
        };
        if shape_check {
            let before = p.clctx.func.clone();
            let before_text = before.display().to_string();
            let derived0 = p.derived.clone();
            let k = (s.run)(p, an);
            if k > 0 || p.clctx.func.display().to_string() != before_text {
                eprintln!("SHAPE-GATE VIOLATION: {} fired on {}", s.name, p.fname);
            }
            p.clctx.func = before;
            *p.derived = derived0;
            p.seed_derived_bits();
            an.invalidate(&p.clctx.func);
        }
        if let Some(t0) = t0 {
            *slot += t0.elapsed();
        }
        return;
    }
    p.cur_step = s.name;
    let prof_chain = crate::config::tracing_on()
        && tracing::enabled!(target: "pliron::pipeline", tracing::Level::INFO);
    if prof_chain {
        let (m, t) = chain_stats(&p.clctx.func);
        if m >= 8 {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::pipeline", "chain {m} (tot {t}) entering {} on {}", s.name, p.fname)
            };
        }
    }
    let k = if s.cached && p.clctx.func.dfg.values_labels.is_none() {
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
    if let Some(t0) = t0 {
        let d = t0.elapsed();
        *slot += d;
        trace(s.name, d.as_secs_f64() * 1e3, p.fname);
    }
    lt_probe(p, s.name);
    // `PLIRON_CLIF_STEPS=1` (with `PLIRON_CLIF`): dump after every step that
    // reports a change, including the steps that don't dump themselves.
    if k > 0 && p.dump && *CLIF_STEPS {
        p.dump(&format!("{} [step]", s.name));
    }
    if s.mutates {
        // Not conditional on `k`: a pass that removes blocks but
        // reports 0 would leave cfg/domtree stale for the next step.
        // `bump` fingerprints the CFG: a value-only step keeps the
        // cached cfg/dt/la and drops just the value-keyed caches.
        an.bump(&p.clctx.func, s.creates);
        if prof_chain {
            let (m, t) = chain_stats(&p.clctx.func);
            if m >= 8 {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::pipeline", "chain {m} (tot {t}) made by {} on {}", s.name, p.fname)
                };
            }
        }
        if k > 0 {
            p.since_reap = true;
            // Alias targets can themselves be aliased later, so chains
            // grow a hop per re-alias — flatten before the next step's
            // per-inst resolves multiply by the depth. Every alias write
            // bumps `alias_epoch`, so no bump since the last compaction
            // means nothing can be deeper than a hop.
            if p.clctx.func.dfg.alias_epoch() != an.compact_epoch {
                p.clctx.func.dfg.compact_aliases();
                an.compact_epoch = p.clctx.func.dfg.alias_epoch();
            }
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
fn s_mergechain(p: &mut Pipe, an: &mut Analysis) -> usize {
    let prof2 = crate::config::tracing_on()
        && tracing::enabled!(target: "pliron::jumpthread", tracing::Level::INFO);
    let tt = |name: &str, t0: Option<std::time::Instant>| {
        if let (true, Some(t0)) = (prof2, t0) {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::jumpthread", "    mc-{name} {:.3}ms", t0.elapsed().as_secs_f64() * 1e3)
            };
        }
    };
    let mut n = 0;
    // `merge_chains` only inspects `jump` predecessors; the dispatch
    // work below needs a conditional branch. The step gate is `MULTI`,
    // so a jump-only function reaches here — the CONDBR guard keeps
    // the dispatch phases on exactly the functions the old step gate
    // admitted.
    // `merge_chains` only inspects `jump` predecessors; the dispatch
    // work below needs a conditional branch. Cached step-entry bits
    // suffice: `merge_chains` touches jump chains only, so it can
    // neither create CONDBR nor remove JUMP's need here — stale-true
    // bits merely over-admit a no-op scan.
    let mc = an.shape_bits(&p.clctx.func);
    // Shared cfg/domtree epoch across the merge + dispatch fixpoints:
    // marked stale on edge edits, recomputed in place on next use.
    let mut cd = jumpthread::Cd::new();
    if crate::pass_enabled("PLIRON_CHAINMERGE") && mc & shape::JUMP != 0 {
        let t0 = prof2.then(std::time::Instant::now);
        jumpthread::merge_chains(&mut p.clctx.func, &mut cd);
        tt("chains", t0);
    }
    let cond = mc & shape::CONDBR != 0;
    if cond && crate::pass_enabled("PLIRON_DISPATCHDUP") {
        let t0 = prof2.then(std::time::Instant::now);
        // Duplicating a dispatch into a jump pred rewires that pred's own
        // jump-preds onto the dispatch — run to fixpoint so multi-hop
        // chains (851/853/854 -> 388 -> 859) fully collapse.
        for _ in 0..6 {
            let k = {
                let (_, d) = cd.get(&p.clctx.func);
                jumpthread::dispatch_dup(&mut p.clctx.func, d)
            };
            n += k;
            if k == 0 {
                break;
            }
            cd.stale();
        }
        tt("dup1", t0);
    }
    if cond && crate::pass_enabled("PLIRON_EDGESPEC2") {
        cd.stale();
        if crate::pass_enabled("PLIRON_REBIND") {
            let t0 = prof2.then(std::time::Instant::now);
            n += jumpthread::rebind_exports(&mut p.clctx.func, an, &mut cd);
            tt("rebind", t0);
        }
        let t0 = prof2.then(std::time::Instant::now);
        n += edgespec::run(&mut p.clctx.func, &mut cd);
        tt("espec", t0);
        if crate::pass_enabled("PLIRON_DISPATCHDUP") {
            let t0 = prof2.then(std::time::Instant::now);
            // Edgespec can leave fresh jump preds on small dispatch blocks.
            for _ in 0..6 {
                let k = {
                    let (_, d) = cd.get(&p.clctx.func);
                    jumpthread::dispatch_dup(&mut p.clctx.func, d)
                };
                n += k;
                if k == 0 {
                    break;
                }
                cd.stale();
            }
            tt("dup2", t0);
        }
    }
    let t0 = prof2.then(std::time::Instant::now);
    let k = if cond {
        clifpeep::brif_fold(&mut p.clctx.func)
    } else {
        0
    };
    tt("brifold", t0);
    n += k;
    if n > 0 {
        let t0 = prof2.then(std::time::Instant::now);
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
    // NonNull builds lazily inside `round` on the first candidate edge;
    // a round that threads nothing never pays for the fixpoint.
    let mut nn: Option<jumpthread::NonNull> = None;
    for i in 0..8 {
        let mut evict = FxHashSet::default();
        let n = {
            let (c, d) = cd.get_or_insert_with(|| {
                let c = ControlFlowGraph::with_function(&p.clctx.func);
                let d = DominatorTree::with_function(&p.clctx.func, &c);
                (c, d)
            });
            jumpthread::round(
                &mut p.clctx.func,
                p.nonnull,
                p.derived,
                &mut nn,
                crate::pass_enabled("PLIRON_DOMCOND"),
                c,
                d,
                Some(&mut evict),
            )
        };
        if n > 0 {
            cd = None;
            if let Some(nnv) = &mut nn {
                nnv.evict_params(&p.clctx.func, &evict);
            }
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

fn s_jumpthread(p: &mut Pipe, an: &mut Analysis) -> usize {
    let sh = an.shape_bits(&p.clctx.func);
    let (n, dead, folds, pmap) = jumpthread::run(&mut p.clctx.func, p.nonnull, p.derived, sh);
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
    let k = slotprom::run(&mut p.clctx.func, cfg, p.nocap, p.flags.keep_dbg_slots, p.dbg_slots);
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
        // Everything the loop runs keeps predecessor sets: param drops
        // shrink edge args, `fold_same_target_brifs` rewrites in place,
        // and `remove_dead_insts` can't reach terminators.
        let ucfg = ControlFlowGraph::with_function(&p.clctx.func);
        for _ in 0..4 {
            let p2 = jumpthread::remove_dead_params(&mut p.clctx.func, &ucfg)
                + jumpthread::fold_same_target_brifs(&mut p.clctx.func);
            let i = jumpthread::remove_dead_insts(&mut p.clctx.func, true);
            (dp, di) = (dp + p2, di + i);
            if p2 + i == 0 {
                break;
            }
        }
        if crate::config::tracing_on()
            && tracing::enabled!(target: "pliron::unroll", tracing::Level::DEBUG)
        {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::unroll", "unroll cleanup {}: {ub} blocks, {dp} params, {di} insts",
                    p.fname
                )
            };
        }
    }
    if k > 0
        && crate::config::tracing_on()
        && tracing::enabled!(target: "pliron::unroll", tracing::Level::DEBUG)
    {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::unroll", "unroll {k} {}", p.fname)
        };
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

fn s_stepcmp(p: &mut Pipe, _an: &mut Analysis) -> usize {
    clifpeep::stepcmp(&mut p.clctx.func)
}

fn s_mulcarry(p: &mut Pipe, _an: &mut Analysis) -> usize {
    clifpeep::mulcarry(&mut p.clctx.func)
}

fn s_selchain(p: &mut Pipe, _an: &mut Analysis) -> usize {
    clifpeep::selchain(&mut p.clctx.func)
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

fn s_loadfwd(p: &mut Pipe, an: &mut Analysis) -> usize {
    let prof = crate::config::tracing_on()
        && tracing::enabled!(target: "pliron::loadfwd", tracing::Level::INFO);
    let t = prof.then(std::time::Instant::now);
    // The dataflow only ever forwards a `load` (every `Fwd` is pushed
    // on a load arm) — without one, the CFG/RPO/proot/iso setup and
    // the fixpoint can produce nothing.
    if an.shape_bits(&p.clctx.func) & shape::LOAD != 0 {
        let breg_now = live_block_region(p);
        let (cfg, pf, ps) = an.lf_inputs(&p.clctx.func);
        p.stats.forwarded += loadfwd::run(
            &mut p.clctx.func,
            cfg,
            pf,
            ps,
            p.noalias,
            p.scoped_na,
            p.invariant,
            p.operand_frozen,
            &breg_now,
            p.region_parent,
            p.nowrite,
            p.call_frozen,
            p.nocap,
        );
    }
    if let Some(t) = t {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::loadfwd", "    lf-run {:?}", t.elapsed())
        };
    }
    if p.flags.slot_dse {
        // A load whose only user was a removed dead store keeps its own
        // slot alive, so alternate the two until nothing changes.
        let has_store = an.shape_bits(&p.clctx.func) & shape::STORE != 0;
        for i in 0..4 {
            let t = prof.then(std::time::Instant::now);
            if has_store {
                p.stats.slot_dse += loadfwd::dead_slot_stores(&mut p.clctx.func);
            }
            if let Some(t) = t {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::loadfwd", "    lf-dss[{i}] {:?}", t.elapsed())
                };
            }
            let t = prof.then(std::time::Instant::now);
            let n = if config().enabled("PLIRON_DEAD_LOADS") {
                jumpthread::remove_dead_insts(
                    &mut p.clctx.func,
                    config().enabled("PLIRON_DEAD_PURE"),
                )
            } else {
                0
            };
            if let Some(t) = t {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::loadfwd", "    lf-rdi[{i}] {n} {:?}", t.elapsed())
                };
            }
            if n == 0 {
                break;
            }
        }
    }
    p.dump("loadfwd");
    0
}

fn s_dse(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = dse::run(
        &mut p.clctx.func,
        p.noalias,
        p.scoped_na,
        p.nowrite,
        p.call_frozen,
        p.nounwind,
        an,
        p.cur_step,
        p.fname,
    );
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
        an.invalidate(&p.clctx.func);
        indvars::run(&mut p.clctx.func, an, p.amode_scale);
    }
    k
}

fn s_licm(p: &mut Pipe, an: &mut Analysis) -> usize {
    let breg_now = live_block_region(p);
    let k = licm::run(
        &mut p.clctx.func,
        an,
        p.noalias,
        p.scoped_na,
        p.deref,
        p.nowrite,
        p.call_frozen,
        p.nounwind,
        p.scoped,
        &breg_now,
        p.region_parent,
        p.operand_frozen,
    );
    // Hoisting can expose `base + iv` adds whose base was loop-internal
    // at indvars time; stepping them into params evicts the base's live
    // range from the loop.
    if k > 0 && p.flags.indvars && config().enabled("PLIRON_INDUCT2") && bisect("indvars2") {
        an.invalidate(&p.clctx.func);
        indvars::run(&mut p.clctx.func, an, p.amode_scale);
    }
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

// The Hacker's Delight trailing-zeros loop `lb=1; while n%(lb*2)==0
// {lb*=2}` is `n & -n` — a band in place of a urem per trailing zero
// (the effectful-egraph paper's case study; LLVM and GCC both miss it).
fn s_lowbit(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = lowbit::run(&mut p.clctx.func, an);
    if k > 0 && p.dump {
        p.dump("lowbit");
    }
    k
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
    // (`PLIRON_CPARM`). The `CONDBR | BPARAM` step gate covers its
    // webmerge (BPARAM) and const-sweep (CONDBR) halves.
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
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::dump", "==== clif {} after cparm ({k}) ====\n{}",
                    p.fname,
                    p.clctx.func.display()
                )
            };
        }
    }
    // Nested range-check diamonds (`is_ascii_alphabetic` &co) fold
    // leaf-up: `run` rescans to fixpoint internally on one
    // cfg/domtree setup. A diamond needs a `brif` header and `jump`
    // arms — `cparm`'s `const_sweep` folds `brif const` into `jump`,
    // so JUMP can appear here even when entry lacked it. Fresh bits.
    let k = if shape_of(&p.clctx.func) & (shape::CONDBR | shape::JUMP)
        == (shape::CONDBR | shape::JUMP)
    {
        ifconv::run(&mut p.clctx.func, p.derived, None)
    } else {
        0
    };
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
    // wasm32 CLIF (i32 pointers) is the only path where `umulhi.i64` is a
    // synthesized sequence rather than one `umulh`/`mul` — that's where
    // divnarrow's i32 expansion wins.
    let k = divconst::run(
        &mut p.clctx.func,
        p.tcfg.pointer_width == target_lexicon::PointerWidth::U32,
    );
    if k > 0 && p.dump {
        p.dump("divconst");
    }
    k
}

fn s_loopvec(p: &mut Pipe, an: &mut Analysis) -> usize {
    // Per-function wide-vector probing off the ISA flags: 256-bit lanes
    // under AVX2, 512 under AVX512F — capped by the codegen-wide
    // `wide_vector_bits` ceiling. loopvec drops a loop back to 128
    // whenever its ops lack wide lowerings.
    let wc = {
        let isa_flag = |n: &str| {
            p.isa
                .isa_flags()
                .iter()
                .find(|f| f.name == n)
                .map_or(false, |v| v.as_bool().unwrap_or(false))
        };
        let vb = if isa_flag("has_avx512f") {
            512
        } else if isa_flag("has_avx2") {
            256
        } else {
            128
        };
        loopvec::WideCaps {
            vbits: if p.x64 { vb.min(x64_wide_bits()) } else { 128 },
            fma: isa_flag("has_fma"),
            avx512bw: isa_flag("has_avx512bw"),
        }
    };
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
        wc,
    );
    // loopvec emits `base + iv*esize` addressing fresh each iteration;
    // a second indvars round turns them into stepped pointers.
    if k > 0 && p.flags.indvars && config().enabled("PLIRON_INDUCT2") && bisect("indvars2") {
        // loopvec rewrote the loop the cache describes.
        an.invalidate(&p.clctx.func);
        indvars::run(&mut p.clctx.func, an, p.amode_scale);
    }
    k
}

fn s_slp(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = slp::run(&mut p.clctx.func, an, p.noalias, p.simd, p.x64);
    p.dump("slp");
    k
}

fn s_switchmap(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = switchmap::run(&mut p.clctx.func);
    // The new bounds tests are often implied by a dominating check.
    let f =
        if k > 0 && config().enabled("PLIRON_DOMCOND") && config().enabled("PLIRON_SWITCHMAP_FOLD")
        {
            // switchmap::run rewrote edges since the last bump — refresh
            // the shared epoch rather than building a private cfg/dt.
            an.invalidate(&p.clctx.func);
            let (cfg, dt) = an.cfg_dt(&p.clctx.func);
            let folds = jumpthread::fold_dominated_conds_at(&mut p.clctx.func, cfg, dt);
            p.stats.dominated_folds += folds;
            if folds > 0 {
                jumpthread::fold_const_branches(&mut p.clctx.func)
            } else {
                0
            }
        } else {
            0
        };
    if k > 0
        && crate::config::tracing_on()
        && tracing::enabled!(target: "pliron::switchmap", tracing::Level::DEBUG)
    {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::switchmap", "switchmap {k} fold {f} {}", p.fname)
        };
    }
    0
}

fn s_constbr2(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = jumpthread::fold_const_branches(&mut p.clctx.func);
    if k > 0
        && crate::config::tracing_on()
        && tracing::enabled!(target: "pliron::constbr", tracing::Level::DEBUG)
    {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::constbr", "constbr {k} {}", p.fname)
        };
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

fn s_splink(p: &mut Pipe, an: &mut Analysis) -> usize {
    let CfgDtLa { la, .. } = an.cfg_dt_la(&p.clctx.func);
    let k = splink::run(&mut p.clctx.func, la, p.x64);
    p.dump("splink");
    k
}

fn s_frozen_loads(p: &mut Pipe, _an: &mut Analysis) -> usize {
    if p.frozen.is_empty() {
        return 0;
    }
    let k = clifpeep::frozen_loads(&mut p.clctx.func, p.frozen);
    if p.dump {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::dump", "==== clif {}: {k} frozen loads, params {:?} ====\n{}",
                p.fname,
                p.frozen,
                p.clctx.func.display()
            )
        };
    }
    k
}

// `isub x, k` → `iadd x, -k` so x64's `lea` lowering can write a
// call-arg/phi result non-destructively instead of `mov` + `sub`.
fn s_sublea(p: &mut Pipe, an: &mut Analysis) -> usize {
    // `ovkill` is the only `cfg` consumer; don't materialize it when off.
    let cfg = crate::pass_enabled("PLIRON_OVKILL").then(|| an.cfg(&p.clctx.func));
    clifpeep::sublea(&mut p.clctx.func, cfg)
}

// Constant folding for insts the loop passes emit after `clifpeep`'s
// early round — see the `csweep2` step.
fn s_csweep2(p: &mut Pipe, _an: &mut Analysis) -> usize {
    clifpeep::const_sweep(&mut p.clctx.func)
}

// `sublea`'s `ovkill` runs before the niche selects collapse to params
// (sameargs3/webmerge): one more bound-decided `icmp x, k` pass after
// them — see the `ovkill2` step.
fn s_ovkill2(p: &mut Pipe, an: &mut Analysis) -> usize {
    let cfg = an.cfg(&p.clctx.func);
    let k = clifpeep::ovkill(&mut p.clctx.func, cfg);
    if k > 0 && p.dump {
        p.dump("ovkill2");
    }
    k
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
    // Nothing pins registers on wasm; the disguised `umulhi x, 0` zero
    // would only cost a widened-mul expansion per cold arg.
    let k = if p.wasm {
        0
    } else {
        clifpeep::coldargs(&mut p.clctx.func)
    };
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

// Conditional push-in: `brif (icmp eq x, k)` proves `x == k` inside the
// taken arm — rewrite in-arm operands to `k` and clone the pure
// predecessor insts they specialize into the arm head, where the
// constant sweeps fold them (eggcc's push-in for `bop(const, x)`
// if-inputs). Runs before celim so arm-local specialization folds
// before condition folding rewrites the arm conditions.
fn s_pushin(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = pushin::run(&mut p.clctx.func, an);
    if k > 0 && p.dump {
        p.dump("pushin");
    }
    k
}

// Reassociate `((r + a) + b) + c` accumulator chains into `r + ((a +
// b) + c)` when `r` is a block param — one add stays on the
// loop-carried edge instead of the whole chain (LLVM's isel shape).
fn s_reassoc(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = reassoc::run(&mut p.clctx.func, an);
    if k > 0 && p.dump {
        p.dump("reassoc");
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
fn s_paramsink(p: &mut Pipe, an: &mut Analysis) -> usize {
    let (_, dom) = an.cfg_dt(&p.clctx.func);
    let (k, moved) = paramsink::run(&mut p.clctx.func, dom);
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
fn s_niche(p: &mut Pipe, an: &mut Analysis) -> usize {
    // Every `fold_of` candidate is an `icmp` — without one the
    // `NonNull` build and the pass itself can't produce a rewrite. The
    // trailing dead-inst sweep stays unguarded.
    let k = if an.shape_bits(&p.clctx.func) & shape::TESTC != 0 {
        let nn = jumpthread::NonNull::new(
            &p.clctx.func,
            an.cfg_dt(&p.clctx.func).0,
            p.nonnull,
            p.derived,
        );
        niche::run(&mut p.clctx.func, &nn)
    } else {
        0
    } + jumpthread::remove_dead_insts(&mut p.clctx.func, true);
    if k > 0 {
        p.dump("niche");
    }
    k
}

// `brif c` edge args `select(c,a,b)` collapse to an arm — the loop-tail
// `csel` materializing `select(end?, 0, cursor)` on the edge where
// `end?` is already known-false.
fn s_selargs(p: &mut Pipe, an: &mut Analysis) -> usize {
    // `selargs` only rewrites `brif` edge args — keep the dead-inst
    // sweep unguarded.
    let k = if an.shape_bits(&p.clctx.func) & shape::CONDBR != 0 {
        clifpeep::selargs(&mut p.clctx.func)
    } else {
        0
    } + jumpthread::remove_dead_insts(&mut p.clctx.func, true);
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
fn s_dgvn(p: &mut Pipe, an: &mut Analysis) -> usize {
    let (_, dt) = an.cfg_dt(&p.clctx.func);
    let k = clifpeep::dgvn(&mut p.clctx.func, dt);
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
    if crate::config::tracing_on()
        && tracing::enabled!(target: "pliron::sameargs", tracing::Level::DEBUG)
    {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::sameargs", "--- sameargs {}", p.fname)
        };
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
fn s_seltest(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = jumpthread::split_niche_tests(&mut p.clctx.func, an);
    if k > 0 && p.dump {
        p.dump("seltest");
    }
    k
}

// Second load-forwarding pass. sameargs' param drops (esp. affparm's
// `p_j = p_i + k` collapse) turn formerly opaque param addresses into
// `iadd` chains that `concrete` peels to exact (root, offset) locs —
// making stores/loads that were unprovable at the early run disjoint.
fn s_loadfwd2(p: &mut Pipe, an: &mut Analysis) -> usize {
    let breg_now = live_block_region(p);
    let (cfg, pf, ps) = an.lf_inputs(&p.clctx.func);
    let k = loadfwd::run(
        &mut p.clctx.func,
        cfg,
        pf,
        ps,
        p.noalias,
        p.scoped_na,
        p.invariant,
        p.operand_frozen,
        &breg_now,
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
fn s_edgespec(p: &mut Pipe, an: &mut Analysis) -> usize {
    // Adopt the shared cache's cfg/dt storage: refreshes inside
    // edgespec then reuse it rather than allocating afresh.
    let mut cd = match an.take_cfg_dt(&p.clctx.func) {
        Some((c, d)) => jumpthread::Cd::from_parts(c, d),
        None => jumpthread::Cd::new(),
    };
    let k = edgespec::run(&mut p.clctx.func, &mut cd);
    if k > 0 && p.dump {
        p.dump("edgespec");
    }
    k
}

fn s_edgesink(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let k = edgesink::run(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("edgesink");
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
fn s_dse2(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = dse::run(
        &mut p.clctx.func,
        p.noalias,
        p.scoped_na,
        p.nowrite,
        p.call_frozen,
        p.nounwind,
        an,
        p.cur_step,
        p.fname,
    );
    if k > 0 && p.dump {
        p.dump("dse2");
    }
    k
}

fn s_dse3(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = dse::run(
        &mut p.clctx.func,
        p.noalias,
        p.scoped_na,
        p.nowrite,
        p.call_frozen,
        p.nounwind,
        an,
        p.cur_step,
        p.fname,
    );
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
fn s_memspec(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = memspec::run(
        &mut p.clctx.func,
        an,
        p.noalias,
        p.scoped_na,
        p.nowrite,
        p.call_frozen,
    );
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


// i64 value webs whose consumers only read the low 32 bits (the
// Option<u32>/usize widenings and uloads i64 loads) re-emitted as i32 —
// wasm i64.* + i32.wrap_i64 per-iteration overhead. Runs late so the
// whole pipeline residuals are covered; clifpeep also calls it early.
fn s_dem64(p: &mut Pipe, _an: &mut Analysis) -> usize {
    let mut k = clifpeep::dem64(&mut p.clctx.func);
    if crate::pass_enabled("PLIRON_ZXCHAIN") {
        k += clifpeep::zext_chain(&mut p.clctx.func);
    }
    if k > 0 && p.dump {
        p.dump("dem64");
    }
    k
}

fn s_postinc(p: &mut Pipe, an: &mut Analysis) -> usize {
    let mut k = indvars::postinc(&mut p.clctx.func, an, 1024);
    if k > 0 {
        an.invalidate(&p.clctx.func);
    }
    k += indvars::postinc_ne(&mut p.clctx.func, an);
    if k > 0 && p.dump {
        p.dump("postinc");
    }
    k
}

// Carry `d`'s recorded facts onto `w` when `w` is `d`'s value
// everywhere — a merge carrier, an alias target, or a sunk/rebased
// replacement.
/// `Pipe::fact_bits` flag bits — which per-value fact tables `d` may
/// appear in. Stale-set bits are safe (the table probe decides); a bit
/// must never be absent while the table holds the value.
pub(crate) mod fact_bits {
    pub const DERIVED: u8 = 1;
    pub const NONNULL: u8 = 2;
    pub const FROZEN: u8 = 4;
    pub const NOALIAS: u8 = 8;
    pub const DEREF: u8 = 16;
    pub const OPERAND_FROZEN: u8 = 32;
    pub const SCOPED_NA: u8 = 64;
}

impl Pipe<'_> {
    /// Seed `fact_bits` from the already-populated fact tables — the
    /// only place bits can be missing is writes made before this `Pipe`
    /// borrowed them. Bits only gate lookups, so a stale-set bit costs
    /// one wasted probe and a missing bit is a skipped carry.
    pub fn seed_fact_bits(&mut self) {
        for v in self.derived.keys() {
            self.fact_bits[v] |= fact_bits::DERIVED;
        }
        for &v in self.nonnull.iter() {
            self.fact_bits[v] |= fact_bits::NONNULL;
        }
        for &v in self.frozen.keys() {
            self.fact_bits[v] |= fact_bits::FROZEN;
        }
        for &v in self.noalias.iter() {
            self.fact_bits[v] |= fact_bits::NOALIAS;
        }
        for &v in self.scoped_na.iter() {
            self.fact_bits[v] |= fact_bits::SCOPED_NA;
        }
        for &v in self.deref.keys() {
            self.fact_bits[v] |= fact_bits::DEREF;
        }
        for &v in self.operand_frozen.keys() {
            self.fact_bits[v] |= fact_bits::OPERAND_FROZEN;
        }
    }

    /// Re-seed one table's bit after a wholesale `*p.derived = ...`
    /// replace — the collect doesn't go through `insert`, so no bits
    /// were set along the way.
    pub fn seed_derived_bits(&mut self) {
        for v in self.derived.keys() {
            self.fact_bits[v] |= fact_bits::DERIVED;
        }
    }
}

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
    // The coverage check below only matters when `d` carries a site
    // fact (derived/nonnull); most carried pairs have neither, so gate
    // the two `try_value_def` + `pp_block` lookups on the fact bits.
    let bits = p.fact_bits[d];
    if bits & (fact_bits::DERIVED | fact_bits::NONNULL) != 0 {
        let dbase = p.derived.get(&d).copied();
        let dnn = p.nonnull.contains(&d);
        if dbase.is_some() || dnn {
            let covered = {
                use cranelift_codegen::ir::ProgramPoint;
                let f = &p.clctx.func;
                // `try_value_def` skips reserved/stale values and doesn't
                // chase aliases (`value_def` resolves unchecked and panics
                // on chains ending at a tombstone); an alias def just falls
                // through to no-carry. Program points compare in O(1): a
                // param's point is its block head, which orders before
                // every inst in the block — the same ordering the old
                // inst-index scan gave.
                let pp = |v: Value| match f.dfg.try_value_def(v) {
                    // A def may be detached (removed from the layout) while
                    // its value still exists — `pp_block`/`pp_cmp` panic on
                    // those, where the old position scan returned `None`.
                    Some(ValueDef::Result(i, _)) if f.layout.inst_block(i).is_some() => {
                        Some(ProgramPoint::Inst(i))
                    }
                    Some(ValueDef::Param(b, _)) if f.layout.is_block_inserted(b) => {
                        Some(ProgramPoint::Block(b))
                    }
                    _ => None,
                };
                match (pp(d), pp(w)) {
                    (Some(a), Some(b)) => {
                        f.layout.pp_block(a) == f.layout.pp_block(b)
                            && f.layout.pp_cmp(a, b) != std::cmp::Ordering::Greater
                    }
                    _ => false,
                }
            };
            if covered {
                if let Some(b) = dbase {
                    p.derived.insert(w, b);
                    p.fact_bits[w] |= fact_bits::DERIVED;
                }
                if dnn {
                    if crate::config::tracing_on()
                        && tracing::enabled!(target: "pliron::pipeline", tracing::Level::DEBUG)
                    {
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
                        if pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::pipeline", "nndbg carry {w} <- {d} [{}] w={wd:?} d={dd:?}", p.cur_step)
                        };
                    }
                    p.nonnull.insert(w);
                    p.fact_bits[w] |= fact_bits::NONNULL;
                }
            }
        }
    }
    if bits & fact_bits::NOALIAS != 0 && p.noalias.contains(&d) {
        p.noalias.insert(w);
        p.fact_bits[w] |= fact_bits::NOALIAS;
    }
    if bits & fact_bits::SCOPED_NA != 0 && p.scoped_na.contains(&d) {
        p.scoped_na.insert(w);
        p.fact_bits[w] |= fact_bits::SCOPED_NA;
    }
    if bits & fact_bits::FROZEN != 0
        && let Some(&f) = p.frozen.get(&d)
    {
        p.frozen.insert(w, f);
        p.fact_bits[w] |= fact_bits::FROZEN;
    }
    if bits & fact_bits::DEREF != 0
        && let Some(&x) = p.deref.get(&d)
    {
        p.deref.insert(w, x);
        p.fact_bits[w] |= fact_bits::DEREF;
    }
    if bits & fact_bits::OPERAND_FROZEN != 0
        && let Some(v) = p.operand_frozen.get(&d)
    {
        let v = v.clone();
        p.operand_frozen.insert(w, v);
        p.fact_bits[w] |= fact_bits::OPERAND_FROZEN;
    }
}

fn carry_pmap(p: &mut Pipe, pmap: &FxHashMap<Value, Value>) {
    // Resolve each pmap link once: chasing per `derived` key walked the
    // chain O(|pmap|) per key — quadratic on functions that drop many
    // params. Memoized resolution is O(|pmap| + |derived|).
    let resolve = |start: Value, memo: &mut FxHashMap<Value, Value>| -> Value {
        if let Some(&r) = memo.get(&start) {
            return r;
        }
        let mut v = start;
        let mut path = Vec::new();
        let r = loop {
            if let Some(&r) = memo.get(&v) {
                break r;
            }
            match pmap.get(&v) {
                Some(&w) if path.len() <= pmap.len() => {
                    path.push(v);
                    v = w;
                }
                _ => break v,
            }
        };
        for &x in &path {
            memo.insert(x, r);
        }
        memo.insert(start, r);
        r
    };
    let mut memo: FxHashMap<Value, Value> = FxHashMap::default();
    // `x` derived from a dropped param is derived from its carrier, and
    // a dropped param's own base moves to the carrier too.
    let keys: Vec<Value> = p.derived.keys().collect();
    for x in keys {
        if let Some(&b) = p.derived.get(&x) {
            let b = resolve(b, &mut memo);
            p.derived.insert(x, b);
        }
    }
    // `pmap[d] = w` makes `w` `d`'s value on every path — the merge
    // invariant — so `d`'s recorded facts hold of `w` too, whatever the
    // merge kind (a phase-A carrier, a deduped sibling, or a
    // materialized `f(siblings)` tree).
    for (&d, &w) in pmap {
        let w = resolve(w, &mut memo);
        carry_one(p, d, w);
    }
}

/// `change_to_alias(d, w)` makes `w` `d`'s value everywhere, so facts
/// keyed on `d` transfer to `w`. Two dozen passes alias values; rather
/// than plumbing a merge map out of each, scan the alias table after
/// mutating steps — re-carrying an already-carried pair is idempotent,
/// and `an.carried` skips pairs already handled.
fn carry_aliases(p: &mut Pipe, an: &mut Analysis) {
    // Steps that wrote no aliases since the last scan can't have new
    // (alias, root) pairs — most mutating steps don't alias anything,
    // so the backlog walk is usually dead work.
    let dirty = p.clctx.func.dfg.alias_writes();
    if dirty.len() == an.alias_wm {
        return;
    }
    if p.derived.is_empty()
        && p.nonnull.is_empty()
        && p.noalias.is_empty()
        && p.frozen.is_empty()
        && p.deref.is_empty()
        && p.operand_frozen.is_empty()
        && p.lifetimes.is_empty()
    {
        // Don't advance the watermark here: a later step may add a fact
        // map entry without touching aliases, and this backlog's pairs
        // still need carrying then.
        return;
    }
    let wm = an.alias_wm;
    an.alias_wm = dirty.len();
    let func = &p.clctx.func;
    let pairs: Vec<(Value, Value)> = dirty[wm..]
        .iter()
        .copied()
        .filter_map(|v| {
            // Tombstone chains from deleted defs dead-end at reserved
            // targets — `resolve_aliases` panics there; just skip them.
            // Re-aliased values repeat in `dirty`; each new write is a
            // fresh pair candidate regardless.
            let r = func.dfg.try_resolve_aliases(v)?;
            (r != v && !an.carried.contains(&(v, r))).then_some((v, r))
        })
        .collect();
    for (d, w) in &pairs {
        an.carried.insert((*d, *w));
        carry_one(p, *d, *w);
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

fn s_armmerge(p: &mut Pipe, _an: &mut Analysis) -> usize {
    armmerge::run(&mut p.clctx.func)
}

// Conditional invariant code motion: an unconditional pure inst both
// brif arms compute exists on every path through the diamond — hoist
// one copy into the predecessor (GVNHoist; eggcc's cicm extracting
// shared `if`-input terms). Shrinks arms toward armmerge's isomorphic
// shape, so it runs just ahead of it.
fn s_cicm(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = cicm::run(&mut p.clctx.func, an);
    if k > 0 && p.dump {
        p.dump("cicm");
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
    if p.wasm {
        // wasm has a real `br_table`; chains only add compares.
        return 0;
    }
    let k = brchain::run(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("brchain");
    }
    k
}

// Merge disjoint-lifetime explicit slots — last, so slot references
// cloned by earlier passes are all accounted for before sharing.
fn s_slotmerge(p: &mut Pipe, _an: &mut Analysis) -> usize {
    // `-g -O0`: every variable keeps its own slot for the debugger.
    if p.flags.keep_dbg_slots {
        return 0;
    }
    let stats = crate::config::tracing_on()
        && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO);
    let smdbg = crate::config::tracing_on()
        && tracing::enabled!(target: "pliron::slotmerge", tracing::Level::DEBUG);
    let before: u64 = (stats || smdbg)
        .then(|| {
            p.clctx
                .func
                .sized_stack_slots
                .values()
                .map(|d| d.size as u64)
                .sum()
        })
        .unwrap_or(0);
    if env_set("PLIRON_SM_INFO_FN") {
        slotmerge::CUR_FN.with(|c| *c.borrow_mut() = p.fname.to_string());
    }
    let k = slotmerge::run(&mut p.clctx.func, p.nowrite, p.lifetimes, p.nocap);
    if smdbg {
        let after: u64 = p
            .clctx
            .func
            .sized_stack_slots
            .values()
            .map(|d| d.size as u64)
            .sum();
        tracing::debug!(target: "pliron::slotmerge", "slotmerge-fn {} merged={k} {before}->{after}B", p.fname);
    }
    if stats {
        let after: u64 = p
            .clctx
            .func
            .sized_stack_slots
            .values()
            .map(|d| d.size as u64)
            .sum();
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats", "slotmerge {k} {before}->{after}B {} {}",
                p.nocap.len(),
                p.fname
            )
        };
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
fn s_coldret(p: &mut Pipe, an: &mut Analysis) -> usize {
    let (cfg, domtree) = an.cfg_dt(&p.clctx.func);
    let k = clifpeep::coldrets(&mut p.clctx.func, cfg, domtree);
    if k > 0 && p.dump {
        p.dump("coldrets");
    }
    k
}

/// The target folds `base + reg*{1,2,4,8}` into a memory addressing mode
/// (x86 SIB; aarch64 extended-register offset). wasm/riscv compute
/// addresses explicitly — stepped pointer IVs still pay off there.
/// Callers whose emit target differs from `isa` (the wasm path keeps a
/// host ISA purely for bookkeeping) pass their own `Pipe::amode_scale`.
pub fn amode_scale(isa: &dyn TargetIsa) -> bool {
    use target_lexicon::Architecture as A;
    matches!(isa.triple().architecture, A::X86_64 | A::Aarch64(_))
}

fn s_indvars(p: &mut Pipe, an: &mut Analysis) -> usize {
    indvars::run(&mut p.clctx.func, an, p.amode_scale)
}

fn s_zipiv(p: &mut Pipe, an: &mut Analysis) -> usize {
    // Each CFG-changing sweep leaves the analyses stale: recompute and
    // sweep again (one sweep per folded loop, bounded).
    let mut k = 0;
    for _ in 0..16 {
        let CfgDtLa { cfg, dt, la } = an.cfg_dt_la(&p.clctx.func);
        let (n, changed) = crate::zipiv::run(&mut p.clctx.func, cfg, dt, la);
        k += n;
        if !changed {
            break;
        }
        an.invalidate(&p.clctx.func);
    }
    if k > 0 && p.dump {
        p.dump("zipiv");
    }
    k
}

fn s_loopdel(p: &mut Pipe, an: &mut Analysis) -> usize {
    loopdel::run(&mut p.clctx.func, an)
}

fn s_ivalign(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = ivalign::run(&mut p.clctx.func, an);
    if k > 0 {
        p.dump("ivalign");
    }
    k
}

fn s_ivretro(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = ivretro::run(&mut p.clctx.func, an);
    if k > 0 {
        p.dump("ivretro");
    }
    k
}

fn s_looprot(p: &mut Pipe, an: &mut Analysis) -> usize {
    let k = looprot::run(&mut p.clctx.func, an, p.derived);
    p.dump("looprot");
    k
}

const JT: Gate = Gate::Flag(FlagId::Jumpthread);

pub static DEFAULT: std::sync::LazyLock<Vec<Step>> = std::sync::LazyLock::new(|| {
    vec![
        Step {
            name: "jumpthread",
            bisect: Some("jumpthread"),
            stat: "",
            verify: true,
            mutates: true,
            // Needs at least one of: an edge (threading/chains/bypass), a
            // non-entry param (`deadp` cleans unreachable param'd blocks),
            // or an `icmp` (`fold_null_tests` — the only sub-phase that can
            // fire on a lone straightline block). Finer sub-phase guards
            // live inside `jumpthread::run`.
            cached: true,
            shape: 0,
            any: shape::MULTI | shape::BPARAM | shape::TESTC,
            creates: shape::CREATED,
            gate: JT,
            run: s_jumpthread,
            site: std::sync::OnceLock::new(),
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
            any: 0,
            creates: shape::CREATED,
            gate: Gate::All(&[JT, Gate::Env("PLIRON_UNROLL")]),
            run: s_unroll,
            site: std::sync::OnceLock::new(),
        },
        step("unreach", "", Gate::Flag(FlagId::Unreach), s_unreach).needs(shape::BLOCKS2),
        Step {
            name: "clifpeep",
            bisect: Some("clifpeep"),
            stat: "",
            verify: false,
            mutates: true,
            cached: true,
            shape: 0,
            any: 0,
            // `memcpy`/`memset` folding emits libcalls — CALL can appear.
            creates: u32::MAX,
            gate: Gate::Flag(FlagId::Peep),
            run: s_clifpeep,
            site: std::sync::OnceLock::new(),
        },
        step("tailmerge", "", Gate::Flag(FlagId::Tailmerge), s_tailmerge).needs(shape::BLOCKS2),
        step("taildup", "", Gate::Flag(FlagId::Taildup), s_taildup).needs(shape::BLOCKS2),
        step("tailrec", "tailrec", Gate::Flag(FlagId::Tailrec), s_tailrec).needs(shape::CALL),
        Step {
            name: "loadfwd",
            bisect: Some("loadfwd"),
            stat: "",
            verify: true,
            mutates: true,
            cached: true,
            // The `remove_dead_insts` cleanup in `s_loadfwd` can fire on
            // any function, so no step-level gate; the `loadfwd` dataflow
            // and dead-store sweep are LOAD/STORE-gated inside instead.
            shape: 0,
            any: 0,
            creates: shape::CREATED,
            gate: Gate::Flag(FlagId::Loadfwd),
            run: s_loadfwd,
            site: std::sync::OnceLock::new(),
        },
        // Stack-slot SSA promotion before the loop passes so the promoted
        // fields are loop-carried SSA values indvars/licm/bcheck can see.
        step(
            "slotprom",
            "slotprom",
            Gate::Env("PLIRON_SLOTPROM"),
            s_slotprom,
        )
        .needs(shape::MEM)
        .creates_all(),
        cstep("dse", "dse", Gate::Flag(FlagId::Dse), s_dse).needs(shape::STORE),
        Step {
            name: "egraph",
            bisect: Some("egraph"),
            stat: "",
            verify: false,
            mutates: true,
            cached: true,
            shape: 0,
            any: 0,
            // Rewrite-by-reconstruction can emit inst kinds the input
            // lacked.
            creates: u32::MAX,
            gate: Gate::OptIn("PLIRON_EGRAPH"),
            run: s_egraph,
            site: std::sync::OnceLock::new(),
        },
        // `prev` seed materialization emits loads.
        cstep("indvars", "indvars", Gate::Flag(FlagId::Indvars), s_indvars)
            .needs(shape::LOOPS)
            .creates_all(),
        step(
            "ldrot",
            "ldrot",
            Gate::All(&[Gate::Flag(FlagId::Indvars), Gate::Env("PLIRON_LDROT")]),
            s_ldrot,
        )
        .needs(shape::LOOPS | shape::MEM),
        cstep("licm", "licm", Gate::Flag(FlagId::Licm), s_licm)
            .needs(shape::LOOPS)
            .creates_all(),
        step("revnorm", "revnorm", Gate::Env("PLIRON_REVNORM"), s_revnorm).needs(shape::LOOPS),
        step(
            "loopidiom",
            "idiom",
            Gate::Flag(FlagId::Loopidiom),
            s_loopidiom,
        )
        .needs(shape::LOOPS | shape::MEM)
        // `memset`/`memcpy` idiom recognition emits libcalls.
        .creates_all(),
        step("lowbit", "lowbit", Gate::Env("PLIRON_LOWBIT"), s_lowbit)
            .needs(shape::LOOPS | shape::DIVREM),
        cstep(
            "ifconv2",
            "ifconv2",
            Gate::All(&[Gate::Flag(FlagId::Loopvec), Gate::Env("PLIRON_IFCONV2")]),
            s_ifconv2,
        )
        // `ifconv` needs the diamond (CONDBR+JUMP — checked inside); `cparm`
        // runs `webmerge` (BPARAM) + a brif const-sweep (CONDBR).
        .needs_any(shape::CONDBR | shape::BPARAM),
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
        cstep("loopvec", "loopvec", Gate::Flag(FlagId::Loopvec), s_loopvec)
            .needs(shape::LOOPS)
            .creates_all(),
        cstep("slp", "slp", Gate::Flag(FlagId::Slp), s_slp)
            .needs(shape::SLP)
            .creates_all(),
        // bcheck's per-loop version guards sit at the inner loop's entry —
        // their len/min/carry parts are invariant in every enclosing loop
        // but the only licm already ran. matmul's guard re-evaluates
        // `min(n-1, len)` and the offset chain ~n² times; hoist it.
        cstep("licm2", "licm", Gate::Flag(FlagId::Licm), s_licm)
            .needs(shape::LOOPS)
            .creates_all(),
        step("switchmap", "", Gate::Env("PLIRON_SWITCHMAP"), s_switchmap).needs(shape::SWITCH),
        Step {
            name: "constbr2",
            bisect: Some("constbr"),
            stat: "",
            verify: true,
            mutates: true,
            cached: false,
            shape: shape::CONDBR,
            any: 0,
            creates: shape::CREATED,
            gate: Gate::OptIn("PLIRON_CONSTBR"),
            run: s_constbr2,
            site: std::sync::OnceLock::new(),
        },
        cstep(
            "loopdel",
            "loopdel",
            Gate::All(&[JT, Gate::Env("PLIRON_LOOPDEL")]),
            s_loopdel,
        )
        // `fold_inc_overflow` needs only a `brif`; loop deletion needs
        // the back edge.
        .needs_any(shape::LOOPS | shape::CONDBR),
        cstep("looprot", "", Gate::Env("PLIRON_LOOPROT"), s_looprot).needs(shape::LOOPS),
        // Lockstep pointer IVs (`zip`/slice `cmp`/`eq`): one exit test.
        step("zipiv", "zipiv", Gate::OptIn("PLIRON_ZIPIV"), s_zipiv).needs(shape::LOOPS),
        // Re-time rotated loops that test the pre-increment IV so the latch
        // tests `iadd p, s` directly — drops the backedge register copy.
        step("ivalign", "ivalign", Gate::Env("PLIRON_IVALIGN"), s_ivalign).needs(shape::LOOPS),
        // Scalar unroll of linear counted loops (e.g. ordered float
        // reductions that can't vectorize): K bodies per branch.
        cstep(
            "punroll",
            "punroll",
            Gate::All(&[JT, Gate::Env("PLIRON_PUNROLL")]),
            s_punroll,
        )
        .needs(shape::LOOPS),
        // punroll clones whole blocks, including in-header insts an earlier
        // hoist could not move (e.g. bcheck's versioned loop header: the
        // thread-base chain is cloned K times into the unrolled group and
        // would recompute per iteration).
        cstep("licm3", "", Gate::Flag(FlagId::Licm), s_licm)
            .needs(shape::LOOPS)
            .creates_all(),
        // Merge identical constants across dominated blocks — unroll/clone
        // passes stamp fresh `iconst`s per copy, and each one is a `mov`
        // downstream instead of a shared register (or `wzr`).
        step(
            "constdedup",
            "constdedup",
            Gate::Env("PLIRON_CSTDEDUP"),
            s_constdedup,
        )
        .needs(shape::ICONST),
        Step {
            name: "frozen_loads",
            bisect: None,
            stat: "",
            verify: false,
            mutates: true,
            cached: false,
            shape: 0,
            any: 0,
            creates: shape::CREATED,
            gate: Gate::Always,
            run: s_frozen_loads,
            site: std::sync::OnceLock::new(),
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
            any: 0,
            creates: shape::CREATED,
            gate: Gate::All(&[Gate::Flag(FlagId::Peep), Gate::Env("PLIRON_COMMUTE")]),
            run: s_commute,
            site: std::sync::OnceLock::new(),
        },
        Step {
            name: "wide_minmax",
            bisect: None,
            stat: "",
            verify: false,
            mutates: true,
            cached: false,
            shape: 0,
            any: 0,
            creates: shape::CREATED,
            gate: Gate::Always,
            run: s_wide_minmax,
            site: std::sync::OnceLock::new(),
        },
        Step {
            name: "coldarg",
            bisect: Some("coldarg"),
            stat: "coldarg",
            verify: false,
            mutates: true,
            cached: false,
            shape: 0,
            any: 0,
            creates: shape::CREATED,
            gate: Gate::Env("PLIRON_COLDARG"),
            run: s_coldarg,
            site: std::sync::OnceLock::new(),
        },
        step("fuse", "fuse", Gate::Env("PLIRON_FUSE"), s_fuse).needs(shape::MULTI),
        cstep("edgefwd", "edgefwd", Gate::Env("PLIRON_EDGEFWD"), s_edgefwd)
            .needs_any(shape::MULTI | shape::BLOCKS2),
        step("foldf", "foldf", Gate::Env("PLIRON_FOLDF"), s_foldf).needs(shape::JUMP),
        step("pushin", "pushin", Gate::OptIn("PLIRON_PUSHIN"), s_pushin)
            .needs(shape::CONDBR),
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
        )
        // The `webmerge` loop needs a block param; the `const_sweep` tail
        // only needs an `iconst` to fold through.
        .needs_any(shape::BPARAM | shape::ICONST),
        // `load (select)` speculation once the compare-exchange diamonds
        // are selects (fuse/sameargs ran) — early enough that loadfwd2 can
        // forward the network's own stores into the fresh arm loads.
        step("loadsel", "loadsel", Gate::Env("PLIRON_LDSEL"), s_loadsel).needs(shape::LOAD),
        step(
            "loadfwd2",
            "loadfwd2",
            Gate::Flag(FlagId::Loadfwd),
            s_loadfwd2,
        )
        .needs(shape::LOAD),
        step("mask01", "mask01", Gate::Env("PLIRON_MASK01"), s_mask01),
        cstep(
            "edgespec",
            "edgespec",
            Gate::Env("PLIRON_EDGESPEC"),
            s_edgespec,
        )
        // Plans need a `jump`/`brif` predecessor: pure-`jump` forwarder
        // chains resolve too, not just conditional dispatches.
        .needs_any(shape::JUMP | shape::CONDBR | shape::BPARAM | shape::EPARAM),
        cstep("bittab", "bittab", Gate::Env("PLIRON_BITTAB"), s_bittab).needs(shape::CONDBR),
        step(
            "coldedge",
            "coldedge",
            Gate::Env("PLIRON_COLDEDGE"),
            s_coldedge,
        ),
        cstep("dse2", "dse2", Gate::Env("PLIRON_DSE2"), s_dse2).needs(shape::STORE),
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
            any: 0,
            creates: shape::CREATED,
            gate: Gate::Env("PLIRON_LCSE"),
            run: s_lcse,
            site: std::sync::OnceLock::new(),
        },
        step("uload32", "uload32", Gate::Env("PLIRON_ULOAD32"), s_uload32).needs(shape::LOAD),
        // `icmp p, K` count latches become `icmp p+s, K+s` — the test folds
        // into the already-computed step value (one-register `add;cmp;jcc`)
        // and leaves boundcmp's post-inc shape behind.
        step("postinc", "postinc", Gate::Env("PLIRON_POSTINC"), s_postinc).needs(shape::LOOPS),
        step("boundcmp", "boundcmp", Gate::Env("PLIRON_BCMP"), s_boundcmp).needs(shape::LOOPS),
        step("selfarg", "selfarg", Gate::Env("PLIRON_SELFARG"), s_selfarg)
            .needs(shape::BPARAM),
        step("brchain", "brchain", Gate::Env("PLIRON_BRCHAIN"), s_brchain).needs(shape::SWITCH),
        // Sink `splat(const)` defs into their use blocks: a cross-region live
        // splat becomes a spill reload per loop iteration; a local one is a
        // single `movi`/`dup` at the use.
        step("splink", "splink", Gate::Env("PLIRON_SPLINK"), s_splink).needs(shape::BLOCKS2),
        // Flag-merge brif blocks (a bool merged through a param, then tested)
        // only settle after edgespec/celim — dup them onto const edges here too.
        step("taildup2", "", Gate::Flag(FlagId::Taildup), s_taildup).needs(shape::BLOCKS2),
        // Late dispatch-block duplication + edge re-spec: collapses the
        // `select -> ireduce -> icmp -> brif` dispatch tail onto a direct brif
        // on the flag (tokenize's block388/block859 discriminant chain —
        // only pays off once jtround can repair multi-pred targets, so this
        // sits right before the looping jtround).
        // Everything inside needs an edge: `merge_chains` a `jump` chain,
        // the dispatch/brif work a conditional (guarded per-phase inside
        // for the CONDBR-less case the old gate used to skip wholesale).
        step("mergechain", "", Gate::Env("PLIRON_LATESPEC"), s_mergechain).needs(shape::MULTI),
        // Select-chain folds `clifpeep` missed while the arms still sat
        // behind block params that mergechain just merged (eqbug).
        step("selchain", "", Gate::OptIn("PLIRON_SELCHAIN"), s_selchain),
        step(
            "jtround",
            "threaded",
            Gate::Env("PLIRON_JTROUND"),
            s_jtround,
        )
        .needs(shape::CONDBR),
        cstep("slotmerge", "", Gate::Env("PLIRON_SLOTMERGE"), s_slotmerge).needs(shape::SLOT),
        step("memcall", "memcall", Gate::Env("PLIRON_MEMCALL"), s_memcall).needs(shape::CALL),
        // Fold constants any earlier pass left behind — late value folds
        // (dgvn/sameargs) can materialize `iconst`s under `ineg`/`ushr`/
        // `ishl` chains the early clifpeep round never saw (matmul's
        // `((-(-216))>>3)<<6` vector-trip bound recomputed per iteration).
        step("csweep2", "", Gate::Env("PLIRON_CSWEEP2"), s_csweep2).needs(shape::ICONST),
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
            any: 0,
            creates: shape::CREATED,
            gate: Gate::Env("PLIRON_LCSE"),
            run: s_lcse,
            site: std::sync::OnceLock::new(),
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
        )
        // The `webmerge` loop needs a block param; the `const_sweep` tail
        // only needs an `iconst` to fold through.
        .needs_any(shape::BPARAM | shape::ICONST),
        // (The old `arghoist` position was a second webmerge call right
        // after sameargs2's fixpoint — webmerge now hoists same-shape edge
        // args via phase-B `P`-keys, so nothing was left for it.)
        // Every candidate is a use-chain rooted at an existing block
        // param — `run` skips param-less blocks outright.
        step(
            "paramsink",
            "paramsink",
            Gate::Env("PLIRON_PARAMSINK"),
            s_paramsink,
        )
        .needs(shape::BPARAM),
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
        // Second ivalign slot: paramsink is what re-times loop params into
        // the `icmp p, bound` / `iadd p, s` backedge shape (sieve's marking
        // loop), so the pattern rarely exists at the post-looprot position.
        // postinc covers only constant bounds — a runtime bound needs the
        // edge retiming.
        step(
            "ivalign2",
            "ivalign",
            Gate::Env("PLIRON_IVALIGN"),
            s_ivalign,
        )
        .needs(shape::LOOPS),
        // Literal `base + arg_iv*K` backedge args indvars seeded while
        // the loop was multi-block — collapsed latches can forward
        // `p + s*K` instead.
        step(
            "ivretro",
            "ivretro",
            Gate::Env("PLIRON_IVRETRO"),
            s_ivretro,
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
        )
        // The `webmerge` loop needs a block param; the `const_sweep` tail
        // only needs an `iconst` to fold through.
        .needs_any(shape::BPARAM | shape::ICONST),
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
            any: 0,
            creates: shape::CREATED,
            gate: Gate::Env("PLIRON_DEADP"),
            run: |p, an| {
                let (cfg, _) = an.cfg_dt(&p.clctx.func);
                jumpthread::remove_dead_params(&mut p.clctx.func, cfg)
            },
            site: std::sync::OnceLock::new(),
        }
        .needs(shape::BPARAM),
        // Phi merge through affine congruence: every in-edge of a param
        // carries the same `root + k` class (`proots::affine_nf` — Ascent),
        // so distinct SSA values that are transposed copies of one carrier
        // still merge. Catches the shapes `taildup`'s per-edge clones and
        // jumpthread's copied `iadd`s leave behind.
        step(
            "webmerge",
            "webmerge",
            Gate::Env("PLIRON_WEBMERGE"),
            s_webmerge,
        )
        .needs(shape::MULTI | shape::BPARAM),
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
        // `icmp cc x, k` folds the bounds decide, round 2: the niche
        // `select`s only collapse to params after sameargs3/webmerge, so
        // `sublea`'s `ovkill` ran while `icmp eq (select c,x,-1), -1` still
        // carried the unbounded arm — `Chars::next`'s `-1` decode check.
        cstep("ovkill2", "ovkill", Gate::Env("PLIRON_OVKILL"), s_ovkill2)
            .needs(shape::TESTC),
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
        step("memspec", "memspec", Gate::Env("PLIRON_MEMSPEC"), s_memspec)
            .needs(shape::MEM)
            .creates_all(),
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
            any: 0,
            creates: shape::CREATED,
            gate: Gate::Env("PLIRON_LCSE"),
            run: s_lcse,
            site: std::sync::OnceLock::new(),
        },
        // Third DSE: the late edge/jtr cleanup can delete a slot's last
        // readers after `dse2` ran (SipHash's `&key` tail-byte loads —
        // hashmap's per-iteration `x` store dies only once the tail branch
        // structure resolves) — the orphaned store would otherwise reach
        // codegen as a stack write per loop iteration.
        cstep("dse3", "dse3", Gate::Env("PLIRON_DSE3"), s_dse3).needs(shape::STORE),
        // Loop-latch merges (`brif …, tail, …` where `tail` loops back) only
        // shrink under MAX_INSTS after the late folds — clone them last so the
        // loop's hot tail lands on the fallthrough edge instead of a `jmp`
        // per iteration (chars_*/tokenize dispatch).
        step("taildup3", "", Gate::Flag(FlagId::Taildup), s_taildup).needs(shape::BLOCKS2),
        // taildup's per-edge clones leave congruent `iadd`/`isub` args on a
        // shared successor — one more merge round after the last clone step.
        step(
            "webmerge2",
            "webmerge",
            Gate::Env("PLIRON_WEBMERGE"),
            s_webmerge,
        )
        .needs(shape::MULTI | shape::BPARAM),
        // taildup/jtr twin-arm clones (`binary_search`'s two update paths)
        // differ only in operand values — merge into one block with `select`s,
        // turning a data-dependent branch per iteration into `cmov`s.
        cstep("cicm", "cicm", Gate::OptIn("PLIRON_CICM"), s_cicm).needs(shape::CONDBR),
        cstep(
            "armmerge",
            "armmerge",
            Gate::Env("PLIRON_ARMMERGE"),
            s_armmerge,
        )
        .needs(shape::CONDBR),
        // The late jtr/mergechain/taildup rounds stamp a fresh `iconst` per
        // block copy (a `mov #10` before every unrolled `madd` — parse_only);
        // merge them back to the dominating def so one register serves the
        // loop instead of a remat per block.
        step(
            "constdedup2",
            "constdedup",
            Gate::Env("PLIRON_CSTDEDUP"),
            s_constdedup,
        )
        .needs(shape::ICONST),
        // Error/merge payloads computed in `b` but used only by one
        // `brif` edge's args (parse_only's `ParseIntError` pack on every
        // digit-validity branch) run unconditionally there — sink them
        // onto the edge so the taken path alone pays.
        step(
            "edgesink",
            "edgesink",
            Gate::Env("PLIRON_EDGESINK"),
            s_edgesink,
        )
        // The `brif` sink loop needs CONDBR; `push_to_use_blocks` only
        // needs a second block to push into.
        .needs_any(shape::CONDBR | shape::BLOCKS2),
        // `acc + a + b + c` chains rooted at a block param keep only
        // one add on the loop-carried edge; runs late so it sees the
        // final chain shapes.
        step("reassoc", "reassoc", Gate::OptIn("PLIRON_REASSOC"), s_reassoc)
            .needs(shape::BPARAM),
        // i64->i32 low-word demotion — after the late folds so their
        // residuals demote too, before coldret so block layout is final.
        // Last, once sameargs/edge folds left the loop-entry arg a plain 0.
        step("mulcarry", "", Gate::OptIn("PLIRON_MULCARRY"), s_mulcarry).needs(shape::LOOPS),
        step("dem64", "dem64", Gate::Env("PLIRON_DEM64"), s_dem64),
        Step {
            name: "coldret",
            bisect: Some("coldret"),
            stat: "coldret",
            verify: false,
            mutates: true,
            cached: false,
            // Every candidate is a `brif` arm whose target is a leaf
            // `return` block.
            shape: shape::CONDBR,
            any: 0,
            creates: shape::CREATED,
            gate: Gate::Env("PLIRON_COLDRET"),
            run: s_coldret,
            site: std::sync::OnceLock::new(),
        },
        // Legality, not optimization: `iconst.i128` reaches isel with no
        // lowering rule and panics. Any pass can mint one, so sweep last.
        Step {
            name: "iconst128",
            bisect: None,
            stat: "",
            verify: false,
            mutates: true,
            cached: false,
            shape: 0,
            any: 0,
            creates: shape::CREATED,
            gate: Gate::Always,
            run: |p, _an| {
                let reap = p.since_reap;
                p.since_reap = false;
                clifpeep::iconst128(&mut p.clctx.func, reap)
            },
            site: std::sync::OnceLock::new(),
        },
        // After everything: the `end + -1` bound and the stepped pointer
        // only settle into one block at the end (parse_only).
        step("stepcmp", "", Gate::OptIn("PLIRON_STEPCMP"), s_stepcmp).needs(shape::LOOPS),
    ]
});

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
        let func = cranelift_reader::parse_functions(OPT_PTR)
            .unwrap()
            .remove(0);
        let v = |n: u32| Value::from_u32(n);
        let (v1, v2, v4) = (v(1), v(2), v(4));
        assert!(matches!(func.dfg.value_def(v1), ValueDef::Result(..)));
        assert!(matches!(func.dfg.value_def(v2), ValueDef::Result(..)));
        assert!(matches!(func.dfg.value_def(v4), ValueDef::Param(..)));

        let isa = host_isa();
        let mut clctx = Context::for_function(func);
        let nowrite: FxHashMap<FuncRef, bool> = Default::default();
        let call_frozen: FxHashMap<FuncRef, u64> = Default::default();
        let nounwind: FxHashSet<FuncRef> = Default::default();
        let scoped: FxHashMap<Inst, Ptr<BasicBlock>> = Default::default();
        let invariant: FxHashSet<Inst> = Default::default();
        let block_region: FxHashMap<Block, Ptr<BasicBlock>> = Default::default();
        let prof: FxHashMap<Block, u64> = Default::default();
        let region_parent: pliron::cols::ColMap<Ptr<BasicBlock>, Ptr<BasicBlock>> =
            Default::default();
        let nocap: FxHashSet<(Inst, u32)> = Default::default();
        let mut nonnull = FxHashSet::default();
        nonnull.insert(v4);
        let mut derived = VMap::default();
        let mut frozen = FxHashMap::default();
        let mut noalias = FxHashSet::default();
        let mut scoped_na = FxHashSet::default();
        let mut deref = FxHashMap::default();
        let mut operand_frozen = FxHashMap::default();
        let mut lifetimes = Vec::new();
        let mut stats = Stats::default();
        let mut fact_bits = SecondaryMap::new();
        let flags = FnFlags::default();
        let uen_syms = FxHashMap::default();
        let mut p = Pipe {
            fact_bits: &mut fact_bits,
            clctx: &mut clctx,
            isa: &*isa,
            fname: "carry_keeps_param_facts_scoped",
            func_id: FuncId::from_u32(0),
            uen_syms: &uen_syms,
            nonnull: &mut nonnull,
            derived: &mut derived,
            frozen: &mut frozen,
            noalias: &mut noalias,
            scoped_na: &mut scoped_na,
            deref: &mut deref,
            nowrite: &nowrite,
            call_frozen: &call_frozen,
            nounwind: &nounwind,
            scoped: &scoped,
            invariant: &invariant,
            operand_frozen: &mut operand_frozen,
            block_region: &block_region,
            prof: &prof,
            region_parent: &region_parent,
            inst_region: Default::default(),
            lifetimes: &mut lifetimes,
            nocap: &nocap,
            dbg_slots: &Default::default(),
            tcfg: isa.frontend_config(),
            simd: true,
            x64: false,
            cmp64: true,
            amode_scale: amode_scale(&*isa),
            wasm: false,
            has_eh: false,
            flags: &flags,
            prof_hook: None,
            dump: false,
            verify_full: false,
            unopt: false,
            since_reap: false,
            stats: &mut stats,
            carry_log: Vec::new(),
            fn_names: &[],
            cur_step: "",
        };
        p.seed_fact_bits();
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
        p.fact_bits[v1] |= fact_bits::NONNULL;
        carry_one(&mut p, v1, v2);
        assert!(
            p.nonnull.contains(&v2),
            "same-block result->result carry dropped a covered fact"
        );
    }
}
