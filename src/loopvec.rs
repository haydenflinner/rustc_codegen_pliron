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
    Block, BlockArg, BlockCall, ConstantData, Endianness, Function, Inst, InstBuilder,
    InstructionData, MemFlagsData, Opcode, Type, Value, ValueDef, types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

use crate::loadfwd::{self, Root};
use crate::loopidiom::{
    Count, Edge, Info, Ins, Param, Pred, count, deadend, def_block, edge_args, emit, gather,
    guard_dead, guard_pred, iconst, outv, param_kinds, trips_idx, trips_ptr,
};

const MAX_LOOPS: usize = 16;
const MAX_CONV: usize = 4;
/// Sanity bound on insts scanned per loop.
const MAX_OPS: usize = 64;

macro_rules! why {
    ($f:expr; $($t:tt)*) => {{
        if std::env::var_os("PLIRON_VEC_DEBUG").is_some() {
            eprintln!("vec bail {}: {}", $f, format_args!($($t)*));
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
/// one element per iteration). `neg` marks a descending stream (`rate < 0`,
/// e.g. `src[n-1-i]`): the vector body loads the contiguous block whose
/// lanes run opposite to iteration order and reverses them.
struct Stream {
    base: Ins,
    /// Entry-side base value for alias-root analysis.
    root_val: Value,
    direct: bool,
    neg: bool,
}

enum VPred {
    Cmp(IntCC, Ins, Ins),
    Aligned(Ins, i64),
    /// `[a, a+len)` and `[b, b+len)` don't overlap and neither wraps. The
    /// bools mark descending streams: their touched range ends at `a(iv0)`
    /// rather than starting there.
    Pair(Ins, bool, Ins, bool),
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
    /// `Some(..)` for `acc = select(c, acc ⊕ d, acc)`: the delta is masked
    /// lane-wise by `c` (inverted when `d` sat on the select's false arm).
    cond: Option<Cond>,
    /// The `acc = acc ⊕ delta` inst (bounds the load's legal uses).
    upd: Inst,
    /// Extra inst allowed to read `acc` (the min/max icmp, or the
    /// conditional update's `acc ⊕ d`).
    aux: Option<Inst>,
}

/// A conditional-update predicate, planned to evaluate per lane: `insts` are
/// the in-body insts of the mask/data tree to re-emit at element lanes.
struct Cond {
    c: Value,
    invert: bool,
    /// `c` is a plain flag, not a mask — wrap its lane value in `icmp ne 0`.
    needs_ne: bool,
    insts: FxHashSet<Inst>,
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
    /// Do-while form: the body runs before the exit test, so the scalar
    /// epilogue must be entered only with ≥1 iteration left (`nm` is then
    /// rounded down from `iters-1`).
    post_tested: bool,
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
    /// Cond-tree insts a conditional reduction may vectorize past the
    /// `can_vec` gate (`emit_val` drops their widening casts).
    extra_vec: FxHashSet<Inst>,
    /// Lane-wise early-exit tests: `brif` on a body-local condition. The
    /// vector body checks `vany_true`/`vall_true` per group and on a hit
    /// resumes the scalar loop at the group's first lane — the scalar body
    /// then re-finds the exact lane and takes the real exit edge with its
    /// original args.
    early: Vec<(Value, bool)>,
}

/// `v` as `coeff*iv + off`, with `off` lifted to an `Ins` expr over values
/// visible outside the loop (pure in-body arithmetic is re-expressed, e.g.
/// `isub(bound-1, iv)` → coeff -1). `None` when `v` isn't affine in `iv` or
/// the invariant part isn't liftable.
fn affine(
    func: &Function,
    info: &Info,
    kinds: &[Param],
    iv: Value,
    v: Value,
    depth: u8,
) -> Option<(i64, Ins)> {
    if depth > 8 {
        return None;
    }
    let v = func.dfg.resolve_aliases(v);
    if v == iv {
        return Some((1, Ins::K(0)));
    }
    if let Some(k) = iconst(func, v) {
        return Some((0, Ins::K(k)));
    }
    if let Some(o) = outv(func, info, kinds, v) {
        return Some((0, Ins::Val(o)));
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return None;
    };
    if !func.layout.inst_block(i).is_some_and(|b| info.body.contains(&b)) {
        return None;
    }
    let d = depth + 1;
    let (ca, cb, mk) = match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } => {
            let (ca, oa) = affine(func, info, kinds, iv, a, d)?;
            let (cb, ob) = affine(func, info, kinds, iv, b, d)?;
            (ca, cb, Ins::Add(Box::new(oa), Box::new(ob)))
        }
        InstructionData::Binary {
            opcode: Opcode::Isub,
            args: [a, b],
        } => {
            let (ca, oa) = affine(func, info, kinds, iv, a, d)?;
            let (cb, ob) = affine(func, info, kinds, iv, b, d)?;
            (ca, cb.wrapping_neg(), Ins::Sub(Box::new(oa), Box::new(ob)))
        }
        InstructionData::Binary {
            opcode: Opcode::Imul,
            args: [a, b],
        } => {
            let (ca, oa) = affine(func, info, kinds, iv, a, d)?;
            let (cb, ob) = affine(func, info, kinds, iv, b, d)?;
            if ca != 0 && cb != 0 {
                return None;
            }
            if ca == 0 && cb == 0 {
                (0, 0, Ins::Mul(Box::new(oa), Box::new(ob)))
            } else if ca != 0 {
                let m = ins_k(&ob)?;
                (ca.wrapping_mul(m), 0, Ins::Mul(Box::new(oa), Box::new(Ins::K(m))))
            } else {
                let m = ins_k(&oa)?;
                (0, cb.wrapping_mul(m), Ins::Mul(Box::new(ob), Box::new(Ins::K(m))))
            }
        }
        InstructionData::Binary {
            opcode: Opcode::Ishl,
            args: [a, b],
        } => {
            let (ca, oa) = affine(func, info, kinds, iv, a, d)?;
            let s = iconst(func, b)?;
            if !(0..64).contains(&s) {
                return None;
            }
            let m = 1i64 << s;
            (
                ca.wrapping_mul(m),
                0,
                Ins::Mul(Box::new(oa), Box::new(Ins::K(m))),
            )
        }
        InstructionData::Unary {
            opcode: Opcode::Ineg,
            arg,
        } => {
            let (ca, oa) = affine(func, info, kinds, iv, arg, d)?;
            (
                0,
                ca.wrapping_neg(),
                Ins::Sub(Box::new(Ins::K(0)), Box::new(oa)),
            )
        }
        // Pointer-width extension of an index expr: read at target width.
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => return affine(func, info, kinds, iv, arg, d),
        _ => return None,
    };
    Some((ca.wrapping_add(cb), mk))
}

/// `e` evaluates to a literal constant.
fn ins_k(e: &Ins) -> Option<i64> {
    match e {
        Ins::K(k) => Some(*k),
        _ => None,
    }
}

/// Leftmost non-iv leaf of `e` — the stream's base value for alias analysis.
fn root_leaf(e: &Ins, iv: Value) -> Option<Value> {
    match e {
        Ins::Val(v) if *v != iv => Some(*v),
        Ins::Add(a, b)
        | Ins::Sub(a, b)
        | Ins::SatSub(a, b)
        | Ins::Mul(a, b)
        | Ins::And(a, b)
        | Ins::Div(a, b) => root_leaf(a, iv).or_else(|| root_leaf(b, iv)),
        _ => None,
    }
}

/// `addr` as a stream: (base expr mentioning `Val(cnt.iv)`, entry-side root
/// value, direct-iv flag, descending flag). `None` if not affine-contiguous
/// at `ebytes`/iter.
fn stream_base(
    func: &Function,
    info: &Info,
    kinds: &[Param],
    cnt: &Count,
    addr: Value,
    ebytes: i64,
) -> Option<(Ins, Value, bool, bool)> {
    let a = func.dfg.resolve_aliases(addr);
    if a == cnt.iv {
        return (cnt.step == ebytes).then_some((Ins::Val(cnt.iv), cnt.iv0, true, false));
    }
    let params = func.dfg.block_params(info.h).to_vec();
    for (j, &p) in params.iter().enumerate() {
        if a == p {
            // A pointer param stepping s bytes/iter: contiguous iff s == e,
            // and `entry + s*iters = C + (s/step)*iv` needs step | s.
            let Param::Step(s) = kinds[j] else {
                return None;
            };
            if s != ebytes && s != -ebytes {
                return None;
            }
            if s % cnt.step != 0 {
                return None;
            }
            let rate = s / cnt.step;
            let neg = rate < 0;
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
            return Some((expr, info.entry_args[j], false, neg));
        }
    }
    let (k, off) = affine(func, info, kinds, cnt.iv, a, 0)?;
    if k * cnt.step != ebytes && k * cnt.step != -ebytes {
        return None;
    }
    let neg = k * cnt.step < 0;
    let expr = Ins::Add(
        Box::new(off.clone()),
        Box::new(Ins::Mul(
            Box::new(Ins::Val(cnt.iv)),
            Box::new(Ins::K(k)),
        )),
    );
    Some((expr, root_leaf(&off, cnt.iv).unwrap_or(cnt.iv0), false, neg))
}

/// `e` is an early-exit test usable lane-wise: a `brif` on a body-local
/// scalar condition (icmp/fcmp replicability is checked against `masks`
/// once the body scan finishes). Returns (cond value, exits-when-true).
fn early_cond(func: &Function, info: &Info, e: Edge) -> Option<(Value, bool)> {
    let InstructionData::Brif { arg, .. } = func.dfg.insts[e.inst] else {
        return None;
    };
    let c = func.dfg.resolve_aliases(arg);
    if !def_block(func, c).is_some_and(|b| info.body.contains(&b)) {
        return None;
    }
    Some((c, e.slot == 0))
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
    fname: &str,
) -> Option<Plan> {
    let Some(info) = gather(func, cfg, dt, la, lp) else {
        why!(fname; "gather loop {:?}", lp);
    };
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
        // The acc update as a lane op + delta: a whitelisted binary
        // (`acc ⊕ d`), a select-diamond min/max (`select(icmp cc acc d)`,
        // post-`ifconv`), or a conditional update (`select(c, acc ⊕ d, acc)`
        // — `filter`-style folds, masked lane-wise). `aux` is an extra inst
        // allowed to read `acc`; `cond` carries (predicate, invert) for the
        // conditional form.
        let upd_of = |i: Inst| -> Option<(Opcode, Value, Option<Inst>, Option<(Value, bool)>)> {
            match func.dfg.insts[i] {
                InstructionData::Binary { opcode, args } if reduc_ok(opcode) => {
                    let (x, y) = (
                        func.dfg.resolve_aliases(args[0]),
                        func.dfg.resolve_aliases(args[1]),
                    );
                    let d = if x == p { y } else if y == p { x } else {
                        return None;
                    };
                    Some((opcode, d, None, None))
                }
                InstructionData::Ternary {
                    opcode: Opcode::Select,
                    args,
                } => {
                    let c = func.dfg.resolve_aliases(args[0]);
                    let (x, y) = (
                        func.dfg.resolve_aliases(args[1]),
                        func.dfg.resolve_aliases(args[2]),
                    );
                    if let ValueDef::Result(ci, _) = func.dfg.value_def(c)
                        && let InstructionData::IntCompare { cond, args: cargs, .. } =
                            func.dfg.insts[ci]
                    {
                        let (a, b) = (
                            func.dfg.resolve_aliases(cargs[0]),
                            func.dfg.resolve_aliases(cargs[1]),
                        );
                        // Both the icmp and the select pair `acc` with the
                        // same single delta value.
                        let (da, dx) = (
                            if a == p { b } else { a },
                            if x == p { y } else { x },
                        );
                        if (a == p) != (b == p) && (x == p) != (y == p) && da == dx && da != p {
                            let d = da;
                            // `select(a cc b, x, y)`: picking the same operand
                            // the comparison favors is min, the other is max.
                            use cranelift_codegen::ir::condcodes::IntCC as CC;
                            let x_is_a = x == a;
                            let op = match (cond, x_is_a) {
                                (CC::UnsignedLessThan | CC::UnsignedLessThanOrEqual, true)
                                | (
                                    CC::UnsignedGreaterThanOrEqual | CC::UnsignedGreaterThan,
                                    false,
                                ) => Opcode::Umin,
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
                            return Some((op, d, Some(ci), None));
                        }
                    }
                    // `select(c, acc ⊕ d, acc)`: `acc ⊕ (d & mask(c))` per
                    // lane. Only zero-identity ops compose this way. `c` may
                    // not read `acc` (checked in `cond_tree`).
                    let (inner, invert) = if y == p && x != p {
                        (x, false)
                    } else if x == p && y != p {
                        (y, true)
                    } else {
                        return None;
                    };
                    let ValueDef::Result(ii, _) = func.dfg.value_def(inner) else {
                        return None;
                    };
                    let InstructionData::Binary {
                        opcode:
                            op @ (Opcode::Iadd | Opcode::Bor | Opcode::Bxor),
                        args: iargs,
                    } = func.dfg.insts[ii]
                    else {
                        return None;
                    };
                    let (ia, ib) = (
                        func.dfg.resolve_aliases(iargs[0]),
                        func.dfg.resolve_aliases(iargs[1]),
                    );
                    let d = if ia == p { ib } else if ib == p { ia } else {
                        return None;
                    };
                    if d == p || c == p {
                        return None;
                    }
                    Some((op, d, Some(ii), Some((c, invert))))
                }
                _ => None,
            }
        };
        let mut upd: Option<(Inst, Opcode, Value, Option<Inst>, Option<(Value, bool)>)> = None;
        let ok = info.latches.iter().all(|&e| {
            let a = func.dfg.resolve_aliases(edge_args(func, e)[j]);
            let ValueDef::Result(i, _) = func.dfg.value_def(a) else {
                return false;
            };
            let Some((op, d, aux, cnd)) = upd_of(i) else {
                return false;
            };
            match upd {
                None => {
                    upd = Some((i, op, d, aux, cnd));
                    true
                }
                Some((pi, po, pd, paux, pc)) => {
                    pi == i && po == op && pd == d && paux == aux && pc == cnd
                }
            }
        });
        if !ok {
            if std::env::var_os("PLIRON_VEC_DEBUG").is_some() {
                eprintln!("vec reduc?: param {j} latch args not one update inst");
            }
            other_ok = false;
            continue;
        }
        let (ui, op, delta, aux, cnd) = upd.unwrap();
        // `acc` may not be read anywhere else in the loop (e.g. an exit
        // test on the accumulator would need the lane-wise partial sums);
        // the select's icmp and the conditional update's inner op are
        // legitimate extra readers, as are insts whose results are dead.
        let mut used: FxHashSet<Value> = FxHashSet::default();
        for &b in &info.body {
            for i in func.layout.block_insts(b) {
                used.extend(func.dfg.inst_args(i).iter().map(|&a| func.dfg.resolve_aliases(a)));
            }
        }
        let allowed: FxHashSet<Inst> = [ui].into_iter().chain(aux).collect();
        // A pure inst whose results are all dead can read `acc` harmlessly
        // (dead insts aren't emitted in the vector body); anything else makes
        // the loop non-vectorizable.
        let bad_use = info
            .body
            .iter()
            .flat_map(|&b| func.layout.block_insts(b))
            .filter(|&i| !deadend(func, func.layout.inst_block(i).unwrap()))
            .any(|i| {
            if allowed.contains(&i)
                || !func.dfg.inst_args(i).iter().any(|&a| func.dfg.resolve_aliases(a) == p)
            {
                return false;
            }
            let op = func.dfg.insts[i].opcode();
            let pure = !(op.can_trap()
                || op.can_load()
                || op.can_store()
                || op.is_call()
                || op.is_terminator()
                || op.other_side_effects());
            !(pure
                && func
                    .dfg
                    .inst_results(i)
                    .iter()
                    .all(|&r| !used.contains(&func.dfg.resolve_aliases(r))))
        });
        if bad_use || reducs.len() >= 2 {
            if std::env::var_os("PLIRON_VEC_DEBUG").is_some() {
                for &b in &info.body {
                    for i in func.layout.block_insts(b) {
                        if !allowed.contains(&i)
                            && func.dfg.inst_args(i).iter().any(|&a| func.dfg.resolve_aliases(a) == p)
                        {
                            eprintln!("vec reduc?: param {j} extra use {}", func.dfg.display_inst(i));
                        }
                    }
                }
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
            cond: cnd.map(|(c, invert)| Cond {
                c,
                invert,
                needs_ne: false,
                insts: FxHashSet::default(),
            }),
            upd: ui,
            aux,
        });
    }
    if !other_ok {
        why!(fname; "non-linear param {:?}", info.h);
    }
    // Interior-block params would need threading through the vector loop.
    if info
        .body
        .iter()
        .any(|&b| b != info.h && !func.dfg.block_params(b).is_empty())
    {
        why!(fname; "interior block params {:?}", info.h);
    }
    // The count exit: pre-tested only, so the epilogue may run 0 iterations.
    // `count`'s `store` arg is a body inst used for post-tested detection —
    // pass a real memory op, not the latch terminator (which never dominates
    // its own edge).
    let Some(body_mem) = info
        .body
        .iter()
        .flat_map(|&b| func.layout.block_insts(b))
        .find(|&i| func.dfg.insts[i].opcode().can_load() || func.dfg.insts[i].opcode().can_store())
        else {
            why!(fname; "no body mem op {:?}", info.h);
        };
    let mut early: Vec<(Value, bool)> = Vec::new();
    let Some((_exit, cnt, extra)) = info.exits.iter().find_map(|&e| {
        let c = count(func, dt, &info, &kinds, e, body_mem)?;
        let mut ps = Vec::new();
        for &e2 in &info.exits {
            if e2 == e {
                continue;
            }
            if !guard_dead(func, &c, e2) {
                match guard_pred(func, &info, &kinds, &c, e2) {
                    Some(pr) => ps.push(pr),
                    // Otherwise a lane-wise early-exit test is still OK for
                    // pure search loops (validated after the body scan).
                    None => early.push(early_cond(func, &info, e2)?),
                }
            }
        }
        Some((e, c, ps))
    }) else {
        why!(fname; "no counting exit {:?}", info.h);
    };
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
                    _ => why!(fname; "terminator {}", func.dfg.display_inst(i)),
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
                why!(fname; "elem type {t}");
            }
            match elem {
                None => elem = Some(t),
                Some(e) if e == t => {}
                _ => why!(fname; "mixed elem types"),
            }
            mems.push(i);
        }
    }
    let Some(elem) = elem else {
        why!(fname; "no mem ops");
    };
    let ebytes = i64::from(elem.bytes());
    let Some(vf) = vf_of(elem) else {
        why!(fname; "vf");
    };
    let Some(vt) = elem.by(vf) else {
        why!(fname; "vec ty");
    };
    if mems.len() + other.len() > MAX_OPS {
        why!(fname; "too many ops");
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
                let Some((base, rootv, direct, neg)) =
                    stream_base(func, &info, &kinds, &cnt, a, ebytes)
                else {
                    why!(fname; "addr {}", func.dfg.display_inst(i));
                };
                streams.push(Stream {
                    base,
                    root_val: rootv,
                    direct,
                    neg,
                });
                by_addr.insert(a, streams.len() - 1);
                streams.len() - 1
            }
        };
        if streams.len() > 8 {
            why!(fname; "too many streams");
        }
        mems2.push((i, idx));
        (if is_store { &mut stores } else { &mut loads }).push((i, idx));
    }
    if stores.is_empty() && reducs.is_empty() && early.is_empty() {
        why!(fname; "no stores");
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
    let mut extra_vec: FxHashSet<Inst> = FxHashSet::default();
    for r in &mut reducs {
        let mut extra_ok: FxHashSet<Inst> = r.aux.into_iter().collect();
        if let Some(cd) = &mut r.cond {
            let p = func.dfg.block_params(info.h)[r.idx];
            if !cond_mask(
                func,
                &info,
                &kinds,
                &can_vec,
                &mut splat_cache,
                elem,
                p,
                cd,
            ) {
                why!(fname; "cond reduc {:?}", info.h);
            }
            extra_ok.extend(cd.insts.iter().copied());
            extra_vec.extend(cd.insts.iter().copied());
        }
        if r.aty != elem {
            if !widen_delta(
                func,
                &info,
                &can_vec,
                &kinds,
                &mut splat_cache,
                elem,
                r,
                &extra_ok,
            ) {
                why!(fname; "reduction type/op {:?}", info.h);
            }
            continue;
        }
        if !vec_op_ok(r.op, elem) {
            why!(fname; "reduction type/op {:?}", info.h);
        }
        let d = func.dfg.resolve_aliases(r.delta);
        if !can_vec.contains(&d) {
            why!(fname; "reduction delta {:?}", info.h);
        }
    }
    // Early exits are only safe for pure search loops: a store would write
    // lanes past the hit, a reduction can't resume mid-sum, and a non-unit
    // step makes the resume position expensive to reconstruct.
    if !early.is_empty()
        && (!stores.is_empty() || !reducs.is_empty() || cnt.step != 1)
    {
        why!(fname; "early exit needs step=1, no stores, no reducs {:?}", info.h);
    }
    for &(cv, _) in &early {
        let cv = func.dfg.resolve_aliases(cv);
        if !masks.contains(&cv) && !can_vec.contains(&cv) {
            why!(fname; "early cond not lane-wise {:?}", info.h);
        }
    }
    // Every stored value must be replicable or a splattable invariant.
    for &(s, _) in &stores {
        let InstructionData::Store { args, .. } = func.dfg.insts[s] else {
            unreachable!()
        };
        let v = func.dfg.resolve_aliases(args[0]);
        if !can_vec.contains(&v) && !is_splat(func, &info, &kinds, &mut splat_cache, v) {
            why!(fname; "store value {}", func.dfg.display_inst(s));
        }
    }
    // Remaining `other` insts stay in the scalar epilogue — only side effects
    // (calls, traps, extra memory ops) make an iteration non-skippable.
    for &i in &other {
        let op = func.dfg.insts[i].opcode();
        if op.can_trap() || op.can_load() || op.can_store() || op.is_call()
            || op.other_side_effects()
        {
            why!(fname; "side effects {}", func.dfg.display_inst(i));
        }
        if func.dfg.inst_results(i).len() > 1 {
            why!(fname; "multi-result {}", func.dfg.display_inst(i));
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
    // The post-tested epilogue jumps straight into the body: it needs ≥1
    // remaining iteration (`nm = (iters-1) & -V` requires `iters ≥ 1`).
    if cnt.post_tested {
        preds.push(VPred::Cmp(
            IntCC::UnsignedGreaterThanOrEqual,
            iters.clone(),
            Ins::K(1),
        ));
    }
    for (a, b) in pairs {
        preds.push(VPred::Pair(
            streams[a].base.clone(),
            streams[a].neg,
            streams[b].base.clone(),
            streams[b].neg,
        ));
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
        post_tested: cnt.post_tested,
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
        extra_vec,
        early,
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
    let extra = matches!(
        pos.func.dfg.value_def(v),
        ValueDef::Result(i, _) if p.extra_vec.contains(&i)
    );
    if !p.can_vec.contains(&v) && !p.masks.contains(&v) && !extra {
        if let Some(&s) = splats.get(&v) {
            return s;
        }
        let lane = vt.lane_type();
        // A constant operand splats to a `vconst` directly (the egraph would
        // fold `splat(iconst)` to one anyway): crucially, `uwiden_*(vconst)`
        // has no simplify rule, so the widen node survives to the lowering
        // patterns that match `imul(uwiden_*, uwiden_*)` for `umull`/`umlal`.
        let kv = match pos.func.dfg.value_def(v) {
            ValueDef::Result(i, _) => match pos.func.dfg.insts[i] {
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm,
                } => Some(imm.bits()),
                _ => None,
            },
            _ => None,
        };
        if let Some(k) = kv {
            let m = if lane.bits() >= 64 {
                u64::MAX
            } else {
                (1u64 << lane.bits()) - 1
            };
            let k = (k as u64) & m;
            let lb = lane.bytes() as usize;
            let mut bytes = Vec::with_capacity(vt.bytes() as usize);
            while bytes.len() < vt.bytes() as usize {
                bytes.extend_from_slice(&k.to_le_bytes()[..lb]);
            }
            let c = pos.func.dfg.constants.insert(bytes.into());
            let s = pos.ins().vconst(vt, c);
            splats.insert(v, s);
            return s;
        }
        let sv = emit_scalar(pos, p, smemo, v, 0);
        // Cond-tree operands may sit wider than the lanes (e.g. `icmp.i64
        // ult byte, 128`): truncate — `mask_node`'s signedness checks proved
        // the masked bits reproduce the operand.
        let sv = if pos.func.dfg.value_type(sv) == lane {
            sv
        } else {
            pos.ins().ireduce(lane, sv)
        };
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
                    let v = pos
                        .ins()
                        .load(vt, MemFlagsData::new().with_notrap(), addrs[j], 0);
                    if p.streams[j].neg {
                        vreverse(pos, vt, v)
                    } else {
                        v
                    }
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
                        // Casts drop at element lanes — `cond_data` only
                        // admits ones where trunc-evaluation commutes.
                        Opcode::Uextend | Opcode::Sextend | Opcode::Ireduce => a,
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

/// Reverse `v`'s element lanes (a descending stream's contiguous block runs
/// opposite to iteration order). On aarch64 this is one `tbl`.
fn vreverse(pos: &mut FuncCursor, vt: Type, v: Value) -> Value {
    let e = vt.lane_type().bytes() as usize;
    let n = vt.bytes() as usize;
    let mut mask = vec![0u8; n];
    for i in 0..n {
        mask[i] = ((n - e - i / e * e) + i % e) as u8;
    }
    let imm = pos
        .func
        .dfg
        .immediates
        .push(ConstantData::from(mask.as_slice()));
    // `shuffle` is typed i8x16x2 in this Cranelift; bitcast in and out.
    // The lane-count change needs an explicit endianness: `little` keeps
    // byte i of the i8x16 view equal to byte i of each lane, so the mask
    // permutes lanes while preserving intra-element byte order.
    let le = MemFlagsData::new().with_endianness(Endianness::Little);
    let b = pos.ins().bitcast(types::I8X16, le, v);
    let b = pos.ins().shuffle(b, b, imm);
    pos.ins().bitcast(vt, le, b)
}

/// Emit a conditional-update predicate's lane mask: `icmp ne v, 0` when the
/// root is a plain flag value, `bnot` when the delta sat on the false arm.
fn emit_mask(
    pos: &mut FuncCursor,
    p: &Plan,
    vmap: &mut FxHashMap<Value, Value>,
    splats: &mut FxHashMap<Value, Value>,
    smemo: &mut FxHashMap<Value, Value>,
    addrs: &[Value],
    cd: &Cond,
) -> Value {
    let mut m = emit_val(pos, p, vmap, splats, smemo, addrs, cd.c, 0);
    if cd.needs_ne {
        let z = pos.ins().iconst(p.elem, 0);
        let z = pos.ins().splat(p.vt, z);
        m = pos.ins().icmp(IntCC::NotEqual, m, z);
    }
    if cd.invert {
        m = pos.ins().bnot(m);
    }
    m
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
    kinds: &[Param],
    splat_cache: &mut FxHashMap<Value, bool>,
    elem: Type,
    r: &mut Reduc,
    extra_ok: &FxHashSet<Inst>,
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
    // One multiplicand operand: an extension edge, or an integer constant —
    // `(x as wide) * C` splats `C` at element lanes and widens it there, so
    // `C` must survive a lane round-trip exactly.
    let opnd = |func: &Function, v: Value, ext_insts: &mut Vec<Inst>| {
        if let Some(x) = ext_of(func, v, ext_insts) {
            return Some(x);
        }
        let v = func.dfg.resolve_aliases(v);
        let e = elem.bits();
        let c = iconst(func, v)?;
        if e >= 64 {
            return None;
        }
        if c >= 0 && (c as u64) < (1u64 << e) {
            Some((v, false))
        } else if c >= -(1i64 << (e - 1)) && c < (1i64 << (e - 1)) {
            Some((v, true))
        } else {
            None
        }
    };
    match func.dfg.insts[di] {
        InstructionData::Binary {
            opcode: Opcode::Imul,
            args,
        } => {
            let Some((a, sa)) = opnd(func, args[0], &mut ext_insts) else {
                return false;
            };
            let Some((b, sb)) = opnd(func, args[1], &mut ext_insts) else {
                return false;
            };
            // The `usdot` lowering pattern only matches unsigned×signed —
            // normalize signed×unsigned into it (imul is commutative).
            let (a, sa, b, sb) = if sa && !sb { (b, sb, a, sa) } else { (a, sa, b, sb) };
            if (!can_vec.contains(&a) && !is_splat(func, info, kinds, splat_cache, a))
                || (!can_vec.contains(&b) && !is_splat(func, info, kinds, splat_cache, b))
            {
                return false;
            }
            // Product lanes need 2*elem bits — cap at i64.
            if 2 * elem.bits() > 64 {
                return false;
            }
            let mut ok = vec![r.upd];
            ok.extend(r.aux);
            ok.extend(chain.iter().copied());
            ok.extend(ext_insts.iter().copied());
            ok.extend(extra_ok.iter().copied());
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
            ok.extend(r.aux);
            ok.extend(chain.iter().copied());
            ok.extend(ext_insts.iter().copied());
            ok.extend(extra_ok.iter().copied());
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

/// Proven unsigned bit-width of a scalar value: `Some(w)` means the value is
/// always in `0..2^w`. Falls back to the value's own type width; `None` only
/// when even that can't be determined. Used to decide whether a wide scalar
/// compare may be evaluated on truncated element lanes.
fn ubits(func: &Function, v: Value) -> Option<u32> {
    let v = func.dfg.resolve_aliases(v);
    let own = || func.dfg.value_type(v).bits();
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return Some(own());
    };
    Some(match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => {
            let c = imm.bits() as u64;
            if c == 0 { 1 } else { 64 - c.leading_zeros() }
        }
        InstructionData::Load { opcode, .. } => match opcode {
            Opcode::Uload8 => 8,
            Opcode::Uload16 => 16,
            Opcode::Uload32 => 32,
            _ => own(),
        },
        InstructionData::Unary {
            opcode: Opcode::Uextend,
            arg,
        } => func.dfg.value_type(func.dfg.resolve_aliases(arg)).bits(),
        InstructionData::Unary {
            opcode: Opcode::Ireduce,
            ..
        } => own(),
        InstructionData::Binary {
            opcode: Opcode::Band,
            args,
        } => ubits(func, args[0])?.min(ubits(func, args[1])?),
        InstructionData::Binary {
            opcode: Opcode::Bor | Opcode::Bxor,
            args,
        } => ubits(func, args[0])?.max(ubits(func, args[1])?),
        InstructionData::Binary {
            opcode: Opcode::Ushr,
            args,
        } => ubits(func, args[0])?.saturating_sub(iconst(func, args[1])?.max(0) as u32),
        InstructionData::IntCompare { .. } | InstructionData::FloatCompare { .. } => 1,
        _ => own(),
    })
}

/// The value provably fits a signed `ebits`-bit lane: its `sext` from
/// `ebits` reproduces the scalar value, so signed lane compares are exact.
fn sfits(func: &Function, v: Value, ebits: u32) -> bool {
    let v = func.dfg.resolve_aliases(v);
    if let Some(w) = ubits(func, v)
        && w < ebits
    {
        return true;
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return func.dfg.value_type(v).bits() <= ebits;
    };
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => {
            let c = imm.bits();
            ebits < 64 && c >= -(1i64 << (ebits - 1)) && c < (1i64 << (ebits - 1))
        }
        InstructionData::Load { opcode, .. } => match opcode {
            Opcode::Sload8 => ebits >= 8,
            Opcode::Sload16 => ebits >= 16,
            Opcode::Sload32 => ebits >= 32,
            _ => false,
        },
        InstructionData::Unary {
            opcode: Opcode::Sextend,
            arg,
        } => func.dfg.value_type(func.dfg.resolve_aliases(arg)).bits() <= ebits,
        _ => func.dfg.value_type(v).bits() <= ebits,
    }
}

/// Validate a conditional-update predicate tree. `mask_tree` walks values
/// that must evaluate to all-ones/all-zeros lane masks (icmp/fcmp results
/// combined bitwise); `data_tree` walks values evaluated on truncated
/// element lanes — exact for ops that commute with truncation, and each
/// compare operand gets a width check matching the condition code's
/// signedness. `acc` may appear nowhere in the tree: vector accumulator
/// lanes are partial sums, not the scalar value. In-body insts the emit will
/// need are collected in `insts`.
fn cond_data(
    func: &Function,
    info: &Info,
    kinds: &[Param],
    can_vec: &FxHashSet<Value>,
    splat_cache: &mut FxHashMap<Value, bool>,
    elem: Type,
    acc: Value,
    v: Value,
    insts: &mut FxHashSet<Inst>,
    depth: usize,
) -> bool {
    let v = func.dfg.resolve_aliases(v);
    if v == acc || depth > 8 || insts.len() > 8 {
        return false;
    }
    if can_vec.contains(&v) {
        return true;
    }
    if is_splat(func, info, kinds, splat_cache, v) {
        // Emit remats `iconst`s at lane width and truncates wider values;
        // a narrower invariant would need a signedness-aware extend — reject.
        let w = func.dfg.value_type(v).bits();
        return w >= elem.bits() || is_const(func, v);
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return false;
    };
    if !info.body.contains(&func.layout.inst_block(i).unwrap_or(info.h)) {
        return false;
    }
    let d = |func: &Function,
             info: &Info,
             kinds: &[Param],
             can_vec: &FxHashSet<Value>,
             splat_cache: &mut FxHashMap<Value, bool>,
             v: Value,
             insts: &mut FxHashSet<Inst>| {
        cond_data(func, info, kinds, can_vec, splat_cache, elem, acc, v, insts, depth + 1)
    };
    let ok = match func.dfg.insts[i] {
        InstructionData::Binary { opcode, args }
            if matches!(
                opcode,
                Opcode::Iadd
                    | Opcode::Isub
                    | Opcode::Band
                    | Opcode::Bor
                    | Opcode::Bxor
                    | Opcode::Ishl
                    | Opcode::Ushr
            ) || (opcode == Opcode::Imul && elem.bits() <= 32) =>
        {
            let shift_ok = !matches!(opcode, Opcode::Ishl | Opcode::Ushr)
                || is_splat(func, info, kinds, splat_cache, args[1]);
            shift_ok
                && d(func, info, kinds, can_vec, splat_cache, args[0], insts)
                && d(func, info, kinds, can_vec, splat_cache, args[1], insts)
        }
        // Widening to `w >= elem` bits then truncating to lanes is just the
        // truncation; extending a narrower value can't be expressed at elem
        // lane count.
        InstructionData::Unary {
            opcode: Opcode::Uextend | Opcode::Sextend,
            arg,
        } => {
            func.dfg.value_type(func.dfg.resolve_aliases(arg)).bits() >= elem.bits()
                && d(func, info, kinds, can_vec, splat_cache, arg, insts)
        }
        InstructionData::Unary {
            opcode: Opcode::Ireduce,
            arg,
        } => {
            func.dfg.value_type(v).bits() >= elem.bits()
                && d(func, info, kinds, can_vec, splat_cache, arg, insts)
        }
        InstructionData::Unary {
            opcode: Opcode::Ineg | Opcode::Bnot,
            arg,
        } => d(func, info, kinds, can_vec, splat_cache, arg, insts),
        _ => false,
    };
    if ok {
        insts.insert(i);
    }
    ok
}

/// A mask-tree node: evaluates to an all-ones/all-zeros lane mask — a
/// compare whose operands survive lane truncation, or a bitwise combination
/// of such nodes (a data operand inside `band`/`bor`/`bxor` would leak 0/1
/// values into what must stay a full mask).
fn mask_node(
    func: &Function,
    info: &Info,
    kinds: &[Param],
    can_vec: &FxHashSet<Value>,
    splat_cache: &mut FxHashMap<Value, bool>,
    elem: Type,
    acc: Value,
    v: Value,
    insts: &mut FxHashSet<Inst>,
    depth: usize,
) -> bool {
    let v = func.dfg.resolve_aliases(v);
    if v == acc || depth > 8 || insts.len() > 8 {
        return false;
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return false;
    };
    if !info.body.contains(&func.layout.inst_block(i).unwrap_or(info.h)) {
        return false;
    }
    let ebits = elem.bits();
    let ok = match func.dfg.insts[i] {
        InstructionData::IntCompare { cond, args, .. } => {
            let ty = func.dfg.value_type(func.dfg.resolve_aliases(args[0]));
            let tb = ty.bits();
            cond_data(func, info, kinds, can_vec, splat_cache, elem, acc, args[0], insts, depth + 1)
                && cond_data(
                    func,
                    info,
                    kinds,
                    can_vec,
                    splat_cache,
                    elem,
                    acc,
                    args[1],
                    insts,
                    depth + 1,
                )
                && match cond {
                    IntCC::Equal | IntCC::NotEqual => true,
                    // Unsigned order survives truncation exactly when both
                    // operands fit the lane width; narrower compares
                    // zero-extend into the lane identically.
                    IntCC::UnsignedLessThan
                    | IntCC::UnsignedLessThanOrEqual
                    | IntCC::UnsignedGreaterThan
                    | IntCC::UnsignedGreaterThanOrEqual => {
                        tb <= ebits
                            || args
                                .iter()
                                .all(|&a| ubits(func, a).is_some_and(|w| w <= ebits))
                    }
                    // Signed compares need sign-preserving lanes: exact when
                    // the compare is at lane width; a wider scalar compare
                    // needs each operand provably in signed lane range; a
                    // narrower one would lose the sign under zext.
                    _ => {
                        tb == ebits
                            || (tb > ebits && args.iter().all(|&a| sfits(func, a, ebits)))
                    }
                }
        }
        InstructionData::FloatCompare { args, .. } => {
            func.dfg.value_type(func.dfg.resolve_aliases(args[0])) == elem
                && cond_data(
                    func,
                    info,
                    kinds,
                    can_vec,
                    splat_cache,
                    elem,
                    acc,
                    args[0],
                    insts,
                    depth + 1,
                )
                && cond_data(
                    func,
                    info,
                    kinds,
                    can_vec,
                    splat_cache,
                    elem,
                    acc,
                    args[1],
                    insts,
                    depth + 1,
                )
        }
        InstructionData::Binary {
            opcode: Opcode::Band | Opcode::Bor | Opcode::Bxor,
            args,
        } => {
            mask_node(func, info, kinds, can_vec, splat_cache, elem, acc, args[0], insts, depth + 1)
                && mask_node(
                    func,
                    info,
                    kinds,
                    can_vec,
                    splat_cache,
                    elem,
                    acc,
                    args[1],
                    insts,
                    depth + 1,
                )
        }
        InstructionData::Unary {
            opcode: Opcode::Bnot,
            arg,
        } => mask_node(func, info, kinds, can_vec, splat_cache, elem, acc, arg, insts, depth + 1),
        _ => false,
    };
    if ok {
        insts.insert(i);
    }
    ok
}

/// Validate a conditional-update predicate `c`: either a mask tree, or a
/// plain flag value the emit wraps in `icmp ne v, 0` — safe only when `c`'s
/// nonzero-ness survives lane truncation (`c < 2^ebits`). `acc` may appear
/// nowhere in the tree: vector accumulator lanes are partial sums, not the
/// scalar value. In-body insts the emit will need land in `cd.insts`.
fn cond_mask(
    func: &Function,
    info: &Info,
    kinds: &[Param],
    can_vec: &FxHashSet<Value>,
    splat_cache: &mut FxHashMap<Value, bool>,
    elem: Type,
    acc: Value,
    cd: &mut Cond,
) -> bool {
    if mask_node(func, info, kinds, can_vec, splat_cache, elem, acc, cd.c, &mut cd.insts, 0) {
        return true;
    }
    if !ubits(func, cd.c).is_some_and(|w| w <= elem.bits()) {
        return false;
    }
    cd.needs_ne = true;
    cond_data(func, info, kinds, can_vec, splat_cache, elem, acc, cd.c, &mut cd.insts, 0)
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
    // nm = iters & -(VF*UNROLL) ; end_v = iv0 + nm*step. A post-tested
    // epilogue can't run 0 iters (the body precedes its test), so round
    // `iters-1` down instead and leave ≥1 scalar iteration (preds give
    // `iters ≥ 1` on this path).
    let iters = emit(&mut pos, iv_ty, &p.iters);
    let mk = pos.ins().iconst(iv_ty, -p.vf * UNROLL as i64);
    let nmv = if p.post_tested {
        let im1 = pos.ins().iadd_imm_s(iters, -1);
        pos.ins().band(im1, mk)
    } else {
        pos.ins().band(iters, mk)
    };
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
            VPred::Pair(a, a_neg, b, b_neg) => {
                let lenb = emit(
                    &mut pos,
                    pty,
                    &Ins::Mul(
                        Box::new(p.iters.clone()),
                        Box::new(Ins::K(i64::from(p.elem.bytes()))),
                    ),
                );
                // A descending stream touches `[a(iv_last), a(iv0)+e)`:
                // evaluate its base at the last iteration's iv instead.
                let last = Ins::Add(
                    Box::new(Ins::Val(p.iv0)),
                    Box::new(Ins::Mul(
                        Box::new(Ins::Sub(
                            Box::new(p.iters.clone()),
                            Box::new(Ins::K(1)),
                        )),
                        Box::new(Ins::K(p.step)),
                    )),
                );
                let ivl = emit(&mut pos, iv_ty, &last);
                let lo = |e: &Ins, neg: bool, pos: &mut FuncCursor| {
                    emit(pos, pty, &subst(e, p.iv, if neg { ivl } else { p.iv0 }))
                };
                let (alo, blo) = (lo(a, *a_neg, &mut pos), lo(b, *b_neg, &mut pos));
                let hi = |e: &Ins, neg: bool, lo: Value, pos: &mut FuncCursor| {
                    if neg {
                        // Top of the range is one element past `e(iv0)`.
                        let t = emit(pos, pty, &subst(e, p.iv, p.iv0));
                        let eb = pos.ins().iconst(pty, i64::from(p.elem.bytes()));
                        pos.ins().iadd(t, eb)
                    } else {
                        pos.ins().iadd(lo, lenb)
                    }
                };
                let ahi = hi(a, *a_neg, alo, &mut pos);
                let bhi = hi(b, *b_neg, blo, &mut pos);
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
    //
    // Early-exit loops put each group in its own block — a lane check can
    // branch out mid-body — with an extra tail block for the backedge and
    // a `resumes[g]` block per group that re-enters the scalar loop at the
    // group's first lane.
    let mut vbs = vec![vb];
    let mut resumes = Vec::new();
    if !p.early.is_empty() {
        for _ in 0..UNROLL {
            let b = pos.func.dfg.make_block();
            pos.func.layout.insert_block_after(b, *vbs.last().unwrap());
            let r = pos.func.dfg.make_block();
            pos.func.layout.insert_block_after(r, b);
            vbs.push(b);
            resumes.push(r);
        }
    }
    let mut splats: FxHashMap<Value, Value> = FxHashMap::default();
    let mut smemo: FxHashMap<Value, Value> = FxHashMap::default();
    let mut back_accs: Vec<Vec<Value>> = p.reducs.iter().map(|_| Vec::new()).collect();
    let gb = i64::from(p.elem.bytes()) * p.vf;
    for g in 0..UNROLL {
        let mut pos = FuncCursor::new(pos.func).at_bottom(vbs[g.min(vbs.len() - 1)]);
        let mut addrs = Vec::new();
        for s in &p.streams {
            let a = if s.direct {
                if s.neg {
                    // Descending pointer iv: the contiguous block ends at
                    // `ivv - g*vf*e`, so load at its lowest address.
                    let off = pos
                        .ins()
                        .iconst(iv_ty, -(((g as i64) + 1) * p.vf - 1) * i64::from(p.elem.bytes()));
                    pos.ins().iadd(ivv, off)
                } else if g == 0 {
                    ivv
                } else {
                    let off = pos.ins().iconst(pty, gb * g as i64);
                    pos.ins().iadd(ivv, off)
                }
            } else {
                // Substitute the iv whose address is the block's lowest
                // byte: the group's first lane ascending, its last
                // descending.
                let adj = if s.neg {
                    (((g as i64) + 1) * p.vf - 1) * p.step
                } else {
                    (g as i64) * p.vf * p.step
                };
                let w = if adj == 0 {
                    ivv
                } else {
                    let c = pos.ins().iconst(iv_ty, adj);
                    pos.ins().iadd(ivv, c)
                };
                emit(&mut pos, pty, &subst(&s.base, p.iv, w))
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
                        let mut vl =
                            pos.ins()
                                .load(p.vt, MemFlagsData::new().with_notrap(), addrs[j], 0);
                        if p.streams[j].neg {
                            vl = vreverse(&mut pos, p.vt, vl);
                        }
                        vmap.insert(r, vl);
                    }
                }
                InstructionData::Store { args, .. } => {
                    let mut vv = emit_val(
                        &mut pos, p, &mut vmap, &mut splats, &mut smemo, &addrs, args[0], 0,
                    );
                    if p.streams[j].neg {
                        vv = vreverse(&mut pos, p.vt, vv);
                    }
                    pos.ins()
                        .store(MemFlagsData::new().with_notrap(), vv, addrs[j], 0);
                }
                _ => unreachable!(),
            }
        }
        // Lane-wise early-exit checks: on a hit, resume the scalar loop at
        // this group's first lane — it re-finds the exact lane and takes
        // the real exit edge with its original args.
        for &(cv, exits_true) in &p.early {
            let m = emit_val(&mut pos, p, &mut vmap, &mut splats, &mut smemo, &addrs, cv, 0);
            if exits_true {
                let hit = pos.ins().vany_true(m);
                pos.ins().brif(hit, resumes[g], &[], vbs[g + 1], &[]);
            } else {
                let all = pos.ins().vall_true(m);
                pos.ins().brif(all, vbs[g + 1], &[], resumes[g], &[]);
            }
        }
        // vacc = vacc ⊕ delta per group; deltas come after the group's
        // memory ops so their loads are already in `vmap`.
        for (k, r) in p.reducs.iter().enumerate() {
            // A conditional update masks its delta: `d & mask`, applied at
            // element lanes — before any widening (`widen(d & m)` sums the
            // kept lanes; `(a & m) * b` zeroes the dropped products).
            let mask = |pos: &mut FuncCursor,
                        vmap: &mut FxHashMap<Value, Value>,
                        splats: &mut FxHashMap<Value, Value>,
                        smemo: &mut FxHashMap<Value, Value>,
                        addrs: &[Value],
                        v: Value|
             -> Value {
                let Some(cd) = &r.cond else { return v };
                let m = emit_mask(pos, p, vmap, splats, smemo, addrs, cd);
                pos.ins().band(v, m)
            };
            let vd = match &r.widen {
                None | Some(Widen::Add { .. }) => {
                    let mut v = emit_val(
                        &mut pos, p, &mut vmap, &mut splats, &mut smemo, &addrs, r.delta, 0,
                    );
                    v = mask(&mut pos, &mut vmap, &mut splats, &mut smemo, &addrs, v);
                    match &r.widen {
                        Some(Widen::Add { signed }) => widen_vec(&mut pos, *signed, v, r.aty),
                        _ => v,
                    }
                }
                Some(Widen::Mul { sa, sb, a, b }) => {
                    let mut av = emit_val(
                        &mut pos, p, &mut vmap, &mut splats, &mut smemo, &addrs, *a, 0,
                    );
                    av = mask(&mut pos, &mut vmap, &mut splats, &mut smemo, &addrs, av);
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
    let mut pos = FuncCursor::new(pos.func).at_bottom(*vbs.last().unwrap());
    let k = pos.ins().iconst(iv_ty, p.step * p.vf * UNROLL as i64);
    let iv2 = pos.ins().iadd(ivv, k);
    let mut back: Vec<Value> = vec![iv2, endv, nm];
    for accs in &back_accs {
        back.extend(accs);
    }
    let bargs: Vec<BlockArg> = back.iter().map(|&v| BlockArg::Value(v)).collect();
    pos.ins().jump(vh, &bargs);
    // Resume blocks: re-enter the scalar loop at group g's first lane,
    // `ivv + g*vf*step`. `Step` params advance s per elapsed iteration
    // (step==1 was required, so elapsed = wg - iv0).
    for (g, &r) in resumes.iter().enumerate() {
        let mut pos = FuncCursor::new(pos.func).at_bottom(r);
        let wg = if g == 0 {
            ivv
        } else {
            let c = pos.ins().iconst(iv_ty, g as i64 * p.vf * p.step);
            pos.ins().iadd(ivv, c)
        };
        let mut args: Vec<Value> = Vec::new();
        for (j, _) in params.iter().enumerate() {
            if j == p.iv_idx {
                args.push(wg);
                continue;
            }
            match p.kinds[j] {
                Param::Inv => args.push(p.entry_args[j]),
                Param::Step(s) => {
                    let d = pos.ins().isub(wg, p.iv0);
                    let sk = pos.ins().iconst(iv_ty, s);
                    let m = pos.ins().imul(d, sk);
                    args.push(pos.ins().iadd(p.entry_args[j], m));
                }
                Param::Other => unreachable!(),
            }
        }
        let bargs: Vec<BlockArg> = args.iter().map(|&v| BlockArg::Value(v)).collect();
        pos.ins().jump(p.h, &bargs);
    }
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
        let Some(p) = plan(func, &cfg, &dt, &la, lp, noalias, fname) else {
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
