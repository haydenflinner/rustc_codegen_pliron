//! `PLIRON_JT_NARROW=1` (vendored Cranelift, aarch64 and x86-64): jump
//! tables are written after all code with 1-, 2- or 4-byte entries holding
//! `(target - lowest target) / unit` (unit 4 on aarch64, 1 on x86-64).
//! These tests compile `br_table` functions for the host, run the machine
//! code, and decode the emitted dispatch to check the width actually
//! chosen — including spans exactly at the 1-byte limit (255 units),
//! backward targets, an aliased (jump-only) arm, several tables, and big
//! functions (aarch64: too far for `adr`, stays inline; x86-64: 4-byte
//! entries).
#![cfg(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    any(target_os = "macos", target_os = "linux")
))]

mod jtcommon;
use jtcommon::*;

/// A `br_table` over `n` arms; arm `i` returns `x + k(i)` after `pad(i)`
/// filler `bor`s; the default returns `x - 1`.
fn table_fn(n: usize, pad: impl Fn(usize) -> usize, k: impl Fn(usize) -> i64) -> String {
    let mut s = String::from("function %f(i32, i64) -> i64 {\nblock0(v0: i32, v1: i64):\n");
    let arms: Vec<String> = (0..n).map(|i| format!("block{}", i + 2)).collect();
    s += &format!("    br_table v0, block1, [{}]\n", arms.join(", "));
    s += "block1:\n    v2 = iconst.i64 -1\n    v3 = iadd v1, v2\n    return v3\n";
    let mut v = 10;
    for i in 0..n {
        s += &format!("block{}:\n", i + 2);
        let mut cur = 1;
        for _ in 0..pad(i) {
            s += &format!("    v{v} = bor v{cur}, v{cur}\n");
            cur = v;
            v += 1;
        }
        s += &format!(
            "    v{v} = iconst.i64 {}\n    v{} = iadd v{cur}, v{v}\n    return v{}\n",
            k(i),
            v + 1,
            v + 1
        );
        v += 2;
    }
    s += "}\n";
    s
}

fn hundreds(i: usize) -> i64 {
    100 * i as i64
}

fn check_runs(code: &Code, n: usize, k: impl Fn(usize) -> i64) {
    let f: extern "C" fn(i32, i64) -> i64 = unsafe { std::mem::transmute(code.entry()) };
    for i in 0..n as i32 + 3 {
        let want = if (i as usize) < n { 7i64.wrapping_add(k(i as usize)) } else { 6 };
        assert_eq!(f(i, 7), want, "index {i}");
    }
    assert_eq!(f(-1, 7), 6, "index -1");
}

#[test]
fn width_steps_from_1_to_2_bytes_exactly_at_255_units() {
    narrow_on();
    let n = 6;
    let mut seen = Vec::new();
    // Grow the last arm one filler at a time. A filler is one 4-byte unit
    // on aarch64; on x86-64 it is 3 bytes, so the first two arms' return
    // constants also vary (`mov` encodings of different lengths) to reach every
    // byte offset.
    let (pads, ks): (std::ops::Range<usize>, &[i64]) = if cfg!(target_arch = "x86_64") {
        (50..100, &[0x7f, 0x1_0000_0000, -1, 0])
    } else {
        (200..300, &[0x7f])
    };
    for p in pads {
        for &k0 in ks {
            for &k1 in ks {
                let k = |i: usize| match i {
                    0 => k0,
                    1 => k1,
                    _ => hundreds(i),
                };
                let code = compile(&table_fn(n, |i| if i == n - 1 { p } else { 3 }, k));
                check_runs(&code, n, k);
                let d = decode_dispatch(&code).expect("no deferred dispatch");
                assert_eq!(d.entries.len(), n + DEFAULT_SLOTS);
                seen.push((d.width, d.max_entry()));
            }
        }
    }
    // Width is 1 exactly while the largest entry fits in a byte.
    for &(w, m) in &seen {
        assert_eq!(w, if m <= 255 { 1 } else { 2 }, "{seen:?}");
    }
    assert!(seen.contains(&(1, 255)), "never hit the 1-byte limit: {seen:?}");
    assert!(seen.contains(&(2, 256)), "never just past the 1-byte limit: {seen:?}");
}

#[test]
fn two_byte_table_with_wide_span() {
    narrow_on();
    let n = 40;
    let code = compile(&table_fn(n, |i| 20 + 7 * i, hundreds));
    check_runs(&code, n, hundreds);
    let d = decode_dispatch(&code).unwrap();
    assert_eq!(d.width, 2);
    assert!(d.max_entry() > 255);
}

