//! Portable SIMD intrinsics, scalarized lane-by-lane into pliron
//! extractelement/insertelement + scalar ops. The Cranelift lowering
//! flattens vectors into per-lane values, so this costs nothing extra there.

use pliron::r#type::TypeHandle;
use pliron::value::Value;
use pliron_llvm::ops::InsertElementOp;
use rustc_codegen_ssa::common::{IntPredicate, RealPredicate, TypeKind};
use rustc_codegen_ssa::mir::operand::{OperandRef, OperandValue};
use rustc_codegen_ssa::traits::*;
use rustc_middle::ty::Ty;

use crate::builder::Builder;
use crate::context::ConstVal;

#[derive(Clone, Copy)]
struct Elem {
    signed: bool,
    float: bool,
}

impl<'a, 'tcx> Builder<'a, 'tcx> {
    fn elem_of(&self, t: Ty<'tcx>) -> (u64, Elem) {
        let (n, et) = t.simd_size_and_type(self.tcx);
        (
            n,
            Elem {
                signed: et.is_signed(),
                float: et.is_floating_point(),
            },
        )
    }

    /// Whether `ty` lowers to native Cranelift SIMD values.
    pub(crate) fn native(&self, ty: TypeHandle) -> bool {
        match self.cx.kind(ty) {
            crate::types::TyK::Vector(e, n) => {
                crate::types::vec_parts(&self.cx.pctx.borrow(), e, n as u64).is_some()
            }
            _ => false,
        }
    }

    fn lane(&mut self, v: Value, i: u64) -> Value {
        let idx = self.const_i32(i as i32);
        self.extract_element(v, idx)
    }

    fn lanes(&mut self, v: Value, n: u64) -> Vec<Value> {
        (0..n).map(|i| self.lane(v, i)).collect()
    }

    /// Lanes of a SIMD argument, which may be an immediate vector value or a
    /// memory-referenced one (non-native lane counts lower to `Memory`).
    fn arg_lanes(&mut self, arg: &OperandRef<'tcx, Value>) -> Vec<Value> {
        match arg.val {
            OperandValue::Immediate(v) => self.lanes(v, self.elem_of(arg.layout.ty).0),
            OperandValue::Ref(place) => {
                let vty = self.immediate_backend_type(arg.layout);
                let et_rust = arg.layout.ty.simd_size_and_type(self.tcx).1;
                let et = self.immediate_backend_type(self.layout_of(et_rust));
                let offs = crate::types::leaves(&self.pctx.borrow(), vty);
                offs.into_iter()
                    .map(|(off, _)| {
                        let offv = self.const_usize(off);
                        let p = self.inbounds_ptradd(place.llval, offv);
                        self.load(et, p, place.align)
                    })
                    .collect()
            }
            _ => vec![],
        }
    }

    fn build_vec(&mut self, ty: TypeHandle, vals: Vec<Value>) -> Value {
        let mut v = self.const_undef(ty);
        for (i, e) in vals.into_iter().enumerate() {
            let idx = self.const_i32(i as i32);
            v = self.mk(|c| InsertElementOp::new(c, v, e, idx));
        }
        v
    }

    fn nonzero(&mut self, x: Value) -> Value {
        let z = self.const_null(self.val_ty(x));
        self.icmp(IntPredicate::IntNE, x, z)
    }

    fn lane_bin(&mut self, op: &str, e: Elem, x: Value, y: Value) -> Option<Value> {
        Some(match (op, e.float) {
            ("add", false) => self.add(x, y),
            ("add", true) => self.fadd(x, y),
            ("sub", false) => self.sub(x, y),
            ("sub", true) => self.fsub(x, y),
            ("mul", false) => self.mul(x, y),
            ("mul", true) => self.fmul(x, y),
            ("div", true) => self.fdiv(x, y),
            ("div", false) if e.signed => self.sdiv(x, y),
            ("div", false) => self.udiv(x, y),
            ("rem", true) => self.frem(x, y),
            ("rem", false) if e.signed => self.srem(x, y),
            ("rem", false) => self.urem(x, y),
            ("shl", _) => self.shl(x, y),
            ("shr", _) if e.signed => self.ashr(x, y),
            ("shr", _) => self.lshr(x, y),
            ("and", _) => self.and(x, y),
            ("or", _) => self.or(x, y),
            ("xor", _) => self.xor(x, y),
            ("min" | "fmin", _) | ("max" | "fmax", _) => {
                let is_min = op.ends_with("min");
                if e.float {
                    // minnum/maxnum: a NaN operand yields the other operand.
                    let y_nan = self.fcmp(RealPredicate::RealUNO, y, y);
                    let ord = self.fcmp(
                        if is_min {
                            RealPredicate::RealOLT
                        } else {
                            RealPredicate::RealOGT
                        },
                        x,
                        y,
                    );
                    let best = self.select(ord, x, y);
                    self.select(y_nan, x, best)
                } else {
                    let p = match (is_min, e.signed) {
                        (true, true) => IntPredicate::IntSLT,
                        (true, false) => IntPredicate::IntULT,
                        (false, true) => IntPredicate::IntSGT,
                        (false, false) => IntPredicate::IntUGT,
                    };
                    let c = self.icmp(p, x, y);
                    self.select(c, x, y)
                }
            }
            _ => return None,
        })
    }

