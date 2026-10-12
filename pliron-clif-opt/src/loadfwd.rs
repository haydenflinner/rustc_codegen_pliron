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
    Block, BlockArg, Function, Inst, InstructionData, Opcode, StackSlot, Type, Value, ValueDef,
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
pub fn param_src(func: &Function) -> FxHashMap<Value, Value> {
    let mut src: FxHashMap<Value, Option<Value>> = FxHashMap::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for bc in func.dfg.insts[i].branch_destination(
                &func.dfg.jump_tables,
                &func.dfg.exception_tables,
            ) {
                let dst = bc.block(&func.dfg.value_lists);
                let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                let params = func.dfg.block_params(dst);
                for (k, a) in args.into_iter().enumerate() {
                    let Some(&p) = params.get(k) else {
                        break;
                    };
                    let v = match a {
                        BlockArg::Value(v) => func.dfg.resolve_aliases(v),
                        // Pseudo-args (try_call ret/exn) can't be read here.
                        _ => {
                            src.insert(p, None);
                            continue;
                        }
                    };
                    if v == p {
                        continue;
                    }
                    match src.entry(p) {
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(Some(v));
                        }
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            if *e.get() != Some(v) {
                                e.insert(None);
                            }
                        }
                    }
                }
            }
        }
    }
    src.into_iter().filter_map(|(p, s)| s.map(|s| (p, s))).collect()
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
    for bc in func.dfg.insts[i]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
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

pub fn root(
    func: &Function,
    ps: &FxHashMap<Value, Value>,
    na: &FxHashSet<Value>,
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
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    (Root::V(v), off)
}

pub fn notrap(func: &Function, i: Inst) -> bool {
    func.dfg.insts[i].memflags().is_some()
        && func.dfg.insts[i].memflags_trap_code(&func.dfg).is_none()
}

/// Kill the locations a store of `ty` at `(r, o)` may overwrite.
pub fn kill(
    av: &mut Avail,
    r: Root,
    o: i64,
    ty: Type,
    iso: &FxHashSet<Root>,
    invs: &FxHashSet<Loc>,
    cur: Option<Region>,
    invreg: &FxHashMap<Loc, Vec<Region>>,
    rpar: &FxHashMap<Region, Region>,
) {
    let n = i64::from(ty.bytes());
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
            _ => iso.contains(&r) || iso.contains(&r2),
        }
    });
}

/// A direct call to a function that writes no memory before returning
/// normally; across `try_call` it must not write at all unless `eh`, where
/// [`unwind_edge`] clears the exception edges instead (nowrite.rs).
pub fn write_free(
    func: &Function,
    i: Inst,
    nw: &FxHashMap<FuncRef, bool>,
    eh: bool,
) -> bool {
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
    isolated_escapes(func, noalias, ps, cfrozen).0
}

