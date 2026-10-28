//! Stackify implementation to produce structured control flow from an
//! arbitrary CFG.
//!
//! See the paper
//!
//! - Norman Ramsey. Beyond Relooper: recursive translation of
//!   unstructured control flow to structured control flow. In ICFP
//!   2022 (Functional Pearl). https://dl.acm.org/doi/10.1145/3547621
//!
//! for more details on how this algorithm works.

use crate::cfg::CFGInfo;
use crate::entity::EntityRef;
use crate::ir::{Block, BlockTarget, FunctionBody, Terminator, Type, Value};
use fxhash::FxHashSet as HashSet;
use std::convert::TryFrom;

#[derive(Clone, Debug)]
pub enum WasmBlock<'a> {
    /// A Wasm block that has the given contents and whose label jumps
    /// to the given CFG block exit.
    Block {
        body: Vec<WasmBlock<'a>>,
        out: Block,
    },
    /// A Wasm loop that has the given contents and whose label jumps
    /// to the given CFG block header.
    Loop {
        body: Vec<WasmBlock<'a>>,
        header: Block,
    },
    /// A leaf node: one CFG block.
    Leaf { block: Block },
    /// A translated unconditional branch.
    Br { target: WasmLabel },
    /// PLIRON: conditional branch (`br_if`), used when rewriting
    /// `If` into the LLVM-style `block`+`br_if` shape.
    BrIf { cond: Value, target: WasmLabel },
    /// A translated conditional. `results`, when set, is the merge
    /// param list both arms produce on the operand stack instead of
    /// via `BlockParams`+`local.set` (LLVM's typed-`if` shape).
    If {
        cond: Value,
        if_true: Vec<WasmBlock<'a>>,
        if_false: Vec<WasmBlock<'a>>,
        results: Option<&'a [(Type, Value)]>,
    },
    /// A translated select (switch).
    Select {
        selector: Value,
        targets: Vec<WasmLabel>,
        default: WasmLabel,
    },
    /// Blockparam transfer.
    BlockParams {
        from: &'a [Value],
        to: &'a [(Type, Value)],
    },
    /// PLIRON: same-join CondBr — for each param, `select` between the
    /// true/false edge args then `local.set`, instead of an `if`/`else`
    /// with BlockParams in each arm. Matches LLVM's diamond shape.
    SelectJoin {
        cond: Value,
        if_true: &'a [Value],
        if_false: &'a [Value],
        to: &'a [(Type, Value)],
    },
    /// A function return instruction.
    Return { values: &'a [Value] },
    /// An unreachable instruction.
    Unreachable,
}

/// Note that we need to drop iteratively, avoiding recursion in the
/// default derived impl, to avoid blowing the stack (the tree can get
/// very deep).
impl<'a> Drop for WasmBlock<'a> {
    fn drop(&mut self) {
        fn children<'a>(b: &mut WasmBlock<'a>, out: &mut Vec<WasmBlock<'a>>) {
            match b {
                WasmBlock::Block { body, .. } | WasmBlock::Loop { body, .. } => {
                    out.append(body);
                }
                WasmBlock::If {
                    if_true,
                    if_false,
                    ..
                } => {
                    out.append(if_true);
                    out.append(if_false);
                }
                _ => {}
            }
        }
        let mut work = vec![];
        children(self, &mut work);
        while let Some(mut b) = work.pop() {
            children(&mut b, &mut work);
        }
    }
}

/// A Wasm branch target label: number of scopes outward to branch to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WasmLabel(u32);
impl WasmLabel {
    fn new(i: usize) -> WasmLabel {
        WasmLabel(u32::try_from(i).unwrap())
    }
    fn add(&self, extra: usize) -> WasmLabel {
        WasmLabel(self.0.checked_add(u32::try_from(extra).unwrap()).unwrap())
    }
    pub fn index(&self) -> u32 {
        self.0
    }
}

pub struct Context<'a, 'b> {
    body: &'a FunctionBody,
    cfg: &'b CFGInfo,
    merge_nodes: HashSet<Block>,
    loop_headers: HashSet<Block>,
    ctrl_stack: Vec<CtrlEntry>,
    // Explicit recursion:
    // - Stack of actions/continuations.
    process_stack: Vec<StackEntry<'a>>,
    // - Stack of result/body vectors.
    result: Vec<Vec<WasmBlock<'a>>>,
    // - Stack of merge-node-children lists.
    merge_node_children: Vec<Vec<Block>>,
}

