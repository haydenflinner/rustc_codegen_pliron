//! `sdiv`/`udiv`/`srem`/`urem` by a nonzero integer constant expanded to the
//! Granlund–Montgomery magic-multiply sequences (LLVM's
//! `DivisionByConstantInfo`, from Hacker's Delight ch. 10): a hardware
//! `sdiv`/`udiv` costs ~10–20 cycles and, being `can_trap`, it also stops
//! loopvec — a `x % k` predicate loop (iterw's `x % 3 != 0` filter) is
//! unvectorizable until the remainder is pure lane-wise math.
//!
//! Scalar i8..i64 is covered (the div/rem opcodes are scalar-only in CLIF).
//! The `mulhi` helper is also exported for loopvec, which emits *vector*
//! `{s,u}mulhi` for ≤32-bit lanes as widen+imul+uzp2 — `smull`/`smull2`
//! +`uzp2` on aarch64 — since no target lowers vector mulhi directly.
//! Unsigned i128 is covered too: `umulhi.i128` has no isa lowering, but the
//! 128×128→256 high half decomposes into four ≤64×≤64 `imul.i128` partial
//! products — ~30 scalar insts vs a `__udivti3`/`__umodti3` libcall per
//! division. Signed i128 still takes the libcall: `smagics` needs a signed
//! high-half the same decomposition does not yield directly.
//!
//! `x % d` rewrites as `x - (x/d)*d` once `x/d` is expanded; powers of two
//! use the `ushr`/`band` (unsigned) or sign-fixup `sshr` (signed) forms.
//!
//! `PLIRON_DIVCONST=0` disables; `PLIRON_DIVCONST_DEBUG` logs expansions.

use crate::clifpeep;
use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    ConstantData, Endianness, Function, InstBuilder, InstructionData, MemFlagsData, Opcode, Type,
    Value, types,
};
use std::rc::Rc;

/// `v`'s low `w` bits.
fn mask(w: u32) -> u64 {
    u64::MAX >> (64 - w)
}

/// `v` sign-extended from `w` bits.
fn sext(v: u64, w: u32) -> i64 {
    ((v << (64 - w)) as i64) >> (64 - w)
}

/// `(magic, shift)` for signed division by a positive divisor `d` (`w`-bit
/// pattern), per LLVM's `SignedDivisionByConstantInfo::get`. Caller
/// guarantees `d >= 2` and `d` not a power of two (those take other
/// paths); negative divisors are handled by dividing by `|d|` and negating
/// the quotient. All arithmetic wraps at `w` bits like the W-bit APInts.
fn magic_signed(d: u64, w: u32) -> Option<(u64, u32)> {
    let m = mask(w);
    let ad = d;
    let p_min = 1u64 << (w - 1); // 2^(w-1)
    let t = p_min;
    let anc = t.wrapping_sub(1).wrapping_sub(t % ad) & m;
    if anc == 0 {
        return None;
    }
    let mut p = w - 1;
    // Q1,R1 = 2^(w-1)/anc; Q2,R2 = 2^(w-1)/ad.
    let (mut q1, mut r1) = (p_min / anc, p_min % anc);
    let (mut q2, mut r2) = (p_min / ad, p_min % ad);
    loop {
        p += 1;
        q1 = (q1 << 1) & m;
        r1 = (r1 << 1) & m;
        if r1 >= anc {
            q1 = q1.wrapping_add(1) & m;
            r1 -= anc;
        }
        q2 = (q2 << 1) & m;
        r2 = (r2 << 1) & m;
        if r2 >= ad {
            q2 = q2.wrapping_add(1) & m;
            r2 -= ad;
        }
        let delta = ad - r2;
        if !(q1 < delta || (q1 == delta && r1 == 0)) {
            break;
        }
        // The search always converges by `p == 2w`; guard anyway.
        if p > 2 * w {
            return None;
        }
    }
    let magic = q2.wrapping_add(1) & m;
    Some((magic, p - w))
}

