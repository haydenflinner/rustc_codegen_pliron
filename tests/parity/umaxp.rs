#![crate_type="lib"]
// memchr's NEON search loop tests four compare results at once with
// `vpmaxq_u8(x, x)` and a 64-bit lane read (`memchr::vector`, used by
// lol_html's tag scanner and lexer through `memchr::memchr`).
use std::arch::aarch64::*;
#[no_mangle]
pub unsafe fn any4(p: *const u8, needle: u8) -> bool {
    let n = vdupq_n_u8(needle);
    let a = vceqq_u8(vld1q_u8(p), n);
    let b = vceqq_u8(vld1q_u8(p.add(16)), n);
    let c = vceqq_u8(vld1q_u8(p.add(32)), n);
    let d = vceqq_u8(vld1q_u8(p.add(48)), n);
    let or = vorrq_u8(vorrq_u8(a, b), vorrq_u8(c, d));
    let m = vpmaxq_u8(or, or);
    vgetq_lane_u64(vreinterpretq_u64_u8(m), 0) != 0
}
#[no_mangle]
pub unsafe fn mask(p: *const u8, needle: u8) -> u64 {
    let a = vceqq_u8(vld1q_u8(p), vdupq_n_u8(needle));
    let s = vshrn_n_u16(vreinterpretq_u16_u8(a), 4);
    vget_lane_u64(vreinterpret_u64_u8(s), 0) & 0x8888888888888888
}
