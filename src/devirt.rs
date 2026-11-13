//! Guarded devirtualization of `dyn Trait` calls (`PLIRON_DEVIRT=1`,
//! opt-in). rustc knows the set of impls of a trait; when few of them can
//! be the callee (`PLIRON_DEVIRT_MAX`, default 2), or a profile says
//! which ones are (see [`plan`]), a virtual call becomes
//!
//! ```text
//! fnptr = load vtable[idx]
//! if fnptr == &<Impl1 as Trait>::method  -> direct call (inlinable)
//! else if fnptr == &<Impl2 as Trait>::method -> direct call
//! else -> indirect call through fnptr
//! ```
//!
//! The guard compares the *loaded function pointer*, and the indirect arm
//! is always kept, so nothing depends on the impl set being complete
//! (downstream crates may add impls) or on function addresses being
//! unique (a CGU-local copy with its own address only makes the guard
//! miss). This module picks the candidates; `Builder::devirt_call` in
//! `builder.rs` emits the diamond.

use rustc_middle::ty::layout::FnAbiOf;
use rustc_middle::ty::{self, Instance, InstanceKind, Ty};
use rustc_target::callconv::FnAbi;

use crate::context::CodegenCx;

pub fn enabled() -> bool {
    crate::env_is("PLIRON_DEVIRT", "1")
}

fn knob(name: &str, default: usize) -> usize {
    crate::env_get(name).and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// A profile build, generating (`PLIRON_PGO`) or using (`PLIRON_PGO_USE`):
/// both must emit the same guard chains, since a profile only applies to
/// a function whose CFG fingerprint matches the instrumented build's.
pub fn profile_mode() -> bool {
    crate::env_set("PLIRON_PGO") || crate::env_set("PLIRON_PGO_USE")
}

/// The targets to guard at one site, from its usable candidates (sorted
/// by symbol). Without a profile: all of them when there are at most
/// `PLIRON_DEVIRT_MAX` (default 2), else none — nothing says which to
/// prefer. In a profile build: the first `PLIRON_DEVIRT_PROF_MAX`
/// (default 8); the hit blocks' counters rank them and
/// `pliron_ir_opt::devirt::prune` keeps the hot ones.
pub fn plan<'tcx>(mut cands: Vec<Instance<'tcx>>) -> Result<Vec<Instance<'tcx>>, String> {
    if profile_mode() {
        cands.truncate(knob("PLIRON_DEVIRT_PROF_MAX", 8));
        if cands.is_empty() {
            return Err("PLIRON_DEVIRT_PROF_MAX is 0".into());
        }
        return Ok(cands);
    }
    let max = knob("PLIRON_DEVIRT_MAX", 2);
    if cands.len() > max {
        return Err(format!("{} candidates, more than {max}, and no profile to rank them", cands.len()));
    }
    Ok(cands)
}

/// Direct-call targets for the virtual call `virt` (an
/// `InstanceKind::Virtual` instance whose `args` are the trait method's
/// with `Self = dyn Trait`), sorted by symbol name, or the reason there
/// are none. Memoized per CGU: the answer depends on which symbols this
/// CGU can link to. Impls that cannot serve this call (other trait args)
/// or have no single instance (generic, blanket) are simply not
/// candidates; the indirect arm covers them.
pub fn candidates<'tcx>(
    cx: &CodegenCx<'tcx>,
    virt: Instance<'tcx>,
    fn_abi: &FnAbi<'tcx, Ty<'tcx>>,
) -> Result<Vec<Instance<'tcx>>, String> {
    if let Some(r) = cx.devirt.borrow().get(&virt) {
        return r.clone();
    }
    let r = compute(cx, virt, fn_abi);
    cx.devirt.borrow_mut().insert(virt, r.clone());
    r
}

