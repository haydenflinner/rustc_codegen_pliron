//! Computation of basic block order in emitted code.
//!
//! This module handles the translation from CLIF BBs to VCode BBs.
//!
//! The basic idea is that we compute a sequence of "lowered blocks" that
//! correspond to one or more blocks in the graph: (CLIF CFG) `union` (implicit
//! block on *every* edge). Conceptually, the lowering pipeline wants to insert
//! moves for phi-nodes on every block-to-block transfer; these blocks always
//! conceptually exist, but may be merged with an "original" CLIF block (and
//! hence not actually exist; this is equivalent to inserting the blocks only on
//! critical edges).
//!
//! In other words, starting from a CFG like this (where each "CLIF block" and
//! "(edge N->M)" is a separate basic block):
//!
//! ```plain
//!
//!              CLIF block 0
//!               /           \
//!       (edge 0->1)         (edge 0->2)
//!              |                |
//!       CLIF block 1         CLIF block 2
//!              \                /
//!           (edge 1->3)   (edge 2->3)
//!                   \      /
//!                 CLIF block 3
//! ```
//!
//! We can produce a CFG of lowered blocks like so:
//!
//! ```plain
//!            +--------------+
//!            | CLIF block 0 |
//!            +--------------+
//!               /           \
//!     +--------------+     +--------------+
//!     | (edge 0->1)  |     | (edge 0->2)  |
//!     | CLIF block 1 |     | CLIF block 2 |
//!     | (edge 1->3)  |     | (edge 2->3)  |
//!     +--------------+     +--------------+
//!                \           /
//!                 \         /
//!                +------------+
//!                |CLIF block 3|
//!                +------------+
//! ```
//!
//! Each `LoweredBlock` names just an original CLIF block, or just an edge block.
//!
//! To compute this lowering, we do a DFS over the CLIF-plus-edge-block graph
//! (never actually materialized, just defined by a "successors" function), and
//! compute the reverse postorder.
//!
//! This algorithm isn't perfect w.r.t. generated code quality: we don't, for
//! example, consider any information about whether edge blocks will actually
//! have content, because this computation happens as part of lowering *before*
//! regalloc, and regalloc may or may not insert moves/spills/reloads on any
//! particular edge. But it works relatively well and is conceptually simple.
//! Furthermore, the [MachBuffer] machine-code sink performs final peephole-like
//! branch editing that in practice elides empty blocks and simplifies some of
//! the other redundancies that this scheme produces.

use crate::dominator_tree::DominatorTree;
use crate::entity::SecondaryMap;
use crate::inst_predicates::visit_block_succs;
use crate::ir::{Block, BlockArg, Function, Inst, Opcode};
use crate::{FxHashMap, FxHashSet};
use crate::{machinst::*, trace};

/// Mapping from CLIF BBs to VCode BBs.
#[derive(Debug)]
pub struct BlockLoweringOrder {
    /// Lowered blocks, in BlockIndex order. Each block is some combination of
    /// (i) a CLIF block, and (ii) inserted crit-edge blocks before or after;
    /// see [LoweredBlock] for details.
    lowered_order: Vec<LoweredBlock>,
    /// BlockIndex values for successors for all lowered blocks, indexing `lowered_order`.
    lowered_succ_indices: Vec<BlockIndex>,
    /// Ranges in `lowered_succ_indices` giving the successor lists for each lowered
    /// block. Indexed by lowering-order index (`BlockIndex`).
    lowered_succ_ranges: Vec<(Option<Inst>, core::ops::Range<usize>)>,
    /// BlockIndex for each original Block.
    blockindex_by_block: SecondaryMap<Block, BlockIndex>,
    /// Cold blocks. These blocks are not reordered in the
    /// `lowered_order` above; the lowered order must respect RPO
    /// (uses after defs) in order for lowering to be
    /// correct. Instead, this set is used to provide `is_cold()`,
    /// which is used by VCode emission to sink the blocks at the last
    /// moment (when we actually emit bytes into the MachBuffer).
    cold_blocks: FxHashSet<BlockIndex>,
    /// PLIRON: targets of true dominance back-edges (loop headers, and
    /// the critical-edge blocks materialized for such edges). Edges
    /// that merely point backward in emission order — e.g. merge
    /// blocks pulled early by hot-path chaining — are excluded so
    /// their padding doesn't execute on the fallthrough path.
    back_edge_targets: FxHashSet<BlockIndex>,
    /// Lowered blocks that are indirect branch targets.
    indirect_branch_targets: FxHashSet<BlockIndex>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoweredBlock {
    /// Block in original CLIF.
    Orig {
        /// Original CLIF block.
        block: Block,
    },

    /// Critical edge between two CLIF blocks.
    CriticalEdge {
        /// The predecessor block.
        pred: Block,

        /// The successor block.
        succ: Block,

        /// The index of this branch in the successor edges from `pred`, following the same
        /// indexing order as `inst_predicates::visit_block_succs`. This is used to distinguish
        /// multiple edges between the same CLIF blocks.
        succ_idx: u32,
    },
}

impl LoweredBlock {
    /// Unwrap an `Orig` block.
    pub fn orig_block(&self) -> Option<Block> {
        match self {
            &LoweredBlock::Orig { block } => Some(block),
            &LoweredBlock::CriticalEdge { .. } => None,
        }
    }

