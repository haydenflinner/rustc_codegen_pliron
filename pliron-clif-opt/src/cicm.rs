//! Conditional invariant code motion (`PLIRON_CICM`, opt-in).
//!
//! When both arms of a `brif` unconditionally compute the same pure
//! inst, the value exists on every path through the diamond: hoist one
//! copy into the predecessor and alias both arm results to it (LLVM
//! GVNHoist; eggcc's `cicm` ruleset does the same by extracting `if`
//! inputs' shared terms into the outer context). It is not speculation
//! — the inst executed on whichever arm ran — so the hoisted copy never
//! adds dynamic work; it deduplicates the arms and shrinks them toward
//! `armmerge`'s isomorphic shape. Pairs chain: once `iadd x, y` is
//! co-hoisted, a `icmp`/`select` on the pair's results matches next
//! round and follows it out.
//!
//! Arms must be single-predecessor — a candidate then provably executes
//! whenever that side runs — and limited to `pure_op`s: a hoisted inst
//! moves ahead of any arm-internal side effects, which is only
//! unobservable when it cannot trap.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::ir::{Block, Function, Inst, InstructionData, Value, ValueDef};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::jumpthread::pure_op;

/// Insts scanned per arm.
const MAX_ARM: usize = 48;
/// Pairs hoisted per diamond.
const MAX_HOIST: usize = 16;
/// Pairing rounds per diamond — each round can only add hmap entries,
/// unlocking deeper chains (add -> cmp -> select tops out quickly).
const MAX_ROUNDS: usize = 8;

/// Where a resolved arm operand lands: `Out` = a value available at the
/// predecessor's terminator, `Pair` = the result of hoisted pair `k`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum R {
    Out(Value),
    Pair(u32),
}

/// Incoming edge count per block.
fn pred_counts(func: &Function) -> FxHashMap<Block, u32> {
    let mut n: FxHashMap<Block, u32> = FxHashMap::default();
    for b in func.layout.blocks() {
        let Some(t) = func.layout.last_inst(b) else {
            continue;
        };
        for bc in func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        {
            *n.entry(bc.block(&func.dfg.value_lists)).or_default() += 1;
        }
    }
    n
}

/// `v` usable as an operand of an inst hoisted to just before `b`'s
/// terminator `ti`: defined in `b` (any inst there precedes `ti`) or in
/// a block dominating it.
fn avail(func: &Function, dt: &DominatorTree, b: Block, ti: Inst, v: Value) -> bool {
    match func.dfg.resolved_value_def(v) {
        ValueDef::Param(pb, _) => pb == b || dt.try_dominates(pb, b, &func.layout),
        ValueDef::Result(i, _) => dt.try_dominates(i, ti, &func.layout),
        _ => false,
    }
}

/// Opcode plus non-value fields identical. Ports
/// `armmerge::same_shape` for the `pure_op` instruction shapes only;
/// anything else bails conservatively.
fn same_inst(t: &InstructionData, e: &InstructionData) -> bool {
    use InstructionData as D;
    if t.opcode() != e.opcode() {
        return false;
    }
    match (t, e) {
        (D::Binary { .. }, D::Binary { .. })
        | (D::Unary { .. }, D::Unary { .. })
        | (D::Ternary { .. }, D::Ternary { .. }) => true,
        (D::UnaryImm { imm: a, .. }, D::UnaryImm { imm: b, .. }) => a.bits() == b.bits(),
        (D::IntCompare { cond: a, .. }, D::IntCompare { cond: b, .. }) => a == b,
        (
            D::StackAddr {
                stack_slot: a,
                offset: oa,
                ..
            },
            D::StackAddr {
                stack_slot: b,
                offset: ob,
                ..
            },
        ) => a == b && oa == ob,
        _ => false,
    }
}