/// `isolated` plus, for every rejected root, the blocks holding its
/// non-benign uses. A root escapes only where the pointer value leaves the
/// tracked load/store-address pattern (stored elsewhere, passed to a call,
/// feeding an untracked op, ...). Loop consumers can treat a root as
/// isolated for the duration of a loop when none of its escape blocks can
/// reach the header: the pointer can't have leaked before the first
/// iteration nor during any iteration.
pub fn isolated_escapes(
    func: &Function,
    noalias: &FxHashSet<Value>,
    ps: &FxHashMap<Value, Value>,
    _cfrozen: &FxHashMap<FuncRef, u64>,
) -> (FxHashSet<Root>, FxHashMap<Root, Vec<Block>>) {
    let mut cand: FxHashSet<Root> = noalias.iter().map(|&v| Root::V(v)).collect();
    if crate::pass_enabled("PLIRON_FWD_SLOTS") {
        cand.extend(func.sized_stack_slots.keys().map(Root::S));
    }
    let mut escapes: FxHashMap<Root, Vec<Block>> = FxHashMap::default();
    if cand.is_empty() {
        return (cand, escapes);
    }
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let data = &func.dfg.insts[i];
            let nargs = func.dfg.inst_args(i).len();
            // A use that is a branch arg (index ≥ the inst's own args):
            // benign iff the param it feeds is the same root — i.e., the
            // pointer stays inside this equivalence class.
            for (k, v) in func.dfg.inst_values(i).enumerate() {
                let (r, _) = root(func, ps, noalias, v);
                if !cand.contains(&r) {
                    continue;
                }
                let fwd = |j: usize| {
                    fwd_arg(func, i, j).is_some_and(|(dst, pos)| {
                        Root::V(psrc(ps, noalias, func, func.dfg.block_params(dst)[pos])) == r
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
                        root(func, ps, noalias, res).0 == r || noalias.contains(&res)
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
                }
            }
        }
    }
    let iso: FxHashSet<Root> = cand
        .into_iter()
        .filter(|r| !escapes.contains_key(r))
        .collect();
    (iso, escapes)
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
    if !(ty.is_int() && ty.bits() <= 64 || matches!(ty, cranelift_codegen::ir::types::F32 | cranelift_codegen::ir::types::F64)) {
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
    let (ra, oa) = root(func, ps, na, args[1]);
    let (rb, ob) = root(func, ps, na, args[2]);
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
    b: Block,
    av: &mut Avail,
    rw: Option<&mut Vec<(Inst, Fwd)>>,
    iso: &FxHashSet<Root>,
    inv_insts: &FxHashSet<Inst>,
    invs: &mut FxHashSet<Loc>,
    cur: Option<Region>,
    ofz: &FxHashMap<Value, Vec<(u64, Region)>>,
    invreg: &mut FxHashMap<Loc, Vec<Region>>,
    rpar: &FxHashMap<Region, Region>,
    nw: &FxHashMap<FuncRef, bool>,
    eh: bool,
) {
    let mut rw = rw;
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
                let (r, o) = root(func, ps, na, arg);
                let off = o.wrapping_add(i64::from(i32::from(offset)));
                let loc = (r, off, ty);
                if std::env::var_os("PLIRON_LFDEBUG").is_some() {
                    eprintln!("load {res} arg {arg} root {r:?} off {off} av={:?}", av.get(&loc));
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
                if let Root::V(rv) = r
                    && let Some(rs) = ofz.get(&rv)
                {
                    for &(bytes, reg) in rs {
                        if off >= 0
                            && off + i64::from(ty.bytes()) <= bytes as i64
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
                } else if let Some(s) =
                    rw.is_some().then(|| sel_split(func, ps, na, av, arg, off, ty)).flatten()
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
                let (r, o) = root(func, ps, na, args[1]);
                let o = o.wrapping_add(i64::from(i32::from(offset)));
                kill(av, r, o, ty, iso, invs, cur, invreg, rpar);
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
                        let (ra, oa) = root(func, ps, na, a);
                        kill(av, ra, oa.wrapping_add(o), ty, iso, invs, cur, invreg, rpar);
                    }
                }
                if av.len() < MAX_ENTRIES {
                    av.insert((r, o, ty), val);
                }
            }
            _ if op.is_call() => {
                if !write_free(func, i, nw, eh) {
                    av.retain(|&(r, o, t), _| {
                        iso.contains(&r)
                            || invs.contains(&(r, o, t))
                            || cur.is_some_and(|cur| {
                                invreg.get(&(r, o, t)).is_some_and(|rs| {
                                    rs.iter().any(|&t2| in_region(cur, t2, rpar))
                                })
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
                            invreg.get(&(r, o, t)).is_some_and(|rs| {
                                rs.iter().any(|&t2| in_region(cur, t2, rpar))
                            })
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
    let mut cfg = ControlFlowGraph::with_function(func);
    let mut rpo = rpo_of(&cfg);
    let mut n = 0;
    let ps = param_src(func);
    // Root-resolution stop set: real `noalias` plus frozen operands —
    // either way the value names its own memory scope. `isolated` keeps
    // the real set so operand values don't get function-wide immunity.
    let stop: FxHashSet<Value> = noalias
        .iter()
        .chain(ofrozen.keys())
        .copied()
        .collect();
    if crate::pass_enabled("PLIRON_LOADPRE") {
        n += pre(func, &ps, &stop, &cfg, &rpo, entry);
        if n > 0 {
            cfg = ControlFlowGraph::with_function(func);
            rpo = rpo_of(&cfg);
        }
    }
    let iso = isolated(func, noalias, &ps, cfrozen);
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
    for _ in 0..MAX_ITERS {
        let mut changed = false;
        for &b in &rpo {
            let Some(mut av) = input(&out, b) else {
                continue;
            };
            transfer(
                func,
                &ps,
                &stop,
                b,
                &mut av,
                None,
                &iso,
                inv_insts,
                &mut invs,
                breg.get(&b).copied(),
                ofrozen,
                &mut invreg,
                rpar,
                nw,
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
    if !stable {
        return 0;
    }
    let mut fwd: Vec<(Inst, Fwd)> = Vec::new();
    for &b in &rpo {
        if let Some(mut av) = input(&out, b) {
            transfer(
                func,
                &ps,
                &stop,
                b,
                &mut av,
                Some(&mut fwd),
                &iso,
                inv_insts,
                &mut invs,
                breg.get(&b).copied(),
                ofrozen,
                &mut invreg,
                rpar,
                nw,
                eh,
            );
        }
    }
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
/// a new block param whose args are filled in later.
fn start_val(func: &mut Function, pre: &mut Pre, b: Block, loc: Loc) -> Value {
    if let Some(&v) = pre.start.get(&(b, loc)) {
        return v;
    }
    let preds = pred_blocks(pre.cfg, b);
    let v = if let [p] = preds[..]
        && p != b
    {
        end_val(func, pre, p, loc)
    } else {
        pre.pending.push((b, loc));
        func.dfg.append_block_param(b, loc.2)
    };
    pre.start.insert((b, loc), v);
    v
}

fn end_val(func: &mut Function, pre: &mut Pre, b: Block, loc: Loc) -> Value {
    match pre.end[&b].get(&loc) {
        Some(&St::Local(v)) => v,
        Some(St::In) => start_val(func, pre, b, loc),
        None => unreachable!("loadpre: location not available at end of {b}"),
    }
}

fn kill_st(st: &mut FxHashMap<Loc, St>, r: Root, o: i64, ty: Type, iso: &FxHashSet<Root>) {
    let n = i64::from(ty.bytes());
    // Same retain rule as `kill` (minus the invariant sets `pre` doesn't
    // track): `st` must agree with the `out` availability sets or
    // `end_val`'s `St::In` lookup finds a loc `st` dropped.
    st.retain(|&(r2, o2, t2), _| match (r, r2) {
        _ if r == r2 => o2 >= o + n || o >= o2 + i64::from(t2.bytes()),
        (Root::S(_), Root::S(_)) => true,
        _ => iso.contains(&r) || iso.contains(&r2),
    });
}

/// Whether a store footprint `w = (root, offset, type)` may clobber `l` —
/// the complement of `kill_st`'s keep rule.
fn may_write(w: Loc, l: Loc) -> bool {
    let (r, o, ty) = w;
    let (r2, o2, t2) = l;
    match (r, r2) {
        _ if r == r2 => !(o2 >= o + i64::from(ty.bytes()) || o >= o2 + i64::from(t2.bytes())),
        (Root::S(_), Root::S(_)) => false,
        _ => true,
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
    cfg: &ControlFlowGraph,
    rpo: &[Block],
    entry: Block,
) -> usize {
    let iso: FxHashSet<Root> = FxHashSet::default();
    let nw: &FxHashMap<FuncRef, bool> = &FxHashMap::default();
    let eh = false;
    let reach: FxHashSet<Block> = rpo.iter().copied().collect();
    let mut out: FxHashMap<Block, FxHashSet<Loc>> = FxHashMap::default();
    let input = |out: &FxHashMap<Block, FxHashSet<Loc>>, b: Block| -> Option<FxHashSet<Loc>> {
        if b == entry || cfg.pred_iter(b).any(|p| !reach.contains(&p.block)) {
            return Some(FxHashSet::default());
        }
        let mut acc: Option<FxHashSet<Loc>> = None;
        for p in cfg.pred_iter(b) {
            if let Some(o) = out.get(&p.block) {
                match &mut acc {
                    None => acc = Some(o.clone()),
                    Some(a) => a.retain(|k| o.contains(k)),
                }
            }
        }
        acc
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
                &ps,
                na,
                b,
                &mut av,
                None,
                &iso,
                // `out` must agree with the `st` scan below, which tracks
                // no invariant immunity — pass empty sets so `transfer`
                // marks and retains nothing extra here.
                &FxHashSet::default(),
                &mut FxHashSet::default(),
                None,
                &FxHashMap::default(),
                &mut FxHashMap::default(),
                &FxHashMap::default(),
                nw,
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
        let mut writes: Vec<Loc> = Vec::new();
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
                    let (r, o) = root(func, ps, na, arg);
                    let loc = (r, o.wrapping_add(i64::from(i32::from(offset))), ty);
                    if let Some(&s) = st.get(&loc) {
                        let v = match s {
                            St::Local(v) => v,
                            St::In => start_val(func, &mut pre, b, loc),
                        };
                        fwd.push((i, v));
                        st.insert(loc, St::Local(v));
                    } else if !clobbered
                        && writes.iter().all(|&w| !may_write(w, loc))
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
                    let (r, o) = root(func, ps, na, args[1]);
                    let o = o.wrapping_add(i64::from(i32::from(offset)));
                    kill_st(&mut st, r, o, ty, &iso);
                    writes.push((r, o, ty));
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
                            let (ra, oa) = root(func, ps, na, a);
                            let a = (ra, oa.wrapping_add(o), ty);
                            kill_st(&mut st, a.0, a.1, a.2, &iso);
                            writes.push(a);
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
                    // `transfer`'s call arm keeps `iso` roots — retain the
                    // same set here or `st` disagrees with `out`.
                    st.retain(|&(r, _, _), _| iso.contains(&r));
                    clobbered = true;
                }
                _ => {}
            }
        }
        pre.end.insert(b, st);
    }
    let mut k = 0;
    while k < pre.pending.len() {
        let (b, loc) = pre.pending[k];
        k += 1;
        let mut pinsts: Vec<(Block, Inst)> = cfg.pred_iter(b).map(|p| (p.block, p.inst)).collect();
        pinsts.sort_unstable();
        pinsts.dedup();
        for (pb, pinst) in pinsts {
            let v = match pre.inserted.get(&(b, loc, pb)) {
                Some(&v) => v,
                None => end_val(func, &mut pre, pb, loc),
            };
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
    for &(i, v) in &fwd {
        let r = func.dfg.first_result(i);
        func.layout.remove_inst(i);
        func.dfg.clear_results(i);
        func.dfg.change_to_alias(r, v);
    }
    fwd.len()
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
