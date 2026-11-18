//! 128-bit integer operations on word halves (`PLIRON_WIDE128=1`, opt-in).
//!
//! `u64 x u64 -> u128` multiplies (foldhash's and ahash's
//! `folded_multiply`, `wyhash`), `(x >> 64) as u64` and `a | (b << 64)`
//! arrive as `i128` operations on `uextend`ed words, and the egraph sees
//! through none of them: lol_html's `Stack::build_state` (foldhash
//! inlined) kept a full 128-bit multiply (`mul`, `umulh`, two `madd`),
//! `isplit` / `iconcat x, 0` / `ireduce` round trips and `orr`s with a
//! zero register, where LLVM has `mul`, `umulh`, `eor`.
//!
//! Forward passes in layout order (repeated while they change something)
//! rewrite each `i128`
//! value whose halves are known into `iconcat lo, hi` of `i64` values,
//! and folds its uses:
//!
//! * `uextend.i128 x` (x: i64) -> `iconcat x, 0`
//! * `ishl c, 0|64`, `ushr c, 0|64` on `c = iconcat a, b` -> `c`,
//!   `iconcat 0, a`, `iconcat b, 0` (shift amounts from constants, also
//!   through `uextend`/`iconcat`)
//! * `bor|bxor|band c1, c2` -> `iconcat (op a1 a2), (op b1 b2)`
//! * `imul (iconcat a, 0), (iconcat b, 0)` -> `iconcat (imul a b),
//!   (umulhi a b)` where `umulhi.i64` is one instruction
//!   (`Target::mulhi64`)
//! * `ireduce.i64 c` -> `a`; `ireduce.iN c` -> `ireduce.iN a`;
//!   `isplit c` -> `a, b`
//!
//! A producer is rewritten only when every use of its result is one of
//! those folds, transitively (`halves_consumed`). An `iadd`/`icmp`/
//! branch keeps the value `i128`: splitting a multiply there inserts
//! `umulhi` whose high half stays live across the still-wide add
//! (pulldown `parse_block`'s widening overflow check, +19k loads with
//! the split). `ireduce` and `isplit` of an `iconcat` still fold; they
//! are the sinks the check stops at.
//!
//! The replaced `i128` instructions become dead and the next
//! dead-code sweep drops them; block parameters stay `i128`.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::{Function, Inst, InstBuilder, InstructionData, Opcode, Value, types};

fn def(func: &Function, v: Value) -> Option<(Inst, InstructionData)> {
    let v = func.dfg.resolve_aliases(v);
    let i = func.dfg.value_def(v).inst()?;
    Some((i, func.dfg.insts[i]))
}

/// The halves of `v` when it is `iconcat lo, hi`.
fn cat(func: &Function, v: Value) -> Option<(Value, Value)> {
    match def(func, v)?.1 {
        InstructionData::Binary { opcode: Opcode::Iconcat, args } => {
            Some((func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1])))
        }
        _ => None,
    }
}

/// An integer constant, also through `uextend` and the low half of an
/// `iconcat` whose high half is the constant zero.
fn konst(func: &Function, v: Value) -> Option<i64> {
    match def(func, v)?.1 {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        InstructionData::Unary { opcode: Opcode::Uextend, arg } => {
            let ty = func.dfg.value_type(arg);
            konst(func, arg).map(|k| if ty.bits() < 64 { k & ((1i64 << ty.bits()) - 1) } else { k })
        }
        InstructionData::Binary { opcode: Opcode::Iconcat, args } if konst(func, args[1]) == Some(0) => {
            konst(func, args[0])
        }
        // A shift amount masked to the width (`x << (n & 127)`) before the
        // egraph has folded the constant mask.
        InstructionData::Binary { opcode: Opcode::Band, args } => Some(konst(func, args[0])? & konst(func, args[1])?),
        _ => None,
    }
}

fn is_zero(func: &Function, v: Value) -> bool {
    konst(func, v) == Some(0)
}

/// Returns the number of instructions rewritten. Layout order is not
/// always dominance order (a cold block laid out last can define a value
/// a later-visited block uses), so the pass repeats while it changes
/// something, at most four times.
pub fn run(func: &mut Function, mulhi64: bool) -> usize {
    let mut n = 0;
    for _ in 0..4 {
        let k = pass(func, mulhi64);
        n += k;
        if k == 0 {
            break;
        }
    }
    n
}

/// `v`'s uses are all ops this pass folds into `i64` halves, transitively.
/// `seen` is 0 unknown, 1 yes, 2 no, 3 on the stack (a cycle is not).
fn halves_consumed(
    func: &Function,
    uses: &crate::uses::Uses,
    v: Value,
    seen: &mut [u8],
) -> bool {
    let v = func.dfg.resolve_aliases(v);
    let i = v.index();
    if i >= seen.len() {
        return false;
    }
    match seen[i] {
        1 => return true,
        2 | 3 => return false,
        _ => {}
    }
    seen[i] = 3;
    let ok = uses.sites(v).iter().all(|&(_, inst)| {
        if func.layout.inst_block(inst).is_none() {
            return true;
        }
        let op = func.dfg.insts[inst].opcode();
        match op {
            Opcode::Ireduce | Opcode::Isplit => true,
            Opcode::Ishl | Opcode::Ushr => {
                let amt = func.dfg.inst_args(inst).get(1).copied();
                let Some(amt) = amt else { return false };
                let Some(k) = konst(func, amt) else { return false };
                let k = k & 127;
                if k != 0 && k != 64 {
                    return false;
                }
                let Some(r) = func.dfg.inst_results(inst).first().copied() else {
                    return false;
                };
                halves_consumed(func, uses, r, seen)
            }
            Opcode::Bor | Opcode::Bxor | Opcode::Band | Opcode::Imul => {
                let Some(r) = func.dfg.inst_results(inst).first().copied() else {
                    return false;
                };
                halves_consumed(func, uses, r, seen)
            }
            _ => false,
        }
    });
    seen[i] = if ok { 1 } else { 2 };
    ok
}

