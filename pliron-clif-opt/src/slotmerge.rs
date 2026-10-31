//! Merge explicit stack slots whose live ranges are disjoint (LLVM's
//! stack-slot coloring, fed by `llvm.lifetime.{start,end}` markers where
//! MIR emitted them). Today every `alloca` gets
//! its own `ExplicitSlot`, so a body that materializes one aggregate per
//! match arm — `parse_item_kind` holds ~40 × 136 B across 20 arms — pays
//! the sum in every frame (~18 KB vs LLVM's ~900 B for the same body).
//! Deep but finite recursion that fits the thread stack under LLVM then
//! overflows under us (stage2 rustc SIGBUS on
//! `tests/ui/expr/if/expr-stack-overflow.rs`), and big frames cost cache
//! and stack-probing even when they fit.
//!
//! For each slot we trace the `stack_addr` result through pure ops, block
//! params, and load/store/call args to find every instruction that can
//! touch the slot's bytes, giving a live interval over layout order.
//! Slots with disjoint intervals are folded onto one representative
//! (largest size / alignment wins) and absorbed slots shrink to zero.
//!
//! Pointers that escape — stored to memory, returned, or passed to a
//! possibly-writing callee — keep their slot live to the end of the
//! function. Two exceptions count as point uses instead: an `sret`
//! argument (the ABI forbids the callee retaining it) and any argument to
//! a write-free callee (`nowrite`), which has nowhere to stash a pointer.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::entity::{EntityRef, SecondaryMap};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    ArgumentPurpose, Block, BlockArg, FuncRef, Function, Inst, InstructionData, Opcode, StackSlot,
    Value, ValueDef, instructions::CallInfo,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

/// Is `v` an `iconst` of literal zero? `coldargs` disguises its cold-arg
/// rebinds as `umulhi x, 0` — a provable 0 whose result doesn't observe
/// the operand's address at all.
fn iconst_zero(func: &Function, v: Value) -> bool {
    let v = func.dfg.resolve_aliases(v);
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return false;
    };
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => {
            let bits = func.dfg.value_type(v).bits();
            let mask = if bits >= 64 {
                u64::MAX
            } else {
                (1u64 << bits) - 1
            };
            (imm.bits() as u64) & mask == 0
        }
        _ => false,
    }
}

// Worklist propagation of a slot-set map over a function: branch args
// flow into destination params (a try_call's *return* payload inherits
// the call args' union); every other inst's results take the union of
// its args' sets. `wl`/`inq` are the caller's initial worklist.
fn propagate(
    func: &Function,
    users: &SecondaryMap<Value, Vec<Inst>>,
    map: &mut SecondaryMap<Value, Vec<StackSlot>>,
    mut wl: VecDeque<Inst>,
    mut inq: FxHashSet<Inst>,
    contains: Option<&Vec<FxHashSet<StackSlot>>>,
    skip_loads: bool,
) {
    while let Some(i) = wl.pop_front() {
        inq.remove(&i);
        if func.dfg.insts[i].opcode().is_branch() {
            let call_slots: Vec<StackSlot> = if func.dfg.insts[i].opcode().is_call() {
                let mut v = Vec::new();
                for &a in func.dfg.inst_args(i) {
                    for &s in &map[func.dfg.resolve_aliases(a)] {
                        if !v.contains(&s) {
                            v.push(s);
                        }
                    }
                }
                // A callee can read through the pointer it was handed:
                // payloads it returns may name any slot reachable
                // through the arg's bytes, not just the arg's slot.
                if let Some(c) = contains {
                    let s0: Vec<StackSlot> = v.clone();
                    for s in s0 {
                        for &s2 in &c[s.index()] {
                            if !v.contains(&s2) {
                                v.push(s2);
                            }
                        }
                    }
                }
                v
            } else {
                Vec::new()
            };
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                let blk = bc.block(&func.dfg.value_lists);
                for (k, ba) in bc.args(&func.dfg.value_lists).enumerate() {
                    let src: Vec<StackSlot> = match ba {
                        BlockArg::Value(v) => map[func.dfg.resolve_aliases(v)].clone(),
                        // A normal-return payload can be the arg pointer
                        // itself (`fn(&T) -> &T`). The exception payload
                        // can't: the unwinder's object is fresh memory,
                        // and a pointer a callee could stash inside it
                        // already escaped via the call's own arg handling.
                        BlockArg::TryCallRet(_) => call_slots.clone(),
                        BlockArg::TryCallExn(_) => Vec::new(),
                    };
                    if src.is_empty() {
                        continue;
                    }
                    let Some(&p) = func.dfg.block_params(blk).get(k) else {
                        continue;
                    };
                    // A float param can't hold a pointer or a byte image
                    // of one; every other param type can carry them.
                    if func.dfg.value_type(p).is_float() {
                        continue;
                    }
                    let mut grew = false;
                    for &s in &src {
                        if !map[p].contains(&s) {
                            map[p].push(s);
                            grew = true;
                        }
                    }
                    if grew {
                        for &u in &users[p] {
                            if inq.insert(u) {
                                wl.push_back(u);
                            }
                        }
                    }
                }
            }
            continue;
        }
        // `umulhi x, 0` is a provable constant (coldargs' disguised-zero
        // rebind): the result carries no address, so the arg's slots must
        // not flow into it — otherwise the `iadd` rebind reads as
        // ptr + ptr and escapes the slot anyway.
        if func.dfg.insts[i].opcode() == Opcode::Umulhi
            && func.dfg.inst_args(i).iter().any(|&a| iconst_zero(func, a))
        {
            continue;
        }
        // A load result is the pointee's bytes, not an address derived
        // from the input: `derived` flow through loads is set precisely
        // by the `contains` fixpoint instead of inheriting the source
        // slot — otherwise plain data (`ushr`/`icmp` on a loaded word)
        // would read as observing the source slot's address.
        if skip_loads && func.dfg.insts[i].opcode().can_load() {
            continue;
        }
        // `PLIRON_SM_PRECISE`: a compare's result is a bool — it carries
        // no address and no byte image (see the pure-op rule in `run`).
        if precise()
            && matches!(
                func.dfg.insts[i].opcode(),
                Opcode::Icmp | Opcode::Fcmp
            )
        {
            continue;
        }
        let mut acc: Vec<StackSlot> = Vec::new();
        for &a in func.dfg.inst_args(i) {
            for &s in &map[func.dfg.resolve_aliases(a)] {
                if !acc.contains(&s) {
                    acc.push(s);
                }
            }
        }
        if acc.is_empty() {
            continue;
        }
        for &r in func.dfg.inst_results(i) {
            let r = func.dfg.resolve_aliases(r);
            let mut grew = false;
            for &s in &acc {
                if !map[r].contains(&s) {
                    map[r].push(s);
                    grew = true;
                }
            }
            if grew {
                for &u in &users[r] {
                    if inq.insert(u) {
                        wl.push_back(u);
                    }
                }
            }
        }
    }
}

/// Resolve a lifetime-marker operand to the slots it names, chasing
/// pointer-affine defs (`stack_addr`, `iadd` rebase, `bitcast`). Run this
/// where the operand is still live — mid-pipeline, the marker's `Value`
/// may already have been folded away, and `derived` at slotmerge time
/// would resolve to nothing.
pub fn marker_slots(func: &Function, v: Value) -> Vec<StackSlot> {
    let mut v = func.dfg.resolve_aliases(v);
    for _ in 0..8 {
        let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
            return Vec::new();
        };
        if let InstructionData::StackAddr { stack_slot, .. } = func.dfg.insts[i] {
            return vec![stack_slot];
        }
        match func.dfg.insts[i].opcode() {
            Opcode::Iadd | Opcode::Bitcast => {
                v = func.dfg.resolve_aliases(func.dfg.inst_args(i)[0]);
            }
            _ => return Vec::new(),
        }
    }
    Vec::new()
}

/// Whether a marker on `v` with byte `size` (`None` or negative: the
/// whole object) covers all of the one slot [`marker_slots`] finds: `v`
/// is that slot's `stack_addr` at offset 0, and `size` reaches its end.
/// A marker on part of a slot (a field address, from a pass that put one
/// local inside another) says nothing about the slot's other bytes.
pub fn marker_whole(func: &Function, v: Value, size: Option<i128>) -> bool {
    let mut v = func.dfg.resolve_aliases(v);
    for _ in 0..8 {
        let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
            return false;
        };
        match func.dfg.insts[i] {
            InstructionData::StackAddr { stack_slot, offset, .. } => {
                let n = i128::from(func.sized_stack_slots[stack_slot].size);
                return i32::from(offset) == 0 && size.is_none_or(|s| s < 0 || s >= n);
            }
            _ if func.dfg.insts[i].opcode() == Opcode::Bitcast => {
                v = func.dfg.resolve_aliases(func.dfg.inst_args(i)[0]);
            }
            _ => return false,
        }
    }
    false
}

/// [`marker_slots`] for lowering's records. Under `PLIRON_SLOTPROM_LT=1`
/// a marker that covers only part of its slot ([`marker_whole`]) is
/// recorded with the slot listed twice: ambiguous to every consumer
/// (slotmerge and slotprom read singleton markers only), since a kill
/// of the whole slot would cut the other bytes' live ranges too.
pub fn marker_slots_lt(func: &Function, v: Value, size: Option<i128>, lt: bool) -> Vec<StackSlot> {
    let ss = marker_slots(func, v);
    if lt && let &[s] = ss.as_slice() && !marker_whole(func, v, size) {
        return vec![s, s];
    }
    ss
}

/// `PLIRON_SM_PRECISE=1` (default off until validated): pure (non-memory,
/// non-call) ops on a slot address are uses, not escapes, and a byte image
/// stored to untracked memory escapes only the pointers it carries, not
/// the slot it was read from.
fn precise() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::env_is("PLIRON_SM_PRECISE", "1"))
}

fn sm_take() -> bool {
    static LIMIT: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    static USED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    match *LIMIT.get_or_init(|| crate::env_get("PLIRON_SM_LIMIT").and_then(|v| v.parse().ok())) {
        None => true,
        Some(n) => USED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < n,
    }
}

/// `PLIRON_SM_MARKFLOW`, at pipeline entry while every marker resolves:
/// an end marker that is effectively at its block's entry — first in the
/// block, or preceded only by instructions that touch no memory — becomes
/// a kill on each incoming `jump` and `try_call` normal-return edge, keyed
/// by (terminator, destination index; `NORMAL_RETURN` for a `try_call`). Passes that delete the block
/// (jumpthread threading past it, edgefwd dropping a forwarding block)
/// retarget those terminators in place, so the kill still sits on the
/// path that used to run the marker; in tests/std, about half the lost
/// end markers were at block entry. `brif`/`br_table` arms are skipped: a
/// pass may swap them in place. Every end marker also gets a `BEFORE_INST`
/// kill on the call/store/terminator right after it, which survives
/// `merge_chains` folding the block into its predecessor.
/// Edge-kill index of a `try_call`'s normal return: the last
/// destination, wherever handler edits leave it.
const NORMAL_RETURN: u32 = u32::MAX;
/// Not an edge: the slot is dead just before this inst.
const BEFORE_INST: u32 = u32::MAX - 1;
/// Every slot is dead just before this inst (the slot field is unused):
/// the pipeline's `_Unwind_Resume` calls.
pub const KILL_ALL: u32 = u32::MAX - 2;
/// Not an edge: the slot is dead just after this inst (a call that only
/// quiet insts and jumps separate from the end marker).
const AFTER_INST: u32 = u32::MAX - 3;

fn dest_index(n: usize, k: u32) -> Option<usize> {
    match k {
        NORMAL_RETURN => n.checked_sub(1),
        k => ((k as usize) < n).then_some(k as usize),
    }
}

pub fn entry_kills(
    func: &Function,
    lifetimes: &[(Vec<StackSlot>, Option<Inst>, Block, bool)],
) -> Vec<(StackSlot, Inst, u32)> {
    let mut out = Vec::new();
    if lifetimes.iter().all(|l| l.3) {
        return out;
    }
    let cfg = ControlFlowGraph::with_function(func);
    let addrs = slot_addrs(func);
    let afterkill = crate::env_is("PLIRON_SM_AFTERKILL", "1");
    for (ds, anchor, b, start) in lifetimes {
        let (&[s], false) = (ds.as_slice(), *start) else { continue };
        if !func.layout.is_block_inserted(*b) {
            continue;
        }
        // The inst right after the marker, when no pass moves it earlier:
        // block merges keep its identity, so "dead just before it" outlives
        // the marker's block. (Loads are out: LICM hoists them.)
        // Quiet insts in between are skipped unless one touches `s`'s
        // address (a new life starting before the kill point).
        let mut next = match *anchor {
            None => func.layout.first_inst(*b),
            Some(a) if func.layout.inst_block(a) == Some(*b) => func.layout.next_inst(a),
            Some(_) => None,
        };
        for _ in 0..16 {
            let Some(n) = next else { break };
            if quiet(func, n)
                && !func.dfg.inst_values(n).any(|v| {
                    addrs[func.dfg.resolve_aliases(v)].as_ref().is_none_or(|x| x.contains(&s))
                })
                && !matches!(func.dfg.insts[n], InstructionData::StackAddr { stack_slot, .. } if stack_slot == s)
            {
                next = func.layout.next_inst(n);
                continue;
            }
            let op = func.dfg.insts[n].opcode();
            if (op.is_call() || op.can_store() || op.is_terminator() || op.can_trap()) && !op.can_load() {
                out.push((s, n, BEFORE_INST));
            }
            break;
        }
        if let Some(a) = *anchor {
            if func.layout.inst_block(a) != Some(*b) {
                continue;
            }
            if !func.layout.block_insts(*b).take_while(|&i| i != a).chain([a]).all(|i| quiet(func, i)) {
                continue;
            }
        }
        // Entering a quiet block that only jumps on to `b` is entering `b`:
        // walk back through such forwarding chains (jumpthread and edgefwd
        // collapse them, deleting every jump in them) and kill on their
        // in-edges too.
        let mut work = vec![*b];
        let mut seen: FxHashSet<Block> = FxHashSet::default();
        while let Some(x) = work.pop() {
            if !seen.insert(x) {
                continue;
            }
            for pr in cfg.pred_iter(x) {
                let d = &func.dfg.insts[pr.inst];
                let k = match d.opcode() {
                    Opcode::Jump => 0,
                    Opcode::TryCall | Opcode::TryCallIndirect => NORMAL_RETURN,
                    _ => continue,
                };
                let dests = d.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                if !dest_index(dests.len(), k)
                    .is_some_and(|k| dests[k].block(&func.dfg.value_lists) == x)
                {
                    continue;
                }
                out.push((s, pr.inst, k));
                if d.opcode() != Opcode::Jump {
                    continue;
                }
                if func.layout.block_insts(pr.block).all(|i| i == pr.inst || quiet(func, i)) {
                    work.push(pr.block);
                    continue;
                }
                // `PLIRON_SM_AFTERKILL=1`: the jump dies too when the
                // chain collapses (jumpthread merges the drop call's block
                // with its forwarding successors), but the call or store
                // before it keeps its identity — kill just after that,
                // if only quiet insts away from `s` sit in between. Calls
                // only: store sinking can move a store past a later use.
                if !afterkill {
                    continue;
                }
                let mut x = func.layout.prev_inst(pr.inst);
                while let Some(i) = x
                    && quiet(func, i)
                    && !func.dfg.inst_values(i).any(|v| addrs[func.dfg.resolve_aliases(v)].as_ref().is_none_or(|a| a.contains(&s)))
                    && !matches!(func.dfg.insts[i], InstructionData::StackAddr { stack_slot, .. } if stack_slot == s)
                {
                    x = func.layout.prev_inst(i);
                }
                if let Some(i) = x {
                    let op = func.dfg.insts[i].opcode();
                    if op.is_call() && !op.is_terminator() {
                        out.push((s, i, AFTER_INST));
                    }
                }
            }
        }
    }
    out
}

