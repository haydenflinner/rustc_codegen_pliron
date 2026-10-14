//! Side tables for a plain pliron-llvm module. The rustc backend fills
//! [`State`] while building IR; other producers (CubeCL, parsed `.pliron`
//! files) emit ordinary `ConstantOp`/`AddressOfOp`/`AllocaOp`s instead,
//! and [`scan`] derives the same tables in one walk.

use cranelift_module::Linkage;
use pliron::basic_block::BasicBlock;
use pliron::builtin::attributes::{
    FPBFloatAttr, FPDoubleAttr, FPHalfAttr, FPSingleAttr, IntegerAttr,
};
use pliron::builtin::attr_interfaces::TypedAttrInterface;
use pliron::builtin::op_interfaces::{
    CallOpInterface, SingleBlockRegionInterface, SymbolOpInterface,
};
use pliron::linked_list::ContainsLinkedList;
use pliron::builtin::ops::ModuleOp;
use pliron::context::{Context, Ptr};
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed, TypedHandle};
use rustc_apfloat::Float;
use pliron::builtin::type_interfaces::FunctionTypeInterface;
use pliron_llvm::attributes::LinkageAttr;
use pliron_llvm::op_interfaces::AlignableOpInterface;
use pliron_llvm::ops::{
    AddressOfOp, AllocaOp, AtomicRmwOp, CallIntrinsicOp, CallOp, ConstantOp, FuncOp, GlobalOp,
    UndefOp,
};
use pliron_llvm::types::FuncType;

use crate::context::{ArgExt, CallInfo, ConstVal, Exts, FuncInfo, GlobalInfo, State};
use crate::types::size_align;

fn linkage(l: Option<&LinkageAttr>, defined: bool) -> Linkage {
    match l {
        Some(LinkageAttr::InternalLinkage | LinkageAttr::PrivateLinkage) => Linkage::Local,
        Some(
            LinkageAttr::WeakAnyLinkage
            | LinkageAttr::WeakODRLinkage
            | LinkageAttr::LinkOnceAnyLinkage
            | LinkageAttr::LinkOnceODRLinkage,
        ) if defined => Linkage::Preemptible,
        _ if defined => Linkage::Export,
        _ => Linkage::Import,
    }
}

fn exts(ctx: &Context, fty: TypeHandle) -> Exts {
    let n = TypedHandle::<FuncType>::from_handle(fty, ctx)
        .map(|t| t.deref(ctx).arg_types().len())
        .unwrap_or(0);
    Exts {
        params: vec![ArgExt::None; n],
        ret: ArgExt::None,
    }
}

fn const_of(ctx: &Context, op: &ConstantOp) -> Option<ConstVal> {
    let v = op.get_value(ctx);
    let a = v.as_any();
    let bits = if let Some(i) = a.downcast_ref::<IntegerAttr>() {
        i.value().to_u128()
    } else if let Some(f) = a.downcast_ref::<FPSingleAttr>() {
        f.0.to_bits()
    } else if let Some(f) = a.downcast_ref::<FPDoubleAttr>() {
        f.0.to_bits()
    } else if let Some(f) = a.downcast_ref::<FPHalfAttr>() {
        f.0.to_bits()
    } else if let Some(f) = a.downcast_ref::<FPBFloatAttr>() {
        f.0.to_bits()
    } else {
        return None;
    };
    Some(ConstVal::Bits(bits))
}

/// `llvm.fabs.v4f32` -> `llvm.fabs`: the lowerer keys intrinsics by base
/// name and reads types from the operands, like the rustc builder emits.
fn base_intrinsic(n: &str) -> String {
    fn is_ty(s: &str) -> bool {
        let s = match s.strip_prefix('v') {
            Some(r) if r.starts_with(|c: char| c.is_ascii_digit()) => {
                r.trim_start_matches(|c: char| c.is_ascii_digit())
            }
            _ => s,
        };
        matches!(s, "f16" | "bf16" | "f32" | "f64" | "f128")
            || (s.len() > 1
                && (s.starts_with('i') || s.starts_with('p'))
                && s[1..].bytes().all(|c| c.is_ascii_digit()))
    }
    let mut parts: Vec<&str> = n.split('.').collect();
    while parts.len() > 2 && is_ty(parts[parts.len() - 1]) {
        parts.pop();
    }
    parts.join(".")
}