    fn lane_cmp(&mut self, op: &str, e: Elem, x: Value, y: Value) -> Option<Value> {
        Some(if e.float {
            let p = match op {
                "eq" => RealPredicate::RealOEQ,
                "ne" => RealPredicate::RealUNE,
                "lt" => RealPredicate::RealOLT,
                "le" => RealPredicate::RealOLE,
                "gt" => RealPredicate::RealOGT,
                "ge" => RealPredicate::RealOGE,
                _ => return None,
            };
            self.fcmp(p, x, y)
        } else {
            use IntPredicate::*;
            let p = match (op, e.signed) {
                ("eq", _) => IntEQ,
                ("ne", _) => IntNE,
                ("lt", true) => IntSLT,
                ("lt", false) => IntULT,
                ("le", true) => IntSLE,
                ("le", false) => IntULE,
                ("gt", true) => IntSGT,
                ("gt", false) => IntUGT,
                ("ge", true) => IntSGE,
                ("ge", false) => IntUGE,
                _ => return None,
            };
            self.icmp(p, x, y)
        })
    }

    fn lane_cast(&mut self, x: Value, from: Elem, to: Elem, to_ty: TypeHandle) -> Value {
        match (from.float, to.float) {
            (false, false) => self.intcast(x, to_ty, from.signed),
            (false, true) if from.signed => self.sitofp(x, to_ty),
            (false, true) => self.uitofp(x, to_ty),
            (true, false) if to.signed => self.fptosi(x, to_ty),
            (true, false) => self.fptoui(x, to_ty),
            (true, true) => {
                let (fw, tw) = (self.float_width(self.val_ty(x)), self.float_width(to_ty));
                if fw < tw {
                    self.fpext(x, to_ty)
                } else if fw > tw {
                    self.fptrunc(x, to_ty)
                } else {
                    x
                }
            }
        }
    }

