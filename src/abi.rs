//! Internal ABI: functions that use Cranelift's tail CC (see
//! `lower::internal_fns`) and return through a hidden sret pointer instead
//! return their scalar leaves in registers. The callee writes into a local
//! alloca and loads the leaves at each return; callers store the returned
//! leaves into the slot they used to pass. SROA then usually promotes both
//! slots, which removes the memory round trip. `PLIRON_SRET2REG=0` disables it.

use pliron::basic_block::BasicBlock;
use pliron::builtin::attributes::TypeAttr;
use pliron::builtin::op_interfaces::{CallOpCallable, CallOpInterface};
use pliron::builtin::types::{IntegerType, Signedness};
use pliron::context::{Context, Ptr};
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed, TypedHandle};
use pliron_llvm::ops::{
    AllocaOp, CallOp, ExtractValueOp, FuncOp, InsertValueOp, LoadOp, ReturnOp, StoreOp,
};
use pliron_llvm::types::{ArrayType, FuncType, StructLayout, StructType};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::context::{ArgExt, ConstVal, State};
use crate::inline::{blocks, direct_callee, ops};
use crate::sroa::{mk_const, ptr_plus};
use crate::types::{TyK, classify, size_align, struct_offsets};

const MAX_REGS: usize = 8;

/// Flatten `ty` into (offset, scalar type); None for anything we don't split.
fn scalar_leaves(
    ctx: &Context,
    ty: TypeHandle,
    base: u64,
    out: &mut Vec<(u64, TypeHandle)>,
) -> bool {
    match classify(ctx, ty) {
        TyK::Int(w) if w % 8 == 0 && w <= 64 => out.push((base, ty)),
        TyK::F32 | TyK::F64 | TyK::Ptr => out.push((base, ty)),
        TyK::Struct(fs, packed) => {
            let (offs, ..) = struct_offsets(ctx, &fs, packed);
            for (f, o) in fs.iter().zip(offs) {
                if !scalar_leaves(ctx, *f, base + o, out) {
                    return false;
                }
            }
        }
        TyK::Array(e, n) if n as usize <= MAX_REGS => {
            let (s, _) = size_align(ctx, e);
            for i in 0..n {
                if !scalar_leaves(ctx, e, base + i * s, out) {
                    return false;
                }
            }
        }
        _ => return false,
    }
    out.len() <= 2 * MAX_REGS
}

fn fits_regs(ctx: &Context, leaves: &[(u64, TypeHandle)]) -> bool {
    let fl = leaves
        .iter()
        .filter(|(_, t)| matches!(classify(ctx, *t), TyK::F32 | TyK::F64))
        .count();
    !leaves.is_empty() && fl <= MAX_REGS && leaves.len() - fl <= MAX_REGS
}

fn call_sites(
    ctx: &Context,
    st: &State<'_>,
    internal: &FxHashSet<String>,
) -> FxHashMap<String, Vec<Ptr<Operation>>> {
    let mut sites: FxHashMap<String, Vec<Ptr<Operation>>> = FxHashMap::default();
    for f in st.funcs.values() {
        if !crate::lower::has_body(ctx, f.op) {
            continue;
        }
        for b in blocks(ctx, f.op) {
            for op in ops(ctx, b) {
                if let Some(c) = direct_callee(ctx, st, op) {
                    if internal.contains(c) {
                        sites.entry(c.clone()).or_default().push(op);
                    }
                }
            }
        }
    }
    sites
}

