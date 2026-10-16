//! instcombine over integer ops. Default: a worklist peephole — a union-find
//! over SSA values plus constant propagation covering the same identities the
//! old `egg` equality-saturation pass did (operands canonicalize through
//! folded chains, so e.g. `sub (add x y) y` still folds to `x`). Constants
//! come from `st.consts`; a result is replaced when its canonical form is a
//! constant or another value that transitively was an operand of it (and so
//! dominates it). `PLIRON_INSTCOMBINE=egg` runs the e-graph version for A/B,
//! `=0` disables the pass.

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
use pliron_llvm::{attributes::ICmpPredicateAttr as P, op_interfaces::BinArithOp, ops::*};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::has_body;
use crate::state::{ConstVal, State};
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

fn run_fn_egg(ctx: &mut Context, st: &mut State, f: Ptr<Operation>) -> (usize, usize) {
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

pub fn run(ctx: &mut Context, st: &mut State) {
    let egg = crate::env_get("PLIRON_INSTCOMBINE").as_deref() == Some("egg");
    let (mut nc, mut nv) = (0, 0);
    let funcs: Vec<_> = st
        .funcs
        .values()
        .map(|f| f.op)
        .filter(|&f| has_body(ctx, f))
        .collect();
    for f in funcs {
        let (c, v) = if egg {
            run_fn_egg(ctx, st, f)
        } else {
            run_fn(ctx, st, f)
        };
        nc += c;
        nv += v;
        // After the worklist: divisor-shaped const exprs (`7u128.max(1)`
        // arrives as icmp+select) have folded to literals `canon_eq` can
        // match against the multiply's operand.
        nv += fold_divmul(ctx, st, f);
    }
    if tracing::enabled!(target: "pliron::stats", tracing::Level::INFO) {
        tracing::info!(target: "pliron::stats",
            "instcombine {}: {nc} values folded to constants, {nv} to an operand",
            st.cgu
        );
    }
}

/// `(x / d) * d` → `x - x % d`, LLVM InstCombine's truncating-division
/// identity — `q*d` is always exactly `x - r`, so a multiply that rebuilds
/// the quotient's product is paying for a division only to undo it. The
/// win is outsized on i128, where the quotient is a ~30-inst expansion and
/// the remainder folds at the limb level. The mul is rewritten even when
/// the quotient feeds other uses; a div that loses its last use is erased.
fn fold_divmul(ctx: &mut Context, st: &State, f: Ptr<Operation>) -> usize {
    let mut hits: Vec<(Ptr<Operation>, Value, Value, bool, Ptr<Operation>)> = Vec::new();
    for b in f.deref(ctx).get_region(0).deref(ctx).iter(ctx) {
        for op in b.deref(ctx).iter(ctx) {
            if !Operation::is_op::<MulOp>(op, ctx) {
                continue;
            }
            if int_width(ctx, op.deref(ctx).get_result(0)).is_none() {
                continue;
            }
            let opnds: Vec<Value> = op.deref(ctx).operands().collect();
            for i in 0..2 {
                let Some(dop) = opnds[i].defining_op() else {
                    continue;
                };
                let signed = Operation::is_op::<SDivOp>(dop, ctx);
                if !signed && !Operation::is_op::<UDivOp>(dop, ctx) {
                    continue;
                }
                let dopnds: Vec<Value> = dop.deref(ctx).operands().collect();
                // Constants are per-use `undef` values, so two `7`s only
                // match through `st.consts`, not `Value` identity.
                if canon_eq(st, dopnds[1], opnds[1 - i]) {
                    hits.push((op, dopnds[0], dopnds[1], signed, dop));
                    break;
                }
            }
        }
    }
    // One `rem` per distinct (x, d) pair shared by all the matching muls;
    // a quotient that loses its last use gets erased with the mul.
    let mut rems: FxHashMap<(Value, Value), Value> = FxHashMap::default();
    let mut divs: FxHashSet<Ptr<Operation>> = FxHashSet::default();
    for (op, x, d, signed, dop) in &hits {
        let rem = *rems.entry((*x, *d)).or_insert_with(|| {
            let rop = if *signed {
                SRemOp::new(ctx, *x, *d).get_operation()
            } else {
                URemOp::new(ctx, *x, *d).get_operation()
            };
            rop.insert_before(ctx, *op);
            rop.deref(ctx).get_result(0)
        });
        let sub = SubOp::new(ctx, *x, rem).get_operation();
        sub.insert_before(ctx, *op);
        let sub_r = sub.deref(ctx).get_result(0);
        let r = op.deref(ctx).get_result(0);
        r.replace_all_uses_with(ctx, &sub_r);
        Operation::erase(*op, ctx);
        divs.insert(*dop);
    }
    for dop in divs {
        if !dop.deref(ctx).get_result(0).is_used(ctx) {
            Operation::erase(dop, ctx);
        }
    }
    hits.len()
}

/// Value identity modulo the constant table — every use of a literal is a
/// distinct `undef` result sharing a `ConstVal`.
fn canon_eq(st: &State, a: Value, b: Value) -> bool {
    a == b
        || match (st.consts.get(&a), st.consts.get(&b)) {
            (Some(ConstVal::Zero), Some(ConstVal::Zero)) => true,
            (Some(ConstVal::Bits(x)), Some(ConstVal::Bits(y))) => x == y,
            (Some(ConstVal::Zero), Some(ConstVal::Bits(y)))
            | (Some(ConstVal::Bits(y)), Some(ConstVal::Zero)) => *y == 0,
            _ => false,
        }
}

// ============================ worklist peephole ============================
//
// The default engine. `egg` explored the same identities ~50x slower: it kept
// every variant node so `sub (add x y) y` could match through an `add` still
// resident in the class. A union-find over canonical values reaches the same
// fixpoint: each fold makes the result's canonical its fold target, operands
// are read through `find`, and the only patterns that must look through one
// op — the self-cancelling add/sub and xor chains — consult the defining
// op's node via `defop`.

/// A canonical value: an SSA value or a width-masked constant.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum K {
    V(Value),
    C(u128),
}

