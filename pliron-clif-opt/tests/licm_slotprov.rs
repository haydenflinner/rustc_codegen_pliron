//! `PLIRON_LICM_SLOTPROV=1`: a stack slot read or written at a variable
//! index (`buf[i]`) is still an object of its own when its address goes
//! nowhere else (`slotprov`). Stores to it then leave loads through other
//! pointers alone, and those loads hoist. Without the rule one `buf[i]`
//! makes the slot "escaped", and every store to it may write anything:
//! hmget inlined into its caller kept five loads of the map's header in
//! the inner loop behind SipHasher's tail buffer.
//!
//! Every test in this binary runs with the knob on.

use cranelift_codegen::ir::{Block, Function, Opcode, Value};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};

/// `v0`: a pointer with 16 dereferenceable bytes and no `noalias` claim,
/// `v1`: the trip count. The loop loads `v0+8`, stores to `ss0` and
/// accesses `ss0` at `INDEX`; `EXTRA` is spliced in ahead of the loop.
fn src(index: &str, access: &str, extra: &str, header_load: &str) -> String {
    format!(
        "function %f(i64, i64) -> i64 {{
    ss0 = explicit_slot 64
    sig0 = (i64)
    fn0 = %g sig0
block0(v0: i64, v1: i64):
    v2 = stack_addr.i64 ss0
    v3 = iconst.i64 0
{extra}    jump block1(v3, v3)
block1(v4: i64, v20: i64):
    v5 = load.i64 notrap {header_load}
    store.i64 notrap v4, v2
    v6 = iadd v2, {index}
{access}
    v8 = iadd v5, v7
    v21 = iadd v20, v8
    v11 = iconst.i64 1
    v9 = iadd v4, v11
    v10 = icmp ult v9, v1
    brif v10, block1(v9, v21), block2
block2:
    return v21
}}"
    )
}

const READ: &str = "    v7 = uload8.i64 notrap v6";
const WRITE: &str = "    istore8 notrap v4, v6\n    v7 = iconst.i64 1";

fn header_load_in_loop(f: &Function) -> bool {
    let h = Block::from_u32(1);
    f.layout.block_insts(h).any(|i| f.dfg.insts[i].opcode() == Opcode::Load)
}

fn run(text: &str) -> Function {
    static ON: std::sync::Once = std::sync::Once::new();
    ON.call_once(|| unsafe { std::env::set_var("PLIRON_LICM_SLOTPROV", "1") });
    let mut f = cranelift_reader::parse_functions(text).unwrap().remove(0);
    let deref: FxHashMap<Value, (u64, bool)> = [(Value::from_u32(0), (16, true))].into_iter().collect();
    pliron_clif_opt::licm::run(
        &mut f,
        &mut Analysis::default(),
        &FxHashSet::default(),
        &FxHashSet::default(),
        &deref,
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashSet::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &pliron::cols::ColMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        false, false,
    );
    let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(&f, &flags) {
        panic!("licm produced invalid IR: {e}\n{}", f.display());
    }
    f
}

#[test]
fn counter_indexed_slot_lets_the_pointer_load_hoist() {
    for access in [READ, WRITE] {
        let f = run(&src("v4", access, "", "v0+8"));
        assert!(!header_load_in_loop(&f), "load through v0 still in the loop:\n{}", f.display());
    }
}

/// `slot + v1`: `v1` is a parameter and may hold an address, so the sum
/// may point into another object (`heap.wrapping_add(&local as usize)`).
#[test]
fn parameter_index_keeps_the_load() {
    for access in [READ, WRITE] {
        let f = run(&src("v1", access, "", "v0+8"));
        assert!(header_load_in_loop(&f), "hoisted past a store that may reach v0:\n{}", f.display());
    }
}

/// The slot's address handed to a call, or written to memory: `v0` may
/// point into the slot.
#[test]
fn escaped_slot_keeps_the_load() {
    for extra in ["    call fn0(v2)\n", "    store.i64 notrap v2, v0\n"] {
        let f = run(&src("v4", READ, extra, "v0+8"));
        assert!(header_load_in_loop(&f), "hoisted past a store to an escaped slot:\n{}", f.display());
    }
}

/// A load of the slot itself against a store at a variable index of the
/// same slot: one object, the load stays.
#[test]
fn same_slot_variable_store_keeps_the_slot_load() {
    let f = run(&src("v4", WRITE, "", "v2+8"));
    assert!(header_load_in_loop(&f), "slot load hoisted past buf[i] = ..:\n{}", f.display());
}
