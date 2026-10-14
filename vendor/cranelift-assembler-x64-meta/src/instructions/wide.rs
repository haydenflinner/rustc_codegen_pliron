//! 256-bit (VEX.256) and 512-bit (EVEX.512) packed-float forms used by
//! pliron's wide-SIMD mode. Operands reuse the `xmm*` locations: the
//! register encoding is identical and callers only pass full-width values.
use crate::dsl::{Feature::*, Inst, Length::*, Location::*, TupleType::*};
use crate::dsl::{evex, fmt, inst, r, rw, vex, w};

#[rustfmt::skip] // Keeps instructions on a single line.
pub fn list() -> Vec<Inst> {
    let y = || vex(L256);
    let z = |t| evex(L512, t);
    vec![
        inst("vmovups", fmt("YA", [w(xmm1), r(xmm_m128)]), y()._0f().op(0x10).r(), (_64b | compat) & avx),
        inst("vmovups", fmt("YB", [w(xmm_m128), r(xmm1)]), y()._0f().op(0x11).r(), (_64b | compat) & avx),
        inst("vmovups", fmt("ZA", [w(xmm1), r(xmm_m128)]), z(FullMem)._0f().w0().op(0x10).r(), (_64b | compat) & avx512f),
        inst("vmovups", fmt("ZB", [w(xmm_m128), r(xmm1)]), z(FullMem)._0f().w0().op(0x11).r(), (_64b | compat) & avx512f),

        inst("vaddps", fmt("Y", [w(xmm1), r(xmm2), r(xmm_m128)]), y()._0f().op(0x58).r(), (_64b | compat) & avx),
        inst("vsubps", fmt("Y", [w(xmm1), r(xmm2), r(xmm_m128)]), y()._0f().op(0x5C).r(), (_64b | compat) & avx),
        inst("vmulps", fmt("Y", [w(xmm1), r(xmm2), r(xmm_m128)]), y()._0f().op(0x59).r(), (_64b | compat) & avx),
        inst("vdivps", fmt("Y", [w(xmm1), r(xmm2), r(xmm_m128)]), y()._0f().op(0x5E).r(), (_64b | compat) & avx),
        inst("vaddps", fmt("Z", [w(xmm1), r(xmm2), r(xmm_m128)]), z(Full)._0f().w0().op(0x58).r(), (_64b | compat) & avx512f),
        inst("vsubps", fmt("Z", [w(xmm1), r(xmm2), r(xmm_m128)]), z(Full)._0f().w0().op(0x5C).r(), (_64b | compat) & avx512f),
        inst("vmulps", fmt("Z", [w(xmm1), r(xmm2), r(xmm_m128)]), z(Full)._0f().w0().op(0x59).r(), (_64b | compat) & avx512f),
        inst("vdivps", fmt("Z", [w(xmm1), r(xmm2), r(xmm_m128)]), z(Full)._0f().w0().op(0x5E).r(), (_64b | compat) & avx512f),

        inst("vfmadd213ps", fmt("Y", [rw(xmm1), r(xmm2), r(xmm_m128)]), y()._66()._0f38().w0().op(0xA8).r(), (_64b | compat) & fma),
        inst("vfmadd213ps", fmt("Z", [rw(xmm1), r(xmm2), r(xmm_m128)]), z(Full)._66()._0f38().w0().op(0xA8).r(), (_64b | compat) & avx512f),

        inst("vbroadcastss", fmt("Y", [w(xmm1), r(xmm2)]), y()._66()._0f38().w0().op(0x18).r(), (_64b | compat) & avx2),
        inst("vbroadcastss", fmt("Z", [w(xmm1), r(xmm2)]), z(Full)._66()._0f38().w0().op(0x18).r(), (_64b | compat) & avx512f),

        inst("vextractf128", fmt("Y", [w(xmm_m128), r(xmm1), r(imm8)]), y()._66()._0f3a().w0().op(0x19).r().ib(), (_64b | compat) & avx),
        inst("vinsertf128", fmt("Y", [w(xmm1), r(xmm2), r(xmm_m128), r(imm8)]), y()._66()._0f3a().w0().op(0x18).r().ib(), (_64b | compat) & avx),
        inst("vextractf32x4", fmt("Z", [w(xmm_m128), r(xmm1), r(imm8)]), z(Mem128)._66()._0f3a().w0().op(0x19).r().ib(), (_64b | compat) & avx512f),
        inst("vinsertf32x4", fmt("Z", [w(xmm1), r(xmm2), r(xmm_m128), r(imm8)]), z(Mem128)._66()._0f3a().w0().op(0x18).r().ib(), (_64b | compat) & avx512f),
        inst("vcvtph2ps", fmt("Y", [w(xmm1), r(xmm_m128)]),
             y()._66()._0f38().w0().op(0x13).r(), (_64b | compat) & avx),
        inst("vcvtph2ps", fmt("Z", [w(xmm1), r(xmm_m128)]),
             z(HalfMem)._66()._0f38().w0().op(0x13).r(), (_64b | compat) & avx512f),
        inst("vcvtps2ph", fmt("Y", [w(xmm_m128), r(xmm1), r(imm8)]),
             y()._66()._0f3a().w0().op(0x1D).r().ib(), (_64b | compat) & avx),
        inst("vcvtps2ph", fmt("Z", [w(xmm_m128), r(xmm1), r(imm8)]),
             z(HalfMem)._66()._0f3a().w0().op(0x1D).r().ib(), (_64b | compat) & avx512f),
    ]
}
