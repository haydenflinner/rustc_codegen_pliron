//! `cmp3`: value-set folds and compare push-through on `Ordering` joins.
//! Each case runs the interpreter before and after over boundary inputs
//! and checks the shape the pass should leave.

use cranelift_codegen::data_value::DataValue;
use cranelift_codegen::dominator_tree::DominatorTree;
use cranelift_codegen::flowgraph::ControlFlowGraph;
use cranelift_codegen::ir::Function;
use cranelift_interpreter::environment::FunctionStore;
use cranelift_interpreter::interpreter::{Interpreter, InterpreterState};
use cranelift_interpreter::step::ControlFlow;

fn run(f: &Function, args: &[DataValue]) -> Vec<DataValue> {
    let mut fs = FunctionStore::default();
    fs.add(f.name.to_string(), f);
    let mut it = Interpreter::new(InterpreterState::default().with_function_store(fs));
    match it.call_by_name(&f.name.to_string(), args).unwrap() {
        ControlFlow::Return(r) => r.to_vec(),
        _ => panic!("no return"),
    }
}

fn opt(src: &str) -> (Function, Function, usize) {
    let f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut g = f.clone();
    let cfg = ControlFlowGraph::with_function(&g);
    let dom = DominatorTree::with_function(&g, &cfg);
    let n = pliron_clif_opt::cmp3::run(&mut g, &dom);
    cranelift_codegen::verifier::verify_function(&g, &cranelift_codegen::settings::Flags::new(
        cranelift_codegen::settings::builder(),
    ))
    .unwrap_or_else(|e| panic!("{e}\n{}", g.display()));
    (f, g, n)
}

const B64: [i64; 7] = [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX];

fn check2(f: &Function, g: &Function, n_args: usize) {
    let mut cases = 0;
    let vals: Vec<i64> = B64.to_vec();
    let mut idx = vec![0usize; n_args];
    loop {
        let args: Vec<DataValue> = idx.iter().map(|&i| DataValue::I64(vals[i])).collect();
        assert_eq!(run(f, &args), run(g, &args), "args {args:?}\n{}", g.display());
        cases += 1;
        let mut k = 0;
        loop {
            if k == n_args {
                assert!(cases > 0);
                return;
            }
            idx[k] += 1;
            if idx[k] < vals.len() {
                break;
            }
            idx[k] = 0;
            k += 1;
        }
    }
}

/// Derived two-field `partial_cmp(..).is_some_and(is_lt)` as rustc lowers
/// it: one spaceship per field merged into `block2`, then
/// `select (p == -2), 0, (p < 0)`. The None test dies by range and the
/// `p < 0` moves into each edge.
const LEX_LT: &str = "function %t(i64, i64, i64, i64) -> i8 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v5 = icmp ult v0, v1
    v6 = icmp ne v0, v1
    v7 = iconst.i8 1
    v8 = iconst.i8 0
    v9 = select v6, v7, v8
    v10 = iconst.i8 -1
    v11 = select v5, v10, v9
    v13 = icmp eq v11, v8
    brif v13, block1, block2(v11)

block1:
    v20 = icmp slt v2, v3
    v21 = icmp ne v2, v3
    v24 = select v21, v7, v8
    v26 = select v20, v10, v24
    jump block2(v26)

block2(v42: i8):
    v43 = iconst.i8 -2
    v44 = icmp eq v42, v43
    v45 = icmp slt v42, v8
    v46 = select v44, v8, v45
    return v46
}";

#[test]
fn lexicographic_lt_pushes_through_join() {
    let (f, g, n) = opt(LEX_LT);
    assert!(n > 0);
    let txt = g.display().to_string();
    assert!(!txt.contains("-2"), "None test survived:\n{txt}");
    assert!(!txt.contains("block2(v42"), "join param survived:\n{txt}");
    check2(&f, &g, 4);
}

/// `binary_search`'s join: two tests (`== Equal`, `== Less`) on one
/// param, the first edge narrowed by its own `!= 0` branch.
const BSEARCH: &str = "function %t(i64, i64, i64, i64) -> i8, i8 {
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    v5 = icmp ult v0, v1
    v6 = icmp ne v0, v1
    v7 = iconst.i8 1
    v8 = iconst.i8 0
    v9 = select v6, v7, v8
    v10 = iconst.i8 -1
    v11 = select v5, v10, v9
    v13 = icmp eq v11, v8
    brif v13, block1, block2(v11)

block1:
    v20 = icmp ugt v2, v3
    v21 = icmp ult v2, v3
    v22 = isub v20, v21
    jump block2(v22)

