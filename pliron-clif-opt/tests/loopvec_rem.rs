//! `PLIRON_VEC_REM=1`: after `loopvec` converts a loop (four vectors per
//! iteration), the scalar loop it keeps is converted once more with one
//! vector per iteration, so trip counts of `vf .. vf*4` reach vector code
//! (`make_ascii_lowercase` on 16..63 bytes ran byte by byte). Checked
//! against the interpreter at trip counts around each tier's boundary.

use cranelift_codegen::ir::{Function, Opcode};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;
use rustc_hash::{FxHashMap, FxHashSet};
use target_lexicon::PointerWidth;

fn run_vec(f: &mut Function) -> usize {
    // The knob is parsed once per process; every test here wants it on.
    unsafe { std::env::set_var("PLIRON_VEC_REM", "1") };
    pliron_clif_opt::loopvec::run(
        f,
        &mut Analysis::default(),
        "rem",
        &FxHashSet::default(),
        TargetFrontendConfig { default_call_conv: CallConv::SystemV, pointer_width: PointerWidth::U64, page_size_align_log2: 12 },
        true,
        false,
        true,
        &FxHashMap::default(),
        pliron_clif_opt::loopvec::WideCaps::BASE,
    )
}

/// Blocks holding a 16-lane vector load: one per vector loop.
fn vector_load_blocks(f: &Function) -> usize {
    f.layout
        .blocks()
        .filter(|&b| {
            f.layout.block_insts(b).any(|i| {
                f.dfg.insts[i].opcode() == Opcode::Load
                    && f.dfg.inst_results(i).iter().any(|&r| f.dfg.value_type(r).is_vector())
            })
        })
        .count()
}

/// `p[i] = p[i] | ((p[i] - 'A' <u 26) << 5)` over `n` bytes in place
/// (`[u8]::make_ascii_lowercase`).
fn lower(n: u32) -> String {
    format!(
        "function %f(i64) {{
    ss0 = explicit_slot 128, align = 16
block0(v0: i64):
    v1 = stack_addr.i64 ss0
    v2 = iconst.i64 {n}
    v3 = iconst.i64 0
    jump block1(v3)
block1(v4: i64):
    v5 = icmp ult v4, v2
    brif v5, block2, block3
block2:
    v6 = iadd v1, v4
    v7 = load.i8 notrap v6
    v8 = iconst.i8 65
    v9 = isub v7, v8
    v10 = iconst.i8 26
    v11 = icmp ult v9, v10
    v18 = iconst.i8 1
    v12 = band v11, v18
    v13 = iconst.i8 5
    v14 = ishl v12, v13
    v15 = bor v7, v14
    store notrap v15, v6
    v16 = iconst.i64 1
    v17 = iadd v4, v16
    jump block1(v17)
block3:
    return
}}"
    )
}

#[test]
fn remainder_loop_is_vectorized() {
    let before = cranelift_reader::parse_functions(&lower(100).replace("v2 = iconst.i64 100", "v30 = iconst.i64 127\n    v2 = band v0, v30")).unwrap().remove(0);
    let mut after = before.clone();
    let n = run_vec(&mut after);
    assert_eq!(n, 2, "main and remainder conversions\n{}", after.display());
    assert_eq!(vector_load_blocks(&after), 2, "{}", after.display());
}

#[test]
fn every_tier_boundary_matches() {
    for n in [0, 1, 15, 16, 17, 31, 47, 63, 64, 65, 79, 80, 127] {
        let before = cranelift_reader::parse_functions(&lower(n)).unwrap().remove(0);
        let mut after = before.clone();
        let k = run_vec(&mut after);
        // Constant trips: the main loop needs 64, the remainder 8 (`i8x8`).
        if n >= 64 {
            assert!(k >= 1, "n={n}: not vectorized\n{}", after.display());
        }
        let cfg = Config { trials: 4, ..Config::default() };
        if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
            panic!("n={n}: {m}\n---- after\n{}", after.display());
        }
    }
}
