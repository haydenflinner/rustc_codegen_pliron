//! `BuilderMethods` emitting pliron LLVM-dialect operations.

use std::num::NonZero;
use std::ops::Deref;

use pliron::basic_block::BasicBlock;
use pliron::builtin::attributes::{IntegerAttr, StringAttr};
use pliron::builtin::op_interfaces::CallOpCallable;
use pliron::builtin::types::IntegerType;
use pliron::context::{Context, Ptr};
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, TypedHandle};
use pliron::utils::apint::APInt;
use pliron::value::Value;
use pliron_llvm::attributes::{
    AtomicOrderingAttr, FCmpPredicateAttr, FastmathFlagsAttr, ICmpPredicateAttr, SyncScopeAttr,
};
use pliron_llvm::op_interfaces::{BinArithOp, CastOpInterface};
use pliron_llvm::ops::*;
use pliron_llvm::types::FuncType;
use rustc_abi::{Align, BackendRepr, HasDataLayout, Scalar, Size, TargetDataLayout, WrappingRange};
use rustc_codegen_ssa::MemFlags;
use rustc_codegen_ssa::common::{
    AtomicRmwBinOp, IntPredicate, RealPredicate, SynchronizationScope,
};
use rustc_codegen_ssa::mir::operand::{OperandRef, OperandValue};
use rustc_codegen_ssa::mir::place::PlaceRef;
use rustc_codegen_ssa::traits::*;
use rustc_middle::ty::layout::{
    FnAbiError, FnAbiOfHelpers, FnAbiRequest, HasTyCtxt, HasTypingEnv, LayoutError,
    LayoutOfHelpers, TyAndLayout,
};
use rustc_middle::ty::{self, AtomicOrdering, Instance, Ty, TyCtxt};
use rustc_span::Span;
use rustc_target::callconv::FnAbi;
use rustc_target::spec::{HasTargetSpec, Target};

use crate::context::{CallInfo, CodegenCx, ConstVal, Exts};
use crate::types::TyK;

pub struct Builder<'a, 'tcx> {
    pub cx: &'a CodegenCx<'tcx>,
    pub block: Ptr<BasicBlock>,
}

impl<'a, 'tcx> Deref for Builder<'a, 'tcx> {
    type Target = CodegenCx<'tcx>;
    fn deref(&self) -> &Self::Target {
        self.cx
    }
}

impl<'a, 'tcx> BackendTypes for Builder<'a, 'tcx> {
    type Function = Ptr<Operation>;
    type BasicBlock = Ptr<BasicBlock>;
    type Funclet = ();
    type Value = Value;
    type Type = TypeHandle;
    type FunctionSignature = TypeHandle;
    type DIScope = ();
    type DILocation = ();
    type DIVariable = ();
}

impl<'a, 'tcx> HasTyCtxt<'tcx> for Builder<'a, 'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.cx.tcx
    }
}
impl<'a, 'tcx> HasDataLayout for Builder<'a, 'tcx> {
    fn data_layout(&self) -> &TargetDataLayout {
        self.cx.data_layout()
    }
}
impl<'a, 'tcx> HasTargetSpec for Builder<'a, 'tcx> {
    fn target_spec(&self) -> &Target {
        self.cx.target_spec()
    }
}
impl<'a, 'tcx> HasTypingEnv<'tcx> for Builder<'a, 'tcx> {
    fn typing_env(&self) -> ty::TypingEnv<'tcx> {
        self.cx.typing_env()
    }
}
impl<'a, 'tcx> LayoutOfHelpers<'tcx> for Builder<'a, 'tcx> {
    fn handle_layout_err(&self, err: LayoutError<'tcx>, span: Span, ty: Ty<'tcx>) -> ! {
        self.cx.handle_layout_err(err, span, ty)
    }
}
impl<'a, 'tcx> FnAbiOfHelpers<'tcx> for Builder<'a, 'tcx> {
    fn handle_fn_abi_err(&self, err: FnAbiError<'tcx>, span: Span, req: FnAbiRequest<'tcx>) -> ! {
        self.cx.handle_fn_abi_err(err, span, req)
    }
}

fn ord(o: AtomicOrdering) -> AtomicOrderingAttr {
    match o {
        AtomicOrdering::Relaxed => AtomicOrderingAttr::Monotonic,
        AtomicOrdering::Acquire => AtomicOrderingAttr::Acquire,
        AtomicOrdering::Release => AtomicOrderingAttr::Release,
        AtomicOrdering::AcqRel => AtomicOrderingAttr::AcqRel,
        AtomicOrdering::SeqCst => AtomicOrderingAttr::SeqCst,
    }
}

impl<'a, 'tcx> Builder<'a, 'tcx> {
    fn push(&mut self, op: Ptr<Operation>) -> Option<Value> {
        let ctx = self.cx.pctx.borrow();
        op.insert_at_back(self.block, &ctx);
        let o = op.deref(&ctx);
        (o.get_num_results() > 0).then(|| o.get_result(0))
    }

    fn mk_op<T: Op>(&mut self, f: impl FnOnce(&mut Context) -> T) -> Ptr<Operation> {
        let p = f(&mut self.cx.pctx.borrow_mut()).get_operation();
        self.push(p);
        p
    }

    /// Record a pointer load whose layout excludes null (`!nonnull` in LLVM).
    fn scalar_nonnull(&mut self, load: Value, s: rustc_abi::Scalar) {
        if matches!(s.primitive(), rustc_abi::Primitive::Pointer(_))
            && !s.valid_range(&*self).contains(0)
        {
            self.nonnull_metadata(load);
        }
        if matches!(s.primitive(), rustc_abi::Primitive::Int(..)) && !s.is_always_valid(&*self) {
            let r = s.valid_range(&*self);
            self.range_metadata(load, r);
        }
    }

