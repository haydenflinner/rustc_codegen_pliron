//! Partial unrolling of latch-tested counted loops (`PLIRON_PUNROLL`).
//!
//! `bcheck` versioning (and plain `for i in 0..n` loops) leaves a fast path
//! that still pays one compare+branch per element; LLVM instead emits K body
//! copies per iteration and tests once. Loops here have the rotated shape
//!
//! ```text
//! h(p0..pn):          # header: entry edge(s) + latch backedge
//!     ...             # h may itself be the latch
//!     jump b1(...)
//! b1: ...
//!     brif (a cc bound) -> h(next..) | exit(..)
//! ```
//!
//! with `a` affine in a header param stepping by a positive constant
//! (`a = iv + d`, backedge `iv' = iv + s`, `s` const > 0) and `bound`
//! loop-invariant. The body must be a *linear chain* — every non-latch block
//! ends in an unconditional `jump` — so no mid-group exit can fire: when the
//! group runs, copies 0..K-2's continue tests are all implied by the guard.
//!
//! ```text
//! hu(p0..pn):
//!     ok1 = (a0 cc bound)                     # body1's precondition;
//!                                             # also forces rem >= 0
//!     ok2 = (bound - a0) ucmp (K-2)*s         # computed wide so it can't wrap
//!     brif ok1 & ok2 -> ub0(p0..pn) | h(p0..pn)
//! ub[k][i]:           # copy k of chain block i
//!     latch k<K-1:  jump ub[k+1][0](cont args)     # continue test elided
//!     latch k==K-1: brif c' -> hu(cont args) | exit(exit args)
//! ```
//!
//! The original loop is untouched: it handles the `< K` tail, plus the
//! a0-fails entry with the same one-iteration semantics the rotation had.
//! Copy order is preserved, so ordered accumulators (`fadd` chains, memory
//! order) are unaffected.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::instructions::InstructionMapper;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Constant, DynamicStackSlot, ExceptionTable, FuncRef, Function,
    GlobalValue, Immediate, Inst, InstBuilder, InstructionData, JumpTable, Opcode, SigRef,
    StackSlot, Value, ValueList, types,
};
use cranelift_codegen::loop_analysis::LoopAnalysis;
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

const MAX_LOOPS: usize = 8;
const MAX_CHAIN: usize = 4;
/// Non-terminator instructions along the whole chain, summed.
const MAX_BODY: usize = 24;
const UNROLL: i64 = 8;
const UNROLL_BIG: i64 = 4;

fn debug() -> bool {
    std::env::var_os("PLIRON_PUNROLL_DEBUG").is_some()
}

struct Vm<'a> {
    func: &'a mut Function,
    vmap: &'a FxHashMap<Value, Value>,
    bmap: &'a FxHashMap<Block, Block>,
}

impl Vm<'_> {
    fn mv(&self, v: Value) -> Value {
        let v = self.func.dfg.resolve_aliases(v);
        self.vmap.get(&v).copied().unwrap_or(v)
    }
    fn mb(&self, b: Block) -> Block {
        self.bmap.get(&b).copied().unwrap_or(b)
    }
}

impl InstructionMapper for Vm<'_> {
    fn map_value(&mut self, v: Value) -> Value {
        self.mv(v)
    }
    fn map_value_list(&mut self, value_list: ValueList) -> ValueList {
        let mut out = ValueList::new();
        let vals: Vec<Value> = value_list.as_slice(&self.func.dfg.value_lists).to_vec();
        for v in vals {
            out.push(self.mv(v), &mut self.func.dfg.value_lists);
        }
        out
    }
    fn map_global_value(&mut self, gv: GlobalValue) -> GlobalValue {
        gv
    }
    fn map_jump_table(&mut self, jt: JumpTable) -> JumpTable {
        jt
    }
    fn map_exception_table(&mut self, et: ExceptionTable) -> ExceptionTable {
        et
    }
    fn map_block_call(&mut self, bc: BlockCall) -> BlockCall {
        let blk = self.mb(bc.block(&self.func.dfg.value_lists));
        let args: Vec<BlockArg> = bc
            .args(&self.func.dfg.value_lists)
            .map(|a| match a {
                BlockArg::Value(v) => BlockArg::Value(self.mv(v)),
                a => a,
            })
            .collect();
        BlockCall::new(blk, args.iter().copied(), &mut self.func.dfg.value_lists)
    }
    fn map_block(&mut self, b: Block) -> Block {
        self.mb(b)
    }
    fn map_func_ref(&mut self, r: FuncRef) -> FuncRef {
        r
    }
    fn map_sig_ref(&mut self, r: SigRef) -> SigRef {
        r
    }
    fn map_stack_slot(&mut self, s: StackSlot) -> StackSlot {
        s
    }
    fn map_dynamic_stack_slot(&mut self, s: DynamicStackSlot) -> DynamicStackSlot {
        s
    }
    fn map_constant(&mut self, c: Constant) -> Constant {
        c
    }
    fn map_immediate(&mut self, i: Immediate) -> Immediate {
        i
    }
}