/// Union-find over `K` with path compression.
struct Uf {
    parent: FxHashMap<K, K>,
}

impl Uf {
    fn find(&mut self, k: K) -> K {
        let mut root = k;
        while let Some(&p) = self.parent.get(&root) {
            root = p;
        }
        let mut cur = k;
        while let Some(&p) = self.parent.get(&cur) {
            self.parent.insert(cur, root);
            cur = p;
        }
        root
    }
}

/// An op in the covered set, for the worklist.
struct POp {
    result: Value,
    /// Covered op kind; `nargs` of `args` are live.
    ic: Pk,
    args: [Value; 3],
    /// Each operand's integer width — `select` mixes i1 and data widths,
    /// and `icmp` operands are wider than its i1 result.
    aws: [u32; 3],
    /// Result integer width.
    rw: u32,
}

/// Covered op kinds. `Cmp` holds the canonicalized predicate (`pk_of`
/// swaps operand order for `UGT/UGE/SGT/SGE`, so the fold code sees only
/// the </<= forms).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pk {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
    Shl,
    LShr,
    AShr,
    Cmp(Ck),
    Select,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ck {
    Eq,
    Ne,
    Ult,
    Ule,
    Slt,
    Sle,
}

impl POp {
    fn nargs(&self) -> usize {
        if self.ic == Pk::Select { 3 } else { 2 }
    }
}

