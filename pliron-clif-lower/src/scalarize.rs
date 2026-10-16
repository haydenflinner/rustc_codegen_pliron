//! Split elementwise LLVM vector ops into per-lane scalar ops. The rustc
//! backend emits SIMD through `pliron.v*` intrinsics, so the lowerer's
//! plain-op paths (casts, fcmp, fp ops) are scalar-only; producers that
//! use `<N x T>` ops directly (CubeCL) go through here first. Lanes are
//! extract/insertelement with constant indices, which lower to lane moves.

use pliron::builtin::attributes::IntegerAttr;
use pliron::builtin::types::{BF16Type, FP16Type, FP32Type, IntegerType, Signedness};
use pliron::context::{Context, Ptr};
use pliron::irbuild::cloning::{IrMapping, clone_operation};
use pliron::irbuild::listener::DummyListener;
use pliron::irbuild::rewriter::IRRewriter;
use pliron::linked_list::ContainsLinkedList;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed};
use pliron::utils::apint::APInt;
use pliron_llvm::op_interfaces::CastOpInterface;
use pliron_llvm::ops::*;
use pliron_llvm::types::{VectorType, VectorTypeKind};

fn vec_of(ctx: &Context, t: TypeHandle) -> Option<(TypeHandle, u32)> {
    t.deref(ctx)
        .downcast_ref::<VectorType>()
        .map(|v| (v.elem_type(), v.num_elements()))
}

fn elementwise(ctx: &Context, op: Ptr<Operation>) -> bool {
    let id = Operation::get_opid(op, ctx);
    macro_rules! any {
        ($($t:ty),*) => { false $(|| id == <$t>::get_opid_static())* };
    }
    if any!(
        AddOp, SubOp, MulOp, UDivOp, SDivOp, URemOp, SRemOp, AndOp, OrOp, XorOp, ShlOp, LShrOp,
        AShrOp, FAddOp, FSubOp, FMulOp, FDivOp, FRemOp, FNegOp, ICmpOp, FCmpOp, TruncOp, ZExtOp,
        SExtOp, FPTruncOp, FPExtOp, FPToUIOp, FPToSIOp, UIToFPOp, SIToFPOp, SelectOp, FreezeOp
    ) {
        return true;
    }
    if let Some(c) = Operation::get_op::<CallIntrinsicOp>(op, ctx) {
        let n = String::from(c.get_attr_llvm_intrinsic_name(ctx).unwrap().clone());
        return ![
            "reduce",
            "masked",
            "vector.",
            "shuffle",
            "experimental",
            "lifetime",
        ]
        .iter()
        .any(|k| n.contains(k));
    }
    false
}

/// Ops the lowerer applies per native SIMD leaf, when every vector they touch
/// is all 128-bit SIMD leaves (`types::leaves`); splitting those would only
/// lose SIMD. 64-bit vectors (`f32x2`) stay split: x64 mis-lowers some of them.
fn native_leafwise(ctx: &Context, op: Ptr<Operation>) -> bool {
    if !crate::types::WIDE_SIMD.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    let o = op.deref(ctx);
    let tys: Vec<_> = o
        .operands()
        .chain([o.get_result(0)])
        .map(|v| v.get_type(ctx))
        .collect();
    let bfv = tys
        .iter()
        .filter(|&&t| vec_of(ctx, t).is_some_and(|(e, _)| is_bf16(ctx, e)))
        .count();
    // bf16 vector leaves are i16 bit patterns: only the f32 casts are native.
    let cast = Operation::get_op::<FPExtOp>(op, ctx).is_some()
        || Operation::get_op::<FPTruncOp>(op, ctx).is_some();
    // Packed f16 vectors (wide f32 twins): likewise only the casts are native.
    let hfv = tys
        .iter()
        .filter(|&&t| {
            vec_of(ctx, t).is_some_and(|(e, _)| e.deref(ctx).is::<FP16Type>())
                && simd_leaves(ctx, t)
        })
        .count();
    let ok = if bfv + hfv == 0 {
        leafwise_kind(ctx, op)
    } else {
        (cast && bfv + hfv == 1) || (bfv == 0 && native_f16() && f16_native_arith(ctx, op))
    };
    ok && tys.iter().all(|&t| simd_leaves(ctx, t))
}

