//! Profile-guided struct field order (both halves opt-in, default off).
//!
//! `PLIRON_FIELD_HEAT=<file>`: count field accesses per `(struct, field)`
//! while the crate is codegen'd and append
//! `"<crate>::<path>\t<field>\t<weight>"` lines to the file.
//! `PLIRON_FIELD_ORDER=<file>`: read such a file and put hotter fields
//! first in plain `repr(Rust)` structs, by overriding `layout_of`.
//!
//! Recording. The backend's builder only sees byte offsets
//! (`PlaceRef::project_field` is an inherent method of `rustc_codegen_ssa`
//! and emits nothing at all for offset 0), so heat comes from a walk over
//! each codegen'd instance's MIR: every `ProjectionElem::Field` on a
//! struct counts once, weighted by how often its MIR block runs. With a
//! block profile (`PLIRON_PGO_USE`) the weight is the measured count:
//! `append_block` gets the MIR block's name (`start`, `bbN`) from
//! `rustc_codegen_ssa`, so `note_block` keeps MIR block -> backend block,
//! and `resolve` reads the counts once `pgo::load` has attached them
//! (`finish_pre`). A MIR block merged into its only predecessor has no
//! backend block of its own and takes the predecessor's count. Without a
//! profile the weight is the static estimate `8^loop depth`.
//!
//! Applying. Only fields of identical size and alignment trade places, so
//! size, alignment and padding of the struct are unchanged by
//! construction. Which structs qualify, and why the answer is the same in
//! every crate of a build: see `permute` and `crate_ok`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use pliron::basic_block::BasicBlock;
use pliron::context::Ptr;
use pliron::operation::Operation;
use rustc_abi::{BackendRepr, FieldIdx, FieldsShape, LayoutData, Niche, ReprOptions, Variants};
use rustc_hir::def_id::{CrateNum, DefId, LOCAL_CRATE};
use rustc_middle::mir::visit::{NonUseContext, PlaceContext, Visitor};
use rustc_middle::mir::{self, Location, ProjectionElem};
use rustc_middle::ty::layout::{LayoutCx, LayoutError, TyAndLayout};
use rustc_middle::ty::{self, Instance, Ty, TyCtxt};
use rustc_middle::util::Providers;

use crate::context::CodegenCx;

/// `<crate name>::<path>` of an ADT: the same string from whichever crate
/// asks (`def_path_str` drops the crate name for local items).
fn type_key(tcx: TyCtxt<'_>, did: DefId) -> String {
    format!("{}{}", tcx.crate_name(did.krate), tcx.def_path(did).to_string_no_crate_verbose())
}

/// Plain `repr(Rust)`: nothing that fixes or constrains the field order.
fn plain_repr(r: &ReprOptions) -> bool {
    !r.inhibit_struct_field_reordering()
        && !r.transparent()
        && !r.simd()
        && r.pack.is_none()
        && r.align.is_none()
        && r.int.is_none()
        && r.scalable.is_none()
}

/// Is every user of this crate's types compiled by this backend under the
/// same order file? True for the crate being compiled and for
/// dependencies outside the sysroot; false for the prebuilt standard
/// library and its private dependencies (identified by where their
/// rlib/rmeta lives, so a user's own `hashbrown` still qualifies while
/// std's copy does not). The answer depends only on the defining crate,
/// never on which crate is asking.
fn crate_ok(tcx: TyCtxt<'_>, cnum: CrateNum) -> bool {
    static CACHE: Mutex<Option<HashMap<CrateNum, bool>>> = Mutex::new(None);
    if let Some(&ok) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&cnum) {
        return ok;
    }
    let ok = crate_ok_uncached(tcx, cnum);
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(cnum, ok);
    ok
}

