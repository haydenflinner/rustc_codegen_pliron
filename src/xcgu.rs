//! Cross-CGU local copies (`PLIRON_XCGU`). LLVM gets cross-CGU inlining from
//! thin-local LTO; here a CGU instead gets an internal copy of each small
//! callee that lives in another CGU of this crate, so the inliner can see its
//! body. A copy is made only when everything the body references resolves
//! from this CGU: defined here, used by this CGU's own items (so already
//! visible), or externally linked in its home CGU.

use rustc_attr_ir::Linkage;
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags as F;
use rustc_middle::mono::{CodegenUnit, CollectionMode, MonoItem};
use rustc_middle::ty::print::with_no_trimmed_paths;
use rustc_middle::ty::{Instance, InstanceKind, TyCtxt};
use rustc_span::Spanned;

// r[impl par.xcgu.eligible]
fn copyable<'tcx>(tcx: TyCtxt<'tcx>, inst: Instance<'tcx>, limit: usize) -> bool {
    // Items and shims (drop glue, closure-once, reify/fnptr, ...) all have
    // MIR bodies LLVM can inline across CGUs; other kinds (virtual,
    // intrinsic, vtable) have no copyable body here.
    if !matches!(inst.def, InstanceKind::Item(_) | InstanceKind::Shim(_)) {
        return false;
    }
    let a = tcx.codegen_instance_attrs(inst.def);
    let bad = F::NAKED
        | F::NO_MANGLE
        | F::RUSTC_STD_INTERNAL_SYMBOL
        | F::USED_COMPILER
        | F::USED_LINKER
        | F::COLD
        | F::THREAD_LOCAL
        | F::FOREIGN_ITEM
        | F::EXTERNALLY_IMPLEMENTABLE_ITEM
        | F::ALLOCATOR
        | F::DEALLOCATOR
        | F::REALLOCATOR
        | F::ALLOCATOR_ZEROED;
    // `InlineAttr::Never` fns can't feed the inliner themselves, but a
    // copy still resolves calls made by *other* copies (LLVM's ThinLTO
    // internalizes them the same way); `predefine_fn` marks the copy
    // `no_inline` so the semantics hold — calls stay calls.
    // `#[cold]` fns are copied too, like `InlineAttr::Never` ones: a copy
    // resolves calls made by other copies (`Vec::reserve` calls the cold
    // `do_reserve_and_handle`, internal to its home CGU, so `reserve`
    // itself was uncoverable). The inliner still charges cold callees
    // double. Opt-in: `PLIRON_XCGU_COLD=1`.
    let bad = if crate::env_is("PLIRON_XCGU_COLD", "1") { bad - F::COLD } else { bad };
    if a.flags.intersects(bad)
        || a.symbol_name.is_some()
        || a.linkage.is_some()
        || a.link_section.is_some()
    {
        return false;
    }
    // Non-Rust-ABI fns are FFI entry points and callbacks, handled by
    // address: a copy has its own address, so a pointer taken in this CGU
    // no longer equals the one taken in the home CGU. zlib-rs picks its
    // free path by `self.zfree == Allocator::RUST.zfree`; with
    // `zalloc_rust`/`zfree_rust` copied, alloc and free disagreed and
    // `deflateEnd` freed an over-aligned pointer (uiua's xlsx writer,
    // 2026-10-22). They rarely matter to the inliner anyway.
    if let InstanceKind::Item(def_id) = inst.def
        && matches!(tcx.def_kind(def_id), rustc_hir::def::DefKind::Fn | rustc_hir::def::DefKind::AssocFn)
        && !tcx.fn_sig(def_id).skip_binder().abi().is_rustic_abi()
    {
        return false;
    }
    // `PLIRON_XCGU_ALWAYS=1` (opt-in): `#[inline(always)]` bodies are
    // copyable at any size; they inline wherever they land, so a copy
    // costs IR-opt time, not code. memchr's `search_slice_with_raw` kept
    // pulldown's 7-statement `scan_nextline` from reaching its callers.
    // `PLIRON_PGO_XCGU=<size>` (opt-in, with `PLIRON_PGO_USE`): a callee
    // the profile saw entered at least as often as its hot count is
    // copyable up to `<size>`. The default limit keeps copies small
    // because most are speculative; a hot callee is where the calls are,
    // and without its body here `PLIRON_PGO_HOT` has nothing to inline
    // (LLVM's thin-local LTO imports hot callees at 10x its limit).
    let size = tcx.size_estimate(inst);
    if size > limit && limit != usize::MAX {
        static HOT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let hot_size =
            *HOT.get_or_init(|| crate::env_get("PLIRON_PGO_XCGU").and_then(|s| s.parse().ok()).unwrap_or(0));
        if size <= hot_size && pliron_ir_opt::pgo::hot_symbol(tcx.symbol_name(inst).name) {
            return true;
        }
    }
    size <= limit
        || (crate::env_is("PLIRON_XCGU_ALWAYS", "1")
            && matches!(
                a.inline,
                rustc_attr_ir::InlineAttr::Always | rustc_attr_ir::InlineAttr::Force { .. }
            ))
}

