//! Cross-CGU local copies (`PLIRON_XCGU`). LLVM gets cross-CGU inlining from
//! thin-local LTO; here a CGU instead gets an internal copy of each small
//! callee that lives in another CGU of this crate, so the inliner can see its
//! body. A copy is made only when everything the body references resolves
//! from this CGU: defined here, used by this CGU's own items (so already
//! visible), or externally linked in its home CGU.

use rustc_attr_ir::{InlineAttr, Linkage};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
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

/// Instances from other CGUs to define here as internal copies.
pub fn copies<'tcx>(tcx: TyCtxt<'tcx>, cgu: &CodegenUnit<'tcx>) -> Vec<Instance<'tcx>> {
    if crate::pass_enabled("PLIRON_XCGU_FIXPOINT") {
        copies_fixpoint(tcx, cgu)
    } else {
        copies_legacy(tcx, cgu)
    }
}

fn copies_legacy<'tcx>(tcx: TyCtxt<'tcx>, cgu: &CodegenUnit<'tcx>) -> Vec<Instance<'tcx>> {
    let limit = std::env::var("PLIRON_XCGU_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let ours = cgu.items();
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
    let depth = std::env::var("PLIRON_XCGU_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3usize);
    let parts = tcx.collect_and_partition_mono_items(()).codegen_units;
    let mut seen: FxHashSet<MonoItem<'tcx>> = direct.clone();
    let mut accepted: FxHashSet<MonoItem<'tcx>> = FxHashSet::default();
    let mut out = Vec::new();
    for _ in 0..depth {
        if cands.is_empty() {
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
                    .filter(|m| !ours.contains_key(m) && !direct.contains(m)),
            );
        }
        let mut visible: FxHashSet<MonoItem<'tcx>> = FxHashSet::default();
        if !need.is_empty() {
            for other in parts {
                for m in &need {
                    if let Some(d) = other.items().get(m)
                        && !d.inlined
                        && matches!(d.linkage, Linkage::External)
                    {
                        visible.insert(*m);
                    }
                }
            }
        }
        let why = std::env::var("PLIRON_STATS_XCGU").ok();
        cands.retain(|c| {
            let ok = refs.get(c).is_some_and(|us| {
                us.iter().all(|u| {
                    ours.contains_key(&u.node)
                        || direct.contains(&u.node)
                        || visible.contains(&u.node)
                        || accepted.contains(&u.node)
                })
            });
            if !ok
                && let Some(w) = &why
                && with_no_trimmed_paths!(c.to_string()).contains(w.as_str())
            {
                let bad: Vec<String> = refs.get(c).map_or(vec!["<no used items>".into()], |us| {
                    us.iter()
                        .filter(|u| {
                            !(ours.contains_key(&u.node)
                                || direct.contains(&u.node)
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
                eprintln!(
                    "xcgu reject {} in {}: {:?}",
                    with_no_trimmed_paths!(c.to_string()),
                    cgu.name(),
                    bad
                );
            }
            ok
        });
        let mut next = Vec::new();
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
    let limit = std::env::var("PLIRON_XCGU_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let depth = std::env::var("PLIRON_XCGU_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3usize);
    let ours = cgu.items();
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
                && copyable(tcx, c, limit)
            {
                frontier.push(c);
            }
        }
    }

    let parts = tcx.collect_and_partition_mono_items(()).codegen_units;
    let inlined: FxHashSet<MonoItem<'tcx>> = parts
        .iter()
        .flat_map(|other| {
            other
                .items()
                .iter()
                .filter_map(|(&item, data)| data.inlined.then_some(item))
        })
        .collect();
    let mut seen = direct.clone();
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
                if !ours.contains_key(&u.node) && !direct.contains(&u.node) {
                    needed.insert(u.node);
                }
                if let MonoItem::Fn(c2) = u.node
                    && !ours.contains_key(&u.node)
                    && seen.insert(u.node)
                    && inlined.contains(&u.node)
                    && copyable(tcx, c2, limit)
                {
                    next.push(c2);
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
                    && !d.inlined
                    && matches!(d.linkage, Linkage::External)
                {
                    visible.insert(*m);
                }
            }
        }
    }

    let mut remaining: FxHashSet<MonoItem<'tcx>> =
        candidates.iter().map(|&c| MonoItem::Fn(c)).collect();
    let why = std::env::var("PLIRON_STATS_XCGU").ok();
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
                        !ours.contains_key(m)
                            && !direct.contains(m)
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
                && with_no_trimmed_paths!(c.to_string()).contains(w.as_str())
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
                        format!("{} @ {:?}", with_no_trimmed_paths!(u.to_string()), home)
                    })
                    .collect();
                eprintln!(
                    "xcgu reject {} in {}: {:?}",
                    with_no_trimmed_paths!(c.to_string()),
                    cgu.name(),
                    bad
                );
            }
        }
    }

    candidates
        .into_iter()
        .filter(|c| remaining.contains(&MonoItem::Fn(*c)))
        .collect()
}
