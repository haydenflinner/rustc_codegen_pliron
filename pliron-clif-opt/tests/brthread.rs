//! brthread: an edge into a block that only re-tests a condition the edge
//! already decides goes straight to the decided successor (lol_html's
//! `tag_is_one_of!` over `LocalName::Hash`). Differentially checked.

use cranelift_codegen::ir::Function;
use pliron_clif_opt::diffcheck::{Config, Facts, check};
use pliron_clif_opt::pipeline::Analysis;

fn run(src: &str) -> (Function, Function, usize) {
    let before = cranelift_reader::parse_functions(src).unwrap().remove(0);
    let mut after = before.clone();
    let mut an = Analysis::default();
    let (cfg, dt) = an.cfg_dt(&after);
    let (cfg, dt) = (cfg.clone(), dt.clone());
    let k = pliron_clif_opt::brthread::run(&mut after, &cfg, &dt);
    (before, after, k)
}

fn brifs_on(f: &Function, v: &str) -> usize {
    f.display().to_string().lines().filter(|l| l.trim_start().starts_with(&format!("brif {v},"))).count()
}

/// `name == 9691 || name == 6 || name == 558` over `Hash(u64) | Bytes`.
const CHAIN: &str = "function %f(i64) -> i8 {
block0(v0: i64):
    v2 = load.i64 notrap readonly v0
    v3 = iconst.i64 0
    v4 = icmp eq v2, v3
    brif v4, block2, block1
block2:
    v10 = load.i64 notrap readonly v0+8
    v11 = iconst.i64 9691
    v12 = icmp eq v10, v11
    brif v12, block9, block1
block1:
    brif v4, block3, block4
block3:
    v20 = load.i64 notrap readonly v0+8
    v21 = iconst.i64 6
    v22 = icmp eq v20, v21
    brif v22, block9, block4
block4:
    brif v4, block5, block8
block5:
    v30 = load.i64 notrap readonly v0+8
    v31 = iconst.i64 558
    v32 = icmp eq v30, v31
    brif v32, block9, block8
block8:
    v40 = iconst.i8 0
    return v40
block9:
    v41 = iconst.i8 1
    return v41
}";

#[test]
fn variant_retests_disappear() {
    let (before, after, k) = run(CHAIN);
    assert!(k >= 3, "{}", after.display());
    assert_eq!(brifs_on(&after, "v4"), 1, "only the first variant test stays:\n{}", after.display());
    let cfg = Config { trials: 64, ..Config::default() };
    if let Err(m) = check(&before, &after, &Facts::default(), &cfg) {
        panic!("{m}\n{}", after.display());
    }
}

/// A re-test the incoming edge does not decide stays.
#[test]
fn undecided_retest_stays() {
    let (_, after, k) = run("function %f(i64, i64) -> i8 {
block0(v0: i64, v1: i64):
    brif v1, block2, block1
block2:
    jump block1
block1:
    brif v0, block3, block4
block3:
    v40 = iconst.i8 0
    return v40
block4:
    v41 = iconst.i8 1
    return v41
}");
    assert_eq!(k, 0, "{}", after.display());
}
