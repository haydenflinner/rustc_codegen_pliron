//! The per-function CLIF optimization pipeline as data.
//!
//! `DEFAULT` lists the steps in their baked order; `PLIRON_PIPELINE=a,b,c`
//! overrides selection and order by step `name` (bisect still applies —
//! env gates are bypassed since listing a step is the explicit request).
//! `Gate::Always` steps are correctness lowerings, not optimizations:
//! they run even under a custom pipeline, appended in table order.

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::{Block, FuncRef, Inst, Value};
use cranelift_codegen::isa::{TargetFrontendConfig, TargetIsa};
use cranelift_module::FuncId;
use pliron::basic_block::BasicBlock;
use pliron::context::Ptr;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::config::{bisect, config, env_set};
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
#[derive(Default)]
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
    }
}

/// Everything a step needs beyond `&mut Function`.
pub struct Pipe<'a> {
    pub clctx: &'a mut Context,
    pub isa: &'a dyn TargetIsa,
    pub fname: &'a str,
    pub func_id: FuncId,
    pub nonnull: &'a FxHashSet<Value>,
    pub derived: &'a mut FxHashMap<Value, Value>,
    pub frozen: &'a FxHashMap<Value, u64>,
    pub noalias: &'a FxHashSet<Value>,
    pub deref: &'a FxHashMap<Value, (u64, bool)>,
    pub nowrite: &'a FxHashMap<FuncRef, bool>,
    pub call_frozen: &'a FxHashMap<FuncRef, u64>,
    pub scoped: &'a FxHashMap<Inst, Ptr<BasicBlock>>,
    pub invariant: &'a FxHashSet<Inst>,
    pub operand_frozen: &'a FxHashMap<Value, Vec<(u64, Ptr<BasicBlock>)>>,
    pub block_region: &'a FxHashMap<Block, Ptr<BasicBlock>>,
    pub region_parent: &'a FxHashMap<Ptr<BasicBlock>, Ptr<BasicBlock>>,
    pub lifetimes: &'a [(Value, Option<Inst>, Block, bool)],
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
}

impl Pipe<'_> {
    fn dump(&self, tag: &str) {
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
    pub gate: Gate,
    pub run: fn(&mut Pipe) -> usize,
}

const fn step(
    name: &'static str,
    stat: &'static str,
    gate: Gate,
    run: fn(&mut Pipe) -> usize,
) -> Step {
    Step {
        name,
        bisect: Some(name),
        stat,
        verify: true,
        gate,
        run,
    }
}

pub fn run_pipeline(p: &mut Pipe) {
    if let Some(list) = &config().pipeline {
        for name in list {
            match DEFAULT
                .iter()
                .find(|s| s.name == name.as_str() && !matches!(s.gate, Gate::Always))
            {
                Some(s) => run_step(p, s),
                None => eprintln!("PLIRON_PIPELINE: unknown step `{name}`"),
            }
        }
        for s in DEFAULT
            .iter()
            .filter(|s| matches!(s.gate, Gate::Always))
        {
            run_step(p, s);
        }
    } else {
        for s in DEFAULT {
            if s.gate.ok(p) {
                run_step(p, s);
            }
        }
    }
}

fn run_step(p: &mut Pipe, s: &Step) {
    if let Some(b) = s.bisect
        && !bisect(b)
    {
        return;
    }
    let k = (s.run)(p);
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

fn s_jumpthread(p: &mut Pipe) -> usize {
    let (n, dead, folds) = jumpthread::run(&mut p.clctx.func, p.nonnull, p.derived);
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
fn s_constbr(p: &mut Pipe) -> usize {
    let k = jumpthread::fold_const_branches(&mut p.clctx.func);
    if k > 0 {
        jumpthread::remove_unreachable_blocks(&mut p.clctx.func);
    }
    k
}

fn s_unroll(p: &mut Pipe) -> usize {
    let k = unroll::run(&mut p.clctx.func, p.nonnull, p.derived);
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
            eprintln!("unroll cleanup {}: {ub} blocks, {dp} params, {di} insts", p.fname);
        }
    }
    if k > 0 && env_set("PLIRON_UNROLL_DEBUG") {
        eprintln!("unroll {k} {}", p.fname);
    }
    p.dump("unroll");
    k
}

