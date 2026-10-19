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
//! A `load.i8`/`load.i16`/`load.i32` that is also `uextend`ed becomes one
//! zero-extending `uload` to i64; the narrow value and the extensions
//! become `ireduce`s, which are free. x64 only folds `uextend (load)`
//! when the load has a single use, so otherwise it re-extends the
//! already-`movzx`ed register (`movl` already zeroes the upper bits, so
//! `uload32` and `load.i32` emit the same insn — this just drops the
//! extra vreg regalloc had to coalesce).

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::entity::packed_option::ReservedValue;
use cranelift_codegen::ir::condcodes::{CondCode, FloatCC, IntCC};
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, ExternalName, Function, Inst, InstBuilder, InstructionData,
    LibCall, MemFlagsData, Opcode, Type, Value, ValueDef, types,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::rc::Rc;

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

/// Full-width integer constant: a plain `iconst`, or the `iconcat` of two
/// `iconst`s that ≥65-bit constants arrive as (see `iconst128`).
fn kval(func: &Function, v: Value) -> Option<u128> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits() as u64 as u128),
        InstructionData::Binary {
            opcode: Opcode::Iconcat,
            args,
        } => Some(
            iconst(func, args[0])? as u64 as u128 | (iconst(func, args[1])? as u64 as u128) << 64,
        ),
        _ => None,
    }
}

/// `c` as a `w`-bit integer constant of type `ty` — an `iconcat` pair for
/// i128, a plain `iconst` below.
fn kcon(pos: &mut FuncCursor, ty: Type, w: u32, c: u128) -> Value {
    if w <= 64 {
        pos.ins().iconst(ty, sext(c as u64, w))
    } else {
        let lo = pos.ins().iconst(types::I64, c as u64 as i64);
        let hi = pos.ins().iconst(types::I64, (c >> 64) as u64 as i64);
        pos.ins().iconcat(lo, hi)
    }
}

fn sext(v: u64, bits: u32) -> i64 {
    if bits >= 64 {
        v as i64
    } else {
        let s = 64 - bits;
        ((v << s) as i64) >> s
    }
}

/// Tight unsigned ceiling on `v` — its proven maximum value. Defaults to the
/// type's max; narrower through `uextend` (a `u8` index extended to i64 is at
/// most 255), `band` masks, `ushr`, `urem`, and narrow `uload*`. Used to fold
/// `icmp`s whose constant bound lies outside the operand's range
/// (`x8 < 0x100000`); `bcheck` also calls it for bounded-index versioning.
pub fn vmax(func: &Function, v: Value) -> u64 {
    vmax_d(func, v, 8)
}

fn vmax_d(func: &Function, v: Value, depth: usize) -> u64 {
    let v = func.dfg.resolve_aliases(v);
    let ty = func.dfg.value_type(v);
    let ty_umax = if ty.bits() >= 64 {
        u64::MAX
    } else {
        (1u64 << ty.bits()) - 1
    };
    if !ty.is_int() || ty.is_vector() || depth == 0 {
        return ty_umax;
    }
    let mut hi = ty_umax;
    let Some(d) = func.dfg.value_def(v).inst() else {
        return hi;
    };
    match func.dfg.insts[d] {
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => hi = hi.min(vmax_d(func, arg, depth - 1)),
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => hi = hi.min(imm.bits() as u64 & ty_umax),
        InstructionData::Binary {
            opcode: Opcode::Band,
            args,
        } => {
            // `a & b`'s set bits are a subset of each operand's, so the
            // value is <= each side's own bound — recurse both.
            for a in args {
                hi = hi.min(vmax_d(func, a, depth - 1));
            }
        }
        InstructionData::Load { opcode, .. } => {
            let bits = match opcode {
                Opcode::Uload8 => 8,
                Opcode::Uload16 => 16,
                Opcode::Uload32 => 32,
                _ => 0,
            };
            if bits > 0 {
                hi = hi.min((1u64 << bits) - 1);
            }
        }
        InstructionData::Binary {
            opcode: Opcode::Ushr,
            args,
        } => {
            if let Some(s) = iconst(func, args[1]) {
                // CLIF masks the shift amount to the operand width.
                let s = (s as u64) % (ty.bits() as u64);
                hi = hi.min(ty_umax >> s);
            }
        }
        InstructionData::Binary {
            opcode: Opcode::Urem,
            args,
        } => {
            if let Some(d) = iconst(func, args[1])
                && d > 0
            {
                hi = hi.min(d as u64 - 1);
            }
        }
        // `a + b` is at most the sum of the operand bounds — the u128
        // div/rem fold's limb sum (`band`-masked chunks) lands a tight
        // bound on its `urem` dividend, unlocking divconst's lz-magic.
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args,
        } => {
            let m =
                vmax_d(func, args[0], depth - 1).saturating_add(vmax_d(func, args[1], depth - 1));
            hi = hi.min(m);
        }
        // `icmp` yields 0/1; `a ^ b` sets no bit clear in both operands.
        InstructionData::IntCompare { .. } => hi = hi.min(1),
        // `a * b` is at most the product of the operand bounds —
        // saturating keeps a wrapped product conservative.
        InstructionData::Binary {
            opcode: Opcode::Imul,
            args,
        } => {
            let m =
                vmax_d(func, args[0], depth - 1).saturating_mul(vmax_d(func, args[1], depth - 1));
            hi = hi.min(m);
        }
        InstructionData::Unary { opcode, .. }
            if matches!(opcode, Opcode::Clz | Opcode::Ctz | Opcode::Popcnt) =>
        {
            hi = hi.min(u64::from(ty.bits()));
        }
        InstructionData::Binary {
            opcode: Opcode::Bxor,
            args,
        } => {
            let m = vmax_d(func, args[0], depth - 1) | vmax_d(func, args[1], depth - 1);
            hi = hi.min(
                m.checked_add(1)
                    .and_then(|x| x.checked_next_power_of_two())
                    .map_or(u64::MAX, |p| p - 1),
            );
        }
        _ => {}
    }
    hi
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
        std::sync::LazyLock::new(|| crate::pass_enabled("PLIRON_WIDEN"));
    *ON
}

fn uloads(func: &mut Function) -> usize {
    let mut users: rustc_hash::FxHashMap<Value, Vec<Inst>> = Default::default();
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

/// Late `uload32` fold: `uextend.i64 (load.i32)` becomes one `uload32`
/// to i64, the i32 uses become an `ireduce` — x64 emits `movl` either
/// way, so this only drops the extra vreg regalloc must coalesce (the
/// `movq` it sometimes inserts between the load's reg and the extended
/// value's reg). Runs in the final-CLIF chain only: early passes
/// (licm/loadfwd/dse/bcheck/celim) match `Opcode::Load` and would lose
/// sight of the load if it were rewritten sooner.
pub fn uloads32(func: &mut Function) -> usize {
    let mut users: rustc_hash::FxHashMap<Value, Vec<Inst>> = Default::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if let InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } = func.dfg.insts[i]
                && func.dfg.value_type(func.dfg.first_result(i)) == types::I64
            {
                let a = func.dfg.resolve_aliases(arg);
                if let Some(d) = func.dfg.value_def(a).inst()
                    && let InstructionData::Load {
                        opcode: Opcode::Load,
                        ..
                    } = func.dfg.insts[d]
                    && func.dfg.value_type(a) == types::I32
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
        let flags = pos.func.dfg.mem_flags[flags].clone();
        pos.goto_inst(l);
        let wide = pos.ins().uload32(flags, arg, offset);
        pos.func.replace(l).ireduce(types::I32, wide);
        for u in exts {
            let res = pos.func.dfg.first_result(u);
            pos.func.dfg.clear_results(u);
            pos.func.layout.remove_inst(u);
            pos.func.dfg.change_to_alias(res, wide);
        }
    }
    n
}

/// The `(op, x, k)` of `v = op x, k` for constant `k` (either operand).
fn bin_k(func: &Function, v: Value, op: Opcode) -> Option<(Value, i64)> {
    let i = func.dfg.value_def(v).inst()?;
    let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
        return None;
    };
    if opcode != op {
        return None;
    }
    bin_k_data(func, args)
}

/// `(non_const, imm)` when the `iadd`/`isub`-shaped args hold a constant —
/// the constant's position doesn't matter to callers.
fn bin_k_data(func: &Function, args: [Value; 2]) -> Option<(Value, i64)> {
    let a = args.map(|a| func.dfg.resolve_aliases(a));
    iconst(func, a[1])
        .map(|k| (a[0], k))
        .or_else(|| iconst(func, a[0]).map(|k| (a[1], k)))
}

/// `v` decomposed as `x + a`: an `iadd`/`isub` with a constant gives the
/// base and signed addend; anything else is `x + 0`.
fn addend(func: &Function, v: Value) -> Option<(Value, i64)> {
    let v = func.dfg.resolve_aliases(v);
    let i = func.dfg.value_def(v).inst()?;
    let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
        return Some((v, 0));
    };
    match opcode {
        Opcode::Iadd => bin_k_data(func, args),
        Opcode::Isub => {
            iconst(func, args[1]).map(|k| (func.dfg.resolve_aliases(args[0]), k.wrapping_neg()))
        }
        _ => Some((v, 0)),
    }
}

/// `icmp eq|ne` normalized to `(x + a) cc k`: `eq` is whether the
/// equality holds when the compare is true (false for `ne`), `x`/`a`
/// come from the add-chain operand — a bare operand is `x + 0` — and `k`
/// is whatever the other operand is.
fn icmp_chain(func: &Function, c: Value) -> Option<(bool, Value, i64, Value)> {
    let i = func.dfg.resolved_value_def(c).inst()?;
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args,
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let eq = match cond {
        IntCC::Equal => true,
        IntCC::NotEqual => false,
        _ => return None,
    };
    // Prefer a side whose addend is non-zero so two compares share the
    // `(x, k)` normal form; fall back to `args[0] + 0`.
    let pick = |a: Value, b: Value| {
        addend(func, a)
            .filter(|&(_, a)| a != 0)
            .map(|(x, a)| (x, a, b))
    };
    pick(args[0], args[1])
        .or_else(|| pick(args[1], args[0]))
        .or_else(|| addend(func, args[0]).map(|(x, a)| (x, a, args[1])))
        .map(|(x, a, k)| (eq, x, a, k))
}

/// `v` as a constant: a plain `iconst`, or `x ± k` over a constant base
/// — select arms arrive as `iadd 0, 1` before `lcse`/`sameargs` fold
/// them to `iconst`.
fn keval(func: &Function, v: Value) -> Option<i64> {
    iconst(func, v)
        .or_else(|| addend(func, v).and_then(|(x, a)| iconst(func, x).map(|k| k.wrapping_add(a))))
}

/// Depth-bounded constant evaluator: through `uextend`/`sext`/`ireduce`
/// and the arithmetic/bitwise binops — enough for field-merge chains
/// whose `ishl`/`band`/`bor` operands never reached an `iconst`
/// (`ishl (uextend 1), 8` building a packed-enum tag byte).
fn ceval(func: &Function, v: Value, d: u32) -> Option<u128> {
    let v = func.dfg.resolve_aliases(v);
    let i = func.dfg.value_def(v).inst()?;
    let w = func.dfg.value_type(func.dfg.first_result(i)).bits();
    let m = u128::MAX >> (128 - w.min(128));
    let arg_w = |a: Value| {
        let a = func.dfg.resolve_aliases(a);
        func.dfg.value_type(a).bits()
    };
    Some(match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => imm.bits() as u64 as u128 & m,
        InstructionData::Unary {
            opcode: Opcode::Uextend | Opcode::Ireduce,
            arg,
        } if d > 0 => {
            let sw = arg_w(arg);
            ceval(func, arg, d - 1)? & (u128::MAX >> (128 - sw.min(128)))
        }
        InstructionData::Unary {
            opcode: Opcode::Sextend,
            arg,
        } if d > 0 => {
            let sw = arg_w(arg);
            let sm = u128::MAX >> (128 - sw.min(128));
            let b = ceval(func, arg, d - 1)? & sm;
            if sw < 128 && b >> (sw - 1) & 1 == 1 {
                b | !sm
            } else {
                b
            }
        }
        InstructionData::Binary { opcode, args } if d > 0 => {
            let a = ceval(func, args[0], d - 1)?;
            let b = ceval(func, args[1], d - 1)?;
            let r = match opcode {
                Opcode::Iadd => a.wrapping_add(b),
                Opcode::Isub => a.wrapping_sub(b),
                Opcode::Imul => a.wrapping_mul(b),
                Opcode::Band => a & b,
                Opcode::Bor => a | b,
                Opcode::Bxor => a ^ b,
                Opcode::Ishl if b < w as u128 => a.wrapping_shl(b as u32),
                Opcode::Ushr if b < w as u128 => a >> b,
                Opcode::Sshr if b < w as u128 => {
                    let sa = if w < 128 && a >> (w - 1) & 1 == 1 {
                        a | !m
                    } else {
                        a
                    };
                    (sa as i128 >> b) as u128
                }
                _ => return None,
            };
            r & m
        }
        _ => return None,
    })
}

/// Given `x + a2 == k2` for certain, is `x + a1 == k1` decided?
/// `⟺ a1 - a2 ≡ k1 - k2 (mod 2^bits)` — known when `k1`/`k2` are the
/// same value or both constants.
fn eq_decided(func: &Function, bits: u32, a1: i64, k1: Value, a2: i64, k2: Value) -> Option<bool> {
    let mask = if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    if func.dfg.resolve_aliases(k1) == func.dfg.resolve_aliases(k2) {
        return Some(a1.wrapping_sub(a2) as u64 & mask == 0);
    }
    match (iconst(func, k1), iconst(func, k2)) {
        (Some(k1), Some(k2)) => Some(
            (k1 as u64).wrapping_sub(k2 as u64) & mask
                == (a1 as u64).wrapping_sub(a2 as u64) & mask,
        ),
        _ => None,
    }
}

/// `v` as `uextend b` where `b` is an i8 provably in {0,1} — returns `b`,
/// usable as a `select` condition.
fn uext01(func: &Function, v: Value) -> Option<Value> {
    let d = func.dfg.resolved_value_def(v).inst()?;
    if let InstructionData::Unary {
        opcode: Opcode::Uextend,
        arg,
    } = func.dfg.insts[d]
        && func.dfg.value_type(arg) == types::I8
        && is01(func, arg, 4)
    {
        return Some(arg);
    }
    None
}

/// `v` is a scalar `icmp` boolean with an unsigned condition — on x64
/// these fuse into the `cmp`'s CF for `adc`/`sbb` consumers.
fn ucmp_bool(func: &Function, v: Value) -> bool {
    let v = func.dfg.resolve_aliases(v);
    let Some(i) = func.dfg.value_def(v).inst() else {
        return false;
    };
    matches!(
        func.dfg.insts[i],
        InstructionData::IntCompare { cond, .. }
            if matches!(
                cond,
                IntCC::UnsignedLessThan
                    | IntCC::UnsignedGreaterThan
                    | IntCC::UnsignedLessThanOrEqual
                    | IntCC::UnsignedGreaterThanOrEqual
            )
    )
}

/// Whether `v` is always 0 or 1.
fn is01(func: &Function, v: Value, depth: u32) -> bool {
    let v = func.dfg.resolve_aliases(v);
    if iconst(func, v).is_some_and(|k| matches!(k, 0 | 1)) {
        return true;
    }
    if let Some(i) = func.dfg.value_def(v).inst() {
        match func.dfg.insts[i] {
            _ if matches!(func.dfg.insts[i].opcode(), Opcode::Icmp | Opcode::Fcmp) => {
                return true;
            }
            InstructionData::Unary {
                opcode: Opcode::Uextend | Opcode::Ireduce,
                arg,
            } if depth > 0 => return is01(func, arg, depth - 1),
            InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            } if depth > 0 => {
                return is01(func, args[1], depth - 1) && is01(func, args[2], depth - 1);
            }
            _ => {}
        }
    }
    bin_k(func, v, Opcode::Band).is_some_and(|(_, k)| k == 1)
        || (depth > 0
            && bin_k(func, v, Opcode::Bxor)
                .is_some_and(|(x, k)| k == 1 && is01(func, x, depth - 1)))
}

/// Bitmask of `v`'s possibly-set bits — `v & cover(v) == v` always.
/// Tight through `band`/`ushr`/`uextend`/narrow loads; the type's
/// all-ones otherwise.
fn cover(func: &Function, v: Value, depth: usize) -> u64 {
    let v = func.dfg.resolve_aliases(v);
    let ty = func.dfg.value_type(v);
    let all = if ty.bits() >= 64 {
        u64::MAX
    } else {
        (1u64 << ty.bits()) - 1
    };
    if !ty.is_int() || ty.is_vector() || depth == 0 {
        return all;
    }
    let Some(d) = func.dfg.value_def(v).inst() else {
        return all;
    };
    match func.dfg.insts[d] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => imm.bits() as u64 & all,
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => cover(func, arg, depth - 1),
        InstructionData::Binary {
            opcode: Opcode::Band,
            args,
        } => cover(func, args[0], depth - 1) & cover(func, args[1], depth - 1),
        InstructionData::Binary {
            opcode: Opcode::Bor,
            args,
        } => (cover(func, args[0], depth - 1) | cover(func, args[1], depth - 1)) & all,
        InstructionData::Binary {
            opcode: Opcode::Ushr,
            args,
        } => match iconst(func, args[1]) {
            Some(s) => cover(func, args[0], depth - 1) >> (s as u64 % ty.bits() as u64),
            None => all,
        },
        InstructionData::Load { opcode, .. } => match opcode {
            Opcode::Uload8 => 0xff,
            Opcode::Uload16 => 0xffff,
            Opcode::Uload32 => 0xffff_ffff,
            _ => all,
        },
        _ => all,
    }
}

/// `v` as a constant lane: `iconst` or `splat (iconst)`, sign-extended.
fn lane_const(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.insts[func.dfg.value_def(v).inst()?] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        InstructionData::Unary {
            opcode: Opcode::Splat,
            arg,
        } => iconst(func, arg),
        _ => None,
    }
}

/// Same value, or same constant bit-pattern in `w` bits.
fn same_lane(func: &Function, a: Value, b: Value, w: u32) -> bool {
    let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
    if a == b {
        return true;
    }
    let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
    match (lane_const(func, a), lane_const(func, b)) {
        (Some(x), Some(y)) => (x as u64 & mask) == (y as u64 & mask),
        _ => false,
    }
}

/// `v` as `min_or_max(x, k)` → `(op, x, k)` with the constant second.
fn minmax_k(func: &Function, v: Value) -> Option<(Opcode, Value, i64)> {
    let i = func.dfg.resolved_value_def(v).inst()?;
    let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
        return None;
    };
    if !matches!(
        opcode,
        Opcode::Umin | Opcode::Umax | Opcode::Smin | Opcode::Smax
    ) {
        return None;
    }
    let (a, b) = (args[0], args[1]);
    if let Some(k) = lane_const(func, b) {
        Some((opcode, a, k))
    } else {
        lane_const(func, a).map(|k| (opcode, b, k))
    }
}

/// `select|bitselect (icmp cc x, y) t, f` folds to integer min/max — the
/// vendored egraph knows the one-level forms but not the nested clamp idiom
/// `x < lo ? lo : min(x, hi)` (needs a `lo <= hi` proof), and folding here
/// lets loopvec emit `umin`/`umax` directly instead of `icmp`+`bitselect`.
fn minmax(pos: &mut FuncCursor, inst: Inst) -> bool {
    let f = &*pos.func;
    let InstructionData::Ternary { opcode, args } = f.dfg.insts[inst] else {
        return false;
    };
    if !matches!(opcode, Opcode::Select | Opcode::Bitselect) {
        return false;
    }
    let ty = f.dfg.value_type(f.dfg.first_result(inst));
    let w = if ty.is_vector() {
        ty.lane_bits()
    } else {
        ty.bits()
    };
    if !ty.lane_type().is_int() || w > 64 {
        return false;
    }
    let [c, t, fv] = args;
    let Some(ci) = f.dfg.resolved_value_def(c).inst() else {
        return false;
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond: cc,
        args: [a, b],
    } = f.dfg.insts[ci]
    else {
        return false;
    };
    let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
    let ku = |k: i64| k as u64 & mask;
    let ks = |k: i64| ((k as u64 & mask) << (64 - w) as i64) >> (64 - w);

    // select(icmp cc a b, t, f): try both arm orders — swapping arms and
    // complementing cc is the same function.
    for (tt, ff, cc) in [(t, fv, cc), (fv, t, cc.complement())] {
        // select(x < y ? x : y) = min etc.
        let direct = if same_lane(f, tt, a, w) && same_lane(f, ff, b, w) {
            Some(match cc {
                IntCC::UnsignedLessThan | IntCC::UnsignedLessThanOrEqual => Opcode::Umin,
                IntCC::UnsignedGreaterThan | IntCC::UnsignedGreaterThanOrEqual => Opcode::Umax,
                IntCC::SignedLessThan | IntCC::SignedLessThanOrEqual => Opcode::Smin,
                IntCC::SignedGreaterThan | IntCC::SignedGreaterThanOrEqual => Opcode::Smax,
                _ => continue,
            })
        } else {
            None
        };
        if let Some(op) = direct {
            let (a, b) = (a, b);
            let r = pos.func.replace(inst);
            match op {
                Opcode::Umin => r.umin(a, b),
                Opcode::Umax => r.umax(a, b),
                Opcode::Smin => r.smin(a, b),
                _ => r.smax(a, b),
            };
            return true;
        }
    }

    // Nested clamp: select(icmp cc x, K1) K1, minmax(x, K2).
    // Normalize so the icmp's constant is the rhs.
    let (mut x, mut k1v, mut cc) = (a, b, cc);
    if lane_const(f, x).is_some() && lane_const(f, k1v).is_none() {
        (x, k1v) = (k1v, x);
        cc = cc.swap_args();
    }
    let Some(k1) = lane_const(f, k1v) else {
        return false;
    };
    for (tt, ff, cc) in [(t, fv, cc), (fv, t, cc.complement())] {
        if lane_const(f, tt).map_or(true, |k| ku(k) != ku(k1)) {
            continue;
        }
        let Some((mop, mx, k2)) = minmax_k(f, ff) else {
            continue;
        };
        if f.dfg.resolve_aliases(mx) != f.dfg.resolve_aliases(x) {
            continue;
        }
        // `x < K1 ? K1 : min(x, K2)` = max(min(x,K2), K1) iff K1 <= K2, etc.
        // The inner min/max must share the icmp's signedness — a mixed
        // signedness changes the result for negative or high-bit lanes.
        let signed = matches!(
            cc,
            IntCC::SignedLessThan
                | IntCC::SignedLessThanOrEqual
                | IntCC::SignedGreaterThan
                | IntCC::SignedGreaterThanOrEqual
        );
        if signed != matches!(mop, Opcode::Smin | Opcode::Smax) {
            continue;
        }
        let less = match cc {
            IntCC::UnsignedLessThan
            | IntCC::UnsignedLessThanOrEqual
            | IntCC::SignedLessThan
            | IntCC::SignedLessThanOrEqual => true,
            IntCC::UnsignedGreaterThan
            | IntCC::UnsignedGreaterThanOrEqual
            | IntCC::SignedGreaterThan
            | IntCC::SignedGreaterThanOrEqual => false,
            _ => continue,
        };
        let (le12, ge12) = if signed {
            (ks(k1) <= ks(k2), ks(k1) >= ks(k2))
        } else {
            (ku(k1) <= ku(k2), ku(k1) >= ku(k2))
        };
        let (min_op, max_op) = if signed {
            (Opcode::Smin, Opcode::Smax)
        } else {
            (Opcode::Umin, Opcode::Umax)
        };
        let want_min = mop == min_op;
        // x<K ? K : min(x,K2) -> max(inner,K)   [K<=K2]
        // x>K ? K : min(x,K2) -> min(x,K)      [K<=K2]
        // x<K ? K : max(x,K2) -> max(x,K)      [K>=K2]
        // x>K ? K : max(x,K2) -> min(inner,K)  [K>=K2]
        let new = if less && want_min && le12 {
            Some((max_op, ff, tt))
        } else if !less && want_min && le12 {
            Some((min_op, x, tt))
        } else if less && !want_min && ge12 {
            Some((max_op, x, tt))
        } else if !less && !want_min && ge12 {
            Some((min_op, ff, tt))
        } else {
            None
        };
        let Some((op, a1, a2)) = new else { continue };
        let r = pos.func.replace(inst);
        match op {
            Opcode::Umin => r.umin(a1, a2),
            Opcode::Umax => r.umax(a1, a2),
            Opcode::Smin => r.smin(a1, a2),
            _ => r.smax(a1, a2),
        };
        return true;
    }
    false
}

/// Peel rustc's `!bool` spellings off a branch/select condition —
/// `bxor x, 1` and `band x, 1` over known 0/1 values, `ireduce`, the
/// `select c, 0, 1` (`!c`) / `select c, 1, 0` residues, and
/// `icmp eq/ne (0/1 x), 0/1` tests. Returns the stripped value and
/// whether the chain logically negates it. When `chain` is given it
/// receives each intermediate `(value, negated-vs-arg)` — an arg whose
/// select on an intermediate condition is decided edge-side.
fn un_not_chain(
    func: &Function,
    arg: Value,
    mut chain: Option<&mut Vec<(Value, bool)>>,
) -> (Value, bool) {
    let (mut v, mut neg) = (func.dfg.resolve_aliases(arg), false);
    for _ in 0..8 {
        if let Some(c) = chain.as_deref_mut() {
            c.push((v, neg));
        }
        match (bin_k(func, v, Opcode::Band), bin_k(func, v, Opcode::Bxor)) {
            (Some((x, 1)), _) if is01(func, x, 4) => v = x,
            (_, Some((x, 1))) if is01(func, x, 4) => {
                v = x;
                neg = !neg;
            }
            _ => {
                // `ireduce` of a 0/1 value is free and `select c, 0, 1`
                // is `!c` while `select c, 1, 0` is `c != 0` — the
                // `Option`-select residue `brif (zext !c) & 1`.
                let Some(i) = func.dfg.value_def(v).inst() else {
                    break;
                };
                match func.dfg.insts[i] {
                    InstructionData::Unary {
                        opcode: Opcode::Ireduce,
                        arg: x,
                    } if is01(func, x, 4) => v = x,
                    InstructionData::Ternary {
                        opcode: Opcode::Select,
                        args,
                    } => {
                        let a = iconst(func, func.dfg.resolve_aliases(args[1]));
                        let b = iconst(func, func.dfg.resolve_aliases(args[2]));
                        match (a, b) {
                            (Some(0), Some(1)) => {
                                v = args[0];
                                neg = !neg;
                            }
                            (Some(1), Some(0)) => v = args[0],
                            _ => break,
                        }
                    }
                    // `icmp eq/ne x, k` on a 0/1 `x` is `x` or `!x` —
                    // `icmp eq (select c, 0, 1), 1` style residue.
                    InstructionData::IntCompare {
                        opcode: Opcode::Icmp,
                        cond,
                        args,
                        ..
                    } if crate::pass_enabled("PLIRON_EQ01")
                        && matches!(cond, IntCC::Equal | IntCC::NotEqual) =>
                    {
                        let (x, k) = if let Some(k) =
                            iconst(func, func.dfg.resolve_aliases(args[1]))
                        {
                            (args[0], k)
                        } else if let Some(k) = iconst(func, func.dfg.resolve_aliases(args[0])) {
                            (args[1], k)
                        } else {
                            break;
                        };
                        if !matches!(k, 0 | 1) || !is01(func, x, 4) {
                            break;
                        }
                        v = x;
                        neg ^= (cond == IntCC::Equal) == (k == 0);
                    }
                    _ => break,
                }
            }
        }
    }
    (v, neg)
}

fn un_not(func: &Function, arg: Value) -> (Value, bool) {
    un_not_chain(func, arg, None)
}

/// `brif (bxor (band c, 1), 1), a, b` (rustc's `!bool` after `i1`
/// normalization) becomes `brif c, b, a`.
pub fn brif_not(func: &mut Function, inst: Inst) -> bool {
    let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[inst] else {
        return false;
    };
    let (v, neg) = un_not(func, arg);
    if v == func.dfg.resolve_aliases(arg) {
        return false;
    }
    let blocks = if neg { [blocks[1], blocks[0]] } else { blocks };
    if let InstructionData::Brif {
        arg: a, blocks: bs, ..
    } = &mut func.dfg.insts[inst]
    {
        *a = v;
        *bs = blocks;
    }
    true
}

/// `brif c, T(.., select c', t, e, ..), E(..)` — an edge arg built on a
/// condition the `un_not` chain equates with the branch's own is decided
/// per edge: `c'` is known-true on the taken arm's edge, known-false on
/// the other. Peel `ireduce`/`uextend`/`sextend` wrappers around a
/// `select c', t, e` (and a bare chain value `c'` itself) and substitute
/// what each edge picks — enum discriminants rematerialized ahead of a
/// dispatch block fold to the edge's constant.
pub fn brif_argspec(func: &mut Function, inst: Inst) -> usize {
    let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[inst] else {
        return 0;
    };
    let mut chain = Vec::with_capacity(8);
    un_not_chain(func, arg, Some(&mut chain));
    /// What `v` becomes on an edge where the chain intermediates take the
    /// values in `tset` — either a decided `iconst` (`K`), or the arm a
    /// decided `select` forwards re-wrapped in `v`'s extends (`W`).
    enum Pick {
        K(i64),
        W(Value, Vec<(Opcode, Type)>),
    }
    fn spec_of(func: &Function, tset: &FxHashMap<Value, bool>, v: Value) -> Option<Pick> {
        let mut ops = Vec::new();
        let mut v = func.dfg.resolve_aliases(v);
        for _ in 0..4 {
            if let Some(&t) = tset.get(&v) {
                // A chain intermediate used as an edge arg: the edge
                // decides it 0 outright (the falsy arm means `v == 0`),
                // or 1 when `v` is a canonical boolean — `brif` only
                // proves nonzero, and the raw `arg` or a peeled `select`
                // condition can be wider than 0/1 (e.g. a call result
                // threaded to a return).
                if !t || is01(func, v, 4) {
                    return Some(Pick::K(t as i64));
                }
                return None;
            }
            let i = func.dfg.value_def(v).inst()?;
            match func.dfg.insts[i] {
                InstructionData::Unary {
                    opcode: op @ (Opcode::Ireduce | Opcode::Uextend | Opcode::Sextend),
                    arg: x,
                } => {
                    ops.push((op, func.dfg.value_type(v)));
                    v = func.dfg.resolve_aliases(x);
                }
                InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } => {
                    let &t = tset.get(&func.dfg.resolve_aliases(args[0]))?;
                    return Some(Pick::W(args[2 - usize::from(t)], ops));
                }
                _ => return None,
            }
        }
        None
    }
    let mut n = 0;
    for (d, &bc) in blocks.iter().enumerate() {
        // truth[x]: value chain value `x` takes on this edge — on the
        // then-edge `arg` is nonzero and each intermediate applies its
        // recorded parity; the else-edge flips it.
        let tset: FxHashMap<Value, bool> =
            chain.iter().map(|&(x, neg)| (x, (d == 0) != neg)).collect();
        let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
        let mut repl = Vec::new();
        for (ai, a) in args.iter().enumerate() {
            let BlockArg::Value(a) = a else {
                continue;
            };
            let ty = func.dfg.value_type(*a);
            let Some(pick) = spec_of(func, &tset, *a) else {
                continue;
            };
            let mut cur = FuncCursor::new(func).at_inst(inst);
            let val = match pick {
                Pick::K(k) => cur.ins().iconst(ty, k),
                Pick::W(base, ops) => {
                    let mut v = cur.func.dfg.resolve_aliases(base);
                    // `ops` were peeled outside-in; replay them
                    // inside-out (innermost first). Const arms fold.
                    for &(op, oty) in ops.iter().rev() {
                        if let Some(k) = iconst(cur.func, v) {
                            let bits = match op {
                                Opcode::Ireduce if oty.bits() < 64 => {
                                    k & ((1i64 << oty.bits()) - 1)
                                }
                                Opcode::Sextend => {
                                    let w = cur.func.dfg.value_type(v).bits();
                                    (k << (64 - w)) >> (64 - w)
                                }
                                _ => k,
                            };
                            v = cur.ins().iconst(oty, bits);
                        } else {
                            v = match op {
                                Opcode::Ireduce => cur.ins().ireduce(oty, v),
                                Opcode::Uextend => cur.ins().uextend(oty, v),
                                _ => cur.ins().sextend(oty, v),
                            };
                        }
                    }
                    v
                }
            };
            repl.push((ai, val));
        }
        if repl.is_empty() {
            continue;
        }
        n += repl.len();
        let mut repl = repl.into_iter().peekable();
        let dfg = &mut func.dfg;
        let bc_mut = &mut dfg.insts[inst]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[d];
        let mut ai = 0;
        bc_mut.update_args(&mut dfg.value_lists, |a| {
            let a = match repl.peek() {
                Some(&(i, v)) if i == ai => {
                    repl.next();
                    BlockArg::Value(v)
                }
                _ => a,
            };
            ai += 1;
            a
        });
    }
    n
}

/// Sweep every `brif` applying `brif_not` (`PLIRON_BRIFNOT`) and
/// `brif_argspec` (`PLIRON_ARGSPEC`) — the late pipeline step's driver
/// after `merge_chains` remerged a dispatch block into its pred.
pub fn brif_fold(func: &mut Function) -> usize {
    let mut n = 0;
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            if pos.func.dfg.insts[inst].opcode() != Opcode::Brif {
                continue;
            }
            if crate::pass_enabled("PLIRON_ARGSPEC") {
                n += brif_argspec(pos.func, inst);
            }
            if crate::pass_enabled("PLIRON_BRIFNOT") && brif_not(pos.func, inst) {
                n += 1;
            }
        }
    }
    n
}
/// becomes `select (icmp cc a b) f, t`: the icmp's own flags then feed the
/// `csel`, instead of materializing the negation with `eor`+`tst` — the
/// shape LLVM emits for partition/insertion-sort inner loops.
fn sel_not(func: &mut Function, inst: Inst) -> bool {
    let InstructionData::Ternary {
        opcode: Opcode::Select,
        args,
    } = func.dfg.insts[inst]
    else {
        return false;
    };
    let (v, neg) = un_not(func, args[0]);
    if v == func.dfg.resolve_aliases(args[0]) {
        return false;
    }
    let (t, f) = if neg {
        (args[2], args[1])
    } else {
        (args[1], args[2])
    };
    if let InstructionData::Ternary { args: a, .. } = &mut func.dfg.insts[inst] {
        a[0] = v;
        a[1] = t;
        a[2] = f;
    }
    true
}

/// rustc lowers `CheckedBinaryOp` (integer ops under `overflow-checks`) to
/// two-result `*overflow` CLIF ops; when the `Assert` on the flag is later
/// optimized away the flag result stays dead, but the checked op still
/// defeats op-level matching (loopvec reductions, idioms) and pays for the
/// flag test in scalar code. Rewrite dead-flag overflow ops to the plain
/// wrapping op — identical first-result semantics.
fn deflag(func: &mut Function) -> usize {
    // A value is used iff it appears in some inst's args or in a branch
    // destination's args (block-call args aren't part of `inst_args`).
    let mut used: FxHashSet<Value> = FxHashSet::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for &a in func.dfg.inst_args(i) {
                used.insert(func.dfg.resolve_aliases(a));
            }
            for a in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .flat_map(|bc| bc.args(&func.dfg.value_lists))
            {
                if let BlockArg::Value(v) = a {
                    used.insert(func.dfg.resolve_aliases(v));
                }
            }
        }
    }
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let plain = match func.dfg.insts[i].opcode() {
                Opcode::SaddOverflow | Opcode::UaddOverflow => Opcode::Iadd,
                Opcode::SsubOverflow | Opcode::UsubOverflow => Opcode::Isub,
                Opcode::SmulOverflow | Opcode::UmulOverflow => Opcode::Imul,
                _ => continue,
            };
            let res = func.dfg.inst_results(i).to_vec();
            let InstructionData::Binary { args, .. } = func.dfg.insts[i] else {
                continue;
            };
            if res.len() != 2 || used.contains(&func.dfg.resolve_aliases(res[1])) {
                continue;
            }
            let fty = func.dfg.value_type(res[1]);
            let mut pos = FuncCursor::new(func).at_inst(i);
            let nv = match plain {
                Opcode::Iadd => pos.ins().iadd(args[0], args[1]),
                Opcode::Isub => pos.ins().isub(args[0], args[1]),
                _ => pos.ins().imul(args[0], args[1]),
            };
            // Detach the old results before aliasing (`change_to_alias`
            // requires unattached values); point the dead flag at a zero
            // const so it isn't left dangling on a removed inst.
            let z = pos.ins().iconst(fty, 0);
            pos.func.dfg.clear_results(i);
            pos.func.dfg.change_to_alias(res[0], nv);
            pos.func.dfg.change_to_alias(res[1], z);
            pos.func.layout.remove_inst(i);
            n += 1;
        }
    }
    n
}

/// A shared cold block with block params makes its incoming edge copies
/// unconditional: regalloc can't place the parallel copy inside the cold
/// successor (other preds feed different values), so every hot
/// predecessor's tail pays the moves — executed even when the cold path
/// is never taken (e.g. punroll's shared `block21(v46) cold` panic
/// blocks). Give each hot->cold param edge a cold adapter block
/// `a: jump C(args)`: the same parallel copy then materializes inside
/// cold code, while the arg vregs are merely live-in to the adapter and
/// the hot edge carries no args at all.
pub fn coldedges(func: &mut Function) -> usize {
    // Collect (pred terminator, edge index, cold target, edge args)
    // first: new blocks are appended while rewriting.
    let mut edges = vec![];
    for b in func.layout.blocks() {
        if func.layout.is_cold(b) {
            continue;
        }
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        for (di, bc) in func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            let c = bc.block(&func.dfg.value_lists);
            if !func.layout.is_cold(c) || func.dfg.block_params(c).is_empty() {
                continue;
            }
            let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
            if args.iter().all(|a| matches!(a, BlockArg::Value(_)))
                && args.len() == func.dfg.block_params(c).len()
            {
                edges.push((t, di, c, args));
            }
        }
    }
    let mut n = 0;
    for (t, di, c, args) in edges {
        let a = func.dfg.make_block();
        func.layout.set_cold(a);
        func.layout.insert_block(a, c);
        FuncCursor::new(func).at_bottom(a).ins().jump(c, &args);
        let bc = func.dfg.block_call(a, &[]);
        let dfg = &mut func.dfg;
        let dests =
            dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables);
        if let Some(d) = dests.get_mut(di) {
            *d = bc;
        }
        n += 1;
    }
    n
}

