//! Profile-guided optimization (docs/pgo.md).
//!
//! `instrument` places counters and emits the `__pliron_prof_*` data
//! objects + registration ctor a profiled build needs; `load` reads a
//! `pliron.prof` back and stores per-block/edge counts in `State` for the
//! optimization pipeline and lowering to consult.
//!
//! Function records are keyed by `(symbol hash, CFG fingerprint)`: a
//! profile taken from an older build never misapplies — the fingerprint
//! just doesn't match. Counters are keyed to *source-level* functions:
//! instrument runs before the inliner, so counts measured inside inlined
//! clones still accrue to the callee's own counters, LLVM-style.

use std::hash::Hasher;

use cranelift_module::Linkage;
use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::SingleBlockRegionInterface;
use pliron::builtin::ops::ModuleOp;
use pliron::builtin::types::{IntegerType, Signedness};
use pliron::context::{Context, Ptr};
use pliron::identifier::Identifier;
use pliron::linked_list::ContainsLinkedList;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed};
use pliron::value::Value;
use pliron_llvm::attributes::{
    AtomicOrderingAttr, AtomicRmwKindAttr, IntegerOverflowFlagsAttr, SyncScopeAttr,
};
use pliron_llvm::op_interfaces::IntBinArithOpWithOverflowFlag;
use pliron_llvm::ops::{
    AddOp, AtomicRmwOp, BrOp, CondBrOp, GlobalOp, LoadOp, ReturnOp, StoreOp, SwitchOp,
    UnreachableOp,
};
use pliron_llvm::types::{ArrayType, PointerType, StructLayout, StructType};
use rustc_hash::{FxHashMap, FxHasher};

use crate::has_body;
use crate::inline::blocks;
use crate::state::{ConstVal, GlobalInfo, State};

/// `.prof` magic: "PLPROF01" little-endian.
pub const MAGIC: u64 = u64::from_le_bytes(*b"PLPROF01");
/// Header magic of a CGU that carries the indirect-call extension
/// (`{crate id, cgu hash, sites ptr, nsites, fns ptr, nfns, names ptr,
/// names len, crate hash}` after `recs`).
pub const MAGIC2: u64 = u64::from_le_bytes(*b"PLPROF02");
/// Magic of the `.prof` trailer holding the indirect-call sites. Readers
/// that stop after the function records never see it.
pub const ICALL_MAGIC: u64 = u64::from_le_bytes(*b"PLICALL2");
/// Target slots per indirect-call site; a site is `SLOTS` x `(address,
/// count)` plus one count for everything else.
pub const SLOTS: usize = 4;
/// `ProfTarget::flags`: the target's symbol is visible to other objects.
pub const TARGET_EXTERNAL: u32 = 1;

/// One callee an indirect-call site reached.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProfTarget {
    pub name: String,
    /// `StableCrateId` of the crate whose object defined the address,
    /// and that crate's hash (SVH) in the instrumented build (0 for a
    /// crate nothing can depend on).
    pub crate_id: u64,
    pub crate_hash: u64,
    /// Hash of the name of the CGU that defined it (`cgu_hash`).
    pub cgu: u64,
    pub flags: u32,
    pub count: u64,
}

/// One indirect-call site's value profile.
#[derive(Clone, Debug, Default)]
pub struct ProfSite {
    /// Calls to targets with no slot or no name (uninstrumented code).
    pub other: u64,
    pub targets: Vec<ProfTarget>,
}

impl ProfSite {
    pub fn merge(&mut self, o: &ProfSite) {
        self.other += o.other;
        for t in &o.targets {
            match self
                .targets
                .iter_mut()
                .find(|x| x.name == t.name && x.crate_id == t.crate_id && x.cgu == t.cgu)
            {
                Some(x) => {
                    x.count += t.count;
                    x.flags |= t.flags;
                }
                None => self.targets.push(t.clone()),
            }
        }
    }
}

/// Hash of a CGU name as the profile stores it.
pub fn cgu_hash(cgu: &str) -> u64 {
    sym_hash(&sanitize(cgu))
}

fn sanitize(cgu: &str) -> String {
    cgu.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect()
}

/// The object format a CGU is emitted as — pliron-ir-opt is rustc-free,
/// so the caller maps `sess.target` onto this before section names or
/// ctor asm are picked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjFmt {
    Elf,
    MachO,
    Coff,
    Wasm,
}

/// A function's counts from one profile record.
#[derive(Clone, Debug, Default)]
pub struct ProfFn {
    pub fingerprint: u64,
    /// Block-entry counts in instrument-time block order.
    pub blocks: Vec<u64>,
    /// `(pred_block_idx, succ_slot, count)` for instrumented edges.
    pub edges: Vec<(u16, u16, u64)>,
    /// Indirect-call sites in `devirt::sites` order.
    pub sites: Vec<ProfSite>,
}

