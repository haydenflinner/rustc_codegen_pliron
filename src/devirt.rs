//! Guarded devirtualization of `dyn Trait` calls without a profile
//! (`PLIRON_DEVIRT=1`, opt-in), and the rustc-side facts for the
//! profile-driven version (`pliron_ir_opt::devirt`, on by default in
//! profile builds): which profiled targets get a home alias and which
//! get a body in the guarding CGU ([`targets`]), and how a target is
//! guarded and called from one CGU ([`IcpCfg::resolve`]).
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
    /// The profiled targets of this crate and its dependencies (empty in
    /// the instrumented build).
    pub targets: &'static Targets,
    /// `PLIRON_DEVIRT_DOWNSTREAM=1`: also guard targets defined in crates
    /// that depend on this one (a closure the final binary passes in).
    /// The object then only links into programs that define them, and
    /// nothing checks that the profile still describes those crates.
    pub downstream: bool,
}

/// The profile's indirect-call targets as this crate sees them
/// (profile-use build; computed once, from the profile and the
/// partition, so every CGU decides alike).
pub struct Targets {
    /// `(symbol, hash of its CGU's name)` of every profiled address that
    /// is a mono item of that CGU of this crate: the home definitions.
    /// The home CGU defines `devirt::home_alias` for each, and guards in
    /// other CGUs compare against it. Not here: addresses the profile
    /// took from cross-CGU copies, and (`PLIRON_XCGU_HOME=mono`) from
    /// copies of a shared item that go back to an import of its owner.
    pub homes: rustc_hash::FxHashSet<(String, u64)>,
    /// The same, per home CGU, sorted.
    pub by_cgu: FxHashMap<u64, Vec<String>>,
    pub bodies: &'static Bodies,
}

/// The part of [`Targets`] the cross-CGU copy selection reads
/// (`xcgu::copies`; `homes` in turn depends on what is copied).
#[derive(Default)]
pub struct Bodies {
    /// Symbol hash of a function with profiled sites -> the symbols its
    /// sites reached.
    wanted: FxHashMap<u64, Vec<String>>,
    /// Profiled symbol -> the instance to copy next to a guard: a mono
    /// item of this crate, or a dependency's function that this crate
    /// can instantiate itself (its MIR is available here).
    inst: FxHashMap<String, Instance<'static>>,
    /// Profiled symbol of a dependency -> the symbol of this crate's own
    /// instantiation (`inst`), and back.
    pub dep_copy: FxHashMap<String, String>,
    pub dep_local: FxHashMap<String, String>,
    /// `(profiled symbol, CGU name)` of this crate's mono items that
    /// carry a profiled symbol, and whether the item is `inlined` there.
    items: Vec<(String, String, bool)>,
}

impl Bodies {
    pub fn is_empty(&self) -> bool {
        self.inst.is_empty()
    }

    /// The instances worth a body in a CGU that holds `caller`: the
    /// profiled targets of its indirect-call sites, by profiled symbol.
    pub fn wanted_by<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        caller: Instance<'tcx>,
    ) -> impl Iterator<Item = (&str, Instance<'tcx>)> {
        let names = if self.wanted.is_empty() {
            None
        } else {
            self.wanted.get(&pliron_ir_opt::pgo::sym_hash(tcx.symbol_name(caller).name))
        };
        names.into_iter().flatten().filter_map(move |n| {
            // One rustc process, one `TyCtxt`: see `index`.
            let i = *self.inst.get(n)?;
            Some((n.as_str(), unsafe { std::mem::transmute::<Instance<'static>, Instance<'tcx>>(i) }))
        })
    }
}

/// [`Bodies`] for the crate being compiled.
pub fn bodies<'tcx>(tcx: TyCtxt<'tcx>) -> &'static Bodies {
    static T: std::sync::OnceLock<Bodies> = std::sync::OnceLock::new();
    T.get_or_init(|| build_bodies(tcx).unwrap_or_default())
}