    fn mark_volatile(&mut self, volatile: bool) {
        if volatile {
            use pliron::linked_list::ContainsLinkedList;
            let tail = self.block.deref(&self.cx.pctx.borrow()).get_tail().unwrap();
            self.st.borrow_mut().volatile.insert(tail);
        }
    }

    pub(crate) fn mk<T: Op>(&mut self, f: impl FnOnce(&mut Context) -> T) -> Value {
        let p = f(&mut self.cx.pctx.borrow_mut()).get_operation();
        self.push(p).expect("op has no result")
    }

    fn bin<T: BinArithOp>(&mut self, a: Value, b: Value) -> Value {
        self.mk(|c| T::new(c, a, b))
    }

    /// Integer op that carries (empty) overflow flags, as the printer requires.
    fn binf<T: pliron_llvm::op_interfaces::IntBinArithOpWithOverflowFlag>(
        &mut self,
        a: Value,
        b: Value,
    ) -> Value {
        let flags = pliron_llvm::attributes::IntegerOverflowFlagsAttr {
            nsw: false,
            nuw: false,
        };
        self.mk(|c| T::new_with_overflow_flag(c, a, b, flags))
    }

    fn cast<T: CastOpInterface>(&mut self, v: Value, ty: TypeHandle) -> Value {
        self.mk(|c| T::new(c, v, ty))
    }

    pub fn intrinsic(&mut self, name: &str, ret: TypeHandle, args: &[Value]) -> Value {
        let tys: Vec<_> = args.iter().map(|a| self.ty_of(*a)).collect();
        let fty = self.type_func(&tys, ret);
        let p = {
            let mut c = self.cx.pctx.borrow_mut();
            let fty = TypedHandle::<FuncType>::from_handle(fty, &c).unwrap();
            CallIntrinsicOp::new(
                &mut c,
                StringAttr::new(name.to_string()),
                fty,
                args.to_vec(),
            )
            .get_operation()
        };
        self.st.borrow_mut().intrinsics.insert(p, name.to_string());
        self.push(p).unwrap_or_else(|| self.const_undef(ret))
    }

    pub fn call_raw(
        &mut self,
        fn_ty: TypeHandle,
        callee: Value,
        args: &[Value],
        exts: Exts,
    ) -> Value {
        let callable = match self.cval(callee) {
            Some(ConstVal::Sym { sym, off: 0 }) if self.st.borrow().funcs.contains_key(&sym) => {
                CallOpCallable::Direct(self.ident(&sym))
            }
            _ => CallOpCallable::Indirect(callee),
        };
        let p = {
            let mut c = self.cx.pctx.borrow_mut();
            let fty = TypedHandle::<FuncType>::from_handle(fn_ty, &c).unwrap();
            CallOp::new(&mut c, callable, fty, args.to_vec()).get_operation()
        };
        self.st
            .borrow_mut()
            .calls
            .insert(p, CallInfo { fn_ty, exts });
        self.st.borrow_mut().last_call = Some(p);
        let ret = match self.kind(fn_ty) {
            TyK::Func(r, ..) => r,
            _ => unreachable!(),
        };
        self.push(p).unwrap_or_else(|| self.const_undef(ret))
    }

    pub fn call_sym(&mut self, sym: &str, ret: TypeHandle, args: &[Value]) -> Value {
        let tys: Vec<_> = args.iter().map(|a| self.ty_of(*a)).collect();
        let fty = self.type_func(&tys, ret);
        self.declare_fn_sym(sym, fty, cranelift_module::Linkage::Import, Exts::default());
        let f = self.sym_addr(sym);
        self.call_raw(fty, f, args, Exts::default())
    }

    fn parent_fn(&self) -> Ptr<Operation> {
        let ctx = self.cx.pctx.borrow();
        self.block.deref(&ctx).get_parent_op(&ctx).unwrap()
    }
}

impl<'a, 'tcx> BuilderMethods<'a, 'tcx> for Builder<'a, 'tcx> {
    type CodegenCx = CodegenCx<'tcx>;

