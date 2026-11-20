//! Loads of stack-slot bytes that constant stores in the same block just
//! wrote become those constants (`PLIRON_SLOTCONST=1`, opt-in).
//!
//! stdarch builds NEON constants through memory (`[0i8; 8]` stored byte by
//! byte, then reloaded as an `i8x8`), and `loadfwd` does not forward a
//! vector load from narrower scalar stores. hashbrown's group probe on
//! aarch64 paid eight `strb wzr` and an `ldr d` per probe in
//! html-rewriter's `ExecutionCtx` (`icmp.i8x8 slt x, <the reload>`).
//!
//! A forward scan of each block keeps, per stack slot byte, the constant
//! last stored to it. A plain `store` of an `iconst`/`f32const`/
//! `f64const`/`vconst` to `stack_addr ss + k` (through `iadd` of
//! constants and the store's own offset) records its little-endian bytes;
//! a store of anything else to the slot forgets the bytes it covers; a
//! store to an address that is not a slot, a call or any other
//! instruction with a memory side effect forgets everything. A `load`
//! from a slot whose every byte is known becomes the constant: `iconst`
//! for integers up to 64 bits, `f32const`/`f64const`, `vconst` for 64-
//! and 128-bit vectors. Linear in the block.
//!
//! `PLIRON_SLOTCOMB=1` (opt-in): the scan also remembers, per byte, which
//! non-constant integer store wrote it (plain `store` of an integer of at
//! most 8 bytes, or `istore8/16/32`), and an integer load whose bytes are
//! exactly covered by whole such pieces and known constants becomes their
//! combination, `const | uextend(p1) << 8*k1 | ...` (little-endian, as the
//! constant path). An enum assembled in a temporary and copied out as one
//! word: brotli's `nodes[pos].u = Union1::shortcut(x)` stored the payload
//! and the tag `2` to the stack and reloaded both as an `i64` on every
//! position (240k stores and 120k loads in compression); the stores then die
//! in `dse` and the slot in `deadslot`. Only for a slot read by that one
//! load and whose address escapes nowhere (`single_read_slots`): otherwise
//! the stores stay and the combination is not worth its shifts.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::immediates::{Ieee32, Ieee64};
use cranelift_codegen::ir::{
    Function, InstBuilder, InstructionData, Opcode, StackSlot, Type, Value, types,
};
use rustc_hash::FxHashMap;