fn simd_leaves(ctx: &Context, t: TypeHandle) -> bool {
    vec_of(ctx, t).is_none() || {
        let l = crate::types::leaves(ctx, t);
        !l.is_empty() && l.iter().all(|x| x.1.is_vector() && x.1.bits() >= 128)
    }
}

fn leafwise_kind(ctx: &Context, op: Ptr<Operation>) -> bool {
    let id = Operation::get_opid(op, ctx);
    macro_rules! any {
        ($($t:ty),*) => { false $(|| id == <$t>::get_opid_static())* };
    }
    any!(
        AddOp, SubOp, MulOp, AndOp, OrOp, XorOp, FAddOp, FSubOp, FMulOp, FDivOp, FNegOp
    ) || Operation::get_op::<CallIntrinsicOp>(op, ctx).is_some_and(|c| {
        let n = String::from(c.get_attr_llvm_intrinsic_name(ctx).unwrap().clone());
        LEAFWISE_INTRINSICS.iter().any(|k| {
            n.strip_prefix(k)
                .is_some_and(|r| r.is_empty() || r.starts_with('.'))
        })
    })
}

/// bf16 vector arithmetic whose `<n x f32>` twin is all SIMD leaves: promote
/// the whole vector so only the (cheap, bitwise) casts are split per lane.
fn bf16_vec_promotable(ctx: &mut Context, op: Ptr<Operation>) -> bool {
    // With native bf16 casts, lane building also moves to the f32 twin (like f16).
    let lane_op = crate::types::NATIVE_BF16.load(std::sync::atomic::Ordering::Relaxed)
        && (Operation::get_op::<InsertElementOp>(op, ctx).is_some()
            || Operation::get_op::<ShuffleVectorOp>(op, ctx).is_some());
    if !crate::types::WIDE_SIMD.load(std::sync::atomic::Ordering::Relaxed)
        || op.deref(ctx).get_num_results() != 1
        || !(lane_op || leafwise_kind(ctx, op))
    {
        return false;
    }
    let t = op.deref(ctx).get_result(0).get_type(ctx);
    match vec_of(ctx, t) {
        Some((e, n)) if is_bf16(ctx, e) => {
            if lane_op {
                let f32t: TypeHandle = FP32Type::get(ctx).into();
                return crate::types::wide_f32_candidate(ctx, f32t, n as u64);
            }
            let w = widen_bf16(ctx, t);
            simd_leaves(ctx, w)
        }
        _ => false,
    }
}

/// Float intrinsics `lower_intrinsic` maps over SIMD leaves.
pub const LEAFWISE_INTRINSICS: [&str; 10] = [
    "llvm.fma",
    "llvm.fmuladd",
    "llvm.sqrt",
    "llvm.fabs",
    "llvm.floor",
    "llvm.ceil",
    "llvm.trunc",
    "llvm.roundeven",
    "llvm.minimum",
    "llvm.maximum",
];

fn idx(ctx: &mut Context, i: u32, before: Ptr<Operation>) -> pliron::value::Value {
    let t = IntegerType::get(ctx, 32, Signedness::Signless);
    let a = IntegerAttr::new(t, APInt::from_u64(i.into(), 32.try_into().unwrap()));
    let c = ConstantOp::new(ctx, Box::new(a)).get_operation();
    c.insert_before(ctx, before);
    c.deref(ctx).get_result(0)
}

fn split(ctx: &mut Context, op: Ptr<Operation>, ety: TypeHandle, n: u32) {
    let vty = op.deref(ctx).get_result(0).get_type(ctx);
    let opnds: Vec<_> = op.deref(ctx).operands().collect();
    let mut rw = IRRewriter::<DummyListener>::default();
    let p = PoisonOp::new(ctx, vty).get_operation();
    p.insert_before(ctx, op);
    let mut acc = p.deref(ctx).get_result(0);
    for i in 0..n {
        let ix = idx(ctx, i, op);
        let mut map = IrMapping::new();
        for &v in &opnds {
            if vec_of(ctx, v.get_type(ctx)).is_some() {
                let e = ExtractElementOp::new(ctx, v, ix).get_operation();
                e.insert_before(ctx, op);
                map.map_value(v, e.deref(ctx).get_result(0));
            }
        }
        let s = clone_operation(op, ctx, &mut rw, &mut map);
        let lane = s.deref(ctx).get_result(0);
        lane.set_type(ctx, ety);
        s.insert_before(ctx, op);
        let ins = InsertElementOp::new(ctx, acc, lane, ix).get_operation();
        ins.insert_before(ctx, op);
        acc = ins.deref(ctx).get_result(0);
    }
    let old = op.deref(ctx).get_result(0);
    old.replace_all_uses_with(ctx, &acc);
    Operation::erase(op, ctx);
}

