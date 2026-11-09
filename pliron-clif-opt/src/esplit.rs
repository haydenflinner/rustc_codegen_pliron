//! Edge specialization (`PLIRON_ESPLIT=1`, opt-in): clone a small
//! multi-pred merge block for a pred edge whose arm fact decides insts
//! the merged context cannot.
//!
//! UTF-8 decode + `is_ascii` is the shape:
//!
//! ```text
//! block66:
//!     v386 = uload8 p
//!     v474 = band v386, 128
//!     v508 = icmp ne v474, 0
//!     brif v508, block69, block91(v386, p')   // ASCII edge: v386 < 128
//! block159: ... jump block91(v143, q)        // 2-byte char
//! block91(c, ptr):
//!     v422 = icmp ult c, 128                 // is_ascii
//!     v423 = uextend.i64 v422
//!     v424 = iadd v423, count
//!     jump head(ptr, v424)
//! ```
//!
//! On the ASCII edge `c`'s range is `[0,128)` — the `icmp`/`uextend`
//! fold to `1` and the `iadd` becomes `count + 1`. The merged block
//! cannot fold them (the multibyte edges carry the full range), so the
//! fold needs a clone of `block91` for that edge alone.
//!
//! Per edge, the env is the edge's own arm fact (the `brif` condition
//! the edge is an arm of, incl. `band`-mask inverses) plus each arg's
//! [`Ranges`] interval. An edge qualifies when `jumpthread::eval` folds
//! at least one inst under it that the param-join env does not. The
//! clone is only reachable from that edge, so the fact is sound inside.
//! Dead params and iconsts are left to `deadp`/dce.

use std::cell::RefCell;

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::{CondCode, IntCC};
use cranelift_codegen::ir::{
    Block, BlockArg, BlockCall, Function, Inst, InstBuilder, InstructionData, Opcode, Value,
};
use rustc_hash::FxHashMap;

use crate::jumpthread;
use crate::pipeline::Analysis;
use crate::ranges::{Range, Ranges};

/// Most non-terminator insts a merge block may have and still clone.
const MAX_INSTS: usize = 8;
/// Most clones per function per run.
const MAX_CLONES: usize = 32;

fn umask(w: u32) -> u64 {
    if w >= 64 { u64::MAX } else { (1u64 << w) - 1 }
}

fn resolve(func: &Function, v: Value) -> Value {
    func.dfg.resolve_aliases(v)
}

/// `iconst` bits of `v`, masked to its type width.
fn konst(func: &Function, v: Value) -> Option<u64> {
    let v = resolve(func, v);
    let i = func.dfg.value_def(v).inst()?;
    if let InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } = func.dfg.insts[i] {
        let w = func.dfg.value_type(v).bits();
        Some(imm.bits() as u64 & umask(w))
    } else {
        None
    }
}

/// `(lo, hi)` unsigned bounds: overrides, then constants, then `Ranges`.
fn rg_get(
    func: &Function,
    rs: &RefCell<Ranges>,
    over: &FxHashMap<Value, (u64, u64)>,
    v: Value,
) -> Option<(u64, u64)> {
    let v = resolve(func, v);
    if let Some(&r) = over.get(&v) {
        return Some(r);
    }
    if let Some(k) = konst(func, v) {
        return Some((k, k));
    }
    if !func.dfg.value_is_valid(v) {
        return None;
    }
    let r: Range = rs.borrow_mut().def(func, v);
    if r.uhi < r.ulo {
        return None;
    }
    Some((r.ulo, r.uhi))
}

/// Intersect `v`'s override range with `[lo, hi]`.
fn rg_tighten(
    func: &Function,
    rs: &RefCell<Ranges>,
    over: &mut FxHashMap<Value, (u64, u64)>,
    v: Value,
    lo: u64,
    hi: u64,
) {
    let v = resolve(func, v);
    let w = func.dfg.value_type(v).bits();
    let m = umask(w);
    let (cl, ch) = rg_get(func, rs, over, v).unwrap_or((0, m));
    over.insert(v, (cl.max(lo).min(m), ch.min(hi)));
}

