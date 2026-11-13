//! Guarded devirtualization of `dyn Trait` calls without a profile
//! (`PLIRON_DEVIRT=1`, opt-in), and the rustc-side facts for the
//! profile-driven version (`pliron_ir_opt::devirt`, on by default in
//! profile builds): which symbols to promote so a guard can name them
//! ([`promotable`]) and which profiled targets link ([`IcpCfg`]).
//!
//! rustc knows which methods the vtables of this crate hold (see
//! `index`); when few of them can be the callee (`PLIRON_DEVIRT_MAX`,
//! default 2, see [`plan`]), a virtual call becomes
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
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_middle::ty::{self, Instance, InstanceKind, Ty, TyCtxt, TypeVisitableExt};
use rustc_target::callconv::FnAbi;

use crate::context::CodegenCx;

/// Blind guards: `PLIRON_DEVIRT=1` in a build without a profile. Profile
/// builds use the value profile instead (`pliron_ir_opt::devirt`, on by
/// default there).
pub fn enabled() -> bool {
    crate::env_is("PLIRON_DEVIRT", "1") && !pliron_ir_opt::devirt::profile_build()
}

fn knob(name: &str, default: usize) -> usize {
    crate::env_get(name).and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// The targets to guard at one site, from its usable candidates (sorted
/// by symbol): all of them when there are at most `PLIRON_DEVIRT_MAX`
/// (default 2), else none — without a profile nothing says which to
/// prefer.
pub fn plan<'tcx>(c: Cands<'tcx>) -> Result<(Vec<Instance<'tcx>>, String), String> {
    let Cands { targets, note } = c;
    let max = knob("PLIRON_DEVIRT_MAX", 2);
    if targets.len() > max {
        return Err(format!(
            "{} candidates, more than {max}, and no profile to rank them; {note}",
            targets.len()
        ));
    }
    Ok((targets, note))
}

/// What `pliron_ir_opt::devirt::promote` needs from rustc, captured once
/// per crate (the deferred pipeline runs without a `TyCtxt`).
pub struct IcpCfg {
    /// This crate's `StableCrateId`, and its hash (SVH; 0 for a crate
    /// nothing can depend on): what the instrumented build stores as the
    /// defining crate of each target.
    pub crate_id: u64,
    pub crate_hash: u64,
    /// Every crate this one depends on, by `StableCrateId`: its symbols
    /// are part of any link this crate is part of.
    pub deps: FxHashMap<u64, Dep>,
    /// This crate is linked right here (a binary, a dylib, ...): cargo
    /// starts such a compile only when its dependencies' rlibs are
    /// complete, so they can be read. A plain rlib may be compiled while
    /// its dependencies are still in codegen (pipelining).
    pub linked: bool,
    pub macho: bool,
    /// Symbols of this crate another CGU can link: `xcgu::home_linkable`
    /// and the promoted vtable slot instances ([`promotable`]).
    pub home: &'static rustc_hash::FxHashSet<String>,
    pub promoted: &'static rustc_hash::FxHashSet<String>,
    /// `PLIRON_DEVIRT_DOWNSTREAM=1`: also guard targets defined in crates
    /// that depend on this one (a closure the final binary passes in).
    /// The object then only links into programs that define them, and
    /// nothing checks that the profile still describes those crates.
    pub downstream: bool,
}

/// A dependency: its hash, its rlib, and (read on first use) the symbols
/// the rlib defines for other objects.
pub struct Dep {
    pub hash: u64,
    pub rlib: Option<std::path::PathBuf>,
    syms: std::sync::OnceLock<Option<rustc_hash::FxHashSet<String>>>,
}