/// `shufflevector a, b, mask` -> per-lane extract from `a ++ b` + insert;
/// negative mask entries are poison lanes and stay poison.
fn shuffle(ctx: &mut Context, op: Ptr<Operation>) {
    let sv = Operation::get_op::<ShuffleVectorOp>(op, ctx).unwrap();
    let mask = sv.get_attr_llvm_shuffle_vector_mask(ctx).unwrap().0.clone();
    let (a, b) = {
        let o = op.deref(ctx);
        (o.get_operand(0), o.get_operand(1))
    };
    let na = vec_of(ctx, a.get_type(ctx)).unwrap().1 as i32;
    let vty = op.deref(ctx).get_result(0).get_type(ctx);
    let p = PoisonOp::new(ctx, vty).get_operation();
    p.insert_before(ctx, op);
    let mut acc = p.deref(ctx).get_result(0);
    for (i, &m) in mask.iter().enumerate() {
        if m < 0 {
            continue;
        }
        let (src, j) = if m < na { (a, m) } else { (b, m - na) };
        let jx = idx(ctx, j as u32, op);
        let e = ExtractElementOp::new(ctx, src, jx).get_operation();
        e.insert_before(ctx, op);
        let lane = e.deref(ctx).get_result(0);
        let ix = idx(ctx, i as u32, op);
        let ins = InsertElementOp::new(ctx, acc, lane, ix).get_operation();
        ins.insert_before(ctx, op);
        acc = ins.deref(ctx).get_result(0);
    }
    let old = op.deref(ctx).get_result(0);
    old.replace_all_uses_with(ctx, &acc);
    Operation::erase(op, ctx);
}

/// Scalarize every elementwise vector op in `func`'s body.
fn is_bf16(ctx: &Context, t: TypeHandle) -> bool {
    t.deref(ctx).is::<BF16Type>()
}

/// Ops whose bf16 semantics differ from the f16 bits the lowerer sees.
fn bf16_arith(ctx: &Context, op: Ptr<Operation>) -> bool {
    let id = Operation::get_opid(op, ctx);
    macro_rules! any {
        ($($t:ty),*) => { false $(|| id == <$t>::get_opid_static())* };
    }
    if any!(
        FAddOp, FSubOp, FMulOp, FDivOp, FRemOp, FNegOp, FCmpOp, FPToUIOp, FPToSIOp, UIToFPOp,
        SIToFPOp
    ) {
        return true;
    }
    Operation::get_op::<CallIntrinsicOp>(op, ctx).is_some_and(|c| {
        let n = String::from(c.get_attr_llvm_intrinsic_name(ctx).unwrap().clone());
        // Reductions take a bf16 vector; the lowerer widens their lanes.
        !n.contains("lifetime") && !n.contains("vector.reduce")
    })
}

/// The lowerer carries bf16 as f16 bits (types.rs), so bf16 arithmetic is
/// rewritten to `fpext` -> f32 op -> `fptrunc`; the lowerer expands those
/// two casts as bit operations.
/// bf16 -> f32, `<n x bf16>` -> `<n x f32>`; anything else unchanged.
fn widen_bf16(ctx: &mut Context, t: TypeHandle) -> TypeHandle {
    let f32t: TypeHandle = FP32Type::get(ctx).into();
    if is_bf16(ctx, t) {
        return f32t;
    }
    match vec_of(ctx, t) {
        Some((e, n)) if is_bf16(ctx, e) => {
            VectorType::get(ctx, f32t, n, VectorTypeKind::Fixed).into()
        }
        _ => t,
    }
}

fn has_bf16(ctx: &Context, t: TypeHandle) -> bool {
    is_bf16(ctx, t) || vec_of(ctx, t).is_some_and(|(e, _)| is_bf16(ctx, e))
}

