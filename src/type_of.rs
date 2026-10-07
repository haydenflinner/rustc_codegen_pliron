//! rustc layouts/ABIs -> pliron LLVM dialect types (port of cg_llvm's type_of.rs/abi.rs).

use pliron::builtin::types::{FP16Type, FP32Type, FP64Type, BF16Type};
use pliron::r#type::{TypeHandle, Typed};
use pliron_llvm::types::{
    ArrayType, FuncType, PointerType, StructLayout, StructType, VectorType, VectorTypeKind,
    VoidType,
};
use rustc_abi::{
    AddressSpace, Align, BackendRepr, FieldsShape, Float, Integer, Primitive, Reg, RegKind,
    Scalar, Size,
};
use rustc_codegen_ssa::common::TypeKind;
use rustc_codegen_ssa::traits::*;
use rustc_middle::ty::layout::{LayoutOf, TyAndLayout};
use rustc_middle::ty::Ty;
use rustc_target::callconv::{ArgAttributes, ArgExtension, CastTarget, FnAbi, IndirectMode, PassMode};

use crate::context::{ArgExt, CodegenCx, Exts, layout_ty_key};
use crate::types::{FP128Type, TyK};

fn ext(a: &ArgAttributes) -> ArgExt {
    match a.arg_ext {
        ArgExtension::Zext => ArgExt::Zext,
        ArgExtension::Sext => ArgExt::Sext,
        ArgExtension::None => ArgExt::None,
    }
}

/// Per-pliron-parameter integer extension, mirroring `fn_decl_backend_type`'s arg order.
pub fn exts_of<'tcx>(fn_abi: &FnAbi<'tcx, Ty<'tcx>>) -> Exts {
    let mut e = Exts::default();
    match &fn_abi.ret.mode {
        PassMode::Direct(a) => e.ret = ext(a),
        PassMode::Indirect { .. } => e.params.push(ArgExt::SRet),
        _ => {}
    }
    for arg in fn_abi.args.iter() {
        match &arg.mode {
            PassMode::Ignore => {}
            PassMode::Direct(a) => e.params.push(ext(a)),
            PassMode::Pair(a, b) => {
                e.params.push(ext(a));
                e.params.push(ext(b));
            }
            PassMode::Indirect { meta_attrs: Some(_), .. } => {
                e.params.push(ArgExt::None);
                e.params.push(ArgExt::None);
            }
            PassMode::Cast { pad_i32_count, .. } => {
                for _ in 0..*pad_i32_count {
                    e.params.push(ArgExt::None);
                }
                e.params.push(ArgExt::None);
            }
            PassMode::Indirect { meta_attrs: None, mode: IndirectMode::OnStack, .. } => {
                e.params.push(ArgExt::ByVal(arg.layout.size.bytes() as u32))
            }
            PassMode::Indirect { meta_attrs: None, .. } => e.params.push(ArgExt::None),
        }
    }
    e
}

impl<'tcx> CodegenCx<'tcx> {
    pub fn type_i1(&self) -> TypeHandle {
        self.int_ty(1)
    }
    pub fn type_ix(&self, bits: u64) -> TypeHandle {
        self.int_ty(bits as u32)
    }
    pub fn type_void(&self) -> TypeHandle {
        VoidType::get(&mut self.pctx.borrow_mut()).into()
    }
    pub fn type_struct(&self, fields: &[TypeHandle], packed: bool) -> TypeHandle {
        let l = if packed { StructLayout::Packed } else { StructLayout::Unpacked };
        StructType::get_unnamed(&mut self.pctx.borrow_mut(), (fields.to_vec(), l)).into()
    }
    pub fn type_vector(&self, elem: TypeHandle, n: u64) -> TypeHandle {
        VectorType::get(&mut self.pctx.borrow_mut(), elem, n as u32, VectorTypeKind::Fixed).into()
    }
    pub fn type_variadic_func(&self, args: &[TypeHandle], ret: TypeHandle) -> TypeHandle {
        FuncType::get(&mut self.pctx.borrow_mut(), ret, args.to_vec(), true).into()
    }
    pub fn type_from_integer(&self, i: Integer) -> TypeHandle {
        self.type_ix(i.size().bits())
    }
    pub fn type_from_float(&self, f: Float) -> TypeHandle {
        match f {
            Float::F16 => self.type_f16(),
            Float::F16B => self.type_f16b(),
            Float::F32 => self.type_f32(),
            Float::F64 => self.type_f64(),
            Float::F128 => self.type_f128(),
        }
    }
    pub fn type_padding_filler(&self, size: Size, align: Align) -> TypeHandle {
        let unit = Integer::approximate_align(self, align);
        let size = size.bytes();
        let unit_size = unit.size().bytes();
        assert_eq!(size % unit_size, 0);
        self.type_array(self.type_from_integer(unit), size / unit_size)
    }

