//! Dead store elimination on Cranelift IR (a narrow `dse`): a plain
//! `store`/`istoreK` is removed when no read of its bytes can happen before
//! another write covers them. The analysis is a backward may-read dataflow:
//! the state at each program point is the set of locations that may be read
//! before being rewritten (`Demands`). Two top flags bound who can still
//! observe memory: `top_non_iso` for barriers (calls, atomics, fences,
//! other side-effecting ops) that read anything reachable, and
//! `top_non_stack` for returns, which expose every `Root::V` — including
//! isolated noalias params, whose pointee is caller memory — but not
//! `Root::S` slots, which die with the frame. Loads add their location,
//! live stores clear the demands their bytes satisfy, and a store whose
//! range meets no demand is dead. Dead stores do not clear demands, so a
//! chain `st A; st B` collapses one link per pass.
//!
//! Locations reuse `loadfwd`'s root+offset model: `(Root, offset, width)`.
//! Anything that isn't a plain load/store — calls that may write, atomics,
//! fences, volatile accesses, traps — is a barrier: it may read every
//! non-isolated location and may write them too, so earlier non-isolated
//! demands are both added and satisfied there. Atomic and volatile stores
//! are never candidates (different inst formats / no `notrap` flags);
//! volatile accesses still read/write their own location like any access
//! (they just can't be removed).

use cranelift_codegen::entity::SecondaryMap;
use cranelift_codegen::ir::{Block, FuncRef, Function, Inst, InstructionData, Opcode, Value};
use crate::vmap::VMap;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::loadfwd::{self, Root};

/// A read demand: root, byte offset, byte width.
type DLoc = (Root, i64, i64);

/// Byte ranges `(off, width)` under one root, kept sorted and deduped so
/// `Demands`'s `PartialEq` is insensitive to union order.
type Ranges = Vec<(i64, i64)>;

/// Insert `(o, w)` into a sorted `Ranges`; false if already present.
fn rput(v: &mut Ranges, o: i64, w: i64) -> bool {
    match v.binary_search(&(o, w)) {
        Ok(_) => false,
        Err(i) => {
            v.insert(i, (o, w));
            true
        }
    }
}

/// Drop entries covered by a write of `[o, o+w)`; returns how many went.
fn rkill(v: &mut Ranges, o: i64, w: i64) -> usize {
    let n = v.len();
    // End offsets in i128: generated code can use offsets near i64::MAX.
    v.retain(|&(o2, w2)| o2 < o || o2 as i128 + w2 as i128 > o as i128 + w as i128);
    n - v.len()
}

/// `a ∩= b` on per-root range maps (entry-wise set intersection).
fn isect(a: &mut FxHashMap<Root, Ranges>, b: &FxHashMap<Root, Ranges>) {
    a.retain(|r, v| match b.get(r) {
        Some(bv) => {
            v.retain(|x| bv.binary_search(x).is_ok());
            !v.is_empty()
        }
        None => false,
    });
}

const MAX_LOCS: usize = 256;
/// Fixpoint pop budget as a multiple of the reachable block count.
/// Convergence is ~1.8 pops/block on real code; the far tail (~6-12x,
/// giant stage2 functions) is merge-point churn that almost never kills
/// a store, and bailing is conservative — the function keeps them.
const MAX_POP_ROUNDS: usize = 6;

/// Locations a later read may observe before an intervening write. Two
/// "top" flags distinguish who can still observe memory:
/// - `top_non_iso`: a barrier (call/atomic/volatile-family op) may read any
///   reachable — i.e. non-isolated — memory;
/// - `top_non_stack`: a `return` lets the caller observe any memory that
///   outlives the frame — every `Root::V` including isolated noalias
///   params (they are caller memory), but not `Root::S` stack slots,
///   which die with the frame.
/// `locs`/`roots` are explicit per-location demands — the only way an
/// isolated root accumulates demand. `cov` tracks byte ranges a live store
/// definitely rewrites later: it satisfies the blanket top demands on
/// exactly those bytes, so a store chain `st A; st A` collapses even when
/// the only pending demand is a return or barrier.
///
/// `locs`/`cov` index ranges by root: stores consult only their own
/// root's vec instead of scanning every pending demand, and the
/// cross-root may-alias rule for non-isolated roots reduces to "any
/// non-iso root key present". `nlocs`/`ncov` cache the entry counts for
/// the `MAX_LOCS` overflow accounting.
#[derive(Clone, PartialEq, Eq, Default)]
struct Demands {
    top_non_iso: bool,
    top_non_stack: bool,
    locs: FxHashMap<Root, Ranges>,
    nlocs: usize,
    /// Whole-root demands for when `locs` overflows: conservative, never
    /// cleared (a store covers one range, not a whole root).
    roots: FxHashSet<Root>,
    /// Bytes a live store provably rewrites before any still-pending read.
    /// Satisfies top demands per-range; `roots` demands stay unsatisfiable
    /// (the lost precision means the read may predate the covering store).
    cov: FxHashMap<Root, Ranges>,
    ncov: usize,
}

