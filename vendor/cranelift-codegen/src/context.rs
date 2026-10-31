//! Cranelift compilation context and main entry point.
//!
//! When compiling many small functions, it is important to avoid repeatedly allocating and
//! deallocating the data structures needed for compilation. The `Context` struct is used to hold
//! on to memory allocations between function compilations.
//!
//! The context does not hold a `TargetIsa` instance which has to be provided as an argument
//! instead. This is because an ISA instance is immutable and can be used by multiple compilation
//! contexts concurrently. Typically, you would have one context per compilation thread and only a
//! single ISA instance.

use crate::alias_analysis::AliasAnalysis;
use crate::dominator_tree::DominatorTree;
use crate::egraph::EgraphPass;
use crate::flowgraph::ControlFlowGraph;
use crate::inline::{Inline, do_inlining};
use crate::ir::Function;
use crate::isa::TargetIsa;
use crate::loop_analysis::LoopAnalysis;
use crate::machinst::{CompiledCode, CompiledCodeStencil};
use crate::nan_canonicalization::do_nan_canonicalization;
use crate::remove_constant_phis::do_remove_constant_phis;
use crate::result::{CodegenResult, CompileResult};
use crate::settings::{FlagsOrIsa, OptLevel};
use crate::trace;
use crate::unreachable_code::eliminate_unreachable_code;
use crate::verifier::{VerifierErrors, VerifierResult, verify_context};
use crate::{CompileError, timing};
#[cfg(feature = "souper-harvest")]
use alloc::string::String;
use alloc::vec::Vec;
use cranelift_control::ControlPlane;
use target_lexicon::Architecture;

#[cfg(feature = "souper-harvest")]
use crate::souper_harvest::do_souper_harvest;

/// Persistent data structures and compilation pipeline.
pub struct Context {
    /// The function we're compiling.
    pub func: Function,

    /// The control flow graph of `func`.
    pub cfg: ControlFlowGraph,

    /// Dominator tree for `func`.
    pub domtree: DominatorTree,

    /// Loop analysis of `func`.
    pub loop_analysis: LoopAnalysis,

    /// Result of MachBackend compilation, if computed.
    pub(crate) compiled_code: Option<CompiledCode>,

    /// Flag: do we want a disassembly with the CompiledCode?
    pub want_disasm: bool,

    /// Reused register allocator context.
    pub(crate) regalloc_ctx: regalloc2::Ctx,
}

impl Context {
    /// Allocate a new compilation context.
    ///
    /// The returned instance should be reused for compiling multiple functions in order to avoid
    /// needless allocator thrashing.
    pub fn new() -> Self {
        Self::for_function(Function::new())
    }

    /// Allocate a new compilation context with an existing Function.
    ///
    /// The returned instance should be reused for compiling multiple functions in order to avoid
    /// needless allocator thrashing.
    pub fn for_function(func: Function) -> Self {
        Self {
            func,
            cfg: ControlFlowGraph::new(),
            domtree: DominatorTree::new(),
            loop_analysis: LoopAnalysis::new(),
            compiled_code: None,
            want_disasm: false,
            regalloc_ctx: regalloc2::Ctx::default(),
        }
    }

    /// Clear all data structures in this context.
    pub fn clear(&mut self) {
        self.func.clear();
        self.cfg.clear();
        self.domtree.clear();
        self.loop_analysis.clear();
        self.compiled_code = None;
        self.want_disasm = false;
    }

    /// Returns the compilation result for this function, available after any `compile` function
    /// has been called.
    pub fn compiled_code(&self) -> Option<&CompiledCode> {
        self.compiled_code.as_ref()
    }

    /// Returns the compilation result for this function, available after any `compile` function
    /// has been called.
    pub fn take_compiled_code(&mut self) -> Option<CompiledCode> {
        self.compiled_code.take()
    }

    /// Set the flag to request a disassembly when compiling with a
    /// `MachBackend` backend.
    pub fn set_disasm(&mut self, val: bool) {
        self.want_disasm = val;
    }

