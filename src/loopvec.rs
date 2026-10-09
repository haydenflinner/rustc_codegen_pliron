//! Elementwise loop vectorization on CLIF (a minimal LLVM loop-vectorizer):
//! `dst[i] = f(src[i], ..)` loops whose only side effects are contiguous
//! affine loads/stores of one element type become 128-bit vector loops with
//! the original loop kept as the scalar epilogue. Runtime checks (trip-count
//! order, bounds guards, range overlap) guard the fast path, as in loopidiom.
//!
//! The scalar loop is never removed: it runs the `iters % VF` tail. Insts the
//! vector loop doesn't replicate simply stay there, so the only body members
//! that can disqualify a loop are side-effecting ops (calls, traps, other
//! memory ops) and stored values that aren't lane-wise replicable.
//!
//! Integer reductions (`s += a[i]`, `s ^= ..`, `s = max(s, a[i])`) are
//! handled the same way: the accumulator becomes a lane-wise vector of
//! partial sums folded back to a scalar on loop exit. Only associative +
//! commutative ops qualify — float `fadd`/`fmul` reductions would reorder
//! rounding and are left scalar, as LLVM requires `reassoc` for them.
//!
//! `PLIRON_VEC=0` disables it; `PLIRON_VEC_DEBUG` logs conversions.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, MemFlagsData,
    Opcode, Type, Value, ValueDef, types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::loadfwd::{self, Root};
use crate::loopidiom::{
    Count, Edge, Info, Ins, Param, Pred, count, deadend, edge_args, emit, gather, guard_dead,
    guard_pred, outv, param_kinds, scaled_addr, trips_idx, trips_ptr,
};

const MAX_LOOPS: usize = 16;
const MAX_CONV: usize = 4;
/// Sanity bound on insts scanned per loop.
const MAX_OPS: usize = 64;

macro_rules! why {
    ($($t:tt)*) => {{
        if std::env::var_os("PLIRON_VEC_DEBUG").is_some() {
            eprintln!("vec bail: {}", format_args!($($t)*));
        }
        return None;
    }};
}

/// Vector lanes for a 128-bit stream of `t`.
fn vf_of(t: Type) -> Option<u32> {
    match t {
        types::I8 => Some(16),
        types::I16 => Some(8),
        types::I32 | types::F32 => Some(4),
        types::I64 | types::F64 => Some(2),
        _ => None,
    }
}

/// Opcodes replicated lane-wise into the vector loop. Scalar icmp/fcmp feed a
/// vector bitselect; shifts take a splatted amount. Integer div/rem have no
/// NEON lowering and `imul` isn't native on 64-bit lanes — those keep the
/// loop scalar.
fn vec_op_ok(op: Opcode, elem: Type) -> bool {
    use Opcode::*;
    match op {
        Iadd | Isub | Band | Bor | Bxor | Ineg | Bnot | Smin | Smax | Umin | Umax | Fadd | Fsub
        | Fmul | Fdiv | Fmin | Fmax | Fneg | Fabs | Sqrt | Fma | Select | Icmp | Fcmp => true,
        Imul => elem.bits() <= 32,
        Ishl | Ushr | Sshr => true,
        _ => false,
    }
}

/// One affine memory stream: `addr = base + iv*rate` in iv units (`base`
/// contains `Val(iv)`), or the iv itself (`direct`, pointer-iv loops stepping
/// one element per iteration).
struct Stream {
    base: Ins,
    /// Entry-side base value for alias-root analysis.
    root_val: Value,
    direct: bool,
}

enum VPred {
    Cmp(IntCC, Ins, Ins),
    Aligned(Ins, i64),
    /// `[a, a+len)` and `[b, b+len)` don't overlap and neither wraps.
    Pair(Ins, Ins),
}

/// One scalar reduction lifted to a vector accumulator: header param `idx`,
/// updated `acc = acc ⊕ delta` on the (single) latch.
struct Reduc {
    idx: usize,
    op: Opcode,
    delta: Value,
    /// Scalar accumulator type (equals `elem` unless `widen` is set).
    aty: Type,
    /// `Some(..)` when the delta widens element lanes to `aty` (u8→u64 sums
    /// etc.) — the vector body widens before accumulating.
    widen: Option<Widen>,
    /// The `acc = acc ⊕ delta` inst (bounds the load's legal uses).
    upd: Inst,
}

enum Widen {
    /// `delta = x as acc_ty` — `delta` holds the extend's operand (or the
    /// extending-load result itself).
    Add { signed: bool },
    /// `delta = (a as acc_ty) * (b as acc_ty)` — a widening dot product,
    /// which aarch64 folds to `sdot`/`usdot` through the pairwise-add tree.
    /// `a`/`b` are the elem-typed lane values being extended.
    Mul {
        sa: bool,
        sb: bool,
        a: Value,
        b: Value,
    },
}

impl Reduc {
    /// Vector accumulator type: 128 bits of `aty` lanes.
    fn vty(&self) -> Type {
        self.aty.by(128 / self.aty.bits()).unwrap()
    }
    /// Lanes in the accumulator vector.
    fn lanes(&self) -> i64 {
        128 / self.aty.bits() as i64
    }
}

struct Plan {
    entry: Edge,
    h: Block,
    body: FxHashSet<Block>,
    entry_args: Vec<Value>,
    /// Header param index of the counted iv.
    iv_idx: usize,
    /// Scalar accumulators vectorized lane-wise (empty for store loops).
    reducs: Vec<Reduc>,
    iv: Value,
    iv_ty: Type,
    iv0: Value,
    step: i64,
    /// Scalar iteration count expression (iv units of `step`, i.e. iters).
    iters: Ins,
    preds: Vec<VPred>,
    kinds: Vec<Param>,
    elem: Type,
    vt: Type,
    vf: i64,
    /// Memory ops in layout order (per-stream program order matters).
    mems: Vec<(Inst, usize)>,
    loads: Vec<(Inst, usize)>,
    streams: Vec<Stream>,
    /// Scalar values replicable lane-wise (load results + pure op results).
    can_vec: FxHashSet<Value>,
    /// Values replicable to all-ones/all-zeros lane masks (icmp/fcmp).
    masks: FxHashSet<Value>,
}

/// `addr` as a stream: (base expr mentioning `Val(cnt.iv)`, entry-side root
/// value, direct-iv flag). `None` if not affine-contiguous at `ebytes`/iter.
fn stream_base(
    func: &Function,
    info: &Info,
    kinds: &[Param],
    cnt: &Count,
    addr: Value,
    ebytes: i64,
) -> Option<(Ins, Value, bool)> {
    let a = func.dfg.resolve_aliases(addr);
    if a == cnt.iv {
        return (cnt.step == ebytes).then_some((Ins::Val(cnt.iv), cnt.iv0, true));
    }
    let params = func.dfg.block_params(info.h).to_vec();
    for (j, &p) in params.iter().enumerate() {
        if a == p {
            // A pointer param stepping s bytes/iter: contiguous iff s == e,
            // and `entry + s*iters = C + (s/step)*iv` needs step | s.
            let Param::Step(s) = kinds[j] else {
                return None;
            };
            if s != ebytes || s % cnt.step != 0 {
                return None;
            }
            let rate = s / cnt.step;
            let c = Ins::Sub(
                Box::new(Ins::Val(info.entry_args[j])),
                Box::new(Ins::Mul(
                    Box::new(Ins::K(rate)),
                    Box::new(Ins::Val(cnt.iv0)),
                )),
            );
            let expr = Ins::Add(
                Box::new(c),
                Box::new(Ins::Mul(
                    Box::new(Ins::K(rate)),
                    Box::new(Ins::Val(cnt.iv)),
                )),
            );
            return Some((expr, info.entry_args[j], false));
        }
    }
    let (base, k) = scaled_addr(func, cnt.iv, a)?;
    if k * cnt.step != ebytes {
        return None;
    }
    let b = outv(func, info, kinds, base)?;
    let expr = Ins::Add(
        Box::new(Ins::Val(b)),
        Box::new(Ins::Mul(
            Box::new(Ins::Val(cnt.iv)),
            Box::new(Ins::K(k)),
        )),
    );
    Some((expr, b, false))
}