/// One counter site: `load ctr; add 1; store ctr` at the head of `block`
/// (or a relaxed `atomicrmw add` under `PLIRON_PGO_ATOMIC`).
fn bump(ctx: &mut Context, st: &mut State, block: Ptr<BasicBlock>, ctrs: &str, idx: u32) {
    let ptr_t = PointerType::get(ctx, 0).into();
    let i64t = IntegerType::get(ctx, 64, Signedness::Signless).into();
    let addr = crate::sroa::mk_const(
        ctx,
        st,
        ptr_t,
        ConstVal::Sym {
            sym: ctrs.to_string(),
            off: idx as i64 * 8,
        },
    );
    let atomic = crate::env_is("PLIRON_PGO_ATOMIC", "1");
    let ops: Vec<Ptr<Operation>> = if atomic {
        let one = crate::sroa::mk_const(ctx, st, i64t, ConstVal::Bits(1));
        let r = AtomicRmwOp::new(
            ctx,
            addr,
            one,
            AtomicRmwKindAttr::Add,
            AtomicOrderingAttr::Monotonic,
            SyncScopeAttr::System,
        )
        .get_operation();
        st.rmw.insert(r, AtomicRmwKindAttr::Add);
        vec![r]
    } else {
        let l = LoadOp::new(ctx, addr, i64t).get_operation();
        let lv = l.deref(ctx).get_result(ctx, 0);
        let one = crate::sroa::mk_const(ctx, st, i64t, ConstVal::Bits(1));
        let a = AddOp::new_with_overflow_flag(
            ctx,
            lv,
            one,
            IntegerOverflowFlagsAttr::default(),
        )
        .get_operation();
        let av = a.deref(ctx).get_result(ctx, 0);
        let s = StoreOp::new(ctx, av, addr).get_operation();
        st.volatile.insert(l);
        st.volatile.insert(s);
        vec![l, a, s]
    };
    // Insert at the block head, before the first real op. An empty
    // (orphaned) block just keeps its slot at 0 forever.
    let Some(head) = block.deref(ctx).iter(ctx).next() else {
        return;
    };
    for op in ops {
        op.insert_before(ctx, head);
    }
}

fn term_kind(ctx: &Context, t: Ptr<Operation>) -> u8 {
    if Operation::is_op::<CondBrOp>(t, ctx) {
        2
    } else if Operation::is_op::<BrOp>(t, ctx) {
        1
    } else if Operation::is_op::<SwitchOp>(t, ctx) {
        3
    } else if Operation::is_op::<ReturnOp>(t, ctx) {
        4
    } else if Operation::is_op::<UnreachableOp>(t, ctx) {
        5
    } else {
        0
    }
}

fn terminator(ctx: &Context, b: Ptr<BasicBlock>) -> Option<Ptr<Operation>> {
    b.deref(ctx).iter(ctx).last()
}

fn succs(ctx: &Context, t: Ptr<Operation>) -> Vec<Ptr<BasicBlock>> {
    t.deref(ctx).successors(ctx).collect()
}

/// Edges block counts can't determine: a terminator with >1 successor
/// reaching a block with >1 predecessor (or reached twice). Returns the
/// edges in `(pred_block_idx, succ_slot)` order — the counter layout.
fn ambiguous_edges(
    ctx: &Context,
    bs: &[Ptr<BasicBlock>],
) -> (rustc_hash::FxHashMap<Ptr<BasicBlock>, u32>, Vec<(u16, u16)>) {
    let mut npred: rustc_hash::FxHashMap<Ptr<BasicBlock>, u32> = FxHashMap::default();
    for b in bs {
        for op in b.deref(ctx).iter(ctx) {
            for s in op.deref(ctx).successors(ctx) {
                *npred.entry(s).or_default() += 1;
            }
        }
    }
    let mut edges = Vec::new();
    for (i, b) in bs.iter().enumerate() {
        let Some(t) = terminator(ctx, *b) else {
            continue;
        };
        let ss = succs(ctx, t);
        if ss.len() <= 1 {
            continue;
        }
        for (si, s) in ss.iter().enumerate() {
            let dup = ss[..si].contains(s);
            if npred.get(s).copied().unwrap_or(0) > 1 || dup {
                edges.push((i as u16, si as u16));
            }
        }
    }
    (npred, edges)
}

/// CFG fingerprint: keyed to the pre-instrumentation shape so a profile
/// from a different build can't misapply. Covers block count, per-block
/// terminator kind/successor count, and the edge-counter layout.
pub fn fingerprint(ctx: &Context, f: Ptr<Operation>) -> (u64, Vec<(u16, u16)>) {
    let bs = blocks(ctx, f);
    let (_, edges) = ambiguous_edges(ctx, &bs);
    let mut h = FxHasher::default();
    h.write_u32(bs.len() as u32);
    for b in &bs {
        match terminator(ctx, *b) {
            Some(t) => {
                h.write_u8(term_kind(ctx, t));
                h.write_u8(t.deref(ctx).get_num_successors() as u8);
            }
            None => h.write_u8(6),
        }
        h.write_u8(b.deref(ctx).get_num_arguments() as u8);
    }
    for (p, s) in &edges {
        h.write_u16(*p);
        h.write_u16(*s);
    }
    (h.finish(), edges)
}

pub fn sym_hash(sym: &str) -> u64 {
    let mut h = FxHasher::default();
    h.write(sym.as_bytes());
    h.finish()
}

