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
use cranelift_codegen::entity::packed_option::ReservedValue;
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::condcodes::{CondCode, FloatCC, IntCC};
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, ExternalName, Function, Inst, InstBuilder, InstructionData, LibCall,
    MemFlagsData, Opcode, Type, Value, ValueDef, types,
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
        // `icmp` yields 0/1; `a ^ b` sets no bit clear in both operands.
        InstructionData::IntCompare { .. } => hi = hi.min(1),
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

/// Number of uses of `v` as an inst arg or branch-edge arg.
fn uses_of(func: &Function, v: Value) -> usize {
    let mut c = 0;
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            c += func
                .dfg
                .inst_args(i)
                .iter()
                .filter(|&&a| func.dfg.resolve_aliases(a) == v)
                .count();
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                c += bc
                    .args(&func.dfg.value_lists)
                    .filter(
                        |a| matches!(a, BlockArg::Value(x) if func.dfg.resolve_aliases(*x) == v),
                    )
                    .count();
            }
        }
    }
    c
}

/// `v` as `uextend b` where `b` is an i8 provably in {0,1} — returns `b`,
/// usable as a `select` condition.
fn uext01(func: &Function, v: Value) -> Option<Value> {
    let d = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
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
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
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
    let Some(ci) = f.dfg.value_def(f.dfg.resolve_aliases(c)).inst() else {
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
                        } else if let Some(k) =
                            iconst(func, func.dfg.resolve_aliases(args[0]))
                        {
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
                // A chain intermediate used as an edge arg: a 0/1
                // decided by the edge itself.
                return Some(Pick::K(t as i64));
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
        let tset: FxHashMap<Value, bool> = chain
            .iter()
            .map(|&(x, neg)| (x, (d == 0) != neg))
            .collect();
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
        let bc_mut = &mut dfg.insts[inst].branch_destination_mut(
            &mut dfg.jump_tables,
            &mut dfg.exception_tables,
        )[d];
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
            ValueDef::Result(i, _) => i != pinst && domtree.dominates(i, pinst, &func.layout),
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
                                        i != pinst && domtree.dominates(i, pinst, &func.layout)
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
    loop {
        // Incoming-edge count per block (self-edges included).
        let mut preds: FxHashMap<cranelift_codegen::ir::Block, u32> = FxHashMap::default();
        for b in func.layout.blocks() {
            let Some(lt) = func.layout.last_inst(b) else {
                continue;
            };
            for bc in func.dfg.insts[lt]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                *preds.entry(bc.block(&func.dfg.value_lists)).or_default() += 1;
            }
        }
        let mut fused = false;
        for b in func.layout.blocks().collect::<Vec<_>>() {
            if func.layout.is_cold(b) {
                continue;
            }
            let Some(lt) = func.layout.last_inst(b) else {
                continue;
            };
            if func.dfg.insts[lt].opcode() != Opcode::Jump {
                continue;
            }
            let dests = func.dfg.insts[lt]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .to_vec();
            let pick = dests.iter().find(|bc| {
                let t = bc.block(&func.dfg.value_lists);
                t != b && !func.layout.is_cold(t) && preds.get(&t).copied().unwrap_or(0) == 1
            });
            let Some(bc) = pick else { continue };
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
                continue;
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
            fused = true;
            break;
        }
        if !fused {
            break;
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
pub fn sameargs(func: &mut Function) -> usize {
    let mut n = 0;
    for _ in 0..8 {
        let k = sameargs_round(func);
        if k != 0 {
            n += k;
            continue;
        }
        // Cheap merges exhausted — webmerge's keyed congruence covers
        // phimerge (phase C bisimulation), affparm (phase B sibling
        // slots) and the structural cases none of them see, then the
        // cheap round cascades on the removals.
        let k = webmerge(func).0;
        n += k;
        if k == 0 {
            break;
        }
    }
    if crate::pass_enabled("PLIRON_CSWEEP") {
        n += const_sweep(func);
    }
    n
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
            && let Some(ci) = func.dfg.value_def(func.dfg.resolve_aliases(arg)).inst()
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
pub fn cparm(func: &mut Function) -> usize {
    let mut n = 0;
    for _ in 0..8 {
        let k = cparm_round(func);
        n += k;
        if k == 0 {
            break;
        }
        if crate::pass_enabled("PLIRON_CSWEEP") {
            n += const_sweep(func);
        }
    }
    n
}

/// One `cparm` round (see `cparm`).
fn cparm_round(func: &mut Function) -> usize {
    // Every edge target -> [(inst, dest index)] (see `sameargs_round`).
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
    // Resolved edge args per block (skip malformed arity).
    let mut eargs: FxHashMap<Block, Vec<Vec<Value>>> = FxHashMap::default();
    for (&b, es) in &edges {
        let np = func.dfg.block_params(b).len();
        let mut rows = Vec::with_capacity(es.len());
        let mut ok = true;
        for &(i, d) in es {
            let bc = &func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d];
            let args: Vec<Value> = bc
                .args(&func.dfg.value_lists)
                .map(|a| match a {
                    BlockArg::Value(v) => func.dfg.resolve_aliases(v),
                    _ => {
                        ok = false;
                        Value::reserved_value()
                    }
                })
                .collect();
            if args.len() != np {
                ok = false;
            }
            if !ok {
                break;
            }
            rows.push(args);
        }
        if ok {
            eargs.insert(b, rows);
        }
    }
    // param -> the single value every incoming edge feeds it, modulo
    // self-passthrough edges (the `sameargs` invariant). The flag param
    // reaches the header through a chain of these (`block20`'s fed
    // `block18`'s fed the header's), so constness chases through them.
    let mut uni: FxHashMap<Value, Value> = FxHashMap::default();
    for (&b, rows) in &eargs {
        let ps = func.dfg.block_params(b);
        for (ix, &p) in ps.iter().enumerate() {
            let mut u = None;
            let mut ok = false;
            for r in rows {
                let a = r[ix];
                if a == p {
                    continue;
                }
                match u {
                    None => {
                        u = Some(a);
                        ok = true;
                    }
                    Some(x) if x == a => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                uni.insert(p, u.unwrap());
            }
        }
    }
    let iconst_of = |func: &Function, v: Value| -> Option<(Type, i64)> {
        match func.dfg.value_def(v) {
            ValueDef::Result(i, _)
                if let InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm,
                } = func.dfg.insts[i] =>
            {
                Some((func.dfg.value_type(v), imm.bits()))
            }
            _ => None,
        }
    };
    let mut pmap: FxHashMap<Value, Value> = FxHashMap::default();
    let mut arg_drops: Vec<(Inst, usize, Vec<usize>)> = vec![];
    let mut param_drops: Vec<Value> = vec![];
    // Per-home-block const cache for carried-chain params (see `mids`).
    let mut mid_news: FxHashMap<(Block, Type, i64), Value> = FxHashMap::default();
    for (&b, rows) in &eargs {
        let ps = func.dfg.block_params(b).to_vec();
        let mut news: Vec<(Type, i64, Value)> = vec![];
        let mut repl: Vec<(usize, Value)> = vec![];
        for (ix, &p) in ps.iter().enumerate() {
            let mut k: Option<(Type, i64)> = None;
            let mut mids: Vec<Value> = vec![];
            let mut differs = false;
            for r in rows {
                let mut v = r[ix];
                // Params chased through on the way to the leaf get the
                // folded constant too — their uses (e.g. `select flag`
                // in an interior arm block) must rewrite.
                let mut path: Vec<Value> = vec![];
                let mut seen: FxHashSet<Value> = FxHashSet::default();
                while v != p
                    && let Some(&u) = uni.get(&v)
                    && seen.insert(v)
                {
                    path.push(v);
                    v = u;
                }
                if v == p {
                    // Self-passthrough edge — preserves the carried
                    // constant.
                    continue;
                }
                match iconst_of(func, v) {
                    Some(tk) => {
                        mids.extend(path);
                        match k {
                            None => k = Some(tk),
                            Some(x) if x == tk => {}
                            _ => {
                                differs = true;
                                break;
                            }
                        }
                    }
                    None => {
                        differs = true;
                        break;
                    }
                }
            }
            if differs {
                continue;
            }
            if let Some((ty, imm)) = k {
                let v = match news.iter().find(|&&(t, kk, _)| t == ty && kk == imm) {
                    Some(&(_, _, v)) => v,
                    None => {
                        let v = FuncCursor::new(&mut *func)
                            .at_first_insertion_point(b)
                            .ins()
                            .iconst(ty, imm);
                        news.push((ty, imm, v));
                        v
                    }
                };
                repl.push((ix, v));
                for m in mids {
                    // `m` is a param of some other block on the carried
                    // chain; its uses live there, not under `b` — the
                    // replacement const must be materialized in `m`'s own
                    // home block or uses become non-dominating (alloc's
                    // `Vec::into_boxed_slice` hit this: an iconst in a
                    // sibling arm fed edges on two other arms).
                    let home = match func.dfg.value_def(m) {
                        ValueDef::Param(bm, _) => bm,
                        _ => continue,
                    };
                    let mv = match mid_news.get(&(home, ty, imm)) {
                        Some(&mv) => mv,
                        None => {
                            let mv = FuncCursor::new(&mut *func)
                                .at_first_insertion_point(home)
                                .ins()
                                .iconst(ty, imm);
                            mid_news.insert((home, ty, imm), mv);
                            mv
                        }
                    };
                    pmap.insert(m, mv);
                }
            }
        }
        if repl.is_empty() {
            continue;
        }
        let mut ixs: Vec<usize> = repl.iter().map(|(ix, _)| *ix).collect();
        ixs.sort_unstable_by(|x, y| y.cmp(x));
        for &(i, d) in &edges[&b] {
            arg_drops.push((i, d, ixs.clone()));
        }
        for (ix, v) in repl {
            pmap.insert(ps[ix], v);
            param_drops.push(ps[ix]);
        }
    }
    if param_drops.is_empty() {
        return 0;
    }
    apply_param_drops(func, pmap, arg_drops, param_drops)
}

/// Shared commit for `sameargs_round`/`cparm_round`: flatten aliases,
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
/// materialization in `sameargs_round` leaves `uextend`/`band`/`select`
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
        func.dfg
            .value_def(func.dfg.resolve_aliases(v))
            .inst()
            .is_some_and(|d| {
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

/// Edge-arg key for `sameargs_round`: `V` is an SSA value, `K` a
/// materializable `iconst` — distinct `iconst` defs of the same
/// (ty, imm) count as the same incoming arg. Pseudo-args keep their
/// identity: `TryCallRet(0)` and `TryCallRet(1)` are different incoming
/// values, so they must never compare equal.
#[derive(Clone, Copy, PartialEq)]
enum AKey {
    V(Value),
    K(Type, i64),
    Other(BlockArg),
}

fn sameargs_round(func: &mut Function) -> usize {
    // Every edge target -> [(inst, dest index)] — jump/brif/br_table and
    // try_call exception edges all appear in `branch_destination`.
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
    // param Value -> replacement, drop-index sets, and per-edge arg drops.
    let mut pmap: FxHashMap<Value, Value> = FxHashMap::default();
    let mut arg_drops: Vec<(Inst, usize, Vec<usize>)> = vec![];
    let mut param_drops: Vec<Value> = vec![];
    for (&b, es) in &edges {
        let ps = func.dfg.block_params(b).to_vec();
        if ps.is_empty() || es.is_empty() {
            continue;
        }
        // Resolve every edge's args once, keeping pseudo-args distinct:
        // `TryCallRet(0)` and `TryCallRet(1)` are different incoming
        // values — collapsing them to one "not a Value" marker makes the
        // duplicate-param merge below fuse a call's distinct results.
        // Edge args keyed so distinct `iconst` defs of the same (ty, imm)
        // compare equal — `Chars`-style decoders pass literal tags
        // (`iconst 1` for `Some`) materialized per arm.
        let mut edge_args: Vec<Vec<AKey>> = Vec::with_capacity(es.len());
        let mut uniform = true;
        for &(i, d) in es {
            let bc = &func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d];
            let args: Vec<AKey> = bc
                .args(&func.dfg.value_lists)
                .map(|a| match a {
                    BlockArg::Value(v) => {
                        let v = func.dfg.resolve_aliases(v);
                        match func.dfg.value_def(v).inst() {
                            Some(i)
                                if let InstructionData::UnaryImm {
                                    opcode: Opcode::Iconst,
                                    imm,
                                } = func.dfg.insts[i] =>
                            {
                                AKey::K(func.dfg.value_type(v), imm.bits())
                            }
                            _ => AKey::V(v),
                        }
                    }
                    a => AKey::Other(a),
                })
                .collect();
            if args.len() != ps.len() {
                uniform = false;
                break;
            }
            edge_args.push(args);
        }
        if !uniform {
            continue;
        }
        let mut repl: Vec<(usize, Value)> = vec![];
        let mut news: Vec<(Type, i64, Value)> = vec![];
        for ix in 0..ps.len() {
            let mut same: Option<AKey> = None;
            let mut differs = false;
            for a in &edge_args {
                let k = a[ix];
                // A self-edge passes the param back unchanged — it
                // preserves whatever value the other edges supplied, so
                // it can't break uniformity. (`RangeInclusive`'s niche
                // flag is such a latch-carried constant.)
                if let AKey::V(v) = k
                    && v == ps[ix]
                {
                    continue;
                }
                if same.is_none() {
                    same = Some(k);
                } else if same != Some(k) {
                    differs = true;
                    break;
                }
            }
            let mut found = false;
            if !differs && let Some(same_k) = same {
                match same_k {
                    AKey::V(v) => {
                        // `v` dominates `b` unless it's defined in `b`
                        // itself (a self-edge passing an in-block
                        // def/param).
                        let in_b = match func.dfg.value_def(v) {
                            ValueDef::Result(i, _) => func.layout.inst_block(i) == Some(b),
                            ValueDef::Param(bb, _) => bb == b,
                            _ => true,
                        };
                        if !in_b {
                            if crate::env_set("PLIRON_SA_DEBUG") {
                                eprintln!("sa V {b} param[{ix}] -> {v}");
                            }
                            repl.push((ix, v));
                            found = true;
                        }
                    }
                    AKey::K(ty, imm) => {
                        // Materialize one canonical const at the top of
                        // `b` (dominates every use of the dropped param).
                        let v = match news.iter().find(|&&(t, k, _)| t == ty && k == imm) {
                            Some(&(_, _, v)) => v,
                            None => {
                                let v = FuncCursor::new(&mut *func)
                                    .at_first_insertion_point(b)
                                    .ins()
                                    .iconst(ty, imm);
                                news.push((ty, imm, v));
                                v
                            }
                        };
                        if crate::env_set("PLIRON_SA_DEBUG") {
                            eprintln!("sa K {b} param[{ix}] -> {ty} {imm}");
                        }
                        repl.push((ix, v));
                        found = true;
                    }
                    AKey::Other(_) => {}
                }
            }
            if !found {
                // Duplicate-param merge: an earlier param with the
                // identical arg vector (resolving through drops).
                for jx in 0..ix {
                    if edge_args.iter().all(|a| a[jx] == a[ix]) {
                        let rep = repl
                            .iter()
                            .find(|(k, _)| *k == jx)
                            .map(|(_, v)| *v)
                            .unwrap_or(ps[jx]);
                        repl.push((ix, rep));
                        break;
                    }
                }
            }
        }
        if repl.is_empty() {
            continue;
        }
        let mut ixs: Vec<usize> = repl.iter().map(|(ix, _)| *ix).collect();
        ixs.sort_unstable_by(|x, y| y.cmp(x));
        for &(i, d) in es {
            arg_drops.push((i, d, ixs.clone()));
        }
        for (ix, v) in repl {
            pmap.insert(ps[ix], v);
            param_drops.push(ps[ix]);
        }
    }
    if param_drops.is_empty() {
        return 0;
    }
    if crate::env_set("PLIRON_SA_DEBUG") {
        eprintln!("sameargs drops {} params", param_drops.len());
    }
    apply_param_drops(func, pmap, arg_drops, param_drops)
}

/// Structural key of a pure arg inst: two insts on different edges
/// compute the same value iff opcode, resolved operands and the extra
/// field (icmp's cond, iadd_imm's imm) all match.
fn arg_shape(func: &Function, di: Inst) -> Option<(Opcode, Vec<Value>, u64)> {
    let rv = |v: &Value| func.dfg.resolve_aliases(*v);
    Some(match func.dfg.insts[di] {
        InstructionData::Binary { opcode, args } => (opcode, args.iter().map(rv).collect(), 0),
        InstructionData::Unary { opcode, arg } => (opcode, vec![rv(&arg)], 0),
        InstructionData::IntCompare { opcode, args, cond } => {
            (opcode, args.iter().map(rv).collect(), cond as u64)
        }
        InstructionData::Ternary { opcode, args }
            if opcode == Opcode::Select =>
        {
            (opcode, args.iter().map(rv).collect(), 0)
        }
        _ => return None,
    })
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

/// When every edge supplies `op(x, y)` for a param slot — jumpthread
/// copies `iadd ptr, off` into each pred of a forwarding block — the
/// param can be the op itself, computed once at the top of the target:
/// the per-edge materialization and its regalloc copies disappear
/// entirely (the `iadd v4215, v4300` cloned into seven tokenize preds).
/// Dead arg insts left in preds get swept by `remove_dead_insts`.
pub fn arghoist(func: &mut Function) -> usize {
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
    let mut pmap: FxHashMap<Value, Value> = FxHashMap::default();
    let mut param_drops: Vec<Value> = vec![];
    let mut inserts: Vec<(Block, Inst)> = vec![];
    // Drop indices per edge — all of a block call's drops must apply
    // together, descending: `bc.remove(1)` renumbers index 3 to 2, so
    // per-param entries would chase slots that already moved.
    let mut drop_ixs: FxHashMap<(Inst, usize), Vec<usize>> = FxHashMap::default();
    for (&b, es) in &edges {
        let ps = func.dfg.block_params(b).to_vec();
        if ps.is_empty() {
            continue;
        }
        let arity_ok = es.iter().all(|&(i, d)| {
            func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d]
                .args(&func.dfg.value_lists)
                .len()
                == ps.len()
        });
        if !arity_ok {
            continue;
        }
        for (ix, &p) in ps.iter().enumerate() {
            if pmap.contains_key(&p) {
                continue;
            }
            let mut shape = None;
            let mut ok = true;
            for &(i, d) in es {
                let a = func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d]
                    .args(&func.dfg.value_lists)
                    .nth(ix);
                let Some(BlockArg::Value(v)) = a else {
                    ok = false;
                    break;
                };
                let Some(di) = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst() else {
                    ok = false;
                    break;
                };
                let Some(s) = arg_shape(func, di) else {
                    ok = false;
                    break;
                };
                if !crate::jumpthread::pure_op(func, di) {
                    ok = false;
                    break;
                }
                match shape {
                    None => shape = Some(s),
                    Some(ref t) if *t != s => {
                        ok = false;
                        break;
                    }
                    _ => {}
                }
            }
            let Some((_, ops, _)) = shape.filter(|_| ok) else {
                continue;
            };
            if !ops
                .iter()
                .all(|&o| dominates_entry(func, &dom, o, b))
            {
                continue;
            }
            // Clone the first edge's arg inst into `b`'s head — its
            // operands dominate, so placement is legal and identical on
            // every entry.
            let di = es
                .iter()
                .find_map(|&(i, d)| {
                    match func.dfg.insts[i]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d]
                        .args(&func.dfg.value_lists)
                        .nth(ix)
                    {
                        Some(BlockArg::Value(v)) => {
                            func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()
                        }
                        _ => None,
                    }
                })
                .unwrap();
            let data = func.dfg.insts[di];
            let ty = func.dfg.value_type(func.dfg.first_result(di));
            let ni = func.dfg.make_inst(data);
            func.dfg.make_inst_results(ni, ty);
            inserts.push((b, ni));
            pmap.insert(p, func.dfg.first_result(ni));
            param_drops.push(p);
            for &(i, d) in es {
                drop_ixs.entry((i, d)).or_default().push(ix);
            }
        }
    }
    if param_drops.is_empty() {
        return 0;
    }
    let arg_drops: Vec<(Inst, usize, Vec<usize>)> = drop_ixs
        .into_iter()
        .map(|((i, d), mut ixs)| {
            ixs.sort_unstable_by(|a, b| b.cmp(a));
            (i, d, ixs)
        })
        .collect();
    for (b, ni) in inserts {
        match func.layout.first_inst(b) {
            Some(fi) => func.layout.insert_inst(ni, fi),
            None => func.layout.append_inst(ni, b),
        }
    }
    let n = param_drops.len();
    apply_param_drops(func, pmap, arg_drops, param_drops);
    n
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
pub fn webmerge(func: &mut Function) -> (usize, FxHashMap<Value, Value>) {
    use crate::proots::Key;
    let t0 = std::time::Instant::now();
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(func);
    let dom = cranelift_codegen::dominator_tree::DominatorTree::with_function(func, &cfg);
    let t1 = std::time::Instant::now();
    let nf = crate::proots::web_nf(func);
    let t2 = std::time::Instant::now();
    let canon = crate::proots::canon_map(func);
    let t3 = std::time::Instant::now();
    if std::env::var_os("PLIRON_WM_PROF").is_some() {
        eprintln!(
            "webmerge {}: cfg+dom {:?} web_nf {:?} canon {:?}",
            func.name,
            t1 - t0,
            t2 - t1,
            t3 - t2
        );
    }
    // Keys memoize across args — shared operand trees key once.
    let mut memo: FxHashMap<Value, crate::proots::Key> = FxHashMap::default();
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
    let mut pmap: FxHashMap<Value, Value> = FxHashMap::default();
    let mut param_drops: Vec<Value> = vec![];
    let mut drop_ixs: FxHashMap<(Inst, usize), Vec<usize>> = FxHashMap::default();
    let mut nblk = 0usize;
    let mut tkeys_sum = std::time::Duration::ZERO;
    let mut tblk_sum = std::time::Duration::ZERO;
    // (target block, class key, param to rebind) — emission happens after
    // the scan so `Value`s stay valid and insertions keep operand order.
    let mut inserts: Vec<(Block, Key, Value)> = vec![];
    for (&b, es) in &edges {
        let ps = func.dfg.block_params(b).to_vec();
        if ps.is_empty() {
            continue;
        }
        // Per-edge resolved args: `eargs[e][ix]` for `Value` args,
        // `skeys[e][ix]` for congruence keys — pseudo-args (`TryCallRet`/
        // `TryCallExn`) are `None` in `eargs` but key as `O`, so two
        // slots fed the same pseudo-arg dedup via `P` while the feeding
        // inst in the key keeps distinct `try_call`s distinct.
        let tkeys = std::time::Instant::now();
        let mut eargs: Vec<Vec<Option<Value>>> = Vec::with_capacity(es.len());
        let mut skeys: Vec<Vec<Key>> = Vec::with_capacity(es.len());
        // `shk[e][ix]` — the key's hash, computed once next to the key
        // itself so phase A's `O(slots × edges)` compares only pay a
        // structural walk when hashes actually agree.
        let mut shk: Vec<Vec<u64>> = Vec::with_capacity(es.len());
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
            eargs.push(
                args.iter()
                    .map(|a| a.as_value().map(|v| func.dfg.resolve_aliases(v)))
                    .collect(),
            );
            let row: Vec<Key> = args
                .iter()
                .map(|a| match a {
                    BlockArg::Value(v) => crate::proots::key_of(
                        func,
                        &nf,
                        &canon,
                        &mut memo,
                        func.dfg.resolve_aliases(*v),
                    ),
                    // `(inst, tag, payload)` — same `BlockCall` arg
                    // is the same value, anything else differs.
                    BlockArg::TryCallRet(x) => {
                        Key::O((i.index() as u64) << 32 | 1 << 30 | *x as u64)
                    }
                    BlockArg::TryCallExn(x) => {
                        Key::O((i.index() as u64) << 32 | 2 << 30 | *x as u64)
                    }
                })
                .collect();
            shk.push(
                row.iter()
                    .map(|k| {
                        let mut h = rustc_hash::FxHasher::default();
                        std::hash::Hash::hash(k, &mut h);
                        std::hash::Hasher::finish(&h)
                    })
                    .collect(),
            );
            skeys.push(row);
        }
        if !arity_ok {
            continue;
        }
        let no_phased = std::env::var_os("PLIRON_WM_NOPHASED").is_some();
        let no_phasec = std::env::var_os("PLIRON_WM_NOPHASEC").is_some();
        let no_phaseb = std::env::var_os("PLIRON_WM_NOPHASEB").is_some();
        // `pk[e][i]` — slot `i`'s key on edge `e`, paramized relative to
        // the block's slots (skip = i), computed lazily: most slots
        // either merge at phase A or are never a viable sibling, so an
        // eager `edges × slots` matrix of deep `Key` clones dominated
        // the pass on param-heavy blocks.
        let vmap: FxHashMap<Value, u32> = ps
            .iter()
            .enumerate()
            .map(|(i, &p)| (p, i as u32))
            .collect();
        // `hmaps[e]`: slot-key hash → first slot index, for `paramize`'s
        // subtree probe. `hmemo` memoizes per-node hashes — `Rc` sharing
        // makes most lookups O(1); see `khash_m`.
        let mut hmemo: FxHashMap<*const Key, u64> = FxHashMap::default();
        let hmaps: Vec<FxHashMap<u64, u32>> = skeys
            .iter()
            .map(|row| {
                let mut m = FxHashMap::default();
                for (i, k) in row.iter().enumerate() {
                    m.entry(crate::proots::khash_m(k, &mut hmemo)).or_insert(i as u32);
                }
                m
            })
            .collect();
        // Each cell carries the key's hash alongside — the scan
        // compares keys `O(slots × edges)` times and a failed
        // structural `==` on a 10⁴-node key costs as much as computing
        // the hash once, so comparisons go through `pkh` first. Node
        // counts (`pkn`) stay lazily separate — only the relational
        // oracle's size cap needs them.
        let hmemo = std::cell::RefCell::new(hmemo);
        let pk: Vec<Vec<std::cell::OnceCell<(Key, u64)>>> = (0..es.len())
            .map(|_| (0..ps.len()).map(|_| std::cell::OnceCell::new()).collect())
            .collect();
        let pkn_: Vec<Vec<std::cell::OnceCell<usize>>> = (0..es.len())
            .map(|_| (0..ps.len()).map(|_| std::cell::OnceCell::new()).collect())
            .collect();
        let pkm = |e: usize, i: usize| -> &(Key, u64) {
            pk[e][i].get_or_init(|| {
                let mut hm = hmemo.borrow_mut();
                let k = crate::proots::paramize(
                    &skeys[e][i],
                    &skeys[e],
                    &ps,
                    i,
                    &hmaps[e],
                    &vmap,
                    &mut hm,
                );
                let h = crate::proots::khash_m(&k, &mut hm);
                (k, h)
            })
        };
        let pk_of = |e: usize, i: usize| -> &Key { &pkm(e, i).0 };
        let pkh = |e: usize, i: usize| pkm(e, i).1;
        let pkn =
            |e: usize, i: usize| *pkn_[e][i].get_or_init(|| crate::proots::nodes(pk_of(e, i)));
        // Slot -> merge key pending materialization in this block. Phase-A
        // keys are `P`-free; phase-B candidates keep `P` leaves until
        // `resolve_p` inlines them.
        let mut pend: FxHashMap<usize, Key> = FxHashMap::default();
        let mut cands: Vec<(usize, Key)> = vec![];
        let tblk = std::time::Instant::now();
        let dkeys = tblk - tkeys;
        nblk += 1;
        tkeys_sum += dkeys;
        if std::env::var_os("PLIRON_WM_PROF").is_some() && dkeys.as_micros() > 200 {
            eprintln!(
                "webmerge {} {b}: {} slots {} edges keying {:?}",
                func.name,
                ps.len(),
                es.len(),
                dkeys
            );
        }

        for (ix, &p) in ps.iter().enumerate() {
            if pmap.contains_key(&p) {
                continue;
            }
            let mut class: Option<(u64, &Key)> = None;
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
                let k = &skeys[e][ix];
                match class {
                    None => {
                        class = Some((shk[e][ix], k));
                        uniform = true;
                    }
                    Some((h, t)) if h != shk[e][ix] || t != k => {
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
            if let Some((_, key)) = class.filter(|_| uniform) {
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
            let kjs: Vec<&Key> = es
                .iter()
                .enumerate()
                .map(|(e, _)| pk_of(e, ix))
                .collect();
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
            // are too hot to run on every non-loop block.
            let is_header = es.iter().any(|&(i, _)| {
                func.layout
                    .inst_block(i)
                    .is_some_and(|eb| dom.dominates(b, eb, &func.layout))
            });
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
                // `i` can't explain edge `e` at all.
                let oracle = |e: usize, kj: &Key, i: usize| -> Option<Option<Key>> {
                    // `key_sub`'s algebraic probes each pay a structural
                    // `==` — on 10⁴-node expression keys that's the
                    // pass's dominant cost, and a diff needing keys that
                    // big is never a relation worth materializing.
                    const REL_KEY_CAP: usize = 512;
                    let ki = pk_of(e, i);
                    if pkn(e, i) <= REL_KEY_CAP && pkn(e, ix) <= REL_KEY_CAP
                        && let Some(d) = crate::proots::key_sub(ki, kj, ty, 8)
                            .map(|d| mk(i, Opcode::Isub, d))
                            .or_else(|| {
                                crate::proots::key_sub(kj, ki, ty, 8)
                                    .map(|d| mk(i, Opcode::Iadd, d))
                            })
                    {
                        return Some(Some(d));
                    }
                    // `diff_b`'s param-pair chase reaches where keys
                    // can't: args that are params of the same
                    // predecessor, diffed through *that* block's
                    // in-edges (tokenize's `pos = cur - base` web).
                    // Calling it on arbitrary value pairs is a
                    // quadratic-with-fanout oracle — far too hot for
                    // this inner loop, so it runs only on the
                    // same-block-param shape.
                    if std::env::var_os("PLIRON_WM_NODIFFB").is_some() {
                        return None;
                    }
                    let (Some(ai), Some(aj)) = (eargs[e][i], eargs[e][ix])
                    else {
                        return None;
                    };
                    if !matches!(
                        (func.dfg.value_def(ai), func.dfg.value_def(aj)),
                        (ValueDef::Param(q1, _), ValueDef::Param(q2, _))
                            if q1 == q2
                    ) {
                        return None;
                    }
                    match crate::indvars::diff_b(
                        func, &cfg, ps[i], ps[ix], ai, aj, 8, false,
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
                for i in 0..ps.len() {
                    if i == ix || func.dfg.value_type(ps[i]) != ty {
                        continue;
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
                if std::env::var_os("PLIRON_WEBMERGE_REL").is_some() {
                    eprintln!("webmerge-rel {b} slot{ix} cand {rk:?} hold={hold}");
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
        if std::env::var_os("PLIRON_WM_PROF").is_some() {
            let d = tblk.elapsed();
            if d.as_micros() > 200 {
                eprintln!(
                    "webmerge {} {b}: {} slots {} edges ixl {:?}",
                    func.name,
                    ps.len(),
                    es.len(),
                    d
                );
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
            if std::env::var_os("PLIRON_WEBMERGE_REL").is_some() {
                eprintln!("webmerge-rel {:?} slot{} {:?}", b, ix, k2);
            }
            pend.insert(ix, k2.clone());
            inserts.push((b, k2, p));
            param_drops.push(p);
            for e in 0..es.len() {
                drop_ixs.entry((es[e].0, es[e].1)).or_default().push(ix);
            }
        }

        let dblk = tblk.elapsed();
        tblk_sum += dblk;
        if std::env::var_os("PLIRON_WM_PROF").is_some() && dblk.as_micros() > 200 {
            eprintln!(
                "webmerge {} {b}: {} slots {} edges scan {:?}",
                func.name,
                ps.len(),
                es.len(),
                dblk
            );
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
        let surv: Vec<usize> = (0..ps.len())
            .filter(|&i| {
                !pmap.contains_key(&ps[i])
                    && !param_drops.contains(&ps[i])
                    && skeys.iter().all(|row| !matches!(row[i], Key::O(_)))
            })
            .collect();
        if surv.len() >= 2 {
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
            loop {
                let leaf = |w: Value| -> Value {
                    let mut w = w;
                    while let Some(&c) = pmap.get(&w) {
                        w = c;
                    }
                    vmap
                        .get(&w)
                        .and_then(|&i| cls.get(&(i as usize)))
                        .map(|&l| ps[l])
                        .unwrap_or(w)
                };
                let sigs: FxHashMap<usize, Vec<Key>> = surv
                    .iter()
                    .map(|&i| {
                        (
                            i,
                            skeys
                                .iter()
                                .map(|row| {
                                    crate::proots::key_rep(&row[i], &ps, &leaf)
                                })
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
    }
    if param_drops.is_empty() {
        return (0, pmap);
    }
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
    let n = param_drops.len();
    apply_param_drops(func, pmap.clone(), arg_drops, param_drops);
    if std::env::var_os("PLIRON_WM_PROF").is_some() {
        eprintln!(
            "webmerge {}: total {:?} keying {:?} ixscan {:?} blocks {}",
            func.name,
            t0.elapsed(),
            tkeys_sum,
            tblk_sum,
            nblk
        );
    }
    (n, pmap)
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
        func.dfg.make_inst_results(ni, ty);
        order.push(ni);
        func.dfg.first_result(ni)
    };
    match key {
        Key::V(w) => Some(*w),
        // `P`/`Q` must be gone by here — `resolve_p` ran at scan time;
        // a survivor means a bug, and dropping the merge is safer than
        // emitting a key referencing a phantom slot. `O` is edge-scoped
        // — nothing in-block materializes it.
        Key::P(_) | Key::Q(_) | Key::O(_) => return None,
        Key::K(ty, bits) => Some(new(
            func,
            order,
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm: cranelift_codegen::ir::immediates::Imm64::new(*bits),
            },
            *ty,
        )),
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
            );
            Some(new(
                func,
                order,
                InstructionData::Binary {
                    opcode: Opcode::Iadd,
                    args: [r, c],
                },
                *ty,
            ))
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
                    let k0 = args
                        .iter()
                        .position(|a| matches!(a, Key::K(_, 0)));
                    let comm = matches!(op, Opcode::Iadd | Opcode::Bor | Opcode::Bxor);
                    match (op, k0) {
                        (Opcode::Band, Some(i)) => return Some(vals[i]),
                        (_, Some(1))
                            if comm
                                || matches!(op, Opcode::Isub | Opcode::Ishl | Opcode::Ushr) =>
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
            Some(new(func, order, data, *ty))
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
                    let Some(di) = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()
                    else {
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

/// Merge same-block param pairs that are equal by coinduction: the edge
/// args agree outright on non-recursive edges and are congruent — same
/// pure op applied to assumed-equal args — on recursive ones. This is the
/// mutually-inductive case sameargs' literal equality can't see: the
/// vectorizer's separate c-load/c-store address streams stay equal iff
/// each other's params do (`p' = q' = p + 64`), so no pair proves first.
///
/// Partition refinement: start with every same-typed param pair pending,
/// drop any pair an edge disproves under the pending assumption, repeat
/// to fixpoint. Surviving pairs are coinductively sound — assume all of
/// them equal at block entry, congruent ops preserve it along every edge.
/// The later param drops to the earlier, as in sameargs.
pub fn phimerge(func: &mut Function) -> usize {
    // Edge target -> [(inst, dest index)].
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
    // Candidate pairs (pi < pj) per block: same type, uniform edge arity.
    let mut pending: Vec<(Block, usize, usize)> = Vec::new();
    for (&b, es) in &edges {
        let ps = func.dfg.block_params(b);
        if ps.len() < 2 || ps.len() > 24 {
            continue;
        }
        let arity_ok = es.iter().all(|&(i, d)| {
            func.dfg.insts[i].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                [d]
                .args(&func.dfg.value_lists)
                .len()
                == ps.len()
        });
        if !arity_ok {
            continue;
        }
        for ix in 0..ps.len() {
            for jx in ix + 1..ps.len() {
                if func.dfg.value_type(ps[ix]) == func.dfg.value_type(ps[jx]) {
                    pending.push((b, ix, jx));
                }
            }
        }
    }
    if pending.is_empty() {
        return 0;
    }
    // uf rep for the pending-pair assumption: pi -> pi, pj -> pi.
    let mut uf: FxHashMap<Value, Value> = FxHashMap::default();
    let find = |uf: &FxHashMap<Value, Value>, mut v: Value| {
        while let Some(&p) = uf.get(&v) {
            if p == v {
                break;
            }
            v = p;
        }
        v
    };
    let mut alive = vec![true; pending.len()];
    loop {
        uf.clear();
        for (k, &(b, ix, jx)) in pending.iter().enumerate() {
            if !alive[k] {
                continue;
            }
            let ps = func.dfg.block_params(b);
            uf.entry(ps[ix]).or_insert(ps[ix]);
            let r = find(&uf, ps[ix]);
            uf.insert(ps[jx], r);
        }
        let mut changed = false;
        for (k, &(b, ix, jx)) in pending.iter().enumerate() {
            if !alive[k] {
                continue;
            }
            let ok = edges[&b].iter().all(|&(i, d)| {
                let bc = &func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d];
                let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                match (args.get(ix), args.get(jx)) {
                    (Some(&BlockArg::Value(x)), Some(&BlockArg::Value(y))) => {
                        phi_cong(func, x, y, &uf, &find, 4)
                    }
                    _ => false,
                }
            });
            if !ok {
                alive[k] = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // Survivors -> pmap/param_drops/arg_drops, grouped per block (later
    // slots first so earlier indices stay valid).
    let mut pmap: FxHashMap<Value, Value> = FxHashMap::default();
    let mut arg_drops: Vec<(Inst, usize, Vec<usize>)> = vec![];
    let mut param_drops: Vec<Value> = vec![];
    let mut per_block: FxHashMap<Block, Vec<(usize, usize)>> = FxHashMap::default();
    for (k, &(b, ix, jx)) in pending.iter().enumerate() {
        if alive[k] {
            per_block.entry(b).or_default().push((ix, jx));
        }
    }
    for (b, mut prs) in per_block {
        let ps = func.dfg.block_params(b).to_vec();
        prs.sort_unstable_by_key(|&(ix, jx)| (ix, jx));
        let mut drops: Vec<usize> = Vec::new();
        for (ix, jx) in prs {
            // pj may already be mapped through an earlier merge.
            let t = pmap.get(&ps[ix]).copied().unwrap_or(ps[ix]);
            pmap.insert(ps[jx], t);
            drops.push(jx);
        }
        drops.sort_unstable_by(|x, y| y.cmp(x));
        drops.dedup();
        for &(i, d) in &edges[&b] {
            arg_drops.push((i, d, drops.clone()));
        }
        for &jx in &drops {
            param_drops.push(ps[jx]);
        }
    }
    if param_drops.is_empty() {
        return 0;
    }
    apply_param_drops(func, pmap, arg_drops, param_drops)
}

/// A block param that equals a sibling param plus a constant on every
/// in-edge is redundant: `p_j = p_i + k`. Replace `p_j`'s uses with an
/// `iadd p_i, k` at the block head and drop the slot from every edge —
/// phimerge's `k = 0` special case, but with an offset. ipnsort's
/// branchless scatter carries `dest = src - 8` through the loop as a
/// param; as an `iadd` the constant offset is visible to `root`'s
/// location keys, so the swap's store and the next element's load come
/// out provably disjoint instead of forcing a reload.
pub fn affparm(func: &mut Function) -> usize {
    // Edge target -> [(inst, dest index)], as phimerge builds it.
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
    if edges.is_empty() {
        return 0;
    }
    // `concrete` peels to exact (base, offset) equalities: equal roots at
    // offsets `k` apart prove the integer relation `arg_j = arg_i + k`
    // regardless of what the unresolved base is.
    let ps = crate::loadfwd::param_src(func);
    let rt = |func: &Function, v: Value| crate::loadfwd::concrete(func, &ps, v);
    // Per block: dropped param indices plus (p_j, base param, k) triples.
    let mut rels: Vec<(Block, Vec<usize>, Vec<(Value, Value, i64, bool)>)> = Vec::new();
    for (&b, es) in &edges {
        let params = func.dfg.block_params(b);
        if params.len() < 2 || params.len() > 24 {
            continue;
        }
        // Per-edge args as plain values; a non-Value arg or arity mismatch
        // disqualifies the block.
        let mut eargs: Vec<Vec<Value>> = Vec::with_capacity(es.len());
        for &(i, d) in es {
            let bc = &func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d];
            let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
            if args.len() != params.len() {
                eargs.clear();
                break;
            }
            let mut vs = Vec::with_capacity(args.len());
            for a in args {
                let BlockArg::Value(v) = a else {
                    vs.clear();
                    break;
                };
                vs.push(v);
            }
            if vs.len() != params.len() {
                eargs.clear();
                break;
            }
            eargs.push(vs);
        }
        if eargs.len() != es.len() {
            continue;
        }
        // Params used as a load/store address inside `b`: when the
        // affine relation holds both ways, keeping such a param alive
        // as the source lets the emitted `iadd` fuse with its memory
        // op as a post-index access (`[x], #k` on aarch64). Dropping
        // the base instead buries the load as `[x, -k]` plus a
        // separate add.
        let mut membase: FxHashSet<usize> = FxHashSet::default();
        for i in func.layout.block_insts(b) {
            let a = match func.dfg.insts[i] {
                InstructionData::Load { arg, .. }
                | InstructionData::LoadNoOffset { arg, .. } => Some(arg),
                InstructionData::Store { args, .. }
                | InstructionData::StoreNoOffset { args, .. } => Some(args[1]),
                _ => None,
            };
            if let Some(a) = a {
                let a = func.dfg.resolve_aliases(a);
                for (ix, &p) in params.iter().enumerate() {
                    if p == a {
                        membase.insert(ix);
                    }
                }
            }
        }
        // `concrete` results cached per arg value: params^2 x edges root
        // calls otherwise dominate compile time in param-heavy fns.
        let mut rc: FxHashMap<Value, (crate::loadfwd::Root, i64)> = FxHashMap::default();
        let mut drops: Vec<usize> = Vec::new();
        let mut triples: Vec<(Value, Value, i64, bool)> = Vec::new();
        'jx: for jx in 0..params.len() {
            if drops.contains(&jx) {
                continue;
            }
            let pj = params[jx];
            let ty = func.dfg.value_type(pj);
            if !ty.is_int() || ty.is_vector() {
                continue;
            }
            for ix in 0..params.len() {
                // Sources are never themselves dropped: a mutual pair
                // (p_a = p_b + 8, p_b = p_a - 8) would otherwise produce
                // cyclic replacements, and a chain (p3 = p2 + k with p2
                // already dropped) folds to the surviving source directly
                // since the composed relation is detected the same way.
                if ix == jx || func.dfg.value_type(params[ix]) != ty || drops.contains(&ix) {
                    continue;
                }
                let mut dk = None;
                let ok = eargs.iter().all(|args| {
                    let (rj, oj) = *rc.entry(args[jx]).or_insert_with(|| rt(func, args[jx]));
                    let (ri, oi) = *rc.entry(args[ix]).or_insert_with(|| rt(func, args[ix]));
                    if rj != ri {
                        return false;
                    }
                    let d = oj.wrapping_sub(oi);
                    match dk {
                        None => {
                            dk = Some(d);
                            true
                        }
                        Some(k) => k == d,
                    }
                });
                if ok && let Some(k) = dk {
                    // Both directions of `p_j = p_i + k` are provable
                    // here. When p_j is a memory base and p_i is not,
                    // drop p_i instead and derive it late: the emitted
                    // `iadd p_j, -k` then sits right after the load on
                    // p_j and fuses into a post-index access.
                    if k != 0 && membase.contains(&jx) && !membase.contains(&ix) {
                        drops.push(ix);
                        triples.push((params[ix], pj, k.wrapping_neg(), true));
                    } else {
                        drops.push(jx);
                        triples.push((pj, params[ix], k, false));
                    }
                    continue 'jx;
                }
            }
        }
        if !drops.is_empty() {
            rels.push((b, drops, triples));
        }
    }
    if rels.is_empty() {
        return 0;
    }
    let mut pmap: FxHashMap<Value, Value> = FxHashMap::default();
    let mut arg_drops: Vec<(Inst, usize, Vec<usize>)> = Vec::new();
    let mut param_drops: Vec<Value> = Vec::new();
    for (b, mut drops, triples) in rels {
        let params = func.dfg.block_params(b).to_vec();
        for (pj, pi, k, late) in triples {
            let w = if k == 0 {
                pi
            } else {
                // Post-index fusion needs `load base` before
                // `iadd base, k` in program order: when keeping the
                // memory-base param, emit the derived add just ahead of
                // the terminator. Only legal when the dropped param has
                // no in-block uses before the terminator itself.
                let last = func.layout.last_inst(b);
                let early = !late
                    || func.layout.block_insts(b).any(|i| {
                        Some(i) != last
                            && func
                                .dfg
                                .inst_values(i)
                                .any(|v| func.dfg.resolve_aliases(v) == pj)
                    });
                if early {
                    FuncCursor::new(func)
                        .at_first_insertion_point(b)
                        .ins()
                        .iadd_imm_s(pi, k)
                } else {
                    FuncCursor::new(func)
                        .at_inst(last.unwrap())
                        .ins()
                        .iadd_imm_s(pi, k)
                }
            };
            pmap.insert(pj, w);
        }
        drops.sort_unstable_by(|x, y| y.cmp(x));
        drops.dedup();
        for &(i, d) in &edges[&b] {
            arg_drops.push((i, d, drops.clone()));
        }
        for &jx in &drops {
            param_drops.push(params[jx]);
        }
    }
    apply_param_drops(func, pmap, arg_drops, param_drops)
}

/// Congruence under the pending-param assumption `uf`: resolved-equal,
/// uf-equal, same constant, or same pure inst shape applied to
/// pointwise-congruent args.
fn phi_cong(
    func: &Function,
    x: Value,
    y: Value,
    uf: &FxHashMap<Value, Value>,
    find: &dyn Fn(&FxHashMap<Value, Value>, Value) -> Value,
    depth: u32,
) -> bool {
    let (x, y) = (func.dfg.resolve_aliases(x), func.dfg.resolve_aliases(y));
    if x == y || find(uf, x) == find(uf, y) {
        return true;
    }
    let (Some(di), Some(dj)) = (func.dfg.value_def(x).inst(), func.dfg.value_def(y).inst()) else {
        return false;
    };
    if func.dfg.value_type(x) != func.dfg.value_type(y) || depth == 0 {
        return false;
    }
    let c = |x: Value, y: Value, d: u32| phi_cong(func, x, y, uf, find, d - 1);
    match (func.dfg.insts[di], func.dfg.insts[dj]) {
        (
            InstructionData::Unary {
                opcode: o1,
                arg: a1,
            },
            InstructionData::Unary {
                opcode: o2,
                arg: a2,
            },
        ) => o1 == o2 && c(a1, a2, depth),
        (
            InstructionData::UnaryImm {
                opcode: o1,
                imm: i1,
            },
            InstructionData::UnaryImm {
                opcode: o2,
                imm: i2,
            },
        ) => o1 == o2 && i1 == i2,
        (
            InstructionData::Binary {
                opcode: o1,
                args: s1,
            },
            InstructionData::Binary {
                opcode: o2,
                args: s2,
            },
        ) => {
            o1 == o2
                && ((c(s1[0], s2[0], depth) && c(s1[1], s2[1], depth))
                    || (phi_comm(o1) && c(s1[0], s2[1], depth) && c(s1[1], s2[0], depth)))
        }
        (
            InstructionData::BinaryImm8 {
                opcode: o1,
                arg: a1,
                imm: m1,
            },
            InstructionData::BinaryImm8 {
                opcode: o2,
                arg: a2,
                imm: m2,
            },
        ) => o1 == o2 && m1 == m2 && c(a1, a2, depth),
        (
            InstructionData::IntCompare {
                cond: c1, args: s1, ..
            },
            InstructionData::IntCompare {
                cond: c2, args: s2, ..
            },
        ) => {
            (c1 == c2 && c(s1[0], s2[0], depth) && c(s1[1], s2[1], depth))
                || (c1 == c2.swap_args() && c(s1[0], s2[1], depth) && c(s1[1], s2[0], depth))
        }
        (
            InstructionData::FloatCompare {
                cond: c1, args: s1, ..
            },
            InstructionData::FloatCompare {
                cond: c2, args: s2, ..
            },
        ) => {
            (c1 == c2 && c(s1[0], s2[0], depth) && c(s1[1], s2[1], depth))
                || (c1 == c2.swap_args() && c(s1[0], s2[1], depth) && c(s1[1], s2[0], depth))
        }
        (
            InstructionData::Ternary {
                opcode: o1,
                args: s1,
            },
            InstructionData::Ternary {
                opcode: o2,
                args: s2,
            },
        ) => o1 == o2 && c(s1[0], s2[0], depth) && c(s1[1], s2[1], depth) && c(s1[2], s2[2], depth),
        _ => false,
    }
}

fn phi_comm(op: Opcode) -> bool {
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
                Some(SgnFold::Bit { b: sign, set: false })
            }
        }
        IntCC::SignedGreaterThan if k == all => {
            if sign_known_clear(func, x, all) {
                Some(SgnFold::Const(true))
            } else {
                Some(SgnFold::Bit { b: sign, set: false })
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
            if uses_of(func, r) != 0 {
                continue;
            }
            for dd in func
                .dfg
                .inst_args(d)
                .iter()
                .filter_map(|&a| func.dfg.value_def(func.dfg.resolve_aliases(a)).inst())
            {
                if seen.insert(dd) && func.dfg.insts[dd].opcode() == Opcode::Iconst {
                    wl.push(dd);
                }
            }
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
                    let Some(d) = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst() else {
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
    if !pos.func.dfg.mem_flags[flags].notrap() {
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
            arg: a2, flags: f2, ..
        } = pos.func.dfg.insts[i]
        else {
            continue;
        };
        if !pos.func.dfg.mem_flags[f2].notrap() {
            continue;
        }
        let Some(d2) = pos
            .func
            .dfg
            .value_def(pos.func.dfg.resolve_aliases(a2))
            .inst()
        else {
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
    let Some(((pa2, offa), (pb2, offb))) = arm(&mut *pos, pa).zip(arm(&mut *pos, pb))
    else {
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
            .value_def(pos.func.dfg.resolve_aliases(match pos.func.dfg.insts[l] {
                InstructionData::Load { arg, .. } => arg,
                _ => unreachable!(),
            }))
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
        let (tv, fv) = (
            if x == pa { la } else { lb },
            if y == pa { la } else { lb },
        );
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
            if let InstructionData::Load { arg, flags, offset, .. } =
                pos.func.dfg.insts[inst]
                && loadsel_pair(&mut pos, inst, arg, flags, offset)
            {
                n += 1;
            }
        }
    }
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
    let mut pos = FuncCursor::new(func);
    while pos.next_block().is_some() {
        while let Some(inst) = pos.next_inst() {
            if let InstructionData::Load { arg, flags, offset, .. } =
                pos.func.dfg.insts[inst]
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
                    && let Some(cd) = pos
                        .func
                        .dfg
                        .value_def(pos.func.dfg.resolve_aliases(c))
                        .inst()
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
                if let Some(d) = x.and_then(|x| {
                    pos.func
                        .dfg
                        .value_def(pos.func.dfg.resolve_aliases(x))
                        .inst()
                }) && let InstructionData::Unary {
                    opcode: Opcode::Uextend,
                    arg: c,
                } = pos.func.dfg.insts[d]
                    && let Some(cd) = pos
                        .func
                        .dfg
                        .value_def(pos.func.dfg.resolve_aliases(c))
                        .inst()
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
                        && pos
                            .func
                            .dfg
                            .value_def(pos.func.dfg.resolve_aliases(x))
                            .inst()
                            .is_some_and(|d| {
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
                                    if inv {
                                        IntCC::Equal
                                    } else {
                                        IntCC::NotEqual
                                    },
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
                        IntCC::UnsignedGreaterThan => {
                            Some(IntCC::UnsignedGreaterThanOrEqual)
                        }
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
    // Region collapse after `brif_not` has normalised `brif` args to the
    // underlying conditions.
    if crate::pass_enabled("PLIRON_SELREGION") {
        n += sel_regions(func);
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
        let Some(ci) = func.dfg.value_def(func.dfg.resolve_aliases(arg)).inst() else {
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
            if preds.len() == 1 {
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
    let Some(i) = pos
        .func
        .dfg
        .value_def(pos.func.dfg.resolve_aliases(x))
        .inst()
    else {
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
            IntCC::UnsignedLessThan => {
                hi = Some(hi.map_or(k as u128, |s| s.min(k as u128)))
            }
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
                } if iconst(pos.func, args[2]) == Some(0) => {
                    rgpair(pos.func, args[0], args[1])
                }
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
    let k = k & if lb == 8 { u64::MAX } else { (1u64 << (lb * 8)) - 1 };
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
    // i64 only: the motivating pattern is the peeled pointer/countdown test.
    // Narrower widths are correct in isolation but a `b-9` whitespace-switch
    // fold in strwork miscompiled when shifted at i8 — untraced downstream
    // interaction, no perf value.
    if !matches!(cc0, IntCC::Equal | IntCC::NotEqual) || ty != types::I64 {
        return false;
    }
    // `isub` must be `x - c` — `c - x` doesn't shift.
    let issub = |v: Value| {
        let i = pos
            .func
            .dfg
            .value_def(pos.func.dfg.resolve_aliases(v))
            .inst()?;
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
        if k > 0 && crate::env_set("PLIRON_STATS") {
            eprintln!("ovkill {k} {}", func.name);
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
        InstructionData::Ternary { opcode, args } => Some(CseKey::T(
            opcode,
            t,
            r(args[0]),
            r(args[1]),
            r(args[2]),
        )),
        _ => None,
    }
}

/// Per-block CSE with the shared keying: later insts in a block alias
/// the earliest equivalent def, which dominates them.
pub fn lcse(func: &mut Function) -> usize {
    let key = cse_key;
    type Seen = FxHashMap<CseKey, Value>;
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
                let (x, y) = (
                    func.dfg.resolve_aliases(x),
                    func.dfg.resolve_aliases(y),
                );
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
    let memoff = |func: &mut Function, v: &mut Value, off: &mut i64, seen: &mut Seen| -> usize {
        if !crate::pass_enabled("PLIRON_MEMOFF") {
            return 0;
        }
        let mut depth = 0;
        loop {
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
            if uses_of(func, a) != 1 {
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
            depth += 1;
        }
        depth
    };
    let mut n = 0;
    let mut seen: Seen = FxHashMap::default();
    for b in func.layout.blocks().collect::<Vec<_>>() {
        seen.clear();
        let mut dead = Vec::new();
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            let data = func.dfg.insts[i];
            match data {
                InstructionData::Load { arg, offset, .. } => {
                    let mut a = arg;
                    let mut off = i64::from(offset);
                    if memoff(func, &mut a, &mut off, &mut seen) > 0 {
                        if let InstructionData::Load { arg, offset, .. } = &mut func.dfg.insts[i] {
                            *arg = a;
                            *offset = (off as i32).into();
                        }
                        n += 1;
                        continue;
                    }
                }
                InstructionData::Store { args, offset, .. } => {
                    let mut a = args[1];
                    let mut off = i64::from(offset);
                    if memoff(func, &mut a, &mut off, &mut seen) > 0 {
                        if let InstructionData::Store { args, offset, .. } = &mut func.dfg.insts[i]
                        {
                            args[1] = a;
                            *offset = (off as i32).into();
                        }
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
                        let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
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
                    let mut neg = |v: Value, seen: &mut Seen| {
                        if let Some(x) = negdef(func, v) {
                            return Some(x);
                        }
                        // Peel a single-use `imul (neg a), y` — rewriting
                        // the mul to `a * y` changes its value, so no other
                        // use may observe it.
                        let mi = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
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
                        if uses_of(func, func.dfg.first_result(mi)) != 1 {
                            return None;
                        }
                        untrack(func, seen, mi);
                        if let InstructionData::Binary { args, .. } = &mut func.dfg.insts[mi] {
                            args[ix] = inner;
                        }
                        Some(func.dfg.first_result(mi))
                    };
                    let sub = if let Some(x) = neg(ra, &mut seen) {
                        Some([rb, x])
                    } else {
                        neg(rb, &mut seen).map(|x| [ra, x])
                    };
                    if let Some(args) = sub {
                        func.dfg.insts[i] = InstructionData::Binary {
                            opcode: Opcode::Isub,
                            args,
                        };
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
                        let d = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
                        if let InstructionData::Binary {
                            opcode: Opcode::Isub,
                            args: [e, x],
                        } = func.dfg.insts[d]
                        {
                            Some((
                                func.dfg.resolve_aliases(e),
                                func.dfg.resolve_aliases(x),
                            ))
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
                func.dfg.clear_results(i);
                func.dfg.change_to_alias(res, x);
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
                    func.dfg.clear_results(i);
                    func.dfg.change_to_alias(res, *e.get());
                    dead.push(i);
                    n += 1;
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(res);
                }
            }
        }
        for i in dead {
            func.layout.remove_inst(i);
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
                .all(|&(_, st)| domtree.dominates(st, l, &func.layout))
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
                let (sz, ty) = chunks
                    .iter()
                    .copied()
                    .find(|&(sz, _)| sz <= rem)
                    .unwrap();
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
        FuncCursor::new(&mut func).at_bottom(entry).ins().return_(&[r]);

        assert_eq!(memcall(&mut func), 1);
        let insts: Vec<Inst> = func.layout.block_insts(entry).collect();
        assert_eq!(
            insts
                .iter()
                .map(|&i| func.dfg.insts[i].opcode())
                .collect::<Vec<_>>(),
            [
                Opcode::Iconst,
                Opcode::Load,
                Opcode::Store,
                Opcode::Return
            ]
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

    /// Loop header params `p, q` fed `(x, x)` on entry and
    /// `(iadd p, 8)`/`(iadd q, 8)` on the back edge are equal by
    /// coinduction — literal-equality sameargs can't merge either without
    /// the other, so phimerge's partition refinement collapses them.
    #[test]
    fn phimerge_inductive_param_pair() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let head = func.dfg.make_block();
        let exit = func.dfg.make_block();
        for blk in [entry, head, exit] {
            func.layout.append_block(blk);
        }
        let p = func.dfg.append_block_param(head, types::I64);
        let q = func.dfg.append_block_param(head, types::I64);
        let n = func.dfg.append_block_param(head, types::I64);

        let x = func.dfg.append_block_param(entry, types::I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        cur.ins().jump(head, &[x.into(), x.into(), x.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(head);
        let eight = cur.ins().iconst(types::I64, 8);
        let p2 = cur.ins().iadd(p, eight);
        let q2 = cur.ins().iadd(q, eight);
        let one = cur.ins().iconst(types::I64, 1);
        let n2 = cur.ins().isub(n, one);
        let z = cur.ins().iconst(types::I64, 0);
        let done = cur.ins().icmp(IntCC::Equal, n2, z);
        cur.ins().brif(
            done,
            exit,
            &[p2.into()],
            head,
            &[p2.into(), q2.into(), n2.into()],
        );

        let r = func.dfg.append_block_param(exit, types::I64);
        FuncCursor::new(&mut func)
            .at_bottom(exit)
            .ins()
            .return_(&[r]);

        assert_eq!(phimerge(&mut func), 1);
        assert_eq!(func.dfg.block_params(head).len(), 2);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("phimerge produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// Head params `p, q` fed `(x, x + 8)` on entry and `(p + 16,
    /// p + 24)` on the back edge: `q = p + 8` holds on every edge, so
    /// affparm drops `q` and its uses become `iadd p, 8` — the collapse
    /// that lets loadfwd prove the ipnsort scatter store disjoint from
    /// the next element's load.
    #[test]
    fn affparm_collapses_offset_param() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let head = func.dfg.make_block();
        let exit = func.dfg.make_block();
        for blk in [entry, head, exit] {
            func.layout.append_block(blk);
        }
        let p = func.dfg.append_block_param(head, types::I64);
        let q = func.dfg.append_block_param(head, types::I64);

        let x = func.dfg.append_block_param(entry, types::I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let eight = cur.ins().iconst(types::I64, 8);
        let xp8 = cur.ins().iadd(x, eight);
        cur.ins().jump(head, &[x.into(), xp8.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(head);
        let c = cur.ins().icmp(IntCC::UnsignedLessThan, q, p);
        let p2 = cur.ins().iadd_imm_s(p, 16);
        let q2 = cur.ins().iadd_imm_s(p2, 8);
        cur.ins()
            .brif(c, exit, &[q.into()], head, &[p2.into(), q2.into()]);

        let r = func.dfg.append_block_param(exit, types::I64);
        FuncCursor::new(&mut func)
            .at_bottom(exit)
            .ins()
            .return_(&[r]);

        assert_eq!(affparm(&mut func), 1);
        assert_eq!(func.dfg.block_params(head), &[q]);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("affparm produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// `q = p + 8` and `p = q - 8` are the same relation: both direction
    /// checks pass, so a naive per-param drop replaces `p` with `q - 8`
    /// and `q` with `p + 8` — cyclic SSA (`w = iadd v, -8` / `v = iadd
    /// w, 8`) that sent `root`'s iadd peel into unbounded recursion.
    /// Sources are never dropped, so exactly one survives.
    #[test]
    fn affparm_mutual_pair_keeps_one_source() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let head = func.dfg.make_block();
        let exit = func.dfg.make_block();
        for blk in [entry, head, exit] {
            func.layout.append_block(blk);
        }
        let p = func.dfg.append_block_param(head, types::I64);
        let q = func.dfg.append_block_param(head, types::I64);

        let x = func.dfg.append_block_param(entry, types::I64);
        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let xp8 = cur.ins().iadd_imm_s(x, 8);
        cur.ins().jump(head, &[x.into(), xp8.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(head);
        let c = cur.ins().icmp(IntCC::UnsignedLessThan, q, p);
        let p2 = cur.ins().iadd_imm_s(p, 16);
        let q2 = cur.ins().iadd_imm_s(p2, 8);
        cur.ins()
            .brif(c, exit, &[q.into()], head, &[p2.into(), q2.into()]);

        let r = func.dfg.append_block_param(exit, types::I64);
        FuncCursor::new(&mut func)
            .at_bottom(exit)
            .ins()
            .return_(&[r]);

        assert_eq!(affparm(&mut func), 1);
        assert_eq!(func.dfg.block_params(head).len(), 1);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("affparm produced invalid IR: {e}\n{}", func.display());
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
        let mut func = loadsel_func(MemFlagsData::new().with_notrap(), true);
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
            let mut func = loadsel_func(flags, sibling);
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
                            func.dfg.insts[func
                                .dfg
                                .value_def(func.dfg.resolve_aliases(arg))
                                .inst()
                                .unwrap()]
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

    /// `select c, pa, pb; *sel` ×2 (the second with swapped arms), sum.
    fn loadsel_func(flags: MemFlagsData, sibling: bool) -> Function {
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
            cur.ins().load(types::I64, flags, s2, 0)
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
}
