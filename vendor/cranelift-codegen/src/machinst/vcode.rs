//! This implements the VCode container: a CFG of Insts that have been lowered.
//!
//! VCode is virtual-register code. An instruction in VCode is almost a machine
//! instruction; however, its register slots can refer to virtual registers in
//! addition to real machine registers.
//!
//! VCode is structured with traditional basic blocks, and
//! each block must be terminated by an unconditional branch (one target), a
//! conditional branch (two targets), or a return (no targets). Note that this
//! slightly differs from the machine code of most ISAs: in most ISAs, a
//! conditional branch has one target (and the not-taken case falls through).
//! However, we expect that machine backends will elide branches to the following
//! block (i.e., zero-offset jumps), and will be able to codegen a branch-cond /
//! branch-uncond pair if *both* targets are not fallthrough. This allows us to
//! play with layout prior to final binary emission, as well, if we want.
//!
//! See the main module comment in `mod.rs` for more details on the VCode-based
//! backend pipeline.

use crate::CodegenError;
use crate::FxHashMap;
use crate::ir::{self, Constant, ConstantData, ValueLabel, types};
use crate::ranges::Ranges;
use crate::timing;
use crate::trace;
use crate::{LabelValueLoc, ValueLocRange};
use crate::{machinst::*, trace_log_enabled};
use regalloc2::{
    Allocation, Edit, Function as RegallocFunction, InstOrEdit, InstPosition, InstRange, Operand,
    OperandConstraint, OperandKind, PReg, PRegSet, ProgPoint, RegClass,
};

use crate::HashMap;
use crate::hash_map::Entry;
use core::cmp::Ordering;
use core::fmt::{self, Write};
use core::mem::take;
use core::ops::Range;
use cranelift_entity::{EntityRef, Keys, entity_impl};

/// PLIRON: `PLIRON_EDGEMERGE` key (see `VCode::edge_merge_key`).
type EdgeKey = (SmallVec<[(Allocation, Allocation); 4]>, BlockIndex, bool);

/// PLIRON: a `PLIRON_*` mode knob for EDGEMERGE/EPIMERGE. Unset defaults
/// to `default`; `0` is off; a digit is itself; anything else is the
/// default. Matches `pliron_cfg::DEFAULT_VALUES`.
fn pliron_mode(var: &str, default: u8) -> u8 {
    match std::env::var(var) {
        Err(_) => default,
        Ok(v) => v.parse::<u8>().unwrap_or(default),
    }
}

/// Index referring to an instruction in VCode.
pub type InsnIndex = regalloc2::Inst;

/// Extension trait for `InsnIndex` to allow conversion to a
/// `BackwardsInsnIndex`.
trait ToBackwardsInsnIndex {
    fn to_backwards_insn_index(&self, num_insts: usize) -> BackwardsInsnIndex;
}

impl ToBackwardsInsnIndex for InsnIndex {
    fn to_backwards_insn_index(&self, num_insts: usize) -> BackwardsInsnIndex {
        BackwardsInsnIndex::new(num_insts - self.index() - 1)
    }
}

/// An index referring to an instruction in the VCode when it is backwards,
/// during VCode construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(
    feature = "enable-serde",
    derive(::serde::Serialize, ::serde::Deserialize)
)]
pub struct BackwardsInsnIndex(InsnIndex);

impl BackwardsInsnIndex {
    pub fn new(i: usize) -> Self {
        BackwardsInsnIndex(InsnIndex::new(i))
    }
}

/// Index referring to a basic block in VCode.
pub type BlockIndex = regalloc2::Block;

/// VCodeInst wraps all requirements for a MachInst to be in VCode: it must be
/// a `MachInst` and it must be able to emit itself at least to a `SizeCodeSink`.
pub trait VCodeInst: MachInst + MachInstEmit {}
impl<I: MachInst + MachInstEmit> VCodeInst for I {}

/// A function in "VCode" (virtualized-register code) form, after
/// lowering.  This is essentially a standard CFG of basic blocks,
/// where each basic block consists of lowered instructions produced
/// by the machine-specific backend.
///
/// Note that the VCode is immutable once produced, and is not
/// modified by register allocation in particular. Rather, register
/// allocation on the `VCode` produces a separate `regalloc2::Output`
/// struct, and this can be passed to `emit`. `emit` in turn does not
/// modify the vcode, but produces an `EmitResult`, which contains the
/// machine code itself, and the associated disassembly and/or
/// metadata as requested.
std::thread_local! {
    /// PLIRON: the CLIF block `Context::shrink_split` split live ranges at,
    /// for the function this thread is compiling (read before regalloc).
    pub(crate) static PLIRON_SPLIT_BLOCK: core::cell::Cell<Option<crate::ir::Block>> =
        const { core::cell::Cell::new(None) };
}

pub struct VCode<I: VCodeInst> {
    /// VReg IR-level types.
    vreg_types: Vec<Type>,

    /// PLIRON: vregs defined by a single rematerializable instruction
    /// (`MachInst::rematerialize`), by vreg index; regalloc2 weights their
    /// bundles down (`RegallocFunction::is_remat`). Filled by
    /// `compute_remat_vregs` (opt-in, `PLIRON_RA2_REMATW=1`).
    remat_vregs: Vec<bool>,

    /// PLIRON: per block, placement's static execution frequency relative
    /// to the entry block (empty when not computed), handed to regalloc2
    /// as `block_hotness` for spill weights under `PLIRON_RA2_FREQ`
    /// (`compute_block_freq`).
    block_freq: Vec<f32>,

    /// PLIRON: the lowered block whose blockparams regalloc2 must not merge
    /// with their inputs (`PLIRON_SPLIT_BLOCK`).
    no_merge_block: Option<BlockIndex>,

    /// Lowered machine instructions in order corresponding to the original IR.
    insts: Vec<I>,

    /// A map from backwards instruction index to the user stack map for that
    /// instruction.
    ///
    /// This is a sparse side table that only has entries for instructions that
    /// are safepoints, and only for a subset of those that have an associated
    /// user stack map.
    user_stack_maps: FxHashMap<BackwardsInsnIndex, ir::UserStackMap>,

    /// A map from backwards instruction index to the debug tags for
    /// that instruction. Each entry indexes a range in the
    /// `debug_tag_pool`.
    debug_tags: FxHashMap<BackwardsInsnIndex, Range<u32>>,

    /// Pooled storage for sequences of debug tags; indexed by entries
    /// in `debug_tags`.
    debug_tag_pool: Vec<ir::DebugTag>,

    /// Operands: pre-regalloc references to virtual registers with
    /// constraints, in one flattened array. This allows the regalloc
    /// to efficiently access all operands without requiring expensive
    /// matches or method invocations on insts.
    operands: Vec<Operand>,

    /// Operand index ranges: for each instruction in `insts`, there
    /// is a tuple here providing the range in `operands` for that
    /// instruction's operands.
    operand_ranges: Ranges,

    /// Clobbers: a sparse map from instruction indices to clobber masks.
    clobbers: FxHashMap<InsnIndex, PRegSet>,

    /// Source locations for each instruction. (`SourceLoc` is a `u32`, so it is
    /// reasonable to keep one of these per instruction.)
    srclocs: Vec<RelSourceLoc>,

    /// Entry block.
    entry: BlockIndex,

    /// Block instruction indices.
    block_ranges: Ranges,

    /// Block successors: index range in the `block_succs` list.
    block_succ_range: Ranges,

    /// Block successor lists, concatenated into one vec. The
    /// `block_succ_range` list of tuples above gives (start, end)
    /// ranges within this list that correspond to each basic block's
    /// successors.
    block_succs: Vec<regalloc2::Block>,

    /// Block predecessors: index range in the `block_preds` list.
    block_pred_range: Ranges,

    /// Block predecessor lists, concatenated into one vec. The
    /// `block_pred_range` list of tuples above gives (start, end)
    /// ranges within this list that correspond to each basic block's
    /// predecessors.
    block_preds: Vec<regalloc2::Block>,

    /// Block parameters: index range in `block_params` below.
    block_params_range: Ranges,

    /// Block parameter lists, concatenated into one vec. The
    /// `block_params_range` list of tuples above gives (start, end)
    /// ranges within this list that correspond to each basic block's
    /// blockparam vregs.
    block_params: Vec<regalloc2::VReg>,

    /// Outgoing block arguments on branch instructions, concatenated
    /// into one list.
    ///
    /// Note that this is conceptually a 3D array: we have a VReg list
    /// per block, per successor. We flatten those three dimensions
    /// into this 1D vec, then store index ranges in two levels of
    /// indirection.
    ///
    /// Indexed by the indices in `branch_block_arg_succ_range`.
    branch_block_args: Vec<regalloc2::VReg>,

    /// Array of sequences of (start, end) tuples in
    /// `branch_block_args`, one for each successor; these sequences
    /// for each block are concatenated.
    ///
    /// Indexed by the indices in `branch_block_arg_succ_range`.
    branch_block_arg_range: Ranges,

    /// For a given block, indices in `branch_block_arg_range`
    /// corresponding to all of its successors.
    branch_block_arg_succ_range: Ranges,

    /// Block-order information.
    block_order: BlockLoweringOrder,

    /// ABI object.
    pub(crate) abi: Callee<I::ABIMachineSpec>,

    /// Constant information used during code emission. This should be
    /// immutable across function compilations within the same module.
    emit_info: I::Info,

    /// Constants.
    pub(crate) constants: VCodeConstants,

    /// Value labels for debuginfo attached to vregs.
    debug_value_labels: Vec<(VReg, InsnIndex, InsnIndex, u32)>,

    pub(crate) sigs: SigSet,

    log2_min_function_alignment: u8,
}

/// The result of `VCode::emit`. Contains all information computed
/// during emission: actual machine code, optionally a disassembly,
/// and optionally metadata about the code layout.
pub struct EmitResult {
    /// The MachBuffer containing the machine code.
    pub buffer: MachBufferFinalized<Stencil>,

    /// Offset of each basic block, recorded during emission. Computed
    /// only if `machine_code_cfg_info` is enabled.
    pub bb_offsets: Vec<CodeOffset>,

    /// Final basic-block edges, in terms of code offsets of
    /// bb-starts. Computed only if `machine_code_cfg_info` is enabled.
    pub bb_edges: Vec<(CodeOffset, CodeOffset)>,

    /// The pretty-printed disassembly, if any. This uses the same
    /// pretty-printing for MachInsts as the pre-regalloc VCode Debug
    /// implementation, but additionally includes the prologue and
    /// epilogue(s), and makes use of the regalloc results.
    pub disasm: Option<String>,

    /// Value-labels information (debug metadata).
    pub value_labels_ranges: ValueLabelsRanges,
}

/// A builder for a VCode function body.
///
/// This builder has the ability to accept instructions in either
/// forward or reverse order, depending on the pass direction that
/// produces the VCode. The lowering from CLIF to VCode<MachInst>
/// ordinarily occurs in reverse order (in order to allow instructions
/// to be lowered only if used, and not merged) so a reversal will
/// occur at the end of lowering to ensure the VCode is in machine
/// order.
///
/// If built in reverse, block and instruction indices used once the
/// VCode is built are relative to the final (reversed) order, not the
/// order of construction. Note that this means we do not know the
/// final block or instruction indices when building, so we do not
/// hand them out. (The user is assumed to know them when appending
/// terminator instructions with successor blocks.)
pub struct VCodeBuilder<I: VCodeInst> {
    /// In-progress VCode.
    pub(crate) vcode: VCode<I>,

    /// In what direction is the build occurring?
    direction: VCodeBuildDirection,

    /// Debug-value label in-progress map, keyed by label. For each
    /// label, we keep disjoint ranges mapping to vregs. We'll flatten
    /// this into (vreg, range, label) tuples when done.
    debug_info: FxHashMap<ValueLabel, Vec<(InsnIndex, InsnIndex, VReg)>>,
}

/// Direction in which a VCodeBuilder builds VCode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VCodeBuildDirection {
    // TODO: add `Forward` once we need it and can test it adequately.
    /// Backward-build pass: we expect the producer to call `emit()`
    /// with instructions in reverse program order within each block.
    Backward,
}

impl<I: VCodeInst> VCodeBuilder<I> {
    /// Create a new VCodeBuilder.
    pub fn new(
        sigs: SigSet,
        abi: Callee<I::ABIMachineSpec>,
        emit_info: I::Info,
        block_order: BlockLoweringOrder,
        constants: VCodeConstants,
        direction: VCodeBuildDirection,
        log2_min_function_alignment: u8,
    ) -> Self {
        let vcode = VCode::new(
            sigs,
            abi,
            emit_info,
            block_order,
            constants,
            log2_min_function_alignment,
        );

        VCodeBuilder {
            vcode,
            direction,
            debug_info: FxHashMap::default(),
        }
    }

    pub fn init_retval_area(&mut self, vregs: &mut VRegAllocator<I>) -> CodegenResult<()> {
        self.vcode.abi.init_retval_area(&self.vcode.sigs, vregs)
    }

    /// Access the ABI object.
    pub fn abi(&self) -> &Callee<I::ABIMachineSpec> {
        &self.vcode.abi
    }

    /// Access the ABI object.
    pub fn abi_mut(&mut self) -> &mut Callee<I::ABIMachineSpec> {
        &mut self.vcode.abi
    }

    pub fn sigs(&self) -> &SigSet {
        &self.vcode.sigs
    }

    pub fn sigs_mut(&mut self) -> &mut SigSet {
        &mut self.vcode.sigs
    }

    /// Access to the BlockLoweringOrder object.
    pub fn block_order(&self) -> &BlockLoweringOrder {
        &self.vcode.block_order
    }

    /// Set the current block as the entry block.
    pub fn set_entry(&mut self, block: BlockIndex) {
        self.vcode.entry = block;
    }

    /// End the current basic block. Must be called after emitting vcode insts
    /// for IR insts and prior to ending the function (building the VCode).
    pub fn end_bb(&mut self) {
        let end_idx = self.vcode.insts.len();
        // Add the instruction index range to the list of blocks.
        self.vcode.block_ranges.push_end(end_idx);
        // End the successors list.
        let succ_end = self.vcode.block_succs.len();
        self.vcode.block_succ_range.push_end(succ_end);
        // End the blockparams list.
        let block_params_end = self.vcode.block_params.len();
        self.vcode.block_params_range.push_end(block_params_end);
        // End the branch blockparam args list.
        let branch_block_arg_succ_end = self.vcode.branch_block_arg_range.len();
        self.vcode
            .branch_block_arg_succ_range
            .push_end(branch_block_arg_succ_end);
    }

    pub fn add_block_param(&mut self, param: VirtualReg) {
        self.vcode.block_params.push(param.into());
    }

    fn add_branch_args_for_succ(&mut self, args: &[Reg]) {
        self.vcode
            .branch_block_args
            .extend(args.iter().map(|&arg| VReg::from(arg)));
        let end = self.vcode.branch_block_args.len();
        self.vcode.branch_block_arg_range.push_end(end);
    }

    /// Push an instruction for the current BB and current IR inst
    /// within the BB.
    pub fn push(&mut self, insn: I, loc: RelSourceLoc) {
        assert!(!insn.is_low_level_branch()); // These are not meant to be in VCode.
        self.vcode.insts.push(insn);
        self.vcode.srclocs.push(loc);
    }

    /// Add a successor block with branch args.
    pub fn add_succ(&mut self, block: BlockIndex, args: &[Reg]) {
        self.vcode.block_succs.push(block);
        self.add_branch_args_for_succ(args);
    }

    /// Add a debug value label to a register.
    pub fn add_value_label(&mut self, reg: Reg, label: ValueLabel) {
        // 1) In the reversed order, we consider the instructions
        //    that define ranges in the "debug_info" array to refer
        //    to the IP **after** them (when reversed):
        //      IP[2]__| Inst 3 |
        //      IP[1]__| Inst 2 |
        //      IP[0]__| Inst 1 |
        //             | Inst 0 |
        //    This is so that we can represent IP[<function start>],
        //    done at the cost of not being to represent IP[<function end>],
        //    which is OK since no values will be live at that point.
        // 2) The live range for "reg" begins at the current IP
        //    and continues until the next, in execution order,
        //    VReg that defines "label". Since the ranges are open
        //    at the end, the subtraction of 1 cancels out:
        //      [last..current IP] <=>
        //      [last..last emitted inst index] <=>
        //      [last..next_inst_index - 1] <=>
        //      [last..next_inst_index)
        //
        let next_inst_index = self.vcode.insts.len();
        if next_inst_index == 0 {
            // This would produce a defective [0..0) range.
            return;
        }
        let next_inst = InsnIndex::new(next_inst_index);
        let labels = self.debug_info.entry(label).or_insert_with(|| vec![]);
        let last = labels
            .last()
            .map(|(_start, end, _vreg)| *end)
            .unwrap_or(InsnIndex::new(0));
        labels.push((last, next_inst, reg.into()));
    }

    /// Access the constants.
    pub fn constants(&mut self) -> &mut VCodeConstants {
        &mut self.vcode.constants
    }

    fn compute_preds_from_succs(&mut self) {
        // Do a linear-time counting sort: first determine how many
        // times each block appears as a successor.
        let mut starts = vec![0u32; self.vcode.num_blocks()];
        for succ in &self.vcode.block_succs {
            starts[succ.index()] += 1;
        }

        // Determine for each block the starting index where that
        // block's predecessors should go. This is equivalent to the
        // ranges we need to store in block_pred_range.
        self.vcode.block_pred_range.reserve(starts.len());
        let mut end = 0;
        for count in starts.iter_mut() {
            let start = end;
            end += *count;
            *count = start;
            self.vcode.block_pred_range.push_end(end as usize);
        }
        let end = end as usize;
        debug_assert_eq!(end, self.vcode.block_succs.len());

        // Walk over the successors again, this time grouped by
        // predecessor, and push the predecessor at the current
        // starting position of each of its successors. We build
        // each group of predecessors in whatever order Ranges::iter
        // returns them; regalloc2 doesn't care.
        self.vcode.block_preds.resize(end, BlockIndex::invalid());
        for (pred, range) in self.vcode.block_succ_range.iter() {
            let pred = BlockIndex::new(pred);
            for succ in &self.vcode.block_succs[range] {
                let pos = &mut starts[succ.index()];
                self.vcode.block_preds[*pos as usize] = pred;
                *pos += 1;
            }
        }
        debug_assert!(self.vcode.block_preds.iter().all(|pred| pred.is_valid()));
    }

    /// Called once, when a build in Backward order is complete, to
    /// perform the overall reversal (into final forward order) and
    /// finalize metadata accordingly.
    fn reverse_and_finalize(&mut self, vregs: &VRegAllocator<I>) {
        let n_insts = self.vcode.insts.len();
        if n_insts == 0 {
            return;
        }

        // Reverse the per-block and per-inst sequences.
        self.vcode.block_ranges.reverse_index();
        self.vcode.block_ranges.reverse_target(n_insts);
        // block_params_range is indexed by block (and blocks were
        // traversed in reverse) so we reverse it; but block-param
        // sequences in the concatenated vec can remain in reverse
        // order (it is effectively an arena of arbitrarily-placed
        // referenced sequences).
        self.vcode.block_params_range.reverse_index();
        // Likewise, we reverse block_succ_range, but the block_succ
        // concatenated array can remain as-is.
        self.vcode.block_succ_range.reverse_index();
        self.vcode.insts.reverse();
        self.vcode.srclocs.reverse();
        // Likewise, branch_block_arg_succ_range is indexed by block
        // so must be reversed.
        self.vcode.branch_block_arg_succ_range.reverse_index();

        // To translate an instruction index *endpoint* in reversed
        // order to forward order, compute `n_insts - i`.
        //
        // Why not `n_insts - 1 - i`? That would be correct to
        // translate an individual instruction index (for ten insts 0
        // to 9 inclusive, inst 0 becomes 9, and inst 9 becomes
        // 0). But for the usual inclusive-start, exclusive-end range
        // idiom, inclusive starts become exclusive ends and
        // vice-versa, so e.g. an (inclusive) start of 0 becomes an
        // (exclusive) end of 10.
        let translate = |inst: InsnIndex| InsnIndex::new(n_insts - inst.index());

        // Generate debug-value labels based on per-label maps.
        for (label, tuples) in &self.debug_info {
            for &(start, end, vreg) in tuples {
                let vreg = vregs.resolve_vreg_alias(vreg);
                let fwd_start = translate(end);
                let fwd_end = translate(start);
                self.vcode
                    .debug_value_labels
                    .push((vreg, fwd_start, fwd_end, label.as_u32()));
            }
        }

        // Now sort debug value labels by VReg, as required
        // by regalloc2.
        self.vcode
            .debug_value_labels
            .sort_unstable_by_key(|(vreg, _, _, _)| *vreg);
    }

    fn collect_operands(&mut self, vregs: &VRegAllocator<I>) {
        let allocatable = PRegSet::from(self.vcode.abi.machine_env());
        for (i, insn) in self.vcode.insts.iter_mut().enumerate() {
            // Push operands from the instruction onto the operand list.
            //
            // We rename through the vreg alias table as we collect
            // the operands. This is better than a separate post-pass
            // over operands, because it has more cache locality:
            // operands only need to pass through L1 once. This is
            // also better than renaming instructions'
            // operands/registers while lowering, because here we only
            // need to do the `match` over the instruction to visit
            // its register fields (which is slow, branchy code) once.

            let mut op_collector =
                OperandCollector::new(&mut self.vcode.operands, allocatable, |vreg| {
                    vregs.resolve_vreg_alias(vreg)
                });
            insn.get_operands(&mut op_collector);
            let (ops, clobbers) = op_collector.finish();
            self.vcode.operand_ranges.push_end(ops);

            if clobbers != PRegSet::default() {
                self.vcode.clobbers.insert(InsnIndex::new(i), clobbers);
            }

            if let Some((dst, src)) = insn.is_move() {
                // We should never see non-virtual registers present in move
                // instructions.
                assert!(
                    src.is_virtual(),
                    "the real register {src:?} was used as the source of a move instruction"
                );
                assert!(
                    dst.to_reg().is_virtual(),
                    "the real register {:?} was used as the destination of a move instruction",
                    dst.to_reg()
                );
            }
        }

        // Translate blockparam args via the vreg aliases table as well.
        for arg in &mut self.vcode.branch_block_args {
            let new_arg = vregs.resolve_vreg_alias(*arg);
            trace!("operandcollector: block arg {:?} -> {:?}", arg, new_arg);
            *arg = new_arg;
        }
    }

    /// Build the final VCode.
    pub fn build(mut self, mut vregs: VRegAllocator<I>) -> VCode<I> {
        self.vcode.vreg_types = take(&mut vregs.vreg_types);

        if self.direction == VCodeBuildDirection::Backward {
            self.reverse_and_finalize(&vregs);
        }
        self.collect_operands(&vregs);

        self.compute_preds_from_succs();
        self.vcode.debug_value_labels.sort_unstable();

        // At this point, nothing in the vcode should mention any
        // VReg which has been aliased. All the appropriate rewriting
        // should have happened above. Just to be sure, let's
        // double-check each field which has vregs.
        // Note: can't easily check vcode.insts, resolved in collect_operands.
        // Operands are resolved in collect_operands.
        vregs.debug_assert_no_vreg_aliases(self.vcode.operands.iter().map(|op| op.vreg()));
        // Currently block params are never aliased to another vreg.
        vregs.debug_assert_no_vreg_aliases(self.vcode.block_params.iter().copied());
        // Branch block args are resolved in collect_operands.
        vregs.debug_assert_no_vreg_aliases(self.vcode.branch_block_args.iter().copied());
        // Debug value labels are resolved in reverse_and_finalize.
        vregs.debug_assert_no_vreg_aliases(
            self.vcode.debug_value_labels.iter().map(|&(vreg, ..)| vreg),
        );

        self.vcode
    }

