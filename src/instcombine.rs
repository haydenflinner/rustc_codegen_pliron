//! instcombine over integer ops via equality saturation (`egg`). Each function's pure
//! integer ops (add/sub/mul/and/or/xor/shifts/icmp/select) go into one e-graph with a
//! constant-folding analysis and algebraic identities. A result is replaced when its
//! class proves it constant, or equal to one of its own operands (which dominates it).
//! Cranelift folds the same algebra later; this runs first so SROA/memcpyopt and the
//! branch structure see the folded values. `PLIRON_INSTCOMBINE=0` disables it.

use egg::{
    Analysis, Applier, DidMerge, EGraph, Id, Language, PatternAst, Rewrite, Runner, Subst, Symbol,
    Var, define_language, rewrite as rw,
};
use pliron::{
    context::{Context, Ptr},
    linked_list::ContainsLinkedList,
    op::Op,
    operation::Operation,
    r#type::Typed,
    value::Value,
};
use pliron_llvm::{attributes::ICmpPredicateAttr as P, ops::*};
use rustc_data_structures::fx::FxHashMap;

use crate::context::{ConstVal, State};
use crate::lower::has_body;
use crate::types::{TyK, classify};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cst(u128, u32);

impl std::fmt::Display for Cst {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}i{}", self.0, self.1)
    }
}
impl std::str::FromStr for Cst {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        let (b, w) = s.split_once('i').ok_or(())?;
        Ok(Cst(b.parse().map_err(|_| ())?, w.parse().map_err(|_| ())?))
    }
}

define_language! {
    pub enum IC {
        "add" = Add([Id; 2]),
        "sub" = Sub([Id; 2]),
        "mul" = Mul([Id; 2]),
        "and" = And([Id; 2]),
        "or" = Or([Id; 2]),
        "xor" = Xor([Id; 2]),
        "shl" = Shl([Id; 2]),
        "lshr" = LShr([Id; 2]),
        "ashr" = AShr([Id; 2]),
        "eq" = Eq([Id; 2]),
        "ne" = Ne([Id; 2]),
        "ult" = Ult([Id; 2]),
        "ule" = Ule([Id; 2]),
        "slt" = Slt([Id; 2]),
        "sle" = Sle([Id; 2]),
        "select" = Select([Id; 3]),
        Const(Cst),
        Leaf(Symbol),
    }
}

fn mask(b: u128, w: u32) -> u128 {
    if w >= 128 { b } else { b & ((1u128 << w) - 1) }
}
fn sext(b: u128, w: u32) -> i128 {
    if w >= 128 {
        b as i128
    } else {
        ((b << (128 - w)) as i128) >> (128 - w)
    }
}

/// Class data: bit width and, if known, the constant value.
#[derive(Default)]
pub struct Fold {
    widths: FxHashMap<Symbol, u32>,
}

impl Analysis<IC> for Fold {
    type Data = (u32, Option<u128>);

    fn make(eg: &mut EGraph<IC, Self>, n: &IC) -> Self::Data {
        let d = |i: &Id| eg[*i].data;
        let bin = |a: &Id, b: &Id, f: &dyn Fn(u128, u128, u32) -> u128| {
            let ((w, x), (_, y)) = (d(a), d(b));
            (w, x.zip(y).map(|(x, y)| mask(f(x, y, w), w)))
        };
        let cmp = |a: &Id, b: &Id, f: &dyn Fn(u128, u128, u32) -> bool| {
            let ((w, x), (_, y)) = (d(a), d(b));
            (1, x.zip(y).map(|(x, y)| f(x, y, w) as u128))
        };
        match n {
            IC::Const(c) => (c.1, Some(mask(c.0, c.1))),
            IC::Leaf(s) => (eg.analysis.widths[s], None),
            IC::Add([a, b]) => bin(a, b, &|x, y, _| x.wrapping_add(y)),
            IC::Sub([a, b]) => bin(a, b, &|x, y, _| x.wrapping_sub(y)),
            IC::Mul([a, b]) => bin(a, b, &|x, y, _| x.wrapping_mul(y)),
            IC::And([a, b]) => bin(a, b, &|x, y, _| x & y),
            IC::Or([a, b]) => bin(a, b, &|x, y, _| x | y),
            IC::Xor([a, b]) => bin(a, b, &|x, y, _| x ^ y),
            // Out-of-range shifts are poison in LLVM; mask like the Cranelift lowering.
            IC::Shl([a, b]) => bin(a, b, &|x, y, w| x << (y % w as u128)),
            IC::LShr([a, b]) => bin(a, b, &|x, y, w| x >> (y % w as u128)),
            IC::AShr([a, b]) => bin(a, b, &|x, y, w| (sext(x, w) >> (y % w as u128)) as u128),
            IC::Eq([a, b]) => cmp(a, b, &|x, y, _| x == y),
            IC::Ne([a, b]) => cmp(a, b, &|x, y, _| x != y),
            IC::Ult([a, b]) => cmp(a, b, &|x, y, _| x < y),
            IC::Ule([a, b]) => cmp(a, b, &|x, y, _| x <= y),
            IC::Slt([a, b]) => cmp(a, b, &|x, y, w| sext(x, w) < sext(y, w)),
            IC::Sle([a, b]) => cmp(a, b, &|x, y, w| sext(x, w) <= sext(y, w)),
            IC::Select([c, x, y]) => {
                let (w, xv) = d(x);
                let yv = d(y).1;
                match d(c).1 {
                    Some(0) => (w, yv),
                    Some(_) => (w, xv),
                    None => (w, xv.filter(|_| xv == yv)),
                }
            }
        }
    }

