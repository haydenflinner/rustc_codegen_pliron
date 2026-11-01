//! A `br_table` whose default is pliron's `unreachable` trap
//! (`PLIRON_SWITCH_KEEPDEAD=1`). An out-of-range index is UB, so:
//! - vendored Cranelift drops the bounds clamp (aarch64 `cmp; b.hs; csel;
//!   csdb` becomes one `mov`);
//! - brchain treats the default as a don't-care. The chain's else edge
//!   takes the first entry's target, so a table with six entries over
//!   four targets still becomes a chain.
//!
//! Each test sets the knob before anything reads it (it is read once).

use cranelift_codegen::ir::{Function, Opcode};
use pliron_clif_opt::diffcheck::{Config, Facts, check};

fn keepdead() {
    unsafe { std::env::set_var("PLIRON_SWITCH_KEEPDEAD", "1") };
}

/// `n`-way table on a loaded index; arm `k` returns `k + 100`.
fn table(n: usize, entries: &[usize]) -> String {
    let mut s = String::from("function %f(i64) -> i64 apple_aarch64 {\nblock0(v0: i64):\n    v1 = load.i32 notrap v0\n");
    let list: Vec<String> = entries.iter().map(|&k| format!("block{}", k + 1)).collect();
    s += &format!("    br_table v1, block99, [{}]\n", list.join(", "));
    for k in 0..n {
        s += &format!("block{}:\n    v{} = iconst.i64 {}\n    return v{}\n", k + 1, 10 + k, 100 + k, 10 + k);
    }
    s += "block99 cold:\n    trap user2\n}\n";
    s
}

#[cfg(target_arch = "aarch64")]
#[test]
fn unreachable_default_drops_the_clamp() {
    use cranelift_codegen::Context;
    use cranelift_codegen::control::ControlPlane;
    use cranelift_codegen::settings::{self, Configurable};
    keepdead();
    let mut b = settings::builder();
    b.set("opt_level", "speed").unwrap();
    let isa = cranelift_codegen::isa::lookup_by_name("aarch64-apple-darwin")
        .unwrap()
        .finish(settings::Flags::new(b))
        .unwrap();
    let all: Vec<usize> = (0..12).collect();
    let f: Function = cranelift_reader::parse_functions(&table(12, &all)).unwrap().remove(0);
    let mut ctx = Context::for_function(f);
    ctx.compile(&*isa, &mut ControlPlane::default()).unwrap();
    let w: Vec<u32> = ctx
        .compiled_code()
        .unwrap()
        .buffer
        .data()
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    // `br xN` is there; no `b.cond` (the clamp's `b.hs`) and no `csdb`.
    assert!(w.iter().any(|x| x & 0xFFFF_FC1F == 0xD61F_0000), "no `br` in {w:08x?}");
    assert!(!w.iter().any(|x| x & 0xFF00_0010 == 0x5400_0000), "conditional branch kept: {w:08x?}");
    assert!(!w.contains(&0xD503_229F), "`csdb` kept: {w:08x?}");
}

#[test]
fn unreachable_default_is_a_chain_dont_care() {
    keepdead();
    // Targets 0,1,0,2,0,3: tests for arms 1-3, the else edge to arm 0.
    let src = table(4, &[0, 1, 0, 2, 0, 3]);
    let before = cranelift_reader::parse_functions(&src).unwrap().remove(0);
    let mut after = before.clone();
    assert!(pliron_clif_opt::brchain::run(&mut after) > 0, "{}", after.display());
    let insts = |f: &Function, op| {
        f.layout.blocks().flat_map(|b| f.layout.block_insts(b)).filter(|&i| f.dfg.insts[i].opcode() == op).count()
    };
    assert_eq!(insts(&after, Opcode::BrTable), 0, "{}", after.display());
    assert_eq!(insts(&after, Opcode::Brif), 3, "{}", after.display());
    if let Err(m) = check(&before, &after, &Facts::default(), &Config::default()) {
        panic!("{m}\n{}", after.display());
    }
}