fn walk_block(ctx: &Context, st: &mut State, b: Ptr<BasicBlock>) {
    let ops: Vec<_> = b.deref(ctx).iter(ctx).collect();
    for op in ops {
        if let Some(c) = Operation::get_op::<ConstantOp>(op, ctx) {
            if let Some(cv) = const_of(ctx, &c) {
                st.consts.insert(op.deref(ctx).get_result(0), cv);
            }
        } else if let Some(a) = Operation::get_op::<AddressOfOp>(op, ctx) {
            let sym = a.get_global_name(ctx).to_string();
            st.consts
                .insert(op.deref(ctx).get_result(0), ConstVal::Sym { sym, off: 0 });
        } else if let Some(a) = Operation::get_op::<AllocaOp>(op, ctx) {
            let ety = TypedAttrInterface::get_type(&*a.get_attr_llvm_alloca_element_type(ctx).unwrap(), ctx);
            let (sz, al) = size_align(ctx, ety);
            let n = match st.consts.get(&op.deref(ctx).get_operand(0)) {
                Some(ConstVal::Bits(n)) => *n as u64,
                _ => panic!("pliron-clif-lower: dynamic alloca size"),
            };
            let al = a.alignment(ctx).map_or(al, |x| al.max(u64::from(x)));
            st.allocas
                .insert(op.deref(ctx).get_result(0), (sz * n, al.max(1)));
        } else if let Some(c) = Operation::get_op::<CallOp>(op, ctx) {
            let fn_ty = c.callee_type(ctx);
            st.calls.insert(
                op,
                CallInfo {
                    fn_ty,
                    exts: exts(ctx, fn_ty),
                    foreign: true,
                    cc: None,
                    tail: false,
                    nocap: None,
                    arg_extent: None,
                },
            );
        } else if let Some(c) = Operation::get_op::<CallIntrinsicOp>(op, ctx) {
            let n = c.get_attr_llvm_intrinsic_name(ctx).unwrap();
            st.intrinsics.insert(op, base_intrinsic(&String::from(n.clone())));
        } else if let Some(r) = Operation::get_op::<AtomicRmwOp>(op, ctx) {
            st.rmw
                .insert(op, r.get_attr_llvm_rmw_kind(ctx).unwrap().clone());
        }
        let regions: Vec<_> = op.deref(ctx).regions().collect();
        for r in regions {
            let blocks: Vec<_> = r.deref(ctx).iter(ctx).collect();
            for b in blocks {
                walk_block(ctx, st, b);
            }
        }
    }
}

/// Cranelift-level pass switches (`PLIRON_<PASS>=0` disables one).
pub fn pass_flags(st: &mut State) {
    use pliron_cfg::pass_enabled;
    st.notrap = pass_enabled("PLIRON_NOTRAP");
    st.jumpthread = pass_enabled("PLIRON_JUMPTHREAD");
    st.loadfwd = pass_enabled("PLIRON_LOADFWD");
    st.slot_dse = pass_enabled("PLIRON_SLOT_DSE");
    st.dse = pass_enabled("PLIRON_DSE");
    st.peep = pass_enabled("PLIRON_PEEP");
    st.tailmerge = pass_enabled("PLIRON_TAILMERGE");
    st.unreach = pass_enabled("PLIRON_UNREACH");
    st.taildup = pass_enabled("PLIRON_TAILDUP");
    st.licm = pass_enabled("PLIRON_LICM");
    st.indvars = pass_enabled("PLIRON_INDUCT");
    st.loopidiom = pass_enabled("PLIRON_IDIOM");
    st.loopvec = pass_enabled("PLIRON_VEC");
    st.slp = pass_enabled("PLIRON_SLP");
    st.bcheck = pass_enabled("PLIRON_BCHECK");
    st.tailrec = pass_enabled("PLIRON_TAILREC");
}

/// Build the side tables [`crate::lower::lower_module`] reads for a module
/// that was not produced by the rustc backend. Globals get detached
/// zero/undef initializer values, like rustc's constants.
pub fn scan(ctx: &mut Context, module: Ptr<Operation>) -> State {
    let mut st = State::default();
    let body = Operation::get_op::<ModuleOp>(module, ctx)
        .unwrap()
        .get_body(ctx, 0);
    let top: Vec<_> = body.deref(ctx).iter(ctx).collect();
    for op in top {
        if let Some(f) = Operation::get_op::<FuncOp>(op, ctx) {
            let id = f.get_symbol_name(ctx);
            let name = id.to_string();
            let defined = crate::has_body(ctx, op);
            let ty: TypeHandle = f.get_type(ctx).into();
            let l = linkage(f.get_attr_llvm_function_linkage(ctx).as_deref(), defined);
            let nparams = exts(ctx, ty).params.len();
            st.funcs.insert(
                name.clone(),
                FuncInfo {
                    op,
                    ty,
                    linkage: l,
                    exts: exts(ctx, ty),
                    no_inline: false,
                    always_inline: false,
                    inline_hint: false,
                    cold: false,
                    nounwind: true,
                    sret_ty: None,
                    frozen: Vec::new(),
                    noalias: Vec::new(),
                    nonnull: Vec::new(),
                    nparams,
                },
            );
            st.ident_to_sym.insert(name.clone(), name.clone());
            st.sym_to_ident.insert(name, id);
            if defined {
                crate::scalarize::run(ctx, op);
                let regions: Vec<_> = op.deref(ctx).regions().collect();
                for r in regions {
                    let blocks: Vec<_> = r.deref(ctx).iter(ctx).collect();
                    for b in blocks {
                        walk_block(ctx, &mut st, b);
                    }
                }
            }
        } else if let Some(g) = Operation::get_op::<GlobalOp>(op, ctx) {
            let id = g.get_symbol_name(ctx);
            let name = id.to_string();
            let ty = g.get_type(ctx);
            let defined = g.get_initializer_value(ctx).is_some()
                || g.get_initializer_region(ctx).is_some()
                || g.get_attr_llvm_global_linkage(ctx).is_none();
            let (_, align) = size_align(ctx, ty);
            let init = defined.then(|| {
                let v = UndefOp::new(ctx, ty).get_operation().deref(ctx).get_result(0);
                st.consts.insert(v, ConstVal::Zero);
                v
            });
            st.globals.insert(
                name.clone(),
                GlobalInfo {
                    ty,
                    init,
                    align: align.max(1),
                    mutable: !g.is_constant(ctx),
                    tls: false,
                    linkage: linkage(g.get_attr_llvm_global_linkage(ctx).as_deref(), defined),
                    used: false,
                    section: None,
                },
            );
            st.ident_to_sym.insert(name.clone(), name.clone());
            st.sym_to_ident.insert(name, id);
        }
    }
    pass_flags(&mut st);
    st
}