    fn merge(&mut self, a: &mut Self::Data, b: Self::Data) -> DidMerge {
        egg::merge_option(&mut a.1, b.1, |x, y| {
            debug_assert_eq!(*x, y);
            DidMerge(false, false)
        })
    }

    fn modify(eg: &mut EGraph<IC, Self>, id: Id) {
        if let (w, Some(c)) = eg[id].data {
            let k = eg.add(IC::Const(Cst(c, w)));
            eg.union(id, k);
            eg[id].nodes.retain(|n| n.is_leaf());
        }
    }
}

/// Applier that adds the constant `f(width of ?x)`.
struct ConstOf(Var, fn(u32) -> u128);
impl Applier<IC, Fold> for ConstOf {
    fn apply_one(
        &self,
        eg: &mut EGraph<IC, Fold>,
        eclass: Id,
        subst: &Subst,
        _: Option<&PatternAst<IC>>,
        _: Symbol,
    ) -> Vec<Id> {
        let w = eg[subst[self.0]].data.0;
        let k = eg.add(IC::Const(Cst(mask((self.1)(w), w), w)));
        if eg.union(eclass, k) {
            vec![eclass]
        } else {
            vec![]
        }
    }
}

fn is_const(
    v: &str,
    f: fn(u128, u32) -> bool,
) -> impl Fn(&mut EGraph<IC, Fold>, Id, &Subst) -> bool {
    let v: Var = v.parse().unwrap();
    move |eg, _, s| matches!(eg[s[v]].data, (w, Some(c)) if f(c, w))
}