fn promote_bf16(ctx: &mut Context, op: Ptr<Operation>) {
    promote_with(ctx, op, has_bf16, widen_bf16)
}

fn is_f16_vec(ctx: &Context, t: TypeHandle) -> bool {
    vec_of(ctx, t).is_some_and(|(e, _)| e.deref(ctx).is::<FP16Type>())
}

/// `<n x half>` -> `<n x f32>`; anything else unchanged.
fn widen_f16_vec(ctx: &mut Context, t: TypeHandle) -> TypeHandle {
    match vec_of(ctx, t) {
        Some((e, n)) if e.deref(ctx).is::<FP16Type>() => {
            let f32t: TypeHandle = FP32Type::get(ctx).into();
            VectorType::get(ctx, f32t, n, VectorTypeKind::Fixed).into()
        }
        _ => t,
    }
}

fn is_f16_any(ctx: &Context, t: TypeHandle) -> bool {
    t.deref(ctx).is::<FP16Type>() || is_f16_vec(ctx, t)
}

fn widen_f16_any(ctx: &mut Context, t: TypeHandle) -> TypeHandle {
    if t.deref(ctx).is::<FP16Type>() {
        FP32Type::get(ctx).into()
    } else {
        widen_f16_vec(ctx, t)
    }
}

/// f16 vector arithmetic (and lane building) whose `<n x f32>` twin may get wide leaves: compute
/// it in f32 (as the per-lane path does) so the f16 side is just F16C casts.
fn f16_vec_promotable(ctx: &mut Context, op: Ptr<Operation>) -> bool {
    let lane_op = Operation::get_op::<InsertElementOp>(op, ctx).is_some()
        || Operation::get_op::<ShuffleVectorOp>(op, ctx).is_some();
    if op.deref(ctx).get_num_results() != 1 || !(lane_op || leafwise_kind(ctx, op)) {
        return false;
    }
    if !lane_op && native_f16() && f16_native_arith(ctx, op) {
        return false;
    }
    let t = op.deref(ctx).get_result(0).get_type(ctx);
    match vec_of(ctx, t) {
        Some((e, n)) if e.deref(ctx).is::<FP16Type>() => {
            let f32t: TypeHandle = FP32Type::get(ctx).into();
            crate::types::wide_f32_candidate(ctx, f32t, n as u64)
        }
        _ => false,
    }
}

fn promote_with(
    ctx: &mut Context,
    op: Ptr<Operation>,
    has_bf16: fn(&Context, TypeHandle) -> bool,
    widen_bf16: fn(&mut Context, TypeHandle) -> TypeHandle,
) {
    let opnds: Vec<_> = op.deref(ctx).operands().collect();
    let mut map = IrMapping::new();
    for &v in &opnds {
        let t = v.get_type(ctx);
        if has_bf16(ctx, t) {
            let f32t = widen_bf16(ctx, t);
            let e = FPExtOp::new(ctx, v, f32t).get_operation();
            e.insert_before(ctx, op);
            map.map_value(v, e.deref(ctx).get_result(0));
        }
    }
    let mut rw = IRRewriter::<DummyListener>::default();
    let s = clone_operation(op, ctx, &mut rw, &mut map);
    s.insert_before(ctx, op);
    let old = op.deref(ctx).get_result(0);
    let mut new = s.deref(ctx).get_result(0);
    let rt = old.get_type(ctx);
    if has_bf16(ctx, rt) {
        let f32t = widen_bf16(ctx, rt);
        new.set_type(ctx, f32t);
        let t = FPTruncOp::new(ctx, new, rt).get_operation();
        t.insert_before(ctx, op);
        new = t.deref(ctx).get_result(0);
    }
    old.replace_all_uses_with(ctx, &new);
    Operation::erase(op, ctx);
}

