//! Loop-invariant code motion on CLIF (LLVM LICM). Hoists speculatable
//! instructions and `notrap` loads the loop cannot clobber into the
//! preheader, and sinks stores of an invariant value to an invariant address
//! out to the loop's exit edges. Load/store reasoning reuses `loadfwd`'s
//! root-based locations and `isolated` roots (stack slots, non-escaping
//! `noalias` params); calls other than `nowrite` ones stop sinking. Register
//! promotion (`PromoteMem2Reg` in the loop) promotes a load+stored isolated
//! location to a block-param version threaded through the body: loaded in
//! the preheader, stores rewritten into the version, stored back on exits.
//! `PLIRON_LICM=0` disables it; `PLIRON_LICM_DEBUG` logs moves.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, FuncRef, Function, Inst, InstBuilder, InstructionData,
    MemFlagsData, Opcode, Type, Value, ValueDef, types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use crate::vmap::VMap;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::loadfwd::{self, Loc, Root, in_region};

const MAX_LOOPS: usize = 32;
const MAX_MOVES: usize = 96;

fn def_block(func: &Function, v: Value) -> Option<Block> {
    match func.dfg.resolved_value_def(v) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        _ => None,
    }
}

/// Speculatable: executing it unconditionally can't trap, touch memory, or
/// branch. Constants and frame addresses are free everywhere, so hoisting
/// them is pure churn — skipped.
pub(crate) fn speculatable(func: &Function, i: Inst) -> bool {
    let op = func.dfg.insts[i].opcode();
    if func.dfg.inst_results(i).is_empty()
        || op.is_terminator()
        || op.can_trap()
        || op.can_load()
        || op.can_store()
        || op.is_call()
        || op.other_side_effects()
    {
        return false;
    }
    !matches!(
        op,
        Opcode::Iconst
            | Opcode::F32const
            | Opcode::F64const
            | Opcode::Vconst
            | Opcode::StackAddr
            | Opcode::GetStackPointer
            | Opcode::FuncAddr
            | Opcode::SymbolValue
    )
}

/// Whether a write-ish instruction `w` can touch `loc`.
fn clobbers(
    func: &Function,
    ps: &VMap<Value>,
    na: &FxHashSet<Value>,
    w: Inst,
    loc: Loc,
    iso: &FxHashSet<Root>,
    nw: &FxHashMap<FuncRef, bool>,
    deref: &FxHashMap<Value, (u64, bool)>,
) -> bool {
    // A `noalias`+`readonly` (`deref` with `!writable`) pointee is
    // exclusively read through this root for the call's duration —
    // no store or call in the loop may write it (writing through the
    // root is UB; writing through any other pointer violates
    // `noalias`). Nothing can clobber the loc.
    if let Root::V(v) = loc.0
        && deref.get(&v).is_some_and(|&(_, writable)| !writable)
    {
        return false;
    }
    let op = func.dfg.insts[w].opcode();
    if op.is_call() {
        return !loadfwd::write_free(func, w, nw, false) && !iso.contains(&loc.0);
    }
    match func.dfg.insts[w] {
        // Every `store`'s address resolves through `root`, flag or not: an
        // untracked store to an isolated root is impossible only because no
        // use of such an address escapes analysis — and this store IS a use.
        InstructionData::Store { args, offset, .. } => {
            let ty = func.dfg.value_type(func.dfg.resolve_aliases(args[0]));
            let (r2, o2) = loadfwd::norm_root(func, ps, na, args[1]);
            let o2 = o2.wrapping_add(i64::from(i32::from(offset)));
            !overlap_keeps((r2, o2, ty), loc, iso)
        }
        _ if op.can_store()
            || op.other_side_effects()
            || matches!(op, Opcode::AtomicLoad | Opcode::Fence) =>
        {
            !iso.contains(&loc.0)
        }
        _ => false,
    }
}

/// Whether a store of `w = (r2,o2,ty2)` provably leaves `loc` intact; mirrors
/// `loadfwd::kill`'s keep-rule.
fn overlap_keeps(
    w: (Root, i64, cranelift_codegen::ir::Type),
    loc: Loc,
    iso: &FxHashSet<Root>,
) -> bool {
    let (r, o, ty) = loc;
    let (r2, o2, ty2) = w;
    match (r, r2) {
        _ if r == r2 => o >= o2 + i64::from(ty2.bytes()) || o2 >= o + i64::from(ty.bytes()),
        (Root::S(_), Root::S(_)) => true,
        _ => iso.contains(&r) || iso.contains(&r2),
    }
}