    /// Add a user stack map for the associated instruction.
    pub fn add_user_stack_map(
        &mut self,
        inst: BackwardsInsnIndex,
        entries: &[ir::UserStackMapEntry],
    ) {
        let stack_map = ir::UserStackMap::new(entries, self.vcode.abi.sized_stackslot_offsets());
        let old_entry = self.vcode.user_stack_maps.insert(inst, stack_map);
        debug_assert!(old_entry.is_none());
    }

    /// Add debug tags for the associated instruction.
    pub fn add_debug_tags(&mut self, inst: BackwardsInsnIndex, entries: &[ir::DebugTag]) {
        let start = u32::try_from(self.vcode.debug_tag_pool.len()).unwrap();
        self.vcode.debug_tag_pool.extend(entries.iter().cloned());
        let end = u32::try_from(self.vcode.debug_tag_pool.len()).unwrap();
        self.vcode.debug_tags.insert(inst, start..end);
    }
}

const NO_INST_OFFSET: CodeOffset = u32::MAX;

impl<I: VCodeInst> VCode<I> {
    /// PLIRON: shrink-wrapping (`PLIRON_SHRINKWRAP=1`). The callee saves
    /// and frame allocation move from the entry block to the save block
    /// `S`, the nearest common dominator of every block that needs them; a
    /// return that `S` does not dominate tears down only the frame record.
    /// serde_json's `skip_to_escape` and pulldown-cmark's scanners return
    /// early through a ten-register save on most calls; LLVM's ShrinkWrap
    /// saves on the slow path only.
    ///
    /// A block needs the frame if it has an instruction that is not
    /// `pliron_frame_free` (calls, stack slots, `sp`/`fp`/`lr`), or an
    /// operand or regalloc move in a stack slot or a clobbered callee-saved
    /// register. `S` must not be the entry, must not sit on a cycle, must
    /// come before every block it dominates in emission order (unwind info
    /// is cumulative by address), and no return outside `S`'s dominance may
    /// be reachable from `S`. Returns `S` and, per block, whether a return
    /// there gets the full epilogue.
    fn pliron_shrink_wrap(&self, ra: &regalloc2::Output, order: &[BlockIndex]) -> Option<(BlockIndex, Vec<bool>)> {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_SHRINKWRAP").as_deref() == Ok("1")) {
            return None;
        }
        let fl = self.abi.frame_layout();
        if fl.clobber_size == 0 && fl.fixed_frame_storage_size + fl.outgoing_args_size == 0 {
            return None;
        }
        if fl.tail_args_size != fl.incoming_args_size || !self.abi.pliron_no_stack_checks() {
            return None;
        }
        let clob: crate::FxHashSet<regalloc2::PReg> =
            fl.clobbered_callee_saves.iter().map(|r| regalloc2::PReg::from(r.to_reg())).collect();
        let bad = |a: regalloc2::Allocation| a.is_stack() || a.as_reg().is_some_and(|r| clob.contains(&r));
        let nb = self.num_blocks();
        let mut needs = vec![false; nb];
        let mut rets = vec![false; nb];
        let mut insn_block = vec![0usize; self.insts.len()];
        for b in 0..nb {
            let r = self.block_ranges.get(b);
            for i in r.start..r.end {
                insn_block[i] = b;
                let inst = &self.insts[i];
                match inst.is_term() {
                    MachTerminator::Ret => {
                        rets[b] = true;
                        continue;
                    }
                    MachTerminator::RetCall => return None,
                    _ => {}
                }
                if !inst.pliron_frame_free() || ra.inst_allocs(InsnIndex::new(i)).iter().any(|&a| bad(a)) {
                    needs[b] = true;
                }
            }
        }
        for (pp, Edit::Move { from, to }) in &ra.edits {
            if bad(*from) || bad(*to) {
                needs[insn_block[pp.inst().index()]] = true;
            }
        }
        let entry = self.entry.index();
        let dbg = std::env::var_os("PLIRON_SHRINKWRAP_DEBUG").is_some();
        if dbg {
            let nl: Vec<usize> = (0..nb).filter(|&b| needs[b]).collect();
            eprintln!("shrink_wrap: needing blocks {:?} (entry needs: {})", &nl[..nl.len().min(20)], needs[entry]);
        }
        if needs[entry] || !needs.iter().any(|&x| x) {
            return None;
        }
        let succs = |b: usize| self.block_succs[self.block_succ_range.get(b)].iter().map(|s| s.index());
        // Reverse postorder from the entry, then iterative dominators.
        let mut rpo = Vec::with_capacity(nb);
        let mut seen = vec![false; nb];
        let mut stack = vec![(entry, false)];
        while let Some((b, done)) = stack.pop() {
            if done {
                rpo.push(b);
                continue;
            }
            if seen[b] {
                continue;
            }
            seen[b] = true;
            stack.push((b, true));
            for s in succs(b) {
                if !seen[s] {
                    stack.push((s, false));
                }
            }
        }
        rpo.reverse();
        let mut rpo_ix = vec![usize::MAX; nb];
        for (k, &b) in rpo.iter().enumerate() {
            rpo_ix[b] = k;
        }
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); nb];
        for &b in &rpo {
            for s in succs(b) {
                preds[s].push(b);
            }
        }
        let mut idom = vec![usize::MAX; nb];
        idom[entry] = entry;
        let intersect = |idom: &[usize], mut a: usize, mut b: usize| {
            while a != b {
                while rpo_ix[a] > rpo_ix[b] {
                    a = idom[a];
                }
                while rpo_ix[b] > rpo_ix[a] {
                    b = idom[b];
                }
            }
            a
        };
        let mut changed = true;
        while changed {
            changed = false;
            for &b in rpo.iter().skip(1) {
                let mut new = usize::MAX;
                for &p in &preds[b] {
                    if idom[p] != usize::MAX {
                        new = if new == usize::MAX { p } else { intersect(&idom, p, new) };
                    }
                }
                if new != usize::MAX && idom[b] != new {
                    idom[b] = new;
                    changed = true;
                }
            }
        }
        let mut sblk: Option<usize> = None;
        for b in 0..nb {
            if needs[b] && seen[b] {
                sblk = Some(match sblk {
                    None => b,
                    Some(x) => intersect(&idom, x, b),
                });
            }
        }
        let mut sb = sblk?;
        // Reachability from S; S on a cycle climbs the dominator tree until
        // it is not (the save runs once, before the loop).
        let reach_from = |sb: usize| {
            let mut reach = vec![false; nb];
            let mut wl: Vec<usize> = succs(sb).collect();
            while let Some(b) = wl.pop() {
                if reach[b] {
                    continue;
                }
                reach[b] = true;
                wl.extend(succs(b));
            }
            reach
        };
        let mut reach = reach_from(sb);
        let mut climb = 0;
        let dom_by = |idom: &[usize], s: usize, mut b: usize| loop {
            if b == s {
                return true;
            }
            if b == entry || idom[b] == usize::MAX {
                return false;
            }
            b = idom[b];
        };
        // Off cycles, and closed: every block reachable from S dominated by
        // it (shrink_split duplicated the shared returns).
        while sb != entry
            && climb < 32
            && (reach[sb] || (0..nb).any(|b| reach[b] && !dom_by(&idom, sb, b)))
        {
            sb = idom[sb];
            reach = reach_from(sb);
            climb += 1;
        }
        if dbg {
            eprintln!("shrink_wrap: save block {sb} (climbed {climb})");
        }
        if sb == entry {
            return None;
        }
        let dominated = |mut b: usize| loop {
            if b == sb {
                return true;
            }
            if b == entry || idom[b] == usize::MAX {
                return false;
            }
            b = idom[b];
        };
        // An outside return reachable from S rules it out.
        if reach[sb] {
            if dbg {
                eprintln!("shrink_wrap: save block on a cycle");
            }
            return None;
        }
        let mut full = vec![false; nb];
        for b in 0..nb {
            if rets[b] {
                if dominated(b) {
                    full[b] = true;
                } else if reach[b] {
                    if dbg {
                        eprintln!("shrink_wrap: return block {b} reachable from S but not dominated");
                    }
                    return None;
                }
            }
        }
        // Emission order: S before every block it dominates.
        let pos_s = order.iter().position(|b| b.index() == sb)?;
        for (k, b) in order.iter().enumerate() {
            if k < pos_s && b.index() != sb && seen[b.index()] && dominated(b.index()) {
                return None;
            }
        }
        Some((BlockIndex::new(sb), full))
    }

    /// PLIRON: the post-allocation rewrites applied at emission: reload PRE
    /// (`reload_pre_rounds`), then reload CSE (`reload_cse`).
    pub(crate) fn post_alloc(&self, ra: &regalloc2::Output) -> Option<regalloc2::Output> {
        match self.reload_pre_rounds(ra) {
            Some(o) => Some(self.reload_cse(&o).unwrap_or(o)),
            None => self.reload_cse(ra),
        }
    }

    /// PLIRON: reload CSE (`PLIRON_RELOADCSE=1`). regalloc2 gives each use
    /// of a spilled value its own reload, often into a fresh register, so
    /// one block may load the same slot several times
    /// (pulldown's `handle_inline_pass1` loaded `[sp, #0x3c0]` into
    /// `x0`, `x8`, `x12` and `x14` within a dozen instructions). Scanning
    /// each block forward, track which registers hold which slot's value;
    /// a reload into a register that already holds it is dropped, and a
    /// reload into another register becomes a register move (eliminated
    /// at rename on Apple cores, against a load's latency). A register's
    /// fact dies when it is written (def, clobber or edit), a slot's when
    /// it is stored. Rematerialized values are skipped: their slot may
    /// never be written.
    pub(crate) fn reload_cse(&self, ra: &regalloc2::Output) -> Option<regalloc2::Output> {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_RELOADCSE").as_deref() != Ok("0"))
            || !self.debug_value_labels.is_empty()
            || ra.edits.is_empty()
        {
            return None;
        }
        let mut remat = crate::FxHashSet::default();
        if std::env::var("PLIRON_REMAT").as_deref() != Ok("0") && !ra.edit_vregs.is_empty() {
            let probe = Writable::from_reg(Reg::from(regalloc2::PReg::new(0, regalloc2::RegClass::Int)));
            for (i, inst) in self.insts.iter().enumerate() {
                if inst.rematerialize(probe).is_none() {
                    continue;
                }
                if let [op] = &self.operands[self.operand_ranges.get(i)]
                    && op.kind() == OperandKind::Def
                {
                    remat.insert(op.vreg().vreg());
                }
            }
        }
        static SPILL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let spill_cse = *SPILL.get_or_init(|| std::env::var("PLIRON_SPILLCSE").as_deref() != Ok("0"));
        let vreg_of = |e: usize| ra.edit_vregs.get(e).copied().flatten();
        let is_remat = |e: usize| vreg_of(e).is_some_and(|v| remat.contains(&v.vreg()));
        // Per edit: keep (None), drop (Some(None)), or replace (Some(Some(edit))).
        let mut fix: Vec<Option<Option<Edit>>> = vec![None; ra.edits.len()];
        let mut changed = false;
        let nb = self.num_blocks();
        // Facts at each block's end, for single-predecessor successors.
        let mut at_end: Vec<Option<crate::FxHashMap<PReg, regalloc2::SpillSlot>>> = vec![None; nb];
        type Holds = crate::FxHashMap<PReg, regalloc2::SpillSlot>;
        // Facts on entry to `b`. With spill CSE (default; `PLIRON_SPILLCSE=0`
        // off): the intersection over
        // all predecessors (unscanned ones, `None`, are optimistic top), so
        // facts survive loop headers; iterated to a fixpoint below.
        // Otherwise: a single already-scanned predecessor's end facts.
        let entry = regalloc2::Function::entry_block(self).index();
        let in_state = |b: usize, at_end: &Vec<Option<Holds>>| -> Holds {
            let preds = regalloc2::Function::block_preds(self, BlockIndex::new(b));
            if !spill_cse {
                return match preds {
                    [p] if p.index() < b => at_end[p.index()].clone().unwrap_or_default(),
                    _ => Default::default(),
                };
            }
            if b == entry || preds.is_empty() {
                return Default::default();
            }
            let mut acc: Option<Holds> = None;
            for p in preds {
                let Some(h) = &at_end[p.index()] else { continue };
                acc = Some(match acc {
                    None => h.clone(),
                    Some(mut a) => {
                        a.retain(|k, v| h.get(k) == Some(v));
                        a
                    }
                });
            }
            acc.unwrap_or_default()
        };
        let mut scan = |b: usize, holds_in: Holds, fix: &mut Vec<Option<Option<Edit>>>, changed: &mut bool| -> Holds {
            let r = self.block_ranges.get(b);
            let mut e = ra
                .edits
                .partition_point(|&(p, _)| p < ProgPoint::before(InsnIndex::new(r.start)));
            // reg -> slot whose value it holds. A block entered only from
            // one already-scanned predecessor starts from that block's end.
            let mut holds = holds_in;
            let kill_reg = |holds: &mut crate::FxHashMap<PReg, regalloc2::SpillSlot>, p: PReg| {
                holds.remove(&p);
            };
            let kill_slot = |holds: &mut crate::FxHashMap<PReg, regalloc2::SpillSlot>, s: regalloc2::SpillSlot| {
                holds.retain(|_, v| *v != s);
            };
            let mut edit = |e: usize, holds: &mut crate::FxHashMap<PReg, regalloc2::SpillSlot>, fix: &mut Vec<Option<Option<Edit>>>, changed: &mut bool| {
                let Edit::Move { from, to } = ra.edits[e].1;
                let remat_e = is_remat(e);
                match (from.as_stack(), to.as_reg(), from.as_reg(), to.as_stack()) {
                    // Reload R <- [S].
                    (Some(s), Some(r), _, _) if !remat_e => {
                        if holds.get(&r) == Some(&s) {
                            fix[e] = Some(None);
                            *changed = true;
                            return;
                        }
                        let src = holds.iter().find(|(p, v)| **v == s && p.class() == r.class()).map(|(p, _)| *p);
                        if let Some(src) = src {
                            fix[e] = Some(Some(Edit::Move { from: Allocation::reg(src), to }));
                            *changed = true;
                        }
                        holds.insert(r, s);
                    }
                    // Spill [S] <- R: R and S now agree. If they already
                    // did (R was reloaded from S, or spilled there, and
                    // neither changed since), the store is dead:
                    // on by default (`PLIRON_SPILLCSE=0` keeps it). A loop's split range
                    // reloaded a loop invariant and stored it straight
                    // back every iteration (pulldown's special-byte scan
                    // with its callback inlined).
                    (_, _, Some(r), Some(s)) if !remat_e => {
                        if spill_cse && holds.get(&r) == Some(&s) {
                            fix[e] = Some(None);
                            *changed = true;
                            return;
                        }
                        kill_slot(holds, s);
                        holds.insert(r, s);
                    }
                    // Register move R2 <- R1: R2 inherits R1's fact.
                    (_, Some(r2), Some(r1), _) => {
                        let f = holds.get(&r1).copied();
                        kill_reg(holds, r2);
                        if let Some(s) = f {
                            holds.insert(r2, s);
                        }
                    }
                    _ => {
                        if let Some(r) = to.as_reg() {
                            kill_reg(holds, r);
                        }
                        if let Some(s) = to.as_stack() {
                            kill_slot(holds, s);
                        }
                    }
                }
            };
            for i in r.clone() {
                let ii = InsnIndex::new(i);
                while e < ra.edits.len() && ra.edits[e].0 <= ProgPoint::before(ii) {
                    edit(e, &mut holds, fix, changed);
                    e += 1;
                }
                for p in self.inst_clobbers(ii) {
                    kill_reg(&mut holds, p);
                }
                for (op, a) in self.operands[self.operand_ranges.get(i)].iter().zip(ra.inst_allocs(ii)) {
                    if op.kind() == OperandKind::Def {
                        if let Some(p) = a.as_reg() {
                            kill_reg(&mut holds, p);
                        }
                        if let Some(s) = a.as_stack() {
                            kill_slot(&mut holds, s);
                        }
                    }
                }
                while e < ra.edits.len() && ra.edits[e].0 <= ProgPoint::after(ii) {
                    edit(e, &mut holds, fix, changed);
                    e += 1;
                }
            }
            holds
        };
        if spill_cse {
            // Optimistic iteration: facts only shrink once a block is
            // scanned; apply nothing unless it settles.
            let mut scratch = vec![None; ra.edits.len()];
            let mut sc = false;
            let mut settled = false;
            for _ in 0..8 {
                let mut moved = false;
                for b in 0..nb {
                    let h = scan(b, in_state(b, &at_end), &mut scratch, &mut sc);
                    if at_end[b].as_ref() != Some(&h) {
                        at_end[b] = Some(h);
                        moved = true;
                    }
                }
                if !moved {
                    settled = true;
                    break;
                }
            }
            if !settled {
                return None;
            }
            for b in 0..nb {
                scan(b, in_state(b, &at_end), &mut fix, &mut changed);
            }
        } else {
            for b in 0..nb {
                let h = scan(b, in_state(b, &at_end), &mut fix, &mut changed);
                at_end[b] = Some(h);
            }
        }
        if !changed {
            return None;
        }
        let mut out = ra.clone();
        out.edits.clear();
        out.edit_vregs.clear();
        let keep_vregs = !ra.edit_vregs.is_empty();
        for (k, (p, ed)) in ra.edits.iter().enumerate() {
            match &fix[k] {
                None => out.edits.push((*p, ed.clone())),
                Some(None) => continue,
                Some(Some(n)) => out.edits.push((*p, n.clone())),
            }
            if keep_vregs {
                out.edit_vregs.push(ra.edit_vregs[k]);
            }
        }
        Some(out)
    }

    /// PLIRON: `reload_pre` repeated while it moves something, up to
    /// `PLIRON_RELOADPRE_ROUNDS` (default 3) times: a reload moved into a
    /// preheader that is itself in a loop can move again.
    pub(crate) fn reload_pre_rounds(&self, ra: &regalloc2::Output) -> Option<regalloc2::Output> {
        static ROUNDS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let n = *ROUNDS.get_or_init(|| {
            std::env::var("PLIRON_RELOADPRE_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(3)
        });
        let mut cur = self.reload_pre(ra)?;
        for _ in 1..n {
            match self.reload_pre(&cur) {
                Some(o) => cur = o,
                None => break,
            }
        }
        Some(cur)
    }

    /// PLIRON: reload PRE (`PLIRON_RELOADPRE=0` disables). regalloc2 puts
    /// a spilled value's reload right before each use, so a value spilled
    /// across a loop's cold path (a call) is reloaded on every trip around
    /// the hot path even when its register survives the trip (serde_json's
    /// escape scan rebuilt its table address per byte). A reload `R <- [S]`
    /// in block B, with nothing earlier in B touching R or writing S, is
    /// redundant along a predecessor that already ends with R holding the
    /// value. When at least one predecessor does, the reload moves to the
    /// ends of the others. Those are single-successor blocks (a block with
    /// several predecessors has no critical in-edges), so each runs at most
    /// as often as B and no path executes more loads. Returns the rewritten
    /// allocation, or `None` when nothing moved.
    pub(crate) fn reload_pre(&self, ra: &regalloc2::Output) -> Option<regalloc2::Output> {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_RELOADPRE").as_deref() != Ok("0"))
            || !self.debug_value_labels.is_empty()
            || ra.edits.is_empty()
        {
            return None;
        }
        #[derive(Clone, Copy)]
        enum It {
            I(usize),
            E(usize),
        }
        let nb = self.num_blocks();
        let mut items: Vec<Vec<It>> = Vec::with_capacity(nb);
        for b in 0..nb {
            let r = self.block_ranges.get(b);
            let mut v = Vec::with_capacity(r.len() + 4);
            let mut e = ra
                .edits
                .partition_point(|&(p, _)| p < ProgPoint::before(InsnIndex::new(r.start)));
            for i in r.clone() {
                let ii = InsnIndex::new(i);
                while e < ra.edits.len() && ra.edits[e].0 <= ProgPoint::before(ii) {
                    v.push(It::E(e));
                    e += 1;
                }
                v.push(It::I(i));
                while e < ra.edits.len() && ra.edits[e].0 <= ProgPoint::after(ii) {
                    v.push(It::E(e));
                    e += 1;
                }
            }
            items.push(v);
        }
        // Vregs whose reload is a rematerialization (see `remat_def` in
        // `emit`): any copy of one holds the same value, so the fact to
        // track is "R was last written with V", whatever happens to S.
        let mut remat = crate::FxHashSet::default();
        if std::env::var("PLIRON_REMAT").as_deref() != Ok("0") && !ra.edit_vregs.is_empty() {
            let probe = Writable::from_reg(Reg::from(regalloc2::PReg::new(0, regalloc2::RegClass::Int)));
            for (i, inst) in self.insts.iter().enumerate() {
                if inst.rematerialize(probe).is_none() {
                    continue;
                }
                if let [op] = &self.operands[self.operand_ranges.get(i)]
                    && op.kind() == OperandKind::Def
                {
                    remat.insert(op.vreg().vreg());
                }
            }
        }
        let vreg_of = |e: usize| ra.edit_vregs.get(e).copied().flatten();
        let ops = |i: usize| {
            self.operands[self.operand_ranges.get(i)]
                .iter()
                .zip(ra.inst_allocs(InsnIndex::new(i)))
        };
        let inst_writes = |i: usize, r: Option<PReg>, s: Option<regalloc2::SpillSlot>| {
            (r.is_some_and(|r| self.inst_clobbers(InsnIndex::new(i)).contains(r)))
                || ops(i).any(|(op, a)| {
                    op.kind() == OperandKind::Def
                        && ((r.is_some() && a.as_reg() == r) || (s.is_some() && a.as_stack() == s))
                })
        };
        // What `b` does to the fact, scanning back from its end: `Some(true)`
        // establishes it, `Some(false)` kills it, `None` passes it through.
        let local = |b: usize, r: PReg, s: regalloc2::SpillSlot, v: Option<VReg>| -> Option<bool> {
            let rm = v.filter(|v| remat.contains(&v.vreg()));
            for &it in items[b].iter().rev() {
                match it {
                    It::E(e) => {
                        let Edit::Move { from, to } = ra.edits[e].1;
                        if let Some(v) = rm {
                            if to.as_reg() == Some(r) {
                                return Some(vreg_of(e) == Some(v));
                            }
                        } else if to.as_reg() == Some(r) {
                            return Some(from.as_stack() == Some(s));
                        } else if to.as_stack() == Some(s) {
                            return Some(from.as_reg() == Some(r));
                        }
                    }
                    It::I(i) => {
                        if let Some(v) = rm {
                            if inst_writes(i, Some(r), None) {
                                let def_v = ops(i).any(|(op, a)| {
                                    op.kind() == OperandKind::Def && a.as_reg() == Some(r) && op.vreg() == v
                                });
                                return Some(
                                    def_v && !self.inst_clobbers(InsnIndex::new(i)).contains(r),
                                );
                            }
                        } else if inst_writes(i, Some(r), Some(s)) {
                            return Some(false);
                        }
                    }
                }
            }
            None
        };
        const REGION: usize = 64;
        let mut dead = crate::FxHashSet::default();
        let mut added: Vec<(ProgPoint, Edit, Option<VReg>)> = vec![];
        let mut budget = 512usize;
        for b in 0..nb {
            let preds = self.block_preds(BlockIndex::new(b));
            if BlockIndex::new(b) == self.entry || preds.is_empty() {
                continue;
            }
            let mut touched = PRegSet::empty();
            let mut wslots = crate::FxHashSet::default();
            for &it in &items[b] {
                match it {
                    It::E(e) => {
                        let Edit::Move { from, to } = ra.edits[e].1;
                        if let (Some(s), Some(r)) = (from.as_stack(), to.as_reg())
                            && !touched.contains(r)
                            && !wslots.contains(&s)
                            && budget > 0
                        {
                            budget -= 1;
                            let v = vreg_of(e);
                            // Greatest fixpoint of "R holds it at the end"
                            // over the blocks reachable backward from the
                            // predecessors through pass-through blocks.
                            let mut val: crate::FxHashMap<usize, bool> = crate::FxHashMap::default();
                            let mut work: Vec<usize> = preds.iter().map(|p| p.index()).collect();
                            let mut through = vec![];
                            let mut ok = true;
                            while let Some(x) = work.pop() {
                                if val.contains_key(&x) {
                                    continue;
                                }
                                if val.len() >= REGION {
                                    ok = false;
                                    break;
                                }
                                match local(x, r, s, v) {
                                    Some(g) => {
                                        val.insert(x, g);
                                    }
                                    None => {
                                        let xp = self.block_preds(BlockIndex::new(x));
                                        let entry = BlockIndex::new(x) == self.entry || xp.is_empty();
                                        val.insert(x, !entry);
                                        if !entry {
                                            through.push(x);
                                            work.extend(xp.iter().map(|p| p.index()));
                                        }
                                    }
                                }
                            }
                            if ok {
                                loop {
                                    let mut changed = false;
                                    for &x in &through {
                                        if val[&x]
                                            && self
                                                .block_preds(BlockIndex::new(x))
                                                .iter()
                                                .any(|p| !val[&p.index()])
                                        {
                                            val.insert(x, false);
                                            changed = true;
                                        }
                                    }
                                    if !changed {
                                        break;
                                    }
                                }
                                let have: Vec<bool> = preds.iter().map(|p| val[&p.index()]).collect();
                                let movable = have.iter().any(|&h| h)
                                    && preds.iter().zip(&have).all(|(p, &h)| {
                                        h || {
                                            let pr = self.block_ranges.get(p.index());
                                            let t = pr.end - 1;
                                            self.block_succs(*p).len() == 1
                                                && self.insts[t].is_term() == MachTerminator::Branch
                                                && !ops(t).any(|(_, a)| a.as_reg() == Some(r))
                                        }
                                    });
                                if movable {
                                    dead.insert(e);
                                    for (p, &h) in preds.iter().zip(&have) {
                                        if !h {
                                            let t = self.block_ranges.get(p.index()).end - 1;
                                            added.push((
                                                ProgPoint::before(InsnIndex::new(t)),
                                                Edit::Move { from, to },
                                                v,
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(r) = from.as_reg() {
                            touched.add(r);
                        }
                        if let Some(r) = to.as_reg() {
                            touched.add(r);
                        }
                        if let Some(s) = to.as_stack() {
                            wslots.insert(s);
                        }
                    }
                    It::I(i) => {
                        touched.union_from(self.inst_clobbers(InsnIndex::new(i)));
                        for (op, a) in ops(i) {
                            if let Some(r) = a.as_reg() {
                                touched.add(r);
                            }
                            if op.kind() == OperandKind::Def
                                && let Some(s) = a.as_stack()
                            {
                                wslots.insert(s);
                            }
                        }
                    }
                }
            }
        }
        if dead.is_empty() {
            return None;
        }
        if *DEBUG.get_or_init(|| std::env::var_os("PLIRON_RELOADPRE_DEBUG").is_some()) {
            eprintln!("reload_pre: moved {} reloads, {} inserted", dead.len(), added.len());
        }
        let has_v = !ra.edit_vregs.is_empty();
        let mut all: Vec<(ProgPoint, Edit, Option<VReg>)> = ra
            .edits
            .iter()
            .enumerate()
            .filter(|(e, _)| !dead.contains(e))
            .map(|(e, (p, ed))| (*p, ed.clone(), vreg_of(e)))
            .collect();
        all.extend(added);
        // Stable: an inserted reload lands after the edits already at the
        // predecessor's end (its edge moves).
        all.sort_by_key(|x| x.0);
        let mut out = ra.clone();
        out.edits = all.iter().map(|(p, ed, _)| (*p, ed.clone())).collect();
        out.edit_vregs = if has_v { all.iter().map(|x| x.2).collect() } else { vec![] };
        Some(out)
    }

    /// PLIRON: fill `block_freq` (see the field) before regalloc when
    /// `PLIRON_RA2_FREQ` is set: the same graph, cold marks and branch
    /// hints the emission-time placement uses.
    pub fn compute_block_freq(&mut self) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        // Unset = on (regalloc2's mode 3); `PLIRON_RA2_FREQ=0` turns it off.
        if !*ON.get_or_init(|| std::env::var("PLIRON_RA2_FREQ").map_or(true, |v| v != "0")) {
            return;
        }
        let nb = self.num_blocks();
        let succs: Vec<Vec<usize>> = (0..nb)
            .map(|b| {
                self.block_succs(BlockIndex::new(b))
                    .iter()
                    .map(|s| s.index())
                    .filter(|&s| s < nb)
                    .collect()
            })
            .collect();
        let cold: Vec<bool> = (0..nb).map(|b| self.block_order.is_cold(BlockIndex::new(b))).collect();
        let unlikely: Vec<Vec<bool>> = (0..nb)
            .map(|b| (0..succs[b].len()).map(|k| self.block_order.is_unlikely_succ(BlockIndex::new(b), k)).collect())
            .collect();
        if let Some(freq) = super::placement::frequencies(self.entry.index(), &succs, &cold, &unlikely) {
            self.block_freq = freq.into_iter().map(|f| f as f32).collect();
        }
    }

    /// PLIRON: fill `remat_vregs` (see the field) before regalloc, and
    /// pick up shrink-wrapping's split block.
    pub fn compute_remat_vregs(&mut self) {
        self.no_merge_block = PLIRON_SPLIT_BLOCK
            .with(|c| c.get())
            .and_then(|b| self.block_order.lowered_index_for_block(b));
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_RA2_REMATW").as_deref() == Ok("1"))
            || std::env::var("PLIRON_REMAT").as_deref() == Ok("0")
        {
            return;
        }
        let probe = Writable::from_reg(Reg::from(regalloc2::PReg::new(0, regalloc2::RegClass::Int)));
        let mut defs = vec![0u8; self.vreg_types.len()];
        let mut remat = vec![false; self.vreg_types.len()];
        for (i, inst) in self.insts.iter().enumerate() {
            let ops = &self.operands[self.operand_ranges.get(i)];
            let r = inst.rematerialize(probe).is_some();
            for op in ops {
                if op.kind() == regalloc2::OperandKind::Def {
                    let v = op.vreg().vreg();
                    if v < defs.len() {
                        defs[v] = defs[v].saturating_add(1);
                        remat[v] = r && ops.len() == 1;
                    }
                }
            }
        }
        for (v, r) in remat.iter_mut().enumerate() {
            *r &= defs[v] == 1;
        }
        self.remat_vregs = remat;
    }

    /// New empty VCode.
    fn new(
        sigs: SigSet,
        abi: Callee<I::ABIMachineSpec>,
        emit_info: I::Info,
        block_order: BlockLoweringOrder,
        constants: VCodeConstants,
        log2_min_function_alignment: u8,
    ) -> Self {
        let n_blocks = block_order.lowered_order().len();
        VCode {
            sigs,
            vreg_types: vec![],
            remat_vregs: vec![],
            block_freq: vec![],
            no_merge_block: None,
            insts: Vec::with_capacity(10 * n_blocks),
            user_stack_maps: FxHashMap::default(),
            debug_tags: FxHashMap::default(),
            debug_tag_pool: vec![],
            operands: Vec::with_capacity(30 * n_blocks),
            operand_ranges: Ranges::with_capacity(10 * n_blocks),
            clobbers: FxHashMap::default(),
            srclocs: Vec::with_capacity(10 * n_blocks),
            entry: BlockIndex::new(0),
            block_ranges: Ranges::with_capacity(n_blocks),
            block_succ_range: Ranges::with_capacity(n_blocks),
            block_succs: Vec::with_capacity(n_blocks),
            block_pred_range: Ranges::default(),
            block_preds: Vec::new(),
            block_params_range: Ranges::with_capacity(n_blocks),
            block_params: Vec::with_capacity(5 * n_blocks),
            branch_block_args: Vec::with_capacity(10 * n_blocks),
            branch_block_arg_range: Ranges::with_capacity(2 * n_blocks),
            branch_block_arg_succ_range: Ranges::with_capacity(n_blocks),
            block_order,
            abi,
            emit_info,
            constants,
            debug_value_labels: vec![],
            log2_min_function_alignment,
        }
    }

    /// Get the number of blocks. Block indices will be in the range `0 ..
    /// (self.num_blocks() - 1)`.
    pub fn num_blocks(&self) -> usize {
        self.block_ranges.len()
    }

    /// PLIRON: blocks that only the normal-return edge of a noreturn
    /// `try_call` reaches (`MachInst::noreturn_continuation`: its lone
    /// `NORETURN_CALL` trap is emitted inline or elided, so the call never
    /// branches there) — the continuation block itself, or the split-edge
    /// block in front of it. `None` when the function has no such edge.
    /// One forward walk from the entry over the other edges.
    fn noreturn_dead_blocks(&self) -> Option<Vec<bool>> {
        let n = self.num_blocks();
        let dead_succ = |b: usize| -> Option<BlockIndex> {
            let r = self.block_ranges.get(b);
            if r.end == r.start {
                return None;
            }
            let l = self.insts[r.end - 1].noreturn_continuation()?;
            let mut hit = None;
            for &s in &self.block_succs[self.block_succ_range.get(b)] {
                if MachLabel::from_block(s) == l {
                    if hit.is_some() {
                        return None;
                    }
                    hit = Some(s);
                }
            }
            hit
        };
        let dead: Vec<Option<BlockIndex>> = (0..n).map(dead_succ).collect();
        if dead.iter().all(|d| d.is_none()) {
            return None;
        }
        let mut live = vec![false; n];
        let mut stack = vec![self.entry];
        live[self.entry.index()] = true;
        while let Some(b) = stack.pop() {
            for &s in &self.block_succs[self.block_succ_range.get(b.index())] {
                if Some(s) != dead[b.index()] && !live[s.index()] {
                    live[s.index()] = true;
                    stack.push(s);
                }
            }
        }
        Some(live.into_iter().map(|l| !l).collect())
    }

    /// The number of lowered instructions.
    pub fn num_insts(&self) -> usize {
        self.insts.len()
    }

    fn compute_clobbers_and_function_calls(
        &self,
        regalloc: &regalloc2::Output,
    ) -> (Vec<Writable<RealReg>>, FunctionCalls) {
        let mut clobbered = PRegSet::default();
        let mut function_calls = FunctionCalls::None;

        // All moves are included in clobbers.
        for (_, Edit::Move { to, .. }) in &regalloc.edits {
            if let Some(preg) = to.as_reg() {
                clobbered.add(preg);
            }
        }

        for (i, range) in self.operand_ranges.iter() {
            let operands = &self.operands[range.clone()];
            let allocs = &regalloc.allocs[range];
            for (operand, alloc) in operands.iter().zip(allocs.iter()) {
                if operand.kind() == OperandKind::Def {
                    if let Some(preg) = alloc.as_reg() {
                        clobbered.add(preg);
                    }
                }
            }

            function_calls.update(self.insts[i].call_type());

            // Also add explicitly-clobbered registers.
            //
            // Skip merging this instruction's clobber list if not
            // "included in clobbers" as per the MachInst. (Some
            // backends use this to implement ABI specifics; e.g.,
            // excluding calls of the same ABI as the current function
            // from clobbers, because by definition everything
            // clobbered by the call can be clobbered by this function
            // without saving as well.
            //
            // This is important for a particular optimization: when
            // some registers are "half-clobbered", e.g. vector/float
            // registers on aarch64, we want them to be seen as
            // clobbered by regalloc so it avoids carrying values
            // across calls in these registers but not seen as
            // clobbered by prologue generation here (because the
            // actual half-clobber implied by the clobber list fits
            // within the clobbers that we allow without
            // clobber-saves).
            if self.insts[i].is_included_in_clobbers() {
                if let Some(&inst_clobbered) = self.clobbers.get(&InsnIndex::new(i)) {
                    clobbered.union_from(inst_clobbered);
                }
            }
        }

        let clobbered_regs = clobbered
            .into_iter()
            .map(|preg| Writable::from_reg(RealReg::from(preg)))
            .collect();

        (clobbered_regs, function_calls)
    }

    /// Emit the instructions to a `MachBuffer`, containing fixed-up
    /// code and external reloc/trap/etc. records ready for use. Takes
    /// the regalloc results as well.
    ///
    /// Returns the machine code itself, and optionally metadata
    /// and/or a disassembly, as an `EmitResult`. The `VCode` itself
    /// is consumed by the emission process.
    pub fn emit(
        mut self,
        regalloc: &regalloc2::Output,
        want_disasm: bool,
        flags: &settings::Flags,
        ctrl_plane: &mut ControlPlane,
    ) -> CodegenResult<EmitResult>
    where
        I: VCodeInst,
    {
        let _tt = timing::vcode_emit();
        let pre_out;
        let regalloc = match self.post_alloc(regalloc) {
            Some(o) => {
                pre_out = o;
                &pre_out
            }
            None => regalloc,
        };
        let mut buffer = MachBuffer::new();
        buffer.set_log2_min_function_alignment(self.log2_min_function_alignment);
        let mut bb_starts: Vec<Option<CodeOffset>> = vec![];

        // The first M MachLabels are reserved for block indices.
        buffer.reserve_labels_for_blocks(self.num_blocks());

        // Register all allocated constants with the `MachBuffer` to ensure that
        // any references to the constants during instructions can be handled
        // correctly.
        buffer.register_constants(&self.constants);

        // Construct the final order we emit code in: cold blocks at the end.
        let mut final_order: SmallVec<[BlockIndex; 16]> = smallvec![];
        let mut cold_blocks: SmallVec<[BlockIndex; 16]> = smallvec![];
        for block in 0..self.num_blocks() {
            let block = BlockIndex::new(block);
            if self.block_order.is_cold(block) {
                cold_blocks.push(block);
            } else {
                final_order.push(block);
            }
        }
        // Count edits per block ahead of time; this is needed for
        // lookahead island emission. (We could derive it per-block
        // with binary search in the edit list, but it's more
        // efficient to do it in one pass here.)
        let mut ra_edits_per_block: SmallVec<[u32; 64]> = smallvec![];
        let mut edit_idx = 0;
        for block in 0..self.num_blocks() {
            let end_inst = InsnIndex::new(self.block_ranges.get(block).end);
            let start_edit_idx = edit_idx;
            while edit_idx < regalloc.edits.len() && regalloc.edits[edit_idx].0.inst() < end_inst {
                edit_idx += 1;
            }
            let end_edit_idx = edit_idx;
            ra_edits_per_block.push((end_edit_idx - start_edit_idx) as u32);
        }

        // PLIRON: chain-based placement of the hot blocks with static
        // branch probabilities (`machinst/placement.rs`, on by default,
        // `PLIRON_PLACE=0` restores the lowering order); cold blocks stay
        // at the end in their original order.
        static PLACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        // Blocks that start a placed loop chain: aligned like loop heads
        // (a rotated loop's chain starts at a body block, not the header).
        let mut chain_heads: Option<Vec<bool>> = None;
        if *PLACE.get_or_init(|| std::env::var("PLIRON_PLACE").as_deref() != Ok("0")) {
            let nb = self.num_blocks();
            let succs: Vec<Vec<usize>> = (0..nb)
                .map(|b| {
                    self.block_succs(BlockIndex::new(b))
                        .iter()
                        .map(|s| s.index())
                        .filter(|&s| s < nb)
                        .collect()
                })
                .collect();
            let cold: Vec<bool> = (0..nb).map(|b| self.block_order.is_cold(BlockIndex::new(b))).collect();
            // A block that is only an unconditional jump, with no regalloc
            // edits, emits nothing: `MachBuffer` aliases its label to the
            // target, so it is free wherever it lands.
            let empty: Vec<bool> = (0..nb)
                .map(|b| {
                    let r = self.block_ranges.get(b);
                    r.len() == 1
                        && ra_edits_per_block[b] == 0
                        && succs[b].len() == 1
                        && self.insts[r.start].is_term() == MachTerminator::Branch
                })
                .collect();
            let unlikely: Vec<Vec<bool>> = (0..nb)
                .map(|b| (0..succs[b].len()).map(|k| self.block_order.is_unlikely_succ(BlockIndex::new(b), k)).collect())
                .collect();
            if let Some((order, heads)) = super::placement::place(self.entry.index(), &succs, &cold, &empty, &unlikely) {
                if order.len() == final_order.len() {
                    final_order = order.into_iter().map(BlockIndex::new).collect();
                    chain_heads = Some(heads);
                }
            }
        }
        final_order.extend(cold_blocks.clone());

        // PLIRON: mark blocks that are targets of backward edges in the
        // final emission order — loop headers — so their first instruction
        // can be 16-byte aligned below (the back-edge jumps to the block's
        // label, which binds after the padding, so the padding costs a few
        // NOPs only on the fallthrough entry). `PLIRON_ALIGN=0` disables.
        static ALIGN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let align_loops =
            *ALIGN.get_or_init(|| std::env::var("PLIRON_ALIGN").map_or(true, |v| v != "0"));
        let mut loop_head = vec![false; self.num_blocks()];
        // Emission position of each loop head's last back-edge source.
        let mut loop_end = vec![0usize; self.num_blocks()];
        let mut order_of = vec![usize::MAX; self.num_blocks()];
        if align_loops {
            for (i, &b) in final_order.iter().enumerate() {
                order_of[b.index()] = i;
            }
            for (pred, range) in self.block_succ_range.iter() {
                for succ in &self.block_succs[range] {
                    let s = succ.index();
                    // PLIRON: under placement, a head whose layout
                    // predecessor branches conditionally into it is not
                    // padded: the pad would turn that fallthrough into a
                    // taken branch plus a jump.
                    let cond_fallthrough = chain_heads.is_some() && order_of[s] > 0 && {
                        let p = final_order[order_of[s] - 1];
                        let pr = self.block_succ_range.get(p.index());
                        let ps = &self.block_succs[pr];
                        // Through empty jump-only blocks (label-aliased):
                        // a critical edge to the head goes via one.
                        let reaches = |t: BlockIndex| -> bool {
                            let mut t = t;
                            for _ in 0..8 {
                                if t.index() == s {
                                    return true;
                                }
                                let r = self.block_ranges.get(t.index());
                                let tr = self.block_succ_range.get(t.index());
                                let ts = &self.block_succs[tr];
                                if r.len() == 1 && ts.len() == 1 && ra_edits_per_block[t.index()] == 0 {
                                    t = ts[0];
                                } else {
                                    return false;
                                }
                            }
                            false
                        };
                        ps.len() >= 2 && ps.iter().any(|&t| reaches(t))
                    };
                    if s < self.num_blocks()
                        && order_of[s] <= order_of[pred]
                        && !cond_fallthrough
                        && (self.block_order.is_back_edge_target(BlockIndex::new(s))
                            || chain_heads.as_ref().is_some_and(|h| h[s]))
                    {
                        loop_head[s] = true;
                        loop_end[s] = loop_end[s].max(order_of[pred]);
                    }
                }
            }
        }

        // Compute/save info we need for the prologue: clobbers and
        // number of spillslots.
        //
        // We clone `abi` here because we will mutate it as we
        // generate the prologue and set other info, but we can't
        // mutate `VCode`. The info it usually carries prior to
        // setting clobbers is fairly minimal so this should be
        // relatively cheap.
        let ti = std::time::Instant::now();
        let (clobbers, function_calls) = self.compute_clobbers_and_function_calls(regalloc);
        self.abi.compute_frame_layout(
            &self.sigs,
            regalloc.num_spillslots,
            clobbers,
            function_calls,
        )?;
        let t_frame = ti.elapsed();

        // Emit blocks.
        let mut cur_srcloc = None;
        let mut last_offset = None;
        let mut inst_offsets = vec![];
        let mut state = I::State::new(&self.abi, core::mem::take(ctrl_plane));

        let mut disasm = String::new();
        // Inst indices replaced by a zero-size nop via pair fusion;
        // `applied` tracks insts whose regalloc results were already
        // written back by lookahead.
        // PLIRON: vreg -> its defining inst, for single-def insts that can
        // be re-executed at a reload (`MachInst::rematerialize`): constants
        // and symbol addresses. A spilled one is rematerialized where
        // regalloc2 would load it (cheaper on the address/dep chain than a
        // load, and it frees the hoisting decisions from spill risk).
        // `PLIRON_REMAT=0` disables.
        static REMAT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let remat_def: Option<crate::FxHashMap<usize, usize>> =
            if *REMAT.get_or_init(|| std::env::var("PLIRON_REMAT").as_deref() != Ok("0"))
                && !regalloc.edit_vregs.is_empty()
            {
                let mut m = crate::FxHashMap::default();
                let probe = Writable::from_reg(Reg::from(regalloc2::PReg::new(0, regalloc2::RegClass::Int)));
                for (i, inst) in self.insts.iter().enumerate() {
                    if inst.rematerialize(probe).is_none() {
                        continue;
                    }
                    let ops = &self.operands[self.operand_ranges.get(i)];
                    if let [op] = ops
                        && op.kind() == regalloc2::OperandKind::Def
                    {
                        m.insert(op.vreg().vreg(), i);
                    }
                }
                Some(m)
            } else {
                None
            };
        // PLIRON: spill slots whose every read is a rematerialized reload:
        // stores into them are dead (`PLIRON_REMAT_STORE=0` keeps them).
        // A slot can be shared by several vregs (a spillset), so the test
        // is per slot: any plain reload or instruction operand in the slot
        // keeps all its stores. Debug value labels may name spill slots.
        static REMAT_STORE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let dead_slots: crate::FxHashSet<usize> = match &remat_def {
            Some(m)
                if self.debug_value_labels.is_empty()
                    && *REMAT_STORE
                        .get_or_init(|| std::env::var("PLIRON_REMAT_STORE").as_deref() != Ok("0")) =>
            {
                let mut written = crate::FxHashSet::default();
                let mut read = crate::FxHashSet::default();
                let probe = Writable::from_reg(Reg::from(regalloc2::PReg::new(0, regalloc2::RegClass::Int)));
                for (ei, (_, Edit::Move { from, to })) in regalloc.edits.iter().enumerate() {
                    if let Some(sl) = to.as_stack() {
                        written.insert(sl.index());
                    }
                    if let Some(sl) = from.as_stack() {
                        let remat = to.is_reg()
                            && regalloc
                                .edit_vregs
                                .get(ei)
                                .copied()
                                .flatten()
                                .and_then(|v| m.get(&v.vreg()))
                                .is_some_and(|&di| self.insts[di].rematerialize(probe).is_some());
                        if !remat {
                            read.insert(sl.index());
                        }
                    }
                }
                for i in 0..self.insts.len() {
                    for a in regalloc.inst_allocs(InsnIndex::new(i)) {
                        if let Some(sl) = a.as_stack() {
                            read.insert(sl.index());
                        }
                    }
                }
                written.difference(&read).copied().collect()
            }
            _ => Default::default(),
        };
        let mut squashed = vec![false; self.insts.len()];
        let mut applied = vec![false; self.insts.len()];

        if !self.debug_value_labels.is_empty() {
            inst_offsets.resize(self.insts.len(), NO_INST_OFFSET);
        }

        let is_forward_edge_cfi_enabled = self.abi.is_forward_edge_cfi_enabled();
        let mut bb_padding = match flags.bb_padding_log2_minus_one() {
            0 => Vec::new(),
            n => vec![0; 1 << (n - 1)],
        };
        let mut total_bb_padding = 0;

        // PLIRON: hoisted out of the per-block loop — a fresh
        // `insts.len()`-sized table per block is quadratic
        // (block_count × inst_count memset) on functions with many
        // blocks. Entries for a block's insts are repopulated by the
        // eager `apply_allocs` pass below before any hazard check
        // reads them, so cross-block leftovers are unreachable.
        let mut preg_ops: Vec<(SmallVec<[Reg; 4]>, SmallVec<[Reg; 4]>)> =
            vec![(smallvec![], smallvec![]); self.insts.len()];

        // PLIRON (`PLIRON_EDITPAIR=0` off, default on): fuse two
        // consecutive same-direction spill/reload edits to adjacent slots
        // into `ldp`/`stp`. Spill traffic is emitted as `Edit::Move`s,
        // which the inst-level pair fusion below never sees.
        static EDITPAIR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let editpair =
            *EDITPAIR.get_or_init(|| std::env::var("PLIRON_EDITPAIR").as_deref() != Ok("0"));
        let mut ep_mem = 0usize;
        let mut ep_next = 0usize;
        let mut ep_dir = 0usize;
        let mut ep_fused = 0usize;
        let mut ep_r2r = 0usize;

        // PLIRON (`PLIRON_MCP=0` off, default on): post-alloc copy
        // propagation — a reg-reg move whose destination provably already
        // holds its source's value class is dead (the `mov x5,x2;
        // mov x2,x5` residue parallel-move resolution leaves). `vclass`
        // maps preg index to a value-class id; movs propagate the class,
        // every other def freshens it, calls and block boundaries clear
        // the map. LLVM's MachineCopyPropagation does the same sweep.
        static MCP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let mcp = *MCP.get_or_init(|| std::env::var("PLIRON_MCP").as_deref() != Ok("0"));
        // PLIRON: only allocatable Int-class pregs participate. Float-class
        // movs have mixed widths (`fmov d` zeroes the upper half, `fmov v`
        // copies 128 bits) so a single class cannot represent equality for
        // both; non-allocatable regs (sp, fp, lr, xzr, pinned) take writes
        // that never appear as operand defs (writeback amodes in the
        // prologue/epilogue), which would leave stale classes.
        let mcp_allocset = if mcp {
            Some(PRegSet::from(self.abi.machine_env()))
        } else {
            None
        };
        let mcp_key = |r: Reg| -> Option<Reg> {
            let preg = r.to_real_reg()?;
            (preg.class() == RegClass::Int
                && mcp_allocset.as_ref().unwrap().contains(preg.into()))
            .then_some(r)
        };
        let mut vclass: FxHashMap<Reg, u32> = FxHashMap::default();
        let mut next_class = 1u32;
        let mut mcp_dead = 0usize;
        // PLIRON (`PLIRON_MCPBB=0` off, default on): per-block exit
        // states seed a block's entry facts from already-emitted preds.
        // Dropping a move requires the fact to hold on every entry
        // edge, so the seed is the intersection of all preds' exit
        // states; a pred emitted later (a backedge, or a block ordered
        // after its successor) contributes nothing and empties the seed.
        static MCPBB: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let mcpbb = *MCPBB.get_or_init(|| std::env::var("PLIRON_MCPBB").as_deref() != Ok("0"));
        // PLIRON FLAGDUP (`PLIRON_FLAGDUP=0` off, default on): elide a
        // compare/test that recomputes an identical, still-live flag
        // state — the same `flag_kind()` Cmp key emitted earlier in the
        // block, only flag-preserving (`Neutral`) insts intervening, and
        // no rewrite of the compare's operand pregs. `flag_state` holds
        // `(key, operand pregs)`; every inst funnelling through
        // `do_emit` (stream insts, RA edits, island jumps, block
        // markers) keeps it in sync, and defs invalidate it via
        // `preg_ops` (stream insts) or the edit destination (moves,
        // reloads, remats). Terminators, calls, block boundaries and
        // anything `flag_kind` does not model conservatively drop it.
        static FLAGDUP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let flagdup =
            *FLAGDUP.get_or_init(|| std::env::var("PLIRON_FLAGDUP").as_deref() != Ok("0"));
        // PLIRON FLAGDUP_BISECT (`PLIRON_FLAGDUP_BISECT=N`): allow only
        // the first N elisions process-wide — a miscompile bisector.
        static FLAGDUP_BISECT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let flagdup_bisect = *FLAGDUP_BISECT.get_or_init(|| {
            std::env::var("PLIRON_FLAGDUP_BISECT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(u64::MAX)
        });
        static FLAGDUP_COUNT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        // PLIRON FLAGDUP_FORMS (`PLIRON_FLAGDUP_FORMS=rc,rt,ic,it`):
        // which compare forms may dedup — reg-reg cmp / reg-reg test /
        // reg-imm cmp / reg-imm test. Debug aid; default all.
        static FLAGDUP_FORMS: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
        let flagdup_forms = *FLAGDUP_FORMS.get_or_init(|| {
            std::env::var("PLIRON_FLAGDUP_FORMS").map_or(0xf, |v| {
                v.split(',').fold(0u8, |m, s| {
                    m | match s.trim() {
                        "rc" => 0x1,
                        "rt" => 0x2,
                        "ic" => 0x4,
                        "it" => 0x8,
                        _ => 0,
                    }
                })
            })
        });
        // PLIRON FLAGDUP_FN (`PLIRON_FLAGDUP_FN=<substr>`): restrict
        // elision to functions whose name contains the substring
        // (`!<substr>` inverts). Miscompile bisect aid.
        static FLAGDUP_FN: std::sync::OnceLock<Option<(bool, String)>> =
            std::sync::OnceLock::new();
        let flagdup_fn = FLAGDUP_FN.get_or_init(|| {
            std::env::var("PLIRON_FLAGDUP_FN").ok().map(|v| {
                if let Some(rest) = v.strip_prefix('!') {
                    (false, alloc::string::String::from(rest))
                } else {
                    (true, v)
                }
            })
        });
        let flagdup_fn_ok = flagdup_fn.as_ref().map_or(true, |(allow, pat)| {
            let m = crate::PLIRON_FN_NAME.with(|s| s.borrow().contains(pat.as_str()));
            m == *allow
        });
        // PLIRON FLAGDUP_LOCAL (`PLIRON_FLAGDUP_LOCAL=N`): allow only the
        // first N elisions within THIS function — deterministic per
        // function (unlike the process-wide counter), so
        // `FLAGDUP_FN=<f> FLAGDUP_LOCAL=N` bisects a bad elision inside
        // a single function.
        static FLAGDUP_LOCAL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let flagdup_local = *FLAGDUP_LOCAL.get_or_init(|| {
            std::env::var("PLIRON_FLAGDUP_LOCAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(u64::MAX)
        });
        let flagdup_local_n = std::cell::Cell::new(0u64);
        let flag_state = std::cell::Cell::new(None::<(u64, PReg, PReg)>);
        let mut order_pos = vec![usize::MAX; self.num_blocks()];
        for (i, &b) in final_order.iter().enumerate() {
            order_pos[b.index()] = i;
        }
        let mut out_class: Vec<Option<FxHashMap<Reg, u32>>> =
            std::iter::repeat_with(|| None).take(self.num_blocks()).collect();
        let mut prev_block: Option<BlockIndex> = None;
        // Blocks an `EDGEMERGE`/`BJALIAS` canonical copy answers for:
        // their labels alias to the canon, so their preds' edges enter
        // the canon and must join its entry-fact intersection.
        let mut canon_dups: Vec<SmallVec<[BlockIndex; 4]>> =
            std::iter::repeat_with(SmallVec::new)
                .take(self.num_blocks())
                .collect();

        static EPROF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let eprof = *EPROF.get_or_init(|| std::env::var_os("PLIRON_EPROF").is_some());
        let mut t_inst = std::time::Duration::ZERO;
        let mut t_island = std::time::Duration::ZERO;
        let mut t_allocs = std::time::Duration::ZERO;
        let mut t_loop = std::time::Duration::ZERO;
        let mut t_items = std::time::Duration::ZERO;
        let mut t_apply = std::time::Duration::ZERO;
        let mut t_fuse = std::time::Duration::ZERO;
        let mut n_islands = 0u32;

        // PLIRON: `PLIRON_JT_NARROW` (default on; `=0` off) lets the ISA defer
        // jump tables to the end of the buffer (narrow entries,
        // `MachDeferredJumpTable`). The deferred table and the dispatch's base
        // address must stay in pc-relative range, so give the buffer, per
        // block in emission order, a sound upper bound on everything still to
        // come: every inst and regalloc edit at its worst-case size plus a
        // veneer, a constant re-emission and an island jump-around; epilogues;
        // one 4-byte table entry per CFG edge; alignment padding.
        static JT_NARROW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let jt_narrow = *JT_NARROW
            .get_or_init(|| std::env::var("PLIRON_JT_NARROW").as_deref() != Ok("0"));
        let jt_suffix: Vec<u64> = if jt_narrow {
            let per_inst = u64::from(I::worst_case_size())
                + u64::from(I::LabelUse::worst_case_veneer_size())
                + 52;
            let epi = self.abi.gen_epilogue().len() as u64 * u64::from(I::worst_case_size());
            let prologue = self.abi.gen_prologue().len() as u64 * per_inst;
            let mut suffix = vec![0u64; final_order.len() + 1];
            for (oi, &b) in final_order.iter().enumerate().rev() {
                let range = self.block_ranges.get(b.index());
                let mut bytes = (range.len() as u64 + u64::from(ra_edits_per_block[b.index()]))
                    * per_inst
                    + 4 * (self.block_succs(b).len() as u64 + 2)
                    + 16
                    + u64::from(I::LabelUse::ALIGN)
                    + bb_padding.len() as u64;
                for i in range.clone() {
                    if matches!(
                        self.insts[i].is_term(),
                        MachTerminator::Ret | MachTerminator::RetCall
                    ) {
                        bytes += epi;
                    }
                }
                if b == self.entry {
                    bytes += prologue;
                }
                suffix[oi] = suffix[oi + 1] + bytes;
            }
            let consts: u64 = self
                .constants
                .iter()
                .map(|(_, d)| d.as_slice().len() as u64 + u64::from(d.alignment()))
                .sum();
            // Tables deferred by *earlier* blocks are written ahead of this
            // block's at the end, so every block's distance to its table
            // includes all of them: count every edge's entry everywhere.
            let tables: u64 = (0..self.num_blocks())
                .map(|b| 4 * (self.block_succs(BlockIndex::new(b)).len() as u64 + 2))
                .sum();
            for s in &mut suffix {
                *s += consts + tables + 64;
            }
            suffix
        } else {
            Vec::new()
        };

        // PLIRON: see `noreturn_dead_blocks`. A dead block emits nothing
        // (only its label is bound) unless it is the last block, so the
        // function still ends in code.
        let noret_dead = if flags.machine_code_cfg_info() {
            None
        } else {
            self.noreturn_dead_blocks()
        };

        // PLIRON: edge-block tail merging (`PLIRON_EDGEMERGE=1`). A block
        // that is only regalloc edits (moves, spills, reloads) and an
        // operand-less jump — typically a split critical edge — emits
        // exactly the same code as any other such block with the same
        // edits and the same successor. Keep the first one in emission
        // order and alias the others' labels to it, so their predecessors
        // branch there (MachBuffer then folds `b.c L; b dup; L:` into
        // `b.!c canon`). rustc's drop/unwind fan-in produces long runs of
        // identical `ldr x8, [sp, #a]; ldr x27, [sp, #b]; b merge` edges.
        // A dropped block's jump keeps NO_INST_OFFSET, like a cold-block
        // inst, so value-label ranges skip it and `-g` changes no code.
        // `machine_code_cfg_info` needs every block's start, so it is off
        // there.
        // `=1` keeps a dup that its predecessor falls into, or that falls
        // into its own successor, unless it is cold: merging it would put
        // that path on an extra taken branch. `=2` merges every dup.
        static EDGEMERGE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
        let edgemerge = *EDGEMERGE.get_or_init(|| pliron_mode("PLIRON_EDGEMERGE", 3));
        let mut merged = vec![false; self.num_blocks()];
        // `PLIRON_EDGEMERGE=3`: like 2 (merge every duplicate) except for
        // blocks inside a loop, approximated as the emission-order interval
        // from a back-edge target to its source: a merged dup there would put
        // an extra taken branch on a path that can run every iteration.
        let mut in_loop = vec![false; if edgemerge == 3 { final_order.len() } else { 0 }];
        if edgemerge == 3 {
            let mut order_of = vec![usize::MAX; self.num_blocks()];
            for (i, &b) in final_order.iter().enumerate() {
                order_of[b.index()] = i;
            }
            let mut diff = vec![0i32; final_order.len() + 1];
            for (pred, range) in self.block_succ_range.iter() {
                for succ in &self.block_succs[range] {
                    let sidx = succ.index();
                    if sidx < self.num_blocks()
                        && order_of[sidx] <= order_of[pred]
                        && self.block_order.is_back_edge_target(BlockIndex::new(sidx))
                    {
                        diff[order_of[sidx]] += 1;
                        diff[order_of[pred] + 1] -= 1;
                    }
                }
            }
            let mut run = 0;
            for (i, d) in diff.iter().take(final_order.len()).enumerate() {
                run += d;
                in_loop[i] = run > 0;
            }
        }
        if edgemerge > 0 && !flags.machine_code_cfg_info() {
            let mut canon: FxHashMap<EdgeKey, BlockIndex> = FxHashMap::default();
            // The last block kept for emission.
            let mut prev: Option<BlockIndex> = None;
            for (oi, &block) in final_order.iter().enumerate() {
                // A `noret_dead` block emits nothing, so it must be neither
                // a copy that others alias to nor a dropped dup.
                let dead = noret_dead.as_ref().is_some_and(|d| d[block.index()]);
                let dup = (!dead)
                    .then(|| self.edge_merge_key(regalloc, block, &loop_head, &ra_edits_per_block))
                    .flatten()
                    .and_then(|key| match canon.entry(key) {
                        Entry::Occupied(e) => Some(*e.get()),
                        Entry::Vacant(e) => {
                            e.insert(block);
                            None
                        }
                    })
                    .filter(|_| {
                        edgemerge == 2
                            || (edgemerge == 3 && !in_loop[oi])
                            || edgemerge > 3
                            || self.block_order.is_cold(block)
                            || (!prev.is_some_and(|p| self.block_preds(block).contains(&p))
                                && final_order.get(oi + 1)
                                    != self.block_succs(block).first())
                    });
                if let Some(c) = dup {
                    merged[block.index()] = true;
                    canon_dups[c.index()].push(block);
                    buffer.alias_label(MachLabel::from_block(block), MachLabel::from_block(c));
                } else if !dead && (ra_edits_per_block[block.index()] != 0
                    || self.block_ranges.get(block.index()).len() != 1
                    || self.block_succs(block).len() != 1
                    || self.insts[self.block_ranges.get(block.index()).start].is_term()
                        != MachTerminator::Branch)
                {
                    // A bare jump with no edits emits nothing (MachBuffer
                    // aliases its label to the target), so it is not
                    // anyone's layout predecessor: a brif's empty split edge
                    // sits between the brif and its other, non-empty one.
                    prev = Some(block);
                }
            }
        }

        // PLIRON: `PLIRON_BJALIAS` — alias the labels of bare-jump blocks
        // (a single operandless `b label` with no regalloc edits) to the
        // jump's target eagerly, before emission, and skip their code
        // entirely. MachBuffer otherwise establishes that alias lazily,
        // only once the block's `b` reaches the buffer tail — but by then
        // forward branches to the block have left the purgatory window,
        // so a `b` to a bare-jump block whose true target lands on the
        // next instruction survives as a dead `b .+4` (~600 of these in
        // the html-rewriter benchmark, vs ~30 with LLVM). With the alias
        // in place up front, every branch to the block resolves to the
        // real target while it can still fold at the tail. Chains of
        // bare jumps resolve to the final non-bare successor; jumps that
        // form a cycle (`b .`, `b x; ...; b y; y: b x`) keep their code.
        static BJALIAS: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
        if !flags.machine_code_cfg_info()
            && *BJALIAS.get_or_init(|| pliron_mode("PLIRON_BJALIAS", 1)) > 0
        {
            const UNRESOLVED: u32 = u32::MAX;
            const CYCLE: u32 = u32::MAX - 1;
            let nb = self.num_blocks();
            let bare: Vec<bool> = (0..nb)
                .map(|b| {
                    let r = self.block_ranges.get(b);
                    r.len() == 1
                        && ra_edits_per_block[b] == 0
                        && self.insts[r.start].is_term() == MachTerminator::Branch
                        && self.block_succs(BlockIndex::new(b)).len() == 1
                        && regalloc.inst_allocs(InsnIndex::new(r.start)).is_empty()
                })
                .collect();
            // resolve[b]: for a bare-jump block, the final non-bare
            // successor's index; CYCLE for blocks on or reaching a cycle.
            let mut resolve: Vec<u32> = (0..nb as u32).collect();
            for (b, bare_b) in bare.iter().enumerate() {
                if *bare_b {
                    resolve[b] = UNRESOLVED;
                }
            }
            let mut path: Vec<u32> = Vec::new();
            let mut onpath: Vec<bool> = vec![false; nb];
            for b in 0..nb {
                if !bare[b] || resolve[b] != UNRESOLVED {
                    continue;
                }
                let mut t = b;
                let result = loop {
                    if resolve[t] != UNRESOLVED {
                        break resolve[t];
                    }
                    if onpath[t] {
                        break CYCLE;
                    }
                    onpath[t] = true;
                    path.push(t as u32);
                    let next = self.block_succs(BlockIndex::new(t))[0].index();
                    if next >= nb {
                        // A non-block jump target (e.g. a trap/veneer
                        // pseudo-succ): keep this block's code.
                        break CYCLE;
                    }
                    t = next;
                };
                for &p in &path {
                    resolve[p as usize] = result;
                    onpath[p as usize] = false;
                }
                path.clear();
            }
            for &block in &final_order {
                let bi = block.index();
                if !bare[bi]
                    || merged[bi]
                    || block == self.entry
                    || noret_dead.as_ref().is_some_and(|d| d[bi])
                {
                    continue;
                }
                let t = resolve[bi] as usize;
                if t < nb {
                    merged[bi] = true;
                    canon_dups[t].push(block);
                    buffer.alias_label(
                        MachLabel::from_block(block),
                        MachLabel::from_block(BlockIndex::new(t)),
                    );
                }
            }
        }

        // PLIRON: `PLIRON_EPIMERGE` emits the epilogue once per function and
        // every later `return` jumps to it, for epilogues of 3+ insts. `=1`
        // keeps a hot return's own copy unless the epilogue is 5+ insts
        // (a hot leaf like `fib`'s base case pays a taken branch); `=2`
        // merges every return.
        static EPIMERGE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
        let epimerge = *EPIMERGE.get_or_init(|| pliron_mode("PLIRON_EPIMERGE", 2));
        let mut epi_label: Option<MachLabel> = None;
        let shrink = self.pliron_shrink_wrap(regalloc, &final_order);

        // PLIRON: `PLIRON_EH_LOCAL=N` bounds applied edgehoists within
        // this function (deterministic bisect); `PLIRON_EH_FN=<substr>`
        // (`!` inverts) gates them by mangled name.
        static EH_LOCAL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let eh_local_lim = *EH_LOCAL.get_or_init(|| {
            std::env::var("PLIRON_EH_LOCAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(u64::MAX)
        });
        let mut eh_local = 0u64;
        let t_loop0 = std::time::Instant::now();
        for (oi, &block) in final_order.iter().enumerate() {
            if merged[block.index()] {
                continue;
            }
            trace!("emitting block {:?}", block);
            if jt_narrow {
                buffer.set_jt_tail_bound(Some(jt_suffix[oi]));
            }

            // Call the new block hook for state
            state.on_new_block();

            if oi + 1 < final_order.len()
                && block != self.entry
                && noret_dead.as_ref().is_some_and(|d| d[block.index()])
            {
                buffer.bind_label(MachLabel::from_block(block), state.ctrl_plane_mut());
                // PLIRON FLAGDUP: flag state cannot be assumed across a
                // block boundary — another pred may enter without having
                // emitted the tracked compare.
                flag_state.set(None);
                continue;
            }

            // Emit NOPs to align the block.
            let mut new_offset = I::align_basic_block(buffer.cur_offset());
            // PLIRON: loop headers get 16-byte alignment.
            if loop_head[block.index()] {
                // PLIRON: emitted size of the loop body (insts × inst
                // size), for the size gates below.
                let (mut est, mut calls) = (0u32, false);
                if let Some(sz) = I::fixed_inst_size() {
                    let bi = block.index();
                    let (from, to) = (order_of[bi], loop_end[bi]);
                    if from <= to && to < final_order.len() {
                        for &lb in &final_order[from..=to] {
                            let r = self.block_ranges.get(lb.index());
                            est += (r.end - r.start) as u32 + ra_edits_per_block[lb.index()];
                            calls |= (r.start..r.end)
                                .any(|i| self.insts[i].call_type() != CallType::None);
                        }
                    }
                    est *= sz;
                }
                // PLIRON: `PLIRON_ALIGN_MAXB=<bytes>` skips the loop-head
                // pad when the emitted body exceeds it — a body larger
                // than a few fetch lines gains nothing from a 16-byte
                // head, and every pad is dead bytes on cold entries
                // (regex-syntax carried 641 nops to LLVM's 220).
                static MAXB: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
                let maxb = *MAXB.get_or_init(|| {
                    std::env::var("PLIRON_ALIGN_MAXB")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(u32::MAX)
                });
                if est <= maxb {
                    // `PLIRON_LOOPALIGN=<bytes>` (power of two, default 16).
                    static LA: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
                    let la = *LA.get_or_init(|| {
                        std::env::var("PLIRON_LOOPALIGN")
                            .ok()
                            .and_then(|v| v.parse().ok())
                            .filter(|v: &u32| v.is_power_of_two())
                            .unwrap_or(16)
                    });
                    new_offset = new_offset.max((buffer.cur_offset() + la - 1) & !(la - 1));
                    if la > 32 {
                        buffer.require_alignment(la);
                    }
                }
                // PLIRON: with fixed-size instructions, a loop that fits in
                // one 64-byte line must not straddle two: the hot loop is
                // then one fetch block per iteration (chars_ascii ran 1.77x
                // slower with an identical 36-byte loop across a 64-byte
                // boundary). The function asks for 64-byte alignment so
                // offsets are absolute. `PLIRON_ALIGN64=0` disables.
                static ALIGN64: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                if I::fixed_inst_size().is_some()
                    && *ALIGN64.get_or_init(|| std::env::var("PLIRON_ALIGN64").as_deref() != Ok("0"))
                {
                    let bytes = est;
                    if !calls && bytes > 0 && bytes <= 64 && (new_offset % 64) + bytes > 64 {
                        new_offset = (new_offset + 63) & !63;
                        buffer.require_alignment(64);
                    }
                }
            }
            while new_offset > buffer.cur_offset() {
                // Pad with NOPs up to the aligned block offset.
                let nop = I::gen_nop((new_offset - buffer.cur_offset()) as usize);
                nop.emit(&mut buffer, &self.emit_info, &mut Default::default());
            }
            assert_eq!(buffer.cur_offset(), new_offset);

            let mut do_emit = |inst: &I,
                           disasm: &mut String,
                           buffer: &mut MachBuffer<I>,
                           state: &mut I::State| {
                // PLIRON FLAGDUP: keep the tracked flag state in sync for
                // every inst funnelling through here — stream insts, RA
                // edit moves/spills/reloads/remats, island jumps, and
                // block markers (`Args`/`Rets`/`Other` clear it, which is
                // also the block-boundary reset).
                if flagdup {
                    match inst.flag_kind() {
                        FlagKind::Cmp { key, a, b } => flag_state.set(Some((key, a, b))),
                        FlagKind::Neutral => {}
                        FlagKind::Other => flag_state.set(None),
                    }
                }
                if want_disasm && !inst.is_args() {
                    let mut s = state.clone();
                    writeln!(disasm, "  {}", inst.pretty_print_inst(&mut s)).unwrap();
                }
                if eprof {
                    let ti = std::time::Instant::now();
                    inst.emit(buffer, &self.emit_info, state);
                    t_inst += ti.elapsed();
                } else {
                    inst.emit(buffer, &self.emit_info, state);
                }
                // The buffer maintains its deadline invariant per-`MachInst`:
                // after each instruction, ensure that the worst-case end of
                // any island the buffer might emit lies before the soonest
                // deadline, even after the next instruction.
                let lookahead = I::worst_case_size() + I::worst_case_island_growth();
                if eprof {
                    let ti = std::time::Instant::now();
                    if buffer.island_needed(lookahead) {
                        n_islands += 1;
                        let jump_around = buffer.get_label();
                        I::gen_jump(jump_around).emit(buffer, &self.emit_info, state);
                        buffer.emit_island(0, state.ctrl_plane_mut());
                        buffer.bind_label(jump_around, state.ctrl_plane_mut());
                    }
                    t_island += ti.elapsed();
                } else if buffer.island_needed(lookahead) {
                    let jump_around = buffer.get_label();
                    I::gen_jump(jump_around).emit(buffer, &self.emit_info, state);
                    buffer.emit_island(0, state.ctrl_plane_mut());
                    buffer.bind_label(jump_around, state.ctrl_plane_mut());
                }
            };

            // Is this the first block? Emit the prologue directly if so.
            if block == self.entry {
                trace!(" -> entry block");
                buffer.start_srcloc(Default::default());
                let pro = if shrink.is_some() { self.abi.gen_prologue_setup() } else { self.abi.gen_prologue() };
                for inst in &pro {
                    do_emit(&inst, &mut disasm, &mut buffer, &mut state);
                }
                buffer.end_srcloc();
            }

            // Now emit the regular block body.

            buffer.bind_label(MachLabel::from_block(block), state.ctrl_plane_mut());
            // PLIRON FLAGDUP: flag state cannot be assumed across a
            // block boundary — another pred may enter without having
            // emitted the tracked compare.
            flag_state.set(None);
            if std::env::var_os("PLIRON_EH_TRACE").is_some() {
                eprintln!(
                    "EHM oi={} b{} @off={:#x} [{}]",
                    oi,
                    block.index(),
                    buffer.cur_offset(),
                    crate::PLIRON_FN_NAME.with(|s| s.borrow().clone())
                );
            }

            if want_disasm {
                // PLIRON: name the CLIF block (or edge) this lowers, so
                // tools can weight code by CLIF loop depth.
                match self.block_order.lowered_order().get(block.index()) {
                    Some(crate::machinst::blockorder::LoweredBlock::Orig { block: cb }) => {
                        writeln!(&mut disasm, "block{}: ; {cb}", block.index()).unwrap()
                    }
                    Some(crate::machinst::blockorder::LoweredBlock::CriticalEdge { pred, succ, .. }) => {
                        writeln!(&mut disasm, "block{}: ; {pred}->{succ}", block.index()).unwrap()
                    }
                    None => writeln!(&mut disasm, "block{}:", block.index()).unwrap(),
                }
            }

            if flags.machine_code_cfg_info() {
                // Track BB starts. If we have backed up due to MachBuffer
                // branch opts, note that the removed blocks were removed.
                let cur_offset = buffer.cur_offset();
                if last_offset.is_some() && cur_offset <= last_offset.unwrap() {
                    for i in (0..bb_starts.len()).rev() {
                        if bb_starts[i].is_some() && cur_offset > bb_starts[i].unwrap() {
                            break;
                        }
                        bb_starts[i] = None;
                    }
                }
                bb_starts.push(Some(cur_offset));
                last_offset = Some(cur_offset);
            }

            // PLIRON: shrink-wrapping's save point: the callee saves and
            // frame allocation, before this block's regalloc moves.
            if shrink.as_ref().is_some_and(|(sb, _)| *sb == block) {
                buffer.start_srcloc(Default::default());
                for inst in &self.abi.gen_prologue_body() {
                    do_emit(&inst, &mut disasm, &mut buffer, &mut state);
                }
                buffer.end_srcloc();
            }

            if let Some(block_start) = I::gen_block_start(
                self.block_order.is_indirect_branch_target(block),
                is_forward_edge_cfi_enabled,
            ) {
                do_emit(&block_start, &mut disasm, &mut buffer, &mut state);
            }

            // Post-alloc use/def registers per inst, recorded by
            // `apply_allocs` below: `get_operands` skips operands that
            // are already physical regs, so they can't be collected
            // after the fact. Needed by pair-fusion hazard checks.
            let apply_allocs = |insts: &mut Vec<I>,
                                preg_ops: &mut Vec<(SmallVec<[Reg; 4]>, SmallVec<[Reg; 4]>)>,
                                iix: InsnIndex| {
                // Update the operands for this inst using the
                // allocations from the regalloc result.
                let mut allocs = regalloc.inst_allocs(iix).iter();
                let (mut uses, mut defs): (SmallVec<[Reg; 4]>, SmallVec<[Reg; 4]>) =
                    (smallvec![], smallvec![]);
                insts[iix.index()].get_operands(
                    &mut |reg: &mut Reg, constraint, kind, _pos| {
                        let alloc =
                            allocs.next().expect("enough allocations for all operands");

                        if let Some(alloc) = alloc.as_reg() {
                            let alloc: Reg = alloc.into();
                            if let OperandConstraint::FixedReg(rreg) = constraint {
                                debug_assert_eq!(Reg::from(rreg), alloc);
                            }
                            *reg = alloc;
                        } else if let Some(alloc) = alloc.as_stack() {
                            let alloc: Reg = alloc.into();
                            *reg = alloc;
                        }
                        match kind {
                            OperandKind::Def => defs.push(*reg),
                            OperandKind::Use => uses.push(*reg),
                        }
                    },
                );
                preg_ops[iix.index()] = (uses, defs);
                debug_assert!(allocs.next().is_none());
            };

            let ti = std::time::Instant::now();
            let mut items: SmallVec<[InstOrEdit; 64]> =
                regalloc.block_insts_and_edits(&self, block).collect();
            t_items += ti.elapsed();

            // PLIRON EDGEHOIST (`PLIRON_EDGEHOIST=1`): this block ends
            // `b.cc T, F` where one edge E is a pure move+jump edge
            // block (a split critical edge, typically a loop
            // back-edge). The loop path then pays an unconditional
            // `b H` per trip on top of the conditional. Hoist E's
            // moves ahead of the conditional, retarget it at E's
            // destination H, and let the exit block fall through —
            // `moves; b.cc H`, one taken branch, the shape LLVM's
            // MachineBlockPlacement picks. Two orientations:
            //
            //   taken=E:  `b.cc E, F` → `moves; b.cc H` + fallthrough F
            //   taken=F:  `b.cc F, E` → `moves; b.!cc H` + fallthrough
            //             through the now-empty E
            //
            // Only when no hoisted move destination is a location the
            // other block needs at its entry (`edgehoist_demanded`),
            // and — for the inverted form — E must be mergeable away
            // (single pred) since the exit path falls through its slot.
            static EDGEHOIST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            static EH_TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            static EH_FN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
            let eh_trace = *EH_TRACE
                .get_or_init(|| std::env::var("PLIRON_EH_TRACE").is_ok());
            if *EDGEHOIST
                .get_or_init(|| pliron_mode("PLIRON_EDGEHOIST", 1) != 0)
                && EH_FN
                    .get_or_init(|| std::env::var("PLIRON_EH_FN").ok())
                    .as_deref()
                    .map(|pat| {
                        let cur = crate::PLIRON_FN_NAME.with(|s| s.borrow().clone());
                        match pat.strip_prefix('!') {
                            Some(neg) => !cur.contains(neg),
                            None => cur.contains(pat),
                        }
                    })
                    .unwrap_or(true)
            {
                'eh: {
                    let Some(&InstOrEdit::Inst(tix)) = items.last() else {
                        break 'eh;
                    };
                    let Some((taken, not_taken)) =
                        self.insts[tix.index()].cond_br_targets()
                    else {
                        break 'eh;
                    };
                    // (edge label, exit label, invert?)
                    let mut cand = None;
                    for &(el, xl, inv) in
                        &[(taken, not_taken, false), (not_taken, taken, true)]
                    {
                        let (ei, xi) = (el.index(), xl.index());
                        if ei >= self.num_blocks() || xi >= self.num_blocks() || ei == xi
                        {
                            continue;
                        }
                        // Already merged (EDGEMERGE dup or BJALIAS): its
                        // label has an alias already.
                        if merged[ei] || noret_dead.as_ref().is_some_and(|d| d[ei]) {
                            if eh_trace {
                                eprintln!("EH b{} e=b{}: merged/dead", block.index(), ei);
                            }
                            continue;
                        }
                        let (e, x) = (BlockIndex::new(ei), BlockIndex::new(xi));
                        let er = self.block_ranges.get(ei);
                        if er.len() != 1 || !self.block_params(e).is_empty() {
                            if eh_trace {
                                eprintln!(
                                    "EH b{} e=b{}: len {} params {}",
                                    block.index(),
                                    ei,
                                    er.len(),
                                    self.block_params(e).len()
                                );
                            }
                            continue;
                        }
                        let Some(hl) = self.insts[er.start].uncond_br_target()
                        else {
                            if eh_trace {
                                eprintln!("EH b{} e=b{}: not jmp", block.index(), ei);
                            }
                            continue;
                        };
                        if hl.index() == ei
                            || self.block_order.is_indirect_branch_target(e)
                        {
                            continue;
                        }
                        let es = self.block_succs(e);
                        if es.len() != 1 || hl.index() != es[0].index() {
                            if eh_trace {
                                eprintln!("EH b{} e=b{}: succs", block.index(), ei);
                            }
                            continue;
                        }
                        let t_items: SmallVec<[InstOrEdit; 8]> =
                            regalloc.block_insts_and_edits(&self, e).collect();
                        let Some((&InstOrEdit::Inst(jix), edits)) = t_items.split_last()
                        else {
                            continue;
                        };
                        if jix.index() != er.start
                            || !edits
                                .iter()
                                .all(|it| matches!(it, InstOrEdit::Edit(_)))
                        {
                            if eh_trace {
                                eprintln!("EH b{} e=b{}: impure", block.index(), ei);
                            }
                            continue;
                        }
                        let Some(demand) = self.edgehoist_demanded(regalloc, x)
                        else {
                            if eh_trace {
                                eprintln!("EH b{} e=b{} x=b{}: demand", block.index(), ei, xi);
                            }
                            continue;
                        };
                        // The hoisted moves run before B's terminator:
                        // none may write a location the branch itself
                        // reads (aarch64 cbz/tbz test a register; x64
                        // JmpCond reads only flags, so this is a no-op
                        // there).
                        let mut br_uses: SmallVec<[Allocation; 4]> =
                            SmallVec::new();
                        for (op, a) in self
                            .inst_operands(tix)
                            .iter()
                            .zip(regalloc.inst_allocs(tix).iter())
                        {
                            if op.kind() == OperandKind::Use {
                                br_uses.push(*a);
                            }
                        }
                        if edits.iter().any(|it| {
                            let InstOrEdit::Edit(Edit::Move { from, to }) = it else {
                                return true;
                            };
                            *from != *to
                                && (demand.contains(to) || br_uses.contains(to))
                        }) {
                            if eh_trace {
                                eprintln!(
                                    "EH b{} e=b{} x=b{}: clobber {:?}",
                                    block.index(),
                                    ei,
                                    xi,
                                    demand
                                );
                            }
                            continue;
                        }
                        cand = Some((e, el, hl, inv, edits.len()));
                        break;
                    }
                    let Some((e, el, hl, inv, nedits)) = cand else {
                        break 'eh;
                    };
                    // E can only stop emitting when nobody else can
                    // reach its label: a single CFG pred (this block)
                    // and no EDGEMERGE/BJALIAS duplicates — those
                    // blocks' preds resolve through E's label and would
                    // skip the shared moves entirely (`canon_dups`, the
                    // selmul Option::fmt miscompile). Without the merge
                    // E keeps its own slot — reachable for any other
                    // preds, dead bytes otherwise.
                    let mergeable = self.block_preds(e).len() == 1
                        && canon_dups[e.index()].is_empty();
                    if eh_trace {
                        let mut pst = state.clone();
                        let edits_dbg: Vec<String> = regalloc
                            .block_insts_and_edits(&self, e)
                            .filter_map(|it| match it {
                                InstOrEdit::Edit(Edit::Move { from, to }) => {
                                    Some(format!("{:?}->{:?}", from, to))
                                }
                                _ => None,
                            })
                            .collect();
                        eprintln!(
                            "EH b{} e={:?} h={:?} inv={} n={} local={} :: {} | {:?} [{}]",
                            block.index(),
                            e,
                            hl,
                            inv,
                            nedits,
                            eh_local,
                            self.insts[tix.index()].pretty_print_inst(&mut pst),
                            edits_dbg,
                            crate::PLIRON_FN_NAME.with(|s| s.borrow().clone()),
                        );
                    }
                    // PLIRON_EH_BISECT=N: allow only the first N
                    // hoists (debug aid; unstable across compiles —
                    // prefer EH_FN+EH_LOCAL).
                    static EH_BISECT: std::sync::OnceLock<u64> =
                        std::sync::OnceLock::new();
                    let eh_lim = *EH_BISECT.get_or_init(|| {
                        std::env::var("PLIRON_EH_BISECT")
                            .ok()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(u64::MAX)
                    });
                    static EH_N: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    if EH_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= eh_lim
                        || eh_local >= eh_local_lim
                    {
                        break 'eh;
                    }
                    eh_local += 1;
                    if inv {
                        self.insts[tix.index()].invert_cond_br();
                    }
                    self.insts[tix.index()].set_cond_br_taken(hl);
                    let at = items.len() - 1;
                    let t_items: SmallVec<[InstOrEdit; 8]> =
                        regalloc.block_insts_and_edits(&self, e).collect();
                    for (i, it) in t_items.iter().take(nedits).enumerate() {
                        items.insert(at + i, it.clone());
                    }
                    // Only merge E out while its label is still
                    // unbound — an earlier-emitted E already wrote its
                    // bytes (`alias_label` ICEs on a bound label).
                    if mergeable && order_pos[e.index()] > oi {
                        merged[e.index()] = true;
                        buffer.alias_label(el, hl);
                    }
                }
            }
            if mcp && mcpbb {
                if let Some(pb) = prev_block.replace(block) {
                    out_class[pb.index()] = Some(std::mem::take(&mut vclass));
                }
                // A `None` out-state for an already-emitted pred means it
                // was merged or skipped: its true exit state is not
                // separately tracked, so it contributes nothing.
                let mut seeded = false;
                let mut feed = |p: BlockIndex, vclass: &mut FxHashMap<Reg, u32>| {
                    match (order_pos[p.index()] < oi)
                        .then(|| out_class[p.index()].as_ref())
                        .flatten()
                    {
                        Some(out) if !seeded => {
                            vclass.clone_from(out);
                            seeded = true;
                            true
                        }
                        Some(out) => {
                            vclass.retain(|k, v| out.get(k) == Some(v));
                            true
                        }
                        None => false,
                    }
                };
                let mut ok = true;
                for &p in self.block_preds(block).iter() {
                    ok &= feed(p, &mut vclass);
                }
                // A merged block's label aliases to this block (an
                // EDGEMERGE canon or a BJALIAS jump-through target): its
                // preds' edges enter here too, so the seed must also
                // satisfy their exit states.
                for &d in &canon_dups[block.index()] {
                    for &p in self.block_preds(d).iter() {
                        ok &= feed(p, &mut vclass);
                    }
                }
                if !ok {
                    vclass.clear();
                }
            } else {
                vclass.clear();
            }

            // PLIRON: when this block unconditionally branches to the next
            // emitted block and that block is a loop header, skip the jump
            // and emit the header's alignment pad at this block's end
            // instead — the block then falls through the NOPs to the aligned
            // header rather than executing a trampoline branch over them
            // (MachBuffer can only fold a jump into the following label when
            // they're byte-adjacent, which pre-label padding breaks).
            let skip_jmp = final_order
                .get(oi + 1)
                .filter(|nb| loop_head[nb.index()])
                .and_then(|_nb| {
                    let succs =
                        &self.block_succs[self.block_succ_range.get(block.index())];
                    let range = self.block_ranges.get(block.index());
                    (succs.len() == 1
                        && succs[0].index() == _nb.index()
                        && range.end > range.start
                        && self.insts[InsnIndex::new(range.end - 1).index()].is_term()
                            == MachTerminator::Branch)
                    .then(|| InsnIndex::new(range.end - 1))
                });

            // Apply allocations eagerly: the peepholes below inspect
            // physical registers, and `preg_ops` must be fully
            // populated before their hazard checks run.
            let ti = std::time::Instant::now();
            for it in &items {
                if let &InstOrEdit::Inst(iix) = it {
                    if !applied[iix.index()] {
                        apply_allocs(&mut self.insts, &mut preg_ops, iix);
                        applied[iix.index()] = true;
                    }
                }
            }
            t_apply += ti.elapsed();
            let ti = std::time::Instant::now();

            // PLIRON: flag twins (`PLIRON_FLAGTWIN=0` disables). `add d,
            // a, b` followed in the block by `adds xzr, a, b` (`cmn`, the
            // carry test of the same sum) becomes `adds d, a, b`, and the
            // later compare goes: lowering can't keep flags live across
            // instructions, so a sum used both as a value and for its
            // carry computes twice (num-bigint-dig's `mac_with_carry` in
            // montgomery's inner loop: `add; str; cmn; cinc`). Every inst
            // in between must neither read nor write flags, nor write
            // `a`/`b`; `d` must not be `a` or `b`; no regalloc edit may
            // sit between them.
            static FLAGTWIN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let flagtwin = *FLAGTWIN
                .get_or_init(|| std::env::var("PLIRON_FLAGTWIN").as_deref() != Ok("0"));
            for k in 0..items.len() {
                if !flagtwin {
                    break;
                }
                let InstOrEdit::Inst(iix) = items[k] else {
                    continue;
                };
                if squashed[iix.index()] || !self.insts[iix.index()].flag_twin_candidate() {
                    continue;
                }
                let (au, ad) = preg_ops[iix.index()].clone();
                if ad.iter().any(|r| au.contains(r)) {
                    continue;
                }
                for j in k + 1..(k + 16).min(items.len()) {
                    let InstOrEdit::Inst(nix) = items[j] else {
                        break;
                    };
                    if squashed[nix.index()] {
                        break;
                    }
                    if let Some(fused) =
                        self.insts[iix.index()].flag_twin(&self.insts[nix.index()])
                    {
                        self.insts[iix.index()] = fused;
                        self.insts[nix.index()] = I::gen_nop(0);
                        squashed[nix.index()] = true;
                        break;
                    }
                    let (_, gd) = &preg_ops[nix.index()];
                    if !self.insts[nix.index()].nzcv_free() || gd.iter().any(|r| au.contains(r)) {
                        break;
                    }
                }
            }

            // Pair fusion (e.g. `ldp`/`stp`): the partner must follow
            // with no regalloc edit between them — a move there could
            // clobber either inst's registers. A gap of pure-ALU insts
            // is also fine: the fused inst can emit at either end of
            // the gap as long as no gap inst touches a register the
            // pair touches, reads one it defines, or (partner-up) is
            // read by the partner after this inst's defs land.
            static NO_FUSE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let no_fuse = *NO_FUSE.get_or_init(|| {
                std::env::var("PLIRON_NO_PAIRSTFUSE").map_or(false, |v| v == "1")
            });
            for k in 0..items.len() {
                if no_fuse {
                    break;
                }
                let InstOrEdit::Inst(iix) = items[k] else {
                    continue;
                };
                if squashed[iix.index()] {
                    continue;
                }
                let mut j = k + 1;
                let mut gap: SmallVec<[InsnIndex; 8]> = smallvec![];
                'fuse: loop {
                    let Some(&InstOrEdit::Inst(nix)) = items.get(j) else {
                        break;
                    };
                    if squashed[nix.index()] {
                        break;
                    }
                    if let Some(fused) = self.insts[iix.index()]
                        .fuse_with_next(&self.insts[nix.index()], &state)
                    {
                        if gap.is_empty() {
                            // The partner still can't read this inst's
                            // defs: the fused pair samples every operand
                            // before either def lands. Two distinct vregs
                            // may share the partner's base preg with this
                            // inst's dest (e.g. `ldr x0,[x0]` then
                            // `ldr x1,[x0+8]` after regalloc reuses x0) —
                            // `ldp x0,x1,[x0]` would read the pre-load
                            // value for the second address.
                            let (_, pd) = &preg_ops[iix.index()];
                            let (pu2, _) = &preg_ops[nix.index()];
                            if !pu2.iter().any(|r| pd.contains(r)) {
                                // Union the partner's operand sets onto
                                // the surviving slot — the fused pair
                                // defines rt2 as well, and the post-index
                                // sweep must see that def.
                                let (u2, d2) = preg_ops[nix.index()].clone();
                                let (u, d) = &mut preg_ops[iix.index()];
                                for r in u2 {
                                    if !u.contains(&r) {
                                        u.push(r);
                                    }
                                }
                                for r in d2 {
                                    if !d.contains(&r) {
                                        d.push(r);
                                    }
                                }
                                self.insts[iix.index()] = fused;
                                self.insts[nix.index()] = I::gen_nop(0);
                                squashed[nix.index()] = true;
                            }
                            break 'fuse;
                        }
                        // Two placements: the pair can emit early at
                        // iix's slot (partner moves up) or late at
                        // nix's slot (this inst moves down). The
                        // typical demand-lowered order `fmul; str`
                        // needs the down placement — the partner's
                        // producers sit in the gap.
                        let (pu, pd) = &preg_ops[iix.index()];
                        let (pu2, pd2) = &preg_ops[nix.index()];
                        // Partner up: no gap inst may write a reg the
                        // partner touches, nor read one the partner
                        // defines. The partner also can't read this
                        // inst's defs — pair operands are read before
                        // either def lands.
                        let ok_up = !pu2.iter().any(|r| pd.contains(r))
                            && gap.iter().all(|&b| {
                                let (bu, bd) = &preg_ops[b.index()];
                                !bd.iter().any(|r| {
                                    pu2.contains(r) || pd2.contains(r)
                                }) && !bu.iter().any(|r| pd2.contains(r))
                            });
                        // This inst down: no gap inst may write a reg
                        // this inst touches, nor read one this inst
                        // defines — and the partner may not read this
                        // inst's defs either.
                        let ok_down = !pu2.iter().any(|r| pd.contains(r))
                            && gap.iter().all(|&b| {
                                let (bu, bd) = &preg_ops[b.index()];
                                !bd.iter().any(|r| {
                                    pu.contains(r) || pd.contains(r)
                                }) && !bu.iter().any(|r| pd.contains(r))
                            });
                        if ok_down {
                            // Union operand sets onto the surviving
                            // slot so the post-index sweep sees the
                            // pair's full register footprint: partner
                            // uses as uses, partner defs (e.g. the pair's
                            // rt2) as defs.
                            let (u, d) = preg_ops[iix.index()].clone();
                            let (u2, d2) = &mut preg_ops[nix.index()];
                            for r in u {
                                if !u2.contains(&r) {
                                    u2.push(r);
                                }
                            }
                            for r in d {
                                if !d2.contains(&r) {
                                    d2.push(r);
                                }
                            }
                            self.insts[iix.index()] = I::gen_nop(0);
                            self.insts[nix.index()] = fused;
                            squashed[iix.index()] = true;
                        } else if ok_up {
                            let (u2, d2) = preg_ops[nix.index()].clone();
                            let (u, d) = &mut preg_ops[iix.index()];
                            for r in u2 {
                                if !u.contains(&r) {
                                    u.push(r);
                                }
                            }
                            for r in d2 {
                                if !d.contains(&r) {
                                    d.push(r);
                                }
                            }
                            self.insts[iix.index()] = fused;
                            self.insts[nix.index()] = I::gen_nop(0);
                            squashed[nix.index()] = true;
                        }
                        break 'fuse;
                    }
                    if !self.insts[nix.index()].pair_fusion_crossable() {
                        break;
                    }
                    gap.push(nix);
                    j += 1;
                    if gap.len() > 8 {
                        break;
                    }
                }
            }

            // Post-index rewrite: a run of loads/stores on one base at
            // successive offsets, followed by `add base, base, #C`,
            // folds the pointer bump into post-indexed writebacks —
            // each access keeps its address since `wb_i = o_{i+1} - o_i`
            // telescopes, the last taking `C - o_n` — and the add is
            // deleted. Between the first access and the add, the base
            // may only be read by chain members (their offsets anchor
            // the writeback chain) and written by the add itself.
            for i in 0..items.len() {
                if no_fuse {
                    break;
                }
                let InstOrEdit::Inst(iix) = items[i] else {
                    continue;
                };
                if squashed[iix.index()] {
                    continue;
                }
                let Some((base, off, ty)) = self.insts[iix.index()].mem_signed_offset()
                else {
                    continue;
                };
                // Post-indexed access is always `[base]` — the chain
                // must start at offset 0.
                if off != 0 {
                    continue;
                }
                // An access that writes its own base reg is legal with
                // a plain offset, but post-index writeback with
                // `Rn ∈ {Rt, Rt2}` is unpredictable — leave it alone.
                if preg_ops[iix.index()].1.iter().any(|r| *r == base) {
                    continue;
                }
                let mut chain: SmallVec<[(InsnIndex, i64, Type); 8]> =
                    smallvec![(iix, off, ty)];
                let mut add_ix = None;
                let mut j = i + 1;
                let mut budget = 64;
                while j < items.len() && budget > 0 {
                    match items[j] {
                        InstOrEdit::Edit(Edit::Move { from, to }) => {
                            let base_p = base.to_real_reg().map(PReg::from);
                            if from.as_reg() == base_p || to.as_reg() == base_p {
                                break;
                            }
                        }
                        InstOrEdit::Inst(jix) => {
                            if squashed[jix.index()] {
                                j += 1;
                                continue;
                            }
                            let (uses, defs) = &preg_ops[jix.index()];
                            if defs.iter().any(|r| *r == base) {
                                if let Some((r, _)) =
                                    self.insts[jix.index()].self_add_imm()
                                    && r == base
                                {
                                    add_ix = Some(jix);
                                }
                                break;
                            }
                            if uses.iter().any(|r| *r == base) {
                                match self.insts[jix.index()].mem_signed_offset() {
                                    Some((r, o, ty)) if r == base => {
                                        chain.push((jix, o, ty));
                                    }
                                    _ => break,
                                }
                            }
                        }
                    }
                    j += 1;
                    budget -= 1;
                }
                let Some(add_ix) = add_ix else { continue };
                let Some((_, c)) = self.insts[add_ix.index()].self_add_imm()
                else {
                    continue;
                };
                let n = chain.len();
                // PLIRON: one writeback per chain (`PLIRON_POSTIDX_ONEWB=0`
                // restores the telescoping chain). Telescoping writebacks (`str [b], #4; str [b],
                // #4; str [b], #4; str [b], #8`) make every access's
                // address wait on the previous access's writeback, so a
                // loop storing a 20-byte struct per iteration (brotli's
                // `BrotliInitZopfliNodes`) carries a 4-cycle recurrence
                // through the base where the `add` it replaced cost one.
                // Instead the last offset-0 member `k` takes the whole
                // bump `#C` and the members after it are re-addressed to
                // `[base + (o_m - C)]` (`stur`), as LLVM's loop-strength
                // reduction lays the same loop out; members before `k`
                // keep their offsets. Refused (the `add` stays) when a
                // re-addressed offset does not encode.
                static ONE_WB: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                let one_wb = *ONE_WB.get_or_init(|| {
                    std::env::var("PLIRON_POSTIDX_ONEWB").map_or(true, |v| v != "0")
                });
                if one_wb {
                    let Some(k) = chain.iter().rposition(|&(_, o, _)| o == 0) else {
                        continue;
                    };
                    let kix = chain[k].0;
                    if preg_ops[kix.index()].1.iter().any(|r| *r == base) {
                        continue;
                    }
                    if !self.insts[kix.index()].post_index_imm_ok(c) {
                        continue;
                    }
                    if chain[k + 1..]
                        .iter()
                        .any(|&(ix, o, _)| !self.insts[ix.index()].mem_offset_ok(o - c))
                    {
                        continue;
                    }
                    if c != 0 {
                        self.insts[kix.index()].set_post_index(c);
                        preg_ops[kix.index()].1.push(base);
                        for &(ix, o, _) in &chain[k + 1..] {
                            self.insts[ix.index()].set_mem_offset(o - c);
                        }
                    }
                    self.insts[add_ix.index()] = I::gen_nop(0);
                    squashed[add_ix.index()] = true;
                    continue;
                }
                let mut wbs: SmallVec<[i64; 8]> = SmallVec::new();
                for m in 0..n {
                    let next = if m + 1 < n { chain[m + 1].1 } else { c };
                    let wb = next - chain[m].1;
                    if !self.insts[chain[m].0.index()].post_index_imm_ok(wb) {
                        wbs.clear();
                        break;
                    }
                    wbs.push(wb);
                }
                if wbs.len() != n {
                    continue;
                }
                for (m, &(ix, off, _)) in chain.iter().enumerate() {
                    // PLIRON: a zero step at offset 0 (a load and a store at
                    // the base before the bump) is the plain access;
                    // post-index `#0` would only add a writeback. Only at
                    // offset 0: the chain's offsets are from the original
                    // base, which the writebacks before `m` have already
                    // advanced by `off` (cmp3 caught `off != 0`).
                    if wbs[m] == 0 && off == 0 {
                        continue;
                    }
                    self.insts[ix.index()].set_post_index(wbs[m]);
                    preg_ops[ix.index()].1.push(base);
                }
                self.insts[add_ix.index()] = I::gen_nop(0);
                squashed[add_ix.index()] = true;
            }

            t_fuse += ti.elapsed();
            // PLIRON: code follows this block, so a call that cannot
            // return needs no trap after it (`TrapCode::NORETURN_CALL`).
            let code_follows = oi + 1 < final_order.len();
            let brange = self.block_ranges.get(block.index());
            let mut k = 0usize;
            while k < items.len() {
                match items[k].clone() {
                    InstOrEdit::Inst(iix) => {
                        // Squashed by a pair fusion (e.g. `ldp`) below.
                        if squashed[iix.index()] {
                            k += 1;
                            continue;
                        }
                        // PLIRON: an unconditional jump to the following
                        // loop header is dropped; the header's pad emitted
                        // at this block's end carries the fallthrough.
                        if Some(iix) == skip_jmp {
                            k += 1;
                            continue;
                        }
                        // PLIRON: drop a `NORETURN_CALL` trap that sits at
                        // the return address of the call just before it
                        // (no regalloc edit, pad or island in between),
                        // as LLVM's `NoTrapAfterNoreturn` does. The last
                        // block keeps it so the return address stays
                        // inside the function, unless `PLIRON_NORETUDF=1`
                        // drops it too: unwinders look up call sites at
                        // `ret_addr - 1`, which still lands on the `bl`.
                        static NORETUDF: std::sync::OnceLock<bool> =
                            std::sync::OnceLock::new();
                        if (code_follows
                            || *NORETUDF.get_or_init(|| pliron_mode("PLIRON_NORETUDF", 1) != 0))
                            && self.insts[iix.index()].trap_code()
                                == Some(crate::ir::TrapCode::NORETURN_CALL)
                            && iix.index() > brange.start
                            && self.insts[iix.index() - 1].call_type() == CallType::Regular
                            && buffer.last_call_ret_addr() == Some(buffer.cur_offset())
                        {
                            k += 1;
                            continue;
                        }
                        if !self.debug_value_labels.is_empty() {
                            // If we need to produce debug info,
                            // record the offset of each instruction
                            // so that we can translate value-label
                            // ranges to machine-code offsets.

                            // Cold blocks violate monotonicity
                            // assumptions elsewhere (that
                            // instructions in inst-index order are in
                            // order in machine code), so we omit
                            // their offsets here. Value-label range
                            // generation below will skip empty ranges
                            // and ranges with to-offsets of zero.
                            if !self.block_order.is_cold(block) {
                                inst_offsets[iix.index()] = buffer.cur_offset();
                            }
                        }

                        // Update the srcloc at this point in the buffer.
                        let srcloc = self.srclocs[iix.index()];
                        if cur_srcloc != Some(srcloc) {
                            if cur_srcloc.is_some() {
                                buffer.end_srcloc();
                            }
                            buffer.start_srcloc(srcloc);
                            cur_srcloc = Some(srcloc);
                        }

                        // If this is a safepoint, compute a stack map
                        // and pass it to the emit state.
                        let stack_map_disasm = if self.insts[iix.index()].is_safepoint() {
                            let (user_stack_map, user_stack_map_disasm) = {
                                // The `user_stack_maps` is keyed by reverse
                                // instruction index, so we must flip the
                                // index. We can't put this into a helper method
                                // due to borrowck issues because parts of
                                // `self` are borrowed mutably elsewhere in this
                                // function.
                                let index = iix.to_backwards_insn_index(self.num_insts());
                                let user_stack_map = self.user_stack_maps.remove(&index);
                                let user_stack_map_disasm = if want_disasm {
                                    user_stack_map.as_ref().map(|m| format!("  ; {m:?}"))
                                } else {
                                    None
                                };
                                (user_stack_map, user_stack_map_disasm)
                            };

                            state.pre_safepoint(user_stack_map);

                            user_stack_map_disasm
                        } else {
                            None
                        };

                        // Place debug tags in the emission buffer
                        // either at the offset prior to the
                        // instruction or after the instruction,
                        // depending on whether this is a call. See
                        // the documentation on [`MachDebugTagPos`]
                        // for details on why.
                        let mut debug_tag_disasm = None;
                        let mut place_debug_tags =
                            |this: &VCode<I>, pos: MachDebugTagPos, buffer: &mut MachBuffer<I>| {
                                // As above, translate the forward instruction
                                // index to a backward index for the lookup.
                                let debug_tag_range = {
                                    let index = iix.to_backwards_insn_index(this.num_insts());
                                    this.debug_tags.get(&index)
                                };
                                if let Some(range) = debug_tag_range {
                                    let start = usize::try_from(range.start).unwrap();
                                    let end = usize::try_from(range.end).unwrap();
                                    let tags = &this.debug_tag_pool[start..end];

                                    if want_disasm {
                                        debug_tag_disasm =
                                            Some(format!("  ; ^-- debug @ {pos:?}: {tags:?}"));
                                    }
                                    buffer.push_debug_tags(pos, tags);
                                }
                            };
                        let debug_tag_pos =
                            if self.insts[iix.index()].call_type() == CallType::Regular {
                                MachDebugTagPos::Post
                            } else {
                                MachDebugTagPos::Pre
                            };

                        if debug_tag_pos == MachDebugTagPos::Pre {
                            place_debug_tags(&self, debug_tag_pos, &mut buffer);
                        }

                        // If the instruction we are about to emit is
                        // a return, place an epilogue at this point
                        // (and don't emit the return; the actual
                        // epilogue will contain it).
                        if self.insts[iix.index()].is_term() == MachTerminator::Ret
                            && shrink.as_ref().is_some_and(|(_, full)| !full[block.index()])
                        {
                            // PLIRON: a return before shrink-wrapping's
                            // save point: frame record only, never the
                            // shared (full) epilogue.
                            for inst in &self.abi.gen_epilogue_frame_only() {
                                do_emit(inst, &mut disasm, &mut buffer, &mut state);
                            }
                        } else if self.insts[iix.index()].is_term() == MachTerminator::Ret {
                            log::trace!("emitting epilogue");
                            let epi = self.abi.gen_epilogue();
                            // PLIRON: shared epilogue (`PLIRON_EPIMERGE`).
                            let epi_ok = match epimerge {
                                0 => false,
                                1 => {
                                    epi.len() >= 5
                                        || (epi.len() >= 3 && self.block_order.is_cold(block))
                                }
                                _ => epi.len() >= 2,
                            };
                            if epi_ok {
                                if let Some(l) = epi_label {
                                    do_emit(&I::gen_jump(l), &mut disasm, &mut buffer, &mut state);
                                } else {
                                    let l = buffer.get_label();
                                    buffer.bind_label(l, state.ctrl_plane_mut());
                                    epi_label = Some(l);
                                    for inst in epi {
                                        do_emit(&inst, &mut disasm, &mut buffer, &mut state);
                                    }
                                }
                            } else {
                                for inst in epi {
                                    do_emit(&inst, &mut disasm, &mut buffer, &mut state);
                                }
                            }
                        } else {
                            if !applied[iix.index()] {
                                let ti = std::time::Instant::now();
                                apply_allocs(&mut self.insts, &mut preg_ops, iix);
                                t_allocs += ti.elapsed();
                                applied[iix.index()] = true;
                            }

                            // PLIRON: a `try_call` into a noreturn callee
                            // whose continuation is a lone `NORETURN_CALL`
                            // trap emits nothing after the call.
                            if code_follows {
                                self.insts[iix.index()].elide_noreturn_continuation();
                            }

                            log::trace!("emitting: {:?}", self.insts[iix.index()]);

                            // Emit the instruction!
                            let mut dead_mov = false;
                            if mcp {
                                if let Some((wd, ws)) = self.insts[iix.index()].is_move() {
                                    let (d, s) = (wd.to_reg(), ws);
                                    match (mcp_key(d), mcp_key(s)) {
                                        // `mov x,x` is a no-op at full
                                        // width (`is_move` only matches
                                        // Size64); emit nothing.
                                        (Some(_), _) if d == s => {
                                            mcp_dead += 1;
                                            dead_mov = true;
                                        }
                                        (Some(dk), Some(sk))
                                            if vclass.contains_key(&dk)
                                                && vclass.get(&dk) == vclass.get(&sk) =>
                                        {
                                            mcp_dead += 1;
                                            dead_mov = true;
                                        }
                                        (Some(dk), sk) => {
                                            let sc = match sk {
                                                Some(sk) => *vclass
                                                    .entry(sk)
                                                    .or_insert_with(|| {
                                                        next_class += 1;
                                                        next_class
                                                    }),
                                                // Source untracked: the dest
                                                // gets a fresh class rather
                                                // than an untracked one.
                                                None => {
                                                    next_class += 1;
                                                    next_class
                                                }
                                            };
                                            vclass.insert(dk, sc);
                                        }
                                        (None, _) => {}
                                    }
                                } else if self.insts[iix.index()].call_type()
                                    != CallType::None
                                {
                                    vclass.clear();
                                } else {
                                    for &d in &preg_ops[iix.index()].1 {
                                        if mcp_key(d).is_some() {
                                            next_class += 1;
                                            vclass.insert(d, next_class);
                                        } else {
                                            vclass.remove(&d);
                                        }
                                    }
                                }
                            }
                            // PLIRON FLAGDUP: a compare/test whose exact
                            // flag state is still tracked is dead. For
                            // flag-preserving insts, additionally drop
                            // the tracked state if they rewrite one of
                            // the compare's operand pregs (`Other` insts
                            // clear it inside `do_emit`; a recorded `Cmp`
                            // is (re)established there on emit).
                            let mut dead_cmp = false;
                            if flagdup && !dead_mov {
                                match self.insts[iix.index()].flag_kind() {
                                    FlagKind::Cmp { key, a, b } => {
                                        // Form gate (`PLIRON_FLAGDUP_FORMS`):
                                        // tag byte low nibble classifies
                                        // reg-reg cmp (1-8) / test (9-c) /
                                        // reg-imm cmp (0x11-0x17) /
                                        // reg-imm test (0x19-0x1c).
                                        let form_bit = match key & 0xff {
                                            0x01..=0x08 => 0x1,
                                            0x09..=0x0c => 0x2,
                                            0x11..=0x17 => 0x4,
                                            _ => 0x8,
                                        };
                                        if flagdup_forms & form_bit != 0
                                            && flagdup_fn_ok
                                            && flag_state.get() == Some((key, a, b))
                                        {
                                            let n = FLAGDUP_COUNT.fetch_add(
                                                1,
                                                std::sync::atomic::Ordering::Relaxed,
                                            );
                                            if std::env::var("PLIRON_FLAGDUP_TRACE")
                                                .is_ok()
                                            {
                                                eprintln!(
                                                    "FLAGDUP elide #{n} {} block{} iix{}: {}",
                                                    crate::PLIRON_FN_NAME
                                                        .with(|s| s.borrow().clone()),
                                                    block.index(),
                                                    iix.index(),
                                                    self.insts[iix.index()]
                                                        .pretty_print_inst(&mut state)
                                                );
                                            }
                                            let ln = flagdup_local_n.get();
                                            if n < flagdup_bisect && ln < flagdup_local {
                                                flagdup_local_n.set(ln + 1);
                                                dead_cmp = true;
                                            }
                                        }
                                    }
                                    FlagKind::Neutral => {
                                        if let Some((_, a, b)) = flag_state.get() {
                                            if preg_ops[iix.index()].1.iter().any(
                                                |d| {
                                                    matches!(
                                                        d.to_real_reg(),
                                                        Some(rr)
                                                            if PReg::from(rr) == a
                                                                || PReg::from(rr) == b
                                                    )
                                                },
                                            ) {
                                                flag_state.set(None);
                                            }
                                        }
                                    }
                                    FlagKind::Other => {}
                                }
                            }
                            if !dead_mov && !dead_cmp {
                                do_emit(
                                    &self.insts[iix.index()],
                                    &mut disasm,
                                    &mut buffer,
                                    &mut state,
                                );
                            }

                            if debug_tag_pos == MachDebugTagPos::Post {
                                place_debug_tags(&self, debug_tag_pos, &mut buffer);
                            }

                            if let Some(stack_map_disasm) = stack_map_disasm {
                                disasm.push_str(&stack_map_disasm);
                                disasm.push('\n');
                            }
                            if let Some(debug_tag_disasm) = debug_tag_disasm {
                                disasm.push_str(&debug_tag_disasm);
                                disasm.push('\n');
                            }
                        }
                    }

                    InstOrEdit::Edit(edit @ Edit::Move { from, to }) => {
                        // PLIRON FLAGDUP: the edit's destination may
                        // rewrite a tracked compare operand; a stack
                        // (non-reg) destination cannot.
                        if let (Some((_, a, b)), Some(preg)) =
                            (flag_state.get(), to.as_reg())
                        {
                            if preg == a || preg == b {
                                flag_state.set(None);
                            }
                        }
                        // PLIRON: a reload of a constant or symbol
                        // address re-executes its definition instead of
                        // the fill (see `remat_def`). Shared by the
                        // edit-pair peek below and the normal path.
                        let remat_inst = |e: &Edit, to: PReg| -> Option<I> {
                            remat_def.as_ref().and_then(|m| {
                                let base = regalloc.edits.as_ptr() as usize;
                                let at = (e as *const Edit) as usize;
                                let sz =
                                    core::mem::size_of::<(regalloc2::ProgPoint, Edit)>();
                                let off =
                                    core::mem::offset_of!((regalloc2::ProgPoint, Edit), 1);
                                let ei = (at - base - off) / sz;
                                let v = regalloc.edit_vregs.get(ei).copied().flatten()?;
                                let &di = m.get(&v.vreg())?;
                                self.insts[di]
                                    .rematerialize(Writable::from_reg(Reg::from(to)))
                            })
                        };
                        // PLIRON (`PLIRON_EDITPAIR`): two consecutive
                        // same-direction spill/reload edits to adjacent
                        // slots fuse into `ldp`/`stp`. Edit items never
                        // reach the inst-level pair fusion above, so
                        // try the generated pair here instead.
                        if editpair {
                            let gen_mem_inst = |e: &Edit| -> Option<I> {
                                let &Edit::Move { from, to } = e;
                                match (from.as_reg(), to.as_reg()) {
                                    (Some(f), None) => {
                                        let slot = to.as_stack().unwrap();
                                        (!dead_slots.contains(&slot.index())).then(|| {
                                            self.abi.gen_spill(slot, RealReg::from(f))
                                        })
                                    }
                                    (None, Some(t)) => {
                                        let slot = from.as_stack().unwrap();
                                        if remat_inst(e, t).is_some() {
                                            return None;
                                        }
                                        Some(self.abi.gen_reload(
                                            Writable::from_reg(RealReg::from(t)),
                                            slot,
                                        ))
                                    }
                                    _ => None,
                                }
                            };
                            if let Some(m1) = gen_mem_inst(edit) {
                                ep_mem += 1;
                                if let Some(&InstOrEdit::Edit(e2)) = items.get(k + 1)
                                    && let Some(m2) = gen_mem_inst(e2)
                                {
                                    ep_next += 1;
                                    let Edit::Move { to: t1, .. } = *edit;
                                    let Edit::Move { to: t2, .. } = *e2;
                                    if t1.is_stack() == t2.is_stack() {
                                        ep_dir += 1;
                                    }
                                    if let Some(fused) = m1.fuse_with_next(&m2, &state) {
                                        ep_fused += 1;
                                        // PLIRON FLAGDUP: the second
                                        // edit's destination may also
                                        // rewrite a tracked compare
                                        // operand (the first was handled
                                        // at the `Edit::Move` arm top).
                                        if let (Some((_, a, b)), Some(preg)) =
                                            (flag_state.get(), t2.as_reg())
                                        {
                                            if preg == a || preg == b {
                                                flag_state.set(None);
                                            }
                                        }
                                        if mcp {
                                            // The pair writes both reload
                                            // dests; freshen each so later
                                            // movs don't dedup against a
                                            // stale class.
                                            for e in [edit, e2] {
                                                let Edit::Move { to, .. } = e;
                                                if let Some(t) = to.as_reg() {
                                                    let tr = Reg::from(t);
                                                    if mcp_key(tr).is_some() {
                                                        next_class += 1;
                                                        vclass.insert(
                                                            tr, next_class,
                                                        );
                                                    } else {
                                                        vclass.remove(&tr);
                                                    }
                                                }
                                            }
                                        }
                                        do_emit(&fused, &mut disasm, &mut buffer, &mut state);
                                        k += 2;
                                        continue;
                                    }
                                }
                            }
                        }
                        // Create a move/spill/reload instruction and
                        // immediately emit it.
                        match (from.as_reg(), to.as_reg()) {
                            (Some(from), Some(to)) => {
                                // Reg-to-reg move.
                                ep_r2r += 1;
                                let (srcreg, dstreg) =
                                    (Reg::from(from), Reg::from(to));
                                // regalloc2 emits `from == to` moves
                                // for vreg-metadata continuity; a
                                // same-preg move is a no-op — emit
                                // nothing (the metadata lives in
                                // `edit_vregs`, not the bytes).
                                if from == to {
                                    mcp_dead += 1;
                                } else {
                                    match mcp
                                        .then(|| (mcp_key(dstreg), mcp_key(srcreg)))
                                        .unwrap_or((None, None))
                                    {
                                        (Some(dk), Some(sk))
                                            if vclass.contains_key(&dk)
                                                && vclass.get(&dk) == vclass.get(&sk) =>
                                        {
                                            mcp_dead += 1;
                                        }
                                        (Some(dk), sk) => {
                                            let sc = match sk {
                                                Some(sk) => {
                                                    *vclass.entry(sk).or_insert_with(|| {
                                                        next_class += 1;
                                                        next_class
                                                    })
                                                }
                                                None => {
                                                    next_class += 1;
                                                    next_class
                                                }
                                            };
                                            vclass.insert(dk, sc);
                                            let from_rreg = Reg::from(from);
                                            let to_rreg = Writable::from_reg(Reg::from(to));
                                            debug_assert_eq!(from.class(), to.class());
                                            let ty = I::canonical_type_for_rc(from.class());
                                            let mv = I::gen_move(to_rreg, from_rreg, ty);
                                            do_emit(&mv, &mut disasm, &mut buffer, &mut state);
                                        }
                                        _ => {
                                            let from_rreg = Reg::from(from);
                                            let to_rreg = Writable::from_reg(Reg::from(to));
                                            debug_assert_eq!(from.class(), to.class());
                                            let ty = I::canonical_type_for_rc(from.class());
                                            let mv = I::gen_move(to_rreg, from_rreg, ty);
                                            do_emit(&mv, &mut disasm, &mut buffer, &mut state);
                                        }
                                    }
                                }
                            }
                            (Some(from), None) => {
                                // Spill from register to spillslot.
                                let to = to.as_stack().unwrap();
                                if !dead_slots.contains(&to.index()) {
                                    let from_rreg = RealReg::from(from);
                                    let spill = self.abi.gen_spill(to, from_rreg);
                                    do_emit(&spill, &mut disasm, &mut buffer, &mut state);
                                }
                            }
                            (None, Some(to)) => {
                                // Load from spillslot to register.
                                let from = from.as_stack().unwrap();
                                if mcp {
                                    let tr = Reg::from(to);
                                    if mcp_key(tr).is_some() {
                                        next_class += 1;
                                        vclass.insert(tr, next_class);
                                    } else {
                                        vclass.remove(&tr);
                                    }
                                }
                                let to_rreg = Writable::from_reg(RealReg::from(to));
                                if let Some(r) = remat_inst(edit, to) {
                                    do_emit(&r, &mut disasm, &mut buffer, &mut state);
                                } else {
                                    let reload = self.abi.gen_reload(to_rreg, from);
                                    do_emit(&reload, &mut disasm, &mut buffer, &mut state);
                                }
                            }
                            (None, None) => {
                                panic!("regalloc2 should have eliminated stack-to-stack moves!");
                            }
                        }
                    }
                }
                k += 1;
            }

            if cur_srcloc.is_some() {
                buffer.end_srcloc();
                cur_srcloc = None;
            }

            // PLIRON: for a skipped header jump, land the pad here — the
            // block falls through it to the aligned header label.
            if skip_jmp.is_some() {
                while buffer.cur_offset() & 15 != 0 {
                    let pad = 16 - (buffer.cur_offset() & 15) as usize;
                    let nop = I::gen_nop(pad);
                    nop.emit(&mut buffer, &self.emit_info, &mut Default::default());
                }
            }

            // Insert padding, if configured, to stress the `MachBuffer`'s
            // relocation and island calculations.
            //
            // Padding can get quite large during fuzzing though so place a
            // total cap on it where when a per-function threshold is exceeded
            // the padding is turned back down to zero. This avoids a small-ish
            // test case generating a GB+ memory footprint in Cranelift for
            // example.
            if !bb_padding.is_empty() {
                // The padding bytes go directly into the buffer without
                // passing through `do_emit`, so check the deadline invariant
                // *before* writing them: if the padding would push the
                // worst-case end of an island past any pending deadline,
                // drain pending fixups via an island now. We're between
                // blocks, so no jump-around is needed.
                let padding_len = bb_padding.len() as u32 + I::LabelUse::ALIGN - 1;
                let lookahead = I::worst_case_size() + I::worst_case_island_growth();
                if buffer.island_needed(padding_len + lookahead) {
                    buffer.emit_island(padding_len + lookahead, ctrl_plane);
                }

                buffer.put_data(&bb_padding);
                buffer.align_to(I::LabelUse::ALIGN);
                total_bb_padding += bb_padding.len();
                if total_bb_padding > (150 << 20) {
                    bb_padding = Vec::new();
                }
            }
        }

        debug_assert!(
            self.user_stack_maps.is_empty(),
            "any stack maps should have been consumed by instruction emission, still have: {:#?}",
            self.user_stack_maps,
        );

        // Do any optimizations on branches at tail of buffer, as if we had
        // bound one last label.
        t_loop += t_loop0.elapsed();
        let ti = std::time::Instant::now();
        buffer.optimize_branches(ctrl_plane);
        let t_obr = ti.elapsed();

        // emission state is not needed anymore, move control plane back out
        *ctrl_plane = state.take_ctrl_plane();

        let func_body_len = buffer.cur_offset();

        // Create `bb_edges` and final (filtered) `bb_starts`.
        let mut bb_edges = vec![];
        let mut bb_offsets = vec![];
        if flags.machine_code_cfg_info() {
            for block in 0..self.num_blocks() {
                if bb_starts[block].is_none() {
                    // Block was deleted by MachBuffer; skip.
                    continue;
                }
                let from = bb_starts[block].unwrap();

                bb_offsets.push(from);
                // Resolve each `succ` label and add edges.
                let succs = self.block_succs(BlockIndex::new(block));
                for &succ in succs.iter() {
                    let to = buffer.resolve_label_offset(MachLabel::from_block(succ));
                    bb_edges.push((from, to));
                }
            }
        }

        self.monotonize_inst_offsets(&mut inst_offsets[..], func_body_len);
        let value_labels_ranges =
            self.compute_value_labels_ranges(regalloc, &inst_offsets[..], func_body_len);

        // Store metadata about frame layout in the MachBuffer.
        buffer.set_frame_layout(self.abi.frame_slot_metadata());

        let ti = std::time::Instant::now();
        let buf = buffer.finish(&self.constants, ctrl_plane);
        let t_fin = ti.elapsed();
        // PLIRON FLAGDUP_TRACE: running elision total (bisect aid).
        if std::env::var("PLIRON_FLAGDUP_TRACE").is_ok() {
            eprintln!(
                "FLAGDUP total {}",
                FLAGDUP_COUNT.load(std::sync::atomic::Ordering::Relaxed)
            );
        }
        if eprof {
            eprintln!(
                "eprof frame={t_frame:?} loop={t_loop:?} items={t_items:?} apply={t_apply:?} fuse={t_fuse:?} inst={t_inst:?} allocs={t_allocs:?} island={t_island:?}({n_islands}) obr={t_obr:?} fin={t_fin:?} ep={ep_mem}/{ep_next}/{ep_dir}/{ep_fused} r2r={ep_r2r} mcp={mcp_dead}"
            );
        }

        Ok(EmitResult {
            buffer: buf,
            bb_offsets,
            bb_edges,
            disasm: if want_disasm { Some(disasm) } else { None },
            value_labels_ranges,
        })
    }

    /// PLIRON: the `PLIRON_EDGEMERGE` key of a block that is only regalloc
    /// edits and an operand-less jump: (edits, successor, indirect-target
    /// flag). Two blocks with one key emit the same code. `None` for any
    /// other block, the entry, and loop heads.
    fn edge_merge_key(
        &self,
        regalloc: &regalloc2::Output,
        block: BlockIndex,
        loop_head: &[bool],
        ra_edits_per_block: &[u32],
    ) -> Option<EdgeKey> {
        let bi = block.index();
        if block == self.entry || loop_head[bi] || ra_edits_per_block[bi] == 0 {
            return None;
        }
        let range = self.block_ranges.get(bi);
        if range.end != range.start + 1 {
            return None;
        }
        let iix = InsnIndex::new(range.start);
        let succs = self.block_succs(block);
        if succs.len() != 1
            || self.insts[iix.index()].is_term() != MachTerminator::Branch
            || !regalloc.inst_allocs(iix).is_empty()
        {
            return None;
        }
        let mut moves: SmallVec<[(Allocation, Allocation); 4]> = smallvec![];
        for it in regalloc.block_insts_and_edits(self, block) {
            match it {
                InstOrEdit::Edit(Edit::Move { from, to }) => moves.push((*from, *to)),
                InstOrEdit::Inst(i) if i == iix => {}
                _ => return None,
            }
        }
        if moves.is_empty() {
            return None;
        }
        Some((moves, succs[0], self.block_order.is_indirect_branch_target(block)))
    }

    /// PLIRON EDGEHOIST: locations (pregs or stack slots) that must
    /// hold particular values at `b`'s entry — the set a hoisted
    /// edge-block move may not clobber. Walks `b` and its successors
    /// (bounded): every vreg used before its in-block def reads the
    /// location it was in at `b`'s entry, and every edit reads its
    /// source location, so all of those are demanded. A live-in
    /// vreg bound to a successor's param cannot have its entry
    /// location named without whole-function liveness, so the
    /// analysis bails (`None`) — the caller then keeps the edge
    /// block.
    fn edgehoist_demanded(
        &self,
        regalloc: &regalloc2::Output,
        b: BlockIndex,
    ) -> Option<crate::FxHashSet<Allocation>> {
        let mut demand = crate::FxHashSet::default();
        let mut work: SmallVec<[BlockIndex; 8]> = smallvec![b];
        let mut seen = crate::FxHashSet::default();
        static EHD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let ehd = *EHD.get_or_init(|| std::env::var("PLIRON_EH_TRACE").is_ok());
        while let Some(x) = work.pop() {
            if !seen.insert(x) || seen.len() > 8 {
                return None;
            }
            let xr = self.block_ranges.get(x.index());
            if xr.len() == 0 {
                return None;
            }
            // `defined` is deliberately per-block: a vreg defined on a
            // parallel path does not mask a live-in use on this one.
            let mut defined: crate::FxHashSet<VReg> = crate::FxHashSet::default();
            for it in regalloc.block_insts_and_edits(self, x) {
                match it {
                    InstOrEdit::Edit(Edit::Move { from, .. }) => {
                        // Any edit in the walk reads a location whose
                        // content at `b`'s entry must be preserved
                        // (conservatively includes edits serving
                        // values defined inside `b`).
                        demand.insert(*from);
                    }
                    InstOrEdit::Inst(iix) => {
                        // Uses first, then defs: a use of a vreg with no
                        // earlier in-block def reads an entry location.
                        // Block params are live-in vregs, so they are
                        // covered by the same rule.
                        for (op, a) in self
                            .inst_operands(iix)
                            .iter()
                            .zip(regalloc.inst_allocs(iix).iter())
                        {
                            if op.kind() == OperandKind::Use && !defined.contains(&op.vreg()) {
                                demand.insert(*a);
                            }
                        }
                        for op in self.inst_operands(iix) {
                            if op.kind() == OperandKind::Def {
                                defined.insert(op.vreg());
                            }
                        }
                    }
                }
            }
            // Out-edges: an arg bound to a successor's param that isn't
            // defined inside this block could be a value live into `b`
            // carried in-place — its entry location can't be named
            // locally, so bail. Every successor is walked (bounded by
            // `seen`) since a value live through `b` may be demanded
            // anywhere downstream without an edit marking it.
            let term = InsnIndex::new(xr.end - 1);
            for (k, s) in self.block_succs(x).iter().enumerate() {
                let si = s.index();
                if si >= self.num_blocks() {
                    continue;
                }
                for v in self.branch_blockparams(x, term, k) {
                    if !defined.contains(v) {
                        if ehd {
                            eprintln!("  EHD b{}->b{}: live-in arg", x.index(), si);
                        }
                        return None;
                    }
                }
                work.push(*s);
            }
        }
        Some(demand)
    }

    fn monotonize_inst_offsets(&self, inst_offsets: &mut [CodeOffset], func_body_len: u32) {
        if self.debug_value_labels.is_empty() {
            return;
        }

        // During emission, branch removal can make offsets of instructions incorrect.
        // Consider the following sequence: [insi][jmp0][jmp1][jmp2][insj]
        // It will be recorded as (say):    [30]  [34]  [38]  [42]  [<would be 46>]
        // When the jumps get removed we are left with (in "inst_offsets"):
        // [insi][jmp0][jmp1][jmp2][insj][...]
        // [30]  [34]  [38]  [42]  [34]
        // Which violates the monotonicity invariant. This method sets offsets of these
        // removed instructions such as to make them appear zero-sized:
        // [insi][jmp0][jmp1][jmp2][insj][...]
        // [30]  [34]  [34]  [34]  [34]
        //
        let mut next_offset = func_body_len;
        for inst_index in (0..(inst_offsets.len() - 1)).rev() {
            let inst_offset = inst_offsets[inst_index];

            // Not all instructions get their offsets recorded.
            if inst_offset == NO_INST_OFFSET {
                continue;
            }

            if inst_offset > next_offset {
                trace!(
                    "Fixing code offset of the removed Inst {}: {} -> {}",
                    inst_index, inst_offset, next_offset
                );
                inst_offsets[inst_index] = next_offset;
                continue;
            }

            next_offset = inst_offset;
        }
    }

    fn compute_value_labels_ranges(
        &self,
        regalloc: &regalloc2::Output,
        inst_offsets: &[CodeOffset],
        func_body_len: u32,
    ) -> ValueLabelsRanges {
        if self.debug_value_labels.is_empty() {
            return ValueLabelsRanges::default();
        }

        if trace_log_enabled!() {
            self.log_value_labels_ranges(regalloc, inst_offsets);
        }

        let mut value_labels_ranges: ValueLabelsRanges = HashMap::new();
        let prog_point_to_inst = |prog_point: ProgPoint| {
            let mut inst = prog_point.inst();
            if prog_point.pos() == InstPosition::After {
                inst = inst.next();
            }
            inst.index()
        };
        // PLIRON: regalloc reports a label only while its vreg is live;
        // `extend_debug_locations` keeps it until the location is
        // overwritten, as LLVM's LiveDebugValues does.
        let locs = self.extend_debug_locations(regalloc, &prog_point_to_inst);
        for (label, from_inst_index, to_inst_index, alloc) in locs {
            let label = ValueLabel::from_u32(label);
            let ranges = value_labels_ranges.entry(label).or_insert_with(|| vec![]);
            let inst_to_offset = |inst_index: usize| {
                // Skip over cold blocks.
                for offset in &inst_offsets[inst_index..] {
                    if *offset != NO_INST_OFFSET {
                        return *offset;
                    }
                }
                func_body_len
            };
            let from_offset = inst_to_offset(from_inst_index);
            let to_offset = inst_to_offset(to_inst_index);

            // Empty ranges or unavailable offsets can happen
            // due to cold blocks and branch removal (see above).
            if from_offset == to_offset {
                continue;
            }

            let loc = if let Some(preg) = alloc.as_reg() {
                LabelValueLoc::Reg(Reg::from(preg))
            } else {
                #[cfg(not(feature = "unwind"))]
                continue;

                #[cfg(feature = "unwind")]
                {
                    let slot = alloc.as_stack().unwrap();
                    let slot_offset = self.abi.get_spillslot_offset(slot);
                    let slot_base_to_caller_sp_offset = self.abi.slot_base_to_caller_sp_offset();
                    let caller_sp_to_cfa_offset =
                        crate::isa::unwind::systemv::caller_sp_to_cfa_offset();
                    // NOTE: this is a negative offset because it's relative to the caller's SP
                    let cfa_to_sp_offset =
                        -((slot_base_to_caller_sp_offset + caller_sp_to_cfa_offset) as i64);
                    LabelValueLoc::CFAOffset(cfa_to_sp_offset + slot_offset)
                }
            };

            // Coalesce adjacent ranges that for the same location
            // to minimize output size here and for the consumers.
            if let Some(last_loc_range) = ranges.last_mut() {
                if last_loc_range.loc == loc && last_loc_range.end == from_offset {
                    trace!(
                        "Extending debug range for {:?} in {:?} to Inst {} ({})",
                        label, loc, to_inst_index, to_offset
                    );
                    last_loc_range.end = to_offset;
                    continue;
                }
            }

            trace!(
                "Recording debug range for {:?} in {:?}: [Inst {}..Inst {}) [{}..{})",
                label, loc, from_inst_index, to_inst_index, from_offset, to_offset
            );

            ranges.push(ValueLocRange {
                loc,
                start: from_offset,
                end: to_offset,
            });
        }

        value_labels_ranges
    }

    /// PLIRON: regalloc's debug locations as `(label, from, to, alloc)`
    /// instruction ranges, each extended past the end of its vreg's
    /// liveness for as long as nothing overwrites the location: a dead
    /// value's register still holds it (LLVM's LiveDebugValues). Within a
    /// block a location ends at an instruction that defines it, clobbers
    /// it (calls) or moves into it (regalloc edits), and at the end of the
    /// label's assignment (the next value with that label in linear
    /// order, `debug_value_labels`). A block entered only from blocks
    /// before it in layout order starts with the locations all of them
    /// end with (a back edge, unvisited yet, gives none). `PLIRON_DBGEXT=0`
    /// keeps regalloc's ranges as they are. Debug info only: no code
    /// changes.
    fn extend_debug_locations(
        &self,
        regalloc: &regalloc2::Output,
        pp: &dyn Fn(ProgPoint) -> usize,
    ) -> Vec<(u32, usize, usize, regalloc2::Allocation)> {
        use regalloc2::Allocation;
        static EXT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let raw: Vec<(u32, usize, usize, Allocation)> = regalloc
            .debug_locations
            .iter()
            .map(|&(l, f, t, a)| (l, pp(f), pp(t), a))
            .filter(|r| r.1 < r.2)
            .collect();
        if !*EXT.get_or_init(|| std::env::var("PLIRON_DBGEXT").as_deref() != Ok("0")) {
            return raw;
        }
        let n = self.num_insts();
        // Each label's assignment intervals, sorted (`debug_value_labels`
        // is sorted by vreg; re-sort per label by start).
        let mut assign: FxHashMap<u32, Vec<(usize, usize)>> = FxHashMap::default();
        for &(_, from, to, l) in &self.debug_value_labels {
            assign.entry(l).or_default().push((from.index(), to.index()));
        }
        for v in assign.values_mut() {
            v.sort_unstable();
        }
        let bound_of = |l: u32, i: usize| -> usize {
            let Some(v) = assign.get(&l) else { return i };
            match v.partition_point(|&(f, _)| f <= i) {
                0 => i,
                k => {
                    let (f, t) = v[k - 1];
                    if f <= i && i < t { t } else { i }
                }
            }
        };
        // Locations of interest, and where each is overwritten: `kills[k]`
        // = allocations invalid from instruction `k` on.
        let mut interest: crate::FxHashSet<Allocation> = Default::default();
        interest.reserve(raw.len().min(64));
        for r in &raw {
            interest.insert(r.3);
        }
        let mut kills: Vec<smallvec::SmallVec<[Allocation; 2]>> = vec![Default::default(); n + 1];
        for i in 0..n {
            let inst = InsnIndex::new(i);
            for (op, &a) in self.inst_operands(inst).iter().zip(regalloc.inst_allocs(inst)) {
                if op.kind() == OperandKind::Def && interest.contains(&a) {
                    kills[i + 1].push(a);
                }
            }
            let cl = self.inst_clobbers(inst);
            if cl != PRegSet::default() {
                for &a in &interest {
                    if let Some(r) = a.as_reg()
                        && cl.contains(r)
                    {
                        kills[i + 1].push(a);
                    }
                }
            }
        }
        for &(p, ref e) in &regalloc.edits {
            let Edit::Move { to, .. } = e;
            if interest.contains(to) {
                let k = if p.pos() == InstPosition::After { p.inst().index() + 1 } else { p.inst().index() };
                kills[k.min(n)].push(*to);
            }
        }
        // Regalloc's ranges as starts: at their first instruction and again
        // at each block start they cover.
        let nb = self.num_blocks();
        let bstart: Vec<usize> = (0..nb).map(|b| self.block_insns(BlockIndex::new(b)).first().index()).collect();
        let mut starts: FxHashMap<usize, Vec<(u32, Allocation, usize)>> = FxHashMap::default();
        for &(l, f, t, a) in &raw {
            starts.entry(f).or_default().push((l, a, t));
            let k0 = bstart.partition_point(|&s| s <= f);
            for &s in &bstart[k0..] {
                if s >= t {
                    break;
                }
                starts.entry(s).or_default().push((l, a, t));
            }
        }
        // label -> (alloc, assignment end, regalloc range end, open since)
        type St = FxHashMap<u32, (Allocation, usize, usize, usize)>;
        let mut out = Vec::new();
        let mut outs: Vec<Option<Vec<(u32, Allocation, usize)>>> = vec![None; nb];
        for b in 0..nb {
            let bi = BlockIndex::new(b);
            let r = self.block_insns(bi);
            let (s0, e0) = (r.first().index(), r.last().index() + 1);
            let mut st: St = St::default();
            let preds = self.block_preds(bi);
            if !preds.is_empty() && preds.iter().all(|p| p.index() < b) {
                let mut first = true;
                let mut acc: FxHashMap<u32, (Allocation, usize)> = FxHashMap::default();
                for p in preds {
                    let o = outs[p.index()].as_deref().unwrap_or(&[]);
                    if first {
                        acc = o.iter().map(|&(l, a, bd)| (l, (a, bd))).collect();
                        first = false;
                    } else {
                        let m: FxHashMap<u32, (Allocation, usize)> = o.iter().map(|&(l, a, bd)| (l, (a, bd))).collect();
                        acc.retain(|l, v| match m.get(l) {
                            Some(&(a, bd)) if a == v.0 => {
                                v.1 = v.1.min(bd);
                                true
                            }
                            _ => false,
                        });
                    }
                }
                for (l, (a, bd)) in acc {
                    if s0 < bd {
                        st.insert(l, (a, bd, 0, s0));
                    }
                }
            }
            // Pending assignment ends and, per location, the labels in it:
            // each instruction looks only at what can end there.
            let mut ends: FxHashMap<usize, Vec<u32>> = FxHashMap::default();
            let mut by_alloc: FxHashMap<Allocation, Vec<u32>> = FxHashMap::default();
            for (&l, &(a, bd, _, _)) in &st {
                ends.entry(bd).or_default().push(l);
                by_alloc.entry(a).or_default().push(l);
            }
            let close = |st: &mut St, out: &mut Vec<_>, l: u32, i: usize| {
                if let Some((a, _, _, since)) = st.remove(&l)
                    && since < i
                {
                    out.push((l, since, i, a));
                }
            };
            for i in s0..e0 {
                if let Some(ls) = ends.remove(&i) {
                    for l in ls {
                        if st.get(&l).is_some_and(|v| v.1 <= i) {
                            close(&mut st, &mut out, l, i);
                        }
                    }
                }
                for a in &kills[i] {
                    let Some(ls) = by_alloc.get_mut(a) else { continue };
                    ls.retain(|&l| match st.get(&l) {
                        Some(v) if v.0 == *a => {
                            if v.2 <= i {
                                close(&mut st, &mut out, l, i);
                                false
                            } else {
                                true
                            }
                        }
                        _ => false,
                    });
                }
                if let Some(ss) = starts.get(&i) {
                    for &(l, a, t) in ss {
                        match st.get_mut(&l) {
                            Some(v) if v.0 == a => v.2 = v.2.max(t),
                            _ => {
                                close(&mut st, &mut out, l, i);
                                let bd = bound_of(l, i).max(t);
                                st.insert(l, (a, bd, t, i));
                                ends.entry(bd).or_default().push(l);
                                by_alloc.entry(a).or_default().push(l);
                            }
                        }
                    }
                }
            }
            for (&l, &(a, _, _, since)) in &st {
                if since < e0 {
                    out.push((l, since, e0, a));
                }
            }
            outs[b] = Some(st.into_iter().map(|(l, (a, bd, _, _))| (l, a, bd)).collect());
        }
        out.sort_unstable_by_key(|&(l, f, _, _)| (l, f));
        out
    }

    fn log_value_labels_ranges(&self, regalloc: &regalloc2::Output, inst_offsets: &[CodeOffset]) {
        debug_assert!(trace_log_enabled!());

        // What debug labels do we have? Note we'll skip those that have not been
        // allocated any location at all. They will show up as numeric gaps in the table.
        let mut labels = vec![];
        for &(label, _, _, _) in &regalloc.debug_locations {
            if Some(&label) == labels.last() {
                continue;
            }
            labels.push(label);
        }

        // Reformat the data on what VRegs were the VLs assigned to by lowering, since
        // the array we have is sorted by VReg, and we want it sorted by VL for easy
        // access in the loop below.
        let mut vregs = vec![];
        for &(vreg, start, end, label) in &self.debug_value_labels {
            if matches!(labels.binary_search(&label), Ok(_)) {
                vregs.push((label, start, end, vreg));
            }
        }
        vregs.sort_unstable_by(
            |(l_label, l_start, _, _), (r_label, r_start, _, _)| match l_label.cmp(r_label) {
                Ordering::Equal => l_start.cmp(r_start),
                cmp => cmp,
            },
        );

        #[derive(PartialEq)]
        enum Mode {
            Measure,
            Emit,
        }
        #[derive(PartialEq)]
        enum Row {
            Head,
            Line,
            Inst(usize, usize),
        }

        let mut widths = vec![0; 3 + 2 * labels.len()];
        let mut row = String::new();
        let mut output_row = |row_kind: Row, mode: Mode| {
            let mut column_index = 0;
            row.clear();

            macro_rules! output_cell_impl {
                ($fill:literal, $span:literal, $($cell_fmt:tt)*) => {
                    let column_start = row.len();
                    {
                        row.push('|');
                        write!(row, $($cell_fmt)*).unwrap();
                    }

                    let next_column_index = column_index + $span;
                    let expected_width: usize = widths[column_index..next_column_index].iter().sum();
                    if mode == Mode::Measure {
                        let actual_width = row.len() - column_start;
                        if actual_width > expected_width {
                            widths[next_column_index - 1] += actual_width - expected_width;
                        }
                    } else {
                        let column_end = column_start + expected_width;
                        while row.len() != column_end {
                            row.push($fill);
                        }
                    }
                    column_index = next_column_index;
                };
            }
            macro_rules! output_cell {
                ($($cell_fmt:tt)*) => {
                    output_cell_impl!(' ', 1, $($cell_fmt)*);
                };
            }

            match row_kind {
                Row::Head => {
                    output_cell!("BB");
                    output_cell!("Inst");
                    output_cell!("IP");
                    for label in &labels {
                        output_cell_impl!(' ', 2, "{:?}", ValueLabel::from_u32(*label));
                    }
                }
                Row::Line => {
                    debug_assert!(mode == Mode::Emit);
                    for _ in 0..3 {
                        output_cell_impl!('-', 1, "");
                    }
                    for _ in &labels {
                        output_cell_impl!('-', 2, "");
                    }
                }
                Row::Inst(block_index, inst_index) => {
                    debug_assert!(inst_index < self.num_insts());
                    if self.block_ranges.get(block_index).start == inst_index {
                        output_cell!("B{}", block_index);
                    } else {
                        output_cell!("");
                    }
                    output_cell!("Inst {inst_index} ");
                    output_cell!("{} ", inst_offsets[inst_index]);

                    for label in &labels {
                        // First, the VReg.
                        use regalloc2::Inst;
                        let vreg_cmp = |inst: usize,
                                        vreg_label: &u32,
                                        range_start: &Inst,
                                        range_end: &Inst| {
                            match vreg_label.cmp(&label) {
                                Ordering::Equal => {
                                    if range_end.index() <= inst {
                                        Ordering::Less
                                    } else if range_start.index() > inst {
                                        Ordering::Greater
                                    } else {
                                        Ordering::Equal
                                    }
                                }
                                cmp => cmp,
                            }
                        };
                        let vreg_index =
                            vregs.binary_search_by(|(l, s, e, _)| vreg_cmp(inst_index, l, s, e));
                        if let Ok(vreg_index) = vreg_index {
                            let mut prev_vreg = None;
                            if inst_index > 0 {
                                let prev_vreg_index = vregs.binary_search_by(|(l, s, e, _)| {
                                    vreg_cmp(inst_index - 1, l, s, e)
                                });
                                if let Ok(prev_vreg_index) = prev_vreg_index {
                                    prev_vreg = Some(vregs[prev_vreg_index].3);
                                }
                            }

                            let vreg = vregs[vreg_index].3;
                            if Some(vreg) == prev_vreg {
                                output_cell!("*");
                            } else {
                                output_cell!("{}", vreg);
                            }
                        } else {
                            output_cell!("");
                        }

                        // Second, the allocated location.
                        let inst_prog_point = ProgPoint::before(Inst::new(inst_index));
                        let range_index = regalloc.debug_locations.binary_search_by(
                            |(range_label, range_start, range_end, _)| match range_label.cmp(label)
                            {
                                Ordering::Equal => {
                                    if *range_end <= inst_prog_point {
                                        Ordering::Less
                                    } else if *range_start > inst_prog_point {
                                        Ordering::Greater
                                    } else {
                                        Ordering::Equal
                                    }
                                }
                                cmp => cmp,
                            },
                        );
                        if let Ok(range_index) = range_index {
                            // Live at this instruction, print the location.
                            if let Some(reg) = regalloc.debug_locations[range_index].3.as_reg() {
                                output_cell!("{:?}", Reg::from(reg));
                            } else {
                                output_cell!("Stk");
                            }
                        } else {
                            // Not live at this instruction.
                            output_cell!("");
                        }
                    }
                }
            }
            row.push('|');

            if mode == Mode::Emit {
                trace!("{}", row.as_str());
            }
        };

        for block_index in 0..self.num_blocks() {
            for inst_index in self.block_ranges.get(block_index) {
                output_row(Row::Inst(block_index, inst_index), Mode::Measure);
            }
        }
        output_row(Row::Head, Mode::Measure);

        output_row(Row::Head, Mode::Emit);
        output_row(Row::Line, Mode::Emit);
        for block_index in 0..self.num_blocks() {
            for inst_index in self.block_ranges.get(block_index) {
                output_row(Row::Inst(block_index, inst_index), Mode::Emit);
            }
        }
    }

    /// Get the IR block for a BlockIndex, if one exists.
    pub fn bindex_to_bb(&self, block: BlockIndex) -> Option<ir::Block> {
        self.block_order.lowered_order()[block.index()].orig_block()
    }

    /// Get the user stack map associated with the given forward instruction index.
    pub fn get_user_stack_map(&self, inst: InsnIndex) -> Option<&ir::UserStackMap> {
        let index = inst.to_backwards_insn_index(self.num_insts());
        self.user_stack_maps.get(&index)
    }
}

impl<I: VCodeInst> core::ops::Index<InsnIndex> for VCode<I> {
    type Output = I;
    fn index(&self, idx: InsnIndex) -> &Self::Output {
        &self.insts[idx.index()]
    }
}

impl<I: VCodeInst> RegallocFunction for VCode<I> {
    fn num_insts(&self) -> usize {
        self.insts.len()
    }

    fn is_remat(&self, vreg: usize) -> bool {
        self.remat_vregs.get(vreg).copied().unwrap_or(false)
    }

    fn no_param_merge(&self, block: BlockIndex) -> bool {
        self.no_merge_block == Some(block)
    }

    fn block_hotness(&self, block: BlockIndex) -> Option<f32> {
        self.block_freq.get(block.index()).copied()
    }

    fn num_blocks(&self) -> usize {
        self.block_ranges.len()
    }

    fn entry_block(&self) -> BlockIndex {
        self.entry
    }

    fn block_insns(&self, block: BlockIndex) -> InstRange {
        let range = self.block_ranges.get(block.index());
        InstRange::new(InsnIndex::new(range.start), InsnIndex::new(range.end))
    }

    fn block_succs(&self, block: BlockIndex) -> &[BlockIndex] {
        let range = self.block_succ_range.get(block.index());
        &self.block_succs[range]
    }

    fn block_preds(&self, block: BlockIndex) -> &[BlockIndex] {
        let range = self.block_pred_range.get(block.index());
        &self.block_preds[range]
    }

    fn block_params(&self, block: BlockIndex) -> &[VReg] {
        // As a special case we don't return block params for the entry block, as all the arguments
        // will be defined by the `Inst::Args` instruction.
        if block == self.entry {
            return &[];
        }

        let range = self.block_params_range.get(block.index());
        &self.block_params[range]
    }

    fn branch_blockparams(&self, block: BlockIndex, _insn: InsnIndex, succ_idx: usize) -> &[VReg] {
        let succ_range = self.branch_block_arg_succ_range.get(block.index());
        debug_assert!(succ_idx < succ_range.len());
        let branch_block_args = self.branch_block_arg_range.get(succ_range.start + succ_idx);
        &self.branch_block_args[branch_block_args]
    }

    fn is_ret(&self, insn: InsnIndex) -> bool {
        match self.insts[insn.index()].is_term() {
            // We treat blocks terminated by an unconditional trap like a return for regalloc.
            MachTerminator::None => self.insts[insn.index()].is_trap(),
            MachTerminator::Ret | MachTerminator::RetCall => true,
            MachTerminator::Branch => false,
        }
    }

    fn is_branch(&self, insn: InsnIndex) -> bool {
        match self.insts[insn.index()].is_term() {
            MachTerminator::Branch => true,
            _ => false,
        }
    }

    fn inst_operands(&self, insn: InsnIndex) -> &[Operand] {
        let range = self.operand_ranges.get(insn.index());
        &self.operands[range]
    }

    fn inst_clobbers(&self, insn: InsnIndex) -> PRegSet {
        self.clobbers.get(&insn).cloned().unwrap_or_default()
    }

    fn num_vregs(&self) -> usize {
        self.vreg_types.len()
    }

    fn debug_value_labels(&self) -> &[(VReg, InsnIndex, InsnIndex, u32)] {
        &self.debug_value_labels
    }

    fn spillslot_size(&self, regclass: RegClass) -> usize {
        self.abi.get_spillslot_size(regclass) as usize
    }

    fn allow_multiple_vreg_defs(&self) -> bool {
        // At least the s390x backend requires this, because the
        // `Loop` pseudo-instruction aggregates all Operands so pinned
        // vregs (RealRegs) may occur more than once.
        true
    }
}

impl<I: VCodeInst> Debug for VRegAllocator<I> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "VRegAllocator {{")?;

        let mut alias_keys = self.vreg_aliases.keys().cloned().collect::<Vec<_>>();
        alias_keys.sort_unstable();
        for key in alias_keys {
            let dest = self.vreg_aliases.get(&key).unwrap();
            writeln!(f, "  {:?} := {:?}", Reg::from(key), Reg::from(*dest))?;
        }

        writeln!(f, "}}")
    }
}

impl<I: VCodeInst> fmt::Debug for VCode<I> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "VCode {{")?;
        writeln!(f, "  Entry block: {}", self.entry.index())?;

        let mut state = Default::default();

        for block in 0..self.num_blocks() {
            let block = BlockIndex::new(block);
            writeln!(
                f,
                "Block {}({:?}):",
                block.index(),
                self.block_params(block)
            )?;
            if let Some(bb) = self.bindex_to_bb(block) {
                writeln!(f, "    (original IR block: {bb})")?;
            }
            for (succ_idx, succ) in self.block_succs(block).iter().enumerate() {
                writeln!(
                    f,
                    "    (successor: Block {}({:?}))",
                    succ.index(),
                    self.branch_blockparams(block, InsnIndex::new(0) /* dummy */, succ_idx)
                )?;
            }
            for inst in self.block_ranges.get(block.index()) {
                writeln!(
                    f,
                    "  Inst {}: {}",
                    inst,
                    self.insts[inst].pretty_print_inst(&mut state)
                )?;
                if let Some(user_stack_map) = self.get_user_stack_map(InsnIndex::new(inst)) {
                    writeln!(f, "    {user_stack_map:?}")?;
                }
            }
        }

        writeln!(f, "}}")?;
        Ok(())
    }
}

/// This structure manages VReg allocation during the lifetime of the VCodeBuilder.
pub struct VRegAllocator<I> {
    /// VReg IR-level types.
    vreg_types: Vec<Type>,

    /// VReg aliases. When the final VCode is built we rewrite all
    /// uses of the keys in this table to their replacement values.
    ///
    /// We use these aliases to rename an instruction's expected
    /// result vregs to the returned vregs from lowering, which are
    /// usually freshly-allocated temps.
    vreg_aliases: FxHashMap<regalloc2::VReg, regalloc2::VReg>,

    /// A deferred error, to be bubbled up to the top level of the
    /// lowering algorithm. We take this approach because we cannot
    /// currently propagate a `Result` upward through ISLE code (the
    /// lowering rules) or some ABI code.
    deferred_error: Option<CodegenError>,

    /// The type of instruction that this allocator makes registers for.
    _inst: core::marker::PhantomData<I>,
}

impl<I: VCodeInst> VRegAllocator<I> {
    /// Make a new VRegAllocator.
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = first_user_vreg_index() + capacity;
        let mut vreg_types = Vec::with_capacity(capacity);
        vreg_types.resize(first_user_vreg_index(), types::INVALID);
        Self {
            vreg_types,
            vreg_aliases: FxHashMap::with_capacity_and_hasher(capacity, Default::default()),
            deferred_error: None,
            _inst: core::marker::PhantomData::default(),
        }
    }

    /// Allocate a fresh ValueRegs.
    pub fn alloc(&mut self, ty: Type) -> CodegenResult<ValueRegs<Reg>> {
        if self.deferred_error.is_some() {
            return Err(CodegenError::CodeTooLarge);
        }
        let v = self.vreg_types.len();
        let (regclasses, tys) = I::rc_for_type(ty)?;

        // Check that new indices are in-bounds for regalloc2's
        // VReg/Operand representation.
        if v + regclasses.len() > VReg::MAX {
            return Err(CodegenError::CodeTooLarge);
        }

        // Check that new indices are in-bounds for our Reg
        // bit-packing on top of the RA2 types, which represents
        // spillslots as well.
        let check = |vreg: regalloc2::VReg| -> CodegenResult<Reg> {
            Reg::from_virtual_reg_checked(vreg).ok_or(CodegenError::CodeTooLarge)
        };

        let regs: ValueRegs<Reg> = match regclasses {
            &[rc0] => ValueRegs::one(check(VReg::new(v, rc0))?),
            &[rc0, rc1] => ValueRegs::two(check(VReg::new(v, rc0))?, check(VReg::new(v + 1, rc1))?),
            // We can extend this if/when we support 32-bit targets; e.g.,
            // an i128 on a 32-bit machine will need up to four machine regs
            // for a `Value`.
            _ => panic!("Value must reside in 1 or 2 registers"),
        };
        for (&reg_ty, &reg) in tys.iter().zip(regs.regs().iter()) {
            let vreg = reg.to_virtual_reg().unwrap();
            debug_assert_eq!(self.vreg_types.len(), vreg.index());
            self.vreg_types.push(reg_ty);
        }

        Ok(regs)
    }

    /// Allocate a fresh ValueRegs, deferring any out-of-vregs
    /// errors. This is useful in places where we cannot bubble a
    /// `CodegenResult` upward easily, and which are known to be
    /// invoked from within the lowering loop that checks the deferred
    /// error status below.
    pub fn alloc_with_deferred_error(&mut self, ty: Type) -> ValueRegs<Reg> {
        match self.alloc(ty) {
            Ok(x) => x,
            Err(e) => {
                self.deferred_error = Some(e);
                self.bogus_for_deferred_error(ty)
            }
        }
    }

    /// Take any deferred error that was accumulated by `alloc_with_deferred_error`.
    pub fn take_deferred_error(&mut self) -> Option<CodegenError> {
        self.deferred_error.take()
    }

    /// Produce an bogus VReg placeholder with the proper number of
    /// registers for the given type. This is meant to be used with
    /// deferred allocation errors (see `Lower::alloc_tmp()`).
    fn bogus_for_deferred_error(&self, ty: Type) -> ValueRegs<Reg> {
        let (regclasses, _tys) = I::rc_for_type(ty).expect("must have valid type");
        match regclasses {
            &[rc0] => ValueRegs::one(VReg::new(0, rc0).into()),
            &[rc0, rc1] => ValueRegs::two(VReg::new(0, rc0).into(), VReg::new(1, rc1).into()),
            _ => panic!("Value must reside in 1 or 2 registers"),
        }
    }

    /// Rewrite any mention of `from` into `to`.
    pub fn set_vreg_alias(&mut self, from: Reg, to: Reg) {
        let from = from.into();
        let resolved_to = self.resolve_vreg_alias(to.into());
        // Disallow cycles (see below).
        assert_ne!(resolved_to, from);

        let old_alias = self.vreg_aliases.insert(from, resolved_to);
        debug_assert_eq!(old_alias, None);
    }

    fn resolve_vreg_alias(&self, mut vreg: regalloc2::VReg) -> regalloc2::VReg {
        // We prevent cycles from existing by resolving targets of
        // aliases eagerly before setting them. If the target resolves
        // to the origin of the alias, then a cycle would be created
        // and the alias is disallowed. Because of the structure of
        // SSA code (one instruction can refer to another's defs but
        // not vice-versa, except indirectly through
        // phis/blockparams), cycles should not occur as we use
        // aliases to redirect vregs to the temps that actually define
        // them.
        while let Some(to) = self.vreg_aliases.get(&vreg) {
            vreg = *to;
        }
        vreg
    }

    #[inline]
    fn debug_assert_no_vreg_aliases(&self, mut list: impl Iterator<Item = VReg>) {
        debug_assert!(list.all(|vreg| !self.vreg_aliases.contains_key(&vreg)));
    }
}

