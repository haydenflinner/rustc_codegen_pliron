//! Stack-slot SSA promotion — mem2reg for `explicit_slot`s
//! (`PLIRON_SLOTPROM`).
//!
//! rustc's lowering leaves small aggregates in stack slots when ir-opt's
//! SROA can't split them — a `HashMap`'s `{hasher, ctrl, mask, items, ..}`
//! struct, a `Vec`'s `{ptr, cap, len}`, the `{l, scratch, offset}` cursor
//! in ipnsort's partition loop. Every read/write then pays a real
//! `load`/`store`; LLVM's mem2reg turns the same fields into loop-carried
//! SSA values, which is exactly the stack traffic the disassembly diffs
//! show in `hmget`'s probe loop and `quicksort`'s partition loop.
//!
//! A slot field is promoted when the slot's address never escapes — every
//! use of every `stack_addr`-derived value is a load/store address or a
//! const `iadd` feeding one — and the field's byte range partially
//! overlaps no other access (a wide `load.i8x16` covering two i64 fields
//! keeps all three on memory; disjoint fields promote independently).
//!
//! The rewrite is block-param SSA construction rather than phi nodes:
//! per promoted field, `livein[b]` (an upward-exposed use, or a successor
//! needing the value past a block that doesn't define it) gets a trailing
//! block param and every predecessor edge appends the reaching value.
//! Loads become aliases of the current value, stores update it and die.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::immediates::Offset32;
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, MemFlagsData, Opcode, StackSlot,
    Type, Value, types,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::loadfwd;
use crate::webroot::{self, Web, Webs};

/// Cap on promoted fields per function — param bloat bound.
const MAX_FIELDS: usize = 48;
/// Cap on memory accesses per slot before the slot is left alone.
const MAX_ACCS: usize = 128;
/// Cap on blocks receiving a param for one field — keeps deep CFGs from
/// growing signatures unboundedly.
const MAX_LIVEIN: usize = 256;
/// Skip gigantic functions outright; the field×block matrices are
/// quadratic-ish and a CGU this size is a rarity anyway.
const MAX_BLOCKS: usize = 8192;

/// A slot address passed to a call operand marked `nocap` (rustc's
/// `captures(none)`, our inferred `nocapture`, or rustc's `noalias` —
/// `&mut`/`&`-frozen reborrows whose scope ends at the call, so no
/// retained copy can legally touch the memory afterward) is a *bounded*
/// escape rather than a disqualifying one: the callee may read/write the
/// slot's bytes during the call but can't retain a usable alias past it.
/// Promoted fields then just need writeback stores before the call and
/// reload loads on its return edges — a cold `reserve_rehash(&mut m)`
/// inside a hot loop is the canonical case: eager per-iteration
/// coherence writes become two bursts bracketing the call.
///
/// The sync set is conservative: every promoted field of the slot is
/// written back and reloaded (the callee's operand extent isn't tracked
/// here), and a call whose normal/exception edges don't land on
/// single-pred trampoline blocks isn't syncable — the reload must run
/// exactly on the call's return path.
///
/// Byte range + access type of one promotable slot field.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Field {
    slot: StackSlot,
    off: i64,
    bytes: i64,
    ty: Type,
}

/// One memory access against a slot's bytes.
#[derive(Clone, Copy)]
struct Acc {
    off: i64,
    bytes: i64,
    /// `Some(ty)` for plain `load`/`store` (promotable kind, value type
    /// `ty`); `None` for narrow/atomic accesses that pin bytes to memory
    /// without being promotable.
    ty: Option<Type>,
}

/// Memory bytes each opcode touches when the address is slot-derived.
/// `None` — not a plain tracked access at all.
fn op_bytes(op: Opcode, ty: Type) -> Option<(i64, bool)> {
    let b = match op {
        Opcode::Load => return Some((i64::from(ty.bytes()), true)),
        Opcode::Store => return Some((i64::from(ty.bytes()), true)),
        Opcode::Uload8 | Opcode::Sload8 | Opcode::Istore8 => 1,
        Opcode::Uload16 | Opcode::Sload16 | Opcode::Istore16 => 2,
        Opcode::Uload32 | Opcode::Sload32 | Opcode::Istore32 => 4,
        Opcode::Uload8x8
        | Opcode::Sload8x8
        | Opcode::Uload16x4
        | Opcode::Sload16x4
        | Opcode::Uload32x2
        | Opcode::Sload32x2 => 8,
        _ => return None,
    };
    Some((b, false))
}

/// The address operand of a load/store is promotable to slot `ss` at
/// `off` when `w` proves the access hits exactly `(ss, off)`: a single
/// slot member, no `other` alternatives, and any constant alternative
/// (`nul`/`bad`) ruled out by `notrap` — the flag asserts the access
/// can't trap, so a path delivering an invalid const is already UB.
fn promo_member(w: &Web, notrap: bool) -> Option<(StackSlot, i64)> {
    if w.other || w.slots.len() != 1 {
        return None;
    }
    let (ss, o) = w.slots[0];
    if o == webroot::ANY || ((w.nul || w.bad) && !notrap) {
        return None;
    }
    Some((ss, o))
}

/// Record one memory access of `bytes` at `off + memop-offset`: promotes
/// when the web proves a single exact slot site, otherwise pins every
/// slot member's range (`ty: None` keeps the bytes on memory); an `ANY`
/// member can't be bounded and escapes its slot.
#[allow(clippy::too_many_arguments)]
fn access(
    w: &Web,
    i: Inst,
    func: &Function,
    off: i64,
    bytes: i64,
    ty: Option<Type>,
    accs: &mut FxHashMap<StackSlot, Vec<Acc>>,
    esc: &mut Vec<StackSlot>,
) {
    match promo_member(w, loadfwd::notrap(func, i)) {
        Some((ss, o)) => accs.entry(ss).or_default().push(Acc {
            off: o.wrapping_add(off),
            bytes,
            ty,
        }),
        None => {
            for &(ss, o) in &w.slots {
                if o == webroot::ANY {
                    esc.push(ss);
                } else {
                    accs.entry(ss).or_default().push(Acc {
                        off: o.wrapping_add(off),
                        bytes,
                        ty: None,
                    });
                }
            }
        }
    }
}