impl Dep {
    /// The external definitions in the rlib's objects, without the
    /// Mach-O underscore; `None` when it cannot be read.
    fn symbols(&self, macho: bool) -> Option<&rustc_hash::FxHashSet<String>> {
        use object::read::archive::ArchiveFile;
        use object::{Object, ObjectSymbol};
        self.syms
            .get_or_init(|| {
                let data = std::fs::read(self.rlib.as_ref()?).ok()?;
                let ar = ArchiveFile::parse(&*data).ok()?;
                let mut out = rustc_hash::FxHashSet::default();
                for m in ar.members() {
                    let m = m.ok()?;
                    // Members that are not objects (`lib.rmeta`) are skipped.
                    let Ok(obj) = object::File::parse(m.data(&*data).ok()?) else { continue };
                    for s in obj.symbols() {
                        if s.is_definition()
                            && s.is_global()
                            && let Ok(n) = s.name()
                        {
                            let n = if macho { n.strip_prefix('_').unwrap_or(n) } else { n };
                            out.insert(n.to_string());
                        }
                    }
                }
                Some(out)
            })
            .as_ref()
    }
}

impl IcpCfg {
    pub fn new(tcx: TyCtxt<'_>) -> Option<Self> {
        if !pliron_ir_opt::devirt::icp_on() || tcx.sess.target.is_like_wasm {
            return None;
        }
        let local = rustc_hir::def_id::LOCAL_CRATE;
        let hash = |c| tcx.crate_hash(c).as_u128() as u64;
        Some(IcpCfg {
            crate_id: tcx.stable_crate_id(local).as_u64(),
            // Only a crate with metadata has a hash to ask for.
            crate_hash: if tcx.needs_hir_hash() { hash(local) } else { 0 },
            deps: tcx
                .crates(())
                .iter()
                .map(|&c| {
                    let rlib = tcx
                        .used_crate_source(c)
                        .paths()
                        .find(|p| p.extension().is_some_and(|e| e == "rlib"))
                        .cloned();
                    (
                        tcx.stable_crate_id(c).as_u64(),
                        Dep { hash: hash(c), rlib, syms: std::sync::OnceLock::new() },
                    )
                })
                .collect(),
            linked: tcx.crate_types().iter().any(|t| *t != rustc_structures::CrateType::Rlib),
            macho: tcx.sess.target.is_like_darwin,
            home: crate::xcgu::home_linkable(tcx),
            promoted: promotable(tcx),
            downstream: crate::env_is("PLIRON_DEVIRT_DOWNSTREAM", "1"),
        })
    }

    /// Does a reference to `t`, which has no body in the current CGU,
    /// link, and is its address the one the profile saw?
    pub fn linkable(&self, t: &pliron_ir_opt::pgo::ProfTarget) -> Result<(), &'static str> {
        let external = t.flags & pliron_ir_opt::pgo::TARGET_EXTERNAL != 0;
        // Symbol names are unique across crates: one of ours that this
        // build defines for other CGUs, whatever the profile's age.
        if self.home.contains(&t.name) || self.promoted.contains(&t.name) {
            return Ok(());
        }
        if t.crate_id == self.crate_id {
            return Err("private to another CGU");
        }
        let Some(dep) = self.deps.get(&t.crate_id) else {
            return if self.downstream && external {
                Ok(())
            } else {
                Err("defined downstream")
            };
        };
        if self.linked {
            // The dependency's objects are on disk: see for ourselves.
            match dep.symbols(self.macho) {
                Some(s) if s.contains(&t.name) => Ok(()),
                Some(_) => Err("not an external symbol of its rlib"),
                None => Err("dependency rlib unreadable"),
            }
        } else if !external {
            Err("private to another crate")
        } else if dep.hash == t.crate_hash && dep.hash != 0 {
            // Same sources and options as profiled, so the same symbols.
            Ok(())
        } else {
            Err("dependency changed since the profile (crate hash)")
        }
    }
}

