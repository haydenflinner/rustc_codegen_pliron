//! CLIF emission helpers for `llvm.minnum`/`llvm.maxnum` and for the vector
//! compare + `bitselect` pair behind `select(fcmp/icmp, x, y)` on vectors.
//!
//! They live here (not in `pliron-clif-lower`) so `tests/vecsel_native.rs` can
//! build a function from the exact same emitters, compile it for the host and
//! run it against a scalar reference — no rustc, no pliron IR.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{self, InstBuilder, MemFlagsData, Type};
use cranelift_frontend::FunctionBuilder;

/// `bitcast` as the lowerer emits it (vector casts are explicitly little-endian).
pub fn bitcast_to(b: &mut FunctionBuilder, t: Type, x: ir::Value) -> ir::Value {
    let from = b.func.dfg.value_type(x);
    let mf = if t.is_vector() || from.is_vector() {
        MemFlagsData::new().with_endianness(ir::Endianness::Little)
    } else {
        MemFlagsData::new()
    };
    b.ins().bitcast(t, mf, x)
}

/// IEEE `minNum`/`maxNum` (LLVM `llvm.minnum`/`llvm.maxnum`, Rust `f32::min`/
/// `max`) on a scalar or one native vector leaf: Cranelift `fmin`/`fmax`
/// propagate NaN, so pick the other operand where exactly one side is NaN.
/// 64-bit vectors (f32x2) have no `bitselect` lowering and go lane by lane.
pub fn fminmax_num(b: &mut FunctionBuilder, max: bool, x: ir::Value, y: ir::Value) -> ir::Value {
    let t = b.func.dfg.value_type(x);
    if t.is_vector() && t.bits() < 128 {
        let mut r = None;
        for i in 0..t.lane_count() {
            let xl = b.ins().extractlane(x, i as u8);
            let yl = b.ins().extractlane(y, i as u8);
            let l = fminmax_num(b, max, xl, yl);
            r = Some(match r {
                None => b.ins().splat(t, l),
                Some(v) => b.ins().insertlane(v, l, i as u8),
            });
        }
        return r.unwrap();
    }
    let m = if max { b.ins().fmax(x, y) } else { b.ins().fmin(x, y) };
    let xn = b.ins().fcmp(FloatCC::Unordered, x, x);
    let yn = b.ins().fcmp(FloatCC::Unordered, y, y);
    if t.is_vector() {
        let xn = bitcast_to(b, t, xn);
        let yn = bitcast_to(b, t, yn);
        let r = b.ins().bitselect(yn, x, m);
        b.ins().bitselect(xn, y, r)
    } else {
        let r = b.ins().select(yn, x, m);
        b.ins().select(xn, y, r)
    }
}

/// `pliron.vcmp.<p>.f` predicate (`p` as `eq ne lt le gt ge`): LLVM `oeq une
/// olt ole ogt oge`.
pub fn vcmp_float_cc(p: &str) -> FloatCC {
    match p {
        "eq" => FloatCC::Equal,
        "ne" => FloatCC::NotEqual,
        "lt" => FloatCC::LessThan,
        "le" => FloatCC::LessThanOrEqual,
        "gt" => FloatCC::GreaterThan,
        _ => FloatCC::GreaterThanOrEqual,
    }
}

/// `pliron.vcmp.<p>.<s|u>` integer predicate.
pub fn vcmp_int_cc(p: &str, signed: bool) -> IntCC {
    match p {
        "eq" => IntCC::Equal,
        "ne" => IntCC::NotEqual,
        "lt" if signed => IntCC::SignedLessThan,
        "le" if signed => IntCC::SignedLessThanOrEqual,
        "gt" if signed => IntCC::SignedGreaterThan,
        "ge" if signed => IntCC::SignedGreaterThanOrEqual,
        "lt" => IntCC::UnsignedLessThan,
        "le" => IntCC::UnsignedLessThanOrEqual,
        "gt" => IntCC::UnsignedGreaterThan,
        _ => IntCC::UnsignedGreaterThanOrEqual,
    }
}

/// `pliron.vcmp.*` on a native leaf: a lane-mask of type `rt`.
pub fn vcmp(
    b: &mut FunctionBuilder,
    p: &str,
    kind: &str,
    x: ir::Value,
    y: ir::Value,
    rt: Type,
) -> ir::Value {
    if kind == "f" {
        let m = b.ins().fcmp(vcmp_float_cc(p), x, y);
        if b.func.dfg.value_type(m) == rt { m } else { bitcast_to(b, rt, m) }
    } else {
        b.ins().icmp(vcmp_int_cc(p, kind == "s"), x, y)
    }
}

/// `pliron.vbitselect` on a native leaf.
pub fn vbitselect(b: &mut FunctionBuilder, mask: ir::Value, x: ir::Value, y: ir::Value) -> ir::Value {
    let t = b.func.dfg.value_type(x);
    let m = if b.func.dfg.value_type(mask) == t { mask } else { bitcast_to(b, t, mask) };
    b.ins().bitselect(m, x, y)
}
