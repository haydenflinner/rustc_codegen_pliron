//! Loads from immutable globals with a known initializer become constants
//! (LLVM folds these in instcombine). Cranelift can't: it has no view of
//! global initializers. codegen_ssa puts every non-scalar constant (e.g. a
//! `None::<char>`) in a private global and loads from it.

use cranelift_module::Linkage;
use pliron::{
    context::{Context, Ptr},
    linked_list::ContainsLinkedList,
    operation::Operation,
    r#type::{TypeHandle, Typed},
    value::Value,
};
use pliron_llvm::ops::{GetElementPtrOp, LoadOp};
use rustc_data_structures::fx::FxHashMap;

use crate::context::{ConstVal, State};
use crate::lower::has_body;
use crate::sroa::{gep_offset, mk_const};
use crate::types::{TyK, classify, size_align, struct_offsets};

/// Byte image of a constant: known bytes, plus pointer relocations by offset.
#[derive(Default)]
struct Image {
    bytes: Vec<Option<u8>>,
    relocs: FxHashMap<usize, (String, i64)>,
}

fn image(ctx: &Context, st: &State<'_>, v: Value, out: &mut Image) -> Option<()> {
    let ty = v.get_type(ctx);
    let (size, _) = size_align(ctx, ty);
    let start = out.bytes.len();
    match st.consts.get(&v)? {
        ConstVal::Bits(b) if size <= 16 => out
            .bytes
            .extend(b.to_le_bytes()[..size as usize].iter().map(|&x| Some(x))),
        ConstVal::Zero => out.bytes.extend((0..size).map(|_| Some(0))),
        ConstVal::Bytes(bs) if bs.len() as u64 == size => {
            out.bytes.extend(bs.iter().map(|&x| Some(x)))
        }
        ConstVal::Sym { sym, off } if size == 8 => {
            out.relocs.insert(start, (sym.clone(), *off));
            out.bytes.extend([None; 8]);
        }
        ConstVal::Agg(es) => {
            let offs = match classify(ctx, ty) {
                TyK::Struct(fs, packed) => struct_offsets(ctx, &fs, packed).0,
                TyK::Array(e, _) | TyK::Vector(e, _) => {
                    let s = size_align(ctx, e).0;
                    (0..es.len() as u64).map(|i| i * s).collect()
                }
                _ => return None,
            };
            for (&e, &o) in es.iter().zip(&offs) {
                // Padding is undefined: leave it unknown.
                out.bytes.resize(start + o as usize, None);
                image(ctx, st, e, out)?;
            }
        }
        _ => return None,
    }
    if out.bytes.len() > start + size as usize {
        return None;
    }
    out.bytes.resize(start + size as usize, None);
    Some(())
}

/// The global symbol and byte offset `p` points at, if constant.
fn sym_off(ctx: &Context, st: &State<'_>, p: Value) -> Option<(String, i64)> {
    if let Some(ConstVal::Sym { sym, off }) = st.consts.get(&p) {
        return Some((sym.clone(), *off));
    }
    let op = p.defining_op()?;
    Operation::get_op::<GetElementPtrOp>(op, ctx)?;
    let o = gep_offset(ctx, st, op)?;
    let (s, base) = sym_off(ctx, st, op.deref(ctx).get_operand(0))?;
    Some((s, base + o))
}

/// Constant loaded by `ty`-typed `load` of `img` at `off`.
fn fold(ctx: &Context, img: &Image, off: i64, ty: TypeHandle) -> Option<ConstVal> {
    let (n, _) = size_align(ctx, ty);
    let off = usize::try_from(off).ok()?;
    let bytes = img.bytes.get(off..off + n as usize)?;
    let known = || -> Option<u128> {
        let mut b = [0u8; 16];
        for (i, x) in bytes.iter().enumerate() {
            b[i] = (*x)?;
        }
        Some(u128::from_le_bytes(b))
    };
    match classify(ctx, ty) {
        TyK::Ptr => {
            if let Some((sym, o)) = img.relocs.get(&off) {
                return Some(ConstVal::Sym {
                    sym: sym.clone(),
                    off: *o,
                });
            }
            (known()? == 0).then_some(ConstVal::Zero)
        }
        TyK::Int(w) if w <= 128 && w % 8 == 0 => Some(ConstVal::Bits(known()?)),
        TyK::F32 | TyK::F64 => Some(ConstVal::Bits(known()?)),
        _ => None,
    }
}

pub fn run(ctx: &mut Context, st: &mut State<'_>) {
    let mut images: FxHashMap<String, Option<Image>> = FxHashMap::default();
    let fns: Vec<_> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| has_body(ctx, f))
        .collect();
    let mut n = 0;
    for f in fns {
        let ops: Vec<Ptr<Operation>> = f
            .deref(ctx)
            .get_region(0)
            .deref(ctx)
            .iter(ctx)
            .flat_map(|b| b.deref(ctx).iter(ctx).collect::<Vec<_>>())
            .collect();
        for op in ops {
            if !Operation::is_op::<LoadOp>(op, ctx) || st.volatile.contains(&op) {
                continue;
            }
            let Some((sym, off)) = sym_off(ctx, st, op.deref(ctx).get_operand(0)) else {
                continue;
            };
            let img = images.entry(sym.clone()).or_insert_with(|| {
                let g = st.globals.get(&sym)?;
                if g.mutable || g.tls || matches!(g.linkage, Linkage::Import | Linkage::Preemptible)
                {
                    return None;
                }
                let mut img = Image::default();
                image(ctx, st, g.init?, &mut img)?;
                Some(img)
            });
            let r = op.deref(ctx).get_result(0);
            let Some(cv) = img
                .as_ref()
                .and_then(|img| fold(ctx, img, off, r.get_type(ctx)))
            else {
                continue;
            };
            let k = mk_const(ctx, st, r.get_type(ctx), cv);
            r.replace_all_uses_with(ctx, &k);
            Operation::erase(op, ctx);
            n += 1;
        }
    }
    if std::env::var_os("PLIRON_STATS").is_some() {
        eprintln!("constload {}: {n} loads folded", st.cgu);
    }
}