fn used<'tcx>(tcx: TyCtxt<'tcx>, inst: Instance<'tcx>) -> Option<&'tcx [Spanned<MonoItem<'tcx>>]> {
    tcx.items_of_instance((inst, CollectionMode::UsedItems))
        .ok()
        .map(|(u, _)| u)
}

/// Deps that need a definition here: `Virtual` calls stay indirect (they
/// dispatch through the vtable), so only `Item`/`Shim` callees count.
///
/// `#[rustc_intrinsic]` fns show up in `used` lists as `Item` instances
/// (via `rustc_intrinsic_const_stable_indirect` fallback bodies), and
/// most of them DO need a body — their callsites take
/// `IntrinsicResult::Fallback` and emit a real call (`integer_min`,
/// `carrying_mul_add`). The exception is the rotate/funnel family, which
/// `codegen_intrinsic_call` lowers to `llvm.fshl`/`llvm.fshr` at every
/// callsite — without the exemption `std::intrinsics::rotate_left` inside
/// SipHash makes `hash_one`'s copy closure uncoverable.
fn needs_body<'tcx>(tcx: TyCtxt<'tcx>, m: &MonoItem<'tcx>) -> bool {
    let MonoItem::Fn(inst) = m else {
        return true;
    };
    match inst.def {
        // Only the *intrinsics* by those names: `core::num::<impl
        // usize>::rotate_left` (an ordinary `#[inline]` method) needs its
        // body like any other callee. Matching the bare name exempted it,
        // and a copied `FxBuildHasher::hash_one` called a `rotate_left`
        // that was internal in its home CGU: undefined symbol at link
        // (nushell `nu-cmd-lang` tests, 2026-10-22).
        InstanceKind::Item(def_id) => !(tcx.intrinsic(def_id).is_some()
            && matches!(
                tcx.opt_item_name(def_id),
                Some(
                    rustc_span::sym::rotate_left
                        | rustc_span::sym::rotate_right
                        | rustc_span::sym::unchecked_funnel_shl
                        | rustc_span::sym::unchecked_funnel_shr
                )
            )),
        InstanceKind::Shim(_) => true,
        _ => false,
    }
}

/// `PLIRON_XCGU_SIZE` default: wasm gets a bigger one — copies exist to
/// feed the inliner, and wasm's IR-inline `WASM_LIMIT`+`WASM_HOT` reach
/// well past the native hot loop (std `hash_one`'s SipHash, the biggest
/// `bench/wasm` call miss, is ~250 MIR stmts).
fn size_limit<'tcx>(tcx: TyCtxt<'tcx>) -> usize {
    crate::env_get("PLIRON_XCGU_SIZE")
        .and_then(|s| s.parse().ok())
        .unwrap_or(
            if tcx.sess.target.arch == rustc_target::spec::Arch::Wasm32 {
                512
            } else {
                50
            },
        )
}

/// `PLIRON_XCGU_PROMOTE` (default `cold`): symbols of Internal fns defined in exactly
/// one CGU (not local copies) that some copyable fn calls but that can't
/// be copied themselves (too big, `#[cold]` chains, ...). Their home CGU
/// defines them hidden-external instead, ThinLTO's promotion of imported
/// functions' locals, so copies elsewhere bind to them: `Vec::reserve`
/// was uncoverable because `do_reserve_and_handle` -> `grow_amortized`
/// were internal to one CGU, and pulldown's `HtmlWriter::run` called it
/// at 30 sites. Computed once per crate.
pub fn promoted<'tcx>(tcx: TyCtxt<'tcx>) -> &'static FxHashSet<String> {
    static P: std::sync::OnceLock<FxHashSet<String>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let mut out = dep_promoted(tcx).clone();
        // wasm's copies feed the waffle inliner and link through
        // pliron-wasm-ld (same `is_like_wasm` gate as `home_mode`), so a
        // promoted home def would only grow the module.
        if tcx.sess.target.is_like_wasm {
            return out;
        }
        let parts = tcx.collect_and_partition_mono_items(()).codegen_units;
        if parts.len() <= 1 {
            return out;
        }
        if crate::env_get("PLIRON_XCGU_PROMOTE") == Some("copyable") {
            // `=copyable`: also promote single-home Internal items that some
            // CGU actually copies — the only items whose duplicated bodies
            // could bind a linkable home def instead. The copy set itself is
            // the signal: the referencing CGU's demand comes through
            // `copies()`, not the partition used-lists. `is_promoted` inside
            // `copies` reads `dep_promoted` only, so acceptance (and hence
            // inlining reachability) stays identical to the `cold` baseline
            // — an earlier draft promoted every copyable item and grew
            // `kernels` text ~10 KB through extra accepted copies.
            // item -> (linkage, inlined, partitions holding it)
            let mut home: FxHashMap<MonoItem<'tcx>, (Linkage, bool, u32)> = FxHashMap::default();
            for p in parts {
                for (&item, data) in p.items() {
                    let e = home.entry(item).or_insert((data.linkage, data.inlined, 0));
                    e.2 += 1;
                }
            }
            for p in parts {
                for inst in copies(tcx, p) {
                    let item = MonoItem::Fn(inst);
                    if matches!(home.get(&item), Some(&(Linkage::Internal, false, 1))) {
                        out.insert(tcx.symbol_name(inst).name.to_string());
                    }
                }
            }
        }
        if crate::env_get("PLIRON_XCGU_HOME") == Some("mono") {
            // `PLIRON_XCGU_HOME=mono`: promote the elected owner's def of
            // every shared `inlined` mono item so the other partitions'
            // bodies can convert to imports after inlining.
            out.extend(inlined_home(tcx).keys().cloned());
        }
        out
    })
}

