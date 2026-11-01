//! `PLIRON_EDGESINK_LOADS=1`: a `notrap` load used only by one `brif`
//! edge (a loop's length reload feeding only its exit) sinks onto that
//! edge, unless something after it in the block may write memory.
//! Differentially checked.

use cranelift_codegen::ir::Opcode;
use pliron_clif_opt::diffcheck::{Config, Facts, check};

const SRC: &str = "function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    jump block1(v1)
block1(v10: i64):
    v11 = load.i64 notrap v0+40
    STORE
    v12 = iconst.i64 1
    v13 = iadd v10, v12
    v14 = icmp ne v13, v2
    brif v14, block1(v13), block2(v11)
block2(v20: i64):
    return v20
}";

fn loads_in_loop(f: &cranelift_codegen::ir::Function) -> usize {
    let b1 = cranelift_codegen::ir::Block::from_u32(1);
    f.layout.block_insts(b1).filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load).count()
}

fn run(src: &str) -> (cranelift_codegen::ir::Function, cranelift_codegen::ir::Function) {
    unsafe { std::env::set_var("PLIRON_EDGESINK_LOADS", "1") };
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    pliron_clif_opt::edgesink::run(&mut after);
    (before, after)
}

#[test]
fn exit_only_load_sinks() {
    let (before, after) = run(&SRC.replace("    STORE\n", ""));
    assert_eq!(loads_in_loop(&after), 0, "load not sunk:\n{}", after.display());
    let cfg = Config { trials: 64, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}

#[test]
fn load_before_a_store_stays() {
    let (_, after) = run(&SRC.replace("STORE", "store.i64 notrap v10, v0+40"));
    assert_eq!(loads_in_loop(&after), 1, "load sunk past a store:\n{}", after.display());
}

/// A load used only by the back edge stays: on that edge it runs every
/// iteration anyway, and the edge block plus the longer-lived address
/// cost spills (pulldown's `handle_inline`, slot reloads after a call).
#[test]
fn backedge_only_load_stays() {
    let src = "function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    jump block1(v1)
block1(v10: i64):
    v11 = load.i64 notrap v0+40
    v14 = icmp ne v10, v2
    brif v14, block1(v11), block2(v10)
block2(v20: i64):
    return v20
}";
    let (_, after) = run(src);
    assert_eq!(loads_in_loop(&after), 1, "back-edge load sunk:\n{}", after.display());
}

/// Reloads after a call stay, even on a loop-exit edge: they run once
/// per call either way, and the edge block cost pulldown's byte-scan
/// index its register across the call.
#[test]
fn reload_after_call_stays() {
    let src = "function %f(i64, i64, i64) -> i64 {
    fn0 = %g(i64)
block0(v0: i64, v1: i64, v2: i64):
    jump block1(v1)
block1(v10: i64):
    call fn0(v0)
    v11 = load.i64 notrap v0+40
    v12 = iconst.i64 1
    v13 = iadd v10, v12
    v14 = icmp ne v13, v2
    brif v14, block1(v13), block2(v11)
block2(v20: i64):
    return v20
}";
    let (_, after) = run(src);
    assert_eq!(loads_in_loop(&after), 1, "reload after a call sunk:\n{}", after.display());
}