/// Reverse-postorder over the dominance cone rooted at `root`, restricted to
/// `cset` (blocks reachable from `root` through `cset` members only).
fn cone_rpo(func: &Function, root: Block, cset: &FxHashSet<Block>) -> Vec<Block> {
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    let mut order: Vec<Block> = Vec::new();
    let mut stack: Vec<(Block, bool)> = vec![(root, false)];
    while let Some((b, ex)) = stack.pop() {
        if ex {
            order.push(b);
            continue;
        }
        if !seen.insert(b) {
            continue;
        }
        stack.push((b, true));
        for i in func.layout.block_insts(b) {
            for bc in func.dfg.insts[i].branch_destination(
                &func.dfg.jump_tables,
                &func.dfg.exception_tables,
            ) {
                let t = bc.block(&func.dfg.value_lists);
                if cset.contains(&t) && !seen.contains(&t) {
                    stack.push((t, false));
                }
            }
        }
    }
    order.reverse();
    order
}

/// Clone `corder` blocks' instructions into their `cmap` images, threading
/// values through `xmap` (already seeded with param/lval bindings; clone
/// results are added as they are emitted).
fn emit_cone_clone(
    func: &mut Function,
    corder: &[Block],
    cmap: &FxHashMap<Block, Block>,
    xmap: &mut FxHashMap<Value, Value>,
) {
    for &b in corder {
        let nb = cmap[&b];
        for (j, &sp) in func.dfg.block_params(b).to_vec().iter().enumerate() {
            xmap.insert(sp, func.dfg.block_params(nb)[j]);
        }
        for ii in func.layout.block_insts(b).collect::<Vec<_>>() {
            let src = func.dfg.insts[ii];
            let data = {
                let mut m = Vm {
                    func,
                    vmap: xmap,
                    bmap: cmap,
                };
                src.map(&mut m)
            };
            let ni = func.dfg.make_inst(data);
            let ctv = func.dfg.ctrl_typevar(ii);
            func.dfg.make_inst_results(ni, ctv);
            func.layout.append_inst(ni, nb);
            for (&o, &nv) in func
                .dfg
                .inst_results(ii)
                .iter()
                .zip(func.dfg.inst_results(ni).iter())
            {
                xmap.insert(func.dfg.resolve_aliases(o), nv);
            }
        }
    }
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    let i = func.dfg.value_def(v).inst()?;
    if let InstructionData::UnaryImm {
        opcode: Opcode::Iconst,
        imm,
    } = func.dfg.insts[i]
    {
        Some(imm.bits())
    } else {
        None
    }
}

fn param_idx(func: &Function, h: Block, v: Value) -> Option<usize> {
    let v = func.dfg.resolve_aliases(v);
    if let cranelift_codegen::ir::ValueDef::Param(b, i) = func.dfg.value_def(v)
        && b == h
    {
        return Some(i);
    }
    None
}

/// `v` as `param[h][i] + d` (d = 0 for a bare param).
fn affine_param(func: &Function, h: Block, v: Value) -> Option<(usize, i64)> {
    let v = func.dfg.resolve_aliases(v);
    if let Some(i) = param_idx(func, h, v) {
        return Some((i, 0));
    }
    let i = func.dfg.value_def(v).inst()?;
    if let InstructionData::Binary {
        opcode: Opcode::Iadd,
        args,
    } = func.dfg.insts[i]
    {
        for (x, c) in [(args[0], args[1]), (args[1], args[0])] {
            if let Some(idx) = param_idx(func, h, x)
                && let Some(k) = iconst(func, c)
            {
                return Some((idx, k));
            }
        }
    }
    None
}

/// The counted continue-test: `a cc bound` where `a` is `p + d`, `p` a header
/// param stepping `+s` along the backedge, `bound` loop-invariant. `bound`
/// is either a plain value usable anywhere the header is reachable or a
/// header param index (`bpidx`) passed back unchanged on the backedge.
struct Cnt {
    pidx: usize,
    d: i64,
    step: i64,
    bound: Value,
    bpidx: Option<usize>,
    le: bool,
    signed: bool,
}

pub fn run(func: &mut Function, name: &str) -> usize {
    let mut tried: FxHashSet<Block> = FxHashSet::default();
    let mut n = 0;
    while n < MAX_LOOPS {
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        let mut la = LoopAnalysis::new();
        la.compute(func, &cfg, &dt);
        let mut hit = false;
        let loops: Vec<_> = la.loops().collect();
        for lp in loops {
            let h = la.loop_header(lp);
            if tried.contains(&h) {
                continue;
            }
            tried.insert(h);
            if run_loop(func, &cfg, &dt, &la, lp, h, name).is_some() {
                n += 1;
                hit = true;
                break; // cfg/la stale; rescan
            }
        }
        if !hit {
            break;
        }
    }
    n
}

