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
    Block, Function, Inst, InstBuilder, InstructionData, Opcode, StackSlot, Type, Value, ValueDef, types,
};
use crate::vmap::VMap;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

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
    /// Bits `shift..shift + bits(mty)` of a wider stored integer `v`,
    /// extended to the load's result type when it differs (`uloadN`/
    /// `sloadN`, `signed` for the latter). Little-endian.
    Narrow { v: Value, shift: u32, mty: Type, signed: bool },
    /// Re-read `rty`-typed `op` bytes from `(addr, offset)` — the
    /// covering store's value is itself a `load` and the source bytes
    /// are still available, so the stack round-trip is bypassed (LLVM
    /// memcpyopt's memcpy→load forwarding: brotli's `sub_match!` reads
    /// u64s back out of `clone_from_slice` buffers).
    SrcLoad {
        op: Opcode,
        addr: Value,
        offset: i32,
        rty: Type,
        flags: cranelift_codegen::ir::MemFlagsData,
    },
}

/// An available integer covering `[off, off + bytes)` of root `r` from a
/// wider store: `(value, bit shift)`. regex's `cached_state` stored an
/// `ip: usize` to a stack slot and reloaded its low half (`u32::try_from`).
fn narrow_src(func: &Function, av: &Avail, r: Root, off: i64, bytes: u32) -> Option<(Value, u32)> {
    av.iter().find_map(|(&(r2, o2, t2), &v)| {
        (r2 == r
            && t2.is_int()
            && !t2.is_vector()
            && t2.bits() <= 64
            && t2.bytes() > bytes
            && func.dfg.value_type(v) == t2
            && o2 <= off
            && off + i64::from(bytes) <= o2 + i64::from(t2.bytes()))
        .then(|| (v, ((off - o2) * 8) as u32))
    })
}

/// Bounded constant evaluation for addend-shaped expressions: `iconst`,
/// extends, and arithmetic on constants. `bunroll`'s clone constants
/// reach the loads wrapped in `ishl`/`imul`/`uextend` chains —
/// `(idx<<3)*1` — that the plain `iconst` peel can't see as an offset.
fn keval(func: &Function, ps: &VMap<Value>, na: &FxHashSet<Value>, v: Value, d: u32) -> Option<i64> {
    if d == 0 {
        return None;
    }
    let v = func.dfg.resolve_aliases(psrc(ps, na, func, v));
    let i = func.dfg.value_def(v).inst()?;
    Some(match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => imm.bits(),
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => {
            let a = func.dfg.resolve_aliases(arg);
            let bits = func.dfg.value_type(a).bits();
            let k = keval(func, ps, na, arg, d - 1)?;
            if bits >= 64 { k } else { k & ((1i64 << bits) - 1i64) }
        }
        InstructionData::Unary {
            opcode: Opcode::Sextend,
            arg,
        } => {
            let a = func.dfg.resolve_aliases(arg);
            let bits = func.dfg.value_type(a).bits();
            let k = keval(func, ps, na, arg, d - 1)?;
            if bits >= 64 { k } else { (k << (64 - bits)) >> (64 - bits).min(63) }
        }
        InstructionData::Binary { opcode, args } => {
            let a = keval(func, ps, na, args[0], d - 1)?;
            let b = keval(func, ps, na, args[1], d - 1)?;
            let sh = (b as u64 % u64::from(func.dfg.ctrl_typevar(i).bits())) as u32;
            match opcode {
                Opcode::Iadd => a.wrapping_add(b),
                Opcode::Isub => a.wrapping_sub(b),
                Opcode::Imul => a.wrapping_mul(b),
                Opcode::Ishl => a.wrapping_shl(sh),
                Opcode::Ushr => ((a as u64) >> sh) as i64,
                Opcode::Band => a & b,
                Opcode::Bor => a | b,
                Opcode::Bxor => a ^ b,
                _ => return None,
            }
        }
        _ => return None,
    })
}

/// `nm.pair` plus an `iadd base, <const expr>` peel — the offset the
/// constant-evaluated addend contributes. Used only on the `SRCCOPY`
/// path so the rest of the machinery's loc naming is untouched.
fn kpair(
    func: &Function,
    ps: &VMap<Value>,
    na: &FxHashSet<Value>,
    nm: &NrMemo,
    v: Value,
) -> ((Root, i64), (Root, i64)) {
    let mut v = func.dfg.resolve_aliases(psrc(ps, na, func, v));
    let mut off = 0i64;
    for _ in 0..8 {
        let Some(i) = func.dfg.value_def(v).inst() else { break };
        let InstructionData::Binary {
            opcode: Opcode::Iadd,
            args,
        } = func.dfg.insts[i]
        else {
            break;
        };
        if let Some(k) = keval(func, ps, na, args[1], 6) {
            off = off.wrapping_add(k);
            v = func.dfg.resolve_aliases(psrc(ps, na, func, args[0]));
        } else if let Some(k) = keval(func, ps, na, args[0], 6) {
            off = off.wrapping_add(k);
            v = func.dfg.resolve_aliases(psrc(ps, na, func, args[1]));
        } else {
            break;
        }
    }
    let ((r0, o0), (r, o)) = nm.pair(func, ps, na, v);
    ((r0, o0.wrapping_add(off)), (r, o.wrapping_add(off)))
}