/// [`Targets`] for the crate being compiled.
pub fn targets<'tcx>(tcx: TyCtxt<'tcx>) -> &'static Targets {
    static T: std::sync::OnceLock<Targets> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let bodies = bodies(tcx);
        let mut out = Targets { homes: Default::default(), by_cgu: Default::default(), bodies };
        let Some(sum) = pliron_ir_opt::pgo::summary().filter(|_| !bodies.items.is_empty()) else {
            return out;
        };
        let local = tcx.stable_crate_id(rustc_hir::def_id::LOCAL_CRATE).as_u64();
        let mono_home = crate::env_get("PLIRON_XCGU_HOME") == Some("mono");
        let mut items: FxHashSet<(&str, u64)> = FxHashSet::default();
        for (name, cgu, inlined) in &bodies.items {
            // As `module_codegen` marks `xcgu_extra`: this CGU's body
            // becomes an import of the elected owner's.
            let goes_home = mono_home
                && *inlined
                && crate::xcgu::inlined_home(tcx).get(name.as_str()).is_some_and(|o| o != cgu);
            if !goes_home {
                items.insert((name, pliron_ir_opt::pgo::cgu_hash(cgu)));
            }
        }
        for (_, t) in &sum.icalls {
            if t.crate_id == local
                && items.contains(&(t.name.as_str(), t.cgu))
                && out.homes.insert((t.name.clone(), t.cgu))
            {
                out.by_cgu.entry(t.cgu).or_default().push(t.name.clone());
            }
        }
        for v in out.by_cgu.values_mut() {
            v.sort();
        }
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::pgo",
                "pliron-pgo: icp targets: {} home aliases, {} bodies of dependencies available here, {} functions with sites",
                out.homes.len(), bodies.dep_copy.len(), bodies.wanted.len());
        }
        out
    })
}

fn build_bodies<'tcx>(tcx: TyCtxt<'tcx>) -> Option<Bodies> {
    use pliron_ir_opt::pgo;
    use rustc_middle::mono::MonoItem;
    if !pliron_ir_opt::devirt::icp_on() || tcx.sess.target.is_like_wasm || !crate::env_set("PLIRON_PGO_USE") {
        return None;
    }
    let sum = pgo::summary()?;
    if sum.icalls.is_empty() {
        return None;
    }
    let local = tcx.stable_crate_id(rustc_hir::def_id::LOCAL_CRATE).as_u64();
    let mut out = Bodies::default();
    let mut local_names: FxHashSet<&str> = FxHashSet::default();
    let mut dep_names: FxHashMap<u64, FxHashSet<&str>> = FxHashMap::default();
    for (caller, t) in &sum.icalls {
        let w = out.wanted.entry(*caller).or_default();
        if !w.contains(&t.name) {
            w.push(t.name.clone());
        }
        if t.crate_id == local {
            local_names.insert(&t.name);
        } else {
            dep_names.entry(t.crate_id).or_default().insert(&t.name);
        }
    }
    // This crate's targets: the mono items carrying a profiled symbol.
    let parts = tcx.collect_and_partition_mono_items(()).codegen_units;
    let mut inst: FxHashMap<String, Instance<'tcx>> = FxHashMap::default();
    for p in parts {
        for (&item, data) in p.items() {
            let MonoItem::Fn(i) = item else { continue };
            let sym = tcx.symbol_name(i).name;
            let Some(&name) = local_names.get(sym) else { continue };
            inst.entry(name.to_string()).or_insert(i);
            out.items.push((name.to_string(), p.name().as_str().to_string(), data.inlined));
        }
    }
    out.items.sort();
    // Dependencies' targets this crate can instantiate itself.
    if !crate::env_is("PLIRON_DEVIRT_DEPCOPY", "0") {
        for &c in tcx.crates(()).iter() {
            let Some(names) = dep_names.get(&tcx.stable_crate_id(c).as_u64()) else { continue };
            for (name, i) in dep_instances(tcx, c, names, parts) {
                out.dep_copy.insert(name.clone(), tcx.symbol_name(i).name.to_string());
                out.dep_local.insert(tcx.symbol_name(i).name.to_string(), name.clone());
                inst.insert(name, i);
            }
        }
    }
    out.inst = unsafe {
        std::mem::transmute::<FxHashMap<String, Instance<'tcx>>, FxHashMap<String, Instance<'static>>>(inst)
    };
    Some(out)
}