    pub fn scalar_type_at(&self, scalar: Scalar) -> TypeHandle {
        match scalar.primitive() {
            Primitive::Int(i, _) => self.type_from_integer(i),
            Primitive::Float(f) => self.type_from_float(f),
            Primitive::Pointer(a) => self.type_ptr_ext(a),
        }
    }

    pub fn layout_type(&self, layout: TyAndLayout<'tcx>) -> TypeHandle {
        if let BackendRepr::Scalar(scalar) = layout.backend_repr {
            if let Some(&t) = self.scache.borrow().get(&layout.ty) {
                return t;
            }
            let t = self.scalar_type_at(scalar);
            self.scache.borrow_mut().insert(layout.ty, t);
            return t;
        }
        let key = layout_ty_key(layout);
        if let Some(&t) = self.tcache.borrow().get(&key) {
            return t;
        }
        let t = self.uncached_layout_type(layout);
        self.tcache.borrow_mut().insert(key, t);
        t
    }

    fn uncached_layout_type(&self, layout: TyAndLayout<'tcx>) -> TypeHandle {
        match layout.backend_repr {
            BackendRepr::Scalar(_) => unreachable!(),
            BackendRepr::ScalarPair { .. } => {
                let a = self.scalar_pair_element_backend_type(layout, 0, false);
                let b = self.scalar_pair_element_backend_type(layout, 1, false);
                return self.type_struct(&[a, b], false);
            }
            BackendRepr::SimdVector { element, count } => {
                let e = self.scalar_type_at(element);
                return self.type_vector(e, count.as_u64());
            }
            BackendRepr::Memory { .. } => {}
            #[allow(unreachable_patterns)]
            _ => self.tcx.dcx().fatal("scalable vectors are not supported by the pliron backend"),
        }
        match layout.fields {
            FieldsShape::Primitive | FieldsShape::Union(_) => {
                let fill = self.type_padding_filler(layout.size, layout.align.abi);
                self.type_struct(&[fill], false)
            }
            FieldsShape::Array { count, .. } => {
                let e = self.layout_type(layout.field(self, 0));
                self.type_array(e, count)
            }
            FieldsShape::Arbitrary { .. } => {
                let (fields, packed) = self.struct_fields(layout);
                self.type_struct(&fields, packed)
            }
        }
    }

    fn struct_fields(&self, layout: TyAndLayout<'tcx>) -> (Vec<TypeHandle>, bool) {
        let field_count = layout.fields.count();
        let mut packed = false;
        let mut offset = Size::ZERO;
        let mut prev_align = layout.align.abi;
        let mut result = Vec::with_capacity(1 + field_count * 2);
        for i in layout.fields.index_by_increasing_offset() {
            let target = layout.fields.offset(i);
            let field = layout.field(self, i);
            let eff = layout.align.abi.min(field.align.abi).restrict_for_offset(target);
            packed |= eff < field.align.abi;
            assert!(target >= offset);
            let padding = target - offset;
            if padding != Size::ZERO {
                result.push(self.type_padding_filler(padding, prev_align.min(eff)));
            }
            result.push(self.layout_type(field));
            offset = target + field.size;
            prev_align = eff;
        }
        if layout.is_sized() && field_count > 0 {
            let padding = layout.size - offset;
            if padding != Size::ZERO {
                result.push(self.type_padding_filler(padding, prev_align));
            }
        }
        (result, packed)
    }

