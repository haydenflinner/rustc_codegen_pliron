//! Constants: recorded as `ConstVal` side-table entries on placeholder values.

use pliron::r#type::TypeHandle;
use pliron::value::Value;
use rustc_abi::{self as abi, HasDataLayout, Primitive, Size, WrappingRange};
use rustc_codegen_ssa::traits::*;
use rustc_const_eval::interpret::{GlobalAlloc, Pointer, Scalar as InterpScalar, read_target_uint};
use rustc_middle::mir::interpret::Allocation;

use crate::context::{CodegenCx, ConstVal, mask};
use crate::types::TyK;

impl<'tcx> CodegenCx<'tcx> {
    fn width_of(&self, t: TypeHandle) -> u32 {
        match self.kind(t) {
            TyK::Int(w) => w,
            TyK::F16 => 16,
            TyK::F32 => 32,
            TyK::F64 | TyK::Ptr => 64,
            _ => 128,
        }
    }

    pub fn const_bytes(&self, bytes: &[u8]) -> Value {
        let ty = self.type_array(self.type_i8(), bytes.len() as u64);
        self.new_value(ty, ConstVal::Bytes(bytes.to_vec()))
    }

    pub fn const_alloc_to_value(&self, alloc: &Allocation) -> Value {
        let dl = self.data_layout();
        let psize = dl.pointer_size().bytes() as usize;
        let mut vals = Vec::new();
        let mut next = 0usize;
        for &(offset, prov) in alloc.provenance().ptrs().iter() {
            let offset = offset.bytes() as usize;
            if offset > next {
                vals.push(self.const_bytes(
                    alloc.inspect_with_uninit_and_ptr_outside_interpreter(next..offset),
                ));
            }
            let ptr_offset = read_target_uint(
                dl.endian,
                alloc.inspect_with_uninit_and_ptr_outside_interpreter(offset..offset + psize),
            )
            .expect("const_alloc_to_value: could not read relocation pointer")
                as u64;
            let address_space = self.tcx.global_alloc(prov.alloc_id()).address_space(self);
            vals.push(self.scalar_to_backend(
                InterpScalar::from_pointer(
                    Pointer::new(prov, Size::from_bytes(ptr_offset)),
                    &self.tcx,
                ),
                abi::Scalar::Initialized {
                    value: Primitive::Pointer(address_space),
                    valid_range: WrappingRange::full(dl.pointer_size()),
                },
                self.type_ptr_ext(address_space),
            ));
            next = offset + psize;
        }
        if alloc.len() >= next {
            vals.push(self.const_bytes(
                alloc.inspect_with_uninit_and_ptr_outside_interpreter(next..alloc.len()),
            ));
        }
        self.const_struct(&vals, true)
    }

    /// Ok(symbol address) or Err(absolute address) for allocations without a symbol.
    pub(crate) fn alloc_to_backend(&self, ga: GlobalAlloc<'tcx>) -> Result<Value, u64> {
        match ga {
            GlobalAlloc::Function { instance, .. } => Ok(self.get_fn_addr(instance, None)),
            GlobalAlloc::Static(def_id) => Ok(self.get_static_addr(def_id)),
            // Like cg_llvm: empty allocations (e.g. `&()`) get a dangling,
            // aligned address instead of a real global.
            GlobalAlloc::Memory(alloc) if alloc.inner().len() == 0 => {
                Err(alloc.inner().align.bytes())
            }
            GlobalAlloc::Memory(alloc) => Ok(self.static_addr_of(alloc, None)),
            GlobalAlloc::VTable(ty, dyn_ty) => {
                let alloc = self
                    .tcx
                    .global_alloc(self.tcx.vtable_allocation((
                        ty,
                        dyn_ty.principal().map(|principal| {
                            self.tcx.instantiate_bound_regions_with_erased(principal)
                        }),
                    )))
                    .unwrap_memory();
                Ok(self.static_addr_of(alloc, None))
            }
            GlobalAlloc::TypeId { .. } => Err(0),
            #[allow(unreachable_patterns)]
            _ => Err(0),
        }
    }
}

