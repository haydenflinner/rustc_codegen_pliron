//! `PLIRON_SWITCH_PRETEST=1` (vendored Cranelift, aarch64 only): a
//! `br_table` on a narrow `(c - s) mod W` index is padded with `s` copies
//! of the default target and indexed by the raw value — the bounds check
//! is `cmp x, #(s+n); b.hs default` with no subtraction. These tests
//! compile byte switches for the host, run every byte value through the
//! machine code, and decode the dispatch to check the padding is there
//! (and points at the default) — failing without the lowering rules.
#![cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]

mod jtcommon;
use jtcommon::*;

/// A byte switch over `[9, 9 + n)`: `v0 - 9` indexes a table of `n`
/// entries alternating two arms, arm `block2` returns `x + 100`, arm
/// `block3` returns `x + 200`, the default `block1` returns `x - 1`.
fn byte_switch(n: usize) -> String {
    let arms: Vec<String> = (0..n).map(|i| format!("block{}", 2 + i % 2)).collect();
    format!(
        "function %f(i8, i64) -> i64 {{\n\
         block0(v0: i8, v1: i64):\n\
         \x20   v2 = iconst.i8 9\n\
         \x20   v3 = isub v0, v2\n\
         \x20   v4 = uextend.i32 v3\n\
         \x20   br_table v4, block1, [{}]\n\
         block1:\n\
         \x20   v10 = iconst.i64 -1\n\
         \x20   v11 = iadd v1, v10\n\
         \x20   return v11\n\
         block2:\n\
         \x20   v12 = iconst.i64 100\n\
         \x20   v13 = iadd v1, v12\n\
         \x20   return v13\n\
         block3:\n\
         \x20   v14 = iconst.i64 200\n\
         \x20   v15 = iadd v1, v14\n\
         \x20   return v15\n\
         }}\n",
        arms.join(", ")
    )
}

fn pretest_on() {
    // SAFETY: every test in this binary sets the same value before compiling.
    unsafe { std::env::set_var("PLIRON_SWITCH_PRETEST", "1") };
}

fn check_runs(code: &Code) {
    let f: extern "C" fn(u8, i64) -> i64 = unsafe { std::mem::transmute(code.entry()) };
    for b in 0..=255u8 {
        let want = match b {
            9..=14 => 7 + if (b - 9) % 2 == 0 { 100 } else { 200 },
            _ => 6,
        };
        assert_eq!(f(b, 7), want, "byte {b:#x}");
    }
}

#[test]
fn padded_table_indexes_the_raw_byte() {
    pretest_on();
    let n = 6;
    let code = compile(&byte_switch(n));
    check_runs(&code);
    let d = decode_dispatch(&code).expect("no deferred dispatch");
    // 9 padding entries in front of the 6 real ones, all pointing at the
    // default target (the same entry value the holes would encode).
    assert_eq!(d.entries.len(), n + 9, "table not padded: {:?}", d.entries);
    let targets = d.targets();
    let default = d.base; // the default block is the first thing after the dispatch
    let _ = default;
    // The first 9 entries share one target and it is the target of no
    // real entry (the arms alternate two distinct other targets).
    let pad: std::collections::HashSet<usize> = targets[..9].iter().copied().collect();
    assert_eq!(pad.len(), 1, "padding entries disagree: {targets:?}");
    let real: std::collections::HashSet<usize> = targets[9..].iter().copied().collect();
    assert_eq!(real.len(), 2, "arms should alternate two targets: {targets:?}");
    assert!(!real.contains(pad.iter().next().unwrap()), "padding must be the default target");
}

#[test]
fn iadd_form_pads_too() {
    pretest_on();
    // `iadd v0, -9` instead of `isub v0, 9`: the second match form.
    let src = byte_switch(6).replace("v2 = iconst.i8 9\n    v3 = isub v0, v2", "v2 = iconst.i8 -9\n    v3 = iadd v0, v2");
    let code = compile(&src);
    check_runs(&code);
    let d = decode_dispatch(&code).expect("no deferred dispatch");
    assert_eq!(d.entries.len(), 15, "table not padded: {:?}", d.entries);
}
