//! A control flow graph represented as mappings of basic blocks to their predecessors
//! and successors.
//!
//! Successors are represented as basic blocks while predecessors are represented by basic
//! blocks. Basic blocks are denoted by tuples of block and branch/jump instructions. Each
//! predecessor tuple corresponds to the end of a basic block.
//!
//! ```c
//!     Block0:
//!         ...          ; beginning of basic block
//!
//!         ...
//!
//!         brif vx, Block1, Block2 ; end of basic block
//!
//!     Block1:
//!         jump block3
//! ```
//!
//! Here `Block1` and `Block2` would each have a single predecessor denoted as `(Block0, brif)`,
//! while `Block3` would have a single predecessor denoted as `(Block1, jump block3)`.

use crate::entity::EntityRef;
use crate::inst_predicates;
use crate::ir::{Block, Function, Inst};
use crate::timing;
use alloc::vec::Vec;

/// A basic block denoted by its enclosing Block and last instruction.
/// PLIRON: `Copy`+`Clone` added — stored flat in the CSR arrays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockPredecessor {
    /// Enclosing Block key.
    pub block: Block,
    /// Last instruction in the basic block.
    pub inst: Inst,
}

impl BlockPredecessor {
    /// Convenient method to construct new BlockPredecessor.
    pub fn new(block: Block, inst: Inst) -> Self {
        Self { block, inst }
    }
}

/// The Control Flow Graph maintains a mapping of blocks to their predecessors
/// and successors where predecessors are basic blocks and successors are
/// basic blocks.
///
/// PLIRON: edge storage is flat CSR (`*_off` + packed arrays) instead of
/// the original per-block bforest maps/sets. Iteration order is preserved:
/// successors come out sorted by block number, predecessors sorted by
/// branch instruction — the same order the bforest comparators produced.
/// Rebuilding reuses the Vec capacities across `compute` calls, which
/// matters because pliron's optimizer rebuilds the CFG tens of times per
/// function.
pub struct ControlFlowGraph {
    /// Canonical edge list `(from, branch_inst, to)`. `build` leaves it
    /// sorted by `(to, inst)`; `recompute_block` patches a block's
    /// out-edges and rebuilds.
    edges: Vec<(Block, Inst, Block)>,
    /// CSR: `succs[succ_off[b]..succ_off[b+1]]` are `b`'s successors in
    /// block order. Length `nblocks + 1`.
    succ_off: Vec<u32>,
    succs: Vec<Block>,
    /// CSR: `preds[pred_off[b]..pred_off[b+1]]` are `b`'s predecessors in
    /// branch-inst order. Length `nblocks + 1`.
    pred_off: Vec<u32>,
    preds: Vec<BlockPredecessor>,
    nblocks: u32,
    valid: bool,
}

impl ControlFlowGraph {
    /// Allocate a new blank control flow graph.
    pub fn new() -> Self {
        Self {
            edges: Vec::new(),
            succ_off: Vec::new(),
            succs: Vec::new(),
            pred_off: Vec::new(),
            preds: Vec::new(),
            nblocks: 0,
            valid: false,
        }
    }

    /// Clear all data structures in this control flow graph.
    ///
    /// PLIRON: keeps the backing Vec capacities so a following `compute`
    /// reallocates nothing.
    pub fn clear(&mut self) {
        self.edges.clear();
        self.succ_off.clear();
        self.succs.clear();
        self.pred_off.clear();
        self.preds.clear();
        self.nblocks = 0;
        self.valid = false;
    }

    /// Allocate and compute the control flow graph for `func`.
    pub fn with_function(func: &Function) -> Self {
        let mut cfg = Self::new();
        cfg.compute(func);
        cfg
    }

    /// Compute the control flow graph of `func`.
    ///
    /// This will clear and overwrite any information already stored in this data structure.
    pub fn compute(&mut self, func: &Function) {
        let _tt = timing::flowgraph();
        self.edges.clear();
        for block in &func.layout {
            inst_predicates::visit_block_succs(func, block, |inst, dest, _| {
                self.edges.push((block, inst, dest));
            });
        }
        self.build(func.dfg.num_blocks() as u32);
        self.valid = true;
    }