/// `(magic, is_add, shift)` for unsigned division by `d` (`d >= 2`), per
/// LLVM's `UnsignedDivisionByConstantInfo::get` with neither the
/// even-divisor pre-shift nor the widened-magic variant. `is_add` marks
/// the W+1-bit magic case: the extra bit is folded into the emitted
/// `(x - q) >>> 1 + q` step instead of the constant. `lz` is a proven
/// bound of the dividend's leading zeros — LLVM's `LeadingZeros`
/// parameter: a smaller `nc` lets the search settle at a magic that fits
/// `w` bits where the full-width search needed `is_add` (`/7` with
/// `lz=2` gives `0x4924…925 >> 1` instead of the fixup).
pub fn magic_unsigned_lz(d: u64, w: u32, lz: u32) -> (u64, bool, u32) {
    debug_assert!(lz < w);
    let m = mask(w);
    let ones = mask(w - lz);
    let signed_min = 1u64 << (w - 1);
    let signed_max = signed_min - 1;
    // NC: largest dividend with NC % d == d - 1; (all_ones + 1 - d) is a
    // w-bit wrap (d can exceed the bounded dividend), so mask it back.
    let nc = ones.wrapping_sub((ones.wrapping_add(1).wrapping_sub(d) & m) % d) & m;
    debug_assert!(nc % d == d - 1);
    let mut p = w - 1;
    let (mut q1, mut r1) = (signed_min / nc, signed_min % nc);
    let (mut q2, mut r2) = (signed_max / d, signed_max % d);
    let mut is_add = false;
    loop {
        p += 1;
        if r1 >= nc - r1 {
            q1 = (q1 << 1).wrapping_add(1) & m;
            r1 = (r1 << 1).wrapping_sub(nc) & m;
        } else {
            q1 = (q1 << 1) & m;
            r1 = (r1 << 1) & m;
        }
        if r2.wrapping_add(1) & m >= d.wrapping_sub(r2) & m {
            if q2 >= signed_max {
                is_add = true;
            }
            q2 = (q2 << 1).wrapping_add(1) & m;
            r2 = (r2 << 1).wrapping_add(1).wrapping_sub(d) & m;
        } else {
            if q2 >= signed_min {
                is_add = true;
            }
            q2 = (q2 << 1) & m;
            r2 = (r2 << 1).wrapping_add(1) & m;
        }
        let delta = d.wrapping_sub(1).wrapping_sub(r2) & m;
        if !(p < 2 * w && (q1 < delta || (q1 == delta && r1 == 0))) {
            break;
        }
    }
    let magic = q2.wrapping_add(1) & m;
    let mut shift = p - w;
    if is_add {
        debug_assert!(shift > 0);
        shift -= 1;
    }
    (magic, is_add, shift)
}

/// `(magic, is_add, shift)` for unsigned division by `d` (`d >= 2`) at
/// 128 bits — `magic_unsigned_lz` with `lz = 0`, since `vmax`'s bound is
/// u64-shaped. All arithmetic is plain `u128`; `nc`'s `(all_ones + 1 - d)`
/// wraps to `2^128 - d` as at narrower widths.
fn magic_unsigned_128(d: u128) -> (u128, bool, u32) {
    let signed_min = 1u128 << 127;
    let signed_max = signed_min - 1;
    let nc = u128::MAX - (u128::MAX.wrapping_add(1).wrapping_sub(d) % d);
    debug_assert!(nc % d == d - 1);
    let mut p = 127u32;
    let (mut q1, mut r1) = (signed_min / nc, signed_min % nc);
    let (mut q2, mut r2) = (signed_max / d, signed_max % d);
    let mut is_add = false;
    loop {
        p += 1;
        if r1 >= nc - r1 {
            q1 = (q1 << 1) + 1;
            r1 = (r1 << 1).wrapping_sub(nc);
        } else {
            q1 <<= 1;
            r1 <<= 1;
        }
        if r2.wrapping_add(1) >= d - r2 {
            if q2 >= signed_max {
                is_add = true;
            }
            q2 = (q2 << 1) + 1;
            r2 = (r2 << 1).wrapping_add(1).wrapping_sub(d);
        } else {
            if q2 >= signed_min {
                is_add = true;
            }
            q2 <<= 1;
            r2 = (r2 << 1).wrapping_add(1);
        }
        let delta = d - 1 - r2;
        if !(p < 256 && (q1 < delta || (q1 == delta && r1 == 0))) {
            break;
        }
    }
    let magic = q2.wrapping_add(1);
    let mut shift = p - 128;
    if is_add {
        shift -= 1;
    }
    (magic, is_add, shift)
}