/// Resolve operand `v` as seen from inside `arm`: aliases, then the
/// param -> predecessor-edge-arg map, then a hoisted-pair result.
fn rsub(
    func: &Function,
    psub: &FxHashMap<Value, Value>,
    hmap: &FxHashMap<Value, u32>,
    arm: Block,
    v: Value,
) -> R {
    let mut v = func.dfg.resolve_aliases(v);
    if let ValueDef::Param(pb, _) = func.dfg.value_def(v)
        && pb == arm
        && let Some(&a) = psub.get(&v)
    {
        v = func.dfg.resolve_aliases(a);
    }
    match hmap.get(&v) {
        Some(&k) => R::Pair(k),
        None => R::Out(v),
    }
}

/// `i`'s operands as seen from `arm`, or `None` when any operand fails
/// to reach an available value or a hoisted pair (an unpaired arm-local
/// def can never match — its counterpart would already be in `hmap`).
fn resolved_args(
    func: &Function,
    dt: &DominatorTree,
    b: Block,
    ti: Inst,
    psub: &FxHashMap<Value, Value>,
    hmap: &FxHashMap<Value, u32>,
    arm: Block,
    i: Inst,
) -> Option<Vec<R>> {
    func.dfg
        .inst_args(i)
        .iter()
        .map(|&w| match rsub(func, psub, hmap, arm, w) {
            R::Pair(k) => Some(R::Pair(k)),
            R::Out(v) if avail(func, dt, b, ti, v) => Some(R::Out(v)),
            R::Out(_) => None,
        })
        .collect()
}

/// All co-hoistable inst pairs of `brif`-diamond `b` -> `(t, e)`, in `t`
/// scan order (emission order: a pair's operands only reference earlier
/// pairs — `t` is SSA, so its defs precede its uses).
fn plan(
    func: &Function,
    dt: &DominatorTree,
    preds: &FxHashMap<Block, u32>,
    b: Block,
) -> Option<Vec<(Inst, Inst, Vec<R>)>> {
    let ti = func.layout.last_inst(b)?;
    let InstructionData::Brif { blocks, .. } = func.dfg.insts[ti] else {
        return None;
    };
    let t = blocks[0].block(&func.dfg.value_lists);
    let e = blocks[1].block(&func.dfg.value_lists);
    if t == e || t == b || e == b {
        return None;
    }
    if func.layout.is_cold(t) != func.layout.is_cold(e) {
        return None;
    }
    // Single-pred each: a candidate's insts run iff its edge was taken,
    // so work executed on every path moves to `b` for free.
    if preds.get(&t).copied().unwrap_or(0) != 1 || preds.get(&e).copied().unwrap_or(0) != 1 {
        return None;
    }
    let edge_args = |slot: usize| -> Vec<Value> {
        blocks[slot]
            .args(&func.dfg.value_lists)
            .filter_map(|a| a.as_value())
            .collect()
    };
    let targs = edge_args(0);
    let eargs = edge_args(1);
    let psub_t: FxHashMap<Value, Value> = func
        .dfg
        .block_params(t)
        .iter()
        .copied()
        .zip(targs.iter().copied())
        .collect();
    let psub_e: FxHashMap<Value, Value> = func
        .dfg
        .block_params(e)
        .iter()
        .copied()
        .zip(eargs.iter().copied())
        .collect();
    let it_list: Vec<Inst> = func
        .layout
        .block_insts(t)
        .take(MAX_ARM + 1)
        .filter(|&i| !func.dfg.insts[i].opcode().is_terminator())
        .collect();
    let ie_list: Vec<Inst> = func
        .layout
        .block_insts(e)
        .take(MAX_ARM + 1)
        .filter(|&i| !func.dfg.insts[i].opcode().is_terminator())
        .collect();
    if it_list.is_empty() || ie_list.is_empty() {
        return None;
    }
    let mut hmap: FxHashMap<Value, u32> = FxHashMap::default();
    let mut pairs: Vec<(Inst, Inst, Vec<R>)> = Vec::new();
    let mut used_t: FxHashSet<Inst> = FxHashSet::default();
    let mut used_e: FxHashSet<Inst> = FxHashSet::default();
    // One resolution pass over both arms, then match T-order against E.
    // New pairs seed `hmap`, so repeat until no match (bounded — each
    // round adds a pair or stops).
    for _ in 0..MAX_ROUNDS {
        if pairs.len() >= MAX_HOIST {
            break;
        }
        let cand = |list: &[Inst], used: &FxHashSet<Inst>, psub: &FxHashMap<Value, Value>, arm| {
            let mut out: Vec<(Inst, Vec<R>)> = Vec::new();
            for &i in list {
                if used.contains(&i) || !pure_op(func, i) {
                    continue;
                }
                if let Some(args) =
                    resolved_args(func, dt, b, ti, psub, &hmap, arm, i)
                {
                    out.push((i, args));
                }
            }
            out
        };
        let ct = cand(&it_list, &used_t, &psub_t, t);
        let ce = cand(&ie_list, &used_e, &psub_e, e);
        let mut found = false;
        't: for (it, at) in &ct {
            for (ie, ae) in &ce {
                // Same opcode/fields and resolved operands — and same
                // result type (`uextend.i64 x` vs `uextend.i32 x` share
                // an operand but differ).
                if at.len() != ae.len()
                    || !same_inst(&func.dfg.insts[*it], &func.dfg.insts[*ie])
                    || *at != *ae
                    || func.dfg.value_type(func.dfg.inst_results(*it)[0])
                        != func.dfg.value_type(func.dfg.inst_results(*ie)[0])
                {
                    continue;
                }
                let k = pairs.len() as u32;
                for &r in func.dfg.inst_results(*it)
                    .iter()
                    .chain(func.dfg.inst_results(*ie).iter())
                {
                    hmap.insert(func.dfg.resolve_aliases(r), k);
                }
                pairs.push((*it, *ie, at.clone()));
                used_t.insert(*it);
                used_e.insert(*ie);
                found = true;
                break 't;
            }
        }
        if !found {
            break;
        }
    }
    if pairs.is_empty() { None } else { Some(pairs) }
}

