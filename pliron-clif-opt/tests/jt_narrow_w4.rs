//! `PLIRON_JT_WIDTH=2|4` forces at least that entry width, so the 2- and
//! 4-byte encodings run even on tables whose span would fit a byte (a
//! real 4-byte deferred table needs a >256 KB span, which the reach bound
//! never allows). Separate binary: the width knob is read once per process.
// r[verify clif.vendor.jt]
#![cfg(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    any(target_os = "macos", target_os = "linux")
))]

mod jtcommon;
use jtcommon::*;

#[test]
fn forced_four_byte_entries() {
    narrow_on();
    // SAFETY: the only test in this binary.
    unsafe { std::env::set_var("PLIRON_JT_WIDTH", "4") };
    let src = "function %f(i32, i64) -> i64 {
block0(v0: i32, v1: i64):
    br_table v0, block1, [block2, block3, block1, block4]
block1:
    v1012 = iconst.i64 -1
    v2 = iadd v1, v1012
    return v2
block2:
    v1013 = iconst.i64 2
    v3 = iadd v1, v1013
    return v3
block3:
    v1014 = iconst.i64 3
    v4 = iadd v1, v1014
    return v4
block4:
    v1015 = iconst.i64 4
    v5 = iadd v1, v1015
    return v5
}
";
    let code = compile(src);
    let d = decode_dispatch(&code).unwrap();
    assert_eq!(d.width, 4);
    assert_eq!(d.table % 4, 0, "4-byte table misaligned");
    let f: extern "C" fn(i32, i64) -> i64 = unsafe { std::mem::transmute(code.entry()) };
    for (i, want) in [(0, 12), (1, 13), (2, 9), (3, 14), (4, 9), (100, 9)] {
        assert_eq!(f(i, 10), want, "index {i}");
    }
}