    /// Compile the function, and emit machine code into a `Vec<u8>`.
    #[deprecated = "use Context::compile"]
    pub fn compile_and_emit(
        &mut self,
        isa: &dyn TargetIsa,
        mem: &mut Vec<u8>,
        ctrl_plane: &mut ControlPlane,
    ) -> CompileResult<'_, &CompiledCode> {
        let compiled_code = self.compile(isa, ctrl_plane)?;
        mem.extend_from_slice(compiled_code.code_buffer());
        Ok(compiled_code)
    }

    /// Internally compiles the function into a stencil.
    ///
    /// Public only for testing and fuzzing purposes.
    pub fn compile_stencil(
        &mut self,
        isa: &dyn TargetIsa,
        ctrl_plane: &mut ControlPlane,
    ) -> CodegenResult<CompiledCodeStencil> {
        static PRECOMPILE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let result;
        trace!("****** START compiling {}", self.func.display_spec());
        {
            let _tt = timing::compile();

            self.verify_if(isa)?;
            crate::machinst::vcode::PLIRON_SPLIT_BLOCK.with(|c| c.set(None));
            self.optimize(isa, ctrl_plane)?;
            // PLIRON: debug hook — dump the CLIF in its final pre-isel form.
            if *PRECOMPILE.get_or_init(|| std::env::var_os("PLIRON_PRECOMPILE").is_some()) {
                eprintln!("==== clif {} precompile ====\n{}", self.func.name, self.func.display());
            }
            result = isa.compile_function(
                &self.func,
                &self.domtree,
                &mut self.regalloc_ctx,
                self.want_disasm,
                ctrl_plane,
            );
        }
        trace!("****** DONE compiling {}\n", self.func.display_spec());
        result
    }

    /// Optimize the function, performing all compilation steps up to
    /// but not including machine-code lowering and register
    /// allocation.
    ///
    /// Public only for testing purposes.
    pub fn optimize(
        &mut self,
        isa: &dyn TargetIsa,
        ctrl_plane: &mut ControlPlane,
    ) -> CodegenResult<()> {
        static COMPILE_EGRAPH: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static POSTOPT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        log::debug!(
            "Number of CLIF instructions to optimize: {}",
            self.func.dfg.num_insts()
        );
        log::debug!(
            "Number of CLIF blocks to optimize: {}",
            self.func.dfg.num_blocks()
        );

        let opt_level = isa.flags().opt_level();
        crate::trace!(
            "Optimizing (opt level {:?}):\n{}",
            opt_level,
            self.func.display()
        );

        if isa.flags().enable_nan_canonicalization() {
            self.canonicalize_nans(isa)?;
        }

        self.verify_if(isa)?;

        // PLIRON: the pliron pipeline seeds `cfg`/`domtree` with the
        // analyses it already built for this exact function state —
        // only recompute what's missing.
        if !self.cfg.is_valid() {
            self.compute_cfg();
        }
        if !self.domtree.is_valid() {
            self.compute_domtree();
        }
        self.eliminate_unreachable_code(isa)?;
        self.remove_constant_phis(isa)?;

        self.func.dfg.resolve_all_aliases();

        // PLIRON: `PLIRON_COMPILE_EGRAPH=0` skips the compile-path
        // egraph entirely — the pliron pipeline already covers its
        // rewrites, and this measures what it still buys.
        if opt_level != OptLevel::None
            && self.egraph_safe()
            && *COMPILE_EGRAPH
                .get_or_init(|| std::env::var("PLIRON_COMPILE_EGRAPH").as_deref() != Ok("0"))
        {
            self.egraph_pass(isa, ctrl_plane)?;
        }

        // PLIRON: cursor-bump hoisting (see below). Runs after the
        // egraph because its elaborator sinks pure insts toward their
        // uses and would undo the placement.
        self.bump_hoist();

        // PLIRON: rematerializable-address sinking (see below). Runs
        // after the egraph for the same reason: elaboration hoists a
        // `tls_value`/`symbol_value`/`func_addr` out of the loop its
        // only uses sit in, buying it a loop-spanning live range (and
        // regalloc2 boundary moves on every region crossing).
        self.remat_sink(isa.triple().architecture == target_lexicon::Architecture::X86_64);
        if matches!(isa.triple().architecture, target_lexicon::Architecture::Aarch64(_)) {
            self.const_sink();
            self.shrink_split();
        }

        // PLIRON: reorder pure insts so a shared operand's last use lands
        // on a destructive two-address op; elaboration's DFS arg order can
        // strand a later use past a `rotl`-like op, forcing a live-range
        // copy at codegen.
        self.lastuse_schedule();

        if *POSTOPT.get_or_init(|| std::env::var_os("PLIRON_POSTOPT").is_some()) {
            eprintln!(
                "==== clif {} postopt ====\n{}",
                self.func.name,
                self.func.display()
            );
        }

        self.verify_if(isa)?;

        Ok(())
    }

    /// PLIRON: "last-use" scheduling.
    ///
    /// An inst `r` that lowers to a destructive two-address form
    /// (integer/fp ALU ops, rotates, shifts) can consume a source operand
    /// in place only when that operand dies at `r`. The egraph
    /// elaborator emits pure insts in DFS order over argument trees and
    /// routinely strands a shared operand's other use *after* such an
    /// `r`, forcing the register allocator to materialize a copy.
    ///
    /// For each `r`, take the operand(s) its lowering may tie to the
    /// destination (arg0; either arg when commutative) and, when one of
    /// them has uses later in the same block, move those uses — plus the
    /// pure producer chain feeding them — to just before `r`, making `r`
    /// a last use. Pure insts are freely movable upward and moving uses
    /// earlier can only shorten live ranges, so this never creates new
    /// copies elsewhere. Placed insts are frozen so cyclic operand
    /// sharing cannot oscillate; the move set and per-block budget keep
    /// the pass effectively linear.
    fn lastuse_schedule(&mut self) {
        use crate::inst_predicates::is_pure_for_egraph;
        use crate::ir::{Inst, Opcode, Value};
        use crate::{FxHashMap, FxHashSet};
        use alloc::vec::Vec;

        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static LUDEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ENABLED.get_or_init(|| std::env::var_os("PLIRON_LASTUSE").is_none_or(|v| v != "0"))
        {
            return;
        }
        let ludebug = *LUDEBUG.get_or_init(|| std::env::var_os("PLIRON_LUDEBUG").is_some());

        // Opcodes whose lowering can tie a source operand to the
        // destination register (two-address forms: add/sub/and/xor/rol/...
        // on x64 and friends). `r` must be in this set for a last use to
        // enable in-place operation.
        fn rmw(op: Opcode) -> bool {
            use Opcode::*;
            matches!(
                op,
                Iadd | Isub
                    | Imul
                    | Band
                    | Bor
                    | Bxor
                    | Ishl
                    | Ushr
                    | Sshr
                    | Rotl
                    | Rotr
                    | Fadd
                    | Fsub
                    | Fmul
                    | Fdiv
            )
        }

        // For commutative ops either source can be the tied operand; for
        // the rest only arg0 is destructive.
        fn commutative(op: Opcode) -> bool {
            use Opcode::*;
            matches!(op, Iadd | Imul | Band | Bor | Bxor | Fadd | Fmul)
        }

        // Bound on insts relocated for a single operand; keeps the pass
        // effectively linear in block size.
        const MAX_MOVE: usize = 24;

        // uses[v] = every inst that reads v, function-wide.
        let mut uses: FxHashMap<Value, Vec<Inst>> = FxHashMap::default();
        for b in self.func.layout.blocks() {
            for inst in self.func.layout.block_insts(b) {
                for &a in self.func.dfg.inst_args(inst) {
                    uses.entry(a).or_default().push(inst);
                }
            }
        }

        let mut pos: FxHashMap<Inst, u32> = FxHashMap::default();
        let mut r_args: Vec<Value> = Vec::new();
        let mut move_set: Vec<Inst> = Vec::new();
        let mut stack: Vec<Inst> = Vec::new();
        let mut seen: FxHashSet<Inst> = FxHashSet::default();
        // Insts that have already been placed (as moved uses or as the
        // inst a move anchored on). A frozen inst is never relocated
        // again — this guarantees termination even when two insts share
        // operands cyclically.
        let mut frozen: FxHashSet<Inst> = FxHashSet::default();
        let blocks: Vec<_> = self.func.layout.blocks().collect();
        for block in blocks {
            let mut order: Vec<Inst> = self.func.layout.block_insts(block).collect();
            // Bound the work on pathological blocks: at most this many
            // accepted moves per block.
            let mut budget = 128usize;
            pos.clear();
            let mut i = 0;
            while i < order.len() {
                let r = order[i];
                i += 1;
                if !rmw(self.func.dfg.insts[r].opcode()) {
                    continue;
                }
                if pos.is_empty() {
                    pos.extend(order.iter().enumerate().map(|(j, &x)| (x, j as u32)));
                }
                let op = self.func.dfg.insts[r].opcode();
                r_args.clear();
                r_args.extend_from_slice(self.func.dfg.inst_args(r));
                // Candidate operands that lowering could tie to `r`'s
                // destination: arg0 always, arg1 too when commutative.
                let ncand = if commutative(op) && r_args.len() > 1 {
                    2
                } else {
                    1
                };
                'cand: for k in 0..ncand {
                    let v = r_args[k];
                    let Some(us) = uses.get(&v) else {
                        if k == 0 {
                            break 'cand;
                        }
                        continue;
                    };
                    // `v` can only die at `r` if all of its uses are in
                    // this block. If it already has no use after `r`, `r`
                    // is fine as-is — stop (the candidate is satisfied).
                    stack.clear();
                    for &u in us {
                        match pos.get(&u) {
                            Some(&p) if p as usize >= i => stack.push(u),
                            Some(_) => {}
                            None => continue 'cand,
                        }
                    }
                    if stack.is_empty() {
                        break 'cand;
                    }
                    // Try to make `r` the last use of `v`: move every
                    // later use, plus the pure producer chain feeding
                    // them that sits after `r`, to just before `r`.
                    move_set.clear();
                    seen.clear();
                    let mut ok = budget > 0 && !frozen.contains(&r);
                    while ok && let Some(u) = stack.pop() {
                        if u == r || frozen.contains(&u) || !is_pure_for_egraph(&self.func, u) {
                            ok = false;
                            break;
                        }
                        if !seen.insert(u) {
                            continue;
                        }
                        move_set.push(u);
                        if move_set.len() > MAX_MOVE {
                            ok = false;
                            break;
                        }
                        for &a in self.func.dfg.inst_args(u) {
                            if let Some(p) = self.func.dfg.value_def(a).inst() {
                                if p == r {
                                    ok = false;
                                    break;
                                }
                                if pos.get(&p).is_some_and(|&pp| pp as usize >= i) {
                                    stack.push(p);
                                }
                            }
                        }
                    }
                    stack.clear();
                    if !ok {
                        continue;
                    }
                    // Emit `move_set` in original order, just before `r`.
                    budget -= 1;
                    if ludebug {
                        eprintln!("lastuse: move {:?} before {}", move_set, r);
                    }
                    frozen.extend(move_set.iter().copied());
                    frozen.insert(r);
                    move_set.sort_by_key(|u| pos[u]);
                    for u in move_set.iter() {
                        self.func.layout.remove_inst(*u);
                        self.func.layout.insert_inst(*u, r);
                    }
                    // Keep `order`/`pos` consistent: `r` keeps its index
                    // (all moved insts came from after it), then reinsert
                    // the moved insts ahead of it and rescan them.
                    order.retain(|u| !seen.contains(u));
                    let ri = order.iter().position(|&x| x == r).unwrap();
                    for (k, &u) in move_set.iter().enumerate() {
                        order.insert(ri + k, u);
                    }
                    pos.clear();
                    pos.extend(order.iter().enumerate().map(|(j, &x)| (x, j as u32)));
                    i = ri;
                    break 'cand;
                }
            }
        }
    }

    /// PLIRON: cursor-bump hoisting for post-index fusion.
    ///
    /// In a split-block loop the `iadd p, k` advancing a cursor often
    /// sits in the latch while `p` — a block param of the header — is
    /// the load/store base there. The bump is the base's only use past
    /// the memory op, so the base stays live across the edge and
    /// `ldr [x],#k` can't fire. Pulling the bump to just before the
    /// header's terminator makes the base dead after the memory op.
    /// The inst is pure and `p` dominates its original site, so the
    /// move is semantics-preserving whenever the header dominates the
    /// site — which also keeps the result dominating all of its uses
    /// (they're dominated by the original block). Runs after the
    /// egraph because its elaborator sinks pure insts toward their
    /// uses and would undo the placement.
    fn bump_hoist(&mut self) {
        use crate::ir::{Block, Inst, InstructionData, Opcode, Value, ValueDef};
        use crate::{FxHashMap, FxHashSet};

        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static POSTIDX_DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ENABLED.get_or_init(|| {
            std::env::var_os("PLIRON_BUMPHOIST").is_none_or(|v| v != "0")
        }) {
            return;
        }

        // Params used as a load/store base in their own block.
        let mut membase: FxHashMap<crate::ir::Value, crate::ir::Block> = FxHashMap::default();
        for b in self.func.layout.blocks() {
            let params = self.func.dfg.block_params(b);
            if params.is_empty() {
                continue;
            }
            for i in self.func.layout.block_insts(b) {
                let a = match self.func.dfg.insts[i] {
                    InstructionData::Load { arg, .. }
                    | InstructionData::LoadNoOffset { arg, .. } => Some(arg),
                    InstructionData::Store { args, .. }
                    | InstructionData::StoreNoOffset { args, .. } => Some(args[1]),
                    _ => None,
                };
                let Some(a) = a else { continue };
                let a = self.func.dfg.resolve_aliases(a);
                if params.contains(&a) {
                    membase.insert(a, b);
                }
            }
        }
        if membase.is_empty() {
            return;
        }
        // Bump candidates: `iadd p,k`/`isub p,k` outside `p`'s block,
        // `k` a small constant that fits the post-index immediate.
        let mut cand: Vec<(crate::ir::Inst, crate::ir::Block)> = Vec::new();
        for b in self.func.layout.blocks() {
            for i in self.func.layout.block_insts(b) {
                let InstructionData::Binary { opcode, args } = self.func.dfg.insts[i] else {
                    continue;
                };
                let comm = opcode == Opcode::Iadd;
                if !comm && opcode != Opcode::Isub {
                    continue;
                }
                for side in 0..2 {
                    if side == 1 && !comm {
                        break;
                    }
                    let x = self.func.dfg.resolve_aliases(args[side]);
                    let Some(&h) = membase.get(&x) else { continue };
                    let okc = self
                        .func
                        .dfg
                        .value_def(self.func.dfg.resolve_aliases(args[1 - side]))
                        .inst()
                        .is_some_and(|ci| {
                            matches!(
                                self.func.dfg.insts[ci],
                                InstructionData::UnaryImm {
                                    opcode: Opcode::Iconst,
                                    imm,
                                } if (-256..=255).contains(&imm.bits())
                            )
                        });
                    if h == b || !okc {
                        continue;
                    }
                    cand.push((i, h));
                    break;
                }
            }
        }
        if cand.is_empty() {
            return;
        }
        let mut moved = 0;
        for (i, h) in cand {
            let b = self.func.layout.inst_block(i).unwrap();
            // `h` must dominate the site so the moved read of `p` sees
            // the same value; `h` then also dominates every use of the
            // result.
            if !self.domtree.block_dominates(h, b) {
                continue;
            }
            // The constant operand must reach the new position too.
            let InstructionData::Binary { args, .. } = self.func.dfg.insts[i] else {
                continue;
            };
            let c = if membase.contains_key(&self.func.dfg.resolve_aliases(args[0])) {
                args[1]
            } else {
                args[0]
            };
            let okc = match self.func.dfg.value_def(self.func.dfg.resolve_aliases(c)) {
                ValueDef::Result(ci, _) => self
                    .func
                    .layout
                    .inst_block(ci)
                    .is_some_and(|cb| cb == h || self.domtree.block_dominates(cb, h)),
                ValueDef::Param(cb, _) => self.domtree.block_dominates(cb, h),
                _ => false,
            };
            if !okc {
                continue;
            }
            let Some(t) = self.func.layout.last_inst(h) else {
                continue;
            };
            self.func.layout.remove_inst(i);
            self.func.layout.insert_inst(i, t);
            moved += 1;
        }

        // Phase 2: leave the base param dead after its memory op.
        //
        // The fused `ldr [x],#k` only pays off when nothing else still
        // needs the pre-bump value: any later use in `h` or in another
        // block keeps the base's vreg live and blocks fusion. The
        // egraph tends to place its own copy of `iadd p,k` in `h` (its
        // result feeds cold successors), so first fold duplicate bumps
        // into one. Then rewrite every other use of `p` to a remat
        // `isub q,k`/`iadd q,k` placed in the using block — an extra
        // `sub x,#k` on paths that leave the loop, in exchange for
        // dropping a live range from the whole loop.
        let mut remats = 0;
        let mut aliased = false;
        for (&p, &h) in membase.iter() {
            let mut kept: Option<(Inst, Value, i64, bool)> = None;
            let mut dups: Vec<Inst> = Vec::new();
            for i in self.func.layout.block_insts(h) {
                let InstructionData::Binary { opcode, args } = self.func.dfg.insts[i]
                else {
                    continue;
                };
                let is_sub = match opcode {
                    Opcode::Iadd => false,
                    Opcode::Isub => true,
                    _ => continue,
                };
                for side in 0..2 {
                    if is_sub && side == 1 {
                        break;
                    }
                    if self.func.dfg.resolve_aliases(args[side]) != p {
                        continue;
                    }
                    let k = self
                        .func
                        .dfg
                        .value_def(self.func.dfg.resolve_aliases(args[1 - side]))
                        .inst()
                        .and_then(|ci| match self.func.dfg.insts[ci] {
                            InstructionData::UnaryImm {
                                opcode: Opcode::Iconst,
                                imm,
                            } => Some(imm.bits()),
                            _ => None,
                        });
                    let Some(k) = k else { break };
                    if let Some((_, _, qk, qsub)) = kept {
                        if qk == k && qsub == is_sub {
                            dups.push(i);
                        } else {
                            // A second distinct step on the same base:
                            // it reads `p` too, so it stands between the
                            // memory op and `kept` in the liveness window
                            // only if placed later; leave it — remat
                            // below rewrites its operand like any other
                            // use, and dominance of `kept` is unaffected.
                        }
                    } else {
                        kept = Some((i, self.func.dfg.first_result(i), k, is_sub));
                    }
                    break;
                }
            }
            let Some((binst, qres, k, is_sub)) = kept else {
                continue;
            };
            for d in dups {
                let dres = self.func.dfg.first_result(d);
                self.func.layout.remove_inst(d);
                self.func.dfg.detach_inst_results(d);
                self.func.dfg.change_to_alias(dres, qres);
                moved += 1;
                aliased = true;
            }
            // Uses of `p` that would keep it live: any inst in another
            // block, plus insts in `h` strictly after the kept bump.
            // (Uses between the memory op and the bump can't be remated
            // — `ploc` needs the bump result — but those make fusion
            // fail anyway.)
            let mut per_block: FxHashMap<Block, Vec<Inst>> = FxHashMap::default();
            let mut h_uses: Vec<Inst> = Vec::new();
            let mut seen_bump = false;
            let mut raw_set: FxHashSet<Value> = FxHashSet::default();
            for i in self.func.layout.block_insts(h) {
                if i == binst {
                    seen_bump = true;
                    continue;
                }
                if !seen_bump {
                    continue;
                }
                for v in self.func.dfg.inst_values(i) {
                    if self.func.dfg.resolve_aliases(v) == p {
                        h_uses.push(i);
                        raw_set.insert(v);
                        break;
                    }
                }
            }
            for bb in self.func.layout.blocks() {
                if bb == h {
                    continue;
                }
                for i in self.func.layout.block_insts(bb) {
                    for v in self.func.dfg.inst_values(i) {
                        if self.func.dfg.resolve_aliases(v) == p {
                            per_block.entry(bb).or_default().push(i);
                            raw_set.insert(v);
                            break;
                        }
                    }
                }
            }
            if per_block.is_empty() && h_uses.is_empty() {
                continue;
            }
            let ty = self.func.dfg.value_type(qres);
            let rop = if is_sub { Opcode::Iadd } else { Opcode::Isub };
            let h_loc = if h_uses.is_empty() {
                None
            } else {
                let nx = self.func.layout.next_inst(binst);
                Some(self.bump_remat(h, nx, qres, k, rop, ty))
            };
            for (&x, us) in per_block.iter() {
                let ploc = self.bump_remat(x, None, qres, k, rop, ty);
                for &u in us {
                    let dfg = &mut self.func.dfg;
                    dfg.insts[u].map_values(
                        &mut dfg.value_lists,
                        &mut dfg.jump_tables,
                        &mut dfg.exception_tables,
                        |v| if raw_set.contains(&v) { ploc } else { v },
                    );
                }
                remats += 1;
            }
            if let Some(h_loc) = h_loc {
                for &u in h_uses.iter() {
                    let dfg = &mut self.func.dfg;
                    dfg.insts[u].map_values(
                        &mut dfg.value_lists,
                        &mut dfg.jump_tables,
                        &mut dfg.exception_tables,
                        |v| if raw_set.contains(&v) { h_loc } else { v },
                    );
                }
                remats += 1;
            }
        }
        if aliased {
            // Lowering tracks per-value use counts and vregs by the
            // raw arg Value, so a detached alias would count on the
            // wrong slot; splice every use to the kept result now.
            self.func.dfg.resolve_all_aliases();
        }
        if moved > 0
            && *POSTIDX_DEBUG.get_or_init(|| std::env::var_os("PLIRON_POSTIDX_DEBUG").is_some())
        {
            eprintln!(
                "bumphoist {}: moved {} remat {}",
                self.func.name, moved, remats
            );
        }
    }

    /// PLIRON: sink rematerializable-address defs into loop use blocks.
    ///
    /// `tls_value`/`symbol_value`/`func_addr` lower to a `lea`/`movsym`
    /// (plus at most one fixed TLS-segment load on COFF/ELF-IE): pure,
    /// non-trapping, identical on every execution. When such a def sits
    /// outside a loop while all of its uses sit inside, its live range
    /// spans every back edge — holding a register hostage for the whole
    /// loop and forcing regalloc2 boundary moves at each region crossing
    /// (count_alpha's `mov %rbp,%rdx`/`mov %rdx,%rbp` pairs around the
    /// `__pliron_eh` check path). The pliron-side `splink` pass does this
    /// for the CLIF pipeline, but the egraph elaborator re-hoists these
    /// ops as loop-invariant — so repeat the sinking here, post-egraph.
    /// x86-64 only (`x64`): there the clone is one RIP-relative `lea`; on
    /// aarch64 it is `adrp` + `add` per loop iteration (pulldown-cmark's
    /// per-byte escape scan; LLVM hoists). `PLIRON_REMATSINK=1` sinks on
    /// every target, `=0` nowhere.
    /// PLIRON: sink one-instruction constants into the loops that use them
    /// when the loop contains a call (`PLIRON_CONSTSINK=1`, aarch64).
    ///
    /// pliron's licm hoists every constant out of every loop, and the
    /// egraph places them at the loop-invariant point too. Across a loop
    /// with a call, each one then needs a callee-saved register or a spill
    /// slot for the whole loop: regex's `Utf8Sequences::next` defined ~40
    /// masks and bounds in its entry block and spilled most of them (472
    /// bytes of spill slots). A `movz`/`movn`/`orr` at the use is cheaper.
    /// Loops without calls keep hoisted constants (SipHash's magics).
    fn const_sink(&mut self) {
        use crate::ir::{InstructionData, Opcode, Value};
        use crate::{FxHashMap, FxHashSet};
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_CONSTSINK").as_deref() == Ok("1")) {
            return;
        }
        // One aarch64 instruction: `movz`/`movn` (one non-zero halfword in
        // `v` or `!v`) or `orr` with a bitmask immediate.
        fn one_inst(v: u64, bits: u32) -> bool {
            let m = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
            let halves = |x: u64| (0..4).filter(|k| (x >> (16 * k)) & 0xffff != 0).count() <= 1;
            halves(v & m)
                || halves(!v & m)
                || crate::isa::aarch64::inst::ImmLogic::maybe_from_u64(
                    v & m,
                    if bits > 32 { crate::ir::types::I64 } else { crate::ir::types::I32 },
                )
                .is_some()
        }
        let mut cands: Vec<crate::ir::Inst> = Vec::new();
        for b in self.func.layout.blocks() {
            for i in self.func.layout.block_insts(b) {
                if let InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } = self.func.dfg.insts[i] {
                    let ty = self.func.dfg.ctrl_typevar(i);
                    if ty.is_int() && ty.bits() <= 64 && one_inst(imm.bits() as u64, ty.bits()) {
                        cands.push(i);
                    }
                }
            }
        }
        if cands.is_empty() {
            return;
        }
        self.compute_cfg();
        self.compute_loop_analysis();
        // Loops that contain a call (in any nested block).
        let mut calls: FxHashSet<crate::loop_analysis::Loop> = FxHashSet::default();
        for b in self.func.layout.blocks() {
            if !self.func.layout.block_insts(b).any(|i| self.func.dfg.insts[i].opcode().is_call()) {
                continue;
            }
            let mut lp = self.loop_analysis.innermost_loop(b);
            while let Some(l) = lp {
                calls.insert(l);
                lp = self.loop_analysis.loop_parent(l);
            }
        }
        if calls.is_empty() {
            return;
        }
        let cset: FxHashSet<crate::ir::Inst> = cands.iter().copied().collect();
        // Where a constant used in `b` should live: walk out from `b`'s
        // innermost loop to the first loop holding the def; if a calling
        // loop lies between, the copy goes just inside the outermost such
        // calling loop -- in `b` itself, or in the preheader of the
        // call-free loop that `b` sits in within it (a hot inner scan keeps
        // its constant hoisted).
        let target = |this: &Self, db: crate::ir::Block, b: crate::ir::Block| -> Option<crate::ir::Block> {
            let mut chain = Vec::new();
            let mut lp = this.loop_analysis.innermost_loop(b);
            while let Some(l) = lp {
                if this.loop_analysis.is_in_loop(db, l) {
                    break;
                }
                chain.push(l);
                lp = this.loop_analysis.loop_parent(l);
            }
            let j = chain.iter().rposition(|l| calls.contains(l))?;
            if j == 0 {
                return Some(b);
            }
            // Preheader of chain[j - 1]: its header's single outside pred,
            // ending in a plain jump.
            let inner = chain[j - 1];
            let h = this.loop_analysis.loop_header(inner);
            let mut outside = this
                .cfg
                .pred_iter(h)
                .filter(|p| !this.loop_analysis.is_in_loop(p.block, inner));
            let p = outside.next()?;
            if outside.next().is_some()
                || !matches!(this.func.dfg.insts[p.inst], InstructionData::Jump { .. })
            {
                return None;
            }
            Some(p.block)
        };
        // (const inst, target block) -> the use blocks it serves.
        let mut sink: FxHashMap<(crate::ir::Inst, crate::ir::Block), Vec<crate::ir::Block>> =
            FxHashMap::default();
        for b in self.func.layout.blocks() {
            if self.loop_analysis.innermost_loop(b).is_none() {
                continue;
            }
            for i in self.func.layout.block_insts(b) {
                for v in self.func.dfg.inst_values(i) {
                    let v = self.func.dfg.resolve_aliases(v);
                    let Some(d) = self.func.dfg.value_def(v).inst() else {
                        continue;
                    };
                    if !cset.contains(&d) {
                        continue;
                    }
                    let Some(db) = self.func.layout.inst_block(d) else {
                        continue;
                    };
                    if db == b {
                        continue;
                    }
                    let Some(t) = target(self, db, b) else {
                        continue;
                    };
                    let e = sink.entry((d, t)).or_default();
                    if !e.contains(&b) {
                        e.push(b);
                    }
                }
            }
        }
        let mut n = 0;
        for ((d, t), bs) in sink {
            let old = self.func.dfg.first_result(d);
            let data = self.func.dfg.insts[d];
            let ni = self.func.dfg.make_inst(data);
            let ctv = self.func.dfg.ctrl_typevar(d);
            self.func.dfg.make_inst_results(ni, ctv);
            // At the top: `t` may itself be one of the use blocks.
            match self.func.layout.first_inst(t) {
                Some(first) => self.func.layout.insert_inst(ni, first),
                None => self.func.layout.append_inst(ni, t),
            }
            let nv = self.func.dfg.first_result(ni);
            for b in bs {
                for i in self.func.layout.block_insts(b).collect::<Vec<_>>() {
                    if i == ni {
                        continue;
                    }
                    let f = &mut self.func.dfg;
                    let raw: Vec<Value> =
                        f.inst_values(i).filter(|&v| f.resolve_aliases(v) == old).collect();
                    if !raw.is_empty() {
                        f.map_inst_values(i, |v| if raw.contains(&v) { nv } else { v });
                    }
                }
            }
            n += 1;
        }
        // Originals left without uses are dropped by later DCE-free
        // lowering anyway (an unused pure def emits nothing).
        let _ = n;
    }

    /// PLIRON: live-range split for shrink-wrapping (`PLIRON_SHRINKWRAP=1`,
    /// aarch64). The save point (VCode `pliron_shrink_wrap`) can only move
    /// below the entry if no earlier block holds a value in a callee-saved
    /// register; but a value loaded early and used past a call is allocated
    /// one for its whole range (serde_json's `skip_to_escape` loaded the
    /// cursor and length into x20/x21 in its entry block). Here the likely
    /// save block `S` (the nearest common dominator of blocks with calls or
    /// stack-slot operations, outside every loop) takes each value live into
    /// its dominance region as a block parameter, so the early range ends at
    /// the edge and the callee-saved copy starts inside `S`; constants are
    /// cloned into `S` instead.
    fn shrink_split(&mut self) {
        use crate::ir::{BlockArg, InstBuilder, InstructionData, Opcode, Value};
        use crate::cursor::{Cursor, FuncCursor};
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        crate::machinst::vcode::PLIRON_SPLIT_BLOCK.with(|c| c.set(None));
        if !*ON.get_or_init(|| std::env::var("PLIRON_SHRINKWRAP").as_deref() == Ok("1")) {
            return;
        }
        const MAX_LIVEIN: usize = 16;
        let Some(entry) = self.func.layout.entry_block() else { return };
        let frame_op = |op: Opcode| {
            op.is_call()
                || matches!(op, Opcode::StackAddr | Opcode::TlsValue | Opcode::DynamicStackAddr
                    | Opcode::GetFramePointer | Opcode::GetStackPointer | Opcode::GetReturnAddress)
        };
        let fblocks: Vec<crate::ir::Block> = self
            .func
            .layout
            .blocks()
            .filter(|&b| self.func.layout.block_insts(b).any(|i| frame_op(self.func.dfg.insts[i].opcode())))
            .collect();
        if fblocks.is_empty() {
            return;
        }
        self.compute_cfg();
        self.compute_domtree();
        self.compute_loop_analysis();
        // Nearest common dominator: walk `idom` up from each block until a
        // block dominating the next one is found.
        let mut sb = fblocks[0];
        for &b in &fblocks[1..] {
            while !self.domtree.block_dominates(sb, b) {
                match self.domtree.idom(sb) {
                    Some(d) => sb = d,
                    None => return,
                }
            }
        }
        let dbg = std::env::var_os("PLIRON_SHRINKWRAP_DEBUG").is_some();
        if dbg {
            eprintln!(
                "shrink_split {}: frame blocks {:?} -> S {sb} (entry {}, in loop {})",
                self.func.name,
                fblocks,
                sb == entry,
                self.loop_analysis.innermost_loop(sb).is_some()
            );
        }
        // Out of loops: the save runs once, before the outermost loop.
        while sb != entry && self.loop_analysis.innermost_loop(sb).is_some() {
            match self.domtree.idom(sb) {
                Some(d) => sb = d,
                None => return,
            }
        }
        // Closed region: every block reachable from S must be dominated by
        // S (a small return block excepted: it is duplicated below), or a
        // value used past S's region stays live across S's calls and keeps
        // its callee-saved register from the entry on (pulldown's
        // `scan_nextline`: the end pointer, used after the SIMD loop rejoins).
        let small_ret = |f: &crate::ir::Function, b: crate::ir::Block| {
            f.layout.last_inst(b).is_some_and(|t| f.dfg.insts[t].opcode().is_return())
                && f.layout.block_insts(b).count() <= 4
        };
        let mut climbs = 0;
        loop {
            if sb == entry || climbs > 32 {
                return;
            }
            let mut seen: crate::FxHashSet<crate::ir::Block> = crate::FxHashSet::default();
            let mut wl: Vec<crate::ir::Block> = self.cfg.succ_iter(sb).collect();
            let mut closed = true;
            while let Some(b) = wl.pop() {
                if !seen.insert(b) {
                    continue;
                }
                if !self.domtree.block_dominates(sb, b) {
                    if small_ret(&self.func, b) {
                        continue;
                    }
                    closed = false;
                    break;
                }
                wl.extend(self.cfg.succ_iter(b));
            }
            if closed {
                break;
            }
            match self.domtree.idom(sb) {
                Some(d) => sb = d,
                None => return,
            }
            climbs += 1;
        }
        if dbg {
            eprintln!("shrink_split: closed save block {sb} after {climbs} climbs");
        }
        // A small return block reached from both sides of S gets a copy for
        // the edges from outside S's region, so every return reachable
        // from S is dominated by it (VCode's shrink-wrap condition).
        let rets: Vec<crate::ir::Block> = self
            .func
            .layout
            .blocks()
            .filter(|&b| {
                !self.domtree.block_dominates(sb, b)
                    && self.func.layout.last_inst(b).is_some_and(|t| self.func.dfg.insts[t].opcode().is_return())
                    && self.func.layout.block_insts(b).count() <= 4
            })
            .collect();
        let mut cloned = false;
        for r in rets {
            let preds: Vec<crate::flowgraph::BlockPredecessor> = self.cfg.pred_iter(r).collect();
            let (inside, outside): (Vec<crate::flowgraph::BlockPredecessor>, Vec<crate::flowgraph::BlockPredecessor>) =
                preds.iter().copied().partition(|p| self.domtree.block_dominates(sb, p.block));
            if inside.is_empty()
                || outside.is_empty()
                || outside.iter().any(|p| !matches!(self.func.dfg.insts[p.inst], InstructionData::Jump { .. } | InstructionData::Brif { .. }))
            {
                continue;
            }
            let nr = self.func.dfg.make_block();
            self.func.layout.insert_block_after(nr, r);
            let mut vmap: crate::FxHashMap<Value, Value> = crate::FxHashMap::default();
            for &p in self.func.dfg.block_params(r).to_vec().iter() {
                let ty = self.func.dfg.value_type(p);
                let np = self.func.dfg.append_block_param(nr, ty);
                vmap.insert(p, np);
            }
            for i in self.func.layout.block_insts(r).collect::<Vec<_>>() {
                let ni = self.func.dfg.clone_inst(i);
                for (o, n) in self.func.dfg.inst_results(i).to_vec().into_iter().zip(self.func.dfg.inst_results(ni).to_vec()) {
                    vmap.insert(o, n);
                }
                let m = vmap.clone();
                self.func.dfg.map_inst_values(ni, |v| m.get(&v).copied().unwrap_or(v));
                self.func.layout.append_inst(ni, nr);
            }
            for p in outside {
                let dfg = &mut self.func.dfg;
                for bc in dfg.insts[p.inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables) {
                    if bc.block(&dfg.value_lists) == r {
                        bc.set_block(nr, &mut dfg.value_lists);
                    }
                }
            }
            cloned = true;
        }
        if cloned {
            self.compute_cfg();
            self.compute_domtree();
            self.compute_loop_analysis();
        }

        // Every edge into S must be a plain jump or brif (args appendable).
        let preds: Vec<crate::ir::Inst> = self.cfg.pred_iter(sb).map(|p| p.inst).collect();
        if preds.is_empty()
            || preds.iter().any(|&i| !matches!(self.func.dfg.insts[i], InstructionData::Jump { .. } | InstructionData::Brif { .. }))
        {
            return;
        }
        let region: Vec<crate::ir::Block> =
            self.func.layout.blocks().filter(|&b| self.domtree.block_dominates(sb, b)).collect();
        let in_region = |f: &crate::ir::Function, dt: &crate::dominator_tree::DominatorTree, v: Value| -> bool {
            match f.dfg.value_def(v) {
                crate::ir::ValueDef::Result(i, _) => f.layout.inst_block(i).is_some_and(|b| dt.block_dominates(sb, b)),
                crate::ir::ValueDef::Param(b, _) => dt.block_dominates(sb, b),
                _ => true,
            }
        };
        let mut liveins: Vec<Value> = Vec::new();
        for &b in &region {
            for i in self.func.layout.block_insts(b) {
                for v in self.func.dfg.inst_values(i) {
                    let v = self.func.dfg.resolve_aliases(v);
                    if !in_region(&self.func, &self.domtree, v) && !liveins.contains(&v) {
                        liveins.push(v);
                    }
                }
            }
        }
        if dbg {
            eprintln!("shrink_split: {} live-ins into {sb}", liveins.len());
        }
        if liveins.is_empty() || liveins.len() > MAX_LIVEIN {
            return;
        }
        crate::machinst::vcode::PLIRON_SPLIT_BLOCK.with(|c| c.set(Some(sb)));
        let mut map: crate::FxHashMap<Value, Value> = crate::FxHashMap::default();
        for &v in &liveins {
            let konst = match self.func.dfg.value_def(v) {
                crate::ir::ValueDef::Result(i, _) => matches!(
                    self.func.dfg.insts[i].opcode(),
                    Opcode::Iconst | Opcode::F32const | Opcode::F64const
                ).then_some(i),
                _ => None,
            };
            let nv = match konst {
                Some(i) => {
                    let data = self.func.dfg.insts[i];
                    let ni = self.func.dfg.make_inst(data);
                    let ctv = self.func.dfg.ctrl_typevar(i);
                    self.func.dfg.make_inst_results(ni, ctv);
                    let mut pos = FuncCursor::new(&mut self.func).at_top(sb);
                    pos.next_inst();
                    match pos.current_inst() {
                        Some(first) => pos.func.layout.insert_inst(ni, first),
                        None => pos.func.layout.append_inst(ni, sb),
                    }
                    let _ = pos.ins();
                    self.func.dfg.first_result(ni)
                }
                None => {
                    let ty = self.func.dfg.value_type(v);
                    let p = self.func.dfg.append_block_param(sb, ty);
                    for &bi in &preds {
                        let dfg = &mut self.func.dfg;
                        for bc in dfg.insts[bi].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables) {
                            if bc.block(&dfg.value_lists) == sb {
                                bc.append_argument(BlockArg::Value(v), &mut dfg.value_lists);
                            }
                        }
                    }
                    p
                }
            };
            map.insert(v, nv);
        }
        for &b in &region {
            for i in self.func.layout.block_insts(b).collect::<Vec<_>>() {
                let f = &mut self.func.dfg;
                let hits = f.inst_values(i).any(|v| map.contains_key(&f.resolve_aliases(v)));
                if hits {
                    let m = map.clone();
                    let resolved: Vec<(Value, Value)> =
                        f.inst_values(i).map(|v| (v, f.resolve_aliases(v))).collect();
                    let rmap: crate::FxHashMap<Value, Value> =
                        resolved.into_iter().filter_map(|(v, r)| m.get(&r).map(|&n| (v, n))).collect();
                    f.map_inst_values(i, |v| rmap.get(&v).copied().unwrap_or(v));
                }
            }
        }
    }

    fn remat_sink(&mut self, x64: bool) {
        use crate::ir::{Inst, InstructionData, Opcode, Value};
        use crate::{FxHashMap, FxHashSet};

        // `Some(on)` forced by `PLIRON_REMATSINK=0|1`, `None` = by target.
        static MODE: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
        static REMATSINK_DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let mode = *MODE.get_or_init(|| match std::env::var("PLIRON_REMATSINK").as_deref() {
            Ok("0") => Some(false),
            Ok("1") => Some(true),
            _ => None,
        });
        if !mode.unwrap_or(x64) {
            return;
        }

        // remat inst -> set of use blocks (other than its def block).
        let mut uses: FxHashMap<Inst, FxHashSet<crate::ir::Block>> = FxHashMap::default();
        for b in self.func.layout.blocks() {
            for i in self.func.layout.block_insts(b) {
                for v in self.func.dfg.inst_values(i) {
                    let v = self.func.dfg.resolve_aliases(v);
                    let Some(d) = self.func.dfg.value_def(v).inst() else {
                        continue;
                    };
                    if self.func.layout.inst_block(d) == Some(b) {
                        continue;
                    }
                    if !matches!(
                        self.func.dfg.insts[d],
                        InstructionData::UnaryGlobalValue {
                            opcode: Opcode::TlsValue
                                | Opcode::SymbolValue
                                | Opcode::FuncAddr,
                            ..
                        }
                    ) {
                        continue;
                    }
                    uses.entry(d).or_default().insert(b);
                }
            }
        }
        // Skip the (re)compute unless a remat addr crosses a block.
        if uses.is_empty() {
            return;
        }
        self.compute_loop_analysis();
        let mut n = 0;
        for (d, bs) in uses {
            let Some(db) = self.func.layout.inst_block(d) else {
                continue;
            };
            let old = self.func.dfg.resolve_aliases(self.func.dfg.first_result(d));
            for &b in &bs {
                // Only sink across a loop boundary: the use's innermost
                // loop must not contain the def.
                let Some(lp) = self.loop_analysis.innermost_loop(b) else {
                    continue;
                };
                if self.loop_analysis.is_in_loop(db, lp) {
                    continue;
                }
                let data = self.func.dfg.insts[d];
                let ni = self.func.dfg.make_inst(data);
                let ctv = self.func.dfg.ctrl_typevar(d);
                self.func.dfg.make_inst_results(ni, ctv);
                match self.func.layout.first_inst(b) {
                    Some(first) => self.func.layout.insert_inst(ni, first),
                    None => self.func.layout.append_inst(ni, b),
                }
                let nv = self.func.dfg.first_result(ni);
                let mut raw: FxHashSet<Value> = FxHashSet::default();
                for i in self.func.layout.block_insts(b) {
                    for v in self.func.dfg.inst_values(i) {
                        if self.func.dfg.resolve_aliases(v) == old {
                            raw.insert(v);
                        }
                    }
                }
                for i in self.func.layout.block_insts(b).collect::<Vec<_>>() {
                    self.func
                        .dfg
                        .map_inst_values(i, |v| if raw.contains(&v) { nv } else { v });
                }
                n += 1;
            }
            // Uses left on `old` (def block, or non-qualifying use
            // blocks) keep the original alive; otherwise drop it.
            let mut still_used = false;
            'outer: for b in self.func.layout.blocks() {
                for i in self.func.layout.block_insts(b) {
                    if self
                        .func
                        .dfg
                        .inst_values(i)
                        .any(|v| self.func.dfg.resolve_aliases(v) == old)
                    {
                        still_used = true;
                        break 'outer;
                    }
                }
            }
            if !still_used {
                self.func.layout.remove_inst(d);
                self.func.dfg.clear_results(d);
            }
        }
        if n > 0
            && *REMATSINK_DEBUG
                .get_or_init(|| std::env::var_os("PLIRON_REMATSINK_DEBUG").is_some())
        {
            eprintln!("rematsink {}: sunk {}", self.func.name, n);
        }
    }

    /// Emit `iconst k; <rop> qres, ck` at the top of `x` (or before
    /// `after` when set) and return the subtraction's result — the
    /// pre-bump value of a cursor param, rematerialized so the cursor
    /// itself can die at its memory op.
    fn bump_remat(
        &mut self,
        x: crate::ir::Block,
        after: Option<crate::ir::Inst>,
        qres: crate::ir::Value,
        k: i64,
        rop: crate::ir::Opcode,
        ty: crate::ir::Type,
    ) -> crate::ir::Value {
        use crate::ir::{InstructionData, Opcode};
        let ck = self.func.dfg.make_inst(InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm: k.into(),
        });
        self.func.dfg.make_inst_results(ck, ty);
        let ckv = self.func.dfg.first_result(ck);
        let ploc = self.func.dfg.make_inst(InstructionData::Binary {
            opcode: rop,
            args: [qres, ckv],
        });
        self.func.dfg.make_inst_results(ploc, ty);
        match after {
            Some(nx) => {
                self.func.layout.insert_inst(ck, nx);
                self.func.layout.insert_inst(ploc, nx);
            }
            None => match self.func.layout.first_inst(x) {
                Some(fi) => {
                    self.func.layout.insert_inst(ck, fi);
                    self.func.layout.insert_inst(ploc, fi);
                }
                None => {
                    self.func.layout.append_inst(ck, x);
                    self.func.layout.append_inst(ploc, x);
                }
            },
        }
        self.func.dfg.first_result(ploc)
    }

    /// Perform function call inlining.
    ///
    /// Returns `true` if any function call was inlined, `false` otherwise.
    pub fn inline(&mut self, inliner: impl Inline) -> CodegenResult<bool> {
        do_inlining(&mut self.func, inliner)
    }

    /// Compile the function,
    ///
    /// Run the function through all the passes necessary to generate
    /// code for the target ISA represented by `isa`. The generated
    /// machine code is not relocated. Instead, any relocations can be
    /// obtained from `compiled_code.buffer.relocs()`.
    ///
    /// Performs any optimizations that are enabled, unless
    /// `optimize()` was already invoked.
    ///
    /// Returns the generated machine code as well as information about
    /// the function's code and read-only data.
    pub fn compile(
        &mut self,
        isa: &dyn TargetIsa,
        ctrl_plane: &mut ControlPlane,
    ) -> CompileResult<'_, &CompiledCode> {
        let stencil = self
            .compile_stencil(isa, ctrl_plane)
            .map_err(|error| CompileError {
                inner: error,
                func: &self.func,
            })?;
        Ok(self
            .compiled_code
            .insert(stencil.apply_params(&self.func.params)))
    }

    /// If available, return information about the code layout in the
    /// final machine code: the offsets (in bytes) of each basic-block
    /// start, and all basic-block edges.
    #[deprecated = "use CompiledCode::get_code_bb_layout"]
    pub fn get_code_bb_layout(&self) -> Option<(Vec<usize>, Vec<(usize, usize)>)> {
        self.compiled_code().map(CompiledCode::get_code_bb_layout)
    }

    /// Creates unwind information for the function.
    ///
    /// Returns `None` if the function has no unwind information.
    #[cfg(feature = "unwind")]
    #[deprecated = "use CompiledCode::create_unwind_info"]
    pub fn create_unwind_info(
        &self,
        isa: &dyn TargetIsa,
    ) -> CodegenResult<Option<crate::isa::unwind::UnwindInfo>> {
        self.compiled_code().unwrap().create_unwind_info(isa)
    }

    /// Run the verifier on the function.
    ///
    /// Also check that the dominator tree and control flow graph are consistent with the function.
    ///
    /// TODO: rename to "CLIF validate" or similar.
    pub fn verify<'a, FOI: Into<FlagsOrIsa<'a>>>(&self, fisa: FOI) -> VerifierResult<()> {
        let mut errors = VerifierErrors::default();
        let _ = verify_context(&self.func, &self.cfg, &self.domtree, fisa, &mut errors);

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Run the verifier only if the `enable_verifier` setting is true.
    pub fn verify_if<'a, FOI: Into<FlagsOrIsa<'a>>>(&self, fisa: FOI) -> CodegenResult<()> {
        let fisa = fisa.into();
        if fisa.flags.enable_verifier() {
            self.verify(fisa)?;
        }
        Ok(())
    }

    /// Perform constant-phi removal on the function.
    pub fn remove_constant_phis<'a, FOI: Into<FlagsOrIsa<'a>>>(
        &mut self,
        fisa: FOI,
    ) -> CodegenResult<()> {
        do_remove_constant_phis(&mut self.func, &mut self.domtree);
        self.verify_if(fisa)?;
        Ok(())
    }

    /// Perform NaN canonicalizing rewrites on the function.
    pub fn canonicalize_nans(&mut self, isa: &dyn TargetIsa) -> CodegenResult<()> {
        // Currently only RiscV64 is the only arch that may not have vector support.
        let has_vector_support = match isa.triple().architecture {
            Architecture::Riscv64(_) => match isa.isa_flags().iter().find(|f| f.name == "has_v") {
                Some(value) => value.as_bool().unwrap_or(false),
                None => false,
            },
            _ => true,
        };
        do_nan_canonicalization(&mut self.func, has_vector_support);
        self.verify_if(isa)
    }

    /// Compute the control flow graph.
    pub fn compute_cfg(&mut self) {
        self.cfg.compute(&self.func)
    }

    /// Compute dominator tree.
    pub fn compute_domtree(&mut self) {
        self.domtree.compute(&self.func, &self.cfg);
    }

    /// Compute the loop analysis.
    pub fn compute_loop_analysis(&mut self) {
        self.loop_analysis
            .compute(&self.func, &self.cfg, &self.domtree)
    }

    /// Compute the control flow graph and dominator tree.
    pub fn flowgraph(&mut self) {
        self.compute_cfg();
        self.compute_domtree()
    }

    /// Perform unreachable code elimination.
    pub fn eliminate_unreachable_code<'a, FOI>(&mut self, fisa: FOI) -> CodegenResult<()>
    where
        FOI: Into<FlagsOrIsa<'a>>,
    {
        let domtree = &self.domtree;
        eliminate_unreachable_code(&mut self.func, &mut self.cfg, |block| {
            domtree.is_reachable(block)
        });
        self.verify_if(fisa)
    }

    /// Replace all redundant loads with the known values in
    /// memory. These are loads whose values were already loaded by
    /// other loads earlier, as well as loads whose values were stored
    /// by a store instruction to the same instruction (so-called
    /// "store-to-load forwarding").
    pub fn replace_redundant_loads(&mut self) -> CodegenResult<()> {
        let mut analysis = AliasAnalysis::new(&self.func, &self.domtree);
        analysis.compute_and_update_aliases(&mut self.func, &self.cfg);
        Ok(())
    }

    /// Harvest candidate left-hand sides for superoptimization with Souper.
    #[cfg(feature = "souper-harvest")]
    pub fn souper_harvest(
        &mut self,
        out: &mut std::sync::mpsc::Sender<String>,
    ) -> CodegenResult<()> {
        do_souper_harvest(&self.func, out);
        Ok(())
    }

    /// PLIRON: the egraph's ISLE prelude computes masks/min/max as u64
    /// (`ty_smin`, `ty_smax`, `ty_mask`, `ty_lane_mask` all `expect`
    /// ≤64), so a >64-bit scalar or >64-lane vector anywhere in the
    /// function panics the whole pass. rustc emits i128 routinely.
    pub fn egraph_safe(&self) -> bool {
        self.func.dfg.values().all(|v| {
            let ty = self.func.dfg.value_type(v);
            (ty.is_vector() || ty.bits() <= 64) && ty.lane_count() <= 64
        })
    }

    /// Run optimizations via the egraph infrastructure.
    pub fn egraph_pass<'a, FOI>(
        &mut self,
        fisa: FOI,
        ctrl_plane: &mut ControlPlane,
    ) -> CodegenResult<()>
    where
        FOI: Into<FlagsOrIsa<'a>>,
    {
        static STATS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let _tt = timing::egraph();

        trace!(
            "About to optimize with egraph phase:\n{}",
            self.func.display()
        );
        let fisa = fisa.into();
        self.compute_loop_analysis();
        let mut alias_analysis = AliasAnalysis::new(&self.func, &self.domtree);
        let mut pass = EgraphPass::new(
            &mut self.func,
            &self.domtree,
            &self.loop_analysis,
            &mut alias_analysis,
            ctrl_plane,
            &mut self.cfg,
        );
        let cfg_maybe_changed = pass.run();
        log::debug!("egraph stats: {:?}", pass.stats);
        if *STATS.get_or_init(|| std::env::var_os("PLIRON_STATS").is_some()) {
            let stats = pass.stats.clone();
            drop(pass);
            eprintln!("egraph stats {:?}: {:?}", self.func.name, stats);
        }
        trace!("After egraph optimization:\n{}", self.func.display());

        // PLIRON: `run` recomputes `cfg` when it rewrites a terminator
        // and its unreachable-block sweep maintains it — the graph is
        // always valid here. `domtree` is stale only in that case:
        // pure-node rewrites keep dominance intact.
        if cfg_maybe_changed {
            self.compute_domtree();
        }

        self.verify_if(fisa)?;

        Ok(())
    }
}