/// `c` as an i128 constant — `iconcat`'d i64 limbs, since `iconst.i128`
/// has no isel rule and an imm64 can't hold a 128-bit pattern anyway.
fn cst128(pos: &mut FuncCursor, c: u128) -> Value {
    let lo = pos.ins().iconst(types::I64, c as u64 as i64);
    let hi = pos.ins().iconst(types::I64, (c >> 64) as u64 as i64);
    pos.ins().iconcat(lo, hi)
}

/// An `i64` shift-amount constant (i128 shifts take a scalar amount).
fn cst_amt(pos: &mut FuncCursor, k: u32) -> Value {
    pos.ins().iconst(types::I64, k as i64)
}

/// `umulhi.i128` synthesized from `imul.i128` partial products — no target
/// lowers the opcode, but with `x = xl + xh·2^64` and `m` split likewise,
/// `P = p1 + (p2 + p3)·2^64 + p4·2^128` and every partial product of two
/// <2^64 operands fits its 128-bit result:
///   `P.hi = p4 + p2.hi + p3.hi + carry64(p1.hi + p2.lo + p3.lo)`.
fn mulhi_u128(pos: &mut FuncCursor, x: Value, m: u128) -> Value {
    let mask = cst128(pos, u64::MAX as u128);
    let c64 = cst_amt(pos, 64);
    let xl = pos.ins().band(x, mask);
    let xh = pos.ins().ushr(x, c64);
    let ml = cst128(pos, m as u64 as u128);
    let mh = cst128(pos, (m >> 64) as u128);
    let p1 = pos.ins().imul(xl, ml);
    let p2 = pos.ins().imul(xl, mh);
    let p3 = pos.ins().imul(xh, ml);
    let p4 = pos.ins().imul(xh, mh);
    let p2l = pos.ins().band(p2, mask);
    let p3l = pos.ins().band(p3, mask);
    let mid = pos.ins().iadd(p2l, p3l);
    let p1h = pos.ins().ushr(p1, c64);
    let mid = pos.ins().iadd(mid, p1h);
    let carry = pos.ins().ushr(mid, c64);
    let p2h = pos.ins().ushr(p2, c64);
    let p3h = pos.ins().ushr(p3, c64);
    let t = pos.ins().iadd(p4, p2h);
    let t = pos.ins().iadd(t, p3h);
    pos.ins().iadd(t, carry)
}

/// `x / d` (or `x % d`) for unsigned i128. `d == 1` reaches the pow2 path
/// (`ushr` by 0 / `band` by 0) which is already the answer.
fn expand_u128(pos: &mut FuncCursor, x: Value, d: u128, rem: bool) -> Value {
    if rem && d.is_power_of_two() {
        let dm1 = cst128(pos, d - 1);
        return pos.ins().band(x, dm1);
    }
    let q = if d.is_power_of_two() {
        let c = cst_amt(pos, d.trailing_zeros());
        pos.ins().ushr(x, c)
    } else {
        let (magic, is_add, shift) = magic_unsigned_128(d);
        let mut q = mulhi_u128(pos, x, magic);
        if is_add {
            let t = pos.ins().isub(x, q);
            let c1 = cst_amt(pos, 1);
            let t = pos.ins().ushr(t, c1);
            q = pos.ins().iadd(t, q);
        }
        if shift > 0 {
            let c = cst_amt(pos, shift);
            q = pos.ins().ushr(q, c);
        }
        q
    };
    if rem {
        let dc = cst128(pos, d);
        let qd = pos.ins().imul(q, dc);
        pos.ins().isub(x, qd)
    } else {
        q
    }
}

