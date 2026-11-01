//! loopidiom addresses a fill by any param stepping in lockstep with the
//! counted one: a `mintrip` pre-loop counts with `j` while the stores
//! index by `len` (`base + len*8`). The memset starts at `base + len0*8`
//! and spans the counted trips. Differentially checked; a side param
//! whose step doesn't match the element size is left alone.

use cranelift_codegen::ir::{Function, Opcode};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::FxHashSet;
use target_lexicon::PointerWidth;

fn tcfg() -> TargetFrontendConfig {
    TargetFrontendConfig { default_call_conv: CallConv::SystemV, pointer_width: PointerWidth::U64, page_size_align_log2: 12 }
}

fn has_call(f: &Function) -> bool {
    f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).any(|i| f.dfg.insts[i].opcode() == Opcode::Call)
}

/// for j in 0..n (n >= 1 by the guard) { base[len] = 0; len += step }
fn src(step: i64) -> String {
    format!(
        "function %f(i64, i64, i64) -> i64 {{
    block0(v0: i64, v1: i64, v2: i64):
        v40 = iconst.i64 7
        v41 = iconst.i64 15
        v3 = band v1, v40
        v4 = band v2, v41
        v42 = iconst.i64 0
        brif v4, block1(v42, v3), block2(v3)
    block1(v10: i64, v11: i64):
        v43 = iconst.i64 3
        v12 = ishl v11, v43
        v13 = iadd v0, v12
        v44 = iconst.i64 0
        store.i64 notrap v44, v13
        v45 = iconst.i64 1
        v14 = iadd v10, v45
        v46 = iconst.i64 {step}
        v15 = iadd v11, v46
        v16 = icmp ne v14, v4
        brif v16, block1(v14, v15), block3
    block2(v20: i64):
        return v20
    block3:
        return v3
    }}"
    )
}

#[test]
fn fill_indexed_by_side_param_becomes_memset() {
    let before = cranelift_reader::parse_functions(&src(1)).unwrap().remove(0);
    let mut after = before.clone();
    pliron_clif_opt::loopidiom::run(&mut after, &mut Analysis::default(), &FxHashSet::default(), tcfg(), "f");
    assert!(has_call(&after), "no memset:\n{}", after.display());
    let s = check(&before, &after, &Facts::default(), &Config { trials: 256, ..Config::default() })
        .unwrap_or_else(|m| panic!("{m}\n{}", after.display()));
    assert!(s.compared > 0, "nothing compared: {:?}", s.why);
}

#[test]
fn side_param_off_element_size_stays() {
    let mut f = cranelift_reader::parse_functions(&src(2)).unwrap().remove(0);
    pliron_clif_opt::loopidiom::run(&mut f, &mut Analysis::default(), &FxHashSet::default(), tcfg(), "f");
    assert!(!has_call(&f), "{}", f.display());
}