fn crate_ok_uncached(tcx: TyCtxt<'_>, cnum: CrateNum) -> bool {
    // By name too: these stay stock even when the sysroot itself is built
    // by this backend (`-Zbuild-std`, self-host), where a prebuilt copy
    // may be mixed in.
    if matches!(
        tcx.crate_name(cnum).as_str(),
        "core" | "alloc" | "std" | "proc_macro" | "test" | "compiler_builtins"
    ) {
        return false;
    }
    if cnum == LOCAL_CRATE {
        return true;
    }
    if tcx.is_compiler_builtins(cnum) || tcx.is_panic_runtime(cnum) || tcx.is_profiler_runtime(cnum)
    {
        return false;
    }
    let src = tcx.used_crate_source(cnum);
    let mut any = false;
    for p in src.paths() {
        any = true;
        let canon = std::fs::canonicalize(p).ok();
        for root in tcx.sess.opts.sysroot.all_paths() {
            let croot = std::fs::canonicalize(root).ok();
            if p.starts_with(root)
                || matches!((&canon, &croot), (Some(c), Some(r)) if c.starts_with(r))
            {
                return false;
            }
        }
    }
    any
}

// ---------------------------------------------------------------- apply

type Order = HashMap<String, HashMap<String, u64>>;

/// The order file, read once: type -> field -> summed weight. Lines that
/// don't parse are skipped; a missing file is an empty table.
fn order() -> Option<&'static Order> {
    static ORDER: OnceLock<Option<Order>> = OnceLock::new();
    ORDER
        .get_or_init(|| {
            let path = pliron_cfg::env_get("PLIRON_FIELD_ORDER").filter(|p| !p.is_empty())?;
            let mut m = Order::new();
            let text = std::fs::read(path).unwrap_or_default();
            for line in String::from_utf8_lossy(&text).lines() {
                let mut it = line.split('\t');
                let (Some(t), Some(f), Some(w), None) = (it.next(), it.next(), it.next(), it.next())
                else {
                    continue;
                };
                let (t, f, w) = (t.trim(), f.trim(), w.trim());
                if t.is_empty() || t.starts_with('#') || f.is_empty() {
                    continue;
                }
                let w = match w.parse::<u64>() {
                    Ok(w) => w,
                    Err(_) => match w.parse::<f64>() {
                        Ok(x) if x.is_finite() && x >= 0.0 => x as u64,
                        _ => continue,
                    },
                };
                let e = m.entry(t.to_string()).or_default().entry(f.to_string()).or_insert(0);
                *e = e.saturating_add(w);
            }
            Some(m)
        })
        .as_ref()
}

type LayoutOf = for<'tcx> fn(
    TyCtxt<'tcx>,
    ty::PseudoCanonicalInput<'tcx, Ty<'tcx>>,
) -> Result<TyAndLayout<'tcx>, &'tcx LayoutError<'tcx>>;

static PREV_LAYOUT_OF: OnceLock<LayoutOf> = OnceLock::new();

/// `CodegenBackend::provide`: wrap `layout_of`, only under
/// `PLIRON_FIELD_ORDER` — unset, no provider changes at all.
pub(crate) fn provide(providers: &mut Providers) {
    if order().is_none() {
        return;
    }
    let _ = PREV_LAYOUT_OF.set(providers.queries.layout_of);
    providers.queries.layout_of = layout_of;
}

fn layout_of<'tcx>(
    tcx: TyCtxt<'tcx>,
    query: ty::PseudoCanonicalInput<'tcx, Ty<'tcx>>,
) -> Result<TyAndLayout<'tcx>, &'tcx LayoutError<'tcx>> {
    let res = (PREV_LAYOUT_OF.get().expect("field_order: provider installed"))(tcx, query);
    match res {
        Ok(l) => Ok(permute(tcx, query.typing_env, l).unwrap_or(l)),
        Err(_) => res,
    }
}