/// A `w`-bit pattern `k` as an `iconst` (div/rem are scalar-only).
fn cst(pos: &mut FuncCursor, ty: Type, w: u32, k: u64) -> Value {
    pos.ins().iconst(ty, sext(k, w))
}

/// Shift amounts are scalar — a lane-typed `iconst`.
fn scst(pos: &mut FuncCursor, ty: Type, w: u32, k: u64) -> Value {
    pos.ins().iconst(ty.lane_type(), sext(k, w))
}

/// Lane-wise `{s,u}mulhi` for a 128-bit vector of ≤32-bit lanes: widen each
/// operand's halves, take the 2×-width products, and gather the high half
/// of every product lane — `uzp2` on aarch64, `pshufd`/`punpck`-family
/// shuffles on x64. Both `imul(widen_*, widen_*)` pairs fold to the extmul
/// lowerings (`smull`/`umull`/`smlal`-family). Scalar `ty` emits the mulhi
/// opcode directly.
pub fn mulhi(pos: &mut FuncCursor, ty: Type, signed: bool, a: Value, b: Value) -> Value {
    if !ty.is_vector() {
        return if signed {
            pos.ins().smulhi(a, b)
        } else {
            pos.ins().umulhi(a, b)
        };
    }
    let e = ty.lane_type().bytes() as usize;
    let n = ty.bytes() as usize;
    let widen = |pos: &mut FuncCursor, v: Value| {
        if signed {
            (pos.ins().swiden_low(v), pos.ins().swiden_high(v))
        } else {
            (pos.ins().uwiden_low(v), pos.ins().uwiden_high(v))
        }
    };
    let (al, ah) = widen(pos, a);
    let (bl, bh) = widen(pos, b);
    let lo = pos.ins().imul(al, bl);
    let hi = pos.ins().imul(ah, bh);
    // The odd lane-sized chunks of `concat(lo, hi)` are the product lanes'
    // high halves in order — a `uzp2` mask.
    let per = n / (2 * e); // product lanes per half vector
    let mut mask_bytes = vec![0u8; n];
    for j in 0..n / e {
        let base = if j < per {
            (2 * j + 1) * e
        } else {
            n + (2 * (j - per) + 1) * e
        };
        for k in 0..e {
            mask_bytes[j * e + k] = (base + k) as u8;
        }
    }
    let imm = pos
        .func
        .dfg
        .immediates
        .push(ConstantData::from(mask_bytes.as_slice()));
    let le = MemFlagsData::new().with_endianness(Endianness::Little);
    let lo = pos.ins().bitcast(types::I8X16, le, lo);
    let hi = pos.ins().bitcast(types::I8X16, le, hi);
    let s = pos.ins().shuffle(lo, hi, imm);
    pos.ins().bitcast(ty, le, s)
}

