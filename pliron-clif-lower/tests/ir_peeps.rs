//! IR-level tests (no rustc, no codegen) for two pliron-IR rewrites in
//! `scalarize`: `vselect_fuse` (PLIRON_VSELECT, default on) and the
//! `fpext(fptrunc(x))` fold of promoted f16 lane ops (PLIRON_F16SPLAT).

use std::sync::Once;
use std::sync::atomic::Ordering::Relaxed;

use pliron::basic_block::BasicBlock;
use pliron::builtin::attributes::IntegerAttr;
use pliron::builtin::types::{FP16Type, FP32Type, FP64Type, IntegerType, Signedness};
use pliron::context::{Context, Ptr};
use pliron::identifier::Identifier;
use pliron::linked_list::ContainsLinkedList;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::TypeHandle;
use pliron::utils::apint::APInt;
use pliron::value::Value;
use pliron_clif_lower::{scalarize, types};
use pliron_llvm::attributes::{FCmpPredicateAttr, ICmpPredicateAttr};
use pliron_llvm::ops::*;
use pliron_llvm::types::{FuncType, VectorType, VectorTypeKind};

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Before anything reads the one-shot config.
        if std::env::var_os("PLIRON_F16SPLAT").is_none() {
            unsafe { std::env::set_var("PLIRON_F16SPLAT", "1") };
        }
        types::WIDE_SIMD.store(true, Relaxed);
    });
}

fn vec(ctx: &mut Context, e: TypeHandle, n: u32) -> TypeHandle {
    VectorType::get(ctx, e, n, VectorTypeKind::Fixed).into()
}

struct F {
    ctx: Context,
    func: Ptr<Operation>,
    blk: Ptr<BasicBlock>,
}

impl F {
    fn new(args: impl FnOnce(&mut Context) -> (Vec<TypeHandle>, TypeHandle)) -> Self {
        init();
        let mut ctx = Context::new();
        let (a, r) = args(&mut ctx);
        let ty = FuncType::get(&ctx, r, a, false);
        let f = FuncOp::new(&mut ctx, Identifier::try_from("t").unwrap(), ty);
        let blk = f.get_or_create_entry_block(&mut ctx);
        F { func: f.get_operation(), ctx, blk }
    }
    fn arg(&self, i: usize) -> Value {
        self.blk.deref(&self.ctx).get_argument(&self.ctx, i)
    }
    fn push(&mut self, op: Ptr<Operation>) -> Value {
        op.insert_at_back(self.blk, &self.ctx);
        op.deref(&self.ctx).get_result(&self.ctx, 0)
    }
    fn idx(&mut self, i: u64) -> Value {
        let t = IntegerType::get(&mut self.ctx, 32, Signedness::Signless);
        let a = IntegerAttr::new(t, APInt::from_u64(i, 32.try_into().unwrap()));
        let c = ConstantOp::new(&mut self.ctx, Box::new(a)).get_operation();
        self.push(c)
    }
    fn ret(&mut self, v: Value) {
        let r = ReturnOp::new(&mut self.ctx, Some(v)).get_operation();
        r.insert_at_back(self.blk, &self.ctx);
    }
    fn ops(&self) -> Vec<Ptr<Operation>> {
        self.blk.deref(&self.ctx).iter(&self.ctx).collect()
    }
    fn count<T: Op>(&self) -> usize {
        self.ops().into_iter().filter(|&o| Operation::get_op::<T>(o, &self.ctx).is_some()).count()
    }
    fn intrinsics(&self) -> Vec<String> {
        self.ops()
            .into_iter()
            .filter_map(|o| Operation::get_op::<CallIntrinsicOp>(o, &self.ctx))
            .map(|c| String::from(c.get_attr_llvm_intrinsic_name(&self.ctx).unwrap().clone()))
            .collect()
    }
}

/// `select(fcmp pred a, b), x, y` on `n` lanes of `e`; returns the function
/// and the compare's value so a test can add a second user.
fn select_fn(
    e: impl FnOnce(&mut Context) -> TypeHandle,
    n: u32,
    pred: Option<FCmpPredicateAttr>,
    ipred: Option<ICmpPredicateAttr>,
    other_user: bool,
) -> F {
    let mut f = F::new(|ctx| {
        let et = e(ctx);
        let v = vec(ctx, et, n);
        (vec![v, v, v, v], v)
    });
    let (a, b, x, y) = (f.arg(0), f.arg(1), f.arg(2), f.arg(3));
    let c = if let Some(p) = pred {
        let o = FCmpOp::new(&mut f.ctx, p, a, b).get_operation();
        f.push(o)
    } else {
        let o = ICmpOp::new(&mut f.ctx, ipred.unwrap(), a, b).get_operation();
        f.push(o)
    };
    let s = SelectOp::new(&mut f.ctx, c, x, y).get_operation();
    let r = f.push(s);
    if other_user {
        // A second user of the mask keeps the compare alive.
        let z = FreezeOp::new(&mut f.ctx, c).get_operation();
        f.push(z);
    }
    f.ret(r);
    f
}

fn f32t(ctx: &mut Context) -> TypeHandle {
    FP32Type::get(ctx).into()
}