#[derive(Clone, Copy, Debug)]
enum CtrlEntry {
    Block { out: Block },
    Loop { header: Block },
    IfThenElse,
}

impl CtrlEntry {
    fn label(&self) -> Block {
        match self {
            CtrlEntry::Block { out } => *out,
            CtrlEntry::Loop { header } => *header,
            CtrlEntry::IfThenElse => Block::invalid(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum StackEntry<'a> {
    DomSubtree(Block),
    EndDomSubtree,
    NodeWithin(Block, usize),
    FinishLoop(Block),
    FinishBlock(Block),
    Else,
    FinishIf(Value),
    DoBranch(Block, &'a BlockTarget),
}

impl<'a, 'b> Context<'a, 'b> {
    pub fn new(body: &'a FunctionBody, cfg: &'b CFGInfo) -> anyhow::Result<Self> {
        let (merge_nodes, loop_headers) = Self::compute_merge_nodes_and_loop_headers(body, cfg)?;
        Ok(Self {
            body,
            cfg,
            merge_nodes,
            loop_headers,
            ctrl_stack: vec![],
            process_stack: vec![],
            result: vec![],
            merge_node_children: vec![],
        })
    }

    fn compute_merge_nodes_and_loop_headers(
        body: &FunctionBody,
        cfg: &CFGInfo,
    ) -> anyhow::Result<(HashSet<Block>, HashSet<Block>)> {
        let mut loop_headers = HashSet::default();
        let mut branched_once = HashSet::default();
        let mut merge_nodes = HashSet::default();

        for (block_rpo, &block) in cfg.rpo.entries() {
            for &succ in &body.blocks[block].succs {
                log::trace!(
                    "block {} ({}) rpo {} has succ {} ({})",
                    block,
                    body.blocks[block].desc,
                    block_rpo,
                    succ,
                    body.blocks[succ].desc,
                );
                let succ_rpo = cfg.rpo_pos[succ].unwrap();
                log::trace!(" -> succ rpo {}", succ_rpo);
                if succ_rpo <= block_rpo {
                    if !cfg.dominates(succ, block) {
                        anyhow::bail!(
                            "Irreducible control flow: edge from {} ({}) to {} ({})",
                            block,
                            body.blocks[block].desc,
                            succ,
                            body.blocks[succ].desc
                        );
                    }
                    // Backward branch.
                    loop_headers.insert(succ);
                } else {
                    // Forward branch.
                    if !branched_once.insert(succ) {
                        merge_nodes.insert(succ);
                    }
                }
            }
        }

        // Make any `select` target a "merge node" too, so it gets its
        // own block.
        for &block in cfg.rpo.values() {
            if let &Terminator::Select {
                ref targets,
                ref default,
                ..
            } = &body.blocks[block].terminator
            {
                for target in &targets[..] {
                    merge_nodes.insert(target.block);
                }
                merge_nodes.insert(default.block);
            }
        }

        Ok((merge_nodes, loop_headers))
    }

    pub fn compute(mut self) -> Vec<WasmBlock<'a>> {
        self.result.push(vec![]);
        self.process_stack
            .push(StackEntry::DomSubtree(self.cfg.entry));
        while let Some(top) = self.process_stack.pop() {
            self.process(top);
        }
        let mut out = self.result.pop().unwrap();
        // PLIRON: rewrite `if`/`else` into `block`+`br_if` (LLVM's
        // shape). `PLIRON_WBRIF=1` rewrites every If (grows CODE —
        // defeats wpeep). `=exit` only rewrites Ifs whose both arms
        // already exit past the if (no fallthrough) — denser and
        // wpeep-friendlier. Default off.
        match std::env::var_os("PLIRON_WBRIF").as_deref().and_then(|v| v.to_str()) {
            Some("1") => rewrite_ifs_to_brif(&mut out, /* only_exits */ false),
            Some("exit") => rewrite_ifs_to_brif(&mut out, /* only_exits */ true),
            _ => {}
        }
        // PLIRON: promote If arms that both end in the same BlockParams
        // into a typed if (values on stack, one set after). Default on;
        // `PLIRON_WIFRET=0` disables.
        if !std::env::var_os("PLIRON_WIFRET").is_some_and(|v| v == "0") {
            promote_if_results(&mut out);
        }
        out
    }

    fn process(&mut self, entry: StackEntry<'a>) {
        match entry {
            StackEntry::DomSubtree(block) => {
                self.handle_dom_subtree(block);
            }
            StackEntry::EndDomSubtree => {
                self.end_dom_subtree();
            }
            StackEntry::NodeWithin(block, start) => {
                self.node_within(block, start);
            }
            StackEntry::FinishLoop(header) => {
                self.finish_loop(header);
            }
            StackEntry::FinishBlock(out) => {
                self.finish_block(out);
            }
            StackEntry::Else => {
                self.else_();
            }
            StackEntry::FinishIf(cond) => {
                self.finish_if(cond);
            }
            StackEntry::DoBranch(source, target) => {
                self.do_branch(source, target);
            }
        }
    }

    fn handle_dom_subtree(&mut self, block: Block) {
        let mut merge_node_children = self
            .cfg
            .dom_children(block)
            .filter(|child| self.merge_nodes.contains(&child))
            .collect::<Vec<_>>();
        // Sort merge nodes so highest RPO number comes first.
        merge_node_children
            .sort_unstable_by_key(|&block| std::cmp::Reverse(self.cfg.rpo_pos[block]));

        let is_loop_header = self.loop_headers.contains(&block);

        log::trace!(
            "handle_dom_subtree: block {} merge_nodes {:?} loop_header {}",
            block,
            merge_node_children,
            is_loop_header
        );

        // `merge_node_children` stack entry is popped by `EndDomSubtree`.
        self.merge_node_children.push(merge_node_children);
        self.process_stack.push(StackEntry::EndDomSubtree);

        if is_loop_header {
            // Control stack and block-list-result-stack entries are
            // popped by `FinishLoop`.
            self.ctrl_stack.push(CtrlEntry::Loop { header: block });
            self.result.push(vec![]);
            self.process_stack.push(StackEntry::FinishLoop(block));
            self.process_stack.push(StackEntry::NodeWithin(block, 0));
        } else {
            // "tail-call" to `NodeWithin` step, but use existing
            // result-stack entry.
            self.process_stack.push(StackEntry::NodeWithin(block, 0));
        }
    }

    fn end_dom_subtree(&mut self) {
        self.merge_node_children.pop();
    }

    fn finish_loop(&mut self, header: Block) {
        self.ctrl_stack.pop();
        let body = self.result.pop().unwrap();
        self.result
            .last_mut()
            .unwrap()
            .push(WasmBlock::Loop { body, header });
    }

    fn resolve_target(ctrl_stack: &[CtrlEntry], target: Block) -> WasmLabel {
        log::trace!("resolve_target: {} in stack {:?}", target, ctrl_stack);
        WasmLabel(
            u32::try_from(
                ctrl_stack
                    .iter()
                    .rev()
                    .position(|frame| frame.label() == target)
                    .expect("Target must be in control stack"),
            )
            .expect("More than 2^32 frames"),
        )
    }

    fn do_branch(&mut self, source: Block, target: &'a BlockTarget) {
        let into = self.result.last_mut().unwrap();
        log::trace!("do_branch: {} -> {:?}", source, target);
        // This will be a branch to some entry in the control stack if
        // the target is either a merge block, or is a backward branch
        // (by RPO number).
        if self.merge_nodes.contains(&target.block)
            || self.cfg.rpo_pos[target.block] <= self.cfg.rpo_pos[source]
        {
            let index = Self::resolve_target(&self.ctrl_stack[..], target.block);
            Self::do_blockparam_transfer(
                &target.args[..],
                &self.body.blocks[target.block].params[..],
                into,
            );
            into.push(WasmBlock::Br { target: index });
        } else {
            // Otherwise, we must dominate the block, so just emit it inline.
            debug_assert!(self.cfg.dominates(source, target.block));
            Self::do_blockparam_transfer(
                &target.args[..],
                &self.body.blocks[target.block].params[..],
                into,
            );
            self.process_stack
                .push(StackEntry::DomSubtree(target.block));
        }
    }

    fn do_branch_select(
        &mut self,
        selector: Value,
        targets: &'a [BlockTarget],
        default: &'a BlockTarget,
    ) {
        let into = self.result.last_mut().unwrap();
        log::trace!("do_branch_select: {:?}, default {:?}", targets, default);
        let mut body = vec![WasmBlock::Select {
            selector,
            targets: (0..targets.len())
                .map(|i| WasmLabel::new(i))
                .collect::<Vec<_>>(),
            default: WasmLabel::new(targets.len()),
        }];

        let mut extra = targets.len() + 1;
        for target in targets.iter().chain(std::iter::once(default)) {
            extra -= 1;
            let outer_body = vec![
                WasmBlock::Block {
                    body,
                    out: Block::invalid(),
                },
                WasmBlock::BlockParams {
                    from: &target.args[..],
                    to: &self.body.blocks[target.block].params[..],
                },
                WasmBlock::Br {
                    target: Self::resolve_target(&self.ctrl_stack[..], target.block).add(extra),
                },
            ];
            body = outer_body;
        }

        into.extend(body.into_iter());
    }

    fn do_blockparam_transfer(
        from: &'a [Value],
        to: &'a [(Type, Value)],
        into: &mut Vec<WasmBlock<'a>>,
    ) {
        into.push(WasmBlock::BlockParams { from, to });
    }

    fn finish_block(&mut self, out: Block) {
        self.ctrl_stack.pop();
        let body = self.result.pop().unwrap();
        self.result
            .last_mut()
            .unwrap()
            .push(WasmBlock::Block { body, out });
    }

    fn else_(&mut self) {
        self.result.push(vec![]);
    }

    fn finish_if(&mut self, cond: Value) {
        let else_body = self.result.pop().unwrap();
        let if_body = self.result.pop().unwrap();
        self.ctrl_stack.pop();
        self.result.last_mut().unwrap().push(WasmBlock::If {
            cond,
            if_true: if_body,
            if_false: else_body,
            results: None,
        });
    }

    fn node_within(&mut self, block: Block, merge_node_start: usize) {
        let merge_nodes = self.merge_node_children.last().unwrap();
        log::trace!("node_within: block {} merge_nodes {:?}", block, merge_nodes);
        let merge_nodes = &merge_nodes[merge_node_start..];
        let into = self.result.last_mut().unwrap();

        if let Some(&first) = merge_nodes.first() {
            // Post-`first` body.
            self.process_stack.push(StackEntry::DomSubtree(first));
            // Block with `first` as its out-label (forward label).
            self.ctrl_stack.push(CtrlEntry::Block { out: first });
            self.result.push(vec![]);
            self.process_stack.push(StackEntry::FinishBlock(first));
            self.process_stack
                .push(StackEntry::NodeWithin(block, merge_node_start + 1));
        } else {
            // Leaf node: emit contents!
            into.push(WasmBlock::Leaf { block });
            match &self.body.blocks[block].terminator {
                &Terminator::Br { ref target } => {
                    self.process_stack.push(StackEntry::DoBranch(block, target));
                }
                &Terminator::CondBr {
                    cond,
                    ref if_true,
                    ref if_false,
                } => {
                    // PLIRON: avoid `if`/`else` when both arms are labeled
                    // branches (merge/backedge). Same join → select-merge;
                    // different joins → `block`+`br_if` fork. `PLIRON_WSEL=0`
                    // / `PLIRON_WBFORK=0` revert each to stock IfThenElse.
                    let t_ctrl = self.is_stack_target(block, if_true);
                    let f_ctrl = self.is_stack_target(block, if_false);
                    if !std::env::var_os("PLIRON_WSEL").is_some_and(|v| v == "0")
                        && t_ctrl
                        && f_ctrl
                        && if_true.block == if_false.block
                        && if_true.args.len() == if_false.args.len()
                        && if_true.args.len()
                            == self.body.blocks[if_true.block].params.len()
                    {
                        self.emit_select_join(cond, if_true, if_false);
                    } else if std::env::var_os("PLIRON_WBFORK").is_some_and(|v| v == "1")
                        && t_ctrl
                        && f_ctrl
                        && if_true.block != if_false.block
                    {
                        // Opt-in: removes if/else but grows CODE (~+7 KiB
                        // regex) — extra block/br and denser local traffic.
                        self.emit_brif_fork(cond, if_true, if_false);
                    } else {
                        self.ctrl_stack.push(CtrlEntry::IfThenElse);
                        self.process_stack.push(StackEntry::FinishIf(cond));
                        self.process_stack
                            .push(StackEntry::DoBranch(block, if_false));
                        self.process_stack.push(StackEntry::Else);
                        self.process_stack
                            .push(StackEntry::DoBranch(block, if_true));
                        self.result.push(vec![]); // if-body
                    }
                }
                &Terminator::Select {
                    value,
                    ref targets,
                    ref default,
                } => {
                    self.do_branch_select(value, targets, default);
                }
                &Terminator::Return { ref values } => {
                    into.push(WasmBlock::Return { values });
                }
                &Terminator::Unreachable | &Terminator::None => {
                    into.push(WasmBlock::Unreachable);
                }
            }
        }
    }

    /// Target is reached by a labeled `br` (merge node or backedge),
    /// not by inlining a dominated subtree.
    fn is_stack_target(&self, source: Block, t: &BlockTarget) -> bool {
        self.merge_nodes.contains(&t.block)
            || self.cfg.rpo_pos[t.block] <= self.cfg.rpo_pos[source]
    }

    /// PLIRON: `brif c, B(args_t), B(args_f)` → select each arg, `br B`.
    fn emit_select_join(&mut self, cond: Value, if_true: &'a BlockTarget, if_false: &'a BlockTarget) {
        let into = self.result.last_mut().unwrap();
        let params = &self.body.blocks[if_true.block].params;
        into.push(WasmBlock::SelectJoin {
            cond,
            if_true: &if_true.args[..],
            if_false: &if_false.args[..],
            to: params,
        });
        let index = Self::resolve_target(&self.ctrl_stack[..], if_true.block);
        into.push(WasmBlock::Br { target: index });
    }

    /// PLIRON: `brif c, T, F` (T≠F, both labeled) →
    /// ```text
    /// block          ;; false-skip
    ///   br_if 0      ;; cond true → skip false arm
    ///   params_F; br F+1
    /// end
    /// params_T; br T
    /// ```
    fn emit_brif_fork(
        &mut self,
        cond: Value,
        if_true: &'a BlockTarget,
        if_false: &'a BlockTarget,
    ) {
        let t_lab = Self::resolve_target(&self.ctrl_stack[..], if_true.block);
        let f_lab = Self::resolve_target(&self.ctrl_stack[..], if_false.block);
        let mut false_body = Vec::new();
        false_body.push(WasmBlock::BrIf {
            cond,
            target: WasmLabel(0),
        });
        Self::do_blockparam_transfer(
            &if_false.args[..],
            &self.body.blocks[if_false.block].params[..],
            &mut false_body,
        );
        false_body.push(WasmBlock::Br {
            target: f_lab.add(1),
        });
        let into = self.result.last_mut().unwrap();
        into.push(WasmBlock::Block {
            body: false_body,
            out: Block::invalid(),
        });
        Self::do_blockparam_transfer(
            &if_true.args[..],
            &self.body.blocks[if_true.block].params[..],
            into,
        );
        into.push(WasmBlock::Br { target: t_lab });
    }
}

/// PLIRON: bump every `Br`/`BrIf` label by `extra` (wrapping new
/// `block`s around an arm shifts relative depths).
fn bump_br_labels<'a>(body: &mut [WasmBlock<'a>], extra: u32) {
    for b in body {
        match b {
            WasmBlock::Br { target } | WasmBlock::BrIf { target, .. } => {
                target.0 = target.0.saturating_add(extra);
            }
            WasmBlock::Block { body, .. } | WasmBlock::Loop { body, .. } => {
                bump_br_labels(body, extra);
            }
            WasmBlock::If {
                if_true,
                if_false,
                ..
            } => {
                bump_br_labels(if_true, extra);
                bump_br_labels(if_false, extra);
            }
            WasmBlock::Select { targets, default, .. } => {
                for t in targets {
                    t.0 = t.0.saturating_add(extra);
                }
                default.0 = default.0.saturating_add(extra);
            }
            _ => {}
        }
    }
}

/// Whether the arm's final branch already leaves the (old) `if` to a
/// label at depth ≥ 1 — i.e. it does not rely on falling through the
/// `if`'s own end. Trailing `br 0` is fallthrough-equivalent.
fn arm_exits_past_if(arm: &[WasmBlock<'_>]) -> bool {
    match arm.last() {
        Some(WasmBlock::Br { target }) if target.index() >= 1 => true,
        Some(WasmBlock::Return { .. }) | Some(WasmBlock::Unreachable) => true,
        _ => false,
    }
}

/// Strip a trailing `br 0` (fallthrough to the old `if` end).
fn strip_trailing_br0<'a>(arm: &mut Vec<WasmBlock<'a>>) {
    if let Some(WasmBlock::Br { target }) = arm.last() {
        if target.index() == 0 {
            arm.pop();
        }
    }
}

/// PLIRON: `If { cond, T, F }` →
/// ```text
/// block          ;; outer join (= old if end)
///   block        ;; skip-false when cond
///     br_if 0    ;; cond true → true arm
///     F'         ;; false arm (labels +1)
///     br 1       ;; to outer, if F' would fall through
///   end
///   T'           ;; true arm (labels unchanged vs old if)
/// end
/// ```
fn rewrite_ifs_to_brif<'a>(body: &mut Vec<WasmBlock<'a>>, only_exits: bool) {
    let mut i = 0;
    while i < body.len() {
        // Recurse first so nested Ifs rewrite bottom-up.
        match &mut body[i] {
            WasmBlock::Block { body: inner, .. } | WasmBlock::Loop { body: inner, .. } => {
                rewrite_ifs_to_brif(inner, only_exits);
            }
            WasmBlock::If {
                if_true, if_false, ..
            } => {
                rewrite_ifs_to_brif(if_true, only_exits);
                rewrite_ifs_to_brif(if_false, only_exits);
            }
            _ => {}
        }
        let Some((cond, mut if_true, mut if_false, results)) = (match &mut body[i] {
            WasmBlock::If {
                cond,
                if_true,
                if_false,
                results,
            } => Some((
                *cond,
                std::mem::take(if_true),
                std::mem::take(if_false),
                *results,
            )),
            _ => None,
        }) else {
            i += 1;
            continue;
        };
        let false_exits = arm_exits_past_if(&if_false);
        let true_exits = arm_exits_past_if(&if_true);
        if only_exits && !(false_exits && true_exits) {
            // Put the If back; only_exits skips fallthrough diamonds.
            body[i] = WasmBlock::If {
                cond,
                if_true,
                if_false,
                results,
            };
            i += 1;
            continue;
        }
        let _ = results; // WBRIF drops typed results (arms become br_if bodies).
        strip_trailing_br0(&mut if_false);
        strip_trailing_br0(&mut if_true);
        bump_br_labels(&mut if_false, 1);
        let mut inner_body = Vec::with_capacity(if_false.len() + 2);
        inner_body.push(WasmBlock::BrIf {
            cond,
            target: WasmLabel(0),
        });
        inner_body.append(&mut if_false);
        if !false_exits {
            // Skip the true arm: branch to the outer block's end.
            inner_body.push(WasmBlock::Br {
                target: WasmLabel(1),
            });
        }
        let outer = WasmBlock::Block {
            body: {
                let mut v = Vec::with_capacity(if_true.len() + 1);
                v.push(WasmBlock::Block {
                    body: inner_body,
                    out: Block::invalid(),
                });
                v.append(&mut if_true);
                v
            },
            out: Block::invalid(),
        };
        body[i] = outer;
        i += 1;
    }
}