/// An integer argument of a call in a cold block that is defined in a hot
/// block pins its virtual register to a fixed ABI register across the whole
/// hot region: the register allocator honors the call's operand constraint
/// at the value's only fixed use, so loop-carried operands like a
/// bounds-check `idx`/`len` get permanently assigned to x0/x1 and every
/// other user — and every backedge — pays a move to shuffle them. Rebind
/// each such arg to a fresh identity def inside the cold block: the copy
/// is still emitted, but at the call site in cold code instead of at
/// every hot use (LLVM places the same `mov`s in the panic block).
///
/// The wrap can't be `iadd x, 0`: cranelift's egraph folds it back to
/// `x` (`iadd_x_plus_zero`), re-attaching the fixed use to the hot
/// range. Instead each cold block materializes one disguised zero per
/// int type — `umulhi x, 0` has no `simplify`/`simplify_skeleton` rule
/// so it survives as an unprovable runtime-0 (`umulh` on aarch64) — and
/// every arg rebinds as `iadd x, z`, a single flagless `add` where the
/// old `sadd_overflow` scheme paid `adds` + a dead flag materialization
/// (`cset`/`seto`) per arg.
pub fn coldargs(func: &mut Function) -> usize {
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        if !func.layout.is_cold(b) {
            continue;
        }
        // Lazily-built `umulhi` zero per int type, shared by all calls
        // in the block.
        let mut zeros: FxHashMap<cranelift_codegen::ir::Type, Value> = FxHashMap::default();
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            if !func.dfg.insts[i].opcode().is_call() {
                continue;
            }
            let mut reb = vec![];
            for &a in func.dfg.inst_args(i) {
                let ty = func.dfg.value_type(a);
                if !ty.is_int() {
                    continue;
                }
                let hot = match func.dfg.value_def(a) {
                    ValueDef::Result(di, _) => func
                        .layout
                        .inst_block(di)
                        .is_some_and(|db| !func.layout.is_cold(db)),
                    // A block param's bundle merges with its incoming
                    // edge sources (typically hot values), so a
                    // fixed-reg call use on the param reaches back
                    // into the hot source bundle and fragments it.
                    ValueDef::Param(..) => true,
                    _ => false,
                };
                if hot {
                    reb.push((a, ty));
                }
            }
            if reb.is_empty() {
                continue;
            }
            let mut pairs = vec![];
            {
                let mut pos = FuncCursor::new(func).at_inst(i);
                for &(a, ty) in &reb {
                    let nv = if ty.bits() <= 64 {
                        let z = match zeros.get(&ty) {
                            Some(&z) => z,
                            None => {
                                let c0 = pos.ins().iconst(ty, 0);
                                let z = pos.ins().umulhi(a, c0);
                                zeros.insert(ty, z);
                                z
                            }
                        };
                        pos.ins().iadd(a, z)
                    } else {
                        // i128: no scalar `umulhi` lowering — keep the
                        // adds+dead-flag rebind. `iconst` has no i128
                        // form; a zero is `iconcat` of two i64 zeros.
                        let lo = pos.ins().iconst(types::I64, 0);
                        let hi = pos.ins().iconst(types::I64, 0);
                        let z = pos.ins().iconcat(lo, hi);
                        pos.ins().sadd_overflow(a, z).0
                    };
                    pairs.push((a, nv));
                }
            }
            for a in func.dfg.inst_args_mut(i) {
                for &(old, nv) in &pairs {
                    if *a == old {
                        *a = nv;
                    }
                }
            }
            n += pairs.len();
        }
    }
    n
}

/// Fold multi-pred forwarder blocks edgefwd leaves behind.
///
/// edgefwd retargets edges past `jump`-only forwarders but skips a block
/// wholesale when any of its params escape into a dominated block, and
/// never touches `brif` forwarders. Handle both leftovers:
///
/// - `b(p0..pk): jump T(a..)` with escaping params: retarget each pred
///   edge that still substitutes cleanly. This is unsafe only when a
///   param is used in a block reachable from T without passing through
///   b — the new edge would reach that use without the param being
///   defined. Uses whose only b-free access goes through b stay sound:
///   the traversal still enters via b, and if b loses every pred it is
///   removed along with everything it dominated.
/// - `b(p0..pk): brif c, T1(a..), T2(d..)`: a `jump`-terminated pred
///   `P: jump b(x..)` absorbs the branch, becoming
///   `P: brif c', T1(a'..), T2(d'..)` with b's params replaced by the
///   edge's args. This duplicates the conditional into each such pred
///   but removes the forwarder and its edge copies entirely — the same
///   fold LLVM's simplifycfg applies to trivial diamonds. Preds with
///   wider terminators (brif/br_table/try_call) can't absorb a second
///   edge and are left alone. Same escape rule, checked against both
///   targets' b-free reach.
///
/// Cold blocks are never folded: coldedges adapters are single-`jump`
/// cold blocks by construction and must survive to emission. Cold preds
/// may still absorb a hot forwarder (the marking follows the block).
/// Runs to a small fixpoint so chains (`P -> bF -> bG -> T`) collapse.
pub fn foldforwarders(func: &mut Function) -> usize {
    use cranelift_codegen::dominator_tree::DominatorTree;
    use cranelift_codegen::flowgraph::ControlFlowGraph;
    use cranelift_codegen::ir::{Block, BlockCall};
    use rustc_hash::FxHashMap;

    enum Rw {
        /// Retarget edge `di` of `pinst` to a new block call.
        Edge {
            pinst: Inst,
            di: usize,
            call: BlockCall,
        },
        /// Replace `pinst` (a `jump`) with a cloned `brif` in `pb`.
        Brif {
            pb: Block,
            pinst: Inst,
            data: InstructionData,
        },
    }

    // Substitute a forwarder-internal value `v` onto the edge
    // (pb, pinst): a forwarder param maps to that edge's arg at the same
    // position; anything else must be a `Value` already valid on the
    // edge — i.e. dominate the predecessor terminator.
    let subst = |func: &Function,
                 domtree: &DominatorTree,
                 params: &[Value],
                 pargs: &[BlockArg],
                 v: Value,
                 pb: Block,
                 pinst: Inst|
     -> Option<BlockArg> {
        let v = func.dfg.resolve_aliases(v);
        if let Some(k) = params.iter().position(|&p| p == v) {
            return Some(pargs[k]);
        }
        let ok = match func.dfg.value_def(v) {
            ValueDef::Result(i, _) => i != pinst && domtree.try_dominates(i, pinst, &func.layout),
            ValueDef::Param(d, _) => domtree.block_dominates(d, pb),
            _ => false,
        };
        ok.then_some(BlockArg::Value(v))
    };

    let subst_args = |func: &Function,
                      domtree: &DominatorTree,
                      params: &[Value],
                      pargs: &[BlockArg],
                      bc: BlockCall,
                      pb: Block,
                      pinst: Inst|
     -> Option<Vec<BlockArg>> {
        let pool = &func.dfg.value_lists;
        let mut out = Vec::new();
        for a in bc.args(pool) {
            match a {
                BlockArg::Value(v) => out.push(subst(func, domtree, params, pargs, v, pb, pinst)?),
                // Pseudo-values can't be introduced onto a `jump` edge,
                // and we only call this for jump preds anyway.
                _ => return None,
            }
        }
        Some(out)
    };

    let mut total = 0;
    for _ in 0..8 {
        let cfg = ControlFlowGraph::with_function(func);
        let domtree = DominatorTree::with_function(func, &cfg);
        let entry = func.layout.entry_block();
        // Every block that uses a param of another block. A forwarder
        // fold is unsafe if a new edge can reach such a use without
        // traversing the param's defining block: the use would lose its
        // definition. (Coarse: any param's escape use counts — a reach
        // path that still passes through the defining block is harmless
        // in principle, but that precision isn't worth it.)
        let mut escape_blocks: FxHashSet<Block> = FxHashSet::default();
        for x in func.layout.blocks() {
            for i in func.layout.block_insts(x) {
                let mut note = |v: Value| {
                    let v = func.dfg.resolve_aliases(v);
                    if let ValueDef::Param(d, _) = func.dfg.value_def(v)
                        && d != x
                    {
                        escape_blocks.insert(x);
                    }
                };
                for &v in func.dfg.inst_args(i) {
                    note(v);
                }
                for bc in func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    for a in bc.args(&func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a {
                            note(v);
                        }
                    }
                }
            }
        }
        // True if folding `b` is unsafe: some escaping-param use is
        // reachable from `targets` without traversing b, so the new
        // edges could reach that use while its param is undefined.
        // Cheap when nothing escapes at all.
        let escapes_hazard =
            |cfg: &ControlFlowGraph, b: Block, targets: &[Block], esc: &FxHashSet<Block>| {
                if esc.is_empty() {
                    return false;
                }
                let mut reach = FxHashSet::default();
                let mut work: Vec<Block> = targets.to_vec();
                while let Some(x) = work.pop() {
                    if x == b || !reach.insert(x) {
                        continue;
                    }
                    for s in cfg.succ_iter(x) {
                        work.push(s);
                    }
                }
                reach.iter().any(|u| esc.contains(u))
            };
        let mut rws: Vec<Rw> = Vec::new();
        // A `jump` can only carry one edge, so at most one Brif rewrite
        // per inst — dedup defensively.
        let mut brifed: FxHashMap<Inst, ()> = FxHashMap::default();
        for b in func.layout.blocks().collect::<Vec<_>>() {
            if Some(b) == entry || func.layout.is_cold(b) {
                continue;
            }
            let insts: Vec<Inst> = func.layout.block_insts(b).collect();
            if insts.len() != 1 {
                continue;
            }
            let term = insts[0];
            let params = func.dfg.block_params(b).to_vec();
            match func.dfg.insts[term] {
                InstructionData::Jump { destination, .. } => {
                    let t = destination.block(&func.dfg.value_lists);
                    if t == b {
                        continue;
                    }
                    let mut targs = Vec::new();
                    let mut ok = true;
                    for a in destination.args(&func.dfg.value_lists) {
                        match a {
                            BlockArg::Value(v) => targs.push(func.dfg.resolve_aliases(v)),
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if !ok || targs.len() != func.dfg.num_block_params(t) {
                        continue;
                    }
                    if escapes_hazard(&cfg, b, &[t], &escape_blocks) {
                        continue;
                    }
                    for (pb, pinst) in cfg
                        .pred_iter(b)
                        .map(|p| (p.block, p.inst))
                        .collect::<Vec<_>>()
                    {
                        if pb == b {
                            continue;
                        }
                        let dests = func.dfg.insts[pinst]
                            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                            .to_vec();
                        for (di, bc) in dests.iter().enumerate() {
                            if bc.block(&func.dfg.value_lists) != b {
                                continue;
                            }
                            let pargs: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                            if pargs.len() != params.len() {
                                continue;
                            }
                            let mut new = Vec::with_capacity(targs.len());
                            let mut ok = true;
                            for &v in &targs {
                                if let Some(k) = params.iter().position(|&p| p == v) {
                                    // Verbatim: valid on this same edge
                                    // even if a try_call pseudo-value.
                                    new.push(pargs[k]);
                                    continue;
                                }
                                let dom = match func.dfg.value_def(v) {
                                    ValueDef::Result(i, _) => {
                                        i != pinst && domtree.try_dominates(i, pinst, &func.layout)
                                    }
                                    ValueDef::Param(d, _) => domtree.block_dominates(d, pb),
                                    _ => false,
                                };
                                if !dom {
                                    ok = false;
                                    break;
                                }
                                new.push(BlockArg::Value(v));
                            }
                            if !ok {
                                continue;
                            }
                            let call = BlockCall::new(t, new, &mut func.dfg.value_lists);
                            rws.push(Rw::Edge { pinst, di, call });
                        }
                    }
                }
                InstructionData::Brif { arg: c, blocks, .. } => {
                    let t1 = blocks[0].block(&func.dfg.value_lists);
                    let t2 = blocks[1].block(&func.dfg.value_lists);
                    if escapes_hazard(&cfg, b, &[t1, t2], &escape_blocks) {
                        continue;
                    }
                    for (pb, pinst) in cfg
                        .pred_iter(b)
                        .map(|p| (p.block, p.inst))
                        .collect::<Vec<_>>()
                    {
                        if pb == b || brifed.contains_key(&pinst) {
                            continue;
                        }
                        // Only a lone `jump` pred can absorb the two-edge
                        // branch without growing a fresh block.
                        let InstructionData::Jump {
                            destination: pd, ..
                        } = func.dfg.insts[pinst]
                        else {
                            continue;
                        };
                        if pd.block(&func.dfg.value_lists) != b {
                            continue;
                        }
                        let pargs: Vec<BlockArg> = pd.args(&func.dfg.value_lists).collect();
                        if pargs.len() != params.len() {
                            continue;
                        }
                        let Some(BlockArg::Value(cv)) =
                            subst(func, &domtree, &params, &pargs, c, pb, pinst)
                        else {
                            continue;
                        };
                        let mut nb = Vec::with_capacity(2);
                        let mut ok = true;
                        for bc in blocks {
                            let Some(new) =
                                subst_args(func, &domtree, &params, &pargs, bc, pb, pinst)
                            else {
                                ok = false;
                                break;
                            };
                            nb.push(BlockCall::new(
                                bc.block(&func.dfg.value_lists),
                                new,
                                &mut func.dfg.value_lists,
                            ));
                        }
                        if !ok || nb.len() != 2 {
                            continue;
                        }
                        let data = InstructionData::Brif {
                            opcode: Opcode::Brif,
                            arg: cv,
                            blocks: [nb[0], nb[1]],
                        };
                        rws.push(Rw::Brif { pb, pinst, data });
                        brifed.insert(pinst, ());
                    }
                }
                _ => continue,
            }
        }
        if rws.is_empty() {
            break;
        }
        let mut applied = 0;
        for rw in rws {
            match rw {
                Rw::Edge { pinst, di, call } => {
                    let dfg = &mut func.dfg;
                    let dests = dfg.insts[pinst]
                        .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables);
                    if let Some(d) = dests.get_mut(di) {
                        *d = call;
                        applied += 1;
                    }
                }
                Rw::Brif { pb, pinst, data } => {
                    let ni = func.dfg.make_inst(data);
                    func.layout.remove_inst(pinst);
                    func.layout.append_inst(ni, pb);
                    applied += 1;
                }
            }
        }
        total += applied;
        if applied == 0 {
            break;
        }
    }
    crate::jumpthread::remove_unreachable_blocks(func);
    total
}

/// Splice a `jump`'s target into its predecessor when the target has no
/// other predecessors. Cranelift's register allocator splits live ranges
/// at block boundaries, so straight-line code chopped into blocks (e.g.
/// bcheck's versioned clones) pays a parallel-copy shuffle on every edge.
/// A block can only have one terminator, so only unconditional edges can
/// be merged: move the target's instructions in, alias its params to the
/// edge args, and the per-edge copies disappear. Repeat to fixpoint: each
/// fusion can expose the next block of the same chain.
pub fn fusechains(func: &mut Function) -> usize {
    use rustc_hash::{FxHashMap, FxHashSet};
    let mut n = 0;
    // Incoming-edge count per block (self-edges included). Fusion only
    // rewires the fusing block's terminator and removes the fused block
    // (whose sole pred edge came from that terminator), so every live
    // block's pred count is invariant under a fusion — one count pass
    // suffices.
    let mut preds: FxHashMap<cranelift_codegen::ir::Block, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        let Some(lt) = func.layout.last_inst(b) else {
            continue;
        };
        for bc in
            func.dfg.insts[lt].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        {
            *preds.entry(bc.block(&func.dfg.value_lists)).or_default() += 1;
        }
    }
    // Snapshot the order once: removed blocks yield `last_inst() == None`
    // and are skipped. Each fusion swaps b's terminator for t's, so
    // re-check b until it no longer ends in a fusable jump — this
    // collapses forward chains in one pass instead of restarting a whole
    // layout scan per fusion (O(fusions × blocks) on EH-inflated CFGs).
    for b in func.layout.blocks().collect::<Vec<_>>() {
        while !func.layout.is_cold(b) {
            let Some(lt) = func.layout.last_inst(b) else {
                break;
            };
            if func.dfg.insts[lt].opcode() != Opcode::Jump {
                break;
            }
            let dests = func.dfg.insts[lt]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .to_vec();
            let pick = dests.iter().find(|bc| {
                let t = bc.block(&func.dfg.value_lists);
                t != b && !func.layout.is_cold(t) && preds.get(&t).copied().unwrap_or(0) == 1
            });
            let Some(bc) = pick else { break };
            let t = bc.block(&func.dfg.value_lists);
            // Edge args must all be plain values (no try_call exn markers)
            // so the successor's params can be aliased to them.
            let params = func.dfg.block_params(t).to_vec();
            let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
            let mut pairs = Vec::with_capacity(args.len());
            let mut ok = args.len() == params.len();
            for (p, a) in params.iter().zip(args) {
                if let BlockArg::Value(v) = a {
                    pairs.push((*p, v));
                } else {
                    ok = false;
                    break;
                }
            }
            // An arg that's one of t's own params means the edge loops back
            // from a block t dominates (dead t->b->t cycle, or a param
            // permutation like `jump t(p2, p1)`): aliasing a param to itself
            // or to a sibling creates an alias cycle, and any use of that
            // param in b would dangle once t is removed.
            let pset: FxHashSet<_> = params.iter().copied().collect();
            if !ok
                || pairs
                    .iter()
                    .any(|&(_, v)| pset.contains(&func.dfg.resolve_aliases(v)))
            {
                break;
            }
            // Detach t's params so they can be aliased to the edge args;
            // every use of a param (in t or anywhere t dominated) then
            // resolves to the value the predecessor passed.
            func.dfg.detach_block_params(t);
            for (p, v) in pairs {
                func.dfg.change_to_alias(p, v);
            }
            let tinsts: Vec<Inst> = func.layout.block_insts(t).collect();
            func.layout.remove_inst(lt);
            for i in tinsts {
                func.layout.remove_inst(i);
                func.layout.append_inst(i, b);
            }
            func.layout.remove_block(t);
            n += 1;
        }
    }
    n
}

/// Block params that receive the same value on every incoming edge are
/// pure copy overhead: regalloc materializes a parallel copy per arg on
/// each conditional pred (that's most of the `mov;b` edge-split blocks).
/// When every pred passes `v` for param `p`, `v` is defined on every pred
/// edge and so dominates the block: drop `p`, rewrite its uses to `v`,
/// and strip that arg slot from every edge — an arg-free conditional edge
/// needs no split at all. Duplicate params (the same arg value on every
/// edge) merge the same way. Jumpthread/merge-shaped CFGs leave thousands
/// of these; iterate to a fixpoint since each drop can expose more.
pub fn sameargs(func: &mut Function) -> (usize, FxHashMap<Value, Value>) {
    let mut n = 0;
    let mut pmap: FxHashMap<Value, Value> = FxHashMap::default();
    // `webmerge` rounds only rewrite values — topology-derived state
    // (cfg/dom/edges) is round-invariant, so it is built once here
    // rather than per call.
    let mut wctx = WmCtx::new(func);
    // A round's merges rebind only values inside the merged blocks'
    // dominated regions — the next round's scan is confined to those
    // regions plus their out-edge targets (`merge_dirty`).
    let mut dirty: Option<FxHashSet<Block>> = None;
    for _ in 0..8 {
        // `webmerge` subsumes the old `sameargs_round`/`phimerge`/
        // `affparm` trio: phase A is the uniform-arg merge (with
        // `emit_key` covering `sameargs`' iconst materialization),
        // phase B the duplicate-param merge, phases C/D the rest.
        // Looping webmerge alone still reaches the cross-block fixpoint
        // — each drop can expose uniform args in successor blocks.
        let (k, pm, merged) = webmerge_ctx(func, &mut wctx, dirty.as_ref());
        dirty = Some(merge_dirty(&wctx, &merged));
        n += k;
        if !pm.is_empty() {
            // Compose into the accumulated map: old carriers may
            // re-drop in this round (chase through `pm`), new carriers
            // may already be drops (chase through `pmap`). Memoized
            // resolution — the per-entry bounded walks made this
            // O(|pm|·|pmap|) per round on functions that drop many
            // params. A cyclic web (`a→b` here, `b→a` earlier) still
            // terminates: an on-path revisit stops the chase.
            let mut memo: FxHashMap<Value, Value> = FxHashMap::default();
            let mut onpath: FxHashSet<Value> = FxHashSet::default();
            let mut path: Vec<Value> = Vec::new();
            let mut resolve = |v0: Value, pmap: &FxHashMap<Value, Value>| -> Value {
                if let Some(&r) = memo.get(&v0) {
                    return r;
                }
                let mut v = v0;
                let r = loop {
                    if let Some(&r) = memo.get(&v) {
                        break r;
                    }
                    if onpath.contains(&v) {
                        break v;
                    }
                    match pm.get(&v).or_else(|| pmap.get(&v)) {
                        Some(&w) => {
                            onpath.insert(v);
                            path.push(v);
                            v = w;
                        }
                        None => break v,
                    }
                };
                for &x in &path {
                    onpath.remove(&x);
                    memo.insert(x, r);
                }
                path.clear();
                memo.insert(v0, r);
                r
            };
            let pairs: Vec<(Value, Value)> = pmap.iter().map(|(&x, &v)| (x, v)).collect();
            for (x, v0) in pairs {
                let w = resolve(v0, &pmap);
                pmap.insert(x, w);
            }
            for (&x, &v) in &pm {
                let w = resolve(v, &pmap);
                pmap.insert(x, w);
            }
        }
        if k == 0 {
            break;
        }
    }
    if crate::pass_enabled("PLIRON_CSWEEP") {
        n += const_sweep(func);
    }
    (n, pmap)
}

/// A `brif` arm to a small `return` block whose sibling arm is a loop header
/// gets its own copy of the return placed right after the source: the loop
/// edge becomes the single taken `jcc` back and the exit falls through.
/// Without the copy the exit shares a return block that RPO can't always
/// place adjacent, and every iteration pays `jcc` + `jmp`. The shared
/// return itself is marked cold for its other preds.
pub fn coldrets(func: &mut Function) -> usize {
    fn leaf_ret(func: &Function, b: Block) -> bool {
        let mut k = 0;
        for i in func.layout.block_insts(b) {
            if func.dfg.insts[i].opcode() == Opcode::Return {
                return k <= 8;
            }
            k += 1;
        }
        false
    }
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(func);
    let domtree = cranelift_codegen::dominator_tree::DominatorTree::with_function(func, &cfg);
    let mut mark = Vec::new();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { blocks, .. } = &func.dfg.insts[t] else {
            continue;
        };
        for side in 0..2 {
            let (t, o) = (
                blocks[side].block(&func.dfg.value_lists),
                blocks[1 - side].block(&func.dfg.value_lists),
            );
            if t == o
                || func.layout.is_cold(t)
                || func.layout.is_cold(o)
                || !leaf_ret(func, t)
                || leaf_ret(func, o)
            {
                continue;
            }
            // `o` is a loop header when some predecessor (itself included,
            // for a self-loop) is dominated by `o`.
            if cfg
                .pred_iter(o)
                .any(|p| domtree.block_dominates(o, p.block))
            {
                mark.push((b, t, side, domtree.block_dominates(o, b)));
            }
        }
    }
    let n = mark.len();
    for (b, t, side, inloop) in mark {
        func.layout.set_cold(t);
        if !inloop {
            // `b` is a pre-loop guard, not a latch: the cold mark alone gets
            // the layout right (loop arm stays adjacent fall-through).
            continue;
        }
        // Copy `t` into a fresh block right after `b`, binding `t`'s params
        // to this edge's args, and retarget the arm.
        let nt = func.dfg.make_block();
        let mut data = func.dfg.insts[func.layout.last_inst(b).unwrap()];
        let InstructionData::Brif { blocks, .. } = &mut data else {
            unreachable!()
        };
        let args: Vec<Value> = blocks[side]
            .args(&func.dfg.value_lists)
            .filter_map(|a| match a {
                BlockArg::Value(v) => Some(v),
                _ => None,
            })
            .collect();
        let mut map: FxHashMap<Value, Value> = func
            .dfg
            .block_params(t)
            .iter()
            .copied()
            .zip(args.iter().copied())
            .collect();
        let insts: Vec<Inst> = func.layout.block_insts(t).collect();
        func.layout.insert_block_after(nt, b);
        for i in insts {
            let ni = func.dfg.clone_inst(i);
            let dfg = &mut func.dfg;
            let mut d = dfg.insts[ni];
            for dst in d.branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables) {
                *dst = dst.deep_clone(&mut dfg.value_lists);
            }
            dfg.insts[ni] = d;
            let vals: Vec<Value> = func
                .dfg
                .inst_values(ni)
                .map(|v| *map.get(&func.dfg.resolve_aliases(v)).unwrap_or(&v))
                .collect();
            func.dfg.overwrite_inst_values(ni, vals.into_iter());
            for (r, nr) in func
                .dfg
                .inst_results(i)
                .iter()
                .copied()
                .zip(func.dfg.inst_results(ni).iter().copied())
            {
                map.insert(r, nr);
            }
            func.layout.append_inst(ni, nt);
        }
        let InstructionData::Brif { blocks, .. } = &mut data else {
            unreachable!()
        };
        blocks[side] = BlockCall::new(nt, std::iter::empty(), &mut func.dfg.value_lists);
        let tinst = func.layout.last_inst(b).unwrap();
        func.dfg.insts[tinst] = data;
        // Keep the loop arm on the `then` (jcc) slot so the back-edge is the
        // single taken branch and `nt` is the fall-through: `brif c, cold-nt,
        // loop` emits `jcc` fwd + `jmp` back otherwise.
        if side == 0
            && let InstructionData::Brif { arg, .. } = func.dfg.insts[tinst]
            && crate::looprot::uses(func, arg) == 1
            && let Some(ci) = func.dfg.resolved_value_def(arg).inst()
            && let InstructionData::IntCompare { cond, .. } = &mut func.dfg.insts[ci]
        {
            *cond = cond.complement();
            let mut data = func.dfg.insts[tinst];
            let InstructionData::Brif { blocks, .. } = &mut data else {
                unreachable!()
            };
            blocks.swap(0, 1);
            func.dfg.insts[tinst] = data;
        }
    }
    n
}

/// Narrow const-param fold ahead of the second ifconv round: drop a
/// block param only when every non-self predecessor edge feeds it the
/// same `iconst` (`RangeInclusive`'s latch-carried exhausted flag).
/// Constant `br_table`/`brif` dispatch then folds via `const_sweep`,
/// exposing the count-if merge `ifconv` flattens. Deliberately skips
/// `sameargs`'s uniform-value and duplicate-param merges — running
/// those this early rewrites `chars_alpha_ascii`'s decoder dispatch
/// (~70% regression) before the branches it depends on have settled.
/// Uniform interior params (`block20`'s fed `block18`'s fed the loop
/// header's) are resolved transitively for the constness check and
/// rewritten to the constant, but keep their edge args — no arity
/// changes outside the folded headers.
pub fn cparm(func: &mut Function) -> (usize, FxHashMap<Value, Value>) {
    let mut n = 0;
    let mut pmap: FxHashMap<Value, Value> = FxHashMap::default();
    // Topology-derived `webmerge` state is round-invariant, but
    // `const_sweep` inside the loop folds `brif`s to `jump`s — rebuild
    // it whenever a sweep changes the edge list.
    let mut wctx = WmCtx::new(func);
    let mut dirty: Option<FxHashSet<Block>> = None;
    for _ in 0..8 {
        // `cparm_round`'s carried-const chase is `webmerge` iterated:
        // each uniform-param link drops via phase A (the const leaf
        // materializes through `emit_key`), exposing the next link.
        let (k, pm, merged) = webmerge_ctx(func, &mut wctx, dirty.as_ref());
        dirty = Some(merge_dirty(&wctx, &merged));
        n += k;
        for v in pmap.values_mut() {
            while let Some(&w) = pm.get(v) {
                *v = w;
            }
        }
        for (&x, &v) in &pm {
            let mut v = v;
            while let Some(&w) = pm.get(&v).or_else(|| pmap.get(&v)) {
                v = w;
            }
            pmap.insert(x, v);
        }
        if k == 0 {
            break;
        }
        if crate::pass_enabled("PLIRON_CSWEEP") {
            let cs = const_sweep(func);
            n += cs;
            if cs > 0 {
                wctx = WmCtx::new(func);
                // Folded branches change the edge list — the dirty set
                // was computed against the old topology.
                dirty = None;
            }
        }
    }
    (n, pmap)
}

/// Shared commit for the param-merge passes: flatten aliases,
/// chase replacements to survivors, rewrite all uses, strip the dropped
/// arg slots off every edge, drop the params.
pub(crate) fn apply_param_drops(
    func: &mut Function,
    mut pmap: FxHashMap<Value, Value>,
    mut arg_drops: Vec<(Inst, usize, Vec<usize>)>,
    mut param_drops: Vec<Value>,
) -> usize {
    // Flatten alias chains into inst data first: a `change_to_alias`'d
    // value (coldargs' `sadd_overflow` rebinds, deflag results) can point
    // at a param we're about to detach — uses through it wouldn't be
    // rewritten and the alias would dangle.
    func.dfg.resolve_all_aliases();
    // A replacement may be a param of another block that's itself being
    // dropped — `a -> b` in one block and `b -> a` in another form a
    // cycle with no surviving value, and chasing into it leaves `w` on a
    // param that's then detached under remaining uses (stage2 ICE'd on
    // `argument v… -> v… is not attached` in rustc_middle /
    // rustc_trait_selection / rustc_ty_utils). Keep the cyclic params:
    // they drop nothing but still terminate other keys' chases.
    let mut cyc = FxHashSet::<Value>::default();
    for &p in pmap.keys() {
        let mut w = p;
        let mut path = vec![p];
        while let Some(&u) = pmap.get(&w) {
            w = u;
            if let Some(at) = path.iter().position(|&x| x == w) {
                cyc.extend(&path[at..]);
                break;
            }
            path.push(w);
        }
    }
    if !cyc.is_empty() {
        pmap.retain(|p, _| !cyc.contains(p));
        param_drops.retain(|p| !cyc.contains(p));
        for (i, d, ixs) in &mut arg_drops {
            let b = func.dfg.insts[*i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[*d]
                .block(&func.dfg.value_lists);
            ixs.retain(|&ix| !cyc.contains(&func.dfg.block_params(b)[ix]));
        }
    }
    // Replacements may themselves be dropped params: chase to a survivor.
    let keys: Vec<Value> = pmap.keys().copied().collect();
    for p in keys {
        let mut w = pmap[&p];
        let mut seen: FxHashSet<Value> = FxHashSet::default();
        while let Some(&u) = pmap.get(&w) {
            if !seen.insert(w) {
                break;
            }
            w = u;
        }
        pmap.insert(p, w);
    }
    // One global use rewrite (inst args and every BlockCall's args).
    for b in func.layout.blocks().collect::<Vec<_>>() {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let dfg = &mut func.dfg;
            let mut data = dfg.insts[i];
            data.map_values(
                &mut dfg.value_lists,
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
                |x| pmap.get(&x).copied().unwrap_or(x),
            );
            dfg.insts[i] = data;
        }
    }
    // Strip the dropped arg slots from every pred edge (descending so
    // earlier indices stay valid), then drop the params themselves.
    for (i, d, ixs) in arg_drops {
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[i]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[d];
        for &ix in &ixs {
            bc.remove(ix, &mut dfg.value_lists);
        }
    }
    for p in &param_drops {
        func.dfg.remove_block_param(*p);
    }
    param_drops.len()
}

/// Fold pure insts whose inputs evaluate to constants — the const-param
/// materialization in `webmerge` leaves `uextend`/`band`/`select`
/// chains on literal tags (`Chars`'s `Option<char>` flag), and `eval`
/// walks them transitively. Constant `brif`s become `jump`s via
/// `fold_const_branches`, then dead blocks are swept.
pub fn const_sweep(func: &mut Function) -> usize {
    let mut env: FxHashMap<Value, u64> = FxHashMap::default();
    let mut hits = Vec::new();
    // `stack_addr`/`dynamic_stack_addr` are never null — hashbrown's
    // `ctrl.is_null()` empty-table guard on a `&RawTable` local compares
    // the `stack_addr` against 0, and the surviving `select c, 0, p`
    // keeps the slot's memory ops keyed under a second, opaque root.
    // (`func_addr`/`symbol_value` are not counted: weak externs resolve
    // to 0.)
    let nz = |v: Value| {
        func.dfg.resolved_value_def(v).inst().is_some_and(|d| {
            matches!(
                func.dfg.insts[d].opcode(),
                Opcode::StackAddr | Opcode::DynamicStackAddr
            )
        })
    };
    let rg = |_: Value| None;
    let mut sels = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let rs = func.dfg.inst_results(i);
            if rs.len() == 1
                && let Some(k) = crate::jumpthread::eval(func, &env, &nz, &rg, i)
            {
                let ty = func.dfg.value_type(rs[0]);
                // `eval` returns raw 64 bits; store the value masked to
                // the result width and emit the iconst sign-extended —
                // narrow immediates (`iconst.i8 -1`) need both to stay
                // canonical.
                let bits = ty.bits();
                let k = if bits >= 64 { k } else { k & ((1 << bits) - 1) };
                let s = if bits >= 64 {
                    k as i64
                } else {
                    ((k << (64 - bits)) as i64) >> (64 - bits)
                };
                hits.push((i, ty, s));
                env.insert(rs[0], k);
                continue;
            }
            // `select (const c), a, b` → the decided arm, when the arm
            // itself isn't a constant `eval` can reach.
            if let InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            } = func.dfg.insts[i]
                && let Some(c) = crate::jumpthread::known(func, &env, args[0])
            {
                sels.push((i, rs[0], args[if c != 0 { 1 } else { 2 }]));
            }
        }
    }
    let mut n = hits.len() + sels.len();
    for (i, ty, k) in hits {
        func.replace(i).iconst(ty, k);
    }
    for (i, res, arm) in sels {
        let arm = func.dfg.resolve_aliases(arm);
        func.dfg.clear_results(i);
        func.layout.remove_inst(i);
        func.dfg.change_to_alias(res, arm);
    }
    if n > 0 {
        // Replacements can expose constant branch conditions.
        let f = crate::jumpthread::fold_const_branches(func);
        if f > 0 {
            crate::unreach::run(func);
        }
        n += f;
    }
    n
}

/// `v` usable at the top of `b`: defined in a block strictly dominating
/// `b`. Excluding `b` itself also excludes self-edge operands bound by
/// `b`'s params — `iadd p1, x` as a self-edge arg is next-iteration's
/// value, not the value on entry, so it must not hoist.
fn dominates_entry(
    func: &Function,
    dom: &cranelift_codegen::dominator_tree::DominatorTree,
    v: Value,
    b: Block,
) -> bool {
    let db = match func.dfg.value_def(v) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(pb, _) => Some(pb),
        ValueDef::Union(_, _) => None,
    };
    matches!(db, Some(db) if db != b && dom.block_dominates(db, b))
}

/// Phi merge through shared-successor congruence — the phi-web merge.
/// Every in-edge of param `p` supplies a value with the same
/// `proots::Key`: a literal (`sameargs`' value case), a const (its K
/// case), a `root + k` affine image (affmerge's class), or a pure-op
/// tree congruent by construction — `key_of` normalizes operands through
/// the `off` web, so `icmp (iadd x, 8), c` edges whose adds are per-edge
/// clones still land in one class (a transitive `arghoist`). The param
/// drops and its uses rebind to a class member that dominates the head
/// when one exists, else to the key re-materialized there (`emit_key`).
/// Returns `(merges, pmap)` — `pmap` maps each dropped param to the
/// carrier its uses rebound to. The pipeline uses it to carry
/// `derived`-pointer facts across the merge.
/// Topology-derived state shared across `webmerge` rounds. A round only
/// rewrites values — drops args, drops block params, inserts
/// non-terminator insts — so block succ/pred structure and the
/// inst→dest edge list are round-invariant. Fixpoint loops (`sameargs`,
/// `cparm`) build this once instead of paying O(E) per round.
pub struct WmCtx {
    cfg: cranelift_codegen::flowgraph::ControlFlowGraph,
    dom: cranelift_codegen::dominator_tree::DominatorTree,
    edges: FxHashMap<Block, Vec<(Inst, usize)>>,
}

impl WmCtx {
    pub fn new(func: &Function) -> Self {
        let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(func);
        let dom = cranelift_codegen::dominator_tree::DominatorTree::with_function(func, &cfg);
        let mut edges: FxHashMap<Block, Vec<(Inst, usize)>> = FxHashMap::default();
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                for (d, bc) in func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .iter()
                    .enumerate()
                {
                    edges
                        .entry(bc.block(&func.dfg.value_lists))
                        .or_default()
                        .push((i, d));
                }
            }
        }
        WmCtx { cfg, dom, edges }
    }
}

/// `k`'s subtree set as `khash_m` values — the necessary-condition
/// index `webmerge`'s phase-D sibling probes filter on: `key_sub`
/// only fires between keys sharing a subtree.
fn subtree_hashes(
    k: &crate::proots::Key,
    hm: &mut FxHashMap<*const crate::proots::Key, u64>,
    seen: &mut FxHashSet<*const crate::proots::Key>,
    out: &mut FxHashSet<u64>,
) {
    use crate::proots::Key;
    if !seen.insert(k as *const Key) {
        return;
    }
    out.insert(crate::proots::khash_m(k, hm));
    match k {
        Key::A(_, b, _) => subtree_hashes(b, hm, seen, out),
        Key::N(_, _, args, _) => {
            for a in args.iter() {
                subtree_hashes(a, hm, seen, out);
            }
        }
        _ => {}
    }
}

pub fn webmerge(func: &mut Function) -> (usize, FxHashMap<Value, Value>) {
    let mut ctx = WmCtx::new(func);
    let (n, pmap, _) = webmerge_ctx(func, &mut ctx, None);
    (n, pmap)
}

/// Blocks a `webmerge_ctx` round can have changed the merge inputs of:
/// the merged blocks, every block they dominate (a dropped param or a
/// materialized carrier can only be *used* under dominator scoping),
/// and the immediate out-edge targets whose arg slots the global
/// `pmap` rewrite may have rewritten.
fn merge_dirty(ctx: &WmCtx, merged: &FxHashSet<Block>) -> FxHashSet<Block> {
    let mut d: FxHashSet<Block> = FxHashSet::default();
    let mut stack: Vec<Block> = merged.iter().copied().collect();
    while let Some(x) = stack.pop() {
        if d.insert(x) {
            stack.extend(ctx.dom.children(x));
        }
    }
    let region: Vec<Block> = d.iter().copied().collect();
    for x in region {
        for s in ctx.cfg.succ_iter(x) {
            d.insert(s);
        }
    }
    d
}