/// What the vtables instantiated in this crate hold, per trait method:
/// `(trait method, its args past Self)` -> the slot instances, one per
/// concrete `Self` that some code of this crate unsizes to a `dyn` of
/// that trait (or of a subtrait). The slot instance comes from
/// `Instance::expect_resolve_for_vtable`, the function rustc fills
/// vtables with, so impl methods, provided methods, closure bodies and
/// shims (`VTable`, `ClosureOnce`, `Reify`, ...) are all exactly the
/// address a vtable holds.
///
/// The `(Self, dyn Trait)` pairs are recomputed the way the mono
/// collector finds them: every `Unsize` pointer coercion in the MIR of
/// every mono item. Not seen: vtables built in upstream crates and
/// vtables that only exist in constants (`static T: &dyn Tr = &X`,
/// promoted `&dyn` values); calls on those objects take the indirect arm.
type Key<'tcx> = (rustc_hir::def_id::DefId, Vec<ty::GenericArg<'tcx>>);
type Index<'tcx> = FxHashMap<Key<'tcx>, Vec<Instance<'tcx>>>;

fn index<'tcx>(tcx: TyCtxt<'tcx>) -> &'tcx Index<'tcx> {
    // One rustc process compiles one crate with one `TyCtxt`, and every
    // CGU's codegen runs inside it: the interned types stored here are
    // only read back under the same `'tcx`.
    static INDEX: std::sync::OnceLock<Index<'static>> = std::sync::OnceLock::new();
    let ix = INDEX.get_or_init(|| unsafe {
        std::mem::transmute::<Index<'tcx>, Index<'static>>(build_index(tcx))
    });
    unsafe { std::mem::transmute::<&'static Index<'static>, &'tcx Index<'tcx>>(ix) }
}

fn build_index<'tcx>(tcx: TyCtxt<'tcx>) -> Index<'tcx> {
    use rustc_middle::mir::{CastKind, Rvalue, StatementKind};
    use rustc_middle::mono::MonoItem;
    use rustc_middle::ty::adjustment::PointerCoercion;
    let env = ty::TypingEnv::fully_monomorphized();
    let mut seen: FxHashSet<Instance<'tcx>> = FxHashSet::default();
    let mut pairs: FxHashSet<(Ty<'tcx>, Ty<'tcx>)> = FxHashSet::default();
    for cgu in tcx.collect_and_partition_mono_items(()).codegen_units {
        for item in cgu.items().keys() {
            let MonoItem::Fn(inst) = *item else { continue };
            if !seen.insert(inst) {
                continue;
            }
            let body = tcx.instance_mir(inst.def);
            for bb in body.basic_blocks.iter() {
                for st in &bb.statements {
                    let StatementKind::Assign(asg) = &st.kind else { continue };
                    let rv = &asg.1;
                    let Rvalue::Cast(CastKind::PointerCoercion(PointerCoercion::Unsize, _), op, target) = rv
                    else {
                        continue;
                    };
                    let mono = |t: Ty<'tcx>| {
                        inst.try_instantiate_mir_and_normalize_erasing_regions(
                            tcx,
                            env,
                            ty::EarlyBinder::bind(tcx, t),
                        )
                        .ok()
                    };
                    let (Some(src), Some(dst)) = (mono(op.ty(body, tcx)), mono(*target)) else {
                        continue;
                    };
                    if let Some((src, dst)) = unsize_tails(tcx, src, dst, 0)
                        && dst.is_trait()
                        && !src.is_trait()
                    {
                        pairs.insert((src, dst));
                    }
                }
            }
        }
    }
    let mut ix: Index<'tcx> = FxHashMap::default();
    for (self_ty, dyn_ty) in pairs {
        let ty::Dynamic(preds, ..) = dyn_ty.kind() else { continue };
        let Some(principal) = preds.principal() else { continue };
        if self_ty.has_escaping_bound_vars() || dyn_ty.has_escaping_bound_vars() {
            continue;
        }
        let root = tcx.instantiate_bound_regions_with_erased(principal.with_self_ty(tcx, self_ty));
        for sup in rustc_type_ir::elaborate::supertraits(tcx, ty::Binder::dummy(root)) {
            let tr = tcx.instantiate_bound_regions_with_erased(sup);
            let Ok(tr) = tcx.try_normalize_erasing_regions(env, ty::Unnormalized::new_wip(tr)) else {
                continue;
            };
            for &method in tcx.own_existential_vtable_entries(tr.def_id) {
                // As `vtable_entries` builds the slot.
                let args = ty::GenericArgs::for_item(tcx, method, |param, _| match param.kind {
                    ty::GenericParamDefKind::Lifetime => tcx.lifetimes.re_erased.into(),
                    _ => tr.args[param.index as usize],
                });
                let Ok(args) = tcx.try_normalize_erasing_regions(env, ty::Unnormalized::new_wip(args))
                else {
                    continue;
                };
                if tcx.instantiate_and_check_impossible_clauses((method, args)) {
                    continue;
                }
                let slot =
                    Instance::expect_resolve_for_vtable(tcx, env, method, args, rustc_span::DUMMY_SP);
                let e = ix.entry((method, args[1..].to_vec())).or_default();
                if !e.contains(&slot) {
                    e.push(slot);
                }
            }
        }
    }
    // Hash-set iteration order must not leak into the guard order.
    for v in ix.values_mut() {
        v.sort_by_key(|i| tcx.symbol_name(*i).name);
    }
    ix
}

