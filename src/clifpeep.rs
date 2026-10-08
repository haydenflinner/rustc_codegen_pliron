//! Cranelift-IR peepholes its egraph lacks.
//!
//! `(K & (1 << s)) != 0` on i128 constants (rustc's `u128` bitset idiom, e.g.
//! `is_id_continue`'s ASCII mask) becomes an i64 select + shift, like LLVM's
//! lowering, instead of a full i128 shift and mask.
//!
//! `uextend (band|bor|bxor a, b)` on narrow ints becomes the op on extended
//! operands, so `uextend (load.i8)` folds into one `movzx` instead of a byte
//! op followed by another `movzx` (UTF-8 decoding, `u8` flag tests).
//!
//! A `load.i8`/`load.i16` that is also `uextend`ed becomes one zero-extending
//! `uload` to i64; the narrow value and the extensions become `ireduce`s,
//! which are free. x64 only folds `uextend (load)` when the load has a single
//! use, so otherwise it re-extends the already-`movzx`ed register.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Function, Inst, InstBuilder, InstructionData, Opcode, Value, types};

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

fn widen(pos: &mut FuncCursor, inst: Inst, arg: Value) -> bool {
    let f = &*pos.func;
    let to = f.dfg.value_type(f.dfg.first_result(inst));
    if to.is_vector() || !to.is_int() || to.bits() > 64 {
        return false;
    }
    let arg = f.dfg.resolve_aliases(arg);
    let Some(d) = f.dfg.value_def(arg).inst() else {
        return false;
    };
    let InstructionData::Binary { opcode, args } = f.dfg.insts[d] else {
        return false;
    };
    if !matches!(opcode, Opcode::Band | Opcode::Bor | Opcode::Bxor) {
        return false;
    }
    let a = pos.ins().uextend(to, args[0]);
    let b = pos.ins().uextend(to, args[1]);
    let r = pos.func.replace(inst);
    match opcode {
        Opcode::Band => r.band(a, b),
        Opcode::Bor => r.bor(a, b),
        _ => r.bxor(a, b),
    };
    true
}

fn widen_on() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("PLIRON_WIDEN").is_ok_and(|v| v == "1"));
    *ON
}

fn uloads(func: &mut Function) -> usize {
    let mut users: rustc_data_structures::fx::FxHashMap<Value, Vec<Inst>> = Default::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if let InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } = func.dfg.insts[i]
            {
                let a = func.dfg.resolve_aliases(arg);
                if let Some(d) = func.dfg.value_def(a).inst()
                    && let InstructionData::Load {
                        opcode: Opcode::Load,
                        ..
                    } = func.dfg.insts[d]
                    && matches!(func.dfg.value_type(a), types::I8 | types::I16)
                {
                    users.entry(a).or_default().push(i);
                }
            }
        }
    }
    let n = users.len();
    let mut pos = FuncCursor::new(func);
    for (r, exts) in users {
        let l = pos.func.dfg.value_def(r).inst().unwrap();
        let InstructionData::Load {
            flags, arg, offset, ..
        } = pos.func.dfg.insts[l]
        else {
            unreachable!()
        };
        let nt = pos.func.dfg.value_type(r);
        let flags = pos.func.dfg.mem_flags[flags].clone();
        pos.goto_inst(l);
        let wide = if nt == types::I8 {
            pos.ins().uload8(types::I64, flags, arg, offset)
        } else {
            pos.ins().uload16(types::I64, flags, arg, offset)
        };
        pos.func.replace(l).ireduce(nt, wide);
        for u in exts {
            let res = pos.func.dfg.first_result(u);
            let to = pos.func.dfg.value_type(res);
            if to == types::I64 {
                pos.func.dfg.clear_results(u);
                pos.func.layout.remove_inst(u);
                pos.func.dfg.change_to_alias(res, wide);
            } else if to.bits() > 64 {
                pos.func.replace(u).uextend(to, wide);
            } else {
                pos.func.replace(u).ireduce(to, wide);
            }
        }
    }
    n
}

/// Returns the number of rewrites.
pub fn run(func: &mut Function) -> usize {
    let mut n = 0;
    if std::env::var("PLIRON_ULOAD").is_ok_and(|v| v == "1") {
        n += uloads(func);
    }
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            if let InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } = pos.func.dfg.insts[inst]
            {
                if widen_on() && widen(&mut pos, inst, arg) {
                    n += 1;
                }
                continue;
            }
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
