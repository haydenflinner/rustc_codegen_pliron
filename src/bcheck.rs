//! Bounds-check loop versioning.
//!
//! A `for i in 0..n { ... a[idx] ... }` loop checks `idx < len` every
//! iteration. When `idx` is affine in the loop counter, its last-iteration
//! value `hi` can be computed in the preheader: `hi < len` implies every
//! iteration's check passes, so the loop is cloned and the checks in the
//! clone jump straight to their in-bounds successor. The original stays as
//! the slow path and still panics on the same iteration, so this is exact.
//!
//! `hi` is evaluated in double-width arithmetic (i128 for i64 indices): a
//! wrapped recurrence or empty/degenerate trip range makes `hi` huge and the
//! guard fails to the slow path — no separate overflow/domain tests needed.
//!
//! Monotonicity: idx = b + i*m for i in [0, T). As an unsigned machine value
//! m is >= 0 in true-arithmetic terms, so idx is non-decreasing in i and
//! idx(T-1) = hi is the max. If hi < len fits the index type, no
//! intermediate i*m or b + i*m wrapped either, so every iteration's
//! machine-computed idx equals its true value and is < len.

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::instructions::InstructionMapper;
use cranelift_codegen::ir::{
    types, Block, BlockArg, BlockCall, Constant, DynamicStackSlot, ExceptionTable,
    ExceptionTableItem, FuncRef, Function, GlobalValue, Immediate, Inst, InstBuilder,
    InstructionData, JumpTable, JumpTableData, Opcode, SigRef, StackSlot, Type, Value, ValueList,
};
use cranelift_codegen::loop_analysis::{Loop, LoopAnalysis};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

const MAX_LOOPS: usize = 8;
const MAX_BODY: usize = 48;
const MAX_CHECKS: usize = 8;
const MAX_AFFINE_DEPTH: usize = 4;
const MAX_VERSIONS: usize = 4;
const MAX_REGION_EXTRA: usize = 32;

/// `v * k` scaled linear term, emitted widened to the guard type.
#[derive(Clone, Copy)]
struct Term {
    v: Lin,
    k: i64,
}

#[derive(Clone, Copy)]
enum Lin {
    K(i64),
    V(Value),
    /// Entry value of header param N (the guard block's param N).
    P(usize),
}

/// `idx = m * i + sum(b)` over true integers (`i` = counter value).
struct Aff {
    m: Lin,
    b: SmallVec<[Term; 2]>,
}

#[derive(Clone, Copy)]
struct Edge {
    inst: Inst,
    slot: usize,
}

/// A `P + step` backedge update; step may be a runtime value or a constant.
#[derive(Clone, Copy)]
enum Step {
    V(Value),
    K(i64),
}

struct LoopInfo {
    /// Header param stepping `+1` per iteration.
    iv: Value,
    /// Param index of `iv`; the entry value is the guard block's param.
    iv0: usize,
    /// The iv's entry value when constant and identical on all entry edges.
    iv0k: Option<i64>,
    /// Loop-invariant exit bound.
    bound: Value,
    /// Last executed counter value: `bound - adj`.
    adj: i64,
    /// Stepped header params: `P_i = init + i*step` (init = entry param idx).
    stepped: FxHashMap<Value, (usize, Step)>,
    /// Guard block; `block_params(g)[i]` is the entry value of param i.
    g: Option<Block>,
    /// Header block.
    h: Block,
    /// Backedge arg for each header param (None = unanalyzable).
    bargs: Vec<Option<Value>>,
}

/// A `brif` whose in-bounds edge is provably always taken given `hi < len`.
struct Chk {
    inst: Inst,
    safe_slot: usize,
    aff: Aff,
    le: bool,
    len: Value,
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    let i = func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()?;
    match func.dfg.insts[i] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

fn iconst_masked(pos: &mut FuncCursor, t: Type, k: i64) -> Value {
    if t == types::I128 {
        let lo = pos.ins().iconst(types::I64, k);
        let hi = pos.ins().iconst(types::I64, k >> 63);
        return pos.ins().iconcat(hi, lo);
    }
    let mask = if t.bits() < 64 {
        (1i64 << t.bits()) - 1
    } else {
        -1
    };
    pos.ins().iconst(t, k & mask)
}

fn def_block(func: &Function, v: Value) -> Option<Block> {
    use cranelift_codegen::ir::ValueDef;
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => func.layout.inst_block(i),
        ValueDef::Param(b, _) => Some(b),
        ValueDef::Union(..) => None,
    }
}

