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
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, FuncRef, Function, Inst, InstructionData, Opcode, Value};
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
    v.retain(|&(o2, w2)| o2 < o || o2 + w2 > o + w);
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
const MAX_ITERS: usize = 32;

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
        let overlap = |v: &Ranges| v.iter().any(|&(o2, w2)| o < o2 + w2 && o2 < o + w);
        // Same-root ranges need the byte check; a non-isolated `r` also
        // may-aliases any demand on another non-isolated root.
        let hit = self.locs.get(&r).is_some_and(overlap)
            || (!iso.contains(&r)
                && self.locs.keys().any(|r2| *r2 != r && !iso.contains(r2)));
        if hit || self.roots.contains(&r) {
            return true;
        }
        ((self.top_non_iso && !iso.contains(&r))
            || (self.top_non_stack && matches!(r, Root::V(_))))
            && !self
                .cov
                .get(&r)
                .is_some_and(|v| v.iter().any(|&(o2, w2)| o2 <= o && o + w <= o2 + w2))
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
        if self.ncov < MAX_LOCS
            && rput(self.cov.entry(r).or_default(), o, w)
        {
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
    ps: &FxHashMap<Value, Value>,
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
    let (r, o) = *memo
        .entry(addr)
        .or_insert_with(|| loadfwd::root(func, ps, na, addr));
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
}

/// Resolve every inst's demand effect in `order`'s reachable blocks.
fn decode(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    nw: &FxHashMap<FuncRef, bool>,
    eh: bool,
    order: &[Block],
) -> SecondaryMap<Block, Vec<Op>> {
    let mut ops: SecondaryMap<Block, Vec<Op>> = SecondaryMap::new();
    let mut memo: FxHashMap<Value, (Root, i64)> = FxHashMap::default();
    for &b in order {
        let mut v = Vec::new();
        for i in func.layout.block_insts(b) {
            match func.dfg.insts[i] {
                InstructionData::Load { .. } => match mem_loc(func, ps, na, &mut memo, i) {
                    Some(l) => v.push(Op::Read(l)),
                    None => v.push(Op::RwAll),
                },
                InstructionData::Store { opcode, .. } => {
                    match mem_loc(func, ps, na, &mut memo, i) {
                        Some(l) => v.push(Op::Store(
                            i,
                            l,
                            loadfwd::notrap(func, i)
                                && matches!(
                                    opcode,
                                    Opcode::Store
                                        | Opcode::Istore8
                                        | Opcode::Istore16
                                        | Opcode::Istore32
                                ),
                        )),
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
        ops[b] = v;
    }
    ops
}

/// Backward transfer through `b`'s decoded ops given `d` = demands at
/// block end. `dead`, when given, collects stores proven dead on this
/// walk.
fn transfer(ops: &[Op], d: &mut Demands, iso: &FxHashSet<Root>, mut dead: Option<&mut Vec<Inst>>) {
    for op in ops.iter().rev() {
        match *op {
            Op::Read((r, o, w)) => d.demand(r, o, w),
            Op::ReadAll => d.read_barrier(iso),
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

/// Remove dead stores in `func`; returns the number removed.
pub fn run(
    func: &mut Function,
    noalias: &FxHashSet<Value>,
    nw: &FxHashMap<FuncRef, bool>,
    cfrozen: &FxHashMap<FuncRef, u64>,
) -> usize {
    let Some(entry) = func.layout.entry_block() else {
        return 0;
    };
    let prof = crate::env_set("PLIRON_DSE_PROF");
    let t0 = std::time::Instant::now();
    let cfg = ControlFlowGraph::with_function(func);
    let ps = &crate::proots::alias_map_cfg(func, &cfg);
    let t_ps = t0.elapsed();
    let iso = loadfwd::isolated(func, noalias, ps, cfrozen);
    let t_iso = t0.elapsed();
    let eh = crate::pass_enabled("PLIRON_NOWRITE_EH");
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
    let ops = decode(func, ps, noalias, nw, eh, &order);
    let t_dec = t0.elapsed();
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
    let mut budget = MAX_ITERS * order.len();
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
        transfer(&ops[b], &mut din, &iso, None);
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
        if crate::env_set("PLIRON_DSE_DEBUG") {
            eprintln!(
                "dse: fixpoint unstable after {MAX_ITERS} iters, bailing ({:?})",
                func.name
            );
        }
        return 0;
    }
    let t_fix = t0.elapsed();
    let mut dead = Vec::new();
    for &b in &order {
        let mut d = out[b].clone();
        transfer(&ops[b], &mut d, &iso, Some(&mut dead));
    }
    for i in &dead {
        func.layout.remove_inst(*i);
    }
    if prof {
        let ninst: usize = order.iter().map(|&b| ops[b].len()).sum();
        eprintln!(
            "dseprof {} blocks={} ops={ninst} pops={pops} ps={t_ps:?} iso={:?} dec={:?} fix={:?} sweep={:?} dead={}",
            func.name,
            order.len(),
            t_iso - t_ps,
            t_dec - t_iso,
            t_fix - t_dec,
            t0.elapsed() - t_fix,
            dead.len(),
        );
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
        run(
            func,
            &FxHashSet::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
        )
    }

    /// `st p; st p; ret`: the second store's coverage satisfies the
    /// return demand on those bytes, so the first dies — previously the
    /// blanket `top_non_stack` demand kept every store to caller memory
    /// alive.
    #[test]
    fn store_chain_kills_earlier() {
        let mut func = func_with(|cur, p| {
            let one = cur.ins().iconst(types::I64, 1);
            cur.ins().store(MemFlagsData::new().with_notrap(), one, p, 0);
            let two = cur.ins().iconst(types::I64, 2);
            cur.ins().store(MemFlagsData::new().with_notrap(), two, p, 0);
        });
        assert_eq!(dse(&mut func), 1);
    }

    /// `st p; ld p; st p; ret`: the load between the stores reads the
    /// first store's bytes, so it stays live.
    #[test]
    fn load_between_stores_keeps_earlier() {
        let mut func = func_with(|cur, p| {
            let one = cur.ins().iconst(types::I64, 1);
            cur.ins().store(MemFlagsData::new().with_notrap(), one, p, 0);
            cur.ins().load(types::I64, MemFlagsData::new().with_notrap(), p, 0);
            let two = cur.ins().iconst(types::I64, 2);
            cur.ins().store(MemFlagsData::new().with_notrap(), two, p, 0);
        });
        assert_eq!(dse(&mut func), 0);
    }

    /// `st p; st p+8; ret`: disjoint ranges don't cover each other — the
    /// first store is still reachable by the return's demand.
    #[test]
    fn disjoint_store_survives() {
        let mut func = func_with(|cur, p| {
            let one = cur.ins().iconst(types::I64, 1);
            cur.ins().store(MemFlagsData::new().with_notrap(), one, p, 0);
            let two = cur.ins().iconst(types::I64, 2);
            cur.ins().store(MemFlagsData::new().with_notrap(), two, p, 8);
        });
        assert_eq!(dse(&mut func), 0);
    }
}