/// `x / d` for `w`-bit pattern `d` with `|d| >= 2` (or `d >= 2` unsigned):
/// emits the quotient's insts at the cursor and returns its value. `None`
/// only if the magic search declined.
fn quotient(
    pos: &mut FuncCursor,
    ty: Type,
    w: u32,
    x: Value,
    d: u64,
    signed: bool,
    lz: u32,
) -> Option<Value> {
    let ad = sext(d, w).unsigned_abs();
    Some(if signed {
        if ad.is_power_of_two() {
            // q = sra(x + ((x >>a (w-1)) >>u (w-k)), k); negate when d < 0.
            let k = ad.trailing_zeros();
            let c1 = scst(pos, ty, w, (w - 1) as u64);
            let c2 = scst(pos, ty, w, (w - k) as u64);
            let c3 = scst(pos, ty, w, k as u64);
            let s = pos.ins().sshr(x, c1);
            let t = pos.ins().ushr(s, c2);
            let a = pos.ins().iadd(x, t);
            let q = pos.ins().sshr(a, c3);
            if sext(d, w) < 0 { pos.ins().ineg(q) } else { q }
        } else {
            // x / -d = -(x / d): build the positive-divisor sequence and
            // negate. LLVM instead negates the magic constant; this form
            // keeps one canonical fixup (d > 0, magic < 0 → add x).
            let (magic, shift) = magic_signed(ad, w)?;
            let mc = cst(pos, ty, w, magic);
            let mut q = mulhi(pos, ty, true, x, mc);
            if sext(magic, w) < 0 {
                q = pos.ins().iadd(q, x);
            }
            if shift > 0 {
                let c = scst(pos, ty, w, shift as u64);
                q = pos.ins().sshr(q, c);
            }
            // Round toward zero: add the dividend's sign bit.
            let c = scst(pos, ty, w, (w - 1) as u64);
            let t = pos.ins().ushr(x, c);
            q = pos.ins().iadd(q, t);
            if sext(d, w) < 0 {
                q = pos.ins().ineg(q);
            }
            q
        }
    } else {
        if d.is_power_of_two() {
            let c = scst(pos, ty, w, d.trailing_zeros() as u64);
            pos.ins().ushr(x, c)
        } else {
            let (magic, is_add, shift) = magic_unsigned_lz(d, w, lz);
            let mc = cst(pos, ty, w, magic);
            let mut q = mulhi(pos, ty, false, x, mc);
            if is_add {
                // The magic needs w+1 bits: q = ((x - q) >>> 1) + q.
                let t = pos.ins().isub(x, q);
                let c = scst(pos, ty, w, 1);
                let t = pos.ins().ushr(t, c);
                q = pos.ins().iadd(t, q);
            }
            if shift > 0 {
                let c = scst(pos, ty, w, shift as u64);
                q = pos.ins().ushr(q, c);
            }
            q
        }
    })
}

/// Expand one matched inst at the cursor; returns the value its result
/// aliases to. `None` only if the magic search declined.
fn expand(pos: &mut FuncCursor, ty: Type, x: Value, d: u64, op: Opcode, lz: u32) -> Option<Value> {
    let w = ty.lane_type().bits();
    let m = mask(w);
    let signed = matches!(op, Opcode::Sdiv | Opcode::Srem);
    let rem = matches!(op, Opcode::Srem | Opcode::Urem);
    // `x % -d == x % d` in truncating semantics: fold the divisor's sign
    // away up front (also lands `x % -1` — the `INT_MIN % -1` trap case —
    // on the `d == 1` → 0 path).
    let d = if rem && signed {
        sext(d, w).unsigned_abs()
    } else {
        d
    };
    let v = if rem && d == 1 {
        // `x % ±1 == 0`.
        cst(pos, ty, w, 0)
    } else if rem && signed && sext(d, w).unsigned_abs().is_power_of_two() {
        // `x % 2^k` keeps x's sign: subtract the toward-zero-rounded
        // multiple, `r = x - ((x + (sign & (|d|-1))) & -|d|)`.
        let ad = sext(d, w).unsigned_abs();
        let sc = scst(pos, ty, w, (w - 1) as u64);
        let m1 = cst(pos, ty, w, ad - 1);
        let md = cst(pos, ty, w, ad.wrapping_neg() & m);
        let s = pos.ins().sshr(x, sc);
        let t = pos.ins().band(s, m1);
        let a = pos.ins().iadd(x, t);
        let qd = pos.ins().band(a, md);
        pos.ins().isub(x, qd)
    } else if rem && !signed && d.is_power_of_two() {
        let dm1 = cst(pos, ty, w, d - 1);
        pos.ins().band(x, dm1)
    } else if !rem && d == 1 {
        x
    } else if !rem && signed && d == m {
        // `x / -1` wraps (`INT_MIN / -1` is unreachable in the original —
        // it would trap — so any wrap is a refinement).
        pos.ins().ineg(x)
    } else {
        // `x % d = x - (x/d)*d` once the quotient is built.
        let q = quotient(pos, ty, w, x, d, signed, lz)?;
        if rem {
            let dc = cst(pos, ty, w, d);
            let qd = pos.ins().imul(q, dc);
            pos.ins().isub(x, qd)
        } else {
            q
        }
    };
    Some(v)
}

