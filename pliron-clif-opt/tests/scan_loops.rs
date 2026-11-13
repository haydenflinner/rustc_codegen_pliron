//! The byte-scan loop fixes of 2026-11-13 (`tests/parity/closing.rs`,
//! `docs/history/2026-11-13-six-features.md`, "Parity by exact counts"):
//! `sameargs` keeps the counter of a pointer/counter pair, `cmpcancel`
//! moves loop-invariant addends out of an exit test and reads sums
//! through single-edge block parameters, and `edgeknown::retests` folds
//! a branch that repeats its predecessor's test.

use cranelift_codegen::ir::{Function, InstructionData, Opcode, Value, ValueDef};
use pliron_clif_opt::{clifpeep, cmpcancel, edgeknown};

fn parse(clif: &str) -> Function {
    cranelift_reader::parse_functions(clif).unwrap().remove(0)
}

fn verify(func: &Function) {
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    cranelift_codegen::verify_function(func, &flags).unwrap_or_else(|e| panic!("{e}\n{}", func.display()));
}

fn block(func: &Function, n: usize) -> cranelift_codegen::ir::Block {
    func.layout.blocks().nth(n).unwrap()
}

fn def_op(func: &Function, v: Value) -> Option<Opcode> {
    match func.dfg.value_def(func.dfg.resolve_aliases(v)) {
        ValueDef::Result(i, _) => Some(func.dfg.insts[i].opcode()),
        _ => None,
    }
}

const LOCKSTEP: &str = "
function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v0, v2)
block1(v3: i64, v4: i64):
    v5 = iconst.i64 1
    v6 = iadd v3, v5
    v7 = iadd v4, v5
    v8 = load.i8 notrap v3
    v9 = uextend.i64 v8
    v10 = icmp eq v9, v1
    brif v10, block2, block1(v6, v7)
block2:
    return v4
}
";

/// What the loop header's one remaining parameter starts from.
fn survivor_entry(func: &Function) -> Value {
    let h = block(func, 1);
    assert_eq!(func.dfg.block_params(h).len(), 1, "{}", func.display());
    let t = func.layout.last_inst(block(func, 0)).unwrap();
    let InstructionData::Jump { destination, .. } = func.dfg.insts[t] else { panic!() };
    func.dfg.resolve_aliases(destination.args(&func.dfg.value_lists).next().unwrap().as_value().unwrap())
}

/// `sameargs` (after the loop transforms) keeps the counter and derives
/// the pointer from it; the count is then ready at the exit.
#[test]
fn sameargs_keeps_the_counter() {
    let mut func = parse(LOCKSTEP);
    let (n, _) = clifpeep::sameargs(&mut func);
    verify(&func);
    assert!(n >= 1, "{}", func.display());
    assert_eq!(def_op(&func, survivor_entry(&func)), Some(Opcode::Iconst), "{}", func.display());
}

/// Plain `webmerge` (what the early `cparm` rounds run) keeps the
/// pointer walk `bytescan`/`punroll` match.
#[test]
fn webmerge_keeps_the_pointer() {
    let mut func = parse(LOCKSTEP);
    let (n, _) = clifpeep::webmerge(&mut func);
    verify(&func);
    assert!(n >= 1, "{}", func.display());
    let e = survivor_entry(&func);
    assert!(matches!(func.dfg.value_def(e), ValueDef::Param(..)), "{}", func.display());
}