    /// Rebuild the CSR tables from `edges`.
    fn build(&mut self, nblocks: u32) {
        self.nblocks = nblocks;
        let nb = nblocks as usize;

        // Successors: sort by (from, to, inst) and drop duplicate edges —
        // a block may name the same destination more than once (brif with
        // identical arms, br_table repeats), matching the old Set's
        // deduplication and block ordering.
        self.edges.sort_unstable();
        self.edges.dedup();
        self.succs.clear();
        self.succ_off.clear();
        self.succ_off.resize(nb + 1, 0);
        let mut prev_from = 0u32;
        let mut seen = false;
        for &(from, _, to) in &self.edges {
            let fi = from.index() as u32;
            if seen && fi != prev_from {
                self.succ_off[prev_from as usize + 1] = self.succs.len() as u32;
            }
            prev_from = fi;
            seen = true;
            self.succs.push(to);
        }
        if seen {
            self.succ_off[prev_from as usize + 1] = self.succs.len() as u32;
        }
        // Blocks with no recorded edges keep offset 0; a forward max-fill
        // turns those into empty ranges at the right position.
        for i in 1..self.succ_off.len() {
            self.succ_off[i] = self.succ_off[i].max(self.succ_off[i - 1]);
        }

        // Predecessors: sort by (to, branch_inst) and dedupe — a branch
        // table can name a destination repeatedly through one
        // instruction, which the old `Map<Inst, Block>` stored once.
        // Iteration stays in branch-inst order.
        self.edges
            .sort_unstable_by_key(|&(_, inst, to)| (to, inst));
        self.edges.dedup_by_key(|&mut (_, inst, to)| (to, inst));
        self.preds.clear();
        self.pred_off.clear();
        self.pred_off.resize(nb + 1, 0);
        let mut prev_to = 0u32;
        seen = false;
        for &(from, inst, to) in &self.edges {
            let ti = to.index() as u32;
            if seen && ti != prev_to {
                self.pred_off[prev_to as usize + 1] = self.preds.len() as u32;
            }
            prev_to = ti;
            seen = true;
            self.preds.push(BlockPredecessor::new(from, inst));
        }
        if seen {
            self.pred_off[prev_to as usize + 1] = self.preds.len() as u32;
        }
        for i in 1..self.pred_off.len() {
            self.pred_off[i] = self.pred_off[i].max(self.pred_off[i - 1]);
        }
    }

    /// Recompute the control flow graph of `block`.
    ///
    /// This is for use after modifying instructions within a specific block. It recomputes all edges
    /// from `block` while leaving edges to `block` intact. Its functionality a subset of that of the
    /// more expensive `compute`, and should be used when we know we don't need to recompute the CFG
    /// from scratch, but rather that our changes have been restricted to specific blocks.
    ///
    /// PLIRON: the CSR tables make single-block range patching awkward,
    /// so this refreshes the block's out-edges in `edges` and rebuilds
    /// the arrays — still just O(E), and nothing calls it hot.
    pub fn recompute_block(&mut self, func: &Function, block: Block) {
        debug_assert!(self.is_valid());
        self.edges.retain(|&(from, _, _)| from != block);
        inst_predicates::visit_block_succs(func, block, |inst, dest, _| {
            self.edges.push((block, inst, dest));
        });
        self.build(self.nblocks);
    }

    /// Get an iterator over the CFG predecessors to `block`.
    pub fn pred_iter(&self, block: Block) -> PredIter<'_> {
        let b = block.index();
        if b + 1 >= self.pred_off.len() {
            return [].iter().copied();
        }
        self.preds[self.pred_off[b] as usize..self.pred_off[b + 1] as usize]
            .iter()
            .copied()
    }

    /// Get an iterator over the CFG successors to `block`.
    pub fn succ_iter(&self, block: Block) -> SuccIter<'_> {
        debug_assert!(self.is_valid());
        let b = block.index();
        if b + 1 >= self.succ_off.len() {
            return [].iter().copied();
        }
        self.succs[self.succ_off[b] as usize..self.succ_off[b + 1] as usize]
            .iter()
            .copied()
    }

    /// Get an iterator over all blocks this control-flow graph has storage for,
    /// in block order.
    ///
    /// This may include blocks that are not in the function's layout (e.g. a
    /// block that was created but never inserted, or has since been removed);
    /// such blocks have no predecessors or successors in the graph.
    pub fn blocks(&self) -> impl ExactSizeIterator<Item = Block> + '_ {
        (0..self.nblocks).map(|i| Block::new(i as usize))
    }

    /// Get the number of blocks this control-flow graph has storage for.
    ///
    /// This equals `func.dfg.num_blocks()` for the function the CFG was
    /// computed from, and is an upper bound on the block indices appearing in
    /// the graph.
    pub fn num_blocks(&self) -> usize {
        self.nblocks as usize
    }

    /// Check if the CFG is in a valid state.
    ///
    /// Note that this doesn't perform any kind of validity checks. It simply checks if the
    /// `compute()` method has been called since the last `clear()`. It does not check that the
    /// CFG is consistent with the function.
    pub fn is_valid(&self) -> bool {
        self.valid
    }
}

/// An iterator over block predecessors. The iterator type is `BlockPredecessor`.
///
/// Each predecessor is an instruction that branches to the block.
pub type PredIter<'a> = core::iter::Copied<core::slice::Iter<'a, BlockPredecessor>>;