/// The divisor's `w`-bit pattern from a scalar `iconst` (div/rem are
/// scalar-only in CLIF, so no vector-divisor form exists).
fn divisor(func: &Function, ty: Type, v: Value) -> Option<u64> {
    let v = func.dfg.resolve_aliases(v);
    let raw = match func.dfg.value_def(v) {
        cranelift_codegen::ir::ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => imm.bits(),
            _ => return None,
        },
        _ => return None,
    };
    Some((raw as u64) & mask(ty.lane_type().bits()))
}

/// An i64 `iconst`'s 64-bit pattern.
fn iconst64(func: &Function, v: Value) -> Option<u64> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.value_def(v) {
        cranelift_codegen::ir::ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } if func.dfg.value_type(v) == types::I64 => Some(imm.bits() as u64),
            _ => None,
        },
        _ => None,
    }
}

/// The divisor's i128 value. Lowering materializes i128 constants as
/// `iconcat`d i64 limbs (the `iconst.i128` form is a builder artifact
/// rewritten before this step runs); accept both.
fn divisor128(func: &Function, v: Value) -> Option<u128> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.value_def(v) {
        cranelift_codegen::ir::ValueDef::Result(i, _) => match func.dfg.insts[i] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => Some(imm.bits() as u128),
            InstructionData::Binary {
                opcode: Opcode::Iconcat,
                args,
            } => {
                let lo = iconst64(func, args[0])?;
                let hi = iconst64(func, args[1])?;
                Some((u128::from(hi) << 64) | u128::from(lo))
            }
            _ => None,
        },
        _ => None,
    }
}