/// The promotable site (slot, absolute offset) of inst `i`'s address
/// operand `arg` as evaluated in block `b`, after `notrap` refinement.
fn site(
    webs: &Webs,
    func: &Function,
    i: Inst,
    b: Block,
    arg: Value,
    offset: Offset32,
) -> Option<(StackSlot, i64)> {
    let w = webs.at(func, func.dfg.resolve_aliases(arg), b);
    let (ss, o) = promo_member(&w, loadfwd::notrap(func, i))?;
    Some((ss, o.wrapping_add(i64::from(i32::from(offset)))))
}

/// Every branch destination of try_call `i` is a dedicated single-pred
/// trampoline (lowering builds `pre`/`normal` per call), so reloads
/// inserted at its top run exactly on that call's return path.
fn single_pred_dests(func: &Function, cfg: &ControlFlowGraph, i: Inst) -> bool {
    let dfg = &func.dfg;
    !dfg.insts[i]
        .branch_destination(&dfg.jump_tables, &dfg.exception_tables)
        .is_empty()
        && dfg.insts[i]
            .branch_destination(&dfg.jump_tables, &dfg.exception_tables)
            .iter()
            .all(|bc| cfg.pred_iter(bc.block(&dfg.value_lists)).count() == 1)
}

/// A zero constant of field type, materialized at the top of `b` — the
/// "undef" reaching value for entry-block reads (the original program
/// read uninitialized slot bytes; any value refines that).
fn undef(func: &mut Function, b: Block, ty: Type) -> Value {
    let mut pos = FuncCursor::new(func).at_first_insertion_point(b);
    if ty.is_int() && !ty.is_vector() {
        if ty == types::I128 {
            let z = pos.ins().iconst(types::I64, 0);
            return pos.ins().iconcat(z, z);
        }
        return pos.ins().iconst(ty, 0);
    }
    if ty.is_vector() {
        let lane = ty.lane_type();
        let z = if lane.is_int() {
            pos.ins().iconst(lane, 0)
        } else {
            match lane {
                types::F32 => pos.ins().f32const(0.0),
                _ => pos.ins().f64const(0.0),
            }
        };
        return pos.ins().splat(ty, z);
    }
    match ty {
        types::F32 => pos.ins().f32const(0.0),
        types::F64 => pos.ins().f64const(0.0),
        _ => {
            let z = pos.ins().iconst(types::I64, 0);
            pos.ins().bitcast(
                ty,
                cranelift_codegen::ir::MemFlagsData::new()
                    .with_endianness(cranelift_codegen::ir::Endianness::Little),
                z,
            )
        }
    }
}

