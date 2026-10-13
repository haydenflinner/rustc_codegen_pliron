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

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, FuncRef, Function, Inst, InstructionData, Opcode, Value};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::loadfwd::{self, Root};

/// A read demand: root, byte offset, byte width.
type DLoc = (Root, i64, i64);

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
#[derive(Clone, PartialEq, Eq, Default)]
struct Demands {
    top_non_iso: bool,
    top_non_stack: bool,
    locs: FxHashSet<DLoc>,
    /// Whole-root demands for when `locs` overflows: conservative, never
    /// cleared (a store covers one range, not a whole root).
    roots: FxHashSet<Root>,
    /// Bytes a live store provably rewrites before any still-pending read.
    /// Satisfies top demands per-range; `roots` demands stay unsatisfiable
    /// (the lost precision means the read may predate the covering store).
    cov: FxHashSet<DLoc>,
}

impl Demands {
    /// Union demand-side state. `cov` is handled separately at merge
    /// points: it must hold on every incoming path, so merges intersect it
    /// rather than union.
    fn union(&mut self, o: &Demands) {
        self.top_non_iso |= o.top_non_iso;
        self.top_non_stack |= o.top_non_stack;
        self.locs.extend(o.locs.iter().copied());
        if self.locs.len() > MAX_LOCS {
            self.roots.extend(self.locs.iter().map(|l| l.0));
            self.locs.clear();
        }
        self.roots.extend(o.roots.iter().copied());
    }

    /// Does some live read demand a byte of `[o, o+w)` on root `r`?
    fn read(&self, r: Root, o: i64, w: i64, iso: &FxHashSet<Root>) -> bool {
        if self.locs.iter().any(|&(r2, o2, w2)| match (r, r2) {
            _ if r == r2 => o < o2 + w2 && o2 < o + w,
            _ => !iso.contains(&r) && !iso.contains(&r2),
        }) || self.roots.contains(&r)
        {
            return true;
        }
        ((self.top_non_iso && !iso.contains(&r))
            || (self.top_non_stack && matches!(r, Root::V(_))))
            && !self.cov.iter().any(|&(r2, o2, w2)| {
                r2 == r && o2 <= o && o + w <= o2 + w2
            })
    }

    /// A write of `[o, o+w)` on `r` satisfies the demands it provably
    /// covers: same root, demand range inside the store range. The store's
    /// own range also joins `cov` so earlier stores under a blanket demand
    /// can die.
    fn written(&mut self, r: Root, o: i64, w: i64) {
        self.locs
            .retain(|&(r2, o2, w2)| r2 != r || o2 < o || o2 + w2 > o + w);
        self.cov
            .retain(|&(r2, o2, w2)| r2 != r || o2 < o || o2 + w2 > o + w);
        if self.cov.len() < MAX_LOCS {
            self.cov.insert((r, o, w));
        }
    }

    /// Add a read of `[o, o+w)` on `r`.
    fn demand(&mut self, r: Root, o: i64, w: i64) {
        if self.locs.len() >= MAX_LOCS {
            self.roots.insert(r);
        } else {
            self.locs.insert((r, o, w));
        }
    }

    /// A may-read barrier (call/atomic/other side effect): observes any
    /// reachable memory, so coverage a later store provides is no help to
    /// earlier non-isolated stores — the barrier's own reads see the
    /// pre-store values. Isolated roots stay precisely tracked.
    fn read_barrier(&mut self, iso: &FxHashSet<Root>) {
        self.top_non_iso = true;
        self.cov.retain(|&(r, _, _)| iso.contains(&r));
    }