/// `dirty`, when `Some`, restricts the scan to those blocks — used by
/// the fixpoint callers with `merge_dirty`. A merge at `b` rewrites
/// only values dominated by `b` (SSA scoping bounds every use of a
/// dropped param and of anything defined under `b`), plus arg slots on
/// edges leaving that region, so a block outside `dom(b) ∪
/// succs(dom(b))` sees identical inputs and would make identical
/// decisions. The third return element is the set of blocks that
/// actually merged this round — feed it to `merge_dirty`.
pub fn webmerge_ctx(
    func: &mut Function,
    ctx: &mut WmCtx,
    dirty: Option<&FxHashSet<Block>>,
) -> (usize, FxHashMap<Value, Value>, FxHashSet<Block>) {
    use crate::proots::Key;
    // Flat `edges × params` matrix: `m[e]` yields the row slice, so the
    // `m[e][ix]` indexing below reads exactly like the `Vec<Vec>` it
    // replaced — one allocation per matrix instead of one per edge.
    struct Mat<T> {
        stride: usize,
        v: Vec<T>,
    }
    impl<T> Mat<T> {
        fn rows(&self) -> std::slice::ChunksExact<'_, T> {
            self.v.chunks_exact(self.stride)
        }
    }
    impl<T> std::ops::Index<usize> for Mat<T> {
        type Output = [T];
        fn index(&self, e: usize) -> &[T] {
            &self.v[e * self.stride..(e + 1) * self.stride]
        }
    }
    // `var_os` reads /proc-level state per call — hoisted out of the
    // per-block loop (16k lookups per webmerge round otherwise). Every
    // clock read in this function only feeds these `wmprof` log lines,
    // so they're all gated on it.
    let wmprof = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::clifpeep", tracing::Level::INFO);
    let t0 = wmprof.then(std::time::Instant::now);
    let WmCtx { cfg, dom, edges } = ctx;
    // One edge-fact scan feeds both the web normal forms and the
    // canonical-class proots — nothing mutates between them.
    let facts = crate::proots::facts_cfg(func, cfg);
    let nf = crate::proots::web_nf_facts(func, &facts);
    let t2 = wmprof.then(std::time::Instant::now);
    let canon = crate::proots::canon_map_facts(&facts);
    let t3 = wmprof.then(std::time::Instant::now);
    if let (Some(t0), Some(t2), Some(t3)) = (t0, t2, t3) {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::clifpeep", "webmerge {}: web_nf {:?} canon {:?}",
                func.name,
                t2 - t0,
                t3 - t2
            )
        };
    }
    // Keys memoize across args — shared operand trees key once.
    // `RefCell` because `skey_of`/`pk_of` materialize lazily from inside
    // immutable borrows of the cell matrices.
    let memo = std::cell::RefCell::new(FxHashMap::<Value, crate::proots::Key>::default());
    let mut vhmemo: FxHashMap<Value, u64> = FxHashMap::default();
    let vnmemo = std::cell::RefCell::new(FxHashMap::<Value, usize>::default());
    let mut pmap: FxHashMap<Value, Value> = FxHashMap::default();
    let mut param_drops: Vec<Value> = vec![];
    let mut drop_ixs: FxHashMap<(Inst, usize), Vec<usize>> = FxHashMap::default();
    let mut nblk = 0usize;
    let mut tkeys_sum = std::time::Duration::ZERO;
    let mut tblk_sum = std::time::Duration::ZERO;
    // (target block, class key, param to rebind) — emission happens after
    // the scan so `Value`s stay valid and insertions keep operand order.
    let mut inserts: Vec<(Block, Key, Value)> = vec![];
    let twloop = wmprof.then(std::time::Instant::now);
    let mut tpre_sum = std::time::Duration::ZERO;
    let wm_nodiffb = pliron_cfg::env_set("PLIRON_WM_NODIFFB");
    let wm_rel = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::clifpeep", tracing::Level::DEBUG);
    // `env::var_os` takes the process env lock per call — keep reads out
    // of the per-block loop (they showed up as ~15% of codegen samples).
    let no_phased = pliron_cfg::env_set("PLIRON_WM_NOPHASED");
    let no_phasec = pliron_cfg::env_set("PLIRON_WM_NOPHASEC");
    let no_phaseb = pliron_cfg::env_set("PLIRON_WM_NOPHASEB");
    let dirty = if pliron_cfg::env_set("PLIRON_WM_NODIRTY") {
        None
    } else {
        dirty
    };
    // The per-block clocks only feed `wmprof` diagnostics — don't pay
    // two syscalls per block per round when it's off.
    let el = |t: Option<std::time::Instant>| t.map_or(std::time::Duration::ZERO, |t| t.elapsed());
    for (&b, es) in edges.iter() {
        let tb0 = wmprof.then(std::time::Instant::now);
        if dirty.is_some_and(|d| !d.contains(&b)) {
            tpre_sum += el(tb0);
            continue;
        }
        let ps = func.dfg.block_params(b).to_vec();
        if ps.is_empty() {
            tpre_sum += el(tb0);
            continue;
        }
        // `diff_b` re-proves the same (x, y) pairs across oracle calls
        // and join paths — two-level memo: head-independent results
        // shared by every query, head-dependent ones per (pi, pj).
        // Scoped per target block: a chase node on *this* block's param
        // pairs is head-sensitive, so entries must not leak into the
        // next block's queries.
        let dbmemo = std::cell::RefCell::new(crate::indvars::DbMemo::default());
        // The `skc`/`pk`/`eargs` matrices below are edges × params —
        // ~50B/cell even while the `Key`s stay lazy, so a megafunction
        // block (thousands of wide params × thousands of preds) spills
        // to tens of GB and OOMs (stage1 `test::shuffle_tests`). The
        // widest joins are also the least valuable merges; skip them.
        if wmprof {
            let rss = std::fs::read_to_string("/proc/self/statm")
                .ok()
                .and_then(|s| s.split_whitespace().nth(1).map(|x| x.to_string()))
                .unwrap_or_default();
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::clifpeep", "wmb {} {b} E={} P={} rss={}",
                    func.name,
                    es.len(),
                    ps.len(),
                    rss
                )
            };
        }
        if es.len().saturating_mul(ps.len()) > 4_000_000 {
            tpre_sum += el(tb0);
            continue;
        }
        tpre_sum += el(tb0);
        // Per-edge resolved args: `eargs[e][ix]` for `Value` args,
        // `svh[e][ix]` for their structural hash — pseudo-args
        // (`TryCallRet`/`TryCallExn`) are `None` in `eargs` but key as
        // `O`, so two slots fed the same pseudo-arg dedup via `P` while
        // the feeding inst in the key keeps distinct `try_call`s
        // distinct. Congruence keys themselves stay *lazy* — `skey_of`
        // materializes on demand: hashing the def-DAG directly via
        // `vhash` skips building ~10⁵-node `Key` trees for slots that
        // never merge (unrolled hash kernels reach ~877k nodes).
        let tkeys = wmprof.then(std::time::Instant::now);
        let mut eargs = Mat {
            stride: ps.len(),
            v: Vec::with_capacity(es.len() * ps.len()),
        };
        let mut svh = Mat {
            stride: ps.len(),
            v: Vec::with_capacity(es.len() * ps.len()),
        };
        // O-payload per pseudo-arg cell for `skey_of` to reconstruct.
        let mut okey = Mat {
            stride: ps.len(),
            v: Vec::with_capacity(es.len() * ps.len()),
        };
        let mut arity_ok = true;
        for &(i, d) in es {
            let args: Vec<BlockArg> = func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d]
                .args(&func.dfg.value_lists)
                .collect();
            if args.len() != ps.len() {
                arity_ok = false;
                break;
            }
            eargs.v.extend(
                args.iter()
                    .map(|a| a.as_value().map(|v| func.dfg.resolve_aliases(v))),
            );
            // `(inst, tag, payload)` — same `BlockCall` arg is the same
            // value, anything else differs.
            svh.v.extend(args.iter().map(|a| match a {
                BlockArg::Value(v) => crate::proots::vhash(
                    func,
                    &nf,
                    &canon,
                    &mut vhmemo,
                    func.dfg.resolve_aliases(*v),
                ),
                BlockArg::TryCallRet(x) => {
                    crate::proots::ohash((i.index() as u64) << 32 | 1 << 30 | *x as u64)
                }
                BlockArg::TryCallExn(x) => {
                    crate::proots::ohash((i.index() as u64) << 32 | 2 << 30 | *x as u64)
                }
            }));
            okey.v.extend(args.iter().map(|a| match a {
                BlockArg::Value(_) => None,
                BlockArg::TryCallRet(x) => {
                    Some((i.index() as u64) << 32 | 1 << 30 | *x as u64)
                }
                BlockArg::TryCallExn(x) => {
                    Some((i.index() as u64) << 32 | 2 << 30 | *x as u64)
                }
            }));
        }
        if !arity_ok {
            continue;
        }
        // `skey_of`/`pk_of`/`paramize` materialize `Key` trees ~`vnodes`
        // nodes deep per (edge, slot) cell — on megafunction operand
        // DAGs (stage1 `test::shuffle_tests`) that turns the lazy
        // matrices into tens of GB of keys. Cap the block's total arg
        // tree size; `vnodes` is memoized so the scan is cheap.
        let mut arg_nodes = 0usize;
        'cap: for r in eargs.rows() {
            for c in r {
                if let Some(v) = c {
                    arg_nodes = arg_nodes.saturating_add(crate::proots::vnodes(
                        func,
                        &nf,
                        &canon,
                        &mut vnmemo.borrow_mut(),
                        *v,
                    ));
                    if arg_nodes > 2_000_000 {
                        break 'cap;
                    }
                }
            }
        }
        if arg_nodes > 2_000_000 {
            continue;
        }
        if pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::clifpeep", tracing::Level::DEBUG)
        {
            for (ix, _) in ps.iter().enumerate() {
                let sz: Vec<usize> = eargs
                    .rows()
                    .map(|r| {
                        r[ix].map_or(1, |v| {
                            crate::proots::vnodes(func, &nf, &canon, &mut vnmemo.borrow_mut(), v)
                        })
                    })
                    .collect();
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::clifpeep", "wmsz {b} slot{ix} {sz:?}")
                };
            }
        }
        // `pk[e][i]` — slot `i`'s key on edge `e`, paramized relative to
        // the block's slots (skip = i), computed lazily: most slots
        // either merge at phase A or are never a viable sibling, so an
        // eager `edges × slots` matrix of deep `Key` clones dominated
        // the pass on param-heavy blocks.
        let vmap: FxHashMap<Value, u32> =
            ps.iter().enumerate().map(|(i, &p)| (p, i as u32)).collect();
        // Keys stay lazy: `skey_of` materializes `skc[e][ix]` on demand
        // — the hash gates above decide most slots never need one.
        let skc = Mat {
            stride: ps.len(),
            v: (0..es.len() * ps.len())
                .map(|_| std::cell::OnceCell::new())
                .collect(),
        };
        let skey_of = |e: usize, ix: usize| -> &Key {
            skc[e][ix].get_or_init(|| match (eargs[e][ix], okey[e][ix]) {
                (Some(v), _) => crate::proots::key_of(func, &nf, &canon, &mut memo.borrow_mut(), v),
                (None, Some(o)) => Key::O(o),
                (None, None) => unreachable!("non-value arg carries an O payload"),
            })
        };
        // `hmaps[e]`: arg hash → first slot index, for `paramize`'s
        // subtree probe — `vhash` is `khash_m`-equivalent by
        // construction, so probes on `Key` nodes hit. `hmemo` memoizes
        // per-node key hashes (`Rc` sharing → O(1) amortized). Built
        // lazily: an `E×P` hash table is a real cost on megablocks and
        // `paramize` is its only consumer.
        let hmemo: FxHashMap<*const Key, u64> = FxHashMap::default();
        let hmaps: std::cell::OnceCell<Vec<FxHashMap<u64, u32>>> = std::cell::OnceCell::new();
        // Each cell carries the key's hash alongside — the scan
        // compares keys `O(slots × edges)` times and a failed
        // structural `==` on a 10⁴-node key costs as much as computing
        // the hash once, so comparisons go through `pkh` first.
        let hmemo = std::cell::RefCell::new(hmemo);
        let pk = Mat {
            stride: ps.len(),
            v: (0..es.len() * ps.len())
                .map(|_| std::cell::OnceCell::new())
                .collect(),
        };
        // A materialized cell costs a `paramize`+hash over the arg's
        // def-DAG — bound the per-block matrix so a megablock's scan
        // can't go quadratic. Past the cap, cells degrade to a
        // sentinel `O` key whose per-(e,i) payload can never equal a
        // real key — misses merges, never invents them.
        const PK_CELL_CAP: usize = 250_000;
        let pkcells = std::cell::Cell::new(0usize);
        let pkm = |e: usize, i: usize| -> &(Key, u64) {
            pk[e][i].get_or_init(|| {
                if pkcells.get() >= PK_CELL_CAP {
                    let s = u64::MAX - (((e as u64) << 32) | i as u64);
                    return (Key::O(s), crate::proots::ohash(s));
                }
                pkcells.set(pkcells.get() + 1);
                let mut hm = hmemo.borrow_mut();
                let slotk = |j: usize| -> &Key { skey_of(e, j) };
                let k = crate::proots::paramize(
                    skey_of(e, i),
                    ps.len(),
                    &slotk,
                    &ps,
                    i,
                    &hmaps.get_or_init(|| {
                        svh.rows()
                            .map(|row| {
                                let mut m = FxHashMap::default();
                                for (i, &h) in row.iter().enumerate() {
                                    m.entry(h).or_insert(i as u32);
                                }
                                m
                            })
                            .collect()
                    })[e],
                    &vmap,
                    &mut hm,
                );
                let h = crate::proots::khash_m(&k, &mut hm);
                (k, h)
            })
        };
        let pk_of = |e: usize, i: usize| -> &Key { &pkm(e, i).0 };
        let pkh = |e: usize, i: usize| pkm(e, i).1;
        // Slot -> merge key pending materialization in this block. Phase-A
        // keys are `P`-free; phase-B candidates keep `P` leaves until
        // `resolve_p` inlines them.
        let mut pend: FxHashMap<usize, Key> = FxHashMap::default();
        let mut cands: Vec<(usize, Key)> = vec![];
        // `key_sub` only proves a relation between keys sharing a
        // subtree — every success branch anchors on `a == x`,
        // `bx == by`, or `c == c2` — so a sibling whose probe-edge key
        // shares no subtree hash with the target's can never fire it.
        // Index subtree hashes per probe edge lazily: O(slots×keysize)
        // once, instead of slots² oracle probes on wide loop headers.
        let mut edge_submap: FxHashMap<
            usize,
            (FxHashMap<u64, Vec<usize>>, Vec<FxHashSet<u64>>, Vec<usize>),
        > = FxHashMap::default();
        // Phase D's diff-seeded probes only pay on loop headers (a
        // deferring edge must reach back into the head's dominance
        // region). Loop-invariant — once per block, not per slot.
        let is_header = !no_phased
            && es.iter().any(|&(i, _)| {
                func.layout
                    .inst_block(i)
                    .is_some_and(|eb| dom.try_dominates(b, eb, &func.layout))
            });
        // Shared `diff_b` probe budget for the block's phase-D oracle,
        // dealt out per slot. `diff_b` chases cost tens of µs each and
        // a slot scans O(slots) siblings for a mate, so a flat
        // per-slot cap is still O(slots² × chase) — ~90k chases and
        // seconds per webmerge round on ~1000-param headers (regex's
        // `c_bounded` dispatch join). Sharing a fixed block budget
        // keeps small headers at the original 96 probes while wide
        // ones degrade evenly to a few probes per slot; an exhausted
        // budget only means "unproven" (no merge), never a wrong merge.
        const DIFF_B_BLOCK_FUEL: usize = 4096;
        let dfuel_cap = (DIFF_B_BLOCK_FUEL / ps.len().max(1)).clamp(2, 96) as i32;
        let tblk = wmprof.then(std::time::Instant::now);
        let dkeys = tblk.and_then(|e| tkeys.map(|s| e - s)).unwrap_or_default();
        nblk += 1;
        tkeys_sum += dkeys;
        if wmprof && dkeys.as_micros() > 200 {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::clifpeep", "webmerge {} {b}: {} slots {} edges keying {:?}",
                    func.name,
                    ps.len(),
                    es.len(),
                    dkeys
                )
            };
        }

        for (ix, &p) in ps.iter().enumerate() {
            if pmap.contains_key(&p) {
                continue;
            }
            // `seed` — the first edge establishing the class hash;
            // hashes gate so structural `==` (and key materialization)
            // only runs when hashes actually agree.
            let mut seed: Option<usize> = None;
            let mut member: Option<Value> = None;
            let mut uniform = false;
            for (e, _) in es.iter().enumerate() {
                let Some(v) = eargs[e][ix] else {
                    uniform = false;
                    break;
                };
                // A self-edge echoing the param preserves whatever the
                // other edges supplied — it can't break uniformity.
                if v == p {
                    continue;
                }
                match seed {
                    None => {
                        seed = Some(e);
                        uniform = true;
                    }
                    Some(se) if svh[se][ix] != svh[e][ix] || skey_of(se, ix) != skey_of(e, ix) => {
                        uniform = false;
                        break;
                    }
                    _ => {}
                }
                // Reuse beats re-materialization: an edge arg already
                // defined above `b`'s head is a free carrier.
                if member.is_none() && dominates_entry(func, &dom, v, b) {
                    member = Some(v);
                }
            }
            if let Some(se) = seed.filter(|_| uniform) {
                let key = skey_of(se, ix);
                match member {
                    Some(w) => {
                        pmap.insert(p, w);
                    }
                    None => {
                        // No member reaches `b`'s head — only mergeable
                        // when the class's shape can be rebuilt there.
                        if !mat_ok(func, &dom, b, key, &ps) {
                            continue;
                        }
                        pend.insert(ix, key.clone());
                        inserts.push((b, key.clone(), p));
                    }
                }
                param_drops.push(p);
                for e in 0..es.len() {
                    drop_ixs.entry((es[e].0, es[e].1)).or_default().push(ix);
                }
                continue;
            }
            if no_phaseb {
                continue;
            }
            // Huge keys can still merge at phase A (identical trees
            // hash-compare fine), but nothing relational pays: every
            // cell a probe touches costs an O(size) `paramize`/hash/
            // walk, and a `P`-leafed tree 10⁵ nodes deep is never a
            // mergeable slot. `vnodes` counts the *would-be* key —
            // capped traversal over the def-DAG, no key built.
            const REL_PROBES_CAP: usize = 2048;
            if !(0..es.len()).all(|e| {
                eargs[e][ix].is_none_or(|v| {
                    crate::proots::vnodes(func, &nf, &canon, &mut vnmemo.borrow_mut(), v)
                        <= REL_PROBES_CAP
                })
            }) {
                continue;
            }
            // Phase B — sibling-slot congruence: `p_j` drops when every
            // edge's slot-j arg keys to one tree over `P(i)` leaves —
            // `p_j = f(p_i)` at the head, uniformly across edges. This
            // subsumes affparm/relparm (`p_j = p_i ± B`) and covers
            // arbitrary pure `f`, multi-slot trees (`f(p_i, p_k)`), and
            // plain param dedup (`p_j ≡ p_i`).
            if ps.len() < 2 {
                continue;
            }
            let mut rel: Option<(usize, &Key)> = None;
            let mut uni = true;
            for (e, _) in es.iter().enumerate() {
                let rk = pk_of(e, ix);
                match rel {
                    None => rel = Some((e, rk)),
                    Some((se, t)) if pkh(se, ix) != pkh(e, ix) || t != rk => {
                        uni = false;
                        break;
                    }
                    _ => {}
                }
            }
            if uni
                && let Some((_, rk)) = rel
                && has_p(rk)
            {
                cands.push((ix, rk.clone()));
                continue;
            }
            // Phase D — deferral-tolerant relational merging, subsuming
            // relparm's loop-invariant-gap case. `p_j = R(slots)` holds
            // when every edge either supplies an `R`-shaped arg or is a
            // recurrence step preserving `R`: `arg_j = p_j + o_j` with
            // each slot `i` referenced by `R` fed `arg_i = p_i + o_i`
            // and `Σ slope_i·o_i = o_j` (the relation shifts by exactly
            // its total slope). Seeds come two ways: a uniform `R`
            // among non-self-step edges, or `key_sub` proving
            // `arg_j = arg_i - D` uniformly — the latter is `isub(P(i),
            // D)`, catching diffs established through rewrite chains
            // (`y0 + (x - d)` et al) a literal match misses.
            if no_phased {
                continue;
            }
            let kjs: Vec<&Key> = es.iter().enumerate().map(|(e, _)| pk_of(e, ix)).collect();
            let selfstep = |k: &Key| {
                // A `p_j + o` self-step (`Q(ix)` base — the param's old
                // value) can only defer — it can never seed `R` (the
                // `Q(ix)` leaf would resolve cyclically).
                matches!(crate::proots::peel_off(k).0, Key::Q(x) if *x == ix as u32)
            };
            // Does edge `e` defer under hypothesis `r`? Either it
            // supplies `r` verbatim, or it is a preserving step:
            // `arg_j = p_j + o_j` while every slot `i` in `r`'s
            // reference set steps `arg_i = p_i + o_i` with
            // `Σ slope_i·o_i = o_j`.
            let defers = |e: usize, r: &Key, rh: u64, proven: &FxHashSet<usize>| -> bool {
                let kj = &kjs[e];
                if (pkh(e, ix) == rh && **kj == *r) || proven.contains(&e) {
                    return true;
                }
                let (bj, oj) = crate::proots::peel_off(kj);
                if !matches!(bj, Key::Q(x) if *x == ix as u32) {
                    return false;
                }
                let mut sum = 0i64;
                for &i in &crate::proots::pset(r) {
                    let (bi, oi) = crate::proots::peel_off(pk_of(e, i as usize));
                    let Some(s) = crate::proots::slope(r, i) else {
                        return false;
                    };
                    if !matches!(bi, Key::Q(x) if *x == i) {
                        return false;
                    }
                    sum += s * oi;
                }
                sum == oj
            };
            // (1) Uniform `R` among non-self-step edges.
            let mut seed: Option<usize> = None;
            let mut ok = true;
            for (e, kj) in kjs.iter().enumerate() {
                if selfstep(kj) {
                    continue;
                }
                match seed {
                    None => seed = Some(e),
                    Some(se) if pkh(se, ix) != pkh(e, ix) || *kjs[se] != **kj => {
                        ok = false;
                        break;
                    }
                    _ => {}
                }
            }
            // A seed with no `P`/`Q` leaves is a uniform absolute class —
            // phase A already judged it — so it can't seed a relation.
            // Each candidate carries the edges its oracle *proved*: for a
            // diff-seeded rel those edges satisfy `arg_i - arg_j = D`
            // without `arg_j` being textually `isub`-shaped, so `defers`
            // can't re-derive them from `kj` alone.
            let khash = |k: &Key| crate::proots::khash_m(k, &mut hmemo.borrow_mut());
            let mut rels: Vec<(Key, FxHashSet<usize>)> = vec![];
            if ok
                && let Some(se) = seed
                && has_p(kjs[se])
            {
                rels.push((
                    kjs[se].clone(),
                    kjs.iter()
                        .enumerate()
                        .filter(|(e, k)| pkh(*e, ix) == pkh(se, ix) && ***k == *kjs[se])
                        .map(|(e, _)| e)
                        .collect(),
                ));
            }
            // (2) Diff-seeded: `arg_j = arg_i ∓ D` uniformly per sibling
            // slot `i` over non-self-step edges — `isub(P(i), D)` /
            // `iadd(P(i), D)`. Two oracles, tried per slot: `key_sub`
            // on the paramized keys (its diffs may themselves reference
            // slots via `P`), then `diff_b` on the raw values — whose
            // param-pair chase proves diffs across whole chains of
            // echoing predecessors (tokenize's `pos = cur - base`).
            // The diff-seeded path only pays on loop headers — a
            // deferring edge must reach back into the head's dominance
            // region anyway — and the `O(slots × edges)` oracle probes
            // are too hot to run on every non-loop block. `is_header`
            // is loop-invariant; computing it per slot was an
            // `O(slots × edges)` dom-query tax on wide headers.
            if !rels
                .iter()
                .any(|(r, pr)| (0..es.len()).all(|e| defers(e, r, khash(r), pr)))
                && is_header
                && func.dfg.value_type(ps[ix]).is_int()
            {
                let ty = func.dfg.value_type(ps[ix]);
                let mk = |i: usize, op: Opcode, d: Key| {
                    Key::N(
                        op,
                        ty,
                        std::rc::Rc::from([Key::P(i as u32), d]),
                        crate::proots::tag::BIN,
                    )
                };
                // `oracle(e, kj, i)` is tri-state: `Some(Some(r))` proves
                // edge `e` supplies `r`; `Some(None)` is a deferring step
                // (`defers` re-verifies structurally); `None` means slot
                // `i` can't explain edge `e` at all. `dfuel_cap` is the
                // slot's share of the block's `diff_b` budget — most
                // viable mates sit early in the candidate order.
                let dfuel = std::cell::Cell::<i32>::new(dfuel_cap);
                let oracle = |e: usize, kj: &Key, i: usize| -> Option<Option<Key>> {
                    // `key_sub`'s algebraic probes each pay a structural
                    // `==` — on 10⁴-node expression keys that's the
                    // pass's dominant cost, and a diff needing keys that
                    // big is never a relation worth materializing.
                    const REL_KEY_CAP: usize = 512;
                    // Check the *raw* sizes first: `pk_of` materializes
                    // the paramized key, an O(size) walk that a cap on
                    // the result can't preempt.
                    let vwithin = |v: Option<Value>| {
                        v.is_some_and(|v| {
                            crate::proots::vnodes(func, &nf, &canon, &mut vnmemo.borrow_mut(), v)
                                <= REL_KEY_CAP
                        })
                    };
                    if vwithin(eargs[e][i]) && vwithin(eargs[e][ix]) {
                        let ki = pk_of(e, i);
                        let d = if crate::proots::within(ki, REL_KEY_CAP)
                            && crate::proots::within(kj, REL_KEY_CAP)
                        {
                            let mut hm = hmemo.borrow_mut();
                            crate::proots::key_sub_h(ki, kj, ty, 8, &mut hm)
                                .map(|d| mk(i, Opcode::Isub, d))
                                .or_else(|| {
                                    crate::proots::key_sub_h(kj, ki, ty, 8, &mut hm)
                                        .map(|d| mk(i, Opcode::Iadd, d))
                                })
                        } else {
                            None
                        };
                        if let Some(d) = d {
                            return Some(Some(d));
                        }
                    }
                    // `diff_b`'s param-pair chase reaches where keys
                    // can't: args that are params of the same
                    // predecessor, diffed through *that* block's
                    // in-edges (tokenize's `pos = cur - base` web).
                    // Calling it on arbitrary value pairs is a
                    // quadratic-with-fanout oracle — far too hot for
                    // this inner loop, so it runs only on the
                    // same-block-param shape.
                    if wm_nodiffb {
                        return None;
                    }
                    let (Some(ai), Some(aj)) = (eargs[e][i], eargs[e][ix]) else {
                        return None;
                    };
                    if !matches!(
                        (func.dfg.value_def(ai), func.dfg.value_def(aj)),
                        (ValueDef::Param(q1, _), ValueDef::Param(q2, _))
                            if q1 == q2
                    ) {
                        return None;
                    }
                    if dfuel.get() <= 0 {
                        return None;
                    }
                    dfuel.set(dfuel.get() - 1);
                    match crate::indvars::diff_b(
                        func,
                        &cfg,
                        ps[i],
                        ps[ix],
                        ai,
                        aj,
                        8,
                        &mut *dbmemo.borrow_mut(),
                        false,
                    ) {
                        Some(crate::indvars::Diff::Lit(bv)) => {
                            // A bound that is itself a param of `b`
                            // was read at the edge source — `Q(k)`,
                            // not `P(k)` — so deferral accounts for
                            // slot `k` stepping.
                            let d = match ps.iter().position(|&p| p == bv) {
                                Some(k) => Key::Q(k as u32),
                                None => Key::V(bv),
                            };
                            Some(Some(mk(i, Opcode::Isub, d)))
                        }
                        // `Head` — this edge preserves the pair's own
                        // diff (echoed or uniformly stepped params) —
                        // `defers` verifies it.
                        Some(crate::indvars::Diff::Head) => Some(None),
                        _ => None,
                    }
                };
                // Only siblings that work on the first non-self-step
                // edge can hold on all of them — probe that edge per
                // slot before paying `O(edges)` scans, so the loop
                // costs `O(slots)` + `viable × edges`, not
                // `slots² × edges`.
                let e0 = match (0..es.len()).find(|&e| !selfstep(&kjs[e])) {
                    Some(e0) => e0,
                    None => continue,
                };
                let (submap, subsets, zeros) = edge_submap.entry(e0).or_insert_with(|| {
                    let mut submap: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
                    let mut subsets: Vec<FxHashSet<u64>> = Vec::with_capacity(ps.len());
                    let mut zeros: Vec<usize> = vec![];
                    for i in 0..ps.len() {
                        let (k, _) = pkm(e0, i);
                        if matches!(k, Key::K(_, 0)) {
                            zeros.push(i);
                        }
                        let mut s = FxHashSet::default();
                        subtree_hashes(
                            k,
                            &mut hmemo.borrow_mut(),
                            &mut FxHashSet::default(),
                            &mut s,
                        );
                        for &h in &s {
                            submap.entry(h).or_default().push(i);
                        }
                        subsets.push(s);
                    }
                    (submap, subsets, zeros)
                });
                let mut candset: FxHashSet<usize> = zeros.iter().copied().collect();
                for &h in &subsets[ix] {
                    if let Some(v) = submap.get(&h) {
                        candset.extend(v.iter().copied());
                    }
                }
                let target_zero = matches!(kjs[e0], Key::K(_, 0));
                for i in 0..ps.len() {
                    if i == ix || func.dfg.value_type(ps[i]) != ty {
                        continue;
                    }
                    // `key_sub` needs a shared subtree or a literal-0
                    // key; `diff_b`'s same-pred param pair is the only
                    // tree-free oracle path — mirror its gate cheaply.
                    if !target_zero && !candset.contains(&i) {
                        let diffable = matches!(
                            (eargs[e0][i], eargs[e0][ix]),
                            (Some(ai), Some(aj))
                                if matches!(
                                    (
                                        func.dfg.value_def(ai),
                                        func.dfg.value_def(aj)
                                    ),
                                    (
                                        ValueDef::Param(q1, _),
                                        ValueDef::Param(q2, _)
                                    ) if q1 == q2
                                )
                        );
                        if !diffable {
                            continue;
                        }
                    }
                    if oracle(e0, &kjs[e0], i).is_none() {
                        continue;
                    }
                    let mut rel: Option<Key> = None;
                    let mut proven: FxHashSet<usize> = FxHashSet::default();
                    let mut uni_ok = true;
                    for (e, kj) in kjs.iter().enumerate() {
                        if selfstep(kj) {
                            continue;
                        }
                        let Some(r) = oracle(e, kj, i) else {
                            uni_ok = false;
                            break;
                        };
                        // Oracle-deferred (`Head`): the chase proved
                        // this edge preserves the pair invariant even
                        // when `kj` isn't structurally `Q(ix)`-based —
                        // count it as proven.
                        let Some(r) = r else {
                            proven.insert(e);
                            continue;
                        };
                        match &rel {
                            None => rel = Some(r),
                            Some(t) if *t == r => {}
                            _ => {
                                uni_ok = false;
                                break;
                            }
                        }
                        proven.insert(e);
                    }
                    if uni_ok && let Some(r) = rel {
                        rels.push((r, proven));
                    }
                }
            }
            for (rk, pr) in rels {
                let hold = has_p(&rk) && (0..es.len()).all(|e| defers(e, &rk, khash(&rk), &pr));
                if wm_rel {
                    if pliron_cfg::tracing_on() {
                        tracing::info!(target: "pliron::clifpeep", "webmerge-rel {b} slot{ix} cand {rk:?} hold={hold}")
                    };
                }
                if hold {
                    cands.push((ix, rk));
                    break;
                }
            }
        }

        // Resolve phase-B candidates now that every slot's fate in `b`
        // is known: `P(i)` inlines the carrier value (member merge), the
        // slot's own pending key (recursive via `pend`), or the surviving
        // param itself. Seed `pend` with the raw `P`-keys *first* —
        // otherwise `p0 = f(p1)` / `p1 = g(p0)` resolves `P(1)` as a
        // surviving param and the emitted tree dangles off the dropped
        // `p1`; with seeds in place the shared `stack` catches the cycle
        // and both resolve to `None`.
        if wmprof {
            let d = el(tblk);
            if d.as_micros() > 200 {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::clifpeep", "webmerge {} {b}: {} slots {} edges ixl {:?}",
                        func.name,
                        ps.len(),
                        es.len(),
                        d
                    )
                };
            }
        }
        for &(ix, ref rk) in &cands {
            pend.insert(ix, rk.clone());
        }
        for (ix, rk) in cands {
            let p = ps[ix];
            if pmap.contains_key(&p) {
                continue;
            }
            let resolved = crate::proots::resolve_p(
                &rk,
                &mut |i, stack| resolve_slot(i, stack, &ps, &pmap, &pend),
                &mut Vec::new(),
            );
            let Some(k2) = resolved else {
                pend.remove(&ix);
                continue;
            };
            if !mat_ok(func, &dom, b, &k2, &ps) {
                pend.remove(&ix);
                continue;
            }
            if wm_rel {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::clifpeep", "webmerge-rel {:?} slot{} {:?}", b, ix, k2)
                };
            }
            pend.insert(ix, k2.clone());
            inserts.push((b, k2, p));
            param_drops.push(p);
            for e in 0..es.len() {
                drop_ixs.entry((es[e].0, es[e].1)).or_default().push(ix);
            }
        }

        let dblk = el(tblk);
        tblk_sum += dblk;
        if wmprof && dblk.as_micros() > 200 {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::clifpeep", "webmerge {} {b}: {} slots {} edges scan {:?}",
                    func.name,
                    ps.len(),
                    es.len(),
                    dblk
                )
            };
        }
        // Phase C — coinductive bisimulation over the surviving params
        // (phimerge, lifted into key space): hypothesize same-type
        // params equivalent, then refine classes by each slot's
        // per-edge key signature with `V` leaves normalized through the
        // partition. A stable multi-member class is a bisimulation —
        // its members are interchangeable on every edge — so all but
        // the leader drop. Unlike phase B's structural `P` matching this
        // closes over self-reference: `(p_i, p_j)` carried identically
        // on the back-edge merge even when no syntactic relation holds.
        if no_phasec {
            continue;
        }
        let tphasec = wmprof.then(std::time::Instant::now);
        // Survivors, capped by key size: `key_rep` rebuilds a whole
        // signature tree per (slot, edge, round), so slots carrying
        // 10⁴-node expression keys make refinement quadratic-by-
        // allocation. Coinductive pairs in real code are tiny (`P(i)`,
        // `V` refs, small steps) — a giant key is never worth refining.
        const BISIM_KEY_CAP: usize = 256;
        let surv: Vec<usize> = (0..ps.len())
            .filter(|&i| {
                !pmap.contains_key(&ps[i])
                    && !param_drops.contains(&ps[i])
                    && (0..es.len()).all(|e| {
                        eargs[e][i].is_some_and(|v| {
                            crate::proots::vnodes(func, &nf, &canon, &mut vnmemo.borrow_mut(), v)
                                <= BISIM_KEY_CAP
                        })
                    })
            })
            .collect();
        if surv.len() >= 2 {
            let tbis = wmprof.then(std::time::Instant::now);
            // Initial partition: one class per value type.
            let mut cls: FxHashMap<usize, usize> = FxHashMap::default();
            for &i in &surv {
                let ty = func.dfg.value_type(ps[i]);
                let leader = *surv
                    .iter()
                    .find(|&&j| func.dfg.value_type(ps[j]) == ty)
                    .unwrap();
                cls.insert(i, leader);
            }
            let mut rounds = 0;
            loop {
                rounds += 1;
                let leaf = |w: Value| -> Value {
                    let mut w = w;
                    while let Some(&c) = pmap.get(&w) {
                        w = c;
                    }
                    vmap.get(&w)
                        .and_then(|&i| cls.get(&(i as usize)))
                        .map(|&l| ps[l])
                        .unwrap_or(w)
                };
                let sigs: FxHashMap<usize, Vec<Key>> = surv
                    .iter()
                    .map(|&i| {
                        (
                            i,
                            (0..es.len())
                                .map(|e| crate::proots::key_rep(skey_of(e, i), &ps, &leaf))
                                .collect(),
                        )
                    })
                    .collect();
                let mut next: FxHashMap<usize, usize> = FxHashMap::default();
                for &i in &surv {
                    let ty = func.dfg.value_type(ps[i]);
                    let leader = surv
                        .iter()
                        .filter(|&&j| func.dfg.value_type(ps[j]) == ty && sigs[&j] == sigs[&i])
                        .min()
                        .copied()
                        .unwrap();
                    next.insert(i, leader);
                }
                if next == cls {
                    break;
                }
                cls = next;
            }
            if tbis.is_some_and(|t| t.elapsed().as_micros() > 200) {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::clifpeep", "webmerge {} {b}: bisim surv={} E={} rounds={} {:?}",
                        func.name,
                        surv.len(),
                        es.len(),
                        rounds,
                        tbis.unwrap().elapsed()
                    )
                };
            }
            // Stable partition: non-leader members drop to their leader.
            for &i in &surv {
                let l = cls[&i];
                if l == i {
                    continue;
                }
                pmap.insert(ps[i], ps[l]);
                param_drops.push(ps[i]);
                for e in 0..es.len() {
                    drop_ixs.entry((es[e].0, es[e].1)).or_default().push(i);
                }
            }
        }
        if tphasec.is_some_and(|t| t.elapsed().as_micros() > 200) {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::clifpeep", "webmerge {} {b}: phaseC P={} E={} {:?}",
                    func.name,
                    ps.len(),
                    es.len(),
                    tphasec.unwrap().elapsed()
                )
            };
        }
    }
    if param_drops.is_empty() {
        return (0, pmap, FxHashSet::default());
    }
    let tprep = wmprof.then(std::time::Instant::now);
    // Materialize each non-member class at its block's head, operands
    // before users, then insert the run before the original first inst.
    let mut orders: FxHashMap<Block, Vec<Inst>> = FxHashMap::default();
    let mut failed = FxHashSet::<Value>::default();
    for (b, key, p) in inserts {
        let mut order = vec![];
        if let Some(v) = emit_key(func, &mut order, &key) {
            pmap.insert(p, v);
            orders.entry(b).or_default().extend(order);
        } else {
            // Emission failed despite `mat_ok` — un-commit the drop so
            // the param keeps its slots and uses instead of dangling.
            failed.insert(p);
        }
    }
    if !failed.is_empty() {
        param_drops.retain(|p| !failed.contains(p));
        drop_ixs.retain(|(i, d), ixs| {
            let b = func.dfg.insts[*i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[*d]
                .block(&func.dfg.value_lists);
            let ps = func.dfg.block_params(b);
            ixs.retain(|&ix| !failed.contains(&ps[ix]));
            !ixs.is_empty()
        });
    }
    // Blocks that actually changed — param drops still attached below,
    // so `value_def` still names their owner block.
    let mut merged: FxHashSet<Block> = orders.keys().copied().collect();
    for (b, insts) in orders {
        match func.layout.first_inst(b) {
            Some(fi) => {
                for ni in insts {
                    func.layout.insert_inst(ni, fi);
                }
            }
            None => {
                for ni in insts {
                    func.layout.append_inst(ni, b);
                }
            }
        }
    }
    let arg_drops: Vec<(Inst, usize, Vec<usize>)> = drop_ixs
        .into_iter()
        .map(|((i, d), mut ixs)| {
            ixs.sort_unstable_by(|a, b| b.cmp(a));
            (i, d, ixs)
        })
        .collect();
    for &p in &param_drops {
        if let ValueDef::Param(b, _) = func.dfg.value_def(p) {
            merged.insert(b);
        }
    }
    let n = param_drops.len();
    let temit = wmprof.then(std::time::Instant::now);
    apply_param_drops(func, pmap.clone(), arg_drops, param_drops);
    let demit = temit.map_or(std::time::Duration::ZERO, |t| t.elapsed());
    if wmprof {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::clifpeep", "webmerge {}: total {:?} wloop {:?} pre {:?} keying {:?} ixscan {:?} prep {:?} emit {:?} blocks {}",
                func.name,
                t0.map_or(std::time::Duration::ZERO, |t| t.elapsed()),
                twloop.map_or(std::time::Duration::ZERO, |t| t.elapsed()),
                tpre_sum,
                tkeys_sum,
                tblk_sum,
                tprep.map_or(std::time::Duration::ZERO, |t| t.elapsed()),
                demit,
                nblk
            )
        };
    }
    (n, pmap, merged)
}

/// Does `key` contain a `P` leaf — i.e. is it a slot-relative shape?
fn has_p(key: &crate::proots::Key) -> bool {
    use crate::proots::Key;
    match key {
        Key::P(_) | Key::Q(_) => true,
        Key::A(_, rk, _) => has_p(rk),
        Key::N(_, _, args, _) => args.iter().any(has_p),
        _ => false,
    }
}