/// `PLIRON_XCGU_HOME=mono`: rustc gives every partition that reaches an
/// `inlined` mono item its own `Internal` copy ("CGU-private copies" —
/// shared generics and drop glue), and each copy emits a `non-external`
/// body the linker can't merge: on `syn` that was 205 functions totalling
/// ~430 KB of rlib text. Maps each such item's symbol to the CGU name
/// elected to emit the single definition — the partition holding the
/// item as a real (`inlined=false`) root when there is one, else the
/// first holder — so `predefine_fn` promotes that def hidden-external
/// (`promoted`) while the other partitions' copies, suppressed in
/// `xcgu_extra`, become `Import`s in `home_copies`.
pub fn inlined_home<'tcx>(tcx: TyCtxt<'tcx>) -> &'static FxHashMap<String, String> {
    static M: std::sync::OnceLock<FxHashMap<String, String>> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let parts = tcx.collect_and_partition_mono_items(()).codegen_units;
        // Instances some CGU copies as `extra`: a sole inlined holder's
        // def still needs promoting when foreign copies exist to bind it.
        let mut copied: FxHashSet<String> = FxHashSet::default();
        for p in parts {
            copied.extend(copies(tcx, p).iter().map(|i| tcx.symbol_name(*i).name.to_string()));
        }
        // sym -> (root holder, first inlined holder, holders, any inlined,
        //         always-elect)
        let mut h: FxHashMap<String, (Option<String>, Option<String>, u32, bool, bool)> =
            FxHashMap::default();
        for p in parts {
            for (&item, data) in p.items() {
                let MonoItem::Fn(inst) = item else { continue };
                let e = h
                    .entry(tcx.symbol_name(inst).name.to_string())
                    .or_default();
                e.2 += 1;
                // Drop glue has a small MIR size estimate but expands
                // into a real body codegen can't inline away — it
                // survives in every holder, so dedup can't lose.
                e.4 |= matches!(
                    inst.def,
                    InstanceKind::Shim(rustc_middle::ty::ShimKind::DropGlue(_, _))
                );
                if data.inlined {
                    e.1.get_or_insert_with(|| p.name().as_str().to_string());
                    e.3 = true;
                } else {
                    e.0 = Some(p.name().as_str().to_string());
                }
            }
        }
        // An item `inlined` in at least one holder gets an owner only when
        // another partition actually duplicates it (a second holder or an
        // `extra` copy): a real-root holder wins, else the first inlined
        // holder. Ordinary small bodies inline away in every CGU and
        // their owner def would emit for nothing (`dead_fns` can't strip
        // `Hidden`), so plain `Item`s need a body over the copy-size bar
        // (`PLIRON_XCGU_MONOMIN`, default 200 MIR stmts — past that the
        // floor stops mattering: every elected owner is a body that
        // plausibly survives inlining); shims and uncopyable bodies
        // always earn it.
        let limit = crate::env_get("PLIRON_XCGU_MONOMIN")
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| 200);
        let mut est: FxHashMap<String, usize> = FxHashMap::default();
        for p in parts {
            for (&item, data) in p.items() {
                let MonoItem::Fn(inst) = item else { continue };
                let e = est
                    .entry(tcx.symbol_name(inst).name.to_string())
                    .or_default();
                *e = (*e).max(data.size_estimate);
            }
        }
        h.into_iter()
            .filter(|(s, (_, _, n, any_inlined, always))| {
                *any_inlined
                    && (*always || est.get(s.as_str()).copied().unwrap_or(0) >= limit)
                    && (*n >= 2 || copied.contains(s.as_str()))
            })
            .filter_map(|(s, (root, inl, _, _, _))| root.or(inl).map(|o| (s, o)))
            .collect()
    })
}