/// The profiled symbols `names` of dependency `krate` that this crate
/// can compile a copy of: instances whose symbol, as `krate` mangles
/// it, is one of `names` and whose MIR is available here — generic
/// instances and closures of generic or `#[inline]` functions always,
/// other functions and their closures when rustc made them cross-crate
/// inlinable (small leaf bodies). A plain function of a dependency has
/// no MIR downstream and stays a linked call.
fn dep_instances<'tcx>(
    tcx: TyCtxt<'tcx>,
    krate: rustc_hir::def_id::CrateNum,
    names: &FxHashSet<&str>,
    parts: &'tcx [rustc_middle::mono::CodegenUnit<'tcx>],
) -> Vec<(String, Instance<'tcx>)> {
    use rustc_hir::def::DefKind;
    use rustc_middle::mono::MonoItem;
    let mut out: Vec<(String, Instance<'tcx>)> = Vec::new();
    let mut left: FxHashSet<&str> = names.clone();
    // The symbol `krate` gave its own instantiation of `i`.
    let there = |i: Instance<'tcx>| rustc_symbol_mangling::symbol_name_for_instance_in_crate(tcx, i, krate);
    let mut take = |i: Instance<'tcx>, left: &mut FxHashSet<&str>| {
        let n = there(i);
        if let Some(&name) = left.get(n.as_str()) {
            left.remove(name);
            out.push((name.to_string(), i));
        }
    };
    // 1. Instances this crate instantiates anyway (a generic function
    //    both crates use with the same arguments).
    let mut seen: FxHashSet<Instance<'tcx>> = FxHashSet::default();
    for p in parts {
        for item in p.items().keys() {
            if left.is_empty() {
                break;
            }
            if let MonoItem::Fn(i) = *item
                && i.args.non_erasable_generics().next().is_some()
                && seen.insert(i)
            {
                take(i, &mut left);
            }
        }
    }
    // 2. The dependency's own non-generic functions and closures with
    //    MIR in its metadata.
    if !left.is_empty() {
        let n = rustc_metadata::creader::CStore::from_tcx(tcx).num_def_ids_untracked(krate);
        for index in 0..n {
            if left.is_empty() {
                break;
            }
            let def_id = rustc_hir::def_id::DefId { krate, index: rustc_hir::def_id::DefIndex::from_usize(index) };
            let i = match tcx.def_kind(def_id) {
                DefKind::Fn | DefKind::AssocFn => {
                    if tcx.generics_of(def_id).requires_monomorphization(tcx) || !tcx.is_mir_available(def_id) {
                        continue;
                    }
                    Instance::mono(tcx, def_id)
                }
                DefKind::Closure if tcx.is_coroutine(def_id) => continue,
                DefKind::Closure => {
                    let root = tcx.typeck_root_def_id(def_id);
                    if tcx.generics_of(root).requires_monomorphization(tcx) || !tcx.is_mir_available(def_id) {
                        continue;
                    }
                    let ty = tcx.type_of(def_id).instantiate_identity().skip_norm_wip();
                    let ty::Closure(_, args) = *ty.kind() else { continue };
                    Instance::new_raw(def_id, tcx.erase_and_anonymize_regions(args))
                }
                _ => continue,
            };
            // Only when the name is wanted: is the body closed (see
            // `closed_downstream`)? A miss in rustc's collector is fatal.
            if left.contains(there(i).as_str()) {
                match closed_downstream(tcx, def_id) {
                    Ok(()) => take(i, &mut left),
                    Err(why) => {
                        if pliron_cfg::tracing_on() {
                            tracing::info!(target: "pliron::pgo", "pliron-pgo: icp: {} has MIR here but cannot be instantiated ({why})", there(i));
                        }
                    }
                }
            }
        }
    }
    if pliron_cfg::tracing_on() && !left.is_empty() {
        let mut l: Vec<&&str> = left.iter().collect();
        l.sort();
        tracing::info!(target: "pliron::pgo", "pliron-pgo: icp: {} profiled targets of {} have no body available here (no MIR in its metadata): {l:?}",
            left.len(), tcx.crate_name(krate));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// A dependency: its hash, its rlib, and (read on first use) the symbols
/// the rlib defines for other objects — home aliases among them.
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
            targets: targets(tcx),
            downstream: crate::env_is("PLIRON_DEVIRT_DOWNSTREAM", "1"),
        })
    }

    /// How the CGU `cgu` (hash of its name) guards and calls the
    /// profiled target `t`, or why it cannot. The guard symbol is the
    /// address the profile saw and must link: the target itself when its
    /// home is this CGU, else the home's alias — defined by a CGU of
    /// this crate (`Targets::homes`), or by a dependency, verified in
    /// this build (see [`Dep`]).
    pub fn resolve(
        &self,
        st: &pliron_ir_opt::state::State,
        cgu: u64,
        t: &pliron_ir_opt::pgo::ProfTarget,
    ) -> Result<pliron_ir_opt::devirt::Resolved, &'static str> {
        use pliron_ir_opt::devirt::{Resolved, home_alias};
        let body =
            |n: &str| st.funcs.get(n).is_some_and(|f| f.linkage != cranelift_module::Linkage::Import);
        let item = t.flags & pliron_ir_opt::pgo::TARGET_ITEM != 0;
        let alias = home_alias(&t.name, t.cgu);
        if t.crate_id == self.crate_id {
            if t.cgu == cgu && body(&t.name) {
                return Ok(Resolved { guard: t.name.clone(), call: t.name.clone(), tag: "L" });
            }
            if !self.targets.homes.contains(&(t.name.clone(), t.cgu)) {
                return Err("profiled address is a cross-CGU copy, or not a mono item of that CGU any more");
            }
            return Ok(if body(&t.name) {
                Resolved { guard: alias, call: t.name.clone(), tag: "C" }
            } else {
                Resolved { guard: alias.clone(), call: alias, tag: "X" }
            });
        }
        let Some(dep) = self.deps.get(&t.crate_id) else {
            return if self.downstream && item {
                Ok(Resolved { guard: alias.clone(), call: alias, tag: "X" })
            } else {
                Err("defined downstream")
            };
        };
        if self.linked {
            // The dependency's objects are on disk: see for ourselves.
            match dep.symbols(self.macho) {
                Some(s) if s.contains(&alias) => {}
                Some(_) => return Err("its rlib defines no home alias for it"),
                None => return Err("dependency rlib unreadable"),
            }
        } else if !item {
            return Err("profiled address is a cross-CGU copy in another crate");
        } else if dep.hash != t.crate_hash || dep.hash == 0 {
            // Same sources and options as profiled, so the same aliases.
            return Err("dependency changed since the profile (crate hash)");
        }
        Ok(match self.targets.bodies.dep_copy.get(&t.name) {
            Some(l) if body(l) => Resolved { guard: alias, call: l.clone(), tag: "C" },
            _ => Resolved { guard: alias.clone(), call: alias, tag: "X" },
        })
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