    /// An unknown writer (call that may write, atomic/fence/other side
    /// effects): reads everything reachable and may satisfy any
    /// non-isolated pending read. Isolated roots stay precisely tracked —
    /// barriers can't reach them.
    fn barrier(&mut self, iso: &FxHashSet<Root>) {
        self.read_barrier(iso);
        self.locs.retain(|&(r, _, _)| iso.contains(&r));
        self.roots.retain(|r| iso.contains(r));
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
fn mem_loc(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    i: Inst,
) -> Option<(Root, i64, i64)> {
    let (addr, off) = match func.dfg.insts[i] {
        InstructionData::Load { arg, offset, .. } => (arg, i32::from(offset)),
        InstructionData::Store { args, offset, .. } => (args[1], i32::from(offset)),
        _ => return None,
    };
    let w = mem_width(func, i)?;
    let (r, o) = loadfwd::root(func, ps, na, addr);
    Some((r, o.wrapping_add(i64::from(off)), w))
}

/// Backward transfer through `b`'s insts given `d` = demands at block end.
/// `dead`, when given, collects stores proven dead on this walk.
fn transfer(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    b: Block,
    d: &mut Demands,
    iso: &FxHashSet<Root>,
    nw: &FxHashMap<FuncRef, bool>,
    eh: bool,
    mut dead: Option<&mut Vec<Inst>>,
) {
    let insts: Vec<Inst> = func.layout.block_insts(b).collect();
    for i in insts.into_iter().rev() {
        match func.dfg.insts[i] {
            InstructionData::Load { .. } => {
                if let Some((r, o, w)) = mem_loc(func, ps, na, i) {
                    d.demand(r, o, w);
                } else {
                    d.barrier(iso);
                }
            }
            InstructionData::Store { opcode, .. } => {
                let Some((r, o, w)) = mem_loc(func, ps, na, i) else {
                    d.barrier(iso);
                    continue;
                };
                // A dead store hides nothing: demands pass through it to
                // earlier stores so a store-store chain collapses. A live
                // store (and stores we may never kill — volatile,
                // untrusted) satisfies the covered demands. The same rule
                // runs in both phases: don't clear on a killable store
                // even when `dead` isn't collecting.
                let dead_store = !d.read(r, o, w, iso)
                    && loadfwd::notrap(func, i)
                    && matches!(
                        opcode,
                        Opcode::Store | Opcode::Istore8 | Opcode::Istore16 | Opcode::Istore32
                    );
                if dead_store {
                    if let Some(ds) = dead.as_deref_mut() {
                        ds.push(i);
                    }
                    continue;
                }
                d.written(r, o, w);
            }
            _ => {
                let op = func.dfg.insts[i].opcode();
                if op.is_call() {
                    // Readonly calls still read anything reachable; writing
                    // calls also satisfy pending non-isolated reads.
                    if loadfwd::write_free(func, i, nw, eh) {
                        d.read_barrier(iso);
                    } else {
                        d.barrier(iso);
                    }
                } else if op.can_load()
                    || op.can_store()
                    || op.other_side_effects()
                    || matches!(op, Opcode::AtomicLoad | Opcode::Fence)
                {
                    d.barrier(iso);
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
    let cfg = ControlFlowGraph::with_function(func);
    let ps = &loadfwd::param_src(func);
    let iso = loadfwd::isolated(func, noalias, ps, cfrozen);
    let eh = crate::pass_enabled("PLIRON_NOWRITE_EH");
    let mut order = Vec::new();
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut stack = vec![entry];
    while let Some(b) = stack.pop() {
        if !seen.insert(b) {
            continue;
        }
        order.push(b);
        for s in cfg.succ_iter(b) {
            stack.push(s);
        }
    }
    // Caller-observable memory at function exits: returns demand every
    // `Root::V` — including isolated noalias params, whose pointee is
    // caller memory — but not stack slots, which die with the frame.
    // Unreachable/tarpit ends demand nothing.
    let mut inn: FxHashMap<Block, Demands> = FxHashMap::default();
    let mut out: FxHashMap<Block, Demands> = FxHashMap::default();
    let mut stable = false;
    // Backward flow: visit successors before predecessors (reverse DFS
    // order) so demands propagate a whole chain per pass instead of one
    // edge — preorder would need CFG-depth iterations to reach entry.
    for _ in 0..MAX_ITERS {
        let mut changed = false;
        for &b in order.iter().rev() {
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
            // counts covered only when every successor path rewrites it.
            // A successor with no recorded state claims no coverage.
            let mut cov: Option<FxHashSet<DLoc>> = None;
            for s in cfg.succ_iter(b) {
                match inn.get(&s) {
                    Some(i) => {
                        match &mut cov {
                            None => cov = Some(i.cov.clone()),
                            Some(c) => c.retain(|l| i.cov.contains(l)),
                        }
                        d.union(i);
                    }
                    // Empty: intersecting stays empty however later
                    // successors merge.
                    None => cov = Some(FxHashSet::default()),
                }
            }
            d.cov = cov.unwrap_or_default();
            // Widen demands upward and coverage downward so the flow
            // can't oscillate: a covering store removes the read demand
            // that justified it, which next round resurfaces the demand
            // and revives the store — a two-state flip-flop that never
            // stabilizes (hashmap's loop-carried `&key` slot). Unioning
            // demands and intersecting coverage is a post-fixpoint, a
            // conservative over-approximation of live reads.
            if let Some(old) = out.get(&b) {
                d.union(old);
                d.cov.retain(|l| old.cov.contains(l));
            }
            if out.get(&b) != Some(&d) {
                out.insert(b, d.clone());
                changed = true;
            }
            let mut din = d;
            transfer(func, ps, noalias, b, &mut din, &iso, nw, eh, None);
            if let Some(old) = inn.get(&b) {
                din.union(old);
                din.cov.retain(|l| old.cov.contains(l));
            }
            if inn.get(&b) != Some(&din) {
                inn.insert(b, din);
                changed = true;
            }
        }
        if !changed {
            stable = true;
            break;
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
    let mut dead = Vec::new();
    for &b in &order {
        let mut d = out.get(&b).cloned().unwrap_or_default();
        transfer(func, ps, noalias, b, &mut d, &iso, nw, eh, Some(&mut dead));
    }
    for i in &dead {
        func.layout.remove_inst(*i);
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
