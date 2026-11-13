//! `PLIRON_SCOPED_NOALIAS=1` (`loadfwd::ScopedNa`): a loc rooted at an
//! inlined callee's `noalias` param survives a store through a pointer
//! not based on it, but only inside the clone's region, only while the
//! param never leaves SSA tracking, and never against a store through
//! the caller-side name of the same object.

use cranelift_codegen::ir::{Block, Function, Opcode, Value};
use pliron::basic_block::BasicBlock;
use pliron::context::{Context, Ptr};
use rustc_hash::{FxHashMap, FxHashSet};

type Region = Ptr<BasicBlock>;

fn loads(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load)
        .count()
}

/// block1 is the clone (region `r`, child of `outer`); `v2` its `noalias`
/// param. Returns the number of loads left.
fn run(src: &str, seed: bool) -> usize {
    unsafe { std::env::set_var("PLIRON_SCOPED_NOALIAS", "1") };
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut ctx = Context::new();
    let outer = BasicBlock::new(&mut ctx, None, vec![]);
    let r = BasicBlock::new(&mut ctx, None, vec![]);
    let mut rpar = pliron::cols::ColMap::default();
    rpar.insert(r, outer);
    let breg: FxHashMap<Block, Region> = f
        .layout
        .blocks()
        .enumerate()
        .map(|(k, b)| (b, if k == 1 { r } else { outer }))
        .collect();
    let na: FxHashSet<Value> = [Value::from_u32(2)].into_iter().collect();
    let _g = seed.then(|| {
        pliron_clif_opt::loadfwd::scoped_enter(pliron_clif_opt::loadfwd::scoped_seed(&f, &na, &breg))
    });
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        &mut f, &cfg, &pf, &ps,
        &na, &na, &FxHashSet::default(),
        &FxHashMap::default(), &breg, &rpar,
        &FxHashMap::default(), &FxHashMap::default(), &FxHashMap::default(),
    );
    loads(&f)
}

// The store through `v1` (an entry param: not based on `v2`) sits inside
// the clone: the reload of `v2+8` forwards. The same store after the
// clone (block2) kills, so the last load stays.
const IN_REGION: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0)

block1(v2: i64):
    v3 = load.i64 notrap v2+8
    store.i64 notrap v3, v1
    v4 = load.i64 notrap v2+8
    jump block2

block2:
    store.i64 notrap v4, v1
    v5 = load.i64 notrap v2+8
    v6 = iadd v3, v5
    return v6
}";

// The store goes through `v0`, the caller-side name of `v2`'s object.
const SAME_OBJECT: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0)

block1(v2: i64):
    v3 = load.i64 notrap v2+8
    store.i64 notrap v3, v0+8
    v4 = load.i64 notrap v2+8
    return v4
}";

// `v2` is stored to memory (`v1+0`), so a pointer loaded later (`v6`, from
// `v1+8`: not forwardable from that store) may be based on it: the store
// through `v6` must kill `v2+8`, with or without the scoped snapshot.
const ESCAPED: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0)

block1(v2: i64):
    store.i64 notrap v2, v1
    v3 = load.i64 notrap v2+8
    v6 = load.i64 notrap v1+8
    store.i64 notrap v3, v6
    v4 = load.i64 notrap v2+8
    return v4
}";

// `ESCAPED` without the escaping store: the control showing the fact
// would otherwise apply to this shape.
const NOT_ESCAPED: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0)

block1(v2: i64):
    v3 = load.i64 notrap v2+8
    v6 = load.i64 notrap v1+8
    store.i64 notrap v3, v6
    v4 = load.i64 notrap v2+8
    return v4
}";

#[test]
fn foreign_store_in_region_keeps_the_loc() {
    assert_eq!(run(IN_REGION, false), 3, "forwarded without the scoped fact");
    assert_eq!(run(IN_REGION, true), 2, "in-region reload kept, or the post-region one dropped");
}

#[test]
fn caller_name_of_the_same_object_kills() {
    assert_eq!(run(SAME_OBJECT, false), 2);
    assert_eq!(run(SAME_OBJECT, true), 2);
}

#[test]
fn escaped_param_gets_nothing() {
    assert_eq!(run(ESCAPED, false), 3);
    assert_eq!(run(ESCAPED, true), 3, "an escaped param was treated as unaliased");
    assert_eq!(run(NOT_ESCAPED, false), 3);
    assert_eq!(run(NOT_ESCAPED, true), 2, "control: the fact applies without the escape");
}

// The escaped pointer read back from where it was stored: plain forwarding
// turns `v6` into `v2`, the store becomes a same-root write at offset 0 and
// `v2+8` forwards by the offset rule — no scoped fact involved.
const ESCAPED_FORWARDED: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0)

block1(v2: i64):
    store.i64 notrap v2, v1
    v3 = load.i64 notrap v2+8
    v6 = load.i64 notrap v1
    store.i64 notrap v3, v6
    v4 = load.i64 notrap v2+8
    return v4
}";

#[test]
fn escaped_and_read_back_is_ordinary_forwarding() {
    assert_eq!(run(ESCAPED_FORWARDED, false), 1);
    assert_eq!(run(ESCAPED_FORWARDED, true), 1);
}
