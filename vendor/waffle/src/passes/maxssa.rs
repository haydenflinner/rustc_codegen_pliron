//! Conversion pass that creates "maximal SSA": only local uses (no
//! uses of defs in other blocks), with all values explicitly passed
//! through blockparams. This makes some other transforms easier
//! because it removes the need to worry about adding blockparams when
//! mutating the CFG (all possible blockparams are already there!).

use crate::cfg::CFGInfo;
use crate::entity::PerEntity;
use crate::ir::{Block, FunctionBody, Value, ValueDef};
use fxhash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::BTreeSet;

pub(crate) fn run(body: &mut FunctionBody, cut_blocks: Option<HashSet<Block>>, cfg: &CFGInfo) {
    MaxSSAPass::new(cut_blocks).run(body, cfg);
}

/// PLIRON: `run`, but a cut block's parameter whose incoming value is
/// the same on every edge becomes an alias of that value when the value
/// is defined in a block outside `clones` (the blocks the reducifier may
/// duplicate). See `MaxSSAPass::clones`.
pub(crate) fn run_with_clones(
    body: &mut FunctionBody,
    cut_blocks: HashSet<Block>,
    clones: HashSet<Block>,
    cfg: &CFGInfo,
) {
    let mut pass = MaxSSAPass::new(Some(cut_blocks));
    pass.clones = Some(clones);
    let cut = pass.cut_blocks.clone().unwrap_or_default();
    let clones = pass.clones.clone().unwrap_or_default();
    pass.run(body, cfg);
    remove_trivial_params(body, &cut, &clones);
}

/// PLIRON: drop block parameters that only ever carry one value. The
/// per-use walk above decides each parameter once, while its
/// predecessors in a cycle still pass parameters not yet decided, so a
/// loop-carried copy of an outer value survives it (`h(k) -> x(k) ->
/// h(k)`: each parameter's incoming values are the outer `k` and the
/// other parameter). Here, as in Braun et al.'s redundant-phi removal,
/// the parameters form a graph (an edge from a parameter to each value
/// passed for it); a strongly connected set of parameters whose
/// incoming values from outside the set are all one value `v` is `v`.
/// At a cut block, `v` must also have one name in every copy the
/// reducifier makes (defined outside `clones`). Tarjan's algorithm
/// visits each set after every set it reaches, so a set sees the
/// earlier sets' aliases. Linear in parameters plus edge arguments.
/// PLIRON: `remove_trivial_params` on a reducible body (no cut blocks):
/// loop-carried parameters that only ever hold one outer value. Called
/// from the wasm emitter under `PLIRON_WASM_PARAMSCC=1`.
pub fn remove_redundant_param_sets(body: &mut FunctionBody) {
    let none = HashSet::default();
    remove_trivial_params(body, &none, &none);
    crate::passes::resolve_aliases::run(body);
}

