//! Equality chains against constants → a balanced search tree
//! (`PLIRON_EQTREE=1`, opt-in).
//!
//! `matches!(x, A | B | C | ..)` and `match x { A => .., B => .., .. }` over
//! sparse constants arrive as a chain of blocks, each `c = icmp eq x, K;
//! brif c, hit, next`. LLVM's SimplifyCFG turns such a chain into a switch
//! and lowers a sparse switch as a binary search on the value. lol_html's
//! `TreeBuilderSimulator::get_feedback_for_start_tag` tests a `LocalName`
//! hash against a dozen 64-bit constants (two or three instructions to
//! build each): LLVM ~4 compares per call, ours 12.
//!
//! Rule: a maximal chain of blocks, each holding only constants, one
//! `icmp eq|ne x, K` (`x` the same value throughout, `K` an `iconst`) and
//! a `brif` on it; the fall-through side of each test is the next chain
//! block, which has the chain as its only predecessor and no params. At
//! least `MIN_CASES` distinct constants. The arguments passed to the hit
//! targets must be defined outside the chain (they then dominate every
//! new block), and no value defined inside the chain may be used outside
//! its own block — the rewrite deletes every chain instruction, so an
//! escaping def (a test block's `icmp` or `iconst` re-used by its hit arm
//! or the default) would leave a use pointing at a detached instruction.
//! A repeated constant keeps its first hit (the later test
//! can never fire). The chain head is rewritten as the root of a tree:
//! `x u< K_mid` splits the sorted constants, leaves of at most `LEAF`
//! constants test them in turn and fall through to the chain's default.
//! Exactly one constant can equal `x`, so the order of tests does not
//! matter.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Block, BlockArg, Function, Inst, InstBuilder, InstructionData, Opcode, Type, Value};
use rustc_hash::FxHashSet;

const MIN_CASES: usize = 6;
const LEAF: usize = 3;
const MAX_CASES: usize = 256;

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

/// One chain link: `x == k` goes to `hit` (with `args`), else to `next`.
struct Link {
    k: i64,
    hit: Block,
    args: Vec<BlockArg>,
    next: Block,
}

/// `b` as a chain link on `x` (any `x` when `None`).
fn link(func: &Function, b: Block, x: Option<Value>) -> Option<(Value, Link)> {
    let t = func.layout.last_inst(b)?;
    let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[t] else { return None };
    let c = func.dfg.resolve_aliases(arg);
    let ci = func.dfg.value_def(c).inst()?;
    if func.layout.inst_block(ci) != Some(b) {
        return None;
    }
    let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[ci] else { return None };
    if !matches!(cond, IntCC::Equal | IntCC::NotEqual) {
        return None;
    }
    let (a0, a1) = (func.dfg.resolve_aliases(args[0]), func.dfg.resolve_aliases(args[1]));
    let (v, k) = match (iconst(func, a0), iconst(func, a1)) {
        (None, Some(k)) => (a0, k),
        (Some(k), None) => (a1, k),
        _ => return None,
    };
    if x.is_some_and(|x| x != v) {
        return None;
    }
    // Only constants, the compare and the branch.
    for i in func.layout.block_insts(b) {
        if i == t || i == ci {
            continue;
        }
        if func.dfg.insts[i].opcode() != Opcode::Iconst {
            return None;
        }
    }
    let (then_bc, else_bc) = (blocks[0], blocks[1]);
    let (hit_bc, next_bc) = if cond == IntCC::Equal { (then_bc, else_bc) } else { (else_bc, then_bc) };
    let next = next_bc.block(&func.dfg.value_lists);
    if next_bc.args(&func.dfg.value_lists).next().is_some() {
        return None;
    }
    Some((
        v,
        Link {
            k,
            hit: hit_bc.block(&func.dfg.value_lists),
            args: hit_bc.args(&func.dfg.value_lists).collect(),
            next,
        },
    ))
}