fn run_fn(ctx: &mut Context, st: &mut State, f: Ptr<Operation>) -> (usize, usize) {
    // Collect the covered ops in layout order.
    let mut ops: Vec<POp> = Vec::new();
    for b in f.deref(ctx).get_region(0).deref(ctx).iter(ctx) {
        for op in b.deref(ctx).iter(ctx) {
            if ops.len() > MAX_OPS || op.deref(ctx).get_num_results() != 1 {
                continue;
            }
            let r = op.deref(ctx).get_result(0);
            let Some(rw) = int_width(ctx, r) else {
                continue;
            };
            let opnds: Vec<Value> = op.deref(ctx).operands().collect();
            if !(2..=3).contains(&opnds.len()) {
                continue;
            }
            let mut aws = [rw; 3];
            for (i, &v) in opnds.iter().enumerate() {
                let Some(w) = int_width(ctx, v) else {
                    aws = [0; 3];
                    break;
                };
                aws[i] = w;
            }
            if aws[0] == 0 {
                continue;
            }
            let Some((ic, swap)) = pk_of(ctx, op, opnds.len()) else {
                continue;
            };
            let mut args = [r; 3];
            for (i, &v) in opnds.iter().enumerate() {
                args[i] = v;
            }
            if swap {
                args.swap(0, 1);
                aws.swap(0, 1);
            }
            ops.push(POp {
                result: r,
                ic,
                args,
                aws,
                rw,
            });
        }
    }
    if ops.is_empty() {
        return (0, 0);
    }

    let mut uf = Uf {
        parent: FxHashMap::default(),
    };
    // Result value -> op index, for looking through one still-live op in
    // the self-cancelling chains below.
    let defop: FxHashMap<Value, usize> =
        ops.iter().enumerate().map(|(i, o)| (o.result, i)).collect();

    // Reverse uses: canonical key -> ops reading it. A union moves the
    // list to the new root so those ops re-evaluate.
    let mut uses: FxHashMap<K, Vec<u32>> = FxHashMap::default();
    for (i, o) in ops.iter().enumerate() {
        for (j, &a) in o.args.iter().take(o.nargs()).enumerate() {
            uses.entry(canon(&mut uf, st, a, o.aws[j]))
                .or_default()
                .push(i as u32);
        }
    }

    let mut work: Vec<u32> = (0..ops.len() as u32).rev().collect();
    let mut inw = vec![true; ops.len()];
    while let Some(i) = work.pop() {
        let i = i as usize;
        inw[i] = false;
        let o = &ops[i];
        let mut a = [K::C(0); 3];
        for (j, &v) in o.args.iter().take(o.nargs()).enumerate() {
            a[j] = canon(&mut uf, st, v, o.aws[j]);
        }
        if let Some(t) = fold_op(&mut uf, st, &defop, &ops, o.ic, o.aws[0], o.rw, &a) {
            let rk = uf.find(key(st, o.result, o.rw));
            if rk != t {
                // Result canonical joins the fold target's class; ops that
                // read the old canonical re-evaluate.
                uf.parent.insert(rk, t);
                if let Some(us) = uses.remove(&rk) {
                    let l = uses.entry(t).or_default();
                    l.extend_from_slice(&us);
                    for &u in &us {
                        if !inw[u as usize] {
                            inw[u as usize] = true;
                            work.push(u);
                        }
                    }
                }
            }
        }
    }

    // Replace uses: result -> canonical constant or the class's leaf value.
    let (mut nc, mut nv) = (0, 0);
    for (i, o) in ops.iter().enumerate() {
        let r = o.result;
        if r.uses(ctx).is_empty() {
            continue;
        }
        let k = uf.find(key(st, r, o.rw));
        let new = match k {
            K::C(c) => {
                nc += 1;
                let ty = r.get_type(ctx);
                let kv = UndefOp::new(ctx, ty).get_operation();
                let kv = kv.deref(ctx).get_result(0);
                st.consts.insert(
                    kv,
                    if c == 0 {
                        ConstVal::Zero
                    } else {
                        ConstVal::Bits(c)
                    },
                );
                kv
            }
            // A `K::V` canonical may only replace results it dominates —
            // i.e. values transitively operand to this op through covered
            // defs (operand defs dominate their uses). Class unions can
            // join results whose canonicals share no dominance domain, so
            // membership in the class alone isn't proof.
            K::V(v) if v != r && safe_leaf(&ops, &defop, i, v) => {
                nv += 1;
                v
            }
            _ => continue,
        };
        r.replace_all_uses_with(ctx, &new);
    }
    (nc, nv)
}

/// Is `v` among the values that transitively fed `ops[i]`'s operands
/// through covered defs? Every such value dominates the op's def — and
/// therefore all uses of its result — so it's always a sound replacement
/// target. Bounded DFS: give up (conservatively) past a depth of operand
/// chains no real fold needs.
fn safe_leaf(ops: &[POp], defop: &FxHashMap<Value, usize>, i: usize, v: Value) -> bool {
    let mut seen = FxHashSet::default();
    let mut stack: Vec<Value> = ops[i].args[..ops[i].nargs()].to_vec();
    while let Some(a) = stack.pop() {
        if a == v {
            return true;
        }
        if !seen.insert(a) || seen.len() > 256 {
            return false;
        }
        if let Some(&d) = defop.get(&a) {
            stack.extend(ops[d].args[..ops[d].nargs()].iter().copied());
        }
    }
    false
}

