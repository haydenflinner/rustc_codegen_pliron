//! `copypair`: interleaved `load; store` copy runs become loads-then-stores
//! groups of `i64x2` items. Each case runs the interpreter before and after
//! and checks the shape. The negative cases must stay untouched: the
//! regrouping moves loads above earlier stores, which is only legal when
//! the footprints are provably disjoint (or the loads are `readonly`).

use cranelift_codegen::data_value::DataValue;
use cranelift_codegen::ir::{Function, InstructionData, Opcode};
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
    let n = pliron_clif_opt::copypair::run(&mut g);
    cranelift_codegen::verifier::verify_function(
        &g,
        &cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder()),
    )
    .unwrap_or_else(|e| panic!("{e}\n{}", g.display()));
    (f, g, n)
}

/// (opcode, type bits) of every load/store in layout order.
fn mem_seq(f: &Function) -> Vec<(Opcode, u32)> {
    let mut v = vec![];
    for b in f.layout.blocks() {
        for i in f.layout.block_insts(b) {
            match f.dfg.insts[i] {
                InstructionData::Load {
                    opcode: Opcode::Load,
                    ..
                } => v.push((Opcode::Load, f.dfg.value_type(f.dfg.first_result(i)).bits())),
                InstructionData::Store {
                    opcode: Opcode::Store,
                    args,
                    ..
                } => v.push((Opcode::Store, f.dfg.value_type(args[0]).bits())),
                _ => {}
            }
        }
    }
    v
}

const ARGS: [i64; 4] = [0, 1, -1, 0x1234_5678_9abc_def0];

fn check(f: &Function, g: &Function) {
    for &a in &ARGS {
        for &b in &ARGS {
            let args = [DataValue::I64(a), DataValue::I64(b)];
            assert_eq!(run(f, &args), run(g, &args), "args {a} {b}\n{}", g.display());
        }
    }
}

/// Fill ss0 (64 B) from the args, copy it to ss1 as eight interleaved
/// 8-byte pairs, return a mix of ss1's words.
const SLOT_COPY: &str = "function %t(i64, i64) -> i64 {
    ss0 = explicit_slot 64
    ss1 = explicit_slot 64
block0(v0: i64, v1: i64):
    v2 = stack_addr.i64 ss0
    v3 = stack_addr.i64 ss1
    store notrap v0, v2
    v4 = iadd v0, v1
    store notrap v4, v2+8
    v5 = iadd v4, v1
    store notrap v5, v2+16
    v6 = iadd v5, v0
    store notrap v6, v2+24
    v7 = iadd v6, v1
    store notrap v7, v2+32
    v8 = bxor v7, v0
    store notrap v8, v2+40
    v9 = iadd v8, v1
    store notrap v9, v2+48
    v10 = bxor v9, v1
    store notrap v10, v2+56
    v20 = load.i64 notrap v2
    store notrap v20, v3
    v21 = load.i64 notrap v2+8
    store notrap v21, v3+8
    v22 = load.i64 notrap v2+16
    store notrap v22, v3+16
    v23 = load.i64 notrap v2+24
    store notrap v23, v3+24
    v24 = load.i64 notrap v2+32
    store notrap v24, v3+32
    v25 = load.i64 notrap v2+40
    store notrap v25, v3+40
    v26 = load.i64 notrap v2+48
    store notrap v26, v3+48
    v27 = load.i64 notrap v2+56
    store notrap v27, v3+56
    v30 = load.i64 notrap v3
    v31 = load.i64 notrap v3+24
    v32 = load.i64 notrap v3+40
    v33 = load.i64 notrap v3+56
    v34 = bxor v30, v31
    v35 = iadd v34, v32
    v36 = bxor v35, v33
    return v36
}";

#[test]
fn slot_to_slot_pairs_regroup() {
    let (f, g, n) = opt(SLOT_COPY);
    assert_eq!(n, 1);
    check(&f, &g);
    let seq = mem_seq(&g);
    // 8 initial scalar stores, then 4 x (i64x2 load) in two groups of
    // loads-then-stores (GROUP = 4 items), then 4 scalar reloads.
    let tail: Vec<_> = seq[8..seq.len() - 4].to_vec();
    let want: Vec<(Opcode, u32)> = std::iter::repeat((Opcode::Load, 128))
        .take(4)
        .chain(std::iter::repeat((Opcode::Store, 128)).take(4))
        .collect();
    assert_eq!(tail, want, "{}", g.display());
}

/// Same copy, but within one slot and the destination overlaps the next
/// source word (`dst = src + 8`): a load must not move above the earlier
/// store. The pass has to leave it alone.
const OVERLAP: &str = "function %t(i64, i64) -> i64 {
    ss0 = explicit_slot 64
block0(v0: i64, v1: i64):
    v2 = stack_addr.i64 ss0
    store notrap v0, v2
    v4 = iadd v0, v1
    store notrap v4, v2+8
    v5 = iadd v4, v1
    store notrap v5, v2+16
    v20 = load.i64 notrap v2
    store notrap v20, v2+8
    v21 = load.i64 notrap v2+8
    store notrap v21, v2+16
    v22 = load.i64 notrap v2+16
    store notrap v22, v2+24
    v30 = load.i64 notrap v2+16
    v31 = load.i64 notrap v2+24
    v34 = bxor v30, v31
    return v34
}";

#[test]
fn overlapping_same_slot_copy_untouched() {
    let (f, g, n) = opt(OVERLAP);
    assert_eq!(n, 0, "{}", g.display());
    assert_eq!(mem_seq(&f), mem_seq(&g));
    check(&f, &g);
}

/// Two unrelated pointers: the store through `v1` may alias the later
/// load through `v0`, so without `readonly` nothing moves.
const HEAP: &str = "function %t(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v20 = load.i64 notrap v0
    store notrap v20, v1
    v21 = load.i64 notrap v0+8
    store notrap v21, v1+8
    v22 = load.i64 notrap v0+16
    store notrap v22, v1+16
    v23 = load.i64 notrap v0+24
    store notrap v23, v1+24
    v40 = iconst.i64 0
    return v40
}";

#[test]
fn unknown_aliasing_pointers_untouched() {
    let (_f, g, n) = opt(HEAP);
    assert_eq!(n, 0, "{}", g.display());
}

/// `readonly` loads cannot observe the stores: the run regroups.
const HEAP_RO: &str = "function %t(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v20 = load.i64 notrap readonly can_move v0
    store notrap v20, v1
    v21 = load.i64 notrap readonly can_move v0+8
    store notrap v21, v1+8
    v22 = load.i64 notrap readonly can_move v0+16
    store notrap v22, v1+16
    v23 = load.i64 notrap readonly can_move v0+24
    store notrap v23, v1+24
    v40 = iconst.i64 0
    return v40
}";

#[test]
fn readonly_loads_regroup_without_can_move() {
    let (_f, g, n) = opt(HEAP_RO);
    assert_eq!(n, 1, "{}", g.display());
    let seq = mem_seq(&g);
    assert_eq!(
        seq,
        vec![
            (Opcode::Load, 128),
            (Opcode::Load, 128),
            (Opcode::Store, 128),
            (Opcode::Store, 128)
        ],
        "{}",
        g.display()
    );
    // `can_move` would let the egraph sink the loads back beside the stores.
    for b in g.layout.blocks() {
        for i in g.layout.block_insts(b) {
            if let InstructionData::Load { flags, .. } = g.dfg.insts[i] {
                assert!(!g.dfg.mem_flags[flags].can_move());
            }
        }
    }
}