/// Strip a trailing `BlockParams` (+ optional trailing `Br`) from an
/// if arm. Returns `(from, to, br_target)`.
fn take_trailing_params<'a>(
    arm: &mut Vec<WasmBlock<'a>>,
) -> Option<(&'a [Value], &'a [(Type, Value)], Option<WasmLabel>)> {
    let br = match arm.last() {
        Some(WasmBlock::Br { target }) => {
            let t = *target;
            arm.pop();
            Some(t)
        }
        _ => None,
    };
    match arm.last() {
        Some(WasmBlock::BlockParams { from, to }) => {
            let from = *from;
            let to = *to;
            arm.pop();
            Some((from, to, br))
        }
        _ => {
            if let Some(t) = br {
                arm.push(WasmBlock::Br { target: t });
            }
            None
        }
    }
}

fn restore_trailing_params<'a>(
    arm: &mut Vec<WasmBlock<'a>>,
    from: &'a [Value],
    to: &'a [(Type, Value)],
    br: Option<WasmLabel>,
) {
    arm.push(WasmBlock::BlockParams { from, to });
    if let Some(t) = br {
        arm.push(WasmBlock::Br { target: t });
    }
}

/// PLIRON: `If` whose arms both end in `BlockParams` to the same
/// merge → typed `if (result …)` leaving the edge args on the stack,
/// then one `local.set` after the if (+ the shared `br` if any).
/// Saves one arm's worth of `local.set`s. Multi-param uses a module
/// functype (`BlockTypeIntern`).
fn promote_if_results<'a>(body: &mut Vec<WasmBlock<'a>>) {
    let mut i = 0;
    while i < body.len() {
        match &mut body[i] {
            WasmBlock::Block { body: inner, .. } | WasmBlock::Loop { body: inner, .. } => {
                promote_if_results(inner);
            }
            WasmBlock::If {
                if_true, if_false, ..
            } => {
                promote_if_results(if_true);
                promote_if_results(if_false);
            }
            _ => {}
        }
        let promoted = (|| {
            let WasmBlock::If {
                if_true,
                if_false,
                results,
                ..
            } = &mut body[i]
            else {
                return None;
            };
            if results.is_some() {
                return None;
            }
            let (from_t, to_t, br_t) = take_trailing_params(if_true)?;
            let (from_f, to_f, br_f) = match take_trailing_params(if_false) {
                Some(x) => x,
                None => {
                    restore_trailing_params(if_true, from_t, to_t, br_t);
                    return None;
                }
            };
            // Same `to` merge (any arity); matching exit br (or both
            // none/br0). Multi-param needs a module functype — only
            // while `compile_with_block_types` allows it.
            let br_ok = match (br_t, br_f) {
                (None, None) => true,
                (Some(a), Some(b)) if a.index() == b.index() => true,
                (Some(a), None) | (None, Some(a)) if a.index() == 0 => true,
                _ => false,
            };
            // Multi-param typed if: opt-in (`PLIRON_WIFRET_MULTI=1`) and
            // only when `compile_with_block_types` is active. Default off
            // — even all-moves sites grew regex-wasm CODE (gets + typeidx
            // LEBs outweighed the saved arm sets; 2026-10-28).
            let multi = to_t.len() > 1
                && super::allow_multi_wifret()
                && std::env::var_os("PLIRON_WIFRET_MULTI")
                    .is_some_and(|v| v != "0" && v != "false");
            let arity_ok = to_t.len() == 1 || multi;
            if to_t.as_ptr() != to_f.as_ptr() || to_t.is_empty() || !br_ok || !arity_ok {
                restore_trailing_params(if_true, from_t, to_t, br_t);
                restore_trailing_params(if_false, from_f, to_f, br_f);
                return None;
            }
            if_true.push(WasmBlock::BlockParams {
                from: from_t,
                to: &[],
            });
            if_false.push(WasmBlock::BlockParams {
                from: from_f,
                to: &[],
            });
            *results = Some(to_t);
            // Shared exit: prefer the non-zero depth; br 0 is fallthrough.
            let br = match (br_t, br_f) {
                (Some(a), _) if a.index() > 0 => Some(a),
                (_, Some(b)) if b.index() > 0 => Some(b),
                _ => None,
            };
            Some((to_t, br))
        })();
        if let Some((to, br)) = promoted {
            let mut insert_at = i + 1;
            body.insert(
                insert_at,
                WasmBlock::BlockParams {
                    from: &[],
                    to,
                },
            );
            insert_at += 1;
            if let Some(t) = br {
                // Labels were relative to inside the if; after the if
                // they are one depth shallower.
                let t = WasmLabel(t.index().saturating_sub(1));
                body.insert(insert_at, WasmBlock::Br { target: t });
                i = insert_at + 1;
            } else {
                i = insert_at;
            }
        } else {
            i += 1;
        }
    }
}