/// This structure tracks the large constants used in VCode that will be emitted separately by the
/// [MachBuffer].
///
/// First, during the lowering phase, constants are inserted using
/// [VCodeConstants.insert]; an intermediate handle, `VCodeConstant`, tracks what constants are
/// used in this phase. Some deduplication is performed, when possible, as constant
/// values are inserted.
///
/// Secondly, during the emission phase, the [MachBuffer] assigns [MachLabel]s for each of the
/// constants so that instructions can refer to the value's memory location. The [MachBuffer]
/// then writes the constant values to the buffer.
#[derive(Default)]
pub struct VCodeConstants {
    constants: PrimaryMap<VCodeConstant, VCodeConstantData>,
    pool_uses: HashMap<Constant, VCodeConstant>,
    well_known_uses: HashMap<*const [u8], VCodeConstant>,
    u64s: HashMap<[u8; 8], VCodeConstant>,
}
impl VCodeConstants {
    /// Initialize the structure with the expected number of constants.
    pub fn with_capacity(expected_num_constants: usize) -> Self {
        Self {
            constants: PrimaryMap::with_capacity(expected_num_constants),
            pool_uses: HashMap::with_capacity(expected_num_constants),
            well_known_uses: HashMap::new(),
            u64s: HashMap::new(),
        }
    }