/// `mulhi64_expensive`: true on targets without a one-instruction i64
/// mulhi (wasm32 — ~20 synthesized ops) where narrowing the expansion to
/// i32 pays for its ireduce/uextend wrapping. Native ISAs emit `umulh`
/// directly, so the wrapping is pure overhead there.
pub fn run(func: &mut Function, mulhi64_expensive: bool) -> usize {
    let mut hits = Vec::new();
    let mut hits128 = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let InstructionData::Binary { opcode, args } = func.dfg.insts[i] else {
                continue;
            };
            if !matches!(
                opcode,
                Opcode::Sdiv | Opcode::Srem | Opcode::Udiv | Opcode::Urem
            ) {
                continue;
            }
            let res = func.dfg.first_result(i);
            let ty = func.dfg.value_type(res);
            let w = ty.lane_type().bits();
            // Scalar-only opcodes, so `ty` is scalar.
            if ty.is_vector() {
                continue;
            }
            let x = func.dfg.resolve_aliases(args[0]);
            if w == 128 {
                // Signed i128 keeps the libcall (see the module doc).
                if !matches!(opcode, Opcode::Udiv | Opcode::Urem) {
                    continue;
                }
                let Some(d) = divisor128(func, args[1]) else {
                    continue;
                };
                if d == 0 {
                    continue;
                }
                hits128.push((i, x, d, opcode));
                continue;
            }
            if !matches!(w, 8 | 16 | 32 | 64) {
                continue;
            }
            let Some(d) = divisor(func, ty, args[1]) else {
                continue;
            };
            if d == 0 {
                continue;
            }
            hits.push((i, ty, x, d, opcode));
        }
    }
    let n = hits.len() + hits128.len();
    if n == 0 {
        return 0;
    }
    // `ovkill`-style bound tables, built lazily on the first i64
    // dividend that might narrow: the CFG walk costs more than the
    // expansion it enables when there's no candidate.
    let mut bctx: Option<(
        ControlFlowGraph,
        crate::vmap::BMap<Rc<clifpeep::BoundsMap>>,
        crate::vmap::BMap<Rc<clifpeep::BoundsMap>>,
        crate::vmap::BSet,
    )> = None;
    let narrow_on = crate::pass_enabled("PLIRON_DIVNARROW");
    let mut pos = FuncCursor::new(func);
    let mut done = 0;
    for (i, x, d, op) in hits128 {
        pos.goto_inst(i);
        let res = pos.func.dfg.first_result(i);
        let v = expand_u128(&mut pos, x, d, op == Opcode::Urem);
        pos.func.dfg.clear_results(i);
        pos.func.layout.remove_inst(i);
        pos.func.dfg.change_to_alias(res, v);
        done += 1;
        if pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::divconst", tracing::Level::DEBUG)
        {
            tracing::debug!(target: "pliron::divconst", "divconst: expanded {op}.i128 by {d:#x}")
        }
    }
    for (i, ty, x, d, op) in hits {
        pos.goto_inst(i);
        let res = pos.func.dfg.first_result(i);
        // Proven leading zeros on the dividend shrink `nc`, often
        // replacing the `(x - q) >>> 1 + q` fixup with a plain shift.
        // Only when the bound still exceeds the divisor — `x % d = x`
        // outright for `d > max(x)` but that's a fold elsewhere.
        let mut vm = clifpeep::vmax(pos.func, x);
        // `umulhi.i64` synthesizes a 64x64->128 product on targets with no
        // native mulhi (wasm: ~20 ops vs ~5 for `umulhi.i32`). When the
        // dividend provably fits u32 — a `brif`-bounded loop counter like
        // `i` in `i % 5` where the exit test is `i >= 1e8` — run the whole
        // expansion at i32 and re-widen the result (`PLIRON_DIVNARROW`).
        // Unsigned only: a `[0, 2^32)` bound can't place a signed dividend.
        // Native ISAs emit one `umulh` — the wrap costs ~2 insts/div
        // (measured: parse_only +5% on aarch64), so wasm only.
        let narrow = narrow_on
            && mulhi64_expensive
            && ty == types::I64
            && !matches!(op, Opcode::Sdiv | Opcode::Srem)
            && d <= u32::MAX as u64
            && if vm <= u32::MAX as u64 {
                true
            } else {
                let Some(b) = pos.func.layout.inst_block(i) else {
                    unreachable!("hit inst with no block")
                };
                let (cfg, ememo, memo, visiting) = bctx.get_or_insert_with(|| {
                    (
                        ControlFlowGraph::with_function(pos.func),
                        crate::vmap::BMap::default(),
                        crate::vmap::BMap::default(),
                        crate::vmap::BSet::default(),
                    )
                });
                let ub = clifpeep::bound_at(pos.func, cfg, ememo, memo, visiting, b, x);
                if ub <= u32::MAX as u64 {
                    vm = ub;
                    true
                } else {
                    false
                }
            };
        let lz = if vm >= d {
            (ty.bits() as u32).saturating_sub(64 - (vm | 1).leading_zeros())
        } else {
            0
        };
        let v = if narrow {
            let x32 = pos.ins().ireduce(types::I32, x);
            let lz32 = if vm >= d {
                ((vm as u32) | 1).leading_zeros()
            } else {
                0
            };
            expand(&mut pos, types::I32, x32, d, op, lz32)
                .map(|v32| pos.ins().uextend(ty, v32))
        } else {
            None
        };
        let Some(v) = v.or_else(|| expand(&mut pos, ty, x, d, op, lz)) else {
            continue;
        };
        pos.func.dfg.clear_results(i);
        pos.func.layout.remove_inst(i);
        pos.func.dfg.change_to_alias(res, v);
        done += 1;
        if pliron_cfg::tracing_on()
            && tracing::enabled!(target: "pliron::divconst", tracing::Level::DEBUG)
        {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::divconst", "divconst: expanded {op}.{ty} by {d:#x}")
            };
        }
    }
    done
}