impl Demands {
    /// Union demand-side state. `cov` is handled separately at merge
    /// points: it must hold on every incoming path, so merges intersect it
    /// rather than union.
    fn union(&mut self, o: &Demands) {
        self.top_non_iso |= o.top_non_iso;
        self.top_non_stack |= o.top_non_stack;
        for (r, v) in &o.locs {
            for &(x, w) in v {
                if rput(self.locs.entry(*r).or_default(), x, w) {
                    self.nlocs += 1;
                }
            }
        }
        if self.nlocs > MAX_LOCS {
            self.roots.extend(self.locs.keys().copied());
            self.locs.clear();
            self.nlocs = 0;
        }
        self.roots.extend(o.roots.iter().copied());
    }

    /// Does some live read demand a byte of `[o, o+w)` on root `r`?
    fn read(&self, r: Root, o: i64, w: i64, iso: &FxHashSet<Root>) -> bool {
        let (o_, e_) = (o as i128, o as i128 + w as i128);
        let overlap = |v: &Ranges| {
            v.iter()
                .any(|&(o2, w2)| o_ < o2 as i128 + w2 as i128 && (o2 as i128) < e_)
        };
        // Same-root ranges need the byte check; a non-isolated `r` also
        // may-aliases any demand on another non-isolated root.
        let hit = self.locs.get(&r).is_some_and(overlap)
            || (!iso.contains(&r) && self.locs.keys().any(|r2| *r2 != r && !iso.contains(r2)));
        if hit || self.roots.contains(&r) {
            return true;
        }
        ((self.top_non_iso && !iso.contains(&r)) || (self.top_non_stack && matches!(r, Root::V(_))))
            && !self
                .cov
                .get(&r)
                .is_some_and(|v| {
                    v.iter()
                        .any(|&(o2, w2)| o2 <= o && e_ <= o2 as i128 + w2 as i128)
                })
    }

    /// A write of `[o, o+w)` on `r` satisfies the demands it provably
    /// covers: same root, demand range inside the store range. The store's
    /// own range also joins `cov` so earlier stores under a blanket demand
    /// can die.
    fn written(&mut self, r: Root, o: i64, w: i64) {
        if let Some(v) = self.locs.get_mut(&r) {
            self.nlocs -= rkill(v, o, w);
            if v.is_empty() {
                self.locs.remove(&r);
            }
        }
        if let Some(v) = self.cov.get_mut(&r) {
            self.ncov -= rkill(v, o, w);
            if v.is_empty() {
                self.cov.remove(&r);
            }
        }
        if self.ncov < MAX_LOCS && rput(self.cov.entry(r).or_default(), o, w) {
            self.ncov += 1;
        }
    }

    /// Add a read of `[o, o+w)` on `r`.
    fn demand(&mut self, r: Root, o: i64, w: i64) {
        if self.nlocs >= MAX_LOCS {
            self.roots.insert(r);
        } else if rput(self.locs.entry(r).or_default(), o, w) {
            self.nlocs += 1;
        }
    }

    /// A call that may unwind ([`Op::Exit`]): the caller can read every
    /// `Root::V` once the frame unwinds, and no later store in this
    /// function runs on that path, so their coverage of `Root::V` bytes
    /// doesn't satisfy it. Stack-slot coverage stays: slots die with the
    /// frame.
    fn exit(&mut self) {
        self.top_non_stack = true;
        self.cov.retain(|r, _| !matches!(r, Root::V(_)));
        self.ncov = self.cov.values().map(Vec::len).sum();
    }

