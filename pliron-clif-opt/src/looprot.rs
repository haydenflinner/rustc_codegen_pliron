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
use cranelift_codegen::loop_analysis::LoopAnalysis;
use crate::vmap::VMap;
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_INSTS: usize = 8;
const MAX_ROTATIONS: usize = 256;

pub fn run(
    func: &mut Function,
    an: &mut crate::pipeline::Analysis,
    derived: &mut VMap<Value>,
    prof: &FxHashMap<Block, u64>,
) -> usize {
    run_with(
        func,
        an,
        derived,
        pliron_cfg::env_is("PLIRON_LOOPROT_SINK", "1"),
        prof,
    )
}

/// `run` with the latch sink (`sink_to_latch`) chosen by the caller
/// (`PLIRON_LOOPROT_SINK`, default on, in the pipeline; filetests pass
/// it directly).
pub fn run_with(
    func: &mut Function,
    an: &mut crate::pipeline::Analysis,
    derived: &mut VMap<Value>,
    sink: bool,
    prof: &FxHashMap<Block, u64>,
) -> usize {
    // Each block is evaluated once, in layout order: re-scanning let cloned
    // headers created by earlier rotations starve late-layout loops (e.g.
    // bcheck clones appended at the end) of the rotation budget.
    // `PLIRON_LOOPROT_COLD` (opt-in): skip loops a cold mark or a zero
    // PGO count flags — rotating them duplicates the exit test for a
    // path that never runs hot.
    let cold = pliron_cfg::env_is("PLIRON_LOOPROT_COLD", "1");
    let reducible = crate::jumpthread::keep_reducible();
    let merge = pliron_cfg::env_is("PLIRON_LOOPROT_MERGE", "1");
    let mut n = 0;
    let blocks: Vec<Block> = func.layout.blocks().collect();
    for h in blocks {
        if n >= MAX_ROTATIONS {
            break;
        }
        if cold && (func.layout.is_cold(h) || prof.get(&h) == Some(&0)) {
            continue;
        }
        // Layout-only checks run before the `cfg_dt` borrow: after a
        // rotation the analysis is stale, so every later block that
        // fails here would otherwise pay a cfg/domtree rebuild just to
        // bail on its own shape.
        let Some((t, s1, s2)) = rotatable_pre(func, h) else {
            continue;
        };
        if merge && merge_latches(func, an, h) {
            an.invalidate(func);
        }
        let ok = {
            let (cfg, domtree) = an.cfg_dt(func);
            rotatable(func, cfg, domtree, h, t, s1, s2)
                && (!reducible || {
                    // `PLIRON_REDUCIBLE`: the latch takes a copy of `h`'s
                    // `brif`; refuse when one of its edges would be a loop
                    // side entrance (see `jumpthread::side_enters`).
                    let l = cfg
                        .pred_iter(h)
                        .find(|p| domtree.block_dominates(h, p.block))
                        .unwrap()
                        .block;
                    let mut la = LoopAnalysis::new();
                    la.compute(func, cfg, domtree);
                    // Both arms inside the loop (the header is a dispatch,
                    // not an exit test): after the copy the loop is
                    // entered at `s1` from `h` and at `s2` from `h` as
                    // well — two entries, no header (regex-syntax's
                    // `escape_into`). One arm must leave the loop.
                    let both_in = la.is_loop_header(h).is_some_and(|lh| {
                        la.is_in_loop(s1, lh) && la.is_in_loop(s2, lh)
                    });
                    !both_in && !crate::jumpthread::side_enters(func, &la, l, h)
                })
        };
        if ok {
            // The header's own increments move to the latch first, so
            // the rotated loop carries one index (`PLIRON_LOOPROT_SINK=0`
            // disables).
            if sink && sink_to_latch(func, an, h) > 0 {
                an.invalidate(func);
            }
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

/// Give a loop with several back edges one latch (LLVM's loop-simplify
/// does the same): a fresh block takes every back edge and jumps to
/// the header (`PLIRON_LOOPROT_MERGE=1`, opt-in).
///
/// Shape: a header that only tests the exit, and a body that reaches
/// it from several places — a `match` whose arms each `continue`:
///
/// ```text
/// h(p):  brif p == end, exit, body
/// arm1:  ... jump h(p + 8)
/// arm2:  ... jump h(p + 8)
/// ```
///
/// Rotation wants one latch to copy the test into and refused these,
/// so every trip ended in `b h` and ran the top test: regex's
/// `Fsm::exec_byte` walks the current NFA states that way
/// (`dfa.rs:959`, `tests/parity/prog_loop.rs`: 16 instructions for an
/// instruction that is skipped, 12 in LLVM's rotated loop). With one
/// latch the rotated test sits there once.
///
/// Opt-in: exact counts are mixed (rust-html-rewriter -0.27M,
/// rust-json -0.16M, but regex +0.63M and pulldown-cmark +0.64M). In
/// the repro the rotated loop recomputes the invariant end pointer
/// (`base + len*8`) in the latch — the elaborator's pop-on-leave rule
/// places it there once the latch follows an exit block in the
/// domtree walk — and the pointer is copied between a caller-saved
/// and a callee-saved register on each path around the call. Those
/// two have to go first.
///
/// Only when every back edge is a `jump`, or one arm of a `brif`, at
/// the end of its block with plain value arguments. Returns whether
/// the CFG changed.
fn merge_latches(func: &mut Function, an: &mut crate::pipeline::Analysis, h: Block) -> bool {
    let latches: Vec<(Block, Inst)> = {
        let (cfg, domtree) = an.cfg_dt(func);
        if !domtree.is_reachable(h) {
            return false;
        }
        cfg.pred_iter(h)
            .filter(|p| domtree.block_dominates(h, p.block))
            .map(|p| (p.block, p.inst))
            .collect()
    };
    if latches.len() < 2 || latches.len() > 16 {
        return false;
    }
    let pool = &func.dfg.value_lists;
    for &(b, i) in &latches {
        if func.layout.last_inst(b) != Some(i) {
            return false;
        }
        let arms: Vec<&BlockCall> = match &func.dfg.insts[i] {
            InstructionData::Jump { destination, .. } => vec![destination],
            InstructionData::Brif { blocks, .. } => blocks.iter().filter(|bc| bc.block(pool) == h).collect(),
            _ => return false,
        };
        if arms.len() != 1 || arms[0].args(pool).any(|a| !matches!(a, BlockArg::Value(_))) {
            return false;
        }
    }
    if !pliron_cfg::bisect("looprot") {
        return false;
    }
    let l = func.dfg.make_block();
    // After the last latch in layout order, so the loop stays contiguous.
    let last = func
        .layout
        .blocks()
        .filter(|b| latches.iter().any(|(lb, _)| lb == b))
        .last()
        .unwrap();
    func.layout.insert_block_after(l, last);
    let tys: Vec<_> = func.dfg.block_params(h).iter().map(|&p| func.dfg.value_type(p)).collect();
    let lp: Vec<BlockArg> = tys.iter().map(|&ty| BlockArg::Value(func.dfg.append_block_param(l, ty))).collect();
    for &(_, i) in &latches {
        let dfg = &mut func.dfg;
        for bc in dfg.insts[i].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables) {
            if bc.block(&dfg.value_lists) == h {
                let args: Vec<BlockArg> = bc.args(&dfg.value_lists).collect();
                *bc = BlockCall::new(l, args.into_iter(), &mut dfg.value_lists);
            }
        }
    }
    use cranelift_codegen::cursor::{Cursor, FuncCursor};
    use cranelift_codegen::ir::InstBuilder;
    FuncCursor::new(func).at_bottom(l).ins().jump(h, &lp);
    true
}

/// Header instructions whose results are used only by the latch's
/// terminator (the index increment feeding the back edge's args) move
/// to the latch, right before that terminator. Rotation then copies
/// only the test: without this the increment is a header value used
/// past the header, becomes a successor param, and the rotated loop
/// carries both `i` and `i + 1` (a `mov` per iteration). Operands are
/// header-dominated, so the move keeps SSA order; only side-effect-free,
/// non-trapping instructions qualify.
fn sink_to_latch(func: &mut Function, an: &mut crate::pipeline::Analysis, h: Block) -> usize {
    let (lb, li) = {
        let (cfg, domtree) = an.cfg_dt(func);
        let Some(l) = cfg.pred_iter(h).find(|p| domtree.block_dominates(h, p.block)) else {
            return 0;
        };
        (l.block, l.inst)
    };
    if lb == h {
        return 0;
    }
    // Backwards over the header: an instruction moves when every use is
    // the latch terminator or an instruction already moving (the `iconst
    // 1` feeding the increment goes with it, instead of becoming a
    // carried param). Re-inserted in header order before the terminator.
    let mut moving: FxHashSet<Inst> = FxHashSet::default();
    {
        let u = an.uses(func);
        let insts: Vec<Inst> = func.layout.block_insts(h).collect();
        for &i in insts.iter().rev() {
            let op = func.dfg.insts[i].opcode();
            if op.is_branch()
                || op.is_terminator()
                || op.is_call()
                || op.can_load()
                || op.can_store()
                || op.can_trap()
                || op.other_side_effects()
            {
                continue;
            }
            let rs = func.dfg.inst_results(i);
            if rs.is_empty()
                || rs.iter().any(|&r| {
                    let s = u.sites(r);
                    s.is_empty() || s.iter().any(|&(b, j)| !((b == lb && j == li) || moving.contains(&j)))
                })
            {
                continue;
            }
            moving.insert(i);
        }
    }
    let moves: Vec<Inst> = func.layout.block_insts(h).filter(|i| moving.contains(i)).collect();
    for &i in &moves {
        func.layout.remove_inst(i);
        func.layout.insert_inst(i, li);
    }
    moves.len()
}

/// The `rotatable` checks that need only the layout and inst data —
/// run before the cfg/domtree borrow so a rotation's stale-analysis
/// rebuild isn't paid per non-candidate. Returns `(terminator, succ1,
/// succ2)` on pass.
fn rotatable_pre(func: &Function, h: Block) -> Option<(Inst, Block, Block)> {
    if Some(h) == func.layout.entry_block() {
        return None;
    }
    let t = func.layout.last_inst(h)?;
    let InstructionData::Brif { blocks, .. } = &func.dfg.insts[t] else {
        return None;
    };
    let pool = &func.dfg.value_lists;
    let (s1, s2) = (blocks[0].block(pool), blocks[1].block(pool));
    if s1 == h || s2 == h || s1 == s2 {
        return None;
    }
    if blocks
        .iter()
        .any(|b| b.args(pool).any(|a| !matches!(a, BlockArg::Value(_))))
    {
        return None;
    }
    let mut k = 0;
    for i in func.layout.block_insts(h) {
        if i == t {
            continue;
        }
        let op = func.dfg.insts[i].opcode();
        if op.is_call() || op.is_branch() || op.is_terminator() {
            return None;
        }
        k += 1;
    }
    if k > MAX_INSTS {
        return None;
    }
    Some((t, s1, s2))
}

/// Header `h` with one latch ending in `jump h`, a small call-free body and a
/// `brif` to two blocks that only `h` reaches. `t`/`s1`/`s2` come from
/// `rotatable_pre`.
fn rotatable(
    func: &Function,
    cfg: &ControlFlowGraph,
    domtree: &DominatorTree,
    h: Block,
    _t: Inst,
    s1: Block,
    s2: Block,
) -> bool {
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
    if !domtree.is_reachable(h) {
        bail!("unreachable");
    }
    let pool = &func.dfg.value_lists;
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
        // `-g`: the copy is the same source operation.
        func.set_srcloc(ni, func.srcloc(i));
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