fn run_loop(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: cranelift_codegen::loop_analysis::Loop,
    h: Block,
    name: &str,
) -> Option<()> {
    macro_rules! bail {
        ($why:expr) => {{
            if debug() {
                eprintln!("punroll {name} block{}: {}", h.as_u32(), $why);
            }
            return None;
        }};
    }
    if !dt.is_reachable(h) || func.layout.is_cold(h) {
        bail!("header unreachable/cold");
    }
    let body: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, lp))
        .collect();
    // Walk the linear chain h -> .. -> latch: `jump` steps land on a fresh
    // in-body block; a mid-chain `brif` is allowed when exactly one dest is
    // a fresh in-body block (the loop continues) and the other leaves the
    // body (a side exit, e.g. a bounds-check panic edge). The `brif` whose
    // dest is `h` is the latch. Anything else, any extra body block, or a
    // cold block disqualifies.
    struct SideExit {
        /// Index in `chain` of the block whose `brif` exits.
        pos: usize,
        /// In-body `brif` edge (to `chain[pos+1]`).
        cont_bc: BlockCall,
        /// Out-of-body `brif` edge (the side exit).
        exit_bc: BlockCall,
        /// `exit_bc` target.
        seblk: Block,
        /// Position (0/1) of the exit dest in the original `brif`.
        epos: usize,
        /// Loop values used inside `seblk`'s dominance cone, in the order
        /// they become clone-root params (empty ⇒ edge retargets the
        /// original block, no clone needed).
        lvals: Vec<Value>,
        /// Dominance cone blocks in emission (RPO) order, cone clones, and
        /// the clone root when `lvals` is non-empty.
        corder: Vec<Block>,
        cmap: FxHashMap<Block, Block>,
        clone_root: Option<Block>,
    }
    let mut chain = vec![h];
    let mut mid_exits: Vec<SideExit> = Vec::new();
    let latch;
    loop {
        let cur = *chain.last().unwrap();
        if func.layout.is_cold(cur) {
            bail!("cold block in chain");
        }
        let t = func.layout.last_inst(cur)?;
        match func.dfg.insts[t] {
            InstructionData::Jump { destination, .. } => {
                let nb = destination.block(&func.dfg.value_lists);
                if nb == h || !body.contains(&nb) || chain.contains(&nb) {
                    bail!("nonlinear chain");
                }
                chain.push(nb);
                if chain.len() > MAX_CHAIN {
                    bail!("chain too long");
                }
            }
            InstructionData::Brif { .. } => {
                let dests: Vec<BlockCall> = func.dfg.insts[t]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .to_vec();
                let d0 = dests[0].block(&func.dfg.value_lists);
                let d1 = dests[1].block(&func.dfg.value_lists);
                if d0 == h || d1 == h {
                    latch = cur;
                    break;
                }
                let (cont_bc, exit_bc, epos) = if body.contains(&d0) && !body.contains(&d1) {
                    (dests[0], dests[1], 1)
                } else if body.contains(&d1) && !body.contains(&d0) {
                    (dests[1], dests[0], 0)
                } else {
                    bail!("nonlinear chain");
                };
                let nb = cont_bc.block(&func.dfg.value_lists);
                if chain.contains(&nb) {
                    bail!("nonlinear chain");
                }
                mid_exits.push(SideExit {
                    pos: chain.len() - 1,
                    cont_bc,
                    exit_bc,
                    seblk: exit_bc.block(&func.dfg.value_lists),
                    epos,
                    lvals: Vec::new(),
                    corder: Vec::new(),
                    cmap: FxHashMap::default(),
                    clone_root: None,
                });
                chain.push(nb);
                if chain.len() > MAX_CHAIN {
                    bail!("chain too long");
                }
            }
            _ => bail!("unhandled terminator"),
        }
    }
    if body.len() != chain.len() {
        bail!("extra body blocks");
    }
    // Loops already doing vector work (loopvec output, SIMD intrinsics) are
    // vf*UNROLL-wide in the element domain; cloning the body again pays a
    // fresh group guard per K*vf*UNROLL elements for no benefit and pessi-
    // mizes addressing (post-index forms get cloned into reg+imm offsets).
    for &b in &chain {
        let vec_param = func
            .dfg
            .block_params(b)
            .iter()
            .any(|&v| func.dfg.value_type(v).is_vector());
        let vec_inst = func.layout.block_insts(b).any(|i| {
            func.dfg
                .inst_args(i)
                .iter()
                .chain(func.dfg.inst_results(i))
                .any(|&v| func.dfg.value_type(v).is_vector())
        });
        if vec_param || vec_inst {
            bail!("vector-typed loop");
        }
    }
    let n_insts: usize = chain
        .iter()
        .map(|&b| func.layout.block_insts(b).count() - 1)
        .sum();
    let k = if n_insts <= 12 {
        UNROLL
    } else if n_insts <= MAX_BODY {
        UNROLL_BIG
    } else {
        bail!("body too big");
    };
    // Mid-chain insts must be plain (no branches/calls-with-edges/etc.).
    for &b in &chain {
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for &i in &insts[..insts.len() - 1] {
            let op = func.dfg.insts[i].opcode();
            if op.is_terminator()
                || op.is_branch()
                || matches!(op, Opcode::TryCall | Opcode::TryCallIndirect)
            {
                bail!("unclonable inst");
            }
        }
    }

    let lt = func.layout.last_inst(latch).unwrap();
    let dests: Vec<BlockCall> = func.dfg.insts[lt]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        .to_vec();
    let Some(cpos) = dests
        .iter()
        .position(|bc| bc.block(&func.dfg.value_lists) == h)
    else {
        bail!("no backedge dest");
    };
    let cont_bc = dests[cpos];
    let exit_bc = dests[1 - cpos];
    if body.contains(&exit_bc.block(&func.dfg.value_lists)) {
        bail!("exit inside loop");
    }
    let InstructionData::Brif { arg: c, .. } = func.dfg.insts[lt] else {
        unreachable!()
    };
    let Some(ci) = func.dfg.value_def(func.dfg.resolve_aliases(c)).inst() else {
        bail!("cond not an icmp");
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond,
        args: [x, y],
    } = func.dfg.insts[ci]
    else {
        bail!("cond not an icmp");
    };
    let cond = if cpos == 0 { cond } else { cond.complement() };

    let hparams = func.dfg.block_params(h).to_vec();
    let cont_args: Vec<Value> = cont_bc
        .args(&func.dfg.value_lists)
        .filter_map(|a| match a {
            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
            _ => None,
        })
        .collect();
    if cont_args.len() != hparams.len() {
        bail!("backedge arity");
    }

    let mut cnt: Option<Cnt> = None;
    for (a, b, cc) in [(x, y, cond), (y, x, cond.swap_args())] {
        let (le, signed) = match cc {
            IntCC::UnsignedLessThan => (false, false),
            IntCC::UnsignedLessThanOrEqual => (true, false),
            IntCC::SignedLessThan => (false, true),
            IntCC::SignedLessThanOrEqual => (true, true),
            _ => continue,
        };
        let Some((pidx, d)) = affine_param(func, h, a) else {
            continue;
        };
        let pty = func.dfg.value_type(hparams[pidx]);
        if !pty.is_int() || pty.bits() < 16 || pty.bits() > 64 {
            continue;
        }
        // Step: p's backedge value must be `p + s`, s a positive const.
        let Some(bi) = func.dfg.value_def(cont_args[pidx]).inst() else {
            continue;
        };
        let mut step = None;
        if let InstructionData::Binary {
            opcode: Opcode::Iadd,
            args,
        } = func.dfg.insts[bi]
        {
            for (px, sc) in [(args[0], args[1]), (args[1], args[0])] {
                if param_idx(func, h, px) == Some(pidx)
                    && let Some(s) = iconst(func, sc)
                    && s > 0
                {
                    step = Some(s);
                }
            }
        }
        let Some(step) = step else { continue };
        // Bound must be loop-invariant: defined outside the body, or a header
        // param passed back unchanged (usable at hu as hu's own param).
        let b = func.dfg.resolve_aliases(b);
        let mut bpidx = None;
        if let Some(bi) = func.dfg.value_def(b).inst() {
            let Some(bb) = func.layout.inst_block(bi) else {
                continue;
            };
            if body.contains(&bb) {
                continue;
            }
            // Its def must dominate every entry edge into the loop.
            if !cfg
                .pred_iter(h)
                .filter(|p| !body.contains(&p.block))
                .all(|p| dt.dominates(bi, p.inst, &func.layout))
            {
                continue;
            }
        } else {
            match param_idx(func, h, b) {
                Some(q) if cont_args[q] == b => bpidx = Some(q),
                _ => continue,
            }
        }
        cnt = Some(Cnt {
            pidx,
            d,
            step,
            bound: b,
            bpidx,
            le,
            signed,
        });
        break;
    }
    let Some(cnt) = cnt else { bail!("no counting test") };

    let ity = func.dfg.value_type(hparams[cnt.pidx]);
    // `bound - a0` must not wrap: <=32-bit counters widen to i64 (signed or
    // unsigned per cc); an i64 counter is kept narrow — a real loop can't
    // span >2^63 iterations.
    let wty = if ity.bits() <= 32 { types::I64 } else { ity };

    // Loop values may be referenced directly by code past the exit edge —
    // SSA defs that dominated those blocks via the loop's single exit edge.
    // Any such use lies in eblk's dominance cone. If the cone uses loop
    // values, the unrolled loop's exit clones the whole cone so the uses
    // bind to the last copy's defs. All bail checks precede emission.
    let mut lvals: FxHashSet<Value> = FxHashSet::default();
    for &p in &hparams {
        lvals.insert(p);
    }
    for &b in &chain {
        lvals.extend(func.dfg.block_params(b).iter().copied());
        for i in func.layout.block_insts(b) {
            lvals.extend(func.dfg.inst_results(i).iter().copied());
        }
    }
    let eblk = exit_bc.block(&func.dfg.value_lists);
    let cone: Vec<Block> = func
        .layout
        .blocks()
        .filter(|&b| {
            !body.contains(&b) && dt.is_reachable(b) && dt.block_dominates(eblk, b)
        })
        .collect();
    let mut needs_cone = false;
    let mut cone_bad = false;
    let mut cone_insts = 0;
    for &cb in &cone {
        for i in func.layout.block_insts(cb) {
            cone_insts += 1;
            if func
                .dfg
                .inst_values(i)
                .any(|v| lvals.contains(&func.dfg.resolve_aliases(v)))
            {
                needs_cone = true;
            }
            let op = func.dfg.insts[i].opcode();
            if matches!(
                op,
                Opcode::TryCall | Opcode::TryCallIndirect | Opcode::BrTable
            ) {
                cone_bad = true;
            }
            for bc in func.dfg.insts[i].branch_destination(
                &func.dfg.jump_tables,
                &func.dfg.exception_tables,
            ) {
                if bc.args(&func.dfg.value_lists).any(|a| {
                    matches!(a, BlockArg::Value(v) if lvals.contains(&func.dfg.resolve_aliases(v)))
                }) {
                    needs_cone = true;
                }
                if body.contains(&bc.block(&func.dfg.value_lists)) {
                    bail!("exit cone re-enters loop");
                }
            }
        }
    }
    if needs_cone && (cone_bad || cone_insts > 128) {
        bail!("exit cone too big");
    }

    // Side exits: the continue edge binds the next chain block's params; the
    // exit edge either retargets the original target (when its dominance
    // cone never references a loop value — edge args are remapped freely) or
    // a per-exit cone clone whose root takes the used loop values as extra
    // params, letting every copy share one clone (LLVM's shape: one cold
    // panic block fed by all unrolled bounds checks). Bounds keep cold
    // panic cones small: <=8 exits, <=8 blocks / 48 insts per cone, <=96
    // cloned insts total.
    if mid_exits.len() > 8 {
        bail!("too many side exits");
    }
    let mut clone_insts = 0usize;
    for e in &mut mid_exits {
        for bc in [e.cont_bc, e.exit_bc] {
            if bc
                .args(&func.dfg.value_lists)
                .any(|a| !matches!(a, BlockArg::Value(_)))
            {
                bail!("side-exit edge has non-value arg");
            }
        }
        let scone: Vec<Block> = func
            .layout
            .blocks()
            .filter(|&b| {
                !body.contains(&b) && dt.is_reachable(b) && dt.block_dominates(e.seblk, b)
            })
            .collect();
        let mut lset: FxHashSet<Value> = FxHashSet::default();
        let mut elvals: Vec<Value> = Vec::new();
        let mut sconebad = false;
        let mut sinsts = 0usize;
        for &cb in &scone {
            for i in func.layout.block_insts(cb) {
                sinsts += 1;
                for v in func.dfg.inst_values(i) {
                    let v = func.dfg.resolve_aliases(v);
                    if lvals.contains(&v) && lset.insert(v) {
                        elvals.push(v);
                    }
                }
                let op = func.dfg.insts[i].opcode();
                if matches!(
                    op,
                    Opcode::TryCall | Opcode::TryCallIndirect | Opcode::BrTable
                ) {
                    sconebad = true;
                }
                for bc in func.dfg.insts[i].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    for a in bc.args(&func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a {
                            let v = func.dfg.resolve_aliases(v);
                            if lvals.contains(&v) && lset.insert(v) {
                                elvals.push(v);
                            }
                        }
                    }
                    if body.contains(&bc.block(&func.dfg.value_lists)) {
                        bail!("exit cone re-enters loop");
                    }
                }
            }
        }
        if elvals.is_empty() {
            continue;
        }
        if sconebad || scone.len() > 8 || sinsts > 48 {
            bail!("side-exit cone too big");
        }
        clone_insts += sinsts;
        if clone_insts > 96 {
            bail!("side-exit clone budget");
        }
        e.lvals = elvals;
        e.corder = scone;
    }

    // hu (unrolled header/guard) plus ONE fused block holding all K copies
    // of the chain, placed just before h so the scalar remainder keeps its
    // position. Chaining copies through jumps (one block per copy) lets the
    // compile-time egraph park every pure combine — e.g. an ordered `fadd`
    // reduction chain — in the last block, splitting each producing `load`
    // from its consumer across a block boundary and defeating the backend's
    // load-sinking (`addss (mem), %xmm` never forms). A single straight-line
    // block keeps each load adjacent to its user.
    let hu = func.dfg.make_block();
    for &p in &hparams {
        let ty = func.dfg.value_type(p);
        func.dfg.append_block_param(hu, ty);
    }
    func.layout.insert_block(hu, h);
    let huparams = func.dfg.block_params(hu).to_vec();
    // Segments: `usegs[kk][j]` is segment j of copy kk. A mid-chain side
    // exit ends a segment (the brif needs a real target); otherwise copies
    // stay in ONE block so load/consumer adjacency survives the egraph
    // (see above). With no side exits, `usegs` is a single fused block —
    // the pre-side-exit shape.
    let nseg = mid_exits.len() + 1;
    let mut usegs: Vec<Vec<Block>> = Vec::with_capacity(k as usize);
    for kk in 0..k as usize {
        if mid_exits.is_empty() && kk > 0 {
            usegs.push(Vec::new());
            continue;
        }
        let mut row = Vec::with_capacity(nseg);
        for _ in 0..nseg {
            let nb = func.dfg.make_block();
            func.layout.insert_block(nb, h);
            row.push(nb);
        }
        usegs.push(row);
    }
    let uf = usegs[0][0];
    for &p in func.dfg.block_params(chain[0]).to_vec().iter() {
        let ty = func.dfg.value_type(p);
        func.dfg.append_block_param(uf, ty);
    }
    let ufparams = func.dfg.block_params(uf).to_vec();

    // Guard: iteration budget `bound - a0` must cover copies 0..K-2's entry
    // tests (i.e. a_{K-2} cc bound), evaluated wide so it can't wrap.
    {
        let mut pos = FuncCursor::new(func).at_bottom(hu);
        let boundv = match cnt.bpidx {
            Some(q) => huparams[q],
            None => cnt.bound,
        };
        let a0 = if cnt.d == 0 {
            huparams[cnt.pidx]
        } else {
            let dc = pos.ins().iconst(ity, cnt.d);
            pos.ins().iadd(huparams[cnt.pidx], dc)
        };
        let cc = match (cnt.signed, cnt.le) {
            (true, true) => IntCC::SignedLessThanOrEqual,
            (true, false) => IntCC::SignedLessThan,
            (false, true) => IntCC::UnsignedLessThanOrEqual,
            (false, false) => IntCC::UnsignedLessThan,
        };
        let ok1 = pos.ins().icmp(cc, a0, boundv);
        let (a0w, bw) = if wty != ity && cnt.signed {
            (
                pos.ins().sextend(wty, a0),
                pos.ins().sextend(wty, boundv),
            )
        } else if wty != ity {
            (
                pos.ins().uextend(wty, a0),
                pos.ins().uextend(wty, boundv),
            )
        } else {
            (a0, boundv)
        };
        let rem = pos.ins().isub(bw, a0w);
        let lim = pos.ins().iconst(wty, (k - 2) * cnt.step);
        let ok2 = pos.ins().icmp(
            if cnt.le {
                IntCC::UnsignedGreaterThanOrEqual
            } else {
                IntCC::UnsignedGreaterThan
            },
            rem,
            lim,
        );
        let ok = pos.ins().band(ok1, ok2);
        let args: Vec<BlockArg> = huparams.iter().map(|&v| v.into()).collect();
        pos.ins().brif(ok, uf, &args, h, &args);
    }

    // When the exit cone references loop values, the unrolled loop's exit
    // clones the whole cone. Create the clone blocks now (the last copy's
    // terminator targets cmap[eblk]); their insts are emitted once the last
    // copy's value map exists. `order` is RPO over the cone from eblk.
    let mut cmap: FxHashMap<Block, Block> = FxHashMap::default();
    let mut corder: Vec<Block> = Vec::new();
    if needs_cone {
        let cset: FxHashSet<Block> = cone.iter().copied().collect();
        corder = cone_rpo(func, eblk, &cset);
        for &b in &corder {
            let nb = func.dfg.make_block();
            for &p in func.dfg.block_params(b).to_vec().iter() {
                let ty = func.dfg.value_type(p);
                func.dfg.append_block_param(nb, ty);
            }
            func.layout.insert_block(nb, h);
            cmap.insert(b, nb);
        }
    }
    let eu = cmap.get(&eblk).copied().unwrap_or(eblk);

    // Side-exit cone clones: one clone per exit (deduped by target), the
    // clone root taking the cone's loop values as appended params so each
    // copy's `brif` carries that copy's bindings on the edge. Contents are
    // emitted immediately — the clone's value map is self-contained (loop
    // values arrive only through the root params).
    let mut seen_se: FxHashMap<Block, usize> = FxHashMap::default();
    for ei in 0..mid_exits.len() {
        if mid_exits[ei].lvals.is_empty() {
            continue;
        }
        if let Some(&first) = seen_se.get(&mid_exits[ei].seblk) {
            let (lvals, corder, cmap, root) = {
                let f = &mid_exits[first];
                (f.lvals.clone(), f.corder.clone(), f.cmap.clone(), f.clone_root)
            };
            let e = &mut mid_exits[ei];
            e.lvals = lvals;
            e.corder = corder;
            e.cmap = cmap;
            e.clone_root = root;
            continue;
        }
        seen_se.insert(mid_exits[ei].seblk, ei);
        let e = &mut mid_exits[ei];
        let cset: FxHashSet<Block> = e.corder.iter().copied().collect();
        e.corder = cone_rpo(func, e.seblk, &cset);
        let cold = func.layout.is_cold(e.seblk);
        for (bi, &b) in e.corder.clone().iter().enumerate() {
            let nb = func.dfg.make_block();
            for &p in func.dfg.block_params(b).to_vec().iter() {
                let ty = func.dfg.value_type(p);
                func.dfg.append_block_param(nb, ty);
            }
            if bi == 0 {
                // RPO root == seblk: appended params carry the loop values
                // this copy's exit edge binds.
                for &lv in &e.lvals {
                    let ty = func.dfg.value_type(lv);
                    func.dfg.append_block_param(nb, ty);
                }
            }
            func.layout.insert_block(nb, h);
            if cold && func.layout.is_cold(b) {
                func.layout.set_cold(nb);
            }
            e.cmap.insert(b, nb);
        }
        e.clone_root = Some(e.cmap[&e.seblk]);
        let root = e.clone_root.unwrap();
        let orig_arity = func.dfg.block_params(e.seblk).len();
        let mut xmap: FxHashMap<Value, Value> = FxHashMap::default();
        for (i, &lv) in e.lvals.iter().enumerate() {
            xmap.insert(lv, func.dfg.block_params(root)[orig_arity + i]);
        }
        emit_cone_clone(func, &e.corder, &e.cmap, &mut xmap);
    }

    // Clone K copies of the chain into the single fused block. Block params
    // bind through `carry`: copy kk>0's header params come from copy kk-1's
    // latch `cont` args, mid-chain params from the preceding block's jump
    // args — all keyed by the source param value, no blocks needed.
    let nobmap: FxHashMap<Block, Block> = FxHashMap::default();
    let mpos: FxHashMap<usize, usize> = mid_exits
        .iter()
        .enumerate()
        .map(|(ei, e)| (e.pos, ei))
        .collect();
    let mut vmap: FxHashMap<Value, Value> = FxHashMap::default();
    let mut carry: FxHashMap<Value, Value> = FxHashMap::default();
    let mut curseg = uf;
    for kk in 0..k as usize {
        let mut seg_i = 0usize;
        for (i, &b) in chain.iter().enumerate() {
            for (j, &sp) in func.dfg.block_params(b).to_vec().iter().enumerate() {
                let nv = if kk == 0 && i == 0 {
                    ufparams[j]
                } else {
                    carry[&sp]
                };
                vmap.insert(sp, nv);
            }
            let insts: Vec<Inst> = func.layout.block_insts(b).collect();
            for &ii in &insts[..insts.len() - 1] {
                let src = func.dfg.insts[ii];
                let data = {
                    let mut m = Vm {
                        func,
                        vmap: &vmap,
                        bmap: &nobmap,
                    };
                    src.map(&mut m)
                };
                let ni = func.dfg.make_inst(data);
                let ctv = func.dfg.ctrl_typevar(ii);
                func.dfg.make_inst_results(ni, ctv);
                func.layout.append_inst(ni, curseg);
                for (&o, &nv) in func
                    .dfg
                    .inst_results(ii)
                    .iter()
                    .zip(func.dfg.inst_results(ni).iter())
                {
                    vmap.insert(func.dfg.resolve_aliases(o), nv);
                }
            }
            let t = *insts.last().unwrap();
            let tdata = func.dfg.insts[t];
            let mv = |func: &Function, a: BlockArg| -> BlockArg {
                match a {
                    BlockArg::Value(v) => {
                        let v = func.dfg.resolve_aliases(v);
                        BlockArg::Value(*vmap.get(&v).unwrap_or(&v))
                    }
                    a => a,
                }
            };
            let mvv = |func: &Function, a: BlockArg| -> Value {
                match mv(func, a) {
                    BlockArg::Value(v) => v,
                    _ => unreachable!("punroll: non-value branch arg"),
                }
            };
            match tdata {
                InstructionData::Jump { .. } => {
                    // Fallthrough: bind the next chain block's params from
                    // this jump's args.
                    let bc = tdata
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[0];
                    let sps = func.dfg.block_params(chain[i + 1]).to_vec();
                    for (j, a) in bc.args(&func.dfg.value_lists).enumerate() {
                        carry.insert(sps[j], mvv(func, a));
                    }
                }
                InstructionData::Brif { arg: c, .. } if mpos.contains_key(&i) => {
                    // Mid-chain side exit: the test is data-dependent, so
                    // every copy keeps it (the guard only covers the latch's
                    // counting test). The continue edge falls into the copy's
                    // next segment; the exit edge targets the shared cone
                    // clone (loop values ride on its appended params) or the
                    // original block when the cone needs no loop values.
                    let e = &mid_exits[mpos[&i]];
                    let c = func.dfg.resolve_aliases(c);
                    let carg = *vmap.get(&c).unwrap_or(&c);
                    let sps = func.dfg.block_params(chain[i + 1]).to_vec();
                    for (j, a) in e.cont_bc.args(&func.dfg.value_lists).enumerate() {
                        carry.insert(sps[j], mvv(func, a));
                    }
                    let mut eargs: Vec<BlockArg> = e
                        .exit_bc
                        .args(&func.dfg.value_lists)
                        .map(|a| mv(func, a))
                        .collect();
                    if e.clone_root.is_some() {
                        for &lv in &e.lvals {
                            let lv = func.dfg.resolve_aliases(lv);
                            eargs.push(BlockArg::Value(*vmap.get(&lv).unwrap_or(&lv)));
                        }
                    }
                    let nxt = usegs[kk][seg_i + 1];
                    let etgt = e.clone_root.unwrap_or(e.seblk);
                    let mut pos = FuncCursor::new(func).at_bottom(curseg);
                    if e.epos == 0 {
                        pos.ins().brif(carg, etgt, &eargs, nxt, &[]);
                    } else {
                        pos.ins().brif(carg, nxt, &[], etgt, &eargs);
                    }
                    curseg = nxt;
                    seg_i += 1;
                }
                InstructionData::Brif { arg: c, .. } => {
                    let c = func.dfg.resolve_aliases(c);
                    let carg = *vmap.get(&c).unwrap_or(&c);
                    if kk + 1 < k as usize {
                        // Iteration kk+1's entry test is implied by the
                        // guard; thread `cont` args into the next copy's
                        // header params and continue. With side exits the
                        // next copy is a new segment and needs a jump; in
                        // the fused-block shape the fallthrough suffices.
                        let sps = func.dfg.block_params(chain[0]).to_vec();
                        for (j, a) in cont_bc.args(&func.dfg.value_lists).enumerate() {
                            carry.insert(sps[j], mvv(func, a));
                        }
                        if !mid_exits.is_empty() {
                            let mut pos = FuncCursor::new(func).at_bottom(curseg);
                            pos.ins().jump(usegs[kk + 1][0], &[]);
                            curseg = usegs[kk + 1][0];
                        }
                    } else {
                        let cont: Vec<BlockArg> = cont_bc
                            .args(&func.dfg.value_lists)
                            .map(|a| mv(func, a))
                            .collect();
                        let exit: Vec<BlockArg> = exit_bc
                            .args(&func.dfg.value_lists)
                            .map(|a| mv(func, a))
                            .collect();
                        let mut pos = FuncCursor::new(func).at_bottom(curseg);
                        if cpos == 0 {
                            pos.ins().brif(carg, hu, &cont, eu, &exit);
                        } else {
                            pos.ins().brif(carg, eu, &exit, hu, &cont);
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
    }
    let mut lastmap: FxHashMap<Value, Value> = vmap;

    // Emit the cloned exit cone, if needed: verbatim copies seen through the
    // last unrolled copy's value map (loop-external values pass through
    // unchanged; cone-internal edges retarget via cmap).
    if needs_cone {
        emit_cone_clone(func, &corder, &cmap, &mut lastmap);
    }

    // Redirect every outside-the-loop entry edge from h to hu.
    let entries: Vec<(Block, Inst)> = cfg
        .pred_iter(h)
        .filter(|p| !body.contains(&p.block))
        .map(|p| (p.block, p.inst))
        .collect();
    for (_pb, pi) in entries {
        let dfg = &mut func.dfg;
        let ndest = dfg.insts[pi]
            .branch_destination(&dfg.jump_tables, &dfg.exception_tables)
            .len();
        for slot in 0..ndest {
            let dfg = &mut func.dfg;
            let bc = &mut dfg.insts[pi].branch_destination_mut(
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
            )[slot];
            if bc.block(&dfg.value_lists) == h {
                let args: Vec<BlockArg> = bc.args(&dfg.value_lists).collect();
                *bc = BlockCall::new(hu, args.iter().copied(), &mut dfg.value_lists);
            }
        }
    }
    Some(())
}