/// Canonical key for a value: `st.consts` constants key by masked value,
/// everything else by SSA name.
fn key(st: &State, v: Value, w: u32) -> K {
    match st.consts.get(&v) {
        Some(ConstVal::Bits(c)) => K::C(mask(*c, w)),
        Some(ConstVal::Zero) => K::C(0),
        _ => K::V(v),
    }
}

fn canon(uf: &mut Uf, st: &State, v: Value, w: u32) -> K {
    uf.find(key(st, v, w))
}

/// The fold rules on canonical operands — the same set the egg `rules()`
/// covered, except `sel-not` (the e-graph's node-shuffling made it cheap;
/// here it would need a `seen` table for a rare rewrite). Returns the
/// canonical the result should join. `aw` is the shared operand width
/// (icmp compares at operand width but folds to i1).
fn fold_op(
    uf: &mut Uf,
    st: &State,
    defop: &FxHashMap<Value, usize>,
    ops: &[POp],
    ic: Pk,
    aw: u32,
    rw: u32,
    a: &[K; 3],
) -> Option<K> {
    let c = |v: u128| Some(K::C(mask(v, rw)));
    let (x, y) = (a[0], a[1]);
    // Look through a still-live defining op for the self-cancelling
    // chains: if `k`'s producer is itself a covered op, yield its kind
    // and canonical operands.
    let mut inner = |k: K| -> Option<(Pk, [K; 2])> {
        let K::V(v) = k else { return None };
        let d = &ops[*defop.get(&v)?];
        Some((
            d.ic,
            [
                canon(uf, st, d.args[0], d.aws[0]),
                canon(uf, st, d.args[1], d.aws[1]),
            ],
        ))
    };
    match ic {
        Pk::Add => match (x, y) {
            (K::C(v), K::C(w)) => c(v.wrapping_add(w)),
            (K::C(0), o) | (o, K::C(0)) => Some(o),
            _ => {
                // (add (sub p q) q) -> p, on either operand position.
                for (i, o) in [(x, y), (y, x)] {
                    if let Some((Pk::Sub, [p, q])) = inner(i) {
                        if q == o {
                            return Some(p);
                        }
                    }
                }
                None
            }
        },
        Pk::Sub => match (x, y) {
            (K::C(v), K::C(w)) => c(v.wrapping_sub(w)),
            (o, K::C(0)) => Some(o),
            _ if x == y => c(0),
            _ => {
                // (sub (add p q) q) -> p and (sub (add p q) p) -> q.
                if let Some((Pk::Add, [p, q])) = inner(x) {
                    if q == y {
                        return Some(p);
                    }
                    if p == y {
                        return Some(q);
                    }
                }
                None
            }
        },
        Pk::Mul => match (x, y) {
            (K::C(v), K::C(w)) => c(v.wrapping_mul(w)),
            (K::C(1), o) | (o, K::C(1)) => Some(o),
            _ => None,
        },
        Pk::And => match (x, y) {
            (K::C(v), K::C(w)) => c(v & w),
            _ if x == y => Some(x),
            (K::C(v), o) | (o, K::C(v)) if v == mask(!0, aw) => Some(o),
            _ => None,
        },
        Pk::Or => match (x, y) {
            (K::C(v), K::C(w)) => c(v | w),
            _ if x == y => Some(x),
            (K::C(0), o) | (o, K::C(0)) => Some(o),
            _ => None,
        },
        Pk::Xor => match (x, y) {
            (K::C(v), K::C(w)) => c(v ^ w),
            (K::C(0), o) | (o, K::C(0)) => Some(o),
            _ if x == y => c(0),
            _ => {
                // (xor (xor p q) q) -> p, on either operand position and
                // either inner operand (`xor` is commutative).
                for (i, o) in [(x, y), (y, x)] {
                    if let Some((Pk::Xor, [p, q])) = inner(i) {
                        if q == o {
                            return Some(p);
                        }
                        if p == o {
                            return Some(q);
                        }
                    }
                }
                None
            }
        },
        // Out-of-range shift amounts are poison in LLVM; the Cranelift
        // lowering wraps them, so the folds do too.
        Pk::Shl => match (x, y) {
            (K::C(v), K::C(w)) => c(v << (w % aw as u128)),
            (o, K::C(0)) => Some(o),
            _ => None,
        },
        Pk::LShr => match (x, y) {
            (K::C(v), K::C(w)) => c(v >> (w % aw as u128)),
            (o, K::C(0)) => Some(o),
            _ => None,
        },
        Pk::AShr => match (x, y) {
            (K::C(v), K::C(w)) => c((sext(v, aw) >> (w % aw as u128)) as u128),
            (o, K::C(0)) => Some(o),
            _ => None,
        },
        Pk::Cmp(p) => {
            let (tr, fl) = (c(1), c(0));
            match p {
                Ck::Eq => match (x, y) {
                    (K::C(v), K::C(w)) => c((v == w) as u128),
                    _ if x == y => tr,
                    _ => None,
                },
                Ck::Ne => match (x, y) {
                    (K::C(v), K::C(w)) => c((v != w) as u128),
                    _ if x == y => fl,
                    (K::C(0), o) | (o, K::C(0)) if aw == 1 => Some(o),
                    _ => None,
                },
                Ck::Ult => match (x, y) {
                    (K::C(v), K::C(w)) => c((v < w) as u128),
                    _ if x == y => fl,
                    (_, K::C(0)) => fl,
                    _ => None,
                },
                Ck::Ule => match (x, y) {
                    (K::C(v), K::C(w)) => c((v <= w) as u128),
                    _ if x == y => tr,
                    _ => None,
                },
                Ck::Slt => match (x, y) {
                    (K::C(v), K::C(w)) => c((sext(v, aw) < sext(w, aw)) as u128),
                    _ if x == y => fl,
                    _ => None,
                },
                Ck::Sle => match (x, y) {
                    (K::C(v), K::C(w)) => c((sext(v, aw) <= sext(w, aw)) as u128),
                    _ if x == y => tr,
                    _ => None,
                },
            }
        }
        Pk::Select => match a[0] {
            K::C(0) => Some(a[2]),
            K::C(_) => Some(a[1]),
            _ if a[1] == a[2] => Some(a[1]),
            _ => None,
        },
    }
}