/// The original promotion set — `PLIRON_XCGU_PROMOTE` `cold`/`1`/`0`
/// modes on dependencies of copyable fns. `is_promoted` consults this
/// (not `promoted`) so the `copyable` experiment can't change which
/// copies `copies()` accepts.
fn dep_promoted<'tcx>(tcx: TyCtxt<'tcx>) -> &'static FxHashSet<String> {
    static P: std::sync::OnceLock<FxHashSet<String>> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let mut out = FxHashSet::default();
        // `=cold` promotes only `#[cold]` deps: enough for `reserve` (its
        // one blocker is the cold `do_reserve_and_handle`) without
        // unlocking every other copy (`=1` reshuffled regex's inlining).
        // Default `cold`; `=1` promotes every uncopyable dep, `=0` none.
        let mode = crate::env_get("PLIRON_XCGU_PROMOTE");
        let cold_only = mode.is_none_or(|m| m == "cold" || m == "copyable");
        if mode == Some("0") {
            return out;
        }
        let parts = tcx.collect_and_partition_mono_items(()).codegen_units;
        if parts.len() <= 1 {
            return out;
        }
        let limit = size_limit(tcx);
        // item -> (linkage, inlined, partitions holding it)
        let mut home: FxHashMap<MonoItem<'tcx>, (Linkage, bool, u32)> = FxHashMap::default();
        for p in parts {
            for (&item, data) in p.items() {
                let e = home.entry(item).or_insert((data.linkage, data.inlined, 0));
                e.2 += 1;
            }
        }
        for p in parts {
            for &item in p.items().keys() {
                let MonoItem::Fn(c) = item else { continue };
                if !copyable(tcx, c, limit) {
                    continue;
                }
                for u in used(tcx, c).unwrap_or(&[]) {
                    let MonoItem::Fn(d) = u.node else { continue };
                    if matches!(home.get(&u.node), Some((Linkage::Internal, false, 1)))
                        && needs_body(tcx, &u.node)
                        && (if cold_only {
                            tcx.codegen_instance_attrs(d.def).flags.contains(F::COLD)
                        } else {
                            !copyable(tcx, d, limit)
                        })
                    {
                        out.insert(tcx.symbol_name(d).name.to_string());
                    }
                }
            }
        }
        out
    })
}

/// `PLIRON_XCGU_HOME`: symbols of fns another CGU of this crate can link
/// to: held by exactly one partition, not an `inlined` per-CGU copy, and
/// not `Internal` (or promoted to hidden-external). A copy of one that
/// inlining left called can go back to calling the home definition.
pub fn home_linkable<'tcx>(tcx: TyCtxt<'tcx>) -> &'static FxHashSet<String> {
    static H: std::sync::OnceLock<FxHashSet<String>> = std::sync::OnceLock::new();
    H.get_or_init(|| {
        let parts = tcx.collect_and_partition_mono_items(()).codegen_units;
        let mut home: FxHashMap<MonoItem<'tcx>, (Linkage, bool, u32)> = FxHashMap::default();
        for p in parts {
            for (&item, data) in p.items() {
                let e = home.entry(item).or_insert((data.linkage, data.inlined, 0));
                e.2 += 1;
            }
        }
        let promoted = promoted(tcx);
        home.into_iter()
            .filter_map(|(item, (linkage, inlined, n))| {
                let MonoItem::Fn(i) = item else { return None };
                let sym = tcx.symbol_name(i).name;
                (n == 1 && !inlined && (linkage != Linkage::Internal || promoted.contains(sym)))
                    .then(|| sym.to_string())
            })
            .collect()
    })
}

/// A copy is *degraded* when it calls a fn of this crate whose body lives
/// in another CGU and is not copied here: the home definition reaches that
/// callee as a local, the copy only through a call. `PLIRON_XCGU_HOME=1`
/// sends only degraded copies home; copies whose callees are all here can
/// beat the home definition (their local linkage feeds spec, sret2reg,
/// deadarg and the internal call ABI: pulldown).
pub fn degraded<'tcx>(
    tcx: TyCtxt<'tcx>,
    inst: Instance<'tcx>,
    cgu: &CodegenUnit<'tcx>,
    copies: &FxHashSet<Instance<'tcx>>,
) -> bool {
    let parts = tcx.collect_and_partition_mono_items(()).codegen_units;
    used(tcx, inst).unwrap_or(&[]).iter().any(|u| {
        let MonoItem::Fn(c) = u.node else { return false };
        c != inst
            && needs_body(tcx, &u.node)
            && !cgu.items().contains_key(&u.node)
            && !copies.contains(&c)
            && parts.iter().any(|p| p.items().contains_key(&u.node))
    })
}

fn is_promoted<'tcx>(tcx: TyCtxt<'tcx>, m: &MonoItem<'tcx>) -> bool {
    matches!(m, MonoItem::Fn(i) if dep_promoted(tcx).contains(tcx.symbol_name(*i).name))
}

/// Instances from other CGUs to define here as internal copies.
// r[impl par.xcgu.copy]
pub fn copies<'tcx>(tcx: TyCtxt<'tcx>, cgu: &CodegenUnit<'tcx>) -> Vec<Instance<'tcx>> {
    let mut out = if crate::pass_enabled("PLIRON_XCGU_FIXPOINT") {
        copies_fixpoint(tcx, cgu)
    } else {
        copies_legacy(tcx, cgu)
    };
    // Debug: `PLIRON_XCGU_ONLY=@file` keeps only copies whose symbol is
    // listed (delta-debug a copy-caused miscompile with fnbisect's `--names`;
    // dropping a copy another one needs can fail to link);
    // `PLIRON_XCGU_LOG=file` appends every copied symbol to `file.<pid>`.
    if let Some(only) = crate::env_get("PLIRON_XCGU_ONLY")
        && let Some(path) = only.strip_prefix('@')
    {
        let keep: FxHashSet<String> = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        out.retain(|&i| keep.contains(tcx.symbol_name(i).name));
    }
    if let Some(log) = crate::env_get("PLIRON_XCGU_LOG") {
        // One file per process and one write per CGU: concurrent rustcs
        // appending line by line interleaved names (`cat <log>.*`).
        use std::io::Write;
        let path = format!("{log}.{}", std::process::id());
        let text: String = out
            .iter()
            .map(|&i| format!("{}\n", tcx.symbol_name(i).name))
            .collect();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(text.as_bytes());
        }
    }
    out
}