/// The reordered layout, or `None` to keep rustc's.
///
/// Qualifies: a struct with plain `repr(Rust)`, defined in a crate
/// `crate_ok` accepts, listed in the order file, whose layout is a sized
/// in-memory aggregate (`BackendRepr::Memory { sized: true }`,
/// `FieldsShape::Arbitrary`, `Variants::Single`) and whose last field
/// cannot be unsized (rustc's own `maybe_unsized` test: an unsizing
/// coercion needs `S<[u8; N]>` and `S<[u8]>` to agree on every offset).
/// `Scalar`/`ScalarPair`/vector structs encode field positions in the
/// repr and are left alone.
///
/// The non-zero-sized fields are taken one ABI-alignment class at a time.
/// If the class's fields tile one contiguous byte span `[s, e)` in rustc's
/// layout (each starts where the previous one ends), the span is
/// re-packed hottest first, ties in rustc's order. Why nothing else can
/// change: every field of the class has alignment `a` and a size that is
/// a multiple of `a`, and `s` is a multiple of `a`, so packing them from
/// `s` in any order puts each one on a multiple of `a`, leaves no gap and
/// ends exactly at `e`. Bytes outside `[s, e)` — other classes, padding,
/// the struct's size and alignment — are not touched, so the type cannot
/// grow and its padding bytes stay where they were. A class that does not
/// tile a span (rustc put a niche field at an end, or interleaved
/// classes) falls back to swapping fields of equal size, which is
/// trivially neutral. Zero-sized fields keep their offsets. Everything
/// else in the layout is copied; the largest niche moves with its field.
fn permute<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: ty::TypingEnv<'tcx>,
    l: TyAndLayout<'tcx>,
) -> Option<TyAndLayout<'tcx>> {
    let ty::Adt(def, _) = l.ty.kind() else { return None };
    if !def.is_struct() || !plain_repr(&def.repr()) {
        return None;
    }
    let data: &LayoutData<FieldIdx, rustc_abi::VariantIdx> = *l;
    if !matches!(data.backend_repr, BackendRepr::Memory { sized: true })
        || !matches!(data.variants, Variants::Single { .. })
    {
        return None;
    }
    let FieldsShape::Arbitrary { offsets, in_memory_order } = &data.fields else { return None };
    let n = offsets.len();
    let variant = def.non_enum_variant();
    if n < 2 || variant.fields.len() != n || in_memory_order.len() != n {
        return None;
    }
    let did = def.did();
    if !crate_ok(tcx, did.krate) || tcx.as_lang_item(did).is_some() {
        return None;
    }
    let heat = order()?.get(&type_key(tcx, did))?;
    let w: Vec<u64> = variant
        .fields
        .iter()
        .map(|f| heat.get(f.name.as_str()).copied().unwrap_or(0))
        .collect();
    if w.iter().all(|&x| x == 0) {
        return None;
    }
    // Swept types keep rustc's order (`access_class`): packing the hot
    // fields of an element wider than one line makes a sequential pass
    // skip lines, and the hardware's stream prefetch stops following it
    // (tests/field_order/sweep.rs: 2-3x slower out of cache). An element
    // of at most `LINE` bytes cannot skip a line. `PLIRON_FIELD_ORDER_SEQ=1`
    // reorders them anyway.
    let seq = heat.get(SEQ_NAME).copied().unwrap_or(0);
    let total: u64 =
        heat.iter().filter(|(k, _)| !k.starts_with('@')).fold(0u64, |a, (_, &v)| a.saturating_add(v));
    if data.size.bytes() > LINE
        && seq.saturating_mul(2) > total
        && !pliron_cfg::env_is("PLIRON_FIELD_ORDER_SEQ", "1")
    {
        log_layout(tcx, did, l.ty, "skip-seq");
        return None;
    }
    // Same test as `rustc_ty_utils::layout` (`maybe_unsized`).
    let last = variant.tail_opt()?;
    let def_env = ty::TypingEnv::new(
        tcx.param_env_normalized_for_post_analysis(did),
        typing_env.typing_mode(),
    );
    if !tcx.type_of(last.did).instantiate_identity().skip_norm_wip().is_sized(tcx, def_env) {
        return None;
    }

    let cx = LayoutCx::new(tcx, typing_env);
    let fl: Vec<TyAndLayout<'tcx>> = (0..n).map(|i| l.field(&cx, i)).collect();
    let off = |i: usize| offsets[FieldIdx::from_usize(i)];

    // One alignment class at a time (non-zero-sized fields only).
    let mut idx: Vec<usize> = (0..n).filter(|&i| fl[i].size.bytes() != 0).collect();
    idx.sort_by_key(|&i| (fl[i].align.abi, off(i)));
    let mut new_off = offsets.clone();
    let mut k = 0;
    while k < idx.len() {
        let a = fl[idx[k]].align.abi;
        let mut e = k + 1;
        while e < idx.len() && fl[idx[e]].align.abi == a {
            e += 1;
        }
        // `slots`: the class by increasing offset; `by_heat`: the same
        // fields hottest first (stable, so ties keep rustc's order).
        let slots = &idx[k..e];
        k = e;
        let mut by_heat = slots.to_vec();
        by_heat.sort_by(|&x, &y| w[y].cmp(&w[x]));
        let (start, end) = (off(slots[0]), off(slots[slots.len() - 1]) + fl[slots[slots.len() - 1]].size);
        // Re-pack the whole span when the class tiles it exactly: every
        // field starts where the previous one ends, sizes and the start
        // are multiples of the class alignment, and no zero-sized field
        // sits strictly inside (it would end up in the middle of a field).
        let tiles = slots.windows(2).all(|p| off(p[0]) + fl[p[0]].size == off(p[1]))
            && start.is_aligned(a)
            && slots.iter().all(|&i| fl[i].size.is_aligned(a))
            && (0..n).all(|z| fl[z].size.bytes() != 0 || off(z) <= start || off(z) >= end);
        if tiles {
            let mut at = start;
            for &f in &by_heat {
                new_off[FieldIdx::from_usize(f)] = at;
                at += fl[f].size;
            }
            debug_assert_eq!(at, end);
        } else {
            // Otherwise only fields of equal size trade slots.
            let mut used = vec![false; by_heat.len()];
            for &slot in slots {
                let j = (0..by_heat.len())
                    .find(|&j| !used[j] && fl[by_heat[j]].size == fl[slot].size)?;
                used[j] = true;
                new_off[FieldIdx::from_usize(by_heat[j])] = off(slot);
            }
        }
    }
    let moved = (0..n).filter(|&i| new_off[FieldIdx::from_usize(i)] != off(i)).count();
    if moved == 0 {
        return None;
    }
    // Memory order: rustc's order re-sorted by the new offsets (stable, so
    // zero-sized fields keep their place next to whatever shares their
    // offset).
    let mut mem: Vec<FieldIdx> = in_memory_order.iter().copied().collect();
    mem.sort_by_key(|&f| new_off[f]);
    let mut new_mem = in_memory_order.clone();
    for (p, f) in mem.into_iter().enumerate() {
        new_mem[p as u32] = f;
    }

    // The niche is a byte range inside exactly one non-zero-sized field.
    // If that field moved, the niche moves by the same delta — but only
    // when the field's own layout confirms the niche is its own;
    // otherwise keep rustc's layout.
    let mut niche = data.largest_niche;
    if let Some(nc) = data.largest_niche {
        let end = nc.offset + nc.value.size(&tcx);
        let host =
            (0..n).find(|&i| fl[i].size.bytes() != 0 && off(i) <= nc.offset && end <= off(i) + fl[i].size)?;
        let old = off(host);
        let new = new_off[FieldIdx::from_usize(host)];
        if new != old {
            let inner = Niche { offset: nc.offset - old, ..nc };
            if fl[host].largest_niche != Some(inner) {
                return None;
            }
            niche = Some(Niche { offset: new + inner.offset, ..nc });
        }
    }

    log_layout(tcx, did, l.ty, &moved.to_string());

    let mut nd = data.clone();
    nd.fields = FieldsShape::Arbitrary { offsets: new_off, in_memory_order: new_mem };
    nd.largest_niche = niche;
    Some(TyAndLayout { ty: l.ty, layout: tcx.mk_layout(nd) })
}