fn s_unreach(p: &mut Pipe) -> usize {
    let k = unreach::run(&mut p.clctx.func);
    p.stats.unreached += k;
    k
}

fn s_clifpeep(p: &mut Pipe) -> usize {
    let k = clifpeep::run(&mut p.clctx.func);
    p.stats.peeped += k;
    k
}

fn s_tailmerge(p: &mut Pipe) -> usize {
    let k = tailmerge::run(&mut p.clctx.func);
    p.stats.merged += k;
    k
}

fn s_taildup(p: &mut Pipe) -> usize {
    let k = taildup::run(&mut p.clctx.func);
    p.stats.dupd += k;
    k
}

// `f(a) + f(b)`-style recursion → accumulate the first call and
// loop on the second's args (LLVM TRE); before loadfwd/licm so
// they can still hoist out of the new back-edge.
fn s_tailrec(p: &mut Pipe) -> usize {
    let k = tailrec::run(&mut p.clctx.func, p.func_id);
    if k > 0 && p.dump {
        p.dump("tailrec");
    }
    k
}

fn s_loadfwd(p: &mut Pipe) -> usize {
    p.stats.forwarded += loadfwd::run(
        &mut p.clctx.func,
        p.noalias,
        p.invariant,
        p.operand_frozen,
        p.block_region,
        p.region_parent,
        p.nowrite,
        p.call_frozen,
    );
    if p.flags.slot_dse {
        // A load whose only user was a removed dead store keeps its own
        // slot alive, so alternate the two until nothing changes.
        for _ in 0..4 {
            p.stats.slot_dse += loadfwd::dead_slot_stores(&mut p.clctx.func);
            if !config().enabled("PLIRON_DEAD_LOADS")
                || jumpthread::remove_dead_insts(
                    &mut p.clctx.func,
                    config().enabled("PLIRON_DEAD_PURE"),
                ) == 0
            {
                break;
            }
        }
    }
    p.dump("loadfwd");
    0
}

