/*
 * Released under the terms of the Apache 2.0 license with LLVM
 * exception. See `LICENSE` for details.
 */

//! Lightweight CFG analyses.

use crate::alloc::vec::Vec;

use crate::{domtree, postorder, Block, Function, Inst, ProgPoint, RegAllocError, VecExt};
use smallvec::{smallvec, SmallVec};

#[derive(Debug, Default)]
pub struct CFGInfoCtx {
    visited: Vec<bool>,
    block_to_rpo: Vec<Option<u32>>,
    backedge: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct CFGInfo {
    /// Postorder traversal of blocks.
    pub postorder: Vec<Block>,
    /// Domtree parents, indexed by block.
    pub domtree: Vec<Block>,
    /// For each instruction, the block it belongs to.
    pub insn_block: Vec<Block>,
    /// For each block, the first instruction.
    pub block_entry: Vec<ProgPoint>,
    /// For each block, the last instruction.
    pub block_exit: Vec<ProgPoint>,
    /// For each block, what is the approximate loop depth?
    ///
    /// This measure is fully precise iff the input CFG is reducible
    /// and blocks are in RPO, so that loop backedges are precisely
    /// those whose block target indices are less than their source
    /// indices. Otherwise, it will be approximate, but should still
    /// be usable for heuristic purposes.
    pub approx_loop_depth: Vec<u32>,
    /// PLIRON: back edges `(header, source)` by block index (dominance-checked
    /// under `PLIRON_RA2_LOOPDEPTH=1`), for `PLIRON_RA2_LOOPSPLIT`'s
    /// innermost-cycle split.
    pub back_edges: Vec<(u32, u32)>,
    /// PLIRON: per block, the shortest back edge `(header, source)` whose
    /// block span covers it (`(u32::MAX, _)` when none), so a split looks a
    /// use's innermost cycle up in O(1).
    pub innermost_cycle: Vec<(u32, u32)>,
}

impl CFGInfo {
    pub fn new<F: Function>(f: &F) -> Result<Self, RegAllocError> {
        let mut ctx = CFGInfoCtx::default();
        let mut this = Self::default();
        this.init(f, &mut ctx)?;
        Ok(this)
    }