/// Symbols of vtable slot instances that are `Internal` and held by
/// exactly one CGU (closures, private or `#[inline]` impl methods, shims):
/// `xcgu::promoted` adds them, so that CGU defines them hidden-external
/// and a guard in any other object — another CGU of this crate, or by
/// name another crate — can refer to the very address the vtable holds.
/// They are address-taken already, so nothing that needs a private symbol
/// applied to them. The candidates depend only on the partition; the
/// profile-use build keeps those its profile names from another CGU.
///
/// An item rustc instantiates per user (`inlined`) qualifies only when
/// its symbol cannot also be this-crate-external somewhere else: its
/// definition is local, or it is a generic instance (whose symbol names
/// the instantiating crate). A non-generic `#[inline]` fn of another
/// crate has one name in every crate that copies it.
pub fn promotable<'tcx>(tcx: TyCtxt<'tcx>) -> &'static rustc_hash::FxHashSet<String> {
    use rustc_middle::mono::MonoItem;
    static P: std::sync::OnceLock<rustc_hash::FxHashSet<String>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let mut out = rustc_hash::FxHashSet::default();
        if (!enabled() && !pliron_ir_opt::devirt::icp_on()) || tcx.sess.target.is_like_wasm {
            return out;
        }
        // item -> (Internal, inlined, holders)
        let mut home: FxHashMap<MonoItem<'tcx>, (bool, bool, u32)> = FxHashMap::default();
        for p in tcx.collect_and_partition_mono_items(()).codegen_units {
            for (&item, data) in p.items() {
                let e = home.entry(item).or_insert((
                    data.linkage == rustc_attr_ir::Linkage::Internal,
                    data.inlined,
                    0,
                ));
                e.2 += 1;
            }
        }
        // Profile-use build: a promoted function gives up what the
        // optimizer does for private ones (measured on html-rewriter:
        // +0.3% instructions when the hot targets are promoted and no
        // guard uses them). Promote only a target some site outside its
        // own CGU names; the instrumented build promotes every
        // candidate, like blind mode.
        let named = crate::env_set("PLIRON_PGO_USE")
            .then(|| pliron_ir_opt::pgo::summary().map(|s| &s.icall_targets));
        // symbol hash -> the CGUs holding that function of this crate
        let mut holders: FxHashMap<u64, Vec<rustc_span::Symbol>> = FxHashMap::default();
        if named.is_some() {
            for p in tcx.collect_and_partition_mono_items(()).codegen_units {
                for item in p.items().keys() {
                    if let MonoItem::Fn(f) = *item {
                        let h = pliron_ir_opt::pgo::sym_hash(tcx.symbol_name(f).name);
                        holders.entry(h).or_default().push(p.name());
                    }
                }
            }
        }
        let holder_of = |i: Instance<'tcx>| {
            tcx.collect_and_partition_mono_items(())
                .codegen_units
                .iter()
                .find(|p| p.items().contains_key(&MonoItem::Fn(i)))
                .map(|p| p.name())
        };
        for &i in index(tcx).values().flatten() {
            let Some(&(true, inlined, 1)) = home.get(&MonoItem::Fn(i)) else { continue };
            if inlined
                && !i.def_id().is_local()
                && i.args.non_erasable_generics().next().is_none()
            {
                continue;
            }
            let sym = tcx.symbol_name(i).name;
            if let Some(named) = named {
                let Some(callers) = named.and_then(|n| n.get(sym)) else { continue };
                let here = holder_of(i);
                // A caller this crate does not hold is in another crate.
                let outside = callers.iter().any(|c| match holders.get(c) {
                    Some(cgus) => cgus.iter().any(|g| Some(*g) != here),
                    None => true,
                });
                if !outside {
                    continue;
                }
            }
            out.insert(sym.to_string());
        }
        out
    })
}