/// `r[off .. off+bytes)` sits inside a covering store `store v` whose `v`
/// is itself a plain `load (q, qoff)` and the source location still maps
/// to `v` (no may-alias store since — the availability kill rules carry
/// that): the load can re-read the original memory instead of the copy.
/// `q` dominates the source load, which dominates the copy store, which
/// is available at this point, so the emitted address is in scope.
fn copy_src(
    func: &Function,
    ps: &VMap<Value>,
    na: &FxHashSet<Value>,
    nm: &NrMemo,
    av: &Avail,
    r: Root,
    off: i64,
    mty: Type,
) -> Option<(Value, i32, cranelift_codegen::ir::MemFlagsData)> {
    let bytes = i64::from(mty.bytes());
    let (v, o2, t2) = av
        .iter()
        .filter(|(l, _)| l.0 == r && l.1 <= off && off + bytes <= l.1 + i64::from(l.2.bytes()))
        .max_by_key(|(l, _)| (i64::from(l.2.bytes()), l.1))
        .map(|(l, &v)| (v, l.1, l.2))?;
    let ValueDef::Result(si, _) = func.dfg.value_def(func.dfg.resolve_aliases(v)) else {
        return None;
    };
    let InstructionData::Load { arg: q, offset: qo, .. } = func.dfg.insts[si] else {
        return None;
    };
    if func.dfg.insts[si].opcode() != Opcode::Load || !notrap(func, si) {
        return None;
    }
    let (rq, oq) = nm.pair(func, ps, na, q).1;
    let base = oq.wrapping_add(i64::from(i32::from(qo)));
    if av.get(&(rq, base, t2)) != Some(&v) {
        return None;
    }
    let d = off - o2;
    // `q`'s own normalization already carries `oq` — the emitted
    // `load (q, off)` covers only the field offset plus the byte delta.
    let offset = i32::try_from(i64::from(i32::from(qo)).checked_add(d)?).ok()?;
    let sdata = func.dfg.insts[si].memflags_data(&func.dfg)?;
    let mut flags = cranelift_codegen::ir::MemFlagsData::new().with_notrap();
    if let Some(e) = sdata.explicit_endianness() {
        flags = flags.with_endianness(e);
    }
    if sdata.aligned() && d % bytes == 0 {
        flags = flags.with_aligned();
    }
    Some((q, offset, flags))
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
pub fn in_region(cur: Region, target: Region, rpar: &pliron::cols::ColMap<Region, Region>) -> bool {
    let mut r = Some(cur);
    while let Some(x) = r {
        if x == target {
            return true;
        }
        r = rpar.get(x).copied().filter(|&n| n != x);
    }
    false
}

/// Block params that always carry one value: every predecessor passes the
/// same argument (or the param itself — a loop-carried passthrough, like
/// `partition`'s `pivot` threaded through the scan loop). Such a param is
/// that value for analysis purposes. Maps param → that value.
/// `proots::alias_map` computes the relation (reachable edges only —
/// args on unreachable edges are indistinguishable from absent ones).
pub fn param_src(func: &Function) -> VMap<Value> {
    crate::proots::alias_map(func)
}

/// Chase `param_src` links (plus value aliases) to the value `v` provably
/// always equals. Bounded: param chains are short, and cycles can't occur
/// in valid SSA — a param can only feed blocks it dominates.
pub fn psrc(
    ps: &VMap<Value>,
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
    ps: &VMap<Value>,
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
    ps: &VMap<Value>,
    na: &FxHashSet<Value>,
    bnd: Option<&VMap<u64>>,
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
    ps: &VMap<Value>,
    na: &FxHashSet<Value>,
    bnd: &VMap<u64>,
    pf: &crate::proots::Facts,
) -> VMap<(Root, i64)> {
    let blocked: FxHashSet<Value> = pf.blocked.iter().copied().collect();
    let mut byp: FxHashMap<Value, Vec<Value>> = FxHashMap::default();
    for &(p, a) in &pf.edge {
        byp.entry(p).or_default().push(a);
    }
    let mut pm: VMap<(Root, i64)> = VMap::default();
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
pub fn concrete(func: &Function, ps: &VMap<Value>, v: Value) -> (Root, i64) {
    root(func, ps, &FxHashSet::default(), v)
}

/// `root` normalized for disjointness/dereferenceability checks: when a
/// `V` scope name (`noalias` param, operand-frozen operand) peels to a
/// `stack_addr`, report the slot itself so two scoped pointers into
/// different locals compare disjoint like any other slot pair.
pub fn norm_root(
    func: &Function,
    ps: &VMap<Value>,
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
    ps: &VMap<Value>,
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

/// Per-phase memo of root/normalization walks — `ps`/`na`/`na0`/
/// `bnd`/`pm` are fixed while a phase analyzes (`func` mutates only
/// through the collected edits and add-only `pre` inserts), so a base
/// pointer repeats its ≤16-hop chain walk once per phase, not once
/// per load/store/fixpoint round. `pair` also serves `norm_root`
/// (it's `root_pair`'s `.1`); `n0` covers the `na0` name; `rm` is
/// `ms_of`'s bounded `root_min`.
#[derive(Default)]
struct NrMemo {
    pair: std::cell::RefCell<FxHashMap<Value, ((Root, i64), (Root, i64))>>,
    n0: std::cell::RefCell<FxHashMap<Value, (Root, i64)>>,
    rm: std::cell::RefCell<FxHashMap<Value, (Root, i64)>>,
}

impl NrMemo {
    fn pair(
        &self,
        func: &Function,
        ps: &VMap<Value>,
        na: &FxHashSet<Value>,
        v: Value,
    ) -> ((Root, i64), (Root, i64)) {
        *self
            .pair
            .borrow_mut()
            .entry(v)
            .or_insert_with(|| root_pair(func, ps, na, v))
    }

    fn n0(
        &self,
        func: &Function,
        ps: &VMap<Value>,
        na0: &FxHashSet<Value>,
        v: Value,
    ) -> (Root, i64) {
        *self
            .n0
            .borrow_mut()
            .entry(v)
            .or_insert_with(|| norm_root(func, ps, na0, v))
    }

    fn rm(
        &self,
        func: &Function,
        ps: &VMap<Value>,
        na: &FxHashSet<Value>,
        bnd: &VMap<u64>,
        v: Value,
    ) -> (Root, i64) {
        *self
            .rm
            .borrow_mut()
            .entry(v)
            .or_insert_with(|| root_min(func, ps, na, Some(bnd), v))
    }
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
    rpar: &pliron::cols::ColMap<Region, Region>,
) {
    let n = i64::from(ty.bytes());
    // `iso` roots can't be reached by untracked pointers at all; `ncc`
    // roots only escape through `nocapture` call operands, so no
    // *lasting* untracked alias exists — for a store's kill set the two
    // are equivalent (calls that receive the root still clobber it —
    // that's the call arm's job, not the store rule's).
    let shel = |x: &Root| iso.contains(x) || ncc.contains(x);
    av.retain(|&(r2, o2, t2), _| {
        // Both immunities below are about stores through *other*
        // pointers. A store through the loc's own root that overlaps it
        // is a real write (the region tag can be stale: CFG merging left
        // smallvec `into_iter`'s `set_len(0)` store in a block still
        // tagged with the inlined `len(&self)` clone — double drop) or
        // UB; killing is sound either way.
        let own = r == r2 && !(o2 >= o.saturating_add(n) || o >= o2.saturating_add(i64::from(t2.bytes())));
        // An `!invariant.load` location holds the same value for the
        // load's whole scope: a write to it through another pointer would
        // be UB, so such a store can't clobber it.
        if !own && invs.contains(&(r2, o2, t2)) {
            return true;
        }
        // A region-frozen operand's pointee can't be written while the
        // clone runs — a store inside the region can't clobber it. This
        // is LLVM's scoped `!noalias`: `*pivot` survives `partition`'s
        // element swaps for the clone's whole span.
        if !own
            && let Some(cur) = cur
            && invreg
                .get(&(r2, o2, t2))
                .is_some_and(|rs| rs.iter().any(|&t| in_region(cur, t, rpar)))
        {
            return true;
        }
        match (r, r2) {
            _ if r == r2 => o2 >= o.saturating_add(n) || o >= o2.saturating_add(i64::from(t2.bytes())),
            (Root::S(_), Root::S(_)) => true,
            _ => {
                shel(&r)
                    || shel(&r2)
                    || ms.is_some_and(|(mr, mo)| mr == r2 && mo >= o2.saturating_add(i64::from(t2.bytes())))
            }
        }
    });
}

/// The other half of the dual-name rule: a loc keyed under an `ofrozen`
/// stop name `V(x)` (`x` in `na` but not in `na0`) is memory that also
/// has a plain name, `n0(x)` — the store arms' extra `na0` kill covers a
/// store made through the stop name, not a loc kept under it. `kill`
/// compares the loc's stop root with the store's plain root, finds them
/// different and keeps the loc whenever the plain root is `iso`/`ncc`.
/// But `iso` only rules out *untracked* pointers, and `x` is derived from
/// that same root: `store 7 -> v32; store 3 -> v6+24; load v32`, with
/// `v32 = v6 + 24` an operand-frozen operand, forwarded 7 (fuzz seed
/// 16782, an infinite loop). Translate each such loc into its plain name
/// and test it against the store's plain footprint `w`. The same object
/// through two names is the store's own write: no immunity applies, as
/// for `kill`'s `own`. Otherwise the loc keeps the immunities `imm`
/// grants (`!invariant.load`, region-frozen) and `kill`'s root rules.
/// `stop0(x)` is `Some(n0(x))` exactly for stop-only names.
fn kill_stops<T>(
    av: &mut FxHashMap<Loc, T>,
    w: Loc,
    stop0: &dyn Fn(Value) -> Option<(Root, i64)>,
    iso: &FxHashSet<Root>,
    ncc: &FxHashSet<Root>,
    imm: &dyn Fn(&Loc) -> bool,
) {
    let (r, o, ty) = w;
    let n = i64::from(ty.bytes());
    let shel = |x: &Root| iso.contains(x) || ncc.contains(x);
    av.retain(|&(r2, o2, t2), _| {
        let Root::V(x) = r2 else {
            return true;
        };
        let Some((rx, ox)) = stop0(x) else {
            return true;
        };
        let ot = ox.wrapping_add(o2);
        if rx == r {
            return ot >= o.saturating_add(n) || o >= ot.saturating_add(i64::from(t2.bytes()));
        }
        if imm(&(r2, o2, t2)) {
            return true;
        }
        match (r, rx) {
            (Root::S(_), Root::S(_)) => true,
            _ => shel(&r) || shel(&rx),
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
    ps: &VMap<Value>,
    cfrozen: &FxHashMap<FuncRef, u64>,
    scoped: &FxHashSet<Value>,
) -> FxHashSet<Root> {
    isolated_escapes(func, noalias, ps, cfrozen, &FxHashMap::default(), scoped).0
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
/// `scoped` is the subset of `noalias` whose claim is region-scoped
/// (clone args seeded from `region_frozen`/`region_noalias`): inside
/// the clone nothing else touches the pointee, but a caller-side access
/// through the underlying object can still reach it, so the root is not
/// isolated function-wide. They stay in `noalias` (scope-keyed locs and
/// the region checks need the `root` stop) but are excluded here.
pub fn isolated_escapes(
    func: &Function,
    noalias: &FxHashSet<Value>,
    ps: &VMap<Value>,
    _cfrozen: &FxHashMap<FuncRef, u64>,
    nocap: &FxHashMap<(Inst, u32), u64>,
    scoped: &FxHashSet<Value>,
) -> (
    FxHashSet<Root>,
    FxHashMap<Root, Vec<Block>>,
    FxHashSet<Root>,
) {
    isolated_escapes_x(func, noalias, ps, _cfrozen, nocap, scoped, false)
}

/// `isolated_escapes`, optionally (`trap_benign`) ignoring uses in blocks
/// that end in `trap` and load nothing. Such a block's escapes (a panic's
/// `fmt::Arguments` holding `&x`) are followed by no code of this frame —
/// a plain `call` unwinds past it — so they can't make a load anywhere
/// else stale. Only for the reading side (forwarding): a store whose sole
/// reader is that escaping call is still live, so DSE/promotion must keep
/// the strict answer.
thread_local! {
    /// `Pipe::dump` for the function loadfwd is running on: `PLIRON_LF_DEBUG=dump`
    /// prints the root verdicts and every escape site of that function only.
    pub static LF_DUMP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The function's symbol, for `PLIRON_OPT_BISECT_DEBUG` lines.
    pub static LF_SYM: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

fn lf_dump() -> bool {
    LF_DUMP.with(|c| c.get())
}

pub fn isolated_escapes_x(
    func: &Function,
    noalias: &FxHashSet<Value>,
    ps: &VMap<Value>,
    _cfrozen: &FxHashMap<FuncRef, u64>,
    nocap: &FxHashMap<(Inst, u32), u64>,
    scoped: &FxHashSet<Value>,
    trap_benign: bool,
) -> (
    FxHashSet<Root>,
    FxHashMap<Root, Vec<Block>>,
    FxHashSet<Root>,
) {
    let mut cand: FxHashSet<Root> = noalias
        .iter()
        .filter(|&v| !scoped.contains(v))
        .map(|&v| Root::V(v))
        .collect();
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
    // Blocks no path from the entry reaches can't escape anything: a pass
    // that folds a branch (looprot, licm2) leaves the dead arm in the layout
    // until the next unreachable sweep, and its call kept regex's `qcur`
    // escaped through licm3 (`PLIRON_ESC_REACH=0` scans every block).
    static REACH: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let reach: Option<FxHashSet<Block>> = REACH
        .get_or_init(|| crate::pass_enabled("PLIRON_ESC_REACH"))
        .then(|| {
            let mut seen = FxHashSet::default();
            let mut stack: Vec<Block> = func.layout.entry_block().into_iter().collect();
            while let Some(b) = stack.pop() {
                if !seen.insert(b) {
                    continue;
                }
                if let Some(t) = func.layout.last_inst(b) {
                    for c in func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                        stack.push(c.block(&func.dfg.value_lists));
                    }
                }
            }
            seen
        });
    let dead_end = |b: Block| {
        func.layout.last_inst(b).is_some_and(|t| func.dfg.insts[t].opcode() == Opcode::Trap)
            && func.layout.block_insts(b).all(|i| !func.dfg.insts[i].opcode().can_load())
    };
    for b in func.layout.blocks() {
        if reach.as_ref().is_some_and(|r| !r.contains(&b)) {
            continue;
        }
        if trap_benign && dead_end(b) {
            continue;
        }
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
                    // The result must itself normalize to `r`: a `base + k`
                    // cursor keys under `r` only when the peel reaches the
                    // same object. A scoped (`noalias`) result that does NOT
                    // normalize to `r` — e.g. `slot + i*16`, whose demand
                    // keys under `V(res)` — is an untracked alias of the
                    // slot's memory, not a benign use: keeping the slot
                    // isolated would let stores die under demands rooted at
                    // the scope name (trait-object-arrays-11205 SIGSEGV).
                    InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        ..
                    } => {
                        let res = func.dfg.first_result(i);
                        rt(func, res).0 == r
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
                    if lf_dump() && pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::loadfwd", "lf-escape {:?} at {}", r, func.dfg.display_inst(i));
                    }
                    escapes.entry(r).or_default().push(b);
                    let bounded = data.opcode().is_call()
                        && match data {
                            InstructionData::CallIndirect { .. }
                            | InstructionData::TryCallIndirect { .. } => k.checked_sub(1),
                            _ => Some(k),
                        }
                        .is_some_and(|ai| nocap.contains_key(&(i, ai as u32)));
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

/// Function-local `captures(none)` for licm's `noalias` shelter — the
/// same judgement LLVM's FunctionAttrs makes on `ptr noalias` params,
/// which rustc's `deduced_param_attrs` never computes for references
/// (`type_could_benefit_from_deduction` only covers by-value params).
/// `isolated_escapes` answers a stricter question: its "escape" is any
/// use it can't key under the root (`iadd v1, i` — a variable-index
/// field address — escapes even though the result still provably
/// carries `v1`'s provenance inside the function). Shelter needs the
/// weaker claim: no `v1`-based value reaches *memory* or an opaque
/// consumer, so nothing loaded in a loop can secretly be `v1`-based.
///
/// `taint[v] & bit(c)` — `v` may carry candidate `c`'s provenance:
/// `c` itself plus pointer-width results of ops fed by tainted operands
/// (loads and calls launder — their results hold whatever memory or the
/// callee had, so for a candidate that never escapes they stay clean —
/// and sub-pointer-width results can't hold a pointer anyway) plus
/// block params a tainted edge arg feeds.
///
/// `esc[b] & bit(c)` — a `c`-based value left SSA tracking in `b`:
/// stored into memory, passed to a call operand the callee may keep
/// (not `nocap`), returned, handed to an indirect/tail call, or eaten
/// by an op with no provenance model. A loop shelters `c` when none of
/// `c`'s escape blocks can reach its header (`licm`'s `reachers`).
#[derive(Default)]
pub struct ProvEsc {
    /// Candidate value → its mask bit.
    pub bit: FxHashMap<Value, u64>,
    /// Value → mask of candidates it may be based on.
    pub taint: FxHashMap<Value, u64>,
    /// Block → mask of candidates whose provenance externalizes there.
    pub esc: FxHashMap<Block, u64>,
}

/// Worklist over `uses`: each candidate owns one bit, so a single pass
/// propagates every candidate's provenance at once. `None` on >63
/// candidates or an over-budget walk — the caller treats it as "nothing
/// is sheltered", which is conservative.
pub fn provescapes(
    func: &Function,
    cands: &FxHashSet<Value>,
    pf: &crate::proots::Facts,
    nocap: &FxHashMap<(Inst, u32), u64>,
) -> Option<ProvEsc> {
    if cands.is_empty() {
        return Some(ProvEsc::default());
    }
    if cands.len() > 63 {
        return None;
    }
    // `bit` keys the raw candidate — that is the `Root::V` name
    // `norm_root` returns for a `noalias` member (its raw value is the
    // scope name; `root_min` deliberately stops before resolving), while
    // `taint`/`esc` work on resolved values where the uses live.
    let bit: FxHashMap<Value, u64> = cands
        .iter()
        .enumerate()
        .map(|(k, &v)| (v, 1u64 << k))
        .collect();
    // A pointer's provenance rides on pointer-width values only: `icmp`'s
    // i8 and narrow ints stay clean, so `select(cnd, p, q)` doesn't
    // inherit a tainted condition's mask.
    let pbits = cands
        .iter()
        .map(|&m| func.dfg.value_type(m).bits())
        .min()
        .unwrap_or(64);
    // Edge arg → params it feeds (`pf.edge` is (param, arg)).
    let mut feeds: FxHashMap<Value, Vec<Value>> = FxHashMap::default();
    for &(p, a) in &pf.edge {
        feeds.entry(a).or_default().push(p);
    }
    let uses = crate::uses::Uses::new(func);
    let mut taint: FxHashMap<Value, u64> = FxHashMap::default();
    for (&v, &m) in &bit {
        // Two `noalias` names can alias-resolve to one value.
        *taint.entry(func.dfg.resolve_aliases(v)).or_insert(0) |= m;
    }
    // `seen[v]` = the candidate mask `v`'s sites were already processed
    // with — starts empty so a seed's first pop propagates its bit.
    let mut seen: FxHashMap<Value, u64> = FxHashMap::default();
    let mut esc: FxHashMap<Block, u64> = FxHashMap::default();
    let mut work: Vec<Value> = cands.iter().map(|&v| func.dfg.resolve_aliases(v)).collect();
    let mut steps = 0usize;
    while let Some(v) = work.pop() {
        let m = taint[&v] & !seen.get(&v).copied().unwrap_or(0);
        seen.insert(v, seen.get(&v).copied().unwrap_or(0) | m);
        if m == 0 {
            continue;
        }
        for &(b, i) in uses.sites(v) {
            steps += 1;
            if steps > 8 * func.dfg.num_values() {
                return None;
            }
            let data = &func.dfg.insts[i];
            let op = data.opcode();
            let nargs = func.dfg.inst_args(i).len();
            let mut used = false;
            for (k, a) in func.dfg.inst_values(i).enumerate() {
                if func.dfg.resolve_aliases(a) != v {
                    continue;
                }
                if k >= nargs {
                    // Edge arg: provenance flows on to the fed params.
                    if let Some(ps) = feeds.get(&v) {
                        for &p in ps {
                            let e = taint.entry(p).or_insert(0);
                            if *e & m != m {
                                *e |= m;
                                work.push(p);
                            }
                        }
                    }
                    continue;
                }
                used = true;
                // Does this operand use let the provenance out of SSA
                // tracking? A `v`-based *address* (store/atomic arg) is
                // still inside the pointee — it can write `*c` but can't
                // plant `c` anywhere else.
                let leak = if op.is_branch() {
                    false
                } else if op.is_return() {
                    true
                } else if op.is_call() {
                    match data {
                        InstructionData::Call { .. } => {
                            !nocap.contains_key(&(i, k as u32))
                        }
                        InstructionData::CallIndirect { .. }
                        | InstructionData::TryCallIndirect { .. } => {
                            k == 0 || !nocap.contains_key(&(i, (k - 1) as u32))
                        }
                        // Tail calls hand the callee the frame; try_call
                        // splits its operand list across call and edge
                        // args — either way the operand can be kept.
                        _ => true,
                    }
                } else if op.can_store() {
                    match op {
                        // `x` (the value written) is operand 0; `p` the
                        // address is operand 1.
                        Opcode::Store
                        | Opcode::Istore8
                        | Opcode::Istore16
                        | Opcode::Istore32
                        | Opcode::AtomicStore
                        | Opcode::Vscatter => k == 0,
                        // `atomic_rmw`/`atomic_cas` put `p` first.
                        Opcode::AtomicRmw | Opcode::AtomicCas => k > 0,
                        _ => true,
                    }
                } else if op.can_load() {
                    false
                } else if func.dfg.inst_results(i).is_empty()
                    || op.is_terminator()
                    || op.other_side_effects()
                {
                    true
                } else {
                    false
                };
                if leak {
                    *esc.entry(b).or_default() |= m;
                    tracing::debug!(target: "pliron::loadfwd", "pe-esc {} {b} m={m:#x} k={k}: {}", func.name, func.dfg.display_inst(i));
                }
            }
            if !used || op.is_call() || op.can_load() || op.is_branch() {
                continue;
            }
            // Producer: results wide enough to hold a pointer inherit
            // the operand's provenance. Call and load results are
            // excluded — they carry whatever the callee or memory held,
            // which is `c`-based only after an escape (and an escaped
            // candidate is never live).
            for &r in func.dfg.inst_results(i) {
                if func.dfg.value_type(r).bits() < pbits {
                    continue;
                }
                let e = taint.entry(r).or_insert(0);
                if *e & m != m {
                    *e |= m;
                    work.push(r);
                }
            }
        }
    }
    Some(ProvEsc { bit, taint, esc })
}

/// Run `b`'s transfer function; with `rw`, record forwarded loads.
/// `load (select c, pa, pb)` with both arms forwarding: the result is
/// `select c, aval(pa), aval(pb)`. Keeps the select to scalar types the
/// targets legalize (aarch64 can't pick an i128 or a vector lane-wise).
fn sel_split(
    func: &Function,
    ps: &VMap<Value>,
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
    ps: &VMap<Value>,
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
    rpar: &pliron::cols::ColMap<Region, Region>,
    nw: &FxHashMap<FuncRef, bool>,
    bnd: &VMap<u64>,
    pm: &VMap<(Root, i64)>,
    eh: bool,
    nm: &NrMemo,
) {
    let mut rw = rw;
    // `na` is `na0` plus the `ofrozen` stop names (`run`'s `stop`): any
    // extra member means a loc can be keyed under a stop name.
    let has_stops = na.len() > na0.len();
    // A proven `(root, min offset)` for an address `root` left opaque —
    // `root_min` through bounded addends, then a `pm` hop through a
    // solved param (a `base + 8` cursor posing as `base`). Lets `kill`
    // keep locs the store provably writes above. `off` is the caller's
    // exact offset from the opaque root value.
    let ms_of = |rv: Value, off: i64| {
        let (mr, mo) = nm.rm(func, ps, na, bnd, rv);
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
                let ((r0, o0), (r, o)) = nm.pair(func, ps, na, arg);
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
                            && off0.saturating_add(i64::from(ty.bytes())) <= bytes as i64
                            && cur.is_some_and(|cur| in_region(cur, reg, rpar))
                        {
                            if pliron_cfg::tracing_on() {
                                tracing::debug!(target: "pliron::loadfwd",
                                    "frozen: {res} loc {loc:?} region {reg:?} (cur {cur:?}, operand {rv})")
                            };
                            invreg.entry(loc).or_default().push(reg);
                        }
                    }
                }
                if let Some(&v) = av.get(&loc) {
                    if let Some(rw) = rw.as_deref_mut() {
                        rw.push((i, Fwd::V(v)));
                    }
                } else if *SRCCOPY
                    && rw.is_some()
                    && let Some((addr, offset, flags)) = {
                        let (_, (r2, o2)) = kpair(func, ps, na, nm, arg);
                        let o2 = o2.wrapping_add(i64::from(i32::from(offset)));
                        copy_src(func, ps, na, nm, av, r2, o2, ty)
                    }
                {
                    rw.as_deref_mut().unwrap().push((
                        i,
                        Fwd::SrcLoad { op: Opcode::Load, addr, offset, rty: ty, flags },
                    ));
                    av.insert(loc, res);
                } else if *NARROW
                    && ty.is_int()
                    && !ty.is_vector()
                    && let Some(rw) = rw.as_deref_mut()
                    && let Some((v, shift)) = narrow_src(func, av, r, off, ty.bytes())
                {
                    rw.push((i, Fwd::Narrow { v, shift, mty: ty, signed: false }));
                    av.insert(loc, res);
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
            // `uloadN`/`sloadN` of an available value (exact width, or the
            // low/high part of a wider store): extract and extend.
            InstructionData::Load { opcode: lop, arg, offset, .. }
                if *NARROW
                    && rw.is_some()
                    && notrap(func, i)
                    && matches!(
                        lop,
                        Opcode::Uload8 | Opcode::Uload16 | Opcode::Uload32 | Opcode::Sload8 | Opcode::Sload16 | Opcode::Sload32
                    ) =>
            {
                let mty = match lop {
                    Opcode::Uload8 | Opcode::Sload8 => types::I8,
                    Opcode::Uload16 | Opcode::Sload16 => types::I16,
                    _ => types::I32,
                };
                let signed = matches!(lop, Opcode::Sload8 | Opcode::Sload16 | Opcode::Sload32);
                let (r, o) = nm.pair(func, ps, na, arg).1;
                let off = o.wrapping_add(i64::from(i32::from(offset)));
                let hit = av.get(&(r, off, mty)).map(|&v| (v, 0)).or_else(|| narrow_src(func, av, r, off, mty.bytes()));
                if let Some((v, shift)) = hit {
                    rw.as_deref_mut().unwrap().push((i, Fwd::Narrow { v, shift, mty, signed }));
                } else if *SRCCOPY
                    && let Some((addr, offset, flags)) = {
                        let (_, (r2, o2)) = kpair(func, ps, na, nm, arg);
                        let o2 = o2.wrapping_add(i64::from(i32::from(offset)));
                        copy_src(func, ps, na, nm, av, r2, o2, mty)
                    }
                {
                    rw.as_deref_mut().unwrap().push((
                        i,
                        Fwd::SrcLoad {
                            op: lop,
                            addr,
                            offset,
                            rty: func.dfg.value_type(func.dfg.first_result(i)),
                            flags,
                        },
                    ));
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
                let (r, o) = nm.pair(func, ps, na, args[1]).1;
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
                let (r0, o0) = nm.n0(func, ps, na0, args[1]);
                let o0 = o0.wrapping_add(i64::from(i32::from(offset)));
                if r0 != r || o0 != o {
                    kill(av, r0, o0, ty, None, iso, ncc, invs, cur, invreg, rpar);
                }
                // Locs kept under an `ofrozen` stop name (`kill_stops`).
                let stop0 = |x: Value| {
                    (has_stops && na.contains(&x) && !na0.contains(&x))
                        .then(|| nm.n0(func, ps, na0, x))
                };
                let imm = |l: &Loc| {
                    invs.contains(l)
                        || cur.is_some_and(|cur| {
                            invreg
                                .get(l)
                                .is_some_and(|rs| rs.iter().any(|&t| in_region(cur, t, rpar)))
                        })
                };
                if has_stops {
                    kill_stops(av, (r0, o0, ty), &stop0, iso, ncc, &imm);
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
                        let (ra, oa) = nm.pair(func, ps, na, a).1;
                        let oa = oa.wrapping_add(o);
                        let ms = match ra {
                            Root::V(arv) => ms_of(arv, oa),
                            _ => None,
                        };
                        kill(av, ra, oa, ty, ms, iso, ncc, invs, cur, invreg, rpar);
                        let (ra0, oa0) = nm.n0(func, ps, na0, a);
                        let oa0 = oa0.wrapping_add(o);
                        if ra0 != ra || oa0 != oa {
                            kill(av, ra0, oa0, ty, None, iso, ncc, invs, cur, invreg, rpar);
                        }
                        if has_stops {
                            kill_stops(av, (ra0, oa0, ty), &stop0, iso, ncc, &imm);
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
                        reach.insert(nm.pair(func, ps, na, v).1.0);
                        // `iso`/`ncc` were classified under `na0`
                        // (`noalias` alone); a `ofrozen` stop on an arg
                        // (`na` includes them) renames its root, so the
                        // plain-`noalias` root must be in `reach` too or
                        // an ncc loc survives a call that mutates it.
                        reach.insert(nm.n0(func, ps, na0, v).0);
                    }
                    av.retain(|&(r, o, t), _| {
                        let keep = iso.contains(&r)
                            || (ncc.contains(&r) && !reach.contains(&r))
                            || invs.contains(&(r, o, t));
                        let kreg = !keep
                            && cur.is_some_and(|cur| {
                                invreg
                                    .get(&(r, o, t))
                                    .is_some_and(|rs| rs.iter().any(|&t2| in_region(cur, t2, rpar)))
                            });
                        if kreg && pliron_cfg::tracing_on() {
                            tracing::debug!(target: "pliron::loadfwd",
                                "call {i}: region keeps {:?} (cur {cur:?}, regions {:?})",
                                (r, o, t), invreg.get(&(r, o, t)))
                        };
                        keep || kreg
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
#[allow(clippy::too_many_arguments)]
pub fn run(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    pf: &crate::proots::Facts,
    ps: &VMap<Value>,
    noalias: &FxHashSet<Value>,
    scoped_na: &FxHashSet<Value>,
    inv_insts: &FxHashSet<Inst>,
    ofrozen: &FxHashMap<Value, Vec<(u64, Region)>>,
    breg: &FxHashMap<Block, Region>,
    rpar: &pliron::cols::ColMap<Region, Region>,
    nw: &FxHashMap<FuncRef, bool>,
    cfrozen: &FxHashMap<FuncRef, u64>,
    nocap: &FxHashMap<(Inst, u32), u64>,
) -> usize {
    run_deref(func, cfg, pf, ps, noalias, scoped_na, inv_insts, ofrozen, breg, rpar, nw, cfrozen, nocap, &FxHashMap::default())
}

/// `run` with the dereferenceable-parameter facts (`Pipe::deref`), which
/// make critical-edge PRE loads from those parameters speculatable.
#[allow(clippy::too_many_arguments)]
pub fn run_deref(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    pf: &crate::proots::Facts,
    ps: &VMap<Value>,
    noalias: &FxHashSet<Value>,
    scoped_na: &FxHashSet<Value>,
    inv_insts: &FxHashSet<Inst>,
    ofrozen: &FxHashMap<Value, Vec<(u64, Region)>>,
    breg: &FxHashMap<Block, Region>,
    rpar: &pliron::cols::ColMap<Region, Region>,
    nw: &FxHashMap<FuncRef, bool>,
    cfrozen: &FxHashMap<FuncRef, u64>,
    nocap: &FxHashMap<(Inst, u32), u64>,
    deref: &FxHashMap<Value, (u64, bool)>,
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
    let t0 = prof2.then(std::time::Instant::now);
    // `cfg`/`pf`/`ps` are step-entry snapshots from `Analysis::lf_inputs`
    // (shared with dse's alias map); a rebuilt CFG after `pre` mutates
    // goes in `cfg2` below. `pf`/`ps` intentionally stay stale across
    // that mutation, exactly as the one-shot build did.
    let mut cfg2: Option<ControlFlowGraph> = None;
    let mut rpo = rpo_of(cfg);
    let mut n = 0;
    // `ms` disjointness pays off in `pre` (more locs survive → more
    // redundancy hoisting — fewer loads execute) but not in the plain
    // forwarding pass: there it only swaps a load for a value defined
    // earlier, lengthening live ranges into regalloc pressure (measured
    // ~10% on hashmap's probe loop vs ~0 on sortw's pivot reloads). The
    // fwd half stays behind a flag for now.
    let pre_ms = crate::pass_enabled("PLIRON_MINROOT_PRE");
    let fwd_ms = crate::env_set("PLIRON_MINROOT_FWD");
    let bnd = if pre_ms || fwd_ms {
        crate::proots::bounds(func, pf)
    } else {
        VMap::default()
    };
    // Root-resolution stop set: real `noalias` plus frozen operands —
    // either way the value names its own memory scope. `isolated` keeps
    // the real set so operand values don't get function-wide immunity.
    let stop: FxHashSet<Value> = noalias.iter().chain(ofrozen.keys()).copied().collect();
    let pm = if pre_ms || fwd_ms {
        pmroots(func, ps, &stop, &bnd, pf)
    } else {
        VMap::default()
    };
    let (pbnd, ppm): (&VMap<u64>, &VMap<(Root, i64)>) = if pre_ms {
        (&bnd, &pm)
    } else {
        (&VMap::default(), &VMap::default())
    };
    let (fbnd, fpm) = if fwd_ms {
        (&bnd, &pm)
    } else {
        (&VMap::default(), &VMap::default())
    };
    // `PLIRON_ESC_TRAP=1`: escapes in dead-end trap blocks don't stop
    // forwarding. num-bigint-dig's `montgomery` formats `&x, &y, &m` in an
    // `assert!` message, so rustc keeps each in a slot and every use
    // reloads it; the slots escape only into the panic's arguments.
    let trap_benign = pliron_cfg::env_is("PLIRON_ESC_TRAP", "1");
    let (iso, _escapes, ncc) = isolated_escapes_x(func, noalias, ps, cfrozen, nocap, scoped_na, trap_benign);
    if let Some(f) = crate::env_get("PLIRON_LF_DEBUG") {
        if (f == "dump" && lf_dump()) || func.name.to_string().contains(&f) {
            if pliron_cfg::tracing_on() {
                tracing::debug!(target: "pliron::loadfwd", "lf-debug {}: iso={:?} ncc={:?} escapes={:?} noalias={:?} scoped={:?} deref={:?}", func.name, iso, ncc, _escapes, noalias, scoped_na, deref)
            };
        }
    }
    if prof2 {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::loadfwd", "    lf-setup {:.3}ms", t0.map_or(std::time::Duration::ZERO, |t| t.elapsed()).as_secs_f64() * 1e3)
        };
    }
    let tt = |name: &str, t0: Option<std::time::Instant>| {
        if let (true, Some(t0)) = (prof2, t0) {
            if pliron_cfg::tracing_on() {
                tracing::info!(target: "pliron::loadfwd", "    lf-{name} {:.3}ms", t0.elapsed().as_secs_f64() * 1e3)
            };
        }
    };
    if crate::pass_enabled("PLIRON_LOADPRE") {
        let t0 = prof2.then(std::time::Instant::now);
        // `pre` needs the real `iso`: its loc-set dataflow runs `kill`,
        // which retains a loc across a store to a different root only when
        // one side is isolated — an empty set would drop every loc at the
        // first unrelated store.
        if *LOADPRE_DEBUG {
            eprintln!("loadpre {}: iso {:?} ncc {:?} escapes {:?}", func.name, iso, ncc, _escapes.keys().collect::<Vec<_>>());
        }
        // Later rounds see the loads earlier ones inserted: PRE around an
        // inner loop leaves them in its preheader, below the outer header,
        // and only another round carries them around the outer loop
        // (regex's `follow_epsilons` reloaded its `&mut SparseSet`'s
        // fields every outer iteration). `PLIRON_LOADPRE_ROUNDS`, default 1.
        let rounds: usize = crate::env_get("PLIRON_LOADPRE_ROUNDS").and_then(|r| r.parse().ok()).unwrap_or(1);
        for _ in 0..rounds.max(1) {
            let k = pre(
                func,
                ps,
                &stop,
                noalias,
                cfg2.as_ref().unwrap_or(cfg),
                &rpo,
                entry,
                &iso,
                &ncc,
                pbnd,
                ppm,
                deref,
            );
            n += k;
            if k == 0 {
                break;
            }
            let c = ControlFlowGraph::with_function(func);
            rpo = rpo_of(&c);
            cfg2 = Some(c);
        }
        tt("pre", t0);
    }
    let cfg: &ControlFlowGraph = cfg2.as_ref().unwrap_or(cfg);
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
    let t0 = prof2.then(std::time::Instant::now);
    let mut iters = 0;
    // Worklist over RPO order: `meet` only shrinks loc sets (a must
    // analysis), so the unique fixpoint is order-independent and a
    // block needs re-transferring only after one of its inputs
    // actually changed — a per-round full rescan just re-verifies
    // stable blocks. The bail mirrors `MAX_ITERS` rounds: a block
    // whose out changes more than that would have needed one more
    // RPO round than the old loop allowed.
    let mut work: VecDeque<Block> = rpo.iter().copied().collect();
    let mut inq: FxHashSet<Block> = work.iter().copied().collect();
    let mut nch: FxHashMap<Block, u32> = FxHashMap::default();
    let mut stable = true;
    // One root-walk memo for the whole fixpoint + scan pair — `ps`/
    // `stop`/`fbnd`/`fpm` are fixed for this phase.
    let nm = NrMemo::default();
    while let Some(b) = work.pop_front() {
        inq.remove(&b);
        let Some(mut av) = input(&out, b) else {
            continue;
        };
        iters += 1;
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
            &nm,
        );
        if out.get(&b) != Some(&av) {
            out.insert(b, av);
            let c = nch.entry(b).or_default();
            *c += 1;
            if *c > MAX_ITERS as u32 {
                stable = false;
                break;
            }
            for s in cfg.succ_iter(b) {
                if inq.insert(s) {
                    work.push_back(s);
                }
            }
        }
    }
    let stable = stable && work.is_empty();
    if prof2 {
        if pliron_cfg::tracing_on() {
            tracing::info!(target: "pliron::loadfwd", "    lf-fxpt {:.3}ms (iters={iters})",
                t0.map_or(std::time::Duration::ZERO, |t| t.elapsed()).as_secs_f64() * 1e3
            )
        };
    }
    if !stable {
        return 0;
    }
    let t0 = prof2.then(std::time::Instant::now);
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
                &nm,
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
            Fwd::Narrow { v, shift, mty, signed } => {
                let rty = func.dfg.value_type(r);
                let mut pos = cranelift_codegen::cursor::FuncCursor::new(func).at_inst(i);
                use cranelift_codegen::cursor::Cursor;
                let mut x = v;
                if shift > 0 {
                    x = pos.ins().ushr_imm_s(x, i64::from(shift));
                }
                if pos.func.dfg.value_type(x) != mty {
                    x = pos.ins().ireduce(mty, x);
                }
                if rty != mty {
                    x = if signed { pos.ins().sextend(rty, x) } else { pos.ins().uextend(rty, x) };
                }
                x
            }
            Fwd::SrcLoad { op, addr, offset, rty, flags } => {
                let mut pos = cranelift_codegen::cursor::FuncCursor::new(func).at_inst(i);
                use cranelift_codegen::cursor::Cursor;
                match op {
                    Opcode::Load => pos.ins().load(rty, flags, addr, offset),
                    Opcode::Uload8 => pos.ins().uload8(rty, flags, addr, offset),
                    Opcode::Uload16 => pos.ins().uload16(rty, flags, addr, offset),
                    Opcode::Uload32 => pos.ins().uload32(flags, addr, offset),
                    Opcode::Sload8 => pos.ins().sload8(rty, flags, addr, offset),
                    Opcode::Sload16 => pos.ins().sload16(rty, flags, addr, offset),
                    Opcode::Sload32 => pos.ins().sload32(flags, addr, offset),
                    _ => unreachable!(),
                }
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
    /// `PLIRON_LOADPRE_CARRY`: block-transfer evaluations left for the
    /// region walks (`region_value`/`region_preserves`) this function;
    /// 0 disables them (rustlantis seed 74680 compiled past the fuzz
    /// timeout with unbounded climbs), and a memo keyed by (block, loc,
    /// seed) makes repeated walks over the same region free.
    budget: std::cell::Cell<usize>,
    memo: std::cell::RefCell<FxHashMap<(Block, Loc, Value), Option<Value>>>,
    /// Merges where PRE of a loc already failed: `out` is fixed for the
    /// pass and the other inputs only tighten, so a retry from the next
    /// load of the same loc would redo the same climb for the same answer.
    failed: FxHashSet<(Block, Loc, bool)>,
}

/// Region-walk budget per function (block transfers).
const CARRY_BUDGET: usize = 4000;

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
        _ if r == r2 => o2 >= o.saturating_add(n) || o >= o2.saturating_add(i64::from(t2.bytes())),
        (Root::S(_), Root::S(_)) => true,
        _ => {
            shel(&r)
                || shel(&r2)
                || ms.is_some_and(|(mr, mo)| mr == r2 && mo >= o2.saturating_add(i64::from(t2.bytes())))
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
        _ if r == r2 => !(o2 >= o.saturating_add(i64::from(ty.bytes())) || o >= o2.saturating_add(i64::from(t2.bytes()))),
        (Root::S(_), Root::S(_)) => false,
        _ => !ms.is_some_and(|(mr, mo)| mr == r2 && mo >= o2.saturating_add(i64::from(t2.bytes()))),
    }
}

/// `may_write` under `kill_st`'s full keep rule: a store to another root
/// can't reach an isolated or bounded-escape (`ncc`) root's memory, nor
/// can a store to such a root reach anything else. PRE's "clobbered
/// earlier in this block?" check must agree with the availability sets
/// it was computed against, or a store to an unrelated `Vec` header
/// (`v25+152` in regex's `follow_epsilons`) blocks PRE of a `noalias`
/// param's fields.
fn may_write_iso(w: Loc, ms: Option<(Root, i64)>, l: Loc, iso: &FxHashSet<Root>, ncc: &FxHashSet<Root>) -> bool {
    let shel = |x: &Root| iso.contains(x) || ncc.contains(x);
    if w.0 != l.0 && (shel(&w.0) || shel(&l.0)) {
        return false;
    }
    may_write(w, ms, l)
}

/// `PLIRON_LOADFWD_NARROW=0` disables forwarding a narrower or extending
/// load from a wider stored value.
static NARROW: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| !pliron_cfg::env_is("PLIRON_LOADFWD_NARROW", "0"));

/// `PLIRON_LOADFWD_SRCCOPY` (opt-in): a load covered by `store (load s)`
/// re-reads `s` directly instead of the copied destination — memcpy→load
/// forwarding through a stack round-trip.
static SRCCOPY: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| pliron_cfg::env_is("PLIRON_LOADFWD_SRCCOPY", "1"));

/// `PLIRON_LOADPRE_CARRY=1`: loop-carried PRE. At a merge, a missing pred
/// that the merge dominates and that preserves the location all the way
/// from the merge's start carries the new param around (its edge arg is
/// the param itself) instead of needing a compensating load; and when no
/// pred of the merge has the location, the search climbs the dominator
/// tree while every block on the way preserves it. LLVM's GVN load PRE
/// gets the same result through its full availability fixpoint: regex's
/// `next_state` reloaded its `&mut SparseSet`'s five fields per decoded
/// element because the only loads sat in one arm of the loop body, so no
/// pred of the arm's merge had them, while the loop header had them on
/// the arm's back edge and lost them only after the `follow_epsilons` call.
static LOADPRE_CARRY: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| pliron_cfg::env_is("PLIRON_LOADPRE_CARRY", "1"));

/// Every block on a path from `top` to `from` (both inclusive) preserves
/// `loc`, and every path into `from` passes `top` (so `top` dominates it).
fn region_preserves(
    func: &Function,
    cfg: &ControlFlowGraph,
    reach: &FxHashSet<Block>,
    pre: &Pre,
    top: Block,
    from: Block,
    incl_from: bool,
    loc: Loc,
    preserves: &dyn Fn(&Function, Block, Loc) -> bool,
) -> bool {
    let keeps = |x: Block| -> bool {
        let key = (x, loc, Value::reserved_value());
        if let Some(&r) = pre.memo.borrow().get(&key) {
            return r == Some(Value::reserved_value());
        }
        if pre.budget.get() == 0 {
            return false;
        }
        pre.budget.set(pre.budget.get() - 1);
        let r = preserves(func, x, loc);
        pre.memo.borrow_mut().insert(key, r.then_some(Value::reserved_value()));
        r
    };
    if !keeps(top) {
        return false;
    }
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut stack = vec![from];
    // `from` excluded (`!incl_from`) means only its prefix up to the load
    // is vouched for by the caller. If the region re-enters `from` — a
    // cycle through it back to itself — its tail is on the path too and
    // the whole block must preserve: lol_html's state machine loaded
    // `self.state` in the block whose `try_call_indirect` then wrote it,
    // and the loop edge came straight back to that block.
    let mut reenters_from = false;
    while let Some(x) = stack.pop() {
        if x == top || !seen.insert(x) {
            continue;
        }
        if seen.len() > 256 || !reach.contains(&x) || ((incl_from || x != from) && !keeps(x)) {
            return false;
        }
        let ps = pred_blocks(cfg, x);
        if ps.is_empty() {
            return false;
        }
        if ps.contains(&from) {
            reenters_from = true;
        }
        stack.extend(ps);
    }
    !(reenters_from && !incl_from) || keeps(from)
}

/// The value `loc` holds at the end of `p` when it holds `seed` at the
/// start of `top`, following every path inside the region `top` dominates
/// down to `p` (`carry` runs one block's transfer): `None` when a path
/// clobbers it, two paths disagree at a merge, or the region has a cycle.
fn region_value(
    func: &Function,
    cfg: &ControlFlowGraph,
    reach: &FxHashSet<Block>,
    pre: &Pre,
    top: Block,
    p: Block,
    loc: Loc,
    seed: Value,
    carry0: &dyn Fn(&Function, Block, Loc, Value) -> Option<Value>,
) -> Option<Value> {
    let carry = |f: &Function, x: Block, loc: Loc, v: Value| -> Option<Value> {
        let key = (x, loc, v);
        if let Some(&r) = pre.memo.borrow().get(&key) {
            return r;
        }
        if pre.budget.get() == 0 {
            return None;
        }
        pre.budget.set(pre.budget.get() - 1);
        let r = carry0(f, x, loc, v);
        pre.memo.borrow_mut().insert(key, r);
        r
    };
    let carry = &carry;
    let mut region: FxHashSet<Block> = FxHashSet::default();
    let mut stack = vec![p];
    while let Some(x) = stack.pop() {
        if x == top || !region.insert(x) {
            continue;
        }
        if region.len() > 256 || !reach.contains(&x) {
            return None;
        }
        let ps = pred_blocks(cfg, x);
        if ps.is_empty() {
            return None;
        }
        stack.extend(ps);
    }
    // Forward fixpoint over the region: a block's value goes unknown ->
    // some value -> lost (never one value to another), so it converges;
    // inner cycles that preserve the value pass it through.
    let mut out: FxHashMap<Block, Option<Option<Value>>> = FxHashMap::default();
    out.insert(top, Some(carry(func, top, loc, seed)));
    let mut order: Vec<Block> = region.iter().copied().collect();
    order.sort_unstable();
    for _ in 0..64 {
        let mut changed = false;
        for &x in &order {
            let known: Vec<Option<Value>> =
                pred_blocks(cfg, x).iter().filter_map(|q| out.get(q).copied().flatten()).collect();
            let new = match known.first() {
                None => None,
                Some(&Some(v)) if known.iter().all(|&k| k == Some(v)) => {
                    let o = carry(func, x, loc, v);
                    if o.is_none() && *LOADPRE_DEBUG {
                        eprintln!("    region {top}->{p} {loc:?}: lost in {x}");
                    }
                    Some(o)
                }
                _ => {
                    if *LOADPRE_DEBUG && out.get(&x).copied().unwrap_or(None) != Some(None) {
                        eprintln!("    region {top}->{p} {loc:?}: disagree/lost at {x}: {known:?}");
                    }
                    Some(None)
                }
            };
            if new != out.get(&x).copied().unwrap_or(None) {
                out.insert(x, new);
                changed = true;
            }
        }
        if !changed {
            return out.get(&p).copied().flatten().flatten();
        }
    }
    if *LOADPRE_DEBUG {
        eprintln!("    region {top}->{p} {loc:?}: no fixpoint");
    }
    None
}

/// `PLIRON_LOADPRE_CARRY_LIMIT`: numbers every carried PRE and every climb
/// attempt in this process; `false` once past the limit.
fn carry_app(kind: &str, b: Block, loc: Loc) -> bool {
    // `PLIRON_LOADPRE_CARRY_ONLY=<substr>`: only in matching symbols.
    if let Some(only) = &*CARRY_ONLY
        && !LF_SYM.with(|s| s.borrow().contains(only.as_str()))
    {
        return false;
    }
    let Some(limit) = *CARRY_LIMIT else { return true };
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    LF_SYM.with(|s| eprintln!("carry-app {n} {kind} {} {b} {loc:?}", s.borrow()));
    n <= limit
}

static CARRY_ONLY: std::sync::LazyLock<Option<String>> =
    std::sync::LazyLock::new(|| std::env::var("PLIRON_LOADPRE_CARRY_ONLY").ok());

static CARRY_LIMIT: std::sync::LazyLock<Option<u64>> =
    std::sync::LazyLock::new(|| std::env::var("PLIRON_LOADPRE_CARRY_LIMIT").ok().and_then(|v| v.parse().ok()));

static LOADPRE_DEBUG: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var_os("PLIRON_LOADPRE_DEBUG").is_some());
static LOADPRE_UP: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| !pliron_cfg::env_is("PLIRON_LOADPRE_UP", "0"));

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
/// memory state at end of every pred. A pred that can also branch elsewhere
/// (a critical edge) would run the load on that other path too, where the
/// address need not be valid — `notrap` only holds where the load was. So
/// such a pred needs a `can_move` (dereferenceable) load: btree's
/// `search_tree_for_bifurcation` loads a `Bound` key through a pointer that
/// is null for `Unbounded`, and the compensating load ran on the
/// `Unbounded` arm of the pred's `br_table` (wasmtime's GC free list
/// segfaulted in `BTreeMap::range`, 2026-10-24).
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
    deref: &FxHashMap<Value, (u64, bool)>,
    preserves: Option<&dyn Fn(&Function, Block, Loc) -> bool>,
    carry: Option<&dyn Fn(&Function, Block, Loc, Value) -> Option<Value>>,
    climb: bool,
) -> Option<Value> {
    // Keyed by `climb` too: the merge-only evaluation of a block failing
    // says nothing about a climb that starts from it.
    if pre.failed.contains(&(b, loc, climb)) {
        return None;
    }
    let r = edge_pre_inner(func, cfg, dom, out, reach, pre, b, i, loc, deref, preserves, carry, climb);
    if r.is_none() {
        pre.failed.insert((b, loc, climb));
    }
    r
}

#[allow(clippy::too_many_arguments)]
fn edge_pre_inner(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dom: &mut Option<DominatorTree>,
    out: &FxHashMap<Block, FxHashSet<Loc>>,
    reach: &FxHashSet<Block>,
    pre: &mut Pre,
    b: Block,
    i: Inst,
    loc: Loc,
    deref: &FxHashMap<Value, (u64, bool)>,
    preserves: Option<&dyn Fn(&Function, Block, Loc) -> bool>,
    carry: Option<&dyn Fn(&Function, Block, Loc, Value) -> Option<Value>>,
    climb: bool,
) -> Option<Value> {
    // `climb`: this is the outermost call for the load, allowed to walk
    // upward (single-pred chain, dominator climb). The calls it makes
    // for the merges it reaches evaluate that merge only: a climb inside
    // a climb multiplied the walk by the depth at every level (rustlantis
    // seed 74680: one `f64` load produced 1.1M climb steps, past the fuzz
    // compile timeout).
    // Inside a dereferenceable parameter's range, by the loc's root and
    // root-relative offset (the load's own address operand may be a copy
    // or a derived pointer the `deref` table doesn't key).
    let in_deref = |loc: Loc| -> bool {
        let size = i64::from(loc.2.bytes());
        match loc.0 {
            Root::V(r) => deref.get(&r).is_some_and(|&(bytes, _)| loc.1 >= 0 && loc.1 + size <= bytes as i64),
            _ => false,
        }
    };
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
    // Upward (`PLIRON_LOADPRE_UP=0` disables): the load's block has one
    // predecessor and the loc is available at none of its ends, but higher
    // up a merge block has it on some in-edges (a loop header: available
    // on the back edges, not on entry). If every block on the single-pred
    // chain from that merge down to `b` keeps the loc (`preserves`), PRE
    // at the merge and use its value here: it dominates the chain. The
    // merge's compensating loads run on paths that never reach this load,
    // so the load must be speculatable. regex's `follow_epsilons` reloaded
    // its `&mut SparseSet`'s buffer pointer and length every iteration.
    if climb && !avail && missing.len() >= 2 && *LOADPRE_CARRY && *LOADPRE_UP {
        // The load's own block is a merge with the loc on no pred: climb
        // the dominator tree from here (the chain walk below is for the
        // single-pred case).
        let Some(preserves) = preserves else { return None };
        if !in_deref(loc)
            && !func.dfg.insts[i].memflags_data(&func.dfg).is_some_and(|f| f.can_move())
        {
            return None;
        }
        if !carry_app("merge-climb", b, loc) {
            return None;
        }
        let mut m = b;
        for _ in 0..8 {
            if dom.is_none() {
                *dom = Some(DominatorTree::with_function(func, cfg));
            }
            let Some(idom) = dom.as_ref().unwrap().idom(m) else { return None };
            if !reach.contains(&idom) || !region_preserves(func, cfg, reach, pre, idom, m, m != b, loc, preserves) {
                if *LOADPRE_DEBUG {
                    eprintln!("edge_pre {b} {i} {loc:?}: merge climb stops at {idom}");
                }
                return None;
            }
            if *LOADPRE_DEBUG {
                eprintln!("edge_pre {b} {i} {loc:?}: merge climb to {idom}");
            }
            m = idom;
            if let Some(v) = edge_pre(func, cfg, dom, out, reach, pre, m, i, loc, deref, Some(preserves), carry, false) {
                return Some(v);
            }
            if cfg.pred_iter(m).any(|p| out.get(&p.block).is_some_and(|o| o.contains(&loc))) {
                return None;
            }
        }
        return None;
    }
    if climb && !avail && missing.len() == 1 && *LOADPRE_UP {
        let Some(preserves) = preserves else { return None };
        let off = match func.dfg.insts[i] {
            InstructionData::Load { offset, .. } => i64::from(i32::from(offset)),
            _ => return None,
        };
        let arg0 = func.dfg.resolve_aliases(func.dfg.inst_args(i)[0]);
        let size = i64::from(func.dfg.value_type(func.dfg.first_result(i)).bytes());
        let spec = func.dfg.insts[i].memflags_data(&func.dfg).is_some_and(|f| f.can_move())
            || deref.get(&arg0).is_some_and(|&(bytes, _)| off >= 0 && off + size <= bytes as i64)
            || in_deref(loc);
        if !spec {
            if *LOADPRE_DEBUG {
                eprintln!("edge_pre {b} {i} {loc:?}: upward bail: not speculatable (deref {:?})", deref.get(&arg0));
            }
            return None;
        }
        let mut m = missing[0].0;
        let mut chain: Vec<Block> = Vec::new();
        loop {
            if m == b || chain.contains(&m) || !reach.contains(&m) || !preserves(func, m, loc) {
                if *LOADPRE_DEBUG {
                    eprintln!(
                        "edge_pre {b} {i} {loc:?}: upward bail at {m}: self={} seen={} reach={} preserves={}",
                        m == b,
                        chain.contains(&m),
                        reach.contains(&m),
                        preserves(func, m, loc)
                    );
                }
                return None;
            }
            chain.push(m);
            let ps = pred_blocks(cfg, m);
            if ps.len() == 1 {
                m = ps[0];
                continue;
            }
            break;
        }
        if chain.len() > 16 {
            return None;
        }
        if *LOADPRE_DEBUG {
            eprintln!("edge_pre {b} {i} {loc:?}: upward to {m} via {chain:?}");
        }
        if !*LOADPRE_CARRY {
            return edge_pre(func, cfg, dom, out, reach, pre, m, i, loc, deref, None, None, false);
        }
        if !carry_app("chain-climb", b, loc) {
            return edge_pre(func, cfg, dom, out, reach, pre, m, i, loc, deref, None, None, false);
        }
        // Climb while the merge has the loc on no pred: each step needs
        // every block between the dominator and the merge to preserve it.
        let mut m = m;
        for _ in 0..8 {
            if let Some(v) = edge_pre(func, cfg, dom, out, reach, pre, m, i, loc, deref, Some(preserves), carry, false) {
                return Some(v);
            }
            if cfg.pred_iter(m).any(|p| out.get(&p.block).is_some_and(|o| o.contains(&loc))) {
                return None;
            }
            if dom.is_none() {
                *dom = Some(DominatorTree::with_function(func, cfg));
            }
            let Some(idom) = dom.as_ref().unwrap().idom(m) else { return None };
            if !reach.contains(&idom) || !region_preserves(func, cfg, reach, pre, idom, m, true, loc, preserves) {
                if *LOADPRE_DEBUG {
                    eprintln!("edge_pre {b} {i} {loc:?}: climb stops at {idom}");
                }
                return None;
            }
            if *LOADPRE_DEBUG {
                eprintln!("edge_pre {b} {i} {loc:?}: climb to {idom}");
            }
            m = idom;
        }
        return None;
    }
    if *LOADPRE_DEBUG {
        eprintln!("edge_pre {b} {i} {loc:?}: avail={avail} missing={}", missing.len());
    }
    // `missing` empty is the fully-available case (`st` would have hit);
    // `avail` false is plain hoisting, not redundancy elimination.
    if !avail || missing.is_empty() {
        return None;
    }
    // Already PRE'd here for an earlier load of the same loc: reuse the
    // param instead of inserting a second set of compensating loads.
    if let Some(&v) = pre.start.get(&(b, loc))
        && missing.iter().all(|&(p, _)| pre.inserted.contains_key(&(b, loc, p)))
    {
        return Some(v);
    }
    // Loop-carried preds (`PLIRON_LOADPRE_CARRY`): dominated by `b`, with
    // the loc's value known at their end given the new param at `b`'s
    // start — the param itself (reserved marker) or a value stored on
    // the way. A self back-edge among the non-carried missing would read
    // memory past the load itself.
    let carried: Vec<Option<Value>> = missing
        .iter()
        .map(|&(p, _)| {
            if !*LOADPRE_CARRY {
                return None;
            }
            let c = carry?;
            region_value(func, cfg, reach, pre, b, p, loc, Value::reserved_value(), c)
        })
        .collect();
    if missing.iter().zip(&carried).any(|(&(p, _), c)| p == b && c.is_none()) {
        return None;
    }
    // `PLIRON_LOADPRE_CARRY_LIMIT=N`: only the first N loop-carried PREs
    // of this process apply (a bisect knob of its own: `PLIRON_OPT_BISECT`
    // counts every gate in every crate, and a limit that cuts other
    // passes short breaks the build before it reaches these). Each
    // application prints `carry-app <n> <symbol>` to stderr when the
    // limit is set.
    if carried.iter().any(|c| c.is_some()) && !carry_app("carried", b, loc) {
        return None;
    }
    let real = carried.iter().filter(|c| c.is_none()).count();
    if real == 0 {
        // Nothing to insert: the loc would be fully available only if
        // every missing pred were carried, which `out` already rejects.
        return None;
    }
    if pre.inserted.len() + real > MAX_INSERTS {
        if *LOADPRE_DEBUG {
            eprintln!("  bail: MAX_INSERTS ({})", pre.inserted.len());
        }
        return None;
    }
    let InstructionData::Load {
        arg, flags, offset, ..
    } = func.dfg.insts[i]
    else {
        return None;
    };
    let arg = func.dfg.resolve_aliases(arg);
    // Speculatable: `can_move`, or inside a dereferenceable parameter's
    // range (`deref`, from rustc's `dereferenceable`: a reference stays
    // valid for the whole call). regex's `follow_epsilons` reloaded its
    // `&mut SparseSet`'s buffer pointer and length at an inner-loop header
    // with ten critical in-edges.
    let in_deref_arg = || {
        let off = match func.dfg.insts[i] {
            InstructionData::Load { offset, .. } => i64::from(i32::from(offset)),
            _ => return false,
        };
        let size = i64::from(func.dfg.value_type(func.dfg.first_result(i)).bytes());
        deref.get(&arg).is_some_and(|&(bytes, _)| off >= 0 && off + size <= bytes as i64)
    };
    let speculatable = func.dfg.insts[i].memflags_data(&func.dfg).is_some_and(|f| f.can_move())
        || in_deref_arg()
        || in_deref(loc);
    for (&(p, pinst), c) in missing.iter().zip(&carried) {
        if c.is_some() {
            continue;
        }
        if func.layout.last_inst(p) != Some(pinst)
            || !matches!(
                func.dfg.insts[pinst].opcode(),
                Opcode::Jump | Opcode::Brif | Opcode::BrTable
            )
        {
            if *LOADPRE_DEBUG {
                eprintln!("  bail: {p} terminator {:?}", func.dfg.insts[pinst].opcode());
            }
            return None;
        }
        let only_b = func.dfg.insts[pinst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .all(|c| c.block(&func.dfg.value_lists) == b);
        if !only_b && !speculatable {
            if *LOADPRE_DEBUG {
                eprintln!("  bail: {p} not only-b and not speculatable");
            }
            return None;
        }
        if dom.is_none() {
            *dom = Some(DominatorTree::with_function(func, cfg));
        }
        let dominates = |v: Value| match func.dfg.value_def(v) {
            ValueDef::Result(di, _) => dom.as_ref().unwrap().try_dominates(di, pinst, &func.layout),
            ValueDef::Param(db, _) => dom.as_ref().unwrap().try_dominates(db, pinst, &func.layout),
            _ => false,
        };
        // The address may be `base + k` computed inside the loop: rebuild
        // it from a dominating `base` in the inserted load's offset.
        let ok = dominates(arg) || {
            let (base, k) = peel_const(func, arg);
            dominates(base) && i32::try_from(k + i64::from(i32::from(offset))).is_ok()
        };
        if !ok {
            if *LOADPRE_DEBUG {
                eprintln!("  bail: address doesn't dominate {p}");
            }
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
    for (&(p, pinst), c) in missing.iter().zip(&carried) {
        if let Some(cv) = *c {
            let cv = if cv == Value::reserved_value() { v } else { cv };
            if *LOADPRE_DEBUG {
                eprintln!("  carried: {p} -> {cv}");
            }
            pre.inserted.insert((b, loc, p), cv);
            continue;
        }
        let d = dom.as_ref().unwrap();
        let dom_arg = match func.dfg.value_def(arg) {
            ValueDef::Result(di, _) => d.try_dominates(di, pinst, &func.layout),
            ValueDef::Param(db, _) => d.try_dominates(db, pinst, &func.layout),
            _ => false,
        };
        let (arg, offset) = if dom_arg {
            (arg, offset)
        } else {
            let (base, k) = peel_const(func, arg);
            (base, cranelift_codegen::ir::immediates::Offset32::new(i32::try_from(k + i64::from(i32::from(offset))).unwrap()))
        };
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

/// `v` as `base + k` through `iadd` by constants (either operand order).
fn peel_const(func: &Function, v: Value) -> (Value, i64) {
    let (mut v, mut k) = (func.dfg.resolve_aliases(v), 0i64);
    for _ in 0..8 {
        let Some(i) = func.dfg.value_def(v).inst() else { break };
        let InstructionData::Binary { opcode: Opcode::Iadd, args } = func.dfg.insts[i] else { break };
        let c = |x: Value| match func.dfg.value_def(func.dfg.resolve_aliases(x)).inst().map(|j| &func.dfg.insts[j]) {
            Some(InstructionData::UnaryImm { opcode: Opcode::Iconst, imm }) => Some(imm.bits()),
            _ => None,
        };
        match (c(args[0]), c(args[1])) {
            (_, Some(n)) => {
                k = k.wrapping_add(n);
                v = func.dfg.resolve_aliases(args[0]);
            }
            (Some(n), _) => {
                k = k.wrapping_add(n);
                v = func.dfg.resolve_aliases(args[1]);
            }
            _ => break,
        }
    }
    (v, k)
}

/// Load forwarding with PRE at joins (LLVM GVN's load PRE / LICM scalar
/// promotion for loop-carried fields): a location available from every
/// predecessor, even as different values, becomes a block param. The key
/// sets follow `transfer` exactly, so every needed end value exists.
fn pre(
    func: &mut Function,
    ps: &VMap<Value>,
    na: &FxHashSet<Value>,
    na0: &FxHashSet<Value>,
    cfg: &ControlFlowGraph,
    rpo: &[Block],
    entry: Block,
    iso: &FxHashSet<Root>,
    ncc: &FxHashSet<Root>,
    bnd: &VMap<u64>,
    pm: &VMap<(Root, i64)>,
    deref: &FxHashMap<Value, (u64, bool)>,
) -> usize {
    let nw: &FxHashMap<FuncRef, bool> = &FxHashMap::default();
    let eh = false;
    let has_stops = na.len() > na0.len();
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
    // Same worklist shape as the forwarding fixpoint above: the key-set
    // meet only shrinks, so re-transferring is needed only when an
    // input changed. Per-block change cap mirrors `MAX_ITERS` rounds.
    let mut work: VecDeque<Block> = rpo.iter().copied().collect();
    let mut inq: FxHashSet<Block> = work.iter().copied().collect();
    let mut nch: FxHashMap<Block, u32> = FxHashMap::default();
    let mut stable = true;
    // One root-walk memo for this whole fixpoint + the apply scan —
    // `ps`/`na`/`na0`/`bnd`/`pm` are fixed for the phase.
    let nm = NrMemo::default();
    while let Some(b) = work.pop_front() {
        inq.remove(&b);
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
            &pliron::cols::ColMap::default(),
            nw,
            bnd,
            pm,
            eh,
            &nm,
        );
        let ks: FxHashSet<Loc> = av.into_keys().collect();
        if out.get(&b) != Some(&ks) {
            out.insert(b, ks);
            let c = nch.entry(b).or_default();
            *c += 1;
            if *c > MAX_ITERS as u32 {
                stable = false;
                break;
            }
            for s in cfg.succ_iter(b) {
                if inq.insert(s) {
                    work.push_back(s);
                }
            }
        }
    }
    if !(stable && work.is_empty()) {
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
        budget: std::cell::Cell::new(CARRY_BUDGET),
        memo: std::cell::RefCell::new(FxHashMap::default()),
        failed: FxHashSet::default(),
    };
    let mut dom: Option<DominatorTree> = None;
    let mut fwd: Vec<(Inst, Value)> = Vec::new();
    // Would block `x` keep `loc` if it were available at its start (the
    // same transfer as the `out` fixpoint)? For `edge_pre`'s upward walk.
    // The entry value must survive, not just the key: a block that
    // overwrites `loc` and then reloads it ends with `loc` available under
    // a new value, and PRE would hand the chain the stale entry value
    // (fuzz seed 27992: block27 stored to `v6` and reloaded it; the
    // `out` fixpoint had dropped the reload at `MAX_ENTRIES`, so PRE
    // went upward through block27).
    let carry = |f: &Function, x: Block, loc: Loc, seed: Value| -> Option<Value> {
        let mut av: Avail = std::iter::once((loc, seed)).collect();
        transfer(
            f,
            ps,
            na,
            na0,
            x,
            &mut av,
            None,
            iso,
            ncc,
            &FxHashSet::default(),
            &mut FxHashSet::default(),
            None,
            &FxHashMap::default(),
            &mut FxHashMap::default(),
            &pliron::cols::ColMap::default(),
            nw,
            bnd,
            pm,
            eh,
            &nm,
        );
        av.get(&loc).copied()
    };
    let preserves = |f: &Function, x: Block, loc: Loc| -> bool {
        let mut av: Avail = std::iter::once((loc, Value::reserved_value())).collect();
        transfer(
            f,
            ps,
            na,
            na0,
            x,
            &mut av,
            None,
            iso,
            ncc,
            &FxHashSet::default(),
            &mut FxHashSet::default(),
            None,
            &FxHashMap::default(),
            &mut FxHashMap::default(),
            &pliron::cols::ColMap::default(),
            nw,
            bnd,
            pm,
            eh,
            &nm,
        );
        av.get(&loc) == Some(&Value::reserved_value())
    };
    // `nm` covers the apply scan too — `func` mutates only through
    // `edge_pre`'s add-only inserts and the `fwd` rewrites below.
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
                    let (r, o) = nm.pair(func, ps, na, arg).1;
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
                        && writes.iter().all(|&(w, ms)| !may_write_iso(w, ms, loc, iso, ncc))
                        && let Some(v) =
                            edge_pre(func, cfg, &mut dom, &out, &reach, &mut pre, b, i, loc, deref, Some(&preserves), Some(&carry), true)
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
                    let (r, o) = nm.pair(func, ps, na, args[1]).1;
                    let o = o.wrapping_add(i64::from(i32::from(offset)));
                    let ms_of = |rv: Value, off: i64| {
                        let (mr, mo) = nm.rm(func, ps, na, bnd, rv);
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
                    let (r0, o0) = nm.n0(func, ps, na0, args[1]);
                    let o0 = o0.wrapping_add(i64::from(i32::from(offset)));
                    if r0 != r || o0 != o {
                        kill_st(&mut st, r0, o0, ty, None, &iso, ncc);
                        writes.push(((r0, o0, ty), None));
                    }
                    // Stop-named locs, as in `transfer` (no immunities
                    // here, matching the empty sets the fixpoint used).
                    let stop0 = |x: Value| {
                        (has_stops && na.contains(&x) && !na0.contains(&x))
                            .then(|| nm.n0(func, ps, na0, x))
                    };
                    if has_stops {
                        kill_stops(&mut st, (r0, o0, ty), &stop0, &iso, ncc, &|_| false);
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
                            let (ra, oa) = nm.pair(func, ps, na, a).1;
                            let oa = oa.wrapping_add(o);
                            let ms = match ra {
                                Root::V(arv) => ms_of(arv, oa),
                                _ => None,
                            };
                            let al = (ra, oa, ty);
                            kill_st(&mut st, al.0, al.1, al.2, ms, &iso, ncc);
                            writes.push((al, ms));
                            let (ra0, oa0) = nm.n0(func, ps, na0, a);
                            let oa0 = oa0.wrapping_add(o);
                            if ra0 != ra || oa0 != oa {
                                let a0 = (ra0, oa0, ty);
                                kill_st(&mut st, a0.0, a0.1, a0.2, None, &iso, ncc);
                                writes.push((a0, None));
                            }
                            if has_stops {
                                kill_stops(&mut st, (ra0, oa0, ty), &stop0, &iso, ncc, &|_| false);
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
                        reach.insert(nm.pair(func, ps, na, v).1.0);
                        reach.insert(nm.n0(func, ps, na0, v).0);
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
    // Only pending pairs own the param they map to: `start_val` also
    // records single-pred pass-throughs (`(b, loc)` -> the pred's param),
    // and inverting all of `start` let such an alias shadow the owning
    // pair, so a dead pair's param slipped onto an edge unpoisoned
    // (rustlantis seeds 810/1276: "argument v14720 is not attached").
    let param_of: FxHashMap<Value, (Block, Loc)> = pre
        .pending
        .iter()
        .filter_map(|&k| pre.start.get(&k).map(|&v| (v, k)))
        .collect();
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
        // A pair that died during propagation had its param removed — its
        // resolved args (which can be dead pairs' own params, now
        // unattached) must not reach the edges (rustlantis seed 410:
        // dangling `v5971` in a block10 call arg).
        if dead.contains(&(b, loc)) {
            continue;
        }
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
