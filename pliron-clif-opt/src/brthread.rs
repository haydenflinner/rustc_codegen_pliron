//! Thread edges into a block that only re-tests a condition each edge
//! already decides (`PLIRON_BRTHREAD=1`, opt-in).
//!
//! lol_html's `tag_is_one_of!` is `name == Tag::A || name == Tag::B || ..`
//! over an enum (`LocalName::Hash(u64) | Bytes(..)`): every `==` tests the
//! variant before the payload, so after `loadfwd` merges the variant loads
//! the chain is
//!
//! ```text
//! block0:  brif v4, block2, block1         ; v4: variant is Hash
//! block2:  brif (payload == A), T, block1
//! block1:  brif v4, block7, block6         ; only the re-test
//! block7:  brif (payload == B), T, block6
//! block6:  brif v4, ...
//! ```
//!
//! `block1` has two predecessors and each decides `v4`: `block0`'s edge is
//! its false side, `block2` sits under `block0`'s true edge. Jump threading
//! (`jumpthread`) only reads edge arguments and `fold_dominated_conds` /
//! `celim` only fold a branch its own block's dominator decides, so the
//! re-tests stayed: 21 `cbz` per call of `is_void_element`, and a payload
//! reload per compare.
//!
//! Facts: a block whose single incoming edge is one side of `brif c` knows
//! `c` on entry; a block inherits its immediate dominator's facts (a linked
//! chain per block, so building them is linear). An edge `P -> B`, `B` a
//! block of only `brif c` with no parameters, retargets to the successor
//! `c` selects when `c` is known on the edge (the edge's own `brif` side, or
//! `P`'s facts), following chains of such blocks up to `MAX_HOPS`, each hop
//! adding the fact it used. Sound after retargeting: a new path into the
//! taken successor carries the value that successor's facts assume, and
//! passes `P`, which the old idom chain's facts held on.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, BlockArg, Function, Inst, InstructionData, Value};
use rustc_hash::FxHashMap;

/// Blocks followed per edge.
const MAX_HOPS: usize = 32;
/// Facts read per lookup (dominator depth walked).
const MAX_WALK: usize = 64;

/// A fact list node: `cond` is `val`; `next` the rest of the chain.
#[derive(Clone, Copy)]
struct Fact {
    cond: Value,
    val: bool,
    next: u32,
}
const NIL: u32 = u32::MAX;

/// `brif c, t, f` of a block made of that instruction alone, no params.
fn retest(func: &Function, b: Block) -> Option<(Inst, Value)> {
    if !func.dfg.block_params(b).is_empty() {
        return None;
    }
    let t = func.layout.first_inst(b)?;
    if func.layout.last_inst(b) != Some(t) {
        return None;
    }
    match func.dfg.insts[t] {
        InstructionData::Brif { arg, blocks, .. } => {
            if blocks[0].block(&func.dfg.value_lists) == blocks[1].block(&func.dfg.value_lists) {
                return None;
            }
            Some((t, func.dfg.resolve_aliases(arg)))
        }
        _ => None,
    }
}

fn lookup(facts: &[Fact], mut at: u32, c: Value) -> Option<bool> {
    let mut n = 0;
    while at != NIL && n < MAX_WALK {
        let f = facts[at as usize];
        if f.cond == c {
            return Some(f.val);
        }
        at = f.next;
        n += 1;
    }
    None
}

pub fn run(func: &mut Function, cfg: &ControlFlowGraph, dt: &DominatorTree) -> usize {
    // Candidates first: nothing to do in most functions.
    let mut cands: FxHashMap<Block, (Inst, Value)> = FxHashMap::default();
    for b in func.layout.blocks() {
        if let Some(r) = retest(func, b) {
            cands.insert(b, r);
        }
    }
    if cands.is_empty() {
        return 0;
    }
    // Facts per block, in dominator-tree preorder (an idom's chain exists
    // before its children's).
    let mut facts: Vec<Fact> = Vec::new();
    let mut head: FxHashMap<Block, u32> = FxHashMap::default();
    for &b in dt.cfg_rpo() {
        let inherit = dt.idom(b).and_then(|i| head.get(&i).copied()).unwrap_or(NIL);
        let mut h = inherit;
        // A single incoming edge that is one side of a `brif`.
        let mut preds = cfg.pred_iter(b);
        if let (Some(p), None) = (preds.next(), preds.next())
            && let InstructionData::Brif { arg, blocks, .. } = func.dfg.insts[p.inst]
        {
            let (t, f) = (blocks[0].block(&func.dfg.value_lists), blocks[1].block(&func.dfg.value_lists));
            if t != f {
                facts.push(Fact { cond: func.dfg.resolve_aliases(arg), val: t == b, next: h });
                h = (facts.len() - 1) as u32;
            }
        }
        head.insert(b, h);
    }
    let mut n = 0;
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for p in blocks {
        let Some(term) = func.layout.last_inst(p) else { continue };
        let at = head.get(&p).copied().unwrap_or(NIL);
        let nd = func.dfg.insts[term].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables).len();
        // The edge's own `brif` side is a fact on that edge.
        let own = match func.dfg.insts[term] {
            InstructionData::Brif { arg, .. } => Some(func.dfg.resolve_aliases(arg)),
            _ => None,
        };
        for d in 0..nd {
            let bc = func.dfg.insts[term].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[d];
            let mut tgt = bc.block(&func.dfg.value_lists);
            if !cands.contains_key(&tgt) {
                continue;
            }
            // Facts gathered along the hops (on top of `at`).
            let mut extra: Vec<(Value, bool)> = Vec::new();
            if let Some(c) = own {
                extra.push((c, d == 0));
            }
            let mut args: Option<Vec<BlockArg>> = None;
            let mut hops = 0;
            while let Some(&(ti, c)) = cands.get(&tgt) {
                if hops >= MAX_HOPS {
                    break;
                }
                let known = extra.iter().rev().find(|e| e.0 == c).map(|e| e.1).or_else(|| lookup(&facts, at, c));
                let Some(v) = known else { break };
                let InstructionData::Brif { blocks: tb, .. } = func.dfg.insts[ti] else { unreachable!() };
                let next = tb[if v { 0 } else { 1 }];
                tgt = next.block(&func.dfg.value_lists);
                args = Some(next.args(&func.dfg.value_lists).collect());
                extra.push((c, v));
                hops += 1;
            }
            if hops == 0 || !pliron_cfg::bisect("brthread") {
                continue;
            }
            let args = args.unwrap_or_default();
            let dfg = &mut func.dfg;
            let nb = cranelift_codegen::ir::BlockCall::new(tgt, args, &mut dfg.value_lists);
            dfg.insts[term].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[d] = nb;
            n += 1;
        }
    }
    if n > 0 {
        crate::jumpthread::remove_unreachable_blocks(func);
    }
    n
}