    pub fn init<F: Function>(&mut self, f: &F, ctx: &mut CFGInfoCtx) -> Result<(), RegAllocError> {
        let nb = f.num_blocks();

        postorder::calculate(
            nb,
            f.entry_block(),
            &mut ctx.visited,
            &mut self.postorder,
            |block| f.block_succs(block),
        )?;

        domtree::calculate(
            nb,
            |block| f.block_preds(block),
            &self.postorder,
            &mut ctx.block_to_rpo,
            &mut self.domtree,
            f.entry_block(),
        );

        let insn_block = self.insn_block.repopulate(f.num_insts(), Block::invalid());
        let block_entry = self
            .block_entry
            .repopulate(nb, ProgPoint::before(Inst::invalid()));
        let block_exit = self
            .block_exit
            .repopulate(nb, ProgPoint::before(Inst::invalid()));
        let (backedge_in, backedge_out) = ctx.backedge.repopulate(nb * 2, 0).split_at_mut(nb);
        let mut back_edges = core::mem::take(&mut self.back_edges);
        back_edges.clear();

        for block in 0..f.num_blocks() {
            let block = Block::new(block);
            for inst in f.block_insns(block).iter() {
                insn_block[inst.index()] = block;
            }
            block_entry[block.index()] = ProgPoint::before(f.block_insns(block).first());
            block_exit[block.index()] = ProgPoint::after(f.block_insns(block).last());

            // Check critical edge condition: if there is more than
            // one predecessor, each must have only one successor
            // (this block).
            let preds = f.block_preds(block).len() + if block == f.entry_block() { 1 } else { 0 };
            if preds > 1 {
                for &pred in f.block_preds(block) {
                    let succs = f.block_succs(pred).len();
                    if succs > 1 {
                        return Err(RegAllocError::CritEdge(pred, block));
                    }
                }
            }

            // Check branch-arg condition: if any successors have more
            // than one predecessor (given above, there will only be
            // one such successor), then the last instruction of this
            // block (the branch) cannot have any args other than the
            // blockparams.
            let mut require_no_branch_args = false;
            for &succ in f.block_succs(block) {
                let preds = f.block_preds(succ).len() + if succ == f.entry_block() { 1 } else { 0 };
                if preds > 1 {
                    require_no_branch_args = true;
                    break;
                }
            }
            if require_no_branch_args {
                let last = f.block_insns(block).last();
                if !f.inst_operands(last).is_empty() {
                    return Err(RegAllocError::DisallowedBranchArg(last));
                }
            }

            // PLIRON `PLIRON_RA2_LOOPDEPTH=1`: only an edge to a block that
            // dominates its source is a loop back edge. Cranelift lays cold
            // blocks out last, and a cold path rejoining the hot code earlier
            // looked like a back edge: regex's `next_state` reached "depth"
            // 12, every use weight saturated at the bundle cap, and ties
            // decided which hot value kept a register.
            let true_back = loopdepth_on();
            for &succ in f.block_succs(block) {
                if succ.index() <= block.index()
                    && (!true_back
                        || domtree::dominates(&self.domtree[..], succ, block))
                {
                    backedge_in[succ.index()] += 1;
                    backedge_out[block.index()] += 1;
                    back_edges.push((succ.index() as u32, block.index() as u32));
                }
            }
        }

        // Shortest spans first; each block takes the first span covering
        // it. `next` skips filled blocks (path-halving union-find), so the
        // whole fill is near-linear in blocks plus back edges.
        let mut ic = core::mem::take(&mut self.innermost_cycle);
        ic.clear();
        ic.resize(nb, (u32::MAX, 0));
        if !back_edges.is_empty() {
            let mut order: Vec<(u32, u32)> = back_edges.clone();
            order.sort_by_key(|&(h, u)| u - h);
            let mut next: Vec<u32> = (0..=nb as u32).collect();
            fn find(next: &mut [u32], mut x: u32) -> u32 {
                while next[x as usize] != x {
                    let p = next[x as usize];
                    next[x as usize] = next[p as usize];
                    x = p;
                }
                x
            }
            for (h, u) in order {
                let mut b = find(&mut next, h);
                while b <= u {
                    ic[b as usize] = (h, u);
                    next[b as usize] = b + 1;
                    b = find(&mut next, b + 1);
                }
            }
        }
        self.innermost_cycle = ic;
        self.back_edges = back_edges;
        let approx_loop_depth = self.approx_loop_depth.cleared();
        let mut backedge_stack: SmallVec<[u32; 4]> = smallvec![];
        let mut cur_depth = 0;
        for block in 0..nb {
            if backedge_in[block] > 0 {
                cur_depth += 1;
                backedge_stack.push(backedge_in[block]);
            }

            approx_loop_depth.push(cur_depth);

            while backedge_stack.len() > 0 && backedge_out[block] > 0 {
                backedge_out[block] -= 1;
                *backedge_stack.last_mut().unwrap() -= 1;
                if *backedge_stack.last().unwrap() == 0 {
                    cur_depth -= 1;
                    backedge_stack.pop();
                }
            }
        }

        Ok(())
    }

    pub fn dominates(&self, a: Block, b: Block) -> bool {
        domtree::dominates(&self.domtree[..], a, b)
    }
}

/// PLIRON: `PLIRON_RA2_LOOPDEPTH=1` (opt-in): dominance-checked back edges
/// for `approx_loop_depth`.
fn loopdepth_on() -> bool {
    #[cfg(feature = "std")]
    {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("PLIRON_RA2_LOOPDEPTH").as_deref() == Ok("1"))
    }
    #[cfg(not(feature = "std"))]
    false
}