/// Identifier for a symbol (same `s{n}_sanitized` scheme as context.rs,
/// plus the sym<->ident maps).
pub(crate) fn ident(st: &mut State, sym: &str) -> Identifier {
    if let Some(i) = st.sym_to_ident.get(sym) {
        return i.clone();
    }
    st.counter += 1;
    let mut s = format!("s{}_", st.counter);
    s.extend(sym.chars().map(|c| {
        if c.is_ascii_alphanumeric() || c == '_' {
            c
        } else {
            '_'
        }
    }));
    let id = Identifier::try_new(s.clone()).unwrap();
    st.sym_to_ident.insert(sym.to_string(), id.clone());
    st.ident_to_sym.insert(s, sym.to_string());
    id
}

fn declare_global(
    ctx: &mut Context,
    st: &mut State,
    module: Ptr<Operation>,
    sym: &str,
    info: GlobalInfo,
) {
    let id = ident(st, sym);
    let op = GlobalOp::new(ctx, id, info.ty).get_operation();
    Operation::get_op::<ModuleOp>(module, ctx)
        .unwrap()
        .append_operation(ctx, op, 0);
    st.globals.insert(sym.to_string(), info);
}

/// Instrument every body in the module. Emits the counters table, the
/// per-function records + header globals, and the counter ops. Returns
/// `(hdr_sym, ctor_sym)` — the caller emits the ctor asm (native) or
/// leaves the profile region to the linker/host (wasm). `fmt` selects
/// data-object sections (native `pliron_prof*`) vs. plain data objects
/// the wasm linker resolves.
pub fn instrument(
    ctx: &mut Context,
    st: &mut State,
    cgu: &str,
    fmt: ObjFmt,
    (crate_id, crate_hash): (u64, u64),
) -> (String, String, String) {
    let wasm = fmt == ObjFmt::Wasm;
    // Indirect-call value profile: native only (wasm keeps the v1 header
    // its host reads).
    let icp = !wasm && crate::devirt::icp_on();
    // CGU names carry '-' ('foo.abc-cgu.0') — illegal in asm operands,
    // and the hdr/ctor syms cross into rsasm text; sanitize the suffix.
    let cgu: String = sanitize(cgu);
    let cgu_id = sym_hash(&cgu);
    let icalls_sym = format!("__pliron_prof_icalls.{cgu}");
    let ctrs_sym = format!("__pliron_prof_ctrs.{cgu}");
    let hdr_sym = format!("__pliron_prof_hdr.{cgu}");
    let ctor_sym = format!("__pliron_prof_ctor.{cgu}");

    let funcs: Vec<(String, Ptr<Operation>)> = st
        .funcs
        .iter()
        .filter(|(n, f)| has_body(ctx, f.op) && !st.dead_fns.contains(*n))
        .map(|(n, f)| (n.clone(), f.op))
        .collect();
    let Some((_, first)) = funcs.first() else {
        // Nothing to count (an incremental build's fallback unit, a unit
        // of statics): no header is defined, so the caller must not emit
        // the constructor that registers it.
        return (String::new(), ctor_sym, ctrs_sym);
    };
    let module = first
        .deref(ctx)
        .get_parent_op(ctx)
        .expect("pgo: func without module");

    let i8t = IntegerType::get(ctx, 8, Signedness::Signless).into();
    let i64t = IntegerType::get(ctx, 64, Signedness::Signless).into();
    let ptr_t = PointerType::get(ctx, 0).into();

    // ---- counters ----
    let mut recs = Vec::new(); // serialized record bytes
    // `PLIRON_PGO_COUNTERS=block` skips edge counters entirely (the CFG
    // layout still feeds the fingerprint, so a block-only profile keeps
    // matching); edge counts alone can't disambiguate block counts, so
    // any other value means block+edge.
    let edges_on = crate::env_get("PLIRON_PGO_COUNTERS").is_none_or(|c| c != "block");
    let mut names = Vec::new(); // sym_hash -> name blob
    let mut nctrs = 0u32;
    let mut nsites = 0u32;
    // (sym hash, symbol, flags) of every body here: the address table.
    let mut ftab: Vec<(u64, String, u32)> = Vec::new();
    for (sym, fop) in funcs {
        let bs = blocks(ctx, fop);
        let (fp, all_edges) = fingerprint(ctx, fop);
        // Sites are numbered before any counter block exists, the way
        // `devirt::promote` numbers them in the profile-use build.
        let fsites = if icp { crate::devirt::sites(ctx, st, &bs) } else { Vec::new() };
        for (k, &(op, callee)) in fsites.iter().enumerate() {
            let site = crate::sroa::mk_const(
                ctx,
                st,
                ptr_t,
                ConstVal::Sym { sym: icalls_sym.clone(), off: (nsites as i64 + k as i64) * (SLOTS as i64 * 2 + 1) * 8 },
            );
            crate::devirt::record_call(ctx, st, op, site, callee);
        }
        if icp {
            let ext = st.funcs.get(&sym).is_some_and(|f| {
                matches!(f.linkage, Linkage::Export | Linkage::Hidden | Linkage::Preemptible)
            }) && !st.xcgu_extra.contains(&sym);
            ftab.push((sym_hash(&sym), sym.clone(), if ext { TARGET_EXTERNAL } else { 0 }));
        }
        let edges = if edges_on { all_edges } else { Vec::new() };
        let sh = sym_hash(&sym);
        let base = nctrs;
        // Block-entry counters, then edge counters in `edges` order.
        for (i, b) in bs.iter().enumerate() {
            bump(ctx, st, *b, &ctrs_sym, base + i as u32);
        }
        nctrs += bs.len() as u32;
        let region = fop.deref(ctx).get_region(ctx, 0);
        for &(pi, si) in &edges {
            let Some(t) = terminator(ctx, bs[pi as usize]) else {
                continue;
            };
            let s = t.deref(ctx).get_successor(ctx, si as usize);
            let arg_tys: Vec<TypeHandle> =
                s.deref(ctx).arguments(ctx).map(|a| a.get_type(ctx)).collect();
            let e = BasicBlock::new(ctx, None, arg_tys);
            e.insert_at_back(region, ctx);
            let args: Vec<Value> = e.deref(ctx).arguments(ctx).collect();
            let br = BrOp::new(ctx, s, args).get_operation();
            br.insert_at_back(e, ctx);
            bump(ctx, st, e, &ctrs_sym, nctrs);
            Operation::replace_successor(t, ctx, si as usize, e);
            nctrs += 1;
        }
        // record: sym_hash, fp, ctr_base, nblocks, nedges, pad, edges[]
        recs.extend_from_slice(&sh.to_le_bytes());
        recs.extend_from_slice(&fp.to_le_bytes());
        recs.extend_from_slice(&(base as u64).to_le_bytes());
        recs.extend_from_slice(&(bs.len() as u32).to_le_bytes());
        recs.extend_from_slice(&(edges.len() as u32).to_le_bytes());
        // The record's spare word: how many of the CGU's sites, in table
        // order, are this function's.
        recs.extend_from_slice(&(fsites.len() as u32).to_le_bytes());
        nsites += fsites.len() as u32;
        for (p, s) in &edges {
            recs.extend_from_slice(&((*p as u32) << 16 | *s as u32).to_le_bytes());
        }
        while recs.len() % 8 != 0 {
            recs.push(0);
        }
        // name blob: sym_hash, len u16, bytes
        names.extend_from_slice(&sh.to_le_bytes());
        names.extend_from_slice(&(sym.len() as u16).to_le_bytes());
        names.extend_from_slice(sym.as_bytes());
        while names.len() % 8 != 0 {
            names.push(0);
        }
    }

    // ---- data objects ----
    // Mach-O wants `seg,sec` form with a 16-byte sectname cap (so
    // `pliron_prof_names` doesn't fit); ELF/COFF names are free-form.
    let sec = |elf: &str, macho: &str| match fmt {
        ObjFmt::Wasm => None,
        ObjFmt::MachO => Some(format!("__DATA,{macho}")),
        _ => Some(elf.to_string()),
    };
    let mk = |ctx: &mut Context, st: &mut State, ty: TypeHandle, cv: ConstVal| {
        crate::sroa::mk_const(ctx, st, ty, cv)
    };
    // counters: [nctrs x i64] zeroed, mutable
    if nctrs > 0 {
        let ty = ArrayType::get(ctx, i64t, nctrs as u64).into();
        let init = mk(ctx, st, ty, ConstVal::Zero);
        declare_global(
            ctx,
            st,
            module,
            &ctrs_sym,
            GlobalInfo {
                ty,
                init: Some(init),
                align: 8,
                mutable: true,
                tls: false,
                linkage: Linkage::Local,
                used: true,
                section: sec("pliron_prof_ctrs", "pliron_prof_ctrs"),
            },
        );
    }
    // header+records: {magic, ctrs ptr, nctrs, recs_len, recs bytes}.
    // One wire layout everywhere: wasm writes only the low 4 bytes of the
    // ctrs reloc (linear addr), so it uses an i64-typed field there too
    // and the offsets stay {0,8,16,24} on both targets.
    // symbol names for `pliron-prof dump` and for naming icall targets
    let names_len = names.len() as u64;
    let names_ty = ArrayType::get(ctx, i8t, names_len).into();
    let names_sym = format!("__pliron_prof_names.{cgu}");
    let init = mk(ctx, st, names_ty, ConstVal::Bytes(names));
    declare_global(
        ctx,
        st,
        module,
        &names_sym,
        GlobalInfo {
            ty: names_ty,
            init: Some(init),
            align: 1,
            mutable: false,
            tls: false,
            linkage: Linkage::Local,
            used: false,
            section: sec("pliron_prof_names", "pliron_pnames"),
        },
    );
    // Indirect-call extension: the site table (zeroed, mutable) and the
    // address table `{sym hash, &fn, flags}` per body, which lets
    // `pliron-prof` turn a recorded address into a symbol.
    let mut ext_tys: Vec<TypeHandle> = Vec::new();
    let mut ext_vals: Vec<Value> = Vec::new();
    if icp {
        let words = nsites as u64 * (SLOTS as u64 * 2 + 1);
        if nsites > 0 {
            let ty = ArrayType::get(ctx, i64t, words).into();
            let init = mk(ctx, st, ty, ConstVal::Zero);
            declare_global(
                ctx,
                st,
                module,
                &icalls_sym,
                GlobalInfo {
                    ty,
                    init: Some(init),
                    align: 8,
                    mutable: true,
                    tls: false,
                    linkage: Linkage::Local,
                    used: true,
                    section: sec("pliron_prof_icall", "pliron_picall"),
                },
            );
        }
        let fns_sym = format!("__pliron_prof_fns.{cgu}");
        let mut tys = Vec::with_capacity(ftab.len() * 3);
        let mut vals = Vec::with_capacity(ftab.len() * 3);
        for (h, sym, flags) in &ftab {
            tys.extend([i64t, ptr_t, i64t]);
            vals.push(mk(ctx, st, i64t, ConstVal::Bits(*h as u128)));
            vals.push(mk(ctx, st, ptr_t, ConstVal::Sym { sym: sym.clone(), off: 0 }));
            vals.push(mk(ctx, st, i64t, ConstVal::Bits(*flags as u128)));
        }
        let fns_ty = StructType::get_unnamed(ctx, (tys, StructLayout::Unpacked)).into();
        let init = mk(ctx, st, fns_ty, ConstVal::Agg(vals));
        declare_global(
            ctx,
            st,
            module,
            &fns_sym,
            GlobalInfo {
                ty: fns_ty,
                init: Some(init),
                align: 8,
                mutable: false,
                tls: false,
                linkage: Linkage::Local,
                used: true,
                section: sec("pliron_prof_fns", "pliron_pfns"),
            },
        );
        let sym_or_null = |ctx: &mut Context, st: &mut State, sym: &str, present: bool| {
            if present {
                (ptr_t, mk(ctx, st, ptr_t, ConstVal::Sym { sym: sym.to_string(), off: 0 }))
            } else {
                (i64t, mk(ctx, st, i64t, ConstVal::Bits(0)))
            }
        };
        let (st_t, st_v) = sym_or_null(ctx, st, &icalls_sym, nsites > 0);
        let (fn_t, fn_v) = sym_or_null(ctx, st, &fns_sym, !ftab.is_empty());
        let (nm_t, nm_v) = sym_or_null(ctx, st, &names_sym, names_len > 0);
        ext_tys = vec![i64t, i64t, st_t, i64t, fn_t, i64t, nm_t, i64t, i64t];
        ext_vals = vec![
            mk(ctx, st, i64t, ConstVal::Bits(crate_id as u128)),
            mk(ctx, st, i64t, ConstVal::Bits(cgu_id as u128)),
            st_v,
            mk(ctx, st, i64t, ConstVal::Bits(nsites as u128)),
            fn_v,
            mk(ctx, st, i64t, ConstVal::Bits(ftab.len() as u128)),
            nm_v,
            mk(ctx, st, i64t, ConstVal::Bits(names_len as u128)),
            mk(ctx, st, i64t, ConstVal::Bits(crate_hash as u128)),
        ];
    }
    // header+records: {magic, ctrs ptr, nctrs, recs_len, recs bytes}
    // (+ the indirect-call extension, 9 words, right after `recs`).
    // One wire layout everywhere: wasm writes only the low 4 bytes of the
    // ctrs reloc (linear addr), so it uses an i64-typed field there too
    // and the offsets stay {0,8,16,24} on both targets.
    let ctrs_t = if wasm { i64t } else { ptr_t };
    let recs_ty = ArrayType::get(ctx, i8t, recs.len() as u64).into();
    let mut hdr_fields = vec![i64t, ctrs_t, i64t, i64t, recs_ty];
    hdr_fields.extend(ext_tys);
    let hdr_ty = StructType::get_unnamed(ctx, (hdr_fields, StructLayout::Unpacked)).into();
    let f0 = mk(ctx, st, i64t, ConstVal::Bits(if icp { MAGIC2 } else { MAGIC } as u128));
    let f1 = mk(
        ctx,
        st,
        ctrs_t,
        ConstVal::Sym {
            sym: ctrs_sym.clone(),
            off: 0,
        },
    );
    let f2 = mk(ctx, st, i64t, ConstVal::Bits(nctrs as u128));
    let f3 = mk(ctx, st, i64t, ConstVal::Bits(recs.len() as u128));
    let f4 = mk(ctx, st, recs_ty, ConstVal::Bytes(recs));
    let mut hdr_vals = vec![f0, f1, f2, f3, f4];
    hdr_vals.extend(ext_vals);
    let init = mk(ctx, st, hdr_ty, ConstVal::Agg(hdr_vals));
    declare_global(
        ctx,
        st,
        module,
        &hdr_sym,
        GlobalInfo {
            ty: hdr_ty,
            init: Some(init),
            align: 8,
            mutable: false,
            tls: false,
            linkage: Linkage::Local,
            used: true,
            section: sec("pliron_prof", "pliron_prof"),
        },
    );

    (hdr_sym, ctor_sym, ctrs_sym)
}