/// Vector bf16/f16 arithmetic -> f32 twins (before [`plan_wide`]).
pub fn promote(ctx: &mut Context, func: Ptr<Operation>) {
    let mut hfv = Vec::new();
    let regions: Vec<_> = func.deref(ctx).regions(ctx).collect();
    for &r in &regions {
        let blocks: Vec<_> = r.deref(ctx).iter(ctx).collect();
        for b in blocks {
            let ops: Vec<_> = b.deref(ctx).iter(ctx).collect();
            hfv.extend(ops.into_iter().filter(|&op| f16_vec_promotable(ctx, op)));
        }
    }
    for op in hfv {
        promote_with(ctx, op, is_f16_any, widen_f16_any);
    }
    let mut bfv = Vec::new();
    let regions: Vec<_> = func.deref(ctx).regions(ctx).collect();
    for &r in &regions {
        let blocks: Vec<_> = r.deref(ctx).iter(ctx).collect();
        for b in blocks {
            let ops: Vec<_> = b.deref(ctx).iter(ctx).collect();
            bfv.extend(ops.into_iter().filter(|&op| bf16_vec_promotable(ctx, op)));
        }
    }
    for op in bfv {
        promote_bf16(ctx, op);
    }
}

pub fn run(ctx: &mut Context, func: Ptr<Operation>) {
    promote(ctx, func);
    let mut todo = Vec::new();
    let mut shuffles = Vec::new();
    let regions: Vec<_> = func.deref(ctx).regions(ctx).collect();
    for r in regions {
        for b in r.deref(ctx).iter(ctx) {
            for op in b.deref(ctx).iter(ctx) {
                let o = op.deref(ctx);
                if Operation::get_op::<ShuffleVectorOp>(op, ctx).is_some() {
                    shuffles.push(op);
                    continue;
                }
                if o.get_num_results() != 1 {
                    continue;
                }
                if let Some((e, n)) = vec_of(ctx, o.get_result(0).get_type(ctx)) {
                    if elementwise(ctx, op) && !native_leafwise(ctx, op) {
                        todo.push((op, e, n));
                    }
                }
            }
        }
    }
    for op in shuffles {
        shuffle(ctx, op);
    }
    for (op, e, n) in todo {
        split(ctx, op, e, n);
    }
    let mut bf = Vec::new();
    let regions: Vec<_> = func.deref(ctx).regions(ctx).collect();
    for r in regions {
        for b in r.deref(ctx).iter(ctx) {
            for op in b.deref(ctx).iter(ctx) {
                let o = op.deref(ctx);
                if o.get_num_results() != 1 || !bf16_arith(ctx, op) {
                    continue;
                }
                if o.operands()
                    .chain([o.get_result(0)])
                    .any(|v| is_bf16(ctx, v.get_type(ctx)))
                {
                    bf.push(op);
                }
            }
        }
    }
    for op in bf {
        promote_bf16(ctx, op);
    }
}

/// Whether `op` has a ymm/zmm lowering for every wide `<n x f32>` it touches.
fn wide_op(ctx: &Context, op: Ptr<Operation>) -> bool {
    let id = Operation::get_opid(op, ctx);
    macro_rules! any {
        ($($t:ty),*) => { false $(|| id == <$t>::get_opid_static())* };
    }
    any!(
        LoadOp,
        StoreOp,
        InsertElementOp,
        ExtractElementOp,
        PoisonOp,
        UndefOp,
        ZeroOp,
        FreezeOp,
        ConstantOp,
        FAddOp,
        FSubOp,
        FMulOp,
        FDivOp,
        BrOp,
        CondBrOp,
        FPExtOp,
        FPTruncOp
    ) || Operation::get_op::<CallIntrinsicOp>(op, ctx).is_some_and(|c| {
        let n = String::from(c.get_attr_llvm_intrinsic_name(ctx).unwrap().clone());
        ["llvm.fma", "llvm.fmuladd"].iter().any(|k| {
            n.strip_prefix(k)
                .is_some_and(|r| r.is_empty() || r.starts_with('.'))
        })
    })
}