fn remove_trivial_params(body: &mut FunctionBody, cut: &HashSet<Block>, clones: &HashSet<Block>) {
    // Nodes: params of non-entry blocks.
    let mut node_of: HashMap<Value, usize> = HashMap::default();
    let mut nodes: Vec<(Block, Value)> = Vec::new();
    for blk in body.blocks.iter() {
        if blk == body.entry {
            continue;
        }
        for &(_, p) in &body.blocks[blk].params {
            node_of.insert(p, nodes.len());
            nodes.push((blk, p));
        }
    }
    if nodes.is_empty() {
        return;
    }
    // Incoming values per node.
    let mut inc: Vec<Vec<Value>> = vec![Vec::new(); nodes.len()];
    for blk in body.blocks.iter() {
        body.blocks[blk].terminator.visit_targets(|t| {
            if t.block == body.entry {
                return;
            }
            for (i, &a) in t.args.iter().enumerate() {
                let p = body.blocks[t.block].params[i].1;
                inc[node_of[&p]].push(body.resolve_alias(a));
            }
        });
    }
    let single_name = |body: &FunctionBody, v: Value| {
        let def = match body.values[v] {
            ValueDef::BlockParam(b, ..) => b,
            ValueDef::Operator(..) | ValueDef::PickOutput(..) => body.value_blocks[v],
            _ => return false,
        };
        !clones.contains(&def)
    };
    // Iterative Tarjan.
    let n = nodes.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on = vec![false; n];
    let mut stk: Vec<usize> = Vec::new();
    let mut next = 0usize;
    let mut dropped: Vec<(Block, Value)> = Vec::new();
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        // (node, next incoming position)
        let mut call: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stk.push(root);
        on[root] = true;
        while let Some(&(v, pos)) = call.last() {
            if pos < inc[v].len() {
                let a = inc[v][pos];
                call.last_mut().unwrap().1 += 1;
                if let Some(&w) = node_of.get(&a) {
                    if index[w] == usize::MAX {
                        index[w] = next;
                        low[w] = next;
                        next += 1;
                        stk.push(w);
                        on[w] = true;
                        call.push((w, 0));
                    } else if on[w] {
                        low[v] = low[v].min(index[w]);
                    }
                }
                continue;
            }
            call.pop();
            if let Some(&(u, _)) = call.last() {
                low[u] = low[u].min(low[v]);
            }
            if low[v] != index[v] {
                continue;
            }
            // `v` roots a strongly connected set: pop it.
            let mut scc: Vec<usize> = Vec::new();
            loop {
                let w = stk.pop().unwrap();
                on[w] = false;
                scc.push(w);
                if w == v {
                    break;
                }
            }
            let members: HashSet<Value> = scc.iter().map(|&w| nodes[w].1).collect();
            let mut outside: Option<Value> = None;
            let mut many = false;
            for &w in &scc {
                for &a in &inc[w] {
                    let a = body.resolve_alias(a);
                    if members.contains(&a) {
                        continue;
                    }
                    match outside {
                        None => outside = Some(a),
                        Some(o) if o == a => {}
                        Some(_) => many = true,
                    }
                }
            }
            let Some(val) = outside.filter(|_| !many) else {
                continue;
            };
            if scc.iter().any(|&w| cut.contains(&nodes[w].0)) && !single_name(body, val) {
                continue;
            }
            for &w in &scc {
                let (blk, p) = nodes[w];
                body.values[p] = ValueDef::Alias(val);
                dropped.push((blk, p));
            }
        }
    }
    if dropped.is_empty() {
        return;
    }
    // Remove the dropped params and their edge arguments.
    let gone: HashSet<Value> = dropped.iter().map(|&(_, p)| p).collect();
    let mut blocks: Vec<Block> = dropped.iter().map(|&(b, _)| b).collect();
    blocks.sort();
    blocks.dedup();
    for blk in blocks {
        let keep: Vec<bool> = body.blocks[blk]
            .params
            .iter()
            .map(|&(_, p)| !gone.contains(&p))
            .collect();
        body.blocks[blk].params.retain(|&(_, p)| !gone.contains(&p));
        let mut preds = body.blocks[blk].preds.clone();
        preds.sort();
        preds.dedup();
        for pred in preds {
            body.blocks[pred].terminator.update_targets(|t| {
                if t.block == blk {
                    let mut i = 0;
                    t.args.retain(|_| {
                        let k = keep[i];
                        i += 1;
                        k
                    });
                }
            });
        }
    }
}

struct MaxSSAPass {
    /// Blocks at which all live values must cross through blockparams
    /// (or if None, then all blocks).
    cut_blocks: Option<HashSet<Block>>,
    /// Additional block args that must be passed to each block, in
    /// order. Value numbers are *original* values.
    new_args: PerEntity<Block, Vec<Value>>,
    /// For each block, a value map: from original value to local copy
    /// of value.
    value_map: HashMap<(Block, Value), Value>,
    /// PLIRON: blocks the reducifier may duplicate (`run_with_clones`).
    /// A cut block keeps a parameter for every value live across it
    /// because its predecessors can be copies of a duplicated region, in
    /// which a value defined inside the region has another name. A value
    /// defined outside every block that may be duplicated has one name
    /// in all copies, so when it is what every edge passes, the
    /// parameter carries nothing and can be an alias, as at any other
    /// block. `None`: stock behaviour (cut blocks keep every parameter).
    clones: Option<HashSet<Block>>,
}

impl MaxSSAPass {
    fn new(cut_blocks: Option<HashSet<Block>>) -> Self {
        Self {
            cut_blocks,
            new_args: PerEntity::default(),
            value_map: HashMap::default(),
            clones: None,
        }
    }

    fn run(mut self, body: &mut FunctionBody, cfg: &CFGInfo) {
        for block in body.blocks.iter() {
            self.visit(body, cfg, block);
        }
        self.update(body);
    }

    fn visit(&mut self, body: &mut FunctionBody, cfg: &CFGInfo, block: Block) {
        // For each use in the block, process the use. Collect all
        // uses first to deduplicate and allow more efficient
        // processing (and to appease the borrow checker).
        let mut uses = BTreeSet::default();
        for &inst in &body.blocks[block].insts {
            match &body.values[inst] {
                &ValueDef::Operator(_, args, _) => {
                    for &arg in &body.arg_pool[args] {
                        let arg = body.resolve_alias(arg);
                        uses.insert(arg);
                    }
                }
                &ValueDef::PickOutput(value, ..) => {
                    let value = body.resolve_alias(value);
                    uses.insert(value);
                }
                _ => {}
            }
        }
        body.blocks[block].terminator.visit_uses(|u| {
            let u = body.resolve_alias(u);
            uses.insert(u);
        });

        for u in uses {
            self.visit_use(body, cfg, block, u);
        }
    }

