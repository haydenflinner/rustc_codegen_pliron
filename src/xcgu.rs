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
use rustc_middle::ty::{Instance, InstanceKind, TyCtxt};
use rustc_span::Spanned;

fn copyable<'tcx>(tcx: TyCtxt<'tcx>, inst: Instance<'tcx>, limit: usize) -> bool {
    if !matches!(inst.def, InstanceKind::Item(_)) {
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
    if cands.is_empty() {
        return cands;
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
        for other in tcx.collect_and_partition_mono_items(()).codegen_units {
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
    cands.retain(|c| {
        refs.get(c).is_some_and(|us| {
            us.iter().all(|u| {
                ours.contains_key(&u.node) || direct.contains(&u.node) || visible.contains(&u.node)
            })
        })
    });
    cands
}