/// Blind mode's linkable names: symbols of vtable slot instances that
/// are `Internal` and held by exactly one CGU (closures, private or
/// `#[inline]` impl methods, shims). `xcgu::promoted` adds them, so that
/// CGU defines them hidden-external and a blind guard in another CGU can
/// refer to the very address the vtable holds. They are address-taken
/// already, so nothing that needs a private symbol applied to them.
/// Empty in profile builds, which name a target through its home alias
/// instead and leave every linkage as it is.
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
        // Blind mode only. Profile builds name a target through its home
        // alias (`pliron_ir_opt::devirt::home_alias`), which leaves the
        // function's linkage alone — in the instrumented and in the
        // profile-use build alike, so both lay code out the same way.
        if !enabled() || tcx.sess.target.is_like_wasm {
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
        for &i in index(tcx).values().flatten() {
            let Some(&(true, inlined, 1)) = home.get(&MonoItem::Fn(i)) else { continue };
            if inlined
                && !i.def_id().is_local()
                && i.args.non_erasable_generics().next().is_none()
            {
                continue;
            }
            out.insert(tcx.symbol_name(i).name.to_string());
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

// ---- bodies of profiled targets for downstream crates ----
//
// A guard buys something only when the call behind it can be inlined, and
// a crate can compile a copy of a dependency's function only when the
// dependency's metadata holds its MIR. rustc encodes MIR for what is
// *reachable*: generic and `#[inline]` functions that downstream code can
// name, and what their bodies mention. A closure inside a plain function
// — `Box::new(move |x| ..) as Box<dyn Fn(..)>`, the commonest promoted
// target there is — is not, so the crate that calls it through the `dyn`
// could only ever get a guarded direct call (html-rewriter: 46k of 192k
// profiled calls go to two such closures of lol_html).
//
// The profile-use build of the defining crate therefore adds its
// profiled closure targets to `reachable_set`, closed the way rustc
// closes it: what the closure's MIR mentions must itself be linkable or
// instantiable downstream. A private function it calls becomes
// reachable (an exported symbol when it is not inlinable, a body with
// its own closure of references when it is), private statics become
// exported, nested closures come along. Anything this walk does not
// understand — thread locals, inline asm, vtable or coroutine
// constants, a generic or constant-evaluated private item — drops the
// target: nothing is added for it and it stays a linked call.
//
// The downstream crate does not trust this: before it instantiates a
// dependency's function the collector did not ask for
// ([`dep_instances`]), [`closed_downstream`] walks the same references
// and requires each to be linkable or to have MIR, since rustc's
// collector answers a miss with a fatal error.

/// What a MIR body refers to that has to exist at link time or be
/// instantiated with it.
struct Refs<'tcx> {
    tcx: TyCtxt<'tcx>,
    fns: Vec<(rustc_hir::def_id::DefId, ty::GenericArgsRef<'tcx>)>,
    closures: Vec<rustc_hir::def_id::DefId>,
    statics: Vec<rustc_hir::def_id::DefId>,
    consts: Vec<rustc_hir::def_id::DefId>,
    seen_allocs: FxHashSet<rustc_middle::mir::interpret::AllocId>,
    bad: Option<&'static str>,
}

impl<'tcx> Refs<'tcx> {
    fn of(tcx: TyCtxt<'tcx>, def_id: rustc_hir::def_id::DefId) -> Refs<'tcx> {
        use rustc_middle::mir::visit::Visitor;
        let mut r = Refs {
            tcx,
            fns: Vec::new(),
            closures: Vec::new(),
            statics: Vec::new(),
            consts: Vec::new(),
            seen_allocs: FxHashSet::default(),
            bad: None,
        };
        r.visit_body(tcx.optimized_mir(def_id));
        for p in tcx.promoted_mir(def_id).iter() {
            r.visit_body(p);
        }
        r
    }

    fn scan_ty(&mut self, t: Ty<'tcx>) {
        for a in t.walk() {
            let ty::GenericArgKind::Type(t) = a.kind() else { continue };
            match *t.kind() {
                ty::FnDef(d, args) => self.fns.push((d, args.skip_binder())),
                ty::Closure(d, _) => self.closures.push(d),
                ty::Coroutine(..) | ty::CoroutineClosure(..) | ty::CoroutineWitness(..) => {
                    self.bad = Some("coroutine")
                }
                _ => {}
            }
        }
    }

    fn alloc(&mut self, id: rustc_middle::mir::interpret::AllocId, depth: usize) {
        use rustc_middle::mir::interpret::GlobalAlloc;
        if !self.seen_allocs.insert(id) {
            return;
        }
        if depth > 16 {
            self.bad = Some("deep constant");
            return;
        }
        match self.tcx.global_alloc(id) {
            GlobalAlloc::Memory(a) => {
                let ids: Vec<_> = a.inner().provenance().provenances().map(|p| p.alloc_id()).collect();
                for i in ids {
                    self.alloc(i, depth + 1);
                }
            }
            GlobalAlloc::Static(d) => self.statics.push(d),
            GlobalAlloc::Function { instance } => match instance.def {
                InstanceKind::Item(d) => self.fns.push((d, instance.args)),
                _ => self.bad = Some("shim address in a constant"),
            },
            GlobalAlloc::VTable(..) => self.bad = Some("vtable constant"),
            GlobalAlloc::TypeId { .. } => {}
        }
    }
}

impl<'tcx> rustc_middle::mir::visit::Visitor<'tcx> for Refs<'tcx> {
    fn visit_ty(&mut self, ty: Ty<'tcx>, _: rustc_middle::mir::visit::TyContext) {
        self.scan_ty(ty);
    }

    fn visit_const_operand(
        &mut self,
        c: &rustc_middle::mir::ConstOperand<'tcx>,
        _: rustc_middle::mir::Location,
    ) {
        use rustc_middle::mir::interpret::Scalar;
        use rustc_middle::mir::{Const, ConstValue};
        self.scan_ty(c.const_.ty());
        match c.const_ {
            Const::Val(ConstValue::Scalar(Scalar::Ptr(p, _)), _) => self.alloc(p.provenance.alloc_id(), 0),
            Const::Val(ConstValue::Indirect { alloc_id, .. }, _) => self.alloc(alloc_id, 0),
            Const::Val(ConstValue::Slice { alloc_id, .. }, _) => self.alloc(alloc_id, 0),
            Const::Val(..) => {}
            // A promoted of this body is walked with it.
            Const::Unevaluated(uv, _) if uv.promoted.is_some() => {}
            Const::Unevaluated(uv, _) => self.consts.push(uv.def),
            Const::Ty(..) => self.bad = Some("type-level constant"),
        }
    }

    fn visit_rvalue(&mut self, rv: &rustc_middle::mir::Rvalue<'tcx>, loc: rustc_middle::mir::Location) {
        if let rustc_middle::mir::Rvalue::ThreadLocalRef(_) = rv {
            self.bad = Some("thread local");
        }
        self.super_rvalue(rv, loc);
    }

    fn visit_terminator(
        &mut self,
        t: &rustc_middle::mir::Terminator<'tcx>,
        loc: rustc_middle::mir::Location,
    ) {
        if let rustc_middle::mir::TerminatorKind::InlineAsm { .. } = t.kind {
            self.bad = Some("inline asm");
        }
        self.super_terminator(t, loc);
    }
}

/// The definition a mentioned function item stands for: a trait method
/// resolves to its impl (`None`: nothing to link — a virtual call, an
/// intrinsic, a shim over types that were scanned on their own).
fn fn_def<'tcx>(
    tcx: TyCtxt<'tcx>,
    d: rustc_hir::def_id::DefId,
    args: ty::GenericArgsRef<'tcx>,
) -> Result<Option<rustc_hir::def_id::DefId>, &'static str> {
    if tcx.trait_of_assoc(d).is_none() {
        return Ok(Some(d));
    }
    if args.has_non_region_param() || args.has_escaping_bound_vars() {
        return Err("trait method on a type parameter");
    }
    let Ok(args) = tcx.try_normalize_erasing_regions(ty::TypingEnv::fully_monomorphized(), ty::Unnormalized::new_wip(args))
    else {
        return Err("trait method arguments do not normalize");
    };
    match Instance::try_resolve(tcx, ty::TypingEnv::fully_monomorphized(), d, args) {
        Ok(Some(i)) => Ok(match i.def {
            InstanceKind::Item(d) => Some(d),
            _ => None,
        }),
        _ => Err("unresolved trait method"),
    }
}