/// `(slot, byte offset)` of an address built from `stack_addr` and
/// constant adds, within a small depth.
fn slot_addr(func: &Function, v: Value, off: i64) -> Option<(StackSlot, i64)> {
    let mut v = func.dfg.resolve_aliases(v);
    let mut off = off;
    for _ in 0..8 {
        let i = func.dfg.value_def(v).inst()?;
        match func.dfg.insts[i] {
            InstructionData::StackAddr { opcode: Opcode::StackAddr, stack_slot, offset } => {
                return Some((stack_slot, off + i64::from(i32::from(offset))));
            }
            InstructionData::Binary { opcode: Opcode::Iadd, args } => {
                let (a, b) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
                if let Some(k) = iconst(func, b) {
                    off += k;
                    v = a;
                } else if let Some(k) = iconst(func, a) {
                    off += k;
                    v = b;
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

/// Little-endian bytes of a constant `v` of its type, if it is one.
fn const_bytes(func: &Function, v: Value) -> Option<Vec<u8>> {
    let v = func.dfg.resolve_aliases(v);
    let ty = func.dfg.value_type(v);
    let i = func.dfg.value_def(v).inst()?;
    let n = ty.bytes() as usize;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } if ty.is_int() && n <= 8 => {
            Some(imm.bits().to_le_bytes()[..n].to_vec())
        }
        InstructionData::UnaryIeee32 { opcode: Opcode::F32const, imm } => {
            Some(imm.bits().to_le_bytes().to_vec())
        }
        InstructionData::UnaryIeee64 { opcode: Opcode::F64const, imm } => {
            Some(imm.bits().to_le_bytes().to_vec())
        }
        InstructionData::UnaryConst { opcode: Opcode::Vconst, constant_handle } => {
            let d = func.dfg.constants.get(constant_handle).as_slice().to_vec();
            (d.len() == n).then_some(d)
        }
        _ => None,
    }
}

fn materialize(pos: &mut FuncCursor, inst: cranelift_codegen::ir::Inst, ty: Type, b: &[u8]) -> bool {
    let mut w = [0u8; 8];
    if ty.is_int() && b.len() <= 8 {
        w[..b.len()].copy_from_slice(b);
        let mut k = i64::from_le_bytes(w);
        // `iconst` immediates are sign-agnostic but must fit the type:
        // keep the low bits only.
        if ty.bits() < 64 {
            k &= (1i64 << ty.bits()) - 1;
        }
        pos.func.replace(inst).iconst(ty, k);
        return true;
    }
    if ty == types::F32 {
        let k = u32::from_le_bytes(b[..4].try_into().unwrap());
        pos.func.replace(inst).f32const(Ieee32::with_bits(k));
        return true;
    }
    if ty == types::F64 {
        let k = u64::from_le_bytes(b[..8].try_into().unwrap());
        pos.func.replace(inst).f64const(Ieee64::with_bits(k));
        return true;
    }
    if ty.is_vector() && matches!(ty.bytes(), 8 | 16) {
        let h = pos.func.dfg.constants.insert(b.to_vec().into());
        pos.func.replace(inst).vconst(ty, h);
        return true;
    }
    false
}

/// `PLIRON_SLOTCOMB`: the slots read by exactly one load whose address
/// reaches nothing but load and store addresses (through constant adds).
/// Combining that load lets `dse` drop the stores and `deadslot` the slot;
/// where the slot is read again, or escapes, the stores stay and the
/// combination only trades one load for shifts and ORs and a longer-lived
/// value (blake3's `compress_chunks_parallel`: +3k loads).
fn single_read_slots(func: &Function) -> rustc_hash::FxHashSet<StackSlot> {
    let mut addr: FxHashMap<Value, StackSlot> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            match func.dfg.insts[i] {
                InstructionData::StackAddr { opcode: Opcode::StackAddr, stack_slot, .. } => {
                    addr.insert(func.dfg.first_result(i), stack_slot);
                }
                InstructionData::Binary { opcode: Opcode::Iadd, args } => {
                    let (a, c) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
                    let base = if iconst(func, c).is_some() { a } else if iconst(func, a).is_some() { c } else { continue };
                    if let Some(&ss) = addr.get(&base) {
                        addr.insert(func.dfg.first_result(i), ss);
                    }
                }
                _ => {}
            }
        }
    }
    let mut loads: FxHashMap<StackSlot, u32> = FxHashMap::default();
    let mut escaped: rustc_hash::FxHashSet<StackSlot> = Default::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let data = func.dfg.insts[i];
            let slot_of = |v: Value| addr.get(&func.dfg.resolve_aliases(v)).copied();
            match data {
                InstructionData::Load { arg, .. } => {
                    if let Some(ss) = slot_of(arg) {
                        *loads.entry(ss).or_default() += 1;
                    }
                }
                InstructionData::Store { args, .. } => {
                    if let Some(ss) = slot_of(args[0]) {
                        escaped.insert(ss);
                    }
                }
                InstructionData::StackAddr { .. } => {}
                InstructionData::Binary { opcode: Opcode::Iadd, args }
                    if addr.contains_key(&func.dfg.first_result(i))
                        && (iconst(func, args[0]).is_some() || iconst(func, args[1]).is_some()) => {}
                _ => {
                    for v in func.dfg.inst_values(i) {
                        if let Some(ss) = slot_of(v) {
                            escaped.insert(ss);
                        }
                    }
                }
            }
        }
    }
    loads.into_iter().filter(|&(ss, n)| n == 1 && !escaped.contains(&ss)).map(|(ss, _)| ss).collect()
}

/// `PLIRON_SLOTCOMB`: replace the integer load `inst` of `ty` from slot `ss`
/// at `off` by the combination of the whole pieces and constant bytes that
/// cover it, when they do exactly (every piece wholly inside the load) and
/// there are exactly two terms. `false`, with nothing changed, otherwise.
fn combine(
    pos: &mut FuncCursor,
    inst: cranelift_codegen::ir::Inst,
    ty: Type,
    ss: StackSlot,
    off: i64,
    known: &FxHashMap<(StackSlot, i64), u8>,
    pieces: &FxHashMap<(StackSlot, i64), (Value, u8, i64)>,
) -> bool {
    let len = ty.bytes() as i64;
    let mut cbits: u64 = 0;
    let mut nconst = 0;
    let mut terms: Vec<(Value, u8, i64)> = Vec::new();
    let mut k = 0;
    while k < len {
        if let Some(&b) = known.get(&(ss, off + k)) {
            cbits |= u64::from(b) << (8 * k);
            nconst += 1;
            k += 1;
            continue;
        }
        let Some(&(v, w, start)) = pieces.get(&(ss, off + k)) else { return false };
        // The piece must start here and lie wholly inside the load.
        if start != off + k || start + i64::from(w) > off + len {
            return false;
        }
        terms.push((v, w, start - off));
        k += i64::from(w);
    }
    // Two terms at most (one piece and the constant bytes, or two pieces):
    // a third and later piece costs a shift and an OR each against one
    // store, and regex's byte-assembled words lost (+0.06%) when they were
    // combined.
    let nterms = terms.len() + usize::from(nconst > 0 && cbits != 0);
    if terms.is_empty() || terms.len() + usize::from(nconst > 0) < 2 || nterms > 2 {
        return false;
    }
    let mut acc: Option<Value> = (nconst > 0 && cbits != 0).then(|| pos.ins().iconst(ty, cbits as i64));
    let last = terms.len() - 1;
    for (j, &(v, w, sh)) in terms.iter().enumerate() {
        let vty = pos.func.dfg.value_type(v);
        let wty = Type::int(u16::from(w) * 8).expect("piece width");
        let mut t = v;
        if vty.bytes() > u32::from(w) {
            t = pos.ins().ireduce(wty, t);
        }
        if wty != ty {
            t = pos.ins().uextend(ty, t);
        }
        if sh > 0 {
            t = pos.ins().ishl_imm(t, 8 * sh);
        }
        if j == last {
            match acc {
                Some(a) => {
                    pos.func.replace(inst).bor(a, t);
                }
                None => {
                    // A single piece shifted, no constant: `t` is the value.
                    let r = pos.func.dfg.first_result(inst);
                    pos.func.dfg.clear_results(inst);
                    pos.func.dfg.change_to_alias(r, t);
                    pos.remove_inst_and_step_back();
                }
            }
        } else {
            acc = Some(match acc {
                Some(a) => pos.ins().bor(a, t),
                None => t,
            });
        }
    }
    true
}

