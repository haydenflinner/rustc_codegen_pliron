//! Bounded full unroll of `0..K` loops (`PLIRON_BUNROLL`, opt-in).
//!
//! A header param that starts at a constant and steps by `+s` on every
//! back-edge, checked in-loop against a small constant bound (postcard's
//! `for i in 0..varint_max` varint decoders — `punroll` rejects them for
//! their mid-loop exits), can't exceed `bound` iterations. Emit `bound`
//! clones of the loop body — each keeps its early exits; back-edges chain
//! to the next clone — and leave the original loop as the post-bound
//! tail, so the transform stays correct wherever the bound read misses
//! (worst case: dead code). Per-copy constants (`7*i` shift amounts, IV
//! offsets) fold in later steps; `unreach`/`jumpthread` drop the tail.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::instructions::InstructionMapper;
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Constant, DynamicStackSlot, ExceptionTable, FuncRef, Function,
    GlobalValue, Immediate, Inst, InstructionData, JumpTable, Opcode, SigRef, StackSlot, Value,
    ValueList,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_BLOCKS: usize = 8;
const MAX_ITERS: usize = 12;
/// Non-terminator insts per loop, summed — each clone costs this again.
const MAX_BODY: usize = 48;

struct Vm<'a> {
    func: &'a mut Function,
    vmap: &'a FxHashMap<Value, Value>,
    bmap: &'a FxHashMap<Block, Block>,
}
impl Vm<'_> {
    fn mv(&self, v: Value) -> Value {
        self.vmap.get(&self.func.dfg.resolve_aliases(v)).copied().unwrap_or(v)
    }
    fn mb(&self, b: Block) -> Block {
        self.bmap.get(&b).copied().unwrap_or(b)
    }
}
impl InstructionMapper for Vm<'_> {
    fn map_value(&mut self, v: Value) -> Value {
        self.mv(v)
    }
    fn map_value_list(&mut self, vl: ValueList) -> ValueList {
        let mut out = ValueList::new();
        for v in vl.as_slice(&self.func.dfg.value_lists).to_vec() {
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

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let v = func.dfg.resolve_aliases(v);
    let i = func.dfg.value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

/// `v` as `h[param] + d` — either directly, through `iadd(.., s)`, or
/// through a param of another in-loop block whose every in-loop in-edge
/// supplies the same affine arg (the rotated-loop latch: `h[p]` flows
/// in, `p+s` comes back out).
fn rec_expr(
    func: &Function,
    cfg: &ControlFlowGraph,
    h: Block,
    inset: &FxHashSet<Block>,
    v: Value,
    depth: usize,
) -> Option<(usize, i64)> {
    if depth == 0 {
        return None;
    }
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.value_def(v) {
        cranelift_codegen::ir::ValueDef::Param(b, pi) if b == h => {
            return Some((pi, 0));
        }
        cranelift_codegen::ir::ValueDef::Param(b, m) if inset.contains(&b) => {
            // Relay param: all in-loop preds must feed the same `h[p]+d`.
            let mut out = None;
            for p in cfg.pred_iter(b) {
                if !inset.contains(&p.block) {
                    continue;
                }
                for bc in func.dfg.insts[p.inst]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    if bc.block(&func.dfg.value_lists) != b {
                        continue;
                    }
                    let Some(BlockArg::Value(a)) = bc.args(&func.dfg.value_lists).nth(m) else {
                        return None;
                    };
                    let e = rec_expr(func, cfg, h, inset, a, depth - 1)?;
                    if out.is_some_and(|o| o != e) {
                        return None;
                    }
                    out = Some(e);
                }
            }
            return out;
        }
        cranelift_codegen::ir::ValueDef::Result(i, _) => {
            if let InstructionData::Binary {
                opcode: Opcode::Iadd,
                args,
            } = func.dfg.insts[i]
            {
                for (x, c) in [(args[0], args[1]), (args[1], args[0])] {
                    if let Some(s) = iconst(func, c) {
                        if let Some((k, d)) = rec_expr(func, cfg, h, inset, x, depth - 1) {
                            return Some((k, d + s));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    None
}

/// One candidate: loop `lp`, header `h`, induction param index `iv`,
/// per-iteration bound `iters`, entry preds (insts outside the loop).
struct Plan {
    h: Block,
    blocks: Vec<Block>,
    /// `(param index, v0, step)` — constant in copy i: `v0 + i*step`.
    ivs: Vec<(usize, i64, i64)>,
    iters: usize,
    entries: Vec<Inst>,
}

fn detect(func: &Function, cfg: &ControlFlowGraph, dt: &DominatorTree, la: &LoopAnalysis, lp: Loop) -> Option<Plan> {
    let h = la.loop_header(lp);
    if !dt.is_reachable(h) {
        return None;
    }
    let dbg = |why: &str| tracing::debug!(target: "pliron::bunroll", "block{}: {why}", h.as_u32());
    // Loop blocks = blocks whose *innermost* loop is `lp` (no nested).
    // Bail on nested loops: blocks in a child loop aren't cloned but
    // outer uses of their values would leak original defs into clones.
    if func
        .layout
        .blocks()
        .any(|b| la.is_in_loop(b, lp) && la.innermost_loop(b) != Some(lp))
    {
        dbg("nested");
        return None;
    }
    let mut blocks: Vec<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.innermost_loop(b) == Some(lp))
        .collect();
    if blocks.len() > MAX_BLOCKS || !blocks.contains(&h) {
        dbg("blocks");
        return None;
    }
    blocks.retain(|&b| b != h);
    blocks.insert(0, h);
    let inset: FxHashSet<Block> = blocks.iter().copied().collect();
    // Only Jump/Brif terminators inside — br_table clones would keep
    // pointing at original blocks.
    for &b in &blocks {
        if let Some(t) = func.layout.last_inst(b) {
            if !matches!(
                func.dfg.insts[t],
                InstructionData::Jump { .. } | InstructionData::Brif { .. }
            ) {
                dbg("term");
                return None;
            }
        }
    }
    // Body size cap.
    let body: usize = blocks
        .iter()
        .map(|&b| func.layout.block_insts(b).count().saturating_sub(1))
        .sum();
    if body > MAX_BODY {
        dbg("body");
        return None;
    }
    let params = func.dfg.block_params(h).to_vec();
    // Split h's preds: entries (outside) vs back-edges (inside).
    let mut entries: Vec<Inst> = Vec::new();
    let mut backs: Vec<Inst> = Vec::new();
    for p in cfg.pred_iter(h) {
        let src = func.dfg.insts[p.inst];
        // Bail on indirect/exception edges: keeps the retarget simple.
        if !matches!(src, InstructionData::Jump { .. } | InstructionData::Brif { .. }) {
            dbg("pred-term");
            return None;
        }
        if inset.contains(&p.block) {
            backs.push(p.inst);
        } else {
            entries.push(p.inst);
        }
    }
    if entries.is_empty() || backs.is_empty() {
        dbg("edges");
        return None;
    }
    // IV: a header param = `param + s` on every back-edge, constant on
    // every entry edge, with s a positive constant.
    // Per-param recurrence: entry arg must be a constant agreed by all
    // entry edges, and every back-edge's arg must resolve — through
    // relay latch params — to `h[k] + d`.
    let mut recs: Vec<Option<(i64, usize, i64)>> = Vec::new(); // (v0, k, d)
    'ivs: for (pi, _) in params.iter().enumerate() {
        let mut v0 = None;
        for &ei in &entries {
            let mut got = false;
            for bc in func.dfg.insts[ei].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                if bc.block(&func.dfg.value_lists) != h {
                    continue;
                }
                let a = bc.args(&func.dfg.value_lists).nth(pi);
                if let Some(BlockArg::Value(v)) = a {
                    if let Some(c) = iconst(func, v) {
                        if v0.is_none_or(|x| x == c) {
                            v0 = Some(c);
                            got = true;
                        }
                    }
                }
            }
            if !got {
                recs.push(None);
                continue 'ivs;
            }
        }
        let mut rec = None;
        let mut ok = true;
        for &bi in &backs {
            let mut got = false;
            for bc in func.dfg.insts[bi].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
                if bc.block(&func.dfg.value_lists) != h {
                    continue;
                }
                if let Some(BlockArg::Value(a)) = bc.args(&func.dfg.value_lists).nth(pi) {
                    if let Some(e) = rec_expr(func, cfg, h, &inset, a, 6) {
                        if rec.is_none_or(|r| r == e) {
                            rec = Some(e);
                            got = true;
                        }
                    }
                }
            }
            ok &= got;
        }
        recs.push(if ok { rec.map(|(k, d)| (v0.unwrap(), k, d)) } else { None });
    }
    // Direct IVs first: `next_j = cur_j + s`, s > 0.
    let mut ivs: Vec<(usize, i64, i64)> = Vec::new();
    for (pi, r) in recs.iter().enumerate() {
        if let Some((v0, k, s)) = r {
            if *k == pi && *s > 0 {
                ivs.push((pi, *v0, *s));
            }
        }
    }
    // Delayed copies: `next_j = cur_k + d` — param_j_i = IV_k_{i-1} + d,
    // valid only when the entry value matches v0_k - s_k + d.
    for (pi, r) in recs.iter().enumerate() {
        let Some((v0, k, d)) = r else { continue };
        if *k == pi || ivs.iter().any(|&(i, _, _)| i == pi) {
            continue;
        }
        if let Some(&(_, vk, sk)) = ivs.iter().find(|&&(i, _, _)| i == *k) {
            if vk - sk + d == *v0 {
                ivs.push((pi, *v0, sk));
            }
        }
    }
    if ivs.is_empty() {
        dbg("iv");
        return None;
    }
    // Bound: an in-loop brif on `icmp cc affine(iv), K` (const small),
    // one target inside the loop, one out — the bound test. Emit K
    // copies; the tail covers any shape that iterates further.
    let mut iters = None;
    for &b in &blocks {
        for i in func.layout.block_insts(b) {
            let InstructionData::IntCompare { args, cond, .. } = func.dfg.insts[i] else {
                continue;
            };
            if !matches!(cond, IntCC::UnsignedLessThan | IntCC::UnsignedGreaterThan | IntCC::UnsignedLessThanOrEqual | IntCC::UnsignedGreaterThanOrEqual | IntCC::Equal | IntCC::NotEqual) {
                continue;
            }
            // `iv(+d)` side vs a small constant — the iv may reach the
            // test through a relay latch param, so use `rec_expr`.
            let mut bound = None;
            for (a, c) in [(args[0], args[1]), (args[1], args[0])] {
                if let Some((k, _d)) = rec_expr(func, cfg, h, &inset, a, 6) {
                    if ivs.iter().any(|&(pi, _, _)| pi == k) {
                        if let Some(kb) = iconst(func, c) {
                            bound = Some(kb);
                        }
                    }
                }
            }
            let Some(k) = bound else {
                continue;
            };
            if !(2..=MAX_ITERS as i64).contains(&k) {
                continue;
            }
            // Its user brif must split in-loop/out-of-loop.
            let t = func.layout.last_inst(b)?;
            let InstructionData::Brif { arg, .. } = func.dfg.insts[t] else {
                continue;
            };
            if func.dfg.resolve_aliases(arg) != func.dfg.resolve_aliases(func.dfg.inst_results(i).first().copied()?) {
                continue;
            }
            let dests = func.dfg.insts[t].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            let [d0, d1] = dests[..] else {
                continue;
            };
            let in0 = inset.contains(&d0.block(&func.dfg.value_lists));
            let in1 = inset.contains(&d1.block(&func.dfg.value_lists));
            if in0 == in1 {
                continue;
            }
            iters = Some(k as usize);
        }
    }
    let Some(iters) = iters else {
        dbg("bound");
        return None;
    };
    // LCSSA cone: cloned exits must not reach a block that references a
    // loop-defined value outside an edge arg — clone paths aren't
    // dominated by the original header, so a free-var use (postcard's
    // `v588 -> v544` alias into the serialize loop's merge) would lose
    // its def. Edge args on in-loop terminators are the sanctioned way
    // out; a free use downstream makes the loop ineligible.
    {
        let mut wl: Vec<Block> = Vec::new();
        let mut seen: FxHashSet<Block> = FxHashSet::default();
        for &b in &blocks {
            let Some(t) = func.layout.last_inst(b) else {
                continue;
            };
            for bc in func.dfg.insts[t]
                .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            {
                let tb = bc.block(&func.dfg.value_lists);
                if !inset.contains(&tb) && seen.insert(tb) {
                    wl.push(tb);
                }
            }
        }
        let mut budget = 64;
        let leaks = |func: &Function, v: Value| -> bool {
            let v = func.dfg.resolve_aliases(v);
            match func.dfg.value_def(v) {
                cranelift_codegen::ir::ValueDef::Param(pb, _) => inset.contains(&pb),
                cranelift_codegen::ir::ValueDef::Result(ii, _) => {
                    func.layout.inst_block(ii).is_some_and(|pb| inset.contains(&pb))
                }
                _ => false,
            }
        };
        while let Some(x) = wl.pop() {
            if budget == 0 {
                dbg("cone-budget");
                return None;
            }
            budget -= 1;
            for i in func.layout.block_insts(x) {
                if func.dfg.inst_args(i).iter().any(|&a| leaks(func, a)) {
                    dbg("leak-arg");
                    return None;
                }
                for bc in func.dfg.insts[i]
                    .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    if bc
                        .args(&func.dfg.value_lists)
                        .any(|a| matches!(a, BlockArg::Value(v) if leaks(func, v)))
                    {
                        dbg("leak-edge");
                        return None;
                    }
                    let tb = bc.block(&func.dfg.value_lists);
                    if seen.insert(tb) {
                        wl.push(tb);
                    }
                }
            }
        }
    }
    dbg(&format!("plan iters={iters}"));
    Some(Plan {
        h,
        blocks,
        ivs,
        iters,
        entries,
    })
}

/// Retarget every `target == from` BlockCall inside inst `i` to `to`
/// (args unchanged).
fn retarget(func: &mut Function, i: Inst, from: Block, to: Block) {
    let data = func.dfg.insts[i];
    let fix = |func: &mut Function, bc: BlockCall| -> BlockCall {
        if bc.block(&func.dfg.value_lists) == from {
            let args: Vec<BlockArg> = bc.args(&func.dfg.value_lists).collect();
            BlockCall::new(to, args.iter().copied(), &mut func.dfg.value_lists)
        } else {
            bc
        }
    };
    match data {
        InstructionData::Jump { opcode, destination } => {
            let d = fix(func, destination);
            func.dfg.insts[i] = InstructionData::Jump { opcode, destination: d };
        }
        InstructionData::Brif { opcode, arg, blocks } => {
            let t = fix(func, blocks[0]);
            let e = fix(func, blocks[1]);
            func.dfg.insts[i] = InstructionData::Brif { opcode, arg, blocks: [t, e] };
        }
        _ => {}
    }
}

fn apply(func: &mut Function, plan: &Plan) {
    let iters = plan.iters;
    // Clone blocks/insts per copy. cmap[i][b] = copy i's clone of b.
    let mut cmap: Vec<FxHashMap<Block, Block>> = Vec::new();
    for i in 0..iters {
        let mut bmap: FxHashMap<Block, Block> = FxHashMap::default();
        for &b in &plan.blocks {
            let nb = func.dfg.make_block();
            func.layout.insert_block(nb, plan.blocks.last().copied().unwrap_or(plan.h));
            bmap.insert(b, nb);
        }
        let mut vmap: FxHashMap<Value, Value> = FxHashMap::default();
        for &b in &plan.blocks {
            let nb = bmap[&b];
            for (pi, &pv) in func.dfg.block_params(b).to_vec().iter().enumerate() {
                let ty = func.dfg.value_type(pv);
                let nv = func.dfg.append_block_param(nb, ty);
                if !(b == plan.h && plan.ivs.iter().any(|&(j, _, _)| j == pi)) {
                    vmap.insert(pv, nv);
                }
            }
            if b == plan.h {
                // Each IV is constant in this copy — an iconst replaces
                // the (still-present, unused) param slot.
                for &(pi, v0, s) in &plan.ivs {
                    let ty = func.dfg.value_type(func.dfg.block_params(b)[pi]);
                    let data = InstructionData::UnaryImm {
                        opcode: Opcode::Iconst,
                        imm: cranelift_codegen::ir::immediates::Imm64::new(v0 + i as i64 * s),
                    };
                    let ci = func.dfg.make_inst(data);
                    func.dfg.make_inst_results(ci, ty);
                    func.layout.append_inst(ci, nb);
                    let cv = func.dfg.first_result(ci);
                    vmap.insert(func.dfg.block_params(b)[pi], cv);
                }
            }
            for ii in func.layout.block_insts(b).collect::<Vec<_>>() {
                let src = func.dfg.insts[ii];
                let data = {
                    let mut m = Vm {
                        func,
                        vmap: &vmap,
                        bmap: &bmap,
                    };
                    src.map(&mut m)
                };
                let ni = func.dfg.make_inst(data);
                let ctv = func.dfg.ctrl_typevar(ii);
                func.set_srcloc(ni, func.srcloc(ii));
                func.dfg.make_inst_results(ni, ctv);
                func.layout.append_inst(ni, nb);
                for (&o, &nv) in func
                    .dfg
                    .inst_results(ii)
                    .iter()
                    .zip(func.dfg.inst_results(ni).iter())
                {
                    vmap.insert(func.dfg.resolve_aliases(o), nv);
                }
            }
        }
        cmap.push(bmap);
    }
    // Back-edges: copy i's edges to cmap[i][h] → next copy's h (or the
    // original h for the last copy — the post-bound tail).
    for i in 0..iters {
        let next = if i + 1 < iters { cmap[i + 1][&plan.h] } else { plan.h };
        for &b in &plan.blocks {
            let nb = cmap[i][&b];
            for ii in func.layout.block_insts(nb).collect::<Vec<_>>() {
                retarget(func, ii, cmap[i][&plan.h], next);
            }
        }
    }
    // Entry edges: out-of-loop preds of h now enter copy 0's header.
    for &ei in &plan.entries {
        retarget(func, ei, plan.h, cmap[0][&plan.h]);
    }
}

pub fn run(func: &mut Function, an: &mut crate::pipeline::Analysis) -> usize {
    if !pliron_cfg::env_is("PLIRON_BUNROLL", "1") {
        return 0;
    }
    let mut plans = Vec::new();
    {
        let crate::pipeline::CfgDtLa { cfg, dt, la } = an.cfg_dt_la(func);
        tracing::debug!(target: "pliron::bunroll", "run: {} loops", la.loops().count());
        for lp in la.loops().take(16) {
            if let Some(p) = detect(func, cfg, dt, la, lp) {
                plans.push(p);
            }
        }
    }
    let n = plans.len();
    for p in plans {
        apply(func, &p);
    }
    n
}