/// Which slots each value may hold an address into, through quiet ops —
/// what `entry_kills` asks of the insts it skips. One pass in reverse
/// postorder (a def's block precedes its uses' blocks, and inst order
/// within a block), so each query is a lookup. `None` = may be anything:
/// block params, and sets grown past 8 slots. Loads and call results are
/// `Some([])`: a pointer reloaded from memory was taken before the marker,
/// so it can't begin a new life of the slot.
fn slot_addrs(func: &Function) -> SecondaryMap<Value, Option<Vec<StackSlot>>> {
    let mut m: SecondaryMap<Value, Option<Vec<StackSlot>>> = SecondaryMap::with_default(None);
    let mut dt = DominatorTree::new();
    dt.compute(func, &ControlFlowGraph::with_function(func));
    for &b in dt.cfg_postorder().iter().rev() {
        for i in func.layout.block_insts(b) {
            let set = match func.dfg.insts[i] {
                InstructionData::StackAddr { stack_slot, .. } => Some(vec![stack_slot]),
                _ if quiet(func, i) => {
                    let mut acc = Some(Vec::new());
                    for &a in func.dfg.inst_args(i) {
                        let a = func.dfg.resolve_aliases(a);
                        // An arg defined later in this order (unreachable or
                        // irreducible flow) reads as `None` = anything.
                        match (&mut acc, &m[a]) {
                            (Some(v), Some(x)) => {
                                for &s in x {
                                    if !v.contains(&s) {
                                        v.push(s);
                                    }
                                }
                            }
                            _ => acc = None,
                        }
                    }
                    acc.filter(|v| v.len() <= 8)
                }
                _ => Some(Vec::new()),
            };
            for &r in func.dfg.inst_results(i) {
                m[r] = set.clone();
            }
        }
    }
    m
}

/// Touches no memory and can't leave the block: an end marker after a run
/// of these is effectively at the block's entry.
fn quiet(func: &Function, i: Inst) -> bool {
    let op = func.dfg.insts[i].opcode();
    !(op.is_call()
        || op.can_load()
        || op.can_store()
        || op.other_side_effects()
        || op.can_trap()
        || op.is_terminator())
}

