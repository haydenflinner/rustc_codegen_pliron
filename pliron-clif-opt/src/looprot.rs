//! Loop rotation (LLVM's `loop-rotate`) on CLIF. A loop header that only
//! tests the exit condition is copied into the latch, so each iteration ends
//! in one `brif` back to the body instead of `jump header` + `brif`. The
//! original header stays as the entry guard. Values of the header used past
//! it become block parameters of its two successors (each must have the
//! header as its only predecessor), fed by the header and by the copy.

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::condcodes::CondCode;
use cranelift_codegen::ir::{Block, BlockArg, BlockCall, Function, Inst, InstructionData, Value};
use crate::vmap::VMap;
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_INSTS: usize = 8;
const MAX_ROTATIONS: usize = 256;

pub fn run(
    func: &mut Function,
    an: &mut crate::pipeline::Analysis,
    derived: &mut VMap<Value>,
) -> usize {
    // Each block is evaluated once, in layout order: re-scanning let cloned
    // headers created by earlier rotations starve late-layout loops (e.g.
    // bcheck clones appended at the end) of the rotation budget.
    let mut n = 0;
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for h in blocks {
        if n >= MAX_ROTATIONS {
            break;
        }
        // `rotatable` runs under the `cfg_dt` borrow; the shared use
        // index is fetched in the gap before `rotate` re-borrows them.
        let ok = {
            let (cfg, domtree) = an.cfg_dt(func);
            rotatable(func, cfg, domtree, h)
        };
        if ok {
            let u = an.uses(func);
            let (cfg, domtree) = an.cfg_dt(func);
            if rotate(func, cfg, domtree, h, derived, &u) {
                n += 1;
                // Edge splits/clones rewired the CFG — the next candidate's
                // reads must see the new shape.
                an.invalidate(func);
            }
        }
    }
    n
}

/// Header `h` with one latch ending in `jump h`, a small call-free body and a
/// `brif` to two blocks that only `h` reaches.
fn rotatable(func: &Function, cfg: &ControlFlowGraph, domtree: &DominatorTree, h: Block) -> bool {
    let dbg = pliron_cfg::tracing_on()
        && tracing::enabled!(target: "pliron::looprot", tracing::Level::DEBUG);
    macro_rules! bail {
        ($($t:tt)*) => {{
            if dbg {
                if pliron_cfg::tracing_on() { tracing::debug!(target: "pliron::looprot", "looprot {h}: bail {}", format_args!($($t)*)) };
            }
            return false;
        }};
    }
    if Some(h) == func.layout.entry_block() || !domtree.is_reachable(h) {
        bail!("entry/unreachable");
    }
    let Some(t) = func.layout.last_inst(h) else {
        bail!("no last inst");
    };
    let InstructionData::Brif { blocks, .. } = &func.dfg.insts[t] else {
        return false;
    };
    let pool = &func.dfg.value_lists;
    let (s1, s2) = (blocks[0].block(pool), blocks[1].block(pool));
    if s1 == h || s2 == h || s1 == s2 {
        return false;
    }
    if blocks
        .iter()
        .any(|b| b.args(pool).any(|a| !matches!(a, BlockArg::Value(_))))
    {
        bail!("non-value blockarg");
    }
    // A multi-pred successor is fine as long as no `h`-defined value is live
    // into its region (`rotate` appends params for those, which other preds
    // couldn't supply); checked there once `need` is known.

    let latches: Vec<_> = cfg
        .pred_iter(h)
        .filter(|p| domtree.block_dominates(h, p.block))
        .collect();
    let [ref l] = latches[..] else {
        bail!("latches {:?}", latches);
    };
    if func.layout.last_inst(l.block) != Some(l.inst) {
        bail!("latch not last inst");
    }
    // The back-edge is either `jump h(args)` or one arm of a `brif`; the brif
    // form appears in `while`-style loops where the body ends in a conditional
    // continue (e.g. hash-table probing's `while mask != 0`).
    match &func.dfg.insts[l.inst] {
        InstructionData::Jump { destination, .. } => {
            if destination
                .args(pool)
                .any(|a| !matches!(a, BlockArg::Value(_)))
            {
                return false;
            }
        }
        InstructionData::Brif { blocks, .. } => {
            let arms: Vec<_> = blocks.iter().filter(|b| b.block(pool) == h).collect();
            if arms.len() != 1 || arms[0].args(pool).any(|a| !matches!(a, BlockArg::Value(_))) {
                bail!("brif latch arms");
            }
        }
        _ => bail!("latch terminator"),
    }
    let mut k = 0;
    for i in func.layout.block_insts(h) {
        if i == t {
            continue;
        }
        let op = func.dfg.insts[i].opcode();
        if op.is_call() || op.is_branch() || op.is_terminator() {
            bail!("header op {:?}", op);
        }
        k += 1;
    }
    if k > MAX_INSTS {
        bail!("header size {k}");
    }
    if dbg {
        if pliron_cfg::tracing_on() {
            tracing::debug!(target: "pliron::looprot", "looprot {h}: rotatable s1={s1} s2={s2} latch={}", l.block)
        };
    }
    true
}

