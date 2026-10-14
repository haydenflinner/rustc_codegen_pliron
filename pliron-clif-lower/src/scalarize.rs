//! Split elementwise LLVM vector ops into per-lane scalar ops. The rustc
//! backend emits SIMD through `pliron.v*` intrinsics, so the lowerer's
//! plain-op paths (casts, fcmp, fp ops) are scalar-only; producers that
//! use `<N x T>` ops directly (CubeCL) go through here first. Lanes are
//! extract/insertelement with constant indices, which lower to lane moves.

use pliron::builtin::attributes::IntegerAttr;
use pliron::builtin::types::{IntegerType, Signedness};
use pliron::context::{Context, Ptr};
use pliron::irbuild::cloning::{IrMapping, clone_operation};
use pliron::irbuild::listener::DummyListener;
use pliron::irbuild::rewriter::IRRewriter;
use pliron::linked_list::ContainsLinkedList;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed};
use pliron::utils::apint::APInt;
use pliron_llvm::ops::*;
use pliron_llvm::types::VectorType;

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
pub fn run(ctx: &mut Context, func: Ptr<Operation>) {
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
                    if elementwise(ctx, op) {
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
}
