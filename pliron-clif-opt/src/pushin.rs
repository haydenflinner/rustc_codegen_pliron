//! Conditional push-in (`PLIRON_PUSHIN`, opt-in).
//!
//! eggcc's `push-in` rule recomputes an `if`-region input of the form
//! `bop(const, x)` inside each arm, where the arm's context — the
//! branch condition — decides it. The CFG form: `brif (icmp eq x, k)`
//! proves `x == k` on the taken edge, so inside the true arm every use
//! of `x` may read `k` (the false arm of `ne` mirrors it, `band`/`bor`
//! conjunctions seed each leaf, and a boolean chain value is a known
//! `iconst` inside the arm that decides it).
//!
//! Two moves per arm:
//!   * rewrite in-region operands naming a fact LHS to the known value
//!     (`add x, 7` in the true arm becomes `add k, 7`, which the
//!     constant sweeps then fold), and
//!   * clone a single-result pure predecessor inst into the arm head
//!     when one of its operands specializes — the `bop(const, x)` half
//!     of the rule — so uses in the region see the folded copy.
//!
//! `celim`/`domcond` already fold *queries* (icmp results) under edge
//! facts; this substitutes operands, so arithmetic and edge args on
//! deeper blocks specialize too.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, Function, Inst, InstructionData, Opcode, Type, Value, ValueDef,
};
use cranelift_codegen::ir::condcodes::IntCC;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::jumpthread::pure_op;
use crate::loopidiom::iconst;

/// Region insts rewritten per arm.
const MAX_REGION: usize = 512;
/// Specialized clones pushed per diamond.
const MAX_PUSH: usize = 8;
/// Diamonds inspected per function.
const MAX_DIAMONDS: usize = 256;

/// `v` provably takes only 0/1 (an `icmp`, a select/extend/logic op
/// over 0/1 values, or a literal). Depth-bounded.
fn is_bool(func: &Function, v: Value, depth: u32) -> bool {
    let v = func.dfg.resolve_aliases(v);
    if depth == 0 {
        return false;
    }
    if let Some(k) = iconst(func, v) {
        return k == 0 || k == 1;
    }
    let ValueDef::Result(i, _) = func.dfg.value_def(v) else {
        return false;
    };
    match func.dfg.insts[i] {
        InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            ..
        } => true,
        InstructionData::Binary {
            opcode: Opcode::Band | Opcode::Bor | Opcode::Bxor,
            args,
        } => args
            .iter()
            .all(|&a| is_bool(func, a, depth - 1)),
        InstructionData::Unary {
            opcode: Opcode::Ireduce | Opcode::Uextend | Opcode::Sextend,
            arg,
        } => is_bool(func, arg, depth - 1),
        InstructionData::Ternary {
            opcode: Opcode::Select,
            args,
        } => is_bool(func, args[1], depth - 1) && is_bool(func, args[2], depth - 1),
        _ => false,
    }
}

/// Decompose a `brif`/`select` condition known to equal `truth` into
/// the leaf values that fixes: the true edge of `band a, b` proves
/// both, the false edge of `bor a, b` proves both, `bxor a, 1` and
/// int-extends are transparent. Everything else is a leaf.
fn leaves(func: &Function, c: Value, truth: bool, out: &mut Vec<(Value, bool)>) {
    fn rec(func: &Function, v: Value, truth: bool, out: &mut Vec<(Value, bool)>, depth: u32) {
        let v = func.dfg.resolve_aliases(v);
        if depth > 0 {
            if let ValueDef::Result(i, _) = func.dfg.value_def(v) {
                match func.dfg.insts[i] {
                    InstructionData::Binary {
                        opcode: Opcode::Band,
                        args,
                    } if truth => {
                        rec(func, args[0], true, out, depth - 1);
                        rec(func, args[1], true, out, depth - 1);
                        return;
                    }
                    InstructionData::Binary {
                        opcode: Opcode::Bor,
                        args,
                    } if !truth => {
                        rec(func, args[0], false, out, depth - 1);
                        rec(func, args[1], false, out, depth - 1);
                        return;
                    }
                    InstructionData::Binary {
                        opcode: Opcode::Bxor,
                        args,
                    } => {
                        let (a, b) = (args[0], args[1]);
                        if iconst(func, a) == Some(1) {
                            rec(func, b, !truth, out, depth - 1);
                            return;
                        }
                        if iconst(func, b) == Some(1) {
                            rec(func, a, !truth, out, depth - 1);
                            return;
                        }
                    }
                    InstructionData::Unary {
                        opcode: Opcode::Ireduce | Opcode::Uextend | Opcode::Sextend,
                        arg,
                    } => {
                        rec(func, arg, truth, out, depth - 1);
                        return;
                    }
                    _ => {}
                }
            }
        }
        out.push((v, truth));
    }
    rec(func, c, truth, out, 8);
}