/// `PLIRON_FIELD_ORDER_LOG=<file>`: one line per layout the order file
/// reached (compiling crate, struct, instantiation, fields moved or
/// `skip-seq`), to count what it did.
fn log_layout<'tcx>(tcx: TyCtxt<'tcx>, did: DefId, ty: Ty<'tcx>, what: &str) {
    let Some(log) = pliron_cfg::env_get("PLIRON_FIELD_ORDER_LOG") else { return };
    use std::io::Write;
    let line = rustc_middle::ty::print::with_no_trimmed_paths!(format!(
        "{}\t{}\t{}\t{what}\n",
        tcx.crate_name(LOCAL_CRATE),
        type_key(tcx, did),
        ty
    ));
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

/// Pseudo-fields in the heat file: the part of a struct's heat that is
/// sequential (`access_class`) and the part that is random.
const SEQ_NAME: &str = "@seq";
const RAND_NAME: &str = "@rand";
const SEQ: u32 = u32::MAX;
const RAND: u32 = u32::MAX - 1;
/// Bytes per cache line for the sweep rule. 64 on every target we care
/// about (Apple's 128-byte figure is the L2 line; the stride experiment
/// shows the loss starting when 64-byte lines are skipped).
const LINE: u64 = 64;

// --------------------------------------------------------------- record

