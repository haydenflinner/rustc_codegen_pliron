//! Cross-block load forwarding on Cranelift IR (LLVM GVN's load elimination,
//! minus PRE). A location is a root (SSA pointer or stack slot) plus a
//! constant offset and a type. Unlike Cranelift's alias analysis, a store
//! only kills locations it may overlap: same root and overlapping bytes, or
//! any location when the roots may alias. Calls keep only isolated roots
//! (see `isolated`); atomics, fences and trapping (volatile) stores kill
//! everything. Only `notrap` loads/stores (all non-volatile accesses at -O)
//! are forwarded.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::entity::packed_option::ReservedValue;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::FuncRef;
use cranelift_codegen::ir::{
    Block, Function, Inst, InstructionData, Opcode, StackSlot, Type, Value, ValueDef,
};
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_ENTRIES: usize = 64;
const MAX_ITERS: usize = 32;

/// A forwarded load's replacement: an available value, or a select of the
/// two arms' forwarded values for `load (select c, pa, pb)` — niche
/// dispatch materialises field addresses this way (RangeInclusive's
/// `select exhausted, &start, &end`). Splitting is only emitted when both
/// arm locations forward, so no speculative load is introduced.
enum Fwd {
    V(Value),
    Sel { c: Value, t: Value, e: Value },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Root {
    V(Value),
    S(StackSlot),
}

pub type Loc = (Root, i64, Type);
type Avail = FxHashMap<Loc, Value>;

type Region = pliron::context::Ptr<pliron::basic_block::BasicBlock>;

/// `cur` is inside region `target` (or is it): regions nest through
/// `region_parent` — same walk licm does for scoped loads.
pub fn in_region(cur: Region, target: Region, rpar: &FxHashMap<Region, Region>) -> bool {
    let mut r = Some(cur);
    while let Some(x) = r {
        if x == target {
            return true;
        }
        r = rpar.get(&x).copied().filter(|&n| n != x);
    }
    false
}

/// Block params that always carry one value: every predecessor passes the
/// same argument (or the param itself — a loop-carried passthrough, like
/// `partition`'s `pivot` threaded through the scan loop). Such a param is
/// that value for analysis purposes. Maps param → that value.
/// `proots::alias_map` computes the relation (reachable edges only —
/// args on unreachable edges are indistinguishable from absent ones).
pub fn param_src(func: &Function) -> FxHashMap<Value, Value> {
    crate::proots::alias_map(func)
}

/// Chase `param_src` links (plus value aliases) to the value `v` provably
/// always equals. Bounded: param chains are short, and cycles can't occur
/// in valid SSA — a param can only feed blocks it dominates.
pub fn psrc(
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    func: &Function,
    mut v: Value,
) -> Value {
    for _ in 0..16 {
        if na.contains(&v) {
            return v;
        }
        v = func.dfg.resolve_aliases(v);
        if na.contains(&v) {
            return v;
        }
        match ps.get(&v) {
            Some(&s) if s != v => v = s,
            _ => return v,
        }
    }
    v
}

/// The (dst block, param index) the `j`th branch-arg use of `i` feeds —
/// `j` counts only `BlockArg::Value`s, matching `inst_values`' ordering.
/// `None` for exception-context uses, which can't be traced.
fn fwd_arg(func: &Function, i: Inst, mut j: usize) -> Option<(Block, usize)> {
    for bc in
        func.dfg.insts[i].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
    {
        for (pos, a) in bc.args(&func.dfg.value_lists).enumerate() {
            if a.as_value().is_some() {
                if j == 0 {
                    return Some((bc.block(&func.dfg.value_lists), pos));
                }
                j -= 1;
            }
        }
    }
    None
}

/// Bounds below this are "small": `base + x` can't wrap a real
/// pointer, so `x`'s contribution to the byte offset is provably `≥ 0`.
const BND_CAP: u64 = 1 << 32;

pub fn root(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    v: Value,
) -> (Root, i64) {
    root_min(func, ps, na, None, v)
}

/// `root` with a lower-bound result: when `bnd` is given, an `iadd`
/// also peels through an operand `bnd` proves `< BND_CAP` (provably
/// nonneg and wrap-safe — `bnd` bounds unsigned values). The returned
/// offset is then only a *minimum*: the value may sit further into
/// `(Root, ...)`'s object. Callers may rely on "addr ≥ root + off",
/// never on exact byte equality.
fn root_min(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    bnd: Option<&FxHashMap<Value, u64>>,
    v: Value,
) -> (Root, i64) {
    // Check the un-resolved value too: a scope root may itself be an
    // alias (`region_frozen_op`'s clone operand often is), and stopping
    // before the peel keeps the scope name as the root.
    if na.contains(&v) {
        return (Root::V(v), 0);
    }
    let mut v = func.dfg.resolve_aliases(v);
    let mut off = 0i64;
    for _ in 0..16 {
        // A `noalias` value is its own scope root: the name carries the
        // alias class (`&v[1..]` computes `pivot + 8` but is a different
        // object), so resolve no further.
        if na.contains(&v) {
            break;
        }
        // A single-source param is its source (`param_src`); keep peeling.
        if let Some(&s) = ps.get(&v) {
            v = func.dfg.resolve_aliases(s);
            continue;
        }
        let Some(i) = func.dfg.value_def(v).inst() else {
            break;
        };
        match func.dfg.insts[i] {
            InstructionData::StackAddr {
                stack_slot, offset, ..
            } => {
                return (
                    Root::S(stack_slot),
                    off.wrapping_add(i64::from(i32::from(offset))),
                );
            }
            // `black_box` hides the value, not the object: its result
            // names the input's exact pointee. The op still clobbers
            // memory where it sits (`can_store`), so no ordering is
            // lost — the peel only sharpens identity.
            InstructionData::Unary {
                opcode: Opcode::BlackBox,
                arg,
            } => v = arg,
            InstructionData::Binary {
                opcode: Opcode::Iadd,
                args,
            } => {
                let c = |x: Value| match &func.dfg.insts[func.dfg.value_def(x).inst()?] {
                    InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm,
                    } => Some(imm.bits()),
                    _ => None,
                };
                let (a, b) = (psrc(ps, na, func, args[0]), psrc(ps, na, func, args[1]));
                if let Some(k) = c(b) {
                    off = off.wrapping_add(k);
                    v = a;
                } else if let Some(k) = c(a) {
                    off = off.wrapping_add(k);
                    v = b;
                } else if let Some(bd) = bnd {
                    // `x + small`: the bounded operand is provably
                    // nonneg, so the result's offset only grows. Check
                    // the raw arg too — a bounded param's bound lives
                    // under the param, not the resolved source.
                    let small = |raw: Value, res: Value| {
                        [func.dfg.resolve_aliases(raw), res]
                            .into_iter()
                            .any(|x| bd.get(&x).is_some_and(|&u| u < BND_CAP))
                    };
                    if small(args[1], b) {
                        v = a;
                    } else if small(args[0], a) {
                        v = b;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    (Root::V(v), off)
}

/// Param-level `root_min`: `p`'s bound in `(root, min offset)` when
/// every reachable in-edge source lower-bounds into the same root.
/// `p + k` self-feeds with `k ≥ 0` are skipped — they can only push the
/// cursor further in (a `k < 0` self-feed fails the param: it would
/// slide the bound back under the entry edge's base). Unlike the
/// `param`-class checks in `isolated`, the result is used *only* to
/// prove a store's disjointness in `kill` — never as a loc key — so a
/// `base + k` cursor posing as its base is harmless here.
fn pmroots(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    bnd: &FxHashMap<Value, u64>,
    pf: &crate::proots::Facts,
) -> FxHashMap<Value, (Root, i64)> {
    let blocked: FxHashSet<Value> = pf.blocked.iter().copied().collect();
    let mut byp: FxHashMap<Value, Vec<Value>> = FxHashMap::default();
    for &(p, a) in &pf.edge {
        byp.entry(p).or_default().push(a);
    }
    let mut pm: FxHashMap<Value, (Root, i64)> = FxHashMap::default();
    loop {
        let mut changed = false;
        for (&p, srcs) in &byp {
            if pm.contains_key(&p) || blocked.contains(&p) {
                continue;
            }
            let mut acc: Option<(Root, i64)> = None;
            let mut ok = true;
            for &a in srcs {
                let (mut r, mut k) = root_min(func, ps, na, Some(bnd), a);
                // A `V(q)` root for an unsolved param `q` is a fine
                // terminal root — `p ≥ q + k` is still a true bound; it
                // just won't match locs keyed under q's deeper root.
                if let Root::V(q) = r
                    && q != p
                    && let Some(&(qr, qo)) = pm.get(&q)
                {
                    r = qr;
                    k = k.wrapping_add(qo);
                }
                if r == Root::V(p) {
                    // `p` fed by `p + k`: preserves `p ≥ base` iff `k ≥ 0`.
                    if k < 0 {
                        ok = false;
                        break;
                    }
                    continue;
                }
                match &mut acc {
                    None => acc = Some((r, k)),
                    Some((ar, am)) if *ar == r => *am = (*am).min(k),
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok && let Some((r, m)) = acc {
                pm.insert(p, (r, m));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    pm
}

pub fn notrap(func: &Function, i: Inst) -> bool {
    func.dfg.insts[i].memflags().is_some()
        && func.dfg.insts[i].memflags_trap_code(&func.dfg).is_none()
}

/// `root` with no `noalias` scope stops: peel scope names to the concrete
/// object underneath. Two scope roots are provably distinct objects only
/// when both peels bottom out somewhere disjoint — today, separate
/// `stack_addr`s. Returning `(V(v), off)` means "unresolved", not
/// "different object": callers must keep treating it as may-alias.
pub fn concrete(func: &Function, ps: &FxHashMap<Value, Value>, v: Value) -> (Root, i64) {
    root(func, ps, &FxHashSet::default(), v)
}

/// `root` normalized for disjointness/dereferenceability checks: when a
/// `V` scope name (`noalias` param, operand-frozen operand) peels to a
/// `stack_addr`, report the slot itself so two scoped pointers into
/// different locals compare disjoint like any other slot pair.
pub fn norm_root(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    v: Value,
) -> (Root, i64) {
    root_pair(func, ps, na, v).1
}

/// `root` plus its `norm_root` normalization in one peel: `((raw, off),
/// (norm, off))`. The raw root keeps the scope name (`ofz` bounds are
/// measured from the operand); the normalized root names the concrete
/// object — locs keyed by it unify scoped and direct accesses of one
/// slot, and stores to other slots are disjoint without `iso`.
pub fn root_pair(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    v: Value,
) -> ((Root, i64), (Root, i64)) {
    let (r, o) = root(func, ps, na, v);
    let n = match r {
        Root::V(rv) => match concrete(func, ps, rv) {
            (Root::S(ss), so) => (Root::S(ss), o.wrapping_add(so)),
            _ => (r, o),
        },
        _ => (r, o),
    };
    ((r, o), n)
}

/// Kill the locations a store of `ty` at `(r, o)` may overwrite. `ms`,
/// when set, is a proven `(root, minimum offset)` for the store's
/// address (from `root_min` through bounded addends): the store then
/// also can't reach any loc in that root ending at or below `ms`'s
/// offset — `base + 8 + 8*i` can't hit `base + 0` when `i ≥ 0`.
pub fn kill(
    av: &mut Avail,
    r: Root,
    o: i64,
    ty: Type,
    ms: Option<(Root, i64)>,
    iso: &FxHashSet<Root>,
    ncc: &FxHashSet<Root>,
    invs: &FxHashSet<Loc>,
    cur: Option<Region>,
    invreg: &FxHashMap<Loc, Vec<Region>>,
    rpar: &FxHashMap<Region, Region>,
) {
    let n = i64::from(ty.bytes());
    // `iso` roots can't be reached by untracked pointers at all; `ncc`
    // roots only escape through `nocapture` call operands, so no
    // *lasting* untracked alias exists — for a store's kill set the two
    // are equivalent (calls that receive the root still clobber it —
    // that's the call arm's job, not the store rule's).
    let shel = |x: &Root| iso.contains(x) || ncc.contains(x);
    av.retain(|&(r2, o2, t2), _| {
        // An `!invariant.load` location holds the same value for the
        // load's whole scope: a write to it would be UB, so no store can
        // clobber it — not even one to the same normalized location.
        if invs.contains(&(r2, o2, t2)) {
            return true;
        }
        // A region-frozen operand's pointee can't be written while the
        // clone runs — a store inside the region can't clobber it. This
        // is LLVM's scoped `!noalias`: `*pivot` survives `partition`'s
        // element swaps for the clone's whole span.
        if let Some(cur) = cur
            && invreg
                .get(&(r2, o2, t2))
                .is_some_and(|rs| rs.iter().any(|&t| in_region(cur, t, rpar)))
        {
            return true;
        }
        match (r, r2) {
            _ if r == r2 => o2 >= o + n || o >= o2 + i64::from(t2.bytes()),
            (Root::S(_), Root::S(_)) => true,
            _ => {
                shel(&r)
                    || shel(&r2)
                    || ms.is_some_and(|(mr, mo)| mr == r2 && mo >= o2 + i64::from(t2.bytes()))
            }
        }
    });
}

/// A direct call to a function that writes no memory before returning
/// normally; across `try_call` it must not write at all unless `eh`, where
/// [`unwind_edge`] clears the exception edges instead (nowrite.rs).
pub fn write_free(func: &Function, i: Inst, nw: &FxHashMap<FuncRef, bool>, eh: bool) -> bool {
    match func.dfg.insts[i] {
        InstructionData::Call { func_ref, .. } => nw.contains_key(&func_ref),
        InstructionData::TryCall { func_ref, .. } => {
            nw.get(&func_ref).is_some_and(|&strict| strict || eh)
        }
        _ => false,
    }
}

/// An exception edge `inst -> b` of a `try_call` whose callee may have written
/// before unwinding: loads available after the call don't survive into `b`.
fn unwind_edge(func: &Function, inst: Inst, b: Block, nw: &FxHashMap<FuncRef, bool>) -> bool {
    match func.dfg.insts[inst] {
        InstructionData::TryCall {
            func_ref,
            exception,
            ..
        } => {
            nw.get(&func_ref) == Some(&false)
                && func.dfg.exception_tables[exception]
                    .normal_return()
                    .block(&func.dfg.value_lists)
                    != b
        }
        _ => false,
    }
}

/// Roots whose memory is reachable only through addresses with that root:
/// stack slots (`PLIRON_FWD_SLOTS`) and rustc-`noalias` entry params
/// (`PLIRON_NOALIAS_FWD`) that never escape. Every use of a value with such
/// a root must keep the address tracked: a load/store address, a const
/// `iadd` that `root` still resolves, an `icmp` (a bool can't carry the
/// pointer), or a branch arg feeding a param that `param_src` resolves
/// back to the same root. Call args are never benign: the callee reads
/// the pointee and may stash the pointer. Anything else lets the address
/// reach memory analysis can't see.
pub fn isolated(
    func: &Function,
    noalias: &FxHashSet<Value>,
    ps: &FxHashMap<Value, Value>,
    cfrozen: &FxHashMap<FuncRef, u64>,
) -> FxHashSet<Root> {
    isolated_escapes(func, noalias, ps, cfrozen, &FxHashSet::default()).0
}

/// `isolated` plus, for every rejected root, the blocks holding its
/// non-benign uses, plus the set of roots whose *only* escapes are
/// `nocapture` call operands (`nocap`: `(inst, arg index)` — arg indices
/// count real args, so the `*Indirect` callee at operand 0 is skipped).
/// A nocap-arg escape is bounded: the callee may read/write the pointee
/// but can't retain the pointer, so no untracked alias outlives the call.
/// Such roots stay reachable memory — calls that receive them must still
/// kill their locs — but stores rooted elsewhere can't reach them.
/// Escapes are recorded for them exactly as for any other escape, so
/// consumers reading only `(iso, escapes)` see no change.
pub fn isolated_escapes(
    func: &Function,
    noalias: &FxHashSet<Value>,
    ps: &FxHashMap<Value, Value>,
    _cfrozen: &FxHashMap<FuncRef, u64>,
    nocap: &FxHashSet<(Inst, u32)>,
) -> (
    FxHashSet<Root>,
    FxHashMap<Root, Vec<Block>>,
    FxHashSet<Root>,
) {
    let mut cand: FxHashSet<Root> = noalias.iter().map(|&v| Root::V(v)).collect();
    if crate::pass_enabled("PLIRON_FWD_SLOTS") {
        cand.extend(func.sized_stack_slots.keys().map(Root::S));
    }
    let mut escapes: FxHashMap<Root, Vec<Block>> = FxHashMap::default();
    // Escaped roots with at least one *unbounded* escape — every use we
    // can't prove leaves no lasting alias.
    let mut ncc_bad: FxHashSet<Root> = FxHashSet::default();
    if cand.is_empty() {
        return (cand, escapes, ncc_bad);
    }
    // `root` is deterministic for the whole scan; the same address value
    // shows up at many use sites (visitor bodies re-read one pointer at
    // every field), so memoize it rather than re-peeling each time.
    // Normalize like the loc keys do (`norm_root`): a `noalias` value
    // that concretes to a stack slot names the slot's memory, and locs
    // rooted through it key under `S(slot)` — an escape recorded under
    // the scope name `V(v)` would leave `S(slot)` isolated while its
    // memory is reachable through the stashed pointer.
    let mut rmemo: FxHashMap<Value, (Root, i64)> = FxHashMap::default();
    let mut rt = |func: &Function, v: Value| {
        *rmemo
            .entry(v)
            .or_insert_with(|| norm_root(func, ps, noalias, v))
    };
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let data = &func.dfg.insts[i];
            let nargs = func.dfg.inst_args(i).len();
            // A use that is a branch arg (index ≥ the inst's own args):
            // benign iff the param it feeds is the same root — i.e., the
            // pointer stays inside this equivalence class.
            for (k, v) in func.dfg.inst_values(i).enumerate() {
                let (r, _) = rt(func, v);
                if !cand.contains(&r) {
                    continue;
                }
                // Benign iff the fed param itself peels to `r` — then
                // `root` keys its memory ops under `r` and the pointer
                // stays inside the tracked class. (A param whose in-edge
                // args merely root at `r` — `base + k` cursors — is not
                // enough: `root` would key them under `V(param)`, their
                // stores would dodge the same-root kill, and the param's
                // own uses would escape scanning entirely.)
                let fwd = |j: usize| {
                    fwd_arg(func, i, j).is_some_and(|(dst, pos)| {
                        norm_root(
                            func,
                            ps,
                            noalias,
                            psrc(ps, noalias, func, func.dfg.block_params(dst)[pos]),
                        )
                        .0 == r
                    })
                };
                let ok = match data {
                    InstructionData::Load { .. } => k == 0,
                    InstructionData::Store { .. } => k == 1,
                    // The result is tracked under the same root, or is
                    // itself a distinct `noalias` scope (a param bound to
                    // `base + k`, like `&v[1..]` next to `pivot`): either
                    // way the address stays tracked.
                    InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        ..
                    } => {
                        let res = func.dfg.first_result(i);
                        rt(func, res).0 == r || noalias.contains(&res)
                    }
                    // A comparison can't carry the pointer anywhere.
                    _ if matches!(data.opcode(), Opcode::Icmp) => true,
                    // A call use is never benign: the callee reads the
                    // pointee and can stash the pointer, so the root has
                    // untracked aliases either way.
                    _ if data.opcode().is_call() => false,
                    _ => k >= nargs && fwd(k - nargs),
                };
                if !ok {
                    escapes.entry(r).or_default().push(b);
                    let bounded = data.opcode().is_call()
                        && match data {
                            InstructionData::CallIndirect { .. }
                            | InstructionData::TryCallIndirect { .. } => k.checked_sub(1),
                            _ => Some(k),
                        }
                        .is_some_and(|ai| nocap.contains(&(i, ai as u32)));
                    if !bounded {
                        ncc_bad.insert(r);
                    }
                }
            }
        }
    }
    let ncc: FxHashSet<Root> = escapes
        .keys()
        .filter(|r| !ncc_bad.contains(r))
        .copied()
        .collect();
    let iso: FxHashSet<Root> = cand
        .into_iter()
        .filter(|r| !escapes.contains_key(r))
        .collect();
    (iso, escapes, ncc)
}

/// Run `b`'s transfer function; with `rw`, record forwarded loads.
/// `load (select c, pa, pb)` with both arms forwarding: the result is
/// `select c, aval(pa), aval(pb)`. Keeps the select to scalar types the
/// targets legalize (aarch64 can't pick an i128 or a vector lane-wise).
fn sel_split(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    av: &Avail,
    arg: Value,
    off: i64,
    ty: Type,
) -> Option<Fwd> {
    if !(ty.is_int() && ty.bits() <= 64
        || matches!(
            ty,
            cranelift_codegen::ir::types::F32 | cranelift_codegen::ir::types::F64
        ))
    {
        return None;
    }
    let (r, _) = root(func, ps, na, arg);
    let Root::V(rv) = r else {
        return None;
    };
    let ValueDef::Result(si, _) = func.dfg.value_def(rv) else {
        return None;
    };
    let InstructionData::Ternary {
        opcode: Opcode::Select,
        args,
    } = func.dfg.insts[si]
    else {
        return None;
    };
    let (ra, oa) = norm_root(func, ps, na, args[1]);
    let (rb, ob) = norm_root(func, ps, na, args[2]);
    let t = *av.get(&(ra, oa.wrapping_add(off), ty))?;
    let e = *av.get(&(rb, ob.wrapping_add(off), ty))?;
    Some(Fwd::Sel {
        c: func.dfg.resolve_aliases(args[0]),
        t,
        e,
    })
}

#[allow(clippy::too_many_arguments)]
fn transfer(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    na0: &FxHashSet<Value>,
    b: Block,
    av: &mut Avail,
    rw: Option<&mut Vec<(Inst, Fwd)>>,
    iso: &FxHashSet<Root>,
    ncc: &FxHashSet<Root>,
    inv_insts: &FxHashSet<Inst>,
    invs: &mut FxHashSet<Loc>,
    cur: Option<Region>,
    ofz: &FxHashMap<Value, Vec<(u64, Region)>>,
    invreg: &mut FxHashMap<Loc, Vec<Region>>,
    rpar: &FxHashMap<Region, Region>,
    nw: &FxHashMap<FuncRef, bool>,
    bnd: &FxHashMap<Value, u64>,
    pm: &FxHashMap<Value, (Root, i64)>,
    eh: bool,
) {
    let mut rw = rw;
    // A proven `(root, min offset)` for an address `root` left opaque —
    // `root_min` through bounded addends, then a `pm` hop through a
    // solved param (a `base + 8` cursor posing as `base`). Lets `kill`
    // keep locs the store provably writes above. `off` is the caller's
    // exact offset from the opaque root value.
    let ms_of = |rv: Value, off: i64| {
        let (mr, mo) = root_min(func, ps, na, Some(bnd), rv);
        let (mr, mo) = match mr {
            Root::V(q) => match pm.get(&q) {
                Some(&(qr, qo)) => (qr, mo.wrapping_add(qo)),
                None => (mr, mo),
            },
            _ => (mr, mo),
        };
        match mo.wrapping_add(off) {
            mo if mo != 0 => Some((mr, mo)),
            _ => None,
        }
    };
    for i in func.layout.block_insts(b) {
        let op = func.dfg.insts[i].opcode();
        match func.dfg.insts[i] {
            InstructionData::Load {
                opcode: Opcode::Load,
                arg,
                offset,
                ..
            } if notrap(func, i) => {
                let res = func.dfg.first_result(i);
                let ty = func.dfg.value_type(res);
                // Loc keys use the normalized root — `ofz` bookkeeping
                // keeps the scope-named one, since its byte bound is
                // measured from the operand, not the slot.
                let ((r0, o0), (r, o)) = root_pair(func, ps, na, arg);
                let off = o.wrapping_add(i64::from(i32::from(offset)));
                let off0 = o0.wrapping_add(i64::from(i32::from(offset)));
                let loc = (r, off, ty);
                if pliron_cfg::tracing_on()
                    && tracing::enabled!(target: "pliron::loadfwd", tracing::Level::DEBUG)
                {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::loadfwd", "load {res} arg {arg} root {r:?} off {off} av={:?}",
                            av.get(&loc)
                        )
                    };
                }
                // `!invariant.load`: the location holds one value for the
                // whole program point range the load is legal at — mark it
                // unkillable so later stores can't evict the forwarding.
                if inv_insts.contains(&i) {
                    invs.insert(loc);
                }
                // `*v` is frozen while the clone that binds `v` runs —
                // inside the region, stores can't clobber locs rooted at
                // the operand (`region_frozen_op`). The bytes bound keeps
                // `pivot + 8` from claiming `pivot`'s frozen slot.
                if let Root::V(rv) = r0
                    && let Some(rs) = ofz.get(&rv)
                {
                    for &(bytes, reg) in rs {
                        if off0 >= 0
                            && off0 + i64::from(ty.bytes()) <= bytes as i64
                            && cur.is_some_and(|cur| in_region(cur, reg, rpar))
                        {
                            invreg.entry(loc).or_default().push(reg);
                        }
                    }
                }
                if let Some(&v) = av.get(&loc) {
                    if let Some(rw) = rw.as_deref_mut() {
                        rw.push((i, Fwd::V(v)));
                    }
                } else if let Some(s) = rw
                    .is_some()
                    .then(|| sel_split(func, ps, na, av, arg, off, ty))
                    .flatten()
                {
                    rw.as_deref_mut().unwrap().push((i, s));
                } else if av.len() < MAX_ENTRIES {
                    av.insert(loc, res);
                }
            }
            InstructionData::Store {
                opcode: Opcode::Store,
                args,
                offset,
                ..
            } if notrap(func, i) => {
                let val = func.dfg.resolve_aliases(args[0]);
                let ty = func.dfg.value_type(val);
                let (r, o) = norm_root(func, ps, na, args[1]);
                let o = o.wrapping_add(i64::from(i32::from(offset)));
                let ms = match r {
                    Root::V(rv) => ms_of(rv, o),
                    _ => None,
                };
                kill(av, r, o, ty, ms, iso, ncc, invs, cur, invreg, rpar);
                // `na` includes `ofrozen` stops but `iso`/`ncc` classify
                // under `na0` — the same memory can carry two root names.
                // Kill under the `na0` name too or the store dodges the
                // classification it was made under.
                let (r0, o0) = norm_root(func, ps, na0, args[1]);
                let o0 = o0.wrapping_add(i64::from(i32::from(offset)));
                if r0 != r || o0 != o {
                    kill(av, r0, o0, ty, None, iso, ncc, invs, cur, invreg, rpar);
                }
                // A store through `select c, pa, pb` may write either
                // arm's location — kill both so kept locs don't forward
                // stale values (the select's own root is opaque `V`).
                if let Root::V(rv) = r
                    && let ValueDef::Result(si, _) = func.dfg.value_def(rv)
                    && let InstructionData::Ternary {
                        opcode: Opcode::Select,
                        args: sargs,
                    } = func.dfg.insts[si]
                {
                    for &a in &sargs[1..] {
                        let (ra, oa) = norm_root(func, ps, na, a);
                        let oa = oa.wrapping_add(o);
                        let ms = match ra {
                            Root::V(arv) => ms_of(arv, oa),
                            _ => None,
                        };
                        kill(av, ra, oa, ty, ms, iso, ncc, invs, cur, invreg, rpar);
                        let (ra0, oa0) = norm_root(func, ps, na0, a);
                        let oa0 = oa0.wrapping_add(o);
                        if ra0 != ra || oa0 != oa {
                            kill(av, ra0, oa0, ty, None, iso, ncc, invs, cur, invreg, rpar);
                        }
                    }
                }
                if av.len() < MAX_ENTRIES {
                    av.insert((r, o, ty), val);
                }
            }
            _ if op.is_call() => {
                if !write_free(func, i, nw, eh) {
                    // Roots this call can reach through its args: a
                    // `nocapture` callee can't stash the pointer, but it
                    // may still write the pointee — drop `ncc` locs the
                    // call is actually handed (other `ncc` locs survive:
                    // no lasting alias exists for it to write through).
                    // For `*Indirect` insts the callee value rides arg 0 —
                    // a fn pointer, harmless to include.
                    let mut reach: FxHashSet<Root> = FxHashSet::default();
                    for v in func.dfg.inst_values(i) {
                        reach.insert(norm_root(func, ps, na, v).0);
                        // `iso`/`ncc` were classified under `na0`
                        // (`noalias` alone); a `ofrozen` stop on an arg
                        // (`na` includes them) renames its root, so the
                        // plain-`noalias` root must be in `reach` too or
                        // an ncc loc survives a call that mutates it.
                        reach.insert(norm_root(func, ps, na0, v).0);
                    }
                    av.retain(|&(r, o, t), _| {
                        iso.contains(&r)
                            || (ncc.contains(&r) && !reach.contains(&r))
                            || invs.contains(&(r, o, t))
                            || cur.is_some_and(|cur| {
                                invreg
                                    .get(&(r, o, t))
                                    .is_some_and(|rs| rs.iter().any(|&t2| in_region(cur, t2, rpar)))
                            })
                    });
                }
            }
            _ if op.can_store()
                || op.other_side_effects()
                || matches!(op, Opcode::AtomicLoad | Opcode::Fence) =>
            {
                av.retain(|&(r, o, t), _| {
                    invs.contains(&(r, o, t))
                        || cur.is_some_and(|cur| {
                            invreg
                                .get(&(r, o, t))
                                .is_some_and(|rs| rs.iter().any(|&t2| in_region(cur, t2, rpar)))
                        })
                });
            }
            _ => {}
        }
    }
}

fn meet(acc: &mut Option<Avail>, x: &Avail) {
    match acc {
        None => *acc = Some(x.clone()),
        Some(a) => a.retain(|k, v| x.get(k) == Some(v)),
    }
}

/// Forward loads in `func`; returns the number removed.
/// `inv_insts`: loads rustc marked `!invariant.load` — their locations
/// forward across any store (the pointee can't legally change).
/// `ofrozen`: caller-side operands bound to `noalias readonly` clone
/// params — `*v` is immutable for the clone's region (`breg`/`rpar`
/// scope the immunity, like LLVM's `!alias.scope`/`!noalias`).
#[allow(clippy::too_many_arguments)]
pub fn run(
    func: &mut Function,
    noalias: &FxHashSet<Value>,
    inv_insts: &FxHashSet<Inst>,
    ofrozen: &FxHashMap<Value, Vec<(u64, Region)>>,
    breg: &FxHashMap<Block, Region>,
    rpar: &FxHashMap<Region, Region>,
    nw: &FxHashMap<FuncRef, bool>,
    cfrozen: &FxHashMap<FuncRef, u64>,
    nocap: &FxHashSet<(Inst, u32)>,
) -> usize {
    let Some(entry) = func.layout.entry_block() else {
        return 0;
    };
    let rpo_of = |cfg: &ControlFlowGraph| {
        // Reverse postorder.
        let mut post = Vec::new();
        let mut seen: FxHashSet<Block> = FxHashSet::default();
        let mut stack = vec![(entry, false)];
        while let Some((b, done)) = stack.pop() {
            if done {
                post.push(b);
                continue;
            }
            if !seen.insert(b) {
                continue;
            }
            stack.push((b, true));
            for s in cfg.succ_iter(b) {
                if !seen.contains(&s) {
                    stack.push((s, false));
                }
            }
        }
        post.reverse();
        post
    };
    let prof2 = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::loadfwd", tracing::Level::INFO);
    let t0 = std::time::Instant::now();
    let mut cfg = ControlFlowGraph::with_function(func);
    let mut rpo = rpo_of(&cfg);
    let mut n = 0;
    let pf = crate::proots::facts(func);
    let ps = &crate::proots::alias_of(&pf);
    // `ms` disjointness pays off in `pre` (more locs survive → more
    // redundancy hoisting — fewer loads execute) but not in the plain
    // forwarding pass: there it only swaps a load for a value defined
    // earlier, lengthening live ranges into regalloc pressure (measured
    // ~10% on hashmap's probe loop vs ~0 on sortw's pivot reloads). The
    // fwd half stays behind a flag for now.
    let pre_ms = crate::pass_enabled("PLIRON_MINROOT_PRE");
    let fwd_ms = crate::env_set("PLIRON_MINROOT_FWD");
    let bnd = if pre_ms || fwd_ms {
        crate::proots::bounds(func, &pf)
    } else {
        FxHashMap::default()
    };
    // Root-resolution stop set: real `noalias` plus frozen operands —
    // either way the value names its own memory scope. `isolated` keeps
    // the real set so operand values don't get function-wide immunity.
    let stop: FxHashSet<Value> = noalias.iter().chain(ofrozen.keys()).copied().collect();
    let pm = if pre_ms || fwd_ms {
        pmroots(func, ps, &stop, &bnd, &pf)
    } else {
        FxHashMap::default()
    };
    let (pbnd, ppm): (&FxHashMap<Value, u64>, &FxHashMap<Value, (Root, i64)>) = if pre_ms {
        (&bnd, &pm)
    } else {
        (&FxHashMap::default(), &FxHashMap::default())
    };
    let (fbnd, fpm) = if fwd_ms {
        (&bnd, &pm)
    } else {
        (&FxHashMap::default(), &FxHashMap::default())
    };
    let (iso, _escapes, ncc) = isolated_escapes(func, noalias, ps, cfrozen, nocap);
    if let Some(f) = crate::env_get("PLIRON_LF_DEBUG") {
        if func.name.to_string().contains(&f) {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::loadfwd", "lf-debug {}: iso={:?} ncc={:?} escapes={:?}", func.name, iso, ncc, _escapes)
            };
        }
    }
    if prof2 {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::loadfwd", "    lf-setup {:.3}ms", t0.elapsed().as_secs_f64() * 1e3)
        };
    }
    let tt = |name: &str, t0: std::time::Instant| {
        if prof2 {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::loadfwd", "    lf-{name} {:.3}ms", t0.elapsed().as_secs_f64() * 1e3)
            };
        }
    };
    if crate::pass_enabled("PLIRON_LOADPRE") {
        let t0 = std::time::Instant::now();
        // `pre` needs the real `iso`: its loc-set dataflow runs `kill`,
        // which retains a loc across a store to a different root only when
        // one side is isolated — an empty set would drop every loc at the
        // first unrelated store.
        n += pre(
            func, ps, &stop, noalias, &cfg, &rpo, entry, &iso, &ncc, pbnd, ppm,
        );
        tt("pre", t0);
        if n > 0 {
            cfg = ControlFlowGraph::with_function(func);
            rpo = rpo_of(&cfg);
        }
    }
    let eh = crate::pass_enabled("PLIRON_NOWRITE_EH");
    let mut invs: FxHashSet<Loc> = FxHashSet::default();
    let mut invreg: FxHashMap<Loc, Vec<Region>> = FxHashMap::default();
    let mut out: FxHashMap<Block, Avail> = FxHashMap::default();
    let input = |out: &FxHashMap<Block, Avail>, b: Block| -> Option<Avail> {
        if b == entry {
            return Some(Avail::default());
        }
        let mut acc = None;
        for p in cfg.pred_iter(b) {
            if let Some(o) = out.get(&p.block) {
                if eh && unwind_edge(func, p.inst, b, nw) {
                    let mut o = o.clone();
                    o.retain(|&(r, _, _), _| iso.contains(&r));
                    meet(&mut acc, &o);
                } else {
                    meet(&mut acc, o);
                }
            }
        }
        acc
    };
    let mut stable = false;
    let t0 = std::time::Instant::now();
    let mut iters = 0;
    for _ in 0..MAX_ITERS {
        iters += 1;
        let mut changed = false;
        for &b in &rpo {
            let Some(mut av) = input(&out, b) else {
                continue;
            };
            transfer(
                func,
                ps,
                &stop,
                noalias,
                b,
                &mut av,
                None,
                &iso,
                &ncc,
                inv_insts,
                &mut invs,
                breg.get(&b).copied(),
                ofrozen,
                &mut invreg,
                rpar,
                nw,
                fbnd,
                fpm,
                eh,
            );
            if out.get(&b) != Some(&av) {
                out.insert(b, av);
                changed = true;
            }
        }
        if !changed {
            stable = true;
            break;
        }
    }
    if prof2 {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::loadfwd", "    lf-fxpt {:.3}ms (iters={iters})",
                t0.elapsed().as_secs_f64() * 1e3
            )
        };
    }
    if !stable {
        return 0;
    }
    let t0 = std::time::Instant::now();
    let mut fwd: Vec<(Inst, Fwd)> = Vec::new();
    for &b in &rpo {
        if let Some(mut av) = input(&out, b) {
            transfer(
                func,
                ps,
                &stop,
                noalias,
                b,
                &mut av,
                Some(&mut fwd),
                &iso,
                &ncc,
                inv_insts,
                &mut invs,
                breg.get(&b).copied(),
                ofrozen,
                &mut invreg,
                rpar,
                nw,
                fbnd,
                fpm,
                eh,
            );
        }
    }
    tt("apply", t0);
    let fwd_cnt = fwd.len();
    for (i, f) in fwd {
        let r = func.dfg.first_result(i);
        let v = match f {
            Fwd::V(v) => v,
            Fwd::Sel { c, t, e } => {
                let ty = func.dfg.value_type(r);
                let sel = func.dfg.make_inst(InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args: [c, t, e],
                });
                func.dfg.make_inst_results(sel, ty);
                func.layout.insert_inst(sel, i);
                func.dfg.first_result(sel)
            }
        };
        func.layout.remove_inst(i);
        func.dfg.clear_results(i);
        func.dfg.change_to_alias(r, v);
    }
    n + fwd_cnt
}