    fn reg_type(&self, r: &Reg) -> TypeHandle {
        match r.kind {
            RegKind::Integer => self.type_ix(r.size.bits()),
            RegKind::Float => match r.size.bits() {
                16 => self.type_f16(),
                32 => self.type_f32(),
                64 => self.type_f64(),
                128 => self.type_f128(),
                _ => panic!("unsupported float reg {r:?}"),
            },
            RegKind::Vector { .. } => self.type_vector(self.type_i8(), r.size.bytes()),
        }
    }
}

impl<'tcx> BaseTypeCodegenMethods for CodegenCx<'tcx> {
    fn type_i8(&self) -> TypeHandle {
        self.int_ty(8)
    }
    fn type_i16(&self) -> TypeHandle {
        self.int_ty(16)
    }
    fn type_i32(&self) -> TypeHandle {
        self.int_ty(32)
    }
    fn type_i64(&self) -> TypeHandle {
        self.int_ty(64)
    }
    fn type_i128(&self) -> TypeHandle {
        self.int_ty(128)
    }
    fn type_isize(&self) -> TypeHandle {
        self.int_ty(self.tcx.data_layout.pointer_size().bits() as u32)
    }
    fn type_f16(&self) -> TypeHandle {
        FP16Type::get(&mut self.pctx.borrow_mut()).into()
    }
    fn type_f16b(&self) -> TypeHandle {
        BF16Type::get(&mut self.pctx.borrow_mut()).into()
    }
    fn type_f32(&self) -> TypeHandle {
        FP32Type::get(&mut self.pctx.borrow_mut()).into()
    }
    fn type_f64(&self) -> TypeHandle {
        FP64Type::get(&mut self.pctx.borrow_mut()).into()
    }
    fn type_f128(&self) -> TypeHandle {
        FP128Type::get(&mut self.pctx.borrow_mut()).into()
    }
    fn type_array(&self, ty: TypeHandle, len: u64) -> TypeHandle {
        ArrayType::get(&mut self.pctx.borrow_mut(), ty, len).into()
    }
    fn type_func(&self, args: &[TypeHandle], ret: TypeHandle) -> TypeHandle {
        FuncType::get(&mut self.pctx.borrow_mut(), ret, args.to_vec(), false).into()
    }
    fn type_kind(&self, ty: TypeHandle) -> TypeKind {
        match self.kind(ty) {
            TyK::Void => TypeKind::Void,
            TyK::Int(_) => TypeKind::Integer,
            TyK::F16 => TypeKind::Half,
            TyK::F32 => TypeKind::Float,
            TyK::F64 => TypeKind::Double,
            TyK::F128 => TypeKind::FP128,
            TyK::Ptr => TypeKind::Pointer,
            TyK::Array(..) => TypeKind::Array,
            TyK::Struct(..) => TypeKind::Struct,
            TyK::Vector(..) => TypeKind::Vector,
            TyK::Func(..) => TypeKind::Function,
            TyK::Other => TypeKind::Token,
        }
    }
    fn type_ptr(&self) -> TypeHandle {
        self.type_ptr_ext(AddressSpace::ZERO)
    }
    fn type_ptr_ext(&self, address_space: AddressSpace) -> TypeHandle {
        PointerType::get(&mut self.pctx.borrow_mut(), address_space.0).into()
    }
    fn element_type(&self, ty: TypeHandle) -> TypeHandle {
        match self.kind(ty) {
            TyK::Array(e, _) | TyK::Vector(e, _) => e,
            k => panic!("element_type of {k:?}"),
        }
    }
    fn vector_length(&self, ty: TypeHandle) -> usize {
        match self.kind(ty) {
            TyK::Vector(_, n) => n as usize,
            k => panic!("vector_length of {k:?}"),
        }
    }
    fn float_width(&self, ty: TypeHandle) -> usize {
        match self.kind(ty) {
            TyK::F16 => 16,
            TyK::F32 => 32,
            TyK::F64 => 64,
            TyK::F128 => 128,
            k => panic!("float_width of {k:?}"),
        }
    }
    fn int_width(&self, ty: TypeHandle) -> u64 {
        match self.kind(ty) {
            TyK::Int(w) => w as u64,
            k => panic!("int_width of {k:?}"),
        }
    }
    fn val_ty(&self, v: pliron::value::Value) -> TypeHandle {
        v.get_type(&self.pctx.borrow())
    }
}