/// `v` is a splattable loop-invariant operand: defined outside the loop, an
/// `Inv` param (its entry value), or a constant.
fn is_splat(
    func: &Function,
    info: &Info,
    kinds: &[Param],
    cache: &mut FxHashMap<Value, bool>,
    v: Value,
) -> bool {
    let v = func.dfg.resolve_aliases(v);
    if let Some(&r) = cache.get(&v) {
        return r;
    }
    let r = outv(func, info, kinds, v).is_some() || is_const(func, v);
    cache.insert(v, r);
    r
}

fn is_const(func: &Function, v: Value) -> bool {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => matches!(
            func.dfg.insts[i].opcode(),
            Opcode::Iconst | Opcode::F32const | Opcode::F64const
        ),
        _ => false,
    }
}

fn plan(
    func: &Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    noalias: &FxHashSet<Value>,
) -> Option<Plan> {
    let info = gather(func, cfg, dt, la, lp)?;
    let kinds = param_kinds(func, &info);
    // A `Param::Other` header param can still be a reduction accumulator:
    // `acc = acc ⊕ delta` for a lane-wise associative+commutative op. Each
    // latch must pass back the *same* update inst (a multi-latch loop with
    // different deltas can't be a single-block vector body), and `acc`'s
    // only in-body use must be that update.
    let reduc_ok = |op: Opcode| {
        matches!(
            op,
            Opcode::Iadd
                | Opcode::Imul
                | Opcode::Band
                | Opcode::Bor
                | Opcode::Bxor
                | Opcode::Smin
                | Opcode::Smax
                | Opcode::Umin
                | Opcode::Umax
        )
    };
    let mut reducs: Vec<Reduc> = Vec::new();
    let mut other_ok = true;
    for (j, &k) in kinds.iter().enumerate() {
        if !matches!(k, Param::Other) {
            continue;
        }
        let p = func.dfg.block_params(info.h)[j];
        // The acc update as a lane op + delta: either a whitelisted binary
        // (`acc ⊕ d`) or a select-diamond min/max (`select(icmp cc acc d)`,
        // post-`ifconv`). `aux` is an extra inst allowed to read `acc`.
        let upd_of = |i: Inst| -> Option<(Opcode, Value, Option<Inst>)> {
            match func.dfg.insts[i] {
                InstructionData::Binary { opcode, args } if reduc_ok(opcode) => {
                    let (x, y) = (
                        func.dfg.resolve_aliases(args[0]),
                        func.dfg.resolve_aliases(args[1]),
                    );
                    let d = if x == p { y } else if y == p { x } else {
                        return None;
                    };
                    Some((opcode, d, None))
                }
                InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } => {
                    let c = func.dfg.resolve_aliases(args[0]);
                    let ValueDef::Result(ci, _) = func.dfg.value_def(c) else {
                        return None;
                    };
                    let InstructionData::IntCompare { cond, args: cargs, .. } =
                        func.dfg.insts[ci]
                    else {
                        return None;
                    };
                    let (a, b) = (
                        func.dfg.resolve_aliases(cargs[0]),
                        func.dfg.resolve_aliases(cargs[1]),
                    );
                    let (x, y) = (
                        func.dfg.resolve_aliases(args[1]),
                        func.dfg.resolve_aliases(args[2]),
                    );
                    // Both the icmp and the select pair `acc` with the same
                    // single delta value.
                    let (da, dx) = (
                        if a == p { b } else { a },
                        if x == p { y } else { x },
                    );
                    if (a == p) == (b == p) || (x == p) == (y == p) || da != dx || da == p {
                        return None;
                    }
                    let d = da;
                    // `select(a cc b, x, y)`: picking the same operand the
                    // comparison favors is min, picking the other is max.
                    use cranelift_codegen::ir::condcodes::IntCC as CC;
                    let x_is_a = x == a;
                    let op = match (cond, x_is_a) {
                        (CC::UnsignedLessThan | CC::UnsignedLessThanOrEqual, true)
                        | (CC::UnsignedGreaterThanOrEqual | CC::UnsignedGreaterThan, false) => {
                            Opcode::Umin
                        }
                        (CC::UnsignedGreaterThanOrEqual | CC::UnsignedGreaterThan, true)
                        | (CC::UnsignedLessThan | CC::UnsignedLessThanOrEqual, false) => {
                            Opcode::Umax
                        }
                        (CC::SignedLessThan | CC::SignedLessThanOrEqual, true)
                        | (CC::SignedGreaterThanOrEqual | CC::SignedGreaterThan, false) => {
                            Opcode::Smin
                        }
                        (CC::SignedGreaterThanOrEqual | CC::SignedGreaterThan, true)
                        | (CC::SignedLessThan | CC::SignedLessThanOrEqual, false) => {
                            Opcode::Smax
                        }
                        _ => return None,
                    };
                    Some((op, d, Some(ci)))
                }
                _ => None,
            }
        };
        let mut upd: Option<(Inst, Opcode, Value, Option<Inst>)> = None;
        let ok = info.latches.iter().all(|&e| {
            let a = func.dfg.resolve_aliases(edge_args(func, e)[j]);
            let ValueDef::Result(i, _) = func.dfg.value_def(a) else {
                return false;
            };
            let Some((op, d, aux)) = upd_of(i) else {
                return false;
            };
            match upd {
                None => {
                    upd = Some((i, op, d, aux));
                    true
                }
                Some((pi, po, pd, paux)) => pi == i && po == op && pd == d && paux == aux,
            }
        });
        if !ok {
            if std::env::var_os("PLIRON_VEC_DEBUG").is_some() {
                eprintln!("vec reduc?: param {j} latch args not one update inst");
            }
            other_ok = false;
            continue;
        }
        let (ui, op, delta, aux) = upd.unwrap();
        // `acc` may not be read anywhere else in the loop (e.g. an exit
        // test on the accumulator would need the lane-wise partial sums);
        // the select's icmp is a legitimate extra reader.
        let allowed: FxHashSet<Inst> = [ui].into_iter().chain(aux).collect();
        let bad_use = info.body.iter().flat_map(|&b| func.layout.block_insts(b)).any(|i| {
            !allowed.contains(&i)
                && func.dfg.inst_args(i).iter().any(|&a| func.dfg.resolve_aliases(a) == p)
        });
        if bad_use || reducs.len() >= 2 {
            if std::env::var_os("PLIRON_VEC_DEBUG").is_some() {
                eprintln!("vec reduc?: param {j} extra use");
            }
            other_ok = false;
            continue;
        }
        reducs.push(Reduc {
            idx: j,
            op,
            delta,
            aty: func.dfg.value_type(p),
            widen: None,
            upd: ui,
        });
    }
    if !other_ok {
        why!("non-linear param {:?}", info.h);
    }
    // Interior-block params would need threading through the vector loop.
    if info
        .body
        .iter()
        .any(|&b| b != info.h && !func.dfg.block_params(b).is_empty())
    {
        why!("interior block params {:?}", info.h);
    }
    // The count exit: pre-tested only, so the epilogue may run 0 iterations.
    // `count`'s `store` arg is a body inst used for post-tested detection —
    // pass a real memory op, not the latch terminator (which never dominates
    // its own edge).
    let body_mem = info
        .body
        .iter()
        .flat_map(|&b| func.layout.block_insts(b))
        .find(|&i| func.dfg.insts[i].opcode().can_load() || func.dfg.insts[i].opcode().can_store())?;
    let (_exit, cnt, extra) = info.exits.iter().find_map(|&e| {
        let c = count(func, dt, &info, &kinds, e, body_mem)?;
        if c.post_tested {
            return None;
        }
        let mut ps = Vec::new();
        for &e2 in &info.exits {
            if e2 == e {
                continue;
            }
            if !guard_dead(func, &c, e2) {
                ps.push(guard_pred(func, &info, &kinds, &c, e2)?);
            }
        }
        Some((e, c, ps))
    })?;
    // Scan body insts into contiguous affine mem ops and the rest, in layout
    // order — the per-stream program order must be preserved in the vector
    // loop (a store followed by a load on the same stream reads back the new
    // value).
    let mut elem: Option<Type> = None;
    let mut mems: Vec<Inst> = Vec::new();
    let mut other: Vec<Inst> = Vec::new();
    for b in func
        .layout
        .blocks()
        .filter(|b| info.body.contains(b) && !deadend(func, *b))
        .collect::<Vec<_>>()
    {
        for i in func.layout.block_insts(b) {
            let op = func.dfg.insts[i].opcode();
            if op.is_terminator() {
                match op {
                    Opcode::Jump => {}
                    Opcode::Brif if info.exits.iter().any(|e| e.inst == i) => {}
                    _ => why!("terminator {}", func.dfg.display_inst(i)),
                }
                continue;
            }
            let t = match func.dfg.insts[i] {
                InstructionData::Load {
                    opcode: Opcode::Load,
                    offset,
                    ..
                } if i32::from(offset) == 0 && loadfwd::notrap(func, i) => {
                    func.dfg.value_type(func.dfg.first_result(i))
                }
                // Extending loads (e.g. `a[i] as u64` from `&[u8]` →
                // `uload8.i64`): the element width is the access width; the
                // widened result may only feed a reduction accumulator.
                InstructionData::Load { opcode, offset, .. }
                    if i32::from(offset) == 0 && loadfwd::notrap(func, i) =>
                {
                    match opcode {
                        Opcode::Uload8 | Opcode::Sload8 => types::I8,
                        Opcode::Uload16 | Opcode::Sload16 => types::I16,
                        Opcode::Uload32 | Opcode::Sload32 => types::I32,
                        _ => {
                            other.push(i);
                            continue;
                        }
                    }
                }
                InstructionData::Store {
                    opcode: Opcode::Store,
                    args,
                    offset,
                    ..
                } if i32::from(offset) == 0 && loadfwd::notrap(func, i) => {
                    func.dfg.value_type(func.dfg.resolve_aliases(args[0]))
                }
                _ => {
                    other.push(i);
                    continue;
                }
            };
            if vf_of(t).is_none() {
                why!("elem type {t}");
            }
            match elem {
                None => elem = Some(t),
                Some(e) if e == t => {}
                _ => why!("mixed elem types"),
            }
            mems.push(i);
        }
    }
    let Some(elem) = elem else {
        why!("no mem ops");
    };
    let ebytes = i64::from(elem.bytes());
    let Some(vf) = vf_of(elem) else {
        why!("vf");
    };
    let Some(vt) = elem.by(vf) else {
        why!("vec ty");
    };
    if mems.len() + other.len() > MAX_OPS {
        why!("too many ops");
    }
    // Group mem insts into streams by identical address value; an in-place
    // `a[i] = f(a[i])` shares one address SSA value and merges naturally.
    let mut streams: Vec<Stream> = Vec::new();
    let mut by_addr: FxHashMap<Value, usize> = FxHashMap::default();
    let mut loads: Vec<(Inst, usize)> = Vec::new();
    let mut stores: Vec<(Inst, usize)> = Vec::new();
    let mut mems2: Vec<(Inst, usize)> = Vec::new();
    for &i in &mems {
        let (arg, is_store) = match func.dfg.insts[i] {
            InstructionData::Load { arg, .. } => (arg, false),
            InstructionData::Store { args, .. } => (args[1], true),
            _ => unreachable!(),
        };
        let a = func.dfg.resolve_aliases(arg);
        let idx = match by_addr.get(&a) {
            Some(&j) => j,
            None => {
                let Some((base, rootv, direct)) =
                    stream_base(func, &info, &kinds, &cnt, a, ebytes)
                else {
                    why!("addr {}", func.dfg.display_inst(i));
                };
                streams.push(Stream {
                    base,
                    root_val: rootv,
                    direct,
                });
                by_addr.insert(a, streams.len() - 1);
                streams.len() - 1
            }
        };
        if streams.len() > 8 {
            why!("too many streams");
        }
        mems2.push((i, idx));
        (if is_store { &mut stores } else { &mut loads }).push((i, idx));
    }
    if stores.is_empty() && reducs.is_empty() {
        why!("no stores");
    }
    // Disjointness: for every (load,store) and (store,store) pair on distinct
    // streams, prove non-overlap on roots or emit a runtime range check.
    // Same-stream pairs are per-iteration same-address — safe in groups.
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for &(_, li) in loads.iter().chain(&stores) {
        for &(_, si) in &stores {
            if li == si {
                continue;
            }
            let (lr, _) = loadfwd::root(func, streams[li].root_val);
            let (sr, _) = loadfwd::root(func, streams[si].root_val);
            let na = |r: Root| matches!(r, Root::V(v) if noalias.contains(&v));
            let proven = match (lr, sr) {
                (Root::S(a), Root::S(b)) => a != b,
                _ => {
                    lr != sr
                        && ((na(lr) && (na(sr) || matches!(sr, Root::S(_))))
                            || (na(sr) && matches!(lr, Root::S(_))))
                }
            };
            if !proven && !pairs.contains(&(li, si)) {
                pairs.push((li, si));
            }
        }
    }
    // Loop-invariant values: outv-able, or a pure in-loop inst whose operands
    // are all invariant (e.g. rustc's `band k, 31` shift mask). emit_scalar
    // re-materializes these in the vector block.
    let mut inv: FxHashSet<Value> = FxHashSet::default();
    loop {
        let mut grew = false;
        for &i in &other {
            let op = func.dfg.insts[i].opcode();
            if op.is_terminator()
                || op.can_trap()
                || op.can_load()
                || op.can_store()
                || op.is_call()
                || op.other_side_effects()
            {
                continue;
            }
            let rs = func.dfg.inst_results(i);
            if rs.len() != 1 {
                continue;
            }
            let rv = func.dfg.resolve_aliases(rs[0]);
            if inv.contains(&rv) {
                continue;
            }
            if func.dfg.inst_args(i).iter().all(|&a| {
                let a = func.dfg.resolve_aliases(a);
                inv.contains(&a) || outv(func, &info, &kinds, a).is_some()
            }) {
                inv.insert(rv);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    // Lane-wise replicability: seeds are load results; a whitelisted op is
    // replicable when all operands are replicable or splattable.
    let mut can_vec: FxHashSet<Value> = FxHashSet::default();
    let mut masks: FxHashSet<Value> = FxHashSet::default();
    let mut splat_cache: FxHashMap<Value, bool> = FxHashMap::default();
    splat_cache.extend(inv.iter().map(|&v| (v, true)));
    for &(l, _) in &loads {
        can_vec.insert(func.dfg.resolve_aliases(func.dfg.first_result(l)));
    }
    loop {
        let mut grew = false;
        for &i in &other {
            let rs = func.dfg.inst_results(i);
            if rs.is_empty() {
                continue;
            }
            let rv = func.dfg.resolve_aliases(rs[0]);
            if can_vec.contains(&rv) || masks.contains(&rv) {
                continue;
            }
            let data = &func.dfg.insts[i];
            if !vec_op_ok(data.opcode(), elem) {
                continue;
            }
            let ok = match data {
                // Vector shifts take a *scalar* amount — per-lane amounts
                // aren't expressible, so arg 1 must be loop-invariant.
                InstructionData::Binary {
                    opcode: Opcode::Ishl | Opcode::Ushr | Opcode::Sshr,
                    args,
                } => {
                    let x = func.dfg.resolve_aliases(args[0]);
                    let s = func.dfg.resolve_aliases(args[1]);
                    (can_vec.contains(&x) || is_splat(func, &info, &kinds, &mut splat_cache, x))
                        && is_splat(func, &info, &kinds, &mut splat_cache, s)
                }
                InstructionData::Binary { args, .. } => args.iter().all(|&a| {
                    let a = func.dfg.resolve_aliases(a);
                    can_vec.contains(&a) || is_splat(func, &info, &kinds, &mut splat_cache, a)
                }),
                InstructionData::Unary { arg, .. } => {
                    let a = func.dfg.resolve_aliases(*arg);
                    can_vec.contains(&a) || is_splat(func, &info, &kinds, &mut splat_cache, a)
                }
                InstructionData::IntCompare { args, .. }
                | InstructionData::FloatCompare { args, .. } => {
                    let ok = args.iter().all(|&a| {
                        let a = func.dfg.resolve_aliases(a);
                        can_vec.contains(&a) || is_splat(func, &info, &kinds, &mut splat_cache, a)
                    });
                    if ok {
                        masks.insert(rv);
                        grew = true;
                    }
                    continue;
                }
                InstructionData::Ternary {
                    args,
                    opcode: Opcode::Select,
                    ..
                } => masks.contains(&func.dfg.resolve_aliases(args[0]))
                    && args[1..].iter().all(|&a| {
                        let a = func.dfg.resolve_aliases(a);
                        can_vec.contains(&a) || is_splat(func, &info, &kinds, &mut splat_cache, a)
                    }),
                InstructionData::Ternary { args, .. } => args.iter().all(|&a| {
                    let a = func.dfg.resolve_aliases(a);
                    can_vec.contains(&a) || is_splat(func, &info, &kinds, &mut splat_cache, a)
                }),
                _ => false,
            };
            if ok {
                can_vec.insert(rv);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    // Reductions: the accumulator is element-typed unless the delta widens
    // it (`acc += a[i] as u64` — a pairwise-widen chain reaches the acc lane
    // width), the op must have a vector form, and the delta must be
    // lane-replicable. Widening is limited to `iadd`: the other ops can't be
    // applied at a wider lane type without changing the result.
    for r in &mut reducs {
        if r.aty != elem {
            if !widen_delta(func, &info, &can_vec, elem, r) {
                why!("reduction type/op {:?}", info.h);
            }
            continue;
        }
        if !vec_op_ok(r.op, elem) {
            why!("reduction type/op {:?}", info.h);
        }
        let d = func.dfg.resolve_aliases(r.delta);
        if !can_vec.contains(&d) {
            why!("reduction delta {:?}", info.h);
        }
    }
    // Every stored value must be replicable or a splattable invariant.
    for &(s, _) in &stores {
        let InstructionData::Store { args, .. } = func.dfg.insts[s] else {
            unreachable!()
        };
        let v = func.dfg.resolve_aliases(args[0]);
        if !can_vec.contains(&v) && !is_splat(func, &info, &kinds, &mut splat_cache, v) {
            why!("store value {}", func.dfg.display_inst(s));
        }
    }
    // Remaining `other` insts stay in the scalar epilogue — only side effects
    // (calls, traps, extra memory ops) make an iteration non-skippable.
    for &i in &other {
        let op = func.dfg.insts[i].opcode();
        if op.can_trap() || op.can_load() || op.can_store() || op.is_call()
            || op.other_side_effects()
        {
            why!("side effects {}", func.dfg.display_inst(i));
        }
        if func.dfg.inst_results(i).len() > 1 {
            why!("multi-result {}", func.dfg.display_inst(i));
        }
    }
    // Trip count in scalar iterations, plus its order/divisibility preds.
    let (iters, it_preds) = if streams.iter().any(|s| s.direct) {
        let (len, ps) = trips_ptr(&cnt, ebytes)?;
        (Ins::Div(Box::new(len), Box::new(Ins::K(ebytes))), ps)
    } else {
        trips_idx(&cnt)?
    };
    let mut preds: Vec<VPred> = extra
        .into_iter()
        .chain(it_preds)
        .map(|p| match p {
            Pred::Cmp(cc, a, b) => VPred::Cmp(cc, a, b),
            Pred::Aligned(a, s) => VPred::Aligned(a, s),
            Pred::Disjoint => unreachable!(),
        })
        .collect();
    for (a, b) in pairs {
        preds.push(VPred::Pair(streams[a].base.clone(), streams[b].base.clone()));
    }
    let iv_idx = func
        .dfg
        .block_params(info.h)
        .iter()
        .position(|&p| p == cnt.iv)?;
    Some(Plan {
        entry: info.entry,
        h: info.h,
        body: info.body.iter().copied().collect(),
        entry_args: info.entry_args,
        iv_idx,
        iv: cnt.iv,
        iv_ty: func.dfg.value_type(cnt.iv),
        iv0: cnt.iv0,
        step: cnt.step,
        reducs,
        iters,
        preds,
        kinds,
        elem,
        vt,
        vf: i64::from(vf),
        mems: mems2,
        loads,
        streams,
        can_vec,
        masks,
    })
}

/// `e` with the counted iv's `Val` replaced by `w`.
fn subst(e: &Ins, iv: Value, w: Value) -> Ins {
    match e {
        Ins::Val(v) => Ins::Val(if *v == iv { w } else { *v }),
        Ins::K(k) => Ins::K(*k),
        Ins::Add(a, b) => Ins::Add(Box::new(subst(a, iv, w)), Box::new(subst(b, iv, w))),
        Ins::Sub(a, b) => Ins::Sub(Box::new(subst(a, iv, w)), Box::new(subst(b, iv, w))),
        Ins::SatSub(a, b) => {
            Ins::SatSub(Box::new(subst(a, iv, w)), Box::new(subst(b, iv, w)))
        }
        Ins::Mul(a, b) => Ins::Mul(Box::new(subst(a, iv, w)), Box::new(subst(b, iv, w))),
        Ins::And(a, b) => Ins::And(Box::new(subst(a, iv, w)), Box::new(subst(b, iv, w))),
        Ins::Div(a, b) => Ins::Div(Box::new(subst(a, iv, w)), Box::new(subst(b, iv, w))),
    }
}

/// Emit `v` as a scalar usable inside `vb`: Inv params resolve to their entry
/// argument, loop-local constants and pure invariants are re-materialized by
/// cloning the inst with remapped operands, outside values are used directly
/// (they dominate `vb`). Anything else is a bug in `plan`.
fn emit_scalar(
    pos: &mut FuncCursor,
    p: &Plan,
    memo: &mut FxHashMap<Value, Value>,
    v: Value,
    depth: usize,
) -> Value {
    let v = pos.func.dfg.resolve_aliases(v);
    if let Some(&s) = memo.get(&v) {
        return s;
    }
    if depth > 32 {
        panic!("emit_scalar cycle");
    }
    let out = match pos.func.dfg.value_def(v) {
        ValueDef::Param(b, j) if b == p.h => match p.kinds[j] {
            Param::Inv => p.entry_args[j],
            _ => unreachable!("non-invariant param used as scalar"),
        },
        // Invariant inst defined inside the loop: clone into vb.
        ValueDef::Result(i, _)
            if pos
                .func
                .layout
                .inst_block(i)
                .is_some_and(|b| p.body.contains(&b)) =>
        {
            let op = pos.func.dfg.insts[i].opcode();
            if op.can_trap()
                || op.can_load()
                || op.can_store()
                || op.is_call()
                || op.is_terminator()
                || op.other_side_effects()
            {
                unreachable!("side-effecting inst used as scalar invariant")
            }
            let ni = pos.func.dfg.clone_inst(i);
            let args: Vec<Value> = pos.func.dfg.inst_args(ni).to_vec();
            let vals: Vec<Value> = args
                .into_iter()
                .map(|a| emit_scalar(pos, p, memo, a, depth + 1))
                .collect();
            pos.func.dfg.overwrite_inst_values(ni, vals.into_iter());
            pos.insert_inst(ni);
            pos.func.dfg.first_result(ni)
        }
        _ => v,
    };
    memo.insert(v, out);
    out
}

/// Emit `v`'s vector value in `vb`, recursively materializing producers:
/// loop-local insts become lane-wise ops; invariants/constants are splatted.
fn emit_val(
    pos: &mut FuncCursor,
    p: &Plan,
    vmap: &mut FxHashMap<Value, Value>,
    splats: &mut FxHashMap<Value, Value>,
    smemo: &mut FxHashMap<Value, Value>,
    addrs: &[Value],
    v: Value,
    depth: usize,
) -> Value {
    let v = pos.func.dfg.resolve_aliases(v);
    if let Some(&vv) = vmap.get(&v) {
        return vv;
    }
    if depth > 64 {
        panic!("loopvec emit cycle");
    }
    let vt = p.vt;
    // Not lane-replicable: an invariant or constant operand — splat it.
    if !p.can_vec.contains(&v) && !p.masks.contains(&v) {
        if let Some(&s) = splats.get(&v) {
            return s;
        }
        let sv = emit_scalar(pos, p, smemo, v, 0);
        let s = pos.ins().splat(vt, sv);
        splats.insert(v, s);
        return s;
    }
    macro_rules! m {
        ($x:expr) => {
            emit_val(pos, p, vmap, splats, smemo, addrs, $x, depth + 1)
        };
    }
    let ValueDef::Result(i, _) = pos.func.dfg.value_def(v) else {
        unreachable!("replicable value not an inst result")
    };
    let data = pos.func.dfg.insts[i];
    let out = match data {
                InstructionData::Load { .. } => {
                    let j = p.loads.iter().find(|&&(l, _)| l == i).unwrap().1;
                    pos.ins()
                        .load(vt, MemFlagsData::new().with_notrap(), addrs[j], 0)
                }
                InstructionData::Binary { opcode, args } => {
                    let a = m!(args[0]);
                    // Shift amounts stay scalar (Cranelift has no per-lane
                    // vector shift); everything else is lane-wise.
                    let b = if matches!(opcode, Opcode::Ishl | Opcode::Ushr | Opcode::Sshr) {
                        emit_scalar(pos, p, smemo, args[1], 0)
                    } else {
                        m!(args[1])
                    };
                    match opcode {
                        Opcode::Iadd => pos.ins().iadd(a, b),
                        Opcode::Isub => pos.ins().isub(a, b),
                        Opcode::Imul => pos.ins().imul(a, b),
                        Opcode::Band => pos.ins().band(a, b),
                        Opcode::Bor => pos.ins().bor(a, b),
                        Opcode::Bxor => pos.ins().bxor(a, b),
                        Opcode::Smin => pos.ins().smin(a, b),
                        Opcode::Smax => pos.ins().smax(a, b),
                        Opcode::Umin => pos.ins().umin(a, b),
                        Opcode::Umax => pos.ins().umax(a, b),
                        Opcode::Ishl => pos.ins().ishl(a, b),
                        Opcode::Ushr => pos.ins().ushr(a, b),
                        Opcode::Sshr => pos.ins().sshr(a, b),
                        Opcode::Fadd => pos.ins().fadd(a, b),
                        Opcode::Fsub => pos.ins().fsub(a, b),
                        Opcode::Fmul => pos.ins().fmul(a, b),
                        Opcode::Fdiv => pos.ins().fdiv(a, b),
                        Opcode::Fmin => pos.ins().fmin(a, b),
                        Opcode::Fmax => pos.ins().fmax(a, b),
                        _ => unreachable!(),
                    }
                }
                InstructionData::Unary { opcode, arg } => {
                    let a = m!(arg);
                    match opcode {
                        Opcode::Ineg => pos.ins().ineg(a),
                        Opcode::Bnot => pos.ins().bnot(a),
                        Opcode::Fneg => pos.ins().fneg(a),
                        Opcode::Fabs => pos.ins().fabs(a),
                        Opcode::Sqrt => pos.ins().sqrt(a),
                        _ => unreachable!(),
                    }
                }
                InstructionData::IntCompare { cond, args, .. } => {
                    let (x, y) = (m!(args[0]), m!(args[1]));
                    pos.ins().icmp(cond, x, y)
                }
                InstructionData::FloatCompare { cond, args, .. } => {
                    let (x, y) = (m!(args[0]), m!(args[1]));
                    pos.ins().fcmp(cond, x, y)
                }
                InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } => {
                    let c = m!(args[0]);
                    let (x, y) = (m!(args[1]), m!(args[2]));
                    // Mask lanes are int-typed; bitselect needs the data type.
                    let xt = pos.func.dfg.value_type(x);
                    let c = if pos.func.dfg.value_type(c) != xt {
                        pos.ins().bitcast(xt, MemFlagsData::new(), c)
                    } else {
                        c
                    };
                    pos.ins().bitselect(c, x, y)
                }
                InstructionData::Ternary {
                    opcode: Opcode::Fma,
                    args,
                } => {
                    let (x, y, z) = (
                        m!(args[0]),
                        m!(args[1]),
                        m!(args[2]),
                    );
                    pos.ins().fma(x, y, z)
                }
            _ => unreachable!("non-vectorizable inst in emit_val"),
        };
    vmap.insert(v, out);
    out
}

/// Validate a widening reduction `acc:T += delta` where T is wider than the
/// element type. Sets `r.widen`/`r.delta` on success.
///
/// `delta` may be wrapped in `ireduce`s (rustc widens then truncates when the
/// cast type exceeds the acc type — truncating a wider extension is the
/// extension). The remaining chain must be one of:
/// - `uextend`/`sextend` of an elem-typed lane value → `Widen::Add`
/// - an extending load (`uload8.i64` &co) → `Widen::Add`
/// - `imul` of two such extensions → `Widen::Mul` (widening dot product)
///
/// Wide intermediate values may only feed the delta chain: anything else in
/// the body reading them would be replicated at the wrong width.
fn widen_delta(
    func: &Function,
    info: &Info,
    can_vec: &FxHashSet<Value>,
    elem: Type,
    r: &mut Reduc,
) -> bool {
    if r.op != Opcode::Iadd
        || !r.aty.is_int()
        || r.aty.bits() > 64
        || r.aty.bits() <= elem.bits()
    {
        return false;
    }
    let mut d = func.dfg.resolve_aliases(r.delta);
    let mut chain: Vec<Inst> = Vec::new();
    let di = loop {
        let ValueDef::Result(i, _) = func.dfg.value_def(d) else {
            return false;
        };
        match func.dfg.insts[i] {
            InstructionData::Unary {
                opcode: Opcode::Ireduce,
                arg,
            } => {
                chain.push(i);
                d = func.dfg.resolve_aliases(arg);
            }
            _ => break i,
        }
    };
    // Uses of `v` inside the body must stay within `ok_insts` — anything else
    // would try to read it as a scalar or replicate it at the wrong width.
    let uses_ok = |func: &Function, v: Value, ok_insts: &[Inst]| {
        info.body
            .iter()
            .flat_map(|&b| func.layout.block_insts(b))
            .all(|i| {
                ok_insts.contains(&i)
                    || !func
                        .dfg
                        .inst_args(i)
                        .iter()
                        .any(|&a| func.dfg.resolve_aliases(a) == v)
            })
    };
    // One extension edge: `(x as wide)` or an extending load → (elem value,
    // signed), possibly wrapped in `ireduce`s (rustc widens to i64 then
    // truncates to the acc type). `ext_insts` accumulates the insts whose
    // results participate.
    let mut ext_insts: Vec<Inst> = vec![di];
    let ext_of = |func: &Function, v: Value, ext_insts: &mut Vec<Inst>| {
        let mut v = func.dfg.resolve_aliases(v);
        loop {
            let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
                return None;
            };
            match func.dfg.insts[i] {
                InstructionData::Unary {
                    opcode: Opcode::Ireduce,
                    arg,
                } => {
                    ext_insts.push(i);
                    v = func.dfg.resolve_aliases(arg);
                }
                InstructionData::Unary { opcode, arg }
                    if matches!(opcode, Opcode::Uextend | Opcode::Sextend) =>
                {
                    ext_insts.push(i);
                    return Some((func.dfg.resolve_aliases(arg), opcode == Opcode::Sextend));
                }
                InstructionData::Load { opcode, .. }
                    if matches!(
                        opcode,
                        Opcode::Uload8
                            | Opcode::Sload8
                            | Opcode::Uload16
                            | Opcode::Sload16
                            | Opcode::Uload32
                            | Opcode::Sload32
                    ) =>
                {
                    ext_insts.push(i);
                    return Some((
                        v,
                        matches!(
                            opcode,
                            Opcode::Sload8 | Opcode::Sload16 | Opcode::Sload32
                        ),
                    ));
                }
                _ => return None,
            }
        }
    };
    match func.dfg.insts[di] {
        InstructionData::Binary {
            opcode: Opcode::Imul,
            args,
        } => {
            let Some((a, sa)) = ext_of(func, args[0], &mut ext_insts) else {
                return false;
            };
            let Some((b, sb)) = ext_of(func, args[1], &mut ext_insts) else {
                return false;
            };
            // The `usdot` lowering pattern only matches unsigned×signed —
            // normalize signed×unsigned into it (imul is commutative).
            let (a, sa, b, sb) = if sa && !sb { (b, sb, a, sa) } else { (a, sa, b, sb) };
            if !can_vec.contains(&a) || !can_vec.contains(&b) {
                return false;
            }
            // Product lanes need 2*elem bits — cap at i64.
            if 2 * elem.bits() > 64 {
                return false;
            }
            let mut ok = vec![r.upd];
            ok.extend(chain.iter().copied());
            ok.extend(ext_insts.iter().copied());
            for &i in &ext_insts {
                let v = func.dfg.resolve_aliases(func.dfg.first_result(i));
                if !uses_ok(func, v, &ok) {
                    return false;
                }
            }
            r.widen = Some(Widen::Mul { sa, sb, a, b });
            true
        }
        _ => {
            let Some((x, signed)) = ext_of(func, d, &mut ext_insts) else {
                return false;
            };
            if !can_vec.contains(&x) {
                return false;
            }
            let mut ok = vec![r.upd];
            ok.extend(chain.iter().copied());
            ok.extend(ext_insts.iter().copied());
            for &i in &ext_insts {
                let v = func.dfg.resolve_aliases(func.dfg.first_result(i));
                if !uses_ok(func, v, &ok) {
                    return false;
                }
            }
            r.delta = x;
            r.widen = Some(Widen::Add { signed });
            true
        }
    }
}

/// The identity element for a reduction op at lane type `elem`.
fn red_identity(op: Opcode, elem: Type) -> i64 {
    let bits = elem.bits();
    let mask = if bits < 64 { (1i64 << bits) - 1 } else { -1 };
    match op {
        Opcode::Iadd | Opcode::Bor | Opcode::Bxor | Opcode::Umax => 0,
        Opcode::Imul => 1,
        Opcode::Band | Opcode::Umin => mask,
        Opcode::Smin => mask >> 1,
        Opcode::Smax => (mask >> 1) + 1, // = INT_MIN once sign-extended
        _ => unreachable!(),
    }
}

/// Scalar `a ⊕ b` for a reduction op.
fn red_emit(pos: &mut FuncCursor, op: Opcode, a: Value, b: Value) -> Value {
    match op {
        Opcode::Iadd => pos.ins().iadd(a, b),
        Opcode::Imul => pos.ins().imul(a, b),
        Opcode::Band => pos.ins().band(a, b),
        Opcode::Bor => pos.ins().bor(a, b),
        Opcode::Bxor => pos.ins().bxor(a, b),
        Opcode::Smin => pos.ins().smin(a, b),
        Opcode::Smax => pos.ins().smax(a, b),
        Opcode::Umin => pos.ins().umin(a, b),
        Opcode::Umax => pos.ins().umax(a, b),
        _ => unreachable!(),
    }
}

/// Signed or unsigned `widen_low`/`widen_high` pair.
fn widen_halves(
    pos: &mut FuncCursor,
    signed: bool,
    v: Value,
) -> (Value, Value) {
    if signed {
        (pos.ins().swiden_low(v), pos.ins().swiden_high(v))
    } else {
        (pos.ins().uwiden_low(v), pos.ins().uwiden_high(v))
    }
}

fn lane_bits(pos: &FuncCursor, v: Value) -> u32 {
    pos.func.dfg.value_type(v).lane_type().bits()
}

/// Widen `v`'s lanes to `aty` width via the `uwiden_low/high`+`iadd_pairwise`
/// chain — on aarch64 each level folds to `uaddlp`/`saddlp`. The last level
/// uses a plain `iadd` of the widened halves: `iadd_pairwise` isn't defined
/// at i64 lanes, and the lane partition is irrelevant since `ve` sums all
/// lanes anyway.
fn widen_vec(pos: &mut FuncCursor, signed: bool, mut v: Value, aty: Type) -> Value {
    let abits = aty.bits();
    loop {
        let lb = lane_bits(pos, v);
        debug_assert!(lb < abits);
        let (wl, wh) = widen_halves(pos, signed, v);
        if 2 * lb == abits {
            return pos.ins().iadd(wl, wh);
        }
        v = pos.ins().iadd_pairwise(wl, wh);
    }
}

/// Widening dot product `vacc += (a as T) * (b as T)` where `a`/`b` are
/// elem-width vectors: products of the widened halves (exact at 2*elem bits),
/// then pairwise-summed down to `aty` lanes. At the i8→i32 width this emits
/// exactly the tree aarch64's `sdot`/`usdot` ISLE rules match.
fn widen_mul(
    pos: &mut FuncCursor,
    sa: bool,
    sb: bool,
    av: Value,
    bv: Value,
    aty: Type,
) -> Value {
    let abits = aty.bits();
    let (al, ah) = widen_halves(pos, sa, av);
    let (bl, bh) = widen_halves(pos, sb, bv);
    let mut parts = vec![pos.ins().imul(al, bl), pos.ins().imul(ah, bh)];
    // The product lanes' own sign for further widening (u*u products fit
    // unsigned; anything else is a signed quantity).
    let ps = sa || sb;
    loop {
        let w = lane_bits(pos, parts[0]);
        if w == abits {
            // Fold the part vectors into one: pairwise only for the i8→i32
            // tree (the `sdot`/`udot`/`usdot` shape). Everywhere else a plain
            // elementwise `iadd` keeps the `smlal`/`umlal` folds lane-exact.
            let i8_dot = w == 32 && lane_bits(pos, av) == 8;
            let mut it = parts.into_iter();
            let mut acc_v = it.next().unwrap();
            if let Some(p2) = it.next() {
                acc_v = if it.len() == 0 && i8_dot {
                    pos.ins().iadd_pairwise(acc_v, p2)
                } else {
                    pos.ins().iadd(acc_v, p2)
                };
            }
            for p in it {
                acc_v = pos.ins().iadd(acc_v, p);
            }
            return acc_v;
        }
        let mut next = Vec::with_capacity(parts.len());
        for p in parts {
            let (l, h) = widen_halves(pos, ps, p);
            // `iadd_pairwise` isn't defined at i64 lanes.
            next.push(if 2 * w == 64 {
                pos.ins().iadd(l, h)
            } else {
                pos.ins().iadd_pairwise(l, h)
            });
        }
        parts = next;
    }
}

/// Vector-body unroll factor: two 128-bit groups per iteration hide load
/// latency and halve loop overhead, matching part of LLVM's default unroll.
const UNROLL: usize = 4;

fn apply(func: &mut Function, p: &Plan, pty: Type) {
    let pb = func.layout.inst_block(p.entry.inst).unwrap();
    let (cb, vh, vb, ve) = (
        func.dfg.make_block(),
        func.dfg.make_block(),
        func.dfg.make_block(),
        func.dfg.make_block(),
    );
    func.layout.insert_block_after(cb, pb);
    func.layout.insert_block_after(vh, cb);
    func.layout.insert_block_after(vb, vh);
    func.layout.insert_block_after(ve, vb);
    let iv_ty = p.iv_ty;
    // Redirect the scalar loop's entry edge through the check block.
    {
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[p.entry.inst].branch_destination_mut(
            &mut dfg.jump_tables,
            &mut dfg.exception_tables,
        )[p.entry.slot];
        *bc = BlockCall::new(cb, core::iter::empty(), &mut dfg.value_lists);
    }
    let ivv = func.dfg.append_block_param(vh, iv_ty);
    let endv = func.dfg.append_block_param(vh, iv_ty);
    let nm = func.dfg.append_block_param(vh, iv_ty);
    let mut pos = FuncCursor::new(func).at_bottom(cb);
    // nm = iters & -(VF*UNROLL) ; end_v = iv0 + nm*step.
    let iters = emit(&mut pos, iv_ty, &p.iters);
    let mk = pos.ins().iconst(iv_ty, -p.vf * UNROLL as i64);
    let nmv = pos.ins().band(iters, mk);
    let sk = pos.ins().iconst(iv_ty, p.step);
    let off = pos.ins().imul(nmv, sk);
    let end = pos.ins().iadd(p.iv0, off);
    // Predicates; all must hold to take the fast path.
    let mut ok: Option<Value> = None;
    for pr in &p.preds {
        let c = match pr {
            VPred::Cmp(cc, a, b) => {
                let (a, b) = (emit(&mut pos, iv_ty, a), emit(&mut pos, iv_ty, b));
                pos.ins().icmp(*cc, a, b)
            }
            VPred::Aligned(a, size) => {
                let m = emit(&mut pos, pty, a);
                let k = pos.ins().iconst(pty, size - 1);
                let r = pos.ins().band(m, k);
                let z = pos.ins().iconst(pty, 0);
                pos.ins().icmp(IntCC::Equal, r, z)
            }
            VPred::Pair(a, b) => {
                let lenb = emit(
                    &mut pos,
                    pty,
                    &Ins::Mul(
                        Box::new(p.iters.clone()),
                        Box::new(Ins::K(i64::from(p.elem.bytes()))),
                    ),
                );
                // Range starts at iteration 0: substitute the entry value.
                let alo = emit(&mut pos, pty, &subst(a, p.iv, p.iv0));
                let blo = emit(&mut pos, pty, &subst(b, p.iv, p.iv0));
                let ahi = pos.ins().iadd(alo, lenb);
                let bhi = pos.ins().iadd(blo, lenb);
                let nwa = pos.ins().icmp(IntCC::UnsignedLessThanOrEqual, alo, ahi);
                let nwb = pos.ins().icmp(IntCC::UnsignedLessThanOrEqual, blo, bhi);
                let x = pos.ins().icmp(IntCC::UnsignedLessThanOrEqual, ahi, blo);
                let y = pos.ins().icmp(IntCC::UnsignedLessThanOrEqual, bhi, alo);
                let nw = pos.ins().band(nwa, nwb);
                let xy = pos.ins().bor(x, y);
                pos.ins().band(nw, xy)
            }
        };
        ok = Some(match ok {
            None => c,
            Some(o) => pos.ins().band(o, c),
        });
    }
    // The vector iv must not wrap: end_v >= iv0.
    let nw = pos
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, p.iv0, end);
    ok = Some(match ok {
        None => nw,
        Some(o) => pos.ins().band(o, nw),
    });
    let ok = ok.unwrap();
    // Vector accumulators (one per unroll group per reduction) start at the
    // op's identity splatted to all lanes; acc0 is folded in on exit.
    let mut v_arg_vals = vec![p.iv0, end, nmv];
    for r in &p.reducs {
        for _ in 0..UNROLL {
            let id = pos.ins().iconst(r.aty, red_identity(r.op, r.aty));
            let v = pos.ins().splat(r.vty(), id);
            v_arg_vals.push(v);
        }
    }
    let h_args: Vec<BlockArg> = p.entry_args.iter().map(|&v| BlockArg::Value(v)).collect();
    let v_args: Vec<BlockArg> = v_arg_vals.iter().map(|&v| BlockArg::Value(v)).collect();
    pos.ins().brif(ok, vh, &v_args, p.h, &h_args);
    // vh: the vector guard. The exit goes through `ve`, which folds each
    // vector accumulator to a scalar so the epilogue resumes mid-accumulation.
    let vaccs: Vec<Vec<Value>> = p
        .reducs
        .iter()
        .map(|r| {
            (0..UNROLL)
                .map(|_| pos.func.dfg.append_block_param(vh, r.vty()))
                .collect()
        })
        .collect();
    let mut pos = FuncCursor::new(pos.func).at_bottom(vh);
    let c = pos.ins().icmp(IntCC::UnsignedLessThan, ivv, endv);
    pos.ins().brif(c, vb, &[], ve, &[]);
    // ve: scalar epilogue entry — resume at iv_v with stepping params advanced
    // by s*nm and each reduction acc = acc0 ⊕ fold(vacc).
    let params = pos.func.dfg.block_params(p.h).to_vec();
    let mut pos = FuncCursor::new(pos.func).at_bottom(ve);
    let mut epi: Vec<Value> = Vec::new();
    for (j, _) in params.iter().enumerate() {
        if j == p.iv_idx {
            epi.push(ivv);
            continue;
        }
        if let Some((k, r)) = p.reducs.iter().enumerate().find(|(_, r)| r.idx == j) {
            // Fold each unroll group's accumulator to a scalar, then combine
            // (reassociation is exact for the whitelisted ops).
            let mut acc = p.entry_args[j];
            for &vacc in &vaccs[k] {
                let mut s = pos.ins().extractlane(vacc, 0);
                for l in 1..r.lanes() {
                    let lane = pos.ins().extractlane(vacc, l as u8);
                    s = red_emit(&mut pos, r.op, s, lane);
                }
                acc = red_emit(&mut pos, r.op, acc, s);
            }
            epi.push(acc);
            continue;
        }
        match p.kinds[j] {
            Param::Inv => epi.push(p.entry_args[j]),
            Param::Step(s) => {
                let sk = pos.ins().iconst(iv_ty, s);
                let d = pos.ins().imul(nm, sk);
                let v = pos.ins().iadd(p.entry_args[j], d);
                epi.push(v);
            }
            Param::Other => unreachable!(),
        }
    }
    let eargs: Vec<BlockArg> = epi.iter().map(|&v| BlockArg::Value(v)).collect();
    pos.ins().jump(p.h, &eargs);
    // vb: UNROLL groups of the body. Group g's streams are offset g*VF
    // elements ahead; same-stream program order is preserved within each
    // group, and groups advance together (a cross-group read-back hazard
    // would need a store->load distance < VF*UNROLL, which can't exist:
    // same-stream addresses differ by whole iterations).
    let mut pos = FuncCursor::new(pos.func).at_bottom(vb);
    let mut splats: FxHashMap<Value, Value> = FxHashMap::default();
    let mut smemo: FxHashMap<Value, Value> = FxHashMap::default();
    let mut back_accs: Vec<Vec<Value>> = p.reducs.iter().map(|_| Vec::new()).collect();
    let gb = i64::from(p.elem.bytes()) * p.vf;
    for g in 0..UNROLL {
        let mut addrs = Vec::new();
        for s in &p.streams {
            let a = if s.direct {
                ivv
            } else {
                emit(&mut pos, pty, &subst(&s.base, p.iv, ivv))
            };
            let a = if g == 0 {
                a
            } else {
                // `direct` streams step `ebytes` per scalar iteration too.
                let off = pos.ins().iconst(pty, gb * g as i64);
                pos.ins().iadd(a, off)
            };
            addrs.push(a);
        }
        // Emit loads eagerly in program order: on a given stream a
        // `store; load` pair reads back the stored vector, so ordering is
        // observable.
        let mut vmap: FxHashMap<Value, Value> = FxHashMap::default();
        for &(i, j) in &p.mems {
            match pos.func.dfg.insts[i] {
                InstructionData::Load { .. } => {
                    let r = pos.func.dfg.resolve_aliases(pos.func.dfg.first_result(i));
                    if p.can_vec.contains(&r) && !vmap.contains_key(&r) {
                        let vl =
                            pos.ins()
                                .load(p.vt, MemFlagsData::new().with_notrap(), addrs[j], 0);
                        vmap.insert(r, vl);
                    }
                }
                InstructionData::Store { args, .. } => {
                    let vv = emit_val(
                        &mut pos, p, &mut vmap, &mut splats, &mut smemo, &addrs, args[0], 0,
                    );
                    pos.ins()
                        .store(MemFlagsData::new().with_notrap(), vv, addrs[j], 0);
                }
                _ => unreachable!(),
            }
        }
        // vacc = vacc ⊕ delta per group; deltas come after the group's
        // memory ops so their loads are already in `vmap`.
        for (k, r) in p.reducs.iter().enumerate() {
            let vd = match &r.widen {
                None => emit_val(
                    &mut pos, p, &mut vmap, &mut splats, &mut smemo, &addrs, r.delta, 0,
                ),
                Some(Widen::Add { signed }) => {
                    let v = emit_val(
                        &mut pos, p, &mut vmap, &mut splats, &mut smemo, &addrs, r.delta, 0,
                    );
                    widen_vec(&mut pos, *signed, v, r.aty)
                }
                Some(Widen::Mul { sa, sb, a, b }) => {
                    let av = emit_val(
                        &mut pos, p, &mut vmap, &mut splats, &mut smemo, &addrs, *a, 0,
                    );
                    let bv = emit_val(
                        &mut pos, p, &mut vmap, &mut splats, &mut smemo, &addrs, *b, 0,
                    );
                    widen_mul(&mut pos, *sa, *sb, av, bv, r.aty)
                }
            };
            // `sdot`/`usdot` lowering patterns match `iadd(dot_tree, acc)`
            // positionally; emit the delta first so the fold can fire.
            let acc = vaccs[k][g];
            back_accs[k].push(if matches!(r.widen, Some(Widen::Mul { .. })) {
                red_emit(&mut pos, r.op, vd, acc)
            } else {
                red_emit(&mut pos, r.op, acc, vd)
            });
        }
    }
    let k = pos.ins().iconst(iv_ty, p.step * p.vf * UNROLL as i64);
    let iv2 = pos.ins().iadd(ivv, k);
    let mut back: Vec<Value> = vec![iv2, endv, nm];
    for accs in &back_accs {
        back.extend(accs);
    }
    let bargs: Vec<BlockArg> = back.iter().map(|&v| BlockArg::Value(v)).collect();
    pos.ins().jump(vh, &bargs);
}

pub fn run(
    func: &mut Function,
    fname: &str,
    noalias: &FxHashSet<Value>,
    tcfg: cranelift_codegen::isa::TargetFrontendConfig,
    simd: bool,
) -> usize {
    if !simd {
        return 0;
    }
    // Bisection gate: PLIRON_VEC_SKIP=foo,bar skips vectorizing matching fns.
    if let Some(skip) = std::env::var_os("PLIRON_VEC_SKIP")
        && !skip.is_empty()
        && skip.to_string_lossy().split(',').any(|s| fname.contains(&*s))
    {
        return 0;
    }
    let cfg = ControlFlowGraph::with_function(func);
    let dt = DominatorTree::with_function(func, &cfg);
    let mut la = LoopAnalysis::new();
    la.compute(func, &cfg, &dt);
    let loops: Vec<Loop> = la.loops().collect();
    let debug = std::env::var_os("PLIRON_VEC_DEBUG").is_some();
    let pty = tcfg.pointer_type();
    let mut n = 0;
    for lp in loops.into_iter().take(MAX_LOOPS) {
        let Some(p) = plan(func, &cfg, &dt, &la, lp, noalias) else {
            continue;
        };
        if debug {
            eprintln!("vec {:?}: {}x{}", p.h, p.vf, p.elem);
        }
        apply(func, &p, pty);
        n += 1;
        if n >= MAX_CONV {
            break;
        }
    }
    n
}