/// The substitutions a leaf fixed to `truth` contributes: `x -> k`
/// pairs go in `subs` (resolved-value keyed); boolean leaves decided on
/// the edge go in `bools` for the caller to materialize as `iconst`s.
fn leaf_subs(
    func: &Function,
    l: Value,
    truth: bool,
    subs: &mut FxHashMap<Value, Value>,
    bools: &mut Vec<(Value, bool)>,
) {
    if let ValueDef::Result(i, _) = func.dfg.value_def(l)
        && let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args,
        } = func.dfg.insts[i]
    {
        // `eq` true / `ne` false pins x == y; with one side an
        // iconst the other becomes it inside the arm.
        let eq = (cond == IntCC::Equal) == truth;
        if eq {
            for (x, y) in [(args[0], args[1]), (args[1], args[0])] {
                let x = func.dfg.resolve_aliases(x);
                let y = func.dfg.resolve_aliases(y);
                if iconst(func, y).is_some()
                    && !matches!(func.dfg.value_def(x), ValueDef::Result(d, _) if func.dfg.insts[d].opcode() == Opcode::Iconst)
                {
                    subs.entry(x).or_insert(y);
                }
            }
        }
    }
    // A boolean leaf's own value is decided on the edge — `flag` reads
    // as a literal inside the arm.
    if is_bool(func, l, 4) {
        bools.push((l, truth));
    }
}

/// `iconst ty, k` at the head of `a`, memoized per (ty, k).
fn mk_const(
    func: &mut Function,
    cache: &mut FxHashMap<(Type, i64), Value>,
    a: Block,
    ty: Type,
    k: i64,
) -> Value {
    *cache.entry((ty, k)).or_insert_with(|| {
        let ni = func.dfg.make_inst(InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm: k.into(),
        });
        func.dfg.make_inst_results(ni, ty);
        match func.layout.first_inst(a) {
            Some(f) => func.layout.insert_inst(ni, f),
            None => func.layout.append_inst(ni, a),
        }
        func.dfg.first_result(ni)
    })
}

/// `w` as a candidate substitution target inside arm `a`: resolve
/// aliases; a param of `a` denotes its predecessor-edge arg.
fn preimage(
    func: &Function,
    a: Block,
    pmap: &FxHashMap<Value, Value>,
    w: Value,
) -> Value {
    let v = func.dfg.resolve_aliases(w);
    if let ValueDef::Param(pb, _) = func.dfg.value_def(v)
        && pb == a
        && let Some(&e) = pmap.get(&v)
    {
        return func.dfg.resolve_aliases(e);
    }
    v
}