/// The collector's `find_tails_for_unsizing`: the (sized, unsized) pair of
/// innermost types an unsizing coercion `src -> dst` changes.
fn unsize_tails<'tcx>(
    tcx: TyCtxt<'tcx>,
    src: Ty<'tcx>,
    dst: Ty<'tcx>,
    depth: usize,
) -> Option<(Ty<'tcx>, Ty<'tcx>)> {
    let env = ty::TypingEnv::fully_monomorphized();
    if depth > 16 {
        return None;
    }
    match (src.kind(), dst.kind()) {
        (&ty::Pat(a, _), &ty::Pat(b, _)) => unsize_tails(tcx, a, b, depth + 1),
        (&ty::Ref(_, a, _), &ty::Ref(_, b, _) | &ty::RawPtr(b, _))
        | (&ty::RawPtr(a, _), &ty::RawPtr(b, _)) => {
            Some(tcx.struct_lockstep_tails_for_codegen(a, b, env))
        }
        _ if let (Some(a), Some(b)) = (src.boxed_ty(), dst.boxed_ty()) => {
            Some(tcx.struct_lockstep_tails_for_codegen(a, b, env))
        }
        (&ty::Adt(a_def, a_args), &ty::Adt(b_def, b_args)) if a_def == b_def => {
            use rustc_middle::traits::{ImplSource, ImplSourceUserDefinedData};
            let cu = tcx.lang_items().coerce_unsized_trait()?;
            let tr = ty::TraitRef::new(tcx, cu, [src, dst]);
            let Ok(ImplSource::UserDefined(ImplSourceUserDefinedData { impl_def_id, .. })) =
                tcx.codegen_select_candidate(env.as_query_input(tr))
            else {
                return None;
            };
            let ty::adjustment::CustomCoerceUnsized::Struct(idx) =
                tcx.coerce_unsized_info(*impl_def_id).ok()?.custom_kind?;
            let field = &a_def.non_enum_variant().fields[idx];
            let norm = |t| tcx.try_normalize_erasing_regions(env, t).ok();
            unsize_tails(tcx, norm(field.ty(tcx, a_args))?, norm(field.ty(tcx, b_args))?, depth + 1)
        }
        _ => None,
    }
}

/// The usable targets of one virtual call, sorted by symbol name, and a
/// note on what was not usable.
#[derive(Clone)]
pub struct Cands<'tcx> {
    pub targets: Vec<Instance<'tcx>>,
    /// e.g. `7 instantiated, 3 usable (2 not linkable, 2 ABI differs)`.
    pub note: String,
}

