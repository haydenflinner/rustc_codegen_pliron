//! `loadfwd` keys locs under `stop` roots (`noalias` + operand-frozen
//! values) but classifies `iso`/`ncc` under `noalias` alone. A call arg
//! that is an operand-frozen `iadd` cursor normalizes to its own root —
//! the escape scan still records the escape under the underlying
//! `noalias` param, so the call's `reach` must carry both names or the
//! `nocap` locs survive a call that mutates them.
//!
//! Real-world shape: `MultiCharEqSearcher::next` passes
//! `&mut self.char_indices.iter` (`iadd self, 16`, region-frozen) to
//! `CharIndices::next` (`nocapture`, mutates the pointee), then reloads
//! the iterator fields. Forwarding the post-call loads to the pre-call
//! values zeroed the consumed-width delta and miscounted
//! `split_whitespace` at `-Os`/wasm inlining limits.

use cranelift_codegen::ir::{Function, Value};
use pliron::basic_block::BasicBlock;
use pliron::context::Context;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};

fn calls(f: &Function) -> Vec<cranelift_codegen::ir::Inst> {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode().is_call())
        .collect()
}

fn loads(f: &Function) -> usize {
    f.layout
        .blocks()
        .flat_map(|b| f.layout.block_insts(b))
        .filter(|&i| f.dfg.insts[i].opcode() == cranelift_codegen::ir::Opcode::Load)
        .count()
}

// v0 is a `noalias` param; v4 = v0 + 16 is operand-frozen (a `stop`
// root). The nocap call receives v4: under `noalias` normalization that
// reaches V(v0), so both post-call loads must reload.
const FN: &str = "function %f(i64) -> i64 {
    sig0 = (i64)
    fn0 = %g sig0

block0(v0: i64):
    v3 = iconst.i64 16
    v4 = iadd v0, v3
    v7 = load.i64 notrap v0+24
    v10 = load.i64 notrap v0+16
    call fn0(v4)
    v40 = load.i64 notrap v0+24
    v43 = load.i64 notrap v0+16
    v57 = isub v40, v43
    v60 = isub v7, v57
    return v60
}";

fn run(f: &mut Function, nocap: &FxHashSet<(cranelift_codegen::ir::Inst, u32)>, ofz: &FxHashMap<Value, Vec<(u64, pliron::context::Ptr<BasicBlock>)>>) -> usize {
    pliron_clif_opt::loadfwd::run(
        f,
        &[Value::from_u32(0)].into_iter().collect(),
        &FxHashSet::default(),
        ofz,
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        &FxHashMap::default(),
        nocap,
    )
}

#[test]
fn frozen_op_arg_reaches_the_underlying_noalias_root() {
    let mut f = cranelift_reader::parse_functions(FN).unwrap().remove(0);
    let mut ctx = Context::new();
    let region = BasicBlock::new(&mut ctx, None, vec![]);
    let ofz: FxHashMap<Value, Vec<(u64, _)>> = [(Value::from_u32(4), vec![(16, region)])]
        .into_iter()
        .collect();
    let nocap: FxHashSet<(cranelift_codegen::ir::Inst, u32)> =
        [(calls(&f)[0], 0)].into_iter().collect();
    run(&mut f, &nocap, &ofz);
    // Both post-call loads must remain: the callee writes through v4.
    assert_eq!(loads(&f), 4, "{}", f.display());
}

#[test]
fn frozen_op_forwarding_preserves_behavior() {
    let before = cranelift_reader::parse_functions(FN).unwrap().remove(0);
    let mut after = before.clone();
    let mut ctx = Context::new();
    let region = BasicBlock::new(&mut ctx, None, vec![]);
    let ofz: FxHashMap<Value, Vec<(u64, _)>> = [(Value::from_u32(4), vec![(16, region)])]
        .into_iter()
        .collect();
    let nocap: FxHashSet<(cranelift_codegen::ir::Inst, u32)> =
        [(calls(&after)[0], 0)].into_iter().collect();
    run(&mut after, &nocap, &ofz);
    let noalias: FxHashSet<Value> = [Value::from_u32(0)].into_iter().collect();
    let facts = Facts {
        noalias: Some(&noalias),
        ..Facts::default()
    };
    let cfg = Config {
        trials: 64,
        ..Config::default()
    };
    if let Err(m) = check(&before, &after, &facts, &cfg) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}
