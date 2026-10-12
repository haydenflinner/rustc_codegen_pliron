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
//! Rotated while-loops (`bcheck` fast clones: `h: brif (a cc bound) ->
//! body | exit; body..: jump h`) unroll the same way — h's own brif is
//! the counting test, so the guard covers copies 0..K-2's entry tests
//! verbatim (entry test of copy kk is `a0 + kk*s`, the same budget the
//! do-while guard proves); the last copy keeps the test (`brif c' ->
//! copy-body | exit`) and the chain's `jump h` becomes `jump hu`.
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
    StackSlot, Type, Value, ValueList, types,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_LOOPS: usize = 8;
const MAX_CHAIN: usize = 4;
/// Non-terminator instructions along the whole chain, summed.
const MAX_BODY: usize = 24;
const UNROLL: i64 = 8;
const UNROLL_BIG: i64 = 4;
/// Unroll-and-jam width: J copies of the outer body fused into one inner
/// loop (J independent reduction chains).
const JAM: usize = 4;
/// Inner-chain non-terminator insts eligible for jamming.
const MAX_JAM_BODY: usize = 16;

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
    /// Exit-on-`a == bound` (a `!=` continue test): the guard's strict
    /// `a_{K-2} < bound` check is the same — a monotone counter that
    /// hasn't reached `bound` can't equal it.
    eq: bool,
}

pub fn run(func: &mut Function, name: &str) -> usize {
    let mut tried: FxHashSet<Block> = FxHashSet::default();
    let mut normed: FxHashSet<Block> = FxHashSet::default();
    let mut n = 0;
    while n < MAX_LOOPS {
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        let mut la = LoopAnalysis::new();
        la.compute(func, &cfg, &dt);
        let mut hit = false;
        let loops: Vec<_> = la.loops().collect();
        // Canonicalize mid-chain counting exits into the latch: bcheck/
        // sameargs leave `brif const -> h` dead latches behind the real
        // trip test sitting mid-chain. Moving the test to the latch
        // gives jam and unroll the canonical do-while shape — jam's
        // "inner latch exits elsewhere" check needs exactly this.
        for lp in loops.iter().copied() {
            let h = la.loop_header(lp);
            if normed.contains(&h) {
                continue;
            }
            normed.insert(h);
            if sink_test(func, &cfg, &dt, &la, lp, h, name) {
                hit = true;
                break; // cfg/la stale; rescan
            }
        }
        // Unroll-and-jam runs first: an outer loop wrapping a single
        // linear inner chain gets J fused copies before punroll can
        // disturb the inner loop's shape. Jammed loop headers join
        // `tried` so a rescan doesn't re-jam the same outer loop.
        if crate::pass_enabled("PLIRON_JAM") {
            for lp in loops.iter().copied() {
                let h = la.loop_header(lp);
                if tried.contains(&h) {
                    continue;
                }
                let nch = la
                    .loops()
                    .filter(|&c| la.loop_parent(c) == Some(lp))
                    .count();
                if debug() {
                    eprintln!("jam {name} block{}: children={}", h.as_u32(), nch);
                }
                if nch == 0 || nch > 2 {
                    continue;
                }
                tried.insert(h);
                let r = if nch == 2 {
                    run_jam_chooser(func, &cfg, &dt, &la, lp, h, name)
                } else {
                    run_jam(func, &cfg, &dt, &la, lp, h, name)
                };
                if let Some(newhs) = r {
                    tried.extend(newhs);
                    n += 1;
                    hit = true;
                    break; // cfg/la stale; rescan
                }
            }
        }
        for lp in loops {
            if hit {
                break;
            }
            let h = la.loop_header(lp);
            if tried.contains(&h) {
                continue;
            }
            tried.insert(h);
            if debug() {
                eprintln!("punroll {name} try block{}", h.as_u32());
            }
            if crate::pass_enabled("PLIRON_PEEL")
                && run_peel(func, &cfg, &dt, &la, lp, h, name).is_some()
            {
                n += 1;
                hit = true;
                break; // cfg/la stale; rescan
            }
            if run_loop(func, &cfg, &dt, &la, lp, h, name).is_some()
                || run_region(func, &cfg, &dt, &la, lp, h, name).is_some()
            {
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

macro_rules! wbail {
    ($name:expr, $h:expr, $why:expr) => {{
        if debug() {
            eprintln!("punroll {} block{}: {}", $name, $h.as_u32(), $why);
        }
        return None;
    }};
}

/// An out-of-body edge target whose dominance cone is cloned once
/// (shared by every copy's exit edge) when the cone uses loop values —
/// the clone root's appended params carry each edge's own bindings.
struct ExitCone {
    /// Exit-edge target.
    seblk: Block,
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

/// A mid-chain `brif` in the linear chain: one dest continues the chain,
/// the other leaves the body (a side exit — bounds-check panic edge, or
/// the loop's own exit when the counting test sits mid-chain).
struct SideExit {
    /// Index in `chain` of the block whose `brif` exits.
    pos: usize,
    /// In-body `brif` edge (to `chain[pos+1]`).
    cont_bc: BlockCall,
    /// Out-of-body `brif` edge (the side exit).
    exit_bc: BlockCall,
    /// Position (0/1) of the exit dest in the original `brif`.
    epos: usize,
    cone: ExitCone,
}

struct Walk {
    body: FxHashSet<Block>,
    chain: Vec<Block>,
    mid_exits: Vec<SideExit>,
    latch: Block,
    /// `jump`-latch rotated loop: the latch is a plain `jump h` and h's
    /// own `brif` (a pos-0 side exit) is the counting test. `false` for
    /// brif-latch do-while loops.
    rotated: bool,
}

/// Walk the linear chain `h -> .. -> latch`: `jump` steps land on a fresh
/// in-body block; a mid-chain `brif` is allowed when exactly one dest is
/// a fresh in-body block (the loop continues) and the other leaves the
/// body. The `brif` whose dest is `h` is the latch (do-while); a `jump h`
/// latch marks a rotated while-loop. Anything else, any extra body
/// block, or a cold block disqualifies.
fn walk(
    func: &Function,
    _cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: cranelift_codegen::loop_analysis::Loop,
    h: Block,
    name: &str,
) -> Option<Walk> {
    if !dt.is_reachable(h) || func.layout.is_cold(h) {
        wbail!(name, h, "header unreachable/cold");
    }
    let body: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, lp))
        .collect();
    let mut chain = vec![h];
    let mut mid_exits: Vec<SideExit> = Vec::new();
    let latch;
    let mut rotated = false;
    loop {
        let cur = *chain.last().unwrap();
        if func.layout.is_cold(cur) {
            wbail!(name, h, "cold block in chain");
        }
        let t = func.layout.last_inst(cur)?;
        match func.dfg.insts[t] {
            InstructionData::Jump { destination, .. } => {
                let nb = destination.block(&func.dfg.value_lists);
                if nb == h {
                    latch = cur;
                    rotated = true;
                    break;
                }
                if !body.contains(&nb) || chain.contains(&nb) {
                    wbail!(name, h, "nonlinear chain");
                }
                chain.push(nb);
                if chain.len() > MAX_CHAIN {
                    wbail!(name, h, "chain too long");
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
                    wbail!(name, h, "nonlinear chain");
                };
                let nb = cont_bc.block(&func.dfg.value_lists);
                if chain.contains(&nb) {
                    wbail!(name, h, "nonlinear chain");
                }
                mid_exits.push(SideExit {
                    pos: chain.len() - 1,
                    cont_bc,
                    exit_bc,
                    epos,
                    cone: ExitCone {
                        seblk: exit_bc.block(&func.dfg.value_lists),
                        lvals: Vec::new(),
                        corder: Vec::new(),
                        cmap: FxHashMap::default(),
                        clone_root: None,
                    },
                });
                chain.push(nb);
                if chain.len() > MAX_CHAIN {
                    wbail!(name, h, "chain too long");
                }
            }
            _ => wbail!(name, h, "unhandled terminator"),
        }
    }
    if body.len() != chain.len() {
        wbail!(name, h, "extra body blocks");
    }
    Some(Walk {
        body,
        chain,
        mid_exits,
        latch,
        rotated,
    })
}

/// The counted continue-test search: `a cc bound` where `a` is
/// `param + d`, `param` stepping `+s` along the backedge (`cont_args`),
/// `bound` loop-invariant. Shared by the latch test (do-while), h's top
/// brif (rotated), and mid-chain exits being sunk to the latch.
fn find_count(
    func: &Function,
    h: Block,
    body: &FxHashSet<Block>,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    x: Value,
    y: Value,
    cond: IntCC,
    hparams: &[Value],
    cont_args: &[Value],
) -> Option<Cnt> {
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
            match func.dfg.value_def(b) {
                // Bound is a header param passed back unchanged on the
                // backedge (usable at hu as hu's own param).
                cranelift_codegen::ir::ValueDef::Param(bb, q)
                    if bb == h && cont_args[q] == b =>
                {
                    bpidx = Some(q);
                }
                // A param of any other block dominating h — e.g. a slice
                // `len` arriving as an entry-block param — is likewise
                // loop-invariant and usable at hu. (An in-body block can
                // never dominate the header, so loop-carried values are
                // still rejected.)
                cranelift_codegen::ir::ValueDef::Param(bb, _)
                    if bb != h && dt.dominates(bb, h, &func.layout) => {}
                _ => continue,
            }
        }
        return Some(Cnt {
            pidx,
            d,
            step,
            bound: b,
            bpidx,
            le,
            signed,
            eq: false,
        });
    }
    None
}

/// Canonicalize a mid-chain counting exit into the latch — `bcheck`/
/// `sameargs` leave a `brif const -> h` dead latch behind the real trip
/// test sitting mid-chain (`h: check; body; brif iv<n -> next | exit;
/// next: iv'; brif 1 -> h | cold`). Moving the test to the latch gives
/// jam and unroll the canonical do-while shape:
///
/// ```text
/// chain[p]:   jump chain[p+1](cont args)          (exit edge drops)
/// latch:      brif test -> h(back args) | exit(exit args)
/// ```
///
/// Only applies when the latch's taken edge is provably `h` (const cond)
/// — its other edge is dead and safe to drop — and the trailing blocks
/// are speculatable (they now run on the iteration the test fails).
fn sink_test(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: cranelift_codegen::loop_analysis::Loop,
    h: Block,
    name: &str,
) -> bool {
    let Some(w) = walk(func, cfg, dt, la, lp, h, name) else {
        return false;
    };
    if w.rotated {
        return false;
    }
    let lt = func.layout.last_inst(w.latch).unwrap();
    let InstructionData::Brif { arg: c_l, .. } = func.dfg.insts[lt] else {
        return false;
    };
    let Some(k) = iconst(func, c_l) else {
        return false;
    };
    let ldests: Vec<BlockCall> = func.dfg.insts[lt]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        .to_vec();
    // `brif k` takes dests[0] when k != 0.
    let taken = if k != 0 { 0 } else { 1 };
    if ldests[taken].block(&func.dfg.value_lists) != h {
        return false;
    }
    let back_bc = ldests[taken];
    let hparams = func.dfg.block_params(h).to_vec();
    let cont_args: Vec<Value> = back_bc
        .args(&func.dfg.value_lists)
        .filter_map(|a| match a {
            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
            _ => None,
        })
        .collect();
    if cont_args.len() != hparams.len() {
        return false;
    }
    for e in &w.mid_exits {
        let te = func.layout.last_inst(w.chain[e.pos]).unwrap();
        let InstructionData::Brif { arg: c, .. } = func.dfg.insts[te] else {
            continue;
        };
        let Some(ci) = func.dfg.value_def(func.dfg.resolve_aliases(c)).inst() else {
            continue;
        };
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args: [x, y],
        } = func.dfg.insts[ci]
        else {
            continue;
        };
        // Normalize cond to "true -> continue" (cont sits at 1 - epos).
        let cond = if e.epos == 1 { cond } else { cond.complement() };
        if find_count(func, h, &w.body, cfg, dt, x, y, cond, &hparams, &cont_args).is_none() {
            continue;
        }
        // The trailing chain must be speculatable — it now executes on
        // the iteration the test fails.
        let ok = w.chain[e.pos + 1..].iter().all(|&b| {
            func.layout.block_insts(b).all(|i| {
                let op = func.dfg.insts[i].opcode();
                op.is_terminator()
                    || !(op.can_load()
                        || op.can_store()
                        || op.can_trap()
                        || op.other_side_effects())
            })
        });
        if !ok {
            return false;
        }
        // Splice: chain[e.pos]'s brif becomes a jump over its cont edge;
        // the latch re-tests `c` — continue to h on the same polarity the
        // mid-exit used, exit on the moved exit edge.
        let cargs: Vec<BlockArg> = e.cont_bc.args(&func.dfg.value_lists).collect();
        let cblk = e.cont_bc.block(&func.dfg.value_lists);
        func.replace(te).jump(cblk, &cargs);
        let eargs: Vec<BlockArg> = e.exit_bc.args(&func.dfg.value_lists).collect();
        let eblk = e.exit_bc.block(&func.dfg.value_lists);
        let bargs: Vec<BlockArg> = back_bc.args(&func.dfg.value_lists).collect();
        if e.epos == 0 {
            func.replace(lt).brif(c, eblk, &eargs, h, &bargs);
        } else {
            func.replace(lt).brif(c, h, &bargs, eblk, &eargs);
        }
        if debug() {
            eprintln!("punroll {name} block{}: sunk mid-chain test", h.as_u32());
        }
        return true;
    }
    false
}

/// Analyze `seblk`'s dominance cone for the shared exit-clone
/// treatment: collect the loop values it references and the cone's
/// blocks. `Err(why)` when the cone is unclonable or over budget;
/// `Ok` with empty `lvals` when the cone is clean (edges retarget the
/// original block — no clone needed).
fn analyze_exit_cone(
    func: &Function,
    body: &FxHashSet<Block>,
    dt: &DominatorTree,
    lvals: &FxHashSet<Value>,
    seblk: Block,
    clone_insts: &mut usize,
) -> Result<ExitCone, &'static str> {
    let mut cone = ExitCone {
        seblk,
        lvals: Vec::new(),
        corder: Vec::new(),
        cmap: FxHashMap::default(),
        clone_root: None,
    };
    let scone: Vec<Block> = func
        .layout
        .blocks()
        .filter(|&b| !body.contains(&b) && dt.is_reachable(b) && dt.block_dominates(seblk, b))
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
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                for a in bc.args(&func.dfg.value_lists) {
                    if let BlockArg::Value(v) = a {
                        let v = func.dfg.resolve_aliases(v);
                        if lvals.contains(&v) && lset.insert(v) {
                            elvals.push(v);
                        }
                    }
                }
                if body.contains(&bc.block(&func.dfg.value_lists)) {
                    return Err("exit cone re-enters loop");
                }
            }
        }
    }
    if elvals.is_empty() {
        return Ok(cone);
    }
    if sconebad || scone.len() > 8 || sinsts > 48 {
        return Err("side-exit cone too big");
    }
    *clone_insts += sinsts;
    if *clone_insts > 96 {
        return Err("side-exit clone budget");
    }
    cone.lvals = elvals;
    cone.corder = scone;
    Ok(cone)
}

/// Emit the shared cone clone for every exit that needs one, deduped by
/// `seblk` (equal cones share a clone). The clone root takes the cone's
/// loop values as appended params so each copy's exit edge carries its
/// own bindings. Contents are emitted immediately — the clone's value
/// map is self-contained (loop values arrive only through the root
/// params).
fn emit_exit_cones(func: &mut Function, h: Block, exits: &mut [&mut ExitCone]) {
    let mut seen_se: FxHashMap<Block, usize> = FxHashMap::default();
    for ei in 0..exits.len() {
        if exits[ei].lvals.is_empty() {
            continue;
        }
        if let Some(&first) = seen_se.get(&exits[ei].seblk) {
            let (lvals, corder, cmap, root) = {
                let f = &exits[first];
                (
                    f.lvals.clone(),
                    f.corder.clone(),
                    f.cmap.clone(),
                    f.clone_root,
                )
            };
            let e = &mut exits[ei];
            e.lvals = lvals;
            e.corder = corder;
            e.cmap = cmap;
            e.clone_root = root;
            continue;
        }
        seen_se.insert(exits[ei].seblk, ei);
        let e = &mut exits[ei];
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
                // RPO root == seblk: appended params carry the loop
                // values this copy's exit edge binds.
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
}

/// Outside uses of body values must stay inside each exit's own cone.
/// An unrolled copy's exit clone binds the loop values used inside it,
/// but its forward edges land on original blocks: from there the peeled
/// path travels un-cloned code. Any ambient use of a body value
/// reachable from an exit target without re-entering the body must
/// therefore be dominated by that target — otherwise a peeled copy can
/// reach the use without the defining inst ever running (most often an
/// alias or live-out value pointing back into the body).
fn exit_continuations_ok(
    func: &Function,
    body: &FxHashSet<Block>,
    dt: &DominatorTree,
    lvals: &FxHashSet<Value>,
    targets: &[Block],
) -> bool {
    let ambient = |func: &Function, b: Block| -> bool {
        for i in func.layout.block_insts(b) {
            if func
                .dfg
                .inst_values(i)
                .any(|v| lvals.contains(&func.dfg.resolve_aliases(v)))
            {
                return true;
            }
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                if bc.args(&func.dfg.value_lists).any(|a| {
                    matches!(a, BlockArg::Value(v) if lvals.contains(&func.dfg.resolve_aliases(v)))
                }) {
                    return true;
                }
            }
        }
        false
    };
    for &tgt in targets {
        let mut seen: FxHashSet<Block> = FxHashSet::default();
        let mut stack = vec![tgt];
        while let Some(b) = stack.pop() {
            if body.contains(&b) || !seen.insert(b) || !dt.is_reachable(b) {
                continue;
            }
            if seen.len() > 512 {
                return false; // exit continuation too big
            }
            if !dt.block_dominates(tgt, b) && ambient(func, b) {
                return false; // body value used outside exit cones
            }
            for i in func.layout.block_insts(b) {
                for bc in func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    stack.push(bc.block(&func.dfg.value_lists));
                }
            }
        }
    }
    true
}