/// Redirect `pinst`'s `slot`th branch destination (which must go to `dst`)
/// through a fresh block carrying the edge's arguments. Returns `nb`, or
/// `None` when the edge carries `try_call`-only `ret`/`exn` arguments that a
/// plain `jump` cannot take.
fn split_edge(func: &mut Function, pinst: Inst, slot: usize) -> Option<Block> {
    let dst;
    let args: Vec<BlockArg>;
    {
        let bc = func.dfg.insts[pinst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[slot];
        dst = bc.block(&func.dfg.value_lists);
        args = bc.args(&func.dfg.value_lists).collect();
    }
    if args.iter().any(|a| !matches!(a, BlockArg::Value(_))) {
        return None;
    }
    let nb = func.dfg.make_block();
    let pb = func.layout.inst_block(pinst).unwrap();
    func.layout.insert_block_after(nb, pb);
    FuncCursor::new(func).at_bottom(nb).ins().jump(dst, &args);
    let dfg = &mut func.dfg;
    let bc = &mut dfg.insts[pinst]
        .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[slot];
    *bc = BlockCall::new(nb, core::iter::empty(), &mut dfg.value_lists);
    Some(nb)
}

/// Insts the loop may move out: speculatable or a `notrap` load no in-loop
/// write can clobber.
type Region = pliron::context::Ptr<pliron::basic_block::BasicBlock>;

#[allow(clippy::too_many_arguments)]
fn hoists(
    func: &Function,
    ps: &VMap<Value>,
    na: &FxHashSet<Value>,
    inv: &dyn Fn(&Function, Value) -> bool,
    writers: &[Inst],
    iso: &FxHashSet<Root>,
    nw: &FxHashMap<FuncRef, bool>,
    deref: &FxHashMap<Value, (u64, bool)>,
    scoped: &FxHashMap<Inst, Region>,
    breg: &FxHashMap<Block, Region>,
    rpar: &pliron::cols::ColMap<Region, Region>,
    ofz: &FxHashMap<Value, Vec<(u64, Region)>>,
    pb: Block,
    i: Inst,
) -> bool {
    let dbg = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::licm", tracing::Level::DEBUG)
        && matches!(func.dfg.insts[i], InstructionData::Load { .. });
    if dbg && let Some(&reg) = scoped.get(&i) {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::licm", "licm? {}: scoped region {reg:?} breg={:?}",
                func.dfg.display_inst(i),
                breg.get(&pb)
            )
        };
    }
    if !func.dfg.inst_args(i).iter().all(|&a| inv(func, a)) {
        if dbg {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::licm", "licm? {}: arg not invariant", func.dfg.display_inst(i))
            };
        }
        return false;
    }
    if speculatable(func, i) {
        return true;
    }
    let InstructionData::Load {
        opcode: Opcode::Load,
        arg,
        offset,
        ..
    } = func.dfg.insts[i]
    else {
        return false;
    };
    if !loadfwd::notrap(func, i) {
        if dbg {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::licm", "licm? {}: trapping", func.dfg.display_inst(i))
            };
        }
        return false;
    }
    let ty = func.dfg.value_type(func.dfg.first_result(i));
    let (r, o) = loadfwd::root(func, ps, na, arg);
    if dbg {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::licm", "licm? {}: root {:?} off {o} iso={} deref={:?}",
                func.dfg.display_inst(i),
                r,
                iso.contains(&r),
                match r {
                    Root::V(v) => deref.get(&v).copied(),
                    _ => None,
                }
            )
        };
    }
    // The clobber scan and the `safe` check below want the concrete object
    // (`norm_root` peels a scope name to its stack slot), while `r` stays
    // the scope name — the `ofz` region check needs it.
    let (nr, no) = loadfwd::norm_root(func, ps, na, arg);
    let loc: Loc = (nr, no.wrapping_add(i64::from(i32::from(offset))), ty);
    // `notrap` only asserts the load can't trap where it sits — invalid
    // addresses are UB, so removal/reorder is fine — but hoisting speculates
    // it onto paths where the address may be garbage (a pointer loaded from
    // an enum payload is only valid on that variant's arm). Require the
    // root dereferenceable unconditionally: a stack slot, a symbol or stack
    // base, or a param rustc marked `dereferenceable` covering the access.
    let safe = match loc.0 {
        // `ss` is unconditionally dereferenceable only within its own
        // bounds — `ss + wild_offset` (a `notrap` load sits on a path
        // that never runs, so its address arithmetic can point outside
        // the frame entirely) is a speculative fault, not a stack
        // access. Seed 41: `load.i128` at `ss84 + 0x2518_0084_2820_8000`.
        Root::S(ss) => {
            loc.1 >= 0
                && loc
                    .1
                    .checked_add(i64::from(ty.bytes()))
                    .is_some_and(|end| end <= i64::from(func.sized_stack_slots[ss].size))
        }
        Root::V(v) => {
            // `loc.1`, not `o` — the peel offset alone ignores the
            // inst's `offset` imm, so `load p+K` would check `[0,
            // bytes)` while reading `[K, K+bytes)`: a `notrap` load on
            // a never-taken path can sit past the `dereferenceable`
            // bound and hoisting it speculates that read (same hazard
            // as the `Root::S` wild offset).
            let proven = deref.get(&v).is_some_and(|&(bytes, _)| {
                loc.1 >= 0
                    && loc
                        .1
                        .checked_add(i64::from(ty.bytes()))
                        .is_some_and(|end| end <= bytes as i64)
            });
            proven
                || func.dfg.value_def(v).inst().is_some_and(|d| {
                    matches!(
                        func.dfg.insts[d].opcode(),
                        Opcode::SymbolValue | Opcode::FuncAddr | Opcode::GetStackPointer
                    )
                })
        }
    };
    // A load tagged region-frozen (`inline::frozen_args`) reads a `noalias
    // readonly` pointee for its inlined clone's duration — no writer inside
    // the region can clobber it, so the clobber scan is unnecessary when the
    // hoist stays in-region: the preheader is `pb`, or a fresh block split
    // on the `pb`→`h` edge (dominated by `pb`, hence in `pb`'s region).
    // Regions nest — `pb` may sit in a deeper clone than the tagged region;
    // walk `region_parent` to check containment.
    if let Some(&target) = scoped.get(&i) {
        let mut r = breg.get(&pb).copied();
        let mut chain = Vec::new();
        while let Some(x) = r {
            chain.push(x);
            if x == target {
                if dbg {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::licm", "licm?   scoped frozen, in-region hoist")
                    };
                }
                // The tag is the dereferenceability proof: `frozen_args`
                // only tags accesses bounded by the param's
                // `dereferenceable` bytes, and a `&T` pointee is live for
                // the whole clone — the hoist stays inside it.
                return true;
            }
            r = rpar.get(x).copied().filter(|&n| n != x);
        }
        if dbg {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::licm", "licm?   scoped miss chain={chain:?} target={target:?}")
            };
        }
    }
    // `region_frozen_op` records (operand-frozen roots) carry the same
    // guarantee as a scoped tag: `*root` is unwritten for the record
    // region's whole span. They reach where `frozen_loads` can't — the
    // tag is bound to the clone that scanned the load, but sibling clones
    // bind the same operand (partition's two `lt` calls), and CFG merging
    // can land the load in a block whose region is the sibling's. As long
    // as the load's block and the hoist target both sit inside the
    // record's region, the hoist can't observe a write.
    if let Root::V(rv) = r
        && let Some(rs) = ofz.get(&rv)
        && let (Some(&pbreg), Some(&breg_)) = (
            breg.get(&pb),
            func.layout.inst_block(i).and_then(|b| breg.get(&b)),
        )
    {
        let off = o.wrapping_add(i64::from(i32::from(offset)));
        for &(bytes, reg) in rs {
            if off >= 0
                && off + i64::from(ty.bytes()) <= bytes as i64
                && in_region(pbreg, reg, rpar)
                && in_region(breg_, reg, rpar)
            {
                if dbg {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::licm", "licm?   operand-frozen, in-region hoist")
                    };
                }
                return true;
            }
        }
    }
    let nw_w = writers
        .iter()
        .filter(|&&w| clobbers(func, ps, na, w, loc, iso, nw, deref))
        .count();
    if dbg {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::licm", "licm?   safe={safe} writers_clobbering={nw_w}")
        };
        for &w in writers {
            if let InstructionData::Store { args, offset, .. } = func.dfg.insts[w]
                && clobbers(func, ps, na, w, loc, iso, nw, deref)
            {
                let (r2, o2) = loadfwd::root(func, ps, na, args[1]);
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::licm", "licm?     clob {} root {r2:?} off {}",
                        func.dfg.display_inst(w),
                        o2.wrapping_add(i64::from(i32::from(offset)))
                    )
                };
            }
        }
    }
    safe && nw_w == 0
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    func: &mut Function,
    ps: &VMap<Value>,
    na: &FxHashSet<Value>,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    iso: &FxHashSet<Root>,
    escapes: &FxHashMap<Root, Vec<Block>>,
    deref: &FxHashMap<Value, (u64, bool)>,
    nw: &FxHashMap<FuncRef, bool>,
    debug: bool,
    scoped: &FxHashMap<Inst, Region>,
    breg: &FxHashMap<Block, Region>,
    rpar: &pliron::cols::ColMap<Region, Region>,
    ofz: &FxHashMap<Value, Vec<(u64, Region)>>,
) -> usize {
    let h = la.loop_header(lp);
    // Exactly one branch destination in from outside.
    let mut entry: Option<(Inst, usize)> = None;
    for p in cfg.pred_iter(h) {
        if la.is_in_loop(p.block, lp) {
            continue;
        }
        for (slot, bc) in func.dfg.insts[p.inst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            if bc.block(&func.dfg.value_lists) != h {
                continue;
            }
            if entry.is_some() {
                if debug {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::licm", "licm: skip {h:?} of {}: multi-entry", func.name)
                    };
                }
                return 0;
            }
            entry = Some((p.inst, slot));
        }
    }
    let Some((pinst, slot)) = entry else {
        if debug {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::licm", "licm: skip {h:?} of {}: no entry", func.name)
            };
        }
        return 0;
    };
    let body: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, lp))
        .collect();
    // Moving an inst to just before `pinst` is sound iff every operand
    // dominates `pinst` and `pinst`'s block dominates every loop block (so
    // hoisted results still dominate their uses). `pinst`'s block must
    // dominate the header (it does when the single edge found above is the
    // only external entry — but cfg is stale across loops after edge splits,
    // so check rather than assume) and the header must dominate the whole
    // body — not a given: LoopAnalysis admits irreducible bodies with
    // external entries bypassing it.
    let pb = func.layout.inst_block(pinst).unwrap();
    if !dt.try_dominates(pb, h, &func.layout) || !body.iter().all(|&b| dt.try_dominates(h, b, &func.layout))
    {
        if debug {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::licm", "licm: skip {h:?} of {}: dom", func.name)
            };
        }
        return 0;
    }
    let inv = |func: &Function, v: Value| {
        // A loop-carried param that merely forwards a preheader value is
        // that value (`param_src`) — `partition`'s pivot is the case that
        // matters: the pointer is constant even though the SSA name isn't.
        let v = loadfwd::psrc(ps, na, func, v);
        if body.contains(&def_block(func, v).unwrap_or(h)) {
            return false;
        }
        match func.dfg.value_def(v) {
            ValueDef::Result(i, _) => dt.try_dominates(i, pinst, &func.layout),
            ValueDef::Param(b, _) => dt.try_dominates(b, pinst, &func.layout),
            _ => false,
        }
    };
    // The loop's memory-writing instructions, for load-hoist checks.
    let writers: Vec<Inst> = body
        .iter()
        .flat_map(|&b| func.layout.block_insts(b))
        .filter(|&i| {
            let op = func.dfg.insts[i].opcode();
            op.is_call()
                || op.can_store()
                || op.other_side_effects()
                || matches!(op, Opcode::AtomicLoad | Opcode::Fence)
        })
        .collect();

    // Insertion point: in `pblock` before its terminator when it ends in a
    // plain `jump h`; otherwise a fresh preheader block on the entry edge.
    let mut n = 0;
    let mut anchor: Option<Inst> = None; // insert before this inst
    if let InstructionData::Jump { .. } = func.dfg.insts[pinst] {
        anchor = Some(pinst);
    }
    let mut ph_inst: Option<Inst> = None; // nb's jump terminator

    // Constants are pure and rematerializable; leaving one in the body pins
    // its dependents there via the `inv` operand check below. They hoist
    // unconditionally (no `MAX_MOVES` share): an order-dependent cap left a
    // loop's real constants (e.g. SipHash's four magics) unhoisted behind
    // 96 junk iconsts.
    let consts: Vec<Inst> = body
        .iter()
        .flat_map(|&b| func.layout.block_insts(b))
        .filter(|&i| {
            matches!(
                func.dfg.insts[i].opcode(),
                Opcode::Iconst | Opcode::F32const | Opcode::F64const | Opcode::Vconst
            )
        })
        .collect();
    let mut cn = 0;
    for i in consts {
        if anchor.is_none() && ph_inst.is_none() {
            // No plain-jump entry edge to hoist to; `try_call`
            // edges can't take a normal trampoline.
            let Some(nb) = split_edge(func, pinst, slot) else {
                break;
            };
            ph_inst = func.layout.last_inst(nb);
        }
        if debug {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::licm", "licm: hoist {} in {:?} of {}",
                    func.dfg.display_inst(i),
                    h,
                    func.name
                )
            };
        }
        func.layout.remove_inst(i);
        func.layout.insert_inst(i, anchor.or(ph_inst).unwrap());
        cn += 1;
    }

    // Two rounds: results of moved instructions are themselves outside the
    // loop, so a second scan exposes dependents (e.g. `a+b` after `a`).
    for _ in 0..2 {
        let mut moved = 0;
        for &b in &body {
            let insts: Vec<Inst> = func.layout.block_insts(b).collect();
            for i in insts {
                if n >= MAX_MOVES {
                    break;
                }
                if hoists(
                    func, ps, na, &inv, &writers, iso, nw, deref, scoped, breg, rpar, ofz, pb, i,
                ) {
                    if debug {
                        if pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::licm", "licm: hoist {} in {:?} of {}",
                                func.dfg.display_inst(i),
                                h,
                                func.name
                            )
                        };
                    }
                    if anchor.is_none() && ph_inst.is_none() {
                        // No plain-jump entry edge to hoist to; `try_call`
                        // edges can't take a normal trampoline.
                        let Some(nb) = split_edge(func, pinst, slot) else {
                            continue;
                        };
                        ph_inst = func.layout.last_inst(nb);
                    }
                    // `inv` judged operands by their `psrc` resolution: a
                    // loop param every edge fills with the same preheader
                    // value counts as that value — but the param itself
                    // does not dominate `pinst`. Move with the resolved
                    // operand, which `inv` proved does.
                    let args: Vec<Value> = func
                        .dfg
                        .inst_args(i)
                        .iter()
                        .map(|&a| loadfwd::psrc(ps, na, func, a))
                        .collect();
                    for (a, r) in func.dfg.inst_args_mut(i).iter_mut().zip(args) {
                        *a = r;
                    }
                    func.layout.remove_inst(i);
                    func.layout.insert_inst(i, anchor.or(ph_inst).unwrap());
                    n += 1;
                    moved += 1;
                }
            }
        }
        if moved == 0 {
            break;
        }
    }

    // Store sinking: exactly one store, invariant address and value, no calls,
    // all loads provably disjoint, and the store dominates every exit edge.
    let stores: Vec<Inst> = body
        .iter()
        .flat_map(|&b| func.layout.block_insts(b))
        .filter(|&i| func.dfg.insts[i].opcode().can_store())
        .collect();
    if stores.len() == 1 {
        let s = stores[0];
        let InstructionData::Store { args, offset, .. } = func.dfg.insts[s] else {
            return n + cn;
        };
        let (val, addr) = (func.dfg.resolve_aliases(args[0]), args[1]);
        let store_ty = func.dfg.value_type(val);
        let (sr, so) = loadfwd::norm_root(func, ps, na, addr);
        let sloc: Loc = (sr, so.wrapping_add(i64::from(i32::from(offset))), store_ty);
        let mut ok = loadfwd::notrap(func, s) && inv(func, val) && inv(func, addr);
        if ok {
            for &b in &body {
                for i in func.layout.block_insts(b) {
                    if i == s {
                        continue;
                    }
                    let op = func.dfg.insts[i].opcode();
                    if op.is_call() || op.other_side_effects() {
                        ok = false;
                        break;
                    }
                    if op.is_terminator()
                        && func.dfg.insts[i]
                            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                            .is_empty()
                        && !matches!(op, Opcode::Trap)
                    {
                        // A `return`/`resume` exits the function without
                        // producing a branch exit edge — sinking would
                        // drop the write on that path.
                        ok = false;
                        break;
                    }
                    if let InstructionData::Load {
                        opcode,
                        arg,
                        offset,
                        ..
                    } = func.dfg.insts[i]
                    {
                        // The load's byte extent, not its (possibly
                        // extended) result type.
                        let ty = match opcode {
                            Opcode::Load => func.dfg.value_type(func.dfg.first_result(i)),
                            Opcode::Uload8 | Opcode::Sload8 => types::I8,
                            Opcode::Uload16 | Opcode::Sload16 => types::I16,
                            Opcode::Uload32 | Opcode::Sload32 => types::I32,
                            _ => {
                                ok = false;
                                break;
                            }
                        };
                        if !loadfwd::notrap(func, i) {
                            ok = false;
                            break;
                        }
                        let (r, o) = loadfwd::norm_root(func, ps, na, arg);
                        let lloc = (r, o.wrapping_add(i64::from(i32::from(offset))), ty);
                        if !overlap_keeps(sloc, lloc, iso) {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    break;
                }
            }
        }
        if ok {
            // Exit edges out of the loop; the store must dominate each.
            let mut exits: Vec<(Inst, usize)> = Vec::new();
            for &b in &body {
                let Some(t) = func.layout.last_inst(b) else {
                    ok = false;
                    break;
                };
                for (slot, bc) in func.dfg.insts[t]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .iter()
                    .enumerate()
                {
                    if !body.contains(&bc.block(&func.dfg.value_lists)) {
                        exits.push((t, slot));
                    }
                }
            }
            if exits.is_empty()
                || !exits.iter().all(|&(t, _)| dt.try_dominates(s, t, &func.layout))
                || !exits.iter().all(|&(t, slot)| {
                    func.dfg.insts[t]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[slot]
                        .args(&func.dfg.value_lists)
                        .all(|a| matches!(a, BlockArg::Value(_)))
                })
            {
                ok = false;
            }
            if ok {
                for (t, slot) in exits {
                    // Every exit was checked splittable; a silent skip here
                    // would sink on only some exits and lose the write.
                    let nb = split_edge(func, t, slot).expect("exit checked splittable");
                    let ni = func.dfg.clone_inst(s);
                    // The cloned store's address/value are the invariant
                    // values already computed outside the loop.
                    let j = func.layout.last_inst(nb).unwrap();
                    func.layout.insert_inst(ni, j);
                    if debug {
                        if pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::licm", "licm: sink {} past {:?} of {}",
                                func.dfg.display_inst(s),
                                h,
                                func.name
                            )
                        };
                    }
                    n += 1;
                }
                func.layout.remove_inst(s);
            }
        }
    }
    if crate::env_get("PLIRON_LICM_PROMOTE") == Some("0") {
        return n + cn;
    }
    cn + n
        + promote(
            func, ps, na, cfg, dt, la, lp, iso, escapes, deref, nw, ofz, debug,
        )
}

