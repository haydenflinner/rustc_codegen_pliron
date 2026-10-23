//! `loopidiom::trip_max` bounds a slice loop by the guard on its length:
//! `end = start + uextend(len) * 4` behind `len <u 9` gives at most 9
//! trips (8 plus the conservative round-up). After loop rotation the
//! guarded block is entered from two `brif`s, each with its own copy of
//! the guard, and the length arrives as a block param — the bound must
//! take the max over both edges. loopvec/punroll use it to skip
//! 16-lane / x8 clones that can never run (bench `arrayvec`).

use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::{Block, Function, Value};

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

const SRC: &str = "
function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v3 = iconst.i64 0
    v9 = iconst.i32 9
    v4 = iconst.i64 4
    v36 = iconst.i64 36
    v32 = iconst.i64 32
    v2 = imul v1, v36
    v14 = iadd v0, v2
    jump block1(v0, v3)

block1(v15: i64, v72: i64):
    v22 = icmp eq v15, v14
    brif v22, block3(v72), block2(v15, v72)

block2(v82: i64, v83: i64):
    v24 = iadd v82, v36
    v30 = iadd v82, v32
    v31 = load.i32 notrap v30
    v33 = uextend.i64 v31
    v80 = icmp ult v31, v9
    brif v80, block7(v82, v83, v24, v33), block6

block12(v187: i64, v197: i64):
    v91 = iadd v187, v32
    v92 = load.i32 notrap v91
    v93 = uextend.i64 v92
    v90 = iadd v187, v36
    v94 = icmp ult v92, v9
    brif v94, block7(v187, v197, v90, v93), block6

block7(v85: i64, v86: i64, v87: i64, v88: i64):
    v47 = imul v88, v4
    v48 = iadd v85, v47
    jump block8(v85, v86)

block8(v49: i64, v74: i64):
    v56 = icmp ne v49, v48
    brif v56, block9(v49, v74), block11(v74)

block9(v95: i64, v96: i64):
    v58 = iadd v95, v4
    v63 = load.i32 notrap v95
    v64 = uextend.i64 v63
    v66 = iadd v96, v64
    v99 = icmp ne v58, v48
    brif v99, block9(v58, v66), block11(v66)

block11(v97: i64):
    v84 = icmp eq v87, v14
    brif v84, block3(v97), block12(v87, v97)

block6:
    trap user1

block3(v70: i64):
    return v70
}
";

fn block(n: u32) -> Block {
    Block::from_u32(n)
}

fn val(f: &Function, name: &str) -> Value {
    // Values keep their textual numbers through the reader.
    let n: u32 = name.trim_start_matches('v').parse().unwrap();
    let v = Value::from_u32(n);
    assert!(f.dfg.value_is_valid(v), "{name}");
    v
}

#[test]
fn slice_loop_bounded_by_len_guard() {
    let f = parse(SRC);
    let cfg = ControlFlowGraph::with_function(&f);
    let dt = DominatorTree::with_function(&f, &cfg);
    let h = block(9);
    let iv0 = pliron_clif_opt::loopidiom::entry_arg(&f, &cfg, &dt, h, 0).expect("entry arg");
    assert_eq!(iv0, val(&f, "v49"));
    let m = pliron_clif_opt::loopidiom::trip_max(&f, &cfg, &dt, h, iv0, val(&f, "v48"), 4, false);
    assert_eq!(m, Some(9));
}

#[test]
fn unguarded_length_has_no_small_bound() {
    // Drop the guard on the rotated entry: block12 now branches on `v22`,
    // which says nothing about `v92`, so the param's max is unbounded.
    let src = SRC.replace("brif v94, block7(v187", "brif v22, block7(v187");
    let f = parse(&src);
    let cfg = ControlFlowGraph::with_function(&f);
    let dt = DominatorTree::with_function(&f, &cfg);
    let h = block(9);
    let iv0 = pliron_clif_opt::loopidiom::entry_arg(&f, &cfg, &dt, h, 0).unwrap();
    let m = pliron_clif_opt::loopidiom::trip_max(&f, &cfg, &dt, h, iv0, val(&f, "v48"), 4, false);
    assert!(m.is_none_or(|m| m > 1 << 30), "{m:?}");
}