/// `resolve_p`'s slot resolver for `webmerge`: a slot merged to a
/// dominating member resolves to that carrier `V`; a slot pending
/// materialization inlines its own key (resolved recursively through
/// the shared `stack`); a surviving param resolves to itself.
fn resolve_slot(
    i: u32,
    stack: &mut Vec<u32>,
    ps: &[Value],
    pmap: &FxHashMap<Value, Value>,
    pend: &FxHashMap<usize, crate::proots::Key>,
) -> Option<crate::proots::Key> {
    use crate::proots::Key;
    let q = ps[i as usize];
    if let Some(&w) = pmap.get(&q) {
        return Some(Key::V(w));
    }
    if let Some(k) = pend.get(&(i as usize)) {
        let mut rs = |j: u32, st: &mut Vec<u32>| resolve_slot(j, st, ps, pmap, pend);
        return crate::proots::resolve_p(k, &mut rs, stack);
    }
    Some(Key::V(q))
}

/// Can `key` be materialized at `b`'s head? Every `V` leaf must dominate
/// `b`'s entry (the same rule that rejects back-edge roots — a leaf
/// defined *in* `b` is next-iteration's value) unless it is one of `b`'s
/// own params — a param is definitionally available at the head. `N`
/// nests freely — `emit_key` rebuilds operand trees recursively — so
/// the only un-materializable class is one rooted at a non-dominating
/// leaf.
fn mat_ok(
    func: &Function,
    dom: &cranelift_codegen::dominator_tree::DominatorTree,
    b: Block,
    key: &crate::proots::Key,
    params: &[Value],
) -> bool {
    use crate::proots::Key;
    match key {
        Key::V(w) => params.contains(w) || dominates_entry(func, dom, *w, b),
        Key::K(..) => true,
        // `P`/`Q` must be resolved before this runs — an unresolved
        // leaf is not materializable. `O` is edge-scoped: it exists
        // only as the arg on a `BlockCall`, never as an in-block value.
        Key::P(_) | Key::Q(_) | Key::O(_) => false,
        Key::A(_, rk, _) => mat_ok(func, dom, b, rk, params),
        Key::N(_, _, args, _) => args.iter().all(|a| mat_ok(func, dom, b, a, params)),
    }
}

/// Re-create `key`'s shape at `b`'s head, operand insts pushed before
/// their users into `order`. Reachable only after `mat_ok` passed, so
/// every `V` leaf is a dominating value and no `N` nests inside `N` args.
fn emit_key(func: &mut Function, order: &mut Vec<Inst>, key: &crate::proots::Key) -> Option<Value> {
    use crate::proots::Key;
    let new = |func: &mut Function, order: &mut Vec<Inst>, data: InstructionData, ty: Type| {
        let ni = func.dfg.make_inst(data);
        // A key missing operands leaves `reserved_value` args — an
        // INVALID typevar would panic constraint resolution, so drop
        // the key rather than emit it. `ni` stays detached (never
        // laid out), which is harmless.
        if func.dfg.inst_args(ni).contains(&Value::reserved_value()) {
            return None;
        }
        // `ty` is the key's result type, but ops like `iconcat` derive
        // their result from an *operand* typevar — feeding the result
        // type as ctrl makes `double_width(i128)` panic.
        let ctrl = if data.opcode().constraints().requires_typevar_operand() {
            func.dfg.ctrl_typevar(ni)
        } else {
            ty
        };
        func.dfg.make_inst_results(ni, ctrl);
        order.push(ni);
        Some(func.dfg.first_result(ni))
    };
    match key {
        Key::V(w) => Some(*w),
        // `P`/`Q` must be gone by here — `resolve_p` ran at scan time;
        // a survivor means a bug, and dropping the merge is safer than
        // emitting a key referencing a phantom slot. `O` is edge-scoped
        // — nothing in-block materializes it.
        Key::P(_) | Key::Q(_) | Key::O(_) => return None,
        Key::K(ty, bits) => new(
            func,
            order,
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm: cranelift_codegen::ir::immediates::Imm64::new(*bits),
            },
            *ty,
        ),
        Key::A(ty, rk, k) => {
            let r = emit_key(func, order, rk)?;
            let c = new(
                func,
                order,
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm: cranelift_codegen::ir::immediates::Imm64::new(*k),
                },
                *ty,
            )?;
            new(
                func,
                order,
                InstructionData::Binary {
                    opcode: Opcode::Iadd,
                    args: [r, c],
                },
                *ty,
            )
        }
        Key::N(op, ty, args, extra) => {
            use crate::proots::tag;
            let t = extra & 0xff;
            let field = extra >> 8;
            // Value args lead; trailing `K` pseudo-args carry literals
            // (immediates, offsets) that fold back into the inst's
            // fields rather than materializing as SSA values.
            let nvals = match t {
                tag::SADDR | tag::UIEEE => 0,
                tag::LOAD => args.len() - 1,
                _ => args.len(),
            };
            let mut vals = [Value::reserved_value(); 4];
            for (i, a) in args.iter().enumerate().take(nvals) {
                vals[i] = emit_key(func, order, a)?;
            }
            let lit = |i: usize| -> Option<i64> {
                match &args[i] {
                    Key::K(_, b) => Some(*b),
                    _ => None,
                }
            };
            let data = match t {
                tag::ICMP => InstructionData::IntCompare {
                    opcode: *op,
                    cond: IntCC::all()[field as usize],
                    args: [vals[0], vals[1]],
                },
                tag::FCMP => InstructionData::FloatCompare {
                    opcode: *op,
                    cond: cranelift_codegen::ir::condcodes::FloatCC::all()[field as usize],
                    args: [vals[0], vals[1]],
                },
                tag::TERN => InstructionData::Ternary {
                    opcode: *op,
                    args: [vals[0], vals[1], vals[2]],
                },
                tag::SADDR => InstructionData::StackAddr {
                    opcode: *op,
                    stack_slot: cranelift_codegen::ir::StackSlot::from_u32(field as u32),
                    offset: cranelift_codegen::ir::immediates::Offset32::from(lit(0)? as i32),
                },
                tag::BIMM8 => InstructionData::BinaryImm8 {
                    opcode: *op,
                    arg: vals[0],
                    imm: field as u8,
                },
                tag::LOAD => InstructionData::Load {
                    opcode: *op,
                    arg: vals[0],
                    flags: cranelift_codegen::ir::MemFlags::from_u32(field as u32),
                    offset: cranelift_codegen::ir::immediates::Offset32::from(lit(1)? as i32),
                },
                tag::LOADNOFF => InstructionData::LoadNoOffset {
                    opcode: *op,
                    arg: vals[0],
                    flags: cranelift_codegen::ir::MemFlags::from_u32(field as u32),
                },
                tag::UIEEE => {
                    let bits = lit(0)?;
                    match *op {
                        Opcode::F16const => InstructionData::UnaryIeee16 {
                            opcode: *op,
                            imm: cranelift_codegen::ir::immediates::Ieee16::with_bits(bits as u16),
                        },
                        Opcode::F32const => InstructionData::UnaryIeee32 {
                            opcode: *op,
                            imm: cranelift_codegen::ir::immediates::Ieee32::with_bits(bits as u32),
                        },
                        Opcode::F64const => InstructionData::UnaryIeee64 {
                            opcode: *op,
                            imm: cranelift_codegen::ir::immediates::Ieee64::with_bits(bits as u64),
                        },
                        _ => return None,
                    }
                }
                tag::BIN => {
                    // Identity/annihilator folds for source shapes like
                    // `iadd 0, x` (the egraph and edge clones produce
                    // them): emitting the shape verbatim is redundant —
                    // emit the simplified operand instead.
                    let k0 = args.iter().position(|a| matches!(a, Key::K(_, 0)));
                    let comm = matches!(op, Opcode::Iadd | Opcode::Bor | Opcode::Bxor);
                    match (op, k0) {
                        (Opcode::Band, Some(i)) => return Some(vals[i]),
                        (_, Some(1))
                            if comm || matches!(op, Opcode::Isub | Opcode::Ishl | Opcode::Ushr) =>
                        {
                            return Some(vals[0]);
                        }
                        (_, Some(0)) if comm => return Some(vals[1]),
                        _ => {}
                    }
                    InstructionData::Binary {
                        opcode: *op,
                        args: [vals[0], vals[1]],
                    }
                }
                tag::UNARY => InstructionData::Unary {
                    opcode: *op,
                    arg: vals[0],
                },
                _ => return None,
            };
            new(func, order, data, *ty)
        }
    }
}

/// `brif c` edge args `select(c, a, b)` resolve to a single arm on each
/// edge: `a` taken, `b` otherwise. Niche-select loop tails pass
/// `select(end?, 0, cursor)` on the continue edge where `end?` is
/// known-false — without this the `csel` materializes a value that is
/// just `cursor` on that edge.
pub fn selargs(func: &mut Function) -> usize {
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let Some(term) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { arg: c, blocks, .. } = func.dfg.insts[term] else {
            continue;
        };
        let c = func.dfg.resolve_aliases(c);
        let bcs = blocks;
        for (ei, bc) in bcs.iter().enumerate() {
            let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
            let mut changed = false;
            let new: Vec<BlockArg> = args
                .iter()
                .map(|&a| {
                    let Some(v) = a.as_value() else { return a };
                    let Some(di) = func.dfg.resolved_value_def(v).inst() else {
                        return a;
                    };
                    let InstructionData::Ternary {
                        opcode: Opcode::Select,
                        args: sa,
                    } = func.dfg.insts[di]
                    else {
                        return a;
                    };
                    if func.dfg.resolve_aliases(sa[0]) != c {
                        return a;
                    }
                    changed = true;
                    BlockArg::Value(sa[1 + ei])
                })
                .collect();
            if changed {
                let dest = bc.block(&func.dfg.value_lists);
                let nc = BlockCall::new(dest, new, &mut func.dfg.value_lists);
                let InstructionData::Brif { blocks, .. } = &mut func.dfg.insts[term] else {
                    continue;
                };
                blocks[ei] = nc;
                n += 1;
            }
        }
    }
    n
}

/// `band v, M` where `v`'s bits already fit `M` (rustc's i1 normalization
/// residue: `icmp` results, `uextend`/`ireduce`/`select` of 0/1 values
/// costs one `and` per element in scalar `count_if` loops the vectorizer
/// leaves behind — e.g. `add acc, (uextend (select lo<=x, x<=hi, 0)) & 1`
/// where the select's arms are already boolean; `ushr`-extracted fields
/// masked by their width, like `(h >> 57) & 0x7f` in SipHash's h2).
/// Runs after sameargs: that's when the merged predicate param has been
/// aliased to its select def and `is01` can see through it. Scalar-only —
/// a vector `icmp`'s all-ones lanes are masks, not 0/1, so `band` on them
/// is a real operation.
pub fn mask01(func: &mut Function) -> usize {
    let mut hits = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let InstructionData::Binary {
                opcode: Opcode::Band,
                args,
            } = func.dfg.insts[i]
            else {
                continue;
            };
            let res = func.dfg.first_result(i);
            let ty = func.dfg.value_type(res);
            if !ty.is_int() || ty.is_vector() {
                continue;
            }
            let all = if ty.bits() >= 64 {
                u64::MAX
            } else {
                (1u64 << ty.bits()) - 1
            };
            let (x, m) = if let Some(m) = iconst(func, args[1]) {
                (args[0], m as u64 & all)
            } else if let Some(m) = iconst(func, args[0]) {
                (args[1], m as u64 & all)
            } else {
                continue;
            };
            // The mask is a no-op when `cover` proves every set bit of `x`
            // is already in `m` (value bound is not enough: 7 <= 8 but
            // `7 & 8 != 7`); `is01` covers the non-const bit-cover case.
            if cover(func, x, 4) & !m == 0 || (m == 1 && is01(func, x, 4)) {
                hits.push((i, res, func.dfg.resolve_aliases(x)));
            }
        }
    }
    let n = hits.len();
    for (i, res, x) in hits {
        func.dfg.clear_results(i);
        func.layout.remove_inst(i);
        func.dfg.change_to_alias(res, x);
    }
    n
}

/// What an `icmp cc x, K` feeding a `brif` is really asking when the
/// compare reduces to a single-bit test (`PLIRON_SGNTEST`).
enum SgnFold {
    /// True iff bit `b` of the compared value is `set`.
    Bit { b: u32, set: bool },
    /// The answer is fixed; `then` is taken iff the icmp is true.
    Const(bool),
}

/// `x`'s sign bit is provably clear — `x <s 0` is const, `x >=s 0` too.
fn sign_known_clear(func: &Function, x: Value, all: u64) -> bool {
    vmax(func, x) <= all >> 1
}

/// `icmp cc x, k` as a one-bit test. `k` is the compare constant masked
/// to the operand width `w = all.count_ones()`; `all` is the type's max.
fn icmp_bit_test(func: &Function, x: Value, cc: IntCC, k: u64, all: u64) -> Option<SgnFold> {
    let w = all.count_ones();
    let sign = w - 1;
    let signbit = 1u64 << sign;
    match cc {
        // `x <s 0` (`x <=s -1`) tests the sign bit; `x >=s 0` (`x >s -1`)
        // tests its complement.
        IntCC::SignedLessThan if k == 0 => {
            if sign_known_clear(func, x, all) {
                Some(SgnFold::Const(false))
            } else {
                Some(SgnFold::Bit { b: sign, set: true })
            }
        }
        IntCC::SignedLessThanOrEqual if k == all => {
            if sign_known_clear(func, x, all) {
                Some(SgnFold::Const(false))
            } else {
                Some(SgnFold::Bit { b: sign, set: true })
            }
        }
        IntCC::SignedGreaterThanOrEqual if k == 0 => {
            if sign_known_clear(func, x, all) {
                Some(SgnFold::Const(true))
            } else {
                Some(SgnFold::Bit {
                    b: sign,
                    set: false,
                })
            }
        }
        IntCC::SignedGreaterThan if k == all => {
            if sign_known_clear(func, x, all) {
                Some(SgnFold::Const(true))
            } else {
                Some(SgnFold::Bit {
                    b: sign,
                    set: false,
                })
            }
        }
        // `x <s INT_MIN` never holds; `x >=s INT_MIN` always does.
        IntCC::SignedLessThan if k == signbit => Some(SgnFold::Const(false)),
        IntCC::SignedGreaterThanOrEqual if k == signbit => Some(SgnFold::Const(true)),
        // `x <u 1<<b` / `x <=u (1<<b)-1` with `x < 2^(b+1)` tests bit b
        // clear — the UTF-8 lead-byte check `byte <u 0x80`.
        IntCC::UnsignedLessThan | IntCC::UnsignedLessThanOrEqual => {
            let kp = k as u128 + u128::from(cc == IntCC::UnsignedLessThanOrEqual);
            if kp == 0 {
                return Some(SgnFold::Const(false));
            }
            let hi = vmax(func, x) as u128;
            if hi < kp {
                return Some(SgnFold::Const(true));
            }
            if kp.is_power_of_two() && hi < 2 * kp {
                return Some(SgnFold::Bit {
                    b: kp.trailing_zeros(),
                    set: false,
                });
            }
            None
        }
        // `x >=u 1<<b` / `x >u (1<<b)-1` with `x < 2^(b+1)` tests bit b.
        IntCC::UnsignedGreaterThanOrEqual | IntCC::UnsignedGreaterThan => {
            let kp = k as u128 + u128::from(cc == IntCC::UnsignedGreaterThan);
            if kp == 0 {
                return Some(SgnFold::Const(true));
            }
            let hi = vmax(func, x) as u128;
            if hi < kp {
                return Some(SgnFold::Const(false));
            }
            if kp.is_power_of_two() && hi < 2 * kp {
                return Some(SgnFold::Bit {
                    b: kp.trailing_zeros(),
                    set: true,
                });
            }
            None
        }
        _ => None,
    }
}

/// Where a `1 << b` test of a chain-peeled value can be emitted.
enum Probe {
    /// `band v, 1<<b` is the brif arg.
    Bit(Value, u32),
    /// The tested bit is provably 0 or 1.
    Const(bool),
}

/// Peel `ireduce`/`band`/`ushr` wrappers off `v` tracking the tested
/// bit, preferring a carrier whose type encodes `1<<b` as a positive
/// `iconst` — aarch64's `tbnz` fusion matches `u64_from_iconst` with
/// exactly one bit, and a narrow type's sign-bit mask stores as
/// all-ones-extended (`band.i8 x, -128` is 57 one bits, no fusion). So
/// `icmp.i8 ult (ireduce.i8 b64), -128` reaches through the reduce and
/// emits `band.i64 b64, 0x80` → `tbnz x, #7`. `peeled` collects the
/// insts traversed so the caller can reap the ones left dead.
fn bit_probe(func: &Function, x: Value, mut b: u32, peeled: &mut Vec<Inst>) -> Probe {
    let mut v = func.dfg.resolve_aliases(x);
    for _ in 0..16 {
        let w = func.dfg.value_type(v).bits();
        let all = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
        let Some(d) = func.dfg.value_def(v).inst() else {
            return Probe::Bit(v, b);
        };
        match func.dfg.insts[d] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => return Probe::Const((imm.bits() as u64 >> b) & 1 == 1),
            InstructionData::Unary {
                opcode: Opcode::Ireduce,
                arg,
            } if func.dfg.value_type(arg).bits() <= 64 => {
                peeled.push(d);
                v = func.dfg.resolve_aliases(arg);
            }
            InstructionData::Binary {
                opcode: Opcode::Band,
                args,
            } => match bin_k_data(func, args) {
                // `(z & m)`'s bit b is z's when `m` keeps it, always
                // clear when it doesn't.
                Some((z, m)) if (m as u64 & all) >> b & 1 == 1 => {
                    peeled.push(d);
                    v = z;
                }
                Some(_) => return Probe::Const(false),
                None => return Probe::Bit(v, b),
            },
            InstructionData::Binary {
                opcode: Opcode::Ushr,
                args,
            } => match iconst(func, args[1]) {
                // `band (ushr z, s), 1<<b` is bit `b+s` of `z` — or
                // always 0 when it shifted out.
                Some(s) if (b as u64 + (s as u64) % (w as u64)) < w as u64 => {
                    peeled.push(d);
                    v = func.dfg.resolve_aliases(args[0]);
                    b += (s as u64 % w as u64) as u32;
                }
                Some(_) => return Probe::Const(false),
                None => return Probe::Bit(v, b),
            },
            _ => {
                // A `band` imm is a single bit as stored iff it sits
                // below the sign bit — or the type is already i64
                // (`iconst.i64 MIN` is the one negative one-bit imm).
                if b + 1 < w || w == 64 {
                    return Probe::Bit(v, b);
                }
                // `b` is the sign bit of a narrow type: extends in the
                // chain may hide a wider carrier underneath.
                match func.dfg.insts[d] {
                    InstructionData::Unary {
                        opcode: Opcode::Uextend,
                        arg,
                    } => {
                        let m = func.dfg.value_type(arg).bits();
                        if b < m {
                            peeled.push(d);
                            v = func.dfg.resolve_aliases(arg);
                        } else {
                            return Probe::Const(false);
                        }
                    }
                    InstructionData::Unary {
                        opcode: Opcode::Sextend,
                        arg,
                    } => {
                        // The wide sign bit is the narrow one's, copied.
                        peeled.push(d);
                        v = func.dfg.resolve_aliases(arg);
                        b = func.dfg.value_type(v).bits() - 1;
                    }
                    _ => return Probe::Bit(v, b),
                }
            }
        }
    }
    Probe::Bit(v, b)
}

/// Drop insts whose result is now unused — the compare and the chain /
/// operand wrappers a `sgntest` rewrite orphaned. Removed insts feed
/// their `iconst` operand defs back into the worklist so the compare's
/// constant dies with it.
fn reap_dead(func: &mut Function, insts: &[Inst]) {
    // One tally over the function — a whole-function use scan per
    // candidate made this fixpoint O(wl × iters × fn) before.
    let mut cnt: FxHashMap<Value, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for &a in func.dfg.inst_args(i) {
                *cnt.entry(func.dfg.resolve_aliases(a)).or_default() += 1;
            }
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for a in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(x) = a {
                        *cnt.entry(func.dfg.resolve_aliases(x)).or_default() += 1;
                    }
                }
            }
        }
    }
    let dec = |func: &Function, cnt: &mut FxHashMap<Value, u32>, d: Inst| {
        for &a in func.dfg.inst_args(d) {
            if let Some(c) = cnt.get_mut(&func.dfg.resolve_aliases(a)) {
                *c = c.saturating_sub(1);
            }
        }
        for bc in
            func.dfg.insts[d].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        {
            for a in bc.args(&func.dfg.value_lists) {
                if let BlockArg::Value(x) = a
                    && let Some(c) = cnt.get_mut(&func.dfg.resolve_aliases(x))
                {
                    *c = c.saturating_sub(1);
                }
            }
        }
    };
    let mut wl: Vec<Inst> = insts.to_vec();
    let mut seen: FxHashSet<Inst> = wl.iter().copied().collect();
    // Fixpoint: each removal can free a candidate seen earlier (the
    // list is roughly outermost-first but chain/extend peels
    // interleave), so rescan until nothing else drops.
    let mut progress = true;
    while progress {
        progress = false;
        let mut i = 0;
        while i < wl.len() {
            let d = wl[i];
            i += 1;
            if func.layout.inst_block(d).is_none() {
                continue;
            }
            let r = func.dfg.first_result(d);
            if cnt.get(&func.dfg.resolve_aliases(r)).copied().unwrap_or(0) != 0 {
                continue;
            }
            for dd in func
                .dfg
                .inst_args(d)
                .iter()
                .filter_map(|&a| func.dfg.resolved_value_def(a).inst())
            {
                if seen.insert(dd) && func.dfg.insts[dd].opcode() == Opcode::Iconst {
                    wl.push(dd);
                }
            }
            dec(func, &mut cnt, d);
            func.dfg.clear_results(d);
            func.layout.remove_inst(d);
            progress = true;
        }
    }
}

/// `brif (icmp cc x, K)` where the compare is a one-bit test —
/// `icmp ult byte, 0x80` (UTF-8 ASCII/continuation in tokenize,
/// chars_*), `icmp slt x, 0` — becomes `brif (band v, 1<<b)` with the
/// targets swapped when the icmp fires on the clear bit. aarch64 fuses
/// the band+brif into a single `tbnz` (LLVM's `ldrsb`+`tbz` shape, and
/// the `uextend`+`cmp`+`b.hs` triple disappears); x64 emits
/// `test`+`jnz`, never worse than `cmp`+`jcc`. Runs as its own late
/// step: the `icmp` form is canonical for every earlier pass.
pub fn sgntest(func: &mut Function) -> usize {
    let mut brifs = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            if func.dfg.insts[i].opcode() == Opcode::Brif {
                brifs.push(i);
            }
        }
    }
    let mut n = 0;
    for i in brifs {
        n += usize::from(sgn_brif(func, i));
    }
    n
}

fn sgn_brif(func: &mut Function, inst: Inst) -> bool {
    let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[inst] else {
        return false;
    };
    // Find the compare under rustc's boolean spellings: `!c` wrappers
    // (`un_not_chain` — it also tracks the parity) and `uextend`/`sextend`
    // of the flag, which keep the 0/1-ness `brif` tests.
    let mut chain = Vec::new();
    let mut dead: Vec<Inst> = Vec::new();
    let mut neg = false;
    let mut cv = func.dfg.resolve_aliases(arg);
    let ci = loop {
        let (v, n) = un_not_chain(func, cv, Some(&mut chain));
        cv = v;
        neg ^= n;
        let Some(i) = func.dfg.value_def(cv).inst() else {
            return false;
        };
        match func.dfg.insts[i] {
            InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                ..
            } => break i,
            InstructionData::Unary {
                opcode: Opcode::Uextend | Opcode::Sextend,
                arg: a,
            } => {
                dead.push(i);
                cv = func.dfg.resolve_aliases(a);
                if dead.len() > 8 {
                    return false;
                }
            }
            _ => return false,
        }
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args,
    } = func.dfg.insts[ci]
    else {
        return false;
    };
    let ty = func.dfg.value_type(args[0]);
    if !ty.is_int() || ty.is_vector() || ty.bits() > 64 {
        return false;
    }
    let all = if ty.bits() >= 64 {
        u64::MAX
    } else {
        (1u64 << ty.bits()) - 1
    };
    let (cc, x, k) = if let Some(k) = iconst(func, args[1]) {
        (cond, args[0], k as u64 & all)
    } else if let Some(k) = iconst(func, args[0]) {
        (cond.swap_args(), args[1], k as u64 & all)
    } else {
        return false;
    };
    let Some(test) = icmp_bit_test(func, x, cc, k, all) else {
        return false;
    };
    // `dead` already holds the peeled extends; add the condition-chain
    // wrappers, the compare, then the operand-side peels `bit_probe`
    // reports (appended below) — outermost first so removals cascade.
    // `reap_dead` rechecks uses live, so a wrapper still referenced
    // elsewhere survives.
    for &(v, _) in &chain {
        if let Some(d) = func.dfg.value_def(v).inst()
            && d != ci
            && !dead.contains(&d)
        {
            dead.push(d);
        }
    }
    dead.push(ci);
    match test {
        SgnFold::Const(t) => {
            // `then` iff the icmp answer `t` survives the chain negation.
            let dest = blocks[usize::from(t == neg)];
            func.dfg.insts[inst] = InstructionData::Jump {
                opcode: Opcode::Jump,
                destination: dest,
            };
        }
        SgnFold::Bit { b, set } => {
            let mut peeled = Vec::new();
            let (v, vb) = match bit_probe(func, x, b, &mut peeled) {
                Probe::Bit(v, vb) => (v, vb),
                Probe::Const(bit) => {
                    dead.extend(peeled);
                    let t = bit == set;
                    let dest = blocks[usize::from(t == neg)];
                    func.dfg.insts[inst] = InstructionData::Jump {
                        opcode: Opcode::Jump,
                        destination: dest,
                    };
                    reap_dead(func, &dead);
                    return true;
                }
            };
            dead.extend(peeled);
            let vty = func.dfg.value_type(v);
            let sh = 64 - vty.bits();
            let imm = ((1u64 << vb) << sh) as i64 >> sh;
            let mut cur = FuncCursor::new(func).at_inst(inst);
            let m = cur.ins().iconst(vty, imm);
            let nb = cur.ins().band(v, m);
            if let InstructionData::Brif {
                arg: a, blocks: bs, ..
            } = &mut cur.func.dfg.insts[inst]
            {
                *a = nb;
                // `then` fires iff `set`'s polarity matches the chain's.
                if set == neg {
                    bs.swap(0, 1);
                }
            }
        }
    }
    reap_dead(func, &dead);
    true
}

/// `select c, a, b` whose every use sits in the region where `c`'s truth
/// is already decided collapses to that arm — the `Option<NonNull>`
/// niche selects (`select end?, 0, ptr`) ifconv materialises: the load
/// using the `Some` arm only executes on the `!end` edge, so the select
/// (and its compare feed) is dead weight. A use is "under `c = f`" when
/// the block of the `brif c` arm proving it dominates the use block and
/// that arm's target is reachable only through it.
fn sel_regions(func: &mut Function) -> usize {
    use cranelift_codegen::dominator_tree::DominatorTree;
    use cranelift_codegen::flowgraph::ControlFlowGraph;
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);
    // Region roots per condition: `brif c` targets reachable only via
    // the matching arm.
    let mut roots: FxHashMap<Value, Vec<(Block, bool)>> = FxHashMap::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { arg: c, blocks, .. } = func.dfg.insts[t] else {
            continue;
        };
        let c = func.dfg.resolve_aliases(c);
        // `brif (icmp eq/ne x, 0)` decides `x`'s truth on each arm too
        // (`eq` inverts it) — selects conditioned on `x` fold the same
        // way once niche tests rewrite the branch to such a compare.
        let mut conds = vec![(c, false)];
        if let Some(ci) = func.dfg.value_def(c).inst()
            && let InstructionData::IntCompare { cond, args: ia, .. } = func.dfg.insts[ci]
            && matches!(cond, IntCC::Equal | IntCC::NotEqual)
        {
            let konst = |v: Value| match func.dfg.value_def(v) {
                ValueDef::Result(i, 0) => match func.dfg.insts[i] {
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                    } => Some(imm.bits()),
                    _ => None,
                },
                _ => None,
            };
            let x = match (konst(ia[0]), konst(ia[1])) {
                (Some(0), None) => Some(ia[1]),
                (None, Some(0)) => Some(ia[0]),
                _ => None,
            };
            if let Some(x) = x {
                conds.push((func.dfg.resolve_aliases(x), cond == IntCC::Equal));
            }
        }
        for (di, want) in [(0usize, true), (1, false)] {
            let d = blocks[di].block(&func.dfg.value_lists);
            if blocks[1 - di].block(&func.dfg.value_lists) == d {
                continue;
            }
            // `d` must be entered only through this arm.
            let ok = cfg.pred_iter(d).all(|p| {
                p.inst == t
                    && func.dfg.insts[p.inst]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                        .iter()
                        .enumerate()
                        .filter(|(_, bc)| bc.block(&func.dfg.value_lists) == d)
                        .all(|(i, _)| i == di)
            });
            if ok {
                for &(rc, inv) in &conds {
                    roots.entry(rc).or_default().push((d, want != inv));
                }
            }
        }
    }
    if roots.is_empty() {
        return 0;
    }
    // Which arm each block is decided under: `(want == true)` means
    // `c != 0` is known on entry. Edge args are evaluated in the edge's
    // source block, so a `select` arg in a decided block folds to its
    // arm in place — `Chars`' `select(end?, ptr, ptr+1)` carried through
    // the exit edge is the motivating case: the exit edge keeps `ptr`
    // (the arm the select yields there anyway), after which the select's
    // remaining uses all sit under `!end` and the whole select collapses
    // to `ptr+1` below.
    let decided = |u: Block, rs: &[(Block, bool)]| -> Option<bool> {
        for &(r, w) in rs {
            if domtree.block_dominates(r, u) {
                return Some(w);
            }
        }
        None
    };
    let mut edge_edits: Vec<(Inst, usize, usize, Value)> = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for (di, bc) in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .enumerate()
            {
                for (ai, a) in bc.args(&func.dfg.value_lists).enumerate() {
                    let BlockArg::Value(v) = a else {
                        continue;
                    };
                    let Some(d) = func.dfg.resolved_value_def(v).inst() else {
                        continue;
                    };
                    let InstructionData::Ternary {
                        opcode: Opcode::Select,
                        args: sa,
                    } = func.dfg.insts[d]
                    else {
                        continue;
                    };
                    let c = func.dfg.resolve_aliases(sa[0]);
                    let Some(rs) = roots.get(&c) else {
                        continue;
                    };
                    if let Some(w) = decided(b, rs) {
                        edge_edits.push((i, di, ai, sa[if w { 1 } else { 2 }]));
                    }
                }
            }
        }
    }
    let mut n = 0;
    for (i, di, ai, v) in edge_edits {
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[i]
            .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[di];
        let mut at = 0;
        bc.update_args(&mut dfg.value_lists, |a| {
            let r = if at == ai { BlockArg::Value(v) } else { a };
            at += 1;
            r
        });
        n += 1;
    }
    // Use blocks of each select result (inst args and edge args alike:
    // an edge arg is evaluated where the edge is taken).
    let mut uses: FxHashMap<Value, FxHashSet<Block>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for &a in func.dfg.inst_args(i) {
                uses.entry(func.dfg.resolve_aliases(a))
                    .or_default()
                    .insert(b);
            }
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for a in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(v) = a {
                        uses.entry(func.dfg.resolve_aliases(v))
                            .or_default()
                            .insert(b);
                    }
                }
            }
        }
    }
    for b in func.layout.blocks().collect::<Vec<_>>() {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            } = func.dfg.insts[i]
            else {
                continue;
            };
            let c = func.dfg.resolve_aliases(args[0]);
            let Some(rs) = roots.get(&c) else {
                continue;
            };
            let Some(us) = uses.get(&func.dfg.first_result(i)) else {
                continue;
            };
            let (mut want_t, mut want_f) = (0usize, 0usize);
            for &u in us {
                match decided(u, rs) {
                    Some(true) => want_t += 1,
                    Some(false) => want_f += 1,
                    None => {
                        want_t += usize::MAX / 2;
                        break;
                    }
                }
            }
            let arm = match (want_t, want_f) {
                (u, _) if u >= usize::MAX / 2 => continue,
                (0, f) if f > 0 => args[2],
                (t, 0) if t > 0 => args[1],
                _ => continue,
            };
            let res = func.dfg.first_result(i);
            func.dfg.clear_results(i);
            func.layout.remove_inst(i);
            func.dfg.change_to_alias(res, arm);
            n += 1;
        }
    }
    n
}

/// `load.notrap (select c, pa, pb)` → `select c, (load pa), (load pb)` —
/// speculating both loads lets them issue in parallel and feeds `cmov`
/// the values directly, instead of serializing pointer-select → load
/// (rustc's small_sort merge networks select between two element
/// pointers then load the winner). Speculating the non-taken arm is
/// only trap-safe if that address is dereferenceable anyway — require a
/// same-block sibling `load (select _, pb, pa)` covering the same two
/// addresses, so the pair already dereferences both unconditionally.
fn loadsel_pair(
    pos: &mut FuncCursor,
    inst: Inst,
    arg: Value,
    flags: cranelift_codegen::ir::MemFlags,
    offset: cranelift_codegen::ir::immediates::Offset32,
) -> bool {
    // Plain `load` only: the rewrite re-emits `load ty`, which for a
    // `uload8.i64`-style extending load would read 8 bytes, not 1.
    if !pos.func.dfg.mem_flags[flags].notrap() || pos.func.dfg.insts[inst].opcode() != Opcode::Load {
        return false;
    }
    let a = pos.func.dfg.resolve_aliases(arg);
    let Some(d) = pos.func.dfg.value_def(a).inst() else {
        return false;
    };
    let InstructionData::Ternary {
        opcode: Opcode::Select,
        args: [c, pa, pb],
    } = pos.func.dfg.insts[d]
    else {
        return false;
    };
    let c = pos.func.dfg.resolve_aliases(c);
    let (pa, pb) = (
        pos.func.dfg.resolve_aliases(pa),
        pos.func.dfg.resolve_aliases(pb),
    );
    let b = pos.func.layout.inst_block(inst).unwrap();
    let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
    // Find the complementary sibling: a same-typed notrap load of
    // `select c, pb, pa` — same condition, swapped arms. The pair
    // picks opposite arms on every execution, so both `pa` and `pb`
    // get dereferenced unconditionally whenever this block runs.
    let mut sib = None;
    let mut sib_after = false;
    for i in pos.func.layout.block_insts(b) {
        if i == inst {
            sib_after = true;
            continue;
        }
        let InstructionData::Load {
            opcode: Opcode::Load,
            arg: a2,
            flags: f2,
            offset: o2,
        } = pos.func.dfg.insts[i]
        else {
            continue;
        };
        // Both halves are rebuilt from `inst`'s offset, so the sibling
        // must read the same field (core's sort9/swap_if_less pair a
        // key load at +0 with a +16 load of the same select).
        if !pos.func.dfg.mem_flags[f2].notrap() || o2 != offset {
            continue;
        }
        let Some(d2) = pos.func.dfg.resolved_value_def(a2).inst() else {
            continue;
        };
        let InstructionData::Ternary {
            opcode: Opcode::Select,
            args: [c2, x, y],
        } = pos.func.dfg.insts[d2]
        else {
            continue;
        };
        let c2 = pos.func.dfg.resolve_aliases(c2);
        let x = pos.func.dfg.resolve_aliases(x);
        let y = pos.func.dfg.resolve_aliases(y);
        if c2 == c
            && x == pb
            && y == pa
            && pos.func.dfg.value_type(pos.func.dfg.first_result(i)) == ty
        {
            sib = Some(i);
            break;
        }
    }
    let Some(sib) = sib else {
        return false;
    };
    // Nothing memory-affecting may sit between the pair: a call or
    // store to `pa`/`pb` could change what the later load reads, and a
    // trap would leave the sibling unexecuted on paths where `inst`
    // runs — either way the arm's validity at the witness wouldn't
    // carry to the speculated load.
    let mut between = false;
    for i in pos.func.layout.block_insts(b) {
        if i == inst || i == sib {
            if between {
                break;
            }
            between = true;
            continue;
        }
        if between {
            let op = pos.func.dfg.insts[i].opcode();
            if op.can_store() || op.can_trap() || op.is_call() {
                return false;
            }
        }
    }
    let flags = pos.func.dfg.mem_flags[flags].clone();
    // Emit `iadd`-armed loads as `base + k`: the arm pointers are often
    // `noalias` scope roots (rustc seeds them when passing `&mut` into
    // noalias callees), so `load pa` keys an opaque root that forwards
    // with nothing, while `load base + k` unifies with the sibling
    // accesses and the network's own stores.
    let arm = |pos: &mut FuncCursor, a: Value| -> Option<(Value, i32)> {
        let mut a = a;
        let mut off = i64::from(i32::from(offset));
        for _ in 0..8 {
            let Some(i) = pos.func.dfg.value_def(a).inst() else {
                break;
            };
            let InstructionData::Binary {
                opcode: Opcode::Iadd,
                args,
            } = pos.func.dfg.insts[i]
            else {
                break;
            };
            let (x, y) = (
                pos.func.dfg.resolve_aliases(args[0]),
                pos.func.dfg.resolve_aliases(args[1]),
            );
            let (base, k) = if let Some(k) = iconst(pos.func, y) {
                (x, k)
            } else if let Some(k) = iconst(pos.func, x) {
                (y, k)
            } else {
                break;
            };
            a = base;
            off = off.wrapping_add(k);
        }
        Some((a, i32::try_from(off).ok()?))
    };
    let Some(((pa2, offa), (pb2, offb))) = arm(&mut *pos, pa).zip(arm(&mut *pos, pb)) else {
        return false;
    };
    pos.goto_inst(if sib_after { inst } else { sib });
    let la = pos.ins().load(ty, flags.clone(), pa2, offa);
    let lb = pos.ins().load(ty, flags, pb2, offb);
    // Rewrite each load of `select c, x, y` to `select c, (x==pa ? la :
    // lb), (y==pa ? la : lb)` — arm order preserved.
    for l in [inst, sib] {
        let ldef = pos
            .func
            .dfg
            .resolved_value_def(match pos.func.dfg.insts[l] {
                InstructionData::Load { arg, .. } => arg,
                _ => unreachable!(),
            })
            .inst()
            .unwrap();
        let InstructionData::Ternary {
            args: [c, x, y], ..
        } = pos.func.dfg.insts[ldef]
        else {
            unreachable!()
        };
        let (x, y) = (
            pos.func.dfg.resolve_aliases(x),
            pos.func.dfg.resolve_aliases(y),
        );
        let (tv, fv) = (if x == pa { la } else { lb }, if y == pa { la } else { lb });
        pos.func.replace(l).select(c, tv, fv);
    }
    pos.goto_inst(inst);
    true
}

/// Speculate both arms of `load.notrap (select c, pa, pb)` pairs into
/// `select c, (load pa), (load pb)`. Runs as its own late step too —
/// the compare-exchange diamonds only collapse into selects after
/// `fuse`/`sameargs`, well past `run`'s usual round.
pub fn loadsel(func: &mut Function) -> usize {
    let mut n = 0;
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            if let InstructionData::Load {
                arg, flags, offset, ..
            } = pos.func.dfg.insts[inst]
                && loadsel_pair(&mut pos, inst, arg, flags, offset)
            {
                n += 1;
            }
        }
    }
    n
}