/// Covered-op kind plus whether operands were reordered to canonicalize a
/// `>/>=` predicate into its `</<=` form.
fn pk_of(ctx: &Context, op: Ptr<Operation>, nargs: usize) -> Option<(Pk, bool)> {
    let (pk, swap) = if Operation::is_op::<AddOp>(op, ctx) {
        (Pk::Add, false)
    } else if Operation::is_op::<SubOp>(op, ctx) {
        (Pk::Sub, false)
    } else if Operation::is_op::<MulOp>(op, ctx) {
        (Pk::Mul, false)
    } else if Operation::is_op::<AndOp>(op, ctx) {
        (Pk::And, false)
    } else if Operation::is_op::<OrOp>(op, ctx) {
        (Pk::Or, false)
    } else if Operation::is_op::<XorOp>(op, ctx) {
        (Pk::Xor, false)
    } else if Operation::is_op::<ShlOp>(op, ctx) {
        (Pk::Shl, false)
    } else if Operation::is_op::<LShrOp>(op, ctx) {
        (Pk::LShr, false)
    } else if Operation::is_op::<AShrOp>(op, ctx) {
        (Pk::AShr, false)
    } else if Operation::is_op::<SelectOp>(op, ctx) {
        (Pk::Select, false)
    } else if let Some(c) = Operation::get_op::<ICmpOp>(op, ctx) {
        match c.predicate(ctx) {
            P::EQ => (Pk::Cmp(Ck::Eq), false),
            P::NE => (Pk::Cmp(Ck::Ne), false),
            P::ULT => (Pk::Cmp(Ck::Ult), false),
            P::UGT => (Pk::Cmp(Ck::Ult), true),
            P::ULE => (Pk::Cmp(Ck::Ule), false),
            P::UGE => (Pk::Cmp(Ck::Ule), true),
            P::SLT => (Pk::Cmp(Ck::Slt), false),
            P::SGT => (Pk::Cmp(Ck::Slt), true),
            P::SLE => (Pk::Cmp(Ck::Sle), false),
            P::SGE => (Pk::Cmp(Ck::Sle), true),
        }
    } else {
        return None;
    };
    let want = if pk == Pk::Select { 3 } else { 2 };
    (nargs == want).then_some((pk, swap))
}