/// One arm of a `brif`: rewrite region operands per `subs`, and clone
/// specializing predecessor insts into the arm head. Returns the
/// number of operand rewrites plus pushed insts.
fn specialize_arm(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    b: Block,
    a: Block,
    bc_args: &[Value],
    subs: &FxHashMap<Value, Value>,
) -> usize {
    // Single-pred only — the edge fact must hold on every way in.
    if a == b
        || !cfg
            .pred_iter(a)
            .map(|p| p.block)
            .eq(std::iter::once(b))
    {
        return 0;
    }
    let pmap: FxHashMap<Value, Value> = func
        .dfg
        .block_params(a)
        .iter()
        .copied()
        .zip(bc_args.iter().copied())
        .collect();
    // The region the arm dominates — the fact's scope.
    let mut region: Vec<Block> = func
        .layout
        .blocks()
        .filter(|&u| dt.try_dominates(a, u, &func.layout))
        .collect();
    let mut ninst = 0usize;
    region.retain(|&u| {
        ninst += func.layout.block_insts(u).count();
        ninst <= MAX_REGION
    });
    if region.is_empty() {
        return 0;
    }
    let inset: FxHashSet<Block> = region.iter().copied().collect();
    let mut n = 0;

    // (b) push: a pure inst in `b` whose operand is pinned by the edge
    // fact recomputes inside the arm, where the pinned operand is a
    // constant — `min(x, 5)` in the `x > 5` arm's image. Only when the
    // arm's region actually reads the result.
    let mut pushes: Vec<(Inst, Vec<Value>)> = Vec::new();
    for i in func.layout.block_insts(b) {
        if !pure_op(func, i) || func.dfg.inst_results(i).len() != 1 {
            continue;
        }
        let spec = func.dfg.inst_args(i).iter().any(|&w| {
            subs.contains_key(&preimage(func, a, &pmap, w))
        });
        if !spec {
            continue;
        }
        let v = func.dfg.resolve_aliases(func.dfg.inst_results(i)[0]);
        // Any in-region use of `v` (operand positions; an `a`-param
        // bound to `v` counts — its in-region uses retarget too).
        let used = inset.iter().any(|&u| {
            func.layout.block_insts(u).any(|ui| {
                func.dfg
                    .inst_values(ui)
                    .any(|w| func.dfg.resolve_aliases(w) == v)
            })
        }) || func
            .dfg
            .block_params(a)
            .iter()
            .zip(bc_args.iter())
            .any(|(&p, &e)| func.dfg.resolve_aliases(e) == v && {
                inset.iter().any(|&u| {
                    func.layout.block_insts(u).any(|ui| {
                        func.dfg
                            .inst_values(ui)
                            .any(|w| func.dfg.resolve_aliases(w) == p)
                    })
                })
            });
        if !used {
            continue;
        }
        let args: Vec<Value> = func
            .dfg
            .inst_args(i)
            .iter()
            .map(|&w| {
                let p = preimage(func, a, &pmap, w);
                *subs.get(&p).unwrap_or(&p)
            })
            .collect();
        pushes.push((i, args));
        if pushes.len() >= MAX_PUSH {
            break;
        }
    }
    for (i, args) in pushes {
        let ni = func.dfg.clone_inst(i);
        let mut data = func.dfg.insts[ni];
        let mut it = args.iter();
        {
            let dfg = &mut func.dfg;
            data.map_values(
                &mut dfg.value_lists,
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
                |w| it.next().copied().unwrap_or(w),
            );
        }
        func.dfg.insts[ni] = data;
        match func.layout.first_inst(a) {
            Some(f) => func.layout.insert_inst(ni, f),
            None => func.layout.append_inst(ni, a),
        }
        let nv = func.dfg.first_result(ni);
        let v = func.dfg.resolve_aliases(func.dfg.inst_results(i)[0]);
        // In-region uses of `v` — and of any `a`-param bound to `v` —
        // now read the specialized copy.
        let psubs: Vec<Value> = func
            .dfg
            .block_params(a)
            .iter()
            .zip(bc_args.iter())
            .filter(|&(_, e)| func.dfg.resolve_aliases(*e) == v)
            .map(|(&p, _)| p)
            .collect();
        for &u in &region {
            for ui in func.layout.block_insts(u).collect::<Vec<_>>() {
                if ui == ni {
                    continue;
                }
                func.dfg.map_inst_values(ui, |w| {
                    let r = func.dfg.resolve_aliases(w);
                    if r == v || psubs.contains(&r) {
                        nv
                    } else {
                        w
                    }
                });
            }
        }
        n += 1;
    }

    // (a) blanket operand substitution over the region.
    for &u in &region {
        for ui in func.layout.block_insts(u).collect::<Vec<_>>() {
            let mut hit = false;
            func.dfg.map_inst_values(ui, |w| {
                let p = preimage(func, a, &pmap, w);
                if let Some(&k) = subs.get(&p)
                    && func.dfg.value_type(k) == func.dfg.value_type(w)
                {
                    hit = true;
                    return k;
                }
                w
            });
            n += hit as usize;
        }
    }
    n
}

