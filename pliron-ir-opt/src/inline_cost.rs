//! Callsite-specialized callee size (`PLIRON_INLINE_SPEC=1`), LLVM's
//! `CallAnalyzer` approach: walk the callee as it would look inlined at one
//! callsite, with the site's constant arguments substituted, and count only
//! the ops that survive.
//!
//! - Values known constant: params whose actual is a constant at this site,
//!   values with a `ConstVal` fact, and results of integer arithmetic,
//!   compares, casts and selects over known values.
//! - Only live blocks count. A `cond_br`/`switch` on a known value makes
//!   only its taken edge live. A block param is known when the block has a
//!   single predecessor and that live edge passes a known value.
//! - Ops that emit no code cost nothing: slots, jumps, constants, no-op
//!   casts, freezes, lifetime/debug/assume markers, and ops that fold to a
//!   known value (including selects and branches on one).
//!
//! The raw op count (`CInfo::n`) instead sizes every callee as if no
//! callsite told it anything, so a helper called with constant arguments
//! at many sites looked as large as its general body (pulldown's
//! `HtmlWriter::write(&'static str)`).

use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use pliron::linked_list::ContainsLinkedList;
use pliron::builtin::op_interfaces::BranchOpInterface;
use pliron::op::op_cast;
use pliron::operation::Operation;
use pliron::r#type::Typed;
use pliron::value::Value;
use pliron_llvm::attributes::ICmpPredicateAttr as P;
use pliron_llvm::ops::*;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::inline::blocks;
use crate::state::{ConstVal, State};
use crate::types::{TyK, classify};

/// Ops and calls left after specialization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpecCost {
    pub n: usize,
    pub ncalls: usize,
}

fn width(ctx: &Context, v: Value) -> Option<u32> {
    match classify(ctx, v.get_type(ctx)) {
        TyK::Int(w) if (1..=128).contains(&w) => Some(w),
        _ => None,
    }
}

fn mask(w: u32) -> u128 {
    if w >= 128 { u128::MAX } else { (1u128 << w) - 1 }
}

fn sext(b: u128, w: u32) -> i128 {
    ((b << (128 - w)) as i128) >> (128 - w)
}

/// No code emitted for `op` itself.
fn free_op(ctx: &Context, st: &State, op: Ptr<Operation>) -> bool {
    Operation::is_op::<AllocaOp>(op, ctx)
        || Operation::is_op::<BrOp>(op, ctx)
        || Operation::is_op::<ConstantOp>(op, ctx)
        || Operation::is_op::<UndefOp>(op, ctx)
        || Operation::is_op::<PoisonOp>(op, ctx)
        || Operation::is_op::<BitcastOp>(op, ctx)
        || Operation::is_op::<AddrSpaceCastOp>(op, ctx)
        || Operation::is_op::<FreezeOp>(op, ctx)
        || st.intrinsics.get(op).is_some_and(|s| {
            s.starts_with("llvm.lifetime") || s.starts_with("llvm.dbg") || s == "llvm.assume"
        })
}

struct Known<'a> {
    st: &'a State,
    map: FxHashMap<Value, u128>,
}

impl Known<'_> {
    fn get(&self, v: Value) -> Option<u128> {
        if let Some(&k) = self.map.get(&v) {
            return Some(k);
        }
        match self.st.consts.get(v)? {
            ConstVal::Bits(b) => Some(*b),
            ConstVal::Zero => Some(0),
            _ => None,
        }
    }
}

/// The value `op`'s single integer result folds to, given `k`.
fn eval(ctx: &Context, k: &Known, op: Ptr<Operation>) -> Option<u128> {
    let o = op.deref(ctx);
    if o.get_num_results() != 1 {
        return None;
    }
    let r = o.get_result(ctx, 0);
    let rw = width(ctx, r)?;
    let a: Vec<Value> = o.operands(ctx).collect();
    macro_rules! is {
        ($t:ty) => {
            Operation::is_op::<$t>(op, ctx)
        };
    }
    if let Some(sel) = Operation::get_op::<SelectOp>(op, ctx) {
        let _ = sel;
        let c = k.get(*a.first()?)?;
        return k.get(if c & 1 != 0 { a[1] } else { a[2] }).map(|x| x & mask(rw));
    }
    if is!(ZExtOp) || is!(TruncOp) || is!(SExtOp) {
        let w = width(ctx, a[0])?;
        let x = k.get(a[0])? & mask(w);
        return Some(if is!(SExtOp) { (sext(x, w) as u128) & mask(rw) } else { x & mask(rw) });
    }
    if let Some(ic) = Operation::get_op::<ICmpOp>(op, ctx) {
        let w = width(ctx, a[0])?;
        let (x, y) = (k.get(a[0])? & mask(w), k.get(a[1])? & mask(w));
        let (sx, sy) = (sext(x, w), sext(y, w));
        return Some(u128::from(match ic.predicate(ctx) {
            P::EQ => x == y,
            P::NE => x != y,
            P::ULT => x < y,
            P::ULE => x <= y,
            P::UGT => x > y,
            P::UGE => x >= y,
            P::SLT => sx < sy,
            P::SLE => sx <= sy,
            P::SGT => sx > sy,
            P::SGE => sx >= sy,
        }));
    }
    if a.len() != 2 {
        return None;
    }
    let m = mask(rw);
    let x = k.get(a[0]).map(|v| v & m);
    let y = k.get(a[1]).map(|v| v & m);
    // Absorbing operands fold without the other side.
    if is!(AndOp) && (x == Some(0) || y == Some(0)) {
        return Some(0);
    }
    if is!(MulOp) && (x == Some(0) || y == Some(0)) {
        return Some(0);
    }
    if is!(OrOp) && (x == Some(m) || y == Some(m)) {
        return Some(m);
    }
    let (x, y) = (x?, y?);
    let sh = |y: u128| (y < rw as u128).then_some(y as u32);
    Some(
        if is!(AddOp) {
            x.wrapping_add(y)
        } else if is!(SubOp) {
            x.wrapping_sub(y)
        } else if is!(MulOp) {
            x.wrapping_mul(y)
        } else if is!(AndOp) {
            x & y
        } else if is!(OrOp) {
            x | y
        } else if is!(XorOp) {
            x ^ y
        } else if is!(ShlOp) {
            x << sh(y)?
        } else if is!(LShrOp) {
            x >> sh(y)?
        } else if is!(AShrOp) {
            (sext(x, rw) >> sh(y)?) as u128
        } else {
            return None;
        } & m,
    )
}