fn compute<'tcx>(
    cx: &CodegenCx<'tcx>,
    virt: Instance<'tcx>,
    fn_abi: &FnAbi<'tcx, Ty<'tcx>>,
) -> Result<Vec<Instance<'tcx>>, String> {
    let tcx = cx.tcx;
    let InstanceKind::Virtual(method, _) = virt.def else {
        return Err("not a virtual call".into());
    };
    // Virtual drop glue reaches `call` as `Virtual(drop_in_place, 0)`.
    let Some(trait_def) = tcx.trait_of_assoc(method) else {
        return Err("not a trait method (virtual drop)".into());
    };
    // A by-value `self` method sits in the vtable as a `VTable` shim that
    // takes the pointer; the method itself has another ABI.
    let sig = tcx.fn_sig(method).instantiate_identity().skip_norm_wip();
    if sig.inputs().skip_binder().is_empty()
        || sig.input(0).skip_binder().is_param(0)
        || !tcx.generics_of(method).has_self
    {
        return Err("by-value self receiver".into());
    }
    // Compile-time bound only: every impl examined costs a normalize, a
    // resolve and an fn ABI, and `dyn Debug` has thousands.
    let scan = knob("PLIRON_DEVIRT_SCAN", 64);
    if tcx.all_impls(trait_def).take(scan + 1).count() > scan {
        return Err(format!("more than {scan} impls to scan"));
    }
    let env = ty::TypingEnv::fully_monomorphized();
    let ntrait = tcx.generics_of(trait_def).count();
    if virt.args.len() < ntrait || ntrait == 0 {
        return Err("unexpected generic args".into());
    }
    let (want_ty, want_exts) = cx.fn_sig(fn_abi);
    let want_exts = format!("{want_exts:?}");
    let mut out = Vec::new();
    let mut why = String::from("no impls");
    for imp in tcx.all_impls(trait_def) {
        if tcx.impl_polarity(imp) != ty::ImplPolarity::Positive {
            why = "negative impl".into();
            continue;
        }
        // Lifetime-only generics are fine (erased); type/const params
        // (including blanket impls) have no single instance.
        if tcx.generics_of(imp).requires_monomorphization(tcx) {
            why = "generic impl".into();
            continue;
        }
        let impl_args = ty::GenericArgs::for_item(tcx, imp, |_, _| tcx.lifetimes.re_erased.into());
        let Ok(tr) = tcx.try_instantiate_and_normalize_erasing_regions(
            impl_args,
            env,
            tcx.impl_trait_ref(imp),
        ) else {
            why = "impl trait ref does not normalize".into();
            continue;
        };
        // `impl Trait<A> for T` only serves `dyn Trait<A>` calls.
        if tr.args.len() != ntrait || tr.args[1..] != virt.args[1..ntrait] {
            why = "impl for other trait args".into();
            continue;
        }
        let args = tcx.mk_args_from_iter(
            std::iter::once(tr.args[0]).chain(virt.args[1..].iter().copied()),
        );
        let Ok(Some(inst)) = Instance::try_resolve(tcx, env, method, args) else {
            why = "method does not resolve".into();
            continue;
        };
        // What `resolve_for_vtable` would wrap in a shim (`#[track_caller]`,
        // closures, nested virtual) is not what the vtable holds.
        if !matches!(inst.def, InstanceKind::Item(_)) || inst.def.requires_caller_location(tcx) {
            why = "vtable entry is a shim".into();
            continue;
        }
        if tcx.is_foreign_item(inst.def_id()) {
            why = "foreign item".into();
            continue;
        }
        if let Err(e) = same_abi(cx, inst, fn_abi, want_ty, &want_exts) {
            why = e;
            continue;
        }
        if !linkable(cx, inst) {
            why = "target not linkable from this CGU".into();
            continue;
        }
        if !out.contains(&inst) {
            out.push(inst);
        }
    }
    if out.is_empty() {
        return Err(why);
    }
    // One order for every build of this crate: the instrumented and the
    // profile-use build must lay the chain out identically.
    out.sort_by_key(|i| tcx.symbol_name(*i).name);
    Ok(out)
}

/// The direct callee must take exactly the lowered argument list of the
/// virtual call: same convention, same pass modes, same types past the
/// receiver, and a receiver that is one thin pointer (`&self`,
/// `Box<Self>`, `Rc<Self>`, `Pin<&mut Self>`, ... all are).
fn same_abi<'tcx>(
    cx: &CodegenCx<'tcx>,
    inst: Instance<'tcx>,
    virt_abi: &FnAbi<'tcx, Ty<'tcx>>,
    want_ty: pliron::r#type::TypeHandle,
    want_exts: &str,
) -> Result<(), String> {
    use rustc_target::callconv::PassMode;
    let abi = cx.fn_abi_of_instance(inst, ty::List::empty());
    if abi.conv != virt_abi.conv
        || abi.c_variadic != virt_abi.c_variadic
        || abi.args.len() != virt_abi.args.len()
        || abi.args.is_empty()
    {
        return Err("ABI differs (convention or arity)".into());
    }
    if abi.ret.layout.ty != virt_abi.ret.layout.ty || abi.ret.mode != virt_abi.ret.mode {
        return Err("ABI differs (return)".into());
    }
    let ptr = |a: &rustc_target::callconv::ArgAbi<'tcx, Ty<'tcx>>| {
        matches!(a.mode, PassMode::Direct(_))
            && matches!(
                a.layout.backend_repr,
                rustc_abi::BackendRepr::Scalar(s)
                    if matches!(s.primitive(), rustc_abi::Primitive::Pointer(_))
            )
    };
    if !ptr(&abi.args[0]) || !ptr(&virt_abi.args[0]) {
        return Err("receiver is not a thin pointer".into());
    }
    for (a, v) in abi.args.iter().zip(virt_abi.args.iter()).skip(1) {
        if a.layout.ty != v.layout.ty || a.mode != v.mode {
            return Err("ABI differs (argument)".into());
        }
    }
    let (t, e) = cx.fn_sig(abi);
    if t != want_ty || format!("{e:?}") != want_exts {
        return Err("lowered signature differs".into());
    }
    Ok(())
}

/// Will a reference to `inst`'s symbol from this CGU resolve at link
/// time? The mono collector never saw this use, so nothing was
/// instantiated or kept external for it: accept only a definition in
/// this CGU, a home definition other CGUs of this crate can bind
/// (`xcgu::home_linkable`), or what the collector itself would link
/// from an upstream crate.
fn linkable<'tcx>(cx: &CodegenCx<'tcx>, inst: Instance<'tcx>) -> bool {
    let tcx = cx.tcx;
    let sym = tcx.symbol_name(inst).name;
    {
        // An xcgu copy made here is not a reason in a profile build: the
        // copy set follows the profile (`PLIRON_PGO_XCGU`), and the
        // candidate list must not differ between gen and use builds.
        let st = cx.st.borrow();
        if st.funcs.get(sym).is_some_and(|f| f.linkage != cranelift_module::Linkage::Import)
            && !(profile_mode() && st.xcgu_extra.contains(sym))
        {
            return true;
        }
    }
    if crate::xcgu::home_linkable(tcx).contains(sym) {
        return true;
    }
    let def_id = inst.def_id();
    !def_id.is_local()
        && (tcx.is_reachable_non_generic(def_id) || inst.upstream_monomorphization(tcx).is_some())
}