/// `PromoteMem2Reg` for one loop: a loop-invariant isolated location the
/// loop stores to (and maybe loads) becomes a register. Candidates are
/// `(root, offset)` groups on isolated roots; every overlapping access must
/// be a plain `load`/`store` of one type at one offset.
fn promote(
    func: &mut Function,
    ps: &VMap<Value>,
    na: &FxHashSet<Value>,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    iso: &FxHashSet<Root>,
    escapes: &FxHashMap<Root, Vec<Block>>,
    deref: &FxHashMap<Value, (u64, bool)>,
    nw: &FxHashMap<FuncRef, bool>,
    ofz: &FxHashMap<Value, Vec<(u64, Region)>>,
    debug: bool,
) -> usize {
    let h = la.loop_header(lp);
    let body: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, lp))
        .collect();
    let mut n = 0;
    // A root rejected function-wide stays promotable here when every escape
    // is strictly post-loop: an escape block that can't reach the header
    // only runs after a loop exit, and `promote_loc` flushes the register
    // back on every exit edge before that. `reachers` is the set of blocks
    // that can reach `h` — the body plus every ancestor (including an
    // enclosing loop's other blocks, which correctly stay disqualifying).
    let mut iso_l = iso.clone();
    if !escapes.is_empty() {
        let mut reachers: FxHashSet<Block> = FxHashSet::default();
        let mut stack = vec![h];
        while let Some(b) = stack.pop() {
            for p in cfg.pred_iter(b) {
                if reachers.insert(p.block) {
                    stack.push(p.block);
                }
            }
        }
        reachers.insert(h);
        for (r, bs) in escapes {
            if bs.iter().all(|b| !reachers.contains(b)) {
                iso_l.insert(*r);
            }
        }
    }
    let iso_l = &iso_l;

    // Group in-loop accesses by (root, offset) -> (ty, saw_store, uniform,
    // byte extent). The extent is the widest access seen: a group mixing an
    // i8 store and an i64 load overlaps every byte the load covers, so its
    // neighbors must all be poisoned even though the group itself is
    // non-uniform and unpromotable.
    let mut groups: FxHashMap<(Root, i64), (Type, bool, bool, i64)> = FxHashMap::default();
    // Set when the loop contains an operand-frozen access that can't be
    // normalized to a concrete slot: `Root::V` groups could overlap it,
    // `Root::S` groups can't (see the `ofz` note in the scan below).
    let mut ofz_access = false;
    for &b in &body {
        for i in func.layout.block_insts(b) {
            // `(is_store, ty, width, addr)`: extending loads (`uload8` etc.)
            // and narrowing stores (`istore8` etc.) touch the same bytes but
            // can't be rewritten to version reads/writes, so they record a
            // non-`INVALID`-equatable ty that makes their group non-uniform
            // and unpromotable while still byte-poisoning neighbors.
            let (is_st, ty, w, addr) = match func.dfg.insts[i] {
                InstructionData::Load {
                    opcode,
                    arg,
                    offset,
                    ..
                } => {
                    let (ty, w) = match opcode {
                        Opcode::Load => {
                            let ty = func.dfg.value_type(func.dfg.first_result(i));
                            (ty, i64::from(ty.bytes()))
                        }
                        Opcode::Uload8 | Opcode::Sload8 => (types::INVALID, 1),
                        Opcode::Uload16 | Opcode::Sload16 => (types::INVALID, 2),
                        Opcode::Uload32 | Opcode::Sload32 => (types::INVALID, 4),
                        _ => return 0,
                    };
                    (false, ty, w, (arg, i32::from(offset)))
                }
                InstructionData::Store {
                    opcode,
                    args,
                    offset,
                    ..
                } => {
                    let (ty, w) = match opcode {
                        Opcode::Store => {
                            let ty = func.dfg.value_type(func.dfg.resolve_aliases(args[0]));
                            (ty, i64::from(ty.bytes()))
                        }
                        Opcode::Istore8 => (types::INVALID, 1),
                        Opcode::Istore16 => (types::INVALID, 2),
                        Opcode::Istore32 => (types::INVALID, 4),
                        _ => return 0,
                    };
                    (true, ty, w, (args[1], i32::from(offset)))
                }
                _ => {
                    let op = func.dfg.insts[i].opcode();
                    if op.can_load() || op.can_store() {
                        return 0;
                    }
                    continue;
                }
            };
            // `norm_root` unifies a scope-named pointer into a slot with
            // the slot's own accesses; a scope that doesn't resolve stays
            // `V` — may-alias as far as the group scan is concerned.
            let (r, o) = loadfwd::norm_root(func, ps, na, addr.0);
            let o = o.wrapping_add(i64::from(addr.1));
            if !iso_l.contains(&r) {
                // An operand-frozen access is never a candidate: the scope
                // is re-entered each iteration, so its reads must see the
                // previous iteration's stores, which promotion would hold
                // back in a register until the exit (aho-corasick's
                // `ByteClassElements::next` re-read a stale cursor and
                // looped forever). `noalias` also makes the pointee
                // exclusive during the region, so it cannot alias a
                // `Root::S` group — but it may shadow an opaque `V` loc, so
                // this loop promotes only S-keyed groups once one appears.
                if let Root::V(v) = r
                    && ofz.contains_key(&v)
                {
                    if debug {
                        if pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::licm", "licm: operand-frozen access {} in {h} — V-keyed promotion off",
                                func.dfg.display_inst(i)
                            )
                        };
                    }
                    ofz_access = true;
                }
                continue;
            }
            let g = groups.entry((r, o)).or_insert((ty, false, true, 0));
            g.2 &= g.0 == ty;
            g.1 |= is_st;
            g.3 = g.3.max(w);
        }
    }
    // An access that byte-overlaps a candidate at a different offset poisons
    // the whole root (partial writes would corrupt the register version).
    let ks: Vec<((Root, i64), (Type, bool, bool, i64))> =
        groups.iter().map(|(&k, &v)| (k, v)).collect();
    let mut bad: FxHashSet<Root> = FxHashSet::default();
    for i in 0..ks.len() {
        for j in 0..ks.len() {
            let ((r, o), (.., e)) = ks[i];
            let ((r2, o2), (.., e2)) = ks[j];
            if r == r2 && o != o2 && o < o2 + e2 && o2 < o + e {
                bad.insert(r);
            }
        }
    }
    for ((r, o), (ty, has_store, uniform, _)) in ks {
        if !uniform || !has_store || bad.contains(&r) || n >= MAX_MOVES {
            continue;
        }
        // An unresolved operand-frozen access may shadow a `V` loc;
        // stack slots are provably disjoint from the scope's pointee.
        if ofz_access && matches!(r, Root::V(_)) {
            continue;
        }
        if !ty.is_int() && !ty.is_float() {
            continue;
        }
        // Invariant address: the V root must be defined outside the loop.
        if let Root::V(v) = r
            && def_block(func, v).is_none_or(|b| body.contains(&b))
        {
            continue;
        }
        let Ok(off) = i32::try_from(o) else { continue };
        // Dereferenceable at entry: a stack slot only within its own
        // bounds (same wild-offset hazard as `hoists`); a `noalias`
        // root needs rustc's `dereferenceable`+writable info covering the
        // loc, or a `notrap` store to it in the header proving the write.
        if let Root::S(ss) = r
            && !(o >= 0
                && o.checked_add(i64::from(ty.bytes()))
                    .is_some_and(|end| end <= i64::from(func.sized_stack_slots[ss].size)))
        {
            continue;
        }
        if let Root::V(v) = r {
            let proven = deref.get(&v).is_some_and(|&(bytes, writable)| {
                writable
                    && o >= 0
                    && o.checked_add(i64::from(ty.bytes()))
                        .is_some_and(|end| end <= bytes as i64)
            });
            let ok = proven
                || func.layout.block_insts(h).any(|i| {
                    matches!(
                        func.dfg.insts[i],
                        InstructionData::Store { opcode: Opcode::Store, args, offset, .. }
                            if loadfwd::notrap(func, i)
                                && {
                                    let (r2, o2) =
                                        loadfwd::norm_root(func, ps, na, args[1]);
                                    (r2, o2.wrapping_add(i64::from(i32::from(offset)))) == (r, o)
                                }
                                && func.dfg.value_type(func.dfg.resolve_aliases(args[0])) == ty
                    )
                });
            if !ok {
                continue;
            }
        }
        n += promote_loc(
            func,
            ps,
            na,
            cfg,
            dt,
            la,
            lp,
            &body,
            h,
            (r, o, ty),
            off,
            iso_l,
            nw,
            deref,
            debug,
        );
    }
    n
}