#[test]
fn backward_and_aliased_targets() {
    narrow_on();
    // block1 (a loop header, emitted before the dispatch) is a table
    // target; block5 is jump-only, so MachBuffer aliases its label to
    // block4's.
    let src = "function %f(i32, i64) -> i64 {
    ss0 = explicit_slot 8
block0(v0: i32, v1: i64):
    v2 = iconst.i64 3
    v90 = stack_addr.i64 ss0
    store v2, v90
    jump block1
block1:
    v91 = stack_addr.i64 ss0
    v3 = load.i64 v91
    brif v3, block2, block3
block2:
    v1001 = iconst.i64 -1
    v4 = iadd v3, v1001
    v92 = stack_addr.i64 ss0
    store v4, v92
    br_table v0, block6, [block1, block4, block5, block6, block1]
block3:
    v1002 = iconst.i64 1000
    v5 = iadd v1, v1002
    return v5
block4:
    v1003 = iconst.i64 4
    v6 = iadd v1, v1003
    return v6
block5:
    jump block4
block6:
    v1004 = iconst.i64 6
    v7 = iadd v1, v1004
    return v7
}
";
    let code = compile(src);
    let d = decode_dispatch(&code).unwrap();
    assert_eq!(d.width, 1);
    assert!(
        d.targets().iter().any(|&t| t < d.dispatch),
        "no backward target: {d:?}"
    );
    let f: extern "C" fn(i32, i64) -> i64 = unsafe { std::mem::transmute(code.entry()) };
    // Index 0/4 loop back to block1 until the counter hits zero.
    assert_eq!(f(0, 1), 1001);
    assert_eq!(f(4, 1), 1001);
    assert_eq!(f(1, 1), 5);
    assert_eq!(f(2, 1), 5);
    assert_eq!(f(3, 1), 7);
    assert_eq!(f(9, 1), 7);
}

/// ~30k instructions in one arm.
fn big() -> String {
    table_fn(8, |i| if i == 3 { 30_000 } else { 2 }, hundreds)
}

#[cfg(target_arch = "aarch64")]
#[test]
fn big_function_falls_back_to_inline_table() {
    narrow_on();
    // The worst-case bound on the code after the dispatch exceeds `adr`'s
    // 1 MB reach, so the table stays inline.
    let code = compile(&big());
    check_runs(&code, 8, hundreds);
    assert!(decode_dispatch(&code).is_none(), "deferred in a huge function");
    assert!(has_inline_dispatch(&code));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn big_function_gets_four_byte_entries() {
    narrow_on();
    // `lea`'s reach is 2 GB; a ~90 KB arm needs 4-byte entries.
    let code = compile(&big());
    check_runs(&code, 8, hundreds);
    let d = decode_dispatch(&code).unwrap();
    assert_eq!(d.width, 4);
    assert!(d.max_entry() > 0xffff);
    assert!(!has_inline_dispatch(&code));
}

#[test]
fn several_tables_in_one_function() {
    narrow_on();
    // Two dispatches; the second table is written after the first.
    let src = "function %f(i32, i64) -> i64 {
block0(v0: i32, v1: i64):
    br_table v0, block1, [block2, block3, block4]
block1:
    v1005 = iconst.i64 -1
    v2 = iadd v1, v1005
    return v2
block2:
    v1006 = iconst.i32 -1
    v3 = iadd v0, v1006
    br_table v3, block5, [block6, block7]
block3:
    v1007 = iconst.i64 3
    v4 = iadd v1, v1007
    return v4
block4:
    v1008 = iconst.i64 4
    v5 = iadd v1, v1008
    return v5
block5:
    v1009 = iconst.i64 50
    v6 = iadd v1, v1009
    return v6
block6:
    v1010 = iconst.i64 60
    v7 = iadd v1, v1010
    return v7
block7:
    v1011 = iconst.i64 70
    v8 = iadd v1, v1011
    return v8
}
";
    let code = compile(src);
    let f: extern "C" fn(i32, i64) -> i64 = unsafe { std::mem::transmute(code.entry()) };
    // v0 = 0 takes block2, where v3 = -1 hits block5.
    assert_eq!(f(0, 1), 51);
    assert_eq!(f(1, 1), 4);
    assert_eq!(f(2, 1), 5);
    assert_eq!(f(3, 1), 0);
    assert_eq!(decode_all(&code).len(), 2);
}