block2(v42: i8):
    v44 = icmp eq v42, v8
    v45 = uextend.i32 v42
    v46 = iconst.i32 255
    v47 = icmp eq v45, v46
    return v44, v47
}";

#[test]
fn two_queries_and_uextend() {
    let (f, g, n) = opt(BSEARCH);
    assert!(n > 0);
    assert!(!g.display().to_string().contains("block2(v42"), "{}", g.display());
    check2(&f, &g, 4);
}

/// `Option<Ordering>` that can really be None (a float-style producer
/// passes -2 on one edge): the `== -2` test must survive.
const MAYBE_NONE: &str = "function %t(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v2 = icmp eq v0, v1
    brif v2, block1, block3

block1:
    v3 = iconst.i8 -2
    jump block2(v3)

block3:
    v5 = icmp slt v0, v1
    v7 = iconst.i8 1
    v10 = iconst.i8 -1
    v11 = select v5, v10, v7
    jump block2(v11)

block2(v42: i8):
    v8 = iconst.i8 0
    v43 = iconst.i8 -2
    v44 = icmp eq v42, v43
    v45 = icmp slt v42, v8
    v46 = select v44, v8, v45
    v47 = bor v44, v46
    return v47
}";

#[test]
fn live_none_is_kept() {
    let (f, g, _) = opt(MAYBE_NONE);
    check2(&f, &g, 2);
    // A real None edge: the result for equal inputs is still 1 (`v44`).
    assert_eq!(
        run(&g, &[DataValue::I64(3), DataValue::I64(3)]),
        vec![DataValue::I8(1)]
    );
}

/// A param with a non-compare use stays (no push), but its set still
/// folds the impossible test.
const OTHER_USE: &str = "function %t(i64, i64) -> i8, i8 {
block0(v0: i64, v1: i64):
    v2 = icmp eq v0, v1
    brif v2, block1, block3

block1:
    v3 = iconst.i8 0
    jump block2(v3)

block3:
    v5 = icmp slt v0, v1
    v7 = iconst.i8 1
    v10 = iconst.i8 -1
    v11 = select v5, v10, v7
    jump block2(v11)

block2(v42: i8):
    v43 = iconst.i8 2
    v44 = icmp eq v42, v43
    v45 = iadd v42, v42
    return v44, v45
}";

#[test]
fn other_use_keeps_param() {
    let (f, g, n) = opt(OTHER_USE);
    assert!(n > 0, "the == 2 test should fold");
    assert!(g.display().to_string().contains("block2(v42"));
    check2(&f, &g, 2);
}

/// Loop-carried ordering: the back edge passes the param itself, so no
/// push; values must still agree.
const LOOP: &str = "function %t(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v5 = icmp slt v0, v1
    v7 = iconst.i8 1
    v10 = iconst.i8 -1
    v11 = select v5, v10, v7
    v12 = iconst.i64 3
    jump block1(v11, v12)

block1(v20: i8, v21: i64):
    v22 = iconst.i64 1
    v23 = isub v21, v22
    v24 = ineg v20
    v25 = icmp eq v23, v22
    brif v23, block1(v24, v23), block2(v20)

block2(v30: i8):
    v31 = iconst.i8 0
    v32 = icmp slt v30, v31
    return v32
}";

#[test]
fn loop_carried_sets() {
    let (f, g, _) = opt(LOOP);
    check2(&f, &g, 2);
}

/// Fuzz seed 17028: the loop header's only use of `p` is `c = icmp eq p, 0`
/// and the back edge passes `c`. Minting the query on that edge arg
/// (`icmp eq c, 0`) collapses through selord onto `icmp ne p, 0`, which
/// reads `p` itself; dropping `p` then left a detached value in the new
/// compare (verifier: "argument is not attached"). The push must bail.
const LOOP_ARG_READS_PARAM: &str = "function %t(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    v5 = icmp slt v0, v1
    v7 = iconst.i8 1
    v10 = iconst.i8 -1
    v11 = select v5, v10, v7
    v12 = iconst.i64 3
    jump block1(v11, v12)

block1(v20: i8, v21: i64):
    v22 = iconst.i64 1
    v23 = isub v21, v22
    v24 = iconst.i8 0
    v25 = icmp eq v20, v24
    brif v23, block1(v25, v23), block2(v25)

block2(v30: i8):
    return v30
}";

#[test]
fn loop_arg_reads_param() {
    let (f, g, _) = opt(LOOP_ARG_READS_PARAM);
    check2(&f, &g, 2);
}
