//! `switch` → `x*s + b + corr[x]` — the corsix.org lookup-table-correction
//! scheme at pliron-IR level: an exact function built from a fast, *wrong*
//! linear approximation plus a small correction table narrower than the
//! values it produces.
//!
//! `switchmap` at the CLIF level already folds the exactly-linear case
//! (`br_table` → `idx*s + b`). This pass handles the *almost*-linear case: a
//! `switch` whose arms all funnel values to one join (or `ret`), where some
//! argument position is well-approximated by `idx*s + b` with residuals that
//! fit a `u8` (or `u16`) correction table:
//!
//! ```text
//!   switch x -> join(v_i)         idx = x - min
//!   becomes:                      out = idx*s + b + zext(corr[idx])
//! ```
//!
//! `s` is chosen from the secant and integer least-squares slopes (each ±1);
//! the winner minimizes the residual span. `corr` lands in a private
//! immutable `[n x i8]`/`[n x i16]` global — one `gep+load+zext+add` per
//! result instead of a jump through a `br_table`.
//!
//! Default destination handling:
//! * `unreachable` (holes/OOB are UB): no bounds check.
//! * trivial (const materialization + `br`/`ret`): one `icmp ule` range
//!   check plus a `select` per result; hole entries get the residual needed
//!   to reproduce the default value (must fit the table width).
//! * real code: requires a dense domain and becomes
//!   `cond_br inrange -> hit_bb else -> default`.
//!
//! Off by default: `PLIRON_LUTCORR=1`. `PLIRON_LUTCORR=0` always disables.

use cranelift_module::Linkage;
use pliron::basic_block::BasicBlock;
use pliron::builtin::op_interfaces::SingleBlockRegionInterface;
use pliron::builtin::ops::ModuleOp;
use pliron::builtin::types::{IntegerType, Signedness};
use pliron::context::{Context, Ptr};
use pliron::identifier::Identifier;
use pliron::op::Op;
use pliron::operation::Operation;
use pliron::r#type::{TypeHandle, Typed};
use pliron::value::Value;
use pliron_cfg::env_is;
use pliron_llvm::attributes::{ICmpPredicateAttr, IntegerOverflowFlagsAttr};
use pliron_llvm::op_interfaces::{CastOpInterface, IntBinArithOpWithOverflowFlag};
use pliron_llvm::ops::{
    AddOp, BrOp, CondBrOp, ConstantOp, GetElementPtrOp, GepIndex, GlobalOp, ICmpOp, LoadOp, MulOp,
    PoisonOp, ReturnOp, SelectOp, StoreOp, SubOp, SwitchOp, TruncOp, UndefOp, UnreachableOp, ZExtOp,
    ZeroOp,
};
use pliron_llvm::types::{ArrayType, PointerType};
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;

use crate::helpers::{bodied, erase_unreachable};
use crate::inline::ops;
use crate::state::{ConstVal, GlobalInfo, State};
use crate::types::{classify, TyK};

/// Largest domain (`max - min + 1`) we materialize a correction table for.
const MAX_N: u64 = 4096;

/// What a switch edge (case or default) computes.
enum Edge {
    /// `br join(vals)`, possibly preceded by `store *ptr <- const` ops (the
    /// shape rustc emits for a `match` result assigned through an alloca).
    Join { j: Ptr<BasicBlock>, vals: Vec<Value>, stores: Vec<(Value, Value)> },
    /// `ret vals` in a trivial block.
    Ret(Vec<Value>),
    /// Reaches `unreachable` — the value is don't-care.
    DontCare,
    /// Real code — usable only as the default fallback.
    Other,
}

/// Is `op` a side-effect-free constant materializer, or a `store` we may
/// re-order into a fold (its dst/val get checked separately)?
fn trivial_op(op: Ptr<Operation>, ctx: &Context) -> bool {
    Operation::is_op::<ConstantOp>(op, ctx)
        || Operation::is_op::<UndefOp>(op, ctx)
        || Operation::is_op::<PoisonOp>(op, ctx)
        || Operation::is_op::<ZeroOp>(op, ctx)
        || Operation::is_op::<StoreOp>(op, ctx)
}

fn edge_of(ctx: &Context, dest: Ptr<BasicBlock>, opds: &[Value]) -> Edge {
    if !opds.is_empty() {
        return Edge::Join { j: dest, vals: opds.to_vec(), stores: vec![] };
    }
    let bos = ops(ctx, dest);
    let Some(&term) = bos.last() else {
        return Edge::Other;
    };
    if !bos[..bos.len() - 1].iter().all(|&o| trivial_op(o, ctx)) {
        return Edge::Other;
    }
    // `llvm.store *ptr <- val`: operands are [value, ptr].
    let mut stores = vec![];
    for &o in &bos[..bos.len() - 1] {
        if Operation::is_op::<StoreOp>(o, ctx) {
            let v = o.deref(ctx).get_operand(ctx, 0);
            let p = o.deref(ctx).get_operand(ctx, 1);
            stores.push((p, v));
        }
    }
    if Operation::is_op::<BrOp>(term, ctx) {
        let d = term.deref(ctx).get_successor(ctx, 0);
        let vals = term.deref(ctx).operands(ctx).collect();
        return Edge::Join { j: d, vals, stores };
    }
    if Operation::is_op::<ReturnOp>(term, ctx) {
        if !stores.is_empty() {
            return Edge::Other;
        }
        return Edge::Ret(term.deref(ctx).operands(ctx).collect());
    }
    if Operation::is_op::<UnreachableOp>(term, ctx) {
        if !stores.is_empty() {
            return Edge::Other;
        }
        return Edge::DontCare;
    }
    Edge::Other
}

/// Constant value of `v`: `st.consts`, `UndefOp`/`PoisonOp`/`ZeroOp` defs,
/// or an `llvm.constant`'s integer attribute.
fn const_of(ctx: &Context, st: &State, v: Value) -> Option<ConstVal> {
    if let Some(cv) = st.consts.get(v) {
        return Some(cv.clone());
    }
    let op = v.defining_op()?;
    if Operation::is_op::<UndefOp>(op, ctx) || Operation::is_op::<PoisonOp>(op, ctx) {
        return Some(ConstVal::Undef);
    }
    if Operation::is_op::<ZeroOp>(op, ctx) {
        return Some(ConstVal::Zero);
    }
    if let Some(c) = Operation::get_op::<ConstantOp>(op, ctx) {
        let a = c.get_value(ctx).as_any();
        if let Some(i) = a.downcast_ref::<pliron::builtin::attributes::IntegerAttr>() {
            return Some(ConstVal::Bits(i.value().to_u128()));
        }
    }
    None
}