/// How the address of an access is formed, per local, flow-insensitive:
/// `STEP`: the local is on a cycle through pointer/integer arithmetic (an
/// induction variable or a stepped pointer) or computed from one;
/// `LOAD`: it holds, or is computed from, a value read out of memory
/// (`order[k]`, `node.next`).
const STEP: u8 = 1;
const LOAD: u8 = 2;

fn local_flow(body: &mir::Body<'_>) -> Vec<u8> {
    use mir::{BinOp, Operand, Rvalue, StatementKind};
    let n = body.local_decls.len();
    let mut flags = vec![0u8; n];
    // dst depends on src; arithmetic (dst, operand) pairs.
    let mut deps: Vec<(mir::Local, mir::Local)> = Vec::new();
    let mut arith: Vec<(mir::Local, mir::Local)> = Vec::new();
    let reads_memory = |pl: &mir::Place<'_>| {
        pl.projection.iter().any(|e| matches!(e, ProjectionElem::Deref | ProjectionElem::Index(_)))
    };
    for data in body.basic_blocks.iter() {
        for st in &data.statements {
            let StatementKind::Assign(asg) = &st.kind else { continue };
            let (place, rv) = &**asg;
            if place.is_indirect() {
                continue;
            }
            let dst = place.local;
            let operand = |op: &Operand<'_>, flags: &mut Vec<u8>, deps: &mut Vec<_>| {
                let Some(pl) = op.place() else { return None };
                if reads_memory(&pl) {
                    flags[dst.as_usize()] |= LOAD;
                    None
                } else {
                    deps.push((dst, pl.local));
                    Some(pl.local)
                }
            };
            match rv {
                Rvalue::Use(op, ..) | Rvalue::Cast(_, op, _) | Rvalue::UnaryOp(_, op) => {
                    operand(op, &mut flags, &mut deps);
                }
                Rvalue::BinaryOp(bop, ops) => {
                    let (a, b) = &**ops;
                    let step = matches!(
                        bop,
                        BinOp::Add
                            | BinOp::AddUnchecked
                            | BinOp::AddWithOverflow
                            | BinOp::Sub
                            | BinOp::SubUnchecked
                            | BinOp::SubWithOverflow
                            | BinOp::Offset
                    );
                    for op in [a, b] {
                        if let Some(src) = operand(op, &mut flags, &mut deps)
                            && step
                        {
                            arith.push((dst, src));
                        }
                    }
                }
                Rvalue::Ref(_, _, pl) | Rvalue::RawPtr(_, pl) => {
                    // An address: the base pointer and the indices, no load
                    // (a second deref on the way is one).
                    let derefs =
                        pl.projection.iter().filter(|e| matches!(e, ProjectionElem::Deref)).count();
                    if derefs > 1 {
                        flags[dst.as_usize()] |= LOAD;
                    }
                    deps.push((dst, pl.local));
                    for e in pl.projection.iter() {
                        if let ProjectionElem::Index(i) = e {
                            deps.push((dst, i));
                        }
                    }
                }
                Rvalue::Aggregate(_, ops) => {
                    for op in ops.iter() {
                        operand(op, &mut flags, &mut deps);
                    }
                }
                _ => {}
            }
        }
    }
    // An arithmetic assignment on a dependency cycle is a recurrence.
    let graph = rustc_data_structures::graph::vec_graph::VecGraph::<mir::Local>::new(n, deps.clone());
    let sccs = rustc_data_structures::graph::scc::Sccs::<mir::Local, usize>::new(&graph);
    for &(dst, src) in &arith {
        if sccs.scc(dst) == sccs.scc(src) {
            flags[dst.as_usize()] |= STEP;
        }
    }
    // Both facts flow along the dependencies (bounded rounds; statements
    // are mostly in dependency order).
    for _ in 0..32 {
        let mut changed = false;
        for &(dst, src) in &deps {
            let f = flags[dst.as_usize()] | flags[src.as_usize()];
            if f != flags[dst.as_usize()] {
                flags[dst.as_usize()] = f;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    flags
}

/// `SEQ` / `RAND` / neither for an access whose address is `base`:
/// sequential when the pointer or an index steps through memory, random
/// when it comes out of memory (and does not step), unknown otherwise
/// (a parameter, a stack local).
fn access_class(flow: &[u8], base: mir::PlaceRef<'_>) -> Option<u32> {
    let mut derefs = 0;
    let mut f = 0u8;
    for e in base.projection {
        match e {
            ProjectionElem::Deref => derefs += 1,
            ProjectionElem::Index(i) => f |= flow[i.as_usize()],
            _ => {}
        }
    }
    if derefs > 0 {
        f |= flow[base.local.as_usize()];
    }
    if derefs > 1 {
        f |= LOAD;
    }
    if f & STEP != 0 {
        Some(SEQ)
    } else if f & LOAD != 0 {
        Some(RAND)
    } else {
        None
    }
}

pub(crate) fn heat_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| pliron_cfg::env_get("PLIRON_FIELD_HEAT").is_some_and(|p| !p.is_empty()))
}