/// An iterator over block successors. The iterator type is `Block`.
pub type SuccIter<'a> = core::iter::Copied<core::slice::Iter<'a, Block>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::{Cursor, FuncCursor};
    use crate::ir::{InstBuilder, types};
    use alloc::vec::Vec;

    #[test]
    fn empty() {
        let func = Function::new();
        ControlFlowGraph::with_function(&func);
    }

    #[test]
    fn no_predecessors() {
        let mut func = Function::new();
        let block0 = func.dfg.make_block();
        let block1 = func.dfg.make_block();
        let block2 = func.dfg.make_block();
        func.layout.append_block(block0);
        func.layout.append_block(block1);
        func.layout.append_block(block2);

        let cfg = ControlFlowGraph::with_function(&func);

        let mut fun_blocks = func.layout.blocks();
        for block in func.layout.blocks() {
            assert_eq!(block, fun_blocks.next().unwrap());
            assert_eq!(cfg.pred_iter(block).count(), 0);
            assert_eq!(cfg.succ_iter(block).count(), 0);
        }
    }

    #[test]
    fn branches_and_jumps() {
        let mut func = Function::new();
        let block0 = func.dfg.make_block();
        let cond = func.dfg.append_block_param(block0, types::I32);
        let block1 = func.dfg.make_block();
        let block2 = func.dfg.make_block();

        let br_block0_block2_block1;
        let br_block1_block1_block2;

        {
            let mut cur = FuncCursor::new(&mut func);

            cur.insert_block(block0);
            br_block0_block2_block1 = cur.ins().brif(cond, block2, &[], block1, &[]);

            cur.insert_block(block1);
            br_block1_block1_block2 = cur.ins().brif(cond, block1, &[], block2, &[]);

            cur.insert_block(block2);
        }

        let mut cfg = ControlFlowGraph::with_function(&func);

        {
            let block0_predecessors = cfg.pred_iter(block0).collect::<Vec<_>>();
            let block1_predecessors = cfg.pred_iter(block1).collect::<Vec<_>>();
            let block2_predecessors = cfg.pred_iter(block2).collect::<Vec<_>>();

            let block0_successors = cfg.succ_iter(block0).collect::<Vec<_>>();
            let block1_successors = cfg.succ_iter(block1).collect::<Vec<_>>();
            let block2_successors = cfg.succ_iter(block2).collect::<Vec<_>>();

            assert_eq!(block0_predecessors.len(), 0);
            assert_eq!(block1_predecessors.len(), 2);
            assert_eq!(block2_predecessors.len(), 2);

            assert_eq!(
                block1_predecessors
                    .contains(&BlockPredecessor::new(block0, br_block0_block2_block1)),
                true
            );
            assert_eq!(
                block1_predecessors
                    .contains(&BlockPredecessor::new(block1, br_block1_block1_block2)),
                true
            );
            assert_eq!(
                block2_predecessors
                    .contains(&BlockPredecessor::new(block0, br_block0_block2_block1)),
                true
            );
            assert_eq!(
                block2_predecessors
                    .contains(&BlockPredecessor::new(block1, br_block1_block1_block2)),
                true
            );

            assert_eq!(block0_successors, [block1, block2]);
            assert_eq!(block1_successors, [block1, block2]);
            assert_eq!(block2_successors, []);
        }

        // Add a new block to hold a return instruction
        let ret_block = func.dfg.make_block();

        {
            let mut cur = FuncCursor::new(&mut func);
            cur.insert_block(ret_block);
            cur.ins().return_(&[]);
        }

        // Change some instructions and recompute block0 and ret_block
        func.replace(br_block0_block2_block1)
            .brif(cond, block1, &[], ret_block, &[]);
        cfg.recompute_block(&func, block0);
        cfg.recompute_block(&func, ret_block);
        let br_block0_block1_ret_block = br_block0_block2_block1;

        {
            let block0_predecessors = cfg.pred_iter(block0).collect::<Vec<_>>();
            let block1_predecessors = cfg.pred_iter(block1).collect::<Vec<_>>();
            let block2_predecessors = cfg.pred_iter(block2).collect::<Vec<_>>();

            let block0_successors = cfg.succ_iter(block0);
            let block1_successors = cfg.succ_iter(block1);
            let block2_successors = cfg.succ_iter(block2);

            assert_eq!(block0_predecessors.len(), 0);
            assert_eq!(block1_predecessors.len(), 2);
            assert_eq!(block2_predecessors.len(), 1);

            assert_eq!(
                block1_predecessors
                    .contains(&BlockPredecessor::new(block0, br_block0_block1_ret_block)),
                true
            );
            assert_eq!(
                block1_predecessors
                    .contains(&BlockPredecessor::new(block1, br_block1_block1_block2)),
                true
            );
            assert_eq!(
                block2_predecessors
                    .contains(&BlockPredecessor::new(block0, br_block0_block1_ret_block)),
                false
            );
            assert_eq!(
                block2_predecessors
                    .contains(&BlockPredecessor::new(block1, br_block1_block1_block2)),
                true
            );

            assert_eq!(block0_successors.collect::<Vec<_>>(), [block1, ret_block]);
            assert_eq!(block1_successors.collect::<Vec<_>>(), [block1, block2]);
            assert_eq!(block2_successors.collect::<Vec<_>>(), []);
        }
    }
}
