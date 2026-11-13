//! `punroll` prices only the body: `unroll_width(n_insts)` never sees
//! the shared cone clone each retained exit emits. Two data exits each
//! cloning ~30 insts behind a 7-inst body add more code than the eight
//! unrolled copies do — the unroll is refused.
//!
//! These are search loops, which `punroll` leaves rolled by default
//! now (`punroll::warm_exit`); the cone pricing still applies under
//! `PLIRON_PUNROLL_SEARCH=1` and under a profile, so this binary sets
//! the knob.

use cranelift_codegen::ir::Function;
use cranelift_codegen::settings;
use pliron_clif_opt::pipeline::Analysis;

/// Rotated `while p != end && *p != a && *p != b`: the header brif is
/// the count test, block2's brif a data exit into `cone1`, and the
/// latch brif's non-header arm a second data exit into `cone2`. Both
/// cones compute on the loaded byte `v12` — a loop value — so each
/// needs a clone of `cone` insts.
fn clif(cone: usize) -> String {
    let chain = |base: usize| -> String {
        let mut s = format!("    v{base} = uextend.i64 v12\n");
        for i in 1..cone {
            s += &format!("    v{} = iadd v{base}, v{base}\n", base + i);
        }
        s
    };
    format!(
        "function %f(i64, i64) -> i64 {{
block0(v0: i64, v1: i64):
    jump block1(v0)
block1(v10: i64):
    v11 = icmp eq v10, v1
    brif v11, block99, block2
block2:
    v12 = load.i8 notrap v10
    v13 = iconst.i8 96
    v14 = icmp eq v12, v13
    v15 = iconst.i64 1
    v16 = iadd v10, v15
    brif v14, block3, block4
block4:
    v17 = icmp ne v12, v13
    brif v17, block1(v16), block5
block3:
{}    return v{}
block5:
{}    return v{}
block99:
    v100 = iconst.i64 0
    return v100
}}",
        chain(200),
        200 + cone - 1,
        chain(300),
        300 + cone - 1,
    )
}

fn run(f: &mut Function) -> usize {
    // SAFETY: both tests set the same value before anything reads it
    // (the knob is read once per process).
    unsafe {
        std::env::set_var("PLIRON_PUNROLL_SEARCH", "1");
    }
    let mut an = Analysis::default();
    pliron_clif_opt::punroll::run(f, &mut an, "conecost", &Default::default())
}

/// Two ~30-inst cones against a 7-inst body: 60 clone insts exceed the
/// 49 the copies add — refuse.
#[test]
fn big_exit_cones_refused() {
    let mut f = cranelift_reader::parse_functions(&clif(31)).unwrap().remove(0);
    assert_eq!(run(&mut f), 0, "unrolled past its cone budget:\n{}", f.display());
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&f, &flags).unwrap_or_else(|e| panic!("{e}"));
}

/// Tiny cones cost one inst each — well under the budget — and the same
/// loop still unrolls.
#[test]
fn small_exit_cones_unroll() {
    let mut f = cranelift_reader::parse_functions(&clif(2)).unwrap().remove(0);
    assert!(run(&mut f) > 0, "loop not unrolled:\n{}", f.display());
    let flags = settings::Flags::new(settings::builder());
    cranelift_codegen::verify_function(&f, &flags).unwrap_or_else(|e| panic!("{e}"));
}
