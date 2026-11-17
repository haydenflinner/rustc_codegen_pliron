//! `punroll` on a do-while whose latch is the counting test: the
//! `slice.iter().filter(..).count()` shape of `contains_rng`, a pointer
//! stepping by 4 until `p + 4 == end`. The latch brif's exit arm leaves
//! the body, so `walk` records it as `latch_exit`; without
//! `PLIRON_PUNROLL_DWLATCH=1` the copy emission keeps that test in every
//! copy (the group guard and K per-element branches), with it the copies
//! 0..K-2 drop it and only the last copy and the scalar remainder test
//! `p + 4 == end`.
//!
//! The knobs are read once per process, so this binary turns them on
//! (and `PLIRON_PUNROLL_COUNTED=1`: counted loops of unknown length stay
//! rolled otherwise). `tests/punroll_condback.rs` covers the knob off.

use cranelift_codegen::ir::{Function, Inst, Opcode, Value};
use cranelift_codegen::settings;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

const COUNT: &str = "function %f(i64, i64, i32, i32) -> i64 {
block0(v0: i64, v1: i64, v2: i32, v3: i32):
    v4 = iconst.i64 0
    jump block1(v0, v4)
block1(v10: i64, v11: i64):
    v12 = load.i32 notrap v10
    v13 = icmp ule v2, v12
    v14 = icmp ule v12, v3
    v15 = band v13, v14
    v16 = uextend.i64 v15
    v17 = iadd v11, v16
    v18 = iconst.i64 4
    v19 = iadd v10, v18
    v20 = icmp eq v19, v1
    brif v20, block2(v17), block1(v19, v17)
block2(v30: i64):
    return v30
}";

fn knobs_on() {
    // SAFETY: every test in this binary sets the same values before any
    // pass reads them.
    unsafe {
        std::env::set_var("PLIRON_PUNROLL_DWLATCH", "1");
        std::env::set_var("PLIRON_PUNROLL_COUNTED", "1");
    }
}

fn insts(f: &Function) -> impl Iterator<Item = Inst> + '_ {
    f.layout.blocks().flat_map(|b| f.layout.block_insts(b))
}

/// Brifs whose condition is an icmp on `bound`: the `p + 4 == end` tests.
fn count_brifs_on(f: &Function, bound: Value) -> usize {
    insts(f)
        .filter(|&i| {
            if f.dfg.insts[i].opcode() != Opcode::Brif {
                return false;
            }
            let c = f.dfg.resolve_aliases(f.dfg.inst_args(i)[0]);
            matches!(f.dfg.value_def(c).inst(), Some(d) if
                f.dfg.insts[d].opcode() == Opcode::Icmp
                    && f.dfg.inst_args(d).iter().any(|&a| f.dfg.resolve_aliases(a) == bound))
        })
        .count()
}

#[test]
fn do_while_counting_latch_is_tested_once_per_group() {
    knobs_on();
    let before = cranelift_reader::parse_functions(COUNT).unwrap().remove(0);
    let mut f = before.clone();
    let bound = f.dfg.block_params(f.layout.entry_block().unwrap())[1];
    let mut an = Analysis::default();
    let n = pliron_clif_opt::punroll::run(&mut f, &mut an, "dwlatch", &Default::default(), false);
    assert!(n > 0, "loop not unrolled:\n{}", f.display());
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&f, &flags).unwrap_or_else(|e| panic!("{e}\n{}", f.display()));
    let loads = insts(&f).filter(|&i| f.dfg.insts[i].opcode() == Opcode::Load).count();
    assert!(loads >= 5, "no group copies:\n{}", f.display());
    // The last group copy and the scalar remainder: 2. Before the knob
    // every one of the K copies kept its own (K + 1 = 9 at K = 8).
    assert!(count_brifs_on(&f, bound) <= 2, "{}", f.display());
    // Same results on random slices (`end` a multiple of 4 past `p`).
    check(&before, &f, &Facts::default(), &Config::default()).unwrap_or_else(|m| panic!("{m}"));
}