fn edges_to(func: &Function, cfg: &ControlFlowGraph, h: Block) -> Vec<Edge> {
    let mut out = Vec::new();
    for p in cfg.pred_iter(h) {
        for (slot, bc) in func.dfg.insts[p.inst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
            .iter()
            .enumerate()
        {
            if bc.block(&func.dfg.value_lists) == h {
                out.push(Edge { inst: p.inst, slot });
            }
        }
    }
    out
}

fn edge_arg(func: &Function, e: &Edge, idx: usize) -> Option<Value> {
    let bc = func.dfg.insts[e.inst]
        .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[e.slot];
    match bc.args(&func.dfg.value_lists).nth(idx)? {
        BlockArg::Value(v) => Some(func.dfg.resolve_aliases(v)),
        _ => None,
    }
}

/// `arg` is `P + step` for header param `P`: the per-iteration delta.
fn step_of(func: &Function, p: Value, arg: Value) -> Option<Step> {
    let arg = func.dfg.resolve_aliases(arg);
    let i = func.dfg.value_def(arg).inst()?;
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } => {
            let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
            if a == p {
                Some(iconst(func, b).map(Step::K).unwrap_or(Step::V(b)))
            } else if b == p {
                Some(iconst(func, a).map(Step::K).unwrap_or(Step::V(a)))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `v` is loop-invariant: defined outside the body, or a header param whose
/// backedge passes it through unchanged.
fn invariant(
    func: &Function,
    info: &LoopInfo,
    body: &FxHashSet<Block>,
    v: Value,
) -> bool {
    let v = func.dfg.resolve_aliases(v);
    // Constants are trivially invariant no matter where the iconst sits.
    if iconst(func, v).is_some() {
        return true;
    }
    match def_block(func, v) {
        Some(b) if b == info.h => {
            if let cranelift_codegen::ir::ValueDef::Param(_, n) = func.dfg.value_def(v)
                && let Some(Some(ba)) = info.bargs.get(n)
            {
                return func.dfg.resolve_aliases(*ba) == v;
            }
            false
        }
        Some(b) => !body.contains(&b),
        None => true,
    }
}

/// Decompose `v` as `m*i + sum(b)` over the counting iv. Each `b` term is
/// `(lin, factor)` so `imul` by a constant scales through.
fn affine(
    func: &Function,
    info: &LoopInfo,
    body: &FxHashSet<Block>,
    depth: usize,
    v: Value,
) -> Option<Aff> {
    if depth == 0 {
        return None;
    }
    let v = func.dfg.resolve_aliases(v);
    if v == info.iv {
        return Some(Aff {
            m: Lin::K(1),
            b: [Term {
                v: Lin::P(info.iv0),
                k: 1,
            }]
            .into_iter()
            .collect(),
        });
    }
    if let Some(&(init, step)) = info.stepped.get(&v) {
        let m = match step {
            Step::K(k) => Lin::K(k),
            Step::V(s) => Lin::V(s),
        };
        return Some(Aff {
            m,
            b: [Term {
                v: Lin::P(init),
                k: 1,
            }]
            .into_iter()
            .collect(),
        });
    }
    let i = func.dfg.value_def(v).inst()?;
    match func.dfg.insts[i] {
        InstructionData::Binary {
            opcode: Opcode::Iadd,
            args: [a, b],
        } => {
            let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
            for (x, y) in [(a, b), (b, a)] {
                if let Some(mut af) = affine(func, info, body, depth - 1, x)
                    && invariant(func, info, body, y)
                {
                    af.b.push(Term {
                        v: Lin::V(y),
                        k: 1,
                    });
                    return Some(af);
                }
            }
            None
        }
        InstructionData::Binary {
            opcode: Opcode::Imul,
            args: [a, b],
        } => {
            let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
            for (x, y) in [(a, b), (b, a)] {
                // counter * invariant: idx = i*y + iv0*y; the iv0*y term is
                // a V*V product we can only express when iv0 is a constant.
                if x == info.iv && invariant(func, info, body, y) {
                    let k = info.iv0k?;
                    return Some(Aff {
                        m: Lin::V(y),
                        b: [Term {
                            v: Lin::V(y),
                            k,
                        }]
                        .into_iter()
                        .collect(),
                    });
                }
                // affine * const: scale m and every b term.
                if let Some(k) = iconst(func, y)
                    && let Some(mut af) = affine(func, info, body, depth - 1, x)
                {
                    // A runtime m stays linear only when it scales a const.
                    if let Lin::K(c) = af.m {
                        af.m = Lin::K(c.wrapping_mul(k));
                        for t in af.b.iter_mut() {
                            t.k = t.k.wrapping_mul(k);
                        }
                        return Some(af);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Same-function remapper for cloning the loop body. Blocks inside the loop
/// map to their clones; everything else is the identity.
struct CloneMap<'a> {
    func: &'a mut Function,
    vmap: &'a FxHashMap<Value, Value>,
    bmap: &'a FxHashMap<Block, Block>,
}

impl CloneMap<'_> {
    fn mv(&self, v: Value) -> Value {
        let v = self.func.dfg.resolve_aliases(v);
        self.vmap.get(&v).copied().unwrap_or(v)
    }
}

impl InstructionMapper for CloneMap<'_> {
    fn map_value(&mut self, value: Value) -> Value {
        self.mv(value)
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
        let def = self.map_block_call(self.func.dfg.jump_tables[jt].default_block());
        let calls: Vec<BlockCall> = self.func.dfg.jump_tables[jt].as_slice().to_vec();
        let table: SmallVec<[BlockCall; 8]> = calls
            .iter()
            .map(|bc| self.map_block_call(*bc))
            .collect();
        self.func
            .dfg
            .jump_tables
            .push(JumpTableData::new(def, &table))
    }
    fn map_exception_table(&mut self, et: ExceptionTable) -> ExceptionTable {
        let (sig, nrv, orig): (_, _, SmallVec<[ExceptionTableItem; 4]>) = {
            let e = &self.func.dfg.exception_tables[et];
            (e.signature(), *e.normal_return(), e.items().collect())
        };
        let nr = self.map_block_call(nrv);
        let items: SmallVec<[ExceptionTableItem; 4]> = orig
            .iter()
            .map(|item| match *item {
                ExceptionTableItem::Tag(t, bc) => {
                    ExceptionTableItem::Tag(t, self.map_block_call(bc))
                }
                ExceptionTableItem::Default(bc) => {
                    ExceptionTableItem::Default(self.map_block_call(bc))
                }
                ExceptionTableItem::Context(v) => ExceptionTableItem::Context(self.mv(v)),
            })
            .collect();
        self.func
            .dfg
            .exception_tables
            .push(cranelift_codegen::ir::ExceptionTableData::new(sig, nr, items))
    }
    fn map_block_call(&mut self, bc: BlockCall) -> BlockCall {
        let blk = bc.block(&self.func.dfg.value_lists);
        let nb = self.bmap.get(&blk).copied().unwrap_or(blk);
        let args: Vec<BlockArg> = bc
            .args(&self.func.dfg.value_lists)
            .map(|a| match a {
                BlockArg::Value(v) => BlockArg::Value(self.mv(v)),
                a => a,
            })
            .collect();
        BlockCall::new(nb, args.iter().copied(), &mut self.func.dfg.value_lists)
    }
    fn map_block(&mut self, block: Block) -> Block {
        self.bmap.get(&block).copied().unwrap_or(block)
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

fn run_loop(func: &mut Function, cfg: &ControlFlowGraph, dt: &DominatorTree, la: &LoopAnalysis, lp: Loop, name: &str, seen: &mut FxHashSet<Block>) -> usize {
    let debug = std::env::var_os("PLIRON_BCHECK_DEBUG").is_some();
    let h = la.loop_header(lp);
    if !seen.insert(h) {
        return 0;
    }
    macro_rules! bail {
        ($why:expr) => {{
            if debug {
                eprintln!("bcheck {name} block{}: {}", h.as_u32(), $why);
            }
            return 0;
        }};
    }
    if !dt.is_reachable(h) {
        bail!("unreachable");
    }
    let body: FxHashSet<Block> = func
        .layout
        .blocks()
        .filter(|&b| la.is_in_loop(b, lp))
        .collect();
    let n_insts: usize = body
        .iter()
        .map(|&b| func.layout.block_insts(b).count())
        .sum();
    if n_insts > MAX_BODY {
        bail!("too big");
    }
    // The clone set is the loop body plus every block fully enclosed by it
    // (all CFG preds inside): cold trap blocks and private exits can freely
    // use body values, so sharing them between versions would break
    // dominance. Blocks with a pred outside the region stay shared — but
    // only when nothing past them uses a region value: a region edge into a
    // shared block is cloned too, and the clone path bypasses the value's
    // def, breaking dominance of uses reachable through the shared block.
    // Any boundary target whose non-region cone uses a region value is
    // absorbed as well.
    let mut region: FxHashSet<Block> = body.clone();
    loop {
        let mut grew = false;
        for b in func.layout.blocks() {
            let mut it = cfg.pred_iter(b);
            if region.contains(&b) || it.next().is_none() {
                continue;
            }
            if cfg.pred_iter(b).all(|p| region.contains(&p.block)) {
                region.insert(b);
                grew = true;
            }
        }
        if !grew {
            let mut rvals: FxHashSet<Value> = FxHashSet::default();
            for &r in &region {
                rvals.extend(func.dfg.block_params(r).iter().copied());
                for i in func.layout.block_insts(r) {
                    rvals.extend(func.dfg.inst_results(i).iter().copied());
                }
            }
            let uses_region = |func: &Function, x: Block| {
                for i in func.layout.block_insts(x) {
                    if func
                        .dfg
                        .inst_args(i)
                        .iter()
                        .any(|&v| rvals.contains(&func.dfg.resolve_aliases(v)))
                    {
                        return true;
                    }
                    for bc in func.dfg.insts[i].branch_destination(
                        &func.dfg.jump_tables,
                        &func.dfg.exception_tables,
                    ) {
                        if bc.args(&func.dfg.value_lists).any(|a| {
                            matches!(a, BlockArg::Value(v) if {
                                rvals.contains(&func.dfg.resolve_aliases(v))
                            })
                        }) {
                            return true;
                        }
                    }
                    if let InstructionData::TryCall { exception, .. }
                    | InstructionData::TryCallIndirect { exception, .. } =
                        func.dfg.insts[i]
                        && func.dfg.exception_tables[exception].items().any(|it| {
                            matches!(it, ExceptionTableItem::Context(v) if {
                                rvals.contains(&func.dfg.resolve_aliases(v))
                            })
                        })
                    {
                        return true;
                    }
                }
                false
            };
            let mut targets: Vec<Block> = Vec::new();
            for &r in &region {
                let Some(t) = func.layout.last_inst(r) else {
                    continue;
                };
                for bc in func.dfg.insts[t].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    let d = bc.block(&func.dfg.value_lists);
                    if !region.contains(&d) && !targets.contains(&d) {
                        targets.push(d);
                    }
                }
            }
            for t in targets {
                let mut seen: FxHashSet<Block> = FxHashSet::default();
                let mut stack = vec![t];
                let mut leak = false;
                while let Some(x) = stack.pop() {
                    if region.contains(&x) || !seen.insert(x) {
                        continue;
                    }
                    if uses_region(func, x) {
                        leak = true;
                        break;
                    }
                    if let Some(t) = func.layout.last_inst(x) {
                        for bc in func.dfg.insts[t].branch_destination(
                            &func.dfg.jump_tables,
                            &func.dfg.exception_tables,
                        ) {
                            stack.push(bc.block(&func.dfg.value_lists));
                        }
                    }
                }
                if leak {
                    region.insert(t);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        if region.len() - body.len() > MAX_REGION_EXTRA {
            bail!("region too big");
        }
    }
    let edges = edges_to(func, cfg, h);
    let mut entries: Vec<Edge> = Vec::new();
    let mut back = None;
    for e in edges {
        let pb = func.layout.inst_block(e.inst).unwrap();
        if body.contains(&pb) {
            if back.replace(e).is_some() {
                bail!("multiple backedges");
            }
        } else {
            entries.push(e);
        }
    }
    let (Some(back), false) = (back, entries.is_empty()) else {
        bail!("no entry/back edge");
    };
    let latch = func.layout.inst_block(back.inst).unwrap();
    let params = func.dfg.block_params(h).to_vec();
    let bargs: Vec<Option<Value>> = params
        .iter()
        .enumerate()
        .map(|(i, _)| edge_arg(func, &back, i))
        .collect();

    // Bound test `brif (icmp cc a, bound)` at `site` whose continue edge is
    // `cs`: for the latch that's the edge back to h; for the header it's the
    // edge staying in the body. Returns (bound, adj) where the last counter
    // value entering the body is `bound - adj`.
    let bound_test = |site: Block, p: Value, ba: Value| -> Option<(Value, i64)> {
        let i = func.layout.last_inst(site)?;
        if func.dfg.insts[i].opcode() != Opcode::Brif {
            return None;
        }
        let dests = func.dfg.insts[i].branch_destination(
            &func.dfg.jump_tables,
            &func.dfg.exception_tables,
        );
        let in_latch = site == latch;
        let cs = if in_latch {
            dests
                .iter()
                .position(|bc| bc.block(&func.dfg.value_lists) == h)?
        } else {
            let mut cs = None;
            for (s, bc) in dests.iter().enumerate() {
                if body.contains(&bc.block(&func.dfg.value_lists)) {
                    if cs.replace(s).is_some() {
                        return None;
                    }
                }
            }
            cs?
        };
        if body.contains(&dests[1 - cs].block(&func.dfg.value_lists)) {
            return None;
        }
        let InstructionData::Brif { arg: c, .. } = func.dfg.insts[i] else {
            return None;
        };
        let ci = func.dfg.value_def(func.dfg.resolve_aliases(c)).inst()?;
        let InstructionData::IntCompare {
            opcode: Opcode::Icmp,
            cond,
            args: [x, y],
        } = func.dfg.insts[ci]
        else {
            return None;
        };
        // Normalize to the continue-condition.
        let cond = if cs == 0 { cond } else { cond.complement() };
        for (a, b, cc) in [(x, y, cond), (y, x, cond.swap_args())] {
            let (a, b) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(b));
            // At the latch, testing `next` (ba = iv+1) bounds the next trip:
            // `next < bound` -> last iv is bound-1. Testing `p` bounds the
            // *following* iteration, so the body can still see iv = bound.
            // At the header the test gates the body directly.
            let adj = match (cc, in_latch, a == ba, a == p) {
                (IntCC::UnsignedLessThan, true, true, _) => 1,
                (IntCC::UnsignedLessThanOrEqual, true, true, _) => 0,
                (IntCC::UnsignedLessThan, true, false, true) => 0,
                (IntCC::UnsignedLessThanOrEqual, true, false, true) => -1,
                (IntCC::UnsignedLessThan, false, _, true) => 1,
                (IntCC::UnsignedLessThanOrEqual, false, _, true) => 0,
                _ => continue,
            };
            return Some((b, adj));
        }
        None
    };

    // Find the counting param: stepped `+1`, with a bound test at the latch
    // or the header.
    let mut info: Option<LoopInfo> = None;
    'iv: for (idx, &p) in params.iter().enumerate() {
        let ty = func.dfg.value_type(p);
        if !ty.is_int() || ty.bits() > 64 {
            continue;
        }
        let Some(ba) = bargs[idx] else { continue };
        if !matches!(step_of(func, p, ba), Some(Step::K(1))) {
            continue;
        }
        // iv's entry value is a constant only when all entry edges agree.
        let mut iv0k = None;
        let mut same = true;
        for e in &entries {
            match edge_arg(func, e, idx).and_then(|v| iconst(func, v)) {
                Some(c) if iv0k.is_none_or(|k| k == c) => iv0k = Some(c),
                _ => {
                    same = false;
                    break;
                }
            }
        }
        if !same {
            iv0k = None;
        }
        for site in [latch, h] {
            let Some((bound, adj)) = bound_test(site, p, ba) else {
                continue;
            };
            let mut lo = LoopInfo {
                iv: p,
                iv0: idx,
                iv0k,
                bound,
                adj,
                stepped: FxHashMap::default(),
                g: None,
                h,
                bargs: bargs.clone(),
            };
            if !invariant(func, &lo, &body, bound) {
                continue;
            }
            for (j, &q) in params.iter().enumerate() {
                if j == idx {
                    continue;
                }
                let Some(qa) = bargs[j] else { continue };
                let Some(s) = step_of(func, q, qa) else { continue };
                let inv = match s {
                    Step::K(_) => true,
                    Step::V(sv) => invariant(func, &lo, &body, sv),
                };
                if inv {
                    lo.stepped.insert(q, (j, s));
                }
            }
            info = Some(lo);
            break 'iv;
        }
    }
    let Some(mut info) = info else { bail!("no counting iv") };

    // Foldable checks: `brif (icmp cc idx, len)` where idx is affine in the
    // counter, len invariant, and the unsafe edge is a cold (panic) block.
    let mut checks: Vec<Chk> = Vec::new();
    'blk: for &b in &body {
        let Some(t) = func.layout.last_inst(b) else { continue };
        if func.dfg.insts[t].opcode() != Opcode::Brif {
            continue;
        }
        let InstructionData::Brif { arg: c, .. } = func.dfg.insts[t] else {
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
        for (a, bb, cc) in [(x, y, cond), (y, x, cond.swap_args())] {
            let (a, bb) = (func.dfg.resolve_aliases(a), func.dfg.resolve_aliases(bb));
            let (safe_true, le) = match cc {
                IntCC::UnsignedLessThan => (true, false),
                IntCC::UnsignedLessThanOrEqual => (true, true),
                IntCC::UnsignedGreaterThanOrEqual => (false, false),
                IntCC::UnsignedGreaterThan => (false, true),
                _ => continue,
            };
            if !invariant(func, &info, &body, bb) {
                continue;
            }
            let Some(aff) = affine(func, &info, &body, MAX_AFFINE_DEPTH, a) else {
                continue;
            };
            let dests = func.dfg.insts[t].branch_destination(
                &func.dfg.jump_tables,
                &func.dfg.exception_tables,
            );
            let safe_slot = if safe_true { 0 } else { 1 };
            let bad = dests[1 - safe_slot].block(&func.dfg.value_lists);
            if !func.layout.is_cold(bad) {
                continue;
            }
            checks.push(Chk {
                inst: t,
                safe_slot,
                aff,
                le,
                len: bb,
            });
            continue 'blk;
        }
        if checks.len() >= MAX_CHECKS {
            break;
        }
    }
    if checks.is_empty() {
        bail!("no foldable checks");
    }

    // Guard/preheader block g takes h's params verbatim; every entry edge
    // is retargeted to it, so its params are the canonical entry values.
    let ity = func.dfg.value_type(info.iv);
    let wty = if ity == types::I32 {
        types::I64
    } else {
        types::I128
    };
    let g = func.dfg.make_block();
    let pb = func.layout.inst_block(entries[0].inst).unwrap();
    func.layout.insert_block_after(g, pb);
    let gtys: Vec<Type> = func
        .dfg
        .block_params(h)
        .iter()
        .map(|&p| func.dfg.value_type(p))
        .collect();
    let gp: Vec<Value> = gtys
        .iter()
        .map(|&t| func.dfg.append_block_param(g, t))
        .collect();
    info.g = Some(g);

    // A loop-invariant value usable at g: constants are rematerialized, an
    // unchanged header param becomes its entry value (g's param).
    let gval = |info: &LoopInfo, gp: &[Value], pos: &mut FuncCursor, v: Value| -> Value {
        let v = pos.func.dfg.resolve_aliases(v);
        let v = if let Some(b) = def_block(pos.func, v)
            && b == info.h
            && let cranelift_codegen::ir::ValueDef::Param(_, n) = pos.func.dfg.value_def(v)
        {
            gp[n]
        } else {
            v
        };
        match iconst(pos.func, v) {
            Some(k) => iconst_masked(pos, ity, k),
            None => v,
        }
    };
    let mut pos = FuncCursor::new(func).at_bottom(g);
    let zext = |pos: &mut FuncCursor, v: Value| -> Value {
        if ity == wty { v } else { pos.ins().uextend(wty, v) }
    };
    let boundv = gval(&info, &gp, &mut pos, info.bound);
    let iv0v = gp[info.iv0];
    let t = {
        let d = pos.ins().isub(boundv, iv0v);
        match info.adj.cmp(&0) {
            core::cmp::Ordering::Equal => d,
            core::cmp::Ordering::Greater => {
                let k = pos.ins().iconst(ity, info.adj);
                pos.ins().isub(d, k)
            }
            core::cmp::Ordering::Less => {
                let k = pos.ins().iconst(ity, -info.adj);
                pos.ins().iadd(d, k)
            }
        }
    };
    let tw = zext(&mut pos, t);
    let mut ok: Option<Value> = None;
    for c in &checks {
        let m = match c.aff.m {
            Lin::K(k) => iconst_masked(&mut pos, wty, k),
            Lin::V(v) => {
                let v = gval(&info, &gp, &mut pos, v);
                zext(&mut pos, v)
            }
            Lin::P(i) => zext(&mut pos, gp[i]),
        };
        let mut hi = pos.ins().imul(m, tw);
        for tm in &c.aff.b {
            let v = match tm.v {
                Lin::K(k) => iconst_masked(&mut pos, wty, k),
                Lin::V(v) => {
                    let v = gval(&info, &gp, &mut pos, v);
                    zext(&mut pos, v)
                }
                Lin::P(i) => zext(&mut pos, gp[i]),
            };
            let term = if tm.k == 1 {
                v
            } else {
                let k = iconst_masked(&mut pos, wty, tm.k);
                pos.ins().imul(v, k)
            };
            hi = pos.ins().iadd(hi, term);
        }
        let lenv = gval(&info, &gp, &mut pos, c.len);
        let lenw = zext(&mut pos, lenv);
        let cc = if c.le {
            IntCC::UnsignedLessThanOrEqual
        } else {
            IntCC::UnsignedLessThan
        };
        let g1 = pos.ins().icmp(cc, hi, lenw);
        ok = Some(match ok {
            None => g1,
            Some(o) => pos.ins().band(o, g1),
        });
    }
    let ok = ok.unwrap();

    // Clone the region blocks in RPO so in-loop defs precede their uses.
    let rpo = {
        let mut seen = FxHashSet::default();
        let mut post = Vec::new();
        let mut stack = vec![(h, false)];
        while let Some((b, done)) = stack.pop() {
            if done {
                post.push(b);
                continue;
            }
            if !seen.insert(b) {
                continue;
            }
            stack.push((b, true));
            if let Some(t) = func.layout.last_inst(b) {
                for bc in func.dfg.insts[t].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                ) {
                    let d = bc.block(&func.dfg.value_lists);
                    if region.contains(&d) {
                        stack.push((d, false));
                    }
                }
            }
        }
        post.reverse();
        post
    };
    let mut bmap: FxHashMap<Block, Block> = FxHashMap::default();
    let mut vmap: FxHashMap<Value, Value> = FxHashMap::default();
    for &b in &rpo {
        let nb = func.dfg.make_block();
        func.layout.append_block(nb);
        if func.layout.is_cold(b) {
            func.layout.set_cold(nb);
        }
        let bparams: Vec<Value> = func.dfg.block_params(b).to_vec();
        for &p in &bparams {
            let ty = func.dfg.value_type(p);
            let np = func.dfg.append_block_param(nb, ty);
            vmap.insert(p, np);
        }
        bmap.insert(b, nb);
    }
    let fold: FxHashMap<Inst, usize> = checks
        .iter()
        .enumerate()
        .map(|(i, c)| (c.inst, i))
        .collect();
    for &b in &rpo {
        let nb = bmap[&b];
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        for i in insts {
            if let Some(&ci) = fold.get(&i) {
                let bc = func.dfg.insts[i].branch_destination(
                    &func.dfg.jump_tables,
                    &func.dfg.exception_tables,
                )[checks[ci].safe_slot];
                let nbc = {
                    let mut cm = CloneMap {
                        func,
                        vmap: &vmap,
                        bmap: &bmap,
                    };
                    cm.map_block_call(bc)
                };
                let args: Vec<BlockArg> = nbc.args(&func.dfg.value_lists).collect();
                let nblk = nbc.block(&func.dfg.value_lists);
                let mut pos = FuncCursor::new(func).at_bottom(nb);
                pos.ins().jump(nblk, &args);
                continue;
            }
            let src_data = func.dfg.insts[i];
            let data = {
                let mut cm = CloneMap {
                    func,
                    vmap: &vmap,
                    bmap: &bmap,
                };
                src_data.map(&mut cm)
            };
            let ni = func.dfg.make_inst(data);
            let ctv = func.dfg.ctrl_typevar(i);
            func.dfg.make_inst_results(ni, ctv);
            func.layout.append_inst(ni, nb);
            for (&o, &n) in func
                .dfg
                .inst_results(i)
                .iter()
                .zip(func.dfg.inst_results(ni).iter())
            {
                vmap.insert(func.dfg.resolve_aliases(o), n);
            }
        }
    }
    // Redirect every entry edge through the guard, keeping its args.
    for e in &entries {
        let args: Vec<BlockArg> = func.dfg.insts[e.inst]
            .branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                [e.slot]
            .args(&func.dfg.value_lists)
            .collect();
        let dfg = &mut func.dfg;
        let bc = &mut dfg.insts[e.inst].branch_destination_mut(
            &mut dfg.jump_tables,
            &mut dfg.exception_tables,
        )[e.slot];
        *bc = BlockCall::new(g, args.iter().copied(), &mut dfg.value_lists);
    }
    let gpargs: Vec<BlockArg> = gp.iter().map(|&v| BlockArg::from(v)).collect();
    let mut pos = FuncCursor::new(func).at_bottom(g);
    let hc = bmap[&h];
    pos.ins().brif(ok, hc, &gpargs, h, &gpargs);

    // Cloned panic blocks are unreachable now; drop anything that lost all
    // predecessors so stale blocks don't keep phantom CFG edges.
    let cfg2 = ControlFlowGraph::with_function(func);
    let dead: Vec<Block> = bmap
        .values()
        .copied()
        .filter(|&b| cfg2.pred_iter(b).next().is_none())
        .collect();
    for b in dead {
        func.layout.remove_block_and_insts(b);
    }
    seen.insert(hc);
    1
}

pub fn run(func: &mut Function, name: &str) -> usize {
    let debug = std::env::var_os("PLIRON_BCHECK_DEBUG").is_some();
    let mut n = 0;
    // Analyses go stale after each versioning, so recompute per iteration:
    // find the first loop that versions, then rescan.
    let mut seen: FxHashSet<Block> = FxHashSet::default();
    for _ in 0..MAX_VERSIONS {
        let cfg = ControlFlowGraph::with_function(func);
        let dt = DominatorTree::with_function(func, &cfg);
        let mut la = LoopAnalysis::new();
        la.compute(func, &cfg, &dt);
        let mut did = 0;
        for lp in la.loops().take(MAX_LOOPS) {
            let k = run_loop(func, &cfg, &dt, &la, lp, name, &mut seen);
            did += k;
            if k > 0 {
                break; // cfg/dt/la now stale; rescan
            }
        }
        n += did;
        if did == 0 {
            break;
        }
        if debug {
            eprintln!("bcheck {name}: versioned");
        }
    }
    n
}