thread_local! {
    /// Function -> (MIR block index, backend block), filled by
    /// `append_block` while the CGU's bodies are built (main thread).
    static BLOCKS: RefCell<HashMap<Ptr<Operation>, Vec<(u32, Ptr<BasicBlock>)>>> =
        RefCell::new(HashMap::new());
}

/// `Builder::append_block`: remember which backend block is MIR block N.
/// `rustc_codegen_ssa` names them `start` (bb0) and `bbN`; every other
/// name is a helper block with no MIR statements.
pub(crate) fn note_block(llfn: Ptr<Operation>, name: &str, bb: Ptr<BasicBlock>) {
    if !heat_on() {
        return;
    }
    let i = if name == "start" {
        0
    } else if let Some(i) = name.strip_prefix("bb").and_then(|s| s.parse::<u32>().ok()) {
        i
    } else {
        return;
    };
    BLOCKS.with(|b| b.borrow_mut().entry(llfn).or_default().push((i, bb)));
}

/// One CGU's accesses waiting for the profile: `(block, key, times)`.
struct Pending {
    keys: Vec<(String, String)>,
    hits: Vec<(Ptr<BasicBlock>, u32, u64)>,
    /// `(MIR block, field)` pairs with no backend block / in instances
    /// whose function was not found (for the trace line).
    unmapped: (u64, u64),
}

static PENDING: Mutex<Option<HashMap<String, Pending>>> = Mutex::new(None);
static TOTALS: Mutex<Option<HashMap<(String, String), u64>>> = Mutex::new(None);

fn add_totals(items: impl Iterator<Item = ((String, String), u64)>) {
    let mut g = TOTALS.lock().unwrap();
    let t = g.get_or_insert_with(HashMap::new);
    for (k, w) in items {
        if w != 0 {
            let e = t.entry(k).or_insert(0);
            *e = e.saturating_add(w);
        }
    }
}

/// Loop depth of every MIR block (natural loops of back edges, one loop
/// per header). Work is bounded; over budget the remaining loops are not
/// counted.
fn loop_depths(body: &mir::Body<'_>) -> Vec<u32> {
    let n = body.basic_blocks.len();
    let mut depth = vec![0u32; n];
    let doms = body.basic_blocks.dominators();
    let preds = body.basic_blocks.predecessors();
    let mut latches: Vec<(mir::BasicBlock, mir::BasicBlock)> = Vec::new();
    for (bb, data) in body.basic_blocks.iter_enumerated() {
        let Some(t) = &data.terminator else { continue };
        for s in t.successors() {
            if doms.is_reachable(bb) && doms.dominates(s, bb) {
                latches.push((s, bb));
            }
        }
    }
    latches.sort();
    let mut mark = vec![u32::MAX; n];
    let mut budget = 1usize << 20;
    let mut work = Vec::new();
    let mut i = 0;
    let mut id = 0u32;
    while i < latches.len() {
        let h = latches[i].0;
        mark[h.as_usize()] = id;
        depth[h.as_usize()] += 1;
        while i < latches.len() && latches[i].0 == h {
            work.push(latches[i].1);
            i += 1;
        }
        while let Some(b) = work.pop() {
            if mark[b.as_usize()] == id {
                continue;
            }
            mark[b.as_usize()] = id;
            depth[b.as_usize()] += 1;
            budget = budget.saturating_sub(1);
            work.extend(preds[b].iter().copied());
        }
        if budget == 0 {
            break;
        }
        id += 1;
    }
    depth
}

