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

/// Returns the number of loads replaced.
pub fn run(func: &mut Function) -> usize {
    let mut n = 0;
    let mut known: FxHashMap<(StackSlot, i64), u8> = FxHashMap::default();
    let mut pos = FuncCursor::new(func);
    while let Some(_b) = pos.next_block() {
        known.clear();
        while let Some(inst) = pos.next_inst() {
            let data = pos.func.dfg.insts[inst];
            match data {
                InstructionData::Store { opcode: Opcode::Store, args, offset, .. } => {
                    let val = args[0];
                    let size = pos.func.dfg.value_type(val).bytes() as i64;
                    match slot_addr(pos.func, args[1], i64::from(i32::from(offset))) {
                        Some((ss, off)) => match const_bytes(pos.func, val) {
                            Some(bs) => {
                                for (k, &x) in bs.iter().enumerate() {
                                    known.insert((ss, off + k as i64), x);
                                }
                            }
                            None => {
                                for k in 0..size {
                                    known.remove(&(ss, off + k));
                                }
                            }
                        },
                        None => known.clear(),
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
                        }
                    }
                }
                _ => {
                    let op = data.opcode();
                    if op.can_store() || op.is_call() || op.other_side_effects() {
                        known.clear();
                    }
                }
            }
        }
    }
    n
}