/// May `v` be re-emitted at the switch? Consts are re-materialized; other
/// values must be defined in the switch's block or be one of its args.
fn available_at(ctx: &Context, st: &State, swb: Ptr<BasicBlock>, v: Value) -> bool {
    if const_of(ctx, st, v).is_some() {
        return true;
    }
    match v.defining_op() {
        None => swb.deref(ctx).arguments(ctx).any(|a| a == v),
        Some(d) => d.deref(ctx).get_parent_block() == Some(swb),
    }
}

/// One join/return argument position.
enum Pos {
    /// Same `Value` (or same const) on every edge.
    Same,
    /// `res_i = idx*s + b` exactly — int result.
    IntLin { s: i64, b: i64 },
    /// `res_i = idx*s + b + tab_i` — int result, `u8`/`u16` table.
    IntTab { s: i64, b: i64, wide: bool, bytes: Vec<u8> },
    /// Same two shapes over `ConstVal::Sym` offsets — pointer result.
    SymLin { sym: String, s: i64, b: i64 },
    SymTab { sym: String, s: i64, b: i64, wide: bool, bytes: Vec<u8> },
    /// No linear structure at all: a raw `n x ty` table of the case
    /// constants (LLVM switch-to-lookup-table for e.g. distinct syms).
    RawTab { ty: TypeHandle, cvs: Vec<ConstVal> },
}

/// The const we emit for a position — used by `Same` and the select path.
enum Out {
    Value(Value),
    Const(ConstVal),
}

/// Fit `v_i ≈ s*idx_i + b`. Returns `(s, b, resids)` — `resids[idx] =
/// v_idx - s*idx` — or `None` if nothing works (never: always finds *a* fit,
/// the caller rejects on residual size).
fn fit(vals: &BTreeMap<u64, i128>) -> Option<(i64, i64, BTreeMap<u64, i128>)> {
    let (&x0, &y0) = vals.iter().next()?;
    let (&x1, &y1) = vals.iter().next_back()?;
    let mut cands: Vec<i128> = vec![0];
    if x1 != x0 {
        let sec = (y1 - y0) / (x1 - x0) as i128;
        cands.extend([sec - 1, sec, sec + 1]);
        let n = vals.len() as i128;
        let (mut sx, mut sy, mut sxy, mut sxx) = (0i128, 0i128, 0i128, 0i128);
        for (&x, &y) in vals {
            sx += x as i128;
            sy += y;
            sxy += x as i128 * y;
            sxx += x as i128 * x as i128;
        }
        let den = n * sxx - sx * sx;
        if den != 0 {
            let num = n * sxy - sx * sy;
            let lsq = if num >= 0 { (num + den / 2) / den } else { (num - den / 2) / den };
            cands.extend([lsq - 1, lsq, lsq + 1]);
        }
    }
    let mut best: Option<(i128, i128, i128, BTreeMap<u64, i128>)> = None;
    for s in cands {
        let (mut lo, mut hi) = (i128::MAX, i128::MIN);
        let mut res = BTreeMap::new();
        for (&x, &y) in vals {
            let r = y - s * x as i128;
            res.insert(x, r);
            lo = lo.min(r);
            hi = hi.max(r);
        }
        let span = hi - lo;
        if best.as_ref().is_none_or(|&(bs, _, _, _)| span < bs) {
            best = Some((span, s, lo, res));
        }
    }
    let (_, s, b, res) = best?;
    Some((i64::try_from(s).ok()?, i64::try_from(b).ok()?, res))
}

/// Result of analyzing one `switch`.
struct Plan {
    cond: Value,
    width: u32,
    min: u64,
    n: u64,
    join: Option<Ptr<BasicBlock>>,
    ret: bool,
    /// `positions[i]` computes the value for `sinks[i]`.
    positions: Vec<(Pos, Out)>,
    sinks: Vec<Sink>,
    /// Default values for the select path, one per position (re-materialized
    /// as `ConstVal`s at the switch).
    default_cvs: Option<Vec<ConstVal>>,
    /// `cond_br` fallback when the default is real code.
    default_else: Option<(Ptr<BasicBlock>, Vec<Value>)>,
    /// Full-domain table (LLVM LUT style): `2^width` entries, no range
    /// check or select. Only when the default is a const set.
    full: bool,
    /// False when the default is `unreachable` → no range check.
    needs_check: bool,
    /// Result type of each position (for truncation).
    arg_tys: Vec<TypeHandle>,
}

/// Where a computed position's value goes.
enum Sink {
    /// Join argument.
    JoinArg,
    /// `ret` value.
    Ret,
    /// `store *ptr <- res` (rustc's match-through-alloca shape).
    Store(Value),
}