    /// The associated in-edge predecessor, if this is a critical edge.
    #[cfg(test)]
    pub fn in_edge(&self) -> Option<Block> {
        match self {
            &LoweredBlock::CriticalEdge { pred, .. } => Some(pred),
            &LoweredBlock::Orig { .. } => None,
        }
    }

    /// The associated out-edge successor, if this is a critical edge.
    #[cfg(test)]
    pub fn out_edge(&self) -> Option<Block> {
        match self {
            &LoweredBlock::CriticalEdge { succ, .. } => Some(succ),
            &LoweredBlock::Orig { .. } => None,
        }
    }
}

impl BlockLoweringOrder {
    /// Compute and return a lowered block order for `f`.
    pub fn new(
        f: &Function,
        domtree: &DominatorTree,
        ctrl_plane: &mut ControlPlane,
    ) -> BlockLoweringOrder {
        trace!("BlockLoweringOrder: function body {:?}", f);

        // Step 1: compute the in-edge and out-edge count of every block.
        let mut block_in_count = SecondaryMap::with_default(0);
        let mut block_out_count = SecondaryMap::with_default(0);
        // PLIRON: predecessor lists for the chained-order loop heuristic.
        let mut block_preds: SecondaryMap<Block, SmallVec<[Block; 4]>> =
            SecondaryMap::with_default(SmallVec::new());

        // Block successors are stored as `LoweredBlocks` to simplify the construction of
        // `lowered_succs` in the final result. Initially, all entries are `Orig` values, and are
        // updated to be `CriticalEdge` when those cases are identified in step 2 below.
        let mut block_succs: SmallVec<[LoweredBlock; 128]> = SmallVec::new();
        let mut block_succ_range = SecondaryMap::with_default(0..0);

        let mut indirect_branch_target_clif_blocks = FxHashSet::default();

        for block in f.layout.blocks() {
            let start = block_succs.len();
            visit_block_succs(f, block, |_, succ, from_table| {
                block_out_count[block] += 1;
                block_in_count[succ] += 1;
                block_preds[succ].push(block);
                block_succs.push(LoweredBlock::Orig { block: succ });

                if from_table {
                    indirect_branch_target_clif_blocks.insert(succ);
                }
            });

            // Ensure that blocks terminated by br_table instructions
            // with an empty jump table are still treated like
            // conditional blocks from the point of view of critical
            // edge splitting. Also do the same for TryCall and
            // TryCallIndirect: we cannot have edge moves before the
            // branch, even if they have empty handler tables and thus
            // would otherwise have only one successor.
            if let Some(inst) = f.layout.last_inst(block) {
                match f.dfg.insts[inst].opcode() {
                    Opcode::BrTable | Opcode::TryCall | Opcode::TryCallIndirect => {
                        block_out_count[block] = block_out_count[block].max(2);
                    }
                    _ => {}
                }
            }

            let end = block_succs.len();
            block_succ_range[block] = start..end;
        }

        // Step 2: walk the postorder from the domtree in reverse to produce our desired node
        // lowering order, identifying critical edges to split along the way.

        // PLIRON: walk a chained order instead of plain RPO. The
        // domtree's `.rev()` DFS heuristic posts a brif's *second*
        // successor first, which lands its subtree at the far end of
        // the emitted order — in rustc-style dispatch code
        // (`brif unusual, common`) that's the hot edge, so every
        // iteration pays an unconditional branch. Instead, start
        // chains at RPO positions and greedily extend each block with
        // its preferred fallthrough successor: a non-cold forward edge
        // whose idom dominates-or-equals this block (preserving the
        // dominator-before-block property lowering relies on),
        // preferring merge points — dispatch preds converge on the hot
        // continuation. `PLIRON_BCHAIN=0` disables.
        static BCHAIN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let bchain = *BCHAIN
            .get_or_init(|| std::env::var("PLIRON_BCHAIN").map_or(true, |v| v != "0"));

        static BCHAIN_DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let bchain_debug =
            *BCHAIN_DEBUG.get_or_init(|| std::env::var_os("PLIRON_BCHAIN_DEBUG").is_some());

        let mut orig_order: Vec<Block> = Vec::new();
        if bchain {
            let rpo: Vec<Block> = domtree.cfg_rpo().copied().collect();
            // PLIRON: a block whose own terminator reaches a dominator is
            // a loop latch — preferring it as the fallthrough puts the
            // loop's hot spine on the not-taken side of the brif, so an
            // iteration pays only the latch's own back-edge branch.
            let mut is_latch = SecondaryMap::with_default(false);
            for &block in &rpo {
                let range = block_succ_range[block].clone();
                is_latch[block] = block_succs[range]
                    .iter()
                    .any(|lb| domtree.block_dominates(lb.orig_block().unwrap(), block));
            }
            let mut best_succ = SecondaryMap::with_default(
                crate::packed_option::PackedOption::<Block>::default(),
            );
            for &block in &rpo {
                let range = block_succ_range[block].clone();
                let mut pick: Option<Block> = None;
                let mut pick_in = 0;
                let mut pick_rank = 0;
                let mut pick_latch = false;
                for lb in &block_succs[range] {
                    let succ = lb.orig_block().unwrap();
                    if f.is_effectively_cold(succ) || domtree.block_dominates(succ, block) {
                        continue;
                    }
                    let idom = match domtree.idom(succ) {
                        Some(d) if domtree.block_dominates(d, block) => d,
                        _ => continue,
                    };
                    // Succ preference, highest first:
                    // - a succ that dominates a predecessor of `block` is
                    //   on a cycle back to it — the loop body past a head's
                    //   exit check. `brif done, exit-merge, continue`
                    //   should fall through to the body, not the exit.
                    // - a domtree child — a continuation only reachable
                    //   through this block, including merges fully inside
                    //   its region (dispatch preds converge on the hot
                    //   continuation).
                    // - a shared merge whose region extends outside
                    //   `block`'s subtree — adjacent only as a last resort;
                    //   pulling it in scatters the region.
                    // In-count breaks ties within a class, and a latch
                    // beats a non-latch of the same count.
                    let rank = if block_preds[block]
                        .iter()
                        .any(|&p| domtree.block_dominates(succ, p))
                    {
                        2
                    } else if idom == block {
                        1
                    } else {
                        0
                    };
                    let latch = is_latch[succ];
                    if pick.is_none()
                        || rank > pick_rank
                        || (rank == pick_rank
                            && ((latch && !pick_latch)
                                || (latch == pick_latch
                                    && block_in_count[succ] >= pick_in)))
                    {
                        pick = Some(succ);
                        pick_in = block_in_count[succ];
                        pick_rank = rank;
                        pick_latch = latch;
                    }
                }
                if let Some(s) = pick {
                    best_succ[block] = s.into();
                    if bchain_debug {
                        eprintln!(
                            "bchain {} {block}: pick {s} rank={pick_rank} latch={pick_latch} in={pick_in}",
                            f.name
                        );
                    }
                }
            }
            let mut emitted = FxHashSet::default();
            for &block in &rpo {
                let mut cur = block;
                while emitted.insert(cur) {
                    orig_order.push(cur);
                    cur = match best_succ[cur].expand() {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
        } else {
            orig_order.extend(domtree.cfg_rpo().copied());
        }

        let mut lowered_order = Vec::new();
        let mut blockindex_by_block = SecondaryMap::with_default(BlockIndex::invalid());
        for &block in &orig_order {
            let idx = BlockIndex::new(lowered_order.len());
            lowered_order.push(LoweredBlock::Orig { block });
            blockindex_by_block[block] = idx;

            if block_out_count[block] > 1 {
                let range = block_succ_range[block].clone();

                // If chaos-mode is enabled in the control plane, iterate over
                // the successors in an arbitrary order, which should have no
                // impact on correctness. The order of the blocks is generally
                // relevant: Uses must be seen before defs for dead-code
                // elimination.
                let succs = ctrl_plane.shuffled(block_succs[range].iter_mut().enumerate());

                // PLIRON: `br_table` entries that pass the same block call
                // (same target, same args) share one split-edge block
                // instead of one identical `mov*; b` block each. The
                // duplicates' table entries point at the shared block; its
                // jump takes the first entry's args (equal by construction).
                // `Lower::lower_branch_blockparam_args` keeps the vcode CFG
                // free of the repeated successor. `PLIRON_EDGEDEDUP` (default on).
                let dedup_dests = if edge_dedup_enabled() {
                    f.layout.last_inst(block).and_then(|inst| {
                        let data = &f.dfg.insts[inst];
                        (data.opcode() == Opcode::BrTable).then(|| {
                            data.branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
                        })
                    })
                } else {
                    None
                };
                let mut seen_calls: FxHashMap<(Block, SmallVec<[BlockArg; 4]>), LoweredBlock> =
                    FxHashMap::default();

                for (succ_ix, lb) in succs {
                    let succ = lb.orig_block().unwrap();
                    if block_in_count[succ] > 1 {
                        // Mutate the successor to be a critical edge, as `block` has multiple
                        // edges leaving it, and `succ` has multiple edges entering it.
                        let edge = LoweredBlock::CriticalEdge {
                            pred: block,
                            succ,
                            succ_idx: succ_ix as u32,
                        };
                        if let Some(dests) = dedup_dests {
                            let call = &dests[succ_ix];
                            let pool = &f.dfg.value_lists;
                            debug_assert_eq!(call.block(pool), succ);
                            let key = (succ, call.args(pool).collect());
                            if let Some(&shared) = seen_calls.get(&key) {
                                *lb = shared;
                                continue;
                            }
                            seen_calls.insert(key, edge);
                        }
                        *lb = edge;
                        lowered_order.push(*lb);
                    }
                }
            }
        }

        let lb_to_bindex = FxHashMap::from_iter(
            lowered_order
                .iter()
                .enumerate()
                .map(|(i, &lb)| (lb, BlockIndex::new(i))),
        );

        // Step 3: build the successor tables given the lowering order. We can't perform this step
        // during the creation of `lowering_order`, as we need `lb_to_bindex` to be fully populated
        // first.
        let mut lowered_succ_indices = Vec::new();
        let mut cold_blocks = FxHashSet::default();
        let mut indirect_branch_targets = FxHashSet::default();
        let mut back_edge_targets = FxHashSet::default();
        let lowered_succ_ranges =
            Vec::from_iter(lowered_order.iter().enumerate().map(|(ix, lb)| {
                let bindex = BlockIndex::new(ix);
                let start = lowered_succ_indices.len();
                let opt_inst = match lb {
                    // Block successors are pulled directly over, as they'll have been mutated when
                    // determining the block order already.
                    &LoweredBlock::Orig { block } => {
                        let range = block_succ_range[block].clone();
                        lowered_succ_indices.extend(block_succs[range].iter().map(|lb| {
                            // PLIRON: tag dominance back-edge targets —
                            // the succ's CLIF block dominates this pred.
                            let s = match lb {
                                LoweredBlock::Orig { block: s } => *s,
                                LoweredBlock::CriticalEdge { succ, .. } => *succ,
                            };
                            if domtree.block_dominates(s, block) {
                                back_edge_targets.insert(lb_to_bindex[lb]);
                            }
                            lb_to_bindex[lb]
                        }));

                        if f.is_effectively_cold(block) {
                            cold_blocks.insert(bindex);
                        }

                        if indirect_branch_target_clif_blocks.contains(&block) {
                            indirect_branch_targets.insert(bindex);
                        }

                        let last = f.layout.last_inst(block).unwrap();
                        let opcode = f.dfg.insts[last].opcode();

                        assert!(opcode.is_terminator());

                        opcode.is_branch().then_some(last)
                    }

                    // Critical edges won't have successor information in block_succ_range, but
                    // they only have a single known successor to record anyway.
                    &LoweredBlock::CriticalEdge { pred, succ, .. } => {
                        let succ_index = lb_to_bindex[&LoweredBlock::Orig { block: succ }];
                        lowered_succ_indices.push(succ_index);
                        if domtree.block_dominates(succ, pred) {
                            back_edge_targets.insert(succ_index);
                        }

                        // Edges inherit indirect branch and cold block metadata from their
                        // successor.

                        if f.is_effectively_cold(succ) {
                            cold_blocks.insert(bindex);
                        }

                        if indirect_branch_target_clif_blocks.contains(&succ) {
                            indirect_branch_targets.insert(bindex);
                        }

                        None
                    }
                };
                let end = lowered_succ_indices.len();
                (opt_inst, start..end)
            }));

        let result = BlockLoweringOrder {
            lowered_order,
            lowered_succ_indices,
            lowered_succ_ranges,
            blockindex_by_block,
            cold_blocks,
            back_edge_targets,
            indirect_branch_targets,
        };

        trace!("BlockLoweringOrder: {:#?}", result);
        result
    }

    /// Get the lowered order of blocks.
    pub fn lowered_order(&self) -> &[LoweredBlock] {
        &self.lowered_order[..]
    }

    /// Get the BlockIndex, if any, for a given Block.
    ///
    /// The result will be `None` if the given Block is unreachable
    /// (and thus does not appear in the lowered order).
    pub fn lowered_index_for_block(&self, block: Block) -> Option<BlockIndex> {
        let idx = self.blockindex_by_block[block];
        if idx.is_valid() { Some(idx) } else { None }
    }

    /// Get the successor indices for a lowered block.
    pub fn succ_indices(&self, block: BlockIndex) -> (Option<Inst>, &[BlockIndex]) {
        let (opt_inst, range) = &self.lowered_succ_ranges[block.index()];
        (*opt_inst, &self.lowered_succ_indices[range.clone()])
    }

    /// Determine whether the given lowered-block index is cold.
    pub fn is_cold(&self, block: BlockIndex) -> bool {
        self.cold_blocks.contains(&block)
    }

    /// PLIRON: whether the given lowered-block index is the target of
    /// a dominance back-edge (a loop header, or the critical-edge
    /// block materialized for such an edge).
    pub fn is_back_edge_target(&self, block: BlockIndex) -> bool {
        self.back_edge_targets.contains(&block)
    }

    /// Determine whether the given lowered block index is an indirect branch
    /// target.
    pub fn is_indirect_branch_target(&self, block: BlockIndex) -> bool {
        self.indirect_branch_targets.contains(&block)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::cursor::{Cursor, FuncCursor};
    use crate::flowgraph::ControlFlowGraph;
    use crate::ir::UserFuncName;
    use crate::ir::types::*;
    use crate::ir::{AbiParam, InstBuilder, Signature};
    use crate::isa::CallConv;

    fn build_test_func(n_blocks: usize, edges: &[(usize, usize)]) -> BlockLoweringOrder {
        assert!(n_blocks > 0);

        let name = UserFuncName::testcase("test0");
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I32));
        let mut func = Function::with_name_signature(name, sig);
        let blocks = (0..n_blocks)
            .map(|i| {
                let bb = func.dfg.make_block();
                assert!(bb.as_u32() == i as u32);
                bb
            })
            .collect::<Vec<_>>();

        let arg0 = func.dfg.append_block_param(blocks[0], I32);

        let mut pos = FuncCursor::new(&mut func);

        let mut edge = 0;
        for i in 0..n_blocks {
            pos.insert_block(blocks[i]);
            let mut succs = vec![];
            while edge < edges.len() && edges[edge].0 == i {
                succs.push(edges[edge].1);
                edge += 1;
            }
            if succs.len() == 0 {
                pos.ins().return_(&[arg0]);
            } else if succs.len() == 1 {
                pos.ins().jump(blocks[succs[0]], &[]);
            } else if succs.len() == 2 {
                pos.ins()
                    .brif(arg0, blocks[succs[0]], &[], blocks[succs[1]], &[]);
            } else {
                panic!("Too many successors");
            }
        }

        let mut cfg = ControlFlowGraph::new();
        cfg.compute(&func);
        let dom_tree = DominatorTree::with_function(&func, &cfg);

        BlockLoweringOrder::new(&func, &dom_tree, &mut Default::default())
    }

    #[test]
    fn test_blockorder_diamond() {
        let order = build_test_func(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]);

        // This test case doesn't need to introduce any critical edges, as all regalloc allocations
        // can sit on either the entry or exit of blocks 1 and 2.
        assert_eq!(order.lowered_order.len(), 4);
    }

    #[test]
    fn test_blockorder_critedge() {
        //            0
        //          /   \
        //         1     2
        //        /  \     \
        //       3    4    |
        //       |\  _|____|
        //       | \/ |
        //       | /\ |
        //       5    6
        //
        // (3 -> 5, and 3 -> 6 are critical edges and must be split)
        //
        let order = build_test_func(
            7,
            &[
                (0, 1),
                (0, 2),
                (1, 3),
                (1, 4),
                (2, 5),
                (3, 5),
                (3, 6),
                (4, 6),
            ],
        );

        assert_eq!(order.lowered_order.len(), 9);
        println!("ordered = {:?}", order.lowered_order);

        // block 0
        assert_eq!(order.lowered_order[0].orig_block().unwrap().as_u32(), 0);
        assert!(order.lowered_order[0].in_edge().is_none());
        assert!(order.lowered_order[0].out_edge().is_none());

        // block 2
        assert_eq!(order.lowered_order[1].orig_block().unwrap().as_u32(), 2);
        assert!(order.lowered_order[1].in_edge().is_none());
        assert!(order.lowered_order[1].out_edge().is_none());

        // block 1
        assert_eq!(order.lowered_order[2].orig_block().unwrap().as_u32(), 1);
        assert!(order.lowered_order[2].in_edge().is_none());
        assert!(order.lowered_order[2].out_edge().is_none());

        // block 4
        assert_eq!(order.lowered_order[3].orig_block().unwrap().as_u32(), 4);
        assert!(order.lowered_order[3].in_edge().is_none());
        assert!(order.lowered_order[3].out_edge().is_none());

        // block 3
        assert_eq!(order.lowered_order[4].orig_block().unwrap().as_u32(), 3);
        assert!(order.lowered_order[4].in_edge().is_none());
        assert!(order.lowered_order[4].out_edge().is_none());

        // critical edge 3 -> 5
        assert!(order.lowered_order[5].orig_block().is_none());
        assert_eq!(order.lowered_order[5].in_edge().unwrap().as_u32(), 3);
        assert_eq!(order.lowered_order[5].out_edge().unwrap().as_u32(), 5);

        // critical edge 3 -> 6
        assert!(order.lowered_order[6].orig_block().is_none());
        assert_eq!(order.lowered_order[6].in_edge().unwrap().as_u32(), 3);
        assert_eq!(order.lowered_order[6].out_edge().unwrap().as_u32(), 6);

        // block 6
        assert_eq!(order.lowered_order[7].orig_block().unwrap().as_u32(), 6);
        assert!(order.lowered_order[7].in_edge().is_none());
        assert!(order.lowered_order[7].out_edge().is_none());

        // block 5
        assert_eq!(order.lowered_order[8].orig_block().unwrap().as_u32(), 5);
        assert!(order.lowered_order[8].in_edge().is_none());
        assert!(order.lowered_order[8].out_edge().is_none());
    }
}

/// PLIRON: `PLIRON_EDGEDEDUP` (default on; `=0` off) shares one split-edge
/// block among the `br_table` entries that pass the same block call.
pub(crate) fn edge_dedup_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PLIRON_EDGEDEDUP").as_deref() != Ok("0"))
}