struct Walk<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'a mir::Body<'tcx>,
    /// `(struct, field index)` -> index into `keys`, `None` = not recorded.
    ids: &'a mut HashMap<(DefId, u32), Option<u32>>,
    keys: &'a mut Vec<(String, String)>,
    /// Accesses per `(MIR block, key)` in this body.
    hits: HashMap<(u32, u32), u64>,
    /// `local_flow` of the body.
    flow: Vec<u8>,
}

impl<'tcx> Walk<'_, 'tcx> {
    fn key(&mut self, def: ty::AdtDef<'tcx>, field: u32) -> Option<u32> {
        let tcx = self.tcx;
        let keys = &mut *self.keys;
        *self.ids.entry((def.did(), field)).or_insert_with(|| {
            if !plain_repr(&def.repr()) || !crate_ok(tcx, def.did().krate) {
                return None;
            }
            let name = match field {
                SEQ => SEQ_NAME.to_string(),
                RAND => RAND_NAME.to_string(),
                _ => def
                    .non_enum_variant()
                    .fields
                    .get(rustc_abi::FieldIdx::from_u32(field))?
                    .name
                    .to_string(),
            };
            keys.push((type_key(tcx, def.did()), name));
            Some(keys.len() as u32 - 1)
        })
    }
}

impl<'tcx> Visitor<'tcx> for Walk<'_, 'tcx> {
    fn visit_place(&mut self, place: &mir::Place<'tcx>, ctx: PlaceContext, loc: Location) {
        if place.projection.is_empty()
            || matches!(ctx, PlaceContext::NonUse(NonUseContext::VarDebugInfo))
        {
            return;
        }
        for (base, elem) in place.as_ref().iter_projections() {
            let ProjectionElem::Field(f, _) = elem else { continue };
            let bt = base.ty(&self.body.local_decls, self.tcx);
            if bt.variant_index.is_some() {
                continue;
            }
            let ty::Adt(def, _) = bt.ty.kind() else { continue };
            if !def.is_struct() {
                continue;
            }
            if let Some(k) = self.key(*def, f.as_u32()) {
                *self.hits.entry((loc.block.as_u32(), k)).or_insert(0) += 1;
                if let Some(c) = access_class(&self.flow, base)
                    && let Some(k) = self.key(*def, c)
                {
                    *self.hits.entry((loc.block.as_u32(), k)).or_insert(0) += 1;
                }
            }
        }
    }
}