/// Direct-call targets for the virtual call `virt` (an
/// `InstanceKind::Virtual` instance whose `args` are the trait method's
/// with `Self = dyn Trait`), or the reason there are none. Memoized per
/// CGU: the answer depends on which symbols this CGU can link to.
pub fn candidates<'tcx>(
    cx: &CodegenCx<'tcx>,
    virt: Instance<'tcx>,
    fn_abi: &FnAbi<'tcx, Ty<'tcx>>,
) -> Result<Cands<'tcx>, String> {
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
) -> Result<Cands<'tcx>, String> {
    let tcx = cx.tcx;
    let InstanceKind::Virtual(method, _) = virt.def else {
        return Err("not a virtual call".into());
    };
    // Virtual drop glue reaches `call` as `Virtual(drop_in_place, 0)`.
    if tcx.trait_of_assoc(method).is_none() {
        return Err("not a trait method (virtual drop)".into());
    }
    if virt.args.is_empty() {
        return Err("unexpected generic args".into());
    }
    let Some(slots) = index(tcx).get(&(method, virt.args[1..].to_vec())) else {
        return Err("no vtable for this trait instantiated in this crate (upstream or constant vtables only)".into());
    };
    // Compile-time bound: each slot costs an fn ABI and a signature.
    let scan = knob("PLIRON_DEVIRT_SCAN", 512);
    if slots.len() > scan {
        return Err(format!("{} instantiated targets, more than {scan} to scan", slots.len()));
    }
    let (want_ty, want_exts) = cx.fn_sig(fn_abi);
    let want_exts = format!("{want_exts:?}");
    let mut out = Vec::new();
    let mut why: Vec<(String, usize)> = Vec::new();
    let mut reject = |w: String| match why.iter_mut().find(|(x, _)| *x == w) {
        Some(e) => e.1 += 1,
        None => why.push((w, 1)),
    };
    for &inst in slots {
        if matches!(inst.def, InstanceKind::Item(_)) && tcx.is_foreign_item(inst.def_id()) {
            reject("foreign item".into());
            continue;
        }
        if let Err(e) = same_abi(cx, inst, fn_abi, want_ty, &want_exts) {
            reject(e);
            continue;
        }
        if !linkable(cx, inst) {
            reject("not linkable from this CGU".into());
            continue;
        }
        out.push(inst);
    }
    let rejects =
        why.iter().map(|(w, n)| format!("{n} {w}")).collect::<Vec<_>>().join(", ");
    if out.is_empty() {
        return Err(format!("{} instantiated, none usable ({rejects})", slots.len()));
    }
    let note = if rejects.is_empty() {
        format!("{} instantiated, all usable", slots.len())
    } else {
        format!("{} instantiated, {} usable ({rejects})", slots.len(), out.len())
    };
    Ok(Cands { targets: out, note })
}

/// The direct callee must take exactly the lowered argument list of the
/// virtual call: same convention, same pass modes, same layouts past the
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
    // Layouts, not types: the two signatures can spell one type with
    // different (erased vs bound) regions.
    if abi.ret.layout.layout != virt_abi.ret.layout.layout || abi.ret.mode != virt_abi.ret.mode {
        if crate::env_set("PLIRON_DEVIRT_DEBUG") {
            return Err(format!(
                "ABI differs (return: {:?} {:?} vs {:?} {:?})",
                abi.ret.layout.ty, abi.ret.mode, virt_abi.ret.layout.ty, virt_abi.ret.mode
            ));
        }
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
        if a.layout.layout != v.layout.layout || a.mode != v.mode {
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
    if cx.st.borrow().funcs.get(sym).is_some_and(|f| f.linkage != cranelift_module::Linkage::Import) {
        return true;
    }
    if crate::xcgu::home_linkable(tcx).contains(sym) || promotable(tcx).contains(sym) {
        return true;
    }
    let def_id = inst.def_id();
    !def_id.is_local()
        && (tcx.is_reachable_non_generic(def_id) || inst.upstream_monomorphization(tcx).is_some())
}