/// Registration ctor + init slot, as rsasm text: ELF gets a `.init_array`
/// entry, Mach-O a `__DATA,__mod_init_func` pointer via rsasm's
/// `.mod_init_func` shorthand, and COFF a pointer in `.CRT$XCU` — the
/// band MSVC's CRT calls via `__initterm(__xc_a, __xc_z)` during init,
/// where `__attribute__((constructor))` lands too. wasm has no ctors —
/// `None` there. `__pliron_prof_register` is declared as a normal import
/// by the caller so the splice resolves it (Mach-O included, where it
/// links as `___pliron_prof_register`).
// r[impl native.pgo.ctor]
pub fn ctor_asm(hdr: &str, ctor: &str, aarch64: bool, fmt: ObjFmt) -> Option<String> {
    // The ctor's section lands after other text whose size need not be a
    // multiple of the instruction size (a function may end in literal
    // bytes): an A64 instruction at an odd address faults.
    let align = if aarch64 { ".balign 4\n" } else { "" };
    let asm = match fmt {
        ObjFmt::Elf => {
            let body = if aarch64 {
                format!("adrp x0, {hdr}\n  add x0, x0, :lo12:{hdr}\n  b __pliron_prof_register")
            } else {
                format!("lea {hdr}(%rip), %rdi\n  jmp __pliron_prof_register")
            };
            let slot = if aarch64 { ".xword" } else { ".quad" };
            format!(
                ".att_syntax\n.text\n.weak {ctor}\n.hidden {ctor}\n{align}{ctor}:\n  {body}\n.section .init_array,\"aw\",@init_array\n.balign 8\n{slot} {ctor}\n.text\n"
            )
        }
        ObjFmt::MachO => {
            // `.weak_definition` + `.private_extern` mirrors the ELF
            // `.weak`/`.hidden` pair under Mach-O's symbol model.
            let (att, body) = if aarch64 {
                (
                    "",
                    format!(
                        "adrp x0, {hdr}@PAGE\n  add x0, x0, {hdr}@PAGEOFF\n  b __pliron_prof_register"
                    ),
                )
            } else {
                (
                    ".att_syntax\n",
                    format!("lea {hdr}(%rip), %rdi\n  jmp __pliron_prof_register"),
                )
            };
            format!(
                "{att}.section __TEXT,__text,regular,pure_instructions\n.globl {ctor}\n.weak_definition {ctor}\n.private_extern {ctor}\n{align}{ctor}:\n  {body}\n.mod_init_func\n.balign 8\n.quad {ctor}\n.text\n"
            )
        }
        ObjFmt::Coff => {
            // COFF has no real weak symbols; the ctor name is already
            // per-CGU unique, and the `.CRT$XCU` slot (not the symbol)
            // is what the CRT walks. A local suffices — LLVM's own
            // ctor functions are `internal` there.
            let (att, body) = if aarch64 {
                (
                    "",
                    format!(
                        "adrp x0, {hdr}\n  add x0, x0, :lo12:{hdr}\n  b __pliron_prof_register"
                    ),
                )
            } else {
                (
                    ".att_syntax\n",
                    format!("lea {hdr}(%rip), %rdi\n  jmp __pliron_prof_register"),
                )
            };
            format!(
                "{att}.text\n{align}{ctor}:\n  {body}\n.section .CRT$XCU,\"dr\"\n.balign 8\n.quad {ctor}\n.text\n"
            )
        }
        ObjFmt::Wasm => return None,
    };
    Some(asm)
}

