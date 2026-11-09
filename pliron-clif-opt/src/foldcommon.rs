//! Consecutive conditional-branch merging — LLVM SimplifyCFG's
//! `FoldBranchToCommonDest` (`PLIRON_FOLDCOMMON`, opt-in).
//!
//! `b: brif A, S(..), F(..)` where `S` has `b` as its only predecessor,
//! holds at most `MAX_SIDE` speculatable insts whose results stay inside
//! `S`, and ends `brif B, T(..), F(..)` — the *same* destination `b`'s
//! other edge names, with the same arguments — folds into `b` as
//! `brif (band A B), T(..), F(..)`: the `a && b` short-circuit chain
//! evaluates `B` in `b` as data instead of control flow. The mirror
//! `brif A, T(..), S(..)` / `brif B, T(..), F(..)` is the `a || b` form
//! (`bor`). Rust lowers `&&`/`||` to exactly this block pair, so after
//! the fold an aarch64 `is_nonzero_cmp` of `band`/`bor` of compares can
//! fuse into one `cmp; ccmp; b` (the `PLIRON_CCMP` lowering rules) —
//! LLVM's dominant `ccmp` shape in the html-rewriter census.
//!
//! `S`'s insts are cloned into `b` (LLVM's "bonus instructions"): pure
//! ops only (`jumpthread::pure_op`), at most `MAX_SIDE`, and only when
//! every result's and every param's uses stay inside `S` (the `Uses`
//! index), so cloning then removing `S` can't dangle a definition.
//! Values `S` uses that `b` also sees dominate `b`'s terminator by the
//! single-predecessor argument `ifconv` already relies on.
//!
//! `S` is removed afterwards: it has no remaining predecessor.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Value,
};
use rustc_hash::FxHashMap;

use crate::jumpthread::pure_op;
use crate::uses::Uses;

/// Speculatable insts a mergeable side block may carry — the folded
/// `brif` costs a `band`/`bor` and the clones, so a long side still
/// wins on blocks but the budget keeps speculation bounded.
const MAX_SIDE: usize = 4;

/// How many times one block may fold — `a && b && c` chains fold one
/// side per round (`band` of a `band` still lowers, just without the
/// deeper `ccmp` fusion, so the cap only costs a repeat visit).
const MAX_FOLDS_PER_BLOCK: usize = 8;

fn thru(map: &FxHashMap<Value, Value>, v: Value) -> Value {
    map.get(&v).copied().unwrap_or(v)
}

/// `v` is an `icmp` result or a `uextend` of one — the operand shapes
/// the `ccmp` `band`/`bor` rules consume.
fn icmp(func: &Function, v: Value) -> Option<Inst> {
    let i = func.dfg.value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::IntCompare {
            opcode: cranelift_codegen::ir::Opcode::Icmp,
            ..
        } => Some(i),
        InstructionData::Unary {
            opcode: cranelift_codegen::ir::Opcode::Uextend,
            arg,
        } => {
            let i = func.dfg.resolved_value_def(arg).inst()?;
            matches!(
                func.dfg.insts[i],
                InstructionData::IntCompare {
                    opcode: cranelift_codegen::ir::Opcode::Icmp,
                    ..
                }
            )
            .then_some(i)
        }
        _ => None,
    }
}

/// The compare's operand type must lower to `ccmp`'s reg/reg form —
/// `emit_icmp` on a scalar i32/i64 compare is always a single
/// flag-producing inst and the secondary becomes the `ccmp`; narrower
/// or i128 compares would decline the fusion and leave the fold worse
/// off than the original `cmp; b` pair.
fn ccmpty(func: &Function, i: Inst) -> bool {
    let InstructionData::IntCompare { args, .. } = func.dfg.insts[i] else {
        return false;
    };
    matches!(
        func.dfg.value_type(func.dfg.resolve_aliases(args[0])),
        cranelift_codegen::ir::types::I32 | cranelift_codegen::ir::types::I64
    )
}

/// A call's resolved `Value` args, or `None` when it carries a
/// non-value (`TryOrigin`-style) argument the fold can't re-emit.
fn value_args(func: &Function, call: BlockCall) -> Option<Vec<Value>> {
    call.args(&func.dfg.value_lists)
        .map(|a| match a {
            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
            _ => None,
        })
        .collect()
}

