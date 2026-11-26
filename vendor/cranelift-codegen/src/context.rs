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

    /// PLIRON: the last `egraph_pass`'s `(survivor, original)` value
    /// pairs (`egraph::Stats::renames`), for clients that keep facts
    /// keyed on values.
    pub egraph_renames: Vec<(crate::ir::Value, crate::ir::Value, bool)>,

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
            egraph_renames: Vec::new(),
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
            self.exit_const_sink();
            self.edge_const_sink();
            self.dup_param_elim();
            self.load_remat();
            self.amode_split();
            self.shrink_split();
            self.pliron_constsink();
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
        // call_pre[j] = number of call instructions in `order[..j]` of the
        // block currently being walked; rebuilt with `pos`. A use moved to
        // just before `r` (and the pure producers dragged with it) crosses
        // every call in between, which stretches its live range across a
        // register clobber and typically forces it into a callee-saved
        // register — worse than the copy the in-place op saves. Moves
        // across calls are refused unless `PLIRON_LUCALLX=0`.
        static NOCALLX: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let nocallx =
            *NOCALLX.get_or_init(|| std::env::var("PLIRON_LUCALLX").as_deref() != Ok("0"));
        let mut call_pre: Vec<u32> = Vec::new();
        // PLIRON_LUCARRY (default on, `=0` off): a move that would compute a block
        // parameter's next value (an edge argument into that parameter)
        // ahead of the parameter's own last read makes the two overlap,
        // and the back edge needs a copy — the in-place op the move was
        // for saves less than that (regex's varint loop: `lsr` hoisted
        // above `orr x, #0x80` cost a `mov` and a `b` per byte). Such
        // moves are refused. `carried[p]`: the arguments of every edge
        // into parameter `p`.
        static LUCARRY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        // Default on (with `PLIRON_DEMANDED`). `PLIRON_LUCARRY=0` restores
        // the old hoist. Cranelift reads the environment directly, so the
        // default lives here; `DEFAULT_ON` lists the name for the off switch.
        let lucarry = *LUCARRY.get_or_init(|| std::env::var("PLIRON_LUCARRY").as_deref() != Ok("0"));
        let mut carried: FxHashMap<Value, Vec<Value>> = FxHashMap::default();
        if lucarry {
            for b in self.func.layout.blocks() {
                let Some(t) = self.func.layout.last_inst(b) else { continue };
                let dfg = &self.func.dfg;
                for bc in dfg.insts[t].branch_destination(&dfg.jump_tables, &dfg.exception_tables) {
                    let d = bc.block(&dfg.value_lists);
                    for (k, a) in bc.args(&dfg.value_lists).enumerate() {
                        if let (crate::ir::BlockArg::Value(a), Some(&p)) = (a, dfg.block_params(d).get(k)) {
                            carried.entry(p).or_default().push(a);
                        }
                    }
                }
            }
        }
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
                    call_pre.clear();
                    call_pre.push(0);
                    for &x in &order {
                        let c =
                            call_pre[call_pre.len() - 1] + self.func.dfg.insts[x].opcode().is_call() as u32;
                        call_pre.push(c);
                    }
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
                            Some(&p) if p as usize >= i => {
                                if nocallx && call_pre[p as usize] != call_pre[i] {
                                    continue 'cand;
                                }
                                stack.push(u);
                            }
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
                    // Any parameter `r` reads (not only `v`: the chain
                    // dragged along for a constant operand can hold it).
                    if ok
                        && lucarry
                        && r_args.iter().any(|p| {
                            carried.get(p).is_some_and(|next| {
                                move_set.iter().any(|&u| {
                                    self.func.dfg.inst_results(u).iter().any(|res| next.contains(res))
                                })
                            })
                        })
                    {
                        ok = false;
                    }
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
                    if nocallx {
                        call_pre.clear();
                        call_pre.push(0);
                        for &x in &order {
                            let c = call_pre[call_pre.len() - 1]
                                + self.func.dfg.insts[x].opcode().is_call() as u32;
                            call_pre.push(c);
                        }
                    }
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
        // PLIRON (`PLIRON_BUMPHOIST_LOOPONLY`, default on since 2026-11-22;
        // `=0` the old rule): params of blocks inside loops only. Entry params qualified too: lol_html's
        // `handle_end_tag_hint` kept `self + 481` and rebuilt `self` as
        // `x21 - 481` before each of five uses.
        static LOOPONLY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let looponly = *LOOPONLY.get_or_init(|| std::env::var("PLIRON_BUMPHOIST_LOOPONLY").as_deref() != Ok("0"));
        if looponly {
            self.compute_loop_analysis();
        }
        let mut membase: FxHashMap<crate::ir::Value, crate::ir::Block> = FxHashMap::default();
        for b in self.func.layout.blocks() {
            let params = self.func.dfg.block_params(b);
            if params.is_empty() {
                continue;
            }
            if looponly && self.loop_analysis.innermost_loop(b).is_none() {
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
        // PLIRON: `PLIRON_BUMPHOIST_INLOOP` (default on since 2026-11-20,
        // `=0` off): weigh the fusion
        // against the base's other reads in blocks every trip of the loop it
        // heads runs (they dominate each latch; not other constant bumps).
        // Fused, each such block pays phase 2's remat `sub`; unfused, the
        // bump stays in its latch, which costs a jump when the bump was all
        // that block held. A base with more such blocks than that is left
        // alone by both phases: brotli's `nodes[i + 1]` pointer IV is read
        // in two (`ldr [x4], #20`, then `sub x0, x4, #20` in each; hoisted
        // without the fusion the bump spilled), pulldown-cmark's byte cursor
        // in one, its end test, with a latch of `add; b` (+109k jumps when
        // it was left alone). One pass over the function.
        static INLOOP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let mut inloop_keep: FxHashSet<Value> = FxHashSet::default();
        let mut useblk: FxHashMap<Value, Vec<Block>> = FxHashMap::default();
        if *INLOOP.get_or_init(|| {
            std::env::var_os("PLIRON_BUMPHOIST_INLOOP").is_none_or(|v| v != "0")
        }) {
            self.compute_loop_analysis();
            let la = &self.loop_analysis;
            let mut latches_of: FxHashMap<Block, Vec<Block>> = FxHashMap::default();
            for bb in self.func.layout.blocks() {
                if self.func.layout.is_cold(bb) {
                    continue;
                }
                for i in self.func.layout.block_insts(bb) {
                    let const_bump = match self.func.dfg.insts[i] {
                        InstructionData::Binary { opcode: Opcode::Iadd | Opcode::Isub, args } => {
                            args.iter().any(|&a| {
                                self.func.dfg.value_def(self.func.dfg.resolve_aliases(a)).inst().is_some_and(
                                    |ci| matches!(self.func.dfg.insts[ci], InstructionData::UnaryImm { opcode: Opcode::Iconst, .. }),
                                )
                            })
                        }
                        _ => false,
                    };
                    if const_bump {
                        continue;
                    }
                    for v in self.func.dfg.inst_values(i) {
                        let v = self.func.dfg.resolve_aliases(v);
                        let Some(&h) = membase.get(&v) else { continue };
                        if h == bb || useblk.get(&v).is_some_and(|u| u.last() == Some(&bb)) {
                            continue;
                        }
                        if let Some(lp) = la.innermost_loop(h)
                            && la.loop_header(lp) == h
                            && la.is_in_loop(bb, lp)
                        {
                            // Only a block every trip runs (it dominates
                            // each latch): a use in a conditional arm is
                            // paid on that arm's trips only, and keeping
                            // the bump in the latch costs that block's
                            // jump on every trip (pulldown-cmark +0.2%).
                            let latches = latches_of.entry(h).or_insert_with(|| {
                                self.cfg
                                    .pred_iter(h)
                                    .map(|pr| pr.block)
                                    .filter(|&pb| la.is_in_loop(pb, lp))
                                    .collect::<Vec<Block>>()
                            });
                            if !latches.is_empty()
                                && latches.iter().all(|&l| self.domtree.block_dominates(bb, l))
                            {
                                useblk.entry(v).or_default().push(bb);
                            }
                        }
                    }
                }
            }
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
                    if let Some(u) = useblk.get(&x) {
                        // The latch empties when the bump moves out: only
                        // constants and a jump stay.
                        let empties = self.func.layout.block_insts(b).all(|j| {
                            j == i
                                || matches!(self.func.dfg.insts[j], InstructionData::UnaryImm { opcode: Opcode::Iconst, .. })
                                || self.func.dfg.insts[j].opcode() == Opcode::Jump
                        });
                        if u.len() > 1 + usize::from(empties) {
                            inloop_keep.insert(x);
                            continue;
                        }
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
            // Only the cursor bump belongs in `h`: its result feeds the
            // next iteration's base, so it shows up in the site's
            // terminator args. Other bumps on the same base (`p+2` on a
            // cold decode path) would compute unconditionally in `h` —
            // leave them so phase 2 remats their base operand in `b`.
            let res = self.func.dfg.first_result(i);
            let feeds_edge = self
                .func
                .layout
                .last_inst(b)
                .is_some_and(|t| {
                    self.func
                        .dfg
                        .inst_values(t)
                        .any(|v| self.func.dfg.resolve_aliases(v) == res)
                });
            if !feeds_edge {
                continue;
            }
            // The constant operand must reach the new position too. The
            // egraph's elaborator sinks `iconst`s into their use blocks —
            // a copy placed at the hoist point is free to remat.
            let InstructionData::Binary { args, .. } = self.func.dfg.insts[i] else {
                continue;
            };
            let c = if membase.contains_key(&self.func.dfg.resolve_aliases(args[0])) {
                args[1]
            } else {
                args[0]
            };
            let mut c_remat: Option<(i64, crate::ir::Type)> = None;
            let okc = match self.func.dfg.value_def(self.func.dfg.resolve_aliases(c)) {
                ValueDef::Result(ci, _) => {
                    let reaches = self
                        .func
                        .layout
                        .inst_block(ci)
                        .is_some_and(|cb| cb == h || self.domtree.block_dominates(cb, h));
                    if reaches {
                        true
                    } else {
                        match self.func.dfg.insts[ci] {
                            InstructionData::UnaryImm {
                                opcode: Opcode::Iconst,
                                imm,
                            } => {
                                c_remat =
                                    Some((imm.bits(), self.func.dfg.value_type(
                                        self.func.dfg.first_result(ci),
                                    )));
                                true
                            }
                            _ => false,
                        }
                    }
                }
                ValueDef::Param(cb, _) => self.domtree.block_dominates(cb, h),
                _ => false,
            };
            if !okc {
                continue;
            }
            let Some(t) = self.func.layout.last_inst(h) else {
                continue;
            };
            if let Some((k, ty)) = c_remat {
                let ci = self.func.dfg.make_inst(InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    imm: k.into(),
                });
                self.func.dfg.make_inst_results(ci, ty);
                let cv = self.func.dfg.first_result(ci);
                self.func.layout.insert_inst(ci, t);
                let dfg = &mut self.func.dfg;
                dfg.insts[i].map_values(
                    &mut dfg.value_lists,
                    &mut dfg.jump_tables,
                    &mut dfg.exception_tables,
                    |v| if v == c { cv } else { v },
                );
            }
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
            if inloop_keep.contains(&p) {
                continue;
            }
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
            let mut far_dups: Vec<Inst> = Vec::new();
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
                    // PLIRON: the same bump again in a dominated block
                    // (the elaborator gives each using successor its
                    // own `iadd p,k`) is the kept result, not a use of
                    // `p` to remat as `(q - k) + k`.
                    if let InstructionData::Binary { opcode, args } = self.func.dfg.insts[i]
                        && opcode == (if is_sub { Opcode::Isub } else { Opcode::Iadd })
                    {
                        let side = (0..2usize).find(|&s| {
                            (s == 0 || !is_sub) && self.func.dfg.resolve_aliases(args[s]) == p
                        });
                        let same = side.is_some_and(|s| {
                            self.func
                                .dfg
                                .value_def(self.func.dfg.resolve_aliases(args[1 - s]))
                                .inst()
                                .is_some_and(|ci| {
                                    matches!(
                                        self.func.dfg.insts[ci],
                                        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm }
                                            if imm.bits() == k
                                    )
                                })
                        });
                        if same {
                            far_dups.push(i);
                            continue;
                        }
                    }
                    for v in self.func.dfg.inst_values(i) {
                        if self.func.dfg.resolve_aliases(v) == p {
                            per_block.entry(bb).or_default().push(i);
                            raw_set.insert(v);
                            break;
                        }
                    }
                }
            }
            for d in far_dups {
                let dres = self.func.dfg.first_result(d);
                self.func.layout.remove_inst(d);
                self.func.dfg.detach_inst_results(d);
                self.func.dfg.change_to_alias(dres, qres);
                moved += 1;
                aliased = true;
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
        if !*ON.get_or_init(|| std::env::var("PLIRON_CONSTSINK_OLD").as_deref() == Ok("1")) {
            return;
        }
        // One aarch64 instruction: `movz`/`movn` (one non-zero halfword in
        // `v` or `!v`) or `orr` with a bitmask immediate.
        #[cfg(feature = "arm64")]
        fn bitmask_imm(v: u64, bits: u32) -> bool {
            crate::isa::aarch64::inst::ImmLogic::maybe_from_u64(
                v,
                if bits > 32 { crate::ir::types::I64 } else { crate::ir::types::I32 },
            )
            .is_some()
        }
        // PLIRON: CONSTSINK is aarch64-tuned; without the arm64 ISA
        // compiled in, no bitmask-immediate oracle exists.
        #[cfg(not(feature = "arm64"))]
        fn bitmask_imm(_v: u64, _bits: u32) -> bool {
            false
        }
        fn one_inst(v: u64, bits: u32) -> bool {
            let m = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
            let halves = |x: u64| (0..4).filter(|k| (x >> (16 * k)) & 0xffff != 0).count() <= 1;
            halves(v & m) || halves(!v & m) || bitmask_imm(v & m, bits)
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

    /// PLIRON: constants that are only branch arguments on a loop exit
    /// edge move onto that edge (`PLIRON_EXITCONST=0` disables). The
    /// egraph rematerializes constants in the block that uses them, and for
    /// a branch argument that is the latch: pulldown-cmark's `parse_block`
    /// scan loop materialized the exit path's `(255, 0, 0, 0)` on every
    /// byte. A new edge block holds them and jumps to the exit target.
    fn exit_const_sink(&mut self) {
        use crate::ir::{BlockArg, BlockCall, InstBuilder, InstructionData, Opcode, Value};
        use crate::cursor::{Cursor, FuncCursor};
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_EXITCONST").as_deref() != Ok("0")) {
            return;
        }
        // Only worth the analyses when some brif passes a constant.
        let mut any = false;
        'scan: for b in self.func.layout.blocks() {
            if let Some(t) = self.func.layout.last_inst(b)
                && let InstructionData::Brif { blocks, .. } = self.func.dfg.insts[t]
            {
                for bc in blocks {
                    for a in bc.args(&self.func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a
                            && let Some(i) = self.func.dfg.value_def(v).inst()
                            && self.func.dfg.insts[i].opcode() == Opcode::Iconst
                        {
                            any = true;
                            break 'scan;
                        }
                    }
                }
            }
        }
        if !any {
            return;
        }
        self.compute_cfg();
        self.compute_domtree();
        self.compute_loop_analysis();
        let mut uses: crate::FxHashMap<Value, u32> = crate::FxHashMap::default();
        for b in self.func.layout.blocks() {
            for i in self.func.layout.block_insts(b) {
                for v in self.func.dfg.inst_values(i) {
                    *uses.entry(self.func.dfg.resolve_aliases(v)).or_default() += 1;
                }
            }
        }
        let blocks: Vec<crate::ir::Block> = self.func.layout.blocks().collect();
        let mut n = 0;
        for b in blocks {
            let Some(lp) = self.loop_analysis.innermost_loop(b) else { continue };
            let Some(t) = self.func.layout.last_inst(b) else { continue };
            let InstructionData::Brif { blocks: [e0, e1], .. } = self.func.dfg.insts[t] else { continue };
            if e0.block(&self.func.dfg.value_lists) == e1.block(&self.func.dfg.value_lists) {
                continue;
            }
            for slot in 0..2 {
                let InstructionData::Brif { blocks, .. } = self.func.dfg.insts[t] else { unreachable!() };
                let bc = blocks[slot];
                let target = bc.block(&self.func.dfg.value_lists);
                if self.loop_analysis.is_in_loop(target, lp) {
                    continue;
                }
                let args: Vec<BlockArg> = bc.args(&self.func.dfg.value_lists).collect();
                // Every use of the constant is an argument of this edge (a
                // zero passed for three fields is one value used thrice).
                let in_args = |f: &crate::ir::Function, v: Value| {
                    args.iter().filter(|a| matches!(a, BlockArg::Value(x) if f.dfg.resolve_aliases(*x) == v)).count() as u32
                };
                let movable = |f: &crate::ir::Function, a: &BlockArg| -> Option<crate::ir::Inst> {
                    let BlockArg::Value(v) = *a else { return None };
                    let v = f.dfg.resolve_aliases(v);
                    let i = f.dfg.value_def(v).inst()?;
                    (f.dfg.insts[i].opcode() == Opcode::Iconst
                        && f.layout.inst_block(i) == Some(b)
                        && uses.get(&v).copied() == Some(in_args(f, v)))
                    .then_some(i)
                };
                let mut moves: Vec<crate::ir::Inst> = args.iter().filter_map(|a| movable(&self.func, a)).collect();
                moves.sort_unstable();
                moves.dedup();
                if moves.is_empty() {
                    continue;
                }
                let e = self.func.dfg.make_block();
                self.func.layout.append_block(e);
                for &i in &moves {
                    self.func.layout.remove_inst(i);
                    self.func.layout.append_inst(i, e);
                }
                FuncCursor::new(&mut self.func).at_bottom(e).ins().jump(target, &args);
                let dfg = &mut self.func.dfg;
                let dests = dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables);
                dests[slot] = BlockCall::new(e, core::iter::empty(), &mut dfg.value_lists);
                n += 1;
            }
        }
        // The cached analyses describe the old CFG; the verifier and later
        // passes read them.
        if n > 0 {
            self.compute_cfg();
            self.compute_domtree();
            self.compute_loop_analysis();
        }
    }

    /// PLIRON (`PLIRON_DUPPARAM=1`, opt-in): two parameters of a block
    /// that receive the same value on every incoming edge are one value
    /// (LLVM's duplicate-phi elimination). The egraph proves values equal
    /// and passes the survivor for each, so a block can end up with
    /// `block454(v3069, v3071, v3072)` fed `(v2293, v2293, v2293)` on every
    /// edge (`encoding_rs::mem::check_str_for_latin1_and_bidi`): three
    /// registers and three moves per edge for one value. A parameter whose
    /// argument on an edge is the block's own parameter (a loop passing
    /// both back) counts as equal there when both sides are one of the
    /// pair. The later parameter becomes an alias of the earlier and its
    /// arguments are dropped. Edges with exception-payload arguments are
    /// left alone; at most four rounds.
    fn dup_param_elim(&mut self) {
        use crate::ir::{BlockArg, BlockCall, Value};
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_DUPPARAM").as_deref() == Ok("1")) {
            return;
        }
        let entry = self.func.layout.entry_block();
        for _round in 0..4 {
            // Incoming edges per block: (branch inst, destination index).
            let mut incoming: crate::FxHashMap<crate::ir::Block, Vec<(crate::ir::Inst, usize)>> =
                crate::FxHashMap::default();
            for b in self.func.layout.blocks() {
                let Some(t) = self.func.layout.last_inst(b) else { continue };
                let f = &self.func;
                for (k, bc) in f.dfg.insts[t]
                    .branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
                    .iter()
                    .enumerate()
                {
                    let tb = bc.block(&f.dfg.value_lists);
                    if f.dfg.num_block_params(tb) >= 2 {
                        incoming.entry(tb).or_default().push((t, k));
                    }
                }
            }
            let mut changed = false;
            let mut blocks: Vec<crate::ir::Block> = incoming.keys().copied().collect();
            blocks.sort_unstable();
            for b in blocks {
                if Some(b) == entry {
                    continue;
                }
                let edges = &incoming[&b];
                let params: Vec<Value> = self.func.dfg.block_params(b).to_vec();
                let args: Vec<Vec<Option<Value>>> = edges
                    .iter()
                    .map(|&(t, k)| {
                        let f = &self.func;
                        let bc = f.dfg.insts[t].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)[k];
                        bc.args(&f.dfg.value_lists)
                            .map(|a| match a {
                                BlockArg::Value(v) => Some(f.dfg.resolve_aliases(v)),
                                _ => None,
                            })
                            .collect()
                    })
                    .collect();
                if args.iter().any(|a| a.len() != params.len()) {
                    continue;
                }
                let mut dead = vec![false; params.len()];
                let mut repl: Vec<(Value, Value)> = Vec::new();
                for i in 0..params.len() {
                    if dead[i] {
                        continue;
                    }
                    for j in i + 1..params.len() {
                        if dead[j] || self.func.dfg.value_type(params[i]) != self.func.dfg.value_type(params[j]) {
                            continue;
                        }
                        let pair = [params[i], params[j]];
                        let same = args.iter().all(|a| match (a[i], a[j]) {
                            (Some(x), Some(y)) => x == y || (pair.contains(&x) && pair.contains(&y)),
                            _ => false,
                        });
                        if same {
                            dead[j] = true;
                            repl.push((params[j], params[i]));
                        }
                    }
                }
                if repl.is_empty() {
                    continue;
                }
                changed = true;
                // Drop the dead positions' arguments on every incoming edge.
                for &(t, k) in edges {
                    let dfg = &mut self.func.dfg;
                    let old: Vec<BlockArg> = {
                        let d = dfg.insts[t].branch_destination(&dfg.jump_tables, &dfg.exception_tables);
                        d[k].args(&dfg.value_lists).collect()
                    };
                    let kept: Vec<BlockArg> =
                        old.into_iter().enumerate().filter(|(n, _)| !dead[*n]).map(|(_, a)| a).collect();
                    let nb = BlockCall::new(b, kept.into_iter(), &mut dfg.value_lists);
                    let d = dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables);
                    d[k] = nb;
                }
                for &(dup, keep) in repl.iter().rev() {
                    self.func.dfg.remove_block_param(dup);
                    self.func.dfg.change_to_alias(dup, keep);
                }
            }
            if !changed {
                break;
            }
            self.func.dfg.resolve_all_aliases();
        }
    }

    /// PLIRON (`PLIRON_EDGECONST`, default `2` since 2026-11-22: `br_table`s only; `=1` also `brif`, `=0` off): constants that are only
    /// arguments of a conditional branch's or a `br_table`'s edges are
    /// materialized on those edges. Every argument of a branch is evaluated
    /// before it, so a `match` that passes a different tag to one join per
    /// arm (pulldown-cmark's `item_to_event`: eleven `movz` ahead of the
    /// jump table on every call, each arm then moving one of them into the
    /// join's parameter) executes all of them on every path. Each target
    /// entered by at least two edges gets a lowering edge block anyway;
    /// this makes it explicit and puts a copy of each constant it passes in
    /// it, which then lands in the parameter's register directly. Edges
    /// with the same target and arguments share one block. Loop exits are
    /// `exit_const_sink`'s and already moved.
    /// `=1` handles `brif` and `br_table`, `=2` `br_table` only.
    fn edge_const_sink(&mut self) {
        use crate::cursor::{Cursor, FuncCursor};
        use crate::ir::{BlockArg, BlockCall, InstBuilder, InstructionData, Opcode, Value};
        static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
        let mode = *MODE.get_or_init(|| {
            std::env::var("PLIRON_EDGECONST").ok().and_then(|v| v.parse().ok()).unwrap_or(2)
        });
        if mode == 0 {
            return;
        }
        // `=3`: `brif` too, but only outside loops and not into a loop
        // header (regex's `Fsm::next_state` varint loop gained register
        // moves: +0.9% under `=1`, still with the loop rule alone).
        // A local analysis: the pass changes the CFG, and `self`'s must not
        // be left looking valid.
        let mut la = crate::loop_analysis::LoopAnalysis::new();
        if mode == 3 {
            let cfg = crate::flowgraph::ControlFlowGraph::with_function(&self.func);
            let dt = crate::dominator_tree::DominatorTree::with_function(&self.func, &cfg);
            la.compute(&self.func, &cfg, &dt);
        }
        let is_multi = |d: &InstructionData, b: crate::ir::Block, la: &crate::loop_analysis::LoopAnalysis| match d {
            InstructionData::Brif { .. } => mode == 1 || (mode == 3 && la.innermost_loop(b).is_none()),
            InstructionData::BranchTable { .. } => true,
            _ => false,
        };
        // Only worth a pass when some multi-way branch passes a constant.
        let mut cands = Vec::new();
        for b in self.func.layout.blocks() {
            let Some(t) = self.func.layout.last_inst(b) else { continue };
            if !is_multi(&self.func.dfg.insts[t], b, &la) {
                continue;
            }
            let f = &self.func;
            let has = f.dfg.insts[t]
                .branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
                .iter()
                .any(|bc| {
                    bc.args(&f.dfg.value_lists).any(|a| {
                        matches!(a, BlockArg::Value(v) if f.dfg.value_def(f.dfg.resolve_aliases(v)).inst()
                            .is_some_and(|i| f.dfg.insts[i].opcode() == Opcode::Iconst && f.layout.inst_block(i) == Some(b)))
                    })
                });
            if has {
                cands.push((b, t));
            }
        }
        if cands.is_empty() {
            return;
        }
        // Incoming edges per block (a `br_table` entry is one): a block
        // entered by one edge needs no edge block, the lowering puts the
        // moves at its head.
        let mut in_edges: crate::FxHashMap<crate::ir::Block, u32> = crate::FxHashMap::default();
        let mut uses: crate::FxHashMap<Value, u32> = crate::FxHashMap::default();
        for b in self.func.layout.blocks() {
            for i in self.func.layout.block_insts(b) {
                for v in self.func.dfg.inst_values(i) {
                    *uses.entry(self.func.dfg.resolve_aliases(v)).or_default() += 1;
                }
                let f = &self.func;
                for bc in f.dfg.insts[i].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables) {
                    *in_edges.entry(bc.block(&f.dfg.value_lists)).or_default() += 1;
                }
            }
        }
        let mut n = 0;
        for (b, t) in cands {
            let dests: Vec<BlockCall> = {
                let f = &self.func;
                f.dfg.insts[t].branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables).to_vec()
            };
            // Constants defined in `b` whose every use is an argument of
            // this branch.
            let mut in_args: crate::FxHashMap<Value, u32> = crate::FxHashMap::default();
            for bc in &dests {
                for a in bc.args(&self.func.dfg.value_lists) {
                    if let BlockArg::Value(v) = a {
                        *in_args.entry(self.func.dfg.resolve_aliases(v)).or_default() += 1;
                    }
                }
            }
            let movable = |f: &crate::ir::Function, v: Value| -> bool {
                f.dfg.value_def(v).inst().is_some_and(|i| {
                    f.dfg.insts[i].opcode() == Opcode::Iconst
                        && f.layout.inst_block(i) == Some(b)
                        && uses.get(&v).copied() == in_args.get(&v).copied()
                })
            };
            // Groups of identical (target, args) edges that pass a movable
            // constant into a block with two or more predecessors.
            let mut groups: Vec<(crate::ir::Block, Vec<BlockArg>, Option<crate::ir::Block>)> = Vec::new();
            let mut slot_group: Vec<Option<usize>> = Vec::with_capacity(dests.len());
            for bc in &dests {
                let target = bc.block(&self.func.dfg.value_lists);
                let args: Vec<BlockArg> = bc.args(&self.func.dfg.value_lists).collect();
                let moves = args.iter().any(|a| matches!(a, BlockArg::Value(v) if movable(&self.func, self.func.dfg.resolve_aliases(*v))));
                // `=3`: not into a loop header either (a loop variable's
                // initial value).
                let into_loop = mode == 3
                    && matches!(self.func.dfg.insts[t], InstructionData::Brif { .. })
                    && la.is_loop_header(target).is_some();
                if !moves || into_loop || in_edges.get(&target).copied().unwrap_or(0) < 2 {
                    slot_group.push(None);
                    continue;
                }
                let g = match groups.iter().position(|(tb, ta, _)| *tb == target && *ta == args) {
                    Some(g) => g,
                    None => {
                        groups.push((target, args, None));
                        groups.len() - 1
                    }
                };
                slot_group.push(Some(g));
            }
            if groups.is_empty() {
                continue;
            }
            let mut moved: Vec<crate::ir::Inst> = Vec::new();
            let mut after = b;
            for g in groups.iter_mut() {
                let e = self.func.dfg.make_block();
                self.func.layout.insert_block_after(e, after);
                after = e;
                let mut cur = FuncCursor::new(&mut self.func).at_bottom(e);
                let mut new_args = Vec::with_capacity(g.1.len());
                for a in &g.1 {
                    let BlockArg::Value(v) = *a else {
                        new_args.push(*a);
                        continue;
                    };
                    let v = cur.func.dfg.resolve_aliases(v);
                    if !movable(cur.func, v) {
                        new_args.push(*a);
                        continue;
                    }
                    // One copy per argument: a constant passed for two
                    // parameters (`(1, 1)`) would otherwise keep both
                    // parameters from sharing its register.
                    let i = cur.func.dfg.value_def(v).inst().unwrap();
                    let InstructionData::UnaryImm { imm, .. } = cur.func.dfg.insts[i] else { unreachable!() };
                    let ty = cur.func.dfg.value_type(v);
                    moved.push(i);
                    let c = cur.ins().iconst(ty, imm);
                    new_args.push(BlockArg::Value(c));
                }
                cur.ins().jump(g.0, &new_args);
                g.2 = Some(e);
            }
            {
                let dfg = &mut self.func.dfg;
                let mut new_calls = Vec::with_capacity(groups.len());
                for g in &groups {
                    new_calls.push(BlockCall::new(g.2.unwrap(), core::iter::empty(), &mut dfg.value_lists));
                }
                let d = dfg.insts[t].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables);
                for (slot, g) in slot_group.iter().enumerate() {
                    if let Some(g) = g {
                        d[slot] = new_calls[*g];
                    }
                }
            }
            moved.sort_unstable();
            moved.dedup();
            // A constant whose every use was an argument of a rewritten
            // edge is dead; one still passed on an edge left alone (a
            // target with one predecessor) stays.
            let mut rewritten: crate::FxHashMap<Value, u32> = crate::FxHashMap::default();
            for (bc, g) in dests.iter().zip(&slot_group) {
                if g.is_some() {
                    for a in bc.args(&self.func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a {
                            *rewritten.entry(self.func.dfg.resolve_aliases(v)).or_default() += 1;
                        }
                    }
                }
            }
            for i in moved {
                let v = self.func.dfg.first_result(i);
                if rewritten.get(&v) == in_args.get(&v) {
                    self.func.layout.remove_inst(i);
                }
            }
            n += 1;
        }
        if n > 0 {
            self.compute_cfg();
            self.compute_domtree();
            self.compute_loop_analysis();
        }
    }

    /// PLIRON: rematerializable-constant sinking (`PLIRON_CONSTSINK=1`,
    /// opt-in). `constdedup` and the block-cloning passes park one def
    /// per distinct constant at a dominating block — often the entry —
    /// and the compile-path egraph re-merges any clones a pipeline pass
    /// made, so a constant used only in deep bail arms pins a register
    /// across the whole function: encoding_rs's `utf8_valid_up_to`
    /// banked 0..=8 in x2-x11 and paid five callee-saved pairs for the
    /// ASCII loop's working set, where LLVM materializes a `mov` inside
    /// each cold exit and spills nothing.
    ///
    /// For every `iconst`/`f32const`/`f64const` def: when every use
    /// sits below one deeper common dominator T, move the def to T's
    /// top (no new instruction, only a shorter live range — but never
    /// into a deeper loop nest, which would re-materialize a licm-hoisted
    /// invariant per trip). When the uses share the def's own block or
    /// scatter over sibling subtrees, clone one copy into each use
    /// block just ahead of its earliest use — the same per-site
    /// rematerialization LLVM's spiller performs: uses that fold into
    /// an immediate operand cost nothing at isel, and edge-arg uses get
    /// their `mov` in the block that runs it. Use blocks sharing one
    /// innermost loop get a single shared copy at their in-loop common
    /// dominator instead, so one trip never pays more materializations
    /// than the banked def would.
    fn pliron_constsink(&mut self) {
        use crate::ir::{Block, Inst, InstructionData, Opcode, Value};
        use crate::loop_analysis::Loop;
        use crate::{FxHashMap, FxHashSet};
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_CONSTSINK_V").as_deref() == Ok("1")) {
            return;
        }
        if self.func.layout.num_blocks() > 16384 {
            return;
        }
        let is_const = |d: InstructionData| {
            matches!(
                d,
                InstructionData::UnaryImm {
                    opcode: Opcode::Iconst,
                    ..
                } | InstructionData::UnaryIeee32 { .. }
                    | InstructionData::UnaryIeee64 { .. }
            )
        };
        // aarch64 immediate encodability for the foldable-use test
        // below: `enc_add` covers add/sub/cmp (`Imm12`, or the negated
        // value — `icmp x, #-16` lowers to `cmn`); `enc_log` covers
        // `band`/`bor`/`bxor` bitmask immediates. Narrow logical ops
        // lower through the 32-bit form, so try both extensions.
        #[cfg(feature = "arm64")]
        fn enc_add(v: u64) -> bool {
            crate::isa::aarch64::inst::Imm12::maybe_from_u64(v).is_some()
                || crate::isa::aarch64::inst::Imm12::maybe_from_u64(v.wrapping_neg()).is_some()
        }
        #[cfg(not(feature = "arm64"))]
        fn enc_add(_v: u64) -> bool {
            false
        }
        #[cfg(feature = "arm64")]
        fn enc_log(v: u64, ty: crate::ir::Type) -> bool {
            use crate::isa::aarch64::inst::ImmLogic;
            if ty.bits() >= 32 {
                ImmLogic::maybe_from_u64(v, ty).is_some()
            } else {
                let s = 64 - ty.bits();
                let sext = (((v << s) as i64) >> s) as u32 as u64;
                ImmLogic::maybe_from_u64(v, crate::ir::types::I32).is_some()
                    || ImmLogic::maybe_from_u64(sext, crate::ir::types::I32).is_some()
            }
        }
        #[cfg(not(feature = "arm64"))]
        fn enc_log(_v: u64, _ty: crate::ir::Type) -> bool {
            false
        }
        // Does `u`'s use of `res` (an `iconst` of `bits` in `ty`) fold
        // into an aarch64 immediate operand at lowering? Only the
        // shapes checked here; every other use keeps the banked def.
        // For noncommutative ops the constant must sit in the RHS arg
        // — `isub c, x` and `cmp c, x` have no immediate encoding
        // (which is what keeps regex's hot-loop `mov #1<<32` banked).
        fn folds_imm(
            dfg: &crate::ir::DataFlowGraph,
            res: Value,
            bits: u64,
            ty: crate::ir::Type,
            u: Inst,
        ) -> bool {
            let (args, enc, rhs_only) = match dfg.insts[u] {
                InstructionData::Binary { opcode, args } => match opcode {
                    Opcode::Iadd => (args, enc_add(bits), false),
                    Opcode::Isub => (args, enc_add(bits), true),
                    Opcode::Band | Opcode::Bor | Opcode::Bxor => (args, enc_log(bits, ty), false),
                    Opcode::Ishl | Opcode::Ushr | Opcode::Sshr | Opcode::Rotl | Opcode::Rotr => {
                        (args, bits < 64, true)
                    }
                    _ => return false,
                },
                InstructionData::IntCompare { args, .. } => (args, enc_add(bits), true),
                _ => return false,
            };
            let mut any = false;
            for (k, &v) in args.iter().enumerate() {
                if dfg.resolve_aliases(v) == res {
                    if rhs_only && k != 1 {
                        return false;
                    }
                    any = true;
                }
            }
            any && enc
        }
        // A terminator use of `res` in block `ub` is cheap only when
        // the const rides a rare transition, not the loop's main
        // flow — LLVM's per-edge remat into a rarely-run pred (utf8's
        // bail `jump block276`). The clone lands at the use block's
        // top, so a block on a trip's main path pays the
        // materialization per iteration (regex's `next_state` fed two
        // zeros on an internal jump — +305K `movz`s). Cheap means the
        // block dominates no latch of its innermost loop and every
        // carrying edge leaves that loop or feeds a latch (see
        // below). A selector arg (`brif` cond, `br_table` index)
        // always needs the register and never counts cheap.
        fn term_cheap(
            dfg: &crate::ir::DataFlowGraph,
            cfg: &crate::flowgraph::ControlFlowGraph,
            dt: &crate::dominator_tree::DominatorTree,
            la: &crate::loop_analysis::LoopAnalysis,
            layout: &crate::ir::Layout,
            res: Value,
            u: Inst,
            ub: Block,
        ) -> bool {
            let d = &dfg.insts[u];
            match d {
                InstructionData::Brif { arg, .. }
                | InstructionData::BranchTable { arg, .. } => {
                    if dfg.resolve_aliases(*arg) == res {
                        return false;
                    }
                }
                // `res` as a call or return operand of a terminator
                // (`try_call` counts as one: it has exception edges):
                // the value goes in a register per execution, like any
                // other non-edge use — never the free edge-arg remat
                // this routine prices. brotli's
                // `SetCost(model, 704, .., 0)` inside
                // `BrotliCreateHqZopfliBackwardReferences`'s loop
                // cloned a per-trip `mov` into its call arms.
                InstructionData::TryCall { args, .. }
                | InstructionData::TryCallIndirect { args, .. }
                | InstructionData::MultiAry { args, .. } => {
                    if args
                        .as_slice(&dfg.value_lists)
                        .iter()
                        .any(|&a| dfg.resolve_aliases(a) == res)
                    {
                        return false;
                    }
                }
                _ => {}
            }
            // The latches of `ub`'s innermost loop: back-edge sources,
            // the preds of the header dominated by it.
            let mut latches: FxHashSet<Block> = FxHashSet::default();
            if let Some(l) = la.innermost_loop(ub) {
                let h = la.loop_header(l);
                for p in cfg.pred_iter(h) {
                    let pb = p.block;
                    if dt.dominates(h, pb, layout) {
                        latches.insert(pb);
                    }
                }
                // `ub` dominates a latch: every trip through that
                // back edge ran `ub` first — the copy pays the loop's
                // main path (regex's `next_state`, `c_utf8_seq`).
                if latches.iter().any(|&p| dt.dominates(ub, p, layout)) {
                    return false;
                }
                // Nor may a carrying edge feed a latch: that routes
                // the copy into the back-edge flow, so it
                // materializes every trip the same way
                // (`c_utf8_seq`'s `iconst` args into latch blocks —
                // +103K). An edge into a plain in-loop block is fine
                // — those are the rare transition paths that pay for
                // this (encoding_rs's utf8 bail edge `jump
                // block276`).
                if !d
                    .branch_destination(&dfg.jump_tables, &dfg.exception_tables)
                    .iter()
                    .all(|bc| {
                        let carries = bc
                            .args(&dfg.value_lists)
                            .any(|a| match a {
                                crate::ir::BlockArg::Value(v) => {
                                    dfg.resolve_aliases(v) == res
                                }
                                _ => false,
                            });
                        !carries || !latches.contains(&bc.block(&dfg.value_lists))
                    })
                {
                    return false;
                }
            }
            true
        }
        // One pass: candidate defs in layout order, plus every use site
        // per def (args resolved through aliases, in layout order so a
        // block's sites arrive inst-ordered and contiguous).
        let mut defs: Vec<Inst> = Vec::new();
        let mut sites: FxHashMap<Inst, Vec<(Block, Inst)>> = FxHashMap::default();
        for b in self.func.layout.blocks() {
            for i in self.func.layout.block_insts(b) {
                if is_const(self.func.dfg.insts[i]) {
                    defs.push(i);
                }
                for v in self.func.dfg.inst_values(i) {
                    let v = self.func.dfg.resolve_aliases(v);
                    let Some(d) = self.func.dfg.value_def(v).inst() else {
                        continue;
                    };
                    if is_const(self.func.dfg.insts[d]) {
                        sites.entry(d).or_default().push((b, i));
                    }
                }
            }
        }
        if defs.is_empty() {
            return;
        }
        self.compute_domtree();
        self.compute_loop_analysis();
        // Dominator-tree depth, memoized (entry = 0).
        let mut memo: FxHashMap<Block, u32> = FxHashMap::default();
        fn depth(
            b: Block,
            dt: &crate::dominator_tree::DominatorTree,
            memo: &mut FxHashMap<Block, u32>,
        ) -> Option<u32> {
            if let Some(&d) = memo.get(&b) {
                return Some(d);
            }
            let d = match dt.idom(b) {
                Some(p) if p != b => depth(p, dt, memo)? + 1,
                _ => 0,
            };
            memo.insert(b, d);
            Some(d)
        }
        fn ncd(
            mut a: Block,
            mut b: Block,
            dt: &crate::dominator_tree::DominatorTree,
            memo: &mut FxHashMap<Block, u32>,
        ) -> Option<Block> {
            let mut da = depth(a, dt, memo)?;
            let mut db = depth(b, dt, memo)?;
            while da > db {
                a = dt.idom(a)?;
                da -= 1;
            }
            while db > da {
                b = dt.idom(b)?;
                db -= 1;
            }
            while a != b {
                a = dt.idom(a)?;
                b = dt.idom(b)?;
            }
            Some(a)
        }
        let mut n = 0;
        for d in defs {
            let Some(home) = self.func.layout.inst_block(d) else {
                continue;
            };
            let res = self
                .func
                .dfg
                .resolve_aliases(self.func.dfg.first_result(d));
            let Some(sites) = sites.get(&d) else { continue };
            if sites.is_empty() {
                continue;
            }
            let home_level = self.loop_analysis.loop_level(home).level();
            let mut groups: Vec<(Block, Vec<Inst>)> = Vec::new();
            for &(b, u) in sites {
                match groups.last_mut() {
                    Some((gb, us)) if *gb == b => us.push(u),
                    _ => groups.push((b, vec![u])),
                }
            }
            let home_group = groups.iter().position(|&(b, _)| b == home);
            if home_group.is_none() {
                // Every use sits strictly below: the deepest position
                // still dominating them all is their common dominator (a
                // branch-arg use counts at its own block — the def must
                // dominate the edge's tail). Moving there adds nothing.
                let mut target = groups[0].0;
                let mut ok = self.domtree.is_reachable(target);
                for &(b, _) in &groups[1..] {
                    if !self.domtree.is_reachable(b) {
                        ok = false;
                        break;
                    }
                    match ncd(target, b, &self.domtree, &mut memo) {
                        Some(t) => target = t,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok && target != home
                    && self.loop_analysis.loop_level(target).level() <= home_level
                {
                    if let Some(anchor) = self.func.layout.first_inst(target) {
                        self.func.layout.remove_inst(d);
                        self.func.layout.insert_inst(d, anchor);
                        n += 1;
                        continue;
                    }
                }
            }
            // The uses either share the def's own block or are scattered
            // under sibling subtrees: rematerialize a copy per use site,
            // just ahead of the site's earliest use. Use args reach `res`
            // possibly through aliases; collect the raw spellings once
            // so the rewrite can match them directly.
            let data = self.func.dfg.insts[d];
            let ctv = self.func.dfg.ctrl_typevar(d);
            let mut raw: FxHashSet<Value> = FxHashSet::default();
            for &(_, u) in sites {
                raw.extend(
                    self.func
                        .dfg
                        .inst_values(u)
                        .filter(|&v| self.func.dfg.resolve_aliases(v) == res),
                );
            }
            // Partition the other use blocks by innermost loop.
            // Top-level uses clone per block; blocks sharing one loop
            // share a single copy at their common dominator, so one
            // trip can never pay more materializations than the banked
            // def would (regex's `cached_state` otherwise doubled a
            // `mov #1<<32` in its DFA loop, +215K executed movs).
            //
            // Deeper than `home` a block keeps the banked def unless
            // every use in it is cheap: an edge arg in a block off
            // its loop's main path (LLVM's per-edge remat —
            // utf8's bail preds) or an operand that folds into an
            // aarch64 immediate at lowering (`band #0x8080..`,
            // `cmp #0`, `add #16`) and so costs nothing wherever the
            // copy lands.
            let (cbits, cty) = match data {
                InstructionData::UnaryImm { imm, .. } => {
                    let m = if ctv.bits() >= 64 {
                        u64::MAX
                    } else {
                        (1u64 << ctv.bits()) - 1
                    };
                    (Some(imm.bits() as u64 & m), ctv)
                }
                _ => (None, ctv),
            };
            // `layout` goes in as an argument: the closure must not
            // capture `self.func`, which the rewrite mutates below.
            let cheap_use = |dfg: &crate::ir::DataFlowGraph,
                             layout: &crate::ir::Layout,
                             ub: Block,
                             ui: Inst|
             -> bool {
                if dfg.insts[ui].opcode().is_terminator() {
                    return term_cheap(
                        dfg,
                        &self.cfg,
                        &self.domtree,
                        &self.loop_analysis,
                        layout,
                        res,
                        ui,
                        ub,
                    );
                }
                let Some(bits) = cbits else { return false };
                folds_imm(dfg, res, bits, cty, ui)
            };
            // A deeper-loop clone is only a win when the banked def
            // dies outright — every use then got a strictly-closer
            // copy and the clone is pure live-range shortening
            // (encoding_rs's utf8 bail path). If any deeper group has
            // a non-cheap use the banked register survives anyway, and
            // a partial clone just adds per-trip materializations on
            // top of it (regex's `c_utf8_seq` bled +129K that way).
            let deep_cheap = groups.iter().all(|&(u, ref us)| {
                u == home
                    || self.loop_analysis.loop_level(u).level() <= home_level
                    || us.iter()
                        .all(|&ui| cheap_use(&self.func.dfg, &self.func.layout, u, ui))
            });
            let mut by_loop: FxHashMap<Option<Loop>, Vec<usize>> = FxHashMap::default();
            for (gi, &(u, _)) in groups.iter().enumerate() {
                if u != home {
                    by_loop
                        .entry(self.loop_analysis.innermost_loop(u))
                        .or_default()
                        .push(gi);
                }
            }
            let remat = |func: &mut crate::ir::Function, anchor: Inst| -> Value {
                let ni = func.dfg.make_inst(data);
                func.dfg.make_inst_results(ni, ctv);
                func.layout.insert_inst(ni, anchor);
                func.dfg.first_result(ni)
            };
            let mut cloned = false;
            let mut served = 0;
            for (lp, gis) in by_loop {
                // A multi-block loop group's copies merge at the
                // group's common dominator (always inside the loop:
                // its header dominates every member). Anything else —
                // a lone block, top-level uses, a failed or `home`
                // NCD — keeps one copy per use block.
                let shared = if lp.is_some() && gis.len() > 1 {
                    let mut t = groups[gis[0]].0;
                    let mut ok = t != home;
                    for &gi in &gis[1..] {
                        match ncd(t, groups[gi].0, &self.domtree, &mut memo) {
                            Some(x) => t = x,
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    (ok && t != home).then_some(t)
                } else {
                    None
                };
                match shared {
                    Some(t) if self.loop_analysis.loop_level(t).level() <= home_level => {
                        // If the dominator is itself a use block, land
                        // ahead of its earliest use; else at its top.
                        let anchor = gis
                            .iter()
                            .find(|&&gi| groups[gi].0 == t)
                            .map(|&gi| groups[gi].1[0])
                            .or_else(|| self.func.layout.first_inst(t));
                        let Some(anchor) = anchor else { continue };
                        let nv = remat(&mut self.func, anchor);
                        for &gi in &gis {
                            for &ui in &groups[gi].1 {
                                self.func
                                    .dfg
                                    .map_inst_values(ui, |v| if raw.contains(&v) { nv } else { v });
                            }
                        }
                        cloned = true;
                        served += gis.len();
                    }
                    _ => {
                        // A failed/deeper NCD falls back to one copy
                        // per use block (still guarded below).
                        for &gi in &gis {
                            let (u, ref us) = groups[gi];
                            // Never materialize into a deeper loop
                            // nest than the def sits in — unless the
                            // copies would kill the banked def
                            // (`deep_cheap` above). The banked def
                            // keeps serving the rest (regex's
                            // `cached_state` sank a `mov #1<<32` from
                            // an outer DFA block into an inner slice
                            // loop — +215K movs).
                            if self.loop_analysis.loop_level(u).level() > home_level && !deep_cheap {
                                continue;
                            }
                            let nv = remat(&mut self.func, us[0]);
                            for &ui in &groups[gi].1 {
                                self.func
                                    .dfg
                                    .map_inst_values(ui, |v| if raw.contains(&v) { nv } else { v });
                            }
                            served += 1;
                            cloned = true;
                        }
                    }
                }
            }
            let all_served = served == groups.len() - usize::from(home_group.is_some());
            if let Some(hg) = home_group {
                // The def keeps serving its in-block uses: slide it down
                // to the first of them.
                let first_use = groups[hg].1[0];
                if self.func.layout.next_inst(d) != Some(first_use) {
                    self.func.layout.remove_inst(d);
                    self.func.layout.insert_inst(d, first_use);
                    cloned = true;
                }
            } else if all_served {
                // Every use got a closer copy: the banked def is dead.
                self.func.layout.remove_inst(d);
                self.func.dfg.clear_results(d);
            }
            n += usize::from(cloned);
        }
        if n > 0 {
            self.compute_cfg();
            self.compute_domtree();
            self.compute_loop_analysis();
        }
    }

    /// PLIRON: re-issue a load near a distant use instead of keeping its
    /// value live (`PLIRON_LOADREMAT=1`, opt-in; `PLIRON_LOADREMAT_DIST`
    /// instructions, default 48). Within one block, when nothing between the
    /// previous copy and the use can write memory (no store, call, atomic
    /// or fence), the same `notrap` load reads the same value. blake3's
    /// `compress_pre` loads its 16 message words once, uses each once per
    /// round for 7 rounds, and with 16 state words also live spilled and
    /// reloaded them (67 stack loads and 16 stores; LLVM 44 and 21). A
    /// fresh load from the input block costs what a reload costs and needs
    /// no spill store.
    fn load_remat(&mut self) {
        use crate::ir::{InstructionData, Opcode, Value};
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static DIST: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_LOADREMAT").as_deref() == Ok("1")) {
            return;
        }
        let dist = *DIST.get_or_init(|| {
            std::env::var("PLIRON_LOADREMAT_DIST").ok().and_then(|s| s.parse().ok()).unwrap_or(48)
        });
        let blocks: Vec<crate::ir::Block> = self.func.layout.blocks().collect();
        for b in blocks {
            let insts: Vec<crate::ir::Inst> = self.func.layout.block_insts(b).collect();
            if insts.len() < 2 * dist {
                continue;
            }
            // Position of the latest memory writer before each index.
            let writes = |op: Opcode| op.can_store() || op.is_call() || op.other_side_effects();
            let mut last_write = vec![usize::MAX; insts.len() + 1];
            let mut lw = usize::MAX;
            for (k, &i) in insts.iter().enumerate() {
                last_write[k] = lw;
                if writes(self.func.dfg.insts[i].opcode()) {
                    lw = k;
                }
            }
            for (k, &l) in insts.iter().enumerate() {
                let InstructionData::Load { opcode, .. } = self.func.dfg.insts[l] else { continue };
                let notrap = self.func.dfg.insts[l].memflags_data(&self.func.dfg).is_some_and(|f| f.notrap());
                if !matches!(opcode, Opcode::Load | Opcode::Uload8 | Opcode::Uload16 | Opcode::Uload32
                    | Opcode::Sload8 | Opcode::Sload16 | Opcode::Sload32)
                    || !notrap
                {
                    continue;
                }
                let v = self.func.dfg.first_result(l);
                // In-block uses, in order.
                let mut uses: Vec<(usize, crate::ir::Inst)> = insts[k + 1..]
                    .iter()
                    .enumerate()
                    .filter(|(_, u)| self.func.dfg.inst_values(**u).any(|x| self.func.dfg.resolve_aliases(x) == v))
                    .map(|(j, &u)| (k + 1 + j, u))
                    .collect();
                uses.sort_unstable();
                let mut cur: (usize, Value) = (k, v);
                for (uk, u) in uses {
                    // The value is fresh enough, or a writer intervenes.
                    if uk - cur.0 <= dist || (last_write[uk] != usize::MAX && last_write[uk] >= k) {
                        continue;
                    }
                    let ni = self.func.dfg.clone_inst(l);
                    self.func.layout.insert_inst(ni, u);
                    let nv = self.func.dfg.first_result(ni);
                    let old = cur.1;
                    let f = &mut self.func.dfg;
                    let raw: Vec<Value> = f.inst_values(u).filter(|&x| f.resolve_aliases(x) == v || x == old).collect();
                    f.map_inst_values(u, |x| if raw.contains(&x) { nv } else { x });
                    cur = (uk, nv);
                }
            }
        }
    }

    /// PLIRON: give a loop's shared `base + index` address a copy per
    /// outside use block (`PLIRON_AMODESPLIT`, default on since 2026-11-22, `=0` off). Lowering folds
    /// `p = iadd a, b` into a load's `[a, b]` amode only when the load is
    /// its one direct use (patch 86). serde_json's escape scan loads each
    /// byte through `p = s + i` and also uses `p` on its exit and escape
    /// paths, so the loop materialized `p` every byte (`add; ldurb [p]`,
    /// LLVM `ldrb [s, i]`). When `p`'s one use in its own block is a
    /// zero-offset load or store address and its other uses sit in blocks
    /// of no deeper loop, each of those blocks gets its own `iadd`.
    fn amode_split(&mut self) {
        use crate::ir::{InstructionData, Opcode, Value};
        use crate::{FxHashMap, FxHashSet};
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("PLIRON_AMODESPLIT").as_deref() != Ok("0")) {
            return;
        }
        fn others_dominated(
            dt: &crate::dominator_tree::DominatorTree,
            x: crate::ir::Block,
            all: &[crate::ir::Block],
        ) -> usize {
            all.iter().filter(|&&y| dt.block_dominates(x, y)).count()
        }
        let mut uses: FxHashMap<Value, Vec<(crate::ir::Inst, crate::ir::Block)>> = FxHashMap::default();
        let mut adds = vec![];
        for b in self.func.layout.blocks() {
            for i in self.func.layout.block_insts(b) {
                if let InstructionData::Binary { opcode: Opcode::Iadd, .. } = self.func.dfg.insts[i]
                    && self.func.dfg.ctrl_typevar(i) == crate::ir::types::I64
                {
                    adds.push((i, b));
                }
                for v in self.func.dfg.inst_values(i) {
                    uses.entry(self.func.dfg.resolve_aliases(v)).or_default().push((i, b));
                }
            }
        }
        if adds.is_empty() {
            return;
        }
        self.compute_loop_analysis();
        let mut cands = vec![];
        for (i, b) in adds {
            if self.loop_analysis.innermost_loop(b).is_none() {
                continue;
            }
            let v = self.func.dfg.first_result(i);
            let Some(us) = uses.get(&v) else { continue };
            let mut local = 0;
            let mut others: FxHashSet<crate::ir::Block> = FxHashSet::default();
            for &(u, ub) in us {
                if ub != b {
                    others.insert(ub);
                    continue;
                }
                let r = |x: Value| self.func.dfg.resolve_aliases(x) == v;
                local += match self.func.dfg.insts[u] {
                    InstructionData::Load { arg, offset, .. } if r(arg) && i32::from(offset) == 0 => 1,
                    InstructionData::Store { args: [x, a], offset, .. }
                        if r(a) && !r(x) && i32::from(offset) == 0 =>
                    {
                        1
                    }
                    _ => 2,
                };
            }
            let lvl = self.loop_analysis.loop_level(b).level();
            // `PLIRON_AMODESPLIT_MAX` (default 16; was 4, which kept serde_json's
            // escape scan out: its escape path uses the address in 5+ blocks).
            static MAXB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
            let maxb = *MAXB.get_or_init(|| {
                std::env::var("PLIRON_AMODESPLIT_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(16)
            });
            if local != 1
                || others.is_empty()
                || others.len() > maxb
                || others.iter().any(|&ub| self.loop_analysis.loop_level(ub).level() > lvl)
            {
                continue;
            }
            cands.push((i, v, others));
        }
        for (i, v, others) in cands {
            // A use block under another one's copy reuses it: dominators
            // first, so a path through several use blocks adds once.
            let all: Vec<crate::ir::Block> = others.into_iter().collect();
            let mut others = all.clone();
            others.sort_by_key(|&x| core::cmp::Reverse(others_dominated(&self.domtree, x, &all)));
            let mut made: Vec<(crate::ir::Block, Value)> = vec![];
            for ub in others {
                let nv = match made.iter().find(|&&(c, _)| self.domtree.block_dominates(c, ub)) {
                    Some(&(_, nv)) => nv,
                    None => {
                        let ni = self.func.dfg.clone_inst(i);
                        let Some(first) = self.func.layout.first_inst(ub) else { continue };
                        self.func.layout.insert_inst(ni, first);
                        let nv = self.func.dfg.first_result(ni);
                        made.push((ub, nv));
                        nv
                    }
                };
                let insts: Vec<crate::ir::Inst> =
                    self.func.layout.block_insts(ub).filter(|&u| self.func.dfg.inst_results(u) != [nv]).collect();
                for u in insts {
                    let f = &mut self.func.dfg;
                    let raw: Vec<Value> = f.inst_values(u).filter(|&x| f.resolve_aliases(x) == v).collect();
                    if !raw.is_empty() {
                        f.map_inst_values(u, |x| if raw.contains(&x) { nv } else { x });
                    }
                }
            }
        }
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
    /// PLIRON: does block `b` hold a call that some value may live across
    /// (see `shrink_split`)? A call followed only by the block's
    /// terminator, a `return` or a `jump` to a return-only block whose
    /// arguments are all results of that call, or a `trap`, is in tail
    /// position.
    fn shrink_live_call(&self, b: crate::ir::Block) -> bool {
        use crate::ir::{BlockArg, Opcode};
        let f = &self.func;
        let ret_only = |blk: crate::ir::Block| {
            let mut it = f.layout.block_insts(blk);
            matches!(it.next(), Some(t) if f.dfg.insts[t].opcode() == Opcode::Return)
        };
        let insts: Vec<crate::ir::Inst> = f.layout.block_insts(b).collect();
        for (k, &i) in insts.iter().enumerate() {
            if !f.dfg.insts[i].opcode().is_call() {
                continue;
            }
            let res: Vec<crate::ir::Value> = f.dfg.inst_results(i).to_vec();
            let from_call = |v: crate::ir::Value| res.contains(&f.dfg.resolve_aliases(v));
            let tail = match &insts[k + 1..] {
                [t] => {
                    let op = f.dfg.insts[*t].opcode();
                    if op == Opcode::Return {
                        f.dfg.inst_args(*t).iter().all(|&v| from_call(v))
                    } else if op == Opcode::Trap {
                        // A call that does not return (a panic) keeps
                        // nothing live either.
                        true
                    } else if op == Opcode::Jump {
                        f.dfg.insts[*t]
                            .branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
                            .iter()
                            .all(|bc| {
                                ret_only(bc.block(&f.dfg.value_lists))
                                    && bc.args(&f.dfg.value_lists).all(|a| match a {
                                        BlockArg::Value(v) => from_call(v),
                                        _ => false,
                                    })
                            })
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if !tail {
                return true;
            }
        }
        false
    }

    fn shrink_split(&mut self) {
        use crate::ir::{BlockArg, InstructionData, Opcode, Value};
        use crate::cursor::{Cursor, FuncCursor};
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        crate::machinst::vcode::PLIRON_SPLIT_BLOCK.with(|c| c.set(None));
        if !*ON.get_or_init(|| std::env::var("PLIRON_SHRINKWRAP").as_deref() != Ok("0")) {
            return;
        }
        // `PLIRON_SHRINKWRAP_FNS=lo..hi`: only functions whose content
        // hash (FNV-1a of the signature and the opcode sequence; the name
        // is only a `u0:N` index here) is in [lo, hi) are shrink-wrapped,
        // for bisecting; `PLIRON_SHRINKWRAP_DEBUG` prints each hash.
        static FNS: std::sync::OnceLock<Option<(u64, u64)>> = std::sync::OnceLock::new();
        let fns = *FNS.get_or_init(|| {
            let v = std::env::var("PLIRON_SHRINKWRAP_FNS").ok()?;
            let (a, b) = v.split_once("..")?;
            Some((a.parse().ok()?, b.parse().ok()?))
        });
        let name = format!("{}", self.func.name);
        let mut h = format!("{}", self.func.signature)
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325u64, |h, c| (h ^ c as u64).wrapping_mul(0x100_0000_01b3));
        if fns.is_some() || std::env::var_os("PLIRON_SHRINKWRAP_DEBUG").is_some() {
            for b in self.func.layout.blocks() {
                for i in self.func.layout.block_insts(b) {
                    h = (h ^ self.func.dfg.insts[i].opcode() as u64).wrapping_mul(0x100_0000_01b3);
                }
            }
        }
        let off = fns.is_some_and(|(lo, hi)| h < lo || h >= hi);
        crate::machinst::vcode::PLIRON_SW_OFF.with(|c| c.set(off));
        if std::env::var_os("PLIRON_SHRINKWRAP_DEBUG").is_some() {
            eprintln!("shrink_split fn {name} hash {h}{}", if off { " (off)" } else { "" });
        }
        if off {
            return;
        }
        // `PLIRON_SHRINKWRAP_MAXLIVEIN=<n>` (default 3): no split when more
        // values than this are live into S (each is a copy on S's path,
        // which is often the hot one: `RawVecInner::grow_amortized` paid
        // four `mov`s per call to skip the saves on its zero-size exit).
        // Exact counts against 16: rust-protobuf -0.38%, regex +0.32%,
        // html-rewriter +0.09%; against no shrink-wrapping every held-out
        // program is equal or better at 3 (PARITY, native item).
        static MAXLI: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let max_livein = *MAXLI.get_or_init(|| {
            std::env::var("PLIRON_SHRINKWRAP_MAXLIVEIN").ok().and_then(|v| v.parse().ok()).unwrap_or(3)
        });
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
        // Without a call the region needs no callee-saved register for a
        // value live across one, so the split only buys moving the frame
        // allocation, and its edge copies cost more than that (serde_json's
        // `skip_to_escape`: five `mov`s per call to skip one `sub sp`;
        // rust-json +0.5% executed instructions). The save point itself is
        // still chosen after regalloc. `PLIRON_SHRINKWRAP_LEAF=1` splits
        // leaf functions too.
        //
        // Likewise when every call is in tail position (only a return, or
        // a jump to a block that only returns, follows it, passing nothing
        // but the call's results): nothing is live across such a call, so
        // no callee-saved register is needed either (`drop_glue` of a
        // `RawVec`: `if cap != 0 { dealloc(ptr, cap) }` got two copies and
        // kept its frame record at the entry).
        static LEAF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*LEAF.get_or_init(|| std::env::var("PLIRON_SHRINKWRAP_LEAF").as_deref() == Ok("1"))
            && !fblocks.iter().any(|&b| self.shrink_live_call(b))
        {
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
        // Only an early exit that is not cold pays for the split: when every
        // way out that avoids S is a cold block (an `Err` return, a panic),
        // S runs on every warm call and its edge copies are pure cost
        // (`RawVecInner::grow_amortized`: the capacity-overflow return;
        // rust-protobuf +0.3%). The save point is still chosen after
        // regalloc. `PLIRON_SHRINKWRAP_COLD=1` splits anyway.
        static COLD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*COLD.get_or_init(|| std::env::var("PLIRON_SHRINKWRAP_COLD").as_deref() == Ok("1")) {
            let f = &self.func;
            let is_ret = |b: crate::ir::Block| {
                f.layout.last_inst(b).is_some_and(|t| f.dfg.insts[t].opcode().is_return())
            };
            let warm_exit = f.layout.blocks().any(|b| {
                !self.domtree.block_dominates(sb, b)
                    && !f.layout.is_cold(b)
                    && (is_ret(b) || self.cfg.succ_iter(b).any(|s| s != sb && is_ret(s)))
            });
            if !warm_exit {
                if dbg {
                    eprintln!("shrink_split: no warm exit avoids {sb}");
                }
                return;
            }
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
        if liveins.is_empty() || liveins.len() > max_livein {
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
        let renames = core::mem::take(&mut pass.stats.renames);
        log::debug!("egraph stats: {:?}", pass.stats);
        if *STATS.get_or_init(|| std::env::var_os("PLIRON_STATS").is_some()) {
            let stats = pass.stats.clone();
            drop(pass);
            eprintln!("egraph stats {:?}: {:?}", self.func.name, stats);
        }
        self.egraph_renames = renames;
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
