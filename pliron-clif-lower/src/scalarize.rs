//! Split elementwise LLVM vector ops into per-lane scalar ops. The rustc
//! backend emits SIMD through `pliron.v*` intrinsics, so the lowerer's
//! plain-op paths (casts, fcmp, fp ops) are scalar-only; producers that
//! use `<N x T>` ops directly (CubeCL) go through here first. Lanes are
//! extract/insertelement with constant indices, which lower to lane moves.

use pliron::builtin::attributes::IntegerAttr;
use pliron::builtin::types::{BF16Type, FP32Type, IntegerType, Signedness};
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
        return !["reduce", "masked", "vector.", "shuffle", "experimental", "lifetime"]
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
    let tys: Vec<_> = o.operands().chain([o.get_result(0)]).map(|v| v.get_type(ctx)).collect();
    let bfv = tys.iter().filter(|&&t| vec_of(ctx, t).is_some_and(|(e, _)| is_bf16(ctx, e))).count();
    // bf16 vector leaves are i16 bit patterns: only the f32 casts are native.
    let cast = Operation::get_op::<FPExtOp>(op, ctx).is_some() || Operation::get_op::<FPTruncOp>(op, ctx).is_some();
    let ok = if bfv == 0 { leafwise_kind(ctx, op) } else { cast && bfv == 1 };
    ok && tys.iter().all(|&t| simd_leaves(ctx, t))
}

fn simd_leaves(ctx: &Context, t: TypeHandle) -> bool {
    vec_of(ctx, t).is_none() || {
        let l = crate::types::leaves(ctx, t);
        !l.is_empty() && l.iter().all(|x| x.1.is_vector() && x.1.bits() == 128)
    }
}

fn leafwise_kind(ctx: &Context, op: Ptr<Operation>) -> bool {
    let id = Operation::get_opid(op, ctx);
    macro_rules! any {
        ($($t:ty),*) => { false $(|| id == <$t>::get_opid_static())* };
    }
    any!(AddOp, SubOp, MulOp, AndOp, OrOp, XorOp, FAddOp, FSubOp, FMulOp, FDivOp, FNegOp)
        || Operation::get_op::<CallIntrinsicOp>(op, ctx).is_some_and(|c| {
            let n = String::from(c.get_attr_llvm_intrinsic_name(ctx).unwrap().clone());
            LEAFWISE_INTRINSICS
                .iter()
                .any(|k| n.strip_prefix(k).is_some_and(|r| r.is_empty() || r.starts_with('.')))
        })
}

/// bf16 vector arithmetic whose `<n x f32>` twin is all SIMD leaves: promote
/// the whole vector so only the (cheap, bitwise) casts are split per lane.
fn bf16_vec_promotable(ctx: &mut Context, op: Ptr<Operation>) -> bool {
    if !crate::types::WIDE_SIMD.load(std::sync::atomic::Ordering::Relaxed)
        || op.deref(ctx).get_num_results() != 1
        || !leafwise_kind(ctx, op)
    {
        return false;
    }
    let t = op.deref(ctx).get_result(0).get_type(ctx);
    match vec_of(ctx, t) {
        Some((e, _)) if is_bf16(ctx, e) => {
            let w = widen_bf16(ctx, t);
            simd_leaves(ctx, w)
        }
        _ => false,
    }
}

/// Float intrinsics `lower_intrinsic` maps over SIMD leaves.
pub const LEAFWISE_INTRINSICS: [&str; 10] = [
    "llvm.fma", "llvm.fmuladd", "llvm.sqrt", "llvm.fabs", "llvm.floor", "llvm.ceil",
    "llvm.trunc", "llvm.roundeven", "llvm.minimum", "llvm.maximum",
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
        Some((e, n)) if is_bf16(ctx, e) => VectorType::get(ctx, f32t, n, VectorTypeKind::Fixed).into(),
        _ => t,
    }
}

fn has_bf16(ctx: &Context, t: TypeHandle) -> bool {
    is_bf16(ctx, t) || vec_of(ctx, t).is_some_and(|(e, _)| is_bf16(ctx, e))
}

fn promote_bf16(ctx: &mut Context, op: Ptr<Operation>) {
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

pub fn run(ctx: &mut Context, func: Ptr<Operation>) {
    let mut bfv = Vec::new();
    let regions: Vec<_> = func.deref(ctx).regions().collect();
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
    let mut todo = Vec::new();
    let mut shuffles = Vec::new();
    let regions: Vec<_> = func.deref(ctx).regions().collect();
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
    let regions: Vec<_> = func.deref(ctx).regions().collect();
    for r in regions {
        for b in r.deref(ctx).iter(ctx) {
            for op in b.deref(ctx).iter(ctx) {
                let o = op.deref(ctx);
                if o.get_num_results() != 1 || !bf16_arith(ctx, op) {
                    continue;
                }
                if o.operands().chain([o.get_result(0)]).any(|v| is_bf16(ctx, v.get_type(ctx))) {
                    bf.push(op);
                }
            }
        }
    }
    for op in bf {
        promote_bf16(ctx, op);
    }
}