    /// A may-read barrier (call/atomic/other side effect): observes any
    /// reachable memory, so coverage a later store provides is no help to
    /// earlier non-isolated stores — the barrier's own reads see the
    /// pre-store values. Isolated roots stay precisely tracked.
    fn read_barrier(&mut self, iso: &FxHashSet<Root>) {
        self.top_non_iso = true;
        if iso.is_empty() {
            self.cov.clear();
            self.ncov = 0;
        } else {
            self.cov.retain(|r, _| iso.contains(r));
            self.ncov = self.cov.values().map(Vec::len).sum();
        }
    }

    /// An unknown writer (call that may write, atomic/fence/other side
    /// effects): reads everything reachable and may satisfy any
    /// non-isolated pending read. Isolated roots stay precisely tracked —
    /// barriers can't reach them.
    fn barrier(&mut self, iso: &FxHashSet<Root>) {
        self.read_barrier(iso);
        if iso.is_empty() {
            self.locs.clear();
            self.nlocs = 0;
            self.roots.clear();
        } else {
            self.locs.retain(|r, _| iso.contains(r));
            self.nlocs = self.locs.values().map(Vec::len).sum();
            self.roots.retain(|r| iso.contains(r));
        }
    }
}

/// Byte width of a load/store instruction (narrow ops included); `None` for
/// insts that aren't in the plain load/store family.
fn mem_width(func: &Function, i: Inst) -> Option<i64> {
    match func.dfg.insts[i] {
        InstructionData::Load { opcode, .. } => Some(match opcode {
            Opcode::Load => i64::from(func.dfg.value_type(func.dfg.first_result(i)).bytes()),
            Opcode::Uload8 | Opcode::Sload8 => 1,
            Opcode::Uload16 | Opcode::Sload16 => 2,
            Opcode::Uload32 | Opcode::Sload32 => 4,
            _ => return None,
        }),
        InstructionData::Store { opcode, args, .. } => Some(match opcode {
            Opcode::Store => i64::from(
                func.dfg
                    .value_type(func.dfg.resolve_aliases(args[0]))
                    .bytes(),
            ),
            Opcode::Istore8 => 1,
            Opcode::Istore16 => 2,
            Opcode::Istore32 => 4,
            _ => return None,
        }),
        _ => None,
    }
}

/// Location `(root, offset, width)` touched by a plain load/store inst.
/// `memo` caches `root` per address value — the same base pointer feeds
/// many accesses, and `root` re-peels the alias/`iadd` chain each call.
fn mem_loc(
    func: &Function,
    ps: &VMap<Value>,
    na: &FxHashSet<Value>,
    memo: &mut FxHashMap<Value, (Root, i64)>,
    i: Inst,
) -> Option<(Root, i64, i64)> {
    let (addr, off) = match func.dfg.insts[i] {
        InstructionData::Load { arg, offset, .. } => (arg, i32::from(offset)),
        InstructionData::Store { args, offset, .. } => (args[1], i32::from(offset)),
        _ => return None,
    };
    let w = mem_width(func, i)?;
    // The demand must key under the *normalized* root: a `noalias`-marked
    // `stack_addr` (scope name) peels to its slot, matching stores that
    // reach the same slot through unmarked `stack_addr`s. With the raw
    // root the two key differently — a load's demand lands under `V(v)`
    // while the slot's stores sit under `S(ss)`, so a live store dies.
    let (r, o) = *memo
        .entry(addr)
        .or_insert_with(|| loadfwd::norm_root(func, ps, na, addr));
    Some((r, o.wrapping_add(i64::from(off)), w))
}

/// One inst's effect on the demand state, decoded once per run so the
/// fixpoint replays `Demands` ops instead of re-reading the DFG.
#[derive(Clone, Copy)]
enum Op {
    /// Plain load: add its loc to `locs`.
    Read(DLoc),
    /// Plain store: `killable` = notrap + plain-opcode family (the
    /// dead-store candidate test); otherwise it just `written`s.
    Store(Inst, DLoc, bool),
    /// `read_barrier` — reads everything reachable, satisfies nothing.
    ReadAll,
    /// `barrier` — reads and may write everything reachable.
    RwAll,
    /// A plain call that may unwind: the unwind leaves this frame like a
    /// `return`, so the caller (e.g. one that catches the panic) can read
    /// every `Root::V` — including isolated noalias params like `&mut
    /// self` that calls can't otherwise reach. `try_call`s don't need it:
    /// their unwind edge stays in this function and reaches the resume
    /// call, which is itself a plain call.
    Exit,
}

