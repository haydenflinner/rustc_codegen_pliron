//! Helpers for inspecting pliron LLVM-dialect types: classification, LLVM
//! data-layout sizes/alignments, and flattening into Cranelift scalar leaves.

use cranelift_codegen::ir::{Type as ClType, types as clt};
use pliron::builtin::type_interfaces::FunctionTypeInterface;
use pliron::builtin::types::{BF16Type, FP16Type, FP32Type, FP64Type, IntegerType};
use pliron::context::Context;
use pliron::derive::pliron_type;
use pliron::r#type::TypeHandle;
use pliron_llvm::types::{
    ArrayType, FuncType, PointerType, StructLayout, StructType, VectorType, VoidType,
};

/// pliron has no builtin binary128 type; this stands in for LLVM's `fp128`.
#[pliron_type(name = "rcg.fp128", generate_get = true, format, verifier = "succ")]
#[derive(Hash, PartialEq, Eq, Debug)]
pub struct FP128Type;

#[derive(Clone, Debug)]
pub enum TyK {
    Void,
    Int(u32),
    F16,
    F32,
    F64,
    F128,
    Ptr,
    Array(TypeHandle, u64),
    Struct(Vec<TypeHandle>, bool),
    Vector(TypeHandle, u32),
    Func(TypeHandle, Vec<TypeHandle>, bool),
    Other,
}

pub fn classify(ctx: &Context, ty: TypeHandle) -> TyK {
    let t = ty.deref(ctx);
    if let Some(i) = t.downcast_ref::<IntegerType>() {
        return TyK::Int(i.width());
    }
    if t.is::<FP32Type>() {
        return TyK::F32;
    }
    if t.is::<FP64Type>() {
        return TyK::F64;
    }
    if t.is::<FP16Type>() || t.is::<BF16Type>() {
        return TyK::F16;
    }
    if t.is::<FP128Type>() {
        return TyK::F128;
    }
    if t.is::<PointerType>() {
        return TyK::Ptr;
    }
    if t.is::<VoidType>() {
        return TyK::Void;
    }
    if let Some(a) = t.downcast_ref::<ArrayType>() {
        return TyK::Array(a.elem_type(), a.size());
    }
    if let Some(s) = t.downcast_ref::<StructType>() {
        return TyK::Struct(
            s.fields().collect(),
            matches!(s.layout(), StructLayout::Packed),
        );
    }
    if let Some(v) = t.downcast_ref::<VectorType>() {
        return TyK::Vector(v.elem_type(), v.num_elements());
    }
    if let Some(f) = t.downcast_ref::<FuncType>() {
        return TyK::Func(f.result_type(), f.arg_types(), f.is_var_arg());
    }
    TyK::Other
}