/// `(base + start) + idx + 1 != base + len` in a loop becomes
/// `idx + 1 != len - start`, the difference computed before the loop.
#[test]
fn cmpcancel_moves_invariants_out() {
    let mut func = parse(
        "
function %f(i64, i64, i64) -> i64 {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iadd v0, v2
    v4 = iadd v0, v1
    v5 = iconst.i64 0
    jump block1(v5)
block1(v6: i64):
    v7 = iadd v3, v6
    v8 = iconst.i64 1
    v9 = iadd v7, v8
    v10 = iadd v6, v8
    v11 = icmp ne v9, v4
    brif v11, block1(v10), block2
block2:
    return v10
}
",
    );
    assert_eq!(cmpcancel::run(&mut func), 1);
    verify(&func);
    let h = block(&func, 1);
    let t = func.layout.last_inst(h).unwrap();
    let InstructionData::Brif { arg, .. } = func.dfg.insts[t] else { panic!() };
    let ValueDef::Result(ci, _) = func.dfg.value_def(arg) else { panic!() };
    let InstructionData::IntCompare { args, .. } = func.dfg.insts[ci] else { panic!() };
    // Left: the counter's own step. Right: `len - start`, in block0.
    let ValueDef::Result(li, _) = func.dfg.value_def(args[0]) else { panic!() };
    let InstructionData::Binary { opcode: Opcode::Iadd, args: la } = func.dfg.insts[li] else {
        panic!("{}", func.display())
    };
    assert_eq!(la[0], func.dfg.block_params(h)[0], "{}", func.display());
    let ValueDef::Result(ri, _) = func.dfg.value_def(args[1]) else { panic!("{}", func.display()) };
    let InstructionData::Binary { opcode: Opcode::Isub, args: ra } = func.dfg.insts[ri] else {
        panic!("{}", func.display())
    };
    let ep = func.dfg.block_params(block(&func, 0)).to_vec();
    assert_eq!((ra[0], ra[1]), (ep[1], ep[2]), "{}", func.display());
    assert_eq!(func.layout.inst_block(ri), Some(block(&func, 0)));
}

/// A parameter of a block with one incoming edge is that edge's
/// argument: `(base + 1) == base + len` reads as `1 == len`.
#[test]
fn cmpcancel_reads_single_edge_params() {
    let mut func = parse(
        "
function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iadd v0, v1
    v3 = iconst.i64 1
    v4 = iadd v0, v3
    brif v1, block1(v4), block2
block1(v5: i64):
    v6 = icmp eq v5, v2
    brif v6, block2, block3
block2:
    return v0
block3:
    return v1
}
",
    );
    assert_eq!(cmpcancel::run(&mut func), 1);
    verify(&func);
    let b1 = block(&func, 1);
    let ci = func
        .layout
        .block_insts(b1)
        .find(|&i| func.dfg.insts[i].opcode() == Opcode::Icmp)
        .unwrap();
    let InstructionData::IntCompare { args, .. } = func.dfg.insts[ci] else { panic!() };
    let len = func.dfg.block_params(block(&func, 0))[1];
    let ops = [def_op(&func, args[0]), def_op(&func, args[1])];
    assert!(ops.contains(&Some(Opcode::Iconst)), "{}", func.display());
    assert!(args.contains(&len), "{}", func.display());
}

/// The second `len == 0` test sits in a block only the first test's
/// false edge reaches.
#[test]
fn retest_folds_a_repeated_branch() {
    let mut func = parse(
        "
function %f(i64) -> i64 {
block0(v0: i64):
    v1 = iconst.i64 0
    v2 = icmp eq v0, v1
    brif v2, block1, block2
block2:
    v3 = icmp eq v0, v1
    brif v3, block1, block3
block1:
    return v1
block3:
    return v0
}
",
    );
    assert_eq!(edgeknown::retests(&mut func), 1);
    verify(&func);
    let b2 = block(&func, 1);
    let t = func.layout.last_inst(b2).unwrap();
    let InstructionData::Jump { destination, .. } = func.dfg.insts[t] else {
        panic!("{}", func.display())
    };
    assert_eq!(destination.block(&func.dfg.value_lists), block(&func, 3));
    // A test with two ways in stays.
    let mut func = parse(
        "
function %f(i64, i64) -> i64 {
block0(v0: i64, v9: i64):
    v1 = iconst.i64 0
    v2 = icmp eq v0, v1
    brif v2, block1, block2
block1:
    brif v9, block2, block3
block2:
    v3 = icmp eq v0, v1
    brif v3, block3, block4
block3:
    return v1
block4:
    return v0
}
",
    );
    assert_eq!(edgeknown::retests(&mut func), 0);
}

const ANY: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0)
block1(v10: i64):
    v11 = icmp eq v10, v1
    brif v11, block3, block2
block2:
    v12 = load.i8 notrap v10
    v13 = iconst.i8 96
    v14 = icmp ne v12, v13
    v15 = iconst.i64 1
    v16 = iadd v10, v15
    brif v14, block1(v16), block4
block3:
    v17 = iconst.i64 0
    return v17
block4:
    v18 = iconst.i64 1
    return v18
}";