/// `PLIRON_DSE_UNWIND=0`: treat calls as never unwinding (the pre-fix,
/// unsound behavior) — only for measuring what the unwind exits cost.
fn no_unwind_env() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| pliron_cfg::env_is("PLIRON_DSE_UNWIND", "0"))
}

/// Resolve every inst's demand effect in `order`'s reachable blocks.
/// `store_blks` collects the blocks holding a killable store — the only
/// blocks the kill sweep can produce dead insts in.
fn decode(
    func: &Function,
    ps: &VMap<Value>,
    na: &FxHashSet<Value>,
    nw: &FxHashMap<FuncRef, bool>,
    nounwind: &FxHashSet<FuncRef>,
    eh: bool,
    order: &[Block],
) -> (SecondaryMap<Block, Vec<Op>>, Vec<Block>) {
    let mut ops: SecondaryMap<Block, Vec<Op>> = SecondaryMap::new();
    let mut memo: FxHashMap<Value, (Root, i64)> = FxHashMap::default();
    let mut store_blks = Vec::new();
    for &b in order {
        let mut v = Vec::new();
        let mut killable = false;
        for i in func.layout.block_insts(b) {
            match func.dfg.insts[i] {
                InstructionData::Load { .. } => match mem_loc(func, ps, na, &mut memo, i) {
                    Some(l) => v.push(Op::Read(l)),
                    None => v.push(Op::RwAll),
                },
                InstructionData::Store { opcode, .. } => {
                    match mem_loc(func, ps, na, &mut memo, i) {
                        Some(l) => {
                            let k = loadfwd::notrap(func, i)
                                && matches!(
                                    opcode,
                                    Opcode::Store
                                        | Opcode::Istore8
                                        | Opcode::Istore16
                                        | Opcode::Istore32
                                );
                            killable |= k;
                            v.push(Op::Store(i, l, k));
                        }
                        None => v.push(Op::RwAll),
                    }
                }
                _ => {
                    let op = func.dfg.insts[i].opcode();
                    if op.is_call() {
                        // Readonly calls still read anything reachable;
                        // writing calls also satisfy pending reads.
                        v.push(if loadfwd::write_free(func, i, nw, eh) {
                            Op::ReadAll
                        } else {
                            Op::RwAll
                        });
                        let unwinds = match func.dfg.insts[i] {
                            InstructionData::Call { func_ref, .. } => !nounwind.contains(&func_ref),
                            InstructionData::CallIndirect { .. } => true,
                            _ => false,
                        };
                        if unwinds && !no_unwind_env() {
                            v.push(Op::Exit);
                        }
                    } else if op.can_load()
                        || op.can_store()
                        || op.other_side_effects()
                        || matches!(op, Opcode::AtomicLoad | Opcode::Fence)
                    {
                        v.push(Op::RwAll);
                    }
                }
            }
        }
        if killable {
            store_blks.push(b);
        }
        ops[b] = v;
    }
    (ops, store_blks)
}

/// Backward transfer through `b`'s decoded ops given `d` = demands at
/// block end. `dead`, when given, collects stores proven dead on this
/// walk.
fn transfer(ops: &[Op], d: &mut Demands, iso: &FxHashSet<Root>, mut dead: Option<&mut Vec<Inst>>) {
    for op in ops.iter().rev() {
        match *op {
            Op::Read((r, o, w)) => d.demand(r, o, w),
            Op::ReadAll => d.read_barrier(iso),
            Op::Exit => d.exit(),
            Op::RwAll => d.barrier(iso),
            Op::Store(i, (r, o, w), killable) => {
                // A dead store hides nothing: demands pass through it to
                // earlier stores so a store-store chain collapses. A live
                // store (and stores we may never kill — volatile,
                // untrusted) satisfies the covered demands. The same rule
                // runs in both phases: don't clear on a killable store
                // even when `dead` isn't collecting.
                if !d.read(r, o, w, iso) && killable {
                    if let Some(ds) = dead.as_deref_mut() {
                        ds.push(i);
                    }
                } else {
                    d.written(r, o, w);
                }
            }
        }
    }
}

