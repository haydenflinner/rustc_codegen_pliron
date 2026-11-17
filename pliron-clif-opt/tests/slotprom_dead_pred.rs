//! slotprom adds a param per promoted field to every livein block and
//! appends the reaching values on the edges into it, but it skipped dead
//! blocks entirely, so a dead block's branch into such a block kept its
//! old argument count. With `PLIRON_TAILDUP=0` dead blocks reach slotprom
//! (taildup's cleanup normally removes them): std's
//! `net::tcp::IntoIncoming::next` failed the verifier ("mismatched
//! argument count", 9 std functions under `harness/offswitch.py`).
//! A dead block's edges now pass the entry undef.

use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::Function;
use rustc_hash::FxHashMap;

/// `ss0` is promoted: `block2` and `block3` read it on entry, so they
/// gain a param each. `block9` has no predecessor but branches to both.
const SRC: &str = "function %t(i64, i64) -> i64 {
    ss0 = explicit_slot 8, align = 8
block0(v0: i64, v1: i64):
    v2 = stack_addr.i64 ss0
    store.i64 notrap v1, v2
    brif v0, block2, block3
block9(v9: i64):
    brif v9, block2, block3
block2:
    v3 = load.i64 notrap v2
    return v3
block3:
    v4 = load.i64 notrap v2
    v5 = iadd v4, v0
    return v5
}";

fn verify(f: &Function) {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(f, &flags) {
        panic!("{e}\n{}", f.display());
    }
}

#[test]
fn dead_block_edges_get_the_new_params() {
    let mut f = cranelift_reader::parse_functions(SRC).unwrap().remove(0);
    let cfg = ControlFlowGraph::with_function(&f);
    let n = pliron_clif_opt::slotprom::run(
        &mut f,
        &cfg,
        &FxHashMap::default(),
        false,
        &Default::default(),
        &pliron_clif_opt::target::Target::host(),
    );
    assert!(n > 0, "expected promotion:\n{}", f.display());
    verify(&f);
}