impl<'tcx> ConstCodegenMethods for CodegenCx<'tcx> {
    fn const_null(&self, t: TypeHandle) -> Value {
        self.new_value(t, ConstVal::Zero)
    }
    fn const_undef(&self, t: TypeHandle) -> Value {
        self.new_value(t, ConstVal::Undef)
    }
    fn const_poison(&self, t: TypeHandle) -> Value {
        self.new_value(t, ConstVal::Undef)
    }
    fn const_bool(&self, val: bool) -> Value {
        self.const_uint(self.type_i1(), val as u64)
    }
    fn const_i8(&self, i: i8) -> Value {
        self.const_int(self.type_i8(), i as i64)
    }
    fn const_i16(&self, i: i16) -> Value {
        self.const_int(self.type_i16(), i as i64)
    }
    fn const_i32(&self, i: i32) -> Value {
        self.const_int(self.type_i32(), i as i64)
    }
    fn const_i64(&self, i: i64) -> Value {
        self.const_int(self.type_i64(), i)
    }
    fn const_int(&self, t: TypeHandle, i: i64) -> Value {
        let w = self.width_of(t);
        self.new_value(t, ConstVal::Bits(mask(i as i128 as u128, w)))
    }
    fn const_u8(&self, i: u8) -> Value {
        self.const_uint(self.type_i8(), i as u64)
    }
    fn const_u32(&self, i: u32) -> Value {
        self.const_uint(self.type_i32(), i as u64)
    }
    fn const_u64(&self, i: u64) -> Value {
        self.const_uint(self.type_i64(), i)
    }
    fn const_u128(&self, i: u128) -> Value {
        self.const_uint_big(self.type_i128(), i)
    }
    fn const_usize(&self, i: u64) -> Value {
        self.const_uint(self.type_isize(), i)
    }
    fn const_uint(&self, t: TypeHandle, i: u64) -> Value {
        self.const_uint_big(t, i as u128)
    }
    fn const_uint_big(&self, t: TypeHandle, u: u128) -> Value {
        let w = self.width_of(t);
        self.new_value(t, ConstVal::Bits(mask(u, w)))
    }
    fn const_real(&self, t: TypeHandle, val: f64) -> Value {
        use rustc_apfloat::ieee::{Double, Half, Quad};
        use rustc_apfloat::{Float, FloatConvert};
        let bits = match self.kind(t) {
            TyK::F32 => (val as f32).to_bits() as u128,
            TyK::F64 => val.to_bits() as u128,
            TyK::F16 => {
                let h: Half = Double::from_bits(val.to_bits() as u128)
                    .convert(&mut false)
                    .value;
                h.to_bits()
            }
            TyK::F128 => {
                let q: Quad = Double::from_bits(val.to_bits() as u128)
                    .convert(&mut false)
                    .value;
                q.to_bits()
            }
            k => panic!("const_real of {k:?}"),
        };
        self.new_value(t, ConstVal::Bits(bits))
    }
    fn const_str(&self, s: &str) -> (Value, Value) {
        let cached = self.st.borrow().strs.get(s).copied();
        let ptr = cached.unwrap_or_else(|| {
            let init = self.const_bytes(s.as_bytes());
            let p = self.private_global(init, 1, false);
            self.st.borrow_mut().strs.insert(s.to_string(), p);
            p
        });
        (ptr, self.const_usize(s.len() as u64))
    }
    fn const_struct(&self, elts: &[Value], packed: bool) -> Value {
        let tys: Vec<_> = elts.iter().map(|v| self.ty_of(*v)).collect();
        let t = self.type_struct(&tys, packed);
        self.new_value(t, ConstVal::Agg(elts.to_vec()))
    }
    fn const_vector(&self, elts: &[Value]) -> Value {
        let t = self.type_vector(self.ty_of(elts[0]), elts.len() as u64);
        self.new_value(t, ConstVal::Agg(elts.to_vec()))
    }
    fn const_to_opt_uint(&self, v: Value) -> Option<u64> {
        match self.cval(v)? {
            ConstVal::Bits(b) => u64::try_from(b).ok(),
            ConstVal::Zero => Some(0),
            _ => None,
        }
    }
    fn const_to_opt_u128(&self, v: Value, sign_ext: bool) -> Option<u128> {
        let b = match self.cval(v)? {
            ConstVal::Bits(b) => b,
            ConstVal::Zero => 0,
            _ => return None,
        };
        let w = self.width_of(self.ty_of(v));
        if sign_ext && w < 128 && (b >> (w - 1)) & 1 == 1 {
            Some(b | (!0u128 << w))
        } else {
            Some(b)
        }
    }
    fn scalar_to_backend_with_pac(
        &self,
        cv: InterpScalar,
        layout: abi::Scalar,
        llty: TypeHandle,
        _schema: Option<&rustc_session::PointerAuthSchema>,
    ) -> Value {
        let bitsize = if layout.is_bool() {
            1
        } else {
            layout.size(self).bits() as u32
        };
        match cv {
            InterpScalar::Int(int) => {
                let data = int.to_bits(layout.size(self));
                self.new_value(llty, ConstVal::Bits(mask(data, bitsize)))
            }
            InterpScalar::Ptr(ptr, _) => {
                let (prov, offset) = ptr.prov_and_relative_offset();
                let ga = self.tcx.global_alloc(prov.alloc_id());
                match self.alloc_to_backend(ga) {
                    Ok(base) => match self.cval(base) {
                        Some(ConstVal::Sym { sym, off }) => self.new_value(
                            llty,
                            ConstVal::Sym {
                                sym,
                                off: off + offset.bytes() as i64,
                            },
                        ),
                        other => panic!("symbol base expected, got {other:?}"),
                    },
                    Err(addr) => self.new_value(
                        llty,
                        ConstVal::Bits(addr.wrapping_add(offset.bytes()) as u128),
                    ),
                }
            }
        }
    }
    fn const_ptr_byte_offset(&self, val: Value, offset: Size) -> Value {
        let ty = self.ty_of(val);
        match self.cval(val) {
            Some(ConstVal::Sym { sym, off }) => self.new_value(
                ty,
                ConstVal::Sym {
                    sym,
                    off: off + offset.bytes() as i64,
                },
            ),
            Some(ConstVal::Bits(b)) => {
                self.new_value(ty, ConstVal::Bits(b + offset.bytes() as u128))
            }
            Some(ConstVal::Zero) => self.new_value(ty, ConstVal::Bits(offset.bytes() as u128)),
            other => panic!("const_ptr_byte_offset of {other:?}"),
        }
    }
}