/// Returns the number of loads replaced.
pub fn run(func: &mut Function) -> usize {
    run_with(func, pliron_cfg::env_is("PLIRON_SLOTCOMB", "1"))
}

/// [`run`] with `PLIRON_SLOTCOMB` chosen by the caller.
pub fn run_with(func: &mut Function, comb: bool) -> usize {
    let single = if comb { single_read_slots(func) } else { Default::default() };
    let mut n = 0;
    let mut known: FxHashMap<(StackSlot, i64), u8> = FxHashMap::default();
    // Byte -> (stored value, its width in bytes, the slot offset it starts at).
    let mut pieces: FxHashMap<(StackSlot, i64), (Value, u8, i64)> = FxHashMap::default();
    let mut pos = FuncCursor::new(func);
    while let Some(_b) = pos.next_block() {
        known.clear();
        pieces.clear();
        while let Some(inst) = pos.next_inst() {
            let data = pos.func.dfg.insts[inst];
            match data {
                InstructionData::Store { opcode: Opcode::Store, args, offset, .. } => {
                    let val = args[0];
                    let size = pos.func.dfg.value_type(val).bytes() as i64;
                    match slot_addr(pos.func, args[1], i64::from(i32::from(offset))) {
                        Some((ss, off)) => {
                            for k in 0..size {
                                pieces.remove(&(ss, off + k));
                            }
                            match const_bytes(pos.func, val) {
                                Some(bs) => {
                                    for (k, &x) in bs.iter().enumerate() {
                                        known.insert((ss, off + k as i64), x);
                                    }
                                }
                                None => {
                                    for k in 0..size {
                                        known.remove(&(ss, off + k));
                                    }
                                    let ty = pos.func.dfg.value_type(val);
                                    if comb && ty.is_int() && size <= 8 {
                                        for k in 0..size {
                                            pieces.insert((ss, off + k), (val, size as u8, off));
                                        }
                                    }
                                }
                            }
                        }
                        None => {
                            known.clear();
                            pieces.clear();
                        }
                    }
                }
                InstructionData::Store { opcode: op @ (Opcode::Istore8 | Opcode::Istore16 | Opcode::Istore32), args, offset, .. }
                    if comb =>
                {
                    let val = args[0];
                    let size: i64 = match op {
                        Opcode::Istore8 => 1,
                        Opcode::Istore16 => 2,
                        _ => 4,
                    };
                    match slot_addr(pos.func, args[1], i64::from(i32::from(offset))) {
                        Some((ss, off)) => {
                            for k in 0..size {
                                known.remove(&(ss, off + k));
                                pieces.insert((ss, off + k), (val, size as u8, off));
                            }
                        }
                        None => {
                            known.clear();
                            pieces.clear();
                        }
                    }
                }
                InstructionData::Load { opcode: Opcode::Load, arg, offset, .. } => {
                    let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
                    if let Some((ss, off)) = slot_addr(pos.func, arg, i64::from(i32::from(offset))) {
                        let bs: Option<Vec<u8>> =
                            (0..ty.bytes() as i64).map(|k| known.get(&(ss, off + k)).copied()).collect();
                        if let Some(bs) = bs {
                            if materialize(&mut pos, inst, ty, &bs) {
                                n += 1;
                            }
                        } else if comb
                            && ty.is_int()
                            && ty.bytes() <= 8
                            && single.contains(&ss)
                            && combine(&mut pos, inst, ty, ss, off, &known, &pieces)
                        {
                            n += 1;
                        }
                    }
                }
                _ => {
                    let op = data.opcode();
                    if op.can_store() || op.is_call() || op.other_side_effects() {
                        known.clear();
                        pieces.clear();
                    }
                }
            }
        }
    }
    n
}