fn rules() -> Vec<Rewrite<IC, Fold>> {
    let x: Var = "?x".parse().unwrap();
    let zero = |c: u128, _| c == 0;
    let one = |c: u128, _| c == 1;
    let ones = |c: u128, w| c == mask(!0, w);
    vec![
        rw!("add-comm"; "(add ?a ?b)" => "(add ?b ?a)"),
        rw!("mul-comm"; "(mul ?a ?b)" => "(mul ?b ?a)"),
        rw!("and-comm"; "(and ?a ?b)" => "(and ?b ?a)"),
        rw!("or-comm"; "(or ?a ?b)" => "(or ?b ?a)"),
        rw!("xor-comm"; "(xor ?a ?b)" => "(xor ?b ?a)"),
        rw!("eq-comm"; "(eq ?a ?b)" => "(eq ?b ?a)"),
        rw!("ne-comm"; "(ne ?a ?b)" => "(ne ?b ?a)"),
        rw!("add-assoc"; "(add (add ?a ?b) ?c)" => "(add ?a (add ?b ?c))"),
        rw!("and-assoc"; "(and (and ?a ?b) ?c)" => "(and ?a (and ?b ?c))"),
        rw!("or-assoc"; "(or (or ?a ?b) ?c)" => "(or ?a (or ?b ?c))"),
        rw!("add-0"; "(add ?x ?z)" => "?x" if is_const("?z", zero)),
        rw!("sub-0"; "(sub ?x ?z)" => "?x" if is_const("?z", zero)),
        rw!("sub-add"; "(sub (add ?x ?y) ?y)" => "?x"),
        rw!("add-sub"; "(add (sub ?x ?y) ?y)" => "?x"),
        rw!("mul-1"; "(mul ?x ?z)" => "?x" if is_const("?z", one)),
        rw!("and-ones"; "(and ?x ?z)" => "?x" if is_const("?z", ones)),
        rw!("and-self"; "(and ?x ?x)" => "?x"),
        rw!("or-0"; "(or ?x ?z)" => "?x" if is_const("?z", zero)),
        rw!("or-self"; "(or ?x ?x)" => "?x"),
        rw!("xor-0"; "(xor ?x ?z)" => "?x" if is_const("?z", zero)),
        rw!("xor-xor"; "(xor (xor ?x ?y) ?y)" => "?x"),
        rw!("shl-0"; "(shl ?x ?z)" => "?x" if is_const("?z", zero)),
        rw!("lshr-0"; "(lshr ?x ?z)" => "?x" if is_const("?z", zero)),
        rw!("ashr-0"; "(ashr ?x ?z)" => "?x" if is_const("?z", zero)),
        rw!("sel-same"; "(select ?c ?x ?x)" => "?x"),
        rw!("sel-not"; "(select (xor ?c ?t) ?x ?y)" => "(select ?c ?y ?x)" if is_const("?t", one)),
        rw!("ne-0-i1"; "(ne ?b ?z)" => "?b" if is_const("?z", zero) if |eg: &mut EGraph<IC, Fold>, _, s: &Subst| eg[s["?b".parse().unwrap()]].data.0 == 1),
        rw!("sub-self"; "(sub ?x ?x)" => { ConstOf(x, |_| 0) }),
        rw!("xor-self"; "(xor ?x ?x)" => { ConstOf(x, |_| 0) }),
        rw!("eq-self"; "(eq ?x ?x)" => { ConstOf(x, |_| 1) }),
        rw!("ule-self"; "(ule ?x ?x)" => { ConstOf(x, |_| 1) }),
        rw!("sle-self"; "(sle ?x ?x)" => { ConstOf(x, |_| 1) }),
        rw!("ne-self"; "(ne ?x ?x)" => { ConstOf(x, |_| 0) }),
        rw!("ult-self"; "(ult ?x ?x)" => { ConstOf(x, |_| 0) }),
        rw!("slt-self"; "(slt ?x ?x)" => { ConstOf(x, |_| 0) }),
        rw!("ult-0"; "(ult ?x ?z)" => { ConstOf(x, |_| 0) } if is_const("?z", zero)),
    ]
}

fn node(ctx: &Context, op: Ptr<Operation>, a: &[Id]) -> Option<IC> {
    macro_rules! is {
        ($t:ty) => {
            Operation::is_op::<$t>(op, ctx)
        };
    }
    let b2 = || [a[0], a[1]];
    Some(if is!(AddOp) {
        IC::Add(b2())
    } else if is!(SubOp) {
        IC::Sub(b2())
    } else if is!(MulOp) {
        IC::Mul(b2())
    } else if is!(AndOp) {
        IC::And(b2())
    } else if is!(OrOp) {
        IC::Or(b2())
    } else if is!(XorOp) {
        IC::Xor(b2())
    } else if is!(ShlOp) {
        IC::Shl(b2())
    } else if is!(LShrOp) {
        IC::LShr(b2())
    } else if is!(AShrOp) {
        IC::AShr(b2())
    } else if is!(SelectOp) {
        IC::Select([a[0], a[1], a[2]])
    } else if let Some(c) = Operation::get_op::<ICmpOp>(op, ctx) {
        let (x, y) = (a[0], a[1]);
        match c.predicate(ctx) {
            P::EQ => IC::Eq([x, y]),
            P::NE => IC::Ne([x, y]),
            P::ULT => IC::Ult([x, y]),
            P::ULE => IC::Ule([x, y]),
            P::UGT => IC::Ult([y, x]),
            P::UGE => IC::Ule([y, x]),
            P::SLT => IC::Slt([x, y]),
            P::SLE => IC::Sle([x, y]),
            P::SGT => IC::Slt([y, x]),
            P::SGE => IC::Sle([y, x]),
        }
    } else {
        return None;
    })
}

