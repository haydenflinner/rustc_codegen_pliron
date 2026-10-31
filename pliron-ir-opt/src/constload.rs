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
use rustc_hash::FxHashMap;

use crate::opt_body;
use crate::sroa::{gep_offset, mk_const};
use crate::state::{ConstVal, State};
use crate::types::{TyK, classify, ptr32, size_align, struct_offsets};

/// Byte image of a constant: known bytes, plus pointer relocations by offset.
#[derive(Default)]
struct Image {
    bytes: Vec<Option<u8>>,
    relocs: FxHashMap<usize, (String, i64)>,
}

/// Pointer size for the target (4 on wasm32, 8 elsewhere).
fn ptrsz() -> u64 {
    if ptr32() { 4 } else { 8 }
}

fn image(ctx: &Context, st: &State, v: Value, out: &mut Image) -> Option<()> {
    let ty = v.get_type(ctx);
    let (size, _) = size_align(ctx, ty);
    let start = out.bytes.len();
    match st.consts.get(v)? {
        ConstVal::Bits(b) if size <= 16 => out
            .bytes
            .extend(b.to_le_bytes()[..size as usize].iter().map(|&x| Some(x))),
        ConstVal::Zero => out.bytes.extend((0..size).map(|_| Some(0))),
        ConstVal::Bytes(bs) if bs.len() as u64 == size => {
            out.bytes.extend(bs.iter().map(|&x| Some(x)))
        }
        // Pointer-sized reloc (a fn/struct address): on wasm32 pointers
        // are 4 bytes, so this must not assume 8.
        ConstVal::Sym { sym, off } if size == ptrsz() => {
            out.relocs.insert(start, (sym.clone(), *off));
            out.bytes.extend(vec![None; size as usize]);
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
fn sym_off(ctx: &Context, st: &State, p: Value) -> Option<(String, i64)> {
    if let Some(ConstVal::Sym { sym, off }) = st.consts.get(p) {
        return Some((sym.clone(), *off));
    }
    let op = p.defining_op()?;
    Operation::get_op::<GetElementPtrOp>(op, ctx)?;
    let o = gep_offset(ctx, st, op)?;
    let (s, base) = sym_off(ctx, st, op.deref(ctx).get_operand(ctx, 0))?;
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

/// A foldable global's initializer size, if `sym` is one: immutable, not
/// thread-local, defined here, with a known initializer.
fn const_global_size(ctx: &Context, st: &State, sym: &str) -> Option<u64> {
    let g = st.globals.get(sym)?;
    if g.mutable || g.tls || matches!(g.linkage, Linkage::Import | Linkage::Preemptible) {
        return None;
    }
    Some(size_align(ctx, g.init?.get_type(ctx)).0)
}

/// The uses of alloca `a` if it is only written by one whole-object
/// `llvm.memcpy` from constant memory and otherwise only read: returns the
/// copy and the lifetime markers (to erase) and the source address.
fn const_copy(ctx: &Context, st: &State, a: Value) -> Option<(Ptr<Operation>, Vec<Ptr<Operation>>, (String, i64))> {
    let size = st.allocas.get(a)?.0;
    let mut init = None;
    let mut markers = Vec::new();
    // (pointer, is the alloca itself)
    let mut work = vec![(a, true)];
    while let Some((p, root)) = work.pop() {
        for u in p.uses_iter(ctx) {
            let o = u.user_op();
            let i = u.find_index(ctx);
            if Operation::is_op::<LoadOp>(o, ctx) {
                if st.volatile.contains(o) {
                    return None;
                }
            } else if Operation::is_op::<GetElementPtrOp>(o, ctx) {
                if i != 0 {
                    return None;
                }
                work.push((o.deref(ctx).get_result(ctx, 0), false));
            } else {
                match st.intrinsics.get(o).map(|s| s.as_str()) {
                    Some("llvm.lifetime.start" | "llvm.lifetime.end") if root => markers.push(o),
                    // Read as a copy source.
                    Some("llvm.memcpy" | "llvm.memmove") if i == 1 && !st.volatile.contains(o) => {}
                    // The one write: the whole object, from constant memory.
                    Some("llvm.memcpy") if i == 0 && root && init.is_none() && !st.volatile.contains(o) => {
                        let src = o.deref(ctx).get_operand(ctx, 1);
                        let n = crate::sroa::const_int(ctx, st, o.deref(ctx).get_operand(ctx, 2))?;
                        let (sym, off) = sym_off(ctx, st, src)?;
                        let gsize = const_global_size(ctx, st, &sym)?;
                        if n != size as i128 || off < 0 || off as u64 + size > gsize {
                            return None;
                        }
                        init = Some((o, (sym, off)));
                    }
                    _ => return None,
                }
            }
        }
    }
    let (m, src) = init?;
    Some((m, markers, src))
}

/// LLVM's `isOnlyCopiedFromConstantMemory`: an alloca whose only write is
/// a whole-object `memcpy` from an immutable global is the global. Reads
/// before the copy were undefined, so reading the global there is a
/// refinement. Uses become the global's address, so `run` can fold the
/// loads; rustc materializes `const` arrays this way (blake3's
/// `MSG_SCHEDULE[round]`: an 896-byte table copied to the stack, then a
/// row of it copied again, and every message index loaded back and bounds
/// checked). Iterates: replacing one copy's alloca makes copies out of it
/// copies from the global. `PLIRON_CONSTCOPY=1` enables (default off).
// r[impl iropt.constload.copy]
pub fn forward_copies(ctx: &mut Context, st: &mut State) {
    let fns: Vec<_> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| opt_body(ctx, st, f))
        .collect();
    let mut n = 0;
    for f in fns {
        loop {
            let mut changed = false;
            for al in crate::sroa::allocas(ctx, f) {
                let a = al.deref(ctx).get_result(ctx, 0);
                let Some((m, markers, (sym, off))) = const_copy(ctx, st, a) else {
                    continue;
                };
                if !crate::bisect_in("constcopy", "") {
                    continue;
                }
                for o in std::iter::once(m).chain(markers) {
                    st.intrinsics.remove(o);
                    Operation::erase(o, ctx);
                }
                let g = mk_const(ctx, st, a.get_type(ctx), ConstVal::Sym { sym, off });
                a.replace_all_uses_with(ctx, &g);
                st.allocas.remove(a);
                st.promoted.remove(a);
                Operation::erase(al, ctx);
                n += 1;
                changed = true;
            }
            if !changed {
                break;
            }
        }
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        tracing::info!(target: "pliron::stats", "constcopy {}: {n} allocas", st.cgu)
    }
}

// r[impl iropt.constload]
pub fn run(ctx: &mut Context, st: &mut State) {
    // Nothing folds unless some global is a foldable const image.
    if !st.globals.values().any(|g| {
        !g.mutable
            && !g.tls
            && !matches!(g.linkage, Linkage::Import | Linkage::Preemptible)
            && g.init.is_some()
    }) {
        return;
    }
    let mut images: FxHashMap<String, Option<Image>> = FxHashMap::default();
    let fns: Vec<_> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| opt_body(ctx, st, f))
        .collect();
    let mut n = 0;
    for f in fns {
        let ops: Vec<Ptr<Operation>> = f
            .deref(ctx)
            .get_region(ctx, 0)
            .deref(ctx)
            .iter(ctx)
            .flat_map(|b| b.deref(ctx).iter(ctx).collect::<Vec<_>>())
            .collect();
        for op in ops {
            if !Operation::is_op::<LoadOp>(op, ctx) || st.volatile.contains(op) {
                continue;
            }
            let Some((sym, off)) = sym_off(ctx, st, op.deref(ctx).get_operand(ctx, 0)) else {
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
            let r = op.deref(ctx).get_result(ctx, 0);
            let Some(cv) = img
                .as_ref()
                .and_then(|img| fold(ctx, img, off, r.get_type(ctx)))
            else {
                continue;
            };
            if !crate::bisect_in("constload", "") {
                continue;
            }
            let k = mk_const(ctx, st, r.get_type(ctx), cv);
            r.replace_all_uses_with(ctx, &k);
            Operation::erase(op, ctx);
            n += 1;
        }
    }
    if pliron_cfg::tracing_on() && tracing::enabled!(target: "pliron::stats", tracing::Level::INFO)
    {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats", "constload {}: {n} loads folded", st.cgu)
        };
    }
}
