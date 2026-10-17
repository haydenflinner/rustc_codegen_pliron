//! Cross-CGU local copies (`PLIRON_XCGU`). LLVM gets cross-CGU inlining from
//! thin-local LTO; here a CGU instead gets an internal copy of each small
//! callee that lives in another CGU of this crate, so the inliner can see its
//! body. A copy is made only when everything the body references resolves
//! from this CGU: defined here, used by this CGU's own items (so already
//! visible), or externally linked in its home CGU.

use rustc_attr_ir::{InlineAttr, Linkage};
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags as F;
use rustc_middle::mono::{CodegenUnit, CollectionMode, MonoItem};
use rustc_middle::ty::print::with_no_trimmed_paths;
use rustc_middle::ty::{Instance, InstanceKind, TyCtxt};
use rustc_span::Spanned;

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
    if a.flags.intersects(bad)
        || matches!(a.inline, InlineAttr::Never)
        || a.symbol_name.is_some()
        || a.linkage.is_some()
        || a.link_section.is_some()
    {
        return false;
    }
    tcx.size_estimate(inst) <= limit
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
        InstanceKind::Item(def_id) => !matches!(
            tcx.opt_item_name(def_id),
            Some(
                rustc_span::sym::rotate_left
                    | rustc_span::sym::rotate_right
                    | rustc_span::sym::unchecked_funnel_shl
                    | rustc_span::sym::unchecked_funnel_shr
            )
        ),
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
        .unwrap_or(if tcx.sess.target.arch == rustc_target::spec::Arch::Wasm32 {
            512
        } else {
            50
        })
}

/// Instances from other CGUs to define here as internal copies.
pub fn copies<'tcx>(tcx: TyCtxt<'tcx>, cgu: &CodegenUnit<'tcx>) -> Vec<Instance<'tcx>> {
    if crate::pass_enabled("PLIRON_XCGU_FIXPOINT") {
        copies_fixpoint(tcx, cgu)
    } else {
        copies_legacy(tcx, cgu)
    }
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
        for u in used(tcx, inst).unwrap_or(&[]) {
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
    let mut seen: FxHashSet<MonoItem<'tcx>> =
        cands.iter().map(|&c| MonoItem::Fn(c)).collect();
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
            need.extend(
                us.iter()
                    .map(|u| u.node)
                    .filter(|m| !ours.contains_key(m)),
            );
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
                    ours.contains_key(&u.node)
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
    let mut direct: FxHashSet<MonoItem<'tcx>> = FxHashSet::default();
    let mut frontier = Vec::new();
    for item in ours.keys() {
        let MonoItem::Fn(inst) = *item else {
            continue;
        };
        for u in used(tcx, inst).unwrap_or(&[]) {
            if direct.insert(u.node)
                && let MonoItem::Fn(c) = u.node
                && !ours.contains_key(&u.node)
            {
                size_report(c);
                if copyable(tcx, c, limit) {
                    frontier.push(c);
                }
            }
        }
    }

    let inlined: FxHashSet<MonoItem<'tcx>> = parts
        .iter()
        .flat_map(|other| {
            other
                .items()
                .iter()
                .filter_map(|(&item, data)| data.inlined.then_some(item))
        })
        .collect();
    // `seen` seeds with the frontier only: items referenced by our own
    // items are NOT guaranteed-emitted elsewhere (an Internal+inlined
    // mono item can be dead-stripped in its home CGU), so they may still
    // need to become local copies themselves.
    let mut seen: FxHashSet<MonoItem<'tcx>> = frontier
        .iter()
        .map(|&c| MonoItem::Fn(c))
        .collect();
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
                if let MonoItem::Fn(c2) = u.node
                    && !ours.contains_key(&u.node)
                    && seen.insert(u.node)
                    && inlined.contains(&u.node)
                {
                    size_report(c2);
                    if copyable(tcx, c2, limit) {
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
                    && matches!(d.linkage, Linkage::External)
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
                        format!("{} [{kind}] @ {:?}", with_no_trimmed_paths!(u.to_string()), home)
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
        if coverable && emit.len() + closure.len() <= cap {
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
            emit.extend(closure);
        } else if let Some(w) = &why
            && with_no_trimmed_paths!(c.to_string()).contains(*w)
        {
            let hole: Vec<String> = if coverable {
                vec![format!("cap: {} + {} > {}", emit.len(), closure.len(), cap)]
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