/// `band w, M`'s range fact inverted onto `w` under `cc (band, other)` —
/// `other` given as its own `(lo, hi)` bounds so a virtual `0` works.
fn band_inverse(
    func: &Function,
    rs: &RefCell<Ranges>,
    over: &mut FxHashMap<Value, (u64, u64)>,
    cc: IntCC,
    band_v: Value,
    other: (u64, u64),
) {
    let Some(i) = func.dfg.value_def(resolve(func, band_v)).inst() else {
        return;
    };
    let InstructionData::Binary { opcode: Opcode::Band, args } = func.dfg.insts[i] else {
        return;
    };
    let (w, mv) = (resolve(func, args[0]), resolve(func, args[1]));
    let Some(mask) = konst(func, mv).filter(|&m| m != 0) else {
        return;
    };
    let Some((_, wh)) = rg_get(func, rs, over, w) else {
        return;
    };
    let (ol, oh) = other;
    match cc {
        // `band w, M == 0`: every M bit is clear. When M's low bit is w's
        // top possible bit, w < lowbit(M) exactly.
        IntCC::Equal if oh == 0 => {
            let lb = mask & mask.wrapping_neg();
            if wh < lb << 1 {
                rg_tighten(func, rs, over, w, 0, lb - 1);
            }
        }
        // `band w, M == M`: every M bit set — `w = M | rest >= M`.
        IntCC::Equal if ol == mask && oh == mask => {
            rg_tighten(func, rs, over, w, mask, wh);
        }
        // `band w, M != 0` with single-bit M: w >= M.
        IntCC::NotEqual if oh == 0 && mask.is_power_of_two() => {
            rg_tighten(func, rs, over, w, mask, wh);
        }
        _ => {}
    }
}

/// Apply `cc(x, y)` edge fact to the override ranges of `x`, `y` and any
/// `band`-def operands.
fn refine_cmp(
    func: &Function,
    rs: &RefCell<Ranges>,
    over: &mut FxHashMap<Value, (u64, u64)>,
    cc: IntCC,
    x: Value,
    y: Value,
) {
    let (xl, xh) = rg_get(func, rs, over, x).unwrap_or((0, u64::MAX));
    let (yl, yh) = rg_get(func, rs, over, y).unwrap_or((0, u64::MAX));
    match cc {
        IntCC::Equal => {
            if yl == yh {
                rg_tighten(func, rs, over, x, yl, yh);
            } else if xl == xh {
                rg_tighten(func, rs, over, y, xl, xh);
            }
        }
        IntCC::NotEqual => {
            if yl == yh {
                let w = func.dfg.value_type(resolve(func, x)).bits();
                let m = umask(w);
                if yl == 0 {
                    rg_tighten(func, rs, over, x, 1, m);
                } else if yl == m {
                    rg_tighten(func, rs, over, x, xl, m.saturating_sub(1));
                }
            }
        }
        IntCC::UnsignedLessThan => {
            if yh > 0 {
                rg_tighten(func, rs, over, x, xl, yh - 1);
            }
            if xl < u64::MAX {
                rg_tighten(func, rs, over, y, xl.saturating_add(1), yh);
            }
        }
        IntCC::UnsignedLessThanOrEqual => {
            rg_tighten(func, rs, over, x, xl, yh);
            rg_tighten(func, rs, over, y, xl, yh);
        }
        IntCC::UnsignedGreaterThan => {
            if yl < u64::MAX {
                rg_tighten(func, rs, over, x, yl.saturating_add(1), xh);
            }
            if xh > 0 {
                rg_tighten(func, rs, over, y, yl, xh - 1);
            }
        }
        IntCC::UnsignedGreaterThanOrEqual => {
            rg_tighten(func, rs, over, x, yl, xh);
            rg_tighten(func, rs, over, y, yl, xh);
        }
        _ => {}
    }
    band_inverse(func, rs, over, cc, x, (yl, yh));
    band_inverse(func, rs, over, cc.swap_args(), y, (xl, xh));
}

/// `brif x`'s fact on the taken (`nonzero`) or not-taken arm, as a
/// tightened range on `x` plus a `band`-inverse against `0`.
fn refine_brif(
    func: &Function,
    rs: &RefCell<Ranges>,
    over: &mut FxHashMap<Value, (u64, u64)>,
    x: Value,
    nonzero: bool,
) {
    let w = func.dfg.value_type(resolve(func, x)).bits();
    if nonzero {
        rg_tighten(func, rs, over, x, 1, umask(w));
        band_inverse(func, rs, over, IntCC::NotEqual, x, (0, 0));
    } else {
        rg_tighten(func, rs, over, x, 0, 0);
        band_inverse(func, rs, over, IntCC::Equal, x, (0, 0));
    }
}