/// `((x & m1) | c1) & m2 | c2 …` — field-merge chains collapse: `&`
/// distributes over `|`, so a `band`/`bor` chain of constants over one
/// base is `band x, M` + `bor K`. rustc builds packed enums field by
/// field — `ParseIntError` inside `str::parse` is
/// `x & ~0xff00 | 0x100 & ~0xff | 1` — and LLVM folds these to a single
/// mask+set pair where we ran four ALU ops per site.
fn bitmerge(func: &mut Function) -> usize {
    let uses = use_counts(func);
    let mut hits = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
                continue;
            };
            if !matches!(opcode, Opcode::Band | Opcode::Bor) {
                continue;
            }
            let res = func.dfg.first_result(i);
            let ty = func.dfg.value_type(res);
            if !ty.is_int() || ty.is_vector() || ty.bits() > 128 {
                continue;
            }
            // Walk the chain inward from `i`: each link is a single-use
            // band/bor with one constant operand; the walk stops at the
            // first operand that isn't (the base).
            let mut best = None;
            for ci in [1usize, 0] {
                let Some(k0) = ceval(func, args[ci], 6) else {
                    continue;
                };
                let mut links = vec![(i, opcode, k0)];
                let mut base = func.dfg.resolve_aliases(args[1 - ci]);
                loop {
                    let Some(d) = func.dfg.value_def(base).inst() else {
                        break;
                    };
                    let InstructionData::Binary {
                        opcode: o2,
                        args: a2,
                    } = func.dfg.insts[d]
                    else {
                        break;
                    };
                    if !matches!(o2, Opcode::Band | Opcode::Bor)
                        || uses.get(&base).copied().unwrap_or(0) != 1
                    {
                        break;
                    }
                    let (k2, other) = if let Some(k) = ceval(func, a2[1], 6) {
                        (k, a2[0])
                    } else if let Some(k) = ceval(func, a2[0], 6) {
                        (k, a2[1])
                    } else {
                        break;
                    };
                    links.push((d, o2, k2));
                    base = func.dfg.resolve_aliases(other);
                }
                if links.len() >= 3 {
                    best = Some((links, base));
                    break;
                }
            }
            let Some((links, base)) = best else { continue };
            let (mut msk, mut acc) = (u128::MAX, 0u128);
            for (_, op, k) in links.iter().rev() {
                if *op == Opcode::Band {
                    msk &= k;
                    acc &= k;
                } else {
                    acc |= k;
                }
            }
            let w = ty.bits();
            let wm = u128::MAX >> (128 - w);
            hits.push((i, links, base, msk & wm, acc & wm, ty, w));
        }
    }
    let n = hits.len();
    let mut dead: Vec<Inst> = Vec::new();
    for (i, links, base, msk, acc, ty, w) in hits {
        // A later hit's chain can contain an earlier hit's head (nested
        // chains); skip insts already folded away.
        if func.layout.inst_block(i).is_none() {
            continue;
        }
        let mut cur = FuncCursor::new(func).at_inst(i);
        let res = cur.func.dfg.first_result(i);
        let base = cur.func.dfg.resolve_aliases(base);
        let wm = u128::MAX >> (128 - w);
        if msk == wm && acc == 0 {
            cur.func.dfg.clear_results(i);
            cur.func.layout.remove_inst(i);
            cur.func.dfg.change_to_alias(res, base);
        } else if msk == wm {
            let ak = kcon(&mut cur, ty, w, acc);
            cur.func.replace(i).bor(base, ak);
        } else if acc == 0 {
            let mk = kcon(&mut cur, ty, w, msk);
            cur.func.replace(i).band(base, mk);
        } else {
            let mk = kcon(&mut cur, ty, w, msk);
            let nb = cur.ins().band(base, mk);
            let ak = kcon(&mut cur, ty, w, acc);
            cur.func.replace(i).bor(nb, ak);
        }
        dead.extend(links[1..].iter().map(|(d, ..)| *d));
    }
    reap_dead(func, &dead);
    n
}

/// Returns the number of rewrites.
pub fn run(func: &mut Function, x64: bool) -> usize {
    let mut n = 0;
    if crate::pass_enabled("PLIRON_DEFLAG") {
        n += deflag(func);
    }
    if crate::pass_enabled("PLIRON_ULOAD") {
        n += uloads(func);
    }
    let mut uses = None;
    let mut dead_sel: Vec<(Inst, Value, Value)> = Vec::new();
    let mut adj_add: Vec<(Inst, Inst, i64)> = Vec::new();
    let mut mul_id: Vec<(Inst, Value, Value)> = Vec::new();
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            if let InstructionData::Load {
                arg, flags, offset, ..
            } = pos.func.dfg.insts[inst]
                && crate::pass_enabled("PLIRON_LDSEL")
                && loadsel_pair(&mut pos, inst, arg, flags, offset)
            {
                n += 1;
                continue;
            }
            if pos.func.dfg.insts[inst].opcode() == Opcode::Brif {
                // `brif_argspec` must see the brif's own chain before
                // `brif_not` strips it.
                if crate::pass_enabled("PLIRON_ARGSPEC") {
                    n += brif_argspec(pos.func, inst);
                }
                if crate::pass_enabled("PLIRON_BRIFNOT") && brif_not(pos.func, inst) {
                    n += 1;
                }
                continue;
            }
            if let InstructionData::Unary {
                opcode: Opcode::Uextend,
                arg,
            } = pos.func.dfg.insts[inst]
            {
                // `uextend.iN (bxor.i8 (icmp cc a b), 1)` → `uextend.iN
                // (icmp !cc a b)`: rustc's `!bool` on a widened compare
                // (partition's `offset += !(x < p)`). Keeping `uextend`
                // outermost lets a consuming `iadd` fold to `add x, w,
                // uxtb` — one fused inst instead of `uxtb` + `eor`.
                let b = pos.func.dfg.resolve_aliases(arg);
                if let Some((c, 1)) = bin_k(pos.func, b, Opcode::Bxor)
                    && let Some(cd) = pos.func.dfg.resolved_value_def(c).inst()
                    && let InstructionData::IntCompare {
                        opcode: Opcode::Icmp,
                        cond,
                        args: [l, r],
                    } = pos.func.dfg.insts[cd]
                {
                    let inv = match cond {
                        IntCC::Equal => IntCC::NotEqual,
                        IntCC::NotEqual => IntCC::Equal,
                        IntCC::SignedLessThan => IntCC::SignedGreaterThanOrEqual,
                        IntCC::SignedGreaterThanOrEqual => IntCC::SignedLessThan,
                        IntCC::SignedGreaterThan => IntCC::SignedLessThanOrEqual,
                        IntCC::SignedLessThanOrEqual => IntCC::SignedGreaterThan,
                        IntCC::UnsignedLessThan => IntCC::UnsignedGreaterThanOrEqual,
                        IntCC::UnsignedGreaterThanOrEqual => IntCC::UnsignedLessThan,
                        IntCC::UnsignedGreaterThan => IntCC::UnsignedLessThanOrEqual,
                        IntCC::UnsignedLessThanOrEqual => IntCC::UnsignedGreaterThan,
                    };
                    let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
                    let i = pos.ins().icmp(inv, l, r);
                    pos.func.replace(inst).uextend(ty, i);
                    n += 1;
                } else if widen_on() && widen(&mut pos, inst, arg) {
                    n += 1;
                }
                continue;
            }
            // `urem x, 2^k` = `x & (2^k - 1)`: scalar urem is a libcall-ish
            // divide, and the mask form reaches loopvec's cond trees.
            if let InstructionData::Binary {
                opcode: Opcode::Urem,
                args,
            } = pos.func.dfg.insts[inst]
            {
                let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
                if let Some(k) = iconst(pos.func, args[1])
                    && ty.is_int()
                    && !ty.is_vector()
                    && ty.bits() <= 64
                    && k > 0
                    && (k as u64).is_power_of_two()
                    && (ty.bits() == 64 || k < (1i64 << ty.bits()))
                {
                    let m = pos.ins().iconst(ty, k - 1);
                    pos.func.replace(inst).band(args[0], m);
                    n += 1;
                }
                continue;
            }
            // `bxor (uextend.iN (icmp cc a b)), 1` → `uextend.iN (icmp !cc
            // a b)`: rustc's `!bool` on a widened compare (partition's
            // `offset += !(x < p)`). Keeping `uextend` outermost lets a
            // consuming `iadd` fold to `add x, w, uxtb` — one fused inst
            // instead of materializing `uxtb` + `eor` around the `add`.
            if let InstructionData::Binary {
                opcode: Opcode::Bxor,
                args: [a0, a1],
            } = pos.func.dfg.insts[inst]
            {
                let x = if iconst(pos.func, a1) == Some(1) {
                    Some(a0)
                } else if iconst(pos.func, a0) == Some(1) {
                    Some(a1)
                } else {
                    None
                };
                if let Some(d) = x.and_then(|x| pos.func.dfg.resolved_value_def(x).inst())
                    && let InstructionData::Unary {
                        opcode: Opcode::Uextend,
                        arg: c,
                    } = pos.func.dfg.insts[d]
                    && let Some(cd) = pos.func.dfg.resolved_value_def(c).inst()
                    && let InstructionData::IntCompare {
                        opcode: Opcode::Icmp,
                        cond,
                        args: [l, r],
                    } = pos.func.dfg.insts[cd]
                {
                    let inv = match cond {
                        IntCC::Equal => IntCC::NotEqual,
                        IntCC::NotEqual => IntCC::Equal,
                        IntCC::SignedLessThan => IntCC::SignedGreaterThanOrEqual,
                        IntCC::SignedGreaterThanOrEqual => IntCC::SignedLessThan,
                        IntCC::SignedGreaterThan => IntCC::SignedLessThanOrEqual,
                        IntCC::SignedLessThanOrEqual => IntCC::SignedGreaterThan,
                        IntCC::UnsignedLessThan => IntCC::UnsignedGreaterThanOrEqual,
                        IntCC::UnsignedGreaterThanOrEqual => IntCC::UnsignedLessThan,
                        IntCC::UnsignedGreaterThan => IntCC::UnsignedLessThanOrEqual,
                        IntCC::UnsignedLessThanOrEqual => IntCC::UnsignedGreaterThan,
                    };
                    let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
                    let i = pos.ins().icmp(inv, l, r);
                    pos.func.replace(inst).uextend(ty, i);
                    n += 1;
                }
                continue;
            }
            // `iadd x, uextend(bool)` / `isub x, uextend(bool)` →
            // `select bool, x+1, x` / `select bool, x-1, x`: x64 pays
            // `setb`+`movzbq` for the widened bool inside `add`, while
            // `select(icmp)` lowers to `cmp`+`cmov` — one fewer inst and
            // a shorter loop-carried recurrence (partition's
            // `offset += is_less`). aarch64 already fuses
            // `add x, w, uxtb`, so this stays x64-only.
            if x64
                && let InstructionData::Binary {
                    opcode: op @ (Opcode::Iadd | Opcode::Isub),
                    args,
                } = pos.func.dfg.insts[inst]
            {
                let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
                let cx = |v| uext01(pos.func, v);
                let sel = if ty.is_int() && !ty.is_vector() && ty.bits() <= 64 {
                    if let Some(c) = cx(args[1]) {
                        Some((c, args[0], if op == Opcode::Iadd { 1 } else { -1 }))
                    } else if op == Opcode::Iadd {
                        cx(args[0]).map(|c| (c, args[1], 1))
                    } else {
                        None
                    }
                } else {
                    None
                };
                // i64 unsigned-`icmp` bools instead fold into
                // `cmp`+`adc`/`sbb` off CF at isel — rewriting to a
                // select would hide that pattern.
                let sel = sel.filter(|(c, ..)| ty != types::I64 || !ucmp_bool(pos.func, *c));
                if let Some((c, x, d)) = sel {
                    let inc = pos.ins().iadd_imm_s(x, d);
                    pos.func.replace(inst).select(c, inc, x);
                    n += 1;
                }
                continue;
            }
            // `imul (imul x, c1), c2` → `imul x, c1·c2` (mod 2^w):
            // constant factors multiply through. A `div_u128_const`
            // quotient `(x - r)·d⁻¹` followed by the source's own `· d`
            // collapses outright — `d⁻¹·d ≡ 1 (mod 2^w)` — recovering the
            // `(x/d)·d = x - x%d` identity the IR-level fold missed. Only
            // when the inner mul is single-use; otherwise we'd clone it.
            let mut imul_folded = false;
            if let InstructionData::Binary {
                opcode: Opcode::Imul,
                args,
            } = pos.func.dfg.insts[inst]
            {
                let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
                let w = ty.bits();
                if ty.is_int() && !ty.is_vector() {
                    let m = u128::MAX >> (128 - w);
                    let hit = (0..2).find_map(|i| {
                        let c2 = kval(pos.func, args[i])?;
                        let xv = pos.func.dfg.resolve_aliases(args[1 - i]);
                        if uses
                            .get_or_insert_with(|| use_counts(pos.func))
                            .get(&xv)
                            .copied()
                            .unwrap_or(0)
                            != 1
                        {
                            return None;
                        }
                        let InstructionData::Binary {
                            opcode: Opcode::Imul,
                            args: ia,
                        } = pos.func.dfg.insts[pos.func.dfg.value_def(xv).inst()?]
                        else {
                            return None;
                        };
                        (0..2).find_map(|j| {
                            let c1 = kval(pos.func, ia[j])?;
                            let x = pos.func.dfg.resolve_aliases(ia[1 - j]);
                            Some((x, c1.wrapping_mul(c2) & m))
                        })
                    });
                    if let Some((x, c)) = hit {
                        if c == 1 {
                            mul_id.push((inst, pos.func.dfg.first_result(inst), x));
                        } else {
                            let kv = kcon(&mut pos, ty, w, c);
                            pos.func.replace(inst).imul(x, kv);
                        }
                        n += 1;
                        imul_folded = true;
                    }
                }
            }
            if imul_folded {
                continue;
            }
            // `imul.i128 x, iconcat(k, 0)` → a 64×128 limb product:
            // `hi' = x_hi·k + umulhi(x_lo, k)` (one `madd` at isel) and
            // `lo' = x_lo·k`. The generic 128×128 legalization multiplies
            // x's limbs by the constant's dead zero high limb as well.
            if let InstructionData::Binary {
                opcode: Opcode::Imul,
                args,
            } = pos.func.dfg.insts[inst]
            {
                let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
                if ty == types::I128 {
                    for i in 0..2 {
                        let x = pos.func.dfg.resolve_aliases(args[i]);
                        let kv = pos.func.dfg.resolve_aliases(args[1 - i]);
                        let Some(kd) = pos.func.dfg.value_def(kv).inst() else {
                            continue;
                        };
                        let InstructionData::Binary {
                            opcode: Opcode::Iconcat,
                            args: kargs,
                        } = pos.func.dfg.insts[kd]
                        else {
                            continue;
                        };
                        if iconst(pos.func, kargs[1]) != Some(0) {
                            continue;
                        }
                        let kl = kargs[0];
                        let (xl, xh) = pos.ins().isplit(x);
                        let h1 = pos.ins().umulhi(xl, kl);
                        let h2 = pos.ins().imul(xh, kl);
                        let h = pos.ins().iadd(h1, h2);
                        let l = pos.ins().imul(xl, kl);
                        pos.func.replace(inst).iconcat(l, h);
                        n += 1;
                        break;
                    }
                }
                continue;
            }
            // `band (x >= lo), (x < hi)` → `(x - lo) <u w` — the
            // `Range*::contains` merge, when rustc lowered `&&` as a
            // plain `&` on bools.
            if let InstructionData::Binary {
                opcode: Opcode::Band,
                args,
            } = pos.func.dfg.insts[inst]
                && crate::pass_enabled("PLIRON_RGPAIR")
                && let Some(f) = rgpair(pos.func, args[0], args[1])
            {
                n += rgapply(&mut pos, inst, f);
                continue;
            }
            if matches!(
                pos.func.dfg.insts[inst].opcode(),
                Opcode::Select | Opcode::Bitselect
            ) {
                // `select c, x, x` → `x` (later GVN rounds unify equivalent
                // defs into identical arms) and `select k, a, b` → the
                // constant's arm. Defer the removal — `pos` points at the
                // inst until `next_inst` advances.
                if let InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } = pos.func.dfg.insts[inst]
                {
                    let arm = if pos.func.dfg.resolve_aliases(args[1])
                        == pos.func.dfg.resolve_aliases(args[2])
                    {
                        Some(args[1])
                    } else {
                        iconst(pos.func, args[0]).map(|c| args[if c != 0 { 1 } else { 2 }])
                    };
                    if let Some(a) = arm {
                        let a = pos.func.dfg.resolve_aliases(a);
                        let r = pos.func.dfg.first_result(inst);
                        dead_sel.push((inst, r, a));
                        n += 1;
                        continue;
                    }
                    // `select (x == k ? k : x)` → `x`: the constant arm
                    // equals `x` on exactly the domain that picks it.
                    // MIR's `Option<NonNull>` sentinel materializes this
                    // shape (`x == 0 ? 0 : x`) and, unfolded, keeps
                    // address webs from resolving to their slot.
                    if let Some(ci) = pos.func.dfg.resolved_value_def(args[0]).inst()
                        && let InstructionData::IntCompare {
                            opcode: Opcode::Icmp,
                            cond,
                            args: [ia, ib],
                        } = pos.func.dfg.insts[ci]
                    {
                        let mut folded = None;
                        for (tt, ff, cc) in [
                            (args[1], args[2], cond),
                            (args[2], args[1], cond.complement()),
                        ] {
                            if cc != IntCC::Equal || iconst(pos.func, tt).is_none() {
                                continue;
                            }
                            let matches = match (iconst(pos.func, ia), iconst(pos.func, ib)) {
                                (Some(ka), None) => Some((ib, ka)),
                                (None, Some(kb)) => Some((ia, kb)),
                                _ => None,
                            };
                            if let Some((x, kc)) = matches
                                && iconst(pos.func, tt) == Some(kc)
                                && pos.func.dfg.resolve_aliases(x)
                                    == pos.func.dfg.resolve_aliases(ff)
                            {
                                folded = Some(x);
                                break;
                            }
                        }
                        if let Some(a) = folded {
                            let a = pos.func.dfg.resolve_aliases(a);
                            let r = pos.func.dfg.first_result(inst);
                            dead_sel.push((inst, r, a));
                            n += 1;
                            continue;
                        }
                    }
                    // `select c2, .., select(c1, t, e)`: the arm reached
                    // under `c2`'s equality outcome carries the fact
                    // `x + a2 == k`, which decides a nested compare
                    // `x + a1 == k` on the same `x`/`k` — `x+3==k` and
                    // `x-5==k` can't co-hold, so `if x+3==k {a+=1}
                    // if x-5!=k {a+=2}` only ever applies `+2`. Rewrite
                    // the outer arm to the nested select's dead arm.
                    if let Some((eq2, x2, a2, k2)) = icmp_chain(pos.func, args[0]) {
                        let arm_i = if eq2 { 1 } else { 2 };
                        let inner = pos.func.dfg.resolve_aliases(args[arm_i]);
                        if let Some(si) = pos.func.dfg.value_def(inner).inst()
                            && let InstructionData::Ternary {
                                opcode: Opcode::Select,
                                args: sa,
                            } = pos.func.dfg.insts[si]
                            && let Some((eq1, x1, a1, k1)) = icmp_chain(pos.func, sa[0])
                            && pos.func.dfg.resolve_aliases(x1) == x2
                            && let Some(v) = eq_decided(
                                pos.func,
                                pos.func.dfg.value_type(k1).bits(),
                                a1,
                                k1,
                                a2,
                                k2,
                            )
                        {
                            let arm = sa[if v == eq1 { 1 } else { 2 }];
                            if let InstructionData::Ternary { args, .. } =
                                &mut pos.func.dfg.insts[inst]
                            {
                                args[arm_i] = arm;
                            }
                            n += 1;
                            continue;
                        }
                    }
                    // `select (x >= lo), (x < hi), 0` → `(x - lo) <u w` —
                    // `Range*::contains` as one sub+compare instead of a
                    // guard branch's worth of work; under loopvec a
                    // single pcmpgtd group per lane.
                    if crate::pass_enabled("PLIRON_RGPAIR")
                        && iconst(pos.func, args[2]) == Some(0)
                        && let Some(f) = rgpair(pos.func, args[0], args[1])
                    {
                        n += rgapply(&mut pos, inst, f);
                        continue;
                    }
                }
                if crate::pass_enabled("PLIRON_SELNOT") && sel_not(pos.func, inst) {
                    n += 1;
                }
                if crate::pass_enabled("PLIRON_MINMAX") && minmax(&mut pos, inst) {
                    n += 1;
                }
                continue;
            }
            // `iadd (select c, k1, k2), K` with adjacent const arms →
            // `select c, k1+K, k2+K`: the arms stay adjacent so aarch64
            // emits a single `csinc` off one materialized register,
            // absorbing the add (3 → 2 insts once the select is dead).
            // Deferred — profitable only if the select's other uses die
            // with the arm-fact rewrite above, which runs after this
            // inst's position in the scan.
            if !x64
                && crate::pass_enabled("PLIRON_SELADD")
                && let InstructionData::Binary {
                    opcode: Opcode::Iadd,
                    args,
                } = pos.func.dfg.insts[inst]
            {
                let sel_k = |i: usize| {
                    iconst(pos.func, args[i ^ 1])?;
                    let v = pos.func.dfg.resolve_aliases(args[i]);
                    let si = pos.func.dfg.value_def(v).inst()?;
                    let InstructionData::Ternary {
                        opcode: Opcode::Select,
                        args: sa,
                    } = pos.func.dfg.insts[si]
                    else {
                        return None;
                    };
                    let (k1, k2) = (keval(pos.func, sa[1])?, keval(pos.func, sa[2])?);
                    (k1.wrapping_sub(k2).abs() == 1).then_some((si, iconst(pos.func, args[i ^ 1])?))
                };
                if let Some((si, k)) = sel_k(0).or_else(|| sel_k(1)) {
                    adj_add.push((inst, si, k));
                    continue;
                }
            }
            let InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                cond,
                args,
            } = pos.func.dfg.insts[inst]
            else {
                continue;
            };
            // `icmp cc x, bound` folds to a flag constant when `bound` is
            // outside `x`'s proven range — either the operand type's extreme
            // (`x >u MAX`, `x <s MIN`, `x <=u MAX`, ...) or a tighter ceiling
            // from `vmax` (`uextend`/`band`/`ushr`/`urem`-bounded values, like
            // `x8 <u 0x100000` bounds checks on narrow indexes).
            let ty = pos.func.dfg.value_type(args[0]);
            if ty.is_int() && !ty.is_vector() && ty.bits() <= 64 {
                let (mut cc, mut k, mut x) = (cond, args[1], args[0]);
                if iconst(pos.func, args[0]).is_some() {
                    cc = cc.swap_args();
                    k = args[0];
                    x = args[1];
                }
                if let Some(k) = iconst(pos.func, k) {
                    let bits = ty.bits() as u32;
                    let umax = if bits == 64 {
                        u64::MAX
                    } else {
                        (1u64 << bits) - 1
                    };
                    let smin = (-1i128 << (bits - 1)) as i64;
                    let smax = ((1i128 << (bits - 1)) - 1) as i64;
                    let ku = k as u64 & umax;
                    // `icmp eq/ne (stack_addr ..), 0` → `0`/`1`: a stack
                    // slot's address is never null — hashbrown's
                    // `ctrl.is_null()` empty-table guard on a `&RawTable`
                    // local compares the `stack_addr` directly, and the
                    // surviving `select c, 0, p` keeps the slot's memory
                    // ops keyed under a second, opaque root. (`func_addr`/
                    // `symbol_value` are not folded: weak externs resolve
                    // to 0.)
                    if ku == 0
                        && matches!(cc, IntCC::Equal | IntCC::NotEqual)
                        && pos.func.dfg.resolved_value_def(x).inst().is_some_and(|d| {
                            matches!(
                                pos.func.dfg.insts[d].opcode(),
                                Opcode::StackAddr | Opcode::DynamicStackAddr
                            )
                        })
                    {
                        let c = i64::from(cc == IntCC::NotEqual);
                        pos.func.replace(inst).iconst(types::I8, c);
                        n += 1;
                        continue;
                    }
                    // `mx`: proven unsigned ceiling on the non-const operand;
                    // `nn`: x's range stays in the signed-positive half, so
                    // signed compares against `k` behave like unsigned ones.
                    let (mx, nn) = if crate::pass_enabled("PLIRON_VMAX") {
                        let m = vmax(pos.func, x);
                        (m, m <= smax as u64)
                    } else {
                        // Type max never fits the signed-positive half.
                        (umax, false)
                    };
                    let mx = mx as i64;
                    let c = match cc {
                        IntCC::UnsignedGreaterThan if ku == umax => Some(0),
                        IntCC::UnsignedLessThanOrEqual if ku == umax => Some(1),
                        IntCC::UnsignedLessThan if ku == 0 => Some(0),
                        IntCC::UnsignedGreaterThanOrEqual if ku == 0 => Some(1),
                        IntCC::SignedGreaterThan if k == smax => Some(0),
                        IntCC::SignedLessThanOrEqual if k == smax => Some(1),
                        IntCC::SignedLessThan if k == smin => Some(0),
                        IntCC::SignedGreaterThanOrEqual if k == smin => Some(1),
                        IntCC::Equal if ku > mx as u64 => Some(0),
                        IntCC::NotEqual if ku > mx as u64 => Some(1),
                        IntCC::UnsignedLessThan if ku > mx as u64 => Some(1),
                        IntCC::UnsignedLessThanOrEqual if ku >= mx as u64 => Some(1),
                        IntCC::UnsignedGreaterThan if ku >= mx as u64 => Some(0),
                        IntCC::UnsignedGreaterThanOrEqual if ku > mx as u64 => Some(0),
                        IntCC::SignedLessThan if nn && k > mx => Some(1),
                        IntCC::SignedLessThan if nn && k <= 0 => Some(0),
                        IntCC::SignedLessThanOrEqual if nn && k >= mx => Some(1),
                        IntCC::SignedLessThanOrEqual if nn && k < 0 => Some(0),
                        IntCC::SignedGreaterThan if nn && k >= mx => Some(0),
                        IntCC::SignedGreaterThan if nn && k < 0 => Some(1),
                        IntCC::SignedGreaterThanOrEqual if nn && k > mx => Some(0),
                        IntCC::SignedGreaterThanOrEqual if nn && k <= 0 => Some(1),
                        _ => None,
                    };
                    if let Some(c) = c {
                        pos.func.replace(inst).iconst(types::I8, c);
                        n += 1;
                        continue;
                    }
                    // `icmp cc (select c, ka, kb), k` with constant arms:
                    // evaluate `ka cc k` / `kb cc k` — equal results fold
                    // the compare to a flag constant; split results make it
                    // `c` or `!c`. Looking through one `uextend`/`ireduce`
                    // wrapper is just arm masking to the compare width
                    // (`x8 == (x64 (select c, 0, 1))` is `c == 1`).
                    if let Some(f) = icmp_selfold(pos.func, x, cc, ku, bits) {
                        match f {
                            SelFold::K(b) => {
                                pos.func.replace(inst).iconst(types::I8, b as i64);
                            }
                            SelFold::C(c, inv) => {
                                let cty = pos.func.dfg.value_type(c);
                                let z = pos.ins().iconst(cty, 0);
                                pos.func.replace(inst).icmp(
                                    if inv { IntCC::Equal } else { IntCC::NotEqual },
                                    c,
                                    z,
                                );
                            }
                        }
                        n += 1;
                        continue;
                    }
                    // `x <=u C` → `x <u C+1`, `x >u C` → `x >=u C+1` —
                    // the strict/swapped-bound forms keep the constant
                    // in the compare's immediate-capable rhs so isel's
                    // CF-fusion rules (`cmp $imm` + `adc`/`sbb`) don't
                    // materialize a register for a reversed compare
                    // (e.g. `count += c <=u 127`). Same result bit for
                    // every consumer.
                    let ncc = match cc {
                        IntCC::UnsignedLessThanOrEqual => Some(IntCC::UnsignedLessThan),
                        IntCC::UnsignedGreaterThan => Some(IntCC::UnsignedGreaterThanOrEqual),
                        _ => None,
                    };
                    if let Some(ncc) = ncc
                        && ku < umax
                    {
                        let c1 = pos.ins().iconst(ty, ku.wrapping_add(1) as i64);
                        if let InstructionData::IntCompare { cond, args, .. } =
                            &mut pos.func.dfg.insts[inst]
                        {
                            *cond = ncc;
                            *args = [x, c1];
                            n += 1;
                            continue;
                        }
                    }
                }
            }
            if crate::pass_enabled("PLIRON_BITTAB")
                && cond == IntCC::NotEqual
                && bittab(&mut pos, inst, args)
            {
                n += 1;
                continue;
            }
            if crate::pass_enabled("PLIRON_ICMPEQFOLD")
                && icmp_eqfold(&mut pos, inst, cond, args, ty, &mut uses)
            {
                n += 1;
                continue;
            }
            if crate::pass_enabled("PLIRON_DIVCHK")
                && ty.is_int()
                && !ty.is_vector()
                && ty.bits() <= 64
                && matches!(cond, IntCC::Equal | IntCC::NotEqual)
                && divchk(&mut pos, inst, cond, args, ty)
            {
                n += 1;
                continue;
            }
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
    for (inst, res, arm) in dead_sel {
        pos.func.dfg.clear_results(inst);
        pos.func.layout.remove_inst(inst);
        pos.func.dfg.change_to_alias(res, arm);
    }
    for (inst, res, x) in mul_id {
        pos.func.dfg.clear_results(inst);
        pos.func.layout.remove_inst(inst);
        pos.func.dfg.change_to_alias(res, x);
    }
    // Apply the deferred adjacent-const `iadd+select` folds: the inner
    // select must be down to the `iadd` as its only use (the arm-fact
    // rewrite can kill a second use), otherwise the rewrite would add a
    // select on top of a still-live one.
    if !adj_add.is_empty() {
        let uses = use_counts(func);
        for (inst, si, k) in adj_add {
            // PLIRON: `si`/`inst` were captured during the scan; the
            // dead_sel/mul_id apply loops above may have deleted them.
            if func.layout.inst_block(si).is_none()
                || func.layout.inst_block(inst).is_none()
            {
                continue;
            }
            let res = func.dfg.first_result(si);
            if uses
                .get(&func.dfg.resolve_aliases(res))
                .copied()
                .unwrap_or(0)
                > 1
            {
                continue;
            }
            let InstructionData::Ternary {
                opcode: Opcode::Select,
                args: sa,
            } = func.dfg.insts[si]
            else {
                continue;
            };
            let (Some(k1), Some(k2)) = (keval(func, sa[1]), keval(func, sa[2])) else {
                continue;
            };
            let ty = func.dfg.value_type(func.dfg.first_result(inst));
            let mut cur = FuncCursor::new(func).at_inst(inst);
            let c = sa[0];
            let t = cur.ins().iconst(ty, k1.wrapping_add(k));
            let f = cur.ins().iconst(ty, k2.wrapping_add(k));
            cur.func.replace(inst).select(c, t, f);
            n += 1;
        }
    }
    // Region collapse after `brif_not` has normalised `brif` args to the
    // underlying conditions.
    if crate::pass_enabled("PLIRON_SELREGION") {
        n += sel_regions(func);
    }
    if crate::pass_enabled("PLIRON_BITMERGE") {
        n += bitmerge(func);
    }
    n
}

/// Forward-propagate unsigned upper bounds across CFG edges — a
/// `brif (icmp uge d, 10)` proves `d <= 9` in the fall-through, params
/// join as the `max` of their edge args — and through add/mul/band
/// chains; kill provably-dead checks:
/// `umul_overflow`/`uadd_overflow` flags, `ult (iadd a,b), a` carries, and
/// compares vs constant the bounds decide. The peeled `acc*10+d` parse
/// chains (`d <= 9`, <= 8 digits) can't overflow their type.
pub fn ovkill(func: &mut Function) -> usize {
    use cranelift_codegen::flowgraph::ControlFlowGraph;
    type BMap = FxHashMap<Value, u64>;
    let cfg = ControlFlowGraph::with_function(func);

    fn umax_ty(ty: Type) -> u64 {
        let b = ty.bits();
        if b >= 64 { u64::MAX } else { (1u64 << b) - 1 }
    }
    fn bnd(m: &BMap, func: &Function, v: Value) -> u64 {
        // Bounds are recorded under the result value as-written; an
        // `ovkill` rewrite can turn it into an alias afterwards, so check
        // both spellings and keep the tighter (either is a valid bound).
        let mut b = u64::MAX;
        if let Some(&k) = m.get(&v) {
            b = b.min(k);
        }
        let v = func.dfg.resolve_aliases(v);
        if let Some(&k) = m.get(&v) {
            b = b.min(k);
        }
        if b != u64::MAX {
            return b;
        }
        if let Some(k) = iconst(func, v) {
            return (k as u64) & umax_ty(func.dfg.value_type(v));
        }
        vmax(func, v)
    }
    fn inst_bounds(func: &Function, m: &mut BMap, i: Inst) {
        for &r in func.dfg.inst_results(i) {
            let t = func.dfg.value_type(r);
            if !t.is_int() || t.is_vector() || t.bits() > 64 {
                continue;
            }
            let um = umax_ty(t);
            let b = match func.dfg.insts[i] {
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm,
                } => (imm.bits() as u64) & um,
                InstructionData::Binary { opcode, args } => {
                    let (a, x) = (bnd(m, func, args[0]), bnd(m, func, args[1]));
                    match opcode {
                        Opcode::Iadd | Opcode::UaddOverflow => {
                            a.checked_add(x).map_or(um, |s| s.min(um))
                        }
                        Opcode::Imul | Opcode::UmulOverflow => {
                            a.checked_mul(x).map_or(um, |s| s.min(um))
                        }
                        Opcode::Band => a.min(x),
                        _ => continue,
                    }
                }
                InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } => bnd(m, func, args[1]).max(bnd(m, func, args[2])),
                _ => continue,
            };
            if b < um {
                m.insert(r, b);
            }
        }
    }

    // `brif (icmp cc x, k)` edge facts: `x <= k-1` on the `ult`/`uge`-fail
    // edges, `x <= k` on `ule`/`ugt`-fail, `x == k` on `eq`-hit. Only
    // meaningful when the cond selects this edge — if every destination
    // is `b` there is no fact.
    fn edge_fact(func: &Function, pi: Inst, matches: &[(usize, &BlockCall)]) -> BMap {
        let mut f = BMap::default();
        if matches.len() != 1 {
            return f;
        }
        let InstructionData::Brif { arg, .. } = func.dfg.insts[pi] else {
            return f;
        };
        let Some(ci) = func.dfg.resolved_value_def(arg).inst() else {
            return f;
        };
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond: cc0,
            args: ca,
        } = func.dfg.insts[ci]
        else {
            return f;
        };
        let norm = if let Some(k) = iconst(func, ca[1]) {
            Some((cc0, func.dfg.resolve_aliases(ca[0]), k))
        } else {
            iconst(func, ca[0]).map(|k| (cc0.swap_args(), func.dfg.resolve_aliases(ca[1]), k))
        };
        if let Some((cc, x, k)) = norm {
            let ub = match (cc, matches[0].0) {
                (IntCC::UnsignedLessThan, 0) => k.checked_sub(1),
                (IntCC::UnsignedLessThanOrEqual, 0) => Some(k),
                (IntCC::Equal, 0) => Some(k),
                (IntCC::NotEqual, 1) => Some(k),
                (IntCC::UnsignedGreaterThan, 1) => Some(k),
                (IntCC::UnsignedGreaterThanOrEqual, 1) => k.checked_sub(1),
                _ => None,
            };
            if let Some(ub) = ub {
                f.insert(x, (ub as u64) & umax_ty(func.dfg.value_type(x)));
            }
        }
        f
    }

    // `ememo`: bounds provable at block entry; `memo`: bounds at end.
    // `visiting`: in-progress `end_b`s — a backedge re-entry returns empty
    // (unbounded), which is conservative and breaks CFG cycles.
    fn entry_b(
        func: &Function,
        cfg: &ControlFlowGraph,
        ememo: &mut FxHashMap<Block, Rc<BMap>>,
        memo: &mut FxHashMap<Block, Rc<BMap>>,
        visiting: &mut FxHashSet<Block>,
        b: Block,
        depth: usize,
    ) -> Rc<BMap> {
        if let Some(m) = ememo.get(&b) {
            return m.clone();
        }
        let mut m = BMap::default();
        let preds: Vec<_> = cfg.pred_iter(b).collect();
        if !preds.is_empty() && depth > 0 {
            // Edge args → this block's params: `min` over a pred's matching
            // edges, `max` over preds (an unsigned bound is an upper bound —
            // the join must cover every incoming value). Each pred's
            // `icmp` fact qualifies its own edge args.
            let ebnd = |pe: &BMap, f: &BMap, v: Value| {
                let v = func.dfg.resolve_aliases(v);
                f.get(&v)
                    .map_or_else(|| bnd(pe, func, v), |&fb| fb.min(bnd(pe, func, v)))
            };
            // Single-pred: every bound at the pred's end still holds here —
            // seed the whole map (arg-less `jump` successors see nothing
            // otherwise). Facts and param bindings may tighten it below.
            if preds.len() == 1 {
                let pe = end_b(func, cfg, ememo, memo, visiting, preds[0].block, depth - 1);
                m = (*pe).clone();
            }
            for (pix, p) in func.dfg.block_params(b).iter().enumerate() {
                let pty = func.dfg.value_type(*p);
                if !pty.is_int() || pty.is_vector() || pty.bits() > 64 {
                    continue;
                }
                let mut bv = Some(0u64);
                for pred in &preds {
                    let (pb, pi) = (pred.block, pred.inst);
                    let pe = end_b(func, cfg, ememo, memo, visiting, pb, depth - 1);
                    let dests = func.dfg.insts[pi]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                    let matches: Vec<(usize, &BlockCall)> = dests
                        .iter()
                        .enumerate()
                        .filter(|(_, bc)| bc.block(&func.dfg.value_lists) == b)
                        .collect();
                    let fact = edge_fact(func, pi, &matches);
                    let eb = matches
                        .iter()
                        .filter_map(|(_, bc)| {
                            bc.args(&func.dfg.value_lists)
                                .nth(pix)
                                .and_then(|a| match a {
                                    BlockArg::Value(v) => Some(v),
                                    _ => None,
                                })
                        })
                        .map(|v| ebnd(&pe, &fact, v))
                        .min();
                    bv = match (bv, eb) {
                        (Some(a), Some(e)) => Some(a.max(e)),
                        _ => None,
                    };
                }
                if let Some(bv) = bv
                    && bv < umax_ty(pty)
                {
                    m.insert(*p, bv);
                }
            }
            // Single-pred: the `icmp` edge fact also bounds `x` itself in
            // this block (`brif (ult d, 10)` → `d <= 9` on the taken edge).
            // Skip the destination scan unless the pred is a `brif` —
            // `edge_fact` only reads those, and a `br_table` pred lists
            // every arm (O(arms²) on a giant dispatch).
            if preds.len() == 1
                && matches!(func.dfg.insts[preds[0].inst], InstructionData::Brif { .. })
            {
                let pi = preds[0].inst;
                let dests = func.dfg.insts[pi]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
                let matches: Vec<(usize, &BlockCall)> = dests
                    .iter()
                    .enumerate()
                    .filter(|(_, bc)| bc.block(&func.dfg.value_lists) == b)
                    .collect();
                for (x, ub) in edge_fact(func, pi, &matches) {
                    m.entry(x).and_modify(|e| *e = (*e).min(ub)).or_insert(ub);
                }
            }
        }
        let m = Rc::new(m);
        ememo.insert(b, m.clone());
        m
    }
    fn end_b(
        func: &Function,
        cfg: &ControlFlowGraph,
        ememo: &mut FxHashMap<Block, Rc<BMap>>,
        memo: &mut FxHashMap<Block, Rc<BMap>>,
        visiting: &mut FxHashSet<Block>,
        b: Block,
        depth: usize,
    ) -> Rc<BMap> {
        if let Some(m) = memo.get(&b) {
            return m.clone();
        }
        if !visiting.insert(b) {
            return Rc::new(BMap::default());
        }
        let mut m = (*entry_b(func, cfg, ememo, memo, visiting, b, depth)).clone();
        for i in func.layout.block_insts(b) {
            inst_bounds(func, &mut m, i);
        }
        visiting.remove(&b);
        let m = Rc::new(m);
        memo.insert(b, m.clone());
        m
    }

    let mut ememo = FxHashMap::default();
    let mut memo = FxHashMap::default();
    let mut visiting = FxHashSet::default();
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        let mut m = (*entry_b(func, &cfg, &mut ememo, &mut memo, &mut visiting, b, 512)).clone();
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            match func.dfg.insts[i] {
                InstructionData::Binary {
                    opcode: op @ (Opcode::UaddOverflow | Opcode::UmulOverflow),
                    args,
                } => {
                    let res = func.dfg.inst_results(i).to_vec();
                    if res.len() != 2 {
                        continue;
                    }
                    let um = umax_ty(func.dfg.value_type(res[0]));
                    let (a, x) = (bnd(&m, func, args[0]), bnd(&m, func, args[1]));
                    let ok = match op {
                        Opcode::UaddOverflow => a.checked_add(x),
                        _ => a.checked_mul(x),
                    };
                    let Some(lo) = ok.filter(|&s| s <= um) else {
                        inst_bounds(func, &mut m, i);
                        continue;
                    };
                    let mut pos = FuncCursor::new(func).at_inst(i);
                    let nv = match op {
                        Opcode::UaddOverflow => pos.ins().iadd(args[0], args[1]),
                        _ => pos.ins().imul(args[0], args[1]),
                    };
                    let z = pos.ins().iconst(types::I8, 0);
                    pos.func.dfg.clear_results(i);
                    pos.func.dfg.change_to_alias(res[0], nv);
                    pos.func.dfg.change_to_alias(res[1], z);
                    pos.func.layout.remove_inst(i);
                    m.insert(nv, lo);
                    n += 1;
                }
                InstructionData::IntCompare {
                    opcode: Opcode::Icmp,
                    cond,
                    args,
                } => {
                    let ty = func.dfg.value_type(args[0]);
                    if !ty.is_int() || ty.is_vector() || ty.bits() > 64 {
                        inst_bounds(func, &mut m, i);
                        continue;
                    }
                    let um = umax_ty(ty);
                    // `ult (a+b), a` / `ult (a+b), b`: the uadd carry —
                    // dead when `max(a)+max(b)` can't wrap.
                    if cond == IntCC::UnsignedLessThan {
                        let s = func.dfg.resolve_aliases(args[0]);
                        let c = func.dfg.resolve_aliases(args[1]);
                        let isum = func.dfg.value_def(s).inst();
                        let add = isum.and_then(|si| match func.dfg.insts[si] {
                            InstructionData::Binary {
                                opcode: Opcode::Iadd,
                                args: aa,
                            } => Some(aa),
                            _ => None,
                        });
                        if let Some(aa) = add {
                            let (a0, a1) = (
                                func.dfg.resolve_aliases(aa[0]),
                                func.dfg.resolve_aliases(aa[1]),
                            );
                            if (a0 == c || a1 == c)
                                && bnd(&m, func, aa[0])
                                    .checked_add(bnd(&m, func, aa[1]))
                                    .is_some_and(|s| s <= um)
                            {
                                func.replace(i).iconst(types::I8, 0);
                                n += 1;
                                continue;
                            }
                        }
                    }
                    // compares a bound decides outright.
                    let (cc, x, k) = if let Some(k) = iconst(func, args[1]) {
                        (cond, func.dfg.resolve_aliases(args[0]), k)
                    } else if let Some(k) = iconst(func, args[0]) {
                        (cond.swap_args(), func.dfg.resolve_aliases(args[1]), k)
                    } else {
                        inst_bounds(func, &mut m, i);
                        continue;
                    };
                    let ku = (k as u64) & um;
                    let b = bnd(&m, func, x);
                    let f = match cc {
                        IntCC::UnsignedLessThan if b < ku => Some(1),
                        IntCC::UnsignedLessThanOrEqual if b <= ku => Some(1),
                        IntCC::UnsignedGreaterThan if b <= ku => Some(0),
                        IntCC::UnsignedGreaterThanOrEqual if b < ku => Some(0),
                        IntCC::Equal if b < ku => Some(0),
                        IntCC::NotEqual if b < ku => Some(1),
                        _ => None,
                    };
                    if let Some(f) = f {
                        func.replace(i).iconst(types::I8, f);
                        n += 1;
                        continue;
                    }
                    inst_bounds(func, &mut m, i);
                }
                _ => inst_bounds(func, &mut m, i),
            }
        }
    }
    n
}

