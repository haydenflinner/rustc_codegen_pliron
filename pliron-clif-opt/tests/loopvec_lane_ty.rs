//! Lane-type admission: a scalar `icmp.i64` (an index compare) inside an
//! `f64` element loop used to enter `masks`, so `emit_val` splatted its
//! i64 operands at `p.vt` — `icmp.f64x2`, which the verifier rejects.
//! The plan must now reject the compare (and anything needing it), not
//! emit a mistyped instruction. Real-world hit: rayon's
//! `partial_insertion_sort` inside criterion.

use cranelift_codegen::ir::{Function, Opcode};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use cranelift_codegen::settings;
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

#[test]
fn int_compare_in_float_loop_stays_scalar() {
    let src = "function %f(i64, i64, i64) {
block0(v0: i64, v1: i64, v2: i64):
    v3 = iconst.i64 0
    v4 = iconst.i64 8
    v20 = iconst.i64 56
    v21 = f64const 0x0.5
    v30 = iconst.i64 1000
    jump block1(v3)
block1(v5: i64):
    v7 = icmp ult v5, v2
    brif v7, block2, block3
block2:
    v11 = iadd v0, v5
    v12 = load.f64 notrap v11
    v60 = iadd.i64 v30, v20
    v50 = icmp ugt v60, v2
    v52 = select v50, v12, v21
    v14 = iadd v1, v5
    store.f64 notrap v52, v14
    v8 = iadd v5, v4
    jump block1(v8)
block3:
    return
}";
    let mut f = parse(src);
    run_vec(&mut f, "f64_i64_cmp");
    let flags = settings::Flags::new(settings::builder());
    if let Err(e) = cranelift_codegen::verify_function(&f, &flags) {
        panic!("{e}\n{}", f.display());
    }
    assert!(
        !f.layout
            .blocks()
            .flat_map(|b| f.layout.block_insts(b))
            .any(|i| f.dfg.insts[i].opcode() == Opcode::Icmp
                && f.dfg.value_type(f.dfg.inst_results(i)[0]).lane_type() == types::F64),
        "emitted icmp on f64 lanes:\n{}",
        f.display()
    );
}

use cranelift_codegen::ir::types;