    fn build(cx: &'a CodegenCx<'tcx>, llbb: Ptr<BasicBlock>) -> Self {
        Builder { cx, block: llbb }
    }
    fn cx(&self) -> &CodegenCx<'tcx> {
        self.cx
    }
    fn llbb(&self) -> Ptr<BasicBlock> {
        self.block
    }
    fn set_span(&mut self, _span: Span) {}

    fn append_block(cx: &'a CodegenCx<'tcx>, llfn: Ptr<Operation>, _name: &str) -> Ptr<BasicBlock> {
        let mut ctx = cx.pctx.borrow_mut();
        let f = Operation::get_op::<FuncOp>(llfn, &ctx).unwrap();
        if f.get_entry_block(&ctx).is_none() {
            return f.get_or_create_entry_block(&mut ctx);
        }
        let region = llfn.deref(&ctx).get_region(0);
        let bb = BasicBlock::new(&mut ctx, None, vec![]);
        bb.insert_at_back(region, &ctx);
        bb
    }

    fn append_sibling_block(&mut self, name: &str) -> Ptr<BasicBlock> {
        let f = self.parent_fn();
        Self::append_block(self.cx, f, name)
    }

    fn switch_to_block(&mut self, llbb: Ptr<BasicBlock>) {
        self.block = llbb;
    }

    fn ret_void(&mut self) {
        self.mk_op(|c| ReturnOp::new(c, None));
    }
    fn ret(&mut self, v: Value) {
        self.mk_op(|c| ReturnOp::new(c, Some(v)));
    }
    fn br(&mut self, dest: Ptr<BasicBlock>) {
        self.mk_op(|c| BrOp::new(c, dest, vec![]));
    }
    fn cond_br(&mut self, cond: Value, then_llbb: Ptr<BasicBlock>, else_llbb: Ptr<BasicBlock>) {
        self.mk_op(|c| CondBrOp::new(c, cond, then_llbb, vec![], else_llbb, vec![]));
    }

    fn cond_br_with_expect(
        &mut self,
        cond: Value,
        then_llbb: Ptr<BasicBlock>,
        else_llbb: Ptr<BasicBlock>,
        expect: Option<bool>,
    ) {
        let op = self.mk_op(|c| CondBrOp::new(c, cond, then_llbb, vec![], else_llbb, vec![]));
        if let Some(e) = expect {
            self.st.borrow_mut().expect.insert(op, e);
        }
    }

    fn switch(
        &mut self,
        v: Value,
        else_llbb: Ptr<BasicBlock>,
        cases: impl ExactSizeIterator<Item = (u128, Ptr<BasicBlock>)>,
    ) {
        let cases: Vec<_> = cases.collect();
        if cases.is_empty() {
            return self.br(else_llbb);
        }
        let ty = self.val_ty(v);
        // wasm.rs has no SwitchOp lowering; it gets the compare chain.
        if crate::pass_enabled("PLIRON_SWITCH") && !self.cx.tcx.sess.target.is_like_wasm {
            let w = self.int_width(ty) as usize;
            let cases = {
                let c = self.cx.pctx.borrow();
                let ity = TypedHandle::<IntegerType>::from_handle(ty, &c).unwrap();
                cases
                    .into_iter()
                    .map(|(val, dest)| {
                        let val = if w < 128 {
                            val & ((1u128 << w) - 1)
                        } else {
                            val
                        };
                        SwitchCase {
                            value: IntegerAttr::new(
                                ity,
                                APInt::from_u128(val, NonZero::new(w).unwrap()),
                            ),
                            dest,
                            dest_opds: vec![],
                        }
                    })
                    .collect::<Vec<_>>()
            };
            self.mk_op(|c| SwitchOp::new(c, v, else_llbb, vec![], cases));
            return;
        }
        let n = cases.len();
        for (i, (val, dest)) in cases.into_iter().enumerate() {
            let c = self.const_uint_big(ty, val);
            let cmp = self.icmp(IntPredicate::IntEQ, v, c);
            if i + 1 == n {
                self.cond_br(cmp, dest, else_llbb);
            } else {
                let next = self.append_sibling_block("switch");
                self.cond_br(cmp, dest, next);
                self.switch_to_block(next);
            }
        }
    }

    fn invoke(
        &mut self,
        llty: TypeHandle,
        fn_attrs: Option<&rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrs>,
        fn_abi: Option<&FnAbi<'tcx, Ty<'tcx>>>,
        llfn: Value,
        return_slot: ReturnSlot<Value>,
        args: &[Value],
        then: Ptr<BasicBlock>,
        catch: Ptr<BasicBlock>,
        funclet: Option<&()>,
        instance: Option<Instance<'tcx>>,
    ) -> Value {
        self.st.borrow_mut().last_call = None;
        let r = self.call(
            llty,
            fn_attrs,
            fn_abi,
            llfn,
            return_slot,
            args,
            funclet,
            instance,
        );
        let last = self.st.borrow_mut().last_call.take();
        if let Some(op) = last {
            self.st.borrow_mut().invokes.insert(op, (catch, false));
        }
        self.br(then);
        r
    }

    fn unreachable(&mut self) {
        self.mk_op(|c| UnreachableOp::new(c));
    }

    fn add(&mut self, a: Value, b: Value) -> Value {
        self.binf::<AddOp>(a, b)
    }
    fn fadd(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FAddOp>(a, b)
    }
    fn fadd_fast(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FAddOp>(a, b)
    }
    fn fadd_algebraic(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FAddOp>(a, b)
    }
    fn sub(&mut self, a: Value, b: Value) -> Value {
        self.binf::<SubOp>(a, b)
    }
    fn fsub(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FSubOp>(a, b)
    }
    fn fsub_fast(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FSubOp>(a, b)
    }
    fn fsub_algebraic(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FSubOp>(a, b)
    }
    fn mul(&mut self, a: Value, b: Value) -> Value {
        self.binf::<MulOp>(a, b)
    }
    fn fmul(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FMulOp>(a, b)
    }
    fn fmul_fast(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FMulOp>(a, b)
    }
    fn fmul_algebraic(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FMulOp>(a, b)
    }
    fn udiv(&mut self, a: Value, b: Value) -> Value {
        self.bin::<UDivOp>(a, b)
    }
    fn exactudiv(&mut self, a: Value, b: Value) -> Value {
        self.bin::<UDivOp>(a, b)
    }
    fn sdiv(&mut self, a: Value, b: Value) -> Value {
        self.bin::<SDivOp>(a, b)
    }
    fn exactsdiv(&mut self, a: Value, b: Value) -> Value {
        self.bin::<SDivOp>(a, b)
    }
    fn fdiv(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FDivOp>(a, b)
    }
    fn fdiv_fast(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FDivOp>(a, b)
    }
    fn fdiv_algebraic(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FDivOp>(a, b)
    }
    fn urem(&mut self, a: Value, b: Value) -> Value {
        self.bin::<URemOp>(a, b)
    }
    fn srem(&mut self, a: Value, b: Value) -> Value {
        self.bin::<SRemOp>(a, b)
    }
    fn frem(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FRemOp>(a, b)
    }
    fn frem_fast(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FRemOp>(a, b)
    }
    fn frem_algebraic(&mut self, a: Value, b: Value) -> Value {
        self.bin::<FRemOp>(a, b)
    }
    fn shl(&mut self, a: Value, b: Value) -> Value {
        self.binf::<ShlOp>(a, b)
    }
    fn lshr(&mut self, a: Value, b: Value) -> Value {
        self.bin::<LShrOp>(a, b)
    }
    fn ashr(&mut self, a: Value, b: Value) -> Value {
        self.bin::<AShrOp>(a, b)
    }
    fn and(&mut self, a: Value, b: Value) -> Value {
        self.bin::<AndOp>(a, b)
    }
    fn or(&mut self, a: Value, b: Value) -> Value {
        self.bin::<OrOp>(a, b)
    }
    fn xor(&mut self, a: Value, b: Value) -> Value {
        self.bin::<XorOp>(a, b)
    }

    fn neg(&mut self, v: Value) -> Value {
        let z = self.const_null(self.val_ty(v));
        self.sub(z, v)
    }
    fn fneg(&mut self, v: Value) -> Value {
        self.mk(|c| FNegOp::new_with_fast_math_flags(c, v, FastmathFlagsAttr::default()))
    }
    fn not(&mut self, v: Value) -> Value {
        let ones = self.const_int(self.val_ty(v), -1);
        self.xor(v, ones)
    }

    fn checked_binop(
        &mut self,
        oop: OverflowOp,
        ty: Ty<'tcx>,
        lhs: Value,
        rhs: Value,
    ) -> (Value, Value) {
        let signed = ty.is_signed();
        if !signed && matches!(oop, OverflowOp::Add | OverflowOp::Sub) {
            let (res, of) = match oop {
                OverflowOp::Add => {
                    let r = self.add(lhs, rhs);
                    (r, self.icmp(IntPredicate::IntULT, r, lhs))
                }
                _ => {
                    let r = self.sub(lhs, rhs);
                    (r, self.icmp(IntPredicate::IntULT, lhs, rhs))
                }
            };
            return (res, of);
        }
        let op = match oop {
            OverflowOp::Add => "add",
            OverflowOp::Sub => "sub",
            OverflowOp::Mul => "mul",
        };
        let name = format!("llvm.{}{op}.with.overflow", if signed { "s" } else { "u" });
        let ity = self.val_ty(lhs);
        let rty = self.type_struct(&[ity, self.type_i1()], false);
        let res = self.intrinsic(&name, rty, &[lhs, rhs]);
        (self.extract_value(res, 0), self.extract_value(res, 1))
    }

    fn from_immediate(&mut self, val: Value) -> Value {
        if matches!(self.kind(self.val_ty(val)), TyK::Int(1)) {
            self.zext(val, self.type_i8())
        } else {
            val
        }
    }
    fn to_immediate_scalar(&mut self, val: Value, scalar: Scalar) -> Value {
        if scalar.is_bool() {
            self.trunc(val, self.type_i1())
        } else {
            val
        }
    }

    fn alloca(&mut self, size: Size, align: Align) -> Value {
        let arr = self.type_array(self.type_i8(), size.bytes());
        let one = self.const_i32(1);
        // Like LLVM, hoist allocas to the entry block so they dominate every use.
        let op = AllocaOp::new(&mut self.cx.pctx.borrow_mut(), arr, one, 0).get_operation();
        let v = {
            use pliron::linked_list::ContainsLinkedList;
            let ctx = self.cx.pctx.borrow();
            let region = self
                .block
                .deref(&ctx)
                .get_parent_region()
                .expect("block has no region");
            let entry = region
                .deref(&ctx)
                .get_head()
                .expect("function has no entry block");
            op.insert_at_front(entry, &ctx);
            op.deref(&ctx).get_result(0)
        };
        self.st
            .borrow_mut()
            .allocas
            .insert(v, (size.bytes(), align.bytes()));
        v
    }

    fn alloca_with_ty(&mut self, layout: TyAndLayout<'tcx>) -> Value {
        self.alloca(layout.size, layout.align.abi)
    }

    fn load(&mut self, ty: TypeHandle, ptr: Value, _align: Align) -> Value {
        self.mk(|c| LoadOp::new(c, ptr, ty))
    }
    fn volatile_load(&mut self, ty: TypeHandle, ptr: Value, _align: Align) -> Value {
        let v = self.mk(|c| LoadOp::new(c, ptr, ty));
        self.mark_volatile(true);
        v
    }
    fn atomic_load(
        &mut self,
        ty: TypeHandle,
        ptr: Value,
        order: AtomicOrdering,
        _volatile: bool,
        _size: Size,
    ) -> Value {
        self.mk(|c| AtomicLoadOp::new(c, ptr, ty, ord(order), SyncScopeAttr::System))
    }

    fn load_operand(&mut self, place: PlaceRef<'tcx, Value>) -> OperandRef<'tcx, Value> {
        if place.layout.is_zst() {
            return OperandRef::zero_sized(place.layout);
        }
        let val = if place.val.llextra.is_some() {
            OperandValue::Ref(place.val)
        } else if matches!(
            place.layout.backend_repr,
            BackendRepr::Scalar(_) | BackendRepr::SimdVector { .. }
        ) {
            let llty = self.backend_type(place.layout);
            let v = self.load(llty, place.val.llval, place.val.align);
            if let BackendRepr::Scalar(s) = place.layout.backend_repr {
                self.scalar_nonnull(v, s);
            }
            OperandValue::Immediate(match place.layout.backend_repr {
                BackendRepr::Scalar(s) => self.to_immediate_scalar(v, s),
                _ => v,
            })
        } else if let BackendRepr::ScalarPair { a, b, b_offset } = place.layout.backend_repr {
            let t0 = self.scalar_pair_element_backend_type(place.layout, 0, false);
            let t1 = self.scalar_pair_element_backend_type(place.layout, 1, false);
            let v0 = self.load(t0, place.val.llval, place.val.align);
            self.scalar_nonnull(v0, a);
            let v0 = self.to_immediate_scalar(v0, a);
            let off = self.const_usize(b_offset.bytes());
            let p1 = self.inbounds_ptradd(place.val.llval, off);
            let v1 = self.load(t1, p1, place.val.align.restrict_for_offset(b_offset));
            self.scalar_nonnull(v1, b);
            let v1 = self.to_immediate_scalar(v1, b);
            OperandValue::Pair(v0, v1)
        } else {
            OperandValue::Ref(place.val)
        };
        OperandRef {
            val,
            layout: place.layout,
            move_annotation: None,
        }
    }

    fn write_operand_repeatedly(
        &mut self,
        elem: OperandRef<'tcx, Value>,
        count: u64,
        dest: PlaceRef<'tcx, Value>,
    ) {
        let isize = self.type_isize();
        let ctr = self.alloca(Size::from_bytes(8), Align::EIGHT);
        let zero = self.const_usize(0);
        self.store(zero, ctr, Align::EIGHT);
        let head = self.append_sibling_block("repeat_head");
        let body = self.append_sibling_block("repeat_body");
        let next = self.append_sibling_block("repeat_next");
        self.br(head);
        self.switch_to_block(head);
        let i = self.load(isize, ctr, Align::EIGHT);
        let n = self.const_usize(count);
        let keep = self.icmp(IntPredicate::IntULT, i, n);
        self.cond_br(keep, body, next);
        self.switch_to_block(body);
        let sz = self.const_usize(elem.layout.size.bytes());
        let off = self.mul(i, sz);
        let p = self.inbounds_ptradd(dest.val.llval, off);
        let align = dest.val.align.restrict_for_offset(elem.layout.size);
        elem.val
            .store(self, PlaceRef::new_sized_aligned(p, elem.layout, align));
        let one = self.const_usize(1);
        let i2 = self.add(i, one);
        self.store(i2, ctr, Align::EIGHT);
        self.br(head);
        self.switch_to_block(next);
    }

    fn range_metadata(&mut self, load: Value, range: WrappingRange) {
        if range.start == 0
            && range.end == 1
            && crate::pass_enabled("PLIRON_BOOLRANGE")
            && let Some(op) = load.defining_op()
        {
            self.st.borrow_mut().bool01.insert(op);
        }
    }
    fn nonnull_metadata(&mut self, load: Value) {
        if let Some(op) = load.defining_op() {
            self.st.borrow_mut().nonnull.insert(op);
        }
    }

    fn store(&mut self, val: Value, ptr: Value, _align: Align) -> Value {
        self.mk_op(|c| StoreOp::new(c, val, ptr));
        val
    }
    fn store_with_flags(&mut self, val: Value, ptr: Value, align: Align, flags: MemFlags) -> Value {
        self.store(val, ptr, align);
        self.mark_volatile(flags.contains(MemFlags::VOLATILE));
        val
    }
    fn atomic_store(
        &mut self,
        val: Value,
        ptr: Value,
        order: AtomicOrdering,
        _volatile: bool,
        _size: Size,
    ) {
        self.mk_op(|c| AtomicStoreOp::new(c, val, ptr, ord(order), SyncScopeAttr::System));
    }

    fn gep(&mut self, ty: TypeHandle, ptr: Value, indices: &[Value]) -> Value {
        let idx = indices.iter().map(|v| GepIndex::Value(*v)).collect();
        self.mk(|c| GetElementPtrOp::new(c, ptr, idx, ty))
    }
    fn inbounds_gep(&mut self, ty: TypeHandle, ptr: Value, indices: &[Value]) -> Value {
        let v = self.gep(ty, ptr, indices);
        if let Some(op) = v.defining_op() {
            self.st.borrow_mut().inbounds.insert(op);
        }
        v
    }

    fn trunc(&mut self, v: Value, t: TypeHandle) -> Value {
        self.cast::<TruncOp>(v, t)
    }
    fn sext(&mut self, v: Value, t: TypeHandle) -> Value {
        self.cast::<SExtOp>(v, t)
    }
    fn zext(&mut self, v: Value, t: TypeHandle) -> Value {
        use pliron_llvm::op_interfaces::CastOpWithNNegInterface;
        self.mk(|c| ZExtOp::new_with_nneg(c, v, t, false))
    }
    fn fptoui_sat(&mut self, v: Value, t: TypeHandle) -> Value {
        self.intrinsic("llvm.fptoui.sat", t, &[v])
    }
    fn fptosi_sat(&mut self, v: Value, t: TypeHandle) -> Value {
        self.intrinsic("llvm.fptosi.sat", t, &[v])
    }
    fn fptoui(&mut self, v: Value, t: TypeHandle) -> Value {
        self.cast::<FPToUIOp>(v, t)
    }
    fn fptosi(&mut self, v: Value, t: TypeHandle) -> Value {
        self.cast::<FPToSIOp>(v, t)
    }
    fn uitofp(&mut self, v: Value, t: TypeHandle) -> Value {
        self.cast::<UIToFPOp>(v, t)
    }
    fn sitofp(&mut self, v: Value, t: TypeHandle) -> Value {
        self.cast::<SIToFPOp>(v, t)
    }
    fn fptrunc(&mut self, v: Value, t: TypeHandle) -> Value {
        self.cast::<FPTruncOp>(v, t)
    }
    fn fpext(&mut self, v: Value, t: TypeHandle) -> Value {
        self.cast::<FPExtOp>(v, t)
    }
    fn ptrtoint(&mut self, v: Value, t: TypeHandle) -> Value {
        self.cast::<PtrToIntOp>(v, t)
    }
    fn inttoptr(&mut self, v: Value, t: TypeHandle) -> Value {
        self.cast::<IntToPtrOp>(v, t)
    }
    fn bitcast(&mut self, v: Value, t: TypeHandle) -> Value {
        if self.val_ty(v) == t {
            v
        } else {
            self.cast::<BitcastOp>(v, t)
        }
    }
    fn intcast(&mut self, v: Value, t: TypeHandle, is_signed: bool) -> Value {
        let (fw, tw) = (self.int_width(self.val_ty(v)), self.int_width(t));
        if fw < tw {
            if is_signed {
                self.sext(v, t)
            } else {
                self.zext(v, t)
            }
        } else if fw > tw {
            self.trunc(v, t)
        } else {
            v
        }
    }
    fn pointercast(&mut self, v: Value, _t: TypeHandle) -> Value {
        v
    }

    fn icmp(&mut self, op: IntPredicate, a: Value, b: Value) -> Value {
        use IntPredicate::*;
        let p = match op {
            IntEQ => ICmpPredicateAttr::EQ,
            IntNE => ICmpPredicateAttr::NE,
            IntUGT => ICmpPredicateAttr::UGT,
            IntUGE => ICmpPredicateAttr::UGE,
            IntULT => ICmpPredicateAttr::ULT,
            IntULE => ICmpPredicateAttr::ULE,
            IntSGT => ICmpPredicateAttr::SGT,
            IntSGE => ICmpPredicateAttr::SGE,
            IntSLT => ICmpPredicateAttr::SLT,
            IntSLE => ICmpPredicateAttr::SLE,
        };
        self.mk(|c| ICmpOp::new(c, p, a, b))
    }
    fn fcmp(&mut self, op: RealPredicate, a: Value, b: Value) -> Value {
        use RealPredicate::*;
        let p = match op {
            RealPredicateFalse => FCmpPredicateAttr::False,
            RealOEQ => FCmpPredicateAttr::OEQ,
            RealOGT => FCmpPredicateAttr::OGT,
            RealOGE => FCmpPredicateAttr::OGE,
            RealOLT => FCmpPredicateAttr::OLT,
            RealOLE => FCmpPredicateAttr::OLE,
            RealONE => FCmpPredicateAttr::ONE,
            RealORD => FCmpPredicateAttr::ORD,
            RealUNO => FCmpPredicateAttr::UNO,
            RealUEQ => FCmpPredicateAttr::UEQ,
            RealUGT => FCmpPredicateAttr::UGT,
            RealUGE => FCmpPredicateAttr::UGE,
            RealULT => FCmpPredicateAttr::ULT,
            RealULE => FCmpPredicateAttr::ULE,
            RealUNE => FCmpPredicateAttr::UNE,
            RealPredicateTrue => FCmpPredicateAttr::True,
        };
        self.mk(|c| FCmpOp::new(c, p, a, b))
    }

    fn memcpy(
        &mut self,
        dst: Value,
        _dst_align: Align,
        src: Value,
        _src_align: Align,
        size: Value,
        flags: MemFlags,
        _tt: Option<rustc_ast::expand::typetree::FncTree>,
    ) {
        let v = self.type_void();
        self.intrinsic("llvm.memcpy", v, &[dst, src, size]);
        self.mark_volatile(flags.contains(MemFlags::VOLATILE));
    }
    fn memmove(
        &mut self,
        dst: Value,
        _dst_align: Align,
        src: Value,
        _src_align: Align,
        size: Value,
        flags: MemFlags,
    ) {
        let v = self.type_void();
        self.intrinsic("llvm.memmove", v, &[dst, src, size]);
        self.mark_volatile(flags.contains(MemFlags::VOLATILE));
    }
    fn memset(
        &mut self,
        ptr: Value,
        fill_byte: Value,
        size: Value,
        _align: Align,
        flags: MemFlags,
    ) {
        let v = self.type_void();
        self.intrinsic("llvm.memset", v, &[ptr, fill_byte, size]);
        self.mark_volatile(flags.contains(MemFlags::VOLATILE));
    }

    fn vscale(&mut self, _ty: TypeHandle) -> Value {
        panic!("vscale is not supported by the pliron backend")
    }

    fn select(&mut self, cond: Value, then_val: Value, else_val: Value) -> Value {
        self.mk(|c| SelectOp::new(c, cond, then_val, else_val))
    }

    fn va_arg(&mut self, _list: Value, _ty: TypeHandle) -> Value {
        self.tcx
            .dcx()
            .fatal("va_arg is not supported by the pliron backend yet")
    }

    fn extract_element(&mut self, vec: Value, idx: Value) -> Value {
        self.mk(|c| ExtractElementOp::new(c, vec, idx))
    }
    fn vector_splat(&mut self, num_elts: usize, elt: Value) -> Value {
        let ty = self.type_vector(self.val_ty(elt), num_elts as u64);
        if self.native(ty) {
            return self.intrinsic("pliron.vsplat", ty, &[elt]);
        }
        let mut v = self.const_undef(ty);
        for i in 0..num_elts {
            let idx = self.const_i32(i as i32);
            v = self.mk(|c| InsertElementOp::new(c, v, elt, idx));
        }
        v
    }
    fn extract_value(&mut self, agg: Value, idx: u64) -> Value {
        self.mk(|c| ExtractValueOp::new(c, agg, vec![idx as u32]).unwrap())
    }
    fn insert_value(&mut self, agg: Value, elt: Value, idx: u64) -> Value {
        self.mk(|c| InsertValueOp::new(c, agg, elt, vec![idx as u32]))
    }

    fn set_personality_fn(&mut self, _personality: Ptr<Operation>) {}
    fn cleanup_landing_pad(&mut self, _pers_fn: Ptr<Operation>) -> (Value, Value) {
        let ptr = self.type_ptr();
        let exn = self.intrinsic("pliron.eh.exn", ptr, &[]);
        (exn, self.const_i32(0))
    }
    fn filter_landing_pad(&mut self, pers_fn: Ptr<Operation>) {
        self.cleanup_landing_pad(pers_fn);
    }
    fn resume(&mut self, exn0: Value, _exn1: Value) {
        let void = self.type_void();
        self.call_sym("_Unwind_Resume", void, &[exn0]);
        self.unreachable();
    }
    fn cleanup_pad(&mut self, _parent: Option<Value>, _args: &[Value]) {}
    fn cleanup_ret(&mut self, _funclet: &(), _unwind: Option<Ptr<BasicBlock>>) {
        self.unreachable();
    }
    fn catch_pad(&mut self, _parent: Value, _args: &[Value]) {}
    fn catch_switch(
        &mut self,
        _parent: Option<Value>,
        _unwind: Option<Ptr<BasicBlock>>,
        _handlers: &[Ptr<BasicBlock>],
    ) -> Value {
        panic!("funclets are not supported by the pliron backend")
    }
    fn get_funclet_cleanuppad(&self, _funclet: &()) -> Value {
        panic!("funclets are not supported by the pliron backend")
    }

    fn atomic_cmpxchg(
        &mut self,
        dst: Value,
        cmp: Value,
        src: Value,
        order: AtomicOrdering,
        failure_order: AtomicOrdering,
        _weak: bool,
    ) -> (Value, Value) {
        let r = self.mk(|c| {
            AtomicCmpxchgOp::new(
                c,
                dst,
                cmp,
                src,
                ord(order),
                ord(failure_order),
                SyncScopeAttr::System,
            )
        });
        (self.extract_value(r, 0), self.extract_value(r, 1))
    }

    fn atomic_rmw(
        &mut self,
        op: AtomicRmwBinOp,
        dst: Value,
        src: Value,
        order: AtomicOrdering,
        _ret_ptr: bool,
    ) -> Value {
        use pliron_llvm::attributes::AtomicRmwKindAttr as K;
        let kind = match op {
            AtomicRmwBinOp::AtomicXchg => K::Xchg,
            AtomicRmwBinOp::AtomicAdd => K::Add,
            AtomicRmwBinOp::AtomicSub => K::Sub,
            AtomicRmwBinOp::AtomicAnd => K::And,
            AtomicRmwBinOp::AtomicNand => K::Nand,
            AtomicRmwBinOp::AtomicOr => K::Or,
            AtomicRmwBinOp::AtomicXor => K::Xor,
            AtomicRmwBinOp::AtomicMax => K::Max,
            AtomicRmwBinOp::AtomicMin => K::Min,
            AtomicRmwBinOp::AtomicUMax => K::UMax,
            AtomicRmwBinOp::AtomicUMin => K::UMin,
        };
        let p = {
            let mut c = self.cx.pctx.borrow_mut();
            AtomicRmwOp::new(&mut c, dst, src, kind, ord(order), SyncScopeAttr::System)
                .get_operation()
        };
        self.st.borrow_mut().rmw.insert(p, op);
        self.push(p).unwrap()
    }

    fn atomic_fence(&mut self, order: AtomicOrdering, scope: SynchronizationScope) {
        let s = match scope {
            SynchronizationScope::SingleThread => SyncScopeAttr::SingleThread,
            SynchronizationScope::CrossThread => SyncScopeAttr::System,
        };
        self.mk_op(|c| FenceOp::new(c, ord(order), s));
    }

    fn set_invariant_load(&mut self, _load: Value) {}
    fn lifetime_start(&mut self, _ptr: Value, _size: Size) {}
    fn lifetime_end(&mut self, _ptr: Value, _size: Size) {}

    fn call(
        &mut self,
        llty: TypeHandle,
        _fn_attrs: Option<&rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrs>,
        fn_abi: Option<&FnAbi<'tcx, Ty<'tcx>>>,
        llfn: Value,
        return_slot: ReturnSlot<Value>,
        args: &[Value],
        _funclet: Option<&()>,
        _instance: Option<Instance<'tcx>>,
    ) -> Value {
        let mut full = Vec::with_capacity(args.len() + 1);
        if let ReturnSlot::Indirect(p) = return_slot {
            full.push(p);
        }
        full.extend_from_slice(args);
        let exts = fn_abi.map(crate::type_of::exts_of).unwrap_or_default();
        self.call_raw(llty, llfn, &full, exts)
    }

    fn tail_call(
        &mut self,
        llty: TypeHandle,
        fn_attrs: Option<&rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrs>,
        fn_abi: &FnAbi<'tcx, Ty<'tcx>>,
        llfn: Value,
        return_slot: ReturnSlot<Value>,
        args: &[Value],
        funclet: Option<&()>,
        instance: Option<Instance<'tcx>>,
    ) {
        let r = self.call(
            llty,
            fn_attrs,
            Some(fn_abi),
            llfn,
            return_slot,
            args,
            funclet,
            instance,
        );
        match self.kind(llty) {
            TyK::Func(ret, ..) if matches!(self.kind(ret), TyK::Void) => self.ret_void(),
            _ => self.ret(r),
        }
    }

    fn apply_attrs_to_cleanup_callsite(&mut self, _llret: Value) {}
}