fn pass(func: &mut Function, mulhi64: bool) -> usize {
    let uses = crate::uses::Uses::new(func);
    let mut seen = vec![0u8; func.dfg.num_values()];
    let mut n = 0;
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            let data = pos.func.dfg.insts[inst];
            // Copy results out so the match guards can borrow `pos.func`
            // (`halves_consumed`) without overlapping this slice.
            let (res, res1) = {
                let results = pos.func.dfg.inst_results(inst);
                if results.is_empty() {
                    continue;
                }
                (results[0], results.get(1).copied())
            };
            let rty = pos.func.dfg.value_type(res);
            match data {
                InstructionData::Unary { opcode: Opcode::Uextend, arg }
                    if rty == types::I128
                        && pos.func.dfg.value_type(arg) == types::I64
                        && halves_consumed(pos.func, &uses, res, &mut seen) =>
                {
                    let z = pos.ins().iconst(types::I64, 0);
                    pos.func.replace(inst).iconcat(arg, z);
                    n += 1;
                }
                InstructionData::Binary { opcode: op @ (Opcode::Ishl | Opcode::Ushr), args }
                    if rty == types::I128 && halves_consumed(pos.func, &uses, res, &mut seen) =>
                {
                    let Some((a, b)) = cat(pos.func, args[0]) else { continue };
                    let Some(k) = konst(pos.func, args[1]) else { continue };
                    // CLIF masks i128 shift amounts to 7 bits.
                    match (op, k & 127) {
                        (_, 0) => {
                            let src = pos.func.dfg.resolve_aliases(args[0]);
                            pos.func.replace(inst).iconcat(a, b);
                            let _ = src;
                        }
                        (Opcode::Ishl, 64) => {
                            let z = pos.ins().iconst(types::I64, 0);
                            pos.func.replace(inst).iconcat(z, a);
                        }
                        (Opcode::Ushr, 64) => {
                            let z = pos.ins().iconst(types::I64, 0);
                            pos.func.replace(inst).iconcat(b, z);
                        }
                        _ => continue,
                    }
                    n += 1;
                }
                InstructionData::Binary { opcode: op @ (Opcode::Bor | Opcode::Bxor | Opcode::Band), args }
                    if rty == types::I128 && halves_consumed(pos.func, &uses, res, &mut seen) =>
                {
                    let (Some((a1, b1)), Some((a2, b2))) = (cat(pos.func, args[0]), cat(pos.func, args[1])) else {
                        continue;
                    };
                    let (lo, hi) = match op {
                        Opcode::Bor => (pos.ins().bor(a1, a2), pos.ins().bor(b1, b2)),
                        Opcode::Bxor => (pos.ins().bxor(a1, a2), pos.ins().bxor(b1, b2)),
                        _ => (pos.ins().band(a1, a2), pos.ins().band(b1, b2)),
                    };
                    pos.func.replace(inst).iconcat(lo, hi);
                    n += 1;
                }
                // Only where `umulhi.i64` is one instruction
                // (`Target::mulhi64`): on wasm32 it is a long sequence and
                // the multiply stays a `__multi3` call (dragon/grisu grew).
                InstructionData::Binary { opcode: Opcode::Imul, args }
                    if rty == types::I128
                        && mulhi64
                        && halves_consumed(pos.func, &uses, res, &mut seen) =>
                {
                    let (Some((a, ah)), Some((b, bh))) = (cat(pos.func, args[0]), cat(pos.func, args[1])) else {
                        continue;
                    };
                    if !is_zero(pos.func, ah) || !is_zero(pos.func, bh) {
                        continue;
                    }
                    let lo = pos.ins().imul(a, b);
                    let hi = pos.ins().umulhi(a, b);
                    pos.func.replace(inst).iconcat(lo, hi);
                    n += 1;
                }
                InstructionData::Unary { opcode: Opcode::Ireduce, arg }
                    if pos.func.dfg.value_type(arg) == types::I128 =>
                {
                    let Some((a, _)) = cat(pos.func, arg) else { continue };
                    if rty == types::I64 {
                        pos.func.dfg.clear_results(inst);
                        pos.remove_inst_and_step_back();
                        pos.func.dfg.change_to_alias(res, a);
                    } else {
                        pos.func.replace(inst).ireduce(rty, a);
                    }
                    n += 1;
                }
                InstructionData::Unary { opcode: Opcode::Isplit, arg } => {
                    let Some((a, b)) = cat(pos.func, arg) else { continue };
                    let (r0, r1) = (res, res1.expect("isplit has two results"));
                    pos.func.dfg.clear_results(inst);
                    pos.remove_inst_and_step_back();
                    pos.func.dfg.change_to_alias(r0, a);
                    pos.func.dfg.change_to_alias(r1, b);
                    n += 1;
                }
                _ => {}
            }
        }
    }
    n
}