#[test]
fn vselect_fuses_f32x4_ordered_compares_and_erases_the_compare() {
    use FCmpPredicateAttr as P;
    for p in [P::OEQ, P::UNE, P::OLT, P::OLE, P::OGT, P::OGE] {
        let mut f = select_fn(f32t, 4, Some(p.clone()), None, false);
        scalarize::vselect_fuse(&mut f.ctx, f.func);
        assert_eq!(f.count::<SelectOp>(), 0, "{p:?}: select not fused");
        assert_eq!(f.count::<FCmpOp>(), 0, "{p:?}: dead compare not erased");
        let i = f.intrinsics();
        assert_eq!(i.len(), 2, "{i:?}");
        assert!(i[0].starts_with("pliron.vcmp.") && i[0].ends_with(".f"), "{i:?}");
        assert_eq!(i[1], "pliron.vbitselect");
    }
}

#[test]
fn vselect_fuses_i32x4_all_int_predicates() {
    use ICmpPredicateAttr as P;
    for p in [P::EQ, P::NE, P::SLT, P::SLE, P::SGT, P::SGE, P::ULT, P::ULE, P::UGT, P::UGE] {
        let mut f = select_fn(
            |ctx| IntegerType::get(ctx, 32, Signedness::Signless).into(),
            4,
            None,
            Some(p.clone()),
            false,
        );
        scalarize::vselect_fuse(&mut f.ctx, f.func);
        assert_eq!(f.count::<SelectOp>(), 0, "{p:?}");
        assert_eq!(f.count::<ICmpOp>(), 0, "{p:?}");
        assert_eq!(f.intrinsics().len(), 2, "{p:?}");
    }
}

#[test]
fn vselect_keeps_compare_with_another_user() {
    let mut f = select_fn(f32t, 4, Some(FCmpPredicateAttr::OGT), None, true);
    scalarize::vselect_fuse(&mut f.ctx, f.func);
    assert_eq!(f.count::<SelectOp>(), 0);
    assert_eq!(f.count::<FCmpOp>(), 1, "compare with a second user must stay");
}

#[test]
fn vselect_leaves_unfusable_selects_alone() {
    use FCmpPredicateAttr as P;
    // Predicates whose NaN behavior differs from the vector compares.
    for p in [P::ORD, P::UNO, P::UGT, P::UGE, P::ULT, P::ULE, P::UEQ, P::ONE] {
        let mut f = select_fn(f32t, 4, Some(p.clone()), None, false);
        scalarize::vselect_fuse(&mut f.ctx, f.func);
        assert_eq!(f.count::<SelectOp>(), 1, "{p:?} must not fuse");
        assert!(f.intrinsics().is_empty());
    }
    // 64-bit lanes and 64-bit vectors (no native 128-bit leaf pair / no ISLE).
    let mut f = select_fn(|c| FP64Type::get(c).into(), 2, Some(P::OGT), None, false);
    scalarize::vselect_fuse(&mut f.ctx, f.func);
    assert_eq!(f.count::<SelectOp>(), 1, "f64x2 must not fuse");
    let mut f = select_fn(f32t, 2, Some(P::OGT), None, false);
    scalarize::vselect_fuse(&mut f.ctx, f.func);
    assert_eq!(f.count::<SelectOp>(), 1, "f32x2 must not fuse");
}

/// CubeCL's f16 line broadcast: `insertelement(zero, h, 0)` then a splat
/// `shufflevector`. `promote` wraps each in fpext/fptrunc; the second fpext
/// of the first fptrunc must fold away (every lane is an exact f16).
#[test]
fn f16_splat_folds_ext_of_trunc() {
    let mut f = F::new(|ctx| {
        let h: TypeHandle = FP16Type::get(ctx).into();
        let v = vec(ctx, h, 8);
        (vec![h], v)
    });
    let h = f.arg(0);
    let v8 = {
        let h16: TypeHandle = FP16Type::get(&mut f.ctx).into();
        vec(&mut f.ctx, h16, 8)
    };
    let z = ZeroOp::new(&mut f.ctx, v8).get_operation();
    let z = f.push(z);
    let i0 = f.idx(0);
    let ins = InsertElementOp::new(&mut f.ctx, z, h, i0).get_operation();
    let ins = f.push(ins);
    let sh = ShuffleVectorOp::new(&mut f.ctx, ins, ins, vec![0; 8]).get_operation();
    let sh = f.push(sh);
    f.ret(sh);
    scalarize::promote(&mut f.ctx, f.func);
    // insert: ext(z), (scalar h is widened by the lowerer, not here); shuffle:
    // its operand is the insert's trunc. Folded => no ext of that trunc.
    let ext_of_trunc = f
        .ops()
        .into_iter()
        .filter(|&o| Operation::get_op::<FPExtOp>(o, &f.ctx).is_some())
        .filter(|&o| {
            let src = o.deref(&f.ctx).get_operand(&f.ctx, 0);
            src.defining_op()
                .is_some_and(|d| Operation::get_op::<FPTruncOp>(d, &f.ctx).is_some())
        })
        .count();
    assert_eq!(ext_of_trunc, 0, "fpext(fptrunc(exact)) not folded");
    assert_eq!(f.count::<ShuffleVectorOp>(), 1);
}