/// Edge args of `pinst`'s successor call to `b`: `(arm_index, call)`.
fn edge_call(func: &Function, pinst: Inst, b: Block) -> Option<(usize, BlockCall)> {
    match func.dfg.insts[pinst] {
        InstructionData::Jump { destination, .. } => {
            (destination.block(&func.dfg.value_lists) == b).then_some((0, destination))
        }
        InstructionData::Brif { blocks, .. } => {
            let t = blocks[0].block(&func.dfg.value_lists) == b;
            let f = blocks[1].block(&func.dfg.value_lists) == b;
            match (t, f) {
                (true, false) => Some((0, blocks[0])),
                (false, true) => Some((1, blocks[1])),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `brif c`'s compare fact on the arm `arm` is 0 (taken) / 1 (not).
fn arm_cmp(func: &Function, pinst: Inst, arm: usize) -> Option<(IntCC, Value, Value)> {
    let InstructionData::Brif { arg, .. } = func.dfg.insts[pinst] else {
        return None;
    };
    let c = resolve(func, arg);
    let i = func.dfg.value_def(c).inst()?;
    if let InstructionData::IntCompare { cond, args, .. } = func.dfg.insts[i] {
        let cc = if arm == 0 { cond } else { cond.complement() };
        return Some((cc, resolve(func, args[0]), resolve(func, args[1])));
    }
    None
}

struct Plan {
    pinst: Inst,
    arm: usize,
    bc: BlockCall,
    folds: Vec<(Inst, u64)>,
}

pub fn run(func: &mut Function, an: &mut Analysis) -> usize {
    let uses = an.uses(func);
    let crate::pipeline::CfgDtLa { cfg, dt, .. } = an.cfg_dt_la(func);
    let rs = RefCell::new(Ranges::new(func, cfg));
    let mut plans: Vec<(Block, Plan)> = Vec::new();
    let mut env: FxHashMap<Value, u64> = FxHashMap::default();
    let mut over: FxHashMap<Value, (u64, u64)> = FxHashMap::default();
    let penv: FxHashMap<Value, u64> = FxHashMap::default();
    let pover: FxHashMap<Value, (u64, u64)> = FxHashMap::default();

    for b in func.layout.blocks() {
        let params = func.dfg.block_params(b).to_vec();
        if params.is_empty() {
            continue;
        }
        let insts: Vec<Inst> = func.layout.block_insts(b).collect();
        if insts.len() < 2 || insts.len() > MAX_INSTS + 1 {
            continue;
        }
        let (body, _) = match insts.split_last() {
            Some((&t, body)) if func.dfg.insts[t].opcode() == Opcode::Jump => (body, t),
            _ => continue,
        };
        if !body.iter().all(|&i| jumpthread::pure_op(func, i)) {
            continue;
        }
        // A clone reaches `b`'s successors carrying only the edge args,
        // so a value bound at `b` — param or body result — used outside
        // `b` (dominated blocks reference it directly) would lose its
        // def on the clone's path. Such blocks are unclonable.
        let esc = |v: Value| uses.escapes(v, b);
        if params.iter().any(|&p| esc(p))
            || body.iter().any(|&i| func.dfg.inst_results(i).iter().any(|&v| esc(v)))
        {
            continue;
        }
        // Entry edges are the preds `b` does not dominate. Cloning the
        // last one (a loop's preheader) would leave `b` reachable only
        // through its own back edges — dead, and its params would stop
        // dominating their in-loop uses.
        let mut entry_left = cfg
            .pred_iter(b)
            .filter(|p| !dt.dominates(b, p.block, &func.layout))
            .count();
        for pr in cfg.pred_iter(b).collect::<Vec<_>>() {
            if plans.len() >= MAX_CLONES {
                break;
            }
            let entry_edge = !dt.dominates(b, pr.block, &func.layout);
            if entry_edge && entry_left < 2 {
                continue;
            }
            let Some((arm, bc)) = edge_call(func, pr.inst, b) else {
                continue;
            };
            let args: Vec<Value> = bc
                .args(&func.dfg.value_lists)
                .filter_map(|a| match a {
                    BlockArg::Value(v) => Some(v),
                    _ => None,
                })
                .collect();
            if args.len() != params.len() {
                continue;
            }
            // Edge-local env: arm-fact refinements, then arg env/ranges.
            env.clear();
            over.clear();
            match arm_cmp(func, pr.inst, arm) {
                Some((cc, x, y)) => refine_cmp(func, &rs, &mut over, cc, x, y),
                None => {
                    if let InstructionData::Brif { arg, .. } = func.dfg.insts[pr.inst] {
                        refine_brif(func, &rs, &mut over, resolve(func, arg), arm == 0);
                    }
                }
            }
            for (&p, &a) in params.iter().zip(&args) {
                let a = resolve(func, a);
                if let Some(k) = konst(func, a) {
                    env.insert(p, k);
                }
                if let Some((lo, hi)) = rg_get(func, &rs, &over, a) {
                    over.insert(p, (lo, hi));
                }
            }
            let rg = |v: Value| rg_get(func, &rs, &over, v);
            let nz = |v: Value| {
                let v = resolve(func, v);
                env.get(&v).is_some_and(|&k| k != 0)
                    || rg_get(func, &rs, &over, v).is_some_and(|(l, _)| l > 0)
            };
            // Baseline: params keep only their merged-context join —
            // anything the edge cannot improve needs no clone.
            let base_rg = |v: Value| {
                let v = resolve(func, v);
                if params.contains(&v) {
                    let r = rs.borrow_mut().def(func, v);
                    return if r.uhi < r.ulo { None } else { Some((r.ulo, r.uhi)) };
                }
                rg_get(func, &rs, &pover, v)
            };
            let base_nz = |v: Value| base_rg(v).is_some_and(|(l, _)| l > 0);
            let mut folds = Vec::new();
            for &i in body {
                if let Some(k) = jumpthread::eval(func, &env, &nz, &rg, i)
                    && jumpthread::eval(func, &penv, &base_nz, &base_rg, i).is_none()
                {
                    let w = func.dfg.value_type(func.dfg.first_result(i)).bits();
                    folds.push((i, k & umask(w)));
                }
            }
            if tracing::enabled!(target: "pliron::esplit", tracing::Level::DEBUG) {
                tracing::debug!(target: "pliron::esplit",
                    "esplit {b} <- {p} arm={arm} over={over:?} env={env:?} folds={folds:?}",
                    p = pr.block);
            }
            if !folds.is_empty() {
                if entry_edge {
                    entry_left -= 1;
                }
                plans.push((b, Plan { pinst: pr.inst, arm, bc, folds }));
            }
        }
    }

    let lim = pliron_cfg::env_get("PLIRON_ESPLIT_LIMIT").and_then(|s| s.parse::<usize>().ok());
    let n = plans.len();
    for (k, (b, plan)) in plans.into_iter().enumerate() {
        if lim.is_some_and(|l| k >= l) {
            break;
        }
        clone_for_edge(func, b, &plan);
    }
    n
}

/// Clone `b` (params + body + terminator) and retarget the plan's edge
/// to the clone, rewriting folded insts to `iconst`.
fn clone_for_edge(func: &mut Function, b: Block, plan: &Plan) {
    let be = func.dfg.make_block();
    func.layout.insert_block_after(be, b);
    if func.layout.is_cold(b) {
        func.layout.set_cold(be);
    }
    let mut vmap: FxHashMap<Value, Value> = FxHashMap::default();
    for &p in &func.dfg.block_params(b).to_vec() {
        let ty = func.dfg.value_type(p);
        let np = func.dfg.append_block_param(be, ty);
        vmap.insert(p, np);
    }
    let insts: Vec<Inst> = func.layout.block_insts(b).collect();
    let mut clone_of: FxHashMap<Inst, Inst> = FxHashMap::default();
    for &i in &insts {
        let ni = func.dfg.clone_inst(i);
        func.set_srcloc(ni, func.srcloc(i));
        let mut data = func.dfg.insts[ni];
        {
            let dfg = &mut func.dfg;
            data.map_values(
                &mut dfg.value_lists,
                &mut dfg.jump_tables,
                &mut dfg.exception_tables,
                |v| vmap.get(&v).copied().unwrap_or(v),
            );
        }
        func.dfg.insts[ni] = data;
        func.layout.append_inst(ni, be);
        clone_of.insert(i, ni);
        let rt: Vec<Value> = func.dfg.inst_results(i).to_vec();
        let rn: Vec<Value> = func.dfg.inst_results(ni).to_vec();
        for (o, nw) in rt.into_iter().zip(rn) {
            vmap.insert(o, nw);
        }
    }
    // Retarget the edge's blockcall to `be` with unchanged args.
    let args: Vec<BlockArg> = plan.bc.args(&func.dfg.value_lists).collect();
    let nbc = BlockCall::new(be, args, &mut func.dfg.value_lists);
    match &mut func.dfg.insts[plan.pinst] {
        InstructionData::Jump { destination, .. } => *destination = nbc,
        InstructionData::Brif { blocks, .. } => blocks[plan.arm] = nbc,
        _ => {}
    }
    // Materialize the folded insts as iconsts in the clone.
    for &(i, k) in &plan.folds {
        let Some(&ni) = clone_of.get(&i) else { continue };
        let r = func.dfg.first_result(ni);
        let ty = func.dfg.value_type(r);
        let mut pos = FuncCursor::new(func).at_inst(ni);
        let c = pos.ins().iconst(ty, k as i64);
        pos.func.dfg.clear_results(ni);
        pos.func.layout.remove_inst(ni);
        pos.func.dfg.change_to_alias(r, c);
    }
}