// ---------------------------------------------------------------------------
// `.prof` file IO (shared layout with tools/pliron-prof)
// ---------------------------------------------------------------------------

fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn rd_u32(b: &[u8], p: &mut usize) -> Option<u32> {
    let v = u32::from_le_bytes(b.get(*p..*p + 4)?.try_into().unwrap());
    *p += 4;
    Some(v)
}
fn rd_u64(b: &[u8], p: &mut usize) -> Option<u64> {
    let v = u64::from_le_bytes(b.get(*p..*p + 8)?.try_into().unwrap());
    *p += 8;
    Some(v)
}

/// Serialize the (merged) profile map: `FxHashMap<(sym_hash, fp), ProfFn>`.
pub fn serialize(m: &FxHashMap<(u64, u64), ProfFn>) -> Vec<u8> {
    let mut out = Vec::new();
    put_u64(&mut out, MAGIC);
    put_u64(&mut out, m.len() as u64);
    for (&(sh, _), f) in m {
        put_u64(&mut out, sh);
        put_u64(&mut out, f.fingerprint);
        put_u32(&mut out, f.blocks.len() as u32);
        put_u32(&mut out, f.edges.len() as u32);
        for &c in &f.blocks {
            put_u64(&mut out, c);
        }
        for &(_, _, c) in &f.edges {
            put_u64(&mut out, c);
        }
        for &(p, s, _) in &f.edges {
            put_u32(&mut out, ((p as u32) << 16) | s as u32);
        }
    }
    // Trailer: indirect-call sites, for the functions that have any.
    let with: Vec<_> = m.iter().filter(|(_, f)| !f.sites.is_empty()).collect();
    if !with.is_empty() {
        put_u64(&mut out, ICALL_MAGIC);
        put_u64(&mut out, with.len() as u64);
        for (&(sh, _), f) in with {
            put_u64(&mut out, sh);
            put_u64(&mut out, f.fingerprint);
            put_u32(&mut out, f.sites.len() as u32);
            for site in &f.sites {
                put_u64(&mut out, site.other);
                put_u32(&mut out, site.targets.len() as u32);
                for t in &site.targets {
                    put_u64(&mut out, t.count);
                    put_u64(&mut out, t.crate_id);
                    put_u64(&mut out, t.crate_hash);
                    put_u64(&mut out, t.cgu);
                    put_u32(&mut out, t.flags);
                    put_u32(&mut out, t.name.len() as u32);
                    out.extend_from_slice(t.name.as_bytes());
                }
            }
        }
    }
    out
}