/// A function or closure outside any generic item (its body has no type
/// parameters to substitute).
fn concrete(tcx: TyCtxt<'_>, d: rustc_hir::def_id::DefId) -> bool {
    !tcx.generics_of(tcx.typeck_root_def_id(d)).requires_monomorphization(tcx)
        && (tcx.is_closure_like(d) || !tcx.generics_of(d).requires_monomorphization(tcx))
}

/// Defining crate: what must join `base` (rustc's reachable set) so that
/// a downstream crate can instantiate `root`; `Err` when the walk meets
/// something it cannot make available.
fn close_upstream<'tcx>(
    tcx: TyCtxt<'tcx>,
    base: &rustc_hir::def_id::LocalDefIdSet,
    root: rustc_hir::def_id::LocalDefId,
) -> Result<Vec<rustc_hir::def_id::LocalDefId>, &'static str> {
    use rustc_hir::def::DefKind;
    let mut add = Vec::new();
    let mut seen: FxHashSet<rustc_hir::def_id::LocalDefId> = FxHashSet::default();
    let mut work = vec![root];
    let budget = 64;
    while let Some(l) = work.pop() {
        if base.contains(&l) || !seen.insert(l) {
            continue;
        }
        if add.len() >= budget {
            return Err("too much to export");
        }
        add.push(l);
        let d = l.to_def_id();
        match tcx.def_kind(d) {
            DefKind::Static { .. } => {
                if tcx.is_thread_local_static(d) {
                    return Err("thread local");
                }
                continue;
            }
            DefKind::Closure => {}
            DefKind::Fn | DefKind::AssocFn => {
                if tcx.is_foreign_item(d) {
                    add.pop();
                    continue;
                }
                // Not inlinable: reachable makes it an exported symbol,
                // and its body stays in this crate.
                if !tcx.cross_crate_inlinable(d) {
                    continue;
                }
            }
            _ => return Err("unexpected kind of item"),
        }
        if !concrete(tcx, d) || !tcx.is_mir_available(d) {
            return Err("generic private item");
        }
        let refs = Refs::of(tcx, d);
        if let Some(why) = refs.bad {
            return Err(why);
        }
        for (f, args) in refs.fns {
            let Some(f) = fn_def(tcx, f, args)? else { continue };
            let Some(fl) = f.as_local() else { continue };
            if base.contains(&fl) {
                continue;
            }
            match tcx.def_kind(f) {
                DefKind::Ctor(..) => {}
                DefKind::Fn | DefKind::AssocFn | DefKind::Closure => {
                    if tcx.intrinsic(f).is_some() {
                        continue;
                    }
                    if !concrete(tcx, f) {
                        return Err("generic private item");
                    }
                    work.push(fl);
                }
                _ => return Err("unexpected kind of item"),
            }
        }
        for c in refs.closures {
            if let Some(cl) = c.as_local()
                && !base.contains(&cl)
            {
                if !concrete(tcx, c) {
                    return Err("generic private item");
                }
                work.push(cl);
            }
        }
        for s in refs.statics {
            if let Some(sl) = s.as_local() {
                work.push(sl);
            }
        }
        if refs.consts.iter().any(|c| c.as_local().is_some_and(|c| !base.contains(&c))) {
            return Err("private constant item");
        }
    }
    Ok(add)
}

