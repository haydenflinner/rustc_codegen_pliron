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
        let result;
        trace!("****** START compiling {}", self.func.display_spec());
        {
            let _tt = timing::compile();

            self.verify_if(isa)?;
            self.optimize(isa, ctrl_plane)?;
            // PLIRON: debug hook — dump the CLIF in its final pre-isel form.
            if std::env::var_os("PLIRON_PRECOMPILE").is_some() {
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

        self.compute_cfg();
        self.compute_domtree();
        self.eliminate_unreachable_code(isa)?;
        self.remove_constant_phis(isa)?;

        self.func.dfg.resolve_all_aliases();

        if opt_level != OptLevel::None {
            self.egraph_pass(isa, ctrl_plane)?;
        }

        // PLIRON: reorder pure insts so a shared operand's last use lands
        // on a destructive two-address op; elaboration's DFS arg order can
        // strand a later use past a `rotl`-like op, forcing a live-range
        // copy at codegen.
        self.lastuse_schedule();
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
        if !*ENABLED.get_or_init(|| std::env::var_os("PLIRON_LASTUSE").is_none_or(|v| v != "0"))
        {
            return;
        }

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
                    if std::env::var_os("PLIRON_LUDEBUG").is_some() {
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

    /// Run optimizations via the egraph infrastructure.
    pub fn egraph_pass<'a, FOI>(
        &mut self,
        fisa: FOI,
        ctrl_plane: &mut ControlPlane,
    ) -> CodegenResult<()>
    where
        FOI: Into<FlagsOrIsa<'a>>,
    {
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
        pass.run();
        log::debug!("egraph stats: {:?}", pass.stats);
        trace!("After egraph optimization:\n{}", self.func.display());

        // Branch optimizations can invalidate these; recompute them.
        self.compute_cfg();
        self.compute_domtree();

        self.verify_if(fisa)?;

        Ok(())
    }
}