/// Emit the unrolled-loop guard at `hu`: iteration budget `bound - a0`
/// must cover copies 0..K-2's entry tests (i.e. `a_{K-2} cc bound`),
/// evaluated wide so it can't wrap. Branches to `uf` (unrolled body) on
/// success and `h` (scalar remainder) otherwise.
fn emit_trip_guard(
    func: &mut Function,
    hu: Block,
    uf: Block,
    h: Block,
    cnt: &Cnt,
    ity: Type,
    wty: Type,
    k: i64,
) {
    let huparams = func.dfg.block_params(hu).to_vec();
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
    // Exit-on-eq latches reduce to the strict unsigned test: a monotone
    // counter below the bound can't equal it.
    let (le, signed) = if cnt.eq { (false, false) } else { (cnt.le, cnt.signed) };
    let cc = match (signed, le) {
        (true, true) => IntCC::SignedLessThanOrEqual,
        (true, false) => IntCC::SignedLessThan,
        (false, true) => IntCC::UnsignedLessThanOrEqual,
        (false, false) => IntCC::UnsignedLessThan,
    };
    let ok1 = pos.ins().icmp(cc, a0, boundv);
    let (a0w, bw) = if wty != ity && signed {
        (
            pos.ins().sextend(wty, a0),
            pos.ins().sextend(wty, boundv),
        )
    } else if wty != ity {
        (pos.ins().uextend(wty, a0), pos.ins().uextend(wty, boundv))
    } else {
        (a0, boundv)
    };
    let rem = pos.ins().isub(bw, a0w);
    let lim = pos.ins().iconst(wty, (k - 2) * cnt.step);
    let ok2 = pos.ins().icmp(
        if le {
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

/// Redirect every outside-the-loop entry edge from `h` to `hu`.
fn redirect_entries(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    body: &FxHashSet<Block>,
    h: Block,
    hu: Block,
) {
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
    let Walk {
        chain,
        mut mid_exits,
        latch,
        rotated,
        body,
    } = walk(func, cfg, dt, la, lp, h, name)?;
    if loop_uses_vectors(func, &chain) {
        bail!("vector-typed loop");
    }
    let n_insts: usize = chain
        .iter()
        .map(|&b| func.layout.block_insts(b).count() - 1)
        .sum();
    let Some(k) = unroll_width(n_insts) else {
        bail!("body too big");
    };
    if !body_insts_clonable(func, &chain) {
        bail!("unclonable inst");
    }

    // Rotated: h's `brif test -> body | exit` is the counting test — pop
    // it out of `mid_exits` (it plays the do-while latch's role, not a
    // side exit's). The chain's `jump h` is the backedge: its args re-bind
    // h's params for the next iteration, exactly like the do-while
    // latch's `cont` edge.
    let top_test = if rotated {
        if mid_exits.first().map(|e| e.pos) != Some(0) {
            bail!("rotated loop without top test");
        }
        Some(mid_exits.remove(0))
    } else {
        None
    };

    let lt = func.layout.last_inst(latch).unwrap();
    // `test_i`/`texit_bc`/`tcpos`: the counting-test brif and its edges —
    // the latch brif for do-while loops, h's own brif for rotated ones
    // (tcpos = position of the continue dest). `back_bc`: the call whose
    // args re-bind h's params on the backedge — the latch `cont` edge, or
    // the rotated `jump h` call.
    let (test_i, texit_bc, tcpos, back_bc) = if rotated {
        let tt = top_test.as_ref().unwrap();
        let ti = func.layout.last_inst(h).unwrap();
        let bcs: Vec<BlockCall> = func.dfg.insts[lt]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .to_vec();
        (ti, tt.exit_bc, 1 - tt.epos, bcs[0])
    } else {
        let dests: Vec<BlockCall> = func.dfg.insts[lt]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .to_vec();
        let Some(cpos) = dests
            .iter()
            .position(|bc| bc.block(&func.dfg.value_lists) == h)
        else {
            bail!("no backedge dest");
        };
        (lt, dests[1 - cpos], cpos, dests[cpos])
    };
    let exit_bc = texit_bc;
    if body.contains(&exit_bc.block(&func.dfg.value_lists)) {
        bail!("exit inside loop");
    }
    let InstructionData::Brif { arg: c, .. } = func.dfg.insts[test_i] else {
        bail!("cond not a brif")
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
    let cond = if tcpos == 0 { cond } else { cond.complement() };

    let hparams = func.dfg.block_params(h).to_vec();
    let cont_args: Vec<Value> = back_bc
        .args(&func.dfg.value_lists)
        .filter_map(|a| match a {
            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
            _ => None,
        })
        .collect();
    if cont_args.len() != hparams.len() {
        bail!("backedge arity");
    }

    let Some(cnt) = find_count(func, h, &body, cfg, dt, x, y, cond, &hparams, &cont_args)
    else {
        bail!("no counting test")
    };

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
        match analyze_exit_cone(func, &body, dt, &lvals, e.cone.seblk, &mut clone_insts) {
            Ok(cone) => e.cone = cone,
            Err(why) => bail!(why),
        }
    }

    {
        let targets: Vec<Block> = std::iter::once(eblk)
            .chain(mid_exits.iter().map(|e| e.cone.seblk))
            .collect();
        if !exit_continuations_ok(func, &body, dt, &lvals, &targets) {
            bail!("body value used outside exit cones");
        }
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
    // Segments: `usegs[kk][j]` is segment j of copy kk. A mid-chain side
    // exit ends a segment (the brif needs a real target); otherwise copies
    // stay in ONE block so load/consumer adjacency survives the egraph
    // (see above). With no side exits, `usegs` is a single fused block —
    // the pre-side-exit shape.
    let nseg = mid_exits.len() + 1;
    let mut usegs: Vec<Vec<Block>> = Vec::with_capacity(k as usize);
    for kk in 0..k as usize {
        // Rotated: the last copy keeps the top test — its chain[0] ends in
        // the test brif, so it needs one extra segment over the mid-exit
        // count. Earlier copies drop the test and stay fused.
        let extra = (rotated && kk + 1 == k as usize) as usize;
        if mid_exits.is_empty() && kk > 0 && extra == 0 {
            usegs.push(Vec::new());
            continue;
        }
        let mut row = Vec::with_capacity(nseg + extra);
        for _ in 0..nseg + extra {
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

    emit_trip_guard(func, hu, uf, h, &cnt, ity, wty, k);

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
    // copy's `brif` carries that copy's bindings on the edge.
    emit_exit_cones(
        func,
        h,
        &mut mid_exits
            .iter_mut()
            .map(|e| &mut e.cone)
            .collect::<Vec<_>>(),
    );

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
                InstructionData::Jump { .. } if i + 1 < chain.len() => {
                    // Fallthrough: bind the next chain block's params from
                    // this jump's args.
                    let bc = tdata
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[0];
                    let sps = func.dfg.block_params(chain[i + 1]).to_vec();
                    for (j, a) in bc.args(&func.dfg.value_lists).enumerate() {
                        carry.insert(sps[j], mvv(func, a));
                    }
                }
                InstructionData::Jump { .. } => {
                    // Rotated backedge (`jump h`): copies 0..K-2 bind
                    // chain[0]'s params for the next copy and move to its
                    // first segment when it has one (empty row ⇒ fused
                    // fallthrough). The last copy becomes `jump hu` —
                    // back to the guard, like the do-while `brif -> hu`.
                    let bc = tdata
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[0];
                    if kk + 1 < k as usize {
                        let sps = func.dfg.block_params(chain[0]).to_vec();
                        for (j, a) in bc.args(&func.dfg.value_lists).enumerate() {
                            carry.insert(sps[j], mvv(func, a));
                        }
                        if !usegs[kk + 1].is_empty() {
                            let mut pos = FuncCursor::new(func).at_bottom(curseg);
                            pos.ins().jump(usegs[kk + 1][0], &[]);
                            curseg = usegs[kk + 1][0];
                        }
                    } else {
                        let cont: Vec<BlockArg> = bc
                            .args(&func.dfg.value_lists)
                            .map(|a| mv(func, a))
                            .collect();
                        let mut pos = FuncCursor::new(func).at_bottom(curseg);
                        pos.ins().jump(hu, &cont);
                    }
                }
                InstructionData::Brif { arg: c, .. } if rotated && i == 0 => {
                    // The top test. Copies 0..K-2's entry tests are
                    // implied by the guard: drop the brif and fall into
                    // the body, binding chain[1]'s params from the cont
                    // edge. The last copy keeps the test — its continue
                    // edge opens a fresh segment holding its own body —
                    // and exits on the original exit edge (clone `eu`
                    // when the exit cone references loop values).
                    let tt = top_test.as_ref().unwrap();
                    let c = func.dfg.resolve_aliases(c);
                    let carg = *vmap.get(&c).unwrap_or(&c);
                    let sps = func.dfg.block_params(chain[1]).to_vec();
                    for (j, a) in tt.cont_bc.args(&func.dfg.value_lists).enumerate() {
                        carry.insert(sps[j], mvv(func, a));
                    }
                    if kk + 1 == k as usize {
                        let eargs: Vec<BlockArg> = tt
                            .exit_bc
                            .args(&func.dfg.value_lists)
                            .map(|a| mv(func, a))
                            .collect();
                        let nxt = usegs[kk][seg_i + 1];
                        let mut pos = FuncCursor::new(func).at_bottom(curseg);
                        if tt.epos == 0 {
                            pos.ins().brif(carg, eu, &eargs, nxt, &[]);
                        } else {
                            pos.ins().brif(carg, nxt, &[], eu, &eargs);
                        }
                        curseg = nxt;
                        seg_i += 1;
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
                    if e.cone.clone_root.is_some() {
                        for &lv in &e.cone.lvals {
                            let lv = func.dfg.resolve_aliases(lv);
                            eargs.push(BlockArg::Value(*vmap.get(&lv).unwrap_or(&lv)));
                        }
                    }
                    let nxt = usegs[kk][seg_i + 1];
                    let etgt = e.cone.clone_root.unwrap_or(e.cone.seblk);
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
                        for (j, a) in back_bc.args(&func.dfg.value_lists).enumerate() {
                            carry.insert(sps[j], mvv(func, a));
                        }
                        if !mid_exits.is_empty() {
                            let mut pos = FuncCursor::new(func).at_bottom(curseg);
                            pos.ins().jump(usegs[kk + 1][0], &[]);
                            curseg = usegs[kk + 1][0];
                        }
                    } else {
                        let cont: Vec<BlockArg> = back_bc
                            .args(&func.dfg.value_lists)
                            .map(|a| mv(func, a))
                            .collect();
                        let exit: Vec<BlockArg> = exit_bc
                            .args(&func.dfg.value_lists)
                            .map(|a| mv(func, a))
                            .collect();
                        let mut pos = FuncCursor::new(func).at_bottom(curseg);
                        if tcpos == 0 {
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

    redirect_entries(func, cfg, &body, h, hu);
    Some(())
}

/// Loops already doing vector work (loopvec output, SIMD intrinsics) are
/// vf*UNROLL-wide in the element domain; cloning the body again pays a
/// fresh group guard per K*vf*UNROLL elements for no benefit and pessi-
/// mizes addressing (post-index forms get cloned into reg+imm offsets).
fn loop_uses_vectors(func: &Function, blocks: &[Block]) -> bool {
    blocks.iter().any(|&b| {
        func.dfg
            .block_params(b)
            .iter()
            .any(|&v| func.dfg.value_type(v).is_vector())
            || func.layout.block_insts(b).any(|i| {
                func.dfg
                    .inst_args(i)
                    .iter()
                    .chain(func.dfg.inst_results(i))
                    .any(|&v| func.dfg.value_type(v).is_vector())
            })
    })
}

/// Copy count for a body of `n_insts` non-terminator instructions. 16 is
/// the small-body cut for a full 8x unroll; it also covers the
/// load+check+store RMW shape bcheck clones leave (`movz; addl (mem)`
/// bodies where stock still unrolls 8). `None` when the body is too big.
fn unroll_width(n_insts: usize) -> Option<i64> {
    if n_insts <= 16 {
        Some(UNROLL)
    } else if n_insts <= MAX_BODY {
        Some(UNROLL_BIG)
    } else {
        None
    }
}

/// Non-terminator insts must be plain (no branches/calls-with-edges/etc.).
fn body_insts_clonable(func: &Function, blocks: &[Block]) -> bool {
    blocks.iter().all(|&b| {
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        insts[..insts.len() - 1].iter().all(|&i| {
            let op = func.dfg.insts[i].opcode();
            !(op.is_terminator()
                || op.is_branch()
                || matches!(op, Opcode::TryCall | Opcode::TryCallIndirect))
        })
    })
}

// ---------------------------------------------------------------------------
// Region unrolling: multi-block bodies
// ---------------------------------------------------------------------------

/// Body-block count cap for `run_region` — each copy clones every block,
/// so regions stay small.
const MAX_REGION_BLOCKS: usize = 8;

/// A body block whose terminator has one edge to `h` — the backedge.
/// `counting` when its test is the trip-count exit (droppable under the
/// guard in interior copies); a non-counting latch is a conditional
/// `break` kept verbatim in every copy, its h-edge retargeted to the
/// next copy's header clone (or `hu` in the last).
struct Latch {
    blk: Block,
    /// Backedge call (to `h`); its args re-bind `h`'s params.
    back_bc: BlockCall,
    /// Slot (0/1) of the h-dest in the `brif` (the other dest is an
    /// out-of-body exit, remapped through `exits` at emission).
    cpos: usize,
    counting: bool,
}

/// `v` as `hparam + off` for the region walk: `h` params resolve to
/// their own index; `iadd` by a constant shifts the offset; a param of
/// an in-body block resolves to whatever its forward pred edges bind
/// (all must agree — a real merge can't be affine). Fuel caps the
/// recursion against degenerate param cycles.
fn resolve_affine(
    func: &Function,
    cfg: &ControlFlowGraph,
    h: Block,
    body: &FxHashSet<Block>,
    v: Value,
    fuel: usize,
) -> Option<(usize, i64)> {
    if fuel == 0 {
        return None;
    }
    let v = func.dfg.resolve_aliases(v);
    if let Some(i) = param_idx(func, h, v) {
        return Some((i, 0));
    }
    match func.dfg.value_def(v) {
        cranelift_codegen::ir::ValueDef::Param(b, j) if body.contains(&b) => {
            // Only `h` has out-of-body preds; a body block with one would
            // be a second entry — not an affine value regardless.
            let mut merged = None;
            for p in cfg.pred_iter(b) {
                if !body.contains(&p.block) {
                    return None;
                }
                for bc in func.dfg.insts[p.inst].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    if bc.block(&func.dfg.value_lists) != b {
                        continue;
                    }
                    let BlockArg::Value(a) = bc.args(&func.dfg.value_lists).nth(j)? else {
                        return None;
                    };
                    let r = resolve_affine(func, cfg, h, body, a, fuel - 1)?;
                    if merged.is_some_and(|m| m != r) {
                        return None;
                    }
                    merged = Some(r);
                }
            }
            merged
        }
        _ => {
            let ii = func.dfg.value_def(v).inst()?;
            let InstructionData::Binary {
                opcode: Opcode::Iadd,
                args,
            } = func.dfg.insts[ii]
            else {
                return None;
            };
            for (x, c) in [(args[0], args[1]), (args[1], args[0])] {
                if let Some(k) = iconst(func, c)
                    && let Some((p, d)) = resolve_affine(func, cfg, h, body, x, fuel - 1)
                {
                    return Some((p, d.wrapping_add(k)));
                }
            }
            None
        }
    }
}

/// `find_count`'s counterpart for region latches: `a cc bound` where `a`
/// resolves (through in-body params) to `hparam + d` stepping `+s` on
/// *every* backedge in `backedges`, `bound` loop-invariant. Also accepts
/// exit-on-`a == bound` (a `!=` continue test) — under a monotone
/// counter it drops to the same strict `< bound` guard.
fn latch_count(
    func: &Function,
    h: Block,
    body: &FxHashSet<Block>,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    x: Value,
    y: Value,
    cond: IntCC,
    hparams: &[Value],
    backedges: &[BlockCall],
) -> Option<Cnt> {
    let try_one = |a: Value, b: Value, cc: IntCC| -> Option<Cnt> {
        let (le, signed, eq) = match cc {
            IntCC::UnsignedLessThan => (false, false, false),
            IntCC::UnsignedLessThanOrEqual => (true, false, false),
            IntCC::SignedLessThan => (false, true, false),
            IntCC::SignedLessThanOrEqual => (true, true, false),
            IntCC::NotEqual => (false, false, true),
            _ => return None,
        };
        let (pidx, d) = resolve_affine(func, cfg, h, body, a, 16)?;
        let pty = func.dfg.value_type(hparams[pidx]);
        if !pty.is_int() || pty.bits() < 16 || pty.bits() > 64 {
            return None;
        }
        // Step: the counter's backedge value must be `p + s` on *every*
        // latch, s the same positive const.
        let mut step = None;
        for bc in backedges {
            let BlockArg::Value(av) = bc.args(&func.dfg.value_lists).nth(pidx)? else {
                return None;
            };
            let (p, s) = resolve_affine(func, cfg, h, body, av, 16)?;
            if p != pidx || s <= 0 || step.is_some_and(|t| t != s) {
                return None;
            }
            step = Some(s);
        }
        let step = step?;
        // Bound must be loop-invariant: defined outside the body, or a
        // header param passed back unchanged on every backedge (usable
        // at hu as hu's own param).
        let b = func.dfg.resolve_aliases(b);
        let mut bpidx = None;
        if let Some(bi) = func.dfg.value_def(b).inst() {
            let bb = func.layout.inst_block(bi)?;
            if body.contains(&bb) {
                return None;
            }
            if !cfg
                .pred_iter(h)
                .filter(|p| !body.contains(&p.block))
                .all(|p| dt.dominates(bi, p.inst, &func.layout))
            {
                return None;
            }
        } else {
            let unchanged = |q: usize| {
                backedges.iter().all(|bc| {
                    matches!(
                        bc.args(&func.dfg.value_lists).nth(q),
                        Some(BlockArg::Value(v)) if func.dfg.resolve_aliases(v) == b
                    )
                })
            };
            match func.dfg.value_def(b) {
                cranelift_codegen::ir::ValueDef::Param(bb, q) if bb == h && unchanged(q) => {
                    bpidx = Some(q);
                }
                cranelift_codegen::ir::ValueDef::Param(bb, _)
                    if bb != h && dt.dominates(bb, h, &func.layout) => {}
                _ => return None,
            }
        }
        Some(Cnt {
            pidx,
            d,
            step,
            bound: b,
            bpidx,
            le,
            signed,
            eq,
        })
    };
    try_one(x, y, cond).or_else(|| try_one(y, x, cond.swap_args()))
}

/// Partial unrolling of a *multi-block* counted loop — the counterpart
/// of `run_loop` for bodies the linear chain can't walk (diamonds like
/// `h: brif sel -> armA | armB; arm*: ...; brif cnt -> h | exit`, where
/// each arm carries its own latch test).
///
/// Copy kk clones every body block once — real CFG, not the chain's
/// fused straight-line block, since the arm-select brif is
/// data-dependent and must stay a branch. Interior copies drop the
/// counting latch tests (guard-covered) and retarget their backedges to
/// the next copy's header clone; the last copy's latches branch to
/// `hu`/exit exactly like the chain path. Conditional (`break`) latches
/// keep their test in every copy — only the h-edge is retargeted. The
/// original loop is untouched and handles the tail.
fn run_region(
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
    // ---- shape ----
    if !dt.is_reachable(h) || func.layout.is_cold(h) {
        bail!("header unreachable/cold");
    }
    // Only for bodies `walk` can't linearize.
    if walk(func, cfg, dt, la, lp, h, name).is_some() {
        bail!("linear shape");
    }
    let body: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, lp))
        .collect();
    if body.len() < 2 || body.len() > MAX_REGION_BLOCKS {
        bail!("region size");
    }
    if body.iter().any(|&b| func.layout.is_cold(b)) {
        bail!("cold body block");
    }
    // An inner loop's blocks sit outside `body` but re-enter it — an
    // edge to one would read as an exit and get cone-cloned.
    if la.loops().any(|c| la.loop_parent(c) == Some(lp)) {
        bail!("nested loop");
    }
    let rpo = cone_rpo(func, h, &body);
    if rpo.len() != body.len() {
        bail!("unreachable body block");
    }
    if loop_uses_vectors(func, &rpo) {
        bail!("vector-typed loop");
    }
    if !body_insts_clonable(func, &rpo) {
        bail!("unclonable inst");
    }
    // Every edge arg must be a plain value (the remap can't carry
    // other BlockArg kinds).
    for &b in &rpo {
        for i in func.layout.block_insts(b) {
            for bc in func.dfg.insts[i]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                if bc
                    .args(&func.dfg.value_lists)
                    .any(|a| !matches!(a, BlockArg::Value(_)))
                {
                    bail!("edge arg not a value");
                }
            }
        }
    }

    // ---- classify edges: latches carry the backedges, everything else
    // leaving the body is an exit ----
    let mut latches: Vec<Latch> = Vec::new();
    let mut exit_blks: FxHashSet<Block> = FxHashSet::default();
    for &b in &rpo {
        let t = func.layout.last_inst(b).unwrap();
        let dests: Vec<BlockCall> = func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .to_vec();
        match func.dfg.insts[t] {
            InstructionData::Jump { .. } => {
                let d = dests[0].block(&func.dfg.value_lists);
                if d == h {
                    bail!("unconditional backedge");
                }
                if !body.contains(&d) {
                    exit_blks.insert(d);
                }
            }
            InstructionData::Brif { .. } => {
                let d0 = dests[0].block(&func.dfg.value_lists);
                let d1 = dests[1].block(&func.dfg.value_lists);
                match (d0 == h, d1 == h) {
                    (true, true) => bail!("degenerate backedge"),
                    (h0, h1) if h0 != h1 => {
                        let cpos = if h0 { 0 } else { 1 };
                        let other = dests[1 - cpos];
                        if body.contains(&other.block(&func.dfg.value_lists)) {
                            bail!("conditional continue");
                        }
                        latches.push(Latch {
                            blk: b,
                            back_bc: dests[cpos],
                            cpos,
                            counting: false,
                        });
                        exit_blks.insert(other.block(&func.dfg.value_lists));
                    }
                    _ => {
                        for d in [d0, d1] {
                            if !body.contains(&d) {
                                exit_blks.insert(d);
                            }
                        }
                    }
                }
            }
            _ => bail!("unhandled terminator"),
        }
    }
    if latches.is_empty() {
        bail!("no latch");
    }
    for l in &latches {
        if l
            .back_bc
            .args(&func.dfg.value_lists)
            .count()
            != func.dfg.block_params(h).len()
        {
            bail!("backedge arity");
        }
    }

    // ---- counting analysis: at least one latch must be the counted
    // exit, and every counting latch must agree on the counter ----
    let hparams = func.dfg.block_params(h).to_vec();
    let backedges: Vec<BlockCall> = latches.iter().map(|l| l.back_bc).collect();
    let mut cnt: Option<Cnt> = None;
    for l in &mut latches {
        let t = func.layout.last_inst(l.blk).unwrap();
        let InstructionData::Brif { arg: c, .. } = func.dfg.insts[t] else {
            unreachable!()
        };
        let counting = (|| {
            let ci = func.dfg.value_def(func.dfg.resolve_aliases(c)).inst()?;
            let InstructionData::IntCompare {
                opcode: Opcode::Icmp,
                cond,
                args: [x, y],
            } = func.dfg.insts[ci]
            else {
                return None;
            };
            let cond = if l.cpos == 0 { cond } else { cond.complement() };
            latch_count(func, h, &body, cfg, dt, x, y, cond, &hparams, &backedges)
        })();
        let Some(c) = counting else { continue };
        match &cnt {
            None => cnt = Some(c),
            Some(u)
                if u.pidx == c.pidx
                    && u.d == c.d
                    && u.step == c.step
                    && u.bound == c.bound
                    && u.le == c.le
                    && u.signed == c.signed
                    && u.eq == c.eq => {}
            Some(_) => bail!("inconsistent counting tests"),
        }
        l.counting = true;
    }
    let Some(cnt) = cnt else {
        bail!("no counting latch");
    };

    // ---- exits: every out-of-body target gets the shared-cone
    // treatment, exactly like chain side exits ----
    let mut lvals: FxHashSet<Value> = FxHashSet::default();
    for &p in &hparams {
        lvals.insert(p);
    }
    for &b in &rpo {
        lvals.extend(func.dfg.block_params(b).iter().copied());
        for i in func.layout.block_insts(b) {
            lvals.extend(func.dfg.inst_results(i).iter().copied());
        }
    }
    let mut clone_insts = 0usize;
    let mut exits: Vec<ExitCone> = Vec::new();
    for &se in &exit_blks {
        match analyze_exit_cone(func, &body, dt, &lvals, se, &mut clone_insts) {
            Ok(cone) => exits.push(cone),
            Err(why) => bail!(why),
        }
    }
    let targets: Vec<Block> = exits.iter().map(|e| e.seblk).collect();
    if !exit_continuations_ok(func, &body, dt, &lvals, &targets) {
        bail!("body value used outside exit cones");
    }

    let n_insts: usize = rpo
        .iter()
        .map(|&b| func.layout.block_insts(b).count() - 1)
        .sum();
    let Some(k) = unroll_width(n_insts) else {
        bail!("body too big");
    };
    let ity = func.dfg.value_type(hparams[cnt.pidx]);
    let wty = if ity.bits() <= 32 { types::I64 } else { ity };

    // ---- emit ----
    // `hu` guard, then K full body clones: copy kk's latch edges go to
    // copy kk+1's header clone, the last copy's to `hu`/the exit cones.
    // Clone blocks take their source's params — interior edges bind
    // them verbatim, so no carry threading is needed.
    let hu = func.dfg.make_block();
    for &p in &hparams {
        let ty = func.dfg.value_type(p);
        func.dfg.append_block_param(hu, ty);
    }
    func.layout.insert_block(hu, h);
    let mut cmaps: Vec<FxHashMap<Block, Block>> = Vec::with_capacity(k as usize);
    for _ in 0..k {
        let mut cm: FxHashMap<Block, Block> = FxHashMap::default();
        for &b in &rpo {
            let nb = func.dfg.make_block();
            for &p in func.dfg.block_params(b).to_vec().iter() {
                let ty = func.dfg.value_type(p);
                func.dfg.append_block_param(nb, ty);
            }
            func.layout.insert_block(nb, h);
            cm.insert(b, nb);
        }
        cmaps.push(cm);
    }
    emit_exit_cones(func, h, &mut exits.iter_mut().collect::<Vec<_>>());
    emit_trip_guard(func, hu, cmaps[0][&h], h, &cnt, ity, wty, k);

    // `seblk` → its clone root (or itself when the cone needed no clone).
    let etgt = |d: Block| -> Block {
        exits
            .iter()
            .find(|e| e.seblk == d)
            .and_then(|e| e.clone_root)
            .unwrap_or(d)
    };
    let latch_at: FxHashMap<Block, &Latch> =
        latches.iter().map(|l| (l.blk, l)).collect();

    for kk in 0..k as usize {
        let cmap = &cmaps[kk];
        let last = kk + 1 == k as usize;
        let hv = if last { hu } else { cmaps[kk + 1][&h] };
        let mut xmap: FxHashMap<Value, Value> = FxHashMap::default();
        for &b in &rpo {
            let nb = cmap[&b];
            for (j, &sp) in func.dfg.block_params(b).to_vec().iter().enumerate() {
                xmap.insert(sp, func.dfg.block_params(nb)[j]);
            }
            let insts: Vec<Inst> = func.layout.block_insts(b).collect();
            for &ii in &insts[..insts.len() - 1] {
                let src = func.dfg.insts[ii];
                let data = {
                    let mut m = Vm {
                        func,
                        vmap: &xmap,
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
            // Terminator: remap each dest by class. `h` → next copy's
            // header clone (`hu` in the last), in-body → this copy's
            // clone, out-of-body → the exit clone root (with the cone's
            // `lvals` appended) or the original block.
            let mv = |func: &Function, a: BlockArg| -> BlockArg {
                match a {
                    BlockArg::Value(v) => {
                        let v = func.dfg.resolve_aliases(v);
                        BlockArg::Value(*xmap.get(&v).unwrap_or(&v))
                    }
                    a => a,
                }
            };
            let edge = |func: &Function, bc: BlockCall| -> (Block, Vec<BlockArg>) {
                let d = bc.block(&func.dfg.value_lists);
                let mut args: Vec<BlockArg> = bc
                    .args(&func.dfg.value_lists)
                    .map(|a| mv(func, a))
                    .collect();
                let t = if d == h {
                    hv
                } else if body.contains(&d) {
                    cmap[&d]
                } else {
                    let t = etgt(d);
                    if t != d {
                        let e = exits.iter().find(|e| e.seblk == d).unwrap();
                        for &lv in &e.lvals {
                            let lv = func.dfg.resolve_aliases(lv);
                            args.push(BlockArg::Value(*xmap.get(&lv).unwrap_or(&lv)));
                        }
                    }
                    t
                };
                (t, args)
            };
            let t = *insts.last().unwrap();
            let tdata = func.dfg.insts[t];
            let dests: Vec<BlockCall> = tdata
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                .to_vec();
            match tdata {
                InstructionData::Jump { .. } => {
                    let (t, args) = edge(func, dests[0]);
                    FuncCursor::new(func).at_bottom(nb).ins().jump(t, &args);
                }
                InstructionData::Brif { arg: c, .. } => {
                    let c = func.dfg.resolve_aliases(c);
                    let carg = *xmap.get(&c).unwrap_or(&c);
                    let l = latch_at.get(&b);
                    if !last && l.is_some_and(|l| l.counting) {
                        // Interior copy: the guard implies continue —
                        // drop the test, jump the next copy's header.
                        let (t, args) = edge(func, dests[l.unwrap().cpos]);
                        FuncCursor::new(func).at_bottom(nb).ins().jump(t, &args);
                    } else {
                        let (t0, a0) = edge(func, dests[0]);
                        let (t1, a1) = edge(func, dests[1]);
                        FuncCursor::new(func)
                            .at_bottom(nb)
                            .ins()
                            .brif(carg, t0, &a0, t1, &a1);
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    if debug() {
        eprintln!("punroll {name} block{}: region unrolled k={k}", h.as_u32());
    }
    redirect_entries(func, cfg, &body, h, hu);
    Some(())
}

// ---------------------------------------------------------------------------
// Loop peeling (PLIRON_PEEL)
// ---------------------------------------------------------------------------

/// How one copy binds an original loop value: `Val` is a concrete binding;
/// `Aff(base, off)` is `base + off` materialized lazily on use — affine
/// header params become `entry + k·step`, so pointer IVs fold into
/// addressing modes and counter IVs collapse to one `iadd` feeding the
/// trip test.
#[derive(Clone, Copy)]
enum Seed {
    Val(Value),
    Aff(Value, i64),
}

fn seed_of(func: &Function, smap: &FxHashMap<Value, Seed>, v: Value) -> Seed {
    let v = func.dfg.resolve_aliases(v);
    smap.get(&v).copied().unwrap_or(Seed::Val(v))
}

/// The concrete value for `v` in the current copy, emitting `iadd base, off`
/// into `nb` when `v` is in affine form (memoized through `smap`).
fn matv(func: &mut Function, nb: Block, smap: &mut FxHashMap<Value, Seed>, v: Value) -> Value {
    let v = func.dfg.resolve_aliases(v);
    match smap.get(&v).copied().unwrap_or(Seed::Val(v)) {
        Seed::Val(x) => x,
        Seed::Aff(b, 0) => b,
        Seed::Aff(b, o) => {
            let ty = func.dfg.value_type(v);
            let mut pos = FuncCursor::new(func).at_bottom(nb);
            let c = pos.ins().iconst(ty, o);
            let r = pos.ins().iadd(b, c);
            smap.insert(v, Seed::Val(r));
            r
        }
    }
}

/// Loop peeling: emit K sequential clones of the chain ahead of the
/// residual loop — LLVM's bounded-trip unroll shape (`movzbl k(p)`, one
/// `cmp` per digit, straight-line through `parse` digit loops). Affine
/// header params are re-expressed as `entry + k·step` per copy rather than
/// threaded through block params, killing the IV-update chain; every test
/// is kept verbatim and the last copy's continue edge lands on the
/// original loop, so the peel is correct for ANY trip count — no bound
/// proof needed, trips > K simply flow into the residual. The cost is
/// pure code size, capped by the small-body budget.
fn run_peel(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    lp: Loop,
    h: Block,
    name: &str,
) -> Option<()> {
    macro_rules! bail {
        ($why:expr) => {{
            if debug() {
                eprintln!("peel {name} block{}: {}", h.as_u32(), $why);
            }
            return None;
        }};
    }
    let Walk {
        chain,
        mid_exits,
        latch,
        body,
        rotated: _,
    } = walk(func, cfg, dt, la, lp, h, name)?;
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
                .chain(func.dfg.inst_results(i).iter())
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
    let kmax = if n_insts <= 12 {
        8usize
    } else if n_insts <= MAX_BODY {
        4
    } else {
        bail!("body too big");
    };
    // Every copy carries its exits verbatim: mid-chain exits plus the
    // latch test. Keep the peeled branch-site count bounded — past ~16
    // sites the straight-line copies predict worse than the rolled loop
    // they replace (measured on strwork's variable-length parse).
    let exits = mid_exits.len() + 1;
    let k = kmax.min(16 / exits.max(1)).max(1);
    for &b in &chain {
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for &i in &insts[..insts.len() - 1] {
            let op = func.dfg.insts[i].opcode();
            // Calls are correct to clone but never worth it: a small linear
            // loop wrapping a call (e.g. a tail-recursion turned loop) gets
            // k call sites for near-zero gain.
            if op.is_terminator()
                || op.is_branch()
                || op.is_call()
                || matches!(op, Opcode::TryCall | Opcode::TryCallIndirect)
            {
                bail!("unclonable inst");
            }
        }
    }

    let hparams = func.dfg.block_params(h).to_vec();
    // The backedge call whose args re-bind h's params (do-while: the latch
    // brif's h dest; rotated: the chain's `jump h`), plus the brif latch's
    // exit edge for do-while loops.
    let lt = func.layout.last_inst(latch).unwrap();
    let bcs: Vec<BlockCall> = func.dfg.insts[lt]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        .to_vec();
    let (back_bc, lexit) = match func.dfg.insts[lt] {
        InstructionData::Jump { .. } => (bcs[0], None),
        InstructionData::Brif { .. } => {
            let Some(cp) = bcs
                .iter()
                .position(|bc| bc.block(&func.dfg.value_lists) == h)
            else {
                bail!("no backedge dest")
            };
            (bcs[cp], Some(bcs[1 - cp]))
        }
        _ => bail!("bad latch"),
    };
    let cont_args: Vec<Value> = back_bc
        .args(&func.dfg.value_lists)
        .filter_map(|a| match a {
            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
            _ => None,
        })
        .collect();
    if cont_args.len() != hparams.len() {
        bail!("backedge arity");
    }
    // Every edge arg in the chain must be a plain value.
    for &b in &chain {
        let t = func.layout.last_inst(b).unwrap();
        for bc in func.dfg.insts[t]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        {
            if bc
                .args(&func.dfg.value_lists)
                .any(|a| !matches!(a, BlockArg::Value(_)))
            {
                bail!("non-value edge arg");
            }
        }
    }

    // Affine header params: backedge arg `p + s` (`isub` counts as -s) or
    // `p` itself (s = 0, invariant).
    let mut steps: Vec<Option<i64>> = Vec::with_capacity(hparams.len());
    for (j, &p) in hparams.iter().enumerate() {
        if !func.dfg.value_type(p).is_int() {
            steps.push(None);
            continue;
        }
        let a = func.dfg.resolve_aliases(cont_args[j]);
        if a == p {
            steps.push(Some(0));
            continue;
        }
        let s = func.dfg.value_def(a).inst().and_then(|i| {
            match func.dfg.insts[i] {
                InstructionData::Binary { opcode: Opcode::Iadd, args } => {
                    for (x, c) in [(args[0], args[1]), (args[1], args[0])] {
                        if func.dfg.resolve_aliases(x) == p {
                            return iconst(func, c);
                        }
                    }
                    None
                }
                InstructionData::Binary { opcode: Opcode::Isub, args } => {
                    if func.dfg.resolve_aliases(args[0]) == p {
                        return iconst(func, args[1]).map(|c| c.wrapping_neg());
                    }
                    None
                }
                _ => None,
            }
        });
        steps.push(s);
    }

    // Loop values ambiently used past an exit edge would bind wrongly from a
    // peeled copy: same check punroll applies — every non-dominated use of a
    // loop value reachable from an exit target disqualifies.
    let mut lvals: FxHashSet<Value> = hparams.iter().copied().collect();
    for &b in &chain {
        lvals.extend(func.dfg.block_params(b).iter().copied());
        for i in func.layout.block_insts(b) {
            lvals.extend(func.dfg.inst_results(i).iter().copied());
        }
    }
    {
        let ambient = |func: &Function, b: Block| -> bool {
            for i in func.layout.block_insts(b) {
                if func
                    .dfg
                    .inst_values(i)
                    .any(|v| lvals.contains(&func.dfg.resolve_aliases(v)))
                {
                    return true;
                }
                for bc in func.dfg.insts[i].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    if bc.args(&func.dfg.value_lists).any(|a| {
                        matches!(a, BlockArg::Value(v) if lvals.contains(&func.dfg.resolve_aliases(v)))
                    }) {
                        return true;
                    }
                }
            }
            false
        };
        let mut tgts: Vec<Block> = mid_exits.iter().map(|e| e.cone.seblk).collect();
        if let Some(ebc) = lexit {
            tgts.push(ebc.block(&func.dfg.value_lists));
        }
        for tgt in tgts {
            let mut seen: FxHashSet<Block> = FxHashSet::default();
            let mut stack = vec![tgt];
            while let Some(b) = stack.pop() {
                if body.contains(&b) || !seen.insert(b) || !dt.is_reachable(b) {
                    continue;
                }
                if seen.len() > 512 {
                    bail!("exit continuation too big");
                }
                // Any lval operand in the exit region is unsalvageable:
                // peeled copies branch to `tgt` directly, so a loop value
                // could never dominate — and semantically it would bind the
                // residual iteration, not the peeled one. Edge args are
                // remapped at the peeled edge, so only bare uses disqualify.
                if ambient(func, b) {
                    bail!("body value used outside exit edges");
                }
                for i in func.layout.block_insts(b) {
                    for bc in func.dfg.insts[i].branch_destination(
                        &func.dfg.jump_tables,
                        &func.dfg.exception_tables,
                    ) {
                        stack.push(bc.block(&func.dfg.value_lists));
                    }
                }
            }
        }
    }

    // `pe` takes h's params verbatim from every entry edge — it is the
    // uniform affine base (`entry + k·s`) and gives multi-entry loops one
    // merge point. Copy blocks take no params at all: the chain is
    // single-pred, so each param binds eagerly from its incoming edge.
    let pe = func.dfg.make_block();
    for &p in &hparams {
        let ty = func.dfg.value_type(p);
        func.dfg.append_block_param(pe, ty);
    }
    func.layout.insert_block(pe, h);
    let peps = func.dfg.block_params(pe).to_vec();
    let cblks: Vec<Vec<Block>> = (0..k)
        .map(|_| {
            chain
                .iter()
                .map(|_| {
                    let nb = func.dfg.make_block();
                    func.layout.insert_block(nb, h);
                    nb
                })
                .collect()
        })
        .collect();
    FuncCursor::new(func).at_bottom(pe).ins().jump(cblks[0][0], &[]);

    let mpos: FxHashMap<usize, usize> = mid_exits
        .iter()
        .enumerate()
        .map(|(ei, e)| (e.pos, ei))
        .collect();
    // `nxt[j]` is the seed for h's param j at the NEXT copy — affine params
    // get their `entry + k·s` form regardless of the carried value.
    let mut nxt: Vec<Seed> = peps.iter().map(|&p| Seed::Val(p)).collect();
    for kk in 0..k {
        let mut smap: FxHashMap<Value, Seed> = FxHashMap::default();
        for (j, &p) in hparams.iter().enumerate() {
            let s = match steps[j] {
                Some(s) => Seed::Aff(peps[j], s.wrapping_mul(kk as i64)),
                None => nxt[j],
            };
            smap.insert(p, s);
        }
        // Params of mid-chain blocks, seeded from the pending edge args.
        let mut pend: Vec<Seed> = Vec::new();
        for (i, &b) in chain.iter().enumerate() {
            let nb = cblks[kk][i];
            if i > 0 {
                for (j, &sp) in func.dfg.block_params(b).to_vec().iter().enumerate() {
                    smap.insert(sp, pend.get(j).copied().unwrap_or(Seed::Val(sp)));
                }
            }
            let insts: Vec<Inst> = func.layout.block_insts(b).collect();
            for &ii in &insts[..insts.len() - 1] {
                let mut kept = false;
                if let InstructionData::Binary { opcode, args } = func.dfg.insts[ii]
                    && matches!(opcode, Opcode::Iadd | Opcode::Isub)
                {
                    let fold = match (opcode, args) {
                        (Opcode::Iadd, [x, c]) | (Opcode::Iadd, [c, x])
                            if iconst(func, c).is_some() =>
                        {
                            match seed_of(func, &smap, x) {
                                Seed::Aff(bv, o) => {
                                    Some((bv, o.wrapping_add(iconst(func, c).unwrap())))
                                }
                                _ => None,
                            }
                        }
                        (Opcode::Isub, [x, c]) if iconst(func, c).is_some() => {
                            match seed_of(func, &smap, x) {
                                Seed::Aff(bv, o) => {
                                    Some((bv, o.wrapping_sub(iconst(func, c).unwrap())))
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some((bv, o)) = fold {
                        let r = func.dfg.resolve_aliases(func.dfg.first_result(ii));
                        smap.insert(r, Seed::Aff(bv, o));
                        kept = true;
                    }
                }
                if kept {
                    continue;
                }
                let ni = func.dfg.clone_inst(ii);
                let srcs: Vec<Value> = func.dfg.inst_values(ni).collect();
                let vals: Vec<Value> = srcs
                    .iter()
                    .map(|&v| matv(func, nb, &mut smap, v))
                    .collect();
                func.dfg.overwrite_inst_values(ni, vals.into_iter());
                func.layout.append_inst(ni, nb);
                let orig: Vec<Value> = func.dfg.inst_results(ii).to_vec();
                let new: Vec<Value> = func.dfg.inst_results(ni).to_vec();
                for (&o, &n) in orig.iter().zip(new.iter()) {
                    smap.insert(func.dfg.resolve_aliases(o), Seed::Val(n));
                }
            }
            let t = *insts.last().unwrap();
            fn eargs(
                func: &mut Function,
                nb: Block,
                smap: &mut FxHashMap<Value, Seed>,
                bc: BlockCall,
            ) -> Vec<BlockArg> {
                let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
                args.iter()
                    .map(|a| match a {
                        BlockArg::Value(v) => BlockArg::Value(matv(func, nb, smap, *v)),
                        a => *a,
                    })
                    .collect()
            }
            fn sargs(
                func: &Function,
                smap: &FxHashMap<Value, Seed>,
                bc: BlockCall,
            ) -> Vec<Seed> {
                bc.args(&func.dfg.value_lists)
                    .map(|a| match a {
                        BlockArg::Value(v) => seed_of(func, smap, v),
                        _ => unreachable!("peel: non-value edge arg"),
                    })
                    .collect()
            }
            match func.dfg.insts[t] {
                InstructionData::Jump { .. } if i + 1 < chain.len() => {
                    let bc = bcs_of(func, t)[0];
                    pend = sargs(func, &smap, bc);
                    FuncCursor::new(func).at_bottom(nb).ins().jump(cblks[kk][i + 1], &[]);
                }
                InstructionData::Jump { .. } => {
                    // Rotated backedge `jump h`.
                    if kk + 1 < k {
                        nxt = sargs(func, &smap, back_bc);
                        FuncCursor::new(func).at_bottom(nb).ins().jump(cblks[kk + 1][0], &[]);
                    } else {
                        let cont = eargs(func, nb, &mut smap, back_bc);
                        FuncCursor::new(func).at_bottom(nb).ins().jump(h, &cont);
                    }
                }
                InstructionData::Brif { arg: c, .. } if mpos.contains_key(&i) => {
                    let e = &mid_exits[mpos[&i]];
                    let carg = matv(func, nb, &mut smap, c);
                    pend = sargs(func, &smap, e.cont_bc);
                    let ex = eargs(func, nb, &mut smap, e.exit_bc);
                    let nxtblk = cblks[kk][i + 1];
                    let mut pos = FuncCursor::new(func).at_bottom(nb);
                    if e.epos == 0 {
                        pos.ins().brif(carg, e.cone.seblk, &ex, nxtblk, &[]);
                    } else {
                        pos.ins().brif(carg, nxtblk, &[], e.cone.seblk, &ex);
                    }
                }
                InstructionData::Brif { arg: c, .. } => {
                    // Do-while latch.
                    let carg = matv(func, nb, &mut smap, c);
                    let ebc = lexit.unwrap();
                    let ex = eargs(func, nb, &mut smap, ebc);
                    let (ctgt, cargs) = if kk + 1 < k {
                        nxt = sargs(func, &smap, back_bc);
                        (cblks[kk + 1][0], Vec::new())
                    } else {
                        (h, eargs(func, nb, &mut smap, back_bc))
                    };
                    let cpos = bcs
                        .iter()
                        .position(|bc| bc.block(&func.dfg.value_lists) == h)
                        .unwrap();
                    let eblk = ebc.block(&func.dfg.value_lists);
                    let mut pos = FuncCursor::new(func).at_bottom(nb);
                    if cpos == 0 {
                        pos.ins().brif(carg, ctgt, &cargs, eblk, &ex);
                    } else {
                        pos.ins().brif(carg, eblk, &ex, ctgt, &cargs);
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    // Redirect every outside-the-body entry edge from h to pe.
    let entries: Vec<(Block, Inst)> = cfg
        .pred_iter(h)
        .filter(|p| !body.contains(&p.block))
        .map(|p| (p.block, p.inst))
        .collect();
    for (_pb, pi) in entries {
        let ndest = func.dfg.insts[pi]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .len();
        for slot in 0..ndest {
            let dfg = &mut func.dfg;
            let bc = &mut dfg.insts[pi].branch_destination_mut(
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
            )[slot];
            if bc.block(&dfg.value_lists) == h {
                let args: Vec<BlockArg> = bc.args(&dfg.value_lists).collect();
                *bc = BlockCall::new(pe, args.iter().copied(), &mut dfg.value_lists);
            }
        }
    }
    Some(())
}

fn bcs_of(func: &Function, i: Inst) -> Vec<BlockCall> {
    func.dfg.insts[i]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        .to_vec()
}

// ---------------------------------------------------------------------------
// Unroll-and-jam (PLIRON_JAM)
// ---------------------------------------------------------------------------

/// A brif mid-exit shared by the inner- and outer-chain walks: one dest
/// continues the chain, the other leaves it.
struct JamExit {
    /// Chain position of the exiting block.
    pos: usize,
    /// In-body continue edge.
    cont_bc: BlockCall,
    /// Out-of-body edge.
    exit_bc: BlockCall,
    /// `exit_bc` target.
    tgt: Block,
    /// Position (0/1) of the exit dest in the original `brif`.
    epos: usize,
    /// Dominance cone of `tgt` (non-body blocks), RPO order.
    corder: Vec<Block>,
    /// Loop/body values referenced inside `corder`, appended as clone
    /// params.
    lvals: Vec<Value>,
    /// The shared clone root, when `lvals` is non-empty.
    clone_root: Option<Block>,
}

/// Unroll-and-jam: outer loop `olp` (header `oh`) whose body is a linear
/// chain wrapping exactly one linear-chain inner loop gets J cloned copies
/// of its body with the J inner loops fused into ONE inner loop — each
/// fused iteration does the work of J consecutive outer iterations.
/// Values independent of the outer IV are emitted once per fused
/// iteration (shared loads/checks); IV-dependent values get J clones, so
/// every output element's arithmetic keeps its original scalar order —
/// bitwise-identical results with J independent dependency chains. The
/// original nest remains as the remainder for the last `n % J` outer
/// iterations and for any shape the fusion can't cover.
fn run_jam(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    olp: Loop,
    oh: Block,
    name: &str,
) -> Option<Vec<Block>> {
    macro_rules! bail {
        ($why:expr) => {{
            if debug() {
                eprintln!("jam {name} block{}: {}", oh.as_u32(), $why);
            }
            return None;
        }};
    }
    let j = JAM;
    if debug() {
        eprintln!("jam {name} block{}: try", oh.as_u32());
    }
    if !dt.is_reachable(oh) || func.layout.is_cold(oh) {
        bail!("header unreachable/cold");
    }
    let ilp = la
        .loops()
        .find(|&c| la.loop_parent(c) == Some(olp))
        .unwrap();
    if la.loops().any(|c| la.loop_parent(c) == Some(ilp)) {
        bail!("inner not innermost");
    }
    let ih = la.loop_header(ilp);
    let obody: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, olp))
        .collect();
    let ibody: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, ilp))
        .collect();

    // ---- inner chain walk (linear chain; exits split into the single
    //      normal `post` exit and cold side exits) ----
    let mut ichain = vec![ih];
    let mut iexits: Vec<JamExit> = Vec::new(); // cold side exits only
    let mut ipost: Option<(usize, BlockCall, BlockCall, usize)> = None; // pos, cont_bc, post_bc, epos
    let mut post: Option<Block> = None;
    let ilatch;
    loop {
        let cur = *ichain.last().unwrap();
        if func.layout.is_cold(cur) {
            bail!("cold block in inner chain");
        }
        let t = func.layout.last_inst(cur)?;
        match func.dfg.insts[t] {
            InstructionData::Jump { destination, .. } => {
                let nb = destination.block(&func.dfg.value_lists);
                if nb == ih || !ibody.contains(&nb) || ichain.contains(&nb) {
                    bail!("nonlinear inner chain");
                }
                ichain.push(nb);
                if ichain.len() > MAX_CHAIN {
                    bail!("inner chain too long");
                }
            }
            InstructionData::Brif { .. } => {
                let dests: Vec<BlockCall> = func.dfg.insts[t]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    .to_vec();
                let d0 = dests[0].block(&func.dfg.value_lists);
                let d1 = dests[1].block(&func.dfg.value_lists);
                if d0 == ih || d1 == ih {
                    ilatch = cur;
                    break;
                }
                let (cont_bc, exit_bc, epos) = if ibody.contains(&d0) && !ibody.contains(&d1) {
                    (dests[0], dests[1], 1)
                } else if ibody.contains(&d1) && !ibody.contains(&d0) {
                    (dests[1], dests[0], 0)
                } else {
                    bail!("nonlinear inner chain");
                };
                let eb = exit_bc.block(&func.dfg.value_lists);
                let nb = cont_bc.block(&func.dfg.value_lists);
                if ichain.contains(&nb) {
                    bail!("nonlinear inner chain");
                }
                if obody.contains(&eb) {
                    // The normal loop exit continuing the outer chain.
                    if post.is_some() {
                        bail!("multiple inner exits");
                    }
                    post = Some(eb);
                    ipost = Some((ichain.len() - 1, cont_bc, exit_bc, epos));
                } else {
                    if !func.layout.is_cold(eb) {
                        bail!("inner exit to warm out-of-loop block");
                    }
                    iexits.push(JamExit {
                        pos: ichain.len() - 1,
                        cont_bc,
                        exit_bc,
                        tgt: eb,
                        epos,
                        corder: Vec::new(),
                        lvals: Vec::new(),
                        clone_root: None,
                    });
                }
                ichain.push(nb);
                if ichain.len() > MAX_CHAIN {
                    bail!("inner chain too long");
                }
            }
            _ => bail!("unhandled inner terminator"),
        }
    }
    if ibody.len() != ichain.len() {
        bail!("extra inner blocks");
    }
    let (ipos, _ipcont, ipost_bc, _ipepos) = ipost?;
    let post = post?;
    // The inner latch's non-backedge dest is the loop exit — it must be
    // the same `post`.
    {
        let lt = func.layout.last_inst(ilatch).unwrap();
        for bc in func.dfg.insts[lt]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        {
            let d = bc.block(&func.dfg.value_lists);
            if d != ih && d != post {
                bail!("inner latch exits elsewhere");
            }
        }
    }
    // Inner-body insts: pure or loads only (they get cloned J ways).
    let mut n_inner = 0usize;
    for &b in &ichain {
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for &i in &insts[..insts.len() - 1] {
            n_inner += 1;
            let op = func.dfg.insts[i].opcode();
            if op.is_terminator()
                || op.is_branch()
                || op.is_call()
                || op.can_store()
                || op.other_side_effects()
            {
                bail!("unclonable inner inst");
            }
            if func
                .dfg
                .inst_args(i)
                .iter()
                .chain(func.dfg.inst_results(i).iter())
                .any(|&v| func.dfg.value_type(v).is_vector())
            {
                bail!("vector inner inst");
            }
        }
    }
    if n_inner > MAX_JAM_BODY {
        bail!("inner body too big");
    }

    // ---- outer chain walk (inner loop = opaque element) ----
    // oh must be a header-tested counted loop: `brif` with a counting
    // `icmp` and one in-body cont dest.
    let lt = func.layout.last_inst(oh)?;
    let dests: Vec<BlockCall> = func.dfg.insts[lt]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        .to_vec();
    if dests.len() != 2 || !matches!(func.dfg.insts[lt], InstructionData::Brif { .. }) {
        bail!("outer header not a brif");
    }
    let (ocont_bc, oexit_bc, ocpos) = if obody.contains(&dests[0].block(&func.dfg.value_lists))
        && !obody.contains(&dests[1].block(&func.dfg.value_lists))
    {
        (dests[0], dests[1], 1)
    } else if obody.contains(&dests[1].block(&func.dfg.value_lists))
        && !obody.contains(&dests[0].block(&func.dfg.value_lists))
    {
        (dests[1], dests[0], 0)
    } else {
        bail!("outer header dests not body/exit");
    };
    // Counted test on the header guard.
    let ohparams = func.dfg.block_params(oh).to_vec();
    let InstructionData::Brif { arg: gc, .. } = func.dfg.insts[lt] else {
        unreachable!()
    };
    let Some(gci) = func.dfg.value_def(func.dfg.resolve_aliases(gc)).inst() else {
        bail!("outer cond not an icmp");
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond: gcc,
        args: [gx, gy],
    } = func.dfg.insts[gci]
    else {
        bail!("outer cond not an icmp");
    };
    let gcc = if ocpos == 1 { gcc } else { gcc.complement() };
    // iv param + step + bound (header-tested `a cc bound`).
    let mut ocnt: Option<(usize, i64, Value, bool, bool)> = None; // pidx, step, bound, le, signed
    for (a, b, cc) in [(gx, gy, gcc), (gy, gx, gcc.swap_args())] {
        let (le, signed) = match cc {
            IntCC::UnsignedLessThan => (false, false),
            IntCC::UnsignedLessThanOrEqual => (true, false),
            IntCC::SignedLessThan => (false, true),
            IntCC::SignedLessThanOrEqual => (true, true),
            _ => continue,
        };
        let Some((pidx, _d)) = affine_param(func, oh, a) else {
            continue;
        };
        let pty = func.dfg.value_type(ohparams[pidx]);
        if !pty.is_int() || pty.bits() < 16 || pty.bits() > 64 {
            continue;
        }
        // Bound: loop-invariant (def outside olp, or an oh param passed
        // back unchanged on the latch edge).
        let b = func.dfg.resolve_aliases(b);
        let mut ok = false;
        if let Some(bi) = func.dfg.value_def(b).inst() {
            if let Some(bb) = func.layout.inst_block(bi)
                && !obody.contains(&bb)
            {
                ok = true;
            }
        } else if let Some(q) = param_idx(func, oh, b) {
            // Must come back unchanged: checked once the latch is found;
            // tentatively accept and verify later.
            ok = true;
            let _ = q;
        }
        if !ok {
            continue;
        }
        ocnt = Some((pidx, 0, b, le, signed));
        break;
    }
    let Some((opidx, _, obound, ole, osigned)) = ocnt else {
        bail!("no outer counting test");
    };
    let ivp = ohparams[opidx];

    // Walk the outer chain: oh -> pre* -> INNER -> post* -> latch.
    let mut opre: Vec<Block> = Vec::new();
    let mut opost: Vec<Block> = Vec::new();
    let mut oexits: Vec<JamExit> = Vec::new();
    let mut seen_inner = false;
    let mut into_inner: Option<(usize, BlockCall)> = None;
    let mut olatch = None;
    {
        // Position counters are indices into (opre ++ opost); pre-INNER
        // exits keep indices < inner_pos.
        let mut cur = ocont_bc.block(&func.dfg.value_lists);
        // The first pre block is entered through the header's cont edge;
        // it may itself be ih (empty pre chain).
        let mut pending_in = Some(ocont_bc);
        loop {
            if cur == ih {
                if seen_inner {
                    bail!("inner entered twice");
                }
                seen_inner = true;
                into_inner = Some((opre.len() + opost.len(), pending_in.take().unwrap()));
                cur = post;
                if cur == oh {
                    bail!("inner exit straight to header");
                }
                continue;
            }
            if seen_inner {
                opost.push(cur);
            } else {
                opre.push(cur);
            }
            if !obody.contains(&cur) || func.layout.is_cold(cur) {
                bail!("outer chain block out of body/cold");
            }
            if opre.len() + opost.len() > MAX_CHAIN + 2 {
                bail!("outer chain too long");
            }
            let t = func.layout.last_inst(cur).unwrap();
            match func.dfg.insts[t] {
                InstructionData::Jump { destination, .. } => {
                    let nb = destination.block(&func.dfg.value_lists);
                    if nb == oh {
                        olatch = Some(cur);
                        break;
                    }
                    pending_in = Some(destination);
                    cur = nb;
                }
                InstructionData::Brif { .. } => {
                    let d: Vec<BlockCall> = func.dfg.insts[t]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                        .to_vec();
                    let d0 = d[0].block(&func.dfg.value_lists);
                    let d1 = d[1].block(&func.dfg.value_lists);
                    if d0 == oh || d1 == oh {
                        olatch = Some(cur);
                        break;
                    }
                    let (cont_bc, exit_bc, epos) = if d0 == ih && !ibody.contains(&d1)
                        || (obody.contains(&d0) && !obody.contains(&d1))
                    {
                        (d[0], d[1], 1)
                    } else if d1 == ih && !ibody.contains(&d0)
                        || (obody.contains(&d1) && !obody.contains(&d0))
                    {
                        (d[1], d[0], 0)
                    } else {
                        bail!("nonlinear outer chain");
                    };
                    let cb = cont_bc.block(&func.dfg.value_lists);
                    let eb = exit_bc.block(&func.dfg.value_lists);
                    if !func.layout.is_cold(eb) && cb != ih {
                        bail!("outer side exit to warm block");
                    }
                    oexits.push(JamExit {
                        pos: opre.len() + opost.len() - 1,
                        cont_bc,
                        exit_bc,
                        tgt: eb,
                        epos,
                        corder: Vec::new(),
                        lvals: Vec::new(),
                        clone_root: None,
                    });
                    if cb == ih {
                        pending_in = Some(cont_bc);
                    }
                    cur = cb;
                }
                _ => bail!("unhandled outer terminator"),
            }
        }
    }
    let Some(olatch) = olatch else {
        bail!("no outer latch");
    };
    if !seen_inner {
        bail!("inner not on outer chain");
    }
    let (inner_pos, ientry_bc) = into_inner.unwrap();
    // Structural checks: outer chain + inner chain == outer body; ih's
    // only outside pred is the chain; post's preds are the inner exit
    // only.
    if obody.len() != ibody.len() + 1 + opre.len() + opost.len() {
        bail!("extra outer blocks");
    }
    for p in cfg.pred_iter(ih) {
        if !ibody.contains(&p.block) && p.block != opre.last().copied().unwrap_or(oh) {
            bail!("inner has extra entries");
        }
    }
    for p in cfg.pred_iter(post) {
        if !ibody.contains(&p.block) {
            bail!("post has extra preds");
        }
    }
    for &b in opre.iter().chain(opost.iter()) {
        let preds: Vec<_> = cfg.pred_iter(b).collect();
        if preds.len() != 1 {
            bail!("outer chain block has extra preds");
        }
    }
    // The latch's non-oh dest (brif latch) may exit the outer loop; a jump
    // latch is fine. oh params bound on the latch edge: used for the bound
    // invariance check below and cloned through J copies automatically.
    {
        let lt = func.layout.last_inst(olatch).unwrap();
        for bc in func.dfg.insts[lt]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
        {
            let d = bc.block(&func.dfg.value_lists);
            if d != oh && !func.layout.is_cold(d) {
                bail!("outer latch exits to warm block");
            }
        }
    }
    // All edge args must be plain values.
    let edge_args_ok = |bc: BlockCall| {
        bc.args(&func.dfg.value_lists)
            .all(|a| matches!(a, BlockArg::Value(_)))
    };
    for bc in [ientry_bc, ipost_bc]
        .into_iter()
        .chain(iexits.iter().flat_map(|e| [e.cont_bc, e.exit_bc]))
        .chain(oexits.iter().flat_map(|e| [e.cont_bc, e.exit_bc]))
        .chain([ocont_bc, oexit_bc])
    {
        if !edge_args_ok(bc) {
            bail!("non-value edge arg");
        }
    }
    // Outer step: the latch's binding for the iv param, traced through
    // pass-through inner params, must be `iv + s`.
    let latch_bc = {
        let lt = func.layout.last_inst(olatch).unwrap();
        func.dfg.insts[lt]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .copied()
            .find(|bc| bc.block(&func.dfg.value_lists) == oh)
            .unwrap()
    };
    let latched: Vec<Value> = latch_bc
        .args(&func.dfg.value_lists)
        .filter_map(|a| match a {
            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
            _ => None,
        })
        .collect();
    if latched.len() != ohparams.len() {
        bail!("outer latch arity");
    }
    // Trace `v` through pass-through block params: if v is a param of a
    // block whose only meaningful bindings copy it unchanged, look at the
    // value bound on the edge entering that block's "context" — for inner
    // params that's the entry edge (latch edges bind the param itself).
    let iv_next = {
        let mut v = latched[opidx];
        for _ in 0..16 {
            let resolved = func.dfg.resolve_aliases(v);
            if resolved != v {
                v = resolved;
                continue;
            }
            if let cranelift_codegen::ir::ValueDef::Param(b, q) = func.dfg.value_def(v) {
                if ibody.contains(&b) {
                    // Param of an inner block: pass-through requires the
                    // inner latch to bind it back to itself.
                    let mut passthrough = true;
                    for bc in func.dfg.insts[func.layout.last_inst(ilatch).unwrap()]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                    {
                        if bc.block(&func.dfg.value_lists) == b {
                            match bc.args(&func.dfg.value_lists).nth(q) {
                                Some(BlockArg::Value(a))
                                    if func.dfg.resolve_aliases(a) == v => {}
                                _ => passthrough = false,
                            }
                        }
                    }
                    if !passthrough {
                        break;
                    }
                    // Follow the edge that enters `b`'s context: for ih
                    // it's the entry edge; for other inner blocks it's
                    // the chain predecessor's cont/jump edge.
                    let mut next = None;
                    if b == ih {
                        next = ientry_bc
                            .args(&func.dfg.value_lists)
                            .nth(q)
                            .and_then(|a| match a {
                                BlockArg::Value(v) => Some(v),
                                _ => None,
                            });
                    } else {
                        // Find the chain predecessor's edge into b.
                        let pos = ichain.iter().position(|&x| x == b).unwrap();
                        let pb = ichain[pos - 1];
                        let pi = func.layout.last_inst(pb).unwrap();
                        for bc in func.dfg.insts[pi]
                            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                        {
                            if bc.block(&func.dfg.value_lists) == b {
                                next = bc.args(&func.dfg.value_lists).nth(q).and_then(|a| {
                                    match a {
                                        BlockArg::Value(v) => Some(v),
                                        _ => None,
                                    }
                                });
                            }
                        }
                    }
                    match next {
                        Some(nv) => {
                            v = nv;
                            continue;
                        }
                        None => break,
                    }
                }
            }
            break;
        }
        v
    };
    let Some((spidx, step)) = affine_param(func, oh, iv_next) else {
        bail!("outer step not affine");
    };
    if spidx != opidx || step <= 0 {
        bail!("outer step not affine");
    }
    // Bound used by the header guard: if it's an oh param it must be
    // passed back unchanged.
    if func.dfg.value_def(obound).inst().is_none()
        && let Some(q) = param_idx(func, oh, obound)
        && latched[q] != obound
    {
        bail!("bound mutates on backedge");
    }

    // ---- dep set: values that vary across the J tile (transitively
    //      dependent on the outer IV) ----
    let mut dep: FxHashSet<Value> = FxHashSet::default();
    dep.insert(ivp);
    loop {
        let mut changed = false;
        for &b in &obody {
            for i in func.layout.block_insts(b) {
                let any_dep = func
                    .dfg
                    .inst_values(i)
                    .any(|v| dep.contains(&func.dfg.resolve_aliases(v)));
                if any_dep {
                    for &r in func.dfg.inst_results(i) {
                        changed |= dep.insert(func.dfg.resolve_aliases(r));
                    }
                }
            }
            // Params: dep if any incoming edge binds a dep arg.
            for p in cfg.pred_iter(b) {
                for bc in func.dfg.insts[p.inst].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    if bc.block(&func.dfg.value_lists) != b {
                        continue;
                    }
                    for (q, a) in bc.args(&func.dfg.value_lists).enumerate() {
                        if let BlockArg::Value(a) = a
                            && dep.contains(&func.dfg.resolve_aliases(a))
                            && let Some(&pp) = func.dfg.block_params(b).get(q)
                        {
                            changed |= dep.insert(func.dfg.resolve_aliases(pp));
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    if dep.contains(&obound) {
        bail!("bound varies");
    }
    for (q, &p) in ohparams.iter().enumerate() {
        if q != opidx && dep.contains(&p) {
            bail!("non-iv header param varies");
        }
    }
    // The exit-to-post condition must be invariant (all copies exit the
    // fused inner loop together).
    {
        let exblk = ichain[ipos];
        let ti = func.layout.last_inst(exblk).unwrap();
        let InstructionData::Brif { arg: c, .. } = func.dfg.insts[ti] else {
            unreachable!()
        };
        if dep.contains(&func.dfg.resolve_aliases(c)) {
            bail!("inner exit cond varies");
        }
    }
    // ---- side exits: shared cone clones when the target's dominance cone
    //      references body values (punroll's shape: one cold target fed by
    //      per-copy edge args) ----
    let lvals_all: FxHashSet<Value> = {
        let mut s = FxHashSet::default();
        for &b in obody.iter() {
            for &p in func.dfg.block_params(b) {
                s.insert(func.dfg.resolve_aliases(p));
            }
            for i in func.layout.block_insts(b) {
                for &r in func.dfg.inst_results(i) {
                    s.insert(func.dfg.resolve_aliases(r));
                }
            }
        }
        s
    };
    for e in iexits.iter_mut().chain(oexits.iter_mut()) {
        let scone: Vec<Block> = func
            .layout
            .blocks()
            .filter(|&b| {
                !obody.contains(&b) && dt.is_reachable(b) && dt.block_dominates(e.tgt, b)
            })
            .collect();
        let mut lset: FxHashSet<Value> = FxHashSet::default();
        let mut bad = false;
        let mut sinsts = 0usize;
        for &cb in &scone {
            for i in func.layout.block_insts(cb) {
                sinsts += 1;
                for v in func.dfg.inst_values(i) {
                    let v = func.dfg.resolve_aliases(v);
                    if lvals_all.contains(&v) && lset.insert(v) {
                        e.lvals.push(v);
                    }
                }
                let op = func.dfg.insts[i].opcode();
                if matches!(
                    op,
                    Opcode::TryCall | Opcode::TryCallIndirect | Opcode::BrTable
                ) {
                    bad = true;
                }
                for bc in func.dfg.insts[i].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    for a in bc.args(&func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a {
                            let v = func.dfg.resolve_aliases(v);
                            if lvals_all.contains(&v) && lset.insert(v) {
                                e.lvals.push(v);
                            }
                        }
                    }
                    if obody.contains(&bc.block(&func.dfg.value_lists)) {
                        bail!("exit cone re-enters loop");
                    }
                }
            }
        }
        if !e.lvals.is_empty() && (bad || scone.len() > 8 || sinsts > 48) {
            bail!("side-exit cone too big");
        }
        e.corder = scone;
    }
    // Outside uses of body values must stay inside side-exit cones.
    {
        let mut coneblks: FxHashSet<Block> = FxHashSet::default();
        for e in iexits.iter().chain(oexits.iter()) {
            coneblks.extend(e.corder.iter().copied());
        }
        for b in func.layout.blocks() {
            if obody.contains(&b) || coneblks.contains(&b) || !dt.is_reachable(b) {
                continue;
            }
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    let v = func.dfg.resolve_aliases(v);
                    if lvals_all.contains(&v) && !dep.contains(&v) {
                        bail!("invariant body value used outside");
                    }
                    if lvals_all.contains(&v) {
                        bail!("body value used outside");
                    }
                }
                for bc in func.dfg.insts[i].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    for a in bc.args(&func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a {
                            let v = func.dfg.resolve_aliases(v);
                            if lvals_all.contains(&v) {
                                bail!("body value used outside");
                            }
                        }
                    }
                }
            }
        }
    }
    // Post-chain stores may not feed a later tile element's inner loads.
    // Heuristic: every post-chain store's base root must differ from every
    // inner load's base root (distinct SSA roots ⇒ no cross-tile memory
    // dependence for well-formed input).
    {
        let root_of = |func: &Function, mut v: Value| -> Option<Value> {
            for _ in 0..32 {
                v = func.dfg.resolve_aliases(v);
                let Some(i) = func.dfg.value_def(v).inst() else {
                    return Some(v);
                };
                match func.dfg.insts[i] {
                    InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        args,
                    } => {
                        // Follow the non-iconst operand.
                        let (a, b) = (args[0], args[1]);
                        if iconst(func, a).is_some() {
                            v = b;
                        } else {
                            v = a;
                        }
                    }
                    _ => return Some(v),
                }
            }
            None
        };
        let mut lbases: FxHashSet<Value> = FxHashSet::default();
        for &b in &ichain {
            for i in func.layout.block_insts(b) {
                if func.dfg.insts[i].opcode().can_load()
                    && let Some(&addr) = func.dfg.inst_args(i).first()
                    && let Some(r) = root_of(func, addr)
                {
                    lbases.insert(func.dfg.resolve_aliases(r));
                } else if func.dfg.insts[i].opcode().can_load() {
                    bail!("un analyzable inner load");
                }
            }
        }
        for &b in &opost {
            for i in func.layout.block_insts(b) {
                let op = func.dfg.insts[i].opcode();
                if op.can_store() || op.is_call() || op.other_side_effects() {
                    if op.can_store() {
                        let args = func.dfg.inst_args(i);
                        match args.last().and_then(|&a| root_of(func, a)) {
                            Some(r) if !lbases.contains(&func.dfg.resolve_aliases(r)) => {}
                            _ => bail!("post-store may feed inner loads"),
                        }
                    } else {
                        bail!("unclonable post inst");
                    }
                }
            }
        }
    }
    // Pre-chain insts must be pure or loads (cloned per copy).
    for &b in &opre {
        for i in func.layout.block_insts(b) {
            let op = func.dfg.insts[i].opcode();
            if op.is_call() || op.can_store() || op.other_side_effects() {
                bail!("unclonable pre inst");
            }
        }
    }

    // ---- emission ----
    // Value maps: `vmaps[d][v]` is copy d's image of v; `dep` values get
    // per-d images, the rest share one.
    let mut vmaps: Vec<FxHashMap<Value, Value>> = (0..j).map(|_| FxHashMap::default()).collect();

    // Side-exit cone clones (shared across all J copies; each copy's edge
    // binds the cone's loop-value params).
    for e in iexits.iter_mut().chain(oexits.iter_mut()) {
        if e.lvals.is_empty() {
            continue;
        }
        let cset: FxHashSet<Block> = e.corder.iter().copied().collect();
        e.corder = cone_rpo(func, e.tgt, &cset);
        let cold = func.layout.is_cold(e.tgt);
        let mut scmap: FxHashMap<Block, Block> = FxHashMap::default();
        for (bi, &b) in e.corder.clone().iter().enumerate() {
            let nb = func.dfg.make_block();
            for &p in func.dfg.block_params(b).to_vec().iter() {
                let ty = func.dfg.value_type(p);
                func.dfg.append_block_param(nb, ty);
            }
            if bi == 0 {
                for &lv in &e.lvals {
                    let ty = func.dfg.value_type(lv);
                    func.dfg.append_block_param(nb, ty);
                }
            }
            func.layout.insert_block(nb, oh);
            if cold && func.layout.is_cold(b) {
                func.layout.set_cold(nb);
            }
            scmap.insert(b, nb);
        }
        let root = scmap[&e.tgt];
        e.clone_root = Some(root);
        let orig_arity = func.dfg.block_params(e.tgt).len();
        let mut xmap: FxHashMap<Value, Value> = FxHashMap::default();
        for (i, &lv) in e.lvals.iter().enumerate() {
            xmap.insert(lv, func.dfg.block_params(root)[orig_arity + i]);
        }
        emit_cone_clone(func, &e.corder, &scmap, &mut xmap);
    }

    // ojh: jammed outer header — same params/insts as oh, but the guard
    // demands budget for all J tile elements: rem = bound - a0 > (J-1)*step.
    let mut jam_ok = None;
    let ojh = func.dfg.make_block();
    for &p in &ohparams {
        let ty = func.dfg.value_type(p);
        func.dfg.append_block_param(ojh, ty);
    }
    func.layout.insert_block(ojh, oh);
    let ojhparams = func.dfg.block_params(ojh).to_vec();
    for d in 0..j {
        for (q, &p) in ohparams.iter().enumerate() {
            vmaps[d].insert(func.dfg.resolve_aliases(p), ojhparams[q]);
        }
    }
    {
        // Clone oh's non-terminator insts verbatim (incl. its consts).
        let insts: Vec<Inst> = func.layout.block_insts(oh).collect();
        let mut omap: FxHashMap<Value, Value> = FxHashMap::default();
        for (q, &p) in ohparams.iter().enumerate() {
            omap.insert(func.dfg.resolve_aliases(p), ojhparams[q]);
        }
        let nbmap: FxHashMap<Block, Block> = FxHashMap::default();
        for &ii in &insts[..insts.len() - 1] {
            if ii == gci {
                continue; // the counting icmp is replaced by the jam guard
            }
            let src = func.dfg.insts[ii];
            let data = {
                let mut m = Vm {
                    func,
                    vmap: &omap,
                    bmap: &nbmap,
                };
                src.map(&mut m)
            };
            let ni = func.dfg.make_inst(data);
            let ctv = func.dfg.ctrl_typevar(ii);
            func.dfg.make_inst_results(ni, ctv);
            func.layout.append_inst(ni, ojh);
            for (&o, &nv) in func
                .dfg
                .inst_results(ii)
                .iter()
                .zip(func.dfg.inst_results(ni).iter())
            {
                omap.insert(func.dfg.resolve_aliases(o), nv);
            }
        }
        for d in 0..j {
            for (&o, &n) in omap.iter() {
                vmaps[d].insert(o, n);
            }
        }
        // Jam guard: all J tile entries must pass `a0 + i*step cc bound`,
        // i.e. the last: (bound - a0) > (J-1)*step — punroll's widened
        // budget test.
        let ity = func.dfg.value_type(ivp);
        let wty = if ity.bits() <= 32 { types::I64 } else { ity };
        let mut pos = FuncCursor::new(func).at_bottom(ojh);
        let boundv = omap
            .get(&obound)
            .copied()
            .unwrap_or_else(|| *vmaps[0].get(&obound).unwrap_or(&obound));
        let a0 = ojhparams[opidx];
        let cc = match (osigned, ole) {
            (true, true) => IntCC::SignedLessThanOrEqual,
            (true, false) => IntCC::SignedLessThan,
            (false, true) => IntCC::UnsignedLessThanOrEqual,
            (false, false) => IntCC::UnsignedLessThan,
        };
        let ok1 = pos.ins().icmp(cc, a0, boundv);
        let (a0w, bw) = if wty != ity && osigned {
            (pos.ins().sextend(wty, a0), pos.ins().sextend(wty, boundv))
        } else if wty != ity {
            (pos.ins().uextend(wty, a0), pos.ins().uextend(wty, boundv))
        } else {
            (a0, boundv)
        };
        let rem = pos.ins().isub(bw, a0w);
        let lim = pos.ins().iconst(wty, (j as i64 - 1) * step);
        let ok2 = pos.ins().icmp(
            if ole {
                IntCC::UnsignedGreaterThanOrEqual
            } else {
                IntCC::UnsignedGreaterThan
            },
            rem,
            lim,
        );
        let ok = pos.ins().band(ok1, ok2);
        // The guard brif is emitted once `pre_0` exists.
        jam_ok = Some(ok);
    }

    // ---- create fused/cloned blocks ----
    // Pre chain: a full clone per copy (params 1:1; for empty pre chain one
    // fresh entry block per copy).
    let mut preblk: Vec<Vec<Block>> = Vec::new();
    for _ in 0..j {
        let mut row = Vec::new();
        if opre.is_empty() {
            let nb = func.dfg.make_block();
            func.layout.insert_block(nb, oh);
            row.push(nb);
        }
        for &b in &opre {
            let nb = func.dfg.make_block();
            for &p in func.dfg.block_params(b).to_vec().iter() {
                let ty = func.dfg.value_type(p);
                func.dfg.append_block_param(nb, ty);
            }
            func.layout.insert_block(nb, oh);
            row.push(nb);
        }
        preblk.push(row);
    }
    // Fused inner blocks: `dep` params expand to J slots; `poff[b][q]`
    // records (first slot index, is_dep).
    let mut fblk: FxHashMap<Block, Block> = FxHashMap::default();
    let mut poff: FxHashMap<Block, Vec<(usize, bool)>> = FxHashMap::default();
    for &b in &ichain {
        let fb = func.dfg.make_block();
        let mut offs = Vec::new();
        for &p in func.dfg.block_params(b).to_vec().iter() {
            let ty = func.dfg.value_type(p);
            let isdep = dep.contains(&func.dfg.resolve_aliases(p));
            offs.push((func.dfg.block_params(fb).len(), isdep));
            for _ in 0..if isdep { j } else { 1 } {
                func.dfg.append_block_param(fb, ty);
            }
        }
        poff.insert(b, offs);
        func.layout.insert_block(fb, oh);
        fblk.insert(b, fb);
    }
    for &b in &ichain {
        let fbp = func.dfg.block_params(fblk[&b]).to_vec();
        for (q, &p) in func.dfg.block_params(b).to_vec().iter().enumerate() {
            let (off, isdep) = poff[&b][q];
            let p = func.dfg.resolve_aliases(p);
            for d in 0..j {
                vmaps[d].insert(p, fbp[off + if isdep { d } else { 0 }]);
            }
        }
    }
    // Tails: a full clone per copy of the post chain.
    let mut tailblk: Vec<Vec<Block>> = Vec::new();
    for _ in 0..j {
        let mut row = Vec::new();
        for &b in &opost {
            let nb = func.dfg.make_block();
            for &p in func.dfg.block_params(b).to_vec().iter() {
                let ty = func.dfg.value_type(p);
                func.dfg.append_block_param(nb, ty);
            }
            func.layout.insert_block(nb, oh);
            row.push(nb);
        }
        tailblk.push(row);
    }
    let fih = fblk[&ih];
    let ity = func.dfg.value_type(ivp);

    // j_d at the top of each copy's first pre block (d>0; d==0 uses the
    // header param directly, already seeded in `vmaps`).
    for d in 1..j {
        let mut pos = FuncCursor::new(func).at_first_insertion_point(preblk[d][0]);
        let dc = pos.ins().iconst(ity, d as i64);
        let jd = pos.ins().iadd(ojhparams[opidx], dc);
        vmaps[d].insert(func.dfg.resolve_aliases(ivp), jd);
    }

    let ex_tgt = |e: &JamExit| e.clone_root.unwrap_or(e.tgt);

    // ---- pre chain emission ----
    // The into-inner edge sits on `opre.last()` (or oh for empty pre).
    for d in 0..j {
        for (pi, &b) in opre.iter().enumerate() {
            let nb = preblk[d][pi];
            for (q, &p) in func.dfg.block_params(b).to_vec().iter().enumerate() {
                vmaps[d].insert(
                    func.dfg.resolve_aliases(p),
                    func.dfg.block_params(nb)[q],
                );
            }
            jam_emit_flat(func, b, nb, d, &mut vmaps);
            let t = func.layout.last_inst(b).unwrap();
            let is_into = inner_pos > 0 && pi == opre.len() - 1;
            match func.dfg.insts[t] {
                InstructionData::Jump { .. } => {
                    // Either the into-inner edge (last pre block) or a
                    // chain-internal jump.
                    if is_into {
                        // Flow continues to the next copy's pre chain; the
                        // final copy's entry into the fused inner loop is
                        // emitted after this loop.
                        if d + 1 < j {
                            let args: Vec<BlockArg> =
                                jam_flat_args(func, d + 1, ocont_bc, &vmaps);
                            FuncCursor::new(func)
                                .at_bottom(nb)
                                .ins()
                                .jump(preblk[d + 1][0], &args);
                        }
                    } else {
                        let bc = func.dfg.insts[t].branch_destination(
                            &func.dfg.jump_tables,
                            &func.dfg.exception_tables,
                        )[0];
                        let args = jam_flat_args(func, d, bc, &vmaps);
                        FuncCursor::new(func)
                            .at_bottom(nb)
                            .ins()
                            .jump(preblk[d][pi + 1], &args);
                    }
                }
                InstructionData::Brif { arg: c, .. } => {
                    let c = func.dfg.resolve_aliases(c);
                    let cm = *vmaps[d].get(&c).unwrap_or(&c);
                    let e = oexits
                        .iter()
                        .find(|e| opre.get(e.pos).copied() == Some(b) && e.pos == pi)
                        .expect("jam: pre brif without exit");
                    let cont_t = if is_into {
                        // cont is the into-inner edge: the check gates
                        // inner entry for this copy — continue to the next
                        // copy's pre chain (or fih for the last).
                        if d + 1 < j { preblk[d + 1][0] } else { fih }
                    } else {
                        preblk[d][pi + 1]
                    };
                    let cargs: Vec<BlockArg> = if is_into && d + 1 < j {
                        jam_flat_args(func, d + 1, ocont_bc, &vmaps)
                    } else {
                        jam_flat_args(func, d, e.cont_bc, &vmaps)
                    };
                    let eargs = jam_exit_args(func, d, e, &vmaps);
                    let etgt = ex_tgt(e);
                    let mut pos = FuncCursor::new(func).at_bottom(nb);
                    if e.epos == 0 {
                        pos.ins().brif(cm, etgt, &eargs, cont_t, &cargs);
                    } else {
                        pos.ins().brif(cm, cont_t, &cargs, etgt, &eargs);
                    }
                }
                _ => unreachable!("jam: pre terminator"),
            }
        }
        // Empty pre chain: the fresh entry block just jumps on.
        if opre.is_empty() && d + 1 < j {
            FuncCursor::new(func)
                .at_bottom(preblk[d][0])
                .ins()
                .jump(preblk[d + 1][0], &[]);
        }
    }
    // Final copy enters the fused inner loop from its last pre block.
    {
        let nb = *preblk[j - 1].last().unwrap();
        let args = jam_fused_args(func, j, ientry_bc, ih, &vmaps, &poff);
        FuncCursor::new(func).at_bottom(nb).ins().jump(fih, &args);
    }

    // ---- fused inner emission ----
    for (ci, &b) in ichain.iter().enumerate() {
        let fb = fblk[&b];
        jam_emit_fused(func, b, fb, j, &dep, &mut vmaps);
        let t = func.layout.last_inst(b).unwrap();
        match func.dfg.insts[t] {
            InstructionData::Jump { .. } => {
                let bc = func.dfg.insts[t]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[0];
                let nb = bc.block(&func.dfg.value_lists);
                let args = jam_fused_args(func, j, bc, nb, &vmaps, &poff);
                FuncCursor::new(func)
                    .at_bottom(fb)
                    .ins()
                    .jump(fblk[&nb], &args);
            }
            InstructionData::Brif { arg: c, .. } => {
                let c = func.dfg.resolve_aliases(c);
                if ci == ipos {
                    // The normal inner exit: invariant cond, cont fused,
                    // exit to the first tail.
                    let cm = *vmaps[0].get(&c).unwrap_or(&c);
                    let (cont_bc, post_bc, epos) = {
                        let dests: Vec<BlockCall> = func.dfg.insts[t]
                            .branch_destination(
                                &func.dfg.jump_tables,
                                &func.dfg.exception_tables,
                            )
                            .to_vec();
                        if dests[0].block(&func.dfg.value_lists) == post {
                            (dests[1], dests[0], 0)
                        } else {
                            (dests[0], dests[1], 1)
                        }
                    };
                    let cblk = fblk[&cont_bc.block(&func.dfg.value_lists)];
                    let cargs = jam_fused_args(
                        func, j,
                        cont_bc,
                        cont_bc.block(&func.dfg.value_lists),
                        &vmaps,
                        &poff,
                    );
                    let pargs = jam_flat_args(func, 0, post_bc, &vmaps);
                    let mut pos = FuncCursor::new(func).at_bottom(fb);
                    if epos == 0 {
                        pos.ins().brif(cm, tailblk[0][0], &pargs, cblk, &cargs);
                    } else {
                        pos.ins().brif(cm, cblk, &cargs, tailblk[0][0], &pargs);
                    }
                } else {
                    let e = iexits
                        .iter()
                        .find(|e| ichain[e.pos] == b)
                        .expect("jam: inner brif without exit");
                    let cblk = fblk[&e.cont_bc.block(&func.dfg.value_lists)];
                    if dep.contains(&c) {
                        // Variant check: J sequential mini-brifs.
                        let mut cur = fb;
                        for d in 0..j {
                            let cm = *vmaps[d].get(&c).unwrap_or(&c);
                            let eargs = jam_exit_args(func, d, e, &vmaps);
                            let etgt = ex_tgt(e);
                            let is_last = d + 1 == j;
                            let (cargs, nxt) = if is_last {
                                (
                                    jam_fused_args(
                                        func, j,
                                        e.cont_bc,
                                        e.cont_bc.block(&func.dfg.value_lists),
                                        &vmaps,
                                        &poff,
                                    ),
                                    cblk,
                                )
                            } else {
                                (Vec::new(), {
                                    let mb = func.dfg.make_block();
                                    func.layout.insert_block(mb, oh);
                                    mb
                                })
                            };
                            let mut pos = FuncCursor::new(func).at_bottom(cur);
                            if e.epos == 0 {
                                pos.ins().brif(cm, etgt, &eargs, nxt, &cargs);
                            } else {
                                pos.ins().brif(cm, nxt, &cargs, etgt, &eargs);
                            }
                            cur = nxt;
                        }
                    } else {
                        let cm = *vmaps[0].get(&c).unwrap_or(&c);
                        let cargs = jam_fused_args(
                            func, j,
                            e.cont_bc,
                            e.cont_bc.block(&func.dfg.value_lists),
                            &vmaps,
                            &poff,
                        );
                        let eargs = jam_exit_args(func, 0, e, &vmaps);
                        let etgt = ex_tgt(e);
                        let mut pos = FuncCursor::new(func).at_bottom(fb);
                        if e.epos == 0 {
                            pos.ins().brif(cm, etgt, &eargs, cblk, &cargs);
                        } else {
                            pos.ins().brif(cm, cblk, &cargs, etgt, &eargs);
                        }
                    }
                }
            }
            _ => unreachable!("jam: inner terminator"),
        }
    }
    // The inner latch may itself be the post-exit brif — handled above via
    // `ipos`; a jump latch already jumped to fih through the Jump arm.
    let _ = ilatch;

    // ---- tail emission ----
    for d in 0..j {
        for (pi, &b) in opost.iter().enumerate() {
            let nb = tailblk[d][pi];
            for (q, &p) in func.dfg.block_params(b).to_vec().iter().enumerate() {
                vmaps[d].insert(
                    func.dfg.resolve_aliases(p),
                    func.dfg.block_params(nb)[q],
                );
            }
            jam_emit_flat(func, b, nb, d, &mut vmaps);
            let t = func.layout.last_inst(b).unwrap();
            match func.dfg.insts[t] {
                InstructionData::Jump { destination, .. } => {
                    let db = destination.block(&func.dfg.value_lists);
                    if db == oh {
                        // Outer latch: intermediate copies fall into the
                        // next tail; the last loops back to ojh.
                        let (tgt, args): (Block, Vec<BlockArg>) = if d + 1 < j {
                            (
                                tailblk[d + 1][0],
                                jam_flat_args(func, d + 1, ipost_bc, &vmaps),
                            )
                        } else {
                            (ojh, jam_flat_args(func, d, destination, &vmaps))
                        };
                        FuncCursor::new(func)
                            .at_bottom(nb)
                            .ins()
                            .jump(tgt, &args);
                    } else {
                        let args = jam_flat_args(func, d, destination, &vmaps);
                        FuncCursor::new(func)
                            .at_bottom(nb)
                            .ins()
                            .jump(tailblk[d][pi + 1], &args);
                    }
                }
                InstructionData::Brif { arg: c, .. } => {
                    let c = func.dfg.resolve_aliases(c);
                    let cm = *vmaps[d].get(&c).unwrap_or(&c);
                    let dests: Vec<BlockCall> = func.dfg.insts[t]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                        .to_vec();
                    let (d0, d1) = (
                        dests[0].block(&func.dfg.value_lists),
                        dests[1].block(&func.dfg.value_lists),
                    );
                    if d0 == oh || d1 == oh {
                        // Outer latch brif.
                        let (oh_bc, out_bc, opos) = if d0 == oh {
                            (dests[0], dests[1], 0)
                        } else {
                            (dests[1], dests[0], 1)
                        };
                        let (otgt, oargs): (Block, Vec<BlockArg>) = if d + 1 < j {
                            (tailblk[d + 1][0], jam_flat_args(func, d + 1, ipost_bc, &vmaps))
                        } else {
                            (ojh, jam_flat_args(func, d, oh_bc, &vmaps))
                        };
                        let xargs = jam_flat_args(func, d, out_bc, &vmaps);
                        let xtgt = out_bc.block(&func.dfg.value_lists);
                        let mut pos = FuncCursor::new(func).at_bottom(nb);
                        if opos == 0 {
                            pos.ins().brif(cm, otgt, &oargs, xtgt, &xargs);
                        } else {
                            pos.ins().brif(cm, xtgt, &xargs, otgt, &oargs);
                        }
                    } else {
                        let e = oexits
                            .iter()
                            .find(|e| {
                                let pos = e.pos;
                                pos >= opre.len()
                                    && opost.get(pos - opre.len()).copied() == Some(b)
                                    && pos - opre.len() == pi
                            })
                            .expect("jam: post brif without exit");
                        let cargs = jam_flat_args(func, d, e.cont_bc, &vmaps);
                        let eargs = jam_exit_args(func, d, e, &vmaps);
                        let etgt = ex_tgt(e);
                        let ctgt = tailblk[d][pi + 1];
                        let mut pos = FuncCursor::new(func).at_bottom(nb);
                        if e.epos == 0 {
                            pos.ins().brif(cm, etgt, &eargs, ctgt, &cargs);
                        } else {
                            pos.ins().brif(cm, ctgt, &cargs, etgt, &eargs);
                        }
                    }
                }
                _ => unreachable!("jam: post terminator"),
            }
        }
    }

    // ---- ojh guard brif + entry redirect ----
    {
        let ok = jam_ok.unwrap();
        let cargs = jam_flat_args(func, 0, ocont_bc, &vmaps);
        let rargs: Vec<BlockArg> = ojhparams.iter().map(|&v| v.into()).collect();
        FuncCursor::new(func)
            .at_bottom(ojh)
            .ins()
            .brif(ok, preblk[0][0], &cargs, oh, &rargs);
    }
    let entries: Vec<(Block, Inst)> = cfg
        .pred_iter(oh)
        .filter(|p| !obody.contains(&p.block) && p.block != ojh)
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
            if bc.block(&dfg.value_lists) == oh {
                let args: Vec<BlockArg> = bc.args(&dfg.value_lists).collect();
                *bc = BlockCall::new(ojh, args.iter().copied(), &mut dfg.value_lists);
            }
        }
    }
    if debug() {
        eprintln!("jam {name} block{}: jammed x{j}", oh.as_u32());
    }
    Some(vec![ojh])
}

/// One inner-loop candidate of a chooser pair: a linear chain with cold
/// side exits, one normal exit continuing the outer chain, and a latch
/// (jump-back or brif-back) whose other dest is that same exit.
struct JamCand {
    ih: Block,
    ibody: FxHashSet<Block>,
    chain: Vec<Block>,
    iexits: Vec<JamExit>,
    /// Chain index of the block bearing the normal exit edge.
    ipos: usize,
    /// In-body edge of the exit-bearing brif, or the latch's backedge.
    #[allow(dead_code)]
    ipcont: BlockCall,
    /// The exit edge into the outer chain.
    post_bc: BlockCall,
    /// Which brif dest is the exit (for arg-order fidelity).
    #[allow(dead_code)]
    ipepos: usize,
    post: Block,
    ilatch: Block,
}

/// Follow `start` through non-inner outer-body blocks until an inner
/// header is reached — a "lane" such as a peeled trip-count test between
/// the chooser and its inner loop. Returns `(lane blocks, which inner,
/// entry edge into the header, side exits)`. Each lane block continues
/// linearly to the next lane block or the header; any other dest is
/// recorded as a side exit `(lane pos, edge, target)` and validated by
/// the caller (the fused inner's `post` for a zero-trip skip, or cold).
fn jam_lane(
    func: &Function,
    ihs: &[Block],
    ibodies: &[FxHashSet<Block>],
    obody: &FxHashSet<Block>,
    start: Block,
) -> Option<(Vec<Block>, usize, BlockCall, Vec<(usize, BlockCall, Block)>)> {
    if !obody.contains(&start) || ibodies.iter().any(|ib| ib.contains(&start)) {
        return None;
    }
    let mut blocks: Vec<Block> = Vec::new();
    let mut exits: Vec<(usize, BlockCall, Block)> = Vec::new();
    let mut cur = start;
    loop {
        let pos = blocks.len();
        let t = func.layout.last_inst(cur)?;
        match func.dfg.insts[t] {
            InstructionData::Jump { destination, .. } => {
                let nb = destination.block(&func.dfg.value_lists);
                if let Some(cand) = ihs.iter().position(|&h| h == nb) {
                    return Some((blocks, cand, destination, exits));
                }
                if !obody.contains(&nb) || ibodies.iter().any(|ib| ib.contains(&nb)) {
                    return None;
                }
                blocks.push(cur);
                cur = nb;
            }
            InstructionData::Brif { .. } => {
                let d: Vec<BlockCall> = func.dfg.insts[t]
                    .branch_destination(
                        &func.dfg.jump_tables,
                        &func.dfg.exception_tables,
                    )
                    .to_vec();
                let mut entry: Option<(BlockCall, usize)> = None;
                let mut body: Option<BlockCall> = None;
                for bc in d {
                    let db = bc.block(&func.dfg.value_lists);
                    if let Some(cand) = ihs.iter().position(|&h| h == db) {
                        if entry.is_some() {
                            return None;
                        }
                        entry = Some((bc, cand));
                    } else if ibodies.iter().any(|ib| ib.contains(&db)) {
                        return None;
                    } else if obody.contains(&db) {
                        if body.is_some() {
                            return None;
                        }
                        body = Some(bc);
                    } else {
                        exits.push((pos, bc, db));
                    }
                }
                blocks.push(cur);
                match (entry, body) {
                    // Reached the inner header; an in-body second dest is a
                    // zero-trip skip to the inner's post block.
                    (Some((bc, cand)), ob) => {
                        if let Some(bc) = ob {
                            exits.push((pos, bc, bc.block(&func.dfg.value_lists)));
                        }
                        return Some((blocks, cand, bc, exits));
                    }
                    (None, Some(bc)) => cur = bc.block(&func.dfg.value_lists),
                    (None, None) => return None,
                }
            }
            _ => return None,
        }
        if blocks.len() > 4 {
            return None;
        }
    }
}

/// Twin-inner variant of [`run_jam`]: the outer body reaches two sibling
/// inner loops through one `brif` chooser (e.g. a hoisted-check fast path
/// vs. its checked fallback — bcheck's widened-guard shape). Each chooser
/// dest may feed its inner through a short lane (a peeled trip-count
/// test) that can also skip straight to the inner's post block or bail
/// out cold. A fused tile evaluates the chooser condition for all J
/// copies up front; when every copy selects the same inner loop the tile
/// runs that inner (and its lane) fused with J lanes, and any
/// disagreement drops the rest of the outer loop to the original
/// remainder — so the fused path needs only ONE of the twins. The
/// counting test may sit on the outer latch (rotated form) or on the
/// outer header itself (header-tested form); a fused latch falls back
/// through the jammed header's guard, which re-checks the next tile.
fn run_jam_chooser(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    dt: &DominatorTree,
    la: &LoopAnalysis,
    olp: Loop,
    oh: Block,
    name: &str,
) -> Option<Vec<Block>> {
    macro_rules! bail {
        ($why:expr) => {{
            if debug() {
                eprintln!("jam2 {name} block{}: {}", oh.as_u32(), $why);
            }
            return None;
        }};
    }
    let j = JAM;
    if !dt.is_reachable(oh) || func.layout.is_cold(oh) {
        bail!("header unreachable/cold");
    }
    let inners: Vec<Loop> = la
        .loops()
        .filter(|&c| la.loop_parent(c) == Some(olp))
        .collect();
    if inners.len() != 2 {
        bail!("not two inner loops");
    }
    for &ilp in &inners {
        if la.loops().any(|c| la.loop_parent(c) == Some(ilp)) {
            bail!("inner not innermost");
        }
    }
    let obody: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, olp))
        .collect();
    let ibodies: Vec<FxHashSet<Block>> = inners
        .iter()
        .map(|&ilp| {
            func.layout
                .blocks()
                .filter(|&b| la.is_in_loop(b, ilp))
                .collect()
        })
        .collect();
    let ihs: Vec<Block> = inners.iter().map(|&ilp| la.loop_header(ilp)).collect();

    // ---- inner chain walks (linear chain; jump- or brif-latch whose
    //      non-backedge dest is the single normal exit into obody) ----
    let mut cands: Vec<JamCand> = Vec::new();
    for (ci, &ih) in ihs.iter().enumerate() {
        let ib = &ibodies[ci];
        let other_ib = &ibodies[1 - ci];
        let mut chain = vec![ih];
        let mut iexits: Vec<JamExit> = Vec::new();
        let mut post: Option<(usize, BlockCall, BlockCall, usize)> = None;
        let ilatch;
        macro_rules! ifail {
            ($why:expr) => {{
                if debug() {
                    eprintln!("jam2 {name} block{}: inner{} {}", oh.as_u32(), ci, $why);
                }
                return None;
            }};
        }
        loop {
            let cur = *chain.last().unwrap();
            if func.layout.is_cold(cur) {
                ifail!("cold block");
            }
            let t = func.layout.last_inst(cur)?;
            match func.dfg.insts[t] {
                InstructionData::Jump { destination, .. } => {
                    let nb = destination.block(&func.dfg.value_lists);
                    if nb == ih {
                        ilatch = cur;
                        break;
                    }
                    if !ib.contains(&nb) || chain.contains(&nb) {
                        ifail!("nonlinear");
                    }
                    chain.push(nb);
                }
                InstructionData::Brif { .. } => {
                    let d: Vec<BlockCall> = func.dfg.insts[t]
                        .branch_destination(
                            &func.dfg.jump_tables,
                            &func.dfg.exception_tables,
                        )
                        .to_vec();
                    let d0 = d[0].block(&func.dfg.value_lists);
                    let d1 = d[1].block(&func.dfg.value_lists);
                    if d0 == ih || d1 == ih {
                        // Latch: the other dest must be the normal exit.
                        let (cont_bc, exit_bc, epos) =
                            if d0 == ih { (d[0], d[1], 1) } else { (d[1], d[0], 0) };
                        let eb = exit_bc.block(&func.dfg.value_lists);
                        if post.is_some() || !obody.contains(&eb) || other_ib.contains(&eb) {
                            ifail!("bad latch exit");
                        }
                        post = Some((chain.len() - 1, cont_bc, exit_bc, epos));
                        ilatch = cur;
                        break;
                    }
                    let (cont_bc, exit_bc, epos) = if ib.contains(&d0) && !ib.contains(&d1) {
                        (d[0], d[1], 1)
                    } else if ib.contains(&d1) && !ib.contains(&d0) {
                        (d[1], d[0], 0)
                    } else {
                        ifail!("nonlinear");
                    };
                    let cb = cont_bc.block(&func.dfg.value_lists);
                    let eb = exit_bc.block(&func.dfg.value_lists);
                    if chain.contains(&cb) {
                        ifail!("nonlinear");
                    }
                    if obody.contains(&eb) && !other_ib.contains(&eb) {
                        if post.is_some() {
                            ifail!("multiple exits");
                        }
                        post = Some((chain.len() - 1, cont_bc, exit_bc, epos));
                    } else {
                        if !func.layout.is_cold(eb) {
                            ifail!("warm side exit");
                        }
                        iexits.push(JamExit {
                            pos: chain.len() - 1,
                            cont_bc,
                            exit_bc,
                            tgt: eb,
                            epos,
                            corder: Vec::new(),
                            lvals: Vec::new(),
                            clone_root: None,
                        });
                    }
                    chain.push(cb);
                }
                _ => ifail!("unhandled terminator"),
            }
            if chain.len() > MAX_CHAIN {
                ifail!("chain too long");
            }
        }
        let Some((ipos, ipcont, post_bc, ipepos)) = post else {
            ifail!("no exit");
        };
        let postb = post_bc.block(&func.dfg.value_lists);
        if ib.len() != chain.len() {
            ifail!("extra inner blocks");
        }
        let mut n_inner = 0usize;
        for &b in &chain {
            let insts: Vec<Inst> = func.layout.block_insts(b).collect();
            for &i in &insts[..insts.len() - 1] {
                n_inner += 1;
                let op = func.dfg.insts[i].opcode();
                if op.is_terminator()
                    || op.is_branch()
                    || op.is_call()
                    || op.can_store()
                    || op.other_side_effects()
                {
                    ifail!("unclonable inst");
                }
                if func
                    .dfg
                    .inst_args(i)
                    .iter()
                    .chain(func.dfg.inst_results(i).iter())
                    .any(|&v| func.dfg.value_type(v).is_vector())
                {
                    ifail!("vector inst");
                }
            }
        }
        if n_inner > MAX_JAM_BODY {
            ifail!("body too big");
        }
        cands.push(JamCand {
            ih,
            ibody: ib.clone(),
            chain,
            iexits,
            ipos,
            ipcont,
            post_bc,
            ipepos,
            post: postb,
            ilatch,
        });
    }

    // ---- outer chain walk: oh -> pre* -> CHOOSER -> lane* -> inner ->
    //      post* -> latch. The chooser is a brif whose dests lead to the
    //      two inner loops (through lanes); the fused inner is whichever
    //      cand validates (prefer the dest[0] lane — "check passed" —
    //      ties to fewer exits). ----
    let mut opre: Vec<Block> = Vec::new();
    let mut opost: Vec<Block> = Vec::new();
    let mut oexits: Vec<JamExit> = Vec::new();
    // Cold exits off the fused lane, and zero-trip lane skips to `post`.
    let mut lexits: Vec<JamExit> = Vec::new();
    let mut lane_post: Vec<(usize, BlockCall)> = Vec::new();
    // (chooser opre index, cond value, fused lane is dest[0])
    let mut chooser: Option<(usize, Value, bool)> = None;
    let mut ientry_bc: Option<BlockCall> = None;
    let mut fused: usize = usize::MAX;
    let mut into_bc: Option<BlockCall> = None; // edge entering the chooser
    let mut lane_bc: Option<BlockCall> = None; // chooser edge into flane[0]/ih
    let mut flane: Vec<Block> = Vec::new();
    let mut pre_entry_bc: Option<BlockCall> = None; // edge into opre[0]
    let mut olatch: Option<Block> = None;
    let mut oexit_bc: Option<BlockCall> = None;
    let mut seen_inner = false;
    {
        let mut cur = oh;
        let mut pending: Option<BlockCall> = None;
        loop {
            let t = func.layout.last_inst(cur)?;
            match func.dfg.insts[t] {
                InstructionData::Jump { destination, .. } => {
                    let nb = destination.block(&func.dfg.value_lists);
                    if nb == oh {
                        if !seen_inner {
                            bail!("outer latch before inner");
                        }
                        if cur != oh {
                            opost.push(cur);
                        }
                        olatch = Some(cur);
                        break;
                    }
                    if ibodies[0].contains(&nb) || ibodies[1].contains(&nb) {
                        bail!("mid-inner entry");
                    }
                    if !obody.contains(&nb) {
                        bail!("jump out of outer body");
                    }
                    if cur != oh {
                        if seen_inner {
                            opost.push(cur);
                        } else {
                            if opre.is_empty() {
                                pre_entry_bc = pending;
                            }
                            opre.push(cur);
                        }
                    }
                    pending = Some(destination);
                    cur = nb;
                }
                InstructionData::Brif { arg: c, .. } => {
                    let d: Vec<BlockCall> = func.dfg.insts[t]
                        .branch_destination(
                            &func.dfg.jump_tables,
                            &func.dfg.exception_tables,
                        )
                        .to_vec();
                    let d0 = d[0].block(&func.dfg.value_lists);
                    let d1 = d[1].block(&func.dfg.value_lists);
                    if d0 == oh || d1 == oh {
                        if !seen_inner {
                            bail!("outer latch before inner");
                        }
                        if cur != oh {
                            opost.push(cur);
                        }
                        olatch = Some(cur);
                        oexit_bc = Some(if d0 == oh { d[1] } else { d[0] });
                        break;
                    }
                    // Chooser: each dest is an inner header directly or a
                    // lane reaching one; the lanes must differ.
                    let lane_of = |db: Block, bc: BlockCall| {
                        if let Some(ci) = ihs.iter().position(|&h| h == db) {
                            Some((Vec::new(), ci, bc, Vec::new()))
                        } else {
                            jam_lane(func, &ihs, &ibodies, &obody, db)
                        }
                    };
                    let l0 = lane_of(d0, d[0]);
                    let l1 = lane_of(d1, d[1]);
                    if let (Some(la), Some(lb)) = (&l0, &l1)
                        && la.1 != lb.1
                        && !seen_inner
                        && cur != oh
                    {
                        // Fuse the lane with fewer cold exits (the
                        // widened-check fast path). `fused_on_d0`
                        // records whether the fused lane is the
                        // cond-true dest.
                        let fc = if cands[la.1].iexits.len() <= cands[lb.1].iexits.len() {
                            la.1
                        } else {
                            lb.1
                        };
                        let (lf, fbc, fd0) = if fc == la.1 {
                            (la, d[0], true)
                        } else {
                            (lb, d[1], false)
                        };
                        // Lane side exits: a skip must reach the fused
                        // inner's post; anything else must be cold.
                        for &(lp, ebc, etgt) in &lf.3 {
                            if etgt == cands[fc].post {
                                lane_post.push((lp, ebc));
                                continue;
                            }
                            if !func.layout.is_cold(etgt) {
                                bail!("warm lane exit");
                            }
                            let lt2 = func.layout.last_inst(lf.0[lp]).unwrap();
                            let ld: Vec<BlockCall> = func.dfg.insts[lt2]
                                .branch_destination(
                                    &func.dfg.jump_tables,
                                    &func.dfg.exception_tables,
                                )
                                .to_vec();
                            let (cbc, epos) =
                                if ld[0].block(&func.dfg.value_lists) == etgt {
                                    (ld[1], 0)
                                } else {
                                    (ld[0], 1)
                                };
                            lexits.push(JamExit {
                                pos: lp,
                                cont_bc: cbc,
                                exit_bc: ebc,
                                tgt: etgt,
                                epos,
                                corder: Vec::new(),
                                lvals: Vec::new(),
                                clone_root: None,
                            });
                        }
                        if opre.is_empty() {
                            pre_entry_bc = pending;
                        }
                        chooser = Some((opre.len(), func.dfg.resolve_aliases(c), fd0));
                        fused = fc;
                        ientry_bc = Some(lf.2);
                        lane_bc = Some(fbc);
                        flane = lf.0.clone();
                        into_bc = pending;
                        opre.push(cur);
                        seen_inner = true;
                        cur = cands[fused].post;
                        continue;
                    }
                    if l0.is_some() || l1.is_some() {
                        bail!("partial inner entry");
                    }
                    if cur == oh {
                        // Header-tested form: oh's brif is the counting
                        // test — in-body dest continues, the other is the
                        // loop exit (handled by the original remainder).
                        let cont_bc = if obody.contains(&d0)
                            && !obody.contains(&d1)
                            && !ibodies[0].contains(&d0)
                            && !ibodies[1].contains(&d0)
                        {
                            d[0]
                        } else if obody.contains(&d1)
                            && !obody.contains(&d0)
                            && !ibodies[0].contains(&d1)
                            && !ibodies[1].contains(&d1)
                        {
                            d[1]
                        } else {
                            bail!("header test shape");
                        };
                        pending = Some(cont_bc);
                        cur = cont_bc.block(&func.dfg.value_lists);
                        continue;
                    }
                    // Ordinary mid-chain exit: one dest in-body, one out.
                    let (cont_bc, exit_bc, epos) = if obody.contains(&d0)
                        && !obody.contains(&d1)
                        && !ibodies[0].contains(&d0)
                        && !ibodies[1].contains(&d0)
                    {
                        (d[0], d[1], 1)
                    } else if obody.contains(&d1)
                        && !obody.contains(&d0)
                        && !ibodies[0].contains(&d1)
                        && !ibodies[1].contains(&d1)
                    {
                        (d[1], d[0], 0)
                    } else {
                        bail!("nonlinear outer chain");
                    };
                    let eb = exit_bc.block(&func.dfg.value_lists);
                    if !func.layout.is_cold(eb) {
                        bail!("warm outer side exit");
                    }
                    if cur != oh {
                        if seen_inner {
                            opost.push(cur);
                        } else {
                            if opre.is_empty() {
                                pre_entry_bc = pending;
                            }
                            opre.push(cur);
                        }
                    }
                    oexits.push(JamExit {
                        pos: opre.len() + opost.len() - 1,
                        cont_bc,
                        exit_bc,
                        tgt: eb,
                        epos,
                        corder: Vec::new(),
                        lvals: Vec::new(),
                        clone_root: None,
                    });
                    pending = Some(cont_bc);
                    cur = cont_bc.block(&func.dfg.value_lists);
                }
                _ => bail!("unhandled outer terminator"),
            }
            if opre.len() + opost.len() > MAX_CHAIN + 2 {
                bail!("outer chain too long");
            }
        }
    }
    let Some(olatch) = olatch else {
        bail!("no outer latch");
    };
    let Some((chpos, _chcond, fused_on_d0)) = chooser else {
        bail!("no chooser found");
    };
    let ientry_bc = ientry_bc.unwrap();
    let ih = cands[fused].ih;
    let ibody = cands[fused].ibody.clone();
    let ichain = cands[fused].chain.clone();
    let ipos = cands[fused].ipos;
    let ipost_bc = cands[fused].post_bc;
    let post = cands[fused].post;
    let ilatch = cands[fused].ilatch;
    let other_ib = cands[1 - fused].ibody.clone();
    // A brif latch's exit edge may not re-enter the loop.
    if let Some(oeb) = oexit_bc {
        if obody.contains(&oeb.block(&func.dfg.value_lists)) {
            bail!("outer latch exits inside");
        }
    }
    // Structural accounting: covered blocks + the other inner's region
    // must partition obody; leftovers only fed by {chooser, leftovers}.
    {
        let mut covered: FxHashSet<Block> = FxHashSet::default();
        covered.insert(oh);
        covered.extend(opre.iter().copied());
        covered.extend(flane.iter().copied());
        covered.extend(ichain.iter().copied());
        covered.extend(opost.iter().copied());
        let chb = opre[chpos];
        let exit_tgts: FxHashSet<Block> = oexits
            .iter()
            .chain(lexits.iter())
            .chain(cands[fused].iexits.iter())
            .map(|e| e.tgt)
            .collect();
        for &b in &obody {
            if covered.contains(&b) {
                continue;
            }
            if other_ib.contains(&b) {
                continue;
            }
            // Exit edges legitimately feed their targets from covered
            // blocks (the fused path clones the exit cone).
            if exit_tgts.contains(&b) {
                continue;
            }
            // Slow tail region: preds must be leftovers/other-inner.
            for p in cfg.pred_iter(b) {
                if covered.contains(&p.block) && p.block != chb {
                    bail!(format!(
                        "leftover block{} fed by covered block{}",
                        b.as_u32(),
                        p.block.as_u32()
                    ));
                }
            }
        }
    }
    // Pred discipline: fused ih entered only from its lane's tail (or the
    // chooser directly) + its own body; post only from the fused body or
    // a lane skip; chain blocks single-pred.
    let ientry_src = flane.last().copied().unwrap_or(opre[chpos]);
    for p in cfg.pred_iter(ih) {
        if !ibody.contains(&p.block) && p.block != ientry_src {
            bail!("inner has extra entries");
        }
    }
    for p in cfg.pred_iter(post) {
        if !ibody.contains(&p.block) && !flane.contains(&p.block) {
            bail!("post has extra preds");
        }
    }
    for &b in opre.iter() {
        if cfg.pred_iter(b).count() != 1 {
            bail!("outer chain block has extra preds");
        }
    }
    for &b in opost.iter().skip(1) {
        if cfg.pred_iter(b).count() != 1 {
            bail!("outer chain block has extra preds");
        }
    }

    // ---- counted test: `affine(iv,+s) cc bound` — found on the outer
    //      latch's brif (rotated form) or, for a jump latch, on the outer
    //      header's own brif (header-tested form). ----
    let ohparams = func.dfg.block_params(oh).to_vec();
    let lt = func.layout.last_inst(olatch).unwrap();
    let lc = match func.dfg.insts[lt] {
        InstructionData::Brif { arg: c, .. } => c,
        _ => {
            let ht = func.layout.last_inst(oh).unwrap();
            match func.dfg.insts[ht] {
                InstructionData::Brif { arg: c, .. } => c,
                _ => bail!("no outer counting test"),
            }
        }
    };
    let Some(lci) = func.dfg.value_def(func.dfg.resolve_aliases(lc)).inst() else {
        bail!("latch cond not an icmp");
    };
    let InstructionData::IntCompare {
        opcode: Opcode::Icmp,
        cond: lcc,
        args: [lx, ly],
    } = func.dfg.insts[lci]
    else {
        bail!("latch cond not an icmp");
    };
    // lcc as written continues the loop when the oh-dest is dest[0]; we
    // only need `affine iv cc bound` in either arg order.
    let mut ocnt: Option<(usize, i64, Value, bool, bool)> = None;
    for (a, b, cc) in [(lx, ly, lcc), (ly, lx, lcc.swap_args())] {
        let (le, signed) = match cc {
            IntCC::UnsignedLessThan => (false, false),
            IntCC::UnsignedLessThanOrEqual => (true, false),
            IntCC::SignedLessThan => (false, true),
            IntCC::SignedLessThanOrEqual => (true, true),
            _ => continue,
        };
        let Some((pidx, _d)) = affine_param(func, oh, a) else {
            continue;
        };
        let pty = func.dfg.value_type(ohparams[pidx]);
        if !pty.is_int() || pty.bits() < 16 || pty.bits() > 64 {
            continue;
        }
        let b = func.dfg.resolve_aliases(b);
        let mut ok = false;
        if let Some(bi) = func.dfg.value_def(b).inst() {
            if let Some(bb) = func.layout.inst_block(bi)
                && !obody.contains(&bb)
            {
                ok = true;
            }
        } else if param_idx(func, oh, b).is_some() {
            ok = true;
        } else if let cranelift_codegen::ir::ValueDef::Param(pblk, _) =
            func.dfg.value_def(b)
        {
            // Loop-invariant bound defined outside the loop body.
            ok = !obody.contains(&pblk);
        }
        if !ok {
            continue;
        }
        ocnt = Some((pidx, 0, b, le, signed));
        break;
    }
    let Some((opidx, _, obound, ole, osigned)) = ocnt else {
        bail!("no outer counting test");
    };
    let ivp = ohparams[opidx];
    // Step: the latch's binding for the iv param must be `iv + s`, s > 0.
    let latch_bc = {
        func.dfg.insts[lt]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .copied()
            .find(|bc| bc.block(&func.dfg.value_lists) == oh)
            .unwrap()
    };
    let latched: Vec<Value> = latch_bc
        .args(&func.dfg.value_lists)
        .filter_map(|a| match a {
            BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
            _ => None,
        })
        .collect();
    if latched.len() != ohparams.len() {
        bail!("outer latch arity");
    }
    let iv_next = func.dfg.resolve_aliases(latched[opidx]);
    let Some((spidx, step)) = affine_param(func, oh, iv_next) else {
        bail!("outer step not affine");
    };
    if spidx != opidx || step <= 0 {
        bail!("outer step not affine");
    }
    // Bound passed as an oh param must come back unchanged on the latch.
    if func.dfg.value_def(obound).inst().is_none()
        && let Some(q) = param_idx(func, oh, obound)
        && latched[q] != obound
    {
        bail!("bound mutates on backedge");
    }
    // All edge args must be plain values.
    for bc in [ientry_bc, ipost_bc, latch_bc]
        .into_iter()
        .chain(cands[fused].iexits.iter().flat_map(|e| [e.cont_bc, e.exit_bc]))
        .chain(oexits.iter().flat_map(|e| [e.cont_bc, e.exit_bc]))
        .chain(lexits.iter().flat_map(|e| [e.cont_bc, e.exit_bc]))
        .chain(lane_post.iter().map(|&(_, bc)| bc))
        .chain(oexit_bc)
        .chain(into_bc)
        .chain(lane_bc)
        .chain(pre_entry_bc)
    {
        if !bc
            .args(&func.dfg.value_lists)
            .all(|a| matches!(a, BlockArg::Value(_)))
        {
            bail!("non-value edge arg");
        }
    }

    // ---- dep set (transitively dependent on the outer IV) ----
    let mut dep: FxHashSet<Value> = FxHashSet::default();
    dep.insert(ivp);
    loop {
        let mut changed = false;
        for &b in &obody {
            for i in func.layout.block_insts(b) {
                if func
                    .dfg
                    .inst_values(i)
                    .any(|v| dep.contains(&func.dfg.resolve_aliases(v)))
                {
                    for &r in func.dfg.inst_results(i) {
                        changed |= dep.insert(func.dfg.resolve_aliases(r));
                    }
                }
            }
            for p in cfg.pred_iter(b) {
                for bc in func.dfg.insts[p.inst].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    if bc.block(&func.dfg.value_lists) != b {
                        continue;
                    }
                    for (q, a) in bc.args(&func.dfg.value_lists).enumerate() {
                        if let BlockArg::Value(a) = a
                            && dep.contains(&func.dfg.resolve_aliases(a))
                            && let Some(&pp) = func.dfg.block_params(b).get(q)
                        {
                            changed |= dep.insert(func.dfg.resolve_aliases(pp));
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    if dep.contains(&obound) {
        bail!("bound varies");
    }
    for (q, &p) in ohparams.iter().enumerate() {
        if q != opidx && dep.contains(&p) {
            bail!("non-iv header param varies");
        }
    }
    // The inner's exit-to-post condition must be invariant.
    {
        let exblk = ichain[ipos];
        let ti = func.layout.last_inst(exblk).unwrap();
        if let InstructionData::Brif { arg: c, .. } = func.dfg.insts[ti]
            && dep.contains(&func.dfg.resolve_aliases(c))
        {
            bail!("inner exit cond varies");
        }
        let _ = exblk;
    }
    // Lane brif conds must also be IV-invariant — a varying cond can't be
    // split per-copy inside the fused lane.
    for &b in &flane {
        let t = func.layout.last_inst(b).unwrap();
        if let InstructionData::Brif { arg: c, .. } = func.dfg.insts[t]
            && dep.contains(&func.dfg.resolve_aliases(c))
        {
            bail!("dep lane cond");
        }
    }
    // ---- side-exit cone clones (same analysis as run_jam) ----
    // `covered` = blocks cloned onto the fused path; values defined only
    // in leftover (other-inner/slow-tail) blocks keep their originals and
    // may be used anywhere the originals were.
    let covered: FxHashSet<Block> = {
        let mut s: FxHashSet<Block> = FxHashSet::default();
        s.insert(oh);
        s.extend(opre.iter().copied());
        s.extend(flane.iter().copied());
        s.extend(ichain.iter().copied());
        s.extend(opost.iter().copied());
        s
    };
    let lvals_all: FxHashSet<Value> = {
        let mut s = FxHashSet::default();
        for &b in covered.iter() {
            for &p in func.dfg.block_params(b) {
                s.insert(func.dfg.resolve_aliases(p));
            }
            for i in func.layout.block_insts(b) {
                for &r in func.dfg.inst_results(i) {
                    s.insert(func.dfg.resolve_aliases(r));
                }
            }
        }
        s
    };
    // Values defined anywhere in the outer body but not covered: a cone
    // referencing them can never be satisfied from a fused exit edge.
    let leftover_vals: FxHashSet<Value> = {
        let mut s = FxHashSet::default();
        for &b in obody.iter() {
            if covered.contains(&b) {
                continue;
            }
            for &p in func.dfg.block_params(b) {
                s.insert(func.dfg.resolve_aliases(p));
            }
            for i in func.layout.block_insts(b) {
                for &r in func.dfg.inst_results(i) {
                    s.insert(func.dfg.resolve_aliases(r));
                }
            }
        }
        s
    };
    let mut all_exits: Vec<&mut JamExit> = Vec::new();
    for e in cands[fused]
        .iexits
        .iter_mut()
        .chain(oexits.iter_mut())
        .chain(lexits.iter_mut())
    {
        all_exits.push(e);
    }


    for e in &mut all_exits {
        let scone: Vec<Block> = func
            .layout
            .blocks()
            .filter(|&b| {
                !obody.contains(&b) && dt.is_reachable(b) && dt.block_dominates(e.tgt, b)
            })
            .collect();
        let mut lset: FxHashSet<Value> = FxHashSet::default();
        let mut bad = false;
        let mut sinsts = 0usize;
        for &cb in &scone {
            for i in func.layout.block_insts(cb) {
                sinsts += 1;
                for v in func.dfg.inst_values(i) {
                    let v = func.dfg.resolve_aliases(v);
                    if lvals_all.contains(&v) && lset.insert(v) {
                        e.lvals.push(v);
                    }
                    if leftover_vals.contains(&v) {
                        bail!("exit cone uses leftover value");
                    }
                }
                let op = func.dfg.insts[i].opcode();
                if matches!(op, Opcode::TryCall | Opcode::TryCallIndirect | Opcode::BrTable) {
                    bad = true;
                }
                for bc in func.dfg.insts[i].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    for a in bc.args(&func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a {
                            let v = func.dfg.resolve_aliases(v);
                            if lvals_all.contains(&v) && lset.insert(v) {
                                e.lvals.push(v);
                            }
                            if leftover_vals.contains(&v) {
                                bail!("exit cone uses leftover value");
                            }
                        }
                    }
                    if obody.contains(&bc.block(&func.dfg.value_lists)) {
                        bail!("exit cone re-enters loop");
                    }
                }
            }
        }
        if !e.lvals.is_empty() && (bad || scone.len() > 8 || sinsts > 48) {
            bail!("side-exit cone too big");
        }
        e.corder = scone;
    }
    // Outside uses of body values must stay inside side-exit cones.
    {
        let mut coneblks: FxHashSet<Block> = FxHashSet::default();
        for e in cands[fused]
            .iexits
            .iter()
            .chain(oexits.iter())
            .chain(lexits.iter())
        {
            coneblks.extend(e.corder.iter().copied());
        }
        for b in func.layout.blocks() {
            if obody.contains(&b) || coneblks.contains(&b) || !dt.is_reachable(b) {
                continue;
            }
            for i in func.layout.block_insts(b) {
                for v in func.dfg.inst_values(i) {
                    let v = func.dfg.resolve_aliases(v);
                    if lvals_all.contains(&v) {
                        bail!(format!(
                            "body value v{} used outside in block{}",
                            v.as_u32(),
                            b.as_u32()
                        ));
                    }
                }
                for bc in func.dfg.insts[i].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    for a in bc.args(&func.dfg.value_lists) {
                        if let BlockArg::Value(v) = a {
                            let v = func.dfg.resolve_aliases(v);
                            if lvals_all.contains(&v) {
                                bail!(format!(
                                    "body value v{} on edge in block{}",
                                    v.as_u32(),
                                    b.as_u32()
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    // Post-chain stores may not feed a later tile element's inner loads.
    {
        let root_of = |func: &Function, mut v: Value| -> Option<Value> {
            for _ in 0..32 {
                v = func.dfg.resolve_aliases(v);
                let Some(i) = func.dfg.value_def(v).inst() else {
                    return Some(v);
                };
                match func.dfg.insts[i] {
                    InstructionData::Binary {
                        opcode: Opcode::Iadd,
                        args,
                    } => {
                        let (a, b) = (args[0], args[1]);
                        if iconst(func, a).is_some() {
                            v = b;
                        } else {
                            v = a;
                        }
                    }
                    _ => return Some(v),
                }
            }
            None
        };
        let mut lbases: FxHashSet<Value> = FxHashSet::default();
        for &b in ichain.iter() {
            for i in func.layout.block_insts(b) {
                if func.dfg.insts[i].opcode().can_load()
                    && let Some(&addr) = func.dfg.inst_args(i).first()
                    && let Some(r) = root_of(func, addr)
                {
                    lbases.insert(func.dfg.resolve_aliases(r));
                }
            }
        }
        for &b in &opost {
            for i in func.layout.block_insts(b) {
                let op = func.dfg.insts[i].opcode();
                if op.can_store() {
                    let args = func.dfg.inst_args(i);
                    match args.last().and_then(|&a| root_of(func, a)) {
                        Some(r) if !lbases.contains(&func.dfg.resolve_aliases(r)) => {}
                        _ => bail!("post-store may feed inner loads"),
                    }
                } else if op.is_call() || op.other_side_effects() {
                    bail!("unclonable post inst");
                }
            }
        }
    }
    // Pre/chooser insts must be pure or loads.
    for &b in &opre {
        for i in func.layout.block_insts(b) {
            let op = func.dfg.insts[i].opcode();
            if op.is_call() || op.can_store() || op.other_side_effects() {
                bail!("unclonable pre inst");
            }
        }
    }

    // ---- emission ----
    let mut vmaps: Vec<FxHashMap<Value, Value>> = (0..j).map(|_| FxHashMap::default()).collect();

    // Side-exit cone clones.
    {
        let mut all: Vec<&mut JamExit> = Vec::new();
        for e in cands[fused]
            .iexits
            .iter_mut()
            .chain(oexits.iter_mut())
            .chain(lexits.iter_mut())
        {
            all.push(e);
        }
        for e in &mut all {
            if e.lvals.is_empty() {
                continue;
            }
            let cset: FxHashSet<Block> = e.corder.iter().copied().collect();
            e.corder = cone_rpo(func, e.tgt, &cset);
            let cold = func.layout.is_cold(e.tgt);
            let mut scmap: FxHashMap<Block, Block> = FxHashMap::default();
            for (bi, &b) in e.corder.clone().iter().enumerate() {
                let nb = func.dfg.make_block();
                for &p in func.dfg.block_params(b).to_vec().iter() {
                    let ty = func.dfg.value_type(p);
                    func.dfg.append_block_param(nb, ty);
                }
                if bi == 0 {
                    for &lv in &e.lvals {
                        let ty = func.dfg.value_type(lv);
                        func.dfg.append_block_param(nb, ty);
                    }
                }
                func.layout.insert_block(nb, oh);
                if cold && func.layout.is_cold(b) {
                    func.layout.set_cold(nb);
                }
                scmap.insert(b, nb);
            }
            let root = scmap[&e.tgt];
            e.clone_root = Some(root);
            let orig_arity = func.dfg.block_params(e.tgt).len();
            let mut xmap: FxHashMap<Value, Value> = FxHashMap::default();
            for (i, &lv) in e.lvals.iter().enumerate() {
                xmap.insert(lv, func.dfg.block_params(root)[orig_arity + i]);
            }
            emit_cone_clone(func, &e.corder, &scmap, &mut xmap);
        }
    }

    // ojh: jammed outer header (rotated form: the counted test lives on
    // the latch, so oh's insts are cloned wholesale — including j_next —
    // and the guard is `j + (J-1)*step cc bound`).
    let ojh = func.dfg.make_block();
    for &p in &ohparams {
        let ty = func.dfg.value_type(p);
        func.dfg.append_block_param(ojh, ty);
    }
    func.layout.insert_block(ojh, oh);
    let ojhparams = func.dfg.block_params(ojh).to_vec();
    for d in 0..j {
        for (q, &p) in ohparams.iter().enumerate() {
            vmaps[d].insert(func.dfg.resolve_aliases(p), ojhparams[q]);
        }
    }
    let ok_guard;
    {
        let insts: Vec<Inst> = func.layout.block_insts(oh).collect();
        let mut omap: FxHashMap<Value, Value> = FxHashMap::default();
        for (q, &p) in ohparams.iter().enumerate() {
            omap.insert(func.dfg.resolve_aliases(p), ojhparams[q]);
        }
        let nbmap: FxHashMap<Block, Block> = FxHashMap::default();
        for &ii in &insts[..insts.len() - 1] {
            let src = func.dfg.insts[ii];
            let data = {
                let mut m = Vm {
                    func,
                    vmap: &omap,
                    bmap: &nbmap,
                };
                src.map(&mut m)
            };
            let ni = func.dfg.make_inst(data);
            let ctv = func.dfg.ctrl_typevar(ii);
            func.dfg.make_inst_results(ni, ctv);
            func.layout.append_inst(ni, ojh);
            for (&o, &nv) in func
                .dfg
                .inst_results(ii)
                .iter()
                .zip(func.dfg.inst_results(ni).iter())
            {
                omap.insert(func.dfg.resolve_aliases(o), nv);
            }
        }
        // j_d = iv + d: materialize in ojh so dep insts defined there
        // (e.g. iv_next feeding the latch args) see copy d's iv.
        {
            let ity0 = func.dfg.value_type(ivp);
            let ivr = func.dfg.resolve_aliases(ivp);
            let mut pos0 = FuncCursor::new(func).at_bottom(ojh);
            for d in 1..j {
                let dc = pos0.ins().iconst(ity0, d as i64);
                let jd = pos0.ins().iadd(ojhparams[opidx], dc);
                vmaps[d].insert(ivr, jd);
            }
        }
        // Dep insts defined in oh get per-d images (invariant ones were
        // already emitted once via `omap` and seed every copy below).
        for d in 1..j {
            for &ii in &insts[..insts.len() - 1] {
                let variant = func
                    .dfg
                    .inst_values(ii)
                    .any(|v| dep.contains(&func.dfg.resolve_aliases(v)));
                if !variant {
                    continue;
                }
                let src = func.dfg.insts[ii];
                let data = {
                    let mut m = Vm {
                        func,
                        vmap: &vmaps[d],
                        bmap: &nbmap,
                    };
                    src.map(&mut m)
                };
                let ni = func.dfg.make_inst(data);
                let ctv = func.dfg.ctrl_typevar(ii);
                func.dfg.make_inst_results(ni, ctv);
                func.layout.append_inst(ni, ojh);
                for (&o, &nv) in func
                    .dfg
                    .inst_results(ii)
                    .iter()
                    .zip(func.dfg.inst_results(ni).iter())
                {
                    vmaps[d].insert(func.dfg.resolve_aliases(o), nv);
                }
            }
        }
        for (&o, &n) in omap.iter() {
            for d in 0..j {
                vmaps[d].entry(o).or_insert(n);
            }
        }
        // Guard: iter j..j+J-1 all valid ⇔ (j + (J-1)*step) cc bound with
        // the latch's comparison, plus the plain `j cc bound` so a
        // degenerate entry still takes the original remainder.
        let ity = func.dfg.value_type(ivp);
        let wty = if ity.bits() <= 32 { types::I64 } else { ity };
        let boundv = *omap.get(&obound).unwrap_or(&obound);
        let cc = match (osigned, ole) {
            (true, true) => IntCC::SignedLessThanOrEqual,
            (true, false) => IntCC::SignedLessThan,
            (false, true) => IntCC::UnsignedLessThanOrEqual,
            (false, false) => IntCC::UnsignedLessThan,
        };
        let mut pos = FuncCursor::new(func).at_bottom(ojh);
        let a0 = ojhparams[opidx];
        let ok1 = pos.ins().icmp(cc, a0, boundv);
        let off = pos.ins().iconst(ity, (j as i64 - 1) * step);
        let alast = pos.ins().iadd(a0, off);
        let ok2 = pos.ins().icmp(cc, alast, boundv);
        // Widening is only needed for the subtraction form; the direct
        // `last cc bound` form needs no rem — keep wty for parity.
        let _ = wty;
        ok_guard = pos.ins().band(ok1, ok2);
    }

    // Pre-chooser blocks: flat clones, one copy of opre[..chpos] per
    // tile element.
    let mut preblk: Vec<Vec<Block>> = Vec::new();
    for _ in 0..j {
        let mut row = Vec::new();
        for &b in &opre[..chpos] {
            let nb = func.dfg.make_block();
            for &p in func.dfg.block_params(b).to_vec().iter() {
                let ty = func.dfg.value_type(p);
                func.dfg.append_block_param(nb, ty);
            }
            func.layout.insert_block(nb, oh);
            row.push(nb);
        }
        preblk.push(row);
    }
    // Fused chooser block: chooser's params dep-split; its insts cloned
    // per-copy where dep; cond evaluated per copy and ANDed.
    let fch = func.dfg.make_block();
    let mut poff: FxHashMap<Block, Vec<(usize, bool)>> = FxHashMap::default();
    {
        let chb = opre[chpos];
        let mut offs = Vec::new();
        for &p in func.dfg.block_params(chb).to_vec().iter() {
            let ty = func.dfg.value_type(p);
            let isdep = dep.contains(&func.dfg.resolve_aliases(p));
            offs.push((func.dfg.block_params(fch).len(), isdep));
            for _ in 0..if isdep { j } else { 1 } {
                func.dfg.append_block_param(fch, ty);
            }
        }
        poff.insert(chb, offs);
        func.layout.insert_block(fch, oh);
    }
    // Fused lane blocks (between chooser and inner header).
    let mut flblk: FxHashMap<Block, Block> = FxHashMap::default();
    for &b in flane.iter() {
        let fb = func.dfg.make_block();
        let mut offs = Vec::new();
        for &p in func.dfg.block_params(b).to_vec().iter() {
            let ty = func.dfg.value_type(p);
            let isdep = dep.contains(&func.dfg.resolve_aliases(p));
            offs.push((func.dfg.block_params(fb).len(), isdep));
            for _ in 0..if isdep { j } else { 1 } {
                func.dfg.append_block_param(fb, ty);
            }
        }
        poff.insert(b, offs);
        func.layout.insert_block(fb, oh);
        flblk.insert(b, fb);
    }
    // Fused inner blocks.
    let mut fblk: FxHashMap<Block, Block> = FxHashMap::default();
    for &b in ichain.iter() {
        let fb = func.dfg.make_block();
        let mut offs = Vec::new();
        for &p in func.dfg.block_params(b).to_vec().iter() {
            let ty = func.dfg.value_type(p);
            let isdep = dep.contains(&func.dfg.resolve_aliases(p));
            offs.push((func.dfg.block_params(fb).len(), isdep));
            for _ in 0..if isdep { j } else { 1 } {
                func.dfg.append_block_param(fb, ty);
            }
        }
        poff.insert(b, offs);
        func.layout.insert_block(fb, oh);
        fblk.insert(b, fb);
    }
    // Fused post-merge: dep-split post params. The fused inner's exits
    // and any lane zero-trip skips all land here; from it each copy's
    // tail chain picks out its own lane's values.
    let fpost = func.dfg.make_block();
    {
        let mut offs = Vec::new();
        for &p in func.dfg.block_params(post).to_vec().iter() {
            let ty = func.dfg.value_type(p);
            let isdep = dep.contains(&func.dfg.resolve_aliases(p));
            offs.push((func.dfg.block_params(fpost).len(), isdep));
            for _ in 0..if isdep { j } else { 1 } {
                func.dfg.append_block_param(fpost, ty);
            }
        }
        poff.insert(post, offs);
        func.layout.insert_block(fpost, oh);
    }
    // `fpost` param slice for copy `d` — the args binding tailblk[d][0]'s
    // (post's flat) params from the fused merge.
    let fpost_args = |func: &Function, d: usize| -> Vec<BlockArg> {
        let fpp = func.dfg.block_params(fpost);
        let mut out = Vec::new();
        for (q, _) in func.dfg.block_params(post).iter().enumerate() {
            let (off, isdep) = poff[&post][q];
            out.push(BlockArg::Value(fpp[off + if isdep { d } else { 0 }]));
        }
        out
    };
    // Seed param maps for the chooser, lane and inner blocks.
    {
        let chb = opre[chpos];
        for (blk, fb) in [(chb, fch)]
            .into_iter()
            .chain(flane.iter().map(|&b| (b, flblk[&b])))
            .chain(ichain.iter().map(|&b| (b, fblk[&b])))
        {
            let fbp = func.dfg.block_params(fb).to_vec();
            for (q, &p) in func.dfg.block_params(blk).to_vec().iter().enumerate() {
                let (off, isdep) = poff[&blk][q];
                let p = func.dfg.resolve_aliases(p);
                for d in 0..j {
                    vmaps[d].insert(p, fbp[off + if isdep { d } else { 0 }]);
                }
            }
        }
    }
    // Tail blocks per copy.
    let mut tailblk: Vec<Vec<Block>> = Vec::new();
    for _ in 0..j {
        let mut row = Vec::new();
        for &b in &opost {
            let nb = func.dfg.make_block();
            for &p in func.dfg.block_params(b).to_vec().iter() {
                let ty = func.dfg.value_type(p);
                func.dfg.append_block_param(nb, ty);
            }
            func.layout.insert_block(nb, oh);
            row.push(nb);
        }
        tailblk.push(row);
    }
    let fih = fblk[&ih];
    // `ivp`'s per-d images were materialized in ojh already.
    let _ = func.dfg.value_type(ivp);
    let ex_tgt = |e: &JamExit| e.clone_root.unwrap_or(e.tgt);

    // Pre-chooser blocks, copy by copy: each copy's chain feeds the
    // next's; the last copy's chain enters the fused chooser.
    for d in 0..j {
        for (pi, &b) in opre[..chpos].iter().enumerate() {
            let nb = preblk[d][pi];
            for (q, &p) in func.dfg.block_params(b).to_vec().iter().enumerate() {
                vmaps[d].insert(
                    func.dfg.resolve_aliases(p),
                    func.dfg.block_params(nb)[q],
                );
            }
            jam_emit_flat(func, b, nb, d, &mut vmaps);
            let t = func.layout.last_inst(b).unwrap();
            // The chain's continue target after block pi for copy d:
            // the next pre block, the next copy's first pre block once
            // this copy's chain ends at the chooser, or the fused
            // chooser itself for the last copy.
            let cont = |func: &Function,
                        bc: BlockCall,
                        vm: &[FxHashMap<Value, Value>]|
             -> (Block, Vec<BlockArg>) {
                if pi + 1 < chpos {
                    debug_assert_eq!(bc.block(&func.dfg.value_lists), opre[pi + 1]);
                    (preblk[d][pi + 1], jam_flat_args(func, d, bc, vm))
                } else if d + 1 < j {
                    (
                        preblk[d + 1][0],
                        jam_flat_args(func, d + 1, pre_entry_bc.unwrap(), vm),
                    )
                } else {
                    (
                        fch,
                        jam_fused_args(func, j, bc, opre[chpos], vm, &poff),
                    )
                }
            };
            match func.dfg.insts[t] {
                InstructionData::Jump { destination, .. } => {
                    let (tgt, args) = cont(func, destination, &vmaps);
                    FuncCursor::new(func).at_bottom(nb).ins().jump(tgt, &args);
                }
                InstructionData::Brif { arg: c, .. } => {
                    let c = func.dfg.resolve_aliases(c);
                    let cm = *vmaps[d].get(&c).unwrap_or(&c);
                    let e = oexits
                        .iter()
                        .find(|e| e.pos == pi)
                        .expect("jam2: pre brif without exit");
                    let (ctgt, cargs) = cont(func, e.cont_bc, &vmaps);
                    let eargs = jam_exit_args(func, d, e, &vmaps);
                    let etgt = ex_tgt(e);
                    let mut pos = FuncCursor::new(func).at_bottom(nb);
                    if e.epos == 0 {
                        pos.ins().brif(cm, etgt, &eargs, ctgt, &cargs);
                    } else {
                        pos.ins().brif(cm, ctgt, &cargs, etgt, &eargs);
                    }
                }
                _ => unreachable!("jam2: pre terminator"),
            }
        }
    }

    // Fused chooser: clone dep insts per-d, shared once.
    {
        let chb = opre[chpos];
        jam_emit_fused(func, chb, fch, j, &dep, &mut vmaps);
        let ti = func.layout.last_inst(chb).unwrap();
        let InstructionData::Brif { arg: c, .. } = func.dfg.insts[ti] else {
            unreachable!()
        };
        let c = func.dfg.resolve_aliases(c);
        // AND of per-copy conds (all copies take the fused lane). For a
        // d1-fused chooser the lane is taken when the cond is false —
        // OR the conds and negate via brif order instead.
        let (ftgt, entry) = if let Some(&lb) = flane.first() {
            (
                flblk[&lb],
                jam_fused_args(func, j, lane_bc.unwrap(), lb, &vmaps, &poff),
            )
        } else {
            (fih, jam_fused_args(func, j, ientry_bc, ih, &vmaps, &poff))
        };
        let rem: Vec<BlockArg> = ojhparams.iter().map(|&v| v.into()).collect();
        let mut pos = FuncCursor::new(func).at_bottom(fch);
        if fused_on_d0 {
            let mut fcond = *vmaps[0].get(&c).unwrap_or(&c);
            for d in 1..j {
                let cd = *vmaps[d].get(&c).unwrap_or(&c);
                fcond = pos.ins().band(fcond, cd);
            }
            pos.ins().brif(fcond, ftgt, &entry, oh, &rem);
        } else {
            let mut fcond = *vmaps[0].get(&c).unwrap_or(&c);
            for d in 1..j {
                let cd = *vmaps[d].get(&c).unwrap_or(&c);
                fcond = pos.ins().bor(fcond, cd);
            }
            pos.ins().brif(fcond, oh, &rem, ftgt, &entry);
        }
    }

    // Fused lane blocks between the chooser and the inner loop: dep
    // insts cloned per copy like the inner itself. A lane terminator
    // either enters `ih` / the next lane block, or exits to `post` (a
    // zero-trip skip) or a cold JamExit.
    for &b in flane.iter() {
        let fb = flblk[&b];
        jam_emit_fused(func, b, fb, j, &dep, &mut vmaps);
        let t = func.layout.last_inst(b).unwrap();
        match func.dfg.insts[t] {
            InstructionData::Jump { destination, .. } => {
                let nb = destination.block(&func.dfg.value_lists);
                if nb == ih {
                    let args = jam_fused_args(func, j, destination, ih, &vmaps, &poff);
                    FuncCursor::new(func).at_bottom(fb).ins().jump(fih, &args);
                } else {
                    let nt = flblk[&nb];
                    let args = jam_fused_args(func, j, destination, nb, &vmaps, &poff);
                    FuncCursor::new(func).at_bottom(fb).ins().jump(nt, &args);
                }
            }
            InstructionData::Brif { arg: c, .. } => {
                let c = func.dfg.resolve_aliases(c);
                let cm = *vmaps[0].get(&c).unwrap_or(&c);
                let dests: Vec<BlockCall> = func.dfg.insts[t]
                    .branch_destination(
                        &func.dfg.jump_tables,
                        &func.dfg.exception_tables,
                    )
                    .to_vec();
                let mut out: Vec<(Block, Vec<BlockArg>)> = Vec::new();
                for bc in dests {
                    let db = bc.block(&func.dfg.value_lists);
                    if db == ih {
                        out.push((fih, jam_fused_args(func, j, bc, ih, &vmaps, &poff)));
                    } else if flblk.contains_key(&db) {
                        out.push((
                            flblk[&db],
                            jam_fused_args(func, j, bc, db, &vmaps, &poff),
                        ));
                    } else if db == post {
                        out.push((fpost, jam_fused_args(func, j, bc, post, &vmaps, &poff)));
                    } else {
                        let e = lexits
                            .iter()
                            .find(|e| flane.get(e.pos).copied() == Some(b) && e.tgt == db)
                            .expect("jam2: lane brif without exit");
                        out.push((ex_tgt(e), jam_exit_args(func, 0, e, &vmaps)));
                    }
                }
                let mut pos = FuncCursor::new(func).at_bottom(fb);
                pos.ins()
                    .brif(cm, out[0].0, &out[0].1, out[1].0, &out[1].1);
            }
            _ => unreachable!("jam2: lane terminator"),
        }
    }

    // Fused inner emission (same as run_jam).
    for (ci, &b) in ichain.iter().enumerate() {
        let fb = fblk[&b];
        jam_emit_fused(func, b, fb, j, &dep, &mut vmaps);
        let t = func.layout.last_inst(b).unwrap();
        match func.dfg.insts[t] {
            InstructionData::Jump { .. } => {
                let bc = func.dfg.insts[t]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[0];
                let nb = bc.block(&func.dfg.value_lists);
                if nb == ih {
                    let args = jam_fused_args(func, j, bc, ih, &vmaps, &poff);
                    FuncCursor::new(func).at_bottom(fb).ins().jump(fih, &args);
                } else {
                    let args = jam_fused_args(func, j, bc, nb, &vmaps, &poff);
                    FuncCursor::new(func)
                        .at_bottom(fb)
                        .ins()
                        .jump(fblk[&nb], &args);
                }
            }
            InstructionData::Brif { arg: c, .. } => {
                let c = func.dfg.resolve_aliases(c);
                if ci == ipos {
                    // The normal inner exit.
                    let cm = *vmaps[0].get(&c).unwrap_or(&c);
                    let dests: Vec<BlockCall> = func.dfg.insts[t]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                        .to_vec();
                    let (cont_bc, post_bc, epos) = if dests[0].block(&func.dfg.value_lists)
                        == post
                    {
                        (dests[1], dests[0], 0)
                    } else {
                        (dests[0], dests[1], 1)
                    };
                    let cb = cont_bc.block(&func.dfg.value_lists);
                    if cb == ih {
                        // Latch exit.
                        let cargs = jam_fused_args(func, j, cont_bc, ih, &vmaps, &poff);
                        let pargs = jam_fused_args(func, j, post_bc, post, &vmaps, &poff);
                        let mut pos = FuncCursor::new(func).at_bottom(fb);
                        if epos == 0 {
                            pos.ins().brif(cm, fpost, &pargs, fih, &cargs);
                        } else {
                            pos.ins().brif(cm, fih, &cargs, fpost, &pargs);
                        }
                    } else {
                        let cargs = jam_fused_args(func, j, cont_bc, cb, &vmaps, &poff);
                        let pargs = jam_fused_args(func, j, post_bc, post, &vmaps, &poff);
                        let ctgt = fblk[&cb];
                        let mut pos = FuncCursor::new(func).at_bottom(fb);
                        if epos == 0 {
                            pos.ins().brif(cm, fpost, &pargs, ctgt, &cargs);
                        } else {
                            pos.ins().brif(cm, ctgt, &cargs, fpost, &pargs);
                        }
                    }
                } else {
                    let e = cands[fused]
                        .iexits
                        .iter()
                        .find(|e| ichain[e.pos] == b)
                        .expect("jam2: inner brif without exit");
                    let cblk = fblk[&e.cont_bc.block(&func.dfg.value_lists)];
                    if dep.contains(&c) {
                        let mut cur = fb;
                        for d in 0..j {
                            let cm = *vmaps[d].get(&c).unwrap_or(&c);
                            let eargs = jam_exit_args(func, d, e, &vmaps);
                            let etgt = ex_tgt(e);
                            let is_last = d + 1 == j;
                            let (cargs, nxt) = if is_last {
                                (
                                    jam_fused_args(
                                        func,
                                        j,
                                        e.cont_bc,
                                        e.cont_bc.block(&func.dfg.value_lists),
                                        &vmaps,
                                        &poff,
                                    ),
                                    cblk,
                                )
                            } else {
                                (Vec::new(), {
                                    let mb = func.dfg.make_block();
                                    func.layout.insert_block(mb, oh);
                                    mb
                                })
                            };
                            let mut pos = FuncCursor::new(func).at_bottom(cur);
                            if e.epos == 0 {
                                pos.ins().brif(cm, etgt, &eargs, nxt, &cargs);
                            } else {
                                pos.ins().brif(cm, nxt, &cargs, etgt, &eargs);
                            }
                            cur = nxt;
                        }
                    } else {
                        let cm = *vmaps[0].get(&c).unwrap_or(&c);
                        let cargs = jam_fused_args(
                            func,
                            j,
                            e.cont_bc,
                            e.cont_bc.block(&func.dfg.value_lists),
                            &vmaps,
                            &poff,
                        );
                        let eargs = jam_exit_args(func, 0, e, &vmaps);
                        let etgt = ex_tgt(e);
                        let mut pos = FuncCursor::new(func).at_bottom(fb);
                        if e.epos == 0 {
                            pos.ins().brif(cm, etgt, &eargs, cblk, &cargs);
                        } else {
                            pos.ins().brif(cm, cblk, &cargs, etgt, &eargs);
                        }
                    }
                }
            }
            _ => unreachable!("jam2: inner terminator"),
        }
    }
    let _ = ilatch;

    // The fused post-merge feeds copy 0's tail; each subsequent copy's
    // tail is entered from the previous copy's latch, binding its slice
    // of the fused merge params.
    {
        let args = fpost_args(func, 0);
        FuncCursor::new(func)
            .at_bottom(fpost)
            .ins()
            .jump(tailblk[0][0], &args);
    }

    // ---- tail emission ----
    for d in 0..j {
        for (pi, &b) in opost.iter().enumerate() {
            let nb = tailblk[d][pi];
            for (q, &p) in func.dfg.block_params(b).to_vec().iter().enumerate() {
                vmaps[d].insert(
                    func.dfg.resolve_aliases(p),
                    func.dfg.block_params(nb)[q],
                );
            }
            jam_emit_flat(func, b, nb, d, &mut vmaps);
            let t = func.layout.last_inst(b).unwrap();
            match func.dfg.insts[t] {
                InstructionData::Jump { destination, .. } => {
                    let db = destination.block(&func.dfg.value_lists);
                    if db == oh {
                        let (tgt, args): (Block, Vec<BlockArg>) = if d + 1 < j {
                            (tailblk[d + 1][0], fpost_args(func, d + 1))
                        } else {
                            (ojh, jam_flat_args(func, d, destination, &vmaps))
                        };
                        FuncCursor::new(func)
                            .at_bottom(nb)
                            .ins()
                            .jump(tgt, &args);
                    } else {
                        let args = jam_flat_args(func, d, destination, &vmaps);
                        FuncCursor::new(func)
                            .at_bottom(nb)
                            .ins()
                            .jump(tailblk[d][pi + 1], &args);
                    }
                }
                InstructionData::Brif { arg: c, .. } => {
                    let c = func.dfg.resolve_aliases(c);
                    let cm = *vmaps[d].get(&c).unwrap_or(&c);
                    let dests: Vec<BlockCall> = func.dfg.insts[t]
                        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                        .to_vec();
                    let (d0, d1) = (
                        dests[0].block(&func.dfg.value_lists),
                        dests[1].block(&func.dfg.value_lists),
                    );
                    if d0 == oh || d1 == oh {
                        let (oh_bc, out_bc, opos) = if d0 == oh {
                            (dests[0], dests[1], 0)
                        } else {
                            (dests[1], dests[0], 1)
                        };
                        let (otgt, oargs): (Block, Vec<BlockArg>) = if d + 1 < j {
                            (tailblk[d + 1][0], fpost_args(func, d + 1))
                        } else {
                            (ojh, jam_flat_args(func, d, oh_bc, &vmaps))
                        };
                        let xargs = jam_flat_args(func, d, out_bc, &vmaps);
                        let xtgt = out_bc.block(&func.dfg.value_lists);
                        let mut pos = FuncCursor::new(func).at_bottom(nb);
                        if opos == 0 {
                            pos.ins().brif(cm, otgt, &oargs, xtgt, &xargs);
                        } else {
                            pos.ins().brif(cm, xtgt, &xargs, otgt, &oargs);
                        }
                    } else {
                        let e = oexits
                            .iter()
                            .find(|e| {
                                e.pos >= opre.len()
                                    && opost.get(e.pos - opre.len()).copied() == Some(b)
                            })
                            .expect("jam2: post brif without exit");
                        let cargs = jam_flat_args(func, d, e.cont_bc, &vmaps);
                        let eargs = jam_exit_args(func, d, e, &vmaps);
                        let etgt = ex_tgt(e);
                        let ctgt = tailblk[d][pi + 1];
                        let mut pos = FuncCursor::new(func).at_bottom(nb);
                        if e.epos == 0 {
                            pos.ins().brif(cm, etgt, &eargs, ctgt, &cargs);
                        } else {
                            pos.ins().brif(cm, ctgt, &cargs, etgt, &eargs);
                        }
                    }
                }
                _ => unreachable!("jam2: post terminator"),
            }
        }
    }

    // ---- ojh guard brif + entry redirect ----
    {
        // The fused path is entered through the first pre block (or the
        // fused chooser directly when there are none); its params bind
        // the edge args that entered the original chain.
        let (gtgt, gargs): (Block, Vec<BlockArg>) = if chpos > 0 {
            (
                preblk[0][0],
                jam_flat_args(func, 0, pre_entry_bc.unwrap(), &vmaps),
            )
        } else {
            (
                fch,
                jam_fused_args(func, j, into_bc.unwrap(), opre[chpos], &vmaps, &poff),
            )
        };
        let rargs: Vec<BlockArg> = ojhparams.iter().map(|&v| v.into()).collect();
        FuncCursor::new(func)
            .at_bottom(ojh)
            .ins()
            .brif(ok_guard, gtgt, &gargs, oh, &rargs);
    }
    let entries: Vec<(Block, Inst)> = cfg
        .pred_iter(oh)
        .filter(|p| !obody.contains(&p.block) && p.block != ojh)
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
            if bc.block(&dfg.value_lists) == oh {
                let args: Vec<BlockArg> = bc.args(&dfg.value_lists).collect();
                *bc = BlockCall::new(ojh, args.iter().copied(), &mut dfg.value_lists);
            }
        }
    }
    if debug() {
        eprintln!("jam2 {name} block{}: jammed x{j} (chooser)", oh.as_u32());
    }
    Some(vec![ojh])
}

/// Clone `ii` into `dst` under `vmap` (identity block map — callers fix
/// terminators themselves).
fn clone_inst_into(
    func: &mut Function,
    ii: Inst,
    dst: Block,
    vmap: &FxHashMap<Value, Value>,
) -> Inst {
    let src = func.dfg.insts[ii];
    let nbmap: FxHashMap<Block, Block> = FxHashMap::default();
    let data = {
        let mut m = Vm {
            func,
            vmap,
            bmap: &nbmap,
        };
        src.map(&mut m)
    };
    let ni = func.dfg.make_inst(data);
    let ctv = func.dfg.ctrl_typevar(ii);
    func.dfg.make_inst_results(ni, ctv);
    func.layout.append_inst(ni, dst);
    ni
}

/// Edge args for a fused inner target: each orig arg supplies J mapped
/// values when the target's param at that position is `dep`, else one.
#[allow(clippy::too_many_arguments)]
fn jam_fused_args(
    func: &Function,
    j: usize,
    bc: BlockCall,
    tgt: Block,
    vmaps: &[FxHashMap<Value, Value>],
    poff: &FxHashMap<Block, Vec<(usize, bool)>>,
) -> Vec<BlockArg> {
    let mut out: Vec<BlockArg> = Vec::new();
    for (q, a) in bc.args(&func.dfg.value_lists).enumerate() {
        let v = match a {
            BlockArg::Value(v) => func.dfg.resolve_aliases(v),
            _ => unreachable!("jam: non-value edge arg"),
        };
        let (_, isdep) = poff[&tgt][q];
        if isdep {
            for d in 0..j {
                out.push(BlockArg::Value(*vmaps[d].get(&v).unwrap_or(&v)));
            }
        } else {
            out.push(BlockArg::Value(*vmaps[0].get(&v).unwrap_or(&v)));
        }
    }
    out
}

/// Plain per-copy edge args for cloned (non-fused) blocks.
fn jam_flat_args(
    func: &Function,
    d: usize,
    bc: BlockCall,
    vmaps: &[FxHashMap<Value, Value>],
) -> Vec<BlockArg> {
    bc.args(&func.dfg.value_lists)
        .map(|a| match a {
            BlockArg::Value(v) => {
                let v = func.dfg.resolve_aliases(v);
                BlockArg::Value(*vmaps[d].get(&v).unwrap_or(&v))
            }
            a => a,
        })
        .collect()
}

/// Side-exit edge: args mapped under vmaps[d] plus appended lval bindings.
fn jam_exit_args(
    func: &Function,
    d: usize,
    e: &JamExit,
    vmaps: &[FxHashMap<Value, Value>],
) -> Vec<BlockArg> {
    let mut out = jam_flat_args(func, d, e.exit_bc, vmaps);
    if e.clone_root.is_some() {
        for &lv in &e.lvals {
            let lv = func.dfg.resolve_aliases(lv);
            out.push(BlockArg::Value(*vmaps[d].get(&lv).unwrap_or(&lv)));
        }
    }
    out
}

/// Clone `src`'s non-terminator insts into `dst`, once per copy for
/// `dep` insts (under vmaps[d]), once for the rest (under vmaps[0]).
fn jam_emit_fused(
    func: &mut Function,
    src: Block,
    dst: Block,
    j: usize,
    dep: &FxHashSet<Value>,
    vmaps: &mut [FxHashMap<Value, Value>],
) {
    let insts: Vec<Inst> = func.layout.block_insts(src).collect();
    for &ii in &insts[..insts.len() - 1] {
        let variant = func
            .dfg
            .inst_values(ii)
            .any(|v| dep.contains(&func.dfg.resolve_aliases(v)));
        if variant {
            for d in 0..j {
                let ni = clone_inst_into(func, ii, dst, &vmaps[d]);
                for (&o, &nv) in func
                    .dfg
                    .inst_results(ii)
                    .iter()
                    .zip(func.dfg.inst_results(ni).iter())
                {
                    vmaps[d].insert(func.dfg.resolve_aliases(o), nv);
                }
            }
        } else {
            let ni = clone_inst_into(func, ii, dst, &vmaps[0]);
            for (&o, &nv) in func
                .dfg
                .inst_results(ii)
                .iter()
                .zip(func.dfg.inst_results(ni).iter())
            {
                let o = func.dfg.resolve_aliases(o);
                for vm in vmaps.iter_mut() {
                    vm.insert(o, nv);
                }
            }
        }
    }
}

/// Clone `src`'s non-terminator insts into `dst` under `vmaps[d]`.
fn jam_emit_flat(
    func: &mut Function,
    src: Block,
    dst: Block,
    d: usize,
    vmaps: &mut [FxHashMap<Value, Value>],
) {
    let insts: Vec<Inst> = func.layout.block_insts(src).collect();
    for &ii in &insts[..insts.len() - 1] {
        let ni = clone_inst_into(func, ii, dst, &vmaps[d]);
        for (&o, &nv) in func
            .dfg
            .inst_results(ii)
            .iter()
            .zip(func.dfg.inst_results(ni).iter())
        {
            vmaps[d].insert(func.dfg.resolve_aliases(o), nv);
        }
    }
}