    fn visit_use(&mut self, body: &mut FunctionBody, cfg: &CFGInfo, block: Block, value: Value) {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Phase {
            Pre,
            Post,
        }

        let mut stack = vec![(block, value, Phase::Pre, None)];

        while let Some(&(block, value, phase, _)) = stack.last() {
            match phase {
                Phase::Pre => {
                    if self.value_map.contains_key(&(block, value)) {
                        stack.pop();
                        continue;
                    }
                    if cfg.def_block[value] == block {
                        stack.pop();
                        continue;
                    }

                    self.new_args[block].push(value);

                    // Create a placeholder value.
                    let ty = body.values[value].ty(&body.type_pool).unwrap();
                    let blockparam = body.add_blockparam(block, ty);
                    self.value_map.insert((block, value), blockparam);

                    stack.pop();
                    stack.push((block, value, Phase::Post, Some(blockparam)));

                    // Recursively visit preds and use the value there, to
                    // ensure they have the value available as well.
                    for i in 0..body.blocks[block].preds.len() {
                        // Don't borrow for whole loop while iterating (`body` is
                        // taken as mut by recursion, but we don't add preds).
                        let pred = body.blocks[block].preds[i];
                        stack.push((pred, value, Phase::Pre, None));
                    }
                }

                Phase::Post => {
                    let Some((_, _, _, Some(blockparam))) = stack.pop() else {
                        unreachable!()
                    };

                    // If all preds have the same value, and this is not a
                    // cut-block, rewrite the blockparam to an alias instead.
                    // PLIRON: at a cut block too, when the value is defined
                    // outside the blocks that may be duplicated (`clones`).
                    let cut = self.is_cut_block(block);
                    if !cut || self.clones.is_some() {
                        let same = iter_all_same(
                            body.blocks[block]
                                .preds
                                .iter()
                                .map(|&pred| *self.value_map.get(&(pred, value)).unwrap_or(&value))
                                .filter(|&val| val != blockparam),
                        )
                        .filter(|&v| !cut || self.single_name(body, v));
                        if let Some(pred_value) = same {
                            body.blocks[block].params.pop();
                            self.new_args[block].pop();
                            body.values[blockparam] = ValueDef::Alias(pred_value);
                            self.value_map.insert((block, value), pred_value);
                        }
                    }
                }
            }
        }
    }

    /// PLIRON: `v` has the same name in every copy the reducifier makes:
    /// its defining block is not one it may duplicate.
    fn single_name(&self, body: &FunctionBody, v: Value) -> bool {
        let Some(clones) = self.clones.as_ref() else {
            return false;
        };
        let v = body.resolve_alias(v);
        let def = match body.values[v] {
            ValueDef::BlockParam(b, ..) => b,
            ValueDef::Operator(..) | ValueDef::PickOutput(..) => body.value_blocks[v],
            _ => return false,
        };
        !clones.contains(&def)
    }

    fn is_cut_block(&self, block: Block) -> bool {
        self.cut_blocks
            .as_ref()
            .map(|cut_blocks| cut_blocks.contains(&block))
            .unwrap_or(true)
    }

    fn update_branch_args(&mut self, body: &mut FunctionBody) {
        for (block, blockdata) in body.blocks.entries_mut() {
            blockdata.terminator.update_targets(|target| {
                for &new_arg in &self.new_args[target.block] {
                    let actual_value = self
                        .value_map
                        .get(&(block, new_arg))
                        .copied()
                        .unwrap_or(new_arg);
                    target.args.push(actual_value);
                }
            });
        }
    }

    fn update_uses(&mut self, body: &mut FunctionBody, block: Block) {
        let resolve = |body: &FunctionBody, value: Value| {
            let value = body.resolve_alias(value);
            self.value_map
                .get(&(block, value))
                .copied()
                .unwrap_or(value)
        };

        for i in 0..body.blocks[block].insts.len() {
            let inst = body.blocks[block].insts[i];
            let mut def = std::mem::take(&mut body.values[inst]);
            match &mut def {
                ValueDef::Operator(_, args, _) => {
                    for i in 0..args.len() {
                        let val = body.arg_pool[*args][i];
                        let val = resolve(body, val);
                        body.arg_pool[*args][i] = val;
                    }
                }
                ValueDef::PickOutput(value, ..) => {
                    *value = resolve(body, *value);
                }
                ValueDef::Alias(value) => {
                    *value = resolve(body, *value);
                }
                _ => {}
            }
            body.values[inst] = def;
        }
        let mut term = std::mem::take(&mut body.blocks[block].terminator);
        term.update_uses(|u| {
            *u = resolve(body, *u);
        });
        body.blocks[block].terminator = term;
    }

    fn update(&mut self, body: &mut FunctionBody) {
        self.update_branch_args(body);
        for block in body.blocks.iter() {
            self.update_uses(body, block);
        }
    }
}

fn iter_all_same<Item: PartialEq + Eq + Copy, I: Iterator<Item = Item>>(iter: I) -> Option<Item> {
    let mut item = None;
    for val in iter {
        if *item.get_or_insert(val) != val {
            return None;
        }
    }
    item
}
