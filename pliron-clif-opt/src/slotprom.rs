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
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, StackSlot, Type, Value,
    types,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::loadfwd::{self, Root, root};

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

/// The `j`th branch-arg use of `i` feeds a param that resolves to root `r`
/// — the address stays inside the tracked equivalence class.
fn feeds_same(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    empty: &FxHashSet<Value>,
    i: Inst,
    mut j: usize,
    r: Root,
) -> bool {
    for bc in
        func.dfg.insts[i].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
    {
        for (pos, a) in bc.args(&func.dfg.value_lists).enumerate() {
            if a.as_value().is_none() {
                continue;
            }
            if j == 0 {
                let dst = bc.block(&func.dfg.value_lists);
                let Some(&p) = func.dfg.block_params(dst).get(pos) else {
                    return false;
                };
                let p = loadfwd::psrc(ps, empty, func, p);
                return root(func, ps, empty, p).0 == r;
            }
            j -= 1;
        }
    }
    false
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

pub fn run(func: &mut Function, cfg: &ControlFlowGraph) -> usize {
    let Some(entry) = func.layout.entry_block() else {
        return 0;
    };
    let nblocks = func.layout.blocks().count();
    if func.sized_stack_slots.is_empty() || nblocks > MAX_BLOCKS {
        return 0;
    }
    let ps = loadfwd::param_src(func);
    let empty: FxHashSet<Value> = FxHashSet::default();

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
    for b in func.layout.blocks() {
        if !reach.contains(&b) {
            continue;
        }
        for i in func.layout.block_insts(b) {
            let nargs = func.dfg.inst_args(i).len();
            for (k, v) in func.dfg.inst_values(i).enumerate() {
                let v = func.dfg.resolve_aliases(v);
                let (r, off) = root(func, &ps, &empty, v);
                let Root::S(slot) = r else { continue };
                let data = &func.dfg.insts[i];
                let mut rec: Option<Acc> = None;
                let mut escapes = false;
                match data {
                    InstructionData::Load {
                        opcode,
                        arg,
                        offset,
                        ..
                    } if k < nargs && func.dfg.resolve_aliases(*arg) == v => {
                        let rty = func.dfg.value_type(func.dfg.first_result(i));
                        match op_bytes(*opcode, rty) {
                            Some((bytes, plain)) => {
                                rec = Some(Acc {
                                    off: off.wrapping_add(i64::from(i32::from(*offset))),
                                    bytes,
                                    ty: plain.then_some(rty),
                                });
                            }
                            None => escapes = true,
                        }
                    }
                    InstructionData::Store {
                        opcode,
                        args,
                        offset,
                        ..
                    } if k < nargs && func.dfg.resolve_aliases(args[1]) == v => {
                        let vty = func.dfg.value_type(args[0]);
                        match op_bytes(*opcode, vty) {
                            Some((bytes, plain)) => {
                                rec = Some(Acc {
                                    off: off.wrapping_add(i64::from(i32::from(*offset))),
                                    bytes,
                                    ty: plain.then_some(vty),
                                });
                            }
                            None => escapes = true,
                        }
                    }
                    InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        ..
                    } => {
                        // Benign only when the sum still resolves to this
                        // slot — a variable add is an untracked address.
                        let res = func.dfg.first_result(i);
                        if root(func, &ps, &empty, res).0 != r {
                            escapes = true;
                        }
                    }
                    _ if k >= nargs => {
                        escapes = !feeds_same(func, &ps, &empty, i, k - nargs, r);
                    }
                    _ => escapes = true,
                }
                if escapes {
                    bad.insert(slot);
                }
                if let Some(a) = rec {
                    accs.entry(slot).or_default().push(a);
                    if accs[&slot].len() > MAX_ACCS {
                        bad.insert(slot);
                    }
                }
            }
        }
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
    for (ix, b) in func.layout.blocks().enumerate() {
        blk_ix.insert(b, ix as u32);
    }

    // Per field: upward-exposed use (ue) and any-store (def) per block,
    // then livein[b] = ue[b] | (liveout[b] & !def[b]) to a fixpoint.
    let mut liveins: Vec<Vec<bool>> = Vec::new();
    let mut kept: Vec<usize> = Vec::new();
    for (fi, f) in fields.iter().enumerate() {
        let mut ue = vec![false; nblocks];
        let mut def = vec![false; nblocks];
        for b in func.layout.blocks() {
            let mut stored = false;
            for i in func.layout.block_insts(b) {
                let (hit, st) = match func.dfg.insts[i] {
                    InstructionData::Load {
                        opcode: Opcode::Load,
                        arg,
                        offset,
                        ..
                    } => {
                        let (r, o) = root(func, &ps, &empty, arg);
                        (
                            r == Root::S(f.slot)
                                && o.wrapping_add(i64::from(i32::from(offset))) == f.off,
                            false,
                        )
                    }
                    InstructionData::Store {
                        opcode: Opcode::Store,
                        args,
                        offset,
                        ..
                    } => {
                        let (r, o) = root(func, &ps, &empty, args[1]);
                        (
                            r == Root::S(f.slot)
                                && o.wrapping_add(i64::from(i32::from(offset))) == f.off,
                            true,
                        )
                    }
                    _ => (false, false),
                };
                if !hit {
                    continue;
                }
                let ix = blk_ix[&b] as usize;
                if st {
                    stored = true;
                    def[ix] = true;
                } else if !stored {
                    ue[ix] = true;
                }
            }
        }
        let mut livein = ue;
        loop {
            let mut changed = false;
            for b in func.layout.blocks() {
                let ix = blk_ix[&b] as usize;
                if livein[ix] || def[ix] || !reach.contains(&b) {
                    continue;
                }
                if cfg
                    .succ_iter(b)
                    .any(|s| blk_ix.get(&s).is_some_and(|&si| livein[si as usize]))
                {
                    livein[ix] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        if livein.iter().filter(|&&x| x).count() <= MAX_LIVEIN {
            kept.push(fi);
            liveins.push(livein);
        }
    }
    if kept.is_empty() {
        return 0;
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
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            match func.dfg.insts[i] {
                InstructionData::Load {
                    opcode: Opcode::Load,
                    arg,
                    offset,
                    ..
                } => {
                    let (r, off) = root(func, &ps, &empty, arg);
                    let off = off.wrapping_add(i64::from(i32::from(offset)));
                    let ty = func.dfg.value_type(func.dfg.first_result(i));
                    if let Some(fi) = fields.iter().position(|f| {
                        Root::S(f.slot) == r
                            && f.off == off
                            && f.bytes == i64::from(ty.bytes())
                            && f.ty == ty
                    }) && let Some(ki) = kept.iter().position(|&k| k == fi)
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
                    offset,
                    ..
                } => {
                    let (r, off) = root(func, &ps, &empty, args[1]);
                    let off = off.wrapping_add(i64::from(i32::from(offset)));
                    let ty = func.dfg.value_type(args[0]);
                    if let Some(fi) = fields.iter().position(|f| {
                        Root::S(f.slot) == r
                            && f.off == off
                            && f.bytes == i64::from(ty.bytes())
                            && f.ty == ty
                    }) && let Some(ki) = kept.iter().position(|&k| k == fi)
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
        let n = run(&mut func, &cfg);
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
        let n = run(&mut func, &cfg);
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
        assert_eq!(run(&mut func, &cfg), 0);
        assert_eq!(memops(&func), 2);
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
        run(&mut func, &cfg);
        // The i32 field overlaps the byte load; it must stay in memory
        // (its store + the uload8 itself = 2 memory ops). The i64 field
        // at +0 is disjoint and promotes away its store.
        assert_eq!(memops(&func), 2);
        verify(&func);
    }
}