/// Dead-argument elimination for internal functions: parameters the body
/// never reads are dropped from the signature and from every call.
/// Opt-in (`PLIRON_DEADARG=1`): stage2 instruction count unchanged.
pub fn dead_args(ctx: &mut Context, st: &mut State<'_>) {
    let internal = crate::lower::internal_fns(ctx, st);
    let sites = call_sites(ctx, st, &internal);
    let mut cands: Vec<&String> = internal.iter().collect();
    cands.sort();
    let mut n = 0;
    for sym in cands {
        let f = &st.funcs[sym];
        let TyK::Func(ret, args, _) = classify(ctx, f.ty) else {
            continue;
        };
        if f.exts.params.len() != args.len() {
            continue;
        }
        let entry = Operation::get_op::<FuncOp>(f.op, ctx)
            .unwrap()
            .get_entry_block(ctx)
            .unwrap();
        let live: Vec<bool> = entry
            .deref(ctx)
            .arguments()
            .map(|a| a.is_used(ctx))
            .collect();
        if live.len() != args.len() || live.iter().all(|&l| l) {
            continue;
        }
        let calls = sites.get(sym).cloned().unwrap_or_default();
        if calls
            .iter()
            .any(|c| st.calls.get(c).is_none_or(|i| i.fn_ty != f.ty))
        {
            continue;
        }
        let keep = |v: &[TypeHandle]| -> Vec<TypeHandle> {
            v.iter().zip(&live).filter(|p| *p.1).map(|p| *p.0).collect()
        };
        let nty = FuncType::get(ctx, ret, keep(&args), false);
        let fop = f.op;
        for i in (0..live.len()).rev().filter(|&i| !live[i]) {
            BasicBlock::remove_argument(entry, ctx, i);
        }
        Operation::get_op::<FuncOp>(fop, ctx)
            .unwrap()
            .set_attr_llvm_func_type(ctx, TypeAttr::new(nty.into()));
        let f = st.funcs.get_mut(sym).unwrap();
        f.ty = nty.into();
        let mut i = 0;
        f.exts.params.retain(|_| {
            i += 1;
            live[i - 1]
        });
        let exts = f.exts.clone();
        let id = st.sym_to_ident[sym].clone();
        for &c in &calls {
            let cargs = Operation::get_op::<CallOp>(c, ctx).unwrap().args(ctx);
            let kept: Vec<_> = cargs
                .iter()
                .zip(&live)
                .filter(|p| *p.1)
                .map(|p| *p.0)
                .collect();
            let nc =
                CallOp::new(ctx, CallOpCallable::Direct(id.clone()), nty, kept).get_operation();
            nc.insert_before(ctx, c);
            if c.deref(ctx).get_num_results() > 0 {
                let (old, new) = (c.deref(ctx).get_result(0), nc.deref(ctx).get_result(0));
                old.replace_all_uses_with(ctx, &new);
            }
            let mut info = st.calls.remove(&c).unwrap();
            info.fn_ty = nty.into();
            info.exts = exts.clone();
            st.calls.insert(nc, info);
            if let Some(u) = st.invokes.remove(&c) {
                st.invokes.insert(nc, u);
            }
            if st.last_call == Some(c) {
                st.last_call = Some(nc);
            }
            Operation::erase(c, ctx);
        }
        n += live.iter().filter(|&&l| !l).count();
    }
    if std::env::var("PLIRON_STATS").is_ok() {
        eprintln!("deadarg {}: {n} params removed", st.cgu);
    }
}

pub fn run(ctx: &mut Context, st: &mut State<'_>) {
    let internal = crate::lower::internal_fns(ctx, st);
    let sites = call_sites(ctx, st, &internal);
    if std::env::var("PLIRON_STATS").is_ok() {
        let (mut total, mut dead, mut ro) = (0, 0, 0);
        for s in &internal {
            let f = &st.funcs[s];
            let e = Operation::get_op::<FuncOp>(f.op, ctx)
                .unwrap()
                .get_entry_block(ctx)
                .unwrap();
            for a in e.deref(ctx).arguments() {
                total += 1;
                if !a.is_used(ctx) {
                    dead += 1;
                } else if matches!(classify(ctx, a.get_type(ctx)), TyK::Ptr)
                    && a.uses(ctx)
                        .into_iter()
                        .all(|u| Operation::is_op::<LoadOp>(u.user_op(), ctx))
                {
                    ro += 1;
                }
            }
        }
        eprintln!(
            "abi {}: {} internal fns, {total} params, {dead} dead, {ro} load-only ptrs",
            st.cgu,
            internal.len()
        );
    }
    let mut n = 0;
    let mut cands: Vec<&String> = internal.iter().collect();
    cands.sort();
    for sym in cands {
        let f = &st.funcs[sym];
        let Some(sty) = f.sret_ty else { continue };
        if f.exts.params.first() != Some(&ArgExt::SRet) {
            continue;
        }
        let mut leaves = Vec::new();
        if !scalar_leaves(ctx, sty, 0, &mut leaves) || !fits_regs(ctx, &leaves) {
            continue;
        }
        let calls = sites.get(sym).cloned().unwrap_or_default();
        if calls
            .iter()
            .any(|c| st.calls.get(c).is_none_or(|i| i.fn_ty != f.ty))
        {
            continue;
        }
        rewrite(ctx, st, sym, sty, &leaves, &calls);
        n += 1;
    }
    if std::env::var("PLIRON_STATS").is_ok() {
        eprintln!("sret2reg {}: {n} functions", st.cgu);
    }
}