/// CubeCL: give `<n x f32>` types ymm/zmm leaves when every op on them (in
/// all of `funcs`, after [`run`]) lowers natively at that width; the rest keep
/// 128-bit leaves. Function parameters stay narrow (ABI).
pub fn plan_wide(ctx: &mut Context, funcs: &[Ptr<Operation>]) {
    use crate::types::{set_wide_f32, wide_f32_candidate};
    let f32h: TypeHandle = FP32Type::get(ctx).into();
    let f16h: TypeHandle = FP16Type::get(ctx).into();
    let bf16h: TypeHandle = BF16Type::get(ctx).into();
    let nbf = crate::types::NATIVE_BF16.load(std::sync::atomic::Ordering::Relaxed);
    let ctx: &Context = ctx;
    let mut ok: std::collections::HashMap<(TypeHandle, u64), bool> = Default::default();
    // A packed `<n x half>` (half-width leaves) stands or falls with its f32 twin.
    let mut note = |ctx: &Context, t: TypeHandle, good: bool, good16: bool| {
        if let Some((e, n)) = vec_of(ctx, t) {
            let half = e == f16h || (nbf && e == bf16h);
            let (twin, good) = if half { (f32h, good16) } else { (e, good) };
            if wide_f32_candidate(ctx, twin, n as u64) {
                *ok.entry((e, n as u64)).or_insert(true) &= good;
            }
        }
    };
    for &f in funcs {
        for r in f.deref(ctx).regions(ctx) {
            for (i, b) in r.deref(ctx).iter(ctx).enumerate() {
                for a in b.deref(ctx).arguments() {
                    note(ctx, a.get_type(ctx), i != 0, i != 0);
                }
                for op in b.deref(ctx).iter(ctx) {
                    // Planned before `run` splits: shuffles and split ops become
                    // extract/insert, which are wide.
                    let good = wide_op(ctx, op) || will_split(ctx, op);
                    let good16 = half_wide_op(ctx, op);
                    if !good16
                        && pliron_cfg::tracing_on()
                        && tracing::enabled!(target: "pliron::scalarize", tracing::Level::DEBUG)
                        && op
                            .deref(ctx)
                            .operands()
                            .chain(op.deref(ctx).results())
                            .any(|v| {
                                vec_of(ctx, v.get_type(ctx))
                                    .is_some_and(|(e, _)| e == f16h || e == bf16h)
                            })
                    {
                        if pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::scalarize", "wide-f16 reject: {}", Operation::get_opid(op, ctx))
                        };
                    }
                    let o = op.deref(ctx);
                    for v in o.operands().chain(o.results()) {
                        note(ctx, v.get_type(ctx), good, good16);
                    }
                }
            }
        }
    }
    // A packed f16 vector needs its f32 twin wide too (casts map leaf to leaf).
    let wide: std::collections::HashSet<_> = ok
        .iter()
        .filter(|&(&(e, n), &g)| {
            g && ((e != f16h && e != bf16h) || ok.get(&(f32h, n)) != Some(&false))
        })
        .map(|x| *x.0)
        .collect();
    set_wide_f32(wide);
}

fn will_split(ctx: &Context, op: Ptr<Operation>) -> bool {
    Operation::get_op::<ShuffleVectorOp>(op, ctx).is_some()
        || (op.deref(ctx).get_num_results() == 1
            && vec_of(ctx, op.deref(ctx).get_result(0).get_type(ctx)).is_some()
            && elementwise(ctx, op)
            && !native_leafwise(ctx, op))
}

/// Ops with an x64 lowering on packed f16 (`F16X8`/`F16X16`) leaves.
fn half_wide_op(ctx: &Context, op: Ptr<Operation>) -> bool {
    let id = Operation::get_opid(op, ctx);
    macro_rules! any {
        ($($t:ty),*) => { false $(|| id == <$t>::get_opid_static())* };
    }
    any!(
        LoadOp, StoreOp, FreezeOp, BrOp, CondBrOp, FPExtOp, FPTruncOp, ConstantOp, ZeroOp,
        PoisonOp, UndefOp
    ) || (native_f16() && f16_native_arith(ctx, op))
}

fn native_f16() -> bool {
    crate::types::NATIVE_F16.load(std::sync::atomic::Ordering::Relaxed)
}

/// f16 vector ops with an AVX512-FP16 lowering.
fn f16_native_arith(ctx: &Context, op: Ptr<Operation>) -> bool {
    let id = Operation::get_opid(op, ctx);
    macro_rules! any {
        ($($t:ty),*) => { false $(|| id == <$t>::get_opid_static())* };
    }
    any!(FAddOp, FSubOp, FMulOp, FDivOp)
        || Operation::get_op::<CallIntrinsicOp>(op, ctx).is_some_and(|c| {
            let n = String::from(c.get_attr_llvm_intrinsic_name(ctx).unwrap().clone());
            ["llvm.fma", "llvm.fmuladd"].iter().any(|k| {
                n.strip_prefix(k)
                    .is_some_and(|r| r.is_empty() || r.starts_with('.'))
            })
        })
}