/// A location's state inside a block: a known value, or whatever it held on
/// entry (available from every predecessor, possibly as different values).
#[derive(Clone, Copy)]
enum St {
    Local(Value),
    In,
}

struct Pre<'a> {
    cfg: &'a ControlFlowGraph,
    end: FxHashMap<Block, FxHashMap<Loc, St>>,
    start: FxHashMap<(Block, Loc), Value>,
    pending: Vec<(Block, Loc)>,
    /// Compensating loads inserted at the end of a predecessor that lacked
    /// the location (partial-availability PRE): `(block, loc, pred)` → the
    /// inserted load's result, used as the edge arg instead of `end_val`.
    inserted: FxHashMap<(Block, Loc, Block), Value>,
}

fn pred_blocks(cfg: &ControlFlowGraph, b: Block) -> Vec<Block> {
    let mut v: Vec<Block> = cfg.pred_iter(b).map(|p| p.block).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// The value `loc` holds on entry to `b`: the single predecessor's value, or
/// a new block param whose args are filled in later. `None` when the pred's
/// end state lacks `loc` — `ins[b]` comes from the `out` fixpoint while
/// `pre.end` comes from the scan, and the two can disagree (MAX_ENTRIES
/// cap, a back-edge pred not yet scanned).
fn start_val(func: &mut Function, pre: &mut Pre, b: Block, loc: Loc) -> Option<Value> {
    if let Some(&v) = pre.start.get(&(b, loc)) {
        return Some(v);
    }
    let preds = pred_blocks(pre.cfg, b);
    let v = if let [p] = preds[..]
        && p != b
    {
        end_val(func, pre, p, loc)?
    } else {
        pre.pending.push((b, loc));
        func.dfg.append_block_param(b, loc.2)
    };
    pre.start.insert((b, loc), v);
    Some(v)
}

fn end_val(func: &mut Function, pre: &mut Pre, b: Block, loc: Loc) -> Option<Value> {
    match pre.end.get(&b)?.get(&loc) {
        Some(&St::Local(v)) => Some(v),
        Some(St::In) => start_val(func, pre, b, loc),
        None => None,
    }
}

fn kill_st(
    st: &mut FxHashMap<Loc, St>,
    r: Root,
    o: i64,
    ty: Type,
    ms: Option<(Root, i64)>,
    iso: &FxHashSet<Root>,
    ncc: &FxHashSet<Root>,
) {
    let n = i64::from(ty.bytes());
    // Same retain rule as `kill` (minus the invariant sets `pre` doesn't
    // track): `st` must agree with the `out` availability sets or
    // `end_val`'s `St::In` lookup finds a loc `st` dropped.
    let shel = |x: &Root| iso.contains(x) || ncc.contains(x);
    st.retain(|&(r2, o2, t2), _| match (r, r2) {
        _ if r == r2 => o2 >= o + n || o >= o2 + i64::from(t2.bytes()),
        (Root::S(_), Root::S(_)) => true,
        _ => {
            shel(&r)
                || shel(&r2)
                || ms.is_some_and(|(mr, mo)| mr == r2 && mo >= o2 + i64::from(t2.bytes()))
        }
    });
}

/// Whether a store footprint `w = (root, offset, type)` may clobber `l` —
/// the complement of `kill_st`'s keep rule. `ms` is the store address's
/// proven `(root, min offset)`, as in `kill`.
pub(crate) fn may_write(w: Loc, ms: Option<(Root, i64)>, l: Loc) -> bool {
    let (r, o, ty) = w;
    let (r2, o2, t2) = l;
    match (r, r2) {
        _ if r == r2 => !(o2 >= o + i64::from(ty.bytes()) || o >= o2 + i64::from(t2.bytes())),
        (Root::S(_), Root::S(_)) => false,
        _ => !ms.is_some_and(|(mr, mo)| mr == r2 && mo >= o2 + i64::from(t2.bytes())),
    }
}

/// Bound on compensating loads `edge_pre` inserts per function.
const MAX_INSERTS: usize = 64;

/// Partial-availability PRE (LLVM GVN's load PRE on the missing edges): a
/// `notrap` load in `b` whose `loc` is available at the end of some but not
/// all predecessors is made fully available by inserting a compensating
/// copy of the load at the end of each missing predecessor, then merging
/// all arms' values through a new block param (the same `start`/`pending`
/// machinery as fully-available locs; `inserted` supplies the edge arg for
/// the preds that got a fresh load).
///
/// Soundness, checked per missing pred `p` before committing: `p`'s
/// terminator must be a plain branch — a `try_call` edge would need the
/// post-call state but anything inserted before it reads pre-call memory
/// — and the load's address operand must dominate it (a pointer computed
/// in `b` can't be reloaded upstream). Callers guarantee nothing in `b`
/// before the load may write `loc`, so the value the load sees equals the
/// memory state at end of every pred. `notrap` loads are speculatable
/// (invalid memory is UB), so running them on `p`'s sibling edges is safe.
fn edge_pre(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dom: &mut Option<DominatorTree>,
    out: &FxHashMap<Block, FxHashSet<Loc>>,
    reach: &FxHashSet<Block>,
    pre: &mut Pre,
    b: Block,
    i: Inst,
    loc: Loc,
) -> Option<Value> {
    let mut avail = false;
    let mut missing: Vec<(Block, Inst)> = Vec::new();
    for p in cfg.pred_iter(b) {
        if !reach.contains(&p.block) {
            return None;
        }
        if out.get(&p.block).is_some_and(|o| o.contains(&loc)) {
            avail = true;
        } else {
            missing.push((p.block, p.inst));
        }
    }
    missing.sort_unstable();
    missing.dedup();
    // `missing` empty is the fully-available case (`st` would have hit);
    // `avail` false is plain hoisting, not redundancy elimination; a self
    // back-edge among the missing would read memory past the load itself.
    if !avail || missing.is_empty() || missing.iter().any(|&(p, _)| p == b) {
        return None;
    }
    if pre.inserted.len() + missing.len() > MAX_INSERTS {
        return None;
    }
    let InstructionData::Load {
        arg, flags, offset, ..
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let arg = func.dfg.resolve_aliases(arg);
    for &(p, pinst) in &missing {
        if func.layout.last_inst(p) != Some(pinst)
            || !matches!(
                func.dfg.insts[pinst].opcode(),
                Opcode::Jump | Opcode::Brif | Opcode::BrTable
            )
        {
            return None;
        }
        if dom.is_none() {
            *dom = Some(DominatorTree::with_function(func, cfg));
        }
        let ok = match func.dfg.value_def(arg) {
            ValueDef::Result(di, _) => dom.as_ref().unwrap().dominates(di, pinst, &func.layout),
            ValueDef::Param(db, _) => dom.as_ref().unwrap().dominates(db, pinst, &func.layout),
            _ => false,
        };
        if !ok {
            return None;
        }
    }
    let v = match pre.start.get(&(b, loc)) {
        Some(&v) => v,
        None => {
            pre.pending.push((b, loc));
            let v = func.dfg.append_block_param(b, loc.2);
            pre.start.insert((b, loc), v);
            v
        }
    };
    for &(p, pinst) in &missing {
        let li = func.dfg.make_inst(InstructionData::Load {
            opcode: Opcode::Load,
            arg,
            flags,
            offset,
        });
        func.dfg.make_inst_results(li, loc.2);
        func.layout.insert_inst(li, pinst);
        pre.inserted.insert((b, loc, p), func.dfg.first_result(li));
    }
    Some(v)
}

/// Load forwarding with PRE at joins (LLVM GVN's load PRE / LICM scalar
/// promotion for loop-carried fields): a location available from every
/// predecessor, even as different values, becomes a block param. The key
/// sets follow `transfer` exactly, so every needed end value exists.
fn pre(
    func: &mut Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
    na0: &FxHashSet<Value>,
    cfg: &ControlFlowGraph,
    rpo: &[Block],
    entry: Block,
    iso: &FxHashSet<Root>,
    ncc: &FxHashSet<Root>,
    bnd: &FxHashMap<Value, u64>,
    pm: &FxHashMap<Value, (Root, i64)>,
) -> usize {
    let nw: &FxHashMap<FuncRef, bool> = &FxHashMap::default();
    let eh = false;
    let reach: FxHashSet<Block> = rpo.iter().copied().collect();
    let mut out: FxHashMap<Block, FxHashSet<Loc>> = FxHashMap::default();
    let input = |out: &FxHashMap<Block, FxHashSet<Loc>>, b: Block| -> Option<FxHashSet<Loc>> {
        if b == entry || cfg.pred_iter(b).any(|p| !reach.contains(&p.block)) {
            return Some(FxHashSet::default());
        }
        // Seed from the smallest pred set: the intersection can't
        // outgrow it, so a 16k-pred merge pays O(min) per pred instead
        // of O(first-set × preds).
        let mut min: Option<&FxHashSet<Loc>> = None;
        for p in cfg.pred_iter(b) {
            if let Some(o) = out.get(&p.block)
                && min.is_none_or(|m| o.len() < m.len())
            {
                min = Some(o);
            }
        }
        let m = min?;
        let mut acc = m.clone();
        for p in cfg.pred_iter(b) {
            if acc.is_empty() {
                break;
            }
            if let Some(o) = out.get(&p.block)
                && !std::ptr::eq(o, m)
            {
                acc.retain(|k| o.contains(k));
            }
        }
        Some(acc)
    };
    let mut stable = false;
    for _ in 0..MAX_ITERS {
        let mut changed = false;
        for &b in rpo {
            let Some(keys) = input(&out, b) else {
                continue;
            };
            let mut av: Avail = keys
                .into_iter()
                .map(|k| (k, Value::reserved_value()))
                .collect();
            transfer(
                func,
                ps,
                na,
                na0,
                b,
                &mut av,
                None,
                iso,
                ncc,
                // `out` must agree with the `st` scan below, which tracks
                // no invariant immunity — pass empty sets so `transfer`
                // marks and retains nothing extra here. `iso` is real:
                // `kill` needs it to keep locs disjoint from the store's
                // root, or every unrelated store empties the set.
                &FxHashSet::default(),
                &mut FxHashSet::default(),
                None,
                &FxHashMap::default(),
                &mut FxHashMap::default(),
                &FxHashMap::default(),
                nw,
                bnd,
                pm,
                eh,
            );
            let ks: FxHashSet<Loc> = av.into_keys().collect();
            if out.get(&b) != Some(&ks) {
                out.insert(b, ks);
                changed = true;
            }
        }
        if !changed {
            stable = true;
            break;
        }
    }
    if !stable {
        return 0;
    }
    let ins: FxHashMap<Block, FxHashSet<Loc>> = rpo
        .iter()
        .map(|&b| (b, input(&out, b).unwrap_or_default()))
        .collect();
    let mut pre = Pre {
        cfg,
        end: FxHashMap::default(),
        start: FxHashMap::default(),
        pending: Vec::new(),
        inserted: FxHashMap::default(),
    };
    let mut dom: Option<DominatorTree> = None;
    let mut fwd: Vec<(Inst, Value)> = Vec::new();
    for &b in rpo {
        let mut st: FxHashMap<Loc, St> = ins[&b].iter().map(|&l| (l, St::In)).collect();
        // May-write footprints since block start plus a cleared-state flag,
        // so a load miss can tell "never tracked — it still reads
        // block-start memory" from "clobbered on the way" (edge PRE would
        // then feed it the stale end-of-pred value).
        let mut writes: Vec<(Loc, Option<(Root, i64)>)> = Vec::new();
        let mut clobbered = false;
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for i in insts {
            let op = func.dfg.insts[i].opcode();
            match func.dfg.insts[i] {
                InstructionData::Load {
                    opcode: Opcode::Load,
                    arg,
                    offset,
                    ..
                } if notrap(func, i) => {
                    let res = func.dfg.first_result(i);
                    let ty = func.dfg.value_type(res);
                    let (r, o) = norm_root(func, ps, na, arg);
                    let loc = (r, o.wrapping_add(i64::from(i32::from(offset))), ty);
                    if let Some(&s) = st.get(&loc) {
                        // `St::In` with no materializable entry value
                        // (scan/fixpoint divergence) — keep the load.
                        let v = match s {
                            St::Local(v) => Some(v),
                            St::In => start_val(func, &mut pre, b, loc),
                        };
                        match v {
                            Some(v) => {
                                fwd.push((i, v));
                                st.insert(loc, St::Local(v));
                            }
                            None => {
                                st.insert(loc, St::Local(res));
                            }
                        }
                    } else if !clobbered
                        && writes.iter().all(|&(w, ms)| !may_write(w, ms, loc))
                        && let Some(v) =
                            edge_pre(func, cfg, &mut dom, &out, &reach, &mut pre, b, i, loc)
                    {
                        fwd.push((i, v));
                        st.insert(loc, St::Local(v));
                    } else if st.len() < MAX_ENTRIES {
                        st.insert(loc, St::Local(res));
                    }
                }
                InstructionData::Store {
                    opcode: Opcode::Store,
                    args,
                    offset,
                    ..
                } if notrap(func, i) => {
                    let val = func.dfg.resolve_aliases(args[0]);
                    let ty = func.dfg.value_type(val);
                    let (r, o) = norm_root(func, ps, na, args[1]);
                    let o = o.wrapping_add(i64::from(i32::from(offset)));
                    let ms_of = |rv: Value, off: i64| {
                        let (mr, mo) = root_min(func, ps, na, Some(bnd), rv);
                        let (mr, mo) = match mr {
                            Root::V(q) => match pm.get(&q) {
                                Some(&(qr, qo)) => (qr, mo.wrapping_add(qo)),
                                None => (mr, mo),
                            },
                            _ => (mr, mo),
                        };
                        match mo.wrapping_add(off) {
                            mo if mo != 0 => Some((mr, mo)),
                            _ => None,
                        }
                    };
                    let ms = match r {
                        Root::V(rv) => ms_of(rv, o),
                        _ => None,
                    };
                    kill_st(&mut st, r, o, ty, ms, &iso, ncc);
                    writes.push(((r, o, ty), ms));
                    // `na0` name too — `iso`/`ncc` classify under it,
                    // so a store named only under `na`'s `ofrozen` stop
                    // would dodge its kill set (see `transfer`).
                    let (r0, o0) = norm_root(func, ps, na0, args[1]);
                    let o0 = o0.wrapping_add(i64::from(i32::from(offset)));
                    if r0 != r || o0 != o {
                        kill_st(&mut st, r0, o0, ty, None, &iso, ncc);
                        writes.push(((r0, o0, ty), None));
                    }
                    // A store through `select c, pa, pb` may write either
                    // arm's location — kill both, mirroring `transfer` (a
                    // stale `St` here would forward values the store
                    // overwrote).
                    if let Root::V(rv) = r
                        && let ValueDef::Result(si, _) = func.dfg.value_def(rv)
                        && let InstructionData::Ternary {
                            opcode: Opcode::Select,
                            args: sargs,
                        } = func.dfg.insts[si]
                    {
                        for &a in &sargs[1..] {
                            let (ra, oa) = norm_root(func, ps, na, a);
                            let oa = oa.wrapping_add(o);
                            let ms = match ra {
                                Root::V(arv) => ms_of(arv, oa),
                                _ => None,
                            };
                            let al = (ra, oa, ty);
                            kill_st(&mut st, al.0, al.1, al.2, ms, &iso, ncc);
                            writes.push((al, ms));
                            let (ra0, oa0) = norm_root(func, ps, na0, a);
                            let oa0 = oa0.wrapping_add(o);
                            if ra0 != ra || oa0 != oa {
                                let a0 = (ra0, oa0, ty);
                                kill_st(&mut st, a0.0, a0.1, a0.2, None, &iso, ncc);
                                writes.push((a0, None));
                            }
                        }
                    }
                    if st.len() < MAX_ENTRIES {
                        st.insert((r, o, ty), St::Local(val));
                    }
                }
                _ if op.is_call()
                    || op.can_store()
                    || op.other_side_effects()
                    || matches!(op, Opcode::AtomicLoad | Opcode::Fence) =>
                {
                    // `transfer`'s call arm keeps `iso` roots plus `ncc`
                    // roots the call can't reach — retain the same set
                    // here or `st` disagrees with `out`. Same dual
                    // normalization as `transfer`: the classification
                    // ran under `na0`, so both names must be in `reach`.
                    let mut reach: FxHashSet<Root> = FxHashSet::default();
                    for v in func.dfg.inst_values(i) {
                        reach.insert(norm_root(func, ps, na, v).0);
                        reach.insert(norm_root(func, ps, na0, v).0);
                    }
                    st.retain(|&(r, _, _), _| {
                        iso.contains(&r) || (ncc.contains(&r) && !reach.contains(&r))
                    });
                    clobbered = true;
                }
                _ => {}
            }
        }
        pre.end.insert(b, st);
    }
    // Feed every new block param one edge arg per predecessor, resolved
    // from `inserted` or the pred's `end` state. Resolve before mutating:
    // the `st` scan above can disagree with the `out` fixpoint on rare
    // shapes (cap-order divergence, lazy `edge_pre` inserts), leaving a loc
    // out of a pred's `end` — poison those pairs (and, transitively, pairs
    // whose arg would be a poisoned param) and roll their params back out
    // rather than violate edge-arg arity.
    let mut resolved: FxHashMap<(Block, Loc), Vec<(Block, Inst, Value)>> = FxHashMap::default();
    let mut dead: FxHashSet<(Block, Loc)> = FxHashSet::default();
    let mut k = 0;
    while k < pre.pending.len() {
        let (b, loc) = pre.pending[k];
        k += 1;
        if resolved.contains_key(&(b, loc)) {
            continue;
        }
        let mut pinsts: Vec<(Block, Inst)> = cfg.pred_iter(b).map(|p| (p.block, p.inst)).collect();
        pinsts.sort_unstable();
        pinsts.dedup();
        let mut args: Vec<(Block, Inst, Value)> = Vec::with_capacity(pinsts.len());
        let mut bad = false;
        for (pb, pinst) in pinsts {
            let v = match pre.inserted.get(&(b, loc, pb)) {
                Some(&v) => Some(v),
                None => match pre.end.get(&pb).and_then(|e| e.get(&loc)) {
                    Some(&St::Local(v)) => Some(v),
                    Some(&St::In) => start_val(func, &mut pre, pb, loc),
                    _ => None,
                },
            };
            match v {
                Some(v) => args.push((pb, pinst, v)),
                None => {
                    bad = true;
                    break;
                }
            }
        }
        if bad {
            dead.insert((b, loc));
        } else {
            resolved.insert((b, loc), args);
        }
    }
    // A pair dies if it failed to resolve or if an arg is a dead pair's
    // param; propagate the latter to fixpoint (dep order is not guaranteed).
    let mut dead_vals: FxHashSet<Value> = FxHashSet::default();
    let param_of: FxHashMap<Value, (Block, Loc)> =
        pre.start.iter().map(|(&k, &v)| (v, k)).collect();
    loop {
        let mut grown = false;
        for (k, args) in &resolved {
            if dead.contains(k) {
                continue;
            }
            if args
                .iter()
                .any(|&(_, _, v)| param_of.get(&v).is_some_and(|d| dead.contains(d)))
            {
                dead.insert(*k);
                grown = true;
            }
        }
        if !grown {
            break;
        }
    }
    for &k in &dead {
        if let Some(&v) = pre.start.get(&k) {
            dead_vals.insert(v);
            func.dfg.remove_block_param(v);
        }
    }
    for (b, loc) in pre.pending.iter().copied() {
        let Some(args) = resolved.get(&(b, loc)) else {
            continue;
        };
        for &(pb, pinst, v) in args {
            let _ = pb;
            let dfg = &mut func.dfg;
            for bc in dfg.insts[pinst]
                .branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)
            {
                if bc.block(&dfg.value_lists) == b {
                    bc.append_argument(v, &mut dfg.value_lists);
                }
            }
        }
    }
    let mut n = 0;
    for &(i, v) in &fwd {
        if dead_vals.contains(&v) {
            continue;
        }
        let r = func.dfg.first_result(i);
        func.layout.remove_inst(i);
        func.dfg.clear_results(i);
        func.dfg.change_to_alias(r, v);
        n += 1;
    }
    n
}

/// Remove stores into stack slots that nothing reads and whose address never
/// escapes, typically left behind once every load out of the slot has been
/// forwarded (`PLIRON_SLOT_DSE`). Returns the number of stores removed.
pub fn dead_slot_stores(func: &mut Function) -> usize {
    let ps = &param_src(func);
    let na: &FxHashSet<Value> = &FxHashSet::default();
    let mut dead: FxHashSet<StackSlot> = func.sized_stack_slots.keys().collect();
    if dead.is_empty() {
        return 0;
    }
    let mut stores = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let data = &func.dfg.insts[i];
            if let InstructionData::StackAddr {
                opcode, stack_slot, ..
            } = *data
                && opcode != Opcode::StackAddr
            {
                dead.remove(&stack_slot);
            }
            for (k, v) in func.dfg.inst_values(i).enumerate() {
                let Root::S(s) = root(func, ps, na, v).0 else {
                    continue;
                };
                if !dead.contains(&s) {
                    continue;
                }
                let ok = match data {
                    InstructionData::Store { .. } if k == 1 => {
                        stores.push((i, s));
                        true
                    }
                    InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        ..
                    } => root(func, ps, na, func.dfg.first_result(i)).0 == Root::S(s),
                    _ => false,
                };
                if !ok {
                    dead.remove(&s);
                }
            }
        }
    }
    let mut n = 0;
    for (i, s) in stores {
        if dead.contains(&s) {
            func.layout.remove_inst(i);
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::InstBuilder;
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::types;
    use cranelift_codegen::ir::{AbiParam, Signature, UserFuncName};
    use cranelift_codegen::isa::CallConv;

    /// `p` fed `base + 8` on the entry edge and `p + 16` on the back
    /// edge bounds as `≥ base + 8` (the self-feed only raises the
    /// bound); `q` fed a foreign pointer on one edge stays unsolved.
    /// `pmroots` feeds `kill`'s disjointness check only, so a wrong
    /// bound here would clobber a loc that a store could reach — the
    /// assertions pin the exact claim the pass may make.
    #[test]
    fn pmroots_cursor_bounds_and_foreign_source_fails() {
        let mut sig = Signature::new(CallConv::Fast);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let entry = func.dfg.make_block();
        let head = func.dfg.make_block();
        let tail = func.dfg.make_block();
        for b in [entry, head, tail] {
            func.layout.append_block(b);
        }
        let base = func.dfg.append_block_param(entry, types::I64);
        let other = func.dfg.append_block_param(entry, types::I64);
        let p = func.dfg.append_block_param(head, types::I64);
        let q = func.dfg.append_block_param(head, types::I64);
        let r = func.dfg.append_block_param(tail, types::I64);

        let mut cur = FuncCursor::new(&mut func).at_bottom(entry);
        let e = cur.ins().iconst(types::I64, 8);
        let p0 = cur.ins().iadd(base, e);
        cur.ins().jump(head, &[p0.into(), p0.into()]);

        let mut cur = FuncCursor::new(&mut func).at_bottom(head);
        let e1 = cur.ins().iconst(types::I64, 8);
        let p1 = cur.ins().iadd(p, e1);
        let e2 = cur.ins().iconst(types::I64, 8);
        let p2 = cur.ins().iadd(p1, e2);
        let done = cur.ins().icmp(IntCC::Equal, p2, q);
        // `q`'s back edge carries a foreign root — `q` must stay
        // unsolved even though its other edge matches `p`'s bound.
        cur.ins()
            .brif(done, tail, &[q.into()], head, &[p2.into(), other.into()]);

        FuncCursor::new(&mut func)
            .at_bottom(tail)
            .ins()
            .return_(&[r]);

        let pf = crate::proots::facts(&func);
        let ps = crate::proots::alias_of(&pf);
        let bnd = crate::proots::bounds(&func, &pf);
        let na: FxHashSet<Value> = FxHashSet::default();
        let pm = pmroots(&func, &ps, &na, &bnd, &pf);
        assert_eq!(pm.get(&p), Some(&(Root::V(base), 8)));
        assert!(!pm.contains_key(&q));
    }
}
