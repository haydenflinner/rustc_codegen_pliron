//! An early-exit search whose `brif` reads a *data* cond — ifconv's
//! `band (select mask, 1, 0), 1` — not a lane mask. loopvec normalized
//! exit-on-hit conds with `bnot`, which turns {0,1} lanes into
//! {0xff,0xfe}: every lane "ok", so `vall_true` skipped every 64-byte group
//! that held a hit. Real-world hit: `str::split_ascii_whitespace` (via
//! `slice::Iter::position(u8::is_ascii_whitespace)`) in rayon's
//! `execute_strings_split`, 2026-10-21.

use cranelift_codegen::ir::Function;
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use rustc_hash::{FxHashMap, FxHashSet};
use target_lexicon::PointerWidth;

fn parse(src: &str) -> Function {
    cranelift_reader::parse_functions(src).unwrap().remove(0)
}

fn tcfg() -> TargetFrontendConfig {
    TargetFrontendConfig {
        default_call_conv: CallConv::SystemV,
        pointer_width: PointerWidth::U64,
        page_size_align_log2: 12,
    }
}

fn run_vec(f: &mut Function, name: &str) -> usize {
    pliron_clif_opt::loopvec::run(
        f,
        &mut pliron_clif_opt::pipeline::Analysis::default(),
        name,
        &FxHashSet::default(),
        tcfg(),
        true,
        false,
        true,
        &FxHashMap::default(),
        pliron_clif_opt::loopvec::WideCaps::BASE,
    )
}

/// `position(|b| b.is_ascii_whitespace() || b == 0)` over 128 bytes, as
/// loopvec receives it (pipeline dump, plus `== 0` so the sandbox's
/// small-integer memory seeds hit inside the first group).
const SCAN: &str = "function %f(i64) -> i64, i64 {
block0(v0: i64):
    v1 = iconst.i64 128
    v14 = iadd v0, v1
    jump block7

block7:
    v43 = iconst.i64 1
    v80 = iconst.i8 0
    v81 = iconst.i8 1
    v97 = iconst.i8 9
    v98 = iconst.i8 10
    v99 = iconst.i8 12
    v100 = iconst.i8 13
    v101 = iconst.i8 32
    v102 = iconst.i8 1
    jump block9(v0)

block9(v35: i64):
    v112 = isub v35, v0
    v42 = icmp eq v35, v14
    brif v42, block12, block11

block11:
    v44 = iadd.i64 v35, v43
    v49 = load.i8 notrap v35
    v83 = icmp eq v49, v97
    v85 = icmp eq v49, v98
    v86 = bor v83, v85
    v88 = icmp eq v49, v99
    v89 = bor v86, v88
    v91 = icmp eq v49, v100
    v92 = bor v89, v91
    v94 = icmp eq v49, v101
    v95 = bor v92, v94
    v104 = icmp eq v49, v80
    v105 = bor v95, v104
    v96 = select.i8 v105, v81, v80
    v103 = band v96, v102
    brif v103, block17, block16

block16:
    jump block9(v44)

block17:
    v64 = iconst.i64 1
    return v64, v112

block12:
    v68 = iconst.i64 0
    v69 = iconst.i64 0
    return v68, v69
}";

#[test]
fn data_cond_early_exit_finds_hit() {
    let before = parse(SCAN);
    let mut after = before.clone();
    let n = run_vec(&mut after, "scan");
    assert!(n > 0, "expected the scan to vectorize:\n{}", after.display());
    let p0 = before.dfg.block_params(before.layout.entry_block().unwrap())[0];
    let deref: FxHashMap<_, _> = [(p0, (128u64, false))].into_iter().collect();
    let facts = Facts {
        deref: Some(&deref),
        ..Facts::default()
    };
    if let Err(m) = check(&before, &after, &facts, &Config::default()) {
        panic!("{m}\n---- after\n{}", after.display());
    }
}