/// Thread a `ty` version through `body` as loop SSA for `loc`: a new param
/// on the header and every merge point, a preheader load `v0`, loads
/// aliased to the current version, stores updating it, and a `store` of the
/// out-version on every exit edge (and before each in-loop `return`).
fn promote_loc(
    func: &mut Function,
    ps: &VMap<Value>,
    na: &FxHashSet<Value>,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    body: &FxHashSet<Block>,
    h: Block,
    loc: Loc,
    off: i32,
    iso: &FxHashSet<Root>,
    nw: &FxHashMap<FuncRef, bool>,
    deref: &FxHashMap<Value, (u64, bool)>,
    debug: bool,
) -> usize {
    let ty = loc.2;
    let _ = (cfg, la, lp);

    // Scan every edge touching `body` first: per-dest incoming list and
    // exit edges, plus `return`-like terminators needing a store-back.
    let mut in_edges: FxHashMap<Block, Vec<(Block, Inst, usize)>> = FxHashMap::default();
    let mut out_edges: Vec<(Inst, usize)> = Vec::new();
    let mut rets: Vec<Inst> = Vec::new();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let dests =
            func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
        if body.contains(&b) && dests.is_empty() {
            match func.dfg.insts[t].opcode() {
                Opcode::Return | Opcode::ReturnCall | Opcode::ReturnCallIndirect => rets.push(t),
                // `trap` never observes memory afterwards.
                Opcode::Trap => {}
                _ => return 0,
            }
            continue;
        }
        for (slot, bc) in dests.iter().enumerate() {
            let d = bc.block(&func.dfg.value_lists);
            if (body.contains(&d) || body.contains(&b))
                && bc
                    .args(&func.dfg.value_lists)
                    .any(|a| !matches!(a, BlockArg::Value(_)))
            {
                // A `try_call` edge we would have to split or extend; its
                // `ret`/`exn` args are only legal on `try_call` itself.
                return 0;
            }
            if body.contains(&d) {
                in_edges.entry(d).or_default().push((b, t, slot));
            } else if body.contains(&b) {
                out_edges.push((t, slot));
            }
        }
    }

    // Unique outside entry edge into the header.
    let mut entry: Option<(Block, Inst, usize)> = None;
    for &(pb, pi, s) in in_edges.get(&h).map_or(&[][..], Vec::as_slice) {
        if !body.contains(&pb) {
            if entry.is_some() {
                return 0;
            }
            entry = Some((pb, pi, s));
        }
    }
    let Some((pb, pinst, pslot)) = entry else {
        return 0;
    };

    // A `Root::V` address is used by the synthesized entry load and by the
    // store-back on every exit/return edge. "Defined outside the loop"
    // doesn't suffice: on irreducible CFGs the def may not dominate the
    // entry edge (same trap as hoisting). Everything the new accesses touch
    // is dominated by `pinst`, so requiring the def to dominate `pinst`
    // covers all of them.
    if let Root::V(v) = loc.0 {
        let ok = match func.dfg.resolved_value_def(v) {
            ValueDef::Result(i, _) => dt.try_dominates(i, pinst, &func.layout),
            ValueDef::Param(b, _) => dt.try_dominates(b, pb, &func.layout),
            _ => false,
        };
        if !ok {
            return 0;
        }
    }

    // The address reused for synthesized accesses: the invariant root
    // itself for V roots; a fresh `stack_addr` for slots (its type is taken
    // from an existing access's address).
    let addr_ty = (|| {
        for &b in body {
            for i in func.layout.block_insts(b) {
                if let InstructionData::Load {
                    opcode: Opcode::Load,
                    arg,
                    ..
                }
                | InstructionData::Store {
                    opcode: Opcode::Store,
                    args: [_, arg],
                    ..
                } = func.dfg.insts[i]
                {
                    return func.dfg.value_type(func.dfg.resolve_aliases(arg));
                }
            }
        }
        types::I64
    })();
    // Merge points needing a version param: the header plus blocks with
    // >=2 distinct version sources (in-loop preds and any outside edge).
    // Any other block must have exactly one in-loop pred and no outside
    // edges — an outside edge into a non-header body block would need a
    // version arg with no param slot to land in.
    let mut unique: FxHashMap<Block, Block> = FxHashMap::default();
    let mut merge: FxHashSet<Block> = FxHashSet::default();
    for &b in body {
        let es = in_edges.get(&b).map_or(&[][..], Vec::as_slice);
        let ins: FxHashSet<Block> = es
            .iter()
            .filter(|(p, ..)| body.contains(p))
            .map(|(p, ..)| *p)
            .collect();
        let outside = es.iter().any(|(p, ..)| !body.contains(p));
        if outside && b != h {
            // An outside edge into a non-header block means `loc`'s version
            // there isn't `v0` — bail rather than risk a stale write.
            return 0;
        }
        if b == h || ins.len() >= 2 {
            merge.insert(b);
        } else if ins.len() == 1 {
            unique.insert(b, ins.into_iter().next().unwrap());
        } else {
            return 0;
        }
    }

    // Single-pred blocks take their pred's out-version; resolve the chains
    // in dependency order and bail (before mutating) if any never resolve.
    let mut resolved: FxHashSet<Block> = merge.clone();
    let mut order: Vec<Block> = merge.iter().copied().collect();
    loop {
        let mut progress = false;
        unique.retain(|&b, &mut p| {
            if resolved.contains(&p) {
                resolved.insert(b);
                order.push(b);
                progress = true;
                false
            } else {
                true
            }
        });
        if unique.is_empty() {
            break;
        }
        if !progress {
            return 0;
        }
    }

    // Load results of `loc` must not leave `body`: they will alias in-body
    // version values.
    let mut lres: FxHashSet<Value> = FxHashSet::default();
    for &b in body {
        for i in func.layout.block_insts(b) {
            if let InstructionData::Load {
                opcode: Opcode::Load,
                arg,
                offset,
                ..
            } = func.dfg.insts[i]
                && func.dfg.value_type(func.dfg.first_result(i)) == ty
                && {
                    let (r, o) = loadfwd::norm_root(func, ps, na, arg);
                    (r, o.wrapping_add(i64::from(i32::from(offset)))) == (loc.0, loc.1)
                }
            {
                lres.insert(func.dfg.first_result(i));
            }
        }
    }
    if !lres.is_empty() {
        for b in func.layout.blocks() {
            for i in func.layout.block_insts(b) {
                let inb = body.contains(&b);
                for &a in func.dfg.inst_args(i) {
                    if !inb && lres.contains(&func.dfg.resolve_aliases(a)) {
                        return 0;
                    }
                }
                for bc in func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    if body.contains(&bc.block(&func.dfg.value_lists)) {
                        continue;
                    }
                    for a in bc.args(&func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a
                            && lres.contains(&func.dfg.resolve_aliases(v))
                        {
                            return 0;
                        }
                    }
                }
            }
        }
    }

    // A store of `loc` that still reaches the entry edge supplies the
    // seed directly: `v0 = load loc` would just read it back. Forwarding
    // keeps the version param congruent with an existing param carrying
    // the same local — an `iconst` init and the reload are equal at
    // runtime but not `phi_cong`, so without this `phimerge` leaves two
    // copies and regalloc pays a per-iteration spill on one.
    let mut seed: Option<Value> = None;
    for i in func.layout.block_insts(pb) {
        if i == pinst {
            break;
        }
        match func.dfg.insts[i] {
            InstructionData::Store {
                opcode: Opcode::Store,
                args,
                offset,
                ..
            } if func.dfg.value_type(func.dfg.resolve_aliases(args[0])) == ty && {
                let (r, o) = loadfwd::norm_root(func, ps, na, args[1]);
                (r, o.wrapping_add(i64::from(i32::from(offset)))) == (loc.0, loc.1)
            } =>
            {
                seed = Some(func.dfg.resolve_aliases(args[0]));
            }
            _ if clobbers(func, ps, na, i, loc, iso, nw, deref) => seed = None,
            _ => {}
        }
    }

    // `v0` = preheader value of `loc`, on the entry edge: the forwarded
    // store's value, else a load before `pinst` when it is a plain jump,
    // else on a fresh edge block whose jump is patched into `h`'s
    // in-edges.
    let v0: Value = if let Some(v) = seed {
        v
    } else if matches!(func.dfg.insts[pinst], InstructionData::Jump { .. }) {
        let a = match loc.0 {
            Root::V(v) => v,
            Root::S(ss) => FuncCursor::new(func)
                .at_inst(pinst)
                .ins()
                .stack_addr(addr_ty, ss, 0),
        };
        FuncCursor::new(func).at_inst(pinst).ins().load(
            ty,
            MemFlagsData::new().with_notrap(),
            a,
            off,
        )
    } else {
        let Some(nb) = split_edge(func, pinst, pslot) else {
            return 0;
        };
        let j = func.layout.last_inst(nb).unwrap();
        let a = match loc.0 {
            Root::V(v) => v,
            Root::S(ss) => FuncCursor::new(func)
                .at_inst(j)
                .ins()
                .stack_addr(addr_ty, ss, 0),
        };
        let v = FuncCursor::new(func).at_inst(j).ins().load(
            ty,
            MemFlagsData::new().with_notrap(),
            a,
            off,
        );
        let es = in_edges.get_mut(&h).unwrap();
        let k = es
            .iter()
            .position(|&(_, pi, s)| pi == pinst && s == pslot)
            .unwrap();
        es[k] = (nb, j, 0);
        v
    };

    // Mutate from here. Version params on merge blocks.
    let mut vin: FxHashMap<Block, Value> = FxHashMap::default();
    for &b in &merge {
        vin.insert(b, func.dfg.append_block_param(b, ty));
    }

    // Walk `body` in dependency order threading the version through loads
    // (aliased to `cur`) and stores (`cur` becomes the stored value).
    let mut vout: FxHashMap<Block, Value> = FxHashMap::default();
    for &b in &order {
        let mut cur = match vin.get(&b) {
            Some(&p) => p,
            None => in_edges[&b]
                .iter()
                .find(|(pb, ..)| body.contains(pb))
                .map(|(pb, ..)| vout[pb])
                .unwrap(),
        };
        for i in func.layout.block_insts(b).collect::<Vec<_>>() {
            match func.dfg.insts[i] {
                InstructionData::Load {
                    opcode: Opcode::Load,
                    arg,
                    offset,
                    ..
                } if func.dfg.value_type(func.dfg.first_result(i)) == ty && {
                    let (r, o) = loadfwd::norm_root(func, ps, na, arg);
                    (r, o.wrapping_add(i64::from(i32::from(offset)))) == (loc.0, loc.1)
                } =>
                {
                    let r = func.dfg.first_result(i);
                    func.layout.remove_inst(i);
                    func.dfg.clear_results(i);
                    func.dfg.change_to_alias(r, cur);
                }
                InstructionData::Store {
                    opcode: Opcode::Store,
                    args,
                    offset,
                    ..
                } if func.dfg.value_type(func.dfg.resolve_aliases(args[0])) == ty && {
                    let (r, o) = loadfwd::norm_root(func, ps, na, args[1]);
                    (r, o.wrapping_add(i64::from(i32::from(offset)))) == (loc.0, loc.1)
                } =>
                {
                    cur = func.dfg.resolve_aliases(args[0]);
                    func.layout.remove_inst(i);
                }
                _ => {}
            }
        }
        vout.insert(b, cur);
    }

    // Edges into merge blocks carry the source's out-version (or `v0` on
    // the entry edge); exit edges split and store it back; an in-body
    // `return` stores its block's version just before it.
    for (d, es) in &in_edges {
        if !vin.contains_key(d) {
            continue;
        }
        for &(pb, pi, slot) in es {
            let v = if body.contains(&pb) { vout[&pb] } else { v0 };
            let dfg = &mut func.dfg;
            let bcs = dfg.insts[pi]
                .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables);
            let bc = bcs[slot];
            let mut args: Vec<BlockArg> = bc.args(&dfg.value_lists).collect();
            args.push(BlockArg::Value(v));
            let dest = bc.block(&dfg.value_lists);
            bcs[slot] = BlockCall::new(dest, args, &mut dfg.value_lists);
        }
    }
    let store_back = |func: &mut Function, at: Inst, v: Value| {
        let a = match loc.0 {
            Root::V(v) => v,
            Root::S(ss) => FuncCursor::new(func)
                .at_inst(at)
                .ins()
                .stack_addr(addr_ty, ss, 0),
        };
        FuncCursor::new(func)
            .at_inst(at)
            .ins()
            .store(MemFlagsData::new().with_notrap(), v, a, off);
    };
    for (t, slot) in out_edges {
        let sb = func.layout.inst_block(t).unwrap();
        let v = vout[&sb];
        // Scanned all-Value above; skipping a store-back would lose a write.
        let nb = split_edge(func, t, slot).expect("out-edge checked splittable");
        let j = func.layout.last_inst(nb).unwrap();
        store_back(func, j, v);
    }
    for t in rets {
        let sb = func.layout.inst_block(t).unwrap();
        let v = vout[&sb];
        store_back(func, t, v);
    }
    if debug {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::licm", "licm: promote {:?} in {:?} of {}", loc, h, func.name)
        };
    }
    1
}