fn resolved_args(func: &Function, i: Inst) -> Vec<Value> {
    func.dfg
        .inst_values(i)
        .map(|v| func.dfg.resolve_aliases(v))
        .collect()
}

fn rotate(
    func: &mut Function,
    cfg: &ControlFlowGraph,
    domtree: &DominatorTree,
    h: Block,
    derived: &mut VMap<Value>,
    useidx: &crate::uses::Uses,
) -> bool {
    let t = func.layout.last_inst(h).unwrap();
    let InstructionData::Brif { blocks, .. } = func.dfg.insts[t] else {
        unreachable!()
    };
    let succ = [
        blocks[0].block(&func.dfg.value_lists),
        blocks[1].block(&func.dfg.value_lists),
    ];
    let l = cfg
        .pred_iter(h)
        .find(|p| domtree.block_dominates(h, p.block))
        .unwrap();

    // Values defined in `h`, and which successor region uses each one outside `h`.
    let mut defs: Vec<Value> = func.dfg.block_params(h).to_vec();
    for i in func.layout.block_insts(h) {
        defs.extend_from_slice(func.dfg.inst_results(i));
    }
    let rdefs: Vec<Value> = defs.iter().map(|&v| func.dfg.resolve_aliases(v)).collect();
    let mut need = [FxHashSet::<Value>::default(), FxHashSet::default()];
    let mut users: Vec<(Inst, usize)> = Vec::new();
    let mut seen_user: FxHashSet<Inst> = FxHashSet::default();
    let mut side_cache: FxHashMap<Block, Option<usize>> = FxHashMap::default();
    for &d in &rdefs {
        for &(b, i) in useidx.sites(d) {
            if b == h || !domtree.is_reachable(b) {
                continue;
            }
            let side = *side_cache
                .entry(b)
                .or_insert_with(|| succ.iter().position(|&s| domtree.block_dominates(s, b)));
            let Some(side) = side else {
                if pliron_cfg::tracing_on()
                    && tracing::enabled!(target: "pliron::looprot", tracing::Level::DEBUG)
                {
                    if pliron_cfg::tracing_on() {
                        tracing::debug!(target: "pliron::looprot", "looprot {h}: bail side-none blk{b} used {d}")
                    };
                }
                return false;
            };
            need[side].insert(d);
            if seen_user.insert(i) {
                users.push((i, side));
            }
        }
    }

    // Params are appended per need[side]; a multi-pred successor can only be
    // accepted when nothing is live into its region — other preds can't
    // supply appended params.
    for side in 0..2 {
        if cfg.pred_iter(succ[side]).count() > 1 && !need[side].is_empty() {
            if pliron_cfg::tracing_on()
                && tracing::enabled!(target: "pliron::looprot", tracing::Level::DEBUG)
            {
                if pliron_cfg::tracing_on() {
                    tracing::debug!(target: "pliron::looprot", "looprot {h}: bail multipred {:?} need {:?}",
                        succ[side], need[side]
                    )
                };
            }
            return false;
        }
    }

    // New parameters on the successors; the header passes its own values.
    let mut repl = [FxHashMap::<Value, Value>::default(), FxHashMap::default()];
    let mut order: [Vec<Value>; 2] = [Vec::new(), Vec::new()];
    for side in 0..2 {
        let mut appended = FxHashSet::default();
        for &v in rdefs
            .iter()
            .filter(|v| need[side].contains(v) && appended.insert(*v))
        {
            let ty = func.dfg.value_type(v);
            let p = func.dfg.append_block_param(succ[side], ty);
            repl[side].insert(v, p);
            order[side].push(v);
            let dfg = &mut func.dfg;
            if let InstructionData::Brif { blocks, .. } = &mut dfg.insts[t] {
                blocks[side].append_argument(v, &mut dfg.value_lists);
            }
        }
    }
    for (i, side) in users {
        let old: Vec<Value> = func.dfg.inst_values(i).collect();
        let new: Vec<Value> = old
            .iter()
            .map(|&x| {
                let r = func.dfg.resolve_aliases(x);
                *repl[side].get(&r).unwrap_or(&x)
            })
            .collect();
        if new != old {
            func.dfg.overwrite_inst_values(i, new.into_iter());
        }
    }

    // Copy the header into the latch, in place of its back-edge to `h`.
    let args: Vec<BlockArg> = match &func.dfg.insts[l.inst] {
        InstructionData::Jump { destination, .. } => {
            destination.args(&func.dfg.value_lists).collect()
        }
        InstructionData::Brif { blocks, .. } => blocks
            .iter()
            .find(|b| b.block(&func.dfg.value_lists) == h)
            .unwrap()
            .args(&func.dfg.value_lists)
            .collect(),
        _ => unreachable!(),
    };
    let mut map: FxHashMap<Value, Value> = FxHashMap::default();
    for (&prm, a) in func.dfg.block_params(h).to_vec().iter().zip(args) {
        let BlockArg::Value(a) = a else {
            unreachable!()
        };
        map.insert(prm, a);
    }
    // For a `brif` latch the cloned test needs its own block (`l` already ends
    // in the conditional); place it right after `l` so the loop arm falls
    // through. For a `jump` latch the clone replaces the terminator in place.
    let brif_latch = matches!(func.dfg.insts[l.inst], InstructionData::Brif { .. });
    let mut h2 = None;
    if brif_latch {
        let b = func.dfg.make_block();
        func.layout.insert_block_after(b, l.block);
        h2 = Some(b);
    }
    let body: Vec<Inst> = func.layout.block_insts(h).collect();
    let mut lt = None;
    for i in body {
        let ni = func.dfg.clone_inst(i);
        let dfg = &mut func.dfg;
        let mut data = dfg.insts[ni];
        for d in data.branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables) {
            *d = d.deep_clone(&mut dfg.value_lists);
        }
        dfg.insts[ni] = data;
        let vals: Vec<Value> = resolved_args(func, ni)
            .into_iter()
            .map(|v| *map.get(&v).unwrap_or(&v))
            .collect();
        func.dfg.overwrite_inst_values(ni, vals.into_iter());
        let res: Vec<(Value, Value)> = func
            .dfg
            .inst_results(i)
            .iter()
            .copied()
            .zip(func.dfg.inst_results(ni).iter().copied())
            .collect();
        for &(o, nr) in &res {
            crate::jumpthread::prop_derived(&func.dfg, derived, o, nr, &map);
        }
        map.extend(res);
        match h2 {
            Some(h2) => func.layout.append_inst(ni, h2),
            None => func.layout.insert_inst(ni, l.inst),
        }
        if i == t {
            lt = Some(ni);
        }
    }
    if let Some(h2) = h2 {
        // Keep the loop arm on the fall-through (else) slot: `brif c, exit,
        // h2` emits `jcc exit` + fall-through into the cloned test.
        if let InstructionData::Brif { blocks, .. } = &func.dfg.insts[l.inst]
            && blocks[0].block(&func.dfg.value_lists) == h
        {
            if let InstructionData::Brif { arg, .. } = func.dfg.insts[l.inst]
                && uses(func, arg) == 1
                && let Some(ci) = func.dfg.resolved_value_def(arg).inst()
                && let InstructionData::IntCompare { cond, .. } = &mut func.dfg.insts[ci]
            {
                *cond = cond.complement();
                let InstructionData::Brif { blocks, .. } = &mut func.dfg.insts[l.inst] else {
                    unreachable!()
                };
                blocks.swap(0, 1);
            }
        }
        let mut data = func.dfg.insts[l.inst];
        let InstructionData::Brif { blocks, .. } = &mut data else {
            unreachable!()
        };
        for b in blocks.iter_mut() {
            if b.block(&func.dfg.value_lists) == h {
                *b = BlockCall::new(h2, std::iter::empty(), &mut func.dfg.value_lists);
            }
        }
        func.dfg.insts[l.inst] = data;
    } else {
        func.layout.remove_inst(l.inst);
    }
    // The cloned test ends each iteration; keep the loop-directed edge on
    // the taken arm so the exit can fall through — `brif c, exit, loop`
    // emits `jcc tramp; jmp tramp` where `brif !c, loop, exit` is a single
    // `jcc` back when the exit lands next in layout.
    let Some(lt) = lt else { return true };
    if succ
        .iter()
        .position(|s| domtree.block_dominates(*s, l.block))
        == Some(1)
        && let InstructionData::Brif { arg, .. } = func.dfg.insts[lt]
        && uses(func, arg) == 1
        && let Some(ci) = func.dfg.resolved_value_def(arg).inst()
        && let InstructionData::IntCompare { cond, .. } = &mut func.dfg.insts[ci]
    {
        *cond = cond.complement();
        let InstructionData::Brif { blocks, .. } = &mut func.dfg.insts[lt] else {
            unreachable!()
        };
        blocks.swap(0, 1);
    }
    true
}

/// Number of times `v` is used as an operand (inst args + branch args),
/// capped at 2 — every caller tests `== 1`.
pub fn uses(func: &Function, v: Value) -> usize {
    let v = func.dfg.resolve_aliases(v);
    let mut n = 0;
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            n += func
                .dfg
                .inst_values(i)
                .filter(|&u| func.dfg.resolve_aliases(u) == v)
                .count();
            if n > 1 {
                return n;
            }
        }
    }
    n
}
