//! `unroll::run_opts(.., pure_only = true)`, the late round of
//! `PLIRON_UNROLL2`: a constant-trip loop that calls a function or holds
//! a `black_box` stays a loop (wasm `k_fib`'s `fib(black_box(28))` loop
//! must not move), while a call on a panic path (`call; trap`) does not
//! count.

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::pipeline::Analysis;

/// Two trips; the body is `body`, and a bounds test may branch to a
/// `call panic; trap` block.
fn src(body: &str) -> String {
    format!(
        "function u0:0(i64) fast {{
    sig0 = (i64) fast
    fn0 = colocated u0:1 sig0

block0(v0: i64):
    v1 = iconst.i64 0
    jump block1(v1)

block1(v10: i64):
    v11 = iconst.i64 8
    v12 = icmp ult v10, v11
    brif v12, block2, block4

block2:
{body}
    v15 = iconst.i64 1
    v16 = iadd v10, v15
    v17 = iconst.i64 2
    v18 = icmp ult v16, v17
    brif v18, block1(v16), block3

block4 cold:
    call fn0(v10)
    trap user1

block3:
    return
}}"
    )
}

fn count(f: &Function, op: Opcode) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == op)
        .count()
}

fn unroll(src: &str, pure_only: bool) -> (Function, usize) {
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let n = pliron_clif_opt::unroll::run_opts(
        &mut f,
        &mut Analysis::default(),
        &rustc_hash::FxHashSet::default(),
        &pliron_clif_opt::vmap::VMap::default(),
        &std::collections::HashMap::default(),
        false,
        pure_only,
    );
    pliron_clif_opt::jumpthread::remove_unreachable_blocks(&mut f);
    (f, n)
}

const STORE: &str = "    v13 = load.i64 notrap v0\n    v14 = iadd v13, v10\n    store notrap v14, v0";
const CALL: &str = "    call fn0(v10)";
const BB: &str = "    v13 = black_box v10\n    store notrap v13, v0";

#[test]
fn panic_path_call_does_not_block() {
    let (f, n) = unroll(&src(STORE), true);
    assert_eq!(n, 1, "{}", f.display());
    assert_eq!(count(&f, Opcode::Store), 2, "{}", f.display());
}

#[test]
fn a_call_in_the_body_blocks_the_late_round() {
    assert_eq!(unroll(&src(CALL), true).1, 0);
    // The early round unrolls it.
    let (f, n) = unroll(&src(CALL), false);
    assert_eq!(n, 1, "{}", f.display());
}

#[test]
fn a_black_box_blocks_the_late_round() {
    assert_eq!(unroll(&src(BB), true).1, 0);
    assert_eq!(unroll(&src(BB), false).1, 1);
}