pub fn run(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    nocap: &FxHashSet<(Inst, u32)>,
) -> usize {
    let Some(entry) = func.layout.entry_block() else {
        return 0;
    };
    let nblocks = func.layout.blocks().count();
    if func.sized_stack_slots.is_empty() || nblocks > MAX_BLOCKS {
        return 0;
    }
    let timer = crate::env_set("PLIRON_SLOTPROM_TIME");
    let t0 = std::time::Instant::now();
    let webs = Webs::new(func, cfg);
    if timer {
        eprintln!("slotprom: webs {:?} ({} blocks)", t0.elapsed(), nblocks);
    }
    let t1 = std::time::Instant::now();

    // Reachable blocks only — dead blocks' accesses stay on memory.
    let mut reach: FxHashSet<Block> = FxHashSet::default();
    let mut st = vec![entry];
    while let Some(b) = st.pop() {
        if reach.insert(b) {
            st.extend(cfg.succ_iter(b));
        }
    }

    // Per-slot access lists; `bad` = escaped slots.
    let mut accs: FxHashMap<StackSlot, Vec<Acc>> = FxHashMap::default();
    let mut bad: FxHashSet<StackSlot> = FxHashSet::default();
    // Slot → call insts whose operand is a bounded (`nocap`) use of the
    // slot's address — promoted fields sync around them.
    let mut sync_calls: FxHashMap<StackSlot, Vec<Inst>> = FxHashMap::default();
    for b in func.layout.blocks() {
        if !reach.contains(&b) {
            continue;
        }
        for i in func.layout.block_insts(b) {
            // A poisoned analysis poisons every query — the results are
            // discarded below anyway, so stop scanning.
            if webs.poisoned() {
                break;
            }
            let nargs = func.dfg.inst_args(i).len();
            for (k, v) in func.dfg.inst_values(i).enumerate() {
                let v = func.dfg.resolve_aliases(v);
                let w = webs.at(func, v, b);
                if w.slots.is_empty() {
                    continue;
                }
                let data = &func.dfg.insts[i];
                let mut esc: Vec<StackSlot> = Vec::new();
                match data {
                    InstructionData::Load {
                        opcode,
                        arg,
                        offset,
                        ..
                    } if k < nargs && func.dfg.resolve_aliases(*arg) == v => {
                        let rty = func.dfg.value_type(func.dfg.first_result(i));
                        match op_bytes(*opcode, rty) {
                            Some((bytes, plain)) => access(
                                &w,
                                i,
                                func,
                                i64::from(i32::from(*offset)),
                                bytes,
                                plain.then_some(rty),
                                &mut accs,
                                &mut esc,
                            ),
                            None => esc.extend(w.slots.iter().map(|&(s, _)| s)),
                        }
                    }
                    InstructionData::Store {
                        opcode,
                        args,
                        offset,
                        ..
                    } if k < nargs => {
                        if func.dfg.resolve_aliases(args[1]) == v {
                            let vty = func.dfg.value_type(args[0]);
                            match op_bytes(*opcode, vty) {
                                Some((bytes, plain)) => access(
                                    &w,
                                    i,
                                    func,
                                    i64::from(i32::from(*offset)),
                                    bytes,
                                    plain.then_some(vty),
                                    &mut accs,
                                    &mut esc,
                                ),
                                None => esc.extend(w.slots.iter().map(|&(s, _)| s)),
                            }
                        } else {
                            // The stored *value* may be a slot-derived
                            // pointer — it outlives promotion.
                            esc.extend(w.slots.iter().map(|&(s, _)| s));
                        }
                    }
                    InstructionData::Binary {
                        opcode: Opcode::Iadd | Opcode::Isub,
                        ..
                    }
                    | InstructionData::Ternary {
                        opcode: Opcode::Select,
                        ..
                    }
                    | InstructionData::IntCompare { .. }
                    | InstructionData::FloatCompare { .. } => {}
                    _ if data.opcode().is_call()
                        && k < nargs
                        && !matches!(
                            data.opcode(),
                            Opcode::ReturnCall
                                | Opcode::ReturnCallIndirect
                                | Opcode::StackSwitch
                        ) =>
                    {
                        // Operand `k`; the callee operand of an indirect
                        // call isn't a pointee arg.
                        let adj = usize::from(matches!(
                            data.opcode(),
                            Opcode::CallIndirect | Opcode::TryCallIndirect
                        ));
                        let bounded = k
                            .checked_sub(adj)
                            .is_some_and(|ai| nocap.contains(&(i, ai as u32)))
                            && (!matches!(
                                data.opcode(),
                                Opcode::TryCall | Opcode::TryCallIndirect
                            ) || single_pred_dests(func, cfg, i));
                        for &(ss, _) in &w.slots {
                            if bounded {
                                let ss_ = sync_calls.entry(ss).or_default();
                                if !ss_.contains(&i) {
                                    ss_.push(i);
                                    if crate::env_set("PLIRON_SLOTPROM_WHY") {
                                        eprintln!("slotprom: bounded call {i} on {ss}");
                                    }
                                }
                            } else {
                                esc.push(ss);
                            }
                        }
                    }
                    // Branch args are benign: the receiving param is
                    // itself a web member, so its own uses classify the
                    // slot's fate.
                    _ if k >= nargs => {}
                    _ => esc.extend(w.slots.iter().map(|&(s, _)| s)),
                }
                for ss in esc {
                    if crate::env_set("PLIRON_SLOTPROM_WHY") && !bad.contains(&ss) {
                        eprintln!(
                            "slotprom: {ss} escaped at {i} {:?} ({})",
                            data.opcode(),
                            func.name
                        );
                    }
                    bad.insert(ss);
                }
            }
        }
    }
    for (ss, list) in &accs {
        if list.len() > MAX_ACCS {
            bad.insert(*ss);
        }
    }
    if timer {
        eprintln!(
            "slotprom: accs scan {:?} ({} poisoned={})",
            t1.elapsed(),
            func.name,
            webs.poisoned()
        );
    }
    let t2 = std::time::Instant::now();
    if webs.poisoned() {
        return 0;
    }
    accs.retain(|s, _| !bad.contains(s));
    if accs.is_empty() {
        return 0;
    }

    // Split each slot's accesses into disjoint, uniformly-typed fields.
    // A byte range partially overlapped by another access stays on memory
    // (its bytes are still read/written through the slot), as do ranges
    // touched by narrow ops or with mixed types.
    let mut fields: Vec<Field> = Vec::new();
    for (slot, list) in accs {
        let mut ranges: Vec<(i64, i64)> = Vec::new();
        for a in &list {
            let rr = (a.off, a.bytes);
            if !ranges.contains(&rr) {
                ranges.push(rr);
            }
        }
        for &(off, bytes) in &ranges {
            if ranges
                .iter()
                .any(|&(o2, b2)| (o2, b2) != (off, bytes) && o2 < off + bytes && off < o2 + b2)
            {
                continue;
            }
            let mut ty = None;
            let mut good = true;
            for a in list.iter().filter(|a| a.off == off && a.bytes == bytes) {
                match (ty, a.ty) {
                    (None, Some(t)) => ty = Some(t),
                    (Some(t), Some(t2)) if t == t2 => {}
                    _ => {
                        good = false;
                        break;
                    }
                }
            }
            if good && let Some(ty) = ty {
                fields.push(Field {
                    slot,
                    off,
                    bytes,
                    ty,
                });
            }
        }
    }
    if fields.is_empty() || fields.len() > MAX_FIELDS {
        return 0;
    }

    let mut blk_ix: FxHashMap<Block, u32> = FxHashMap::default();
    let mut blocks_vec: Vec<Block> = Vec::new();
    for (ix, b) in func.layout.blocks().enumerate() {
        blk_ix.insert(b, ix as u32);
        blocks_vec.push(b);
    }

    // Bounded calls read the slot's memory, so each counts as a use of
    // every field of its slot: without it a store-only field has an empty
    // livein, loses its store, and is never written back before the call.
    let sync_insts: FxHashMap<StackSlot, FxHashSet<Inst>> = sync_calls
        .iter()
        .map(|(s, v)| (*s, v.iter().copied().collect()))
        .collect();

    // Each access inst's slot site — field-invariant, so it is computed
    // once here rather than per field inside the livein scan.
    let mut sites: FxHashMap<Inst, (Option<(StackSlot, i64)>, bool)> =
        FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let s = match func.dfg.insts[i] {
                InstructionData::Load {
                    opcode: Opcode::Load,
                    arg,
                    offset,
                    ..
                } => Some((site(&webs, func, i, b, arg, offset), false)),
                InstructionData::Store {
                    opcode: Opcode::Store,
                    args,
                    offset,
                    ..
                } => Some((site(&webs, func, i, b, args[1], offset), true)),
                _ => None,
            };
            if let Some(s) = s {
                sites.insert(i, s);
            }
        }
    }

    // (slot, off) → field index and inst → its synced slots: the ue/def
    // scan below walks each inst once for *all* fields rather than once
    // per field.
    let field_ix: FxHashMap<(StackSlot, i64), usize> = fields
        .iter()
        .enumerate()
        .map(|(fi, f)| ((f.slot, f.off), fi))
        .collect();
    let mut slot_fields: FxHashMap<StackSlot, Vec<usize>> = FxHashMap::default();
    for (fi, f) in fields.iter().enumerate() {
        slot_fields.entry(f.slot).or_default().push(fi);
    }
    let mut sync_inst_slots: FxHashMap<Inst, Vec<StackSlot>> = FxHashMap::default();
    for (slot, insts) in &sync_insts {
        for &i in insts {
            sync_inst_slots.entry(i).or_default().push(*slot);
        }
    }

    // Per field: upward-exposed use (ue) and any-store (def) per block,
    // then livein[b] = ue[b] | (liveout[b] & !def[b]) to a fixpoint.
    let mut ues: Vec<Vec<bool>> = vec![vec![false; nblocks]; fields.len()];
    let mut defs: Vec<Vec<bool>> = vec![vec![false; nblocks]; fields.len()];
    let mut stored: Vec<bool> = vec![false; fields.len()];
    for b in func.layout.blocks() {
        let ix = blk_ix[&b] as usize;
        stored.fill(false);
        for i in func.layout.block_insts(b) {
            if let Some(slots) = sync_inst_slots.get(&i) {
                for &ss in slots {
                    if let Some(fis) = slot_fields.get(&ss) {
                        for &fi in fis {
                            if !stored[fi] {
                                ues[fi][ix] = true;
                            }
                        }
                    }
                }
            }
            let Some(&(Some((ss, off)), st)) = sites.get(&i) else {
                continue;
            };
            let Some(&fi) = field_ix.get(&(ss, off)) else {
                continue;
            };
            if st {
                stored[fi] = true;
                defs[fi][ix] = true;
            } else if !stored[fi] {
                ues[fi][ix] = true;
            }
        }
    }
    let mut liveins: Vec<Vec<bool>> = Vec::new();
    let mut kept: Vec<usize> = Vec::new();
    for (fi, f) in fields.iter().enumerate() {
        let ue = std::mem::take(&mut ues[fi]);
        let def = &defs[fi];
        // Backward-propagation worklist: a block is live-in when any
        // successor is, unless it stores the field itself. One visit per
        // edge — a chain CFG would otherwise cost a full block scan per
        // fixpoint round (O(blocks²) per field).
        let mut livein = ue;
        let mut wl: Vec<u32> = Vec::new();
        for (ix, &x) in livein.iter().enumerate() {
            if x {
                wl.push(ix as u32);
            }
        }
        while let Some(six) = wl.pop() {
            for p in cfg.pred_iter(blocks_vec[six as usize]) {
                let p = p.block;
                let Some(&pix) = blk_ix.get(&p) else {
                    continue;
                };
                if !reach.contains(&p) || livein[pix as usize] || def[pix as usize] {
                    continue;
                }
                livein[pix as usize] = true;
                wl.push(pix);
            }
        }
        // A synced field must round-trip through a store offset imm —
        // one that doesn't fit stays on memory (sound: the callee just
        // sees it there like today).
        let fits = !sync_calls.contains_key(&f.slot) || i32::try_from(f.off).is_ok();
        if fits && livein.iter().filter(|&&x| x).count() <= MAX_LIVEIN {
            kept.push(fi);
            liveins.push(livein);
        }
    }
    if timer {
        eprintln!(
            "slotprom: liveins {:?} ({} fields)",
            t2.elapsed(),
            fields.len()
        );
    }
    let t3 = std::time::Instant::now();
    if kept.is_empty() {
        return 0;
    }

    // Per bounded call: the kept-field indices written back before it,
    // and for try_call terminators the single-pred destination blocks
    // whose tops get the reloads (normal + exception edges alike).
    let mut sync: FxHashMap<Inst, Vec<usize>> = FxHashMap::default();
    let mut reload_top: FxHashMap<Block, Vec<usize>> = FxHashMap::default();
    for (slot, insts) in &sync_calls {
        let kis: Vec<usize> = kept
            .iter()
            .enumerate()
            .filter(|&(_, &fi)| fields[fi].slot == *slot)
            .map(|(ki, _)| ki)
            .collect();
        if kis.is_empty() {
            continue;
        }
        for &i in insts {
            // A call can take several promoted slots — accumulate the
            // field sets; a plain `insert` would drop all but the last.
            sync.entry(i).or_default().extend(kis.iter().copied());
            if matches!(
                func.dfg.insts[i].opcode(),
                Opcode::TryCall | Opcode::TryCallIndirect
            ) {
                let dfg = &func.dfg;
                for bc in
                    dfg.insts[i].branch_destination(&dfg.jump_tables, &dfg.exception_tables)
                {
                    reload_top
                        .entry(bc.block(&dfg.value_lists))
                        .or_default()
                        .extend(kis.iter().copied());
                }
            }
        }
    }

    // New params: every reachable livein block gets one per kept field
    // (entry gets a materialized undef instead — it has no preds).
    let mut params: Vec<FxHashMap<u32, Value>> = vec![FxHashMap::default(); kept.len()];
    for (ki, &fi) in kept.iter().enumerate() {
        for b in func.layout.blocks().collect::<Vec<_>>() {
            let ix = blk_ix[&b] as usize;
            if !liveins[ki][ix] {
                continue;
            }
            let v = if b == entry {
                undef(func, b, fields[fi].ty)
            } else {
                func.dfg.append_block_param(b, fields[fi].ty)
            };
            params[ki].insert(ix as u32, v);
        }
    }

    // A per-field undef defined in `entry` (dominates everything
    // reachable): the reaching value for upward-exposed loads with no
    // param and for edges out of blocks that never defined the field.
    let fallback: Vec<Value> = kept
        .iter()
        .map(|&fi| undef(func, entry, fields[fi].ty))
        .collect();

    // field index → kept index: the per-inst site check can't afford
    // linear `position` scans.
    let mut kept_ix: FxHashMap<usize, usize> = FxHashMap::default();
    for (ki, &fi) in kept.iter().enumerate() {
        kept_ix.insert(fi, ki);
    }
    if timer {
        eprintln!("slotprom: plan {:?}", t3.elapsed());
    }
    let t4 = std::time::Instant::now();

    // Rewrite: alias loads, drop stores, append reaching values on edges.
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        if !reach.contains(&b) {
            continue;
        }
        let bix = blk_ix[&b] as usize;
        let mut cur: Vec<Option<Value>> = vec![None; kept.len()];
        for (ki, _) in kept.iter().enumerate() {
            if liveins[ki][bix] {
                cur[ki] = params[ki].get(&(bix as u32)).copied();
            }
        }
        // Snapshot before emitting sync insts — the inserted
        // loads/stores must not be re-walked as promotable accesses.
        let insts = func.layout.block_insts(b).collect::<Vec<_>>();
        // Post-call reloads: this block is a try_call's return
        // trampoline — the callee may have rewritten the synced bytes.
        // Runs unconditionally so a later sync writeback can't push a
        // stale value over the callee's writes.
        if let Some(kis) = reload_top.get(&b) {
            let mut pos = FuncCursor::new(func).at_first_insertion_point(b);
            let mut sas: FxHashMap<StackSlot, Value> = FxHashMap::default();
            for &ki in kis {
                let f = fields[kept[ki]];
                let sa = *sas
                    .entry(f.slot)
                    .or_insert_with(|| pos.ins().stack_addr(types::I64, f.slot, 0));
                cur[ki] = Some(pos.ins().load(
                    f.ty,
                    MemFlagsData::new().with_notrap(),
                    sa,
                    f.off as i32,
                ));
            }
        }
        for i in insts {
            if let Some(kis) = sync.get(&i) {
                // Writeback before the call: the callee reads the
                // operand's memory, so promoted fields become stores.
                // (cur=None ⇒ the field's bytes are untouched on this
                // path — nothing to push.)
                let mut sas: FxHashMap<StackSlot, Value> = FxHashMap::default();
                {
                    let mut pos = FuncCursor::new(func).at_inst(i);
                    for &ki in kis {
                        let f = fields[kept[ki]];
                        let sa = *sas
                            .entry(f.slot)
                            .or_insert_with(|| pos.ins().stack_addr(types::I64, f.slot, 0));
                        if let Some(v) = cur[ki] {
                            pos.ins().store(
                                MemFlagsData::new().with_notrap(),
                                v,
                                sa,
                                f.off as i32,
                            );
                        }
                    }
                }
                // call/call_indirect fall through in this block — reload
                // what the callee may have written. try_call reloads
                // live at its destination trampolines instead.
                if matches!(
                    func.dfg.insts[i].opcode(),
                    Opcode::Call | Opcode::CallIndirect
                ) {
                    let mut pos = FuncCursor::new(func).after_inst(i);
                    for &ki in kis {
                        let f = fields[kept[ki]];
                        let sa = *sas
                            .entry(f.slot)
                            .or_insert_with(|| pos.ins().stack_addr(types::I64, f.slot, 0));
                        cur[ki] = Some(pos.ins().load(
                            f.ty,
                            MemFlagsData::new().with_notrap(),
                            sa,
                            f.off as i32,
                        ));
                    }
                }
            }
            match func.dfg.insts[i] {
                InstructionData::Load {
                    opcode: Opcode::Load, ..
                } => {
                    let ty = func.dfg.value_type(func.dfg.first_result(i));
                    if let Some(&(Some((ss, off)), _)) = sites.get(&i)
                        && let Some(&fi) = field_ix.get(&(ss, off))
                        && fields[fi].bytes == i64::from(ty.bytes())
                        && fields[fi].ty == ty
                        && let Some(&ki) = kept_ix.get(&fi)
                    {
                        let cv = cur[ki].unwrap_or(fallback[ki]);
                        let res = func.dfg.first_result(i);
                        func.layout.remove_inst(i);
                        func.dfg.clear_results(i);
                        func.dfg.change_to_alias(res, cv);
                        n += 1;
                    }
                }
                InstructionData::Store {
                    opcode: Opcode::Store,
                    args,
                    ..
                } => {
                    let ty = func.dfg.value_type(args[0]);
                    if let Some(&(Some((ss, off)), _)) = sites.get(&i)
                        && let Some(&fi) = field_ix.get(&(ss, off))
                        && fields[fi].bytes == i64::from(ty.bytes())
                        && fields[fi].ty == ty
                        && let Some(&ki) = kept_ix.get(&fi)
                    {
                        cur[ki] = Some(func.dfg.resolve_aliases(args[0]));
                        func.layout.remove_inst(i);
                        n += 1;
                    }
                }
                _ => {}
            }
        }
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        // Mutate the terminator's stored block-calls in place: copies
        // would leave the original EntityList pointing at stale pool
        // storage if `append_argument` has to grow the list.
        // First resolve which destinations need which extra args while the
        // value list pool is only borrowed immutably.
        let adds: Vec<Vec<Value>> = {
            let dfg = &func.dfg;
            dfg.insts[term]
                .branch_destination(&dfg.jump_tables, &dfg.exception_tables)
                .iter()
                .map(|bc| {
                    let d = bc.block(&dfg.value_lists);
                    let Some(&dix) = blk_ix.get(&d) else {
                        return Vec::new();
                    };
                    if !reach.contains(&d) {
                        return Vec::new();
                    }
                    kept.iter()
                        .enumerate()
                        .filter(|(ki, _)| liveins[*ki][dix as usize])
                        .map(|(ki, _)| cur[ki].unwrap_or(fallback[ki]))
                        .collect()
                })
                .collect()
        };
        let dfg = &mut func.dfg;
        for (bc, vs) in dfg.insts[term]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
            .iter_mut()
            .zip(adds)
        {
            for v in vs {
                bc.append_argument(BlockArg::Value(v), &mut dfg.value_lists);
            }
        }
    }
    if timer {
        eprintln!("slotprom: rewrite {:?}", t4.elapsed());
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::{
        AbiParam, MemFlagsData, Signature, StackSlotData, StackSlotKind, UserFuncName,
    };
    use cranelift_codegen::isa::CallConv;

    fn verify(func: &Function) {
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(func, &flags) {
            panic!("slotprom produced invalid IR: {e}\n{}", func.display());
        }
    }


    fn empty_nocap() -> FxHashSet<(Inst, u32)> {
        FxHashSet::default()
    }

    fn memops(func: &Function) -> usize {
        let mut n = 0;
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                match func.dfg.insts[i] {
                    InstructionData::Load { .. } | InstructionData::Store { .. } => n += 1,
                    _ => {}
                }
            }
        }
        n
    }

    /// A loop-carried i64 cell in an 8-byte explicit slot: init in the
    /// entry block, load+store in the loop header, read on exit. After
    /// promotion no plain load/store remains and the blocks that need
    /// the value carry it as a param.
    #[test]
    fn promotes_loop_carried_field() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let h = func.dfg.make_block();
        let body = func.dfg.make_block();
        let done = func.dfg.make_block();
        for b in [b0, h, body, done] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let arg0 = func.dfg.append_block_param(b0, types::I64);
        let flags = MemFlagsData::new();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a0 = cur.ins().stack_addr(types::I64, ss, 0);
        cur.ins().store(flags, arg0, a0, 0);
        cur.ins().jump(h, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(h);
        let x = cur.ins().load(types::I64, flags, a0, 0);
        let one = cur.ins().iconst(types::I64, 1);
        let more = cur.ins().icmp(IntCC::UnsignedGreaterThan, x, one);
        cur.ins().brif(more, body, &[], done, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(body);
        let x2 = cur.ins().load(types::I64, flags, a0, 0);
        let one2 = cur.ins().iconst(types::I64, 1);
        let sub = cur.ins().isub(x2, one2);
        cur.ins().store(flags, sub, a0, 0);
        cur.ins().jump(h, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(done);
        let r = cur.ins().load(types::I64, flags, a0, 0);
        cur.ins().return_(&[r]);

        let cfg = ControlFlowGraph::with_function(&func);
        let n = run(&mut func, &cfg, &empty_nocap());
        assert!(n > 0, "expected promotion");
        assert_eq!(memops(&func), 0, "no plain load/store should remain");
        assert!(func.dfg.num_block_params(h) >= 1);
        assert!(func.dfg.num_block_params(done) >= 1);
        verify(&func);
    }

    /// Two disjoint fields (i64 at +0, i32 at +8) promote independently.
    #[test]
    fn promotes_disjoint_fields() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let done = func.dfg.make_block();
        for b in [b0, done] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 4));
        let flags = MemFlagsData::new();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        let five = cur.ins().iconst(types::I64, 5);
        let nine = cur.ins().iconst(types::I32, 9);
        cur.ins().store(flags, five, a, 0);
        cur.ins().store(flags, nine, a, 8);
        cur.ins().jump(done, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(done);
        let w = cur.ins().load(types::I64, flags, a, 0);
        let n = cur.ins().load(types::I32, flags, a, 8);
        let nw = cur.ins().uextend(types::I64, n);
        let r = cur.ins().iadd(w, nw);
        cur.ins().return_(&[r]);

        let cfg = ControlFlowGraph::with_function(&func);
        let n = run(&mut func, &cfg, &empty_nocap());
        assert!(n >= 4, "two stores + two loads should fold, got {n}");
        assert_eq!(memops(&func), 0);
        verify(&func);
    }

    /// An escaping address (passed to a call) keeps the slot on memory.
    #[test]
    fn escaped_slot_is_left_alone() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig.clone());
        let esig = func.import_signature(sig.clone());
        let ext = func.dfg.ext_funcs.push(cranelift_codegen::ir::ExtFuncData {
            name: cranelift_codegen::ir::ExternalName::user(Default::default()),
            signature: esig,
            colocated: false,
            patchable: false,
        });
        let b0 = func.dfg.make_block();
        for b in [b0] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let arg0 = func.dfg.append_block_param(b0, types::I64);
        let flags = MemFlagsData::new();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        cur.ins().store(flags, arg0, a, 0);
        cur.ins().call(ext, &[a]);
        let r = cur.ins().load(types::I64, flags, a, 0);
        cur.ins().return_(&[r]);

        let cfg = ControlFlowGraph::with_function(&func);
        assert_eq!(run(&mut func, &cfg, &empty_nocap()), 0);
        assert_eq!(memops(&func), 2);
        verify(&func);
    }

    /// A slot whose only escape is a `nocap` call operand still
    /// promotes: writeback before the call, reload after it, and no
    /// other memory ops survive.
    #[test]
    fn nocap_call_promotes_with_sync() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig.clone());
        let esig = func.import_signature(sig.clone());
        let ext = func.dfg.ext_funcs.push(cranelift_codegen::ir::ExtFuncData {
            name: cranelift_codegen::ir::ExternalName::user(Default::default()),
            signature: esig,
            colocated: false,
            patchable: false,
        });
        let b0 = func.dfg.make_block();
        func.layout.append_block(b0);
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let arg0 = func.dfg.append_block_param(b0, types::I64);
        let flags = MemFlagsData::new();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        cur.ins().store(flags, arg0, a, 0);
        let call = cur.ins().call(ext, &[a]);
        let r = cur.ins().load(types::I64, flags, a, 0);
        cur.ins().return_(&[r]);

        let mut nocap = empty_nocap();
        nocap.insert((call, 0));
        let cfg = ControlFlowGraph::with_function(&func);
        let n = run(&mut func, &cfg, &nocap);
        assert!(n > 0, "expected promotion");
        // Only the sync pair (writeback store + reload) remains.
        assert_eq!(memops(&func), 2);
        verify(&func);
    }

    /// A field stored on a predecessor path and read only through a
    /// bounded call's memory still has to reach the call: with no SSA
    /// load there is no live-in param, but the callee reads the bytes, so
    /// the writeback must re-materialize the reaching value. (rustc's
    /// drop-glue chains: a copied struct's field is dropped through a
    /// call, never re-loaded.)
    #[test]
    fn store_only_field_syncs_at_bounded_call() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig.clone());
        let esig = func.import_signature(sig.clone());
        let ext = func.dfg.ext_funcs.push(cranelift_codegen::ir::ExtFuncData {
            name: cranelift_codegen::ir::ExternalName::user(Default::default()),
            signature: esig,
            colocated: false,
            patchable: false,
        });
        let b0 = func.dfg.make_block();
        let b1 = func.dfg.make_block();
        for b in [b0, b1] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let arg0 = func.dfg.append_block_param(b0, types::I64);
        let flags = MemFlagsData::new();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        cur.ins().store(flags, arg0, a, 0);
        cur.ins().jump(b1, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(b1);
        let call = cur.ins().call(ext, &[a]);
        let z = cur.ins().iconst(types::I64, 0);
        cur.ins().return_(&[z]);

        let mut nocap = empty_nocap();
        nocap.insert((call, 0));
        let cfg = ControlFlowGraph::with_function(&func);
        run(&mut func, &cfg, &nocap);
        // The callee must see arg0 in *a: writeback store + post-call
        // reload — if the store is silently dropped the callee reads
        // uninitialized slot memory.
        assert_eq!(memops(&func), 2, "missing writeback/reload pair");
        verify(&func);
    }

    /// One call can take several promoted slots as bounded args: the
    /// writeback set is keyed by call inst, so both slots' stores must
    /// land before it — not just the last one iterated.
    #[test]
    fn two_bounded_args_both_sync() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig.clone());
        let esig = func.import_signature(sig.clone());
        let ext = func.dfg.ext_funcs.push(cranelift_codegen::ir::ExtFuncData {
            name: cranelift_codegen::ir::ExternalName::user(Default::default()),
            signature: esig,
            colocated: false,
            patchable: false,
        });
        let b0 = func.dfg.make_block();
        let b1 = func.dfg.make_block();
        for b in [b0, b1] {
            func.layout.append_block(b);
        }
        let ss0 =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let ss1 =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let arg0 = func.dfg.append_block_param(b0, types::I64);
        let arg1 = func.dfg.append_block_param(b0, types::I64);
        let flags = MemFlagsData::new();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a0 = cur.ins().stack_addr(types::I64, ss0, 0);
        let a1 = cur.ins().stack_addr(types::I64, ss1, 0);
        cur.ins().store(flags, arg0, a0, 0);
        cur.ins().store(flags, arg1, a1, 0);
        cur.ins().jump(b1, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(b1);
        let call = cur.ins().call(ext, &[a0, a1]);
        let z = cur.ins().iconst(types::I64, 0);
        cur.ins().return_(&[z]);

        let mut nocap = empty_nocap();
        nocap.insert((call, 0));
        nocap.insert((call, 1));
        let cfg = ControlFlowGraph::with_function(&func);
        run(&mut func, &cfg, &nocap);
        // Each arg's bytes reach the callee through memory: two
        // writebacks before the call, two reloads after.
        assert_eq!(memops(&func), 4, "missing writeback/reload pairs");
        verify(&func);
    }

    /// The same call without the `nocap` mark is a disqualifying escape.
    #[test]
    fn unmarked_call_does_not_promote() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig.clone());
        let esig = func.import_signature(sig.clone());
        let ext = func.dfg.ext_funcs.push(cranelift_codegen::ir::ExtFuncData {
            name: cranelift_codegen::ir::ExternalName::user(Default::default()),
            signature: esig,
            colocated: false,
            patchable: false,
        });
        let b0 = func.dfg.make_block();
        func.layout.append_block(b0);
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let arg0 = func.dfg.append_block_param(b0, types::I64);
        let flags = MemFlagsData::new();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        cur.ins().store(flags, arg0, a, 0);
        cur.ins().call(ext, &[a]);
        let r = cur.ins().load(types::I64, flags, a, 0);
        cur.ins().return_(&[r]);

        let cfg = ControlFlowGraph::with_function(&func);
        assert_eq!(run(&mut func, &cfg, &empty_nocap()), 0);
        verify(&func);
    }

    /// A field whose bytes a narrower op also touches stays on memory:
    /// `store.i32` at +8 plus `uload8` at +8 must not promote the i32
    /// away underneath the byte read.
    #[test]
    fn narrow_access_pins_field() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let done = func.dfg.make_block();
        for b in [b0, done] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 4));
        let flags = MemFlagsData::new();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        let five = cur.ins().iconst(types::I64, 5);
        let nine = cur.ins().iconst(types::I32, 9);
        cur.ins().store(flags, five, a, 0);
        cur.ins().store(flags, nine, a, 8);
        cur.ins().jump(done, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(done);
        let byte = cur.ins().uload8(types::I32, flags, a, 8);
        let w = cur.ins().uextend(types::I64, byte);
        cur.ins().return_(&[w]);

        let cfg = ControlFlowGraph::with_function(&func);
        run(&mut func, &cfg, &empty_nocap());
        // The i32 field overlaps the byte load; it must stay in memory
        // (its store + the uload8 itself = 2 memory ops). The i64 field
        // at +0 is disjoint and promotes away its store.
        assert_eq!(memops(&func), 2);
        verify(&func);
    }

    /// hashbrown's lazy-empty idiom: a param fed `&slot` on one edge and
    /// `0` on the other, dereferenced only under the `p != 0` arm. Edge
    /// restriction drops the null source, so the load promotes.
    #[test]
    fn nullable_param_guarded_load_promotes() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let join = func.dfg.make_block();
        let useb = func.dfg.make_block();
        let nullb = func.dfg.make_block();
        for b in [b0, join, useb, nullb] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let x = func.dfg.append_block_param(b0, types::I64);
        let p = func.dfg.append_block_param(join, types::I64);
        let flags = MemFlagsData::new();
        let nt = MemFlagsData::new().with_notrap();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        cur.ins().store(flags, x, a, 0);
        let z = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, x, z);
        cur.ins().brif(c, join, &[z.into()], join, &[a.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(join);
        let z2 = cur.ins().iconst(types::I64, 0);
        let t = cur.ins().icmp(IntCC::Equal, p, z2);
        cur.ins().brif(t, nullb, &[], useb, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(useb);
        let r = cur.ins().load(types::I64, nt, p, 0);
        cur.ins().return_(&[r]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(nullb);
        let z3 = cur.ins().iconst(types::I64, 0);
        cur.ins().return_(&[z3]);

        let cfg = ControlFlowGraph::with_function(&func);
        let n = run(&mut func, &cfg, &empty_nocap());
        assert!(n > 0, "guarded load through nullable param should promote");
        assert_eq!(memops(&func), 0);
        verify(&func);
    }

    /// A sibling param carrying `&slot` on the *null* edge and a heap
    /// pointer on the non-null edge may be stored to memory on the
    /// non-null arm: edge correlation proves the stored value can't be
    /// the slot there, so the slot still promotes.
    #[test]
    fn correlated_sibling_store_is_safe() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let join = func.dfg.make_block();
        let useb = func.dfg.make_block();
        let nullb = func.dfg.make_block();
        for b in [b0, join, useb, nullb] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let x = func.dfg.append_block_param(b0, types::I64);
        let hp = func.dfg.append_block_param(b0, types::I64);
        let p = func.dfg.append_block_param(join, types::I64);
        let q = func.dfg.append_block_param(join, types::I64);
        let flags = MemFlagsData::new();
        let nt = MemFlagsData::new().with_notrap();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        cur.ins().store(flags, x, a, 0);
        let z = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, x, z);
        // Null edge binds (0, &slot); non-null edge binds (&slot, heap).
        cur.ins().brif(c, join, &[z.into(), a.into()], join, &[a.into(), hp.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(join);
        let z2 = cur.ins().iconst(types::I64, 0);
        let t = cur.ins().icmp(IntCC::Equal, p, z2);
        cur.ins().brif(t, nullb, &[], useb, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(useb);
        cur.ins().store(nt, q, hp, 0);
        let r = cur.ins().load(types::I64, nt, p, 0);
        cur.ins().return_(&[r]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(nullb);
        let z3 = cur.ins().iconst(types::I64, 0);
        cur.ins().return_(&[z3]);

        let cfg = ControlFlowGraph::with_function(&func);
        let n = run(&mut func, &cfg, &empty_nocap());
        assert!(n > 0, "correlated sibling store must not escape the slot");
        // Only the real heap store remains.
        assert_eq!(memops(&func), 1);
        verify(&func);
    }

    /// Same web without the correlation: the stored param is `&slot` on
    /// the *non-null* edge too, so the store genuinely escapes.
    #[test]
    fn uncorrelated_sibling_store_escapes() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let join = func.dfg.make_block();
        let useb = func.dfg.make_block();
        let nullb = func.dfg.make_block();
        for b in [b0, join, useb, nullb] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let x = func.dfg.append_block_param(b0, types::I64);
        let hp = func.dfg.append_block_param(b0, types::I64);
        let p = func.dfg.append_block_param(join, types::I64);
        let q = func.dfg.append_block_param(join, types::I64);
        let flags = MemFlagsData::new();
        let nt = MemFlagsData::new().with_notrap();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        cur.ins().store(flags, x, a, 0);
        let z = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, x, z);
        cur.ins().brif(c, join, &[z.into(), a.into()], join, &[a.into(), a.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(join);
        let z2 = cur.ins().iconst(types::I64, 0);
        let t = cur.ins().icmp(IntCC::Equal, p, z2);
        cur.ins().brif(t, nullb, &[], useb, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(useb);
        // `q` may be `&slot` on the non-null edge — storing it into an
        // unrelated object escapes.
        cur.ins().store(nt, q, hp, 0);
        let r = cur.ins().load(types::I64, nt, p, 0);
        cur.ins().return_(&[r]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(nullb);
        let z3 = cur.ins().iconst(types::I64, 0);
        cur.ins().return_(&[z3]);

        let cfg = ControlFlowGraph::with_function(&func);
        assert_eq!(run(&mut func, &cfg, &empty_nocap()), 0);
        verify(&func);
    }

    /// `to_exact_fixed_str` shape: four i64 fields are stored in one
    /// block, then the slot address is passed (nocap) to a call in a
    /// later join block that already carries its own params. The stored
    /// values must be written back before the call even though the call
    /// block's param list is not empty.
    #[test]
    fn writeback_through_join_with_params() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig.clone());
        let un = func.declare_imported_user_function(
            cranelift_codegen::ir::UserExternalName::new(0, 0),
        );
        let mut csig = Signature::new(CallConv::Fast);
        csig.params.push(AbiParam::new(types::I64));
        csig.params.push(AbiParam::new(types::I64));
        csig.returns.push(AbiParam::new(types::I64));
        let esig = func.import_signature(csig);
        let ext = func.dfg.ext_funcs.push(cranelift_codegen::ir::ExtFuncData {
            name: cranelift_codegen::ir::ExternalName::user(un),
            signature: esig,
            colocated: false,
            patchable: false,
        });
        let b0 = func.dfg.make_block();
        let left = func.dfg.make_block();
        let right = func.dfg.make_block();
        let join = func.dfg.make_block();
        let callb = func.dfg.make_block();
        for b in [b0, left, right, join, callb] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 32, 3));
        let x = func.dfg.append_block_param(b0, types::I64);
        let jp = func.dfg.append_block_param(join, types::I64);
        let flags = MemFlagsData::new();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        cur.ins().store(flags, x, a, 0);
        cur.ins().store(flags, x, a, 8);
        cur.ins().store(flags, x, a, 16);
        cur.ins().store(flags, x, a, 24);
        let z = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, x, z);
        cur.ins().brif(c, left, &[], right, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(left);
        cur.ins().jump(join, &[x.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(right);
        cur.ins().jump(join, &[z.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(join);
        cur.ins().jump(callb, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(callb);
        let call = cur.ins().call(ext, &[jp, a]);
        let r = cur.ins().iconst(types::I64, 0);
        cur.ins().return_(&[r]);

        let mut nocap = empty_nocap();
        nocap.insert((call, 1));
        let cfg = ControlFlowGraph::with_function(&func);
        run(&mut func, &cfg, &nocap);
        // The callee reads ss29 through `a`: 4 writeback stores before
        // the call plus 4 reloads after it — none may be dropped.
        assert_eq!(memops(&func), 8, "missing writeback/reload pairs:\n{func}");
        verify(&func);
    }

    /// Without `notrap` a load through a `{slot, 0}` param can't promote:
    /// the null edge isn't ruled out by anything else here (the guard
    /// tests a different value), so the field stays in memory.
    #[test]
    fn unguarded_nullable_load_stays() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let join = func.dfg.make_block();
        for b in [b0, join] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let x = func.dfg.append_block_param(b0, types::I64);
        let p = func.dfg.append_block_param(join, types::I64);
        let flags = MemFlagsData::new();

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        cur.ins().store(flags, x, a, 0);
        let z = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, x, z);
        cur.ins().brif(c, join, &[z.into()], join, &[a.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(join);
        let r = cur.ins().load(types::I64, flags, p, 0);
        cur.ins().return_(&[r]);

        let cfg = ControlFlowGraph::with_function(&func);
        assert_eq!(run(&mut func, &cfg, &empty_nocap()), 0);
        assert_eq!(memops(&func), 2);
        verify(&func);
    }
}