/// The `.prof` trailer (see `serialize`), merged into `m`. Absent or
/// truncated trailers leave the functions without sites.
fn parse_icalls(b: &[u8], p: &mut usize, m: &mut FxHashMap<(u64, u64), ProfFn>) -> Option<()> {
    if rd_u64(b, p)? != ICALL_MAGIC {
        return None;
    }
    let n = rd_u64(b, p)?;
    for _ in 0..n {
        let sh = rd_u64(b, p)?;
        let fp = rd_u64(b, p)?;
        let ns = rd_u32(b, p)? as usize;
        let mut sites = Vec::with_capacity(ns.min(1 << 16));
        for _ in 0..ns {
            let other = rd_u64(b, p)?;
            let nt = rd_u32(b, p)? as usize;
            let mut targets = Vec::with_capacity(nt.min(64));
            for _ in 0..nt {
                let count = rd_u64(b, p)?;
                let crate_id = rd_u64(b, p)?;
                let crate_hash = rd_u64(b, p)?;
                let cgu = rd_u64(b, p)?;
                let flags = rd_u32(b, p)?;
                let len = rd_u32(b, p)? as usize;
                let name = String::from_utf8(b.get(*p..*p + len)?.to_vec()).ok()?;
                *p += len;
                targets.push(ProfTarget { name, crate_id, crate_hash, cgu, flags, count });
            }
            sites.push(ProfSite { other, targets });
        }
        let f = m.entry((sh, fp)).or_default();
        f.fingerprint = fp;
        if f.sites.len() < sites.len() {
            f.sites.resize(sites.len(), ProfSite::default());
        }
        for (i, s) in sites.iter().enumerate() {
            f.sites[i].merge(s);
        }
    }
    Some(())
}