/// Successor edges of terminator `t` that stay live given `k`, with the
/// operands each passes.
fn live_edges(ctx: &Context, k: &Known, t: Ptr<Operation>) -> Vec<(Ptr<BasicBlock>, Vec<Value>)> {
    if let Some(cb) = Operation::get_op::<CondBrOp>(t, ctx) {
        let c = k.get(t.deref(ctx).get_operand(ctx, 0));
        let td = (t.deref(ctx).get_successor(ctx, 0), cb.get_true_dest_operands(ctx));
        let fd = (t.deref(ctx).get_successor(ctx, 1), cb.get_false_dest_operands(ctx));
        return match c {
            Some(c) if c & 1 != 0 => vec![td],
            Some(_) => vec![fd],
            None => vec![td, fd],
        };
    }
    if let Some(sw) = Operation::get_op::<SwitchOp>(t, ctx) {
        let c = t.deref(ctx).get_operand(ctx, 0);
        let cases = sw.cases(ctx);
        if let (Some(kv), Some(w)) = (k.get(c), width(ctx, c)) {
            let m = mask(w);
            return match cases.iter().find(|x| x.value.value().to_u128() & m == kv & m) {
                Some(x) => vec![(x.dest, x.dest_opds.clone())],
                None => vec![(sw.default_dest(ctx), sw.default_dest_operands(ctx))],
            };
        }
        let mut v = vec![(sw.default_dest(ctx), sw.default_dest_operands(ctx))];
        v.extend(cases.into_iter().map(|x| (x.dest, x.dest_opds)));
        return v;
    }
    let o = Operation::get_op_dyn(t, ctx);
    let br = op_cast::<dyn BranchOpInterface>(&*o);
    t.deref(ctx)
        .successors(ctx)
        .enumerate()
        .map(|(i, s)| (s, br.map(|b| b.successor_operands(ctx, i)).unwrap_or_default()))
        .collect()
}

/// `callee`'s surviving ops and calls when called with `actuals`.
pub fn spec_cost(ctx: &Context, st: &State, callee: Ptr<Operation>, actuals: &[Value]) -> SpecCost {
    let bs = blocks(ctx, callee);
    let Some(&entry) = bs.first() else { return SpecCost::default() };
    let mut k = Known { st, map: FxHashMap::default() };
    for (p, a) in entry.deref(ctx).arguments(ctx).zip(actuals) {
        if let Some(v) = k.get(*a) {
            k.map.insert(p, v);
        }
    }
    // Predecessor counts over the whole CFG: a block param is only known
    // through a block's sole edge (back edges keep loop headers unknown).
    let mut npred: FxHashMap<Ptr<BasicBlock>, usize> = FxHashMap::default();
    for &b in &bs {
        for op in b.deref(ctx).iter(ctx) {
            for s in op.deref(ctx).successors(ctx) {
                *npred.entry(s).or_default() += 1;
            }
            if let Some(&(l, _)) = st.invokes.get(op) {
                *npred.entry(l).or_default() += 1;
            }
        }
    }
    let mut live: FxHashSet<Ptr<BasicBlock>> = [entry].into_iter().collect();
    let mut cost = SpecCost::default();
    // Layout order approximates RPO; a block first reached by a later
    // block's edge (a back edge) is visited in a second sweep.
    let mut done: FxHashSet<Ptr<BasicBlock>> = FxHashSet::default();
    loop {
        let mut progressed = false;
        for &b in &bs {
            if !live.contains(&b) || done.contains(&b) {
                continue;
            }
            done.insert(b);
            progressed = true;
            for op in b.deref(ctx).iter(ctx) {
                if let Some(&(l, _)) = st.invokes.get(op) {
                    live.insert(l);
                }
                let is_term = op.deref(ctx).get_num_successors() > 0
                    || Operation::is_op::<ReturnOp>(op, ctx)
                    || Operation::is_op::<UnreachableOp>(op, ctx);
                if is_term {
                    let edges = live_edges(ctx, &k, op);
                    let folded = edges.len() < op.deref(ctx).get_num_successors();
                    if !free_op(ctx, st, op) && !folded {
                        cost.n += 1;
                    }
                    for (s, opds) in edges {
                        live.insert(s);
                        if npred.get(&s).copied() == Some(1) {
                            for (p, a) in s.deref(ctx).arguments(ctx).zip(opds) {
                                if let Some(v) = k.get(a) {
                                    k.map.insert(p, v);
                                }
                            }
                        }
                    }
                    continue;
                }
                if free_op(ctx, st, op) {
                    continue;
                }
                if let Some(v) = eval(ctx, &k, op) {
                    let r = op.deref(ctx).get_result(ctx, 0);
                    k.map.insert(r, v);
                    continue;
                }
                cost.n += 1;
                if Operation::is_op::<CallOp>(op, ctx) {
                    cost.ncalls += 1;
                }
            }
        }
        if !progressed {
            break;
        }
    }
    cost
}
