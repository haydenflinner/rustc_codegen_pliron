//! Cranelift-IR peepholes its egraph lacks.
//!
//! `(K & (1 << s)) != 0` on i128 constants (rustc's `u128` bitset idiom, e.g.
//! `is_id_continue`'s ASCII mask) becomes an i64 select + shift, like LLVM's
//! lowering, instead of a full i128 shift and mask.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Function, InstBuilder, InstructionData, Opcode, Value, types};

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

fn const128(func: &Function, v: Value) -> Option<(i64, i64)> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::Binary {
            opcode: Opcode::Iconcat,
            args,
        } => Some((iconst(func, args[0])?, iconst(func, args[1])?)),
        _ => None,
    }
}

/// `(K, s)` if `v` is `band K, (ishl 1, s)` with a constant i128 `K`.
fn bit_test(func: &Function, v: Value) -> Option<((i64, i64), Value)> {
    let v = func.dfg.resolve_aliases(v);
    let InstructionData::Binary {
        opcode: Opcode::Band,
        args,
    } = func.dfg.insts[func.dfg.value_def(v).inst()?]
    else {
        return None;
    };
    for (k, sh) in [(args[0], args[1]), (args[1], args[0])] {
        let Some(k) = const128(func, k) else { continue };
        let sh = func.dfg.resolve_aliases(sh);
        let Some(i) = func.dfg.value_def(sh).inst() else {
            continue;
        };
        if let InstructionData::Binary {
            opcode: Opcode::Ishl,
            args: [one, amt],
        } = func.dfg.insts[i]
            && const128(func, one) == Some((1, 0))
        {
            return Some((k, func.dfg.resolve_aliases(amt)));
        }
    }
    None
}

/// `amt` as i64 when it is `uextend x` or `uextend x & m` (`m` < 2^63).
fn narrow(pos: &mut FuncCursor, amt: Value) -> Option<Value> {
    let f = &*pos.func;
    let (a, m) = match f.dfg.insts[f.dfg.value_def(amt).inst()?] {
        InstructionData::Binary {
            opcode: Opcode::Band,
            args,
        } => {
            let (a, b) = (
                f.dfg.resolve_aliases(args[0]),
                f.dfg.resolve_aliases(args[1]),
            );
            match (const128(f, a), const128(f, b)) {
                (_, Some((m, 0))) => (a, Some(m)),
                (Some((m, 0)), _) => (b, Some(m)),
                _ => return None,
            }
        }
        _ => (amt, None),
    };
    let InstructionData::Unary {
        opcode: Opcode::Uextend,
        arg,
    } = f.dfg.insts[f.dfg.value_def(a).inst()?]
    else {
        return None;
    };
    let x = f.dfg.resolve_aliases(arg);
    let xt = f.dfg.value_type(x);
    if xt.is_vector() || !xt.is_int() || xt.bits() > 64 {
        return None;
    }
    let x64 = if xt == types::I64 {
        x
    } else {
        pos.ins().uextend(types::I64, x)
    };
    Some(match m {
        Some(m) => {
            let c = pos.ins().iconst(types::I64, m);
            pos.ins().band(x64, c)
        }
        None => x64,
    })
}

/// Returns the number of rewrites.
pub fn run(func: &mut Function) -> usize {
    let mut n = 0;
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            let InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                cond,
                args,
            } = pos.func.dfg.insts[inst]
            else {
                continue;
            };
            if !matches!(cond, IntCC::Equal | IntCC::NotEqual)
                || pos.func.dfg.value_type(args[0]) != types::I128
            {
                continue;
            }
            let (x, z) = if const128(pos.func, args[1]) == Some((0, 0)) {
                (args[0], args[1])
            } else {
                (args[1], args[0])
            };
            if const128(pos.func, z) != Some((0, 0)) {
                continue;
            }
            let Some(((lo, hi), amt)) = bit_test(pos.func, x) else {
                continue;
            };
            // i128 `ishl` masks its amount to 0..128: bit 6 picks the half.
            let s = match pos.func.dfg.value_type(amt) {
                types::I128 => match narrow(&mut pos, amt) {
                    Some(s) => s,
                    None => pos.ins().isplit(amt).0,
                },
                types::I64 => amt,
                _ => continue,
            };
            let c64 = pos.ins().iconst(types::I64, 64);
            let half = pos.ins().band(s, c64);
            let l = pos.ins().iconst(types::I64, lo);
            let h = pos.ins().iconst(types::I64, hi);
            let w = pos.ins().select(half, h, l);
            let sh = pos.ins().ushr(w, s);
            let one = pos.ins().iconst(types::I64, 1);
            let b = pos.ins().band(sh, one);
            let zero = pos.ins().iconst(types::I64, 0);
            pos.func.replace(inst).icmp(cond, b, zero);
            n += 1;
        }
    }
    n
}