/// `x % C ==/!= 0` needs only a divisibility test, not the remainder.
/// For odd C, `x·M` (M = C⁻¹ mod 2^w) is the quotient exactly when C|x:
/// unsigned `x·M <=u B`; signed `x·M + floor(B/2) <=u B`, B = (2^w-1)/C.
/// (LLVM emits the same magic for `x % 3 != 0` filters.)
fn divchk(pos: &mut FuncCursor, inst: Inst, cond: IntCC, args: [Value; 2], ty: Type) -> bool {
    // Multiplicative inverse of odd `c` mod 2^64 via Newton iteration —
    // doubles correct bits each step (c already has 3 from x₀ = c).
    fn modinv(c: u64) -> u64 {
        let mut x = c;
        for _ in 0..6 {
            x = x.wrapping_mul(2u64.wrapping_sub(c.wrapping_mul(x)));
        }
        x
    }
    let w = ty.bits() as u32;
    let umax = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
    // Normalize `icmp cc rem, 0` with the constant on the right.
    let (x, _d) = match (iconst(pos.func, args[0]), iconst(pos.func, args[1])) {
        (None, Some(0)) => (args[0], args[1]),
        (Some(0), None) => (args[1], args[0]),
        _ => return false,
    };
    let Some(i) = pos.func.dfg.resolved_value_def(x).inst() else {
        return false;
    };
    let InstructionData::Binary { opcode, args: ra } = pos.func.dfg.insts[i] else {
        return false;
    };
    let signed = match opcode {
        Opcode::Urem => false,
        Opcode::Srem => true,
        _ => return false,
    };
    // `x % C == 0` iff `x % |C| == 0` — take the magnitude; the masked
    // value must itself be odd and >= 3 for the magic form. (`iconst`
    // returns the raw bit pattern — sign-extend to the operand width
    // for `srem` so a stored -3 isn't read as a huge divisor.)
    let Some(c) = iconst(pos.func, ra[1]).map(|c| {
        let cu = c as u64 & umax;
        if signed && cu >> (w - 1) != 0 {
            ((cu | !umax) as i64).unsigned_abs()
        } else {
            cu
        }
    }) else {
        return false;
    };
    if c < 3 || c % 2 == 0 {
        return false;
    }
    let m = modinv(c) & umax;
    let b = umax / c;
    let xv = ra[0];
    let mk = |pos: &mut FuncCursor, k: u64| pos.ins().iconst(ty, k as i64);
    let km = mk(pos, m);
    let p = pos.ins().imul(xv, km);
    let p = if signed {
        let ko = mk(pos, b / 2);
        pos.ins().iadd(p, ko)
    } else {
        p
    };
    let cc = if cond == IntCC::Equal {
        IntCC::UnsignedLessThanOrEqual
    } else {
        IntCC::UnsignedGreaterThan
    };
    let k = mk(pos, b);
    pos.func.replace(inst).icmp(cc, p, k);
    true
}

/// Inst-arg + branch-arg use counts for every value, built on demand.
fn use_counts(func: &Function) -> FxHashMap<Value, usize> {
    let mut m: FxHashMap<Value, usize> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for &a in func.dfg.inst_args(i) {
                *m.entry(func.dfg.resolve_aliases(a)).or_default() += 1;
            }
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for a in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(x) = a {
                        *m.entry(func.dfg.resolve_aliases(x)).or_default() += 1;
                    }
                }
            }
        }
    }
    m
}

/// What `icmp cc (select c, ka, kb), k` reduced to.
enum SelFold {
    /// The compare is constant regardless of `c`.
    K(bool),
    /// The compare is `c`'s truth value, or its negation.
    C(Value, bool),
}

/// Lane constant: `iconst`, `splat(k)`, or a uniform `vconst` — the
/// per-lane value as u64 (vector bounds reach the compares as splats).
fn kof(func: &Function, v: Value) -> Option<u64> {
    let v = func.dfg.resolve_aliases(v);
    let d = func.dfg.value_def(v).inst()?;
    match func.dfg.insts[d] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits() as u64),
        InstructionData::Unary {
            opcode: Opcode::Splat,
            arg,
        } => kof(func, arg),
        InstructionData::UnaryConst {
            opcode: Opcode::Vconst,
            constant_handle,
        } => {
            let ty = func.dfg.value_type(func.dfg.first_result(d));
            let lb = (ty.lane_bits() / 8) as usize;
            if lb == 0 || lb > 8 {
                return None;
            }
            let d = func.dfg.constants.get(constant_handle).as_slice();
            if d.is_empty() || d.len() % lb != 0 || !d.chunks(lb).all(|c| c == &d[..lb]) {
                return None;
            }
            let mut k = 0u64;
            for (i, &b) in d[..lb].iter().enumerate() {
                k |= (b as u64) << (8 * i);
            }
            Some(k)
        }
        _ => None,
    }
}

/// What `a && b` reduced to when both are unsigned compares on `x`.
enum RgFold {
    /// `(x - lo) <u w` — one sub+compare; `lo`'s bound value is reused
    /// for the subtract. The `Option` is a `& 1`-per-lane mask a side
    /// carried — vector `icmp` lanes are all-ones and the merged
    /// compare must be re-normalized to 0/1.
    Ck(Value, Value, u64, Option<Value>),
    /// The range is empty or covers the whole type.
    K(bool),
}

/// `a && b` as the range test `x ∈ [lo, hi)` / `[lo, hi]` — the
/// two-compare predicate `Range*::contains`/`matches!` emit — rewritten
/// as `(x - lo) <u w`: one sub+icmp instead of two icmps and the merge
/// op, and one `pcmpgtd` group per lane under loopvec.
fn rgpair(func: &Function, a: Value, b: Value) -> Option<RgFold> {
    // `(x, cc, k, kv, m01)` for `icmp cc x, k` or `icmp cc k, x` —
    // `kv` the bound value itself (`iconst`/`splat`) so the subtract
    // can reuse the materialized splat instead of minting a second
    // one; `m01` a `& 1`-per-lane normalizer the merge must reapply.
    let cmp = |v: Value| -> Option<(Value, IntCC, u64, Value, Option<Value>)> {
        let mut v = func.dfg.resolve_aliases(v);
        // `band c, m` around an `icmp` is i1-normalization residue: a
        // scalar compare is already 0/1 so an odd mask is a no-op; a
        // vector compare's all-ones lanes need the `& splat(1)` kept
        // (and only the all-ones mask vanishes outright).
        let mut m01 = None;
        loop {
            let d = func.dfg.value_def(v).inst()?;
            let InstructionData::Binary {
                opcode: Opcode::Band,
                args,
            } = func.dfg.insts[d]
            else {
                break;
            };
            let mut next = None;
            for &a in args.iter() {
                let other = if a == args[0] { args[1] } else { args[0] };
                let Some(k) = kof(func, a) else { continue };
                let lb = func.dfg.value_type(other).lane_bits();
                let lm = if lb == 64 { u64::MAX } else { (1u64 << lb) - 1 };
                if !func.dfg.value_type(v).is_vector() {
                    if k & 1 == 1 {
                        next = Some(other);
                        break;
                    }
                } else if k & lm == lm {
                    next = Some(other);
                    break;
                } else if k == 1 {
                    m01 = Some(func.dfg.resolve_aliases(a));
                    next = Some(other);
                    break;
                }
            }
            v = func.dfg.resolve_aliases(next?);
        }
        let d = func.dfg.value_def(v).inst()?;
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args,
        } = func.dfg.insts[d]
        else {
            return None;
        };
        if let Some(k) = kof(func, args[1]) {
            Some((
                func.dfg.resolve_aliases(args[0]),
                cond,
                k,
                func.dfg.resolve_aliases(args[1]),
                m01,
            ))
        } else {
            Some((
                func.dfg.resolve_aliases(args[1]),
                cond.swap_args(),
                kof(func, args[0])?,
                func.dfg.resolve_aliases(args[0]),
                m01,
            ))
        }
    };
    let (x1, c1, k1, v1, m1) = cmp(a)?;
    let (x2, c2, k2, v2, m2) = cmp(b)?;
    if x1 != x2 {
        return None;
    }
    let ty = func.dfg.value_type(x1);
    if !ty.lane_type().is_int() || ty.lane_bits() > 64 {
        return None;
    }
    let lb = ty.lane_bits();
    let umax = if lb == 64 { u64::MAX } else { (1u64 << lb) - 1 };
    // One side must give the lo bound (`x >= k`), the other the
    // exclusive hi bound (`x < k`); extra same-side compares just
    // tighten their bound.
    let (mut lo, mut hi, mut lo_v) = (None::<u128>, None::<u128>, x1);
    for (cc, k, kv) in [(c1, k1 & umax, v1), (c2, k2 & umax, v2)] {
        match cc {
            IntCC::UnsignedGreaterThanOrEqual => {
                if lo.is_none_or(|s| k as u128 >= s) {
                    lo_v = kv;
                }
                lo = Some(lo.map_or(k as u128, |s| s.max(k as u128)))
            }
            IntCC::UnsignedGreaterThan => {
                if lo.is_none_or(|s| k as u128 + 1 >= s) {
                    lo_v = kv;
                }
                lo = Some(lo.map_or(k as u128 + 1, |s| s.max(k as u128 + 1)))
            }
            IntCC::UnsignedLessThan => hi = Some(hi.map_or(k as u128, |s| s.min(k as u128))),
            IntCC::UnsignedLessThanOrEqual => {
                hi = Some(hi.map_or(k as u128 + 1, |s| s.min(k as u128 + 1)))
            }
            _ => return None,
        }
    }
    let (lo, hi) = (lo?, hi?);
    if hi <= lo {
        return Some(RgFold::K(false));
    }
    if lo == 0 && hi > umax as u128 {
        return Some(RgFold::K(true));
    }
    Some(RgFold::Ck(x1, lo_v, (hi - lo) as u64, m1.or(m2)))
}

/// `band`/`select c1,c2,0` merges of unsigned range compares —
/// `ifconv2` mints the select form, which the main `clifpeep` run is
/// too early to see, so this also runs as its own step right after it
/// (before `loopvec`, which turns the single compare into one
/// `pcmpgtd` group per lane).
pub fn rgmerge(func: &mut Function) -> usize {
    let mut n = 0;
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            let f = match pos.func.dfg.insts[inst] {
                InstructionData::Binary {
                    opcode: Opcode::Band,
                    args,
                } => rgpair(pos.func, args[0], args[1]),
                InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } if iconst(pos.func, args[2]) == Some(0) => rgpair(pos.func, args[0], args[1]),
                // `bitselect m, t, 0` is `band m, t` — loopvec's
                // vectorized `select` arrives this way.
                InstructionData::Ternary {
                    opcode: Opcode::Bitselect,
                    args,
                } if kof(pos.func, args[2]) == Some(0) => {
                    if let Some(f) = rgpair(pos.func, args[0], args[1]) {
                        Some(f)
                    } else {
                        let (m, t) = (args[0], args[1]);
                        pos.func.replace(inst).band(m, t);
                        n += 1;
                        continue;
                    }
                }
                _ => None,
            };
            if let Some(f) = f {
                n += rgapply(&mut pos, inst, f);
            }
        }
    }
    n
}

/// A `ty`-typed materialized `k` — `iconst` scalar or a splat `vconst`
/// for vectors.
fn kvconst(pos: &mut FuncCursor, ty: Type, k: u64) -> Value {
    if !ty.is_vector() {
        return pos.ins().iconst(ty, k as i64);
    }
    let lb = (ty.lane_bits() / 8) as usize;
    let k = k & if lb == 8 {
        u64::MAX
    } else {
        (1u64 << (lb * 8)) - 1
    };
    let mut bytes = Vec::with_capacity(lb * ty.lane_count() as usize);
    for _ in 0..ty.lane_count() {
        bytes.extend_from_slice(&k.to_le_bytes()[..lb]);
    }
    let h = pos.func.dfg.constants.insert(bytes.into());
    pos.ins().vconst(ty, h)
}

/// Emit an `rgpair` fold in place of `inst`. Scalar `select`s/`band`s
/// of `icmp`s yield `i8` like the replacement compare; vector bands
/// keep the all-ones lane mask.
fn rgapply(pos: &mut FuncCursor, inst: Inst, f: RgFold) -> usize {
    match f {
        RgFold::K(b) => {
            let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
            if ty.is_vector() {
                let n = (ty.lane_bits() / 8) as usize * ty.lane_count() as usize;
                let h = pos
                    .func
                    .dfg
                    .constants
                    .insert(vec![if b { 0xff } else { 0 }; n].into());
                pos.func.replace(inst).vconst(ty, h);
            } else {
                pos.func.replace(inst).iconst(ty, b as i64);
            }
        }
        RgFold::Ck(x, lo_v, w, m01) => {
            let ty = pos.func.dfg.value_type(x);
            // `lo_v` is the compare's own bound operand — the splat or
            // `iconst` as-materialized, so `x - lo` costs one inst.
            let x = if kof(pos.func, lo_v) == Some(0) {
                x
            } else {
                pos.ins().isub(x, lo_v)
            };
            let w = kvconst(pos, ty, w);
            match m01 {
                Some(m) => {
                    let c = pos.ins().icmp(IntCC::UnsignedLessThan, x, w);
                    pos.func.replace(inst).band(c, m);
                }
                None => {
                    pos.func.replace(inst).icmp(IntCC::UnsignedLessThan, x, w);
                }
            }
        }
    }
    1
}

/// `icmp cc (select c, ka, kb), k` with `ka`/`kb` constants: evaluate
/// `ka cc k` and `kb cc k` at the compare width — equal results make the
/// compare a flag constant, a split makes it `c` (`K(false)`) or `!c`
/// (`K(true)`). One `uextend`/`ireduce` around the `select` is just arm
/// masking to the compare width, so it looks through a single wrapper.
fn icmp_selfold(func: &Function, x: Value, cc: IntCC, k: u64, w: u32) -> Option<SelFold> {
    let mut v = func.dfg.resolve_aliases(x);
    if let Some(d) = func.dfg.value_def(v).inst()
        && let InstructionData::Unary {
            opcode: Opcode::Uextend | Opcode::Ireduce,
            arg,
        } = func.dfg.insts[d]
    {
        v = func.dfg.resolve_aliases(arg);
    }
    let d = func.dfg.value_def(v).inst()?;
    let InstructionData::Ternary {
        opcode: Opcode::Select,
        args: [c, a, b],
    } = func.dfg.insts[d]
    else {
        return None;
    };
    let (Some(ka), Some(kb)) = (iconst(func, a), iconst(func, b)) else {
        return None;
    };
    if w > 64 {
        return None;
    }
    // Arm values at the compare width: `ireduce` truncates them to `w`
    // bits, `uextend` zero-pads from the select's own (narrower) width —
    // masking to `min(aw, w)` covers both.
    let aw = func.dfg.value_type(func.dfg.first_result(d)).bits();
    let am = if aw.min(w) == 64 {
        u64::MAX
    } else {
        (1u64 << aw.min(w)) - 1
    };
    let m = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
    let s = |v: i64| (v as u64) & am;
    let (sa, sb, sk) = (s(ka), s(kb), k & m);
    let sx = |a: u64| {
        let (x, y) = (sext(a, w), sext(sk, w));
        match cc {
            IntCC::Equal => a == sk,
            IntCC::NotEqual => a != sk,
            IntCC::SignedLessThan => x < y,
            IntCC::SignedGreaterThanOrEqual => x >= y,
            IntCC::SignedGreaterThan => x > y,
            IntCC::SignedLessThanOrEqual => x <= y,
            IntCC::UnsignedLessThan => a < sk,
            IntCC::UnsignedGreaterThanOrEqual => a >= sk,
            IntCC::UnsignedGreaterThan => a > sk,
            IntCC::UnsignedLessThanOrEqual => a <= sk,
        }
    };
    let c = func.dfg.resolve_aliases(c);
    match (sx(sa), sx(sb)) {
        (t, f) if t == f => Some(SelFold::K(t)),
        (true, false) => Some(SelFold::C(c, false)),
        (false, true) => Some(SelFold::C(c, true)),
        _ => unreachable!(),
    }
}

/// Guarded bit-table probe: `icmp ne (band (ushr s, c), 1), 0` where
/// `s = select(icmp ult c, N), K, 0` — rustc's unicode tables emit this
/// (`char::is_whitespace` picks `K` when `c < 64` so the shift can't
/// wrap). The probe is nonzero only when bit `c` of `K` is set, which
/// requires `c <= msb(K)` anyway — so the `select` collapses into a
/// tightened bound: `(c <=u msb(K)) && (K >> c) & 1`. Drops the `cmov`
/// `select` and lets `K` ride a register/`bt` index directly — LLVM's
/// `cmp c,msb+1; ja; bt` shape.
fn bittab(pos: &mut FuncCursor, inst: Inst, args: [Value; 2]) -> bool {
    // `icmp ne x, 0` (const on either side).
    let x = if iconst(pos.func, args[1]) == Some(0) {
        args[0]
    } else if iconst(pos.func, args[0]) == Some(0) {
        args[1]
    } else {
        return false;
    };
    // `x` must be `(ushr s, e) & 1`.
    let Some((u, 1)) = bin_k(pos.func, x, Opcode::Band) else {
        return false;
    };
    let u = pos.func.dfg.resolve_aliases(u);
    let Some(ui) = pos.func.dfg.value_def(u).inst() else {
        return false;
    };
    let InstructionData::Binary {
        opcode: Opcode::Ushr,
        args: ua,
    } = pos.func.dfg.insts[ui]
    else {
        return false;
    };
    // `s` must be `select g, K, 0`.
    let s = pos.func.dfg.resolve_aliases(ua[0]);
    let Some(si) = pos.func.dfg.value_def(s).inst() else {
        return false;
    };
    let InstructionData::Ternary {
        opcode: Opcode::Select,
        args: sa,
    } = pos.func.dfg.insts[si]
    else {
        return false;
    };
    let Some(kv) = iconst(pos.func, sa[1]) else {
        return false;
    };
    if kv == 0 || iconst(pos.func, sa[2]) != Some(0) {
        return false;
    }
    // `g` must be `icmp ult c, N` (either operand order) with `msb(K) < N`.
    let g = pos.func.dfg.resolve_aliases(sa[0]);
    let Some(gi) = pos.func.dfg.value_def(g).inst() else {
        return false;
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond: gcc,
        args: ga,
    } = pos.func.dfg.insts[gi]
    else {
        return false;
    };
    let (c, n, gty) = if iconst(pos.func, ga[1]).is_some() {
        (
            pos.func.dfg.resolve_aliases(ga[0]),
            iconst(pos.func, ga[1]).unwrap(),
            gcc,
        )
    } else if iconst(pos.func, ga[0]).is_some() {
        (
            pos.func.dfg.resolve_aliases(ga[1]),
            iconst(pos.func, ga[0]).unwrap(),
            gcc.swap_args(),
        )
    } else {
        return false;
    };
    if gty != IntCC::UnsignedLessThan {
        return false;
    }
    let gbits = pos.func.dfg.value_type(c).bits();
    if gbits > 64 || n <= 0 || (gbits < 64 && n as u64 >= 1u64 << gbits) {
        return false;
    }
    // `n` past the mask's shift width would let `c >= width` reach the
    // `ushr`, which wraps its index mod width — the tightened
    // `c <= msb` guard must cut every such `c`, so `n <= width`.
    let sbits = pos.func.dfg.value_type(s).bits();
    if n > sbits as i64 {
        return false;
    }
    let msb = 63 - (kv as u64).leading_zeros() as i64;
    if msb < 0 || msb >= n {
        return false;
    }
    // The shift index must be `c` itself or `uextend c`.
    let e = pos.func.dfg.resolve_aliases(ua[1]);
    let e_ok = e == c
        || pos
            .func
            .dfg
            .value_def(e)
            .inst()
            .is_some_and(|d| match pos.func.dfg.insts[d] {
                InstructionData::Unary {
                    opcode: Opcode::Uextend,
                    arg,
                } => pos.func.dfg.resolve_aliases(arg) == c,
                _ => false,
            });
    if !e_ok {
        return false;
    }
    // `c <=u msb(K) && (K >> e) & 1 != 0`.
    let cty = pos.func.dfg.value_type(c);
    let lim = pos.ins().iconst(cty, msb);
    let g2 = pos.ins().icmp(IntCC::UnsignedLessThanOrEqual, c, lim);
    let sty = pos.func.dfg.value_type(s);
    let sh = pos.ins().ushr(sa[1], e);
    let one = pos.ins().iconst(sty, 1);
    let m = pos.ins().band(sh, one);
    let z = pos.ins().iconst(sty, 0);
    let b = pos.ins().icmp(IntCC::NotEqual, m, z);
    pos.func.replace(inst).band(g2, b);
    true
}

/// `x >=u 1`, `x >u 0`, `x <=u 0`, `x <u 1` are equality tests;
/// `icmp eq/ne (x + c1), c2` → `icmp eq/ne x, c2 - c1` (wrapping add is a
/// bijection, so equality survives the shift). Together they fold peeled
/// countdowns `x - k >=u 1` to a single `cmp x, k`.
fn icmp_eqfold(
    pos: &mut FuncCursor,
    inst: Inst,
    cond: IntCC,
    args: [Value; 2],
    ty: Type,
    uses: &mut Option<FxHashMap<Value, usize>>,
) -> bool {
    if !ty.is_int() || ty.is_vector() || ty.bits() > 64 {
        return false;
    }
    let ck = iconst(pos.func, args[1])
        .map(|k| (cond, args[0], k))
        .or_else(|| iconst(pos.func, args[0]).map(|k| (cond.swap_args(), args[1], k)));
    let eqcc = match ck {
        Some((IntCC::UnsignedGreaterThanOrEqual, _, 1))
        | Some((IntCC::UnsignedGreaterThan, _, 0)) => Some(IntCC::NotEqual),
        Some((IntCC::UnsignedLessThanOrEqual, _, 0)) | Some((IntCC::UnsignedLessThan, _, 1)) => {
            Some(IntCC::Equal)
        }
        _ => None,
    };
    if let Some(ncc) = eqcc {
        let x0 = ck.unwrap().1;
        let z = pos.ins().iconst(ty, 0);
        pos.func.replace(inst).icmp(ncc, x0, z);
        return true;
    }
    let Some((cc0, x0, k0)) = ck else {
        return false;
    };
    // Sound for eq/ne at any width: `(x + c1) == k` and `x == k - c1` are
    // the same test mod 2^bits. (Narrow widths were once gated off after a
    // strwork miscompile that was never traced to this fold; if it
    // resurfaces the downstream bug is still there to fix.)
    if !matches!(cc0, IntCC::Equal | IntCC::NotEqual) {
        return false;
    }
    // `isub` must be `x - c` — `c - x` doesn't shift.
    let issub = |v: Value| {
        let i = pos.func.dfg.resolved_value_def(v).inst()?;
        let InstructionData::Binary {
            opcode: Opcode::Isub,
            args: aa,
        } = pos.func.dfg.insts[i]
        else {
            return None;
        };
        iconst(pos.func, aa[1]).map(|c1| (aa[0], c1))
    };
    let adj = bin_k(pos.func, x0, Opcode::Iadd)
        .map(|(v, c1)| (v, k0.wrapping_sub(c1)))
        .or_else(|| issub(x0).map(|(v, c1)| (v, k0.wrapping_add(c1))));
    let Some((v, kk)) = adj else { return false };
    // Only shift when `x0` is dead apart from this compare — then the
    // add/sub folds away entirely. When it's still needed (e.g. a loop's
    // latch `iv - c` passed back as the next IV), `icmp x0, 0` rides that
    // def for free and keeps the countdown-mergeable `x - c != 0` shape;
    // shifting to `icmp x, c` forced `lea`+`mov`+`cmp`+`jcc` on x64.
    if uses
        .get_or_insert_with(|| use_counts(pos.func))
        .get(&pos.func.dfg.resolve_aliases(x0))
        .copied()
        .unwrap_or(0)
        > 1
    {
        return false;
    }
    let c = pos.ins().iconst(ty, kk);
    pos.func.replace(inst).icmp(cc0, v, c);
    true
}

/// `isub x, k` → `iadd x, -k`: x64 lowers scalar iadd as `lea`, which
/// writes any reg without destroying the input — `sub`'s operand
/// destruction forces a `mov` when the input stays live (e.g. `n-1` as a
/// call arg next to loop-carried `n-2`). Runs late so the earlier loop
/// passes keep seeing canonical `isub` IV updates — which also makes it
/// the first icmp fold after punroll's peeled countdowns.
pub fn sublea(func: &mut Function) -> usize {
    let mut n = 0;
    let mut uses = None;
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            // Post-punroll icmp normalization: the peeled countdowns and
            // shifted compares only exist once punroll has run.
            if crate::pass_enabled("PLIRON_ICMPEQFOLD")
                && let InstructionData::IntCompare {
                    opcode: Opcode::Icmp,
                    cond,
                    args,
                } = pos.func.dfg.insts[inst]
            {
                let ty = pos.func.dfg.value_type(args[0]);
                if icmp_eqfold(&mut pos, inst, cond, args, ty, &mut uses) {
                    n += 1;
                }
                continue;
            }
            let InstructionData::Binary {
                opcode: Opcode::Isub,
                args,
            } = pos.func.dfg.insts[inst]
            else {
                continue;
            };
            let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
            if !ty.is_int()
                || ty.is_vector()
                || ty.bits() > 64
                || iconst(pos.func, args[1]).is_none()
            {
                continue;
            }
            let k = iconst(pos.func, args[1]).unwrap();
            let nk = pos.ins().iconst(ty, k.wrapping_neg());
            pos.func.replace(inst).iadd(args[0], nk);
            n += 1;
        }
    }
    // Bound-propagated dead-check elimination — sees punroll's peeled
    // `acc*10+d` chains, which is where it pays.
    if crate::pass_enabled("PLIRON_OVKILL") {
        let k = ovkill(func);
        if k > 0
            && pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
        {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::stats", "ovkill {k} {}", func.name)
            };
        }
        n += k;
    }
    n
}

/// `umin`/`umax`/`smin`/`smax` on scalar ints wider than a register (i128):
/// no Cranelift target lowers them — the egraph's `select(icmp)->min` rules
/// don't restrict by width, and loopvec's scalar reduction epilogue emits
/// `umin` for `u128` accumulators — so split them back into `icmp` + `select`
/// (which do lower on i128: cmp/sbcs + a pair of csel).
pub fn wide_minmax(func: &mut Function) -> usize {
    let mut n = 0;
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            let InstructionData::Binary { opcode, args } = pos.func.dfg.insts[inst] else {
                continue;
            };
            let cc = match opcode {
                Opcode::Umin => IntCC::UnsignedLessThan,
                Opcode::Umax => IntCC::UnsignedGreaterThan,
                Opcode::Smin => IntCC::SignedLessThan,
                Opcode::Smax => IntCC::SignedGreaterThan,
                _ => continue,
            };
            let ty = pos.func.dfg.value_type(pos.func.dfg.first_result(inst));
            if ty.is_vector() || !ty.is_int() || ty.bits() <= 64 {
                continue;
            }
            let [x, y] = args;
            let c = pos.ins().icmp(cc, x, y);
            pos.func.replace(inst).select(c, x, y);
            n += 1;
        }
    }
    n
}

/// `iconst.i128` is legal CLIF but no target lowers it (the aarch64 ISLE
/// `imm` helper has no I128 rule, so it panics in isel). Any pass that
/// materializes a scalar constant through `iconst(ty, …)` can mint one —
/// sweep the whole function last rather than audit every emitter. The
/// Imm64 sign-extends, so the high half is `k >> 63`.
pub fn iconst128(func: &mut Function) -> usize {
    let mut n = 0;
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            let InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } = pos.func.dfg.insts[inst]
            else {
                continue;
            };
            if pos.func.dfg.value_type(pos.func.dfg.first_result(inst)) != types::I128 {
                continue;
            }
            let k = imm.bits();
            let lo = pos.ins().iconst(types::I64, k);
            let hi = pos.ins().iconst(types::I64, k >> 63);
            pos.func.replace(inst).iconcat(lo, hi);
            n += 1;
        }
    }
    n
}

/// Loads within the dereferenceable bytes of a frozen param (rustc `noalias
/// readonly` `&T`: immutable and live for the whole call) become `readonly
/// can_move`, which Cranelift's egraph treats as pure: it merges repeats and
/// hoists them out of loops, as LLVM's LICM/GVN do with the same facts.
pub fn frozen_loads(func: &mut Function, frozen: &rustc_hash::FxHashMap<Value, u64>) -> usize {
    let mut n = 0;
    let mut hoist = Vec::new();
    let insts: Vec<Inst> = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .collect();
    for i in insts {
        let InstructionData::Load {
            opcode,
            arg,
            flags,
            offset,
        } = func.dfg.insts[i]
        else {
            continue;
        };
        let bytes = match opcode {
            Opcode::Load => func.dfg.value_type(func.dfg.first_result(i)).bytes(),
            Opcode::Uload8 | Opcode::Sload8 => 1,
            Opcode::Uload16 | Opcode::Sload16 => 2,
            Opcode::Uload32 | Opcode::Sload32 => 4,
            _ => continue,
        };
        let a = func.dfg.resolve_aliases(arg);
        let (base, k) = if frozen.contains_key(&a) {
            (a, 0)
        } else {
            match bin_k(func, a, Opcode::Iadd) {
                Some(xk) => xk,
                None => continue,
            }
        };
        let Some(&size) = frozen.get(&base) else {
            continue;
        };
        let lo = k + i64::from(offset);
        if lo < 0 || lo as u64 + u64::from(bytes) > size {
            continue;
        }
        let d = func.dfg.mem_flags[flags];
        if !d.notrap() || (d.readonly() && d.can_move()) {
            continue;
        }
        let nf = func
            .dfg
            .mem_flags
            .insert_unchecked(d.with_readonly().with_can_move());
        if let InstructionData::Load { flags, .. } = &mut func.dfg.insts[i] {
            *flags = nf;
        }
        n += 1;
        if let Ok(off) = i32::try_from(lo) {
            // Without `can_move` the egraph keeps the load where we put it
            // instead of sinking it back to its (in-loop) uses.
            let hf = func.dfg.mem_flags.insert_unchecked(d.with_readonly());
            hoist.push((i, opcode, base, off, hf));
        }
    }
    // Opt-in: on hm.rs it pins the key's fields in callee-saved registers
    // across the probe loop and costs +7% instructions.
    if !hoist.is_empty() && crate::env_is("PLIRON_FROZEN_HOIST", "1") {
        hoist_frozen(func, hoist);
    }
    n
}

/// Frozen memory is dereferenceable and immutable for the whole call, so a
/// load of it inside a loop can be done once in the entry block. Cranelift's
/// own LICM skips these when they are `uload8/16` (not "pure" for its egraph).
fn hoist_frozen(
    func: &mut Function,
    hoist: Vec<(Inst, Opcode, Value, i32, cranelift_codegen::ir::MemFlags)>,
) {
    use cranelift_codegen::dominator_tree::DominatorTree;
    use cranelift_codegen::flowgraph::ControlFlowGraph;
    use cranelift_codegen::loop_analysis::LoopAnalysis;
    let Some(entry) = func.layout.entry_block() else {
        return;
    };
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(func, &cfg, &dt);
    let mut done: rustc_hash::FxHashMap<(Opcode, cranelift_codegen::ir::Type, Value, i32), Value> =
        Default::default();
    for (i, opcode, base, off, flags) in hoist {
        let Some(b) = func.layout.inst_block(i) else {
            continue;
        };
        if la.innermost_loop(b).is_none() || !dt.is_reachable(b) {
            continue;
        }
        let r = func.dfg.first_result(i);
        let ty = func.dfg.value_type(r);
        let nv = *done.entry((opcode, ty, base, off)).or_insert_with(|| {
            let mut pos = FuncCursor::new(func).at_first_insertion_point(entry);
            let (ni, dfg) = pos.ins().Load(opcode, ty, flags, off.into(), base);
            dfg.first_result(ni)
        });
        func.layout.remove_inst(i);
        func.dfg.clear_results(i);
        func.dfg.change_to_alias(r, nv);
    }
}

/// Shared value-numbering key for `lcse`/`dgvn`: duplicate pure insts
/// collapse to the earliest value computing them, so redundant chains —
/// the vectorizer's separate c-load/c-store address streams, rustc's
/// repeated index math — become one block arg where `sameargs` can merge
/// the params. Keyed on (opcode, result type, resolved args, imm/cond).
#[derive(Clone, PartialEq, Eq, Hash)]
enum CseKey {
    U(Opcode, Type, Value),
    UI(Opcode, Type, i64),
    F32(Type, u32),
    F64(Type, u64),
    B(Opcode, Type, Value, Value),
    B8(Opcode, Type, Value, u8),
    IC(IntCC, Type, Value, Value),
    FC(FloatCC, Type, Value, Value),
    T(Opcode, Type, Value, Value, Value),
}

// Commutative integer ops get sorted args; fadd/fmul are bitwise
// commutative too (same NaN semantics both ways).
fn cse_comm(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::Iadd
            | Opcode::Imul
            | Opcode::Band
            | Opcode::Bor
            | Opcode::Bxor
            | Opcode::Umin
            | Opcode::Umax
            | Opcode::Smin
            | Opcode::Smax
            | Opcode::Fadd
            | Opcode::Fmul
    )
}

