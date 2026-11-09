//! `PLIRON_LOOPDEL`: a post-tested countdown loop whose only effects are
//! `st(f(iv))` to an invariant address and a possibly-aliased load feeding
//! the exit edge folds to one closed-form store plus one re-executed load
//! on the fast path — `num_bigint_dig::biguint::monty::montgomery`'s
//! `*p = --i` loops, which LLVM emits as a single final store.

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Function, Opcode};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::FxHashSet;
use target_lexicon::PointerWidth;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn tcfg() -> TargetFrontendConfig {
    TargetFrontendConfig {
        default_call_conv: CallConv::SystemV,
        pointer_width: PointerWidth::U64,
        page_size_align_log2: 12,
    }
}

/// Blocks reachable from the entry (a folded loop stays in the layout
/// as unreachable dead blocks for the sweep passes).
fn live_blocks(f: &Function) -> FxHashSet<cranelift_codegen::ir::Block> {
    let entry = f.layout.entry_block().unwrap();
    let (mut seen, mut wl) = (FxHashSet::default(), vec![entry]);
    while let Some(b) = wl.pop() {
        if !seen.insert(b) {
            continue;
        }
        for i in f.layout.block_insts(b) {
            for bc in f
                .dfg
                .insts[i]
                .branch_destination(&f.dfg.jump_tables, &f.dfg.exception_tables)
            {
                wl.push(bc.block(&f.dfg.value_lists));
            }
        }
    }
    seen
}

fn live_ops(f: &Function, op: Opcode) -> usize {
    live_blocks(f)
        .into_iter()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == op)
        .count()
}

/// Reachable blocks with a self-backedge.
fn live_self_loops(f: &Function) -> usize {
    let cfg = ControlFlowGraph::with_function(f);
    live_blocks(f)
        .into_iter()
        .filter(|&b| cfg.pred_iter(b).any(|p| p.block == b))
        .count()
}

/// `do { *v0 = --iv; len = *(v0+8) } while iv >= 1` — the montgomery
/// shape: the load may alias the store (`v0` vs `v0+8` are the same
/// root), but it reads post-store state, so the fast path re-executes
/// it after the folded store.
const COUNTDOWN: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v1)

block1(v10: i64):
    v11 = iconst.i64 1
    v12 = isub.i64 v10, v11
    store.i64 notrap v12, v0
    v13 = load.i64 notrap v0+8
    v14 = icmp.i64 uge v12, v11
    brif v14, block1(v12), block2(v13)

block2(v20: i64):
    return v20
}";

#[test]
fn countdown_store_folds() {
    unsafe { std::env::set_var("PLIRON_LOOPDEL", "1") };
    let mut f = parse(COUNTDOWN);
    pliron_clif_opt::loopidiom::run(
        &mut f,
        &mut Analysis::default(),
        &FxHashSet::default(),
        tcfg(),
        "f",
    );
    // The loop is bypassed: reachable code has one folded store and one
    // re-executed load; nothing reachable branches into it.
    assert_eq!(live_self_loops(&f), 0, "loop still live:\n{}", f.display());
    assert_eq!(live_ops(&f, Opcode::Store), 1, "stores:\n{}", f.display());
    assert_eq!(live_ops(&f, Opcode::Load), 1, "loads:\n{}", f.display());
}

/// The store must be inside the loop body to fold: a store placed where
/// the last iteration's read would race (load *before* the store in the
/// same block — reads the penultimate state) must be declined.
const PRE_STORE_LOAD: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v1)

block1(v10: i64):
    v11 = iconst.i64 1
    v12 = isub.i64 v10, v11
    v13 = load.i64 notrap v0+8
    store.i64 notrap v12, v0
    v14 = icmp.i64 uge v12, v11
    brif v14, block1(v12), block2(v13)

block2(v20: i64):
    return v20
}";

#[test]
fn pre_store_load_declines() {
    unsafe { std::env::set_var("PLIRON_LOOPDEL", "1") };
    let mut f = parse(PRE_STORE_LOAD);
    pliron_clif_opt::loopidiom::run(
        &mut f,
        &mut Analysis::default(),
        &FxHashSet::default(),
        tcfg(),
        "f",
    );
    // Same-root load *before* the store reads a mid-iteration state —
    // the loop must survive reachable.
    assert_eq!(
        live_self_loops(&f),
        1,
        "aliased pre-store load folded:\n{}",
        f.display()
    );
}