/// Everything `b` needs to absorb side block `s`.
struct Side {
    /// `S`'s `brif` condition.
    cond: Value,
    /// Speculatable body insts to clone (excludes the terminator).
    clone: Vec<Inst>,
    /// Param → `b`-edge-arg substitution, seeded before cloning.
    map: FxHashMap<Value, Value>,
    /// `S`'s non-common edge.
    new_edge: BlockCall,
}

/// Validate `s` as `b`'s mergeable side. `common_b` is `b`'s BlockCall
/// to the shared destination (the outer edge one of `S`'s must match);
/// `common_is_taken` says which of `S`'s own edges that is (`false` =
/// `S`'s else edge — the `&&` form; `true` = its taken edge — `||`).
fn side(
    func: &Function,
    uses: &Uses,
    npreds: &FxHashMap<Block, usize>,
    b: Block,
    term: Inst,
    s: Block,
    sedge: BlockCall,
    common_b: BlockCall,
    common_is_taken: bool,
) -> Option<Side> {
    if s == b || func.layout.entry_block() == Some(s) {
        return None;
    }
    // `b` must be `S`'s only predecessor — the edge naming `s` says `b`
    // is one, so a count of one settles it.
    if npreds.get(&s).copied().unwrap_or(0) != 1 {
        return None;
    }
    let sterm = func.layout.last_inst(s)?;
    let InstructionData::Brif {
        arg: cond,
        blocks: targs,
        ..
    } = func.dfg.insts[sterm]
    else {
        return None;
    };
    let (s_common, s_new) = if common_is_taken {
        (targs[0], targs[1])
    } else {
        (targs[1], targs[0])
    };
    // No self-loops or new backedges into `b` from the merged edges.
    let (new_b, com_b) = (
        s_new.block(&func.dfg.value_lists),
        s_common.block(&func.dfg.value_lists),
    );
    if new_b == s
        || com_b == s
        || new_b == b
        || com_b == b
        || com_b != common_b.block(&func.dfg.value_lists)
    {
        return None;
    }
    // Param substitution: `S`'s params take the args `b`'s edge sends.
    let params = func.dfg.block_params(s).to_vec();
    let mut map: FxHashMap<Value, Value> = FxHashMap::default();
    for (p, a) in params.iter().zip(sedge.args(&func.dfg.value_lists)) {
        match a {
            BlockArg::Value(v) => {
                map.insert(func.dfg.resolve_aliases(*p), func.dfg.resolve_aliases(v));
            }
            _ => return None,
        }
    }
    // `S`'s edge to the shared destination must carry the same args the
    // outer edge does — one `brif` edge hands the block one list. `S`'s
    // args are compared after param substitution; anything still mapped
    // to an `S`-local value can't equal a `b`-scope argument.
    let want = value_args(func, common_b)?;
    let got = value_args(func, s_common)?;
    if got
        .iter()
        .map(|&v| thru(&map, v))
        .ne(want.iter().copied())
    {
        return None;
    }
    // `S`'s new-target edge args must all be `Value`s (they remap
    // through `map` + the clones at emit time).
    value_args(func, s_new)?;
    // The merged branch condition must be one width: `band`/`bor` need
    // equal scalar-int operand types. Both conditions must be `icmp`s
    // (possibly `uextend`-wrapped): a `band`/`bor` of two compares is
    // the only shape the `ccmp` fusion consumes, and folding anything
    // else trades `cmp; b` pairs for `cset; op; cbnz` — measured worse
    // on html-rewriter.
    let InstructionData::Brif { arg: a, .. } = func.dfg.insts[term] else {
        return None;
    };
    let (a, c) = (
        func.dfg.resolve_aliases(a),
        thru(&map, func.dfg.resolve_aliases(cond)),
    );
    let (ta, tc) = (func.dfg.value_type(a), func.dfg.value_type(c));
    let (ai, ci) = (icmp(func, a), icmp(func, c));
    if ta != tc
        || ta.is_vector()
        || !ta.is_int()
        || ai.is_none()
        || ci.is_none()
        || !(ccmpty(func, ai.unwrap()) || ccmpty(func, ci.unwrap()))
    {
        return None;
    }
    // `S`'s body: bounded, speculatable, and everything it defines —
    // results *and* params — used only inside `S` (`uses` lists every
    // `inst_values` site, branch args included).
    let mut clone = Vec::new();
    for i in func.layout.block_insts(s) {
        if i == sterm {
            break;
        }
        if clone.len() >= MAX_SIDE || !pure_op(func, i) {
            return None;
        }
        for &r in func.dfg.inst_results(i).iter() {
            let r = func.dfg.resolve_aliases(r);
            if uses.sites(r).iter().any(|&(ub, _)| ub != s) {
                return None;
            }
        }
        clone.push(i);
    }
    for &p in &params {
        let p = func.dfg.resolve_aliases(p);
        if uses.sites(p).iter().any(|&(ub, _)| ub != s) {
            return None;
        }
    }
    Some(Side {
        cond: func.dfg.resolve_aliases(cond),
        clone,
        map,
        new_edge: s_new,
    })
}