fn copies_legacy<'tcx>(tcx: TyCtxt<'tcx>, cgu: &CodegenUnit<'tcx>) -> Vec<Instance<'tcx>> {
    let limit = size_limit(tcx);
    let parts = tcx.collect_and_partition_mono_items(()).codegen_units;
    // Single-CGU builds already hold every mono item: no foreign body can
    // be a copy candidate, so skip the used-item scans entirely.
    if parts.len() <= 1 {
        return Vec::new();
    }
    let ours = cgu.items();
    let why = crate::env_get("PLIRON_STATS_XCGU");
    // Copies exist to feed the cross-CGU inliner; more copies than the
    // CGU's own fns would double its IR-opt work for marginal picks.
    // `PLIRON_XCGU_CAP` overrides (absolute count; >=usize::MAX disables).
    let cap = crate::env_get("PLIRON_XCGU_CAP")
        .and_then(|s| s.parse().ok())
        .unwrap_or(ours.len());
    let mut direct: FxHashSet<MonoItem<'tcx>> = FxHashSet::default();
    let mut cands = Vec::new();
    for item in ours.keys() {
        let MonoItem::Fn(inst) = *item else { continue };
        let us = used(tcx, inst);
        if why.is_some() && us.is_none() && pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats",
                "xcgu: used() failed for {}", with_no_trimmed_paths!(inst.to_string()));
        }
        for u in us.unwrap_or(&[]) {
            if let Some(w) = &why
                && let MonoItem::Fn(c) = u.node
                && !ours.contains_key(&u.node)
                && with_no_trimmed_paths!(c.to_string()).contains(*w)
            {
                tracing::info!(target: "pliron::stats",
                    "xcgu cand {} in {}: copyable={} size={}",
                    with_no_trimmed_paths!(c.to_string()),
                    cgu.name(),
                    copyable(tcx, c, limit),
                    tcx.size_estimate(c));
            }
            if direct.insert(u.node)
                && let MonoItem::Fn(c) = u.node
                && !ours.contains_key(&u.node)
                && copyable(tcx, c, limit)
            {
                cands.push(c);
            }
        }
    }
    // Callees of accepted copies are candidates too (up to `PLIRON_XCGU_DEPTH`
    // levels), so a small wrapper's own small callee in a third CGU inlines.
    let depth = crate::env_get("PLIRON_XCGU_DEPTH")
        .and_then(|s| s.parse().ok())
        .unwrap_or(3usize);
    // `seen` seeds with the frontier only: items referenced by our own
    // items are NOT guaranteed-emitted elsewhere (an Internal+inlined
    // mono item can be dead-stripped in its home CGU), so they may still
    // need to become local copies themselves.
    let mut seen: FxHashSet<MonoItem<'tcx>> = cands.iter().map(|&c| MonoItem::Fn(c)).collect();
    let mut accepted: FxHashSet<MonoItem<'tcx>> = FxHashSet::default();
    let mut out = Vec::new();
    for _ in 0..depth {
        if cands.is_empty() || out.len() >= cap {
            break;
        }
        let mut refs: FxHashMap<Instance<'tcx>, &[Spanned<MonoItem<'tcx>>]> = FxHashMap::default();
        let mut need: FxHashSet<MonoItem<'tcx>> = FxHashSet::default();
        for &c in &cands {
            let Some(us) = used(tcx, c) else { continue };
            refs.insert(c, us);
            need.extend(us.iter().map(|u| u.node).filter(|m| !ours.contains_key(m)));
        }
        let mut visible: FxHashSet<MonoItem<'tcx>> = FxHashSet::default();
        if !need.is_empty() {
            for other in parts {
                for m in &need {
                    if let Some(d) = other.items().get(m)
                        && matches!(d.linkage, Linkage::External)
                    {
                        visible.insert(*m);
                    }
                }
            }
        }
        let why = crate::env_get("PLIRON_STATS_XCGU");
        cands.retain(|c| {
            let ok = refs.get(c).is_some_and(|us| {
                us.iter().all(|u| {
                    // A dep on the candidate itself is satisfied by the
                    // copy's own recursive call.
                    u.node == MonoItem::Fn(*c)
                        || ours.contains_key(&u.node)
                        || visible.contains(&u.node)
                        || accepted.contains(&u.node)
                        || !needs_body(tcx, &u.node)
                })
            });
            if !ok
                && let Some(w) = &why
                && with_no_trimmed_paths!(c.to_string()).contains(*w)
            {
                let bad: Vec<String> = refs.get(c).map_or(vec!["<no used items>".into()], |us| {
                    us.iter()
                        .filter(|u| {
                            !(ours.contains_key(&u.node)
                                || visible.contains(&u.node)
                                || accepted.contains(&u.node))
                        })
                        .map(|u| {
                            let home: Vec<String> = parts
                                .iter()
                                .filter_map(|p| {
                                    p.items()
                                        .get(&u.node)
                                        .map(|d| format!("{:?}/inlined={}", d.linkage, d.inlined))
                                })
                                .collect();
                            format!(
                                "{} @ {:?}",
                                with_no_trimmed_paths!(u.node.to_string()),
                                home
                            )
                        })
                        .collect()
                });
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::stats", "xcgu reject {} in {}: {:?}",
                        with_no_trimmed_paths!(c.to_string()),
                        cgu.name(),
                        bad
                    )
                };
            }
            ok
        });
        let mut next = Vec::new();
        // The depth loop's BFS order keeps direct callees first, so a
        // truncate drops only the deepest, least-reachable copies.
        cands.truncate(cap.saturating_sub(out.len()));
        for &c in &cands {
            accepted.insert(MonoItem::Fn(c));
            for u in refs[&c] {
                if seen.insert(u.node)
                    && let MonoItem::Fn(c2) = u.node
                    && !ours.contains_key(&u.node)
                    && copyable(tcx, c2, limit)
                {
                    next.push(c2);
                }
            }
        }
        out.append(&mut cands);
        cands = next;
    }
    out
}