fn analyze(ctx: &Context, st: &State, swb: Ptr<BasicBlock>, swop: Ptr<Operation>) -> Option<Plan> {
    let sw = Operation::get_op::<SwitchOp>(swop, ctx)?;
    let cond = swop.deref(ctx).get_operand(ctx, 0);
    let TyK::Int(width) = classify(ctx, cond.get_type(ctx)) else {
        return None;
    };
    if width > 64 {
        return None;
    }
    let cases = sw.cases(ctx);
    if cases.len() < 2 {
        return None;
    }
    let ddest = sw.default_dest(ctx);
    let dopds = sw.default_dest_operands(ctx);
    let dflt = edge_of(ctx, ddest, &dopds);

    let mut join: Option<Ptr<BasicBlock>> = None;
    let mut ret = false;
    let mut nargs = 0usize;
    // Ordered alloca slots (rustc's `match` arms each `store *a_k <- c_i`),
    // plus each edge's per-slot stored value.
    let mut slots: Vec<Value> = vec![];
    let mut edges: Vec<(u64, Option<(Vec<Value>, FxHashMap<Value, Value>)>)> =
        Vec::with_capacity(cases.len());

    let add_edge = |j: Ptr<BasicBlock>,
                        vals: Vec<Value>,
                        stores: Vec<(Value, Value)>,
                        join: &mut Option<Ptr<BasicBlock>>,
                        nargs: &mut usize,
                        slots: &mut Vec<Value>|
     -> Option<(Vec<Value>, FxHashMap<Value, Value>)> {
        match *join {
            Some(pj) if pj != j => return None,
            None => {
                *nargs = j.deref(ctx).arguments(ctx).count();
                *join = Some(j);
            }
            _ => {}
        }
        if vals.len() != *nargs {
            return None;
        }
        // Per-edge store map; a duplicated slot keeps the last store.
        let mut smap: FxHashMap<Value, Value> = FxHashMap::default();
        for (p, v) in &stores {
            smap.insert(*p, *v);
        }
        if slots.is_empty() {
            *slots = stores.iter().map(|(p, _)| *p).collect();
        }
        if smap.len() != slots.len()
            || !slots.iter().all(|p| smap.contains_key(p))
        {
            return None;
        }
        Some((vals, smap))
    };

    for c in &cases {
        let v = c.value.value().to_u64();
        match edge_of(ctx, c.dest, &c.dest_opds) {
            Edge::Join { j, vals, stores } => {
                if ret {
                    return None;
                }
                let e = add_edge(j, vals, stores, &mut join, &mut nargs, &mut slots)?;
                edges.push((v, Some(e)));
            }
            Edge::Ret(vals) => {
                if join.is_some() {
                    return None;
                }
                ret = true;
                nargs = vals.len();
                edges.push((v, Some((vals, FxHashMap::default()))));
            }
            Edge::DontCare => edges.push((v, None)),
            Edge::Other => return None,
        }
    }

    let default_cvs_possible = !matches!(dflt, Edge::DontCare | Edge::Other);
    let (default_vals, default_else, needs_check) = match dflt {
        Edge::Join { j, vals, stores } => {
            if ret {
                return None;
            }
            let e = add_edge(j, vals, stores, &mut join, &mut nargs, &mut slots)?;
            (Some(e), None, true)
        }
        Edge::Ret(vals) => {
            if join.is_some() || (ret && vals.len() != nargs) {
                return None;
            }
            ret = true;
            nargs = vals.len();
            (Some((vals, FxHashMap::default())), None, true)
        }
        Edge::DontCare => (None, None, false),
        Edge::Other => (None, Some((ddest, dopds)), true),
    };
    if join.is_none() && !ret {
        return None;
    }
    let arity = nargs + slots.len();
    if arity == 0 || arity > 8 {
        return None;
    }

    // Domain: distinct case values.
    let mut vs: Vec<u64> = edges.iter().map(|e| e.0).collect();
    vs.sort_unstable();
    vs.dedup();
    let (&min, &max) = (vs.first()?, vs.last()?);
    let n = max - min + 1;
    if n < 2 || n > MAX_N {
        return None;
    }
    let dense = vs.len() as u64 == n;
    if default_else.is_some() && !dense {
        return None; // hole inputs must reach the default's own dest
    }
    // LLVM-style full-domain LUT: with a const default and a narrow index
    // type, size the tables to all `2^width` inputs — the range check and
    // selects disappear entirely.
    let full = default_cvs_possible && default_else.is_none() && width <= 8 && arity <= 4;
    let (min, n) = if full { (0, 1u64 << width) } else { (min, n) };
    // Full-domain tables have holes by construction — the index space is
    // all of `2^width` and the default fills them.
    let dense = dense && !full;
    let dense = dense || false;

    let mut sinks: Vec<Sink> = Vec::with_capacity(arity);
    for _ in 0..nargs {
        sinks.push(if ret { Sink::Ret } else { Sink::JoinArg });
    }
    for p in &slots {
        sinks.push(Sink::Store(*p));
    }

    // Result type of each position: join arg types, then the stored types.
    let mut arg_tys: Vec<TypeHandle> = Vec::with_capacity(arity);
    if let Some(j) = join {
        arg_tys.extend(j.deref(ctx).arguments(ctx).map(|a| a.get_type(ctx)));
    } else {
        arg_tys.extend(
            edges
                .iter()
                .find_map(|e| e.1.as_ref())
                .or(default_vals.as_ref())
                .map(|ev| ev.0.iter().map(|v| v.get_type(ctx)))
                .into_iter()
                .flatten(),
        );
    }
    if arg_tys.len() != nargs {
        return None;
    }
    for (k, p) in slots.iter().enumerate() {
        // The stored type comes from any edge's stored value for this slot.
        let ty = edges
            .iter()
            .find_map(|e| e.1.as_ref())
            .or(default_vals.as_ref())?
            .1
            .get(p)?
            .get_type(ctx);
        let _ = k;
        arg_tys.push(ty);
    }
    if arg_tys.len() != arity {
        return None;
    }

    // Per-edge per-position const vals: [edge][position].
    let mut evals: Vec<Vec<Value>> = Vec::with_capacity(edges.len());
    let mut dontcare: Vec<bool> = Vec::with_capacity(edges.len());
    for (_, e) in &edges {
        match e {
            None => {
                evals.push(vec![]);
                dontcare.push(true);
            }
            Some((vals, smap)) => {
                let mut row = vals.clone();
                for p in &slots {
                    row.push(smap[p]);
                }
                evals.push(row);
                dontcare.push(false);
            }
        }
    }
    let drow: Option<Vec<Value>> = default_vals.as_ref().map(|(vals, smap)| {
        let mut row = vals.clone();
        for p in &slots {
            row.push(smap[p]);
        }
        row
    });

    // Select path: every default value must be a const (re-materialized at
    // the switch — default-block values don't dominate it).
    let default_cvs: Option<Vec<ConstVal>> = match &drow {
        Some(dv) => Some(
            dv.iter()
                .map(|&v| const_of(ctx, st, v))
                .collect::<Option<Vec<_>>>()?,
        ),
        None => None,
    };

    let mut positions: Vec<(Pos, Out)> = Vec::with_capacity(arity);
    for j in 0..arity {
        // `Same`: identical Value across case edges AND the default.
        let mut same = true;
        let mut first: Option<Value> = None;
        let mut vals: BTreeMap<u64, i128> = BTreeMap::new();
        let mut raw: BTreeMap<u64, ConstVal> = BTreeMap::new();
        let mut sym: Option<String> = None;
        let mut kind_sym: Option<bool> = None;
        let mut mixed = false;
        for (i, (cv, _)) in edges.iter().enumerate() {
            if dontcare[i] {
                continue;
            }
            let v = evals[i][j];
            match first {
                None => first = Some(v),
                Some(f) if f != v => same = false,
                _ => {}
            }
            let Some(cvv) = const_of(ctx, st, v) else {
                return None; // non-const, non-uniform: nothing to compute
            };
            let idx = *cv - min;
            let bits: i128 = match cvv {
                ConstVal::Bits(b) => {
                    if kind_sym == Some(true) {
                        mixed = true;
                    }
                    kind_sym.get_or_insert(false);
                    raw.insert(idx, ConstVal::Bits(b));
                    b as i128
                }
                ConstVal::Zero => {
                    raw.insert(idx, ConstVal::Zero);
                    0
                }
                ConstVal::Sym { sym: ref s, off } => {
                    if kind_sym == Some(false) {
                        mixed = true;
                    }
                    kind_sym.get_or_insert(true);
                    match &sym {
                        None => sym = Some(s.clone()),
                        Some(ps) if *ps != *s => mixed = true, // distinct syms
                        _ => {}
                    }
                    raw.insert(idx, ConstVal::Sym { sym: s.clone(), off });
                    off as i128
                }
                ConstVal::Undef => continue, // don't-care: excluded from fit
                _ => return None,
            };
            if vals.insert(idx, bits).is_some() {
                return None; // duplicate case value, conflicting payloads
            }
        }
        if vals.is_empty() {
            return None;
        }
        // Same folds in the default's value on the select path.
        if let Some(dv) = &drow {
            if let (Some(f), true) = (first, same) {
                same = dv[j] == f;
            }
        }
        if same {
            if let Some(v) = first {
                if available_at(ctx, st, swb, v) {
                    positions.push((
                        Pos::Same,
                        match const_of(ctx, st, v) {
                            Some(cv) => Out::Const(cv),
                            None => Out::Value(v),
                        },
                    ));
                    continue;
                }
            }
        }
        let is_sym = kind_sym == Some(true) && !mixed;
        // Hole fill for the linear path: the select path needs the
        // default's residual to fit (holes in [min,max] are observable
        // through the select); the unreachable path fills 0 (never
        // observable); dense → no holes. `None` = the default doesn't
        // fit this position's linear model — the RawTab path fills holes
        // with the default const directly instead.
        let hole_fit: Option<i128> = if dense || default_else.is_some() {
            Some(0)
        } else if let Some(dv) = &default_cvs {
            match &dv[j] {
                ConstVal::Bits(b) if !is_sym => Some(*b as i128),
                ConstVal::Zero if !is_sym => Some(0),
                ConstVal::Sym { sym: s, off } if is_sym && sym.as_ref() == Some(s) => {
                    Some(*off as i128)
                }
                ConstVal::Undef => Some(0),
                _ => None,
            }
        } else {
            Some(0)
        };
        // Linear + correction-table fit, skipped on mixed kinds and on an
        // unfittable default residual.
        let pos = if !mixed {
            hole_fit.and_then(|fill| fit(&vals).map(|(s, b, res)| (s, b, res, fill)))
        } else {
            None
        };
        let mut pos = pos.and_then(|(s, b, res, fill)| {
            let (mut maxc, mut corr) = (0i128, BTreeMap::new());
            let mut bad = false;
            for i in 0..n {
                let c = if let Some(&r) = res.get(&i) {
                    r - b as i128
                } else {
                    fill - s as i128 * i as i128 - b as i128
                };
                if c < 0 {
                    bad = true;
                    break;
                }
                maxc = maxc.max(c);
                corr.insert(i, c);
            }
            if bad || maxc > 65535 {
                return None;
            }
            let wide = maxc > 255;
            let mut bytes = Vec::with_capacity(n as usize * (1 + wide as usize));
            for i in 0..n {
                let c = corr[&i];
                if wide {
                    bytes.extend_from_slice(&(c as u16).to_le_bytes());
                } else {
                    bytes.push(c as u8);
                }
            }
            Some(if maxc == 0 {
                if is_sym {
                    Pos::SymLin { sym: sym.clone().unwrap_or_default(), s, b }
                } else {
                    Pos::IntLin { s, b }
                }
            } else if is_sym {
                Pos::SymTab { sym: sym.clone().unwrap_or_default(), s, b, wide, bytes }
            } else {
                Pos::IntTab { s, b, wide, bytes }
            })
        });

        // Fallback: a raw `n x ty` table of the case constants — LLVM's
        // switch-to-lookup-table, covering distinct syms and unbounded
        // residuals alike.
        if pos.is_none() {
            let ty = arg_tys[j];
            let raw_ok = |cv: &ConstVal| match classify(ctx, ty) {
                TyK::Int(_) => {
                    matches!(cv, ConstVal::Bits(_) | ConstVal::Zero | ConstVal::Undef)
                }
                TyK::Ptr => {
                    matches!(cv, ConstVal::Sym { .. } | ConstVal::Zero | ConstVal::Undef)
                }
                _ => false,
            };
            let mut cvs: Vec<ConstVal> = Vec::with_capacity(n as usize);
            for i in 0..n {
                let cv = if let Some(c) = raw.get(&i) {
                    c.clone()
                } else if let Some(dv) = &default_cvs {
                    dv[j].clone()
                } else {
                    ConstVal::Zero
                };
                if !raw_ok(&cv) {
                    return None;
                }
                cvs.push(cv);
            }
            // Smallest integer element covering every entry.
            let ty = if let TyK::Int(_) = classify(ctx, ty) {
                let maxv = cvs
                    .iter()
                    .map(|cv| match cv {
                        ConstVal::Bits(b) => *b,
                        _ => 0,
                    })
                    .max()
                    .unwrap_or(0);
                let w = if maxv <= 0xff {
                    8
                } else if maxv <= 0xffff {
                    16
                } else if maxv <= u64::from(u32::MAX) as u128 {
                    32
                } else {
                    64
                };
                IntegerType::get(ctx, w, Signedness::Signless).into()
            } else {
                ty
            };
            pos = Some(Pos::RawTab { ty, cvs });
        }
        positions.push((pos.unwrap(), Out::Const(ConstVal::Zero)));
    }
    // Worth it only when ≥1 position emits a table — all-linear switches
    // are switchmap's job at the CLIF level.
    if !positions
        .iter()
        .any(|(p, _)| {
            matches!(p, Pos::IntTab { .. } | Pos::SymTab { .. } | Pos::RawTab { .. })
        })
    {
        return None;
    }
    Some(Plan {
        cond,
        width,
        min,
        n,
        join,
        ret,
        positions,
        sinks,
        default_cvs,
        default_else,
        full,
        needs_check,
        arg_tys,
    })
}