impl<'a, 'tcx> AbiBuilderMethods for Builder<'a, 'tcx> {
    fn get_param(&mut self, index: usize) -> Value {
        let f = self.parent_fn();
        let ctx = self.cx.pctx.borrow();
        let entry = Operation::get_op::<FuncOp>(f, &ctx)
            .unwrap()
            .get_entry_block(&ctx)
            .unwrap();
        entry.deref(&ctx).get_argument(index)
    }
}

impl<'a, 'tcx> ArgAbiBuilderMethods<'tcx> for Builder<'a, 'tcx> {
    fn store_fn_arg(
        &mut self,
        arg_abi: &rustc_target::callconv::ArgAbi<'tcx, Ty<'tcx>>,
        idx: &mut usize,
        dst: PlaceRef<'tcx, Value>,
    ) {
        use rustc_target::callconv::PassMode;
        let mut next = |bx: &mut Self| {
            let v = bx.get_param(*idx);
            *idx += 1;
            v
        };
        match &arg_abi.mode {
            PassMode::Ignore => {}
            PassMode::Pair(..) => {
                let a = next(self);
                let b = next(self);
                OperandValue::Pair(a, b).store(self, dst);
            }
            PassMode::Indirect {
                meta_attrs: Some(_),
                ..
            } => {
                let a = next(self);
                let b = next(self);
                OperandValue::Ref(rustc_codegen_ssa::mir::place::PlaceValue {
                    llval: a,
                    llextra: Some(b),
                    align: arg_abi.layout.align.abi,
                })
                .store(self, dst);
            }
            PassMode::Direct(_)
            | PassMode::Indirect {
                meta_attrs: None, ..
            }
            | PassMode::Cast { .. } => {
                if let PassMode::Cast { pad_i32_count, .. } = arg_abi.mode {
                    for _ in 0..pad_i32_count {
                        next(self);
                    }
                }
                let v = next(self);
                self.store_arg(arg_abi, v, dst);
            }
        }
    }

    fn store_arg(
        &mut self,
        arg_abi: &rustc_target::callconv::ArgAbi<'tcx, Ty<'tcx>>,
        val: Value,
        dst: PlaceRef<'tcx, Value>,
    ) {
        use rustc_target::callconv::PassMode;
        match &arg_abi.mode {
            PassMode::Ignore => {}
            PassMode::Indirect {
                meta_attrs: None, ..
            } => {
                let place = PlaceRef::new_sized(val, arg_abi.layout);
                OperandValue::Ref(place.val).store(self, dst);
            }
            PassMode::Cast { cast, .. } => {
                // The cast type may be larger than the Rust type; go through a scratch slot.
                let size = arg_abi.layout.size.max(cast.size(self));
                let align = arg_abi.layout.align.abi.max(cast.align(self));
                let scratch = self.alloca(size, align);
                self.store(val, scratch, align);
                let sz = self.const_usize(arg_abi.layout.size.bytes());
                self.memcpy(
                    dst.val.llval,
                    dst.val.align,
                    scratch,
                    align,
                    sz,
                    MemFlags::empty(),
                    None,
                );
            }
            _ => {
                OperandRef::from_immediate_or_packed_pair(self, val, arg_abi.layout)
                    .val
                    .store(self, dst);
            }
        }
    }
}