// r[impl par.xcgu.closed]
fn copies_fixpoint<'tcx>(tcx: TyCtxt<'tcx>, cgu: &CodegenUnit<'tcx>) -> Vec<Instance<'tcx>> {
    let limit = size_limit(tcx);
    // Frontier rounds of mono-item use-chains to descend. The emitted set
    // is bounded by `cap` regardless, so a generous bound here just lets
    // deep `#[inline]` helper chains (std's SipHash path is five mono-item
    // levels under `hash_one`) become candidates.
    let depth = crate::env_get("PLIRON_XCGU_DEPTH")
        .and_then(|s| s.parse().ok())
        .unwrap_or(8usize);
    let ours = cgu.items();
    let parts = tcx.collect_and_partition_mono_items(()).codegen_units;
    if parts.len() <= 1 {
        return Vec::new();
    }
    // Same bound as the legacy path: copies never exceed the CGU's own
    // item count (`PLIRON_XCGU_CAP` overrides; 0 would take none).
    let cap = crate::env_get("PLIRON_XCGU_CAP")
        .and_then(|s| s.parse().ok())
        .unwrap_or(ours.len());
    let why = crate::env_get("PLIRON_STATS_XCGU");
    // Size-rejected candidates are otherwise invisible: they never reach
    // `remaining`, so neither `xcgu reject` (fixpoint) nor `xcgu nocopy`
    // (emit) ever names them.
    let size_report = |c: Instance<'tcx>| {
        if let Some(w) = &why
            && !copyable(tcx, c, limit)
            && with_no_trimmed_paths!(c.to_string()).contains(*w)
            && pliron_cfg::tracing_on()
        {
            tracing::info!(target: "pliron::stats", "xcgu toobig {}: {}",
                with_no_trimmed_paths!(c.to_string()),
                tcx.size_estimate(c)
            );
        }
    };
    let inlined: FxHashSet<MonoItem<'tcx>> = parts
        .iter()
        .flat_map(|other| {
            other
                .items()
                .iter()
                .filter_map(|(&item, data)| data.inlined.then_some(item))
        })
        .collect();
    // `PLIRON_XCGU_INLINED` (default on): `inlined=true` deps ignore the
    // size cap. rustc's partitioning already duplicates an inlined body
    // into every CGU that wants it — the cap is what stops *this* CGU's
    // copy closure from covering them (std's SipHash `d_rounds` is 127 vs
    // the native 50: `hash_one` never covered, so hashmap's probe loop
    // kept `bl hash_one` twice a lookup).
    let inl_size = crate::env_is("PLIRON_XCGU_INLINED", "1");
    let mut direct: FxHashSet<MonoItem<'tcx>> = FxHashSet::default();
    let mut frontier = Vec::new();
    for item in ours.keys() {
        let MonoItem::Fn(inst) = *item else {
            continue;
        };
        let us = used(tcx, inst);
        if why.is_some() && us.is_none() && pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::stats",
                "xcgu: used() failed for {}", with_no_trimmed_paths!(inst.to_string()));
        }
        for u in us.unwrap_or(&[]) {
            if let Some(w) = &why
                && let MonoItem::Fn(c) = u.node
                && !ours.contains_key(&u.node)
                && with_no_trimmed_paths!(c.to_string()).contains(*w)
            {
                tracing::info!(target: "pliron::stats",
                    "xcgu cand {} in {}: copyable={} size={}",
                    with_no_trimmed_paths!(c.to_string()),
                    cgu.name(),
                    copyable(tcx, c, limit),
                    tcx.size_estimate(c));
            }
            if direct.insert(u.node)
                && let MonoItem::Fn(c) = u.node
                && !ours.contains_key(&u.node)
            {
                size_report(c);
                if copyable(tcx, c, limit)
                    || (inl_size
                        && inlined.contains(&u.node)
                        && copyable(tcx, c, usize::MAX))
                {
                    frontier.push(c);
                }
            }
        }
    }

    // Items bindable without a copy: some partition emits the symbol
    // externally. An Internal dep (e.g. `RawVecInner::deallocate` under a
    // `RawVec::drop` copy) can only be satisfied by a copy of its own, so
    // the frontier must descend into it even though it isn't inlined in
    // its home CGU — otherwise the dep is unreachable and the parent's
    // copy dies in the remaining-prune below.
    let externs: FxHashSet<MonoItem<'tcx>> = parts
        .iter()
        .flat_map(|other| {
            other
                .items()
                .iter()
                .filter(|(item, data)| matches!(data.linkage, Linkage::External) || is_promoted(tcx, item))
                .map(|(&item, _)| item)
        })
        .collect();
    // `seen` seeds with the frontier only: items referenced by our own
    // items are NOT guaranteed-emitted elsewhere (an Internal+inlined
    // mono item can be dead-stripped in its home CGU), so they may still
    // need to become local copies themselves.
    let mut seen: FxHashSet<MonoItem<'tcx>> = frontier.iter().map(|&c| MonoItem::Fn(c)).collect();
    // `PLIRON_XCGU_XDEPS=1`: also descend into external deps of copies.
    // A copy emitted in a CGU whose caller binds a local copy instead of
    // the home definition keeps its `visible` deps as out-of-line calls,
    // which leaves them opaque to the inliner and IPO analyses in this
    // CGU; the home CGU's own copy inlines them. Making the external deps
    // copies too closes that gap at the cost of more emit.
    let xdeps = crate::env_is("PLIRON_XCGU_XDEPS", "1");
    let mut candidates = Vec::new();
    let mut refs: FxHashMap<Instance<'tcx>, &[Spanned<MonoItem<'tcx>>]> = FxHashMap::default();
    let mut needed: FxHashSet<MonoItem<'tcx>> = FxHashSet::default();
    for _ in 0..=depth {
        if frontier.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for c in frontier {
            let Some(us) = used(tcx, c) else {
                continue;
            };
            refs.insert(c, us);
            candidates.push(c);
            for u in us {
                if !ours.contains_key(&u.node) {
                    needed.insert(u.node);
                }
                // A non-external dep can't bind to another CGU's symbol at
                // all — it needs a local copy. External deps resolve fine
                // as calls, so a private copy is only worthwhile when
                // their home CGU inlined them (the ThinLTO-import shape).
                if let MonoItem::Fn(c2) = u.node
                    && !ours.contains_key(&u.node)
                    && seen.insert(u.node)
                    && (inlined.contains(&u.node) || !externs.contains(&u.node) || xdeps)
                {
                    size_report(c2);
                    if copyable(tcx, c2, limit)
                        || (inl_size
                            && inlined.contains(&u.node)
                            && copyable(tcx, c2, usize::MAX))
                    {
                        next.push(c2);
                    }
                }
            }
        }
        frontier = next;
    }

    let mut visible: FxHashSet<MonoItem<'tcx>> = FxHashSet::default();
    if !needed.is_empty() {
        for other in parts {
            for m in &needed {
                if let Some(d) = other.items().get(m)
                    && (matches!(d.linkage, Linkage::External) || is_promoted(tcx, m))
                {
                    visible.insert(*m);
                }
            }
        }
    }

    let mut remaining: FxHashSet<MonoItem<'tcx>> =
        candidates.iter().map(|&c| MonoItem::Fn(c)).collect();
    loop {
        let removed: Vec<(Instance<'tcx>, Vec<MonoItem<'tcx>>)> = candidates
            .iter()
            .copied()
            .filter(|c| remaining.contains(&MonoItem::Fn(*c)))
            .filter_map(|c| {
                let bad: Vec<_> = refs[&c]
                    .iter()
                    .map(|u| u.node)
                    .filter(|m| {
                        needs_body(tcx, m)
                            && !ours.contains_key(m)
                            && !visible.contains(m)
                            && !remaining.contains(m)
                    })
                    .collect();
                (!bad.is_empty()).then_some((c, bad))
            })
            .collect();
        if removed.is_empty() {
            break;
        }
        for (c, bad) in removed {
            remaining.remove(&MonoItem::Fn(c));
            if let Some(w) = &why
                && with_no_trimmed_paths!(c.to_string()).contains(*w)
            {
                let bad: Vec<String> = bad
                    .iter()
                    .map(|u| {
                        let home: Vec<String> = parts
                            .iter()
                            .filter_map(|p| {
                                p.items()
                                    .get(u)
                                    .map(|d| format!("{:?}/inlined={}", d.linkage, d.inlined))
                            })
                            .collect();
                        let kind = match u {
                            MonoItem::Fn(inst) => format!("{:?}", inst.def),
                            _ => "?".into(),
                        };
                        format!(
                            "{} [{kind}] @ {:?}",
                            with_no_trimmed_paths!(u.to_string()),
                            home
                        )
                    })
                    .collect();
                if pliron_cfg::tracing_on() {
                    tracing::info!(target: "pliron::stats", "xcgu reject {} in {}: {:?}",
                        with_no_trimmed_paths!(c.to_string()),
                        cgu.name(),
                        bad
                    )
                };
            }
        }
    }

    // The emit set must stay ref-closed: a caller emitted without its
    // copy deps leaves dangling refs (stage2: 257 undefined symbols —
    // `take(cap)` kept BFS-front callers while cutting their callee
    // copies). Spend the cap on whole closures instead: admit a
    // candidate only together with its uncovered dep closure, so the
    // closest callees win the budget and anything cut takes its
    // referrers down with it.
    let mut emit: FxHashSet<MonoItem<'tcx>> = FxHashSet::default();
    // `inlined=true` items admitted by the `PLIRON_XCGU_INLINED` size
    // exemption don't spend the cap: the partitioner already duplicates
    // them into every CGU that asks, so they aren't the unplanned extra
    // emit the cap bounds. Charging them let one big exempted closure
    // (std `visit_seq`, size 110) evict small copies like postcard's
    // `take_from_bytes` (`cap: 224 + 1 > 224`).
    //
    // `PLIRON_XCGU_TINY` (default 8 MIR stmts, `=0` disables): closures
    // made entirely of <=N-stmt bodies don't spend the cap either. A
    // 3-stmt predicate like `IsNotEmpty::call_mut` costs ~nothing to
    // copy and optimize, but the cap's arbitrary admission order
    // evicted it (`cap: 190 + 2 > 190`), leaving a real call in
    // tokenize's per-token path.
    let tiny_max = crate::env_get("PLIRON_XCGU_TINY")
        .and_then(|s| s.parse().ok())
        .unwrap_or(8usize);
    let mut charged = 0usize;
    for &c in &candidates {
        let m = MonoItem::Fn(c);
        if !remaining.contains(&m) || emit.contains(&m) {
            continue;
        }
        let mut closure: FxHashSet<MonoItem<'tcx>> = FxHashSet::default();
        let mut stack = vec![m];
        while let Some(n) = stack.pop() {
            // `visible` only resolves *deps* (the copy calls the external
            // symbol) — it must not skip `m` itself: a candidate that is
            // External in its home CGU is exactly what a local copy is for
            // (std `hash_one` is External in cgu.0 and needed a body here
            // to inline into hashbrown's probe loop).
            if ours.contains_key(&n)
                || (n != m && (visible.contains(&n) || emit.contains(&n)))
                || !needs_body(tcx, &n)
            {
                continue;
            }
            if !remaining.contains(&n) || !closure.insert(n) {
                continue;
            }
            if let MonoItem::Fn(inst) = n {
                stack.extend(refs[&inst].iter().map(|u| u.node));
            }
        }
        let coverable = closure.iter().all(|n| match n {
            MonoItem::Fn(inst) => refs[inst].iter().all(|u| {
                ours.contains_key(&u.node)
                    || visible.contains(&u.node)
                    || emit.contains(&u.node)
                    || closure.contains(&u.node)
                    || !needs_body(tcx, &u.node)
            }),
            _ => false,
        });
        let cost = closure
            .iter()
            .filter(|n| match n {
                MonoItem::Fn(inst) => {
                    !(inl_size && inlined.contains(n) && !copyable(tcx, *inst, limit))
                }
                _ => true,
            })
            .count();
        let tiny = tiny_max > 0
            && closure.iter().all(|n| match n {
                MonoItem::Fn(inst) => tcx.size_estimate(*inst) <= tiny_max,
                _ => false,
            });
        if coverable && (tiny || charged + cost <= cap) {
            if let Some(w) = &why
                && with_no_trimmed_paths!(c.to_string()).contains(*w)
                && pliron_cfg::tracing_on()
            {
                tracing::info!(target: "pliron::stats", "xcgu emit {} [{:?}] in {} (+{} deps; closure={:?})",
                    with_no_trimmed_paths!(c.to_string()),
                    c.def,
                    cgu.name(),
                    closure.len(),
                    closure,
                );
            }
            charged += cost;
            emit.extend(closure);
        } else if let Some(w) = &why
            && with_no_trimmed_paths!(c.to_string()).contains(*w)
        {
            let hole: Vec<String> = if coverable {
                vec![format!("cap: {} + {} > {}", charged, cost, cap)]
            } else {
                closure
                    .iter()
                    .filter(|n| match n {
                        MonoItem::Fn(inst) => !refs[inst].iter().all(|u| {
                            ours.contains_key(&u.node)
                                || visible.contains(&u.node)
                                || emit.contains(&u.node)
                                || closure.contains(&u.node)
                                || !needs_body(tcx, &u.node)
                        }),
                        _ => true,
                    })
                    .map(|n| with_no_trimmed_paths!(n.to_string()))
                    .collect()
            };
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::stats", "xcgu nocopy {} in {}: {:?}",
                    with_no_trimmed_paths!(c.to_string()),
                    cgu.name(),
                    hole
                )
            };
        }
    }
    candidates
        .into_iter()
        .filter(|c| emit.contains(&MonoItem::Fn(*c)))
        .collect()
}