/// Remove dead stores in `func`; returns the number removed. `step` and
/// `fname` label the `PLIRON_DSE_PROF` line (the pipeline step that
/// invoked us and the function's real symbol — `func.name` is just
/// `u0:0` for every function we build).
pub fn run(
    func: &mut Function,
    noalias: &FxHashSet<Value>,
    scoped_na: &FxHashSet<Value>,
    nw: &FxHashMap<FuncRef, bool>,
    cfrozen: &FxHashMap<FuncRef, u64>,
    nounwind: &FxHashSet<FuncRef>,
    an: &mut crate::pipeline::Analysis,
    step: &str,
    fname: &str,
) -> usize {
    let Some(entry) = func.layout.entry_block() else {
        return 0;
    };
    let prof =
        pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::dse", tracing::Level::INFO);
    // Only a plain notrap store is killable, and `decode` can only
    // demote candidates to `RwAll`, never create them — a function with
    // no candidate skips the whole CFG/alias/iso setup and the dataflow.
    let mut candidate = false;
    'scan: for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if let InstructionData::Store { opcode, .. } = func.dfg.insts[i]
                && matches!(
                    opcode,
                    Opcode::Store | Opcode::Istore8 | Opcode::Istore16 | Opcode::Istore32
                )
                && loadfwd::notrap(func, i)
            {
                candidate = true;
                break 'scan;
            }
        }
    }
    if !candidate {
        return 0;
    }
    // Shared per-`ver` inputs: the three pipeline runs (and any other
    // pass wired to `Analysis`) build these at most once per IR state.
    let (cfg, ps, iso, t_ps, t_iso) = an.dse_inputs(func, noalias, scoped_na, cfrozen);
    let eh = crate::pass_enabled("PLIRON_NOWRITE_EH");
    let t = prof.then(std::time::Instant::now);
    let mut order = Vec::new();
    let mut seen: SecondaryMap<Block, bool> = SecondaryMap::new();
    let mut stack = vec![entry];
    while let Some(b) = stack.pop() {
        if seen[b] {
            continue;
        }
        seen[b] = true;
        order.push(b);
        for s in cfg.succ_iter(b) {
            stack.push(s);
        }
    }
    // Decode each inst's demand effect once: the fixpoint and the kill
    // sweep then replay `Demands` ops instead of re-reading the DFG and
    // re-peeling `root` chains.
    let (ops, store_blks) = decode(func, ps, noalias, nw, nounwind, eh, &order);
    let t_dec = t.map_or(std::time::Duration::ZERO, |t| t.elapsed());
    if store_blks.is_empty() {
        // Every candidate's address failed `mem_loc` — the fixpoint and
        // sweep can only ever report `dead = []`.
        if prof {
            let ninst: usize = order.iter().map(|&b| ops[b].len()).sum();
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::dse", "dseprof {step} {fname} blocks={} ops={ninst} pops=0 ps={t_ps:?} iso={t_iso:?} dec={t_dec:?} fix=0ns sweep=0ns dead=0",
                    order.len(),
                )
            };
        }
        return 0;
    }
    // Caller-observable memory at function exits: returns demand every
    // `Root::V` — including isolated noalias params, whose pointee is
    // caller memory — but not stack slots, which die with the frame.
    // Unreachable/tarpit ends demand nothing.
    let mut inn: SecondaryMap<Block, Demands> = SecondaryMap::new();
    let mut out: SecondaryMap<Block, Demands> = SecondaryMap::new();
    // `done[b]`: `inn[b]` already includes `transfer(out[b])` — so a pop
    // where `out[b]` is unchanged needs no re-transfer, just the merge
    // check. Without it the fixpoint re-transfers every block on every
    // round, even ones whose successor states never moved.
    let mut done: SecondaryMap<Block, bool> = SecondaryMap::new();
    // Worklist over predecessors: seeding with `order` and popping gives
    // the old loop's successors-first sweep; afterwards only preds of
    // blocks whose `inn` actually changed are revisited. States only grow
    // (demands union, coverage intersects) so this terminates; `budget`
    // bounds the total pops exactly like the old iteration cap.
    let mut inq: SecondaryMap<Block, bool> = SecondaryMap::new();
    let mut stack: Vec<Block> = order.clone();
    for &b in &order {
        inq[b] = true;
    }
    let mut pops = 0usize;
    let mut budget = MAX_POP_ROUNDS * order.len();
    let mut stable = true;
    while let Some(b) = stack.pop() {
        inq[b] = false;
        pops += 1;
        if budget == 0 {
            stable = false;
            break;
        }
        budget -= 1;
        let mut d = Demands {
            top_non_stack: func.layout.last_inst(b).is_some_and(|t| {
                matches!(
                    func.dfg.insts[t].opcode(),
                    Opcode::Return | Opcode::ReturnCall | Opcode::ReturnCallIndirect
                )
            }),
            ..Demands::default()
        };
        // Demands union over successors; coverage intersects — a byte
        // counts covered only when every successor path rewrites it. A
        // successor with no recorded state claims no coverage — a
        // never-written `inn` entry is the empty `Demands`, same thing.
        let mut cov: Option<FxHashMap<Root, Ranges>> = None;
        for s in cfg.succ_iter(b) {
            let i = &inn[s];
            match &mut cov {
                None => cov = Some(i.cov.clone()),
                Some(c) => isect(c, &i.cov),
            }
            d.union(i);
        }
        d.cov = cov.unwrap_or_default();
        d.ncov = d.cov.values().map(Vec::len).sum();
        // The old sweep also unioned `out[b]` back in (widen demands,
        // intersect coverage) to stop a store-demand flip-flop from
        // oscillating (hashmap's loop-carried `&key` slot). The worklist
        // gets the same monotonicity from the `inn` update below —
        // `din.union(old)`/`cov.retain(old)` — and since every `inn`
        // entry only grows demands and shrinks coverage, the succ merge
        // already subsumes the old `out[b]`: re-unioning it is a no-op.
        if done[b] && out[b] == d {
            continue;
        }
        let mut din = d.clone();
        transfer(&ops[b], &mut din, iso, None);
        // `done` is also "inn[b] was computed": only then does the old
        // state widen this result — the flip-flop guard. A first visit
        // keeps its coverage whole, else `∩ empty` would erase it.
        if done[b] {
            din.union(&inn[b]);
            isect(&mut din.cov, &inn[b].cov);
            din.ncov = din.cov.values().map(Vec::len).sum();
        }
        out[b] = d;
        done[b] = true;
        if din != inn[b] {
            inn[b] = din;
            for p in cfg.pred_iter(b) {
                if seen[p.block] && !inq[p.block] {
                    inq[p.block] = true;
                    stack.push(p.block);
                }
            }
        }
    }
    if !stable {
        if pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::dse", tracing::Level::DEBUG)
        {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::dse", "dse: fixpoint over pop budget ({MAX_POP_ROUNDS}x blocks), bailing ({fname})"
                )
            };
        }
        return 0;
    }
    let t_fix = t.map_or(std::time::Duration::ZERO, |t| t.elapsed() - t_dec);
    let dump =
        pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::dse", tracing::Level::INFO);
    if dump {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::dse", "dse-dump {fname} iso={iso:?}")
        };
        for &b in &order {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::dse", "  {b:?} inn tops={}/{} locs={:?} roots={:?} cov={:?}",
                    inn[b].top_non_iso,
                    inn[b].top_non_stack,
                    inn[b].locs,
                    inn[b].roots,
                    inn[b].cov
                )
            };
        }
    }
    let mut dead = Vec::new();
    // Only a block holding a killable store can produce a dead inst —
    // skip the re-transfer for every other reachable block.
    for &b in &store_blks {
        let mut d = out[b].clone();
        if dump {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::dse", "  sweep {b:?} out locs={:?} tops={}/{}", d.locs, d.top_non_iso, d.top_non_stack)
            };
        }
        transfer(&ops[b], &mut d, iso, Some(&mut dead));
    }
    if dump {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::dse", "  dead={dead:?}")
        };
    }
    for i in &dead {
        func.layout.remove_inst(*i);
    }
    if prof {
        let t_sweep = t.map_or(std::time::Duration::ZERO, |t| t.elapsed() - t_fix - t_dec);
        let ninst: usize = order.iter().map(|&b| ops[b].len()).sum();
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::dse", "dseprof {step} {fname} blocks={} ops={ninst} pops={pops} ps={t_ps:?} iso={t_iso:?} dec={t_dec:?} fix={t_fix:?} sweep={t_sweep:?} dead={}",
                order.len(),
                dead.len(),
            )
        };
    }
    dead.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::types;
    use cranelift_codegen::ir::{AbiParam, InstBuilder, MemFlagsData, Signature, UserFuncName};
    use cranelift_codegen::isa::CallConv;

    /// One-entry-block function `fn(p: i64)`; `body` fills the block, then
    /// it returns. Stores go through the param pointer so the return's
    /// blanket caller-memory demand is the only read in play.
    fn func_with(body: impl Fn(&mut cranelift_codegen::cursor::FuncCursor, Value)) -> Function {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        func.layout.append_block(entry);
        let p = func.dfg.append_block_param(entry, types::I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        body(&mut cur, p);
        cur.ins().return_(&[]);
        func
    }

    fn dse(func: &mut Function) -> usize {
        dse_with(func, &FxHashSet::default(), &FxHashSet::default())
    }

    fn dse_with(func: &mut Function, noalias: &FxHashSet<Value>, nounwind: &FxHashSet<FuncRef>) -> usize {
        run(
            func,
            noalias,
            &FxHashSet::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            nounwind,
            &mut crate::pipeline::Analysis::default(),
            "dse",
            "t",
        )
    }

    /// `fn(p noalias)`: `st p; call f(); st p; ret`. Returns the function,
    /// the param and `f`'s FuncRef.
    fn store_call_store() -> (Function, Value, FuncRef) {
        use cranelift_codegen::ir::{ExtFuncData, ExternalName, UserExternalNameRef};
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let sref = func.import_signature(Signature::new(CallConv::Fast));
        let fr = func.import_function(ExtFuncData {
            name: ExternalName::user(UserExternalNameRef::from_u32(0)),
            signature: sref,
            colocated: false,
            patchable: false,
        });
        let entry = func.dfg.make_block();
        func.layout.append_block(entry);
        let p = func.dfg.append_block_param(entry, types::I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let one = cur.ins().iconst(types::I64, 1);
        let two = cur.ins().iconst(types::I64, 2);
        cur.ins().store(MemFlagsData::new().with_notrap(), one, p, 0);
        cur.ins().call(fr, &[]);
        cur.ins().store(MemFlagsData::new().with_notrap(), two, p, 0);
        cur.ins().return_(&[]);
        (func, p, fr)
    }

    /// A call that may unwind exits to the caller, which can read `*p`
    /// (noalias, so the call itself can't): the first store stays even
    /// though the second rewrites it on the normal path.
    #[test]
    fn store_before_unwinding_call_stays() {
        let (mut func, p, _) = store_call_store();
        let na: FxHashSet<Value> = [p].into_iter().collect();
        assert_eq!(dse_with(&mut func, &na, &FxHashSet::default()), 0);
    }

    /// The same shape with a callee known not to unwind: the call can't
    /// see `*p` and can't leave, so the first store is dead.
    #[test]
    fn store_before_nounwind_call_dies() {
        let (mut func, p, fr) = store_call_store();
        let na: FxHashSet<Value> = [p].into_iter().collect();
        let nu: FxHashSet<FuncRef> = [fr].into_iter().collect();
        assert_eq!(dse_with(&mut func, &na, &nu), 1);
    }

    /// `st p; st p; ret`: the second store's coverage satisfies the
    /// return demand on those bytes, so the first dies — previously the
    /// blanket `top_non_stack` demand kept every store to caller memory
    /// alive.
    #[test]
    fn store_chain_kills_earlier() {
        let mut func = func_with(|cur, p| {
            let one = cur.ins().iconst(types::I64, 1);
            cur.ins()
                .store(MemFlagsData::new().with_notrap(), one, p, 0);
            let two = cur.ins().iconst(types::I64, 2);
            cur.ins()
                .store(MemFlagsData::new().with_notrap(), two, p, 0);
        });
        assert_eq!(dse(&mut func), 1);
    }

    /// `st p; ld p; st p; ret`: the load between the stores reads the
    /// first store's bytes, so it stays live.
    #[test]
    fn load_between_stores_keeps_earlier() {
        let mut func = func_with(|cur, p| {
            let one = cur.ins().iconst(types::I64, 1);
            cur.ins()
                .store(MemFlagsData::new().with_notrap(), one, p, 0);
            cur.ins()
                .load(types::I64, MemFlagsData::new().with_notrap(), p, 0);
            let two = cur.ins().iconst(types::I64, 2);
            cur.ins()
                .store(MemFlagsData::new().with_notrap(), two, p, 0);
        });
        assert_eq!(dse(&mut func), 0);
    }

    /// `st p; st p+8; ret`: disjoint ranges don't cover each other — the
    /// first store is still reachable by the return's demand.
    #[test]
    fn disjoint_store_survives() {
        let mut func = func_with(|cur, p| {
            let one = cur.ins().iconst(types::I64, 1);
            cur.ins()
                .store(MemFlagsData::new().with_notrap(), one, p, 0);
            let two = cur.ins().iconst(types::I64, 2);
            cur.ins()
                .store(MemFlagsData::new().with_notrap(), two, p, 8);
        });
        assert_eq!(dse(&mut func), 0);
    }

    /// `st ss; ld (ss + p*16); ret`: a slot read through a dynamic-offset
    /// pointer must keep its stores — the load's demand keys under the
    /// computed `V` root, not `S(ss)`, so the slot must not count as
    /// isolated for that to be safe.
    #[test]
    fn dynamic_offset_load_keeps_slot_store() {
        use cranelift_codegen::ir::stackslot::{StackSlotData, StackSlotKind};
        let mut func = func_with(|cur, p| {
            let ss = cur.func.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                32,
                3,
            ));
            let base = cur.ins().stack_addr(types::I64, ss, 0);
            let one = cur.ins().iconst(types::I64, 1);
            cur.ins()
                .store(MemFlagsData::new().with_notrap(), one, base, 0);
            let off = cur.ins().imul_imm(p, 16);
            let addr = cur.ins().iadd(base, off);
            cur.ins()
                .load(types::I64, MemFlagsData::new().with_notrap(), addr, 0);
        });
        assert_eq!(dse(&mut func), 0);
    }

    /// Store in one block, dynamic-offset load in a successor loop block:
    /// the slot must escape (the demand keys under `V(addr)`), so neither
    /// store may die.
    #[test]
    fn dynamic_offset_load_in_loop_keeps_slot_store() {
        use cranelift_codegen::ir::stackslot::{StackSlotData, StackSlotKind};
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let ss = func.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            32,
            3,
        ));
        let entry = func.dfg.make_block();
        let looop = func.dfg.make_block();
        let exit = func.dfg.make_block();
        func.layout.append_block(entry);
        func.layout.append_block(looop);
        func.layout.append_block(exit);
        let i = func.dfg.append_block_param(looop, types::I64);
        let p = func.dfg.append_block_param(entry, types::I64);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
            let base = cur.ins().stack_addr(types::I64, ss, 0);
            let one = cur.ins().iconst(types::I64, 1);
            cur.ins()
                .store(MemFlagsData::new().with_notrap(), one, base, 0);
            let eight = cur.ins().iconst(types::I64, 8);
            let hi = cur.ins().iadd(base, eight);
            cur.ins()
                .store(MemFlagsData::new().with_notrap(), one, hi, 0);
            let zero = cur.ins().iconst(types::I64, 0);
            cur.ins().jump(looop, &[zero.into()]);
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(looop);
            let base = cur.ins().stack_addr(types::I64, ss, 0);
            let off = cur.ins().imul_imm(i, 16);
            let addr = cur.ins().iadd(base, off);
            let x = cur
                .ins()
                .load(types::I64, MemFlagsData::new().with_notrap(), addr, 0);
            let addr8 = cur.ins().iadd_imm(addr, 8);
            let y = cur
                .ins()
                .load(types::I64, MemFlagsData::new().with_notrap(), addr8, 0);
            let i2 = cur.ins().iadd_imm(i, 1);
            let useit = cur.ins().iadd(x, y);
            let done = cur.ins().icmp_imm_s(cranelift_codegen::ir::condcodes::IntCC::Equal, useit, 3);
            cur.ins().brif(done, exit, &[], looop, &[i2.into()]);
        }
        FuncCursor::new(&mut func)
            .at_bottom(exit)
            .ins()
            .return_(&[]);
        let _ = p;
        assert_eq!(dse(&mut func), 0);
    }
}