/// `(slot, byte offset)` an address points at: `stack_addr` under
/// constant adds, through block params every predecessor feeds the same
/// value (`same_in`; lowering threads a `try_call`'s slot operand into its
/// successors that way).
fn slot_at(func: &Function, same_in: &FxHashMap<Value, Value>, v: Value, imm: i64) -> Option<(StackSlot, i64)> {
    let mut v = v;
    let mut off = imm;
    for _ in 0..8 {
        v = func.dfg.resolve_aliases(v);
        if let Some(&x) = same_in.get(&v) {
            v = x;
            continue;
        }
        let i = func.dfg.value_def(v).inst()?;
        match func.dfg.insts[i] {
            InstructionData::StackAddr { stack_slot, offset, .. } => {
                return Some((stack_slot, off + i64::from(i32::from(offset))));
            }
            InstructionData::Binary { opcode: Opcode::Iadd, args } => {
                let k = |x: Value| crate::jumpthread::known(func, &FxHashMap::default(), x);
                if let Some(c) = k(args[1]) {
                    off += c as i64;
                    v = args[0];
                } else if let Some(c) = k(args[0]) {
                    off += c as i64;
                    v = args[1];
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

/// `PLIRON_SM_COALESCE`: whole or partial copies `v = load S+k; store v,
/// D+k` (v used only by the store) whose source dies and destination is
/// born inside the copy. Keyed by D: (S, the window's positions, its
/// (load, store) pairs). A group qualifies when its pairs cover disjoint
/// bytes, nothing else in the window can touch S or D (no calls; no
/// memory op whose address may derive from either — any memory op at all
/// if either escaped), and S's and D's live sets meet only in the window.
#[allow(clippy::too_many_arguments)]
fn copy_groups(
    func: &Function,
    order: &[Block],
    ipos: &SecondaryMap<Inst, u32>,
    users: &SecondaryMap<Value, Vec<Inst>>,
    derived: &SecondaryMap<Value, Vec<StackSlot>>,
    iv: &[(u32, u32)],
    segs: &[Option<Vec<(u32, u32)>>],
    escaped: &[bool],
    esc_seen: &[bool],
) -> FxHashMap<StackSlot, (StackSlot, (u32, u32), Vec<(Inst, Inst)>)> {
    let nslots = iv.len();
    let mut out = FxHashMap::default();
    let mut taken: FxHashSet<StackSlot> = FxHashSet::default();
    let coalcall = crate::env_is("PLIRON_SM_COALCALL", "1");
    // Block param -> the one value all its incoming edges pass.
    let mut incoming: FxHashMap<Value, Option<Value>> = FxHashMap::default();
    for &b in order {
        for i in func.layout.block_insts(b) {
            for bc in func.dfg.insts[i].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                let dst = bc.block(&func.dfg.value_lists);
                let params = func.dfg.block_params(dst);
                for (k, a) in bc.args(&func.dfg.value_lists).enumerate() {
                    let Some(&p) = params.get(k) else { break };
                    let a = match a {
                        BlockArg::Value(a) => Some(func.dfg.resolve_aliases(a)),
                        _ => None,
                    };
                    incoming
                        .entry(p)
                        .and_modify(|e| {
                            if *e != a {
                                *e = None;
                            }
                        })
                        .or_insert(a);
                }
            }
        }
    }
    let same_in: FxHashMap<Value, Value> =
        incoming.into_iter().filter_map(|(p, a)| Some((p, a?))).filter(|(p, a)| p != a).collect();
    for &b in order {
        // (S, D) -> [(load, store, offset, bytes)]
        let mut groups: FxHashMap<(StackSlot, StackSlot), Vec<(Inst, Inst, i64, i64)>> = FxHashMap::default();
        for i in func.layout.block_insts(b) {
            let InstructionData::Store { opcode: Opcode::Store, args: [v, da], offset: doff, .. } = func.dfg.insts[i]
            else {
                continue;
            };
            let v = func.dfg.resolve_aliases(v);
            let Some(l) = func.dfg.value_def(v).inst() else { continue };
            let InstructionData::Load { opcode: Opcode::Load, arg: sa, offset: soff, .. } = func.dfg.insts[l] else {
                continue;
            };
            if func.layout.inst_block(l) != Some(b) || ipos[l] > ipos[i] || users[v].as_slice() != [i] {
                continue;
            }
            let (Some((s, ks)), Some((d, kd))) =
                (slot_at(func, &same_in, sa, i64::from(i32::from(soff))), slot_at(func, &same_in, da, i64::from(i32::from(doff))))
            else {
                continue;
            };
            if s == d || ks != kd || s.index() >= nslots || d.index() >= nslots {
                continue;
            }
            let n = i64::from(func.dfg.value_type(v).bytes());
            groups.entry((s, d)).or_default().push((l, i, ks, n));
        }
        for ((s, d), mut ps) in groups {
            if escaped[s.index()] || escaped[d.index()] || iv[s.index()].0 == u32::MAX || iv[d.index()].0 == u32::MAX
                || taken.contains(&d)
            {
                continue;
            }
            ps.sort_by_key(|p| p.2);
            if ps.windows(2).any(|w| w[0].2 + w[0].3 > w[1].2) {
                continue;
            }
            let lo = ps.iter().map(|p| ipos[p.0].min(ipos[p.1])).min().unwrap();
            let hi = ps.iter().map(|p| ipos[p.0].max(ipos[p.1])).max().unwrap();
            let mine: FxHashSet<Inst> = ps.iter().flat_map(|p| [p.0, p.1]).collect();
            let either_escaped = esc_seen[s.index()] || esc_seen[d.index()];
            let touches = |x: Value| derived[func.dfg.resolve_aliases(x)].iter().any(|&t| t == s || t == d);
            let clean = func.layout.block_insts(b).filter(|&x| (lo..=hi).contains(&ipos[x]) && !mine.contains(&x)).all(|x| {
                let op = func.dfg.insts[x].opcode();
                if op.is_call() {
                    return false;
                }
                if op.can_load() || op.can_store() {
                    return !either_escaped && !func.dfg.inst_args(x).iter().any(|&a| touches(a));
                }
                true
            });
            if !clean {
                continue;
            }
            let ls = segs[s.index()].clone().unwrap_or_else(|| vec![iv[s.index()]]);
            let ld = segs[d.index()].clone().unwrap_or_else(|| vec![iv[d.index()]]);
            if !seg_isect(&ls, &ld).iter().all(|&(a, z)| lo <= a && z <= hi) {
                continue;
            }
            taken.insert(d);
            out.insert(d, (s, (lo, hi), ps.iter().map(|p| (p.0, p.1)).collect()));
        }
        // `PLIRON_SM_COALCALL=1`: a whole-slot `memcpy(D, S, size)` call is
        // the same copy in one inst (a `Drop` value built in a temporary,
        // then moved into its binding). Recorded as the pair (call, call).
        if !coalcall {
            continue;
        }
        for i in func.layout.block_insts(b) {
            use cranelift_codegen::ir::{ExternalName, LibCall};
            let CallInfo::Direct(fr, cargs) = func.dfg.insts[i].analyze_call(&func.dfg.value_lists, &func.dfg.exception_tables)
            else {
                continue;
            };
            if !matches!(func.dfg.ext_funcs[fr].name, ExternalName::LibCall(LibCall::Memcpy)) || cargs.len() != 3 {
                continue;
            }
            let len = func.dfg.value_def(func.dfg.resolve_aliases(cargs[2])).inst().and_then(|l| match func.dfg.insts[l] {
                InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
                _ => None,
            });
            let (Some((d, 0)), Some((s, 0)), Some(n)) =
                (slot_at(func, &same_in, cargs[0], 0), slot_at(func, &same_in, cargs[1], 0), len)
            else {
                continue;
            };
            if s == d
                || s.index() >= nslots
                || d.index() >= nslots
                || n != i64::from(func.sized_stack_slots[s].size)
                || n != i64::from(func.sized_stack_slots[d].size)
                || escaped[s.index()]
                || escaped[d.index()]
                || iv[s.index()].0 == u32::MAX
                || iv[d.index()].0 == u32::MAX
                || taken.contains(&d)
            {
                continue;
            }
            let p = ipos[i];
            let ls = segs[s.index()].clone().unwrap_or_else(|| vec![iv[s.index()]]);
            let ld = segs[d.index()].clone().unwrap_or_else(|| vec![iv[d.index()]]);
            if !seg_isect(&ls, &ld).iter().all(|&(a, z)| p <= a && z <= p) {
                continue;
            }
            taken.insert(d);
            out.insert(d, (s, (p, p), vec![(i, i)]));
        }
    }
    out
}

/// `a` minus `b`, both sorted disjoint segment lists.
fn seg_sub(a: &[(u32, u32)], b: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for &(mut lo, hi) in a {
        for &(bl, bh) in b {
            if bh < lo || bl > hi {
                continue;
            }
            if bl > lo {
                out.push((lo, bl - 1));
            }
            lo = bh.saturating_add(1);
            if lo > hi {
                break;
            }
        }
        if lo <= hi {
            out.push((lo, hi));
        }
    }
    out
}

/// Intersection of two sorted disjoint segment lists.
fn seg_isect(a: &[(u32, u32)], b: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        let (lo, hi) = (a[i].0.max(b[j].0), a[i].1.min(b[j].1));
        if lo <= hi {
            out.push((lo, hi));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// `PLIRON_SM_MARKFLOW`: the live region of a marked, escaped slot.
/// Storage is live from any use (a pointer to it only exists after its
/// `stack_addr` is used) until a StorageDead kill on every path; the
/// slot's live set is that region as position segments (`segs`) — not a
/// hull: MIR has no StorageDead on unwind paths, so most slots reach the
/// cleanup blocks at the end of RPO, and a hull would span everything in
/// between. `iv` keeps the hull for the convex consumers. Missing markers can
/// only extend it: a pass that deleted or duplicated a marker's block
/// leaves paths without a kill, which then stay live to the return. A
/// kill whose anchor didn't survive sits at its block's end, and is
/// dropped when the block also uses the slot (a new life may start after
/// the true marker).
#[allow(clippy::too_many_arguments)]
fn mark_flow(
    func: &Function,
    order: &[Block],
    bfirst: &SecondaryMap<Block, u32>,
    blast: &SecondaryMap<Block, u32>,
    ipos: &SecondaryMap<Inst, u32>,
    kills: &[(StackSlot, Block, u32, bool)],
    edge_kills: &[(StackSlot, Inst, u32)],
    marked: &[bool],
    esc_seen: &[bool],
    uses: &[Vec<u32>],
    iv: &mut [(u32, u32)],
    segs: &mut [Option<Vec<(u32, u32)>>],
) {
    let nslots = marked.len();
    let mut tid = vec![usize::MAX; nslots];
    let mut tracked = Vec::new();
    for s in 0..nslots {
        if marked[s] && esc_seen[s] {
            tid[s] = tracked.len();
            tracked.push(s);
        }
    }
    if tracked.is_empty() {
        return;
    }
    let nb = order.len();
    let mut bidx = SecondaryMap::<Block, usize>::with_default(usize::MAX);
    for (k, &b) in order.iter().enumerate() {
        bidx[b] = k;
    }
    let block_of = |p: u32| order.partition_point(|&b| bfirst[b] <= p) - 1;
    // Events per block: (pos, order, tracked id), order 0 = kill just
    // before the inst at pos (`BEFORE_INST` kills), 1 = use, 2 = kill
    // just after it (an exact anchor's).
    let mut ev: Vec<Vec<(u32, u8, usize)>> = vec![Vec::new(); nb];
    for (t, &s) in tracked.iter().enumerate() {
        for &p in &uses[s] {
            ev[block_of(p)].push((p, 1, t));
        }
    }
    for &(s, n, k) in edge_kills {
        let Some(b) = func.layout.inst_block(n).filter(|&b| bidx[b] != usize::MAX) else { continue };
        if k == KILL_ALL {
            ev[bidx[b]].extend((0..tracked.len()).map(|t| (ipos[n], 0, t)));
            continue;
        }
        let t = tid.get(s.index()).copied().unwrap_or(usize::MAX);
        if k == BEFORE_INST && t != usize::MAX {
            ev[bidx[b]].push((ipos[n], 0, t));
        }
        if k == AFTER_INST && t != usize::MAX {
            ev[bidx[b]].push((ipos[n], 2, t));
        }
    }
    for &(s, b, p, exact) in kills {
        let t = tid[s.index()];
        if t == usize::MAX || bidx[b] == usize::MAX {
            continue;
        }
        let k = bidx[b];
        if !exact && ev[k].iter().any(|&(_, o, t2)| o == 1 && t2 == t) {
            continue;
        }
        ev[k].push((p, 2, t));
    }
    for e in &mut ev {
        e.sort_unstable();
    }
    let w = tracked.len().div_ceil(64);
    let get = |v: &[u64], t: usize| v[t / 64] >> (t % 64) & 1 != 0;
    // gen/kill summary: the last event per slot decides its live-out.
    let mut gen_b = vec![0u64; nb * w];
    let mut kill_b = vec![0u64; nb * w];
    for k in 0..nb {
        for &(_, o, t) in &ev[k] {
            let (g, x) = (&mut gen_b[k * w..][..w], &mut kill_b[k * w..][..w]);
            if o != 1 {
                x[t / 64] |= 1 << (t % 64);
                g[t / 64] &= !(1 << (t % 64));
            } else {
                g[t / 64] |= 1 << (t % 64);
                x[t / 64] &= !(1 << (t % 64));
            }
        }
    }
    let cfg = ControlFlowGraph::with_function(func);
    // Edge kills: (terminator, destination block) -> slots dead on entry
    // through that terminator. A slot is masked only if every destination
    // index of the terminator that reaches the block kills it (a `brif`
    // with both arms to one block, one killed, still carries it).
    let mut emask: FxHashMap<(Inst, Block), Vec<u64>> = FxHashMap::default();
    {
        let mut by_term: FxHashMap<Inst, Vec<(u32, usize)>> = FxHashMap::default();
        for &(s, term, k) in edge_kills {
            let t = tid.get(s.index()).copied().unwrap_or(usize::MAX);
            if t != usize::MAX && k != BEFORE_INST && k != AFTER_INST && k != KILL_ALL && func.layout.inst_block(term).is_some() {
                by_term.entry(term).or_default().push((k, t));
            }
        }
        for (term, ks) in by_term {
            let dests: Vec<Block> = func.dfg.insts[term]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .map(|bc| bc.block(&func.dfg.value_lists))
                .collect();
            for &d in &dests {
                let mut m = vec![0u64; w];
                for t in 0..tracked.len() {
                    let all = dests.iter().enumerate().filter(|&(_, &x)| x == d).all(|(k, _)| {
                        ks.iter().any(|&(k2, t2)| dest_index(dests.len(), k2) == Some(k) && t2 == t)
                    });
                    if all {
                        m[t / 64] |= 1 << (t % 64);
                    }
                }
                if m.iter().any(|&x| x != 0) {
                    emask.insert((term, d), m);
                }
            }
        }
    }
    let mut live_in = vec![0u64; nb * w];
    let mut live_out = vec![0u64; nb * w];
    loop {
        let mut changed = false;
        for k in 0..nb {
            let mut inn = vec![0u64; w];
            for pr in cfg.pred_iter(order[k]) {
                let pk = bidx[pr.block];
                if pk != usize::MAX {
                    let m = emask.get(&(pr.inst, order[k]));
                    for j in 0..w {
                        inn[j] |= live_out[pk * w + j] & !m.map_or(0, |m| m[j]);
                    }
                }
            }
            for j in 0..w {
                let o = inn[j] & !kill_b[k * w + j] | gen_b[k * w + j];
                if o != live_out[k * w + j] || inn[j] != live_in[k * w + j] {
                    changed = true;
                    live_out[k * w + j] = o;
                    live_in[k * w + j] = inn[j];
                }
            }
        }
        if !changed {
            break;
        }
    }
    // Walk the blocks in position order: `open[t]` is the start of the
    // segment slot t is live in, if any.
    let mut out: Vec<Vec<(u32, u32)>> = vec![Vec::new(); tracked.len()];
    let mut open: Vec<Option<u32>> = vec![None; tracked.len()];
    for k in 0..nb {
        let b = order[k];
        for t in 0..tracked.len() {
            open[t] = get(&live_in[k * w..][..w], t).then_some(bfirst[b]);
        }
        for &(p, o, t) in &ev[k] {
            if o != 1 {
                // A kill before the inst at p leaves p itself dead (an
                // empty segment when it opens there: nothing to add).
                let hi = if o == 0 { p.checked_sub(1) } else { Some(p) };
                if let Some(lo) = open[t].take()
                    && let Some(hi) = hi.filter(|&h| h >= lo)
                {
                    add_seg(&mut out[t], (lo, hi));
                }
            } else if open[t].is_none() {
                open[t] = Some(p);
            }
        }
        for t in 0..tracked.len() {
            if let Some(lo) = open[t] {
                add_seg(&mut out[t], (lo, blast[b]));
            }
        }
    }
    for (t, &s) in tracked.iter().enumerate() {
        let sg = std::mem::take(&mut out[t]);
        if let (Some(f), Some(l)) = (sg.first(), sg.last()) {
            iv[s] = (f.0, l.1);
        }
        segs[s] = Some(sg);
    }
}

/// `PLIRON_SM_FLOWIV=1`: byte liveness for every bounded slot that
/// doesn't escape and `mark_flow` doesn't track, as position segments
/// (LLVM StackColoring's block liveness, without markers). Such a slot's
/// bytes are only touched at its recorded uses, so storage is needed
/// only at those uses and wherever a later read can still see an earlier
/// write. The convex RPO interval also covered every block laid out
/// between two uses: a `match` whose arms share a tail placed late made
/// each arm's temporaries look live across all the other arms
/// (regex-syntax `NestLimiter::check`: 19 slots, each live 16..863 of
/// 877, no merge at all), and a loop around a `match` (an inlined
/// visitor) pinned every arm's temporaries across the whole loop.
///
/// Per use: a *read* (gen) is any recorded read, plus a store of the
/// slot's own address — its bytes become reachable through the
/// container, whose live range `contains` adds only from that store on.
/// A *kill* is the start of a straight-line run of constant-offset
/// stores (or a `mem*` call) that covers every byte before any read: the
/// slot is dead just before it. (An `sret` call is not a kill: the
/// callee may write only part of the slot — an enum payload whose
/// discriminant was stored first.) Everything else (partial writes, address
/// arithmetic, branch arguments) occupies its own position only.
///
/// Liveness is clipped to the points reachable from some use: before a
/// slot's first access its bytes are uninitialized, so a read with no
/// definition on the way (an out-param call) doesn't reach back to the
/// function entry. The convex interval made the same assumption.
///
/// Under `PLIRON_SM_MARKFLOW` the end-marker kills `mark_flow` trusts
/// for escaped slots (MIR StorageDead: nothing reads the old bytes
/// after) also stop liveness here — `kills` (just after an exact anchor,
/// or at the end of a block with no use of the slot), `BEFORE_INST` /
/// `KILL_ALL` kills, and terminator edge kills. Start markers (MIR
/// StorageLive: the old bytes are dead, LLVM's `lifetime.start`) kill
/// too, when still exactly placed: their anchor inst is in their own
/// block, or they head a block still in the layout. Without them an
/// `sret` call (a read here: the callee may write only part of the
/// slot) or a partly written enum passed to a call kept its slot live
/// round every loop it sat in — regex-syntax `NestLimiter::check`'s
/// 128-byte `ast::Error` across the whole inlined visitor loop.
#[allow(clippy::too_many_arguments)]
fn use_flow(
    func: &Function,
    order: &[Block],
    bfirst: &SecondaryMap<Block, u32>,
    blast: &SecondaryMap<Block, u32>,
    ipos: &SecondaryMap<Inst, u32>,
    derived: &SecondaryMap<Value, Vec<StackSlot>>,
    uses: &[Vec<u32>],
    reads: &[Vec<Inst>],
    writes: &[Vec<Inst>],
    escaped: &[bool],
    kills: &[(StackSlot, Block, u32, bool)],
    edge_kills: &[(StackSlot, Inst, u32)],
    starts: &[(StackSlot, Block, u32, u8)],
    iv: &mut [(u32, u32)],
    segs: &mut [Option<Vec<(u32, u32)>>],
) {
    let tracked: Vec<usize> = (0..iv.len())
        .filter(|&s| iv[s].0 != u32::MAX && !escaped[s] && segs[s].is_none() && !uses[s].is_empty())
        .collect();
    if tracked.is_empty() {
        return;
    }
    let nb = order.len();
    let mut bidx = SecondaryMap::<Block, usize>::with_default(usize::MAX);
    let mut inst_at: Vec<Inst> = Vec::new();
    for (k, &b) in order.iter().enumerate() {
        bidx[b] = k;
        inst_at.extend(func.layout.block_insts(b));
    }
    let block_of = |p: u32| order.partition_point(|&b| bfirst[b] <= p) - 1;
    let cfg = ControlFlowGraph::with_function(func);
    let mut memo: FxHashMap<(Value, StackSlot), Option<i64>> = FxHashMap::default();
    let mut base_off = |v: Value, s: StackSlot| slot_offset(func, &cfg, &mut memo, v, s);
    // Byte range a definite write of `s` at `i` stores, when constant.
    let mut write_range = |i: Inst, s: StackSlot| -> Option<(i64, i64)> {
        let a = func.dfg.inst_args(i);
        match func.dfg.insts[i] {
            InstructionData::Store { opcode, args, offset, .. } => {
                let n = match opcode {
                    Opcode::Istore8 => 1,
                    Opcode::Istore16 => 2,
                    Opcode::Istore32 => 4,
                    Opcode::Store => i64::from(func.dfg.value_type(args[0]).bytes()),
                    _ => return None,
                };
                // Wild offsets (fuzzed code) overflow: no known range.
                let o = base_off(args[1], s)?.checked_add(i64::from(offset))?;
                Some((o, o.checked_add(n)?))
            }
            _ if func.dfg.insts[i].opcode().is_call() => {
                // mem* libcall: (dst, src|byte, len).
                let o = base_off(*a.first()?, s)?;
                let l = func.dfg.value_def(func.dfg.resolve_aliases(*a.get(2)?)).inst()?;
                match func.dfg.insts[l] {
                    InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some((o, o.checked_add(imm.bits())?)),
                    _ => None,
                }
            }
            _ => None,
        }
    };
    let mut tid = vec![usize::MAX; iv.len()];
    for (t, &s) in tracked.iter().enumerate() {
        tid[s] = t;
    }
    // Per block, per tracked slot: events (slot, pos, order, gen, kill),
    // sorted. Order 0 = dead just before the inst at pos, 1 = the use at
    // pos, 2 = dead just after it. A use's `kill` is a covering write
    // run's start (dead before it).
    let mut ev: Vec<Vec<(usize, u32, u8, bool, bool)>> = vec![Vec::new(); nb];
    for (t, &s) in tracked.iter().enumerate() {
        let ss = StackSlot::from_u32(s as u32);
        let size = i64::from(func.sized_stack_slots[ss].size);
        let rd: FxHashSet<Inst> = reads[s].iter().copied().collect();
        let wr: FxHashSet<Inst> = writes[s].iter().copied().collect();
        let mut ps: Vec<u32> = uses[s].clone();
        ps.sort_unstable();
        ps.dedup();
        // Forward scan for covering write runs, block by block.
        let mut cur_block = usize::MAX;
        let mut run: Option<usize> = None; // index into this slot's events
        let mut cov: Vec<(i64, i64)> = Vec::new();
        let mut mine: Vec<(usize, u32, u8, bool, bool)> = Vec::new();
        for &p in &ps {
            let k = block_of(p);
            if k != cur_block {
                cur_block = k;
                run = None;
                cov.clear();
            }
            let i = inst_at[p as usize];
            let op = func.dfg.insts[i].opcode();
            let leaks_addr = matches!(op, Opcode::Store | Opcode::Istore8 | Opcode::Istore16 | Opcode::Istore32 | Opcode::AtomicStore)
                && func.dfg.inst_args(i).first().is_some_and(|&v| derived[func.dfg.resolve_aliases(v)].contains(&ss));
            let reads_here = rd.contains(&i) || leaks_addr;
            mine.push((k, p, 1, reads_here, false));
            if reads_here {
                run = None;
                cov.clear();
            } else if let Some((lo, hi)) = wr.contains(&i).then(|| write_range(i, ss)).flatten() {
                let r = *run.get_or_insert(mine.len() - 1);
                add_seg_i64(&mut cov, (lo.max(0), hi.min(size)));
                if cov.first().is_some_and(|&(a, b)| a <= 0 && b >= size) {
                    mine[r].4 = true;
                    run = None;
                    cov.clear();
                }
            }
        }
        for (k, p, o, g, x) in mine {
            ev[k].push((t, p, o, g, x));
        }
    }
    for &(s, b, p, exact) in kills {
        let (t, k) = (tid.get(s.index()).copied().unwrap_or(usize::MAX), bidx[b]);
        if t == usize::MAX || k == usize::MAX {
            continue;
        }
        // As in `mark_flow`: an inexact kill (block end) only where the
        // block has no use of the slot.
        if !exact && ev[k].iter().any(|e| e.0 == t && e.2 == 1) {
            continue;
        }
        ev[k].push((t, p, 2, false, true));
    }
    for &(s, b, p, o) in starts {
        let (t, k) = (tid.get(s.index()).copied().unwrap_or(usize::MAX), bidx[b]);
        if t != usize::MAX && k != usize::MAX {
            ev[k].push((t, p, o, false, true));
        }
    }
    for &(s, n, kk) in edge_kills {
        let Some(k) = func.layout.inst_block(n).map(|b| bidx[b]).filter(|&k| k != usize::MAX) else { continue };
        if kk == KILL_ALL {
            ev[k].extend((0..tracked.len()).map(|t| (t, ipos[n], 0, false, true)));
        } else if (kk == BEFORE_INST || kk == AFTER_INST)
            && let Some(&t) = tid.get(s.index()).filter(|&&t| t != usize::MAX)
        {
            ev[k].push((t, ipos[n], if kk == AFTER_INST { 2 } else { 0 }, false, true));
        }
    }
    for e in &mut ev {
        e.sort_unstable();
    }
    let w = tracked.len().div_ceil(64);
    // Transfer: the first event from the top decides (gen or kill).
    let mut gen_b = vec![0u64; nb * w];
    let mut kill_b = vec![0u64; nb * w];
    for k in 0..nb {
        let mut seen_t = usize::MAX;
        for &(t, _, o, g, x) in &ev[k] {
            if t == seen_t {
                continue;
            }
            if g {
                gen_b[k * w + t / 64] |= 1 << (t % 64);
                seen_t = t;
            } else if x || o != 1 {
                kill_b[k * w + t / 64] |= 1 << (t % 64);
                seen_t = t;
            }
        }
    }
    // fin: reachable from a use over at least one edge.
    let mut any_b = vec![0u64; nb * w];
    for k in 0..nb {
        for &(t, _, o, _, _) in &ev[k] {
            if o == 1 {
                any_b[k * w + t / 64] |= 1 << (t % 64);
            }
        }
    }
    let mut fin = vec![0u64; nb * w];
    loop {
        let mut changed = false;
        for k in 0..nb {
            for pr in cfg.pred_iter(order[k]) {
                let pk = bidx[pr.block];
                if pk == usize::MAX {
                    continue;
                }
                for j in 0..w {
                    let v = fin[pk * w + j] | any_b[pk * w + j];
                    if v & !fin[k * w + j] != 0 {
                        fin[k * w + j] |= v;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    // Per block: (successor index, edge mask) — an edge kill masks a slot
    // only if every destination index of the terminator that reaches
    // that successor kills it (as in `mark_flow`).
    let mut by_term: FxHashMap<Inst, Vec<(u32, usize)>> = FxHashMap::default();
    for &(s, term, kk) in edge_kills {
        let t = tid.get(s.index()).copied().unwrap_or(usize::MAX);
        if t != usize::MAX && kk != BEFORE_INST && kk != AFTER_INST && kk != KILL_ALL && func.layout.inst_block(term).is_some() {
            by_term.entry(term).or_default().push((kk, t));
        }
    }
    let succs = succ_masks(func, order, &bidx, &by_term, w);
    let mut live_in = vec![0u64; nb * w];
    let mut live_out = vec![0u64; nb * w];
    loop {
        let mut changed = false;
        for k in (0..nb).rev() {
            for j in 0..w {
                let mut o = 0u64;
                for (qk, m) in &succs[k] {
                    o |= live_in[qk * w + j] & !m[j];
                }
                let inn = gen_b[k * w + j] | (o & !kill_b[k * w + j]);
                if o != live_out[k * w + j] || inn != live_in[k * w + j] {
                    live_out[k * w + j] = o;
                    live_in[k * w + j] = inn;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let get = |v: &[u64], k: usize, t: usize| v[k * w + t / 64] >> (t % 64) & 1 != 0;
    let mut out: Vec<Vec<(u32, u32)>> = vec![Vec::new(); tracked.len()];
    for k in 0..nb {
        let b = order[k];
        // Walk each slot's events bottom-up from its live-out.
        let mut lo_i = 0;
        let evs = &ev[k];
        for t in 0..tracked.len() {
            let mut hi_i = lo_i;
            while hi_i < evs.len() && evs[hi_i].0 == t {
                hi_i += 1;
            }
            // Defined part of the block: from its entry when reachable
            // from a use, else from its own first use.
            let floor = if get(&fin, k, t) {
                Some(bfirst[b])
            } else {
                evs[lo_i..hi_i].iter().find(|e| e.2 == 1).map(|e| e.1)
            };
            let mut live = get(&live_out, k, t);
            let mut open = live.then_some(blast[b]);
            let mut seg = |lo: u32, hi: u32| {
                if let Some(f) = floor
                    && lo.max(f) <= hi
                {
                    add_seg(&mut out[t], (lo.max(f), hi));
                }
            };
            for &(_, p, o, g, x) in evs[lo_i..hi_i].iter().rev() {
                match o {
                    1 => {
                        let h = *open.get_or_insert(p);
                        let before = g || (live && !x);
                        if !before {
                            seg(p, h);
                            open = None;
                        }
                        live = before;
                    }
                    // Dead just after p / just before p.
                    _ => {
                        let lo = if o == 2 { p + 1 } else { p };
                        if let Some(h) = open.take()
                            && lo <= h
                        {
                            seg(lo, h);
                        }
                        live = false;
                    }
                }
            }
            if let Some(h) = open {
                seg(bfirst[b], h);
            }
            lo_i = hi_i;
        }
    }
    for (t, &s) in tracked.iter().enumerate() {
        let sg = std::mem::take(&mut out[t]);
        if let (Some(f), Some(l)) = (sg.first(), sg.last()) {
            iv[s] = (f.0, l.1);
            segs[s] = Some(sg);
        }
    }
}

/// Per block (in `order`): its terminator's distinct successors (as
/// `order` indices) with a mask of the ids killed on that edge. As in
/// `mark_flow`, an id is masked only if every destination index of the
/// terminator that reaches the successor kills it (a `brif` with both
/// arms to one block, one killed, still carries it). `by_term`:
/// terminator -> (edge-kill destination index, id).
fn succ_masks(
    func: &Function,
    order: &[Block],
    bidx: &SecondaryMap<Block, usize>,
    by_term: &FxHashMap<Inst, Vec<(u32, usize)>>,
    w: usize,
) -> Vec<Vec<(usize, Vec<u64>)>> {
    order
        .iter()
        .map(|&b| {
            let Some(term) = func.layout.last_inst(b) else { return Vec::new() };
            let dests: Vec<Block> = func.dfg.insts[term]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .map(|bc| bc.block(&func.dfg.value_lists))
                .collect();
            let ks = by_term.get(&term);
            let mut out: Vec<(usize, Vec<u64>)> = Vec::new();
            for (j, &d) in dests.iter().enumerate() {
                let qk = bidx[d];
                if qk == usize::MAX || dests[..j].contains(&d) {
                    continue;
                }
                let mut m = vec![0u64; w];
                if let Some(ks) = ks {
                    for &(_, t) in ks {
                        let all = dests.iter().enumerate().filter(|&(_, &x)| x == d).all(|(i2, _)| {
                            ks.iter().any(|&(k2, t2)| dest_index(dests.len(), k2) == Some(i2) && t2 == t)
                        });
                        if all {
                            m[t / 64] |= 1 << (t % 64);
                        }
                    }
                }
                out.push((qk, m));
            }
            out
        })
        .collect()
}

/// Slots whose lifetime markers the code contradicts: a recorded read is
/// reachable from one of the slot's kills (end markers, edge and
/// before-inst kills, exactly placed start markers — the set `mark_flow`
/// and `use_flow` would apply) with no write of the slot on the way, so
/// it would observe bytes the markers call dead. Passes that forward or
/// sink memory accesses don't move markers: stage-2
/// `PromoteTemps::run_pass` reads a 56-byte slot in a block far past an
/// end-marker kill, and poisoning it after the kill freed a garbage
/// `Vec`. Any write clears the dead state (partial writes too — this
/// only has to catch contradictions, not prove liveness). A call the slot
/// is passed to counts as a read after an end marker, but after a start
/// marker it presumably writes (an out-param or `sret` filling the fresh
/// storage) and clears the state; a load is a read either way.
#[allow(clippy::too_many_arguments)]
fn stale_markers(
    func: &Function,
    order: &[Block],
    bfirst: &SecondaryMap<Block, u32>,
    ipos: &SecondaryMap<Inst, u32>,
    uses: &[Vec<u32>],
    reads: &[Vec<Inst>],
    writes: &[Vec<Inst>],
    kills: &[(StackSlot, Block, u32, bool)],
    edge_kills: &[(StackSlot, Inst, u32)],
    starts: &[(StackSlot, Block, u32, u8)],
) -> Vec<bool> {
    let nslots = uses.len();
    let nb = order.len();
    let mut bidx = SecondaryMap::<Block, usize>::with_default(usize::MAX);
    for (k, &b) in order.iter().enumerate() {
        bidx[b] = k;
    }
    let block_of = |p: u32| order.partition_point(|&b| bfirst[b] <= p) - 1;
    // Per block: (pos, order, kind, slot); order 0 = before the inst at
    // pos, 1 = at it, 2 = after it; kind 0 = end kill, 1 = load-like
    // read, 2 = write, 3 = start kill, 4 = read by a call (a call both
    // reading and writing reads first).
    let mut ev: Vec<Vec<(u32, u8, u8, usize)>> = vec![Vec::new(); nb];
    let mut used: Vec<FxHashSet<usize>> = vec![FxHashSet::default(); nb];
    for s in 0..nslots {
        for &p in &uses[s] {
            used[block_of(p)].insert(s);
        }
        for (list, kind) in [(&reads[s], 1u8), (&writes[s], 2u8)] {
            for &i in list.iter() {
                if let Some(b) = func.layout.inst_block(i)
                    && bidx[b] != usize::MAX
                {
                    let kind = if kind == 1 && func.dfg.insts[i].opcode().is_call() { 4 } else { kind };
                    ev[bidx[b]].push((ipos[i], 1, kind, s));
                }
            }
        }
    }
    for &(s, b, p, exact) in kills {
        let k = bidx[b];
        if s.index() < nslots && k != usize::MAX && (exact || !used[k].contains(&s.index())) {
            ev[k].push((p, 2, 0, s.index()));
        }
    }
    for &(s, b, p, o) in starts {
        let k = bidx[b];
        if s.index() < nslots && k != usize::MAX {
            ev[k].push((p, o, 3, s.index()));
        }
    }
    let mut by_term: FxHashMap<Inst, Vec<(u32, usize)>> = FxHashMap::default();
    for &(s, n, kk) in edge_kills {
        let Some(k) = func.layout.inst_block(n).map(|b| bidx[b]).filter(|&k| k != usize::MAX) else { continue };
        if kk == KILL_ALL || s.index() >= nslots {
            continue; // nothing in this frame runs after `_Unwind_Resume`
        } else if kk == BEFORE_INST {
            ev[k].push((ipos[n], 0, 0, s.index()));
        } else if kk == AFTER_INST {
            ev[k].push((ipos[n], 2, 0, s.index()));
        } else {
            by_term.entry(n).or_default().push((kk, s.index()));
        }
    }
    for e in &mut ev {
        e.sort_unstable();
    }
    let w = nslots.div_ceil(64).max(1);
    let succs = succ_masks(func, order, &bidx, &by_term, w);
    // State per slot: dead after an end kill (first w words) or after a
    // start kill (next w). Joins OR both.
    let mut dead_in = vec![0u64; nb * 2 * w];
    let run = |k: usize, st: &mut [u64], stale: &mut Option<&mut Vec<bool>>| {
        for &(_, _, kind, s) in &ev[k] {
            let (j, bit) = (s / 64, 1u64 << (s % 64));
            let (de, ds) = (st[j] & bit != 0, st[w + j] & bit != 0);
            let bad = match kind {
                0 => {
                    st[j] |= bit;
                    st[w + j] &= !bit;
                    false
                }
                3 => {
                    st[w + j] |= bit;
                    st[j] &= !bit;
                    false
                }
                1 => de || ds,
                4 => de,
                _ => false,
            };
            if matches!(kind, 2 | 4) {
                st[j] &= !bit;
                st[w + j] &= !bit;
            }
            if bad && let Some(v) = stale {
                v[s] = true;
            }
        }
    };
    loop {
        let mut changed = false;
        for k in 0..nb {
            let mut st = dead_in[k * 2 * w..][..2 * w].to_vec();
            run(k, &mut st, &mut None);
            for (qk, m) in &succs[k] {
                for j in 0..2 * w {
                    // An edge kill is an end kill.
                    let v = st[j] | if j < w { m[j] } else { 0 };
                    if v & !dead_in[qk * 2 * w + j] != 0 {
                        dead_in[qk * 2 * w + j] |= v;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut stale = vec![false; nslots];
    for k in 0..nb {
        let mut st = dead_in[k * 2 * w..][..2 * w].to_vec();
        run(k, &mut st, &mut Some(&mut stale));
    }
    stale
}

/// `v`'s constant offset into slot `s` when it is always `stack_addr
/// s+off`: directly, `iadd` of a constant, or a block param every
/// incoming edge passes the same such address in (rustc's lowering hands
/// a `match` arm's temporaries to a shared tail that way).
fn slot_offset(
    func: &Function,
    cfg: &ControlFlowGraph,
    memo: &mut FxHashMap<(Value, StackSlot), Option<i64>>,
    v: Value,
    s: StackSlot,
) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    if let Some(&r) = memo.get(&(v, s)) {
        return r;
    }
    // Pending: a cycle through block params contributes nothing new.
    memo.insert((v, s), None);
    let r = match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::StackAddr { stack_slot, offset, .. } if stack_slot == s => Some(i64::from(offset)),
            InstructionData::Binary { opcode: Opcode::Iadd, args } => {
                let k = |x: Value| match func.dfg.value_def(func.dfg.resolve_aliases(x)) {
                    ValueDef::Result(c, _) => match func.dfg.insts[c] {
                        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
                        _ => None,
                    },
                    _ => None,
                };
                match (k(args[0]), k(args[1])) {
                    (None, Some(c)) => slot_offset(func, cfg, memo, args[0], s).and_then(|o| o.checked_add(c)),
                    (Some(c), None) => slot_offset(func, cfg, memo, args[1], s).and_then(|o| o.checked_add(c)),
                    _ => None,
                }
            }
            _ => None,
        },
        ValueDef::Param(b, idx) => {
            let mut seen: Option<i64> = None;
            let mut ok = cfg.pred_iter(b).next().is_some();
            'preds: for pr in cfg.pred_iter(b) {
                for bc in func.dfg.insts[pr.inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                    if bc.block(&func.dfg.value_lists) != b {
                        continue;
                    }
                    let a = match bc.args(&func.dfg.value_lists).nth(idx) {
                        Some(BlockArg::Value(a)) => slot_offset(func, cfg, memo, a, s),
                        _ => None,
                    };
                    match (a, seen) {
                        (Some(o), None) => seen = Some(o),
                        (Some(o), Some(p)) if o == p => {}
                        _ => {
                            ok = false;
                            break 'preds;
                        }
                    }
                }
            }
            if ok { seen } else { None }
        }
        _ => None,
    };
    memo.insert((v, s), r);
    r
}

/// [`add_seg`] over half-open byte ranges.
fn add_seg_i64(v: &mut Vec<(i64, i64)>, (lo, hi): (i64, i64)) {
    if lo >= hi {
        return;
    }
    v.push((lo, hi));
    v.sort_unstable();
    let mut m: Vec<(i64, i64)> = Vec::with_capacity(v.len());
    for &(a, b) in v.iter() {
        match m.last_mut() {
            Some(l) if a <= l.1 => l.1 = l.1.max(b),
            _ => m.push((a, b)),
        }
    }
    *v = m;
}

/// Union `(lo, hi)` into the sorted, disjoint segment list `v`, merging
/// overlapping and adjacent segments.
fn add_seg(v: &mut Vec<(u32, u32)>, (lo, hi): (u32, u32)) {
    let a = v.partition_point(|&(_, h)| h.saturating_add(1) < lo);
    let mut b = a;
    let (mut lo2, mut hi2) = (lo, hi);
    while b < v.len() && v[b].0 <= hi.saturating_add(1) {
        lo2 = lo2.min(v[b].0);
        hi2 = hi2.max(v[b].1);
        b += 1;
    }
    v.splice(a..b, [(lo2, hi2)]);
}

/// Whether two sorted, disjoint segment lists share a position.
fn segs_overlap(a: &[(u32, u32)], b: &[(u32, u32)]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].1 < b[j].0 {
            i += 1;
        } else if b[j].1 < a[i].0 {
            j += 1;
        } else {
            return true;
        }
    }
    false
}

thread_local! {
    /// The symbol of the function the pipeline is running slotmerge on
    /// (`PLIRON_SM_INFO_FN` filtering; CLIF names are only `u0:N`).
    pub static CUR_FN: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// `PLIRON_SM_INFO` reporting, narrowed by `PLIRON_SM_INFO_FN=<substr>` to
/// matching symbols — parallel CGU threads interleave the lines otherwise.
fn info_on() -> bool {
    crate::env_set("PLIRON_SM_INFO")
        && crate::env_get("PLIRON_SM_INFO_FN").is_none_or(|f| CUR_FN.with(|c| c.borrow().contains(f)))
}

/// `PLIRON_NOCAP_CALLEE=1`: `nocap` is keyed by the call insts lowering
/// made, and passes that duplicate or rebuild a call (jumpthread, taildup,
/// unroll, ...) make new insts without the fact — 3 of `lower_item_kind`'s
/// 7 calls passing a 352-byte by-value arg indirectly survived only as
/// copies, each then an escape. Every source of the fact is a property of
/// the direct callee (its ABI's `captures(none)`/`noalias`, our
/// `nocapture` inference, drop glue), so carry it to every current direct
/// call of that callee at the same arg index. A removed inst keeps its
/// data, so the original's callee is still readable.
fn nocap_by_callee(func: &Function, nocap: &FxHashSet<(Inst, u32)>) -> Option<FxHashSet<(Inst, u32)>> {
    if nocap.is_empty() || !pliron_cfg::env_is("PLIRON_NOCAP_CALLEE", "1") {
        return None;
    }
    let callee = |i: Inst| match func.dfg.insts[i].analyze_call(&func.dfg.value_lists, &func.dfg.exception_tables) {
        CallInfo::Direct(fr, _) | CallInfo::DirectWithSig(fr, _, _) => Some(fr),
        _ => None,
    };
    let facts: FxHashSet<(FuncRef, u32)> =
        nocap.iter().filter_map(|&(i, k)| Some((callee(i)?, k))).collect();
    let mut out = nocap.clone();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if let Some(fr) = callee(i) {
                for k in 0..func.dfg.inst_args(i).len() as u32 {
                    if facts.contains(&(fr, k)) {
                        out.insert((i, k));
                    }
                }
            }
        }
    }
    Some(out)
}

pub fn run(
    func: &mut Function,
    nowrite: &FxHashMap<FuncRef, bool>,
    lifetimes: &[(Vec<StackSlot>, Option<Inst>, Block, bool)],
    nocap: &FxHashSet<(Inst, u32)>,
) -> usize {
    run_with(func, nowrite, lifetimes, nocap, &[])
}

/// `run` plus `entry_kills`' edge kills (`PLIRON_SM_MARKFLOW`).
pub fn run_with(
    func: &mut Function,
    nowrite: &FxHashMap<FuncRef, bool>,
    lifetimes: &[(Vec<StackSlot>, Option<Inst>, Block, bool)],
    nocap: &FxHashSet<(Inst, u32)>,
    edge_kills: &[(StackSlot, Inst, u32)],
) -> usize {
    let wide = nocap_by_callee(func, nocap);
    let nocap = wide.as_ref().unwrap_or(nocap);
    // A resume reached through a `try_call` (a callee's, inlined at a call
    // that had a landing pad) unwinds into this frame's handler, not out
    // of the frame: its `KILL_ALL` would end lives the handler still
    // reads. `PLIRON_SM_RESUMETRY=0` honors it anyway.
    let ek_resume: Vec<(StackSlot, Inst, u32)>;
    let edge_kills = if crate::pass_enabled("PLIRON_SM_RESUMETRY")
        && edge_kills.iter().any(|&(_, i, k)| k == KILL_ALL && matches!(func.dfg.insts[i].opcode(), Opcode::TryCall | Opcode::TryCallIndirect))
    {
        ek_resume = edge_kills
            .iter()
            .copied()
            .filter(|&(_, i, k)| k != KILL_ALL || !matches!(func.dfg.insts[i].opcode(), Opcode::TryCall | Opcode::TryCallIndirect))
            .collect();
        &ek_resume[..]
    } else {
        edge_kills
    };
    if info_on() {
        eprintln!(
            "sm-info: slotmerge {} nslots={}",
            func.name,
            func.sized_stack_slots.len()
        );
    }
    if func.sized_stack_slots.len() < 2 {
        return 0;
    }

    // Intervals are measured in instruction positions over a CFG
    // reverse-postorder walk, not the final layout: cold blocks (shared
    // landing pads) sink to the tail in layout, which would stretch
    // every slot unwinding into them to function end — the same window
    // LLVM's stack coloring measures before machine sinking. The
    // exception edges of a `try_call` are CFG successors here, so a pad
    // measures next to the calls that can unwind to it. `bpos`/`bfirst`/
    // `blast` give each block's position range; `order` covers every
    // block (unreachable ones append at the end).
    let mut succ_map: SecondaryMap<Block, Vec<Block>> = SecondaryMap::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                succ_map[b].push(bc.block(&func.dfg.value_lists));
            }
        }
    }
    let mut order: Vec<Block> = Vec::with_capacity(func.layout.blocks().count());
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    if let Some(entry) = func.layout.entry_block() {
        seen.insert(entry);
        let mut stack = vec![(entry, 0usize)];
        while let Some(&(b, ci)) = stack.last() {
            if ci < succ_map[b].len() {
                stack.last_mut().unwrap().1 += 1;
                let s = succ_map[b][ci];
                if seen.insert(s) {
                    stack.push((s, 0));
                }
            } else {
                order.push(b);
                stack.pop();
            }
        }
    }
    order.reverse();
    for b in func.layout.blocks() {
        if seen.insert(b) {
            order.push(b);
        }
    }
    let mut bpos = SecondaryMap::<Block, u32>::new();
    let mut bfirst = SecondaryMap::<Block, u32>::new();
    let mut blast = SecondaryMap::<Block, u32>::new();
    let mut ipos = SecondaryMap::<Inst, u32>::new();
    let mut next = 0u32;
    for (i, &b) in order.iter().enumerate() {
        bpos[b] = i as u32;
        bfirst[b] = next;
        for i2 in func.layout.block_insts(b) {
            ipos[i2] = next;
            next += 1;
        }
        blast[b] = next.saturating_sub(1).max(bfirst[b]);
    }
    let last = next.saturating_sub(1);

    // insts reading each value — the propagation worklist's reverse edges.
    let mut users: SecondaryMap<Value, Vec<Inst>> = SecondaryMap::new();
    for &b in &order {
        for i in func.layout.block_insts(b) {
            for &a in func.dfg.inst_args(i) {
                users[func.dfg.resolve_aliases(a)].push(i);
            }
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for ba in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(v) = ba {
                        users[func.dfg.resolve_aliases(v)].push(i);
                    }
                }
            }
        }
    }

    // `derived[v]` = slots whose address `v` may hold, and
    // `from_bytes[v]` = slots whose byte image `v` may carry (a vector
    // load of an aggregate keeps the whole image — `extractlane`d lanes
    // can reconstitute stored pointers). Seeds: `stack_addr` results
    // (derived), and every load's results (from_bytes, keyed by the
    // address's slots). Both propagate over args like `derived` above;
    // a `try_call`'s payloads conservatively inherit call-arg unions.
    let mut derived: SecondaryMap<Value, Vec<StackSlot>> = SecondaryMap::new();
    let mut wl: VecDeque<Inst> = VecDeque::new();
    let mut inq: FxHashSet<Inst> = FxHashSet::default();
    for &b in &order {
        for i in func.layout.block_insts(b) {
            if let InstructionData::StackAddr { stack_slot, .. } = func.dfg.insts[i]
                && let Some(&r) = func.dfg.inst_results(i).first()
            {
                let r = func.dfg.resolve_aliases(r);
                if !derived[r].contains(&stack_slot) {
                    derived[r].push(stack_slot);
                }
            }
            inq.insert(i);
            wl.push_back(i);
        }
    }
    propagate(func, &users, &mut derived, wl, inq, None, true);

    // `from_bytes` seeds: every load's results carry the byte image of
    // the slot its address derives from — an aggregate copy moves
    // contained pointers with it, so a store of the loaded bytes into
    // another slot must flow those pointers dst-ward.
    let mut from_bytes: SecondaryMap<Value, Vec<StackSlot>> = SecondaryMap::new();
    let mut wl: VecDeque<Inst> = VecDeque::new();
    let mut inq: FxHashSet<Inst> = FxHashSet::default();
    for &b in &order {
        for i in func.layout.block_insts(b) {
            if func.dfg.insts[i].opcode().can_load() {
                if let Some(&a) = func.dfg.inst_args(i).first() {
                    for &s in &derived[func.dfg.resolve_aliases(a)] {
                        for &r in func.dfg.inst_results(i) {
                            let r = func.dfg.resolve_aliases(r);
                            if !from_bytes[r].contains(&s) {
                                from_bytes[r].push(s);
                            }
                        }
                    }
                }
            }
            inq.insert(i);
            wl.push_back(i);
        }
    }
    propagate(func, &users, &mut from_bytes, wl, inq, None, false);

    // Per-slot use positions and live interval [lo, hi] (`u32::MAX` hi is
    // normalized to `last` afterwards).
    let nslots = func.sized_stack_slots.len();
    let mut iv: Vec<(u32, u32)> = vec![(u32::MAX, 0); nslots];
    let mut uses: Vec<Vec<u32>> = vec![Vec::new(); nslots];
    // Per-slot byte accesses. `reads` can observe stored bytes; `writes`
    // definitely store them. A backward-edge span must stretch the
    // interval of a slot whose bytes can be live across trips — reads in
    // the span not dominated by an in-span write — but a slot every
    // in-span read of which is dominated by an in-span write is
    // re-initialized each trip, so its plain use window already suffices.
    let mut reads: Vec<Vec<Inst>> = vec![Vec::new(); nslots];
    let mut writes: Vec<Vec<Inst>> = vec![Vec::new(); nslots];
    let dbg = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::slotmerge", tracing::Level::DEBUG);
    let mut esc: Vec<(StackSlot, Inst)> = Vec::new();
    // `llvm.lifetime.{start,end}` markers (MIR StorageLive/Dead) pin
    // validity windows in LLVM's model: a pointer into dead storage can't
    // be validly dereferenced, so escapes couldn't extend a marked slot's
    // live range past StorageDead. In practice rustc's MIR doesn't uphold
    // that strongly enough — stage2 `lower_to_hir` read a corrupted
    // DefIndex when escapes were exempted — so markers only contribute
    // use positions. `PLIRON_SLOTMERGE_MARK` re-enables the exemption for
    // experiments.
    //
    // Positions resolve eagerly because anchors recorded at lowering decay
    // mid-pipeline: a pass can delete the anchor inst or fold its whole
    // block. A slot is only `marked` when *every* singleton marker on it
    // resolves — one lost end marker leaves the window unbounded, which
    // is worse than no marker at all (the escape exemption would clip a
    // live range the source semantics extended to function end).
    let in_layout: FxHashSet<Block> = order.iter().copied().collect();
    let mut marked = vec![false; nslots];
    let mut mark_lost = vec![false; nslots];
    let mut marker_pos: Vec<Option<u32>> = Vec::new();
    // `PLIRON_SM_MARKFLOW=1` (with MARK): only end markers count, as kills
    // — (slot, block, position, exact anchor) — and a forward dataflow
    // from every use decides where an escaped slot is live (`mark_flow`).
    let markflow = crate::env_is("PLIRON_SLOTMERGE_MARK", "1") && pliron_cfg::env_is("PLIRON_SM_MARKFLOW", "1");
    let mut kills: Vec<(StackSlot, Block, u32, bool)> = Vec::new();
    if crate::env_is("PLIRON_SLOTMERGE_MARK", "1") {
        let (mut single, mut multi, mut none, mut lost) = (0u32, 0u32, 0u32, 0u32);
        let mut lost_start = 0u32;
        marker_pos.reserve(lifetimes.len());
        for (ds, anchor, b, start) in lifetimes {
            // The marker sits right after the anchor instruction: a start
            // marker is live from the next position, an end marker's last
            // live point is the anchor itself. No usable anchor degrades
            // to the block's start, and a block that left the layout
            // resolves to nothing.
            let pos = match anchor {
                &Some(i) if func.layout.inst_block(i).is_some() => {
                    if *start {
                        (ipos[i] + 1).min(last)
                    } else {
                        ipos[i]
                    }
                }
                _ if in_layout.contains(b) => bfirst[*b],
                _ => u32::MAX,
            };
            marker_pos.push((pos != u32::MAX).then_some(pos));
            // A marker naming exactly one slot is that storage's window;
            // a unioned (phi-derived) operand can't be trusted as a
            // per-slot bound, so ambiguous markers mark nothing.
            match ds.as_slice() {
                &[s] if s.index() < nslots => {
                    if pos == u32::MAX {
                        mark_lost[s.index()] = true;
                        lost += 1;
                        lost_start += u32::from(*start);
                    } else {
                        marked[s.index()] = true;
                        single += 1;
                    }
                }
                [] => none += 1,
                _ => multi += 1,
            }
        }
        if markflow {
            // Flow mode: a slot is tracked when any end marker naming it
            // alone still has its block. Lost markers only drop kills.
            marked.fill(false);
            for ((ds, anchor, b, start), pos) in lifetimes.iter().zip(&marker_pos) {
                if let (&[s], false, Some(_)) = (ds.as_slice(), *start, pos)
                    && s.index() < nslots
                {
                    marked[s.index()] = true;
                    let exact = anchor.is_some_and(|i| func.layout.inst_block(i) == Some(*b));
                    kills.push((s, *b, if exact { ipos[anchor.unwrap()] } else { blast[*b] }, exact));
                }
            }
            for &(s, term, _) in edge_kills {
                if s.index() < nslots && func.layout.inst_block(term).is_some() {
                    marked[s.index()] = true;
                }
            }
            // `PLIRON_SM_STARTONLY=1`: a slot with a start marker and no
            // end marker (MIR's StorageLive of a temporary passed to a
            // diverging call — `format_args!` pieces for `bug!`, 110 of
            // them in stage-2 `walk_expr::<TailCallCkVisitor>`) is in
            // scope from that start to the frame's exit, and in use only
            // from its first access: `mark_flow`'s live set with no kills
            // but the frame-leaving ones. LLVM's StackColoring treats it
            // the same way; a slot with no marker at all stays in scope
            // throughout and is never merged.
            if crate::env_is("PLIRON_SM_STARTONLY", "1") {
                for ((ds, _, _, start), pos) in lifetimes.iter().zip(&marker_pos) {
                    if let (&[s], true, Some(_)) = (ds.as_slice(), *start, pos)
                        && s.index() < nslots
                    {
                        marked[s.index()] = true;
                    }
                }
            }
            // `PLIRON_SM_ESCFLOW=1`: every slot, markers or not. Passes
            // lose markers wholesale (jumpthread dropped 895 of stage-2
            // `lower_item_kind`'s 2,324, leaving six 328-byte slots with
            // neither start nor end), and an escaped slot is still only
            // in use from its first access: `mark_flow` keeps it live from
            // there to the frame's exits. Under `PLIRON_SM_STARTS` two such
            // slots share storage only if no path touches both, so no
            // run can see both addresses. Slots a tail call or stack
            // switch receives stay escaped (`hard_esc` below).
            if crate::env_is("PLIRON_SM_ESCFLOW", "1") {
                marked.fill(true);
            }
        } else {
            for s in 0..nslots {
                marked[s] &= !mark_lost[s];
            }
        }
        if info_on() {
            eprintln!(
                "sm-info: markers: {single} singleton {multi} union {none} empty {lost} lost ({lost_start} start)"
            );
        }
    }
    // An escaped slot can never fold: its address reached an opaque
    // sink, so sharing storage with anything could flip a retained
    // pointer's identity — interval disjointness can't rule that out.
    // `esc_seen` records escapes on marked slots too: markers widen a
    // marked-escaped slot's interval to its declared window below.
    let mut escaped = vec![false; nslots];
    // Slots whose address outlives the frame (tail calls, stack
    // switches): escaped whatever their markers say.
    let mut hard_esc: Vec<StackSlot> = Vec::new();
    let mut esc_seen = vec![false; nslots];
    macro_rules! escape {
        ($s:expr, $i:expr) => {{
            esc_seen[$s.index()] = true;
            if !marked[$s.index()] {
                iv[$s.index()].1 = u32::MAX;
                escaped[$s.index()] = true;
            }
            if dbg || info_on() {
                esc.push(($s, $i));
            }
        }};
    }
    // Escaping a pointer to `s` exposes `s`'s bytes — including pointers
    // stored inside it — to whoever holds the escapee. Escape `s` and
    // the whole contains-reach. (`contains` no longer self-seeds, so the
    // subject is escaped explicitly here.)
    macro_rules! escape_deep {
        ($c:expr, $s:expr, $i:expr) => {{
            escape!($s, $i);
            for s2 in $c[$s.index()].clone() {
                escape!(s2, $i);
            }
        }};
    }
    // (stored, container): `&stored` was written into `container`'s bytes.
    // A reload through the container can expose the pointer at any point
    // in the container's live range, so the stored slot's interval must
    // contain the container's — still bounded when the container is.
    let mut edges: FxHashSet<(StackSlot, StackSlot)> = FxHashSet::default();
    // (src, dst): src's byte image was copied into dst — a `memcpy`/
    // `memmove` libcall or an aggregate load/store move — so pointers
    // contained in src are now contained in dst as well.
    let mut cpy: FxHashSet<(StackSlot, StackSlot)> = FxHashSet::default();
    // contains[d] = slots a pointer reloaded out of d's bytes may name:
    // every slot whose pointer was stored into d (d itself only via an
    // actual self-store), and the closure through byte-copies.
    let mut contains: Vec<FxHashSet<StackSlot>> =
        (0..nslots).map(|_| FxHashSet::default()).collect();

    // The prepass edges are built from `derived`, and `derived` gains
    // entries once load results resolve through `contains` — a reloaded
    // pointer's pointee is only known after the pointer was stored —
    // so iterate the three to a joint fixpoint. All inserts are
    // monotone; a pathological function that fails to converge still
    // exits soundly (unmerged is always safe).
    for round in 0..8 {
        let mut grew = 0usize;
        for &b in &order {
            for i in func.layout.block_insts(b) {
                let d = &func.dfg.insts[i];
                let op = d.opcode();
                let args = func.dfg.inst_args(i).to_vec();
                let aslot = |k: usize| -> &[StackSlot] {
                    args.get(k)
                        .map(|&a| derived[func.dfg.resolve_aliases(a)].as_slice())
                        .unwrap_or(&[])
                };
                if op.is_call() {
                    let info = d.analyze_call(&func.dfg.value_lists, &func.dfg.exception_tables);
                    if let CallInfo::Direct(fr, cargs) | CallInfo::DirectWithSig(fr, _, cargs) =
                        info
                    {
                        use cranelift_codegen::ir::{ExternalName, LibCall};
                        if cargs.len() >= 2
                            && matches!(
                                func.dfg.ext_funcs[fr].name,
                                ExternalName::LibCall(LibCall::Memcpy)
                                    | ExternalName::LibCall(LibCall::Memmove)
                            )
                        {
                            for &x in &derived[func.dfg.resolve_aliases(cargs[1])] {
                                for &d2 in &derived[func.dfg.resolve_aliases(cargs[0])] {
                                    if x != d2 {
                                        grew += cpy.insert((x, d2)) as usize;
                                    }
                                }
                            }
                        }
                    }
                } else if op.can_store() {
                    match op {
                        Opcode::Store
                        | Opcode::Istore8
                        | Opcode::Istore16
                        | Opcode::Istore32
                        | Opcode::AtomicStore => {
                            let dst: Vec<StackSlot> = aslot(1).to_vec();
                            for &s in aslot(0) {
                                for &d2 in &dst {
                                    // A self-store (s == d2) is real: the
                                    // slot's bytes then name the slot.
                                    grew += edges.insert((s, d2)) as usize;
                                }
                            }
                            for &s in &from_bytes[func.dfg.resolve_aliases(args[0])] {
                                for &d2 in &dst {
                                    if s != d2 {
                                        grew += cpy.insert((s, d2)) as usize;
                                    }
                                }
                            }
                        }
                        Opcode::AtomicRmw | Opcode::AtomicCas => {
                            let dst: Vec<StackSlot> = aslot(0).to_vec();
                            for k in 1..args.len() {
                                for &s in aslot(k) {
                                    for &d2 in &dst {
                                        grew += edges.insert((s, d2)) as usize;
                                    }
                                }
                                for &s in &from_bytes[func.dfg.resolve_aliases(args[k])] {
                                    for &d2 in &dst {
                                        if s != d2 {
                                            grew += cpy.insert((s, d2)) as usize;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // `contains` fixpoint: direct stored pointers plus the
        // transitive closure over byte-copies.
        loop {
            let mut changed = false;
            for &(s, d) in &edges {
                if contains[d.index()].insert(s) {
                    changed = true;
                }
            }
            for &(x, d) in &cpy {
                let add: Vec<StackSlot> = contains[x.index()].iter().copied().collect();
                for s in add {
                    if contains[d.index()].insert(s) {
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        // A load result may hold the address of any slot the source's
        // bytes contain — the reconstituted pointer attributes its
        // uses to the pointee, not the container.
        let mut wl2: VecDeque<Inst> = VecDeque::new();
        let mut inq2: FxHashSet<Inst> = FxHashSet::default();
        for &b in &order {
            for i in func.layout.block_insts(b) {
                if !func.dfg.insts[i].opcode().can_load() {
                    continue;
                }
                let Some(&a) = func.dfg.inst_args(i).first() else {
                    continue;
                };
                // The loaded bytes name the slots stored into the
                // pointee — not the pointee itself, unless it was
                // actually self-stored (`contains` doesn't self-seed).
                let mut acc: Vec<StackSlot> = Vec::new();
                for &s0 in &derived[func.dfg.resolve_aliases(a)] {
                    for &s in &contains[s0.index()] {
                        if !acc.contains(&s) {
                            acc.push(s);
                        }
                    }
                }
                for &r in func.dfg.inst_results(i) {
                    let r = func.dfg.resolve_aliases(r);
                    let mut grew2 = false;
                    for &s in &acc {
                        if !derived[r].contains(&s) {
                            derived[r].push(s);
                            grew2 = true;
                            grew += 1;
                        }
                    }
                    if grew2 {
                        for &u in &users[r] {
                            if inq2.insert(u) {
                                wl2.push_back(u);
                            }
                        }
                    }
                }
            }
        }
        propagate(func, &users, &mut derived, wl2, inq2, Some(&contains), true);
        if grew == 0 {
            break;
        }
        if round == 7 {
            return 0;
        }
    }
    let use_at = |iv: &mut Vec<(u32, u32)>, uses: &mut Vec<Vec<u32>>, s: StackSlot, p: u32| {
        uses[s.index()].push(p);
        let e = &mut iv[s.index()];
        e.0 = e.0.min(p);
        e.1 = e.1.max(p);
    };
    {
        for &b in &order {
            for i in func.layout.block_insts(b) {
                let pos = ipos[i];
                let d = &func.dfg.insts[i];
                let op = d.opcode();
                // `stack_addr` itself is not a use: taking the address
                // doesn't touch the slot's bytes, and treating it as one
                // would extend every interval to the (hoisted) addr site.
                if let &InstructionData::StackAddr { .. } = d {
                    continue;
                }
                if op.is_branch() && !op.is_call() {
                    continue; // control flow only; args handled by params
                }
                let args = func.dfg.inst_args(i).to_vec();
                let arg_slots = |k: usize| -> &[StackSlot] {
                    args.get(k)
                        .map(|&a| derived[func.dfg.resolve_aliases(a)].as_slice())
                        .unwrap_or(&[])
                };
                if op.is_call() {
                    match op {
                        // A tail-called callee can outlive this frame; a
                        // stack switch may touch it from another stack.
                        Opcode::ReturnCall | Opcode::ReturnCallIndirect | Opcode::StackSwitch => {
                            for k in 0..args.len() {
                                for &s in arg_slots(k) {
                                    use_at(&mut iv, &mut uses, s, pos);
                                    reads[s.index()].push(i);
                                    escape_deep!(contains, s, i);
                                    hard_esc.push(s);
                                    hard_esc.extend(contains[s.index()].iter().copied());
                                }
                            }
                        }
                        _ => {
                            let info =
                                d.analyze_call(&func.dfg.value_lists, &func.dfg.exception_tables);
                            let plain = matches!(info, CallInfo::Direct(..));
                            match info {
                                CallInfo::Direct(fr, cargs)
                                | CallInfo::DirectWithSig(fr, _, cargs) => {
                                    let sret = func.dfg.signatures
                                        [func.dfg.ext_funcs[fr].signature]
                                        .params
                                        .first()
                                        .is_some_and(|p| {
                                            p.purpose == ArgumentPurpose::StructReturn
                                        });
                                    // nowrite `true` never writes; `false`
                                    // may write on panic paths. A plain call
                                    // unwinds us either way (our frame dies
                                    // with it), so both count; a try_call
                                    // lands in our own pads, which could
                                    // observe a pointer the callee stashed
                                    // while panicking — strict only.
                                    let nw = if plain {
                                        nowrite.contains_key(&fr)
                                    } else {
                                        nowrite.get(&fr) == Some(&true)
                                    };
                                    // libc mem* copies through the pointers
                                    // but never retains them (the copy
                                    // edges are collected in the prepass).
                                    use cranelift_codegen::ir::{ExternalName, LibCall};
                                    let copier = matches!(
                                        func.dfg.ext_funcs[fr].name,
                                        ExternalName::LibCall(LibCall::Memcpy)
                                            | ExternalName::LibCall(LibCall::Memmove)
                                    );
                                    let nokeep = copier
                                        || matches!(
                                            func.dfg.ext_funcs[fr].name,
                                            ExternalName::LibCall(LibCall::Memset)
                                        );
                                    for (k, &a) in cargs.iter().enumerate() {
                                        let ds = derived[func.dfg.resolve_aliases(a)].clone();
                                        for &s in &ds {
                                            use_at(&mut iv, &mut uses, s, pos);
                                        }
                                        // A mem* dst arg definitely writes
                                        // its (single) pointee; other args
                                        // may observe stale bytes.
                                        if nokeep && k == 0 {
                                            if let &[s] = ds.as_slice() {
                                                writes[s.index()].push(i);
                                            }
                                        } else {
                                            for &s in &ds {
                                                reads[s.index()].push(i);
                                            }
                                        }
                                        // A memcpy/memmove into untracked
                                        // memory carries the source's byte
                                        // image — and every pointer inside
                                        // it — somewhere we can't follow.
                                        // The copy edges only form for
                                        // slot dsts, so the contents escape
                                        // here (the container's own address
                                        // stays put, so it does not).
                                        if copier && k == 1 && arg_slots(0).is_empty() {
                                            for &s in &ds {
                                                for s2 in contains[s.index()].clone() {
                                                    escape!(s2, i);
                                                }
                                            }
                                        }
                                        for &s in &ds {
                                            // rustc's captures(none): the
                                            // callee can't retain the
                                            // pointer past the call.
                                            if !nw
                                                && !nokeep
                                                && !(sret && k == 0)
                                                && !nocap.contains(&(i, k as u32))
                                            {
                                                escape_deep!(contains, s, i);
                                            } else if !nokeep && !(sret && k == 0) {
                                                // ...but only that pointer:
                                                // one the callee loads *out
                                                // of* the pointee is fair
                                                // game — it can hand it back
                                                // (cranelift's
                                                // `compute_use_states`
                                                // closure returns an iterator
                                                // built from `*env`), so the
                                                // contained slots escape.
                                                for s2 in contains[s.index()].clone() {
                                                    escape!(s2, i);
                                                }
                                            }
                                        }
                                    }
                                }
                                CallInfo::Indirect(_, cargs) => {
                                    for (k, &a) in cargs.iter().enumerate() {
                                        for &s in &derived[func.dfg.resolve_aliases(a)] {
                                            use_at(&mut iv, &mut uses, s, pos);
                                            reads[s.index()].push(i);
                                            if !nocap.contains(&(i, k as u32)) {
                                                escape_deep!(contains, s, i);
                                            } else {
                                                // As for direct calls: the
                                                // pointee's pointers escape.
                                                for s2 in contains[s.index()].clone() {
                                                    escape!(s2, i);
                                                }
                                            }
                                        }
                                    }
                                }
                                CallInfo::NotACall => {}
                            }
                        }
                    }
                } else if op.is_return() {
                    // A returned pointer outlives the frame either way:
                    // after `return` nothing can access the slot again on
                    // this path, so it adds no live-range constraint.
                } else if op.can_store() {
                    match op {
                        // store v, p / atomic_store v, p — a stored
                        // pointer escapes; the address is the use. When
                        // `p` is itself slot-derived the pointer stays
                        // inside our frame: a container edge bounds the
                        // escape to the container's live range instead.
                        Opcode::Store
                        | Opcode::Istore8
                        | Opcode::Istore16
                        | Opcode::Istore32
                        | Opcode::AtomicStore => {
                            let dst: Vec<StackSlot> = arg_slots(1).to_vec();
                            for &s in arg_slots(0) {
                                use_at(&mut iv, &mut uses, s, pos);
                                if dst.is_empty() {
                                    escape_deep!(contains, s, i);
                                }
                            }
                            if dst.is_empty() {
                                // A stored byte image leaks every pointer
                                // it carried, not the container's own
                                // address — that is in the image only if
                                // the slot holds a pointer to itself, and
                                // then `contains[s]` names it (self-stores
                                // are recorded). Escaping `s` too pinned
                                // every `x = f()` result slot whose bytes
                                // were later copied to the heap: stage-2
                                // rustc's AST walkers kept 76 112-byte
                                // slots apart (9 KB frames vs LLVM's
                                // 384 B), overflowing the stack.
                                for &s in &from_bytes[func.dfg.resolve_aliases(args[0])] {
                                    if !precise() {
                                        escape!(s, i);
                                    }
                                    for s2 in contains[s.index()].clone() {
                                        escape!(s2, i);
                                    }
                                }
                            }
                            // `store` through a pointer that can only
                            // name `s` is a definite write of `s`'s
                            // bytes; a maybe-pointer can't reinit.
                            if let &[s] = arg_slots(1) {
                                writes[s.index()].push(i);
                            }
                            for &s in arg_slots(1) {
                                use_at(&mut iv, &mut uses, s, pos);
                            }
                        }
                        // atomic_rmw p, v / atomic_cas p, e, n
                        Opcode::AtomicRmw | Opcode::AtomicCas => {
                            let dst: Vec<StackSlot> = arg_slots(0).to_vec();
                            for &s in &dst {
                                use_at(&mut iv, &mut uses, s, pos);
                                reads[s.index()].push(i);
                            }
                            for k in 1..args.len() {
                                for &s in arg_slots(k) {
                                    use_at(&mut iv, &mut uses, s, pos);
                                    if dst.is_empty() {
                                        escape_deep!(contains, s, i);
                                    }
                                }
                                if dst.is_empty() {
                                    // As for `store`: the image's pointers
                                    // escape, not the container.
                                    for &s in &from_bytes[func.dfg.resolve_aliases(args[k])] {
                                        if !precise() {
                                            escape!(s, i);
                                        }
                                        for s2 in contains[s.index()].clone() {
                                            escape!(s2, i);
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            for k in 0..args.len() {
                                for &s in arg_slots(k) {
                                    use_at(&mut iv, &mut uses, s, pos);
                                    reads[s.index()].push(i);
                                    escape!(s, i);
                                }
                            }
                        }
                    }
                } else if op.can_load() {
                    for &s in arg_slots(0) {
                        use_at(&mut iv, &mut uses, s, pos);
                        reads[s.index()].push(i);
                    }
                    for k in 1..args.len() {
                        for &s in arg_slots(k) {
                            use_at(&mut iv, &mut uses, s, pos);
                            escape_deep!(contains, s, i);
                        }
                    }
                } else if op.other_side_effects() || op.is_terminator() {
                    for k in 0..args.len() {
                        for &s in arg_slots(k) {
                            use_at(&mut iv, &mut uses, s, pos);
                            reads[s.index()].push(i);
                            escape_deep!(contains, s, i);
                        }
                    }
                } else {
                    // Pure ops on a slot-derived value observe the
                    // address itself, not the slot's bytes — folding the
                    // slot onto another would change the result
                    // (`icmp`/`isub` ptrdiff/bit ops/truncation). Only
                    // pointer-affine ops keep a faithful same-slot
                    // address; everything else escapes the slot.
                    let affine = match op {
                        Opcode::Select
                        | Opcode::Uextend
                        | Opcode::Bitcast
                        | Opcode::Splat
                        | Opcode::Insertlane
                        | Opcode::Extractlane => true,
                        // ptr + scalar / ptr - scalar stays a pointer
                        // into the same slot; ptr + ptr / ptr - ptr is
                        // data — an identity observation.
                        Opcode::Iadd => arg_slots(0).is_empty() || arg_slots(1).is_empty(),
                        Opcode::Isub => arg_slots(1).is_empty(),
                        // `umulhi x, 0` is a constant — the result
                        // doesn't observe the operand's address
                        // (coldargs' disguised-zero rebind).
                        Opcode::Umulhi => args.iter().any(|&a| iconst_zero(func, a)),
                        _ => false,
                    };
                    if !affine {
                        // `PLIRON_SM_PRECISE=1`: a pure op is a use, not an
                        // escape. Its result inherits the operands' slots
                        // (`derived`, except compares: bools carry none), so
                        // every later use extends their intervals and a real
                        // sink (untracked store, capturing call) escapes them
                        // there. Any observation combining two slots'
                        // addresses then happens where both are live — they
                        // never fold. Escaping here instead pinned every
                        // slot whose address met an `icmp`/`band`/`isub`
                        // (stage-2 rustc's parser: 2.5-9x LLVM's frames,
                        // deep-recursion UI tests overflowed the stack).
                        let pu = precise();
                        for k in 0..args.len() {
                            for &s in arg_slots(k) {
                                use_at(&mut iv, &mut uses, s, pos);
                                if !pu {
                                    escape_deep!(contains, s, i);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // Marker positions widen a marked-escaped slot's interval to its
    // declared window — opaque accesses through a retained pointer are
    // covered while the marker says the storage lives. Everywhere else
    // real uses already bound the interval; extra marker positions would
    // just stretch it (a marker-only slot is dead storage the unused-slot
    // sweep reclaims anyway).
    // `Some` for the slots `mark_flow` tracks: their live set, which the
    // convex `iv` only bounds.
    for &s in &hard_esc {
        if s.index() < nslots {
            marked[s.index()] = false;
            escaped[s.index()] = true;
            iv[s.index()].1 = u32::MAX;
        }
    }
    let mut segs: Vec<Option<Vec<(u32, u32)>>> = vec![None; nslots];
    let flowiv = crate::env_is("PLIRON_SM_FLOWIV", "1");
    // Exactly placed start markers: dead just after the anchor, or just
    // before the first inst of the block they head. Kills for `use_flow`;
    // without FLOWIV they still tell `stale_markers` where a new life
    // begins.
    let mut starts: Vec<(StackSlot, Block, u32, u8)> = Vec::new();
    if markflow {
        for (ds, anchor, b, start) in lifetimes {
            let (&[s], true) = (ds.as_slice(), *start) else { continue };
            if s.index() >= nslots || !in_layout.contains(b) {
                continue;
            }
            match *anchor {
                Some(a) if func.layout.inst_block(a) == Some(*b) => starts.push((s, *b, ipos[a], 2)),
                None => starts.push((s, *b, bfirst[*b], 0)),
                Some(_) => {}
            }
        }
    }
    // Drop every marker of a slot the code contradicts; a marked slot
    // whose address escaped then counts as escaped after all.
    // `PLIRON_SM_STALECHECK=0` trusts the markers as before.
    let mut ek_ok: Vec<(StackSlot, Inst, u32)> = edge_kills.to_vec();
    if markflow && crate::pass_enabled("PLIRON_SM_STALECHECK") {
        let stale = stale_markers(func, &order, &bfirst, &ipos, &uses, &reads, &writes, &kills, edge_kills, &starts);
        let gone = |s: StackSlot| stale.get(s.index()).copied().unwrap_or(false);
        kills.retain(|k| !gone(k.0));
        ek_ok.retain(|k| k.2 == KILL_ALL || !gone(k.0));
        starts.retain(|k| !gone(k.0));
        for s in 0..nslots {
            if stale[s] && marked[s] {
                marked[s] = false;
                if esc_seen[s] {
                    escaped[s] = true;
                    iv[s].1 = u32::MAX;
                }
            }
        }
        if info_on() {
            let n: Vec<StackSlot> = (0..nslots).filter(|&s| stale[s]).map(|s| StackSlot::from_u32(s as u32)).collect();
            eprintln!("sm-info: stale markers on {} slots: {n:?}", n.len());
        }
    }
    let edge_kills: &[(StackSlot, Inst, u32)] = &ek_ok;
    if markflow {
        mark_flow(func, &order, &bfirst, &blast, &ipos, &kills, edge_kills, &marked, &esc_seen, &uses, &mut iv, &mut segs);
    }
    if flowiv {
        // The marker kills are only collected (and trusted) in flow mode.
        let (k, ek, st): (&[_], &[_], &[_]) = if markflow { (&kills, edge_kills, &starts) } else { (&[], &[], &[]) };
        use_flow(func, &order, &bfirst, &blast, &ipos, &derived, &uses, &reads, &writes, &escaped, k, ek, st, &mut iv, &mut segs);
    }
    // `PLIRON_SM_INFO_SLOT=n`: slot n's accesses and kills by position
    // (positions are RPO, not layout — find a poison site's culprit).
    if info_on()
        && let Some(n) = crate::env_get("PLIRON_SM_INFO_SLOT").and_then(|v| v.parse::<usize>().ok())
        && n < nslots
    {
        let ss = StackSlot::from_u32(n as u32);
        let blk = |p: u32| order.iter().find(|&&b| bfirst[b] <= p && p <= blast[b]).copied();
        for (what, v) in [("read", &reads[n]), ("write", &writes[n])] {
            for &i in v {
                eprintln!("sm-info: {ss} {what} pos={} {:?}: {}", ipos[i], func.layout.inst_block(i), func.dfg.display_inst(i));
            }
        }
        for &(s, b, p, end) in &kills {
            if s == ss {
                eprintln!("sm-info: {ss} kill pos={p} {b} end={end}");
            }
        }
        for &(s, b, p, k) in &starts {
            if s == ss {
                eprintln!("sm-info: {ss} start pos={p} {b} kind={k}");
            }
        }
        for &(s, t, w) in edge_kills {
            if s == ss {
                eprintln!("sm-info: {ss} edge-kill term pos={} {:?} succ={w}", ipos[t], blk(ipos[t]));
            }
        }
        for &b in &order {
            eprintln!("sm-info: block {b} pos {}..={} succ {:?}", bfirst[b], blast[b], succ_map[b]);
        }
    }
    for ((slots, _, _, _), pos) in lifetimes.iter().zip(&marker_pos) {
        let Some(pos) = pos.filter(|_| !markflow) else { continue };
        for &s in slots {
            if s.index() < nslots && marked[s.index()] && esc_seen[s.index()] {
                use_at(&mut iv, &mut uses, s, pos);
            }
        }
    }

    for e in iv.iter_mut() {
        if e.1 == u32::MAX && e.0 != u32::MAX {
            e.1 = last;
        }
    }

    // A backward edge lets a path re-enter an earlier region. A slot
    // whose in-span reads can observe bytes written on an earlier trip —
    // any read not dominated by an in-span write of the same slot —
    // stays live across the whole span. Slot addresses are fixed, so a
    // carried pointer alone doesn't force this; only stale data does.
    let mut spans: Vec<(u32, u32)> = Vec::new();
    for &b in &order {
        for i in func.layout.block_insts(b) {
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                let v = bc.block(&func.dfg.value_lists);
                // `<=`: a self-loop is a backedge too.
                if bpos[v] <= bpos[b] {
                    spans.push((bfirst[v], blast[b]));
                    if info_on() {
                        eprintln!(
                            "sm-info: backedge {b}->{v} span=({},{})",
                            bfirst[v], blast[b]
                        );
                    }
                }
            }
        }
    }
    // A span is (header start, back-edge source end) in RPO positions,
    // but RPO doesn't keep a loop's blocks contiguous: a nested loop
    // whose header lies inside span A can have its body (and back edges)
    // placed past A's end, and paths through that body still return to
    // A's header. cranelift's `compute_use_states` lost a 72-byte
    // iterator slot that way (`PLIRON_SM_POISON` found it). Close the
    // spans: A grows to cover any span whose header sits inside it.
    loop {
        let mut grew = false;
        for a in 0..spans.len() {
            for b in 0..spans.len() {
                let (alo, ahi) = spans[a];
                let (blo, bhi) = spans[b];
                if alo <= blo && blo <= ahi && bhi > ahi {
                    spans[a].1 = bhi;
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    if info_on() {
        let mut big: Vec<(u32, u32)> = spans.clone();
        big.sort();
        big.dedup();
        big.sort_by_key(|&(lo, hi)| hi - lo);
        eprintln!(
            "sm-info: {} backedge spans, widest: {:?}",
            spans.len(),
            big.iter().rev().take(6).collect::<Vec<_>>()
        );
    }
    if !spans.is_empty() {
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        // A read of a container is a read of every slot whose address it
        // holds: the callee dereferences through it. prqlc's closure env
        // held `&requirements`; the calls read only the env, so the
        // once-written `requirements` slot looked fresh in the loop and
        // `log` temps were merged onto it (2026-10-22).
        let mut via: Vec<Vec<Inst>> = vec![Vec::new(); nslots];
        for d in 0..nslots {
            for &s in &contains[d] {
                if s.index() < nslots {
                    via[s.index()].extend(reads[d].iter().copied());
                }
            }
        }
        for (s, e) in iv.iter_mut().enumerate() {
            // Marked slots too: a marker window is measured in the same RPO
            // positions, so a loop body placed past the exit still needs
            // the span (selmul `parse_only`'s `black_box(&mut acc)` under
            // PLIRON_SLOTMERGE_MARK + poison). A flow-tracked slot's
            // liveness already went round every back edge.
            if segs[s].is_some() {
                continue;
            }
            for &(lo, hi) in &spans {
                if !uses[s].iter().any(|&p| lo <= p && p <= hi) {
                    continue;
                }
                // A read after the span sees bytes any trip may have
                // written: partial in-loop writes accumulate across trips
                // (smallsort's bidirectional merge fills its scratch one
                // element per trip at each end; the copy-back after the
                // loop reads it all), so they are live across the back
                // edge — unless a whole-slot write in or after the span
                // dominates the read and supersedes them.
                let fresh = reads[s].iter().chain(&via[s]).all(|&r| {
                    let rp = ipos[r];
                    if rp < lo {
                        return true;
                    }
                    let after = rp > hi;
                    writes[s].iter().any(|&w| {
                        let wp = ipos[w];
                        lo <= wp
                            && (after || wp <= hi)
                            && covers_slot(func, w, StackSlot::from_u32(s as u32))
                            && dt.dominates(w, r, &func.layout)
                    })
                });
                if !fresh {
                    e.0 = e.0.min(lo);
                    e.1 = e.1.max(hi);
                }
            }
        }
    }

    // `contains` is closed over stores and byte-copies: a slot reachable
    // through another's bytes stays reachable for that container's whole
    // live range, so its interval must contain the container's. (The
    // store/copy edges were gathered before the scan; `contains` is the
    // fixpoint's result — iterate just the interval extension since
    // growing `d`'s range can widen `s`'s in turn.)
    loop {
        let mut changed = false;
        for d in 0..nslots {
            let (dlo, dhi) = iv[d];
            for &s in &contains[d] {
                if s.index() < nslots && dlo != u32::MAX && iv[s.index()].0 != u32::MAX {
                    if let Some(ds) = segs[d].clone().or(segs[s.index()].as_ref().map(|_| vec![(dlo, dhi)]))
                        && let Some(sv) = &mut segs[s.index()]
                    {
                        let n0: u32 = sv.iter().map(|&(a, b)| b - a + 1).sum();
                        for g in ds {
                            add_seg(sv, g);
                        }
                        let n1: u32 = sv.iter().map(|&(a, b)| b - a + 1).sum();
                        if n1 != n0 {
                            changed = true;
                            iv[s.index()] = (sv[0].0, sv[sv.len() - 1].1);
                        }
                        continue;
                    }
                }
                let e = &mut iv[s.index()];
                if e.0 != u32::MAX {
                    if e.0 > dlo {
                        e.0 = dlo;
                        changed = true;
                    }
                    if e.1 < dhi {
                        e.1 = dhi;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    if dbg {
        let mut cc: Vec<(usize, usize)> = contains
            .iter()
            .enumerate()
            .map(|(s, c)| (s, c.len()))
            .filter(|&(_, n)| n > 4)
            .collect();
        cc.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::slotmerge", "slotmerge: biggest contains: {:?}",
                cc.iter().take(8).map(|&(s, n)| (StackSlot::from_u32(s as u32), n)).collect::<Vec<_>>())
        };
        for s in 0..nslots {
            let s = StackSlot::from_u32(s as u32);
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::slotmerge", "slotmerge: {s} iv={:?} uses={:?}",
                    iv[s.index()],
                    uses[s.index()],
                )
            };
        }
        for &(s, i) in &esc {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::slotmerge", "slotmerge: {s} escapes at {i} {}: {}",
                    func.layout.inst_block(i).unwrap(),
                    func.dfg.display_inst(i),
                )
            };
        }
    }
    if info_on() {
        let mut unbounded = 0;
        let mut bounded = 0;
        let mut whole = 0;
        for e in &iv {
            if e.0 == u32::MAX {
                unbounded += 1;
            } else {
                bounded += 1;
                if e.0 <= 1 && e.1 + 64 >= last {
                    whole += 1;
                }
            }
        }
        eprintln!(
            "sm-info: {nslots} slots: {bounded} bounded ({whole} whole-fn), \
             {unbounded} unused, {} escaped, {} marked",
            escaped.iter().filter(|&&x| x).count(),
            marked.iter().filter(|&&x| x).count(),
        );
        let mut hist: FxHashMap<(u32, u32), u32> = FxHashMap::default();
        for (s, e) in iv.iter().enumerate() {
            if e.0 != u32::MAX && escaped[s] {
                *hist.entry((e.0 / 5000, e.1 / 5000)).or_default() += 1;
            }
        }
        let mut h: Vec<_> = hist.iter().collect();
        h.sort_by_key(|&(_, n)| std::cmp::Reverse(*n));
        eprintln!("sm-info: escaped-slot interval buckets (lo,hi)/5k: {h:?}");
        let mut hist2: FxHashMap<u32, u32> = FxHashMap::default();
        let mut spanb = vec![0u64; 8];
        for (s, e) in iv.iter().enumerate() {
            if e.0 != u32::MAX && !escaped[s] {
                *hist2.entry(e.1 - e.0).or_default() += 1;
                let span = e.1 - e.0;
                let b = match span {
                    0..=63 => 0,
                    64..=255 => 1,
                    256..=1023 => 2,
                    1024..=4095 => 3,
                    4096..=16383 => 4,
                    _ => 5,
                };
                spanb[b] += 1;
            }
        }
        let mut h2: Vec<_> = hist2.iter().map(|(&k, &n)| (k, n)).collect();
        h2.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        eprintln!(
            "sm-info: bounded interval lens (top10): {:?} span-hist: {spanb:?} last={last}",
            h2.iter().take(10).collect::<Vec<_>>()
        );
        let mut ops: FxHashMap<String, u32> = FxHashMap::default();
        for &(_, i) in &esc {
            *ops.entry(func.dfg.insts[i].opcode().to_string())
                .or_default() += 1;
        }
        let mut opv: Vec<_> = ops.into_iter().collect();
        opv.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        eprintln!("sm-info: escape insts by opcode: {opv:?}");
        let mut by_fn: FxHashMap<String, u32> = FxHashMap::default();
        for &(_, i) in &esc {
            if let InstructionData::TryCall { func_ref, .. }
            | InstructionData::Call { func_ref, .. } = func.dfg.insts[i]
            {
                *by_fn
                    .entry(format!(
                        "{func_ref}/{:?}",
                        func.dfg.ext_funcs[func_ref].name
                    ))
                    .or_default() += 1;
            }
        }
        let mut fnv: Vec<_> = by_fn.into_iter().collect();
        fnv.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        eprintln!("sm-info: escape insts by callee: {fnv:?}");
        // `PLIRON_SM_INFO=all`: the first escape of every slot, not just
        // the first 8 overall.
        let all = info_on() && crate::env_is("PLIRON_SM_INFO", "all");
        let mut shown = FxHashSet::default();
        for &(s, i) in esc.iter().filter(|&&(s, _)| !all || shown.insert(s)).take(if all { usize::MAX } else { 8 }) {
            // Which args carry the slot, and whether each is nocap.
            let how: Vec<String> = func
                .dfg
                .inst_args(i)
                .iter()
                .enumerate()
                .filter(|&(_, &a)| derived[func.dfg.resolve_aliases(a)].contains(&s))
                .map(|(k, _)| format!("arg{k}{}", if nocap.contains(&(i, k as u32)) { ":nocap" } else { "" }))
                .collect();
            eprintln!(
                "sm-info:   {s} escapes at {}: {} [{}]",
                func.layout.inst_block(i).unwrap(),
                func.dfg.display_inst(i),
                if how.is_empty() { "contained".to_string() } else { how.join(",") }
            );
        }
    }
    // `PLIRON_SM_POISON=1`: a use-after-scope check for this analysis.
    // Merge nothing; instead overwrite each slot that coloring would have
    // considered with 0xA5 bytes wherever the analysis says it is dead —
    // so a wrong interval reads garbage at its first stale access, in
    // every run, whether or not a merge would have reused the bytes.
    if crate::env_set("PLIRON_SM_POISON") {
        return poison_dead(func, &order, &bfirst, &blast, &succ_map, &iv, &segs, &escaped);
    }

    // `PLIRON_SM_COALESCE=1`: copy coalescing (LLVM MemCpyOpt's stack
    // move). D -> (S, copy window, the window's (load, store) pairs).
    let coalesce = crate::env_is("PLIRON_SM_COALESCE", "1");
    let coal = if coalesce {
        copy_groups(func, &order, &ipos, &users, &derived, &iv, &segs, &escaped, &esc_seen)
    } else {
        FxHashMap::default()
    };
    // Greedy interval coloring, best-fit by region size.
    //
    // `PLIRON_SM_BYSIZE=1`: largest slot first, as LLVM's StackColoring
    // does. In `lo` order a small temporary that starts first claims a
    // region a later, larger disjoint one can never join (regions only
    // take members no bigger than their rep), so a big `match`'s arms
    // each got their own regions. Largest-first, every region is opened
    // by its biggest member and smaller disjoint ones pack in; free
    // regions are then tested by segment overlap, since `end < lo` only
    // holds for members taken in `lo` order. Ties go by `lo`, so a copy
    // source still precedes its same-size destination for coalescing.
    let bysize = crate::env_is("PLIRON_SM_BYSIZE", "1");
    let mut cand: Vec<(std::cmp::Reverse<u32>, u32, StackSlot)> = (0..nslots)
        .map(|s| StackSlot::from_u32(s as u32))
        .filter(|&s| iv[s.index()].0 != u32::MAX && !escaped[s.index()])
        .map(|s| {
            let key = if bysize { func.sized_stack_slots[s].size } else { 0 };
            (std::cmp::Reverse(key), iv[s.index()].0, s)
        })
        .collect();
    cand.sort();
    let segs_on = markflow || coalesce || bysize || flowiv;
    // (region end, region size, region align_shift, representative)
    let mut regions: Vec<(u32, u32, u8, StackSlot)> = Vec::new();
    // With flow-tracked slots a region's occupancy has holes: its members'
    // positions as one sorted, disjoint segment list (flow mode only — for
    // convex members taken in `lo` order it equals `..=end`).
    let mut occ: Vec<Vec<(u32, u32)>> = Vec::new();
    // `PLIRON_SM_STARTS=1` (with segments): LLVM StackColoring's
    // interference test. Two slots are in use together only if one starts
    // being in use while the other is live, so they conflict iff a start
    // of one lies in the other's live set — not wherever both sets
    // overlap. A shared cleanup tail where every arm's slots are live (MIR
    // has no StorageDead on unwind paths), reached from different arms,
    // then no longer chains them all together (lower_item_kind: two
    // 328-byte slots per arm, none merged). A slot only starts being in
    // use at an access, so its recorded uses are its starts. `ost[r]`:
    // every member's starts, sorted.
    let starts_on = segs_on && crate::env_is("PLIRON_SM_STARTS", "1");
    let mut ost: Vec<Vec<u32>> = Vec::new();
    // Each region's members, for the coalescing test under `starts_on`
    // (members' live sets may overlap there, so the source's segments
    // can't simply be subtracted from `occ`).
    let mut members: Vec<Vec<StackSlot>> = Vec::new();
    // Each region's coalesced copy windows (see the coalescing test).
    let mut wins: Vec<Vec<(u32, u32)>> = Vec::new();
    let pts_in = |pts: &[u32], sg: &[(u32, u32)]| {
        pts.iter().any(|&p| {
            let k = sg.partition_point(|&(_, b)| b < p);
            sg.get(k).is_some_and(|&(a, _)| a <= p)
        })
    };
    let mut rep: FxHashMap<StackSlot, StackSlot> = FxHashMap::default();
    // Region index of each region's representative slot, and the copy
    // destinations that joined their source's region.
    let mut reg_of: FxHashMap<StackSlot, usize> = FxHashMap::default();
    let mut coalesced: Vec<StackSlot> = Vec::new();
    for (_, _, s) in cand {
        let (lo, hi) = iv[s.index()];
        let mine: Vec<(u32, u32)> = segs[s.index()].clone().unwrap_or_else(|| vec![(lo, hi)]);
        let my_starts: Vec<u32> = if starts_on {
            let mut v = uses[s.index()].clone();
            v.sort_unstable();
            v.dedup();
            v
        } else {
            Vec::new()
        };
        let (size, align) = {
            let d = &func.sized_stack_slots[s];
            (d.size, d.align_shift)
        };
        let mut best: Option<usize> = None;
        // A copy destination joins its source's region when they overlap
        // only inside the copy window: the copy then rewrites bytes with
        // themselves and goes away. With the pair gone the bytes sit in
        // memory across the whole window, so it is occupied too — no
        // other member may live there (a slot between the copy's load and
        // store would overwrite them), and none may join it later.
        if let Some((src, w, _)) = coal.get(&s)
            && let Some(&r) = rep.get(src).and_then(|x| reg_of.get(x))
            && regions[r].1 >= size
            && regions[r].2 >= align
        {
            let sseg: Vec<(u32, u32)> = segs[src.index()].clone().unwrap_or_else(|| vec![iv[src.index()]]);
            let inside = seg_isect(&mine, &sseg).iter().all(|&(a, b)| w.0 <= a && b <= w.1);
            let win = (!crate::env_is("PLIRON_SM_COALWIN", "0")).then_some(*w);
            let free = if starts_on {
                // The other members' live sets and starts, rebuilt: the
                // source may overlap them where no one starts.
                let (mut ol, mut os) = (Vec::new(), Vec::new());
                for &m in members[r].iter().filter(|&&m| m != *src) {
                    for &g in segs[m.index()].as_deref().unwrap_or(&[iv[m.index()]]) {
                        add_seg(&mut ol, g);
                    }
                    os.extend(&uses[m.index()]);
                }
                os.sort_unstable();
                !pts_in(&my_starts, &ol)
                    && !pts_in(&os, &mine)
                    && !win.is_some_and(|w| segs_overlap(&ol, &[w]) || pts_in(&os, &[w]))
                    // `ol` is rebuilt from the members' own segments, so
                    // it misses the copy windows of pairs coalesced
                    // earlier: their bytes must stay put across the window
                    // (seed 13475: a later dest's dead stores landed inside
                    // one and clobbered the value the removed copy carried).
                    && !segs_overlap(&wins[r], &mine)
                    && !win.is_some_and(|w| segs_overlap(&wins[r], &[w]))
            } else {
                let others = seg_sub(&occ[r], &sseg);
                !segs_overlap(&others, &mine) && !win.is_some_and(|w| segs_overlap(&others, &[w]))
            };
            if inside && free && sm_take() {
                rep.insert(s, regions[r].3);
                regions[r].0 = regions[r].0.max(hi);
                for g in mine.into_iter().chain(win) {
                    add_seg(&mut occ[r], g);
                }
                if let Some(w) = win {
                    add_seg(&mut wins[r], w);
                }
                if starts_on {
                    ost[r].extend(&my_starts);
                    ost[r].sort_unstable();
                }
                members[r].push(s);
                coalesced.push(s);
                continue;
            }
        }
        for (r, &(end, rsize, ralign, _)) in regions.iter().enumerate() {
            let free = if starts_on {
                // A coalesced pair's window has no starts in `ost`, but its
                // bytes are live across it.
                !pts_in(&my_starts, &occ[r]) && !pts_in(&ost[r], &mine) && !segs_overlap(&wins[r], &mine)
            } else if markflow || bysize || flowiv {
                !segs_overlap(&occ[r], &mine)
            } else {
                end < lo
            };
            // Largest-first, a member's stricter alignment just raises the
            // rep's (the rewrite below grows size and alignment alike).
            if free
                && rsize >= size
                && (bysize || ralign >= align)
                && best.is_none_or(|b| regions[b].1 > rsize)
            {
                best = Some(r);
            }
        }
        // `PLIRON_SM_LIMIT=n`: accept only the first n merges in this
        // process (delta-debug a bad merge; pair with PLIRON_OPT_FNS).
        let best = best.filter(|_| sm_take());
        match best {
            Some(r) => {
                rep.insert(s, regions[r].3);
                regions[r].0 = regions[r].0.max(hi);
                regions[r].2 = regions[r].2.max(align);
                if segs_on {
                    for g in mine {
                        add_seg(&mut occ[r], g);
                    }
                }
                if starts_on {
                    ost[r].extend(&my_starts);
                    ost[r].sort_unstable();
                }
                members[r].push(s);
            }
            None => {
                rep.insert(s, s);
                reg_of.insert(s, regions.len());
                regions.push((hi, size, align, s));
                occ.push(if segs_on { mine } else { Vec::new() });
                ost.push(my_starts);
                members.push(vec![s]);
                wins.push(Vec::new());
            }
        }
    }
    if info_on() {
        let ncand = rep.len();
        eprintln!("sm-info: merge: {ncand} cand -> {} regions", regions.len());
        // Per slot: singleton markers naming it, (start, end) x (resolved, lost).
        let mut mk = vec![[0u32; 4]; nslots];
        for ((ds, _, _, start), pos) in lifetimes.iter().zip(&marker_pos) {
            if let &[s] = ds.as_slice()
                && s.index() < nslots
            {
                mk[s.index()][2 * usize::from(!*start) + usize::from(pos.is_none())] += 1;
            }
        }
        let mut ek = vec![(0u32, 0u32); nslots];
        for &(s, term, _) in edge_kills {
            if s.index() < nslots {
                ek[s.index()].0 += 1;
                ek[s.index()].1 += u32::from(func.layout.inst_block(term).is_some());
            }
        }
        for s in 0..nslots {
            let ss = StackSlot::from_u32(s as u32);
            if iv[s].0 != u32::MAX {
                let [sr, sl, er, el] = mk[s];
                let (ekn, eka) = ek[s];
                eprintln!(
                    "sm-info:   {ss} iv={:?} esc={} seen={} marked={} rep={:?} size={} markers start={sr}+{sl}lost end={er}+{el}lost edge-kills={eka}/{ekn} segs={:?}",
                    iv[s],
                    escaped[s],
                    esc_seen[s],
                    marked[s],
                    rep.get(&ss),
                    func.sized_stack_slots[ss].size,
                    segs[s].as_ref().map(|v| v.iter().take(8).collect::<Vec<_>>()),
                );
            }
        }
    }

    // Rewrite `stack_addr`s onto representatives; grow each rep to its
    // largest member and zero the absorbed slots. Slots with no live
    // interval may still be referenced by `stack_addr` insts whose
    // derived uses we failed to track — keep their size unless no
    // `stack_addr` in the function names them at all.
    let mut addr_refs: FxHashSet<StackSlot> = FxHashSet::default();
    for &b in &order {
        for i in func.layout.block_insts(b) {
            if let InstructionData::StackAddr { stack_slot, .. } = func.dfg.insts[i] {
                addr_refs.insert(stack_slot);
            }
        }
    }
    let mut merged = 0;
    let mut grown: Vec<(StackSlot, u32, u8)> = Vec::new();
    for s in 0..nslots {
        let s = StackSlot::from_u32(s as u32);
        match rep.get(&s).copied() {
            None => {
                if iv[s.index()].0 == u32::MAX && !addr_refs.contains(&s) {
                    func.sized_stack_slots[s].size = 0;
                }
            }
            Some(r) if r != s => {
                merged += 1;
                let sd = &func.sized_stack_slots[s];
                grown.push((r, sd.size, sd.align_shift));
                func.sized_stack_slots[s].size = 0;
                // `-g`: `r`'s bytes now hold `s`'s data part of the time,
                // so it no longer locates `r`'s variable.
                func.sized_stack_slots[r].key = None;
            }
            _ => {}
        }
    }
    if info_on() {
        let (mut nz, mut bytes) = (0u32, 0u64);
        let mut cat: FxHashMap<&'static str, (u32, u64)> = FxHashMap::default();
        for s in 0..nslots {
            let s = StackSlot::from_u32(s as u32);
            let sz = func.sized_stack_slots[s].size as u64;
            if sz == 0 {
                continue;
            }
            nz += 1;
            bytes += sz;
            let k = if escaped[s.index()] {
                "escaped"
            } else if rep.get(&s) == Some(&s) && iv[s.index()].0 != u32::MAX {
                "region-rep"
            } else if iv[s.index()].0 == u32::MAX {
                "unused-referenced"
            } else {
                "other"
            };
            let e = cat.entry(k).or_default();
            e.0 += 1;
            e.1 += sz;
        }
        eprintln!("sm-info: post: {nz} nonzero slots {bytes}B by-kind {cat:?}");
    }
    if merged > 0 {
        let insts: Vec<Inst> = order
            .iter()
            .flat_map(|&b| func.layout.block_insts(b))
            .collect();
        for i in insts {
            if let InstructionData::StackAddr { stack_slot, .. } = &mut func.dfg.insts[i]
                && let Some(&r) = rep.get(stack_slot)
            {
                *stack_slot = r;
            }
        }
        for (r, size, align) in grown {
            let rd = &mut func.sized_stack_slots[r];
            rd.size = rd.size.max(size);
            rd.align_shift = rd.align_shift.max(align);
        }
        // Each coalesced pair now loads a location and stores the value
        // straight back, with nothing else touching either in the window.
        let mut gone = 0usize;
        for d in &coalesced {
            for &(l, st) in &coal[d].2 {
                if l == st {
                    // A coalesced `memcpy(D, S, n)` call: memcpy returns D.
                    let dst = func.dfg.inst_args(l)[0];
                    let res: Vec<Value> = func.dfg.inst_results(l).to_vec();
                    func.dfg.detach_inst_results(l);
                    for r in res {
                        func.dfg.change_to_alias(r, dst);
                    }
                    func.layout.remove_inst(l);
                    gone += 1;
                    continue;
                }
                func.layout.remove_inst(st);
                func.layout.remove_inst(l);
                gone += 1;
            }
        }
        if info_on() {
            eprintln!("sm-info: coalesced {} copies, {gone} load/store pairs removed", coalesced.len());
        }
    }
    merged
}

/// Bytes of each slot `poison_dead` overwrites (the rest stay as they are:
/// a wrong interval still trips on the first 256 bytes in practice).
const POISON_MAX: u32 = 256;

// r[impl verify.poison]
/// See `PLIRON_SM_POISON` in `run`. A slot's interval `[lo, hi]` is in the
/// same RPO positions coloring uses; outside it the bytes are claimed
/// dead. Poison right after the instruction at `hi` (unless it ends its
/// block) and at the head of every block lying wholly outside the
/// interval that is entered from a block overlapping it: any path that
/// leaves the live range and comes back without rewriting the slot then
/// reads 0xA5..., exactly what a merged neighbour's write could have left.
fn poison_dead(
    func: &mut Function,
    order: &[Block],
    bfirst: &SecondaryMap<Block, u32>,
    blast: &SecondaryMap<Block, u32>,
    succ_map: &SecondaryMap<Block, Vec<Block>>,
    iv: &[(u32, u32)],
    segs: &[Option<Vec<(u32, u32)>>],
    escaped: &[bool],
) -> usize {
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::{InstBuilder, types};
    let mut pos_inst: Vec<Inst> = Vec::new();
    for &b in order {
        pos_inst.extend(func.layout.block_insts(b));
    }
    let overlaps = |b: Block, lo: u32, hi: u32| bfirst[b] <= hi && blast[b] >= lo;
    // `PLIRON_SM_POISON_SLOTS=a..b`: poison only slot indices in [a, b)
    // (bisect which slot's interval is wrong).
    let range: Option<(usize, usize)> = crate::env_get("PLIRON_SM_POISON_SLOTS").and_then(|r| {
        let (a, b) = r.split_once("..")?;
        Some((a.parse().ok()?, b.parse().ok()?))
    });
    let mut sites: Vec<(StackSlot, Option<Block>, Option<Inst>)> = Vec::new();
    for s in 0..iv.len() {
        let (lo, hi) = iv[s];
        if lo == u32::MAX || escaped[s] || range.is_some_and(|(a, b)| s < a || s >= b) {
            continue;
        }
        let ss = StackSlot::from_u32(s as u32);
        if info_on() {
            eprintln!("sm-info: poison {ss} iv=({lo}, {hi}) at-hi={:?}", pos_inst.get(hi as usize));
        }
        // A flow-tracked slot is dead in its segments' holes too: poison
        // after every segment's end and at every block entered from live
        // code that its segments miss.
        let sg: Vec<(u32, u32)> = segs[s].clone().unwrap_or_else(|| vec![(lo, hi)]);
        let live = |b: Block| sg.iter().any(|&(l, h)| overlaps(b, l, h));
        for &(_, h) in &sg {
            if let Some(&i) = pos_inst.get(h as usize)
                && !func.dfg.insts[i].opcode().is_terminator()
            {
                sites.push((ss, None, Some(i)));
            }
        }
        let mut heads: FxHashSet<Block> = FxHashSet::default();
        for &b in order {
            if !live(b) {
                continue;
            }
            for &t in &succ_map[b] {
                if !live(t) {
                    heads.insert(t);
                }
            }
        }
        sites.extend(heads.into_iter().map(|t| (ss, Some(t), None)));
    }
    // `PLIRON_SM_POISON_SITES=a..b`: keep only sites a..b (in the order
    // `PLIRON_SM_INFO` lists them) — bisect which exit path is wrong.
    if let Some((a, b)) = crate::env_get("PLIRON_SM_POISON_SITES").and_then(|r| {
        let (a, b) = r.split_once("..")?;
        Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
    }) {
        sites = sites.into_iter().enumerate().filter(|(k, _)| *k >= a && *k < b).map(|(_, x)| x).collect();
    }
    if info_on() {
        for (k, (ss, head, after)) in sites.iter().enumerate() {
            let span = head.map(|b| (bfirst[b], blast[b]));
            eprintln!("sm-info: poison site {k}: {ss} head={head:?} pos={span:?} after={after:?}");
        }
    }
    let n = sites.len();
    // The pointer type `stack_store` needs: whatever this function's
    // `stack_addr`s produce (i32 on wasm32).
    let pty = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .find(|&i| matches!(func.dfg.insts[i], InstructionData::StackAddr { .. }))
        .map(|i| func.dfg.value_type(func.dfg.first_result(i)))
        .unwrap_or(types::I64);
    for (ss, head, after) in sites {
        let size = func.sized_stack_slots[ss].size.min(POISON_MAX);
        let mut cur = FuncCursor::new(func);
        match (head, after) {
            (Some(b), _) => {
                let Some(first) = cur.func.layout.first_inst(b) else {
                    continue;
                };
                cur = cur.at_inst(first);
            }
            (None, Some(i)) => {
                cur = cur.after_inst(i);
            }
            _ => continue,
        }
        let w = cur.ins().iconst(types::I64, 0xA5A5_A5A5_A5A5_A5A5u64 as i64);
        let mut off = 0u32;
        while off + 8 <= size {
            cur.ins().stack_store(pty, w, ss, off as i32);
            off += 8;
        }
        if off < size {
            let b8 = cur.ins().iconst(types::I8, 0xA5);
            while off < size {
                cur.ins().stack_store(pty, b8, ss, off as i32);
                off += 1;
            }
        }
    }
    n
}

/// Does write `w` (a store or a `memset`/`memcpy` libcall recorded in
/// `writes`) overwrite all of `s`? Only then does it make the slot fresh
/// for a loop trip: an 8-byte store to an array iterator's counter left
/// the array bytes behind it live across the back edge (tests/loopvec,
/// caught by `PLIRON_SM_POISON`), so a merge there could clobber them.
fn covers_slot(func: &Function, w: Inst, s: StackSlot) -> bool {
    let size = func.sized_stack_slots[s].size as i64;
    // `v` = `stack_addr s+off` (through aliases): `off`.
    let base_off = |v: Value| -> Option<i64> {
        let v = func.dfg.resolve_aliases(v);
        let i = func.dfg.value_def(v).inst()?;
        match func.dfg.insts[i] {
            InstructionData::StackAddr { stack_slot, offset, .. } if stack_slot == s => {
                Some(i64::from(offset))
            }
            _ => None,
        }
    };
    match func.dfg.insts[w] {
        InstructionData::Store { args, offset, .. } => {
            let bytes = i64::from(func.dfg.value_type(args[0]).bytes());
            base_off(args[1]).is_some_and(|o| o + i64::from(offset) == 0 && bytes >= size)
        }
        _ => {
            // mem* libcall: (dst, src|byte, len).
            let a = func.dfg.inst_args(w);
            let len = a.get(2).and_then(|&l| {
                let i = func.dfg.value_def(func.dfg.resolve_aliases(l)).inst()?;
                match func.dfg.insts[i] {
                    InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
                    _ => None,
                }
            });
            a.first().and_then(|&d| base_off(d)) == Some(0) && len.is_some_and(|n| n >= size)
        }
    }
}