fn apply(func: &mut Function, b: Block, pairs: &[(Inst, Inst, Vec<R>)]) {
    let ti = func.layout.last_inst(b).unwrap();
    let mut hoisted: Vec<Value> = Vec::with_capacity(pairs.len());
    for (it, _, args) in pairs {
        let ni = func.dfg.clone_inst(*it);
        let mut data = func.dfg.insts[ni];
        let mut it2 = args.iter();
        {
            let dfg = &mut func.dfg;
            data.map_values(
                &mut dfg.value_lists,
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
                |v| match it2.next() {
                    Some(R::Out(x)) => *x,
                    Some(R::Pair(j)) => hoisted[*j as usize],
                    None => v,
                },
            );
        }
        func.dfg.insts[ni] = data;
        func.layout.insert_inst(ni, ti);
        hoisted.push(func.dfg.first_result(ni));
    }
    for (k, (it, ie, _)) in pairs.iter().enumerate() {
        let vh = hoisted[k];
        for i in [*it, *ie] {
            let rs: Vec<Value> = func.dfg.inst_results(i).to_vec();
            func.dfg.detach_inst_results(i);
            for r in rs {
                func.dfg.change_to_alias(r, vh);
            }
            func.layout.remove_inst(i);
        }
    }
}

pub fn run(func: &mut Function, an: &mut crate::pipeline::Analysis) -> usize {
    let preds = pred_counts(func);
    let mut n = 0;
    // Snapshot the block list: `apply` mutates inst lists but not the
    // block set or edges, so one `pred_counts`/`dt` stays valid — the
    // hoisted values only add defs inside already-present blocks.
    let dt_blocks: Vec<Block> = func.layout.blocks().collect();
    let (_, dt) = an.cfg_dt(func);
    for b in dt_blocks {
        let Some(pairs) = plan(func, dt, &preds, b) else {
            continue;
        };
        n += pairs.len();
        apply(func, b, &pairs);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::types::{I32, I64};
    use cranelift_codegen::ir::{
        AbiParam, BlockArg, Function, InstBuilder, Signature, UserFuncName,
    };
    use cranelift_codegen::isa::CallConv;

    fn verify(func: &Function) {
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        cranelift_codegen::verify_function(func, &flags).unwrap();
    }

    /// Both arms compute `x*2` then `+1` differently-factored — the
    /// identical `ishl x, 1`/`iadd …, 1` chain hoists to `b`.
    #[test]
    fn hoists_common_chain() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.params.push(AbiParam::new(I32));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b = func.dfg.make_block();
        let t = func.dfg.make_block();
        let e = func.dfg.make_block();
        for bl in [b, t, e] {
            func.layout.append_block(bl);
        }
        let x = func.dfg.append_block_param(b, I64);
        let c = func.dfg.append_block_param(b, I32);
        func.dfg.append_block_param(t, I64);
        func.dfg.append_block_param(e, I64);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(b);
            cur.ins().brif(c, t, &[BlockArg::Value(x)], e, &[BlockArg::Value(x)]);
        }
        {
            // t: r = (p*2)+1 … same chain in e.
            let p = func.dfg.block_params(t)[0];
            let mut cur = FuncCursor::new(&mut func).at_bottom(t);
            let k1 = cur.ins().iconst(I64, 1);
            let d = cur.ins().ishl(p, k1);
            let r = cur.ins().iadd(d, k1);
            cur.ins().return_(&[r]);
        }
        {
            let p = func.dfg.block_params(e)[0];
            let mut cur = FuncCursor::new(&mut func).at_bottom(e);
            let k1 = cur.ins().iconst(I64, 1);
            let d = cur.ins().ishl(p, k1);
            let r = cur.ins().iadd(d, k1);
            cur.ins().return_(&[r]);
        }
        verify(&func);
        let mut an = crate::pipeline::Analysis::default();
        let n = run(&mut func, &mut an);
        verify(&func);
        // iconst+ishl+iadd all co-hoist.
        assert_eq!(n, 3, "{}", func.display());
        // Both arms are now terminator-only.
        for a in [t, e] {
            let insts: Vec<_> = func.layout.block_insts(a).collect();
            assert_eq!(insts.len(), 1, "{}", func.display());
        }
    }

    /// An arm-only expression (operand not available in `b`) must not
    /// hoist.
    #[test]
    fn keeps_arm_local() {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.params.push(AbiParam::new(I32));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let b = func.dfg.make_block();
        let t = func.dfg.make_block();
        let e = func.dfg.make_block();
        for bl in [b, t, e] {
            func.layout.append_block(bl);
        }
        let x = func.dfg.append_block_param(b, I64);
        let c = func.dfg.append_block_param(b, I32);
        func.dfg.append_block_param(t, I64);
        func.dfg.append_block_param(e, I64);
        {
            let mut cur = FuncCursor::new(&mut func).at_bottom(b);
            cur.ins().brif(c, t, &[BlockArg::Value(x)], e, &[BlockArg::Value(x)]);
        }
        {
            // t: p^5 — e computes something else on its own param use.
            let p = func.dfg.block_params(t)[0];
            let mut cur = FuncCursor::new(&mut func).at_bottom(t);
            let k = cur.ins().iconst(I64, 5);
            let r = cur.ins().bxor(p, k);
            cur.ins().return_(&[r]);
        }
        {
            let p = func.dfg.block_params(e)[0];
            let mut cur = FuncCursor::new(&mut func).at_bottom(e);
            let k = cur.ins().iconst(I64, 7);
            let r = cur.ins().bxor(p, k);
            cur.ins().return_(&[r]);
        }
        verify(&func);
        let mut an = crate::pipeline::Analysis::default();
        assert_eq!(run(&mut func, &mut an), 0);
        verify(&func);
    }
}