/// Parse `.prof` bytes into `{(sym_hash, fp) -> ProfFn}`, merging repeated
/// records (counts add).
pub fn parse(b: &[u8]) -> Option<FxHashMap<(u64, u64), ProfFn>> {
    let mut p = 0usize;
    if rd_u64(b, &mut p)? != MAGIC {
        return None;
    }
    let n = rd_u64(b, &mut p)?;
    let mut m: FxHashMap<(u64, u64), ProfFn> = FxHashMap::default();
    for _ in 0..n {
        let sh = rd_u64(b, &mut p)?;
        let fp = rd_u64(b, &mut p)?;
        let nb = rd_u32(b, &mut p)? as usize;
        let ne = rd_u32(b, &mut p)? as usize;
        let mut blocks = Vec::with_capacity(nb);
        for _ in 0..nb {
            blocks.push(rd_u64(b, &mut p)?);
        }
        let mut edges = Vec::with_capacity(ne);
        let mut counts = Vec::with_capacity(ne);
        for _ in 0..ne {
            counts.push(rd_u64(b, &mut p)?);
        }
        for c in counts {
            let e = rd_u32(b, &mut p)?;
            edges.push(((e >> 16) as u16, (e & 0xffff) as u16, c));
        }
        let f = m.entry((sh, fp)).or_default();
        f.fingerprint = fp;
        if f.blocks.len() < nb {
            f.blocks.resize(nb, 0);
        }
        for (i, &c) in blocks.iter().enumerate() {
            f.blocks[i] += c;
        }
        for &(pe, se, c) in &edges {
            if let Some(x) = f.edges.iter_mut().find(|x| x.0 == pe && x.1 == se) {
                x.2 += c;
            } else {
                f.edges.push((pe, se, c));
            }
        }
    }
    let _ = parse_icalls(b, &mut p, &mut m);
    Some(m)
}

// ---------------------------------------------------------------------------
// load(): profile -> State
// ---------------------------------------------------------------------------

/// What a consumer outside the IR passes needs from the profile: the
/// hot count and each symbol's entry count (the largest, when a symbol
/// has several fingerprints).
pub struct Summary {
    pub hot: u64,
    pub entry: FxHashMap<u64, u64>,
    /// Every symbol some indirect-call site of the profile reached, with
    /// the symbol hashes of the functions holding those sites.
    pub icall_targets: FxHashMap<String, Vec<u64>>,
}

/// The summary of `PLIRON_PGO_USE`'s profile, read once per process.
pub fn summary() -> Option<&'static Summary> {
    static S: std::sync::OnceLock<Option<Summary>> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let path = crate::env_get("PLIRON_PGO_USE")?;
        let profiles = parse(&std::fs::read(path).ok()?)?;
        let mut counts: Vec<u64> =
            profiles.iter().flat_map(|(_, f)| f.blocks.iter().copied()).filter(|&c| c > 0).collect();
        counts.sort_unstable_by(|a, b| b.cmp(a));
        let total: u128 = counts.iter().map(|&c| c as u128).sum();
        let (mut acc, mut hot) = (0u128, u64::MAX);
        for &c in &counts {
            acc += c as u128;
            if acc * 100 >= total * 99 {
                hot = c.max(2);
                break;
            }
        }
        let mut entry: FxHashMap<u64, u64> = FxHashMap::default();
        for ((sh, _), f) in &profiles {
            let e = entry.entry(*sh).or_default();
            *e = (*e).max(f.blocks.first().copied().unwrap_or(0));
        }
        let mut icall_targets: FxHashMap<String, Vec<u64>> = FxHashMap::default();
        for ((caller, _), f) in &profiles {
            for t in f.sites.iter().flat_map(|s| s.targets.iter()) {
                let e = icall_targets.entry(t.name.clone()).or_default();
                if !e.contains(caller) {
                    e.push(*caller);
                }
            }
        }
        Some(Summary { hot, entry, icall_targets })
    })
    .as_ref()
}