fn s_dse(p: &mut Pipe) -> usize {
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
fn s_egraph(p: &mut Pipe) -> usize {
    let has_try_call = p
        .clctx
        .func
        .layout
        .blocks()
        .flat_map(|b| p.clctx.func.layout.block_insts(b))
        .any(|i| p.clctx.func.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::TryCall);
    if has_try_call {
        return 0;
    }
    let mut cp = ControlPlane::default();
    // The egraph indexes values directly; our passes leave
    // `Value` aliases it doesn't resolve.
    p.clctx.func.dfg.resolve_all_aliases();
    p.clctx.compute_cfg();
    p.clctx.compute_domtree();
    p.clctx
        .egraph_pass(p.isa, &mut cp)
        .unwrap_or_else(|e| panic!("egraph broke `{}`: {e}\n{}", p.fname, p.clctx.func.display()));
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
fn s_ldrot(p: &mut Pipe) -> usize {
    indvars::rotld(&mut p.clctx.func)
}

fn s_licm(p: &mut Pipe) -> usize {
    let k = licm::run(
        &mut p.clctx.func,
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
fn s_revnorm(p: &mut Pipe) -> usize {
    revnorm::run(&mut p.clctx.func)
}

fn s_loopidiom(p: &mut Pipe) -> usize {
    loopidiom::run(&mut p.clctx.func, p.noalias, p.tcfg, p.fname)
}

// A second ifconv round: loadfwd/egraph dedup re-loaded predicate
// operands that made jumpthread's arm blocks non-pure, leaving
// `filter(pred)` range-check diamonds folded only now that they're
// pure forwarders. Flattening them to `select` chains is what lets
// loopvec see the count-if/conditional-update shapes.
fn s_ifconv2(p: &mut Pipe) -> usize {
    // Carried-constant params (`RangeInclusive`'s exhausted flag
    // — a self-edge param fed a literal at entry) fold here so
    // the dispatch arms they drive die before flattening
    // (`PLIRON_CPARM`).
    if config().enabled("PLIRON_CPARM") {
        let k = clifpeep::cparm(&mut p.clctx.func);
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
fn s_unswitch(p: &mut Pipe) -> usize {
    let k = bcheck::unswitch(&mut p.clctx.func, p.fname);
    if k > 0 && p.dump {
        p.dump("unswitch");
    }
    k
}

// bcheck before loopvec: the versioned fast path has no per-iteration
// bounds branches, so it's vectorizable where the checked loop isn't.
fn s_bcheck(p: &mut Pipe) -> usize {
    let k = bcheck::run(&mut p.clctx.func, p.fname, p.has_eh);
    p.dump("bcheck");
    k
}

fn s_loopvec(p: &mut Pipe) -> usize {
    let k = loopvec::run(
        &mut p.clctx.func,
        p.fname,
        p.noalias,
        p.tcfg,
        p.simd,
        p.x64,
        p.cmp64,
    );
    // loopvec emits `base + iv*esize` addressing fresh each iteration;
    // a second indvars round turns them into stepped pointers.
    if k > 0
        && p.flags.indvars
        && config().enabled("PLIRON_INDUCT2")
        && bisect("indvars2")
    {
        indvars::run(&mut p.clctx.func);
    }
    k
}

fn s_slp(p: &mut Pipe) -> usize {
    let k = slp::run(&mut p.clctx.func, p.noalias, p.simd);
    if env_set("PLIRON_XPACK_CLIF") {
        eprintln!(
            "==== clif {} after slp ====\n{}",
            p.fname,
            p.clctx.func.display()
        );
    }
    k
}

fn s_switchmap(p: &mut Pipe) -> usize {
    let k = switchmap::run(&mut p.clctx.func);
    // The new bounds tests are often implied by a dominating check.
    let f = if k > 0
        && config().enabled("PLIRON_DOMCOND")
        && config().enabled("PLIRON_SWITCHMAP_FOLD")
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

fn s_constbr2(p: &mut Pipe) -> usize {
    let k = jumpthread::fold_const_branches(&mut p.clctx.func);
    if k > 0 && env_set("PLIRON_CONSTBR_DEBUG") {
        eprintln!("constbr {k} {}", p.fname);
    }
    k
}

fn s_punroll(p: &mut Pipe) -> usize {
    let k = punroll::run(&mut p.clctx.func, p.fname);
    if k > 0 && p.dump {
        p.dump("punroll");
    }
    k
}

fn s_frozen_loads(p: &mut Pipe) -> usize {
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
fn s_sublea(p: &mut Pipe) -> usize {
    clifpeep::sublea(&mut p.clctx.func)
}

// `op multi_use, one_use` → `op one_use, multi_use` on commutative
// binops: the fewer-use operand takes two-address dst, so regalloc
// clobbers it in place instead of copying the live one (`movdqa`
// splat copies in matmul's `mulpd` loop).
fn s_commute(p: &mut Pipe) -> usize {
    commute::run(&mut p.clctx.func)
}

// Scalar min/max wider than a register (i128) has no Cranelift
// lowering on any target; several passes can emit it.
fn s_wide_minmax(p: &mut Pipe) -> usize {
    clifpeep::wide_minmax(&mut p.clctx.func)
}

// Rebind cold-call args defined in hot code so their ABI register
// pinning lands at the cold call site instead of constraining the
// whole hot loop's register allocation.
fn s_coldarg(p: &mut Pipe) -> usize {
    let k = clifpeep::coldargs(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("coldargs");
    }
    k
}

// Splice single-predecessor successors into their predecessor so
// hot loops broken into several blocks by cold-edge branches lose
// the per-edge register-allocation copy shuffle.
fn s_fuse(p: &mut Pipe) -> usize {
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
fn s_edgefwd(p: &mut Pipe) -> usize {
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
fn s_foldf(p: &mut Pipe) -> usize {
    let k = clifpeep::foldforwarders(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("foldf");
    }
    k
}

// Fold compares decided by dominating branch conditions (bounds
// checks implied by earlier checks) before the cold-edge split:
// a folded check's dead panic edge then never gets an adapter.
fn s_celim(p: &mut Pipe) -> usize {
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
fn s_ivrefold(p: &mut Pipe) -> usize {
    let k = ivrefold::run(&mut p.clctx.func);
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
fn s_deadrec(p: &mut Pipe) -> usize {
    let k = ivrefold::deadrecs(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("deadrec");
    }
    k
}

// Per-block CSE of duplicate pure insts so redundant chains
// (the vectorizer's separate load/store address streams,
// rustc's repeated index math) collapse to one value before
// sameargs merges the now-identical edge args.
fn s_lcse(p: &mut Pipe) -> usize {
    let k = clifpeep::lcse(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("lcse");
    }
    k
}

// Drop block params that receive the same value on every
// incoming edge (and merge duplicate params): each surviving
// param costs a regalloc parallel copy per conditional pred —
// the `mov;b` edge-split blocks that dominate the block count.
// Runs before coldedges so adapters inherit smaller arg lists.
fn s_sameargs(p: &mut Pipe) -> usize {
    if env_set("PLIRON_SA_DEBUG") || env_set("PLIRON_PHI_DEBUG") {
        eprintln!("--- sameargs {}", p.fname);
    }
    let k = clifpeep::sameargs(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("sameargs");
    }
    k
}

// `band v, 1` on an already-0/1 scalar (rustc's i1 normalization
// residue around `icmp`/`uextend`/`select` booleans) — one `and`
// per element in scalar count_if loops. After sameargs: the merged
// predicate param is only alias-visible once it has dropped.
fn s_mask01(p: &mut Pipe) -> usize {
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
fn s_edgespec(p: &mut Pipe) -> usize {
    let k = edgespec::run(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("edgespec");
    }
    k
}

// Sparse small-domain membership trees (is_whitespace/is_ascii_*
// `icmp`+`brif` chains) become one guarded `ushr` bitmask test when
// the accepted set fits a 64-bit window.
fn s_bittab(p: &mut Pipe) -> usize {
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
fn s_coldedge(p: &mut Pipe) -> usize {
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
fn s_dse2(p: &mut Pipe) -> usize {
    let k = dse::run(&mut p.clctx.func, p.noalias, p.nowrite, p.call_frozen);
    if k > 0 && p.dump {
        p.dump("dse2");
    }
    k
}

// Lane-by-lane constant stores into a stack slot that whole-vector
// loads read back (rustc's `Simd` const materialization, e.g.
// hashbrown's `u8x8` special-byte checks in `reserve_rehash`) fold
// to `iconst`+`bitcast`, and the lane stores go dead.
fn s_vecslot(p: &mut Pipe) -> usize {
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
fn s_uload32(p: &mut Pipe) -> usize {
    let k = clifpeep::uloads32(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("uload32");
    }
    k
}

// Loop count tests against a wide constant rematerialize it per
// iteration; compare `(a - j0) cc (K - j0)` instead — the bound is
// a non-constant preheader value codegen pins in a register. Runs
// late (after loop rotation) so latch-copied tests are covered.
fn s_boundcmp(p: &mut Pipe) -> usize {
    let k = indvars::boundcmp(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("boundcmp");
    }
    k
}

// Dead self-feed recurrences — boundcmp's countdown replaces the
// bound-tested iv with `rem`, leaving the iv's `+=` update feeding
// only its own edge arg. Runs after boundcmp/looprot.
fn s_selfarg(p: &mut Pipe) -> usize {
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
fn s_brchain(p: &mut Pipe) -> usize {
    let k = brchain::run(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("brchain");
    }
    k
}

// Merge disjoint-lifetime explicit slots — last, so slot references
// cloned by earlier passes are all accounted for before sharing.
fn s_slotmerge(p: &mut Pipe) -> usize {
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
        eprintln!("slotmerge {k} {before}->{after}B {} {}", p.nocap.len(), p.fname);
    }
    if k > 0 && p.dump {
        p.dump("slotmerge");
    }
    k
}

// Leaf `return` blocks that share a `brif` with a loop header are
// colder than the loop: mark them cold so the emitted RPO order puts
// them last and the loop arm falls through. Runs last — it's pure
// layout metadata, so nothing downstream needs to see it.
fn s_coldret(p: &mut Pipe) -> usize {
    let k = clifpeep::coldrets(&mut p.clctx.func);
    if k > 0 && p.dump {
        p.dump("coldrets");
    }
    k
}

fn s_indvars(p: &mut Pipe) -> usize {
    indvars::run(&mut p.clctx.func)
}

fn s_loopdel(p: &mut Pipe) -> usize {
    loopdel::run(&mut p.clctx.func)
}

fn s_looprot(p: &mut Pipe) -> usize {
    let k = looprot::run(&mut p.clctx.func);
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
        gate: JT,
        run: s_jumpthread,
    },
    step("constbr", "constbr", Gate::All(&[JT, Gate::Env("PLIRON_CONSTBR")]), s_constbr),
    Step {
        name: "unroll",
        bisect: Some("unroll"),
        stat: "",
        verify: true,
        gate: Gate::All(&[JT, Gate::Env("PLIRON_UNROLL")]),
        run: s_unroll,
    },
    step("unreach", "", Gate::Flag(FlagId::Unreach), s_unreach),
    Step {
        name: "clifpeep",
        bisect: Some("clifpeep"),
        stat: "",
        verify: false,
        gate: Gate::Flag(FlagId::Peep),
        run: s_clifpeep,
    },
    step("tailmerge", "", Gate::Flag(FlagId::Tailmerge), s_tailmerge),
    step("taildup", "", Gate::Flag(FlagId::Taildup), s_taildup),
    step("tailrec", "tailrec", Gate::Flag(FlagId::Tailrec), s_tailrec),
    Step {
        name: "loadfwd",
        bisect: Some("loadfwd"),
        stat: "",
        verify: true,
        gate: Gate::Flag(FlagId::Loadfwd),
        run: s_loadfwd,
    },
    step("dse", "dse", Gate::Flag(FlagId::Dse), s_dse),
    Step {
        name: "egraph",
        bisect: Some("egraph"),
        stat: "",
        verify: false,
        gate: Gate::OptIn("PLIRON_EGRAPH"),
        run: s_egraph,
    },
    step("indvars", "indvars", Gate::Flag(FlagId::Indvars), s_indvars),
    step(
        "ldrot",
        "ldrot",
        Gate::All(&[Gate::Flag(FlagId::Indvars), Gate::Env("PLIRON_LDROT")]),
        s_ldrot,
    ),
    step("licm", "licm", Gate::Flag(FlagId::Licm), s_licm),
    step("revnorm", "revnorm", Gate::Env("PLIRON_REVNORM"), s_revnorm),
    step("loopidiom", "idiom", Gate::Flag(FlagId::Loopidiom), s_loopidiom),
    step(
        "ifconv2",
        "ifconv2",
        Gate::All(&[Gate::Flag(FlagId::Loopvec), Gate::Env("PLIRON_IFCONV2")]),
        s_ifconv2,
    ),
    step("unswitch", "unswitch", Gate::Env("PLIRON_UNSWITCH"), s_unswitch),
    step("bcheck", "bcheck", Gate::Flag(FlagId::Bcheck), s_bcheck),
    step("loopvec", "loopvec", Gate::Flag(FlagId::Loopvec), s_loopvec),
    step("slp", "slp", Gate::Flag(FlagId::Slp), s_slp),
    step("switchmap", "", Gate::Env("PLIRON_SWITCHMAP"), s_switchmap),
    Step {
        name: "constbr2",
        bisect: Some("constbr"),
        stat: "",
        verify: true,
        gate: Gate::OptIn("PLIRON_CONSTBR"),
        run: s_constbr2,
    },
    step(
        "loopdel",
        "loopdel",
        Gate::All(&[JT, Gate::Env("PLIRON_LOOPDEL")]),
        s_loopdel,
    ),
    step("looprot", "", Gate::Env("PLIRON_LOOPROT"), s_looprot),
    // Scalar unroll of linear counted loops (e.g. ordered float
    // reductions that can't vectorize): K bodies per branch.
    step(
        "punroll",
        "punroll",
        Gate::All(&[JT, Gate::Env("PLIRON_PUNROLL")]),
        s_punroll,
    ),
    Step {
        name: "frozen_loads",
        bisect: None,
        stat: "",
        verify: false,
        gate: Gate::Always,
        run: s_frozen_loads,
    },
    step(
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
        gate: Gate::All(&[Gate::Flag(FlagId::Peep), Gate::Env("PLIRON_COMMUTE")]),
        run: s_commute,
    },
    Step {
        name: "wide_minmax",
        bisect: None,
        stat: "",
        verify: false,
        gate: Gate::Always,
        run: s_wide_minmax,
    },
    Step {
        name: "coldarg",
        bisect: Some("coldarg"),
        stat: "coldarg",
        verify: false,
        gate: Gate::Env("PLIRON_COLDARG"),
        run: s_coldarg,
    },
    step("fuse", "fuse", Gate::Env("PLIRON_FUSE"), s_fuse),
    step("edgefwd", "edgefwd", Gate::Env("PLIRON_EDGEFWD"), s_edgefwd),
    step("foldf", "foldf", Gate::Env("PLIRON_FOLDF"), s_foldf),
    step("celim", "celim", Gate::Env("PLIRON_CELIM"), s_celim),
    step("ivrefold", "ivrefold", Gate::Env("PLIRON_IVREFOLD"), s_ivrefold),
    step("deadrec", "deadrec", Gate::Env("PLIRON_DEADREC"), s_deadrec),
    step("lcse", "lcse", Gate::Env("PLIRON_LCSE"), s_lcse),
    step("sameargs", "sameargs", Gate::Env("PLIRON_SAMEARG"), s_sameargs),
    step("mask01", "mask01", Gate::Env("PLIRON_MASK01"), s_mask01),
    step("edgespec", "edgespec", Gate::Env("PLIRON_EDGESPEC"), s_edgespec),
    step("bittab", "bittab", Gate::Env("PLIRON_BITTAB"), s_bittab),
    step("coldedge", "coldedge", Gate::Env("PLIRON_COLDEDGE"), s_coldedge),
    step("dse2", "dse2", Gate::Env("PLIRON_DSE2"), s_dse2),
    step("vecslot", "vecslot", Gate::Env("PLIRON_VECSLOT"), s_vecslot),
    // Late value numbering: the edge transforms and vecslot leave
    // identical insts in one block; also drives the mem-offset and
    // neg folds on late-introduced addressing.
    Step {
        name: "lcse2",
        bisect: Some("lcse"),
        stat: "lcse",
        verify: true,
        gate: Gate::Env("PLIRON_LCSE"),
        run: s_lcse,
    },
    step("uload32", "uload32", Gate::Env("PLIRON_ULOAD32"), s_uload32),
    step("boundcmp", "boundcmp", Gate::Env("PLIRON_BCMP"), s_boundcmp),
    step("selfarg", "selfarg", Gate::Env("PLIRON_SELFARG"), s_selfarg),
    step("brchain", "brchain", Gate::Env("PLIRON_BRCHAIN"), s_brchain),
    step("slotmerge", "", Gate::Env("PLIRON_SLOTMERGE"), s_slotmerge),
    Step {
        name: "coldret",
        bisect: Some("coldret"),
        stat: "coldret",
        verify: false,
        gate: Gate::Env("PLIRON_COLDRET"),
        run: s_coldret,
    },
];