    /// Returns `None` for SIMD intrinsics that aren't implemented yet.
    pub fn simd_intrinsic(
        &mut self,
        name: &str,
        args: &[OperandRef<'tcx, Value>],
        ret_rty: Ty<'tcx>,
        ret: TypeHandle,
        span: rustc_span::Span,
        instance: rustc_middle::ty::Instance<'tcx>,
    ) -> Option<Value> {
        let a = |i: usize| args[i].immediate();
        let op = name.strip_prefix("simd_")?;

        // Mirror the LLVM backend's `generic_simd_intrinsic` validation:
        // emit `InvalidMonomorphization` diagnostics for ill-typed arguments
        // rather than panicking during lowering.
        use rustc_codegen_ssa::diagnostics::{
            ExpectedPointerMutability, InvalidMonomorphization as IM,
        };
        use rustc_middle::ty::{self, consts::ConstExt};
        let name_sym = rustc_span::Symbol::intern(name);
        macro_rules! fail {
            ($err:expr) => {{
                self.tcx.dcx().emit_err($err);
                return Some(self.const_undef(ret));
            }};
        }
        macro_rules! require {
            ($cond:expr, $err:expr) => {
                if !$cond { fail!($err) }
            };
        }
        macro_rules! require_simd {
            ($ty:expr, $variant:ident) => {{
                require!($ty.is_simd(), IM::$variant { span, name: name_sym, ty: $ty });
                $ty.simd_size_and_type(self.tcx)
            }};
        }

        let base = op.strip_suffix("_dyn").unwrap_or(op);
        if base == "splat" {
            let (out_len, out_ty) = require_simd!(ret_rty, SimdReturn);
            require!(
                args[0].layout.ty == out_ty,
                IM::ExpectedVectorElementType {
                    span,
                    name: name_sym,
                    expected_element: out_ty,
                    vector_type: ret_rty
                }
            );
            return Some(self.vector_splat(out_len as usize, a(0)));
        }
        if base == "select_bitmask" {
            require_simd!(args[1].layout.ty, SimdArgument);
            let (len, _) = args[1].layout.ty.simd_size_and_type(self.tcx);
            let expected_int_bits = (len.max(8) - 1).next_power_of_two();
            let expected_bytes = len / 8 + u64::from(!len.is_multiple_of(8));
            let mask_ty = args[0].layout.ty;
            let ok = match *mask_ty.kind() {
                ty::Int(i) => i.bit_width() == Some(expected_int_bits),
                ty::Uint(i) => i.bit_width() == Some(expected_int_bits),
                ty::Array(elem, l) => {
                    matches!(elem.kind(), ty::Uint(ty::UintTy::U8))
                        && l.try_to_target_usize(self.tcx).unwrap_or(u64::MAX) == expected_bytes
                }
                _ => false,
            };
            require!(
                ok,
                IM::InvalidBitmask { span, name: name_sym, mask_ty, expected_int_bits, expected_bytes }
            );
        }

        // Every intrinsic below takes a SIMD vector as its first argument
        // (`select_bitmask` was handled above; `splat` returned already).
        let in_ty = args[0].layout.ty;
        let (in_len, in_elem) = if base == "select_bitmask" {
            (0, in_ty)
        } else {
            require_simd!(in_ty, SimdInput)
        };
        match base {
            "eq" | "ne" | "lt" | "le" | "gt" | "ge" => {
                let (out_len, out_ty) = require_simd!(ret_rty, SimdReturn);
                require!(
                    out_len == in_len,
                    IM::ReturnLengthInputType {
                        span, name: name_sym, in_len, in_ty, ret_ty: ret_rty, out_len
                    }
                );
                require!(
                    out_ty.is_integral(),
                    IM::ReturnIntegerType { span, name: name_sym, ret_ty: ret_rty, out_ty }
                );
            }
            "and" | "or" | "xor" | "shl" | "shr" | "saturating_add" | "saturating_sub" => {
                require!(
                    in_elem.is_integral(),
                    IM::ExpectedVectorElementType {
                        span,
                        name: name_sym,
                        expected_element: in_elem,
                        vector_type: in_ty
                    }
                );
            }
            "cast" => {
                let (out_len, _) = require_simd!(ret_rty, SimdReturn);
                require!(
                    out_len == in_len,
                    IM::ReturnLengthInputType {
                        span, name: name_sym, in_len, in_ty, ret_ty: ret_rty, out_len
                    }
                );
            }
            "select" => {
                require!(
                    in_elem.is_integral(),
                    IM::MaskWrongElementType { span, name: name_sym, ty: in_elem }
                );
                let (v_len, _) = require_simd!(args[1].layout.ty, SimdSecond);
                require_simd!(args[2].layout.ty, SimdThird);
                require!(
                    v_len == in_len,
                    IM::MismatchedLengths { span, name: name_sym, m_len: in_len, v_len }
                );
            }
            "insert" => {
                require!(
                    args[2].layout.ty == in_elem,
                    IM::InsertedType {
                        span,
                        name: name_sym,
                        in_elem,
                        in_ty,
                        out_ty: args[2].layout.ty
                    }
                );
                if name == "simd_insert"
                    && let Some(ConstVal::Bits(bits)) = self.cval(a(1))
                {
                    require!(
                        bits < in_len as u128,
                        IM::SimdIndexOutOfBounds {
                            span,
                            name: name_sym,
                            arg_idx: 1,
                            total_len: in_len as u128
                        }
                    );
                }
            }
            "extract" => {
                require!(
                    ret_rty == in_elem,
                    IM::ReturnType { span, name: name_sym, in_elem, in_ty, ret_ty: ret_rty }
                );
                if name == "simd_extract"
                    && let Some(ConstVal::Bits(bits)) = self.cval(a(1))
                {
                    require!(
                        bits < in_len as u128,
                        IM::SimdIndexOutOfBounds {
                            span,
                            name: name_sym,
                            arg_idx: 1,
                            total_len: in_len as u128
                        }
                    );
                }
            }
            "shuffle" | "shuffle_const_generic" => {
                let idx_len = if name == "simd_shuffle_const_generic" {
                    instance.args[2].expect_const().to_branch().len() as u64
                } else {
                    let idx_ty = args[2].layout.ty;
                    if idx_ty.is_simd()
                        && matches!(
                            idx_ty.simd_size_and_type(self.tcx).1.kind(),
                            ty::Uint(ty::UintTy::U32)
                        )
                    {
                        idx_ty.simd_size_and_type(self.tcx).0
                    } else {
                        fail!(IM::SimdShuffle { span, name: name_sym, ty: idx_ty })
                    }
                };
                let (out_len, out_ty) = require_simd!(ret_rty, SimdReturn);
                require!(
                    out_len == idx_len,
                    IM::ReturnLength {
                        span,
                        name: name_sym,
                        in_len: idx_len,
                        ret_ty: ret_rty,
                        out_len
                    }
                );
                require!(
                    in_elem == out_ty,
                    IM::ReturnElement {
                        span,
                        name: name_sym,
                        in_elem,
                        in_ty,
                        ret_ty: ret_rty,
                        out_ty
                    }
                );
                let total_len = u128::from(in_len) * 2;
                if name == "simd_shuffle_const_generic" {
                    for (i, c) in
                        instance.args[2].expect_const().to_branch().iter().enumerate()
                    {
                        if u128::from(c.to_leaf().to_u32()) >= total_len {
                            fail!(IM::SimdIndexOutOfBounds {
                                span,
                                name: name_sym,
                                arg_idx: i as u64,
                                total_len
                            });
                        }
                    }
                } else if let Some(ConstVal::Agg(ids)) = self.cval(a(2)) {
                    for (i, id) in ids.iter().enumerate() {
                        if self.const_to_opt_u128(*id, true).is_some_and(|k| k >= total_len) {
                            fail!(IM::SimdIndexOutOfBounds {
                                span,
                                name: name_sym,
                                arg_idx: i as u64,
                                total_len
                            });
                        }
                    }
                }
            }
            "masked_load" | "masked_store" => {
                let pointer_ty = args[1].layout.ty;
                let values_ty = args[2].layout.ty;
                let (values_len, values_elem) = require_simd!(values_ty, SimdThird);
                require!(
                    values_len == in_len,
                    IM::ThirdArgumentLength {
                        span,
                        name: name_sym,
                        in_len,
                        in_ty,
                        arg_ty: values_ty,
                        out_len: values_len
                    }
                );
                if base == "masked_load" {
                    require_simd!(ret_rty, SimdReturn);
                    require!(
                        ret_rty == values_ty,
                        IM::ExpectedReturnType {
                            span,
                            name: name_sym,
                            in_ty: values_ty,
                            ret_ty: ret_rty
                        }
                    );
                }
                let mutability = if base == "masked_store" {
                    ExpectedPointerMutability::Mut
                } else {
                    ExpectedPointerMutability::Not
                };
                require!(
                    matches!(*pointer_ty.kind(), ty::RawPtr(p_ty, _) if p_ty == values_elem),
                    IM::ExpectedElementType {
                        span,
                        name: name_sym,
                        expected_element: values_elem,
                        second_arg: pointer_ty,
                        in_elem: values_elem,
                        in_ty: values_ty,
                        mutability
                    }
                );
                require!(
                    in_elem.is_integral(),
                    IM::MaskWrongElementType { span, name: name_sym, ty: in_elem }
                );
            }
            "gather" | "scatter" => {
                let ptrs_ty = args[1].layout.ty;
                let mask_ty = args[2].layout.ty;
                let (ptrs_len, ptr_elem) = require_simd!(ptrs_ty, SimdSecond);
                let (mask_len, mask_elem) = require_simd!(mask_ty, SimdThird);
                require!(
                    ptrs_len == in_len,
                    IM::SecondArgumentLength {
                        span,
                        name: name_sym,
                        in_len,
                        in_ty,
                        arg_ty: ptrs_ty,
                        out_len: ptrs_len
                    }
                );
                require!(
                    mask_len == in_len,
                    IM::ThirdArgumentLength {
                        span,
                        name: name_sym,
                        in_len,
                        in_ty,
                        arg_ty: mask_ty,
                        out_len: mask_len
                    }
                );
                if base == "gather" {
                    require!(
                        ret_rty == in_ty,
                        IM::ExpectedReturnType { span, name: name_sym, in_ty, ret_ty: ret_rty }
                    );
                }
                let mutability = if base == "scatter" {
                    ExpectedPointerMutability::Mut
                } else {
                    ExpectedPointerMutability::Not
                };
                require!(
                    matches!(*ptr_elem.kind(), ty::RawPtr(p_ty, _) if p_ty == in_elem),
                    IM::ExpectedElementType {
                        span,
                        name: name_sym,
                        expected_element: ptr_elem,
                        second_arg: ptrs_ty,
                        in_elem,
                        in_ty,
                        mutability
                    }
                );
                require!(
                    mask_elem.is_integral(),
                    IM::MaskWrongElementType { span, name: name_sym, ty: mask_elem }
                );
            }
            "bitmask" => {
                let expected_int_bits = (in_len.max(8) - 1).next_power_of_two();
                let expected_bytes = in_len / 8 + u64::from(!in_len.is_multiple_of(8));
                let ok = match *ret_rty.kind() {
                    ty::Int(i) => i.bit_width() == Some(expected_int_bits),
                ty::Uint(i) => i.bit_width() == Some(expected_int_bits),
                    ty::Array(elem, l) => {
                        matches!(elem.kind(), ty::Uint(ty::UintTy::U8))
                            && l.try_to_target_usize(self.tcx).unwrap_or(u64::MAX)
                                == expected_bytes
                    }
                    _ => false,
                };
                require!(
                    ok,
                    IM::CannotReturn {
                        span,
                        name: name_sym,
                        ret_ty: ret_rty,
                        expected_int_bits,
                        expected_bytes
                    }
                );
            }
            _ if base.starts_with("reduce_") => {
                if matches!(
                    base,
                    "reduce_all" | "reduce_any" | "reduce_and" | "reduce_or" | "reduce_xor"
                ) {
                    require!(
                        in_elem.is_integral(),
                        IM::UnsupportedSymbol {
                            span,
                            name: name_sym,
                            symbol: name_sym,
                            in_ty,
                            in_elem,
                            ret_ty: ret_rty
                        }
                    );
                } else {
                    require!(
                        ret_rty == in_elem,
                        IM::ReturnType { span, name: name_sym, in_elem, in_ty, ret_ty: ret_rty }
                    );
                }
            }
            _ => {}
        }

        if op == "expose_provenance" || op == "with_exposed_provenance" {
            let (n, _) = self.elem_of(ret_rty);
            let et = self.element_type(ret);
            let xs = self.arg_lanes(&args[0]);
            let out = xs
                .into_iter()
                .map(|x| {
                    if op == "expose_provenance" {
                        self.ptrtoint(x, et)
                    } else {
                        self.inttoptr(x, et)
                    }
                })
                .collect();
            return Some(self.build_vec(ret, out));
        }
        if op == "cast_ptr" {
            // Opaque pointers: pointer-to-pointer lane casts are no-ops.
            return Some(a(0));
        }
        // `select_bitmask` takes an integer mask first; its arm uses `args[1]`.
        let (n, e) = if args[0].layout.ty.is_simd() {
            self.elem_of(args[0].layout.ty)
        } else {
            (0, Elem { signed: false, float: false })
        };
        match base {
            "add" | "sub" | "mul" | "div" | "rem" | "shl" | "shr" | "and" | "or" | "xor"
            | "fmin" | "fmax" | "minimum_number_nsz" | "maximum_number_nsz" => {
                let op = match base {
                    "minimum_number_nsz" => "fmin",
                    "maximum_number_nsz" => "fmax",
                    o => o,
                };
                if self.native(ret) {
                    let (x, y) = (a(0), a(1));
                    let r = match (op, e.float) {
                        ("add", false) => Some(self.add(x, y)),
                        ("sub", false) => Some(self.sub(x, y)),
                        ("and", false) => Some(self.and(x, y)),
                        ("or", false) => Some(self.or(x, y)),
                        ("xor", false) => Some(self.xor(x, y)),
                        ("add", true) => Some(self.fadd(x, y)),
                        ("sub", true) => Some(self.fsub(x, y)),
                        ("mul", true) => Some(self.fmul(x, y)),
                        ("div", true) => Some(self.fdiv(x, y)),
                        _ => None,
                    };
                    if r.is_some() {
                        return r;
                    }
                }
                let (xs, ys) = (self.arg_lanes(&args[0]), self.arg_lanes(&args[1]));
                let mut out = Vec::new();
                for (x, y) in xs.into_iter().zip(ys) {
                    out.push(self.lane_bin(op, e, x, y)?);
                }
                Some(self.build_vec(ret, out))
            }
            "saturating_add" | "saturating_sub" => {
                let sym = if base == "saturating_add" {
                    "add"
                } else {
                    "sub"
                };
                let llvm = format!("llvm.{}{sym}.sat", if e.signed { "s" } else { "u" });
                let et = self.element_type(ret);
                let (xs, ys) = (self.arg_lanes(&args[0]), self.arg_lanes(&args[1]));
                let out = xs
                    .into_iter()
                    .zip(ys)
                    .map(|(x, y)| self.intrinsic(&llvm, et, &[x, y]))
                    .collect();
                Some(self.build_vec(ret, out))
            }
            "neg" => {
                let xs = self.arg_lanes(&args[0]);
                let out = xs
                    .into_iter()
                    .map(|x| if e.float { self.fneg(x) } else { self.neg(x) })
                    .collect();
                Some(self.build_vec(ret, out))
            }
            "fabs" | "fsqrt" | "floor" | "ceil" | "trunc" | "round_ties_even" | "ctpop"
            | "ctlz" | "cttz" | "bswap" | "bitreverse" | "round" => {
                let i = match base {
                    "fsqrt" => "llvm.sqrt".to_string(),
                    "round_ties_even" => "llvm.roundeven".to_string(),
                    o => format!("llvm.{o}"),
                };
                let et = self.element_type(ret);
                let xs = self.arg_lanes(&args[0]);
                let out = xs
                    .into_iter()
                    .map(|x| self.intrinsic(&i, et, &[x]))
                    .collect();
                Some(self.build_vec(ret, out))
            }
            "eq" | "ne" | "lt" | "le" | "gt" | "ge"
                if self.native(self.val_ty(a(0))) && self.native(ret) =>
            {
                let k = if e.float {
                    "f"
                } else if e.signed {
                    "s"
                } else {
                    "u"
                };
                Some(self.intrinsic(&format!("pliron.vcmp.{base}.{k}"), ret, &[a(0), a(1)]))
            }
            "select"
                if matches!(args[0].val, OperandValue::Immediate(v) if self.native(self.val_ty(v)))
                    && self.native(ret) =>
            {
                Some(self.intrinsic("pliron.vbitselect", ret, &[a(0), a(1), a(2)]))
            }
            "bitmask"
                if matches!(args[0].val, OperandValue::Immediate(v) if self.native(self.val_ty(v)))
                    && self.type_kind(ret) == TypeKind::Integer =>
            {
                Some(self.intrinsic("pliron.vhigh_bits", ret, &[a(0)]))
            }
            "eq" | "ne" | "lt" | "le" | "gt" | "ge" => {
                let et = self.element_type(ret);
                let (xs, ys) = (self.arg_lanes(&args[0]), self.arg_lanes(&args[1]));
                let mut out = Vec::new();
                for (x, y) in xs.into_iter().zip(ys) {
                    let c = self.lane_cmp(base, e, x, y)?;
                    out.push(self.sext(c, et));
                }
                Some(self.build_vec(ret, out))
            }
            "select" => {
                let (ms, xs, ys) = (
                    self.arg_lanes(&args[0]),
                    self.arg_lanes(&args[1]),
                    self.arg_lanes(&args[2]),
                );
                let mut out = Vec::new();
                for ((m, x), y) in ms.into_iter().zip(xs).zip(ys) {
                    let c = self.nonzero(m);
                    out.push(self.select(c, x, y));
                }
                Some(self.build_vec(ret, out))
            }
            "select_bitmask" => {
                let (rn, _) = self.elem_of(args[1].layout.ty);
                let m = match args[0].val {
                    OperandValue::Immediate(v) => v,
                    OperandValue::Ref(place) => {
                        // `[u8; N]` mask: load it as one integer.
                        let nbytes = rn / 8 + u64::from(!rn.is_multiple_of(8));
                        let mt = self.cx.type_ix((nbytes * 8).next_power_of_two());
                        self.load(mt, place.llval, place.align)
                    }
                    _ => return None,
                };
                let mt = self.val_ty(m);
                let (xs, ys) = (self.arg_lanes(&args[1]), self.arg_lanes(&args[2]));
                let mut out = Vec::new();
                for (i, (x, y)) in xs.into_iter().zip(ys).enumerate() {
                    let sh = self.const_uint(mt, i as u64);
                    let b = self.lshr(m, sh);
                    let one = self.const_uint(mt, 1);
                    let b = self.and(b, one);
                    let c = self.nonzero(b);
                    out.push(self.select(c, x, y));
                }
                Some(self.build_vec(ret, out))
            }
            "gather" => {
                // Masked-off lanes load their fallback value from a stack slot,
                // so every lane can load unconditionally.
                let (_, ety) = ret_rty.simd_size_and_type(self.tcx);
                let el = self.layout_of(ety);
                let (size, align) = (el.size, el.align.abi);
                let et = self.element_type(ret);
                let slot = self.alloca(size * n, align);
                let (vs, ps, ms) = (
                    self.arg_lanes(&args[0]),
                    self.arg_lanes(&args[1]),
                    self.arg_lanes(&args[2]),
                );
                let mut out = Vec::new();
                for (i, ((v, p), m)) in vs.into_iter().zip(ps).zip(ms).enumerate() {
                    let off = self.const_usize(size.bytes() * i as u64);
                    let fb = self.inbounds_ptradd(slot, off);
                    self.store(v, fb, align);
                    let c = self.nonzero(m);
                    let p = self.select(c, p, fb);
                    out.push(self.load(et, p, align));
                }
                Some(self.build_vec(ret, out))
            }
            "scatter" => {
                // (values, ptrs, mask): lane i stores to ptrs[i] iff mask[i].
                // Masked-off lanes store into a scratch slot, so every lane
                // can store unconditionally.
                let (_, ety) = args[0].layout.ty.simd_size_and_type(self.tcx);
                let el = self.layout_of(ety);
                let (size, align) = (el.size, el.align.abi);
                let slot = self.alloca(size, align);
                for (v, p, m) in self
                    .lanes(a(0), n)
                    .into_iter()
                    .zip(self.arg_lanes(&args[1]))
                    .zip(self.arg_lanes(&args[2]))
                    .map(|((v, p), m)| (v, p, m))
                {
                    let c = self.nonzero(m);
                    let p = self.select(c, p, slot);
                    self.store(v, p, align);
                }
                Some(self.const_undef(ret))
            }
            "masked_load" => {
                // (mask, ptr, fallback): lane i = ptr[i] iff mask[i], else
                // fallback[i]. Same trick as gather: masked-off lanes load
                // the fallback out of a stack slot.
                let (_, ety) = ret_rty.simd_size_and_type(self.tcx);
                let el = self.layout_of(ety);
                let (size, align) = (el.size, el.align.abi);
                let et = self.element_type(ret);
                let slot = self.alloca(size * n, align);
                let mut out = Vec::new();
                for (i, (m, v)) in self
                    .lanes(a(0), n)
                    .into_iter()
                    .zip(self.arg_lanes(&args[2]))
                    .enumerate()
                {
                    let off = self.const_usize(size.bytes() * i as u64);
                    let fb = self.inbounds_ptradd(slot, off);
                    self.store(v, fb, align);
                    let c = self.nonzero(m);
                    let lp = self.inbounds_ptradd(a(1), off);
                    let p = self.select(c, lp, fb);
                    out.push(self.load(et, p, align));
                }
                Some(self.build_vec(ret, out))
            }
            "masked_store" => {
                // (mask, ptr, values): lane i stores values[i] to ptr[i] iff
                // mask[i]; masked-off lanes store into a scratch slot.
                let (_, ety) = args[2].layout.ty.simd_size_and_type(self.tcx);
                let el = self.layout_of(ety);
                let (size, align) = (el.size, el.align.abi);
                let slot = self.alloca(size, align);
                for (i, (m, v)) in self
                    .lanes(a(0), n)
                    .into_iter()
                    .zip(self.arg_lanes(&args[2]))
                    .enumerate()
                {
                    let off = self.const_usize(size.bytes() * i as u64);
                    let c = self.nonzero(m);
                    let lp = self.inbounds_ptradd(a(1), off);
                    let p = self.select(c, lp, slot);
                    self.store(v, p, align);
                }
                Some(self.const_undef(ret))
            }
            "funnel_shl" | "funnel_shr" => {
                // llvm.fshl/fshr: concat(a,b) as a 2W-bit int shifted by
                // s mod W; keep the high (shl) or low (shr) W bits.
                let et = self.element_type(ret);
                let w = self.int_width(et);
                let wide = self.type_ix(w * 2);
                let wm = self.const_uint(et, w - 1);
                let wv = self.const_uint(wide, w);
                let mut out = Vec::new();
                for ((x, y), s) in self
                    .lanes(a(0), n)
                    .into_iter()
                    .zip(self.arg_lanes(&args[1]))
                    .zip(self.arg_lanes(&args[2]))
                {
                    let s = self.and(s, wm);
                    let s = self.intcast(s, wide, false);
                    let xw = self.intcast(x, wide, false);
                    let xw = self.shl(xw, wv);
                    let yw = self.intcast(y, wide, false);
                    let cat = self.or(xw, yw);
                    let r = if base == "funnel_shl" {
                        let r = self.shl(cat, s);
                        self.lshr(r, wv)
                    } else {
                        self.lshr(cat, s)
                    };
                    out.push(self.intcast(r, et, false));
                }
                Some(self.build_vec(ret, out))
            }
            "fsin" | "fcos" | "fexp" | "fexp2" | "flog" | "flog2" | "flog10" => {
                // libm per lane, matching the scalar mapping in intrinsic.rs.
                let f = base.strip_prefix('f').unwrap();
                let et = self.element_type(ret);
                let name = if self.float_width(et) == 32 {
                    format!("{f}f")
                } else {
                    f.to_string()
                };
                let out = self
                    .lanes(a(0), n)
                    .into_iter()
                    .map(|x| self.call_sym(&name, et, &[x]))
                    .collect();
                Some(self.build_vec(ret, out))
            }
            "fma" | "relaxed_fma" => {
                let et = self.element_type(ret);
                let (xs, ys, zs) = (
                    self.arg_lanes(&args[0]),
                    self.arg_lanes(&args[1]),
                    self.arg_lanes(&args[2]),
                );
                let out = xs
                    .into_iter()
                    .zip(ys)
                    .zip(zs)
                    .map(|((x, y), z)| self.intrinsic("llvm.fma", et, &[x, y, z]))
                    .collect();
                Some(self.build_vec(ret, out))
            }
            "extract" => Some(self.extract_element(a(0), a(1))),
            "insert" => {
                let (v, idx, x) = (a(0), a(1), a(2));
                Some(self.mk(|c| InsertElementOp::new(c, v, x, idx)))
            }
            "shuffle" | "shuffle_const_generic" => {
                let ids: Vec<u64> = if name == "simd_shuffle_const_generic" {
                    instance.args[2]
                        .expect_const()
                        .to_branch()
                        .iter()
                        .map(|c| c.to_leaf().to_u32() as u64)
                        .collect()
                } else {
                    let Some(ConstVal::Agg(ids)) = self.cval(a(2)) else {
                        return None;
                    };
                    ids.iter()
                        .map(|i| self.const_to_opt_u128(*i, false).unwrap_or(u128::MAX) as u64)
                        .collect()
                };
                let (xs, ys) = (self.arg_lanes(&args[0]), self.arg_lanes(&args[1]));
                let mut out = Vec::new();
                for k in ids {
                    out.push(if k < n {
                        xs[k as usize]
                    } else {
                        ys[(k - n) as usize]
                    });
                }
                Some(self.build_vec(ret, out))
            }
            "bitmask" => {
                let xs = self.arg_lanes(&args[0]);
                let int_ret = self.type_kind(ret) == TypeKind::Integer;
                let acc_ty = if int_ret {
                    ret
                } else {
                    let nbytes = n / 8 + u64::from(!n.is_multiple_of(8));
                    self.cx.type_ix((nbytes * 8).next_power_of_two())
                };
                let mut acc = self.const_null(acc_ty);
                for (i, x) in xs.into_iter().enumerate() {
                    let lt = self.val_ty(x);
                    let w = self.int_width(lt);
                    let sh = self.const_uint(lt, w - 1);
                    let b = self.lshr(x, sh);
                    let b = self.intcast(b, acc_ty, false);
                    let s = self.const_uint(acc_ty, i as u64);
                    let b = self.shl(b, s);
                    acc = self.or(acc, b);
                }
                if int_ret {
                    Some(acc)
                } else {
                    let nbytes = n / 8 + u64::from(!n.is_multiple_of(8));
                    let i8t = self.cx.type_i8();
                    let mut agg = self.const_undef(ret);
                    for j in 0..nbytes {
                        let sh = self.const_uint(acc_ty, j * 8);
                        let byte = self.lshr(acc, sh);
                        let byte = self.intcast(byte, i8t, false);
                        agg = self.insert_value(agg, byte, j);
                    }
                    Some(agg)
                }
            }
            "reduce_any" | "reduce_all" => {
                let xs = self.arg_lanes(&args[0]);
                let mut acc = self.const_bool(base == "reduce_all");
                for x in xs {
                    let c = self.nonzero(x);
                    acc = if base == "reduce_all" {
                        self.and(acc, c)
                    } else {
                        self.or(acc, c)
                    };
                }
                Some(acc)
            }
            _ if base.starts_with("reduce_") => {
                let r = base.strip_prefix("reduce_").unwrap();
                let (r, ordered) = match r.strip_suffix("_ordered") {
                    Some(r) => (r, true),
                    None => (r.strip_suffix("_unordered").unwrap_or(r), false),
                };
                let xs = self.arg_lanes(&args[0]);
                let mut it = xs.into_iter();
                let mut acc = if ordered { a(1) } else { it.next()? };
                for x in it {
                    acc = self.lane_bin(r, e, acc, x)?;
                }
                Some(acc)
            }
            "cast" | "as" => {
                let (_, to) = self.elem_of(ret_rty);
                let et = self.element_type(ret);
                let xs = self.arg_lanes(&args[0]);
                let out = xs
                    .into_iter()
                    .map(|x| self.lane_cast(x, e, to, et))
                    .collect();
                Some(self.build_vec(ret, out))
            }
            _ => None,
        }
    }

    /// Lane-wise emulation of the `llvm.x86.*` SIMD intrinsics that
    /// runtime-dispatching crates (aho-corasick, half, ...) actually reach.
    pub fn llvm_x86_intrinsic(
        &mut self,
        name: &str,
        args: &[OperandRef<'tcx, Value>],
        ret_rty: Ty<'tcx>,
        ret: TypeHandle,
    ) -> Option<Value> {
        use rustc_codegen_ssa::common::RealPredicate::*;
        let a = |i: usize| args[i].immediate();
        match name {
            "llvm.x86.ssse3.pshuf.b.128" | "llvm.x86.avx2.pshuf.b" => {
                // r[i] = b[i] & 0x80 ? 0 : a[16 * (i / 16) + (b[i] & 15)]
                let (n, _) = self.elem_of(ret_rty);
                let one = rustc_abi::Align::ONE;
                let slot = self.alloca(rustc_abi::Size::from_bytes(n), one);
                self.store(a(0), slot, one);
                let (i8t, isz) = (self.type_i8(), self.type_isize());
                let zero = self.const_u8(0);
                let bs = self.arg_lanes(&args[1]);
                let mut out = Vec::new();
                for (i, b) in bs.into_iter().enumerate() {
                    let m = self.const_u8(0x0f);
                    let idx = self.and(b, m);
                    let idx = self.zext(idx, isz);
                    let base = self.const_usize(i as u64 / 16 * 16);
                    let off = self.add(idx, base);
                    let p = self.inbounds_ptradd(slot, off);
                    let v = self.load(i8t, p, one);
                    let hb = self.const_u8(0x80);
                    let h = self.and(b, hb);
                    let c = self.nonzero(h);
                    out.push(self.select(c, zero, v));
                }
                Some(self.build_vec(ret, out))
            }
            "llvm.x86.pclmulqdq" | "llvm.x86.pclmulqdq.256" | "llvm.x86.pclmulqdq.512" => {
                // Per 128-bit lane: carry-less multiply of the qwords picked by imm bits 0 and 4.
                // At -O0 the immediate arrives as a runtime `IMM8 as u8`, so select on it.
                let imm = a(2);
                let (m0, m4) = (self.const_u8(1), self.const_u8(0x10));
                let (b0, b4) = (self.and(imm, m0), self.and(imm, m4));
                let (hi_a, hi_b) = (self.nonzero(b0), self.nonzero(b4));
                let (n, _) = self.elem_of(ret_rty);
                let (xs, ys) = (self.arg_lanes(&args[0]), self.arg_lanes(&args[1]));
                let stub = self.cx.llvm_intrinsic_stub("pliron.clmul64");
                let (i64t, void) = (self.type_i64(), self.type_void());
                let al = rustc_abi::Align::EIGHT;
                let slot = self.alloca(rustc_abi::Size::from_bytes(16), al);
                let eight = self.const_usize(8);
                let mut out = Vec::new();
                for l in 0..n as usize / 2 {
                    let x = self.select(hi_a, xs[2 * l + 1], xs[2 * l]);
                    let y = self.select(hi_b, ys[2 * l + 1], ys[2 * l]);
                    self.call_sym(&stub, void, &[x, y, slot]);
                    out.push(self.load(i64t, slot, al));
                    let hi = self.inbounds_ptradd(slot, eight);
                    out.push(self.load(i64t, hi, al));
                }
                Some(self.build_vec(ret, out))
            }
            "llvm.x86.sse.max.ps"
            | "llvm.x86.sse2.max.pd"
            | "llvm.x86.avx.max.ps.256"
            | "llvm.x86.avx.max.pd.256"
            | "llvm.x86.avx512.max.ps.512"
            | "llvm.x86.avx512.max.pd.512" => {
                // maxps returns the second operand unless a > b (NaNs, +-0).
                let (n, _) = self.elem_of(ret_rty);
                let (xs, ys) = (self.arg_lanes(&args[0]), self.arg_lanes(&args[1]));
                let mut out = Vec::new();
                for (x, y) in xs.into_iter().zip(ys) {
                    let c = self.fcmp(RealOGT, x, y);
                    out.push(self.select(c, x, y));
                }
                Some(self.build_vec(ret, out))
            }
            "llvm.x86.sse.min.ps"
            | "llvm.x86.sse2.min.pd"
            | "llvm.x86.avx.min.ps.256"
            | "llvm.x86.avx.min.pd.256"
            | "llvm.x86.avx512.min.ps.512"
            | "llvm.x86.avx512.min.pd.512" => {
                // minps returns the second operand unless a < b (NaNs, +-0).
                let (n, _) = self.elem_of(ret_rty);
                let (xs, ys) = (self.arg_lanes(&args[0]), self.arg_lanes(&args[1]));
                let mut out = Vec::new();
                for (x, y) in xs.into_iter().zip(ys) {
                    let c = self.fcmp(RealOLT, x, y);
                    out.push(self.select(c, x, y));
                }
                Some(self.build_vec(ret, out))
            }
            "llvm.x86.avx512.mask.cmp.ps.512" | "llvm.x86.avx512.mask.cmp.pd.512" => {
                // (a, b, imm, k, sae) -> integer bitmask of the predicate, ANDed with k.
                let imm = self.const_to_opt_u128(a(2), false)? as u8 & 0xf;
                let (n, _) = self.elem_of(args[0].layout.ty);
                let mt = self.val_ty(a(3));
                let (xs, ys) = (self.arg_lanes(&args[0]), self.arg_lanes(&args[1]));
                let mut acc = self.const_int(mt, 0);
                for (i, (x, y)) in xs.into_iter().zip(ys).enumerate() {
                    let bit = match X86_CMP[imm as usize] {
                        RealPredicateFalse => continue,
                        RealPredicateTrue => self.const_int(mt, 1),
                        p => {
                            let c = self.fcmp(p, x, y);
                            self.zext(c, mt)
                        }
                    };
                    let sh = self.const_int(mt, i as i64);
                    let bit = self.shl(bit, sh);
                    acc = self.or(acc, bit);
                }
                Some(self.and(acc, a(3)))
            }
            "llvm.x86.sse.cmp.ps"
            | "llvm.x86.sse2.cmp.pd"
            | "llvm.x86.avx.cmp.ps.256"
            | "llvm.x86.avx.cmp.pd.256" => {
                // Predicates 16..31 only differ in signaling, so the low 4 bits suffice.
                let imm = self.const_to_opt_u128(a(2), false)? as u8 & 0xf;
                let pred = X86_CMP[imm as usize];
                let (n, _) = self.elem_of(ret_rty);
                let et = self.element_type(ret);
                let it = self.type_ix(if name.contains(".ps") { 32 } else { 64 });
                let (xs, ys) = (self.arg_lanes(&args[0]), self.arg_lanes(&args[1]));
                let mut out = Vec::new();
                for (x, y) in xs.into_iter().zip(ys) {
                    let m = match pred {
                        RealPredicateFalse => self.const_int(it, 0),
                        RealPredicateTrue => self.const_int(it, -1),
                        p => {
                            let c = self.fcmp(p, x, y);
                            self.sext(c, it)
                        }
                    };
                    out.push(self.bitcast(m, et));
                }
                Some(self.build_vec(ret, out))
            }
            "llvm.x86.vcvtps2ph.128" => {
                // <4 x f32> -> low 4 lanes of <8 x i16>; callers use round-to-nearest.
                let (f16, i16t) = (self.type_f16(), self.type_i16());
                let xs = self.arg_lanes(&args[0]);
                let mut out = Vec::new();
                for x in xs {
                    let h = self.fptrunc(x, f16);
                    out.push(self.bitcast(h, i16t));
                }
                for _ in 0..4 {
                    out.push(self.const_i16(0));
                }
                Some(self.build_vec(ret, out))
            }
            _ => None,
        }
    }
}

/// x86 `cmpps` immediate (low 4 bits) to predicate.
const X86_CMP: [rustc_codegen_ssa::common::RealPredicate; 16] = {
    use rustc_codegen_ssa::common::RealPredicate::*;
    [RealOEQ, RealOLT, RealOLE, RealUNO, RealUNE, RealUGE, RealUGT, RealORD, RealUEQ, RealULT, RealULE, RealPredicateFalse, RealONE, RealOGE, RealOGT, RealPredicateTrue]
};
