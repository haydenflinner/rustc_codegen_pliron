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
    // `v2` is a scoped name only, not a `noalias` root (lowering under
    // the knob): to every other pass it is the single-source param it is.
    let na: FxHashSet<Value> = FxHashSet::default();
    let sna: FxHashSet<Value> = [Value::from_u32(2)].into_iter().collect();
    let _g = seed.then(|| {
        pliron_clif_opt::loadfwd::scoped_enter(pliron_clif_opt::loadfwd::scoped_seed(&f, &na, &sna, &breg))
    });
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        &mut f, &cfg, &pf, &ps,
        &na, &sna, &FxHashSet::default(),
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

// The store goes through `v0`, the caller-side name of `v2`'s object:
// the reload must see the stored `v1`, not the `v3` read before it.
const SAME_OBJECT: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0)

block1(v2: i64):
    v3 = load.i64 notrap v2+8
    store.i64 notrap v1, v0+8
    v4 = load.i64 notrap v2+8
    v5 = iadd v3, v4
    return v5
}";

// The reverse: a store through the scoped param is a store to the
// caller's memory. The caller's reload after the clone must not forward
// the value read before it (licm made the same mistake with a `noalias`
// root: regex, rust-json and rust-html-rewriter miscompiled).
const STORE_THROUGH_PARAM: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    v3 = load.i64 notrap v0+8
    jump block1(v0)

block1(v2: i64):
    store.i64 notrap v1, v2+8
    jump block2

block2:
    v4 = load.i64 notrap v0+8
    v5 = iadd v3, v4
    return v5
}";

/// The two operands of the function's last `iadd`, alias-resolved, after
/// loadfwd ran on `src` with the scoped snapshot.
fn sum_operands(src: &str) -> (Value, Value) {
    unsafe { std::env::set_var("PLIRON_SCOPED_NOALIAS", "1") };
    let mut f = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut ctx = Context::new();
    let outer = BasicBlock::new(&mut ctx, None, vec![]);
    let r = BasicBlock::new(&mut ctx, None, vec![]);
    let mut rpar = pliron::cols::ColMap::default();
    rpar.insert(r, outer);
    let breg: FxHashMap<Block, Region> =
        f.layout.blocks().enumerate().map(|(k, b)| (b, if k == 1 { r } else { outer })).collect();
    let na: FxHashSet<Value> = FxHashSet::default();
    let sna: FxHashSet<Value> = [Value::from_u32(2)].into_iter().collect();
    let _g = pliron_clif_opt::loadfwd::scoped_enter(pliron_clif_opt::loadfwd::scoped_seed(&f, &na, &sna, &breg));
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    pliron_clif_opt::loadfwd::run(
        &mut f, &cfg, &pf, &ps,
        &na, &sna, &FxHashSet::default(),
        &FxHashMap::default(), &breg, &rpar,
        &FxHashMap::default(), &FxHashMap::default(), &FxHashMap::default(),
    );
    let add = f
        .layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == Opcode::Iadd)
        .last()
        .unwrap();
    let a = f.dfg.inst_args(add);
    (f.dfg.resolve_aliases(a[0]), f.dfg.resolve_aliases(a[1]))
}

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
    let (v1, v3) = (Value::from_u32(1), Value::from_u32(3));
    // The reload is the stored value (forwarded through either name) or
    // still a load; never the stale `v3`.
    let (a, b) = sum_operands(SAME_OBJECT);
    assert_eq!(a, v3);
    assert_ne!(b, v3, "a store through the caller's name did not kill the scoped loc");
    assert_eq!(b, v1, "the two names of one object did not share the stored value");
}

#[test]
fn store_through_the_param_kills_the_caller_name() {
    let (v1, v3) = (Value::from_u32(1), Value::from_u32(3));
    let (a, b) = sum_operands(STORE_THROUGH_PARAM);
    assert_eq!(a, v3);
    assert_ne!(b, v3, "a store through the scoped param did not kill the caller-named loc");
    assert_eq!(b, v1, "the two names of one object did not share the stored value");
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

/// `loadfwd::scoped_probe` on `src`: (locs the fact put back, loads left).
fn probe(src: &str) -> (u32, usize) {
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
    let na: FxHashSet<Value> = FxHashSet::default();
    let sna: FxHashSet<Value> = [Value::from_u32(2)].into_iter().collect();
    let _g = pliron_clif_opt::loadfwd::scoped_enter(pliron_clif_opt::loadfwd::scoped_seed(&f, &na, &sna, &breg));
    let cfg = cranelift_codegen::flowgraph::ControlFlowGraph::with_function(&f);
    let pf = pliron_clif_opt::proots::facts_cfg(&f, &cfg);
    let ps = pliron_clif_opt::proots::alias_of(&pf);
    let hits = pliron_clif_opt::loadfwd::scoped_probe(|| {
        pliron_clif_opt::loadfwd::run(
            &mut f, &cfg, &pf, &ps,
            &na, &sna, &FxHashSet::default(),
            &FxHashMap::default(), &breg, &rpar,
            &FxHashMap::default(), &FxHashMap::default(), &FxHashMap::default(),
        )
    });
    (hits, loads(&f))
}

// A candidate whose clone holds no store or call a loc has to survive:
// the fact adds nothing, and the `scopedfwd` step must leave the function
// to the regular `loadfwd` (the early run made pulldown-cmark's
// `Rev<Iter<u8>>::try_fold` scanners 40% longer).
const NOTHING_TO_SHELTER: &str = "function %f(i64, i64) -> i64 {
block0(v0: i64, v1: i64):
    jump block1(v0)

block1(v2: i64):
    v3 = load.i64 notrap v2+8
    v4 = load.i64 notrap v2+8
    jump block2

block2:
    store.i64 notrap v4, v1
    v5 = load.i64 notrap v2+8
    v6 = iadd v3, v5
    return v6
}";

#[test]
fn probe_counts_only_what_the_fact_shelters() {
    // The probe rewrites nothing: all three loads are still there.
    assert_eq!(probe(IN_REGION), (1, 3));
    assert_eq!(probe(NOTHING_TO_SHELTER), (0, 3));
    // The store through the caller's name of the same object is not
    // sheltered (rule 5).
    assert_eq!(probe(SAME_OBJECT).0, 0);
    // After a probe the pass still runs in full.
    assert_eq!(run(IN_REGION, true), 2);
}