fn cse_key(func: &Function, i: Inst) -> Option<CseKey> {
    let r = |v: Value| func.dfg.resolve_aliases(v);
    let t = func.dfg.value_type(func.dfg.first_result(i));
    match func.dfg.insts[i] {
        InstructionData::Unary { opcode, arg } => Some(CseKey::U(opcode, t, r(arg))),
        InstructionData::UnaryImm { opcode, imm } => Some(CseKey::UI(opcode, t, imm.bits())),
        InstructionData::UnaryIeee32 { opcode: _, imm } => Some(CseKey::F32(t, imm.bits())),
        InstructionData::UnaryIeee64 { opcode: _, imm } => Some(CseKey::F64(t, imm.bits())),
        InstructionData::Binary { opcode, args } => {
            let (mut a, mut b) = (r(args[0]), r(args[1]));
            if cse_comm(opcode) && a > b {
                std::mem::swap(&mut a, &mut b);
            }
            Some(CseKey::B(opcode, t, a, b))
        }
        InstructionData::BinaryImm8 { opcode, arg, imm } => {
            Some(CseKey::B8(opcode, t, r(arg), imm))
        }
        InstructionData::IntCompare {
            opcode: _,
            cond: mut cc,
            args,
        } => {
            let (mut a, mut b) = (r(args[0]), r(args[1]));
            if a > b {
                std::mem::swap(&mut a, &mut b);
                cc = cc.swap_args();
            }
            Some(CseKey::IC(cc, t, a, b))
        }
        InstructionData::FloatCompare {
            opcode: _,
            cond: mut cc,
            args,
        } => {
            let (mut a, mut b) = (r(args[0]), r(args[1]));
            if a > b {
                std::mem::swap(&mut a, &mut b);
                cc = cc.swap_args();
            }
            Some(CseKey::FC(cc, t, a, b))
        }
        InstructionData::Ternary { opcode, args } => {
            Some(CseKey::T(opcode, t, r(args[0]), r(args[1]), r(args[2])))
        }
        _ => None,
    }
}

/// Per-block CSE with the shared keying: later insts in a block alias
/// the earliest equivalent def, which dominates them.
pub fn lcse(func: &mut Function) -> usize {
    let key = cse_key;
    type Seen = FxHashMap<CseKey, Value>;
    // `uses`/`dec`/`inc` maintain an exact use tally (`usecnt` below) —
    // `uses_of`'s whole-function scan per query is quadratic here, where
    // the memoff/neg guards run it per candidate.
    let uses = |func: &Function, cnt: &FxHashMap<Value, u32>, v: Value| -> u32 {
        cnt.get(&func.dfg.resolve_aliases(v)).copied().unwrap_or(0)
    };
    let dec = |func: &Function, cnt: &mut FxHashMap<Value, u32>, v: Value| {
        if let Some(c) = cnt.get_mut(&func.dfg.resolve_aliases(v)) {
            *c = c.saturating_sub(1);
        }
    };
    let inc = |func: &Function, cnt: &mut FxHashMap<Value, u32>, v: Value| {
        *cnt.entry(func.dfg.resolve_aliases(v)).or_default() += 1;
    };
    // In-place arg mutation (memoff reassociation, `neg`'s imul peel)
    // changes what `d`'s result computes, but `seen` may already map its old
    // key to it — a later identical inst would then alias to a value that no
    // longer matches. Drop the stale entry before mutating.
    let untrack = |func: &Function, seen: &mut Seen, d: Inst| {
        let Some(k) = key(func, d) else { return };
        let res = func.dfg.first_result(d);
        if seen.get(&k) == Some(&res) {
            seen.remove(&k);
        }
    };
    // Def block of a value: the inst's block for results, the param's own
    // block for block params, `None` for unions.
    let def_block = |func: &Function, v: Value| -> Option<Block> {
        match func.dfg.value_def(v) {
            ValueDef::Result(d, _) => func.layout.inst_block(d),
            ValueDef::Param(p, _) => Some(p),
            ValueDef::Union(_, _) => None,
        }
    };
    // Loop structure for the ident-alias guard at the fold site below.
    let la = {
        use cranelift_codegen::dominator_tree::DominatorTree;
        use cranelift_codegen::flowgraph::ControlFlowGraph;
        use cranelift_codegen::loop_analysis::LoopAnalysis;
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        let mut la = LoopAnalysis::new();
        la.compute(func, &cfg, &dt);
        la
    };
    // `x*1` / `x+0` / `x|0` / `x^0` / `x&-1` / `x<<0` / `x>>0` -> `x`: these
    // survive GVN when rustc's `index*elemsize` idiom degenerates to *1.
    let ident = |func: &Function, data: InstructionData, res_ty: Type| -> Option<Value> {
        // `uextend (ireduce v)` == `v` when `v` provably fits the reduced
        // width (`clz`/`ctz`/`popcnt` counts, masked values) — kills the
        // `mov wN, wN` isel emits to re-zero the top half.
        if let InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } = data
        {
            let a = func.dfg.resolve_aliases(arg);
            if !crate::pass_enabled("PLIRON_EXTRED") {
                return None;
            }
            if let Some(d) = func.dfg.value_def(a).inst()
                && let InstructionData::Unary {
                    opcode: Opcode::Ireduce,
                    arg: x,
                } = func.dfg.insts[d]
            {
                let x = func.dfg.resolve_aliases(x);
                let w = func.dfg.value_type(func.dfg.first_result(d)).bits();
                if func.dfg.value_type(x) == res_ty && w < 64 && (vmax(func, x) >> w) == 0 {
                    return Some(x);
                }
            }
            return None;
        }
        // `select c, x, x` is `x` — a decided discriminant select left
        // behind by spec folds.
        if let InstructionData::Ternary {
            opcode: Opcode::Select,
            args,
        } = data
        {
            let (t, f) = (
                func.dfg.resolve_aliases(args[1]),
                func.dfg.resolve_aliases(args[2]),
            );
            if t == f {
                return Some(t);
            }
            return None;
        }
        let InstructionData::Binary { opcode, args } = data else {
            return None;
        };
        let [a, b] = args.map(|v| func.dfg.resolve_aliases(v));
        let (ka, kb) = (iconst(func, a), iconst(func, b));
        match opcode {
            Opcode::Imul => {
                if ka == Some(1) {
                    Some(b)
                } else {
                    (kb == Some(1)).then_some(a)
                }
            }
            Opcode::Iadd | Opcode::Bor | Opcode::Bxor => {
                if ka == Some(0) {
                    Some(b)
                } else {
                    (kb == Some(0)).then_some(a)
                }
            }
            Opcode::Band => {
                if ka == Some(-1) {
                    Some(b)
                } else {
                    (kb == Some(-1)).then_some(a)
                }
            }
            Opcode::Ishl | Opcode::Ushr | Opcode::Sshr => (kb == Some(0)).then_some(a),
            Opcode::Isub => {
                // `isub (iadd x, y), x` → `y` — cursor deltas like
                // `(cur + len) - cur` collapse to the byte length. Only
                // the single-step case produces an alias; constant chains
                // (`+2`/`+3`/`+4` decode paths) fold in the main loop.
                let d = func.dfg.value_def(a).inst()?;
                let InstructionData::Binary {
                    opcode: Opcode::Iadd,
                    args: [x, y],
                } = func.dfg.insts[d]
                else {
                    return None;
                };
                let (x, y) = (func.dfg.resolve_aliases(x), func.dfg.resolve_aliases(y));
                if b == x {
                    Some(y)
                } else {
                    (b == y).then_some(x)
                }
            }
            _ => None,
        }
    };
    // `load/store (iadd base, K)` → `load/store base, off+K`: the
    // constant rides the memory operand's offset field instead of
    // costing an in-loop add (`hashbrown` slot addressing). Nested
    // `(a + k) + x` chains: rewrite the outer add's args to
    // `(a, x)` — in-place, so its operands still dominate it — and
    // carry `k` into the offset too (outer add must be single-use).
    let memoff = |func: &mut Function,
                  v: &mut Value,
                  off: &mut i64,
                  seen: &mut Seen,
                  cnt: &mut FxHashMap<Value, u32>|
     -> usize {
        if !crate::pass_enabled("PLIRON_MEMOFF") {
            return 0;
        }
        let mut depth = 0;
        loop {
            // A single-use `iadd` chain costs O(len) per inst — O(N²) on
            // encoder-style fns (stage2's `FileEncoder` spent ~75s here).
            // Chains this deep are rare; cap the peel.
            if depth >= 64 {
                break;
            }
            let a = func.dfg.resolve_aliases(*v);
            if let Some((b, k)) = bin_k(func, a, Opcode::Iadd) {
                match off.checked_add(k).and_then(|x| i32::try_from(x).ok()) {
                    Some(no) => {
                        *off = i64::from(no);
                        *v = b;
                        depth += 1;
                        continue;
                    }
                    None => break,
                }
            }
            let Some(d) = func.dfg.value_def(a).inst() else {
                break;
            };
            let InstructionData::Binary {
                opcode: Opcode::Iadd,
                args,
            } = func.dfg.insts[d]
            else {
                break;
            };
            if !crate::pass_enabled("PLIRON_MEMOFF_NEST") {
                break;
            }
            let mut inner = None;
            for (ix, &c) in args.iter().enumerate() {
                if let Some((b, k)) = bin_k(func, func.dfg.resolve_aliases(c), Opcode::Iadd) {
                    inner = Some((ix, b, k));
                    break;
                }
            }
            let Some((ix, b, k)) = inner else {
                break;
            };
            if uses(func, cnt, a) != 1 {
                break;
            }
            let Some(no) = off.checked_add(k).and_then(|x| i32::try_from(x).ok()) else {
                break;
            };
            *off = i64::from(no);
            let other = args[1 - ix];
            untrack(func, seen, d);
            func.dfg.insts[d] = InstructionData::Binary {
                opcode: Opcode::Iadd,
                args: [b, other],
            };
            dec(func, cnt, args[ix]);
            inc(func, cnt, b);
            depth += 1;
        }
        depth
    };
    let mut n = 0;
    let mut seen: Seen = FxHashMap::default();
    // A `uses_of` query scans the whole function — quadratic when the
    // memoff/neg guards run it per candidate (stage2 hit a function where
    // lcse dominated for hours). Count every use once, then keep the
    // tally exact at each in-place arg swap and alias fold below.
    let mut usecnt: FxHashMap<Value, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for &a in func.dfg.inst_args(i) {
                *usecnt.entry(func.dfg.resolve_aliases(a)).or_default() += 1;
            }
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for a in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(x) = a {
                        *usecnt.entry(func.dfg.resolve_aliases(x)).or_default() += 1;
                    }
                }
            }
        }
    }
    let mut compact_at = 1024;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        seen.clear();
        let mut dead = Vec::new();
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let data = func.dfg.insts[i];
            match data {
                InstructionData::Load { arg, offset, .. } => {
                    let mut a = arg;
                    let mut off = i64::from(offset);
                    if memoff(func, &mut a, &mut off, &mut seen, &mut usecnt) > 0 {
                        if let InstructionData::Load { arg, offset, .. } = &mut func.dfg.insts[i] {
                            *arg = a;
                            *offset = (off as i32).into();
                        }
                        dec(func, &mut usecnt, arg);
                        inc(func, &mut usecnt, a);
                        n += 1;
                        continue;
                    }
                }
                InstructionData::Store { args, offset, .. } => {
                    let mut a = args[1];
                    let mut off = i64::from(offset);
                    if memoff(func, &mut a, &mut off, &mut seen, &mut usecnt) > 0 {
                        if let InstructionData::Store { args, offset, .. } = &mut func.dfg.insts[i]
                        {
                            args[1] = a;
                            *offset = (off as i32).into();
                        }
                        dec(func, &mut usecnt, args[1]);
                        inc(func, &mut usecnt, a);
                        n += 1;
                        continue;
                    }
                }
                InstructionData::Binary {
                    opcode: Opcode::Iadd,
                    args,
                } if crate::pass_enabled("PLIRON_NEGFOLD") => {
                    let ra = func.dfg.resolve_aliases(args[0]);
                    let rb = func.dfg.resolve_aliases(args[1]);
                    // `iadd (ineg a), b` → `isub b, a` — isel turns the
                    // subtract into `sub b, a, lsl #s` when `a` is a shift.
                    // `isub 0, a` is the same negation (that's what
                    // `isub 0, x` lowers from). `(-a) * k` is `-(a * k)`, so
                    // a single-use `imul (isub 0, a), k` operand lets the
                    // neg ride through to `isub b, (a * k)` — an `msub`.
                    let negdef = |func: &Function, v: Value| {
                        let i = func.dfg.resolved_value_def(v).inst()?;
                        match func.dfg.insts[i] {
                            InstructionData::Unary {
                                opcode: Opcode::Ineg,
                                arg,
                            } => Some(func.dfg.resolve_aliases(arg)),
                            InstructionData::Binary {
                                opcode: Opcode::Isub,
                                args: [z, a],
                            } if iconst(func, func.dfg.resolve_aliases(z)) == Some(0) => {
                                Some(func.dfg.resolve_aliases(a))
                            }
                            _ => None,
                        }
                    };
                    let mut neg = |v: Value, seen: &mut Seen, cnt: &mut FxHashMap<Value, u32>| {
                        if let Some(x) = negdef(func, v) {
                            return Some(x);
                        }
                        // Peel a single-use `imul (neg a), y` — rewriting
                        // the mul to `a * y` changes its value, so no other
                        // use may observe it.
                        let mi = func.dfg.resolved_value_def(v).inst()?;
                        let InstructionData::Binary {
                            opcode: Opcode::Imul,
                            args: margs,
                        } = func.dfg.insts[mi]
                        else {
                            return None;
                        };
                        let (ix, inner) = margs
                            .iter()
                            .enumerate()
                            .find_map(|(ix, &m)| negdef(func, m).map(|x| (ix, x)))?;
                        if uses(func, cnt, func.dfg.first_result(mi)) != 1 {
                            return None;
                        }
                        untrack(func, seen, mi);
                        if let InstructionData::Binary { args, .. } = &mut func.dfg.insts[mi] {
                            args[ix] = inner;
                        }
                        dec(func, cnt, margs[ix]);
                        inc(func, cnt, inner);
                        Some(func.dfg.first_result(mi))
                    };
                    let sub = if let Some(x) = neg(ra, &mut seen, &mut usecnt) {
                        Some([rb, x])
                    } else {
                        neg(rb, &mut seen, &mut usecnt).map(|x| [ra, x])
                    };
                    if let Some(newargs) = sub {
                        func.dfg.insts[i] = InstructionData::Binary {
                            opcode: Opcode::Isub,
                            args: newargs,
                        };
                        dec(func, &mut usecnt, args[0]);
                        dec(func, &mut usecnt, args[1]);
                        inc(func, &mut usecnt, newargs[0]);
                        inc(func, &mut usecnt, newargs[1]);
                        n += 1;
                        continue;
                    }
                }
                InstructionData::Binary {
                    opcode: Opcode::Isub,
                    args,
                } => {
                    // `isub (iadd ..(iadd b, c₁)..cₙ), b` → `iconst Σcᵢ`:
                    // cursor deltas like `(cur + len) - cur` collapse to
                    // the byte length once late passes substitute edge
                    // args and expose the iadd chain (`x - x` folds too,
                    // as the k=0 case).
                    let b = func.dfg.resolve_aliases(args[1]);
                    let mut v = func.dfg.resolve_aliases(args[0]);
                    let mut k: i64 = 0;
                    let mut folded = false;
                    for _ in 0..8 {
                        if v == b {
                            func.dfg.insts[i] = InstructionData::UnaryImm {
                                opcode: Opcode::Iconst,
                                imm: k.into(),
                            };
                            dec(func, &mut usecnt, args[0]);
                            dec(func, &mut usecnt, args[1]);
                            folded = true;
                            break;
                        }
                        let Some(d) = func.dfg.value_def(v).inst() else {
                            break;
                        };
                        let InstructionData::Binary {
                            opcode: Opcode::Iadd,
                            args: ia,
                        } = func.dfg.insts[d]
                        else {
                            break;
                        };
                        let (x, y) = (
                            func.dfg.resolve_aliases(ia[0]),
                            func.dfg.resolve_aliases(ia[1]),
                        );
                        if let Some(c) = iconst(func, y) {
                            k = k.wrapping_add(c);
                            v = x;
                        } else if let Some(c) = iconst(func, x) {
                            k = k.wrapping_add(c);
                            v = y;
                        } else {
                            break;
                        }
                    }
                    if folded {
                        n += 1;
                        continue;
                    }
                    // `isub (isub e, a), (isub e, b)` → `isub b, a`: the
                    // shared minuend telescopes —
                    // `(e - a) - (e - b) = b - a` in wrapping arithmetic —
                    // so `end`-relative lengths recomputed per iteration
                    // collapse to a pure cursor delta (`pre_len - len`
                    // becomes `ptr' - ptr`), deading the inner subs when
                    // single-use and shortening the dep chain off `e`.
                    // Each rewrite drops the result a def-level deeper in
                    // the DAG, so repeat applications terminate.
                    if !crate::pass_enabled("PLIRON_SUBSUB") {
                        continue;
                    }
                    let sub_of = |func: &Function, v: Value| -> Option<(Value, Value)> {
                        let d = func.dfg.resolved_value_def(v).inst()?;
                        if let InstructionData::Binary {
                            opcode: Opcode::Isub,
                            args: [e, x],
                        } = func.dfg.insts[d]
                        {
                            Some((func.dfg.resolve_aliases(e), func.dfg.resolve_aliases(x)))
                        } else {
                            None
                        }
                    };
                    if let (Some((e0, a)), Some((e1, bb))) =
                        (sub_of(func, args[0]), sub_of(func, args[1]))
                        && e0 == e1
                    {
                        func.dfg.insts[i] = InstructionData::Binary {
                            opcode: Opcode::Isub,
                            args: [bb, a],
                        };
                        dec(func, &mut usecnt, args[0]);
                        dec(func, &mut usecnt, args[1]);
                        inc(func, &mut usecnt, bb);
                        inc(func, &mut usecnt, a);
                        n += 1;
                        continue;
                    }
                }
                _ => {}
            }
            // Single-result insts whose result isn't already an alias.
            if func.dfg.inst_results(i).len() != 1 {
                continue;
            }
            let res = func.dfg.first_result(i);
            if func.dfg.resolve_aliases(res) != res {
                continue;
            }
            if let Some(x) = ident(func, data, func.dfg.value_type(res))
                // `x` must be `res`-typed for the alias — resolved args
                // can drift across types on malformed or stale chains.
                && func.dfg.value_type(x) == func.dfg.value_type(res)
                // An identity alias deletes `i` and moves `res`'s uses
                // onto `x`, stretching `x`'s live range from `i` to
                // `res`'s last use. Inside a loop that merge drops the
                // copy ra2 could have rematerialized at the use —
                // forcing `x` live-through where an in-loop recompute
                // would do (`tokenize`'s scan loop). Same-block copies
                // still fold; `Union` defs (no single block) fold too.
                && (la.loop_level(b).level() == 0
                    || matches!(def_block(func, x), Some(xb) if xb == b)
                    || def_block(func, x).is_none())
            {
                // `res`'s uses now resolve to `x` — move its tally over.
                let nu = uses(func, &usecnt, res);
                usecnt.insert(res, 0);
                func.dfg.clear_results(i);
                func.dfg.change_to_alias(res, x);
                *usecnt.entry(func.dfg.resolve_aliases(x)).or_default() += nu;
                dead.push(i);
                n += 1;
                continue;
            }
            let Some(k) = key(func, i) else { continue };
            match seen.entry(k) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    // Alias eagerly so later insts in this block key on the
                    // resolved args (an iconst dedup must unblock the imul
                    // dedup that feeds it); only the layout removal defers.
                    let nu = uses(func, &usecnt, res);
                    usecnt.insert(res, 0);
                    func.dfg.clear_results(i);
                    func.dfg.change_to_alias(res, *e.get());
                    *usecnt
                        .entry(func.dfg.resolve_aliases(*e.get()))
                        .or_default() += nu;
                    dead.push(i);
                    n += 1;
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(res);
                }
            }
        }
        for i in dead {
            for a in func.dfg.inst_args(i).to_vec() {
                dec(func, &mut usecnt, a);
            }
            func.layout.remove_inst(i);
        }
        // Folds alias each dropped result to its survivor; when a survivor
        // is itself folded later the chain grows a hop, and every later
        // `resolve_aliases` in this pass re-walks it. Flatten once the
        // backlog is worth an O(values) sweep (stage2 `rustc_query_impl`
        // cgu.15 spent ~3.5min resolving pass-grown chains).
        if n >= compact_at {
            func.dfg.compact_aliases();
            compact_at = n + 1024;
        }
    }
    n
}

/// Dominator-scoped value numbering. `lcse` clears its table per block,
/// so an identical pure inst in a *dominated* block never merges — a
/// diamond that recomputes `i+1`/`p+4` on each edge keeps every copy and
/// the merge block passes them through as params. Walk the domtree with
/// a scoped table: every visible entry dominates the current block, so a
/// repeat can alias the in-scope def directly. `sameargs` then sees
/// uniform edge args, drops the param, and the shared computation lives
/// once above the branch instead of once per arm.
pub fn dgvn(func: &mut Function) -> usize {
    use cranelift_codegen::dominator_tree::DominatorTree;
    use cranelift_codegen::flowgraph::ControlFlowGraph;
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let Some(entry) = func.layout.entry_block() else {
        return 0;
    };
    let mut seen: FxHashMap<CseKey, Value> = FxHashMap::default();
    // DFS over the domtree; `undo` restores `seen` when a block's subtree
    // completes so entries never leak to sibling subtrees.
    let mut undo: Vec<CseKey> = Vec::new();
    let mut marks: Vec<usize> = Vec::new();
    let mut stack = vec![(entry, false)];
    let mut n = 0;
    // Same pass-grown alias-chain hazard as `lcse` above.
    let mut compact_at = 1024;
    while let Some((b, exit)) = stack.pop() {
        if exit {
            let mark = marks.pop().unwrap();
            while undo.len() > mark {
                seen.remove(&undo.pop().unwrap());
            }
            continue;
        }
        marks.push(undo.len());
        stack.push((b, true));
        for c in dt.children(b).collect::<Vec<_>>() {
            stack.push((c, false));
        }
        let mut dead = Vec::new();
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            if func.dfg.inst_results(i).len() != 1 {
                continue;
            }
            let res = func.dfg.first_result(i);
            if func.dfg.resolve_aliases(res) != res {
                continue;
            }
            // `select c, x, x` → `x`: an earlier CSE round can unify two
            // formerly-distinct arm defs into aliases of one value.
            if let InstructionData::Ternary {
                opcode: Opcode::Select,
                args,
            } = func.dfg.insts[i]
                && func.dfg.resolve_aliases(args[1]) == func.dfg.resolve_aliases(args[2])
            {
                let a = func.dfg.resolve_aliases(args[1]);
                func.dfg.clear_results(i);
                func.dfg.change_to_alias(res, a);
                dead.push(i);
                n += 1;
                continue;
            }
            let Some(k) = cse_key(func, i) else { continue };
            match seen.entry(k) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    let v = func.dfg.resolve_aliases(*e.get());
                    func.dfg.clear_results(i);
                    func.dfg.change_to_alias(res, v);
                    dead.push(i);
                    n += 1;
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    undo.push(e.key().clone());
                    e.insert(res);
                }
            }
        }
        for i in dead {
            func.layout.remove_inst(i);
        }
        if n >= compact_at {
            func.dfg.compact_aliases();
            compact_at = n + 1024;
        }
    }
    n
}

/// A block param whose only uses feed its own edge args is a dead
/// recurrence — e.g. `j += i` left running when a countdown compare moves
/// the bound test to a `rem` param. Remove the param, its edge args on
/// every predecessor, and the now-dead feed insts.
pub fn selfarg(func: &mut Function) -> usize {
    // use map: v -> (inst operand uses, branch-arg uses as (inst, slot, arg
    // index) so `p` passed to `h` at its own index is recognizable).
    let mut uses: FxHashMap<Value, (Vec<Inst>, Vec<(Inst, usize, usize)>)> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for &a in func.dfg.inst_args(i) {
                uses.entry(func.dfg.resolve_aliases(a))
                    .or_default()
                    .0
                    .push(i);
            }
            for (d, bc) in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .enumerate()
            {
                let blk = bc.block(&func.dfg.value_lists);
                for (ai, a) in bc.args(&func.dfg.value_lists).enumerate() {
                    if let BlockArg::Value(v) = a {
                        uses.entry(func.dfg.resolve_aliases(v))
                            .or_default()
                            .1
                            .push((i, d, ai));
                    }
                    let _ = blk;
                }
            }
        }
    }
    let mut n = 0;
    for h in func.layout.blocks().collect::<Vec<_>>() {
        // Entry params are signature args, not edge-fed — never drop.
        if Some(h) == func.layout.entry_block() {
            continue;
        }
        let mut drops: Vec<(usize, Value, FxHashSet<Inst>)> = Vec::new();
        for (idx, p) in func.dfg.block_params(h).to_vec().into_iter().enumerate() {
            // Feed closure: insts consuming `p`, and insts consuming feed
            // insts' results. Every use must stay inside the closure or be
            // an edge arg to `h` at `idx`. `p` must not appear as an edge
            // arg anywhere but (h, idx).
            let mut feed: Vec<Inst> = Vec::new();
            let mut inset: FxHashSet<Inst> = FxHashSet::default();
            let mut ok = true;
            // Feed insts get deleted — they must be pure and non-trapping.
            let pure = |i: Inst| {
                let op = func.dfg.insts[i].opcode();
                !(op.can_load()
                    || op.can_store()
                    || op.can_trap()
                    || op.is_call()
                    || op.is_branch()
                    || op.is_terminator()
                    || op.other_side_effects())
            };
            if let Some((ops, edges)) = uses.get(&p) {
                for &i in ops {
                    if !pure(i) {
                        ok = false;
                    } else if inset.insert(i) {
                        feed.push(i);
                    }
                }
                for &(i, d, ai) in edges {
                    let bc = func.dfg.insts[i]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d];
                    if bc.block(&func.dfg.value_lists) != h || ai != idx {
                        ok = false;
                    }
                }
            }
            if !ok {
                continue;
            }
            while ok && let Some(i) = feed.pop() {
                if func.dfg.insts[i].opcode().is_terminator() {
                    break;
                }
                for &r in func.dfg.inst_results(i) {
                    let r = func.dfg.resolve_aliases(r);
                    let Some((ops, edges)) = uses.get(&r) else {
                        continue;
                    };
                    for &u in ops {
                        if !pure(u) {
                            ok = false;
                        } else if inset.insert(u) {
                            feed.push(u);
                        }
                    }
                    for &(u, d, ai) in edges {
                        let bc = func.dfg.insts[u]
                            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                            [d];
                        if bc.block(&func.dfg.value_lists) != h || ai != idx {
                            ok = false;
                        }
                    }
                }
            }
            if ok {
                drops.push((idx, p, inset));
            }
        }
        // Apply in descending param order so earlier removals don't shift
        // later indices.
        drops.sort_by_key(|(idx, _, _)| usize::MAX - idx);
        for (idx, p, inset) in drops {
            for b2 in func.layout.blocks().collect::<Vec<_>>() {
                for i in func.layout.block_insts(b2).collect::<Vec<_>>() {
                    let ndest = func.dfg.insts[i]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                        .len();
                    for d in 0..ndest {
                        let dfg = &mut func.dfg;
                        let bc = &mut dfg.insts[i].branch_destination_mut(
                            &mut dfg.jump_tables,
                            &mut dfg.exception_tables,
                        )[d];
                        if bc.block(&dfg.value_lists) == h {
                            bc.remove(idx, &mut dfg.value_lists);
                        }
                    }
                }
            }
            func.dfg.remove_block_param(p);
            for i in inset {
                // A feed inst can appear in another param's closure too
                // (e.g. `p += q` where both drop); the first removal wins.
                if func.layout.inst_block(i).is_some() {
                    func.layout.remove_inst(i);
                    func.dfg.clear_results(i);
                }
            }
            n += 1;
        }
    }
    n
}

/// MIR materializes `Simd` constants lane-by-lane — `stack_addr`, a row of
/// `store.i8 k` at consecutive offsets, then a whole-vector `load` back.
/// That's ten instructions per loop iteration to build a constant
/// (hashbrown's `u8x8` special-byte checks in `reserve_rehash`). When every
/// byte a slot load reads is covered by constant stores that dominate it —
/// and the slot's address never escapes those accesses — the load becomes
/// `iconst` (`+bitcast` for vectors/floats); when all of the slot's loads
/// fold, the lane stores go dead and are dropped.
pub fn vecslot(func: &mut Function) -> usize {
    use cranelift_codegen::dominator_tree::DominatorTree;
    use cranelift_codegen::flowgraph::ControlFlowGraph;
    use cranelift_codegen::ir::StackSlot;

    #[derive(Default)]
    struct Slot {
        // byte offset -> (byte value, covering store inst)
        bytes: FxHashMap<i64, (u8, Inst)>,
        stores: Vec<Inst>,
        loads: Vec<(Inst, i64, usize)>,
        bad: bool,
    }

    // value -> insts using it as an operand; values on an edge disqualify
    // their slot (address escapes into a param).
    let mut uses: FxHashMap<Value, Vec<Inst>> = FxHashMap::default();
    let mut edgevals: Vec<Value> = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for &a in func.dfg.inst_args(i) {
                uses.entry(func.dfg.resolve_aliases(a)).or_default().push(i);
            }
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for a in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(v) = a {
                        edgevals.push(v);
                    }
                }
            }
        }
    }

    // value -> (slot, byte offset) for `stack_addr` and `iadd` chains off
    // it. Results may be defined in a later-laid-out block, so propagate to
    // a fixpoint.
    let mut slotval: FxHashMap<Value, (StackSlot, i64)> = FxHashMap::default();
    for _ in 0..8 {
        let mut grew = false;
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                match func.dfg.insts[i] {
                    InstructionData::StackAddr {
                        stack_slot, offset, ..
                    } => {
                        for &r in func.dfg.inst_results(i) {
                            grew |= slotval.insert(r, (stack_slot, offset.into())).is_none();
                        }
                    }
                    InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        args,
                    } => {
                        let r = func.dfg.first_result(i);
                        for (a, b) in [(args[0], args[1]), (args[1], args[0])] {
                            if let Some(&(ss, o)) = slotval.get(&func.dfg.resolve_aliases(a))
                                && let Some(k) = iconst(func, b)
                                && slotval.insert(r, (ss, o + k)).is_none()
                            {
                                grew = true;
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if !grew {
            break;
        }
    }

    let mut slots: FxHashMap<StackSlot, Slot> = FxHashMap::default();
    for v in edgevals {
        if let Some(&(ss, _)) = slotval.get(&func.dfg.resolve_aliases(v)) {
            slots.entry(ss).or_default().bad = true;
        }
    }
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            match func.dfg.insts[i] {
                InstructionData::Store {
                    opcode,
                    args,
                    offset,
                    ..
                } => {
                    let p = func.dfg.resolve_aliases(args[1]);
                    let Some(&(ss, o)) = slotval.get(&p) else {
                        continue;
                    };
                    let off = o + i64::from(offset);
                    let (n, k) = match opcode {
                        Opcode::Store => {
                            let t = func.dfg.value_type(func.dfg.resolve_aliases(args[0]));
                            let k = iconst(func, args[0]);
                            (t.bytes() as usize, k.map(|x| x as u64 as u128))
                        }
                        Opcode::Istore8 => (1, iconst(func, args[0]).map(|x| x as u64 as u128)),
                        _ => (0, None),
                    };
                    let s = slots.entry(ss).or_default();
                    match (n, k) {
                        (1..=16, Some(k)) if s.bytes.len() < 65536 => {
                            s.stores.push(i);
                            for j in 0..n as i64 {
                                let byte = (k >> (j * 8)) as u8;
                                if s.bytes.insert(off + j, (byte, i)).is_some() {
                                    s.bad = true;
                                }
                            }
                        }
                        _ => s.bad = true,
                    }
                }
                InstructionData::Load { arg, offset, .. } => {
                    let p = func.dfg.resolve_aliases(arg);
                    let Some(&(ss, o)) = slotval.get(&p) else {
                        continue;
                    };
                    let n = func.dfg.value_type(func.dfg.first_result(i)).bytes() as usize;
                    slots
                        .entry(ss)
                        .or_default()
                        .loads
                        .push((i, o + i64::from(offset), n));
                }
                InstructionData::LoadNoOffset { arg, .. } => {
                    let p = func.dfg.resolve_aliases(arg);
                    let Some(&(ss, o)) = slotval.get(&p) else {
                        continue;
                    };
                    let n = func.dfg.value_type(func.dfg.first_result(i)).bytes() as usize;
                    slots.entry(ss).or_default().loads.push((i, o, n));
                }
                _ => {
                    // A non-load/store inst consuming a slot-rooted value:
                    // the address escapes (unless it's an addr-chain iadd).
                    for &a in func.dfg.inst_args(i) {
                        let a = func.dfg.resolve_aliases(a);
                        if let Some(&(ss, _)) = slotval.get(&a) {
                            let chain = func
                                .dfg
                                .inst_results(i)
                                .iter()
                                .any(|&r| slotval.contains_key(&func.dfg.resolve_aliases(r)));
                            if !chain {
                                slots.entry(ss).or_default().bad = true;
                            }
                        }
                    }
                }
            }
        }
    }
    // A `load`/`store` that uses a slot-rooted value as anything but its
    // address operand also escapes it (e.g. `store ss_addr, p`).
    for (v, &(ss, _)) in &slotval {
        for &u in uses.get(v).into_iter().flatten() {
            let escaped = match func.dfg.insts[u] {
                InstructionData::Load { arg, .. } | InstructionData::LoadNoOffset { arg, .. } => {
                    func.dfg.resolve_aliases(arg) != *v
                }
                InstructionData::Store { args, .. } => func.dfg.resolve_aliases(args[1]) != *v,
                _ => false,
            };
            if escaped {
                slots.entry(ss).or_default().bad = true;
            }
        }
    }

    if slots.values().all(|s| s.bad || s.loads.is_empty()) {
        return 0;
    }
    let cfg = ControlFlowGraph::with_function(func);
    let domtree = DominatorTree::with_function(func, &cfg);

    let mut n = 0;
    for (ss, s) in slots {
        if s.bad || s.loads.is_empty() {
            continue;
        }
        let mut all_folded = true;
        for &(l, off, ln) in s.loads.iter() {
            let Some(lbytes) = (0..ln as i64)
                .map(|j| s.bytes.get(&(off + j)).copied())
                .collect::<Option<Vec<_>>>()
            else {
                all_folded = false;
                continue;
            };
            if !lbytes
                .iter()
                .all(|&(_, st)| domtree.try_dominates(st, l, &func.layout))
            {
                all_folded = false;
                continue;
            }
            let lty = func.dfg.value_type(func.dfg.first_result(l));
            let le: u128 = lbytes
                .iter()
                .enumerate()
                .fold(0u128, |acc, (j, (b, _))| acc | (*b as u128) << (j * 8));
            let mut pos = FuncCursor::new(func).at_inst(l);
            let nv = if lty.is_int() && !lty.is_vector() {
                if ln <= 8 {
                    let m = if lty.bits() == 64 {
                        u64::MAX
                    } else {
                        (1u64 << lty.bits()) - 1
                    };
                    pos.ins().iconst(lty, (le as u64 & m) as i64)
                } else if ln == 16 {
                    let lo = pos.ins().iconst(types::I64, le as u64 as i64);
                    let hi = pos.ins().iconst(types::I64, (le >> 64) as u64 as i64);
                    pos.ins().iconcat(lo, hi)
                } else {
                    all_folded = false;
                    continue;
                }
            } else if matches!(ln, 4 | 8 | 16) && (lty.is_vector() || lty.is_float()) {
                // Uniform lanes lower to `movi`/`dup` (`iconst.i64`+`bitcast`
                // goes through `fmov` on a GPR and can't rematerialize).
                let lb = lty.lane_bits();
                let lmask: u128 = if lb == 128 {
                    u128::MAX
                } else {
                    (1u128 << lb) - 1
                };
                let lanes = ln * 8 / lb as usize;
                let uniform = lty.is_vector()
                    && lanes > 1
                    && (1..lanes).all(|j| (le >> (j as u32 * lb)) & lmask == le & lmask);
                if uniform {
                    let et = lty.lane_type();
                    let mask = if et.bits() == 64 {
                        u64::MAX
                    } else {
                        (1u64 << et.bits()) - 1
                    };
                    let k = pos.ins().iconst(et, (le as u64 & mask) as i64);
                    pos.ins().splat(lty, k)
                } else {
                    let wide = match ln {
                        4 => pos.ins().iconst(types::I32, le as u32 as i64),
                        8 => pos.ins().iconst(types::I64, le as u64 as i64),
                        _ => {
                            let lo = pos.ins().iconst(types::I64, le as u64 as i64);
                            let hi = pos.ins().iconst(types::I64, (le >> 64) as u64 as i64);
                            pos.ins().iconcat(lo, hi)
                        }
                    };
                    pos.ins().bitcast(
                        lty,
                        cranelift_codegen::ir::MemFlagsData::new()
                            .with_endianness(cranelift_codegen::ir::Endianness::Little),
                        wide,
                    )
                }
            } else {
                all_folded = false;
                continue;
            };
            let lres = func.dfg.first_result(l);
            func.dfg.clear_results(l);
            if func.dfg.value_is_attached(lres) {
                // Still attached somewhere else — leave the load alone
                // rather than corrupt the DFG aliasing a live result.
                all_folded = false;
                continue;
            }
            func.dfg.change_to_alias(lres, nv);
            func.layout.remove_inst(l);
            n += 1;
        }
        if all_folded {
            // Every load folded: the lane stores are unobservable, and the
            // `stack_addr`/`iadd` addr chain goes dead with them — the
            // escape check proved chain values feed only this slot's
            // accesses or other chain insts.
            for i in &s.stores {
                if func.layout.inst_block(*i).is_some() {
                    func.layout.remove_inst(*i);
                    func.dfg.clear_results(*i);
                    n += 1;
                }
            }
            for (v, &(ss2, _)) in &slotval {
                if ss2 != ss {
                    continue;
                }
                if let Some(d) = func.dfg.value_def(*v).inst()
                    && func.layout.inst_block(d).is_some()
                {
                    func.layout.remove_inst(d);
                    func.dfg.clear_results(d);
                    n += 1;
                }
            }
        }
    }
    n
}

