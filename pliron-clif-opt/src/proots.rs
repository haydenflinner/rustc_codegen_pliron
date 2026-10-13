//! Uniform-source block-param analysis, shared by `param_src`,
//! `param_coeffs`, and the acc-class queries in loopvec's `AccCx`. One
//! Ascent fixpoint replaces the per-pass hand-rolled param scans: edge
//! args flow in as facts, the derived relations answer "does every
//! in-edge agree" — on the value (`alias`), or on carrying one chosen
//! value (`like`, see [`like_map`]).
//!
//! Fact extraction is the interesting half: edge args are
//! alias-resolved, self-carries (`edge(p, p)` — "keep my previous
//! value", which makes every uniform reading inductively exact) are
//! dropped, non-`Value` args mark the param `blocked`, and unreachable
//! predecessors contribute nothing.

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, BlockArg, Function, Inst, Value};
use rustc_hash::{FxHashMap, FxHashSet};

/// Extracted edge facts for one function. `detail[p]` mirrors the
/// `(terminator, dest slot, source block, arg)` tuples `AccCx::in_args`
/// walks — one CFG pass feeds both the Ascent relations and that query.
pub struct Facts {
    /// `(param, arg)` on every reachable in-edge (aliases resolved,
    /// `arg != param`).
    pub edge: Vec<(Value, Value)>,
    /// Params with a non-`Value` arg somewhere — opaque to every class.
    pub blocked: Vec<Value>,
    /// `param → [(terminator inst, dest slot, pred block, arg)]`.
    pub detail: FxHashMap<Value, Vec<(Inst, usize, Block, Value)>>,
}

pub fn facts(func: &Function) -> Facts {
    let Some(entry) = func.layout.entry_block() else {
        return Facts {
            edge: Vec::new(),
            blocked: Vec::new(),
            detail: FxHashMap::default(),
        };
    };
    // Reachable = BFS over the CFG (equivalent to `dt.is_reachable`,
    // without needing the dominator tree).
    let cfg = ControlFlowGraph::with_function(func);
    let mut reach: FxHashSet<Block> = [entry].into_iter().collect();
    let mut stack = vec![entry];
    while let Some(b) = stack.pop() {
        for s in cfg.succ_iter(b) {
            if reach.insert(s) {
                stack.push(s);
            }
        }
    }
    let mut f = Facts {
        edge: Vec::new(),
        blocked: Vec::new(),
        detail: FxHashMap::default(),
    };
    for b in func.layout.blocks() {
        for p in func.dfg.block_params(b) {
            f.detail.entry(*p).or_default();
        }
    }
    // Layout order (not the `reach` set's iteration order) keeps `edge`
    // and `detail` deterministic — `acc_leaves`'s leaf order feeds
    // downstream plan decisions.
    for b in func.layout.blocks() {
        if !reach.contains(&b) {
            continue;
        }
        for i in func.layout.block_insts(b) {
            for (slot, bc) in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .iter()
                .enumerate()
            {
                let dst = bc.block(&func.dfg.value_lists);
                let params = func.dfg.block_params(dst);
                for (k, a) in bc.args(&func.dfg.value_lists).enumerate() {
                    let Some(&p) = params.get(k) else {
                        break;
                    };
                    match a {
                        BlockArg::Value(v) => {
                            let v = func.dfg.resolve_aliases(v);
                            if v != p {
                                f.edge.push((p, v));
                                f.detail.entry(p).or_default().push((i, slot, b, v));
                            }
                        }
                        _ => f.blocked.push(p),
                    }
                }
            }
        }
    }
    f
}

ascent::ascent! {
    struct Proots;

    /// param → in-edge arg.
    relation edge_arg(Value, Value);
    /// param with an opaque (non-`Value`) edge arg.
    relation blocked(Value);

    /// param fed ≥2 distinct values.
    relation diffs(Value);
    diffs(p) <-- edge_arg(p, a1), edge_arg(p, a2), if a1 != a2;

    /// Every in-edge delivers the same value — `param_src`'s relation.
    relation alias(Value, Value);
    alias(p, a) <-- edge_arg(p, a), !diffs(p), !blocked(p);
}

/// The alias relation over pre-extracted `facts` — `param_src`'s job.
pub fn alias_of(f: &Facts) -> FxHashMap<Value, Value> {
    let mut p = Proots::default();
    p.edge_arg = f.edge.clone();
    p.blocked = f.blocked.iter().map(|&v| (v,)).collect();
    p.run();
    p.alias.into_iter().collect()
}

/// The alias relation alone — `param_src`'s job.
pub fn alias_map(func: &Function) -> FxHashMap<Value, Value> {
    alias_of(&facts(func))
}

ascent::ascent! {
    /// "Always carries `acc`" for one acc candidate — loopvec's `AccCx`
    /// is_acc as a stratified fixpoint: an interior param is `bad` when a
    /// reachable edge feeds it a concretely non-acc or opaque arg, or any
    /// already-bad param; `like` is what remains. The depth bound and
    /// phi-cycle false negatives of the recursive version disappear.
    struct AccR;

    relation edge_arg(Value, Value);
    /// Params of in-body, non-header blocks.
    relation interior(Value);
    /// `{p}` — the acc itself.
    relation accv(Value);
    /// Params with an opaque (non-`Value`) edge arg.
    relation blocked(Value);

    relation bad(Value);
    bad(q) <-- interior(q), edge_arg(q, a), !interior(a), !accv(a);
    bad(q) <-- interior(q), blocked(q);
    // No trackable edges (self-carry only or unreachable preds): the
    // value is indeterminate, and feeding params must see it as non-acc.
    bad(q) <-- interior(q), !edge_arg(q, _);
    bad(q) <-- edge_arg(q, a), bad(a);

    /// Interior param whose every reachable in-edge carries acc.
    /// Requiring an `edge_arg` keeps unreachable/self-only params out.
    relation like(Value);
    like(q) <-- interior(q), edge_arg(q, _), !bad(q);
}

/// Interior params that provably always carry `acc`.
pub fn like_map(f: &Facts, interior: impl Iterator<Item = Value>, acc: Value) -> FxHashSet<Value> {
    let mut p = AccR::default();
    p.edge_arg = f.edge.clone();
    p.blocked = f.blocked.iter().map(|&v| (v,)).collect();
    p.interior = interior.map(|v| (v,)).collect();
    p.accv = vec![(acc,)];
    p.run();
    p.like.into_iter().map(|(v,)| v).collect()
}