/// Using crate: can `root`, a function of a dependency with MIR in its
/// metadata, be instantiated here — is everything its body (and the
/// bodies of the concrete functions that come with it) mentions linkable
/// or instantiable? rustc guarantees it for what its own reachability
/// pass exported; this checks what [`provide`]'s addition must have.
fn closed_downstream<'tcx>(tcx: TyCtxt<'tcx>, root: rustc_hir::def_id::DefId) -> Result<(), &'static str> {
    use rustc_hir::def::DefKind;
    let mut seen: FxHashSet<rustc_hir::def_id::DefId> = FxHashSet::default();
    let mut work = vec![root];
    while let Some(d) = work.pop() {
        if !seen.insert(d) {
            continue;
        }
        if seen.len() > 256 {
            return Err("too many bodies to check");
        }
        if !tcx.is_mir_available(d) {
            return Err("no MIR in the dependency's metadata");
        }
        let refs = Refs::of(tcx, d);
        if let Some(why) = refs.bad {
            return Err(why);
        }
        // What the collector will ask of a function this body uses
        // (`should_codegen_locally`).
        let mut need = |f: rustc_hir::def_id::DefId, work: &mut Vec<_>| -> Result<(), &'static str> {
            if f.is_local() || tcx.is_foreign_item(f) || tcx.is_reachable_non_generic(f) {
                return Ok(());
            }
            if matches!(tcx.def_kind(f), DefKind::Ctor(..)) || tcx.intrinsic(f).is_some() {
                return Ok(());
            }
            if !tcx.is_mir_available(f) {
                return Err("uses a private function of its crate");
            }
            // Generic bodies were reachable by rustc's own rules, and so
            // closed by them.
            if concrete(tcx, f) {
                work.push(f);
            }
            Ok(())
        };
        for (f, args) in refs.fns {
            if let Some(f) = fn_def(tcx, f, args)? {
                need(f, &mut work)?;
            }
        }
        for c in refs.closures {
            need(c, &mut work)?;
        }
        for s in refs.statics {
            if !s.is_local() && !tcx.is_reachable_non_generic(s) {
                return Err("uses a private static of its crate");
            }
        }
        // Constant items always have their MIR in the metadata, and the
        // defining crate adds nothing whose body names a private one.
        let _ = refs.consts;
    }
    Ok(())
}