/// Set for 32-bit-pointer targets (wasm32) before any layout query.
pub static PTR32: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn ptr32() -> bool {
    PTR32.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn align_to(x: u64, a: u64) -> u64 {
    x.div_ceil(a) * a
}

/// (size, align) following the x86_64/aarch64 LLVM data layouts rustc uses.
pub fn size_align(ctx: &Context, ty: TypeHandle) -> (u64, u64) {
    match classify(ctx, ty) {
        TyK::Int(w) => {
            let b = (w.max(1) as u64).div_ceil(8).next_power_of_two();
            (b, b.min(16))
        }
        TyK::F16 => (2, 2),
        TyK::F32 => (4, 4),
        TyK::F64 => (8, 8),
        TyK::F128 => (16, 16),
        TyK::Ptr => {
            if ptr32() {
                (4, 4)
            } else {
                (8, 8)
            }
        }
        TyK::Void | TyK::Func(..) | TyK::Other => (0, 1),
        TyK::Array(e, n) => {
            let (s, a) = size_align(ctx, e);
            (s * n, a)
        }
        TyK::Struct(fs, packed) => {
            let (_, s, a) = struct_offsets(ctx, &fs, packed);
            (s, a)
        }
        TyK::Vector(e, n) => {
            let (s, _) = size_align(ctx, e);
            let t = (s * n as u64).next_power_of_two();
            (t, t)
        }
    }
}

pub fn struct_offsets(ctx: &Context, fields: &[TypeHandle], packed: bool) -> (Vec<u64>, u64, u64) {
    let mut off = 0;
    let mut align = 1;
    let mut offs = Vec::with_capacity(fields.len());
    for &f in fields {
        let (s, a) = size_align(ctx, f);
        let a = if packed { 1 } else { a };
        off = align_to(off, a);
        offs.push(off);
        off += s;
        align = align.max(a);
    }
    (offs, align_to(off, align), align)
}

pub fn int_cl(w: u32) -> ClType {
    match w {
        0..=8 => clt::I8,
        9..=16 => clt::I16,
        17..=32 => clt::I32,
        33..=64 => clt::I64,
        _ => clt::I128,
    }
}

/// Flatten a type into its scalar leaves `(byte offset, cranelift type)`.
/// Vectors are flattened lane-wise like arrays.
pub fn leaves(ctx: &Context, ty: TypeHandle) -> Vec<(u64, ClType)> {
    let mut out = Vec::new();
    leaves_into(ctx, ty, 0, &mut out);
    out
}

fn leaves_into(ctx: &Context, ty: TypeHandle, base: u64, out: &mut Vec<(u64, ClType)>) {
    match classify(ctx, ty) {
        TyK::Int(w) => out.push((base, int_cl(w))),
        TyK::F16 => out.push((base, clt::F16)),
        TyK::F32 => out.push((base, clt::F32)),
        TyK::F64 => out.push((base, clt::F64)),
        TyK::F128 => out.push((base, clt::F128)),
        TyK::Ptr => out.push((base, if ptr32() { clt::I32 } else { clt::I64 })),
        TyK::Void | TyK::Func(..) | TyK::Other => {}
        TyK::Array(e, n) => {
            let (s, _) = size_align(ctx, e);
            for i in 0..n {
                leaves_into(ctx, e, base + i * s, out);
            }
        }
        TyK::Vector(e, n) => {
            if let Some((t, k)) = vec_parts(ctx, e, n as u64) {
                return out.extend((0..k).map(|i| (base + i * 16, t)));
            }
            let (s, _) = size_align(ctx, e);
            for i in 0..n as u64 {
                leaves_into(ctx, e, base + i * s, out);
            }
        }
        TyK::Struct(fs, packed) => {
            let (offs, _, _) = struct_offsets(ctx, &fs, packed);
            for (f, o) in fs.iter().zip(offs) {
                leaves_into(ctx, *f, base + o, out);
            }
        }
    }
}

/// 128-bit int/float vectors are one native Cranelift SIMD leaf; other vectors
/// are flattened lane-wise. `PLIRON_SIMD=0` flattens everything.
pub fn native_vec(ctx: &Context, e: TypeHandle, n: u64) -> Option<ClType> {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| crate::pass_enabled("PLIRON_SIMD")) {
        return None;
    }
    let lane = match classify(ctx, e) {
        TyK::Int(w @ (8 | 16 | 32 | 64)) => ClType::int(w as u16)?,
        TyK::F32 => clt::F32,
        TyK::F64 => clt::F64,
        _ => return None,
    };
    if lane.bits() as u64 * n != 128 {
        return None;
    }
    lane.by(n as u32)
}

/// Native SIMD parts of a vector: one 128-bit value, or two halves for 256-bit
/// (Cranelift x64 has no 256-bit vectors; memchr's AVX2 path uses them).
pub fn vec_parts(ctx: &Context, e: TypeHandle, n: u64) -> Option<(ClType, u64)> {
    if let Some(t) = native_vec(ctx, e, n) {
        return Some((t, 1));
    }
    (n % 2 == 0)
        .then(|| native_vec(ctx, e, n / 2))?
        .map(|t| (t, 2))
}

/// Element types of an aggregate (struct fields, or `n` copies of the element).
pub fn members(ctx: &Context, ty: TypeHandle) -> Vec<TypeHandle> {
    match classify(ctx, ty) {
        TyK::Struct(fs, _) => fs,
        TyK::Array(e, n) => vec![e; n as usize],
        TyK::Vector(e, n) => vec![e; n as usize],
        k => panic!("not an aggregate: {k:?}"),
    }
}

/// Leaf range covered by the member at `indices` (as in extractvalue), plus its type.
pub fn leaf_range(ctx: &Context, ty: TypeHandle, indices: &[u32]) -> (usize, usize, TypeHandle) {
    let mut start = 0;
    let mut cur = ty;
    for &i in indices {
        let ms = members(ctx, cur);
        for m in &ms[..i as usize] {
            start += leaves(ctx, *m).len();
        }
        cur = ms[i as usize];
    }
    (start, leaves(ctx, cur).len(), cur)
}