pub fn run(func: &mut Function) -> usize {
    let cfg = ControlFlowGraph::with_function(func);
    let single_pred = |b: Block, p: Block| -> bool {
        let mut it = cfg.pred_iter(b);
        matches!((it.next(), it.next()), (Some(q), None) if q.block == p)
    };
    // The rewrite deletes every instruction in a chain, so a chain is
    // only safe to rewrite when no value defined inside it is used in
    // another block (a hit arm or the default re-using the test block's
    // compare or constant is legal SSA — its def dominated the use — but
    // would be left pointing at a detached instruction). One pass over
    // the function marks every instruction whose result escapes its
    // block; uses in blocks the rewrite removes are over-rejected, which
    // is harmless.
    let mut escapes: FxHashSet<Inst> = FxHashSet::default();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            for v in func.dfg.inst_values(i) {
                if let Some(d) = func.dfg.resolved_value_def(v).inst()
                    && func.layout.inst_block(d) != Some(b)
                {
                    escapes.insert(d);
                }
            }
        }
    }
    let blocks: Vec<Block> = func.layout.blocks().collect();
    let mut taken: FxHashSet<Block> = FxHashSet::default();
    let mut plans: Vec<(Block, Value, Vec<Link>, Block, Vec<Block>)> = Vec::new();
    for b in blocks {
        if taken.contains(&b) {
            continue;
        }
        let Some((x, first)) = link(func, b, None) else { continue };
        let mut members = vec![b];
        let mut links = vec![first];
        loop {
            let nb = links.last().unwrap().next;
            if links.len() >= MAX_CASES
                || members.contains(&nb)
                || !func.dfg.block_params(nb).is_empty()
                || !single_pred(nb, *members.last().unwrap())
            {
                break;
            }
            let Some((_, l)) = link(func, nb, Some(x)) else { break };
            members.push(nb);
            links.push(l);
        }
        let default = links.last().unwrap().next;
        if links.len() < MIN_CASES || members.contains(&default) {
            continue;
        }
        // `x` and every hit argument must come from outside the chain.
        let inside = |v: Value| -> bool {
            let v = func.dfg.resolve_aliases(v);
            match func.dfg.value_def(v).inst() {
                Some(i) => func.layout.inst_block(i).is_some_and(|ib| members.contains(&ib)),
                None => match func.dfg.value_def(v) {
                    cranelift_codegen::ir::ValueDef::Param(pb, _) => members.contains(&pb),
                    _ => false,
                },
            }
        };
        if inside(x) || links.iter().any(|l| l.args.iter().any(|a| matches!(a, BlockArg::Value(v) if inside(*v)))) {
            continue;
        }
        // Nothing defined inside the chain may be used outside its block.
        if members
            .iter()
            .any(|&m| func.layout.block_insts(m).any(|i| escapes.contains(&i)))
        {
            continue;
        }
        // Distinct constants, first hit kept.
        let mut seen = FxHashSet::default();
        links.retain(|l| seen.insert(l.k));
        if links.len() < MIN_CASES {
            continue;
        }
        for &m in &members {
            taken.insert(m);
        }
        plans.push((b, x, links, default, members));
    }
    let mut n = 0;
    for (head, x, mut links, default, _members) in plans {
        if !pliron_cfg::bisect("eqtree") {
            continue;
        }
        let ty = func.dfg.value_type(x);
        let bits = ty.bits();
        let norm = |k: i64| -> u64 { if bits >= 64 { k as u64 } else { (k as u64) & ((1u64 << bits) - 1) } };
        links.sort_by_key(|l| norm(l.k));
        // The head keeps its identity: clear it and grow the tree from it.
        while let Some(i) = func.layout.first_inst(head) {
            func.layout.remove_inst(i);
        }
        emit(func, head, x, ty, &links, default, norm);
        n += 1;
    }
    if n > 0 {
        crate::jumpthread::remove_unreachable_blocks(func);
    }
    n
}

/// Fill `at` (empty) with a test of `x` against `links` (sorted).
fn emit(func: &mut Function, at: Block, x: Value, ty: Type, links: &[Link], default: Block, norm: impl Fn(i64) -> u64 + Copy) {
    if links.len() <= LEAF {
        // Linear tests; each miss falls to the next block, the last to
        // the default.
        let mut cur = at;
        for (j, l) in links.iter().enumerate() {
            let next = if j + 1 == links.len() {
                None
            } else {
                let nb = func.dfg.make_block();
                func.layout.insert_block_after(nb, cur);
                Some(nb)
            };
            let mut pos = FuncCursor::new(func).at_bottom(cur);
            let k = pos.ins().iconst(ty, l.k);
            let c = pos.ins().icmp(IntCC::Equal, x, k);
            let miss = next.unwrap_or(default);
            pos.ins().brif(c, l.hit, &l.args, miss, &[]);
            if let Some(nb) = next {
                cur = nb;
            }
        }
        return;
    }
    let mid = links.len() / 2;
    let lo = func.dfg.make_block();
    let hi = func.dfg.make_block();
    func.layout.insert_block_after(lo, at);
    func.layout.insert_block_after(hi, lo);
    let mut pos = FuncCursor::new(func).at_bottom(at);
    let k = pos.ins().iconst(ty, links[mid].k);
    let c = pos.ins().icmp(IntCC::UnsignedLessThan, x, k);
    pos.ins().brif(c, lo, &[], hi, &[]);
    let _ = norm(links[mid].k);
    emit(func, lo, x, ty, &links[..mid], default, norm);
    emit(func, hi, x, ty, &links[mid..], default, norm);
}