/// Fold `b`'s `brif` once. Returns true on a merge.
fn fold_once(
    func: &mut Function,
    uses: &Uses,
    npreds: &FxHashMap<Block, usize>,
    b: Block,
) -> bool {
    let Some(term) = func.layout.last_inst(b) else {
        return false;
    };
    if func.layout.inst_block(term) != Some(b) {
        return false;
    }
    let InstructionData::Brif {
        arg: a,
        blocks: e,
        ..
    } = func.dfg.insts[term]
    else {
        return false;
    };
    // `&&`: side on the taken edge, shared else. `||`: side on the else
    // edge, shared taken.
    for (si, ci, or) in [(0usize, 1usize, false), (1, 0, true)] {
        let s = e[si].block(&func.dfg.value_lists);
        let Some(sd) = side(func, uses, npreds, b, term, s, e[si], e[ci], or) else {
            continue;
        };
        // Clone `S`'s body into `b` before `term`, remapping params and
        // results (ifconv's `emit_arm` idiom).
        let mut map = sd.map;
        let cond;
        {
            let mut pos = FuncCursor::new(func).at_inst(term);
            for &i in &sd.clone {
                let ni = pos.func.dfg.clone_inst(i);
                pos.func.set_srcloc(ni, pos.func.srcloc(i));
                let vals: Vec<Value> = pos
                    .func
                    .dfg
                    .inst_args(ni)
                    .iter()
                    .map(|&x| thru(&map, pos.func.dfg.resolve_aliases(x)))
                    .collect();
                pos.func.dfg.overwrite_inst_values(ni, vals.into_iter());
                pos.insert_inst(ni);
                for (&o, &nr) in pos
                    .func
                    .dfg
                    .inst_results(i)
                    .to_vec()
                    .iter()
                    .zip(pos.func.dfg.inst_results(ni).iter())
                {
                    map.insert(pos.func.dfg.resolve_aliases(o), nr);
                }
            }
            let c = thru(&map, sd.cond);
            let a = pos.func.dfg.resolve_aliases(a);
            cond = if or {
                pos.ins().bor(a, c)
            } else {
                pos.ins().band(a, c)
            };
        }
        // `S`'s non-common edge args, remapped through the full map.
        let new_args: Vec<Value> = sd
            .new_edge
            .args(&func.dfg.value_lists)
            .filter_map(|x| match x {
                BlockArg::Value(v) => Some(thru(&map, func.dfg.resolve_aliases(v))),
                _ => None,
            })
            .collect();
        let nc = {
            let nb = sd.new_edge.block(&func.dfg.value_lists);
            BlockCall::new(
                nb,
                new_args.into_iter().map(BlockArg::Value),
                &mut func.dfg.value_lists,
            )
        };
        let (t, f) = if or { (e[0], nc) } else { (nc, e[1]) };
        func.dfg.insts[term] = InstructionData::Brif {
            opcode: cranelift_codegen::ir::Opcode::Brif,
            arg: cond,
            blocks: [t, f],
        };
        func.layout.remove_block_and_insts(s);
        return true;
    }
    false
}

pub fn run(func: &mut Function) -> usize {
    // Pred counts and the use index are pass-entry snapshots: a fold
    // swaps the `b` -> `s` edge for `b` -> `new_b` and deletes `s` ->
    // `common` while `b` keeps its own `common` edge, so every live
    // block's count is invariant (and dead `s`'s doesn't matter). New
    // uses a fold creates name mapped `b`-scope values, so a stale
    // `uses` entry can only over-report a checked value's sites.
    let cfg = ControlFlowGraph::with_function(func);
    let npreds: FxHashMap<Block, usize> = func
        .layout
        .blocks()
        .map(|b| (b, cfg.pred_iter(b).count()))
        .collect();
    let uses = Uses::new(func);
    let mut n = 0;
    for b in func.layout.blocks().collect::<Vec<_>>() {
        for _ in 0..MAX_FOLDS_PER_BLOCK {
            if !fold_once(func, &uses, &npreds, b) {
                break;
            }
            n += 1;
        }
    }
    n
}