/// A search loop (`while p != end && *p != c`, the data exit going to
/// ordinary code) stays rolled without a trip count, and so does a
/// counted loop whose length nothing tells.
#[test]
fn punroll_leaves_search_loops_rolled() {
    use pliron_clif_opt::pipeline::Analysis;
    let mut f = parse(ANY);
    let bound = f.dfg.block_params(block(&f, 0))[1];
    let mut an = Analysis::default();
    pliron_clif_opt::punroll::run(&mut f, &mut an, "any", &Default::default());
    verify(&f);
    // No group guard: whatever copies exist (peeling may add some),
    // each byte load still has its own `p == end` test in front.
    let insts: Vec<_> = f.layout.blocks().flat_map(|b| f.layout.block_insts(b).collect::<Vec<_>>()).collect();
    let loads = insts.iter().filter(|&&i| f.dfg.insts[i].opcode() == Opcode::Load).count();
    let bound_tests = insts
        .iter()
        .filter(|&&i| {
            f.dfg.insts[i].opcode() == Opcode::Brif
                && matches!(f.dfg.value_def(f.dfg.resolve_aliases(f.dfg.inst_args(i)[0])), ValueDef::Result(d, _)
                    if f.dfg.insts[d].opcode() == Opcode::Icmp
                        && f.dfg.inst_args(d).iter().any(|&a| f.dfg.resolve_aliases(a) == bound))
        })
        .count();
    assert_eq!(loads, bound_tests, "{}", f.display());
    let mut f = parse(
        "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v0, v2)
block1(v10: i64, v11: i64):
    v12 = load.i8 notrap v10
    v13 = iconst.i64 1
    v14 = iadd v10, v13
    v15 = uextend.i64 v12
    v16 = iadd v11, v15
    v17 = icmp ult v14, v1
    brif v17, block1(v14, v16), block2(v16)
block2(v20: i64):
    return v20
}",
    );
    let before = f.display().to_string();
    let mut an = Analysis::default();
    pliron_clif_opt::punroll::run(&mut f, &mut an, "dowhile", &Default::default());
    verify(&f);
    // (Peeling may still copy the body; the group guard's `umin` is
    // what marks an unroll.)
    assert!(!f.display().to_string().contains("umin"), "{}\n{before}", f.display());
}

/// Fuzz seed 74256: `d = 0 + w` in an inner loop, `w` the *outer*
/// header's parameter, and the outer back edge passes `d` for `w` — so
/// `d` is `w`'s step in the outer loop. Sinking `d` out of the inner
/// loop must read `w` itself, not "the step of `w`" (which is `d`).
#[test]
fn exit_sink_does_not_read_a_value_through_itself() {
    let mut f = parse(
        "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v0)
block1(v3: i64):
    jump block2(v2)
block2(v4: i64):
    v5 = iadd v2, v3
    v6 = iconst.i64 1
    v7 = iadd v4, v6
    v8 = icmp ne v7, v1
    brif v8, block2(v7), block3
block3:
    v10 = isub v3, v0
    v9 = icmp eq v5, v1
    brif v9, block4, block1(v5)
block4:
    v11 = iadd v5, v10
    return v11
}",
    );
    pliron_clif_opt::edgesink::run(&mut f);
    verify(&f);
    // The inner loop keeps only its counter.
    let inner = block(&f, 2);
    let adds = f
        .layout
        .block_insts(inner)
        .filter(|&i| matches!(f.dfg.insts[i].opcode(), Opcode::Iadd | Opcode::Isub))
        .count();
    assert_eq!(adds, 1, "{}", f.display());
}

/// Fuzz seed 76003: as above, but a second instruction reads the outer
/// header's parameter, and its step — computed in the inner loop — is
/// itself sunk out of that inner loop first. The second instruction's
/// clone must not name the step.
#[test]
fn exit_sink_does_not_read_a_step_that_was_sunk() {
    let mut f = parse(
        "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v2 = iconst.i64 0
    jump block1(v0)
block1(v3: i64):
    v10 = isub v3, v0
    jump block2(v2)
block2(v4: i64):
    v5 = iadd v2, v3
    v6 = iconst.i64 1
    v7 = iadd v4, v6
    v8 = icmp ne v7, v1
    brif v8, block2(v7), block3
block3:
    v9 = icmp eq v5, v1
    brif v9, block4, block1(v5)
block4:
    v11 = iadd v5, v10
    return v11
}",
    );
    pliron_clif_opt::edgesink::run(&mut f);
    verify(&f);
}