pub fn run(func: &mut Function, an: &mut crate::pipeline::Analysis) -> usize {
    let (cfg, dt) = an.cfg_dt(func);
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut n = 0;
    'outer: for b in blocks {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        let InstructionData::Brif { arg, blocks: bs, .. } = func.dfg.insts[t] else {
            continue;
        };
        let (bt, be) = (
            bs[0].block(&func.dfg.value_lists),
            bs[1].block(&func.dfg.value_lists),
        );
        if bt == be {
            continue;
        }
        let cond = func.dfg.resolve_aliases(arg);
        // Leaf conditions decided per arm.
        let mut tl = Vec::new();
        let mut el = Vec::new();
        leaves(func, cond, true, &mut tl);
        leaves(func, cond, false, &mut el);
        if tl.is_empty() && el.is_empty() {
            continue;
        }
        for (arm, lvs) in [(bt, &tl), (be, &el)] {
            if lvs.is_empty() {
                continue;
            }
            let mut subs: FxHashMap<Value, Value> = FxHashMap::default();
            let mut bools: Vec<(Value, bool)> = Vec::new();
            for &(l, truth) in lvs {
                leaf_subs(func, l, truth, &mut subs, &mut bools);
            }
            let mut kcache: FxHashMap<(Type, i64), Value> = FxHashMap::default();
            for (l, truth) in bools {
                let ty = func.dfg.value_type(l);
                let k = mk_const(func, &mut kcache, arm, ty, truth as i64);
                subs.entry(l).or_insert(k);
            }
            if subs.is_empty() {
                continue;
            }
            let slot = if arm == bt { 0 } else { 1 };
            let bc_args: Vec<Value> = bs[slot]
                .args(&func.dfg.value_lists)
                .filter_map(|a| a.as_value())
                .collect();
            n += specialize_arm(func, cfg, dt, b, arm, &bc_args, &subs);
            if n > MAX_DIAMONDS * MAX_PUSH {
                break 'outer;
            }
        }
        if n / MAX_PUSH >= MAX_DIAMONDS {
            break;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::condcodes::IntCC;
    use cranelift_codegen::ir::types::{I32, I64};
    use cranelift_codegen::ir::{
        AbiParam, BlockArg, Function, InstBuilder, Opcode, Signature, UserFuncName,
    };
    use cranelift_codegen::isa::CallConv;

    fn verify(func: &Function) {
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        cranelift_codegen::verify_function(func, &flags).unwrap();
    }

    fn cfg_dt(func: &Function) -> (ControlFlowGraph, DominatorTree) {
        let c = ControlFlowGraph::with_function(func);
        (c, DominatorTree::with_function(func, &ControlFlowGraph::with_function(func)))
    }

    /// `brif (icmp eq x, 5)` — inside the true arm `x` reads as `5`,
    /// and the predecessor's `x * 4` pushes in as `5 * 4`.
    #[test]
    fn specializes_true_arm() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b = func.dfg.make_block();
        let t = func.dfg.make_block();
        let e = func.dfg.make_block();
        for bl in [b, t, e] {
            func.layout.append_block(bl);
        }
        let x = func.dfg.append_block_param(b, I64);
        let (m, five);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(b);
            five = cur.ins().iconst(I64, 5);
            let cc = cur.ins().icmp(IntCC::Equal, x, five);
            let two = cur.ins().iconst(I64, 2);
            m = cur.ins().ishl(x, two);
            cur.ins().brif(cc, t, &[], e, &[]);
        }
        {
            // t: r = m + x — both operands specialize inside the arm.
            let mut cur = FuncCursor::new(&mut func).at_bottom(t);
            let r = cur.ins().iadd(m, x);
            cur.ins().return_(&[r]);
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(e);
            cur.ins().return_(&[x]);
        }
        verify(&func);
        let mut an = crate::pipeline::Analysis::default();
        let n = run(&mut func, &mut an);
        verify(&func);
        assert!(n >= 2, "expected operand subst + push, got {n}\n{}", func.display());
        // The pushed ishl in t reads the const directly.
        let tshl = func
            .layout
            .block_insts(t)
            .find(|&i| func.dfg.insts[i].opcode() == Opcode::Ishl);
        if let Some(i) = tshl {
            let args = func.dfg.inst_args(i).to_vec();
            assert_eq!(func.dfg.resolve_aliases(args[0]), five, "{}", func.display());
        }
        // The arm's own iadd now reads the const for x's slot.
        let iadd = func
            .layout
            .block_insts(t)
            .find(|&i| func.dfg.insts[i].opcode() == Opcode::Iadd)
            .unwrap();
        assert!(
            func.dfg
                .inst_args(iadd)
                .iter()
                .any(|&a| func.dfg.resolve_aliases(a) == five),
            "{}",
            func.display()
        );
    }

    /// The else arm of `ne` proves the equality.
    #[test]
    fn specializes_else_arm() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b = func.dfg.make_block();
        let t = func.dfg.make_block();
        let e = func.dfg.make_block();
        for bl in [b, t, e] {
            func.layout.append_block(bl);
        }
        let x = func.dfg.append_block_param(b, I64);
        let seven;
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(b);
            seven = cur.ins().iconst(I64, 7);
            let cc = cur.ins().icmp(IntCC::NotEqual, x, seven);
            cur.ins().brif(cc, t, &[], e, &[]);
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(t);
            cur.ins().return_(&[x]);
        }
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(e);
            let r = cur.ins().isub(x, seven);
            cur.ins().return_(&[r]);
        }
        verify(&func);
        let mut an = crate::pipeline::Analysis::default();
        let n = run(&mut func, &mut an);
        verify(&func);
        assert!(n >= 1, "{}\n{}", n, func.display());
        let sub = func
            .layout
            .block_insts(e)
            .find(|&i| func.dfg.insts[i].opcode() == Opcode::Isub)
            .unwrap();
        assert!(
            func.dfg
                .inst_args(sub)
                .iter()
                .all(|&a| func.dfg.resolve_aliases(a) == seven),
            "{}",
            func.display()
        );
    }
}