type ReachableSet = for<'tcx> fn(TyCtxt<'tcx>, ()) -> rustc_hir::def_id::LocalDefIdSet;
static PREV_REACHABLE: std::sync::OnceLock<ReachableSet> = std::sync::OnceLock::new();

/// `CodegenBackend::provide`: in a profile-use build with promotion on,
/// `reachable_set` also holds this crate's profiled closure targets
/// (see above). `PLIRON_DEVIRT_EXPORT=0` leaves the query alone.
pub(crate) fn provide(providers: &mut rustc_middle::util::Providers) {
    if !pliron_ir_opt::devirt::icp_on()
        || !crate::env_set("PLIRON_PGO_USE")
        || crate::env_is("PLIRON_DEVIRT_EXPORT", "0")
    {
        return;
    }
    let _ = PREV_REACHABLE.set(providers.queries.reachable_set);
    providers.queries.reachable_set = reachable_set;
}

fn reachable_set<'tcx>(tcx: TyCtxt<'tcx>, (): ()) -> rustc_hir::def_id::LocalDefIdSet {
    let mut set = (PREV_REACHABLE.get().expect("devirt: provider installed"))(tcx, ());
    // Nothing depends on a crate that is only an executable. The
    // profile is not an input the incremental dep graph knows.
    if tcx.sess.target.is_like_wasm
        || tcx.sess.opts.incremental.is_some()
        || !tcx.sess.opts.output_types.should_codegen()
        || tcx.crate_types().iter().all(|t| *t == rustc_structures::CrateType::Executable)
    {
        return set;
    }
    let Some(sum) = pliron_ir_opt::pgo::summary() else { return set };
    let local = tcx.stable_crate_id(rustc_hir::def_id::LOCAL_CRATE).as_u64();
    let names: FxHashSet<&str> = sum
        .icalls
        .iter()
        .filter(|(_, t)| t.crate_id == local && t.flags & pliron_ir_opt::pgo::TARGET_ITEM != 0)
        .map(|(_, t)| t.name.as_str())
        .collect();
    if names.is_empty() {
        return set;
    }
    // The profiled targets among this crate's closures outside generic
    // items (a closure of a generic or `#[inline]` function is reachable
    // with it already).
    let mut roots: Vec<(String, rustc_hir::def_id::LocalDefId)> = Vec::new();
    for l in tcx.hir_body_owners() {
        let d = l.to_def_id();
        if set.contains(&l) || !matches!(tcx.def_kind(d), rustc_hir::def::DefKind::Closure) || !concrete(tcx, d) {
            continue;
        }
        let ty::Closure(_, args) = *tcx.type_of(d).instantiate_identity().skip_norm_wip().kind() else { continue };
        let i = Instance::new_raw(d, tcx.erase_and_anonymize_regions(args));
        let sym = tcx.symbol_name(i).name;
        if names.contains(sym) {
            roots.push((sym.to_string(), l));
        }
    }
    roots.sort_by(|a, b| a.0.cmp(&b.0));
    for (sym, root) in roots {
        match close_upstream(tcx, &set, root) {
            Ok(add) => {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::pgo", "pliron-pgo: icp: {sym} made instantiable downstream ({} items reachable for it)", add.len());
                }
                set.extend(add);
            }
            Err(why) => {
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::pgo", "pliron-pgo: icp: {sym} stays private to this crate ({why})");
                }
            }
        }
    }
    set
}