    /// Insert a constant; using this method indicates that a constant value will be used and thus
    /// will be emitted to the `MachBuffer`. The current implementation can deduplicate constants
    /// that are [VCodeConstantData::Pool] or [VCodeConstantData::WellKnown] but not
    /// [VCodeConstantData::Generated].
    pub fn insert(&mut self, data: VCodeConstantData) -> VCodeConstant {
        match data {
            VCodeConstantData::Generated(_) => self.constants.push(data),
            VCodeConstantData::Pool(constant, _) => match self.pool_uses.get(&constant) {
                None => {
                    let vcode_constant = self.constants.push(data);
                    self.pool_uses.insert(constant, vcode_constant);
                    vcode_constant
                }
                Some(&vcode_constant) => vcode_constant,
            },
            VCodeConstantData::WellKnown(data_ref) => {
                match self.well_known_uses.entry(data_ref as *const [u8]) {
                    Entry::Vacant(v) => {
                        let vcode_constant = self.constants.push(data);
                        v.insert(vcode_constant);
                        vcode_constant
                    }
                    Entry::Occupied(o) => *o.get(),
                }
            }
            VCodeConstantData::U64(value) => match self.u64s.entry(value) {
                Entry::Vacant(v) => {
                    let vcode_constant = self.constants.push(data);
                    v.insert(vcode_constant);
                    vcode_constant
                }
                Entry::Occupied(o) => *o.get(),
            },
        }
    }