fn rewrite(
    ctx: &mut Context,
    st: &mut State<'_>,
    sym: &str,
    sty: TypeHandle,
    leaves: &[(u64, TypeHandle)],
    calls: &[Ptr<Operation>],
) {
    let fop = st.funcs[sym].op;
    let old_ty = st.funcs[sym].ty;
    let TyK::Func(_, args, _) = classify(ctx, old_ty) else {
        unreachable!()
    };
    let tys: Vec<TypeHandle> = leaves.iter().map(|l| l.1).collect();
    let rty: TypeHandle = StructType::get_unnamed(ctx, (tys, StructLayout::Unpacked)).into();
    let nty = FuncType::get(ctx, rty, args[1..].to_vec(), false);

    // Callee: the sret pointer becomes a local slot, returns load its leaves.
    let (size, align) = size_align(ctx, sty);
    let entry = Operation::get_op::<FuncOp>(fop, ctx)
        .unwrap()
        .get_entry_block(ctx)
        .unwrap();
    let i8t = IntegerType::get(ctx, 8, Signedness::Signless).into();
    let i32t = IntegerType::get(ctx, 32, Signedness::Signless).into();
    let arr = ArrayType::get(ctx, i8t, size).into();
    let one = mk_const(ctx, st, i32t, ConstVal::Bits(1));
    let a = AllocaOp::new(ctx, arr, one, 0).get_operation();
    a.insert_at_front(entry, ctx);
    let slot = a.deref(ctx).get_result(0);
    st.allocas.insert(slot, (size, align));
    let sret = entry.deref(ctx).get_argument(0);
    sret.replace_all_uses_with(ctx, &slot);
    BasicBlock::remove_argument(entry, ctx, 0);
    let rets: Vec<_> = blocks(ctx, fop)
        .into_iter()
        .flat_map(|b| ops(ctx, b))
        .filter(|&op| Operation::is_op::<ReturnOp>(op, ctx))
        .collect();
    for r in rets {
        let mut agg = mk_const(ctx, st, rty, ConstVal::Undef);
        for (i, &(off, t)) in leaves.iter().enumerate() {
            let p = ptr_plus(ctx, st, slot, off, r);
            let ld = LoadOp::new(ctx, p, t).get_operation();
            ld.insert_before(ctx, r);
            let v = ld.deref(ctx).get_result(0);
            let iv = InsertValueOp::new(ctx, agg, v, vec![i as u32]).get_operation();
            iv.insert_before(ctx, r);
            agg = iv.deref(ctx).get_result(0);
        }
        ReturnOp::new(ctx, Some(agg))
            .get_operation()
            .insert_before(ctx, r);
        Operation::erase(r, ctx);
    }
    Operation::get_op::<FuncOp>(fop, ctx)
        .unwrap()
        .set_attr_llvm_func_type(ctx, TypeAttr::new(nty.into()));
    let f = st.funcs.get_mut(sym).unwrap();
    f.ty = nty.into();
    f.exts.params.remove(0);
    f.exts.ret = ArgExt::None;
    let exts = f.exts.clone();

    // Callers: call without the slot pointer, then store the leaves into it.
    let id = st.sym_to_ident[sym].clone();
    for &c in calls {
        let cargs = Operation::get_op::<CallOp>(c, ctx).unwrap().args(ctx);
        let nc = CallOp::new(
            ctx,
            CallOpCallable::Direct(id.clone()),
            nty,
            cargs[1..].to_vec(),
        )
        .get_operation();
        nc.insert_before(ctx, c);
        let r = nc.deref(ctx).get_result(0);
        for (i, &(off, _)) in leaves.iter().enumerate() {
            let ev = ExtractValueOp::new(ctx, r, vec![i as u32])
                .unwrap()
                .get_operation();
            ev.insert_before(ctx, c);
            let v = ev.deref(ctx).get_result(0);
            let p = ptr_plus(ctx, st, cargs[0], off, c);
            StoreOp::new(ctx, v, p)
                .get_operation()
                .insert_before(ctx, c);
        }
        let mut info = st.calls.remove(&c).unwrap();
        info.fn_ty = nty.into();
        info.exts = exts.clone();
        st.calls.insert(nc, info);
        if let Some(u) = st.invokes.remove(&c) {
            st.invokes.insert(nc, u);
        }
        if st.last_call == Some(c) {
            st.last_call = Some(nc);
        }
        Operation::erase(c, ctx);
    }
    let _: TypedHandle<FuncType> = nty;
}