impl<'a, 'tcx> CoverageInfoBuilderMethods<'tcx> for Builder<'a, 'tcx> {
    fn add_coverage(
        &mut self,
        _instance: Instance<'tcx>,
        _kind: &rustc_middle::mir::coverage::CoverageKind,
    ) {
    }
}

impl<'a, 'tcx> DebugInfoBuilderMethods<'tcx> for Builder<'a, 'tcx> {
    fn dbg_scope_fn(
        &mut self,
        _instance: Instance<'tcx>,
        _fn_abi: &FnAbi<'tcx, Ty<'tcx>>,
        _llfn: Option<Ptr<Operation>>,
    ) {
    }
    fn dbg_create_lexical_block(&mut self, _pos: rustc_span::BytePos, _parent: ()) {}
    fn dbg_location_clone_with_discriminator(&mut self, _loc: (), _d: u32) -> Option<()> {
        Some(())
    }
    fn dbg_loc(&mut self, _scope: (), _inlined_at: Option<()>, _span: Span) {}
    fn extend_scope_to_file(&mut self, _scope: (), _file: &rustc_span::SourceFile) {}
    fn create_dbg_var(
        &mut self,
        _name: rustc_span::Symbol,
        _ty: Ty<'tcx>,
        _scope: (),
        _kind: rustc_codegen_ssa::mir::debuginfo::VariableKind,
        _span: Span,
    ) {
    }
    fn dbg_var_addr(
        &mut self,
        _dbg_var: (),
        _dbg_loc: (),
        _variable_alloca: Value,
        _direct_offset: Size,
        _indirect_offsets: &[Size],
        _fragment: &Option<std::ops::Range<Size>>,
    ) {
    }
    fn dbg_var_value(
        &mut self,
        _dbg_var: (),
        _dbg_loc: (),
        _value: Value,
        _direct_offset: Size,
        _indirect_offsets: &[Size],
        _fragment: &Option<std::ops::Range<Size>>,
    ) {
    }
    fn set_dbg_loc(&mut self, _dbg_loc: ()) {}
    fn clear_dbg_loc(&mut self) {}
    fn insert_reference_to_gdb_debug_scripts_section_global(&mut self) {}
    fn set_var_name(&mut self, _value: Value, _name: &str) {}
}

impl<'a, 'tcx> AsmBuilderMethods<'tcx> for Builder<'a, 'tcx> {
    fn codegen_inline_asm(
        &mut self,
        template: &[rustc_ast::InlineAsmTemplatePiece],
        operands: &[InlineAsmOperandRef<'tcx, Self>],
        options: rustc_ast::InlineAsmOptions,
        line_spans: &[Span],
        instance: Instance<'_>,
        dest: Option<Ptr<BasicBlock>>,
        _catch_funclet: Option<(Ptr<BasicBlock>, Option<&()>)>,
    ) {
        self.inline_asm(template, operands, options, line_spans, instance, dest)
    }
}

impl<'a, 'tcx> StaticBuilderMethods for Builder<'a, 'tcx> {
    fn get_static(&mut self, def_id: rustc_hir::def_id::DefId) -> Value {
        self.cx.get_static_addr(def_id)
    }
}