/// Whether the profile saw `sym` entered at least as often as its hot count.
pub fn hot_symbol(sym: &str) -> bool {
    summary().is_some_and(|s| s.entry.get(&sym_hash(sym)).is_some_and(|&c| c >= s.hot))
}

/// Read a `.prof` file and populate `st.prof`, `st.prof_bidx`,
/// `st.prof_edges`. Seeds `st.expect` on strongly biased cond_brs
/// (>=4:1 with >=8 samples). Returns the number of matched functions.
pub fn load(ctx: &Context, st: &mut State, path: &str) -> usize {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("pliron-pgo: cannot read {path}: {e}");
            return 0;
        }
    };
    let Some(profiles) = parse(&data) else {
        eprintln!("pliron-pgo: {path}: bad profile (magic)");
        return 0;
    };
    // The hot count, over the whole profile (not just this CGU's
    // functions): walk block counts from the largest down until 99% of
    // the total is covered.
    {
        let mut counts: Vec<u64> = profiles
            .iter()
            .flat_map(|(_, f)| f.blocks.iter().copied())
            .filter(|&c| c > 0)
            .collect();
        counts.sort_unstable_by(|a, b| b.cmp(a));
        let total: u128 = counts.iter().map(|&c| c as u128).sum();
        let mut acc: u128 = 0;
        st.prof_hot = None;
        for &c in &counts {
            acc += c as u128;
            if acc * 100 >= total * 99 {
                st.prof_hot = Some(c.max(2));
                break;
            }
        }
    }
    // Group by sym hash: several fingerprints per symbol may exist.
    let mut by_sym: FxHashMap<u64, Vec<(u64, ProfFn)>> = FxHashMap::default();
    for ((sh, fp), f) in profiles {
        by_sym.entry(sh).or_default().push((fp, f));
    }
    let mut matched = 0;
    let mut stale = 0;
    for (sym, f) in &st.funcs {
        if !has_body(ctx, f.op) {
            continue;
        }
        let sh = sym_hash(sym);
        let Some(cands) = by_sym.get(&sh) else {
            continue;
        };
        let (fp, _) = fingerprint(ctx, f.op);
        let Some(pf) = cands.iter().find(|(p, _)| *p == fp).map(|(_, p)| p) else {
            stale += 1;
            continue;
        };
        matched += 1;
        let bs = blocks(ctx, f.op);
        for (i, b) in bs.iter().enumerate() {
            if let Some(&c) = pf.blocks.get(i) {
                st.prof_bcount.insert(*b, c);
            }
        }
        st.prof.insert(f.op, pf.clone());
        // Edge counts per terminator's successor slot.
        for (i, b) in bs.iter().enumerate() {
            let Some(t) = terminator(ctx, *b) else {
                continue;
            };
            let ss = succs(ctx, t);
            if ss.is_empty() {
                continue;
            }
            let mut counts: Vec<Option<u64>> = vec![None; ss.len()];
            for si in 0..ss.len() {
                if ss.len() == 1 {
                    counts[si] = pf.blocks.get(i).copied();
                    continue;
                }
                if let Some(&(_, _, c)) = pf
                    .edges
                    .iter()
                    .find(|&&(p, s, _)| p as usize == i && s as usize == si)
                {
                    counts[si] = Some(c);
                } else {
                    // Uninstrumented multi-succ edge: the target had a
                    // single pred, so its block count is the edge count.
                    let s = ss[si];
                    let bidx = bs.iter().position(|x| *x == s);
                    if let Some(j) = bidx {
                        counts[si] = pf.blocks.get(j).copied();
                    }
                }
            }
            if counts.iter().all(|c| c.is_some()) {
                let counts: Vec<u64> = counts.into_iter().flatten().collect();
                // Seed expect on strongly biased cond_brs.
                if counts.len() == 2 && Operation::is_op::<CondBrOp>(t, ctx) {
                    let (a, c) = (counts[0], counts[1]);
                    let tot = a + c;
                    if tot >= 8 && (a >= 4 * c || c >= 4 * a) {
                        // `PLIRON_PGO_EXPECT=0`: ablation, no branch
                        // expectations from the profile.
                        if !crate::env_is("PLIRON_PGO_EXPECT", "0") {
                            st.expect.insert(t, a >= c);
                        }
                    }
                }
                st.prof_edges.insert(t, counts);
            }
        }
    }
    if pliron_cfg::tracing_on() {
        tracing::info!(target: "pliron::pgo", "pliron-pgo: {path}: {matched} functions matched, {stale} stale")
    };
    matched
}

/// Profiled entry count of the block, if a matching record exists.
pub fn block_count(st: &State, b: Ptr<BasicBlock>) -> Option<u64> {
    st.prof_bcount.get(b).copied()
}

/// Total entry count (block 0) of the FuncOp, if profiled.
pub fn func_count(st: &State, f: Ptr<Operation>) -> Option<u64> {
    st.prof.get(f)?.blocks.first().copied()
}