    /// Return the number of constants inserted.
    pub fn len(&self) -> usize {
        self.constants.len()
    }

    /// Iterate over the `VCodeConstant` keys inserted in this structure.
    pub fn keys(&self) -> Keys<VCodeConstant> {
        self.constants.keys()
    }

    /// Iterate over the `VCodeConstant` keys and the data (as a byte slice) inserted in this
    /// structure.
    pub fn iter(&self) -> impl Iterator<Item = (VCodeConstant, &VCodeConstantData)> {
        self.constants.iter()
    }

    /// Returns the data associated with the specified constant.
    pub fn get(&self, c: VCodeConstant) -> &VCodeConstantData {
        &self.constants[c]
    }

    /// Checks if the given [VCodeConstantData] is registered as
    /// used by the pool.
    pub fn pool_uses(&self, constant: &VCodeConstantData) -> bool {
        match constant {
            VCodeConstantData::Pool(c, _) => self.pool_uses.contains_key(c),
            _ => false,
        }
    }
}

/// A use of a constant by one or more VCode instructions; see [VCodeConstants].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VCodeConstant(u32);
entity_impl!(VCodeConstant);

/// Identify the different types of constant that can be inserted into [VCodeConstants]. Tracking
/// these separately instead of as raw byte buffers allows us to avoid some duplication.
pub enum VCodeConstantData {
    /// A constant already present in the Cranelift IR
    /// [ConstantPool](crate::ir::constant::ConstantPool).
    Pool(Constant, ConstantData),
    /// A reference to a well-known constant value that is statically encoded within the compiler.
    WellKnown(&'static [u8]),
    /// A constant value generated during lowering; the value may depend on the instruction context
    /// which makes it difficult to de-duplicate--if possible, use other variants.
    Generated(ConstantData),
    /// A constant of at most 64 bits. These are deduplicated as
    /// well. Stored as a fixed-size array of `u8` so that we do not
    /// encounter endianness problems when cross-compiling.
    U64([u8; 8]),
}
impl VCodeConstantData {
    /// Retrieve the constant data as a byte slice.
    pub fn as_slice(&self) -> &[u8] {
        match self {
            VCodeConstantData::Pool(_, d) | VCodeConstantData::Generated(d) => d.as_slice(),
            VCodeConstantData::WellKnown(d) => d,
            VCodeConstantData::U64(value) => &value[..],
        }
    }

    /// Calculate the alignment of the constant data.
    pub fn alignment(&self) -> u32 {
        if self.as_slice().len() <= 8 { 8 } else { 16 }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use core::mem::size_of;

    #[test]
    fn size_of_constant_structs() {
        assert_eq!(size_of::<Constant>(), 4);
        assert_eq!(size_of::<VCodeConstant>(), 4);
        assert_eq!(size_of::<ConstantData>(), 3 * size_of::<usize>());
        assert_eq!(size_of::<VCodeConstantData>(), 4 * size_of::<usize>());
        assert_eq!(
            size_of::<PrimaryMap<VCodeConstant, VCodeConstantData>>(),
            3 * size_of::<usize>()
        );
        // TODO The VCodeConstants structure's memory size could be further optimized.
        // With certain versions of Rust, each `HashMap` in `VCodeConstants` occupied at
        // least 48 bytes, making an empty `VCodeConstants` cost 120 bytes.
    }
}