pub fn run(
    func: &mut Function,
    an: &mut crate::pipeline::Analysis,
    noalias: &FxHashSet<Value>,
    deref: &FxHashMap<Value, (u64, bool)>,
    nw: &FxHashMap<FuncRef, bool>,
    cfrozen: &FxHashMap<FuncRef, u64>,
    scoped: &FxHashMap<Inst, pliron::context::Ptr<pliron::basic_block::BasicBlock>>,
    breg: &FxHashMap<Block, pliron::context::Ptr<pliron::basic_block::BasicBlock>>,
    rpar: &pliron::cols::ColMap<pliron::context::Ptr<pliron::basic_block::BasicBlock>, pliron::context::Ptr<pliron::basic_block::BasicBlock>>,
    ofz: &FxHashMap<Value, Vec<(u64, pliron::context::Ptr<pliron::basic_block::BasicBlock>)>>,
) -> usize {
    let mut la = LoopAnalysis::new();
    let (cfg, dt) = an.cfg_dt(func);
    la.compute(func, cfg, dt);
    let ps = &loadfwd::param_src(func);
    let (iso, escapes, _) =
        loadfwd::isolated_escapes(func, noalias, ps, cfrozen, &FxHashSet::default());
    // Operand-frozen values name their own scope roots (same merged set
    // loadfwd uses): peeling past one would lose the `ofz` claim before the
    // region check can apply it.
    let mut stop: FxHashSet<Value> = noalias.clone();
    stop.extend(ofz.keys().copied());
    let debug = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::licm", tracing::Level::DEBUG);
    // Innermost first: values hoisted out of an inner loop become candidates
    // for the enclosing loop on the same pass.
    let mut loops: Vec<Loop> = la.loops().collect();
    loops.sort_by_key(|&lp| {
        std::cmp::Reverse(
            func.layout
                .blocks()
                .filter(|&b| la.is_in_loop(b, lp))
                .map(|b| la.loop_level(b).level())
                .max()
                .unwrap_or(0),
        )
    });
    // `run_loop` already caps its own moves at MAX_MOVES; a shared budget
    // starves every later loop once one loop's rematerialized constants
    // consume it (e.g. `SplitWhitespace::next`: ~90 iconsts hoisted from an
    // early loop left zero moves for the hot char loop's loads). Keep a
    // looser function-level bound only as a compile-time guard.
    let mut n = 0;
    for lp in loops.into_iter().take(MAX_LOOPS) {
        // `la` is owned by this pass and stays valid across `run_loop`'s
        // edge splits/promotion (splits only lengthen paths, and fresh
        // blocks are in no loop), but `cfg`/`dt` no longer reflect the
        // CFG — e.g. a split edge leaves `pred_iter` without the new
        // block and without the pred's redirected edge, so an entry
        // scan can count one outside edge where several exist. Read the
        // fresh pair through `an` each iteration.
        let n2 = {
            let (cfg, dt) = an.cfg_dt(func);
            run_loop(
                func, &ps, &stop, cfg, dt, &la, lp, &iso, &escapes, deref, nw, debug, scoped, breg,
                rpar, ofz,
            )
        };
        n += n2;
        if n2 > 0 {
            an.invalidate(func);
        }
        if n >= MAX_MOVES * MAX_LOOPS {
            break;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::{
        AbiParam, Function, InstBuilder, Signature, StackSlotData, StackSlotKind, UserFuncName,
    };
    use cranelift_codegen::isa::CallConv;

    /// `run` caches `cfg`/`dt`/`la` once, but `run_loop` splits edges: a pred
    /// redirected through a split block leaves the stale `pred_iter` seeing
    /// the old pred whose edge no longer reaches `h` and not seeing the new
    /// block at all, so the unique-entry scan undercounts and hoists to a
    /// block that doesn't dominate the header (pulldown-cmark's
    /// `Chars::advance_by` ICE'd stage2 on this).
    /// Shape: sibling loop `sb`'s exit edge enters outer header `h`; `h`
    /// also has the entry edge from `b0` and holds an iconst the header
    /// uses.
    #[test]
    fn entry_scan_counts_split_edge_preds() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let rest = func.dfg.make_block();
        let inh = func.dfg.make_block();
        let sb = func.dfg.make_block();
        let h = func.dfg.make_block();
        let hbody = func.dfg.make_block();
        let done = func.dfg.make_block();
        for b in [b0, rest, inh, sb, h, hbody, done] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let hp = func.dfg.append_block_param(h, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let c0 = cur.ins().iconst(types::I8, 1);
        let forty_two = cur.ins().iconst(types::I64, 42);
        cur.ins().brif(c0, rest, &[], h, &[forty_two.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(rest);
        cur.ins().jump(inh, &[]);

        // Loop `sb`: the store makes `ss` a promote candidate; `sb`'s exit
        // edge to `h` gets split by `promote_loc`, leaving `h` entered from
        // both the `b0` edge and the fresh split block.
        let mut cur = FuncCursor::new(&mut func).at_bottom(inh);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        let seven = cur.ins().iconst(types::I64, 7);
        cur.ins().store(MemFlagsData::new(), seven, a, 0);
        cur.ins().jump(sb, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(sb);
        let c2 = cur.ins().iconst(types::I8, 0);
        let three = cur.ins().iconst(types::I64, 3);
        cur.ins().brif(c2, h, &[three.into()], inh, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(h);
        let k = cur.ins().iconst(types::I64, 0);
        let r = cur.ins().icmp(IntCC::UnsignedGreaterThan, hp, k);
        cur.ins().brif(r, hbody, &[], done, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(hbody);
        let one = cur.ins().iconst(types::I64, 1);
        cur.ins().jump(h, &[one.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(done);
        let z = cur.ins().iconst(types::I64, 0);
        cur.ins().return_(&[z]);

        run(
            &mut func,
            &mut crate::pipeline::Analysis::default(),
            &FxHashSet::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &pliron::cols::ColMap::default(),
            &FxHashMap::default(),
        );
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("licm produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// A header param `p` that every edge fills with the same preheader
    /// value resolves through `param_src` to that value, so `inv` accepts
    /// it — but the param does not dominate the preheader. Hoisting must
    /// move the resolved operand, not the param (memchr's
    /// `searcher_kind_two_way` ICE'd stage1 on this: an `iadd p, 48` moved
    /// out still naming `p`).
    #[test]
    fn hoist_rewrites_forwarded_param_operand() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let h = func.dfg.make_block();
        let lbody = func.dfg.make_block();
        let done = func.dfg.make_block();
        for b in [b0, h, lbody, done] {
            func.layout.append_block(b);
        }
        let p = func.dfg.append_block_param(h, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let fv = cur.ins().iconst(types::I64, 5);
        cur.ins().jump(h, &[fv.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(h);
        let one = cur.ins().iconst(types::I64, 1);
        let r = cur.ins().icmp(IntCC::UnsignedGreaterThan, p, one);
        cur.ins().brif(r, lbody, &[], done, &[]);

        // `iadd p, k` in the body; `p` forwards `fv` on both edges into
        // `h`, so `psrc(p)` is `fv` and `inv(p)` holds — the hoisted inst
        // must use `fv`, not `p`. `s` needs no use: the verifier checks a
        // moved inst's operands regardless.
        let mut cur = FuncCursor::new(&mut func).at_bottom(lbody);
        let k = cur.ins().iconst(types::I64, 48);
        cur.ins().iadd(p, k);
        cur.ins().jump(h, &[fv.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(done);
        let z = cur.ins().iconst(types::I64, 0);
        cur.ins().return_(&[z]);

        run(
            &mut func,
            &mut crate::pipeline::Analysis::default(),
            &FxHashSet::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &pliron::cols::ColMap::default(),
            &FxHashMap::default(),
        );
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("licm produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// `load notrap` at `stack_addr ss + wild_offset` is not a stack
    /// access — the address arithmetic left the slot entirely — so it
    /// is not unconditionally dereferenceable and must not hoist out
    /// of the loop onto the unconditional preheader path (rustlantis
    /// seed 41 SIGSEGV: `load.i128` at `ss84 + 0x2518_0084_2820_8000`,
    /// a load that sat on a path the program never runs).
    /// In-bounds slot accesses still hoist.
    #[test]
    fn wild_offset_load_stays_in_loop() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let h = func.dfg.make_block();
        let lbody = func.dfg.make_block();
        let done = func.dfg.make_block();
        for b in [b0, h, lbody, done] {
            func.layout.append_block(b);
        }
        let ss =
            func.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 64, 4));
        let p = func.dfg.append_block_param(h, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let fv = cur.ins().iconst(types::I64, 5);
        let a = cur.ins().stack_addr(types::I64, ss, 0);
        let wild = cur
            .ins()
            .iconst(types::I64, 0x2518_0084_2820_8000u64 as i64);
        let eight = cur.ins().iconst(types::I64, 8);
        cur.ins().jump(h, &[fv.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(h);
        let one = cur.ins().iconst(types::I64, 1);
        let r = cur.ins().icmp(IntCC::UnsignedGreaterThan, p, one);
        cur.ins().brif(r, lbody, &[], done, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(lbody);
        let wildp = cur.ins().iadd(a, wild);
        let w = cur
            .ins()
            .load(types::I128, MemFlagsData::new().with_notrap(), wildp, 0);
        let okp = cur.ins().iadd(a, eight);
        let ok = cur
            .ins()
            .load(types::I64, MemFlagsData::new().with_notrap(), okp, 0);
        cur.ins().jump(h, &[fv.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(done);
        let z = cur.ins().iconst(types::I64, 0);
        cur.ins().return_(&[z]);

        run(
            &mut func,
            &mut crate::pipeline::Analysis::default(),
            &FxHashSet::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &pliron::cols::ColMap::default(),
            &FxHashMap::default(),
        );
        let blk = |v: Value| {
            func.layout
                .inst_block(func.dfg.value_def(v).inst().unwrap())
                .unwrap()
        };
        assert_eq!(
            blk(w),
            lbody,
            "wild-offset load left the loop:\n{}",
            func.display()
        );
        assert_eq!(
            blk(ok),
            b0,
            "in-bounds load failed to hoist:\n{}",
            func.display()
        );
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("licm produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// `load.i64 notrap p+8` against a `dereferenceable(8)` root reads
    /// bytes 8..16 — past the proven range. The bound must cover the
    /// access *including* the inst's `offset` imm; checking only the
    /// peel offset would speculate a read past the marked object onto
    /// the unconditional path (a `notrap` load on a never-taken path
    /// can carry any offset). In-bounds accesses still hoist.
    #[test]
    fn offset_past_deref_bound_stays_in_loop() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let h = func.dfg.make_block();
        let lbody = func.dfg.make_block();
        let done = func.dfg.make_block();
        for b in [b0, h, lbody, done] {
            func.layout.append_block(b);
        }
        let p = func.dfg.append_block_param(h, types::I64);
        let q = func.dfg.append_block_param(h, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let fv = cur.ins().iconst(types::I64, 5);
        let ptr = cur.ins().iconst(types::I64, 0x1000);
        cur.ins().jump(h, &[fv.into(), ptr.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(h);
        let one = cur.ins().iconst(types::I64, 1);
        let r = cur.ins().icmp(IntCC::UnsignedGreaterThan, p, one);
        cur.ins().brif(r, lbody, &[], done, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(lbody);
        let wild = cur
            .ins()
            .load(types::I64, MemFlagsData::new().with_notrap(), q, 8);
        let ok = cur
            .ins()
            .load(types::I64, MemFlagsData::new().with_notrap(), q, 0);
        cur.ins().jump(h, &[fv.into(), ptr.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(done);
        let z = cur.ins().iconst(types::I64, 0);
        cur.ins().return_(&[z]);

        let mut deref = FxHashMap::default();
        deref.insert(ptr, (8, false));
        run(
            &mut func,
            &mut crate::pipeline::Analysis::default(),
            &FxHashSet::default(),
            &deref,
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &pliron::cols::ColMap::default(),
            &FxHashMap::default(),
        );
        let blk = |v: Value| {
            func.layout
                .inst_block(func.dfg.value_def(v).inst().unwrap())
                .unwrap()
        };
        assert_eq!(
            blk(wild),
            lbody,
            "load past the dereferenceable bound left the loop:\n{}",
            func.display()
        );
        assert_eq!(
            blk(ok),
            b0,
            "in-bounds load failed to hoist:\n{}",
            func.display()
        );
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("licm produced invalid IR: {e}\n{}", func.display());
        }
    }

    /// A `noalias`+`readonly` pointee (`deref` with `!writable`) is only
    /// ever read through its root — no store or call in the loop can
    /// legally write it, so the clobber scan must not count writers
    /// against it. Without that, a `&T` param's field loads miss
    /// hoisting whenever the loop body contains any store at all
    /// (the typical case). A `writable` root's overlapping load still
    /// stays — its pointee may be written.
    #[test]
    fn readonly_root_load_hoists_past_store() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b0 = func.dfg.make_block();
        let h = func.dfg.make_block();
        let lbody = func.dfg.make_block();
        let done = func.dfg.make_block();
        for b in [b0, h, lbody, done] {
            func.layout.append_block(b);
        }
        let p = func.dfg.append_block_param(h, types::I64);
        let rp = func.dfg.append_block_param(h, types::I64);
        let wp = func.dfg.append_block_param(h, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(b0);
        let fv = cur.ins().iconst(types::I64, 5);
        let ro = cur.ins().iconst(types::I64, 0x1000);
        let wr = cur.ins().iconst(types::I64, 0x2000);
        cur.ins().jump(h, &[fv.into(), ro.into(), wr.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(h);
        let one = cur.ins().iconst(types::I64, 1);
        let r = cur.ins().icmp(IntCC::UnsignedGreaterThan, p, one);
        cur.ins().brif(r, lbody, &[], done, &[]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(lbody);
        let x = cur.ins().iconst(types::I64, 7);
        cur.ins().store(MemFlagsData::new(), x, wp, 0);
        let a = cur
            .ins()
            .load(types::I64, MemFlagsData::new().with_notrap(), rp, 0);
        let b = cur
            .ins()
            .load(types::I32, MemFlagsData::new().with_notrap(), wp, 4);
        cur.ins().store(MemFlagsData::new(), b, wp, 8);
        cur.ins().jump(h, &[a.into(), ro.into(), wr.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(done);
        cur.ins().return_(&[fv]);

        let mut deref = FxHashMap::default();
        deref.insert(ro, (16, false));
        deref.insert(wr, (16, true));
        run(
            &mut func,
            &mut crate::pipeline::Analysis::default(),
            &FxHashSet::default(),
            &deref,
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
            &pliron::cols::ColMap::default(),
            &FxHashMap::default(),
        );
        let blk = |v: Value| {
            func.layout
                .inst_block(func.dfg.value_def(v).inst().unwrap())
                .unwrap()
        };
        assert_eq!(
            blk(a),
            b0,
            "readonly-root load missed the hoist past an unrelated store:\n{}",
            func.display()
        );
        assert_eq!(
            blk(b),
            lbody,
            "writable-root load left a loop that writes it:\n{}",
            func.display()
        );
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        if let Err(e) = cranelift_codegen::verify_function(&func, &flags) {
            panic!("licm produced invalid IR: {e}\n{}", func.display());
        }
    }
}
