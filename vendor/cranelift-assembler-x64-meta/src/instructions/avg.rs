use crate::dsl::{Feature::*, Inst, Length::*, Location::*};
use crate::dsl::{align, fmt, inst, r, rex, rw, vex, w};

#[rustfmt::skip] // Keeps instructions on a single line.
pub fn list() -> Vec<Inst> {
    vec![
        inst("pavgb", fmt("A", [rw(xmm1), r(align(xmm_m128))]), rex([0x66, 0x0F, 0xE0]), (_64b | compat) & sse2).alt(avx, "vpavgb_b"),
        inst("pavgw", fmt("A", [rw(xmm1), r(align(xmm_m128))]), rex([0x66, 0x0F, 0xE3]), (_64b | compat) & sse2).alt(avx, "vpavgw_b"),
        inst("vpavgb", fmt("B", [w(xmm1), r(xmm2), r(xmm_m128)]), vex(L128)._66()._0f().op(0xE0), (_64b | compat) & avx),
        inst("vpavgw", fmt("B", [w(xmm1), r(xmm2), r(xmm_m128)]), vex(L128)._66()._0f().op(0xE3), (_64b | compat) & avx),
        // PLIRON: packed sum of absolute differences — per 8-byte group,
        // `sum(|a_i - b_i|)` into the corresponding 64-bit lane. With a zero
        // operand this is a u8x16 -> u64x2 byte-sum reduction.
        inst("psadbw", fmt("A", [rw(xmm1), r(align(xmm_m128))]), rex([0x66, 0x0F, 0xF6]), (_64b | compat) & sse2).alt(avx, "vpsadbw_b"),
        inst("vpsadbw", fmt("B", [w(xmm1), r(xmm2), r(xmm_m128)]), vex(L128)._66()._0f().op(0xF6), (_64b | compat) & avx),
    ]
}