/// `call %Mem{cpy,move,set}(dst, src, iconst n)` for small `n` expands into
/// unaligned load/store chunks — a few `movq`-scale ops instead of a libc
/// call inside a hot loop (rustc emits `ptr::copy_nonoverlapping(_, _, 1)`
/// as a runtime-size `llvm.memcpy`; the len folds to `iconst` only at CLIF
/// level, so lower.rs's const-size inlining can't see it).
///
/// Every load precedes every store, so the same expansion is also correct
/// for overlapping `memmove` (all source bytes are buffered in registers
/// first); the `memset` fill must be an `iconst` so it splats into wider
/// chunk constants. The result value (the dst pointer) aliases the dst arg.
pub fn memcall(func: &mut Function) -> usize {
    /// Largest byte count expanded inline — caps the expansion at eight
    /// 8-byte chunks plus remainders.
    const MAX: u64 = 64;
    let mut done = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            if func.dfg.insts[i].opcode() != Opcode::Call {
                continue;
            }
            let InstructionData::Call { func_ref: fref, .. } = func.dfg.insts[i] else {
                continue;
            };
            let ExternalName::LibCall(lc) = func.dfg.ext_funcs[fref].name else {
                continue;
            };
            if !matches!(lc, LibCall::Memcpy | LibCall::Memmove | LibCall::Memset) {
                continue;
            }
            let args = func.dfg.inst_args(i).to_vec();
            if args.len() != 3 {
                continue;
            }
            let Some(n) = iconst(func, args[2]).and_then(|k| u64::try_from(k).ok()) else {
                continue;
            };
            if n > MAX {
                continue;
            }
            let (dst, src) = (args[0], args[1]);
            let fill = if lc == LibCall::Memset {
                // Only the low byte is the fill value.
                let Some(f) = iconst(func, src) else { continue };
                (f as u8) as u64
            } else {
                0
            };
            let mut pos = FuncCursor::new(func).at_inst(i);
            let flags = MemFlagsData::new();
            let mut rem = n;
            let mut off = 0i32;
            let mut vals = Vec::new();
            // Decompose into 8/4/2/1-byte chunks.
            let chunks = [
                (8u64, types::I64),
                (4u64, types::I32),
                (2u64, types::I16),
                (1u64, types::I8),
            ];
            while rem > 0 {
                let (sz, ty) = chunks.iter().copied().find(|&(sz, _)| sz <= rem).unwrap();
                let v = if lc == LibCall::Memset {
                    let splat = match sz {
                        8 => 0x0101_0101_0101_0101u64,
                        4 => 0x0101_0101u64,
                        2 => 0x0101u64,
                        _ => 0x01u64,
                    };
                    // Replicate the byte, then sign-extend to the iconst's
                    // expected imm form.
                    let bits = fill * splat;
                    let bits = ((bits << (64 - sz * 8)) as i64) >> (64 - sz * 8);
                    pos.ins().iconst(ty, bits)
                } else {
                    pos.ins().load(ty, flags, src, off)
                };
                vals.push((v, sz));
                rem -= sz;
                off += sz as i32;
            }
            let mut off = 0i32;
            for (v, sz) in vals {
                pos.ins().store(flags, v, dst, off);
                off += sz as i32;
            }
            let res = pos.func.dfg.inst_results(i).to_vec();
            pos.func.dfg.clear_results(i);
            if let Some(r) = res.first() {
                pos.func.dfg.change_to_alias(*r, dst);
            }
            pos.func.layout.remove_inst(i);
            done += 1;
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::{AbiParam, ExtFuncData, Signature, UserFuncName};
    use cranelift_codegen::isa::CallConv;

    /// `call %Memcpy(dst, src, iconst 8)` becomes `load.i64; store.i64`
    /// with the dst pointer aliased to the call's result.
    #[test]
    fn memcall_inlines_const_copy() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        func.layout.append_block(entry);
        let dst = func.dfg.append_block_param(entry, types::I64);
        let src = func.dfg.append_block_param(entry, types::I64);

        let mut msig = Signature::new(CallConv::Fast);
        msig.params = vec![AbiParam::new(types::I64); 3];
        msig.returns.push(AbiParam::new(types::I64));
        let sref = func.import_signature(msig);
        let fref = func.import_function(ExtFuncData {
            name: ExternalName::LibCall(LibCall::Memcpy),
            signature: sref,
            colocated: false,
            patchable: false,
        });

        let ci = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
            let n = cur.ins().iconst(types::I64, 8);
            cur.ins().call(fref, &[dst, src, n])
        };
        let r = func.dfg.first_result(ci);
        FuncCursor::new(&mut func)
            .at_bottom(entry)
            .ins()
            .return_(&[r]);

        assert_eq!(memcall(&mut func), 1);
        let insts: Vec<Inst> = func.layout.block_insts(entry).collect();
        assert_eq!(
            insts
                .iter()
                .map(|&i| func.dfg.insts[i].opcode())
                .collect::<Vec<_>>(),
            [Opcode::Iconst, Opcode::Load, Opcode::Store, Opcode::Return]
        );
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("memcall produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// `a`'s only edge feeds it `b`'s param and `b`'s only edge feeds it
    /// `a`'s: dropping both maps each param onto the other, a replacement
    /// cycle with no survivor — the old chase left `w` on a dropped param
    /// and `remove_block_param` detached it under the icmp uses (stage2
    /// ICE'd `argument v… is not attached` on rustc_middle et al).
    #[test]
    fn cyclic_param_drops_keep_a_survivor() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let a = func.dfg.make_block();
        let b = func.dfg.make_block();
        for blk in [entry, a, b] {
            func.layout.append_block(blk);
        }
        let pa = func.dfg.append_block_param(a, types::I64);
        let pb = func.dfg.append_block_param(b, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let z = cur.ins().iconst(types::I64, 0);
        cur.ins().return_(&[z]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(a);
        let one = cur.ins().iconst(types::I64, 1);
        cur.ins().icmp(IntCC::NotEqual, pa, one);
        cur.ins().jump(b, &[pa.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(b);
        let two = cur.ins().iconst(types::I64, 2);
        cur.ins().icmp(IntCC::NotEqual, pb, two);
        cur.ins().jump(a, &[pb.into()]);

        sameargs(&mut func);
        assert_eq!(func.dfg.block_params(a).len(), 1);
        assert_eq!(func.dfg.block_params(b).len(), 1);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("sameargs produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// `x + uextend(icmp)` becomes `select icmp, x + 1, x` on x64 (the
    /// `offset += is_less` idiom — `setb`+`movzbq`+`add` vs `lea`+`cmov`),
    /// and stays an add elsewhere where `add x, w, uxtb` already fuses.
    #[test]
    fn bool_add_becomes_select_on_x64() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        func.layout.append_block(entry);
        let a = func.dfg.append_block_param(entry, types::I64);
        let b = func.dfg.append_block_param(entry, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let c = cur.ins().icmp(IntCC::SignedLessThan, a, b);
        let z = cur.ins().uextend(types::I64, c);
        let s = cur.ins().iadd(a, z);
        let d = cur.ins().isub(s, z);
        // unsigned bools stay as `iadd x, uextend(icmp)` on x64 — isel
        // fuses them into `cmp`+`adc`/`sbb` off CF.
        let cu = cur.ins().icmp(IntCC::UnsignedLessThan, a, b);
        let zu = cur.ins().uextend(types::I64, cu);
        let e = cur.ins().iadd(d, zu);
        cur.ins().return_(&[e]);

        let sels = |f: &Function| {
            f.layout
                .blocks()
                .flat_map(|b| f.layout.block_insts(b))
                .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Select)
                .count()
        };
        let mut f2 = func.clone();
        assert_eq!(run(&mut func, true), 2);
        assert_eq!(sels(&func), 2);
        assert_eq!(run(&mut f2, false), 0);
        assert_eq!(sels(&f2), 0);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!(
                "bool_add select produced invalid IR: {e}\n{}",
                func.display()
            );
        }
    }

    /// `select (x-5 != k), (select (x+3 == k), 1, 0) + 2, sel(..)` — the
    /// `x-5 == k` fact on the false arm refutes `x+3 == k` there, so the
    /// nested select collapses to 0; then the dead `select(p1,1,0)`
    /// leaves `+2` merging into `select(p1,3,2)`.
    #[test]
    fn arm_fact_folds_correlated_selects() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I8));
        sig.params.push(AbiParam::new(types::I8));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        func.layout.append_block(entry);
        let x = func.dfg.append_block_param(entry, types::I8);
        let k = func.dfg.append_block_param(entry, types::I8);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let three = cur.ins().iconst(types::I8, 3);
        let x3 = cur.ins().iadd(x, three);
        let p1 = cur.ins().icmp(IntCC::Equal, x3, k);
        let one = cur.ins().iconst(types::I64, 1);
        let zero = cur.ins().iconst(types::I64, 0);
        let a1 = cur.ins().select(p1, one, zero);
        let five = cur.ins().iconst(types::I8, 5);
        let xm5 = cur.ins().isub(x, five);
        let p2 = cur.ins().icmp(IntCC::NotEqual, xm5, k);
        let two = cur.ins().iconst(types::I64, 2);
        let a12 = cur.ins().iadd(a1, two);
        let r = cur.ins().select(p2, a12, a1);
        cur.ins().return_(&[r]);

        run(&mut func, false);
        // The outer select's false arm must be the constant 0, and its
        // true arm `select(p1, 3, 2)` — adjacent consts for `csinc`.
        let ret = func
            .layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b))
            .find(|&i| func.dfg.insts[i].opcode() == Opcode::Return)
            .unwrap();
        let InstructionData::Ternary {
            opcode: Opcode::Select,
            args: ra,
        } = func.dfg.insts[func
            .dfg
            .value_def(func.dfg.inst_args(ret)[0])
            .inst()
            .unwrap()]
        else {
            panic!("outer select missing\n{}", func.display())
        };
        assert_eq!(iconst(&func, ra[2]), Some(0), "{}", func.display());
        let InstructionData::Ternary {
            opcode: Opcode::Select,
            args: ta,
        } = func.dfg.insts[func.dfg.value_def(ra[1]).inst().unwrap()]
        else {
            panic!("true arm not a select\n{}", func.display())
        };
        assert_eq!(iconst(&func, ta[1]), Some(3));
        assert_eq!(iconst(&func, ta[2]), Some(2));
        assert_eq!(
            func.dfg.resolve_aliases(ta[0]),
            func.dfg.resolve_aliases(p1)
        );
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("arm_fact fold produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// `((x & m1) | c1) & m2 | c2` collapses to `band x, m1&m2` +
    /// `bor ((c1&m2)|c2)` — the packed-enum field-merge shape. `ishl` of
    /// constants counts as a constant operand.
    #[test]
    fn bitmerge_collapses_field_chain() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        func.layout.append_block(entry);
        let x = func.dfg.append_block_param(entry, types::I64);

        // (x & -65281) | (1<<8), then (& -256) | (1<<0) — the
        // ParseIntError tag/field inserts from parse_only.
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let m1 = cur.ins().iconst(types::I64, -65281);
        let b1 = cur.ins().band(x, m1);
        let one8 = cur.ins().iconst(types::I8, 1);
        let e1 = cur.ins().uextend(types::I64, one8);
        let sh8 = cur.ins().iconst(types::I64, 8);
        let c1 = cur.ins().ishl(e1, sh8);
        let o1 = cur.ins().bor(b1, c1);
        let m2 = cur.ins().iconst(types::I64, -256);
        let b2 = cur.ins().band(o1, m2);
        let sh0 = cur.ins().iconst(types::I64, 0);
        let c2 = cur.ins().ishl(e1, sh0);
        let o2 = cur.ins().bor(b2, c2);
        cur.ins().return_(&[o2]);

        run(&mut func, false);
        // Must collapse to `band x, -65536` then `bor , 257` — the ret's
        // arg is a `bor` over a `band`, no surviving intermediate links.
        let ret = func
            .layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b))
            .find(|&i| func.dfg.insts[i].opcode() == Opcode::Return)
            .unwrap();
        let rv = func.dfg.inst_args(ret)[0];
        let InstructionData::Binary {
            opcode: Opcode::Bor,
            args: ba,
        } = func.dfg.insts[func.dfg.resolved_value_def(rv).inst().unwrap()]
        else {
            panic!("result not a bor\n{}", func.display())
        };
        assert_eq!(
            ceval(&func, ba[1], 6).or_else(|| ceval(&func, ba[0], 6)),
            Some(257),
            "{}",
            func.display()
        );
        let band_v = if ceval(&func, ba[1], 6).is_some() {
            ba[0]
        } else {
            ba[1]
        };
        let InstructionData::Binary {
            opcode: Opcode::Band,
            args: aa,
        } = func.dfg.insts[func.dfg.value_def(band_v).inst().unwrap()]
        else {
            panic!("inner op not a band\n{}", func.display())
        };
        let m = ceval(&func, aa[1], 6)
            .or_else(|| ceval(&func, aa[0], 6))
            .unwrap();
        assert_eq!(m, 0xffff_ffff_ffff_0000);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("bitmerge produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// `imul (imul x, c1), c2` reassociates to `imul x, c1·c2`, and a
    /// product of 1 (the `div_u128_const` `(x-r)·d⁻¹` quotient rescaled by
    /// `d`) aliases the inner operand away entirely — for i128 too, where
    /// constants are `iconcat` pairs.
    #[test]
    fn imul_const_chain_reassociates() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I128));
        sig.returns.push(AbiParam::new(types::I128));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        func.layout.append_block(entry);
        let x = func.dfg.append_block_param(entry, types::I128);

        // `x · d⁻¹ · d` — the divconst quotient shape. d=7,
        // d⁻¹ = 0x6db6db6db6db6db6db6db6db6db6db7 mod 2^128.
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let il = cur.ins().iconst(types::I64, 0x6db6db6db6db6db7u64 as i64);
        let ih = cur.ins().iconst(types::I64, 0xb6db6db6db6db6dbu64 as i64);
        let inv = cur.ins().iconcat(il, ih);
        let q = cur.ins().imul(x, inv);
        let sl = cur.ins().iconst(types::I64, 7);
        let sh = cur.ins().iconst(types::I64, 0);
        let seven = cur.ins().iconcat(sl, sh);
        let r = cur.ins().imul(q, seven);
        cur.ins().return_(&[r]);

        run(&mut func, false);
        let ret = func
            .layout
            .blocks()
            .flat_map(|b| func.layout.block_insts(b))
            .find(|&i| func.dfg.insts[i].opcode() == Opcode::Return)
            .unwrap();
        let rv = func.dfg.resolve_aliases(func.dfg.inst_args(ret)[0]);
        // The whole `(x-0)·inv·7` chain must collapse to `x` itself.
        assert_eq!(rv, func.dfg.resolve_aliases(x), "{}", func.display());
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("imul reassoc produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// A dominated diamond arm recomputing an inst its dominator already
    /// emitted aliases the dominating def (`i+1` on each looprot/punroll
    /// edge); a sibling-arm copy with no dominator must be left alone.
    #[test]
    fn dgvn_aliases_dominated_dup_only() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let head = func.dfg.make_block();
        let taken = func.dfg.make_block();
        let merge = func.dfg.make_block();
        for b in [head, taken, merge] {
            func.layout.append_block(b);
        }
        let i = func.dfg.append_block_param(head, types::I64);
        let p = func.dfg.append_block_param(head, types::I64);
        let acc = func.dfg.append_block_param(merge, types::I64);

        let (cond, i1) = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(head);
            let one = cur.ins().iconst(types::I64, 1);
            let i1 = cur.ins().iadd(i, one);
            let p1 = cur.ins().iadd(p, one);
            let cond = cur.ins().icmp(IntCC::UnsignedLessThan, i1, p1);
            cur.ins().brif(cond, taken, &[], merge, &[i1.into()]);
            (cond, i1)
        };
        let _ = cond;
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(taken);
            let one = cur.ins().iconst(types::I64, 1);
            let i1dup = cur.ins().iadd(i, one);
            cur.ins().jump(merge, &[i1dup.into()]);
        }
        FuncCursor::new(&mut func)
            .at_bottom(merge)
            .ins()
            .return_(&[acc]);

        // Both the `iconst 1` copy and the `iadd` copy alias away.
        assert_eq!(dgvn(&mut func), 2);
        // The dominated copy's result now resolves to `i1`.
        let arg = func.dfg.insts[func.layout.last_inst(taken).unwrap()]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[0]
            .args(&func.dfg.value_lists)
            .next()
            .and_then(|a| match a {
                BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
                _ => None,
            });
        assert_eq!(arg, Some(i1));
    }

    /// Complementary `load.notrap (select c, pa, pb)` /
    /// `load.notrap (select c, pb, pa)` — the pair dereferences both
    /// addresses unconditionally, so each folds to a select of two
    /// shared speculative loads.
    #[test]
    fn loadsel_speculates_complementary_pair() {
        let mut func = loadsel_func(MemFlagsData::new().with_notrap(), true, 0, false);
        let entry = func.layout.entry_block().unwrap();
        assert!(run(&mut func, true) > 0);
        let ops: Vec<Opcode> = func
            .layout
            .block_insts(entry)
            .map(|i| func.dfg.insts[i].opcode())
            .collect();
        assert_eq!(
            ops,
            [
                Opcode::Select,
                Opcode::Load,
                Opcode::Load,
                Opcode::Select,
                Opcode::Select,
                Opcode::Select,
                Opcode::Iadd,
                Opcode::Return,
            ],
            "{}",
            func.display()
        );
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("loadsel produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// A lone `load (select …)` may only speculatively dereference the
    /// selected arm — without the complementary sibling the non-taken
    /// arm could fault, so nothing is rewritten. Same for a
    /// trap-checked pair.
    #[test]
    fn loadsel_needs_sibling_and_notrap() {
        for (flags, sibling) in [
            (MemFlagsData::new().with_notrap(), false),
            (MemFlagsData::new(), true),
        ] {
            let mut func = loadsel_func(flags, sibling, 0, false);
            let entry = func.layout.entry_block().unwrap();
            run(&mut func, true);
            let ops: Vec<Opcode> = func
                .layout
                .block_insts(entry)
                .map(|i| func.dfg.insts[i].opcode())
                .collect();
            assert_eq!(
                ops.iter().filter(|&&o| o == Opcode::Load).count(),
                if sibling { 2 } else { 1 },
                "{}",
                func.display()
            );
            // Every surviving load still addresses a select — no
            // speculative value-selects were introduced.
            assert!(
                func.layout.block_insts(entry).all(|i| {
                    match func.dfg.insts[i] {
                        InstructionData::Load { arg, .. } => {
                            func.dfg.insts[func.dfg.resolved_value_def(arg).inst().unwrap()]
                                .opcode()
                                == Opcode::Select
                        }
                        _ => true,
                    }
                }),
                "{}",
                func.display()
            );
        }
    }

    /// A sibling at another offset (`*sel` vs `sel->field`) or an
    /// extending load names different bytes than the rewritten pair
    /// would read — the pair must stay as is (miscompiled
    /// `core::slice::sort::shared::smallsort::swap_if_less`, found by
    /// `clif-check report --check` over the std corpus).
    #[test]
    fn loadsel_needs_matching_offset_and_plain_load() {
        for (sib_off, narrow) in [(16, false), (0, true)] {
            let mut func = loadsel_func(MemFlagsData::new().with_notrap(), true, sib_off, narrow);
            let entry = func.layout.entry_block().unwrap();
            run(&mut func, true);
            let selects_feeding_loads = func
                .layout
                .block_insts(entry)
                .filter(|&i| match func.dfg.insts[i] {
                    InstructionData::Load { arg, .. } => {
                        func.dfg.insts[func.dfg.resolved_value_def(arg).inst().unwrap()].opcode()
                            == Opcode::Select
                    }
                    _ => false,
                })
                .count();
            assert_eq!(selects_feeding_loads, 2, "off={sib_off} narrow={narrow}\n{}", func.display());
        }
    }

    /// `select c, pa, pb; *sel` ×2 (the second with swapped arms, at
    /// `sib_off`, a `uload8` if `narrow`), sum.
    fn loadsel_func(flags: MemFlagsData, sibling: bool, sib_off: i32, narrow: bool) -> Function {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params = vec![
            AbiParam::new(types::I64),
            AbiParam::new(types::I64),
            AbiParam::new(types::I8),
        ];
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        func.layout.append_block(entry);
        let pa = func.dfg.append_block_param(entry, types::I64);
        let pb = func.dfg.append_block_param(entry, types::I64);
        let c = func.dfg.append_block_param(entry, types::I8);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let s1 = cur.ins().select(c, pa, pb);
        let l1 = cur.ins().load(types::I64, flags.clone(), s1, 0);
        let l2 = if sibling {
            let s2 = cur.ins().select(c, pb, pa);
            if narrow {
                cur.ins().uload8(types::I64, flags, s2, sib_off)
            } else {
                cur.ins().load(types::I64, flags, s2, sib_off)
            }
        } else {
            cur.ins().iconst(types::I64, 0)
        };
        let r = cur.ins().iadd(l1, l2);
        cur.ins().return_(&[r]);
        func
    }

    /// `brif (icmp ult (ireduce.i8 (uload8.i64 p)), -128)` — the UTF-8
    /// ASCII test — becomes `brif (band load, 0x80)` on the wide load
    /// value with the arms swapped (bit set == multibyte), so isel emits
    /// one `tbnz`. The compare, reduce, and constant all die.
    #[test]
    fn sgntest_lead_byte() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let ascii = func.dfg.make_block();
        let multi = func.dfg.make_block();
        for blk in [entry, ascii, multi] {
            func.layout.append_block(blk);
        }
        let p = func.dfg.append_block_param(entry, types::I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let w = cur
            .ins()
            .uload8(types::I64, MemFlagsData::new().with_notrap(), p, 0);
        let b8 = cur.ins().ireduce(types::I8, w);
        let k = cur.ins().iconst(types::I8, -128);
        let c = cur.ins().icmp(IntCC::UnsignedLessThan, b8, k);
        cur.ins().brif(c, ascii, &[], multi, &[]);
        for (i, blk) in [ascii, multi].iter().enumerate() {
            let mut cur = FuncCursor::new(&mut func).at_bottom(*blk);
            let r = cur.ins().iconst(types::I64, i as i64);
            cur.ins().return_(&[r]);
        }

        assert_eq!(sgntest(&mut func), 1);
        let ops: Vec<Opcode> = func
            .layout
            .block_insts(entry)
            .map(|i| func.dfg.insts[i].opcode())
            .collect();
        assert_eq!(
            ops,
            [Opcode::Uload8, Opcode::Iconst, Opcode::Band, Opcode::Brif],
            "{}",
            func.display()
        );
        let br = func.layout.block_insts(entry).last().unwrap();
        let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[br] else {
            panic!()
        };
        // Band operand is the load itself; then/else swapped.
        let InstructionData::Binary { args, .. } =
            func.dfg.insts[func.dfg.value_def(arg).inst().unwrap()]
        else {
            panic!()
        };
        assert_eq!(func.dfg.resolve_aliases(args[0]), w);
        assert_eq!(
            blocks[0].block(&func.dfg.value_lists),
            multi,
            "{}",
            func.display()
        );
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("sgntest produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// `brif (icmp slt x, 0)` keeps the same arm order — bit set is the
    /// then-edge — and the mask lands on a positive `1 << 63`… no: on
    /// i32 the sign bit is still one stored bit only via i64, so the
    /// fold emits `band.i32 x, MIN` — not fused, but never worse.
    #[test]
    fn sgntest_sign_bit_i32() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I32));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let neg = func.dfg.make_block();
        let pos = func.dfg.make_block();
        for blk in [entry, neg, pos] {
            func.layout.append_block(blk);
        }
        let x = func.dfg.append_block_param(entry, types::I32);
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let z = cur.ins().iconst(types::I32, 0);
        let c = cur.ins().icmp(IntCC::SignedLessThan, x, z);
        cur.ins().brif(c, neg, &[], pos, &[]);
        for (i, blk) in [neg, pos].iter().enumerate() {
            let mut cur = FuncCursor::new(&mut func).at_bottom(*blk);
            let r = cur.ins().iconst(types::I64, i as i64);
            cur.ins().return_(&[r]);
        }

        assert_eq!(sgntest(&mut func), 1);
        let ops: Vec<Opcode> = func
            .layout
            .block_insts(entry)
            .map(|i| func.dfg.insts[i].opcode())
            .collect();
        assert_eq!(
            ops,
            [Opcode::Iconst, Opcode::Band, Opcode::Brif],
            "{}",
            func.display()
        );
        // Same arm order: the sign bit *is* the predicate.
        let br = func.layout.block_insts(entry).last().unwrap();
        let InstructionData::Brif { blocks, .. } = func.dfg.insts[br] else {
            panic!()
        };
        assert_eq!(blocks[0].block(&func.dfg.value_lists), neg);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("sgntest produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// A range test that is *not* one bit (`x <u 96` on an i8) is left
    /// alone — it needs real compare semantics, not a mask.
    #[test]
    fn sgntest_skips_range() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I8));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let a = func.dfg.make_block();
        let b = func.dfg.make_block();
        for blk in [entry, a, b] {
            func.layout.append_block(blk);
        }
        let x = func.dfg.append_block_param(entry, types::I8);
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let k = cur.ins().iconst(types::I8, 96);
        let c = cur.ins().icmp(IntCC::UnsignedLessThan, x, k);
        cur.ins().brif(c, a, &[], b, &[]);
        for blk in [a, b] {
            let mut cur = FuncCursor::new(&mut func).at_bottom(blk);
            let r = cur.ins().iconst(types::I64, 0);
            cur.ins().return_(&[r]);
        }
        assert_eq!(sgntest(&mut func), 0);
    }

    /// Two preds pass `x + 8` to a merge param, but through different
    /// def chains (`iadd x,8` vs `iadd(isub x,4),12`). The congruence is
    /// transitive, so neither literal-equality `sameargs` nor one-level
    /// `arg_shape` `arghoist` can see it. The param drops and `x + 8` is
    /// materialized once at the merge head.
    #[test]
    fn webmerge_congruent_deltas() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let p1 = func.dfg.make_block();
        let p2 = func.dfg.make_block();
        let s = func.dfg.make_block();
        for blk in [entry, p1, p2, s] {
            func.layout.append_block(blk);
        }
        let x = func.dfg.append_block_param(entry, types::I64);
        let ps = func.dfg.append_block_param(s, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let zero = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, x, zero);
        cur.ins().brif(c, p1, &[], p2, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p1);
        let k1 = cur.ins().iconst(types::I64, 8);
        let j1 = cur.ins().iadd(x, k1);
        cur.ins().jump(s, &[j1.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p2);
        // A different def chain — `iadd(isub x, 4), 12` — same class
        // (x, 8). `arghoist`'s shape key can't see through it.
        let four = cur.ins().iconst(types::I64, 4);
        let m1 = cur.ins().isub(x, four);
        let twelve = cur.ins().iconst(types::I64, 12);
        let j2 = cur.ins().iadd(m1, twelve);
        cur.ins().jump(s, &[j2.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(s);
        let one = cur.ins().iconst(types::I64, 1);
        let r = cur.ins().iadd(ps, one);
        cur.ins().return_(&[r]);

        assert_eq!(webmerge(&mut func).0, 1);
        assert!(func.dfg.block_params(s).is_empty());
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("webmerge produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// `k = 0` collapse: one edge passes `x`, the other `iadd x, 0` —
    /// same class, param rebinds to `x` directly (no materialization).
    #[test]
    fn webmerge_zero_delta() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let p1 = func.dfg.make_block();
        let s = func.dfg.make_block();
        for blk in [entry, p1, s] {
            func.layout.append_block(blk);
        }
        let x = func.dfg.append_block_param(entry, types::I64);
        let ps = func.dfg.append_block_param(s, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let zero = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, x, zero);
        cur.ins().brif(c, p1, &[], s, &[x.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p1);
        let k0 = cur.ins().iconst(types::I64, 0);
        let j = cur.ins().iadd(x, k0);
        cur.ins().jump(s, &[j.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(s);
        let one = cur.ins().iconst(types::I64, 1);
        let r = cur.ins().iadd(ps, one);
        cur.ins().return_(&[r]);

        assert_eq!(webmerge(&mut func).0, 1);
        assert!(func.dfg.block_params(s).is_empty());
        // No `x + 0` rematerialization — `ps`'s use bound straight to
        // `x`, so `s` still holds exactly the three original insts.
        assert_eq!(func.layout.block_insts(s).count(), 3);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("webmerge produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// Mismatched deltas — `x + 8` vs `x + 4` — are a real phi; the
    /// param must survive.
    #[test]
    fn webmerge_rejects_mismatched_delta() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let p1 = func.dfg.make_block();
        let p2 = func.dfg.make_block();
        let s = func.dfg.make_block();
        for blk in [entry, p1, p2, s] {
            func.layout.append_block(blk);
        }
        let x = func.dfg.append_block_param(entry, types::I64);
        let ps = func.dfg.append_block_param(s, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let zero = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, x, zero);
        cur.ins().brif(c, p1, &[], p2, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p1);
        let e8 = cur.ins().iconst(types::I64, 8);
        let j1 = cur.ins().iadd(x, e8);
        cur.ins().jump(s, &[j1.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p2);
        let e4 = cur.ins().iconst(types::I64, 4);
        let j2 = cur.ins().iadd(x, e4);
        cur.ins().jump(s, &[j2.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(s);
        let one = cur.ins().iconst(types::I64, 1);
        let r = cur.ins().iadd(ps, one);
        cur.ins().return_(&[r]);

        assert_eq!(webmerge(&mut func).0, 0);
        assert_eq!(func.dfg.block_params(s).len(), 1);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("webmerge produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// Congruent `icmp`s: each edge passes `x + 8 < 5` but the `iadd` and
    /// the `icmp` are cloned per edge — `arghoist` can't fire (operand
    /// values differ) and `affmerge` can't see a compare. The uniform
    /// `N` key re-materializes both insts at `s`'s head.
    #[test]
    fn webmerge_congruent_op() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I8));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let p1 = func.dfg.make_block();
        let p2 = func.dfg.make_block();
        let s = func.dfg.make_block();
        for blk in [entry, p1, p2, s] {
            func.layout.append_block(blk);
        }
        let x = func.dfg.append_block_param(entry, types::I64);
        let ps = func.dfg.append_block_param(s, types::I8);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let zero = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, x, zero);
        cur.ins().brif(c, p1, &[], p2, &[]);

        for p in [p1, p2] {
            let mut cur = FuncCursor::new(&mut func).at_bottom(p);
            let e8 = cur.ins().iconst(types::I64, 8);
            let j = cur.ins().iadd(x, e8);
            let e5 = cur.ins().iconst(types::I64, 5);
            let cmp = cur.ins().icmp(IntCC::UnsignedLessThan, j, e5);
            cur.ins().jump(s, &[cmp.into()]);
        }

        let mut cur = FuncCursor::new(&mut func).at_bottom(s);
        cur.ins().return_(&[ps]);

        assert_eq!(webmerge(&mut func).0, 1);
        assert!(func.dfg.block_params(s).is_empty());
        // Head of `s` = iconst 8, iadd, iconst 5, icmp, then the return.
        assert_eq!(func.layout.block_insts(s).count(), 5);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("webmerge produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// When a class member already dominates the merge — `j` computed in
    /// `entry`, one edge passing it directly, the other passing a clone —
    /// the param rebinds to it with no new insts.
    #[test]
    fn webmerge_member_reuse() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let p1 = func.dfg.make_block();
        let s = func.dfg.make_block();
        for blk in [entry, p1, s] {
            func.layout.append_block(blk);
        }
        let x = func.dfg.append_block_param(entry, types::I64);
        let ps = func.dfg.append_block_param(s, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let e8 = cur.ins().iconst(types::I64, 8);
        let j = cur.ins().iadd(x, e8);
        let c = cur.ins().icmp(IntCC::Equal, x, e8);
        cur.ins().brif(c, p1, &[], s, &[j.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p1);
        let e8b = cur.ins().iconst(types::I64, 8);
        let jb = cur.ins().iadd(x, e8b);
        cur.ins().jump(s, &[jb.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(s);
        let one = cur.ins().iconst(types::I64, 1);
        let r = cur.ins().iadd(ps, one);
        cur.ins().return_(&[r]);

        assert_eq!(webmerge(&mut func).0, 1);
        assert!(func.dfg.block_params(s).is_empty());
        // `ps` rebound to entry's `j` — `s` keeps only iconst+iadd+ret.
        assert_eq!(func.layout.block_insts(s).count(), 3);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("webmerge produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// Sibling-slot congruence: both edges pass `(a, a + 8)` but `a`
    /// differs per edge — no uniform key for slot 1, yet `p1 = p0 + 8`
    /// holds slot-relatively. `paramize` turns each arg-1 key into
    /// `A(P(0), 8)`; `p1` drops and `p0 + 8` materializes at the head.
    #[test]
    fn webmerge_sibling_slot_affine() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let p1 = func.dfg.make_block();
        let p2 = func.dfg.make_block();
        let s = func.dfg.make_block();
        for blk in [entry, p1, p2, s] {
            func.layout.append_block(blk);
        }
        let x = func.dfg.append_block_param(entry, types::I64);
        let s0 = func.dfg.append_block_param(s, types::I64);
        let s1 = func.dfg.append_block_param(s, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let zero = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, x, zero);
        cur.ins().brif(c, p1, &[], p2, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p1);
        let e8 = cur.ins().iconst(types::I64, 8);
        let j1 = cur.ins().iadd(x, e8);
        cur.ins().jump(s, &[x.into(), j1.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p2);
        let three = cur.ins().iconst(types::I64, 3);
        let z = cur.ins().imul(x, three);
        let e8b = cur.ins().iconst(types::I64, 8);
        let j2 = cur.ins().iadd(z, e8b);
        cur.ins().jump(s, &[z.into(), j2.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(s);
        let r = cur.ins().iadd(s0, s1);
        cur.ins().return_(&[r]);

        assert_eq!(webmerge(&mut func).0, 1);
        // `s1` dropped: one param survives, and the head carries the
        // reconstructed `s0 + 8` (iconst + iadd) feeding `iadd s0, s1'`.
        assert_eq!(func.dfg.block_params(s).len(), 1);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("webmerge produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// Param dedup as the degenerate sibling relation: both slots carry
    /// the same value on every edge — each slot paramizes to `P(other)`,
    /// the seeded `pend` resolves one to a real `V` leaf and the merge
    /// picks a survivor.
    #[test]
    fn webmerge_sibling_slot_dedup() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let p1 = func.dfg.make_block();
        let p2 = func.dfg.make_block();
        let s = func.dfg.make_block();
        for blk in [entry, p1, p2, s] {
            func.layout.append_block(blk);
        }
        let x = func.dfg.append_block_param(entry, types::I64);
        let s0 = func.dfg.append_block_param(s, types::I64);
        let s1 = func.dfg.append_block_param(s, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let zero = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, x, zero);
        cur.ins().brif(c, p1, &[], p2, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p1);
        cur.ins().jump(s, &[x.into(), x.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p2);
        let three = cur.ins().iconst(types::I64, 3);
        let z = cur.ins().imul(x, three);
        cur.ins().jump(s, &[z.into(), z.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(s);
        let r = cur.ins().iadd(s0, s1);
        cur.ins().return_(&[r]);

        assert_eq!(webmerge(&mut func).0, 1);
        assert_eq!(func.dfg.block_params(s).len(), 1);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("webmerge produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// Negative: sibling relation differs across edges — edge `p1` has
    /// `arg1 = arg0 + 8` but edge `p2` has `arg1 = arg0 + 4`. The
    /// parameterized keys disagree, nothing merges.
    #[test]
    fn webmerge_sibling_slot_mismatch() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let p1 = func.dfg.make_block();
        let p2 = func.dfg.make_block();
        let s = func.dfg.make_block();
        for blk in [entry, p1, p2, s] {
            func.layout.append_block(blk);
        }
        let x = func.dfg.append_block_param(entry, types::I64);
        let s0 = func.dfg.append_block_param(s, types::I64);
        let s1 = func.dfg.append_block_param(s, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let zero = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, x, zero);
        cur.ins().brif(c, p1, &[], p2, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p1);
        let e8 = cur.ins().iconst(types::I64, 8);
        let j1 = cur.ins().iadd(x, e8);
        cur.ins().jump(s, &[x.into(), j1.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(p2);
        let three = cur.ins().iconst(types::I64, 3);
        let z = cur.ins().imul(x, three);
        let e4 = cur.ins().iconst(types::I64, 4);
        let j2 = cur.ins().iadd(z, e4);
        cur.ins().jump(s, &[z.into(), j2.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(s);
        let r = cur.ins().iadd(s0, s1);
        cur.ins().return_(&[r]);

        assert_eq!(webmerge(&mut func).0, 0);
        assert_eq!(func.dfg.block_params(s).len(), 2);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("webmerge produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// Coinductive merge: a 3-param rotation `(p0,p1,p2) <- (p1,p2,p0)`
    /// on the back-edge. No slot has a uniform key (phase A misses —
    /// `x` vs `p_j` disagree), and no slot's arg keys to a sibling's
    /// arg (phase B misses — the back-edge args are *params*, not
    /// structural functions of them). Under the all-equal hypothesis
    /// every slot's signature is `[V(x), V(rep)]` — a bisimulation —
    /// so `p1`/`p2` drop to `p0`.
    #[test]
    fn webmerge_bisim_rotation() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let s = func.dfg.make_block();
        let latch = func.dfg.make_block();
        let exit = func.dfg.make_block();
        for blk in [entry, s, latch, exit] {
            func.layout.append_block(blk);
        }
        let x = func.dfg.append_block_param(entry, types::I64);
        let p0 = func.dfg.append_block_param(s, types::I64);
        let p1 = func.dfg.append_block_param(s, types::I64);
        let p2 = func.dfg.append_block_param(s, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        cur.ins().jump(s, &[x.into(), x.into(), x.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(s);
        let zero = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, p0, zero);
        cur.ins().brif(c, exit, &[], latch, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(latch);
        cur.ins().jump(s, &[p1.into(), p2.into(), p0.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(exit);
        let r = cur.ins().iadd(p1, p2);
        cur.ins().return_(&[r]);

        assert_eq!(webmerge(&mut func).0, 2);
        assert_eq!(func.dfg.block_params(s).len(), 1);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("webmerge produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// Bisimulation rejects a false hypothesis: same rotation but the
    /// entry edge passes `(x, x, y)` — `p2` starts distinct. Round two
    /// sees `p1`'s back-arg resolve to the `p2` class, splits
    /// `{p0,p1}`, nothing merges.
    #[test]
    fn webmerge_bisim_rejects_divergent() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let s = func.dfg.make_block();
        let latch = func.dfg.make_block();
        let exit = func.dfg.make_block();
        for blk in [entry, s, latch, exit] {
            func.layout.append_block(blk);
        }
        let x = func.dfg.append_block_param(entry, types::I64);
        let y = func.dfg.append_block_param(entry, types::I64);
        let p0 = func.dfg.append_block_param(s, types::I64);
        let p1 = func.dfg.append_block_param(s, types::I64);
        let p2 = func.dfg.append_block_param(s, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        cur.ins().jump(s, &[x.into(), x.into(), y.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(s);
        let zero = cur.ins().iconst(types::I64, 0);
        let c = cur.ins().icmp(IntCC::Equal, p0, zero);
        cur.ins().brif(c, exit, &[], latch, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(latch);
        cur.ins().jump(s, &[p1.into(), p2.into(), p0.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(exit);
        let r = cur.ins().iadd(p1, p2);
        cur.ins().return_(&[r]);

        assert_eq!(webmerge(&mut func).0, 0);
        assert_eq!(func.dfg.block_params(s).len(), 3);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("webmerge produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// Deep alias chains make per-inst `resolve_aliases` calls quadratic:
    /// a K-hop chain walked per arg turns an M-inst block into O(M*K).
    /// `lcse`'s keying resolves every arg, so it must compact first.
    #[test]
    fn lcse_alias_chains_stay_linear() {
        const K: usize = 2000;
        const M: usize = 2000;
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        func.layout.append_block(entry);

        // v_chain[0] -> v_chain[1] -> ... -> root (iconst), then M
        // `iadd chain_head, chain_head` insts that each resolve the full
        // chain through `cse_key`.
        let root = {
            let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
            cur.ins().iconst(types::I64, 1)
        };
        let mut chain = vec![Value::reserved_value(); K];
        for slot in chain.iter_mut().rev() {
            func.dfg.make_invalid_value_for_parser();
            *slot = Value::from_u32(func.dfg.len_values() as u32 - 1);
        }
        for i in (0..K).rev() {
            let src = if i + 1 == K { root } else { chain[i + 1] };
            func.dfg.make_value_alias_for_serialization(src, chain[i]);
            func.dfg.set_alias_type_for_parser(chain[i]);
        }
        for i in 0..K {
            assert_ne!(
                func.dfg.resolve_aliases(chain[i]),
                chain[i],
                "chain[{i}] resolves to itself"
            );
        }
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::clifpeep", "root={root} head={} last={}", chain[0], chain[K - 1])
        };
        let head = chain[0];
        let mut last = root;
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
            for _ in 0..M {
                last = cur.ins().iadd(head, head);
            }
            cur.ins().return_(&[last]);
        }

        let t = std::time::Instant::now();
        lcse(&mut func);
        assert!(
            t.elapsed().as_secs() < 5,
            "lcse took {:?} on {K}-deep alias chain",
            t.elapsed()
        );
    }
}