fn int_width(ctx: &Context, v: Value) -> Option<u32> {
    match classify(ctx, v.get_type(ctx)) {
        TyK::Int(w) if (1..=128).contains(&w) => Some(w),
        _ => None,
    }
}

const MAX_OPS: usize = 20_000;

fn run_fn(ctx: &mut Context, st: &mut State<'_>, f: Ptr<Operation>) -> (usize, usize) {
    let mut eg: EGraph<IC, Fold> = EGraph::new(Fold::default());
    let mut ids: FxHashMap<Value, Id> = FxHashMap::default();
    let mut leaves: FxHashMap<Symbol, Value> = FxHashMap::default();
    let mut roots: Vec<(Ptr<Operation>, Value, Id)> = Vec::new();
    let blocks: Vec<_> = f.deref(ctx).get_region(0).deref(ctx).iter(ctx).collect();
    let mut nops = 0;
    for b in blocks {
        let ops: Vec<_> = b.deref(ctx).iter(ctx).collect();
        for op in ops {
            nops += 1;
            if nops > MAX_OPS || op.deref(ctx).get_num_results() != 1 {
                continue;
            }
            let r = op.deref(ctx).get_result(0);
            if int_width(ctx, r).is_none() {
                continue;
            }
            let opnds: Vec<Value> = op.deref(ctx).operands().collect();
            let mut a = Vec::with_capacity(opnds.len());
            for &v in &opnds {
                let id = if let Some(&id) = ids.get(&v) {
                    id
                } else if let Some(vw) = int_width(ctx, v) {
                    let id = match st.consts.get(&v) {
                        Some(ConstVal::Bits(c)) => eg.add(IC::Const(Cst(*c, vw))),
                        Some(ConstVal::Zero) => eg.add(IC::Const(Cst(0, vw))),
                        _ => {
                            let s = Symbol::from(format!("v{}", leaves.len()));
                            eg.analysis.widths.insert(s, vw);
                            leaves.insert(s, v);
                            eg.add(IC::Leaf(s))
                        }
                    };
                    ids.insert(v, id);
                    id
                } else {
                    break;
                };
                a.push(id);
            }
            if a.len() != opnds.len() {
                continue;
            }
            let Some(n) = node(ctx, op, &a) else { continue };
            let id = eg.add(n);
            ids.insert(r, id);
            roots.push((op, r, id));
        }
    }
    if roots.is_empty() {
        return (0, 0);
    }
    let runner = Runner::default()
        .with_egraph(eg)
        .with_iter_limit(6)
        .with_node_limit(20 * roots.len() + 1000)
        .with_time_limit(std::time::Duration::from_millis(200))
        .run(&rules());
    let eg = runner.egraph;
    let (mut nc, mut nv) = (0, 0);
    for (op, r, id) in roots {
        let class = &eg[eg.find(id)];
        if r.uses(ctx).is_empty() {
            continue;
        }
        let new = if let (_, Some(c)) = class.data {
            nc += 1;
            let ty = r.get_type(ctx);
            let k = UndefOp::new(ctx, ty).get_operation();
            let kv = k.deref(ctx).get_result(0);
            st.consts.insert(
                kv,
                if c == 0 {
                    ConstVal::Zero
                } else {
                    ConstVal::Bits(c)
                },
            );
            kv
        } else {
            let opnds: Vec<Value> = op.deref(ctx).operands().collect();
            let Some(v) = class
                .nodes
                .iter()
                .find_map(|n| match n {
                    IC::Leaf(s) => Some(leaves[s]).filter(|v| opnds.contains(v)),
                    _ => None,
                })
                .or_else(|| {
                    opnds
                        .iter()
                        .copied()
                        .find(|o| ids.get(o).is_some_and(|&i| eg.find(i) == class.id))
                })
            else {
                continue;
            };
            nv += 1;
            v
        };
        r.replace_all_uses_with(ctx, &new);
    }
    (nc, nv)
}

pub fn run(ctx: &mut Context, st: &mut State<'_>) {
    let (mut nc, mut nv) = (0, 0);
    let funcs: Vec<_> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| has_body(ctx, f))
        .collect();
    for f in funcs {
        let (c, v) = run_fn(ctx, st, f);
        nc += c;
        nv += v;
    }
    if std::env::var_os("PLIRON_STATS").is_some() {
        eprintln!(
            "instcombine {}: {nc} values folded to constants, {nv} to an operand",
            st.cgu
        );
    }
}