impl<'tcx> LayoutTypeCodegenMethods<'tcx> for CodegenCx<'tcx> {
    fn backend_type(&self, layout: TyAndLayout<'tcx>) -> TypeHandle {
        self.layout_type(layout)
    }
    fn cast_backend_type(&self, cast: &CastTarget) -> TypeHandle {
        let unit = self.reg_type(&cast.rest.unit);
        let rest_count = if cast.rest.total == Size::ZERO {
            0
        } else {
            cast.rest.total.bytes().div_ceil(cast.rest.unit.size.bytes())
        };
        if cast.prefix.is_empty() {
            if rest_count == 1 && (!cast.rest.is_consecutive || cast.rest.unit != Reg::i128()) {
                return unit;
            }
            return self.type_array(unit, rest_count);
        }
        let mut args: Vec<_> = cast.prefix.iter().map(|r| self.reg_type(r)).collect();
        args.extend((0..rest_count).map(|_| unit));
        self.type_struct(&args, false)
    }
    fn fn_decl_backend_type(&self, fn_abi: &FnAbi<'tcx, Ty<'tcx>>) -> TypeHandle {
        let mut args = Vec::new();
        let ret = match &fn_abi.ret.mode {
            PassMode::Ignore => self.type_void(),
            PassMode::Direct(_) | PassMode::Pair(..) => self.immediate_backend_type(fn_abi.ret.layout),
            PassMode::Cast { cast, .. } => self.cast_backend_type(cast),
            PassMode::Indirect { .. } => {
                args.push(self.type_ptr());
                self.type_void()
            }
        };
        let nargs = if fn_abi.c_variadic { fn_abi.fixed_count as usize } else { fn_abi.args.len() };
        for arg in &fn_abi.args[..nargs] {
            let t = match &arg.mode {
                PassMode::Ignore => continue,
                PassMode::Direct(_) => self.immediate_backend_type(arg.layout),
                PassMode::Pair(..) => {
                    args.push(self.scalar_pair_element_backend_type(arg.layout, 0, true));
                    args.push(self.scalar_pair_element_backend_type(arg.layout, 1, true));
                    continue;
                }
                PassMode::Indirect { meta_attrs: Some(_), .. } => {
                    let ptr_ty = Ty::new_mut_ptr(self.tcx, arg.layout.ty);
                    let pl = self.layout_of(ptr_ty);
                    args.push(self.scalar_pair_element_backend_type(pl, 0, true));
                    args.push(self.scalar_pair_element_backend_type(pl, 1, true));
                    continue;
                }
                PassMode::Cast { cast, pad_i32_count } => {
                    for _ in 0..*pad_i32_count {
                        args.push(self.type_i32());
                    }
                    self.cast_backend_type(cast)
                }
                PassMode::Indirect { meta_attrs: None, .. } => self.type_ptr(),
            };
            args.push(t);
        }
        if fn_abi.c_variadic {
            self.type_variadic_func(&args, ret)
        } else {
            self.type_func(&args, ret)
        }
    }
    fn fn_ptr_backend_type(&self, _fn_abi: &FnAbi<'tcx, Ty<'tcx>>) -> TypeHandle {
        self.type_ptr()
    }
    fn reg_backend_type(&self, ty: &Reg) -> TypeHandle {
        self.reg_type(ty)
    }
    fn immediate_backend_type(&self, layout: TyAndLayout<'tcx>) -> TypeHandle {
        match layout.backend_repr {
            BackendRepr::Scalar(s) if s.is_bool() => self.type_i1(),
            _ => self.layout_type(layout),
        }
    }
    fn scalar_pair_element_backend_type(
        &self,
        layout: TyAndLayout<'tcx>,
        index: usize,
        immediate: bool,
    ) -> TypeHandle {
        let BackendRepr::ScalarPair { a, b, .. } = layout.backend_repr else {
            panic!("not a scalar pair: {layout:?}")
        };
        let s = [a, b][index];
        if immediate && s.is_bool() {
            return self.type_i1();
        }
        self.scalar_type_at(s)
    }
}