/// After a CGU's bodies are built: walk the MIR of every instance it
/// defines. Without a profile the static weights go straight to the
/// crate totals; with one, the accesses wait (keyed by CGU name) for
/// `resolve`.
pub(crate) fn record_cgu<'tcx>(
    cx: &CodegenCx<'tcx>,
    cgu: &str,
    insts: impl Iterator<Item = Instance<'tcx>>,
) {
    let tcx = cx.tcx;
    let profiled = pliron_cfg::env_get("PLIRON_PGO_USE").is_some();
    let blocks = BLOCKS.with(|b| std::mem::take(&mut *b.borrow_mut()));
    let mut ids = HashMap::new();
    let mut keys = Vec::new();
    let mut pend: Vec<(Ptr<BasicBlock>, u32, u64)> = Vec::new();
    let mut stat: HashMap<u32, u64> = HashMap::new();
    let mut unmapped = (0u64, 0u64);
    for inst in insts {
        let body = tcx.instance_mir(inst.def);
        let mut walk = Walk {
            tcx,
            body,
            ids: &mut ids,
            keys: &mut keys,
            hits: HashMap::new(),
            flow: local_flow(body),
        };
        walk.visit_body(body);
        let hits = walk.hits;
        if hits.is_empty() {
            continue;
        }
        if !profiled {
            let depth = loop_depths(body);
            for ((bb, k), c) in hits {
                let d = depth.get(bb as usize).copied().unwrap_or(0).min(6);
                let e = stat.entry(k).or_insert(0);
                *e = e.saturating_add(c.saturating_mul(1u64 << (3 * d)));
            }
            continue;
        }
        let op = cx.st.borrow().funcs.get(tcx.symbol_name(inst).name).map(|f| f.op);
        let Some(list) = op.and_then(|op| blocks.get(&op)) else {
            unmapped.1 += hits.len() as u64;
            continue;
        };
        let mut llbb: Vec<Option<Ptr<BasicBlock>>> = vec![None; body.basic_blocks.len()];
        for &(i, b) in list {
            if let Some(s) = llbb.get_mut(i as usize) {
                *s = Some(b);
            }
        }
        let preds = body.basic_blocks.predecessors();
        for ((bb, k), c) in hits {
            // A block merged into its only predecessor was emitted there.
            let mut cur = mir::BasicBlock::from_u32(bb);
            let mut steps = 0;
            let b = loop {
                if let Some(b) = llbb[cur.as_usize()] {
                    break Some(b);
                }
                let &[p] = preds[cur].as_slice() else { break None };
                steps += 1;
                if steps > 64 {
                    break None;
                }
                cur = p;
            };
            match b {
                Some(b) => pend.push((b, k, c)),
                None => unmapped.0 += 1,
            }
        }
    }
    if !profiled {
        add_totals(stat.into_iter().map(|(k, w)| (keys[k as usize].clone(), w)));
    } else if !pend.is_empty() {
        // HashMap iteration order above is arbitrary; sums don't care.
        PENDING
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(cgu.to_string(), Pending { keys, hits: pend, unmapped });
    }
}

/// `finish_pre`, right after `pgo::load`: turn the CGU's waiting accesses
/// into weights with the measured block counts. A block without a count
/// (function absent from the profile, or stale) contributes nothing.
pub(crate) fn resolve(cgu: &str, st: &pliron_ir_opt::state::State) {
    if !heat_on() {
        return;
    }
    let Some(p) = PENDING.lock().unwrap().as_mut().and_then(|m| m.remove(cgu)) else { return };
    let mut sums: HashMap<u32, u64> = HashMap::new();
    let (mut counted, mut zero, mut nocount) = (0u64, 0u64, 0u64);
    for (b, k, c) in p.hits {
        match pliron_ir_opt::pgo::block_count(st, b) {
            Some(n) => {
                if n == 0 {
                    zero += 1;
                } else {
                    counted += 1;
                }
                let e = sums.entry(k).or_insert(0);
                *e = e.saturating_add(n.saturating_mul(c));
            }
            None => nocount += 1,
        }
    }
    // `PLIRON_TRACE=pliron::field_order`: how the `(block, field)` pairs
    // of this CGU found their counts.
    if pliron_cfg::tracing_on() {
        tracing::info!(target: "pliron::field_order",
            "field-heat {cgu}: {counted} counted, {zero} count 0, {nocount} no profile, {} no backend block, {} no function",
            p.unmapped.0, p.unmapped.1);
    }
    add_totals(sums.into_iter().map(|(k, w)| (p.keys[k as usize].clone(), w)));
}

/// End of the crate's codegen (`join_codegen`): append the totals, sorted,
/// in one write so concurrent rustc processes don't interleave lines.
pub(crate) fn flush() {
    if !heat_on() {
        return;
    }
    let Some(path) = pliron_cfg::env_get("PLIRON_FIELD_HEAT") else { return };
    let Some(t) = TOTALS.lock().unwrap().take() else { return };
    let mut rows: Vec<_> = t.into_iter().collect();
    rows.sort();
    let mut out = String::new();
    for ((ty, f), w) in rows {
        out.push_str(&format!("{ty}\t{f}\t{w}\n"));
    }
    if out.is_empty() {
        return;
    }
    use std::io::Write;
    let r = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(out.as_bytes()));
    if let Err(e) = r {
        tracing::warn!(target: "pliron::field_order", "PLIRON_FIELD_HEAT: {path}: {e}");
    }
}