/// `s{n}_sanitized` Identifier + sym maps (same scheme as pgo/context).
fn ident(st: &mut State, sym: &str) -> Identifier {
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

fn global_for(
    ctx: &mut Context,
    st: &mut State,
    module: Ptr<Operation>,
    sym: &str,
    elem: TypeHandle,
    n: u64,
    init: ConstVal,
) -> Value {
    let aty: TypeHandle = ArrayType::get(ctx, elem, n).into();
    let op = GlobalOp::new(ctx, ident(st, sym), aty).get_operation();
    Operation::get_op::<ModuleOp>(module, ctx)
        .unwrap()
        .append_operation(ctx, op, 0);
    let init_v = crate::sroa::mk_const(ctx, st, aty, init);
    st.globals.insert(
        sym.to_string(),
        GlobalInfo {
            ty: aty,
            init: Some(init_v),
            align: crate::types::size_align(ctx, elem).1 as u64,
            mutable: false,
            tls: false,
            linkage: Linkage::Local,
            used: false,
            section: None,
        },
    );
    crate::sroa::mk_const(
        ctx,
        st,
        PointerType::get(ctx, 0).into(),
        ConstVal::Sym { sym: sym.to_string(), off: 0 },
    )
}

/// Emit `o` into hit_bb (cond_br path) or before the switch (select path).
fn push_op(
    ctx: &mut Context,
    hit: Option<Ptr<BasicBlock>>,
    swop: Ptr<Operation>,
    o: Ptr<Operation>,
) {
    if let Some(h) = hit {
        o.insert_at_back(h, ctx);
    } else {
        o.insert_before(ctx, swop);
    }
}

fn mk_mul(
    ctx: &mut Context,
    st: &mut State,
    hit: Option<Ptr<BasicBlock>>,
    swop: Ptr<Operation>,
    idx: Value,
    zero: Value,
    i64t: TypeHandle,
    s: i64,
) -> Value {
    if s == 0 {
        return zero;
    }
    if s == 1 {
        return idx;
    }
    let sc = crate::sroa::mk_const(ctx, st, i64t, ConstVal::Bits(s as u64 as u128));
    let m = MulOp::new_with_overflow_flag(ctx, idx, sc, IntegerOverflowFlagsAttr::default())
        .get_operation();
    push_op(ctx, hit, swop, m);
    m.deref(ctx).get_result(ctx, 0)
}

fn trunc_to(
    ctx: &mut Context,
    hit: Option<Ptr<BasicBlock>>,
    swop: Ptr<Operation>,
    v: Value,
    ty: TypeHandle,
) -> Value {
    if let TyK::Int(w) = classify(ctx, ty) {
        if w < 64 {
            let t = TruncOp::new(ctx, v, ty).get_operation();
            push_op(ctx, hit, swop, t);
            return t.deref(ctx).get_result(ctx, 0);
        }
    }
    v
}

/// Table + reconstruction for IntTab/SymTab: `idx*s + b + zext(tab[idx])`
/// (`gep(sym + b, ...)` when the result is a symbol offset).
fn emit_tab(
    ctx: &mut Context,
    st: &mut State,
    module: Ptr<Operation>,
    hit: Option<Ptr<BasicBlock>>,
    swop: Ptr<Operation>,
    idx: Value,
    idxs: Value,
    zero: Value,
    i64t: TypeHandle,
    i8t: TypeHandle,
    i16t: TypeHandle,
    ptrt: TypeHandle,
    symv: Option<String>,
    s: i64,
    b: i64,
    wide: bool,
    bytes: &[u8],
    argty: TypeHandle,
    n: u64,
    tag: &str,
    ctr: usize,
) -> Value {
    let elem = if wide { i16t } else { i8t };
    let tname = format!("__pliron_lut.{tag}.{ctr}");
    let tabv = global_for(ctx, st, module, &tname, elem, n, ConstVal::Bytes(bytes.to_vec()));
    let g = GetElementPtrOp::new(ctx, tabv, vec![GepIndex::Value(idxs)], elem).get_operation();
    push_op(ctx, hit, swop, g);
    let gp = g.deref(ctx).get_result(ctx, 0);
    let l = LoadOp::new(ctx, gp, elem).get_operation();
    push_op(ctx, hit, swop, l);
    let lp = l.deref(ctx).get_result(ctx, 0);
    let z = ZExtOp::new(ctx, lp, i64t).get_operation();
    push_op(ctx, hit, swop, z);
    let zp = z.deref(ctx).get_result(ctx, 0);
    let t = mk_mul(ctx, st, hit, swop, idx, zero, i64t, s);
    let a = AddOp::new_with_overflow_flag(
        ctx,
        t,
        zp,
        IntegerOverflowFlagsAttr::default(),
    )
    .get_operation();
    push_op(ctx, hit, swop, a);
    let u = a.deref(ctx).get_result(ctx, 0);
    if let Some(sym) = symv {
        let base = crate::sroa::mk_const(ctx, st, ptrt, ConstVal::Sym { sym, off: b });
        let g2 = GetElementPtrOp::new(ctx, base, vec![GepIndex::Value(u)], i8t).get_operation();
        push_op(ctx, hit, swop, g2);
        g2.deref(ctx).get_result(ctx, 0)
    } else {
        let u2 = if b != 0 {
            let bc = crate::sroa::mk_const(ctx, st, i64t, ConstVal::Bits(b as u64 as u128));
            let a2 = AddOp::new_with_overflow_flag(ctx, u, bc, IntegerOverflowFlagsAttr::default())
                .get_operation();
            push_op(ctx, hit, swop, a2);
            a2.deref(ctx).get_result(ctx, 0)
        } else {
            u
        };
        trunc_to(ctx, hit, swop, u2, argty)
    }
}

fn emit(
    ctx: &mut Context,
    st: &mut State,
    fname: &str,
    fop: Ptr<Operation>,
    swb: Ptr<BasicBlock>,
    swop: Ptr<Operation>,
    p: &Plan,
) {
    let i64t: TypeHandle = IntegerType::get(ctx, 64, Signedness::Signless).into();
    let i8t: TypeHandle = IntegerType::get(ctx, 8, Signedness::Signless).into();
    let i16t: TypeHandle = IntegerType::get(ctx, 16, Signedness::Signless).into();
    let ptrt: TypeHandle = PointerType::get(ctx, 0).into();
    let region = swb
        .deref(ctx)
        .get_parent_region()
        .expect("lutcorr: block without region");
    let module = fop
        .deref(ctx)
        .get_parent_op(ctx)
        .expect("lutcorr: func without module");

    // Ops go to hit_bb under the cond_br path, else before the switch.
    let hit = if p.default_else.is_some() {
        let b = BasicBlock::new(ctx, None, vec![]);
        b.insert_at_back(region, ctx);
        Some(b)
    } else {
        None
    };

    let zero = crate::sroa::mk_const(ctx, st, i64t, ConstVal::Bits(0));
    // idx = zext(x) - min, in i64.
    let x64 = if p.width < 64 {
        let z = ZExtOp::new(ctx, p.cond, i64t).get_operation();
        push_op(ctx, hit, swop, z);
        z.deref(ctx).get_result(ctx, 0)
    } else {
        p.cond
    };
    let idx = if p.full || p.min == 0 {
        x64
    } else {
        let minv = crate::sroa::mk_const(ctx, st, i64t, ConstVal::Bits(p.min as u128));
        let idxo =
            SubOp::new_with_overflow_flag(ctx, x64, minv, IntegerOverflowFlagsAttr::default())
                .get_operation();
        push_op(ctx, hit, swop, idxo);
        idxo.deref(ctx).get_result(ctx, 0)
    };
    // inrange + (for the select path) a clamped table index. A full-domain
    // table covers every input: no check, no clamp, no selects.
    let (inrange, idxs) = if p.needs_check && !p.full {
        let nm1 = crate::sroa::mk_const(ctx, st, i64t, ConstVal::Bits((p.n - 1) as u128));
        let c = ICmpOp::new(ctx, ICmpPredicateAttr::ULE, idx, nm1).get_operation();
        push_op(ctx, hit, swop, c);
        let cv = c.deref(ctx).get_result(ctx, 0);
        let gidx = if p.default_else.is_some() {
            idx // table load only runs on the hit path
        } else {
            let sel = SelectOp::new(ctx, cv, idx, zero).get_operation();
            push_op(ctx, hit, swop, sel);
            sel.deref(ctx).get_result(ctx, 0)
        };
        (Some(cv), gidx)
    } else {
        (None, idx)
    };

    let mut outs: Vec<(Value, &Sink)> = Vec::with_capacity(p.positions.len());
    let mut tabs = 0usize;
    for (j, (pos, outv)) in p.positions.iter().enumerate() {
        let res: Value = match pos {
            Pos::Same => match outv {
                Out::Value(v) => *v,
                Out::Const(cv) => crate::sroa::mk_const(ctx, st, p.arg_tys[j], cv.clone()),
            },
            Pos::IntLin { s, b } => {
                let t = mk_mul(ctx, st, hit, swop, idx, zero, i64t, *s);
                let u = if *b != 0 {
                    let bc =
                        crate::sroa::mk_const(ctx, st, i64t, ConstVal::Bits(*b as u64 as u128));
                    let a = AddOp::new_with_overflow_flag(
                        ctx,
                        t,
                        bc,
                        IntegerOverflowFlagsAttr::default(),
                    )
                    .get_operation();
                    push_op(ctx, hit, swop, a);
                    a.deref(ctx).get_result(ctx, 0)
                } else {
                    t
                };
                trunc_to(ctx, hit, swop, u, p.arg_tys[j])
            }
            Pos::SymLin { sym, s, b } => {
                let t = mk_mul(ctx, st, hit, swop, idx, zero, i64t, *s);
                let base = crate::sroa::mk_const(
                    ctx,
                    st,
                    ptrt,
                    ConstVal::Sym { sym: sym.clone(), off: *b },
                );
                let g = GetElementPtrOp::new(ctx, base, vec![GepIndex::Value(t)], i8t)
                    .get_operation();
                push_op(ctx, hit, swop, g);
                g.deref(ctx).get_result(ctx, 0)
            }
            Pos::IntTab { s, b, wide, bytes } => {
                tabs += 1;
                emit_tab(
                    ctx, st, module, hit, swop, idx, idxs, zero, i64t, i8t, i16t, ptrt, None, *s,
                    *b, *wide, bytes, p.arg_tys[j], p.n, fname, tabs,
                )
            }
            Pos::SymTab { sym, s, b, wide, bytes } => {
                tabs += 1;
                emit_tab(
                    ctx, st, module, hit, swop, idx, idxs, zero, i64t, i8t, i16t, ptrt,
                    Some(sym.clone()), *s, *b, *wide, bytes, p.arg_tys[j], p.n, fname, tabs,
                )
            }
            Pos::RawTab { ty, cvs } => {
                tabs += 1;
                let elems: Vec<Value> = cvs
                    .iter()
                    .map(|cv| crate::sroa::mk_const(ctx, st, *ty, cv.clone()))
                    .collect();
                let tname = format!("__pliron_lut.{fname}.{tabs}");
                let tabv = global_for(
                    ctx, st, module, &tname, *ty, p.n, ConstVal::Agg(elems),
                );
                let g = GetElementPtrOp::new(ctx, tabv, vec![GepIndex::Value(idxs)], *ty)
                    .get_operation();
                push_op(ctx, hit, swop, g);
                let gp = g.deref(ctx).get_result(ctx, 0);
                let l = LoadOp::new(ctx, gp, *ty).get_operation();
                push_op(ctx, hit, swop, l);
                let lv = l.deref(ctx).get_result(ctx, 0);
                if let (TyK::Int(ew), TyK::Int(aw)) =
                    (classify(ctx, *ty), classify(ctx, p.arg_tys[j]))
                {
                    if ew < aw {
                        let z = ZExtOp::new(ctx, lv, p.arg_tys[j]).get_operation();
                        push_op(ctx, hit, swop, z);
                        z.deref(ctx).get_result(ctx, 0)
                    } else {
                        lv
                    }
                } else {
                    lv
                }
            }
        };
        // Select-path default: `inrange ? res : default`.
        let res = if let (Some(cv), Some(dvs)) = (inrange, &p.default_cvs) {
            if matches!(pos, Pos::Same) {
                res
            } else {
                let dv = crate::sroa::mk_const(ctx, st, p.arg_tys[j], dvs[j].clone());
                let sel = SelectOp::new(ctx, cv, res, dv).get_operation();
                push_op(ctx, hit, swop, sel);
                sel.deref(ctx).get_result(ctx, 0)
            }
        } else {
            res
        };
        outs.push((res, &p.sinks[j]));
    }

    // Route each result through its sink: store slots first (the join then
    // loads them as before), join args to `br`, ret values to `return`.
    let mut jargs: Vec<Value> = vec![];
    let mut rargs: Vec<Value> = vec![];
    for (res, sink) in outs {
        match sink {
            Sink::Store(ptr) => {
                let so = StoreOp::new(ctx, res, *ptr).get_operation();
                push_op(ctx, hit, swop, so);
            }
            Sink::JoinArg => jargs.push(res),
            Sink::Ret => rargs.push(res),
        }
    }
    if let Some((edest, eopds)) = &p.default_else {
        let cb = CondBrOp::new(
            ctx,
            inrange.expect("lutcorr: else default needs a range check"),
            hit.unwrap(),
            vec![],
            *edest,
            eopds.clone(),
        )
        .get_operation();
        cb.insert_before(ctx, swop);
    }
    if p.ret {
        let r = ReturnOp::new(ctx, rargs.first().copied()).get_operation();
        push_op(ctx, hit, swop, r);
    } else if let Some(j) = p.join {
        let br = BrOp::new(ctx, j, jargs).get_operation();
        push_op(ctx, hit, swop, br);
    }
    Operation::erase(swop, ctx);
}

fn fold_switches(ctx: &mut Context, st: &mut State) -> usize {
    let mut folded = 0usize;
    for fname in bodied(ctx, st) {
        let Some(fop) = st.funcs.get(&fname).map(|f| f.op) else {
            continue;
        };
        if !crate::helpers::opt_body(ctx, st, fop) {
            continue;
        }
        let mut sws: Vec<(Ptr<BasicBlock>, Ptr<Operation>)> = vec![];
        for b in crate::inline::blocks_iter(ctx, fop) {
            if let Some(&t) = ops(ctx, b).last() {
                if Operation::is_op::<SwitchOp>(t, ctx) {
                    sws.push((b, t));
                }
            }
        }
        if sws.is_empty() {
            continue;
        }
        let mut dirty = false;
        for (b, t) in sws {
            if !crate::bisect_in("lutcorr", &fname) {
                continue;
            }
            let Some(plan) = analyze(ctx, st, b, t) else {
                continue;
            };
            emit(ctx, st, &fname, fop, b, t, &plan);
            folded += 1;
            dirty = true;
            tracing::debug!(target: "pliron::lutcorr", %fname, "switch folded to lutcorr");
        }
        if dirty {
            let bs: Vec<Ptr<BasicBlock>> = crate::inline::blocks_iter(ctx, fop).collect();
            let om: FxHashMap<_, _> = bs.iter().map(|&b| (b, ops(ctx, b))).collect();
            erase_unreachable(ctx, st, &bs, &|b| om[&b].clone());
        }
    }
    folded
}

/// Fold eligible `switch` ops. Returns the number folded.
pub fn run(ctx: &mut Context, st: &mut State) -> usize {
    if !env_is("PLIRON_LUTCORR", "1") {
        return 0;
    }
    fold_switches(ctx, st)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inline::blocks;
    use crate::state::{Exts, FuncInfo};
    use pliron::builtin::attributes::IntegerAttr;
    use pliron::builtin::op_interfaces::SymbolOpInterface;
    use pliron::irfmt::parsers::spaced;
    use pliron::parsable::parse_from_str;
    use pliron::printable::Printable;
    use pliron_llvm::ops::FuncOp;

    fn setup(ctx: &mut Context, st: &mut State, input: &str) -> Ptr<Operation> {
        let op = parse_from_str(spaced(Operation::top_level_parser()), ctx, input).expect("parse");
        for fop in blocks(ctx, op)
            .into_iter()
            .flat_map(|b| ops(ctx, b))
        {
            let Some(f) = Operation::get_op::<FuncOp>(fop, ctx) else {
                continue;
            };
            let sym = f.get_symbol_name(ctx).to_string();
            st.ident_to_sym.insert(sym.clone(), sym.clone());
            st.funcs.insert(
                sym.clone(),
                FuncInfo {
                    op: fop,
                    ty: f.get_type(ctx).into(),
                    linkage: Linkage::Local,
                    exts: Exts::default(),
                    no_inline: false,
                    always_inline: false,
                    inline_hint: false,
                    cold: false,
                    nounwind: false,
                    odr: false,
                    sret_ty: None,
                    frozen: Vec::new(),
                    noalias: Vec::new(),
                    nonnull: Vec::new(),
                    nparams: 1,
                    tail_cc: false,
                },
            );
            // `llvm.constant` results -> st.consts entries.
            for b in blocks(ctx, fop) {
                for o in ops(ctx, b) {
                    if let Some(c) = Operation::get_op::<ConstantOp>(o, ctx) {
                        if let Some(i) = c
                            .get_value(ctx)
                            .as_any()
                            .downcast_ref::<IntegerAttr>()
                        {
                            st.consts.insert(
                                o.deref(ctx).get_result(ctx, 0),
                                ConstVal::Bits(i.value().to_u128()),
                            );
                        }
                    }
                }
            }
        }
        op
    }

    /// The showcase shape: an enum→string-offset switch whose offsets are
    /// `17*i + {0,0,0,1,0}` — nearly the corsix example verbatim.
    #[test]
    fn folds_offset_table() {
        let input = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @f: llvm.func <builtin.integer i64 (builtin.integer i8) variadic = false> [] {
          ^entry(x: builtin.integer i8):
            llvm.switch x, ^default()
            [
              { <0: i8> : ^bb0() },
              { <1: i8> : ^bb1() },
              { <2: i8> : ^bb2() },
              { <3: i8> : ^bb3() },
              { <4: i8> : ^bb4() }
            ]
            ^default():
            llvm.unreachable
            ^bb0():
            o0 = llvm.constant <builtin.integer <0: i64>> : builtin.integer i64;
            llvm.return o0
            ^bb1():
            o1 = llvm.constant <builtin.integer <17: i64>> : builtin.integer i64;
            llvm.return o1
            ^bb2():
            o2 = llvm.constant <builtin.integer <34: i64>> : builtin.integer i64;
            llvm.return o2
            ^bb3():
            o3 = llvm.constant <builtin.integer <52: i64>> : builtin.integer i64;
            llvm.return o3
            ^bb4():
            o4 = llvm.constant <builtin.integer <68: i64>> : builtin.integer i64;
            llvm.return o4
          }
        }
        "#;
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let module = setup(ctx, st, input);
        let n = fold_switches(ctx, st);
        assert_eq!(n, 1);
        let out = module.deref(ctx).print(ctx, &pliron::printable::State::default()).to_string();
        assert!(out.contains("llvm.load"), "should load the table:\n{out}");
        assert!(out.contains("llvm.mul"), "should compute idx*17:\n{out}");
        assert!(!out.contains("llvm.switch"), "switch must be gone:\n{out}");
        assert!(out.contains("__pliron_lut"), "table global:\n{out}");
    }

    /// The shape rustc actually emits: each arm stores its constants to
    /// shared allocas, then branches to a join that loads them.
    #[test]
    fn folds_store_shaped_arms() {
        let input = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @f: llvm.func <builtin.integer i32 (builtin.integer i8) variadic = false> [] {
          ^entry(x: builtin.integer i8):
            one = llvm.constant <builtin.integer <1: i64>> : builtin.integer i64;
            p = llvm.alloca [builtin.integer i32 x one] : llvm.ptr (0);
            llvm.switch x, ^default()
            [
              { <0: i8> : ^bb0() },
              { <1: i8> : ^bb1() },
              { <2: i8> : ^bb2() }
            ]
            ^bb0():
            c0 = llvm.constant <builtin.integer <10: i32>> : builtin.integer i32;
            llvm.store *p <- c0;
            llvm.br ^join()
            ^bb1():
            c1 = llvm.constant <builtin.integer <25: i32>> : builtin.integer i32;
            llvm.store *p <- c1;
            llvm.br ^join()
            ^bb2():
            c2 = llvm.constant <builtin.integer <30: i32>> : builtin.integer i32;
            llvm.store *p <- c2;
            llvm.br ^join()
            ^join():
            v = llvm.load p : builtin.integer i32;
            llvm.return v
            ^default():
            llvm.unreachable
          }
        }
        "#;
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let module = setup(ctx, st, input);
        let n = fold_switches(ctx, st);
        assert_eq!(n, 1);
        let out = module.deref(ctx).print(ctx, &pliron::printable::State::default()).to_string();
        assert!(!out.contains("llvm.switch"), "switch must be gone:\n{out}");
        assert!(out.contains("llvm.store"), "computed store must remain:\n{out}");
        assert!(out.contains("llvm.load"), "join load preserved:\n{out}");
    }

    /// A switch whose default is real code and whose domain is dense folds
    /// to `cond_br inrange -> hit_bb else default`.
    #[test]
    fn folds_dense_else_default() {
        let input = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @f: llvm.func <builtin.integer i64 (builtin.integer i8) variadic = false> [] {
          ^entry(x: builtin.integer i8):
            llvm.switch x, ^else()
            [
              { <0: i8> : ^bb0() },
              { <1: i8> : ^bb1() },
              { <2: i8> : ^bb2() }
            ]
            ^bb0():
            o0 = llvm.constant <builtin.integer <10: i64>> : builtin.integer i64;
            llvm.return o0
            ^bb1():
            o1 = llvm.constant <builtin.integer <20: i64>> : builtin.integer i64;
            llvm.return o1
            ^bb2():
            o2 = llvm.constant <builtin.integer <31: i64>> : builtin.integer i64;
            llvm.return o2
            ^else():
            o9 = llvm.constant <builtin.integer <99: i64>> : builtin.integer i64;
            d = llvm.call @g (o9) : llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false>;
            llvm.return d
          };
          llvm.func @g: llvm.func <builtin.integer i64 (builtin.integer i64) variadic = false> []
        }
        "#;
        let ctx = &mut Context::new();
        let st = &mut State::default();
        let module = setup(ctx, st, input);
        let n = fold_switches(ctx, st);
        assert_eq!(n, 1);
        let out = module.deref(ctx).print(ctx, &pliron::printable::State::default()).to_string();
        assert!(out.contains("llvm.condbr") || out.contains("llvm.cond_br"), "cond_br:\n{out}");
        assert!(out.contains("llvm.load"), "table load on hit path:\n{out}");
    }

    /// Residuals wider than u16 → leave the switch alone.
    #[test]
    fn rejects_wide_residuals() {
        let input = r#"
        builtin.module @m {
        ^block_0_0():
          llvm.func @f: llvm.func <builtin.integer i64 (builtin.integer i8) variadic = false> [] {
          ^entry(x: builtin.integer i8):
            llvm.switch x, ^default()
            [
              { <0: i8> : ^bb0() },
              { <1: i8> : ^bb1() }
            ]
            ^default():
            llvm.unreachable
            ^bb0():
            o0 = llvm.constant <builtin.integer <0: i64>> : builtin.integer i64;
            llvm.return o0
            ^bb1():
            o1 = llvm.constant <builtin.integer <70000: i64>> : builtin.integer i64;
            llvm.return o1
          }
        }
        "#;
        let ctx = &mut Context::new();
        let st = &mut State::default();
        setup(ctx, st, input);
        assert_eq!(fold_switches(ctx, st), 0);
    }
}
