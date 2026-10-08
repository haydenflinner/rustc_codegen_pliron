//! x86-64 SIMD encoding tests: MMX, 3DNow!, SSE through SSE4.2, AVX, AVX2,
//! FMA, AVX-512 and its subsets, FP16, the VEX and XOP extensions, BMI, AMX
//! and the newer system instructions.
//!
//! Every expected byte string here was produced by an oracle — llvm-mc 22.1
//! unless a comment says GNU as 2.46; the extension tests from
//! `fma_orders_and_the_named_predicates_fold_into_the_opcode` on were taken
//! where GNU as 2.47 and llvm-mc 22 agree, or from GNU as 2.47 where a comment
//! says so — and never by rsasm itself. The differential corpora in
//! tools/mc-diff and tools/gas-diff cover far more; this file pins the cases
//! that exercise a distinct piece of the encoder, so a regression names the
//! piece that broke.
#![cfg(feature = "x86")]

mod common;
use common::*;

/// Asserts that `src` assembles to `want`, written as space-separated hex.
#[track_caller]
fn enc(src: &str, want: &str) {
    let got = hex(&text(src));
    assert_eq!(got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
}

/// The same, in Intel syntax.
#[track_caller]
fn intel(src: &str, want: &str) {
    let full = format!(".intel_syntax noprefix\n{src}");
    let got = hex(&text(&full));
    assert_eq!(got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
}

/// The same, in 32-bit mode.
#[track_caller]
fn enc32(src: &str, want: &str) {
    let got = hex(&text_for("i386", src));
    assert_eq!(got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
}

/// Asserts that `src` is rejected with a diagnostic containing `needle`.
#[track_caller]
fn rejects(src: &str, needle: &str) {
    let e = errors(src);
    assert!(
        e.contains(needle),
        "\nsource: {src}\nwanted: {needle}\n   got: {e}"
    );
}

#[test]
fn mmx_packed_arithmetic_and_moves() {
    enc("paddb %mm1, %mm2", "0f fc d1");
    enc("paddw (%rax), %mm3", "0f fd 18");
    enc("pmaddwd %mm1, %mm2", "0f f5 d1");
    enc("pxor %mm1, %mm2", "0f ef d1");
    enc("pcmpgtd %mm1, %mm2", "0f 66 d1");
    enc("packuswb %mm1, %mm2", "0f 67 d1");
    enc("punpckhdq %mm1, %mm2", "0f 6a d1");
    enc("emms", "0f 77");
    enc("movd %eax, %mm0", "0f 6e c0");
    enc("movd %mm0, (%rax)", "0f 7e 00");
    enc("pshufw $0x1b, %mm1, %mm2", "0f 70 d1 1b");
    enc("pmovmskb %mm1, %eax", "0f d7 c1");
}

/// `psllw $3, %mm2` has no reg-field operand: the register goes to r/m and the
/// operation lives in the `/6`.
#[test]
fn mmx_shift_by_immediate_uses_a_digit_extension() {
    enc("psllw %mm1, %mm2", "0f f1 d1");
    enc("psllw $3, %mm2", "0f 71 f2 03");
    enc("psrld $4, %mm5", "0f 72 d5 04");
    enc("psraw $1, %mm0", "0f 71 e0 01");
    enc("psllq $63, %mm7", "0f 73 f7 3f");
}

/// AT&T `movq` means `mov` with a `q` suffix whenever that fits the operands, and
/// the MMX/SSE quadword move only when it does not.
#[test]
fn movq_is_mov_with_a_suffix_first_and_the_vector_move_second() {
    enc("movq %rbx, %rax", "48 89 d8");
    enc("movq $1, %rax", "48 c7 c0 01 00 00 00");
    enc("movq %mm1, %mm2", "0f 6f d1");
    enc("movq (%rax), %mm0", "0f 6f 00");
    enc("movq %rax, %mm0", "48 0f 6e c0");
    enc("movq %mm0, %rax", "48 0f 7e c0");
    enc("movq %xmm0, %xmm1", "f3 0f 7e c8");
    enc("movq (%rax), %xmm1", "f3 0f 7e 08");
    enc("movq %xmm1, (%rax)", "66 0f d6 08");
    enc("movq %r8, %xmm9", "66 4d 0f 6e c8");
}

/// 3DNow! is `0F 0F /r` followed by a byte that picks the operation, in the
/// position an immediate would take — after any displacement.
#[test]
fn three_dnow_selects_the_operation_with_a_trailing_suffix_byte() {
    enc("pfadd %mm1, %mm2", "0f 0f d1 9e");
    enc("pfadd 8(%rax), %mm2", "0f 0f 50 08 9e");
    enc("pfsub %mm1, %mm2", "0f 0f d1 9a");
    enc("pfsubr %mm1, %mm2", "0f 0f d1 aa");
    enc("pfmul %mm1, %mm2", "0f 0f d1 b4");
    enc("pfrcp %mm1, %mm2", "0f 0f d1 96");
    enc("pfrsqrt %mm1, %mm2", "0f 0f d1 97");
    enc("pfmax %mm1, %mm2", "0f 0f d1 a4");
    enc("pfmin %mm1, %mm2", "0f 0f d1 94");
    enc("pfcmpeq %mm1, %mm2", "0f 0f d1 b0");
    enc("pfcmpge %mm1, %mm2", "0f 0f d1 90");
    enc("pfcmpgt %mm1, %mm2", "0f 0f d1 a0");
    enc("pfacc %mm1, %mm2", "0f 0f d1 ae");
    enc("pf2id %mm1, %mm2", "0f 0f d1 1d");
    enc("pi2fd %mm1, %mm2", "0f 0f d1 0d");
    enc("pavgusb %mm1, %mm2", "0f 0f d1 bf");
    enc("pmulhrw %mm1, %mm2", "0f 0f d1 b7");
    enc("femms", "0f 0e");
    enc("prefetch (%rax)", "0f 0d 00");
    enc("prefetchw (%rax)", "0f 0d 08");
    // 3DNow!+
    enc("pf2iw %mm1, %mm2", "0f 0f d1 1c");
    enc("pi2fw %mm1, %mm2", "0f 0f d1 0c");
    enc("pfnacc %mm1, %mm2", "0f 0f d1 8a");
    enc("pfpnacc %mm1, %mm2", "0f 0f d1 8e");
    enc("pswapd %mm1, %mm2", "0f 0f d1 bb");
}

/// The RIP-relative bias must include the suffix byte that follows the
/// displacement. llvm-mc 22.1 gets this wrong (it addresses `data+1`), so this
/// expectation comes from GNU as, confirmed with objdump.
#[test]
fn three_dnow_rip_relative_counts_the_suffix_byte() {
    enc(
        "pfadd data(%rip), %mm0\nret\ndata:\n.quad 0",
        "0f 0f 05 01 00 00 00 9e c3 00 00 00 00 00 00 00 00",
    );
}

/// One opcode, four meanings: no prefix is packed single, `66` packed double,
/// `F3` scalar single and `F2` scalar double. The prefix precedes REX.
#[test]
fn sse_mandatory_prefix_selects_the_flavour() {
    enc("addps %xmm1, %xmm2", "0f 58 d1");
    enc("addpd %xmm1, %xmm2", "66 0f 58 d1");
    enc("addss %xmm1, %xmm2", "f3 0f 58 d1");
    enc("addsd %xmm1, %xmm2", "f2 0f 58 d1");
    enc("addss (%rax), %xmm2", "f3 0f 58 10");
    enc("addsd %xmm9, %xmm10", "f2 45 0f 58 d1");
    enc("sqrtss %xmm1, %xmm2", "f3 0f 51 d1");
    enc("rsqrtps %xmm1, %xmm2", "0f 52 d1");
    enc("rcpss %xmm1, %xmm2", "f3 0f 53 d1");
    enc("andnpd %xmm1, %xmm2", "66 0f 55 d1");
    enc("xorps %xmm1, %xmm2", "0f 57 d1");
}

#[test]
fn sse_moves() {
    enc("movaps %xmm1, %xmm2", "0f 28 d1");
    enc("movaps %xmm2, (%rax)", "0f 29 10");
    enc("movups %xmm1, 8(%r9)", "41 0f 11 49 08");
    enc("movss %xmm1, %xmm2", "f3 0f 10 d1");
    enc("movss %xmm2, (%rax)", "f3 0f 11 10");
    enc("movsd %xmm2, 16(%rsp)", "f2 0f 11 54 24 10");
    enc("movdqa (%rax), %xmm2", "66 0f 6f 10");
    enc("movdqu %xmm10, (%rax)", "f3 44 0f 7f 10");
    enc("movhlps %xmm1, %xmm2", "0f 12 d1");
    enc("movlps (%rax), %xmm1", "0f 12 08");
    enc("movntdq %xmm1, (%rax)", "66 0f e7 08");
    enc("movd %eax, %xmm0", "66 0f 6e c0");
    enc("movd %xmm0, %eax", "66 0f 7e c0");
    enc("movmskps %xmm1, %eax", "0f 50 c1");
    enc("maskmovdqu %xmm1, %xmm2", "66 0f f7 d1");
}

/// The `l`/`q` suffix of the scalar integer conversions picks REX.W.
#[test]
fn sse_compares_shuffles_and_conversions() {
    enc("cmpps $1, %xmm1, %xmm2", "0f c2 d1 01");
    enc("cmpsd $7, (%rax), %xmm2", "f2 0f c2 10 07");
    enc("ucomiss (%rax), %xmm2", "0f 2e 10");
    enc("shufps $0x1b, %xmm1, %xmm2", "0f c6 d1 1b");
    enc("pshufd $0x1b, %xmm1, %xmm2", "66 0f 70 d1 1b");
    enc("pshuflw $0x1b, %xmm1, %xmm2", "f2 0f 70 d1 1b");
    enc("unpckhpd %xmm1, %xmm2", "66 0f 15 d1");
    enc("cvtsi2ssl %eax, %xmm1", "f3 0f 2a c8");
    enc("cvtsi2ssq %rax, %xmm1", "f3 48 0f 2a c8");
    enc("cvtsi2sdl (%rax), %xmm1", "f2 0f 2a 08");
    enc("cvttss2si %xmm1, %rax", "f3 48 0f 2c c1");
    enc("cvtsd2si %xmm1, %r10", "f2 4c 0f 2d d1");
    enc("cvtps2pd %xmm1, %xmm2", "0f 5a d1");
    enc("cvttpd2dq %xmm1, %xmm2", "66 0f e6 d1");
    enc("cvtpi2ps %mm1, %xmm2", "0f 2a d1");
}

#[test]
fn sse2_integer_operations_are_the_mmx_opcodes_under_66() {
    enc("paddb %xmm1, %xmm2", "66 0f fc d1");
    enc("paddq %xmm11, %xmm12", "66 45 0f d4 e3");
    enc("pmuludq %xmm1, %xmm2", "66 0f f4 d1");
    enc("psllw $3, %xmm2", "66 0f 71 f2 03");
    enc("psrlq $4, %xmm10", "66 41 0f 73 d2 04");
    enc("psrldq $4, %xmm1", "66 0f 73 d9 04");
    enc("pslldq $8, %xmm15", "66 41 0f 73 ff 08");
    enc("punpcklqdq %xmm1, %xmm2", "66 0f 6c d1");
    enc("pmovmskb %xmm1, %eax", "66 0f d7 c1");
    enc("pinsrw $2, %eax, %xmm0", "66 0f c4 c0 02");
    enc("pextrw $2, %xmm0, %eax", "66 0f c5 c0 02");
}

#[test]
fn sse3_ssse3_and_sse4() {
    enc("haddpd %xmm1, %xmm2", "66 0f 7c d1");
    enc("movddup %xmm1, %xmm2", "f2 0f 12 d1");
    enc("lddqu (%rax), %xmm1", "f2 0f f0 08");
    enc("pshufb %xmm1, %xmm2", "66 0f 38 00 d1");
    enc("pshufb %mm1, %mm2", "0f 38 00 d1");
    enc("pabsd %mm1, %mm2", "0f 38 1e d1");
    enc("palignr $3, %xmm1, %xmm2", "66 0f 3a 0f d1 03");
    enc("ptest %xmm1, %xmm2", "66 0f 38 17 d1");
    enc("pcmpeqq %xmm1, %xmm2", "66 0f 38 29 d1");
    enc("pmulld %xmm1, %xmm2", "66 0f 38 40 d1");
    enc("pmovzxdq (%rax), %xmm2", "66 0f 38 35 10");
    enc("pblendvb %xmm0, %xmm1, %xmm2", "66 0f 38 10 d1");
    enc("pblendw $0xaa, %xmm1, %xmm2", "66 0f 3a 0e d1 aa");
    enc("roundps $1, %xmm1, %xmm2", "66 0f 3a 08 d1 01");
    enc("roundsd $4, %xmm1, %xmm2", "66 0f 3a 0b d1 04");
    enc("insertps $0x10, %xmm1, %xmm2", "66 0f 3a 21 d1 10");
    enc("pextrb $1, %xmm0, (%rax)", "66 0f 3a 14 00 01");
    enc("pextrq $1, %xmm0, %rax", "66 48 0f 3a 16 c0 01");
    enc("pinsrq $1, %rax, %xmm0", "66 48 0f 3a 22 c0 01");
    enc("pcmpgtq %xmm1, %xmm2", "66 0f 38 37 d1");
    enc("pcmpestri $0x0c, %xmm1, %xmm2", "66 0f 3a 61 d1 0c");
    enc("pcmpistri $0x0c, %xmm1, %xmm2", "66 0f 3a 63 d1 0c");
}

/// `crc32`'s suffix names the source width; a 16-bit source still needs `66`,
/// which must come before the mandatory `F2`.
#[test]
fn crc32_popcnt_aes_and_clmul() {
    enc("crc32b %al, %eax", "f2 0f 38 f0 c0");
    enc("crc32w %ax, %eax", "66 f2 0f 38 f1 c0");
    enc("crc32l %eax, %eax", "f2 0f 38 f1 c0");
    enc("crc32q %rax, %rax", "f2 48 0f 38 f1 c0");
    enc("crc32b %al, %rax", "f2 48 0f 38 f0 c0");
    enc("popcnt %eax, %ebx", "f3 0f b8 d8");
    enc("popcntw %ax, %bx", "66 f3 0f b8 d8");
    enc("popcntq %rax, %rbx", "f3 48 0f b8 d8");
    enc("aesenc %xmm1, %xmm2", "66 0f 38 dc d1");
    enc("aesenclast %xmm1, %xmm2", "66 0f 38 dd d1");
    enc("aesdec %xmm1, %xmm2", "66 0f 38 de d1");
    enc("aesdeclast %xmm1, %xmm2", "66 0f 38 df d1");
    enc("aesimc %xmm1, %xmm2", "66 0f 38 db d1");
    enc("aeskeygenassist $1, %xmm1, %xmm2", "66 0f 3a df d1 01");
    enc("pclmulqdq $0x11, %xmm1, %xmm2", "66 0f 3a 44 d1 11");
    enc("aesenc (%rax), %xmm9", "66 44 0f 38 dc 08");
}

/// `C5` has room for `R`, `vvvv`, `L` and `pp` only, and implies the `0F` map.
/// Anything that needs `X`, `B`, `W` or another map takes `C4`.
#[test]
fn vex_two_byte_form_only_when_x_b_and_w_are_clear() {
    enc("vaddps %xmm1, %xmm2, %xmm3", "c5 e8 58 d9");
    enc("vaddps %ymm1, %ymm2, %ymm3", "c5 ec 58 d9");
    // R alone still fits in two bytes.
    enc("vaddps %xmm1, %xmm2, %xmm10", "c5 68 58 d1");
    // So does an extended vvvv.
    enc("vaddps %xmm1, %xmm10, %xmm2", "c5 a8 58 d1");
    // B, X and W do not.
    enc("vaddps %xmm12, %xmm13, %xmm14", "c4 41 10 58 f4");
    enc("vaddps (%r8), %xmm2, %xmm1", "c4 c1 68 58 08");
    enc("vaddps (%rax,%r9), %xmm2, %xmm1", "c4 a1 68 58 0c 08");
    enc("vmovq %rax, %xmm1", "c4 e1 f9 6e c8");
    // Nor does the 0F 38 map.
    enc("vpshufb %ymm1, %ymm2, %ymm3", "c4 e2 6d 00 d9");
    enc("vzeroupper", "c5 f8 77");
    enc("vzeroall", "c5 fc 77");
}

/// When a move's source needs `B` and its destination does not, both reference
/// assemblers switch to the store opcode, which puts the source in `R` instead.
#[test]
fn vex_register_moves_use_the_store_opcode_to_avoid_three_bytes() {
    enc("vmovaps %xmm9, %xmm1", "c5 78 29 c9");
    enc("vmovaps %xmm1, %xmm9", "c5 78 28 c9");
    enc("vmovups %ymm12, %ymm0", "c5 7c 11 e0");
    enc("vmovdqa %xmm9, %xmm1", "c5 79 7f c9");
    enc("vmovq %xmm9, %xmm1", "c5 79 d6 c9");
    enc("vmovss %xmm9, %xmm2, %xmm3", "c5 6a 11 cb");
}

/// Three-operand forms put the first source in `vvvv`; shift-by-immediate puts the
/// *destination* there and the source in r/m.
#[test]
fn vex_vvvv_carries_the_non_destructive_source() {
    enc("vsubps %ymm1, %ymm2, %ymm3", "c5 ec 5c d9");
    enc("vaddsd (%rax), %xmm2, %xmm3", "c5 eb 58 18");
    enc("vcmpps $1, %ymm1, %ymm2, %ymm3", "c5 ec c2 d9 01");
    enc("vpsllw $3, %ymm1, %ymm2", "c5 ed 71 f1 03");
    enc("vpsrad $3, %ymm11, %ymm12", "c4 c1 1d 72 e3 03");
    // A shift count register is an xmm even at 256 bits.
    enc("vpsllw %xmm3, %ymm1, %ymm2", "c5 f5 f1 d3");
    enc("vcvtsi2ss %eax, %xmm1, %xmm2", "c5 f2 2a d0");
    enc("vcvtsi2ssq %rax, %xmm1, %xmm2", "c4 e1 f2 2a d0");
    enc("vpinsrq $1, %rax, %xmm1, %xmm2", "c4 e3 f1 22 d0 01");
}

#[test]
fn vex_is4_names_a_register_in_the_immediate_byte() {
    enc("vblendvps %ymm4, %ymm1, %ymm2, %ymm3", "c4 e3 6d 4a d9 40");
    enc("vpblendvb %xmm4, %xmm1, %xmm2, %xmm3", "c4 e3 69 4c d9 40");
}

#[test]
fn avx_lanes_broadcasts_and_permutes() {
    enc("vbroadcastss (%rax), %ymm1", "c4 e2 7d 18 08");
    enc("vbroadcastsd %xmm0, %ymm1", "c4 e2 7d 19 c8");
    enc("vbroadcasti128 (%rax), %ymm1", "c4 e2 7d 5a 08");
    enc("vpbroadcastd %xmm0, %ymm1", "c4 e2 7d 58 c8");
    enc("vinsertf128 $1, %xmm1, %ymm2, %ymm3", "c4 e3 6d 18 d9 01");
    enc("vextracti128 $1, %ymm1, (%rax)", "c4 e3 7d 39 08 01");
    enc("vperm2i128 $0x31, %ymm1, %ymm2, %ymm3", "c4 e3 6d 46 d9 31");
    enc("vpermq $0x1b, %ymm1, %ymm2", "c4 e3 fd 00 d1 1b");
    enc("vpermilps $0x1b, %ymm1, %ymm2", "c4 e3 7d 04 d1 1b");
    enc("vpsllvq %ymm1, %ymm2, %ymm3", "c4 e2 ed 47 d9");
    enc("vmaskmovps %ymm3, %ymm2, (%rax)", "c4 e2 6d 2e 18");
    enc("vcvtpd2ps %ymm1, %xmm2", "c5 fd 5a d1");
    enc("vpmovsxbw %xmm1, %ymm2", "c4 e2 7d 20 d1");
}

/// The index register's class follows the index count, which is not always the
/// destination's: four qword indices yield four dword results.
#[test]
fn avx2_gathers_take_a_vector_index_in_the_sib_byte() {
    enc(
        "vgatherdps %xmm2, (%rax,%xmm1,4), %xmm3",
        "c4 e2 69 92 1c 88",
    );
    enc(
        "vgatherdps %ymm2, (%rax,%ymm1,4), %ymm3",
        "c4 e2 6d 92 1c 88",
    );
    enc(
        "vgatherqps %xmm2, (%rax,%ymm1,4), %xmm3",
        "c4 e2 6d 93 1c 88",
    );
    enc(
        "vgatherdpd %ymm2, (%rax,%xmm1,8), %ymm3",
        "c4 e2 ed 92 1c c8",
    );
    enc(
        "vpgatherdd %ymm2, 4(%rax,%ymm9,2), %ymm3",
        "c4 a2 6d 90 5c 48 04",
    );
    enc(
        "vpgatherqq %xmm2, (%rax,%xmm13,8), %xmm3",
        "c4 a2 e9 91 1c e8",
    );
}

#[test]
fn fma_orders_and_precisions() {
    enc("vfmadd132ps %xmm1, %xmm2, %xmm3", "c4 e2 69 98 d9");
    enc("vfmadd213pd %ymm1, %ymm2, %ymm3", "c4 e2 ed a8 d9");
    enc("vfmadd231ss %xmm1, %xmm2, %xmm3", "c4 e2 69 b9 d9");
    enc("vfmadd231sd (%rax), %xmm2, %xmm3", "c4 e2 e9 b9 18");
    enc("vfnmsub231ss %xmm1, %xmm2, %xmm3", "c4 e2 69 bf d9");
    enc("vfmsubadd231pd %ymm1, %ymm2, %ymm3", "c4 e2 ed b7 d9");
}

/// `R'` extends reg, `V'` extends `vvvv`, and for a register-direct r/m the
/// otherwise unused `X` supplies its fifth bit.
#[test]
fn evex_prefix_and_high_register_bits() {
    enc("vaddps %zmm1, %zmm2, %zmm3", "62 f1 6c 48 58 d9");
    enc("vaddps %zmm17, %zmm2, %zmm3", "62 b1 6c 48 58 d9");
    enc("vaddps %zmm1, %zmm18, %zmm3", "62 f1 6c 40 58 d9");
    enc("vaddps %zmm1, %zmm2, %zmm19", "62 e1 6c 48 58 d9");
    enc("vaddps %zmm25, %zmm26, %zmm27", "62 01 2c 40 58 d9");
    enc("vaddps %zmm9, %zmm10, %zmm11", "62 51 2c 48 58 d9");
    // A high register forces EVEX even at 128 bits.
    enc("vaddps %xmm16, %xmm1, %xmm2", "62 b1 74 08 58 d0");
    enc("vmovaps %xmm17, %xmm1", "62 b1 7c 08 28 c9");
    // EVEX has no shorter form to reach for, so no store-opcode swap.
    enc("vmovdqa32 %zmm9, %zmm2", "62 d1 7d 48 6f d1");
}

#[test]
fn evex_writemask_and_zeroing() {
    enc("vaddps %ymm1, %ymm2, %ymm3 {%k2}", "62 f1 6c 2a 58 d9");
    enc("vaddps %zmm1, %zmm2, %zmm3 {%k1}{z}", "62 f1 6c c9 58 d9");
    enc("vaddps (%rax), %zmm2, %zmm3 {%k1}{z}", "62 f1 6c c9 58 18");
    enc("vmovapd (%rax), %zmm1 {%k3}", "62 f1 fd 4b 28 08");
    // A writemask on an otherwise VEX-encodable instruction selects EVEX.
    enc("vaddps %xmm1, %xmm2, %xmm3 {%k1}", "62 f1 6c 09 58 d9");
    enc("vpcmpeqd %zmm1, %zmm2, %k3 {%k4}", "62 f1 6d 4c 76 d9");
}

/// Rounding control reuses `L'L` for the mode and sets `b`; `{sae}` sets `b` and
/// leaves `L'L` at zero.
#[test]
fn evex_embedded_rounding_and_sae() {
    enc("vaddps {rn-sae}, %zmm1, %zmm2, %zmm3", "62 f1 6c 18 58 d9");
    enc("vaddps {rd-sae}, %zmm1, %zmm2, %zmm3", "62 f1 6c 38 58 d9");
    enc("vaddps {ru-sae}, %zmm1, %zmm2, %zmm3", "62 f1 6c 58 58 d9");
    enc("vaddps {rz-sae}, %zmm1, %zmm2, %zmm3", "62 f1 6c 78 58 d9");
    enc("vaddss {rd-sae}, %xmm1, %xmm2, %xmm3", "62 f1 6e 38 58 d9");
    enc("vmaxps {sae}, %zmm1, %zmm2, %zmm3", "62 f1 6c 18 5f d9");
    enc(
        "vcmpps $0, {sae}, %zmm1, %zmm2, %k1",
        "62 f1 6c 18 c2 c9 00",
    );
    enc("vcomiss {sae}, %xmm1, %xmm2", "62 f1 7c 18 2f d1");
}

#[test]
fn evex_broadcast() {
    enc("vaddps (%rax){1to4}, %xmm2, %xmm3", "62 f1 6c 18 58 18");
    enc("vaddps (%rax){1to8}, %ymm2, %ymm3", "62 f1 6c 38 58 18");
    enc(
        "vaddps (%rax){1to16}, %zmm2, %zmm3 {%k1}",
        "62 f1 6c 59 58 18",
    );
    enc("vaddpd (%rax){1to8}, %zmm2, %zmm3", "62 f1 ed 58 58 18");
    enc("vpaddd (%rax){1to16}, %zmm2, %zmm3", "62 f1 6d 58 fe 18");
    enc("vpaddq (%rax){1to8}, %zmm2, %zmm3", "62 f1 ed 58 d4 18");
    enc(
        "vpternlogq $0x96, (%rax){1to8}, %zmm2, %zmm3",
        "62 f3 ed 58 25 18 96",
    );
    // Half-vector sources broadcast half as many dwords.
    enc("vcvtdq2pd (%rax){1to8}, %zmm2", "62 f1 7e 58 e6 10");
}

/// Full Vector: N is the vector width in bytes — 64, 32 or 16.
#[test]
fn disp8_scales_by_the_whole_register_for_full_vector_tuples() {
    enc("vmovaps 64(%rax), %zmm0", "62 f1 7c 48 28 40 01");
    enc("vmovaps 32(%rax), %ymm18", "62 e1 7c 28 28 50 01");
    enc("vaddps 16(%rax), %xmm18, %xmm3", "62 f1 6c 00 58 58 01");
    enc("vaddps 128(%rax), %zmm2, %zmm3", "62 f1 6c 48 58 58 02");
    enc("vaddps -64(%rax), %zmm2, %zmm3", "62 f1 6c 48 58 58 ff");
    // The ends of the range: -128 * 64 and 127 * 64.
    enc("vaddps -8192(%rax), %zmm2, %zmm3", "62 f1 6c 48 58 58 80");
    enc("vaddps 8128(%rax), %zmm2, %zmm3", "62 f1 6c 48 58 58 7f");
    // Not a multiple of N, or out of range after dividing: disp32.
    enc(
        "vaddps 4(%rax), %zmm2, %zmm3",
        "62 f1 6c 48 58 98 04 00 00 00",
    );
    enc(
        "vaddps -8256(%rax), %zmm2, %zmm3",
        "62 f1 6c 48 58 98 c0 df ff ff",
    );
    enc(
        "vaddps 8192(%rax), %zmm2, %zmm3",
        "62 f1 6c 48 58 98 00 20 00 00",
    );
    // The same displacement under VEX is not scaled at all.
    enc("vmovaps 64(%rax), %ymm0", "c5 fc 28 40 40");
}

/// Broadcast turns a Full Vector access into a single element, sized by W.
#[test]
fn disp8_scales_by_one_element_under_broadcast() {
    enc(
        "vaddps 4(%rax){1to16}, %zmm2, %zmm3",
        "62 f1 6c 58 58 58 01",
    );
    enc("vaddpd 8(%rax){1to8}, %zmm2, %zmm3", "62 f1 ed 58 58 58 01");
    enc("vpaddq 8(%rax){1to8}, %zmm2, %zmm3", "62 f1 ed 58 d4 58 01");
}

/// Tuple1 Scalar: N is one element, whatever the vector length.
#[test]
fn disp8_scales_by_the_element_for_tuple1_scalar() {
    enc("vaddss 4(%rax), %xmm18, %xmm3", "62 f1 6e 00 58 58 01");
    enc("vaddsd 8(%rax), %xmm18, %xmm3", "62 f1 ef 00 58 58 01");
    // Four bytes is not a multiple of a double.
    enc(
        "vaddsd 4(%rax), %xmm18, %xmm3",
        "62 f1 ef 00 58 98 04 00 00 00",
    );
    enc("vpbroadcastd 4(%rax), %zmm2", "62 f2 7d 48 58 50 01");
    enc("vpbroadcastq 8(%rax), %zmm2", "62 f2 fd 48 59 50 01");
    enc("vpbroadcastw 2(%rax), %zmm2", "62 f2 7d 48 79 50 01");
    enc("vmovq 8(%rax), %xmm17", "62 e1 fe 08 7e 48 01");
    enc(
        "vgatherdps 8(%rax,%zmm1,4), %zmm2 {%k1}",
        "62 f2 7d 49 92 54 88 02",
    );
}

/// Half, quarter and eighth memory; Tuple4; and `vmovddup`'s special case.
#[test]
fn disp8_scales_by_a_fraction_for_the_other_tuples() {
    // Half Vector (64 / 2)
    enc("vcvtps2pd 32(%rax), %zmm2", "62 f1 7c 48 5a 50 01");
    // Half, Quarter, Eighth Mem
    enc("vpmovqd %zmm1, 32(%rax)", "62 f2 7e 48 35 48 01");
    enc("vpmovdb %zmm1, 16(%rax)", "62 f2 7e 48 31 48 01");
    enc("vpmovqb %zmm1, 8(%rax)", "62 f2 7e 48 32 48 01");
    enc("vpmovzxbd 16(%rax), %zmm2", "62 f2 7d 48 31 50 01");
    enc("vpmovzxbq 8(%rax), %zmm2", "62 f2 7d 48 32 50 01");
    // Tuple4, sized by W
    enc(
        "vextractf32x4 $1, %zmm1, 16(%rax)",
        "62 f3 7d 48 19 48 01 01",
    );
    enc(
        "vinserti64x4 $1, 32(%rax), %zmm1, %zmm2",
        "62 f3 f5 48 3a 50 01 01",
    );
    enc("vbroadcasti64x4 32(%rax), %zmm1", "62 f2 fd 48 5b 48 01");
    // Full Vector Mem never broadcasts
    enc("vmovdqu64 %zmm2, 64(%rax)", "62 f1 fe 48 7f 50 01");
    enc("vpaddw 64(%rax), %zmm2, %zmm3", "62 f1 6d 48 fd 58 01");
    // movddup reads one double at 128 bits, the whole register above
    enc("vmovddup 8(%rax), %xmm17", "62 e1 ff 08 12 48 01");
    enc("vmovddup 32(%rax), %ymm1", "c5 ff 12 48 20");
    enc("vmovddup 64(%rax), %zmm1", "62 f1 ff 48 12 48 01");
}

#[test]
fn disp8_is_never_used_for_rip_relative_addressing() {
    enc(
        "vaddps 64(%rip), %zmm1, %zmm2",
        "62 f1 74 48 58 15 40 00 00 00",
    );
}

#[test]
fn avx512f_instructions() {
    enc("vmovdqa32 %zmm1, %zmm2", "62 f1 7d 48 6f d1");
    enc("vmovdqu64 (%rax), %zmm2", "62 f1 fe 48 6f 10");
    enc("vpaddd %zmm1, %zmm2, %zmm3", "62 f1 6d 48 fe d9");
    enc("vpaddq %zmm1, %zmm2, %zmm3 {%k1}", "62 f1 ed 49 d4 d9");
    enc(
        "vpternlogd $0xff, %zmm1, %zmm2, %zmm3",
        "62 f3 6d 48 25 d9 ff",
    );
    enc("vpcmpd $1, %zmm1, %zmm2, %k3", "62 f3 6d 48 1f d9 01");
    enc("vpcmpeqd %zmm1, %zmm2, %k3", "62 f1 6d 48 76 d9");
    enc("vpcmpeqq %zmm1, %zmm2, %k3", "62 f2 ed 48 29 d9");
    enc("vpbroadcastd %xmm1, %zmm2", "62 f2 7d 48 58 d1");
    enc("vpbroadcastd %eax, %zmm2", "62 f2 7d 48 7c d0");
    enc("vpbroadcastq %rax, %zmm2", "62 f2 fd 48 7c d0");
    enc("vpermd %zmm1, %zmm2, %zmm3", "62 f2 6d 48 36 d9");
    enc("vpermq $1, %zmm1, %zmm2", "62 f3 fd 48 00 d1 01");
    enc("vpermi2d %zmm1, %zmm2, %zmm3", "62 f2 6d 48 76 d9");
    enc("vpermt2ps %zmm1, %zmm2, %zmm3", "62 f2 6d 48 7f d9");
    enc("vpslld $3, %zmm1, %zmm2", "62 f1 6d 48 72 f1 03");
    enc("vpsraq $3, %zmm1, %zmm2", "62 f1 ed 48 72 e1 03");
    enc("vprold $3, %zmm1, %zmm2", "62 f1 6d 48 72 c9 03");
    enc("vextracti64x4 $1, %zmm1, %ymm2", "62 f3 fd 48 3b ca 01");
    enc("vcvtsi2sdq %rax, %xmm1, %xmm17", "62 e1 f7 08 2a c8");
}

/// `V'` carries the fifth bit of a vector index.
#[test]
fn avx512_gather_and_scatter() {
    enc(
        "vgatherdps (%rax,%zmm1,4), %zmm2 {%k1}",
        "62 f2 7d 49 92 14 88",
    );
    enc(
        "vgatherdps (%rax,%zmm17,4), %zmm2 {%k1}",
        "62 f2 7d 41 92 14 88",
    );
    enc(
        "vgatherqps (%rax,%zmm1,4), %ymm2 {%k1}",
        "62 f2 7d 49 93 14 88",
    );
    enc(
        "vgatherdpd (%rax,%ymm1,8), %zmm2 {%k1}",
        "62 f2 fd 49 92 14 c8",
    );
    enc(
        "vgatherdps (%rax,%xmm17,4), %xmm2 {%k1}",
        "62 f2 7d 01 92 14 88",
    );
    enc(
        "vscatterdps %zmm2, (%rax,%zmm1,4) {%k1}",
        "62 f2 7d 49 a2 14 88",
    );
    enc(
        "vpscatterqd %ymm2, (%rax,%zmm1,4) {%k1}",
        "62 f2 7d 49 a1 14 88",
    );
}

/// The width suffix picks `pp` and `W` together; GPR transfers of dword and
/// qword masks use `F2`.
#[test]
fn opmask_instructions() {
    enc("kmovw %k1, %k2", "c5 f8 90 d1");
    enc("kmovw %eax, %k1", "c5 f8 92 c8");
    enc("kmovw %k1, %eax", "c5 f8 93 c1");
    enc("kmovw (%rax), %k1", "c5 f8 90 08");
    enc("kmovb %k1, %k2", "c5 f9 90 d1");
    enc("kmovd %eax, %k1", "c5 fb 92 c8");
    enc("kmovq %k1, %k2", "c4 e1 f8 90 d1");
    enc("kmovq %rax, %k2", "c4 e1 fb 92 d0");
    enc("kandw %k1, %k2, %k3", "c5 ec 41 d9");
    enc("korw %k1, %k2, %k3", "c5 ec 45 d9");
    enc("kxorq %k1, %k2, %k3", "c4 e1 ec 47 d9");
    enc("knotw %k1, %k2", "c5 f8 44 d1");
    enc("kortestw %k1, %k2", "c5 f8 98 d1");
    enc("kshiftlw $3, %k1, %k2", "c4 e3 f9 32 d1 03");
    enc("kunpckbw %k1, %k2, %k3", "c5 ed 4b d9");
}

/// Intel syntax writes decorators on the destination and rounding control last.
#[test]
fn intel_syntax_simd() {
    intel("paddb mm2, mm1", "0f fc d1");
    intel("pfadd mm2, qword ptr [rax]", "0f 0f 10 9e");
    intel("movq xmm1, xmm0", "f3 0f 7e c8");
    intel("movq mm0, rax", "48 0f 6e c0");
    intel("addss xmm2, dword ptr [rax+8]", "f3 0f 58 50 08");
    intel("cvtsi2sd xmm1, rax", "f2 48 0f 2a c8");
    intel("crc32 eax, byte ptr [rax]", "f2 0f 38 f0 00");
    intel("vaddps ymm3, ymm2, ymmword ptr [rax+8]", "c5 ec 58 58 08");
    intel(
        "vgatherqps xmm3, dword ptr [rax+ymm1*4], xmm2",
        "c4 e2 6d 93 1c 88",
    );
    intel("vpblendvb xmm3, xmm2, xmm1, xmm4", "c4 e3 69 4c d9 40");
    intel("vaddps zmm3 {k1} {z}, zmm2, zmm1", "62 f1 6c c9 58 d9");
    intel("vaddps zmm3, zmm2, zmm1, {rn-sae}", "62 f1 6c 18 58 d9");
    intel(
        "vaddps zmm3, zmm2, zmmword ptr [rax+64]",
        "62 f1 6c 48 58 58 01",
    );
    intel(
        "vaddps zmm3, zmm2, dword ptr [rax]{1to16}",
        "62 f1 6c 58 58 18",
    );
    intel(
        "vaddpd zmm3, zmm2, qword ptr [rax+8]{1to8}",
        "62 f1 ed 58 58 58 01",
    );
    intel("vcmpps k1 {k2}, zmm2, zmm1, 0", "62 f1 6c 4a c2 c9 00");
    intel("vcmpps k1, zmm2, zmm1, {sae}, 0", "62 f1 6c 18 c2 c9 00");
    intel(
        "vgatherdps zmm2 {k1}, dword ptr [rax+zmm17*4+8]",
        "62 f2 7d 41 92 54 88 02",
    );
    intel(
        "vscatterdps [rax+zmm1*4] {k1}, zmm2",
        "62 f2 7d 49 a2 14 88",
    );
    intel("kmovw k1, word ptr [rax]", "c5 f8 90 08");
    intel(
        "vextracti64x4 ymmword ptr [rax+32], zmm1, 1",
        "62 f3 fd 48 3b 48 01 01",
    );
}

/// VEX and EVEX work outside 64-bit mode as long as no extension bit is needed.
#[test]
fn thirty_two_bit_mode_simd() {
    enc32("paddb %mm1, %mm2", "0f fc d1");
    enc32("movq %xmm0, %xmm1", "f3 0f 7e c8");
    enc32("addps (%eax), %xmm1", "0f 58 08");
    enc32("pfadd %mm1, %mm2", "0f 0f d1 9e");
    enc32("vaddps 8(%eax), %ymm2, %ymm3", "c5 ec 58 58 08");
    enc32("vpermq $1, %ymm1, %ymm2", "c4 e3 fd 00 d1 01");
    enc32(
        "vgatherdps %xmm2, (%eax,%xmm1,4), %xmm3",
        "c4 e2 69 92 1c 88",
    );
    enc32("vaddps %zmm1, %zmm2, %zmm3 {%k1}", "62 f1 6c 49 58 d9");
    enc32("vmovaps 64(%eax), %zmm0", "62 f1 7c 48 28 40 01");
    enc32("kmovw %k1, %k2", "c5 f8 90 d1");
}

#[test]
fn nonsense_decorators_are_diagnosed() {
    rejects(
        "vaddps %zmm1, %zmm2, %zmm3 {%k9}",
        "`k9` is not a mask register",
    );
    rejects(
        "vaddps %zmm1, %zmm2, %zmm3 {%k0}",
        "`k0` cannot be used as a writemask",
    );
    rejects(
        "vaddps %zmm1, %zmm2, %zmm3 {%rax}",
        "`rax` is not a mask register",
    );
    rejects(
        "vaddps %zmm1, %zmm2, %zmm3 {foo}",
        "`foo` is not a mask register",
    );
    rejects(
        "vaddps %zmm1, %zmm2, %zmm3 {}",
        "expected `z`, a mask register or `1toN`",
    );
    rejects("vaddps %zmm1, %zmm2, %zmm3 {%k1", "expected `}`");
    rejects(
        "vaddps %zmm1, %zmm2, %zmm3 {%k1}{%k2}",
        "only one writemask",
    );
    rejects("vaddps %zmm1, %zmm2, %zmm3 {%k1}{z}{z}", "duplicate `{z}`");
    rejects(
        "vaddps %zmm1, %zmm2, %zmm3 {z}",
        "`{z}` requires a writemask",
    );
}

#[test]
fn broadcast_counts_are_checked_against_the_instruction() {
    // `{1to7}` has the same length as the `{1to8}` this instruction wants, so
    // this needs the written count itself, not just its spelling's length.
    rejects(
        "vaddpd (%rax){1to7}, %zmm2, %zmm3",
        "broadcasts as `{1to8}`",
    );
    rejects(
        "vaddps (%rax){1to7}, %zmm2, %zmm3",
        "broadcasts as `{1to16}`",
    );
    rejects(
        "vaddps (%rax){1to4}, %ymm2, %ymm3",
        "broadcasts as `{1to8}`",
    );
    rejects(
        "vpaddq (%rax){1to16}, %zmm2, %zmm3",
        "broadcasts as `{1to8}`",
    );
    rejects("vaddps (%rax){2to8}, %ymm2, %ymm3", "expected `{1toN}`");
    rejects("vpaddd (%rax){16}, %zmm2, %zmm3", "expected `1toN`");
    rejects(
        "vaddps (%rax){1to16}{1to16}, %zmm2, %zmm3",
        "duplicate broadcast",
    );
    rejects(
        "vaddps %zmm1{1to16}, %zmm2, %zmm3",
        "needs a memory operand",
    );
    // The lexer's own complaint about `1to16` is withdrawn, not doubled.
    let e = errors("vaddps (%rax){1to7}, %zmm2, %zmm3");
    assert!(!e.contains("invalid digit"), "{e}");
}

#[test]
fn decorators_on_instructions_that_take_none_are_diagnosed() {
    rejects(
        "addps %xmm1, %xmm2 {%k1}",
        "only available on AVX-512 forms",
    );
    rejects(
        "addps (%rax){1to4}, %xmm2",
        "only available on AVX-512 forms",
    );
    rejects(
        "addps {rn-sae}, %xmm1, %xmm2",
        "only available on AVX-512 forms",
    );
    rejects("vmovntps %zmm1, (%rax) {%k1}", "takes no writemask");
    rejects("vgatherdps (%rax,%zmm1,4), %zmm2", "requires a writemask");
}

#[test]
fn rounding_control_is_checked_against_the_instruction() {
    rejects(
        "vaddps {rn-sae}, %ymm1, %ymm2, %ymm3",
        "takes no embedded rounding",
    );
    rejects(
        "vmaxps {rn-sae}, %zmm1, %zmm2, %zmm3",
        "takes no embedded rounding",
    );
    rejects("vaddps {sae}, %zmm1, %zmm2, %zmm3", "not `{sae}`");
    rejects(
        "vaddps {rn-sae}, (%rax), %zmm2, %zmm3",
        "cannot be combined with a memory",
    );
    rejects(
        "vaddps {rn-sae}, {rz-sae}, %zmm1, %zmm2, %zmm3",
        "only one rounding",
    );
    rejects(
        "vaddps {bogus-sae}, %zmm1, %zmm2, %zmm3",
        "expected a rounding-control",
    );
}

#[test]
fn register_classes_are_checked() {
    rejects(
        "addps %xmm16, %xmm1",
        "only reachable through an EVEX-encoded",
    );
    rejects(
        "movq (%rax,%xmm1,4), %rax",
        "can only index memory in a gather",
    );
    rejects(
        "vgatherdps (%rax,%rbx,4), %zmm2 {%k1}",
        "needs a vector index",
    );
    rejects("vaddps %zmm1, %xmm2, %xmm3", "no form of `vaddps`");
    rejects("vpaddd %zmm1, %zmm2, %k3", "no form of `vpaddd`");
    rejects("psllw $1, (%rax)", "no form of `psllw`");
}

#[test]
fn extended_registers_do_not_exist_outside_64_bit_mode() {
    for src in [
        "vaddps %ymm1, %ymm2, %ymm9",
        "vaddps %zmm1, %zmm2, %zmm17",
        "vmovq %rax, %xmm0",
    ] {
        let e = errors_for("i386", src);
        assert!(e.contains("only available in 64-bit mode"), "{src}: {e}");
    }
}

#[test]
fn a_broadcast_count_must_match_the_operand_exactly() {
    // The lexer used to report `1to8` as a malformed number, and the x86
    // parser withdrew the error and read the count back out of the message
    // text; when that failed it fell back to a length check that could not
    // tell `{1to7}` from `{1to8}`. The count now arrives as a token.
    let e = errors("vaddpd (%rax){1to7}, %zmm2, %zmm3\n");
    assert!(e.contains("broadcasts as `{1to8}`"), "{e}");
    let e = errors("vaddps (%rax){1to8}, %zmm2, %zmm3\n");
    assert!(e.contains("broadcasts as `{1to16}`"), "{e}");
}

#[test]
fn a_broadcast_spelling_outside_a_decorator_is_still_a_bad_number() {
    let e = errors("movq $1to16, %rax\n");
    assert!(
        e.contains("invalid digit `t` for base-10 literal `1to16`"),
        "{e}"
    );
}

#[test]
fn fma_orders_and_the_named_predicates_fold_into_the_opcode() {
    // FMA's opcode is a base per operation plus the operand order; the
    // compare and carry-less multiply names supply the immediate, which sits
    // where an immediate would and so counts toward a RIP-relative bias.
    enc("vfmadd132ps %xmm1, %xmm2, %xmm3", "c4 e2 69 98 d9");
    enc("vfnmsub231sd 8(%rax), %xmm2, %xmm3", "c4 e2 e9 bf 58 08");
    enc(
        "vfmsubadd213pd %zmm25, %zmm2, %zmm3{%k1}",
        "62 92 ed 49 a7 d9",
    );
    enc(
        "vfmadd231ps (%rax){1to16}, %zmm2, %zmm3",
        "62 f2 6d 58 b8 18",
    );
    enc(
        "vfmadd132sd {rz-sae}, %xmm1, %xmm2, %xmm3",
        "62 f2 ed 78 99 d9",
    );
    enc("cmpneqsd %xmm1, %xmm2", "f2 0f c2 d1 04");
    enc(
        "cmpltps x(%rip), %xmm1\nx: ret",
        "0f c2 0d 00 00 00 00 01 c3",
    );
    enc("vcmpnge_uqpd %ymm1, %ymm2, %ymm3", "c5 ed c2 d9 19");
    enc(
        "vcmptrue_usps 0x40(%rax), %zmm2, %k3{%k4}",
        "62 f1 6c 4c c2 58 01 1f",
    );
    enc("vpcmpnequb %zmm1, %zmm2, %k1", "62 f3 6d 48 3e c9 04");
    enc("vpcmpltq (%rax){1to8}, %zmm2, %k5", "62 f3 ed 58 1f 28 01");
    enc("vpclmulhqlqdq %xmm1, %xmm2, %xmm3", "c4 e3 69 44 d9 01");
    enc("pclmullqhqdq (%rax), %xmm2", "66 0f 3a 44 10 10");
}

#[test]
fn avx512bw_dq_tuples_scale_the_displacement_by_the_access() {
    // An `xmm` shift count is sixteen bytes at every length (Mem128); the
    // `64x2` and `32x8` sub-vectors are two and eight elements (Tuple2/8).
    enc("vpsllw %xmm1, %zmm2, %zmm3", "62 f1 6d 48 f1 d9");
    enc("vpsraw 0x10(%rax), %zmm2, %zmm3", "62 f1 6d 48 e1 58 01");
    enc("vpsrlq 0x10(%rax), %zmm2, %zmm3", "62 f1 ed 48 d3 58 01");
    enc(
        "vinserti64x2 $1, 0x10(%rax), %zmm2, %zmm3",
        "62 f3 ed 48 38 58 01 01",
    );
    enc(
        "vextractf32x8 $1, %zmm2, 0x20(%rax)",
        "62 f3 7d 48 1b 50 01 01",
    );
    enc("vbroadcasti64x2 0x10(%rax), %zmm1", "62 f2 fd 48 5a 48 01");
    enc("vbroadcastf32x2 0x8(%rax), %ymm1", "62 f2 7d 28 19 48 01");
    enc("vpmovm2b %k3, %zmm17", "62 e2 7e 48 28 cb");
    enc("vpmovq2m %ymm20, %k2", "62 b2 fe 28 39 d4");
    enc("vcvtsd2usi 0x8(%rax), %eax", "62 f1 7f 08 79 40 01");
}

#[test]
fn a_length_spelling_or_a_broadcast_names_an_ambiguous_memory_width() {
    // A narrowing conversion's memory source could be either length; AT&T
    // puts the length on the mnemonic, and a `{1toN}` implies it.
    enc("vfpclasspsz $3, 0x40(%rax), %k1", "62 f3 7d 48 66 48 01 03");
    enc("vfpclasspdx $3, 0x10(%rax), %k1", "62 f3 fd 08 66 48 01 03");
    enc("vcvtpd2psy 0x20(%rax), %xmm1", "c5 fd 5a 48 20");
    enc("vcvtpd2ps (%rax){1to4}, %xmm1", "62 f1 fd 38 5a 08");
    enc("vcvtqq2ps (%rax){1to8}, %ymm1", "62 f1 fc 58 5b 08");
    enc("vcvtqq2phz 0x40(%rax), %xmm1", "62 f5 fc 48 5b 48 01");
}

#[test]
fn avx512_late_subsets() {
    enc("vpshldvw 0x40(%rax), %zmm1, %zmm2", "62 f2 f5 48 70 50 01");
    enc(
        "vpshrdd $3, (%rax){1to16}, %zmm1, %zmm2",
        "62 f3 75 58 73 10 03",
    );
    enc("vpcompressw %zmm1, 0x2(%rax)", "62 f2 fd 48 63 48 01");
    enc("vpexpandb 0x1(%rax), %zmm1", "62 f2 7d 48 62 48 01");
    enc("vpopcntq (%rax){1to8}, %zmm1", "62 f2 fd 58 55 08");
    enc("vp2intersectd %zmm1, %zmm2, %k0", "62 f2 6f 48 68 c1");
    enc(
        "vcvtne2ps2bf16 (%rax){1to16}, %zmm1, %zmm2",
        "62 f2 77 58 72 10",
    );
    enc("vdpbf16ps 0x40(%rax), %zmm1, %zmm2", "62 f2 76 48 52 50 01");
    enc(
        "vgf2p8affineqb $1, (%rax){1to8}, %zmm1, %zmm2",
        "62 f3 f5 58 ce 10 01",
    );
    enc("vaesenc 0x40(%rax), %zmm1, %zmm2", "62 f2 75 48 dc 50 01");
    enc("vpclmulqdq $0x11, %ymm1, %ymm2, %ymm3", "c4 e3 6d 44 d9 11");
}

#[test]
fn fp16_lives_in_maps_5_and_6_with_word_elements() {
    // Half-precision broadcasts repeat a word, and the widening conversions
    // read half or a quarter of the register in words.
    enc(
        "vaddph 0x4(%rax){1to32}, %zmm1, %zmm2",
        "62 f5 74 58 58 50 02",
    );
    enc("vaddsh 0x2(%rax), %xmm1, %xmm2", "62 f5 76 08 58 50 01");
    enc("vfmadd132ph (%rax), %ymm1, %ymm2", "62 f6 75 28 98 10");
    enc(
        "vcvtph2qq 0x4(%rax), %zmm1",
        "62 f5 7d 48 7b 88 04 00 00 00",
    );
    enc("vcvtph2pd (%rax){1to8}, %zmm1", "62 f5 7c 58 5a 08");
    enc(
        "vcvtph2dq 0x10(%rax), %zmm1",
        "62 f5 7d 48 5b 88 10 00 00 00",
    );
    enc("vcvtsh2usi 0x2(%rax), %rax", "62 f5 fe 08 79 40 01");
    enc("vmovw %xmm1, %eax", "62 f5 7d 08 7e c8");
    enc("vmovsh %xmm1, 0x2(%rax)", "62 f5 7e 08 11 48 01");
    enc("vfmulcph %zmm1, %zmm2, %zmm3", "62 f6 6e 48 d6 d9");
    enc("vcmpeqph %zmm1, %zmm2, %k1", "62 f3 6c 48 c2 c9 00");
}

#[test]
fn pseudo_prefixes_choose_the_encoding() {
    // AVX-VNNI's mnemonics are AVX-512's, which both references prefer.
    enc("vpdpbusd %xmm1, %xmm2, %xmm3", "62 f2 6d 08 50 d9");
    enc("{vex} vpdpbusd %xmm1, %xmm2, %xmm3", "c4 e2 69 50 d9");
    enc("{vex3} vaddps %xmm1, %xmm2, %xmm3", "c4 e1 68 58 d9");
    enc("{evex} vaddps %xmm1, %xmm2, %xmm3", "62 f1 6c 08 58 d9");
}

#[test]
fn bmi_tbm_and_lwp_put_a_general_register_in_vvvv() {
    enc("andn %rax, %rbx, %rcx", "c4 e2 e0 f2 c8");
    enc("andnl (%rax), %ebx, %ecx", "c4 e2 60 f2 08");
    enc("shlx %rcx, (%rax), %rdx", "c4 e2 f1 f7 10");
    enc("blsr %r9, %r10", "c4 c2 a8 f3 c9");
    enc("pdep %r8, %r9, %r10", "c4 42 b3 f5 d0");
    enc("rorx $3, %eax, %ebx", "c4 e3 7b f0 d8 03");
    enc("bextr $0x1234, %rax, %rbx", "8f ea f8 10 d8 34 12 00 00");
    enc("blcfill %r9, %r10", "8f c9 a8 01 c9");
    enc("llwpcb %eax", "8f e9 78 12 c0");
    enc("lwpins $1, (%rax), %rbx", "8f ea e0 12 00 01 00 00 00");
}

#[test]
fn xop_and_fma4_let_w_choose_the_memory_operand() {
    // Between registers XOP takes W0 and FMA4 W1, as both references do.
    enc("vpcmov %ymm1, %ymm2, %ymm3, %ymm4", "8f e8 64 a2 e2 10");
    enc("vpcmov (%rax), %ymm2, %ymm3, %ymm4", "8f e8 e4 a2 20 20");
    enc("vpcmov %ymm2, (%rax), %ymm3, %ymm4", "8f e8 64 a2 20 20");
    enc("vprotb $3, %xmm1, %xmm2", "8f e8 78 c0 d1 03");
    enc("vprotd %xmm1, (%rax), %xmm2", "8f e9 70 92 10");
    enc("vprotd (%rax), %xmm1, %xmm2", "8f e9 f0 92 10");
    enc("vpcomltb %xmm1, %xmm2, %xmm3", "8f e8 68 cc d9 00");
    enc("vfrczps %ymm1, %ymm2", "8f e9 7c 80 d1");
    enc("vfmaddps %xmm1, %xmm2, %xmm3, %xmm4", "c4 e3 e1 68 e1 20");
    enc("vfmaddps (%rax), %xmm2, %xmm3, %xmm4", "c4 e3 e1 68 20 20");
    enc("vfmaddps %xmm2, (%rax), %xmm3, %xmm4", "c4 e3 61 68 20 20");
    enc(
        "vpermil2ps $2, %ymm1, %ymm2, %ymm3, %ymm4",
        "c4 e3 65 48 e2 12",
    );
}

#[test]
fn vex_crypto_and_conversion_extensions() {
    enc("vcvtph2ps (%rax), %ymm1", "c4 e2 7d 13 08");
    enc("vcvtps2ph $4, %zmm1, (%rax)", "62 f3 7d 48 1d 08 04");
    enc("sha256rnds2 %xmm0, %xmm1, %xmm2", "0f 38 cb d1");
    enc("sha1rnds4 $3, %xmm1, %xmm2", "0f 3a cc d1 03");
    enc("vsha512rnds2 %xmm1, %ymm2, %ymm3", "c4 e2 6f cb d9");
    enc("vsm3rnds2 $1, %xmm1, %xmm2, %xmm3", "c4 e3 69 de d9 01");
    enc("vsm4key4 %ymm1, %ymm2, %ymm3", "c4 e2 6e da d9");
    enc("vpdpwsud %ymm1, %ymm2, %ymm3", "c4 e2 6e d2 d9");
    enc("vbcstnesh2ps (%rax), %ymm1", "c4 e2 7d b1 08");
}

#[test]
fn amx_always_writes_a_sib_byte() {
    enc("tileloadd (%rax), %tmm1", "c4 e2 7b 4b 0c 20");
    enc("tilestored %tmm2, 8(%rsp,%r12,4)", "c4 a2 7a 4b 54 a4 08");
    enc("tdpbssd %tmm1, %tmm2, %tmm3", "c4 e2 73 5e da");
    enc("tilezero %tmm5", "c4 e2 7b 49 e8");
    enc("tilerelease", "c4 e2 78 49 c0");
    enc("ldtilecfg (%rax)", "c4 e2 78 49 00");
}

#[test]
fn system_extensions() {
    // The address-sized register of `movdir64b` asks for `67` when it is
    // narrower than the mode's addresses. The implicit-register spellings of
    // `monitor` and friends are GNU as's (2.47); llvm-mc refuses them.
    enc("incsspq %rax", "f3 48 0f ae e8");
    enc("rdsspd %r9d", "f3 41 0f 1e c9");
    enc("wrussq %rax, (%rbx)", "66 48 0f 38 f5 03");
    enc("endbr64", "f3 0f 1e fa");
    enc("rstorssp (%rax)", "f3 0f 01 28");
    enc("movdir64b (%ecx), %eax", "67 66 0f 38 f8 01");
    enc("movdir64b (%rcx), %rax", "66 0f 38 f8 01");
    enc("enqcmds (%rcx), %r9", "f3 44 0f 38 f8 09");
    enc("monitor %rax, %ecx, %edx", "0f 01 c8");
    enc("mwait %eax, %ecx", "0f 01 c9");
    enc("umonitor %eax", "67 f3 0f ae f0");
    enc("tpause %eax, %edx, %eax", "66 0f ae f0");
    enc("hreset $1", "f3 0f 3a f0 c0 01");
    enc("aesenc128kl (%rax), %xmm1", "f3 0f 38 dc 08");
    enc("aesencwide256kl 8(%rax)", "f3 0f 38 d8 50 08");
    enc("encodekey128 %eax, %ebx", "f3 0f 38 fa d8");
    enc("xsave64 (%rax)", "48 0f ae 20");
    enc("cmpaxadd %eax, %ebx, (%rcx)", "c4 e2 79 e7 19");
    enc("aadd %r9, (%r10)", "4d 0f 38 fc 0a");
    enc("rdpid %rax", "f3 0f c7 f8");
    enc("senduipi %r10", "f3 41 0f c7 f2");
    enc("ptwriteq (%rax)", "f3 48 0f ae 20");
}

#[test]
fn byte_extracts_put_their_register_in_rm() {
    // `pextrb`, `vpextrb` and `extractps` once put the register in ModRM.reg,
    // which `%xmm0` with `%eax` could not show.
    enc("pextrb $1, %xmm2, %eax", "66 0f 3a 14 d0 01");
    enc("vpextrb $1, %xmm2, %rax", "c4 e3 79 14 d0 01");
    enc("extractps $1, %xmm2, %rax", "66 0f 3a 17 d0 01");
    enc("vmovntps %ymm1, (%rax)", "c5 fc 2b 08");
}

#[test]
fn intel_syntax_extensions() {
    // Under `{1toN}` the size keyword names the element; the rounding
    // operand follows the register operands and precedes an immediate.
    intel(
        "vfmadd132ps zmm3{k1}{z}, zmm2, zmmword ptr [rax]",
        "62 f2 6d c9 98 18",
    );
    intel(
        "vaddph zmm2, zmm1, word ptr [rax+0x4]{1to32}",
        "62 f5 74 58 58 50 02",
    );
    intel("vcvtph2qq zmm1, word ptr [rax]{1to8}", "62 f5 7d 58 7b 08");
    intel("vcvtpd2ps xmm1, ymmword ptr [rax]", "c5 fd 5a 08");
    intel("vcvtpd2ps xmm1, qword ptr [rax]{1to4}", "62 f1 fd 38 5a 08");
    intel(
        "vfpclassps k1, zmmword ptr [rax], 3",
        "62 f3 7d 48 66 08 03",
    );
    intel("vpcmpnequb k1, zmm2, zmm1", "62 f3 6d 48 3e c9 04");
    intel("vaddps zmm3, zmm2, zmm1, {rn-sae}", "62 f1 6c 18 58 d9");
    intel("vcmpps k1, zmm2, zmm1, {sae}, 1", "62 f1 6c 18 c2 c9 01");
    intel("vcvtsi2sd xmm2, xmm1, {rz-sae}, rax", "62 f1 f7 78 2a d0");
    intel("{vex} vpdpbusd xmm3, xmm2, xmm1", "c4 e2 69 50 d9");
    intel("tileloadd tmm1, [rax+rbx*2]", "c4 e2 7b 4b 0c 58");
    intel("tdpbssd tmm3, tmm2, tmm1", "c4 e2 73 5e da");
    intel("andn rcx, rbx, qword ptr [rax]", "c4 e2 e0 f2 08");
    intel("shlx rdx, qword ptr [rax], rcx", "c4 e2 f1 f7 10");
    intel(
        "vpcmov ymm4, ymm3, ymm2, ymmword ptr [rax]",
        "8f e8 e4 a2 20 20",
    );
    intel(
        "vfmaddps xmm4, xmm3, xmmword ptr [rax], xmm2",
        "c4 e3 61 68 20 20",
    );
    intel("movdir64b eax, [ecx]", "67 66 0f 38 f8 01");
    intel("cmpaxadd dword ptr [rcx], ebx, eax", "c4 e2 79 e7 19");
    intel(
        "vbroadcasti64x2 zmm1, xmmword ptr [rax+0x10]",
        "62 f2 fd 48 5a 48 01",
    );
    // GNU as 2.47; llvm-mc refuses the implicit operands.
    intel("monitor rax, ecx, edx", "0f 01 c8");
}

#[test]
fn extensions_in_32_bit_mode() {
    enc32(
        "vaddph 0x4(%eax){1to32}, %zmm1, %zmm2",
        "62 f5 74 58 58 50 02",
    );
    enc32("vfmadd231ps (%eax), %ymm1, %ymm2", "c4 e2 75 b8 10");
    enc32("andnl (%eax), %ebx, %ecx", "c4 e2 60 f2 08");
    enc32("vpcmov %xmm1, %xmm2, %xmm3, %xmm4", "8f e8 60 a2 e2 10");
    enc32("movdir64b (%ecx), %eax", "66 0f 38 f8 01");
    enc32("monitor %eax, %ecx, %edx", "0f 01 c8");
    enc32("umonitor %ax", "67 f3 0f ae f0");
    enc32("rdpid %eax", "f3 0f c7 f8");
    enc32("vcvtsd2usi 0x8(%eax), %eax", "62 f1 7f 08 79 40 01");
    enc32("encodekey128 %eax, %ebx", "f3 0f 38 fa d8");
}

#[test]
fn extension_operands_both_references_refuse() {
    for (src, needle) in [
        (
            "vcvtpd2ps (%rax), %xmm1",
            "as in `vcvtpd2psx` or `vcvtpd2psy`",
        ),
        (
            "vfpclassps $1, (%rax), %k1",
            "needs the size of its memory operand",
        ),
        ("vcvtqq2ph (%rax), %xmm1", "as in `vcvtqq2phx`"),
        (
            "vcmpeqps %zmm1, %zmm2, %k1{%k2}{z}",
            "mask register destination",
        ),
        ("vmovaps %zmm1, (%rax){%k1}{z}", "`{z}` cannot be used"),
        ("vaddps %zmm1, {rn-sae}, %zmm2, %zmm3", "misplaced"),
        ("vcmpps {sae}, $1, %zmm1, %zmm2, %k1", "misplaced"),
        ("vcvtsi2sd {rz-sae}, %rax, %xmm1, %xmm2", "misplaced"),
        (
            "tdpbssd %tmm1, %tmm1, %tmm2",
            "tile registers must be different",
        ),
        ("tileloadd foo(%rip), %tmm1", "relative to `rip`"),
        ("{vex} vaddps %zmm1, %zmm2, %zmm3", "no VEX encoding"),
        (
            "vpermil2ps $16, %ymm1, %ymm2, %ymm3, %ymm4",
            "must be 0 to 15",
        ),
        ("vmovntps %ymm1, (%rax){%k1}", "takes no writemask"),
    ] {
        rejects(src, needle);
    }
    // GNU as 2.47 refuses these, where llvm-mc assembles them.
    rejects(
        "vfmulcph %zmm1, %zmm2, %zmm1",
        "must differ from both sources",
    );
    rejects("vpdpbusd %zmm1, %zmm2, %zmm17{z}", "requires a writemask");
}

#[test]
fn avx10_2_rows_share_the_avx512_shapes() {
    // The VEX-only integer additions keep VEX for plain operands and take
    // EVEX for a writemask; `vmpsadbw`'s EVEX form moved to `F3`.
    enc("vpdpbuud %zmm1, %zmm2, %zmm3", "62 f2 6c 48 50 d9");
    enc("vpdpbuud %xmm1, %xmm2, %xmm3", "c4 e2 68 50 d9");
    enc("vpdpbuud %xmm1, %xmm2, %xmm3{%k1}", "62 f2 6c 09 50 d9");
    enc(
        "vmpsadbw $1, %xmm1, %xmm2, %xmm3{%k1}",
        "62 f3 6e 09 42 d9 01",
    );
    enc(
        "vminmaxps $1, (%rax){1to16}, %zmm1, %zmm2",
        "62 f3 75 58 52 10 01",
    );
    enc(
        "vminmaxsh $1, 0x2(%rax), %xmm1, %xmm2",
        "62 f3 74 08 53 50 01 01",
    );
    enc("vcomxsd 0x8(%rax), %xmm1", "62 f1 ff 08 2f 48 01");
    enc("vucomxsh {sae}, %xmm1, %xmm2", "62 f5 7e 18 2e d1");
    enc("vcvttps2dqs (%rax){1to16}, %zmm1", "62 f5 7c 58 6d 08");
    enc("vcvttps2qqs 0x20(%rax), %zmm1", "62 f5 7d 48 6d 48 01");
    enc("vcvttpd2dqsy 0x20(%rax), %xmm1", "62 f5 fc 28 6d 48 01");
    enc("vcvttsd2sis 0x8(%rax), %eax", "62 f5 7f 08 6d 40 01");
    enc("vcvtph2ibs {rn-sae}, %zmm1, %zmm2", "62 f5 7c 18 69 d1");
    enc("vaddbf16 0x40(%rax), %zmm1, %zmm2", "62 f5 75 48 58 50 01");
    enc(
        "vfmadd132bf16 (%rax){1to32}, %zmm1, %zmm2",
        "62 f6 74 58 98 10",
    );
    enc(
        "vfpclassbf16z $1, 0x40(%rax), %k1",
        "62 f3 7f 48 66 48 01 01",
    );
    enc("vcvtph2bf8 %zmm1, %ymm2", "62 f2 7e 48 74 d1");
    enc("vcvtph2hf8x 0x10(%rax), %xmm1", "62 f5 7e 08 18 48 01");
    enc("vcvt2ph2bf8 %zmm1, %zmm2, %zmm3", "62 f2 6f 48 74 d9");
    enc("vcvtbiasph2hf8 %zmm1, %zmm2, %ymm3", "62 f5 6c 48 18 d9");
    enc("vcvthf82ph 0x20(%rax), %zmm1", "62 f5 7f 48 1e 48 01");
    enc("vmovd %xmm1, %xmm2", "62 f1 7e 08 7e d1");
    enc("vmovw %xmm1, %xmm2", "62 f5 7e 08 6e d1");
    enc("vmovrsq 0x40(%rax), %zmm1", "62 f5 fe 48 6f 48 01");
    enc("movrs (%rax), %rbx", "48 0f 38 8b 18");
    enc("prefetchrst2 (%rax)", "0f 18 20");
    // After the immediate, not before it.
    rejects("vminmaxpd {sae}, $3, %zmm1, %zmm2, %zmm3", "misplaced");
}
