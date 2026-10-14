// Cranelift x86 libcall stubs: ISLE lowers SSSE3-less `swizzle.i8x16`
// to `call __cranelift_x86_pshufb` (SysV: both args in xmm regs, gcc
// vector extension matches that ABI). cg_clif emits these per-object;
// we provide one archive linked by cc-wild instead.
typedef unsigned char v16u8 __attribute__((vector_size(16)));

v16u8 __cranelift_x86_pshufb(v16u8 a, v16u8 mask) {
    v16u8 r;
    for (int i = 0; i < 16; i++) {
        r[i] = (mask[i] & 0x80) ? 0 : a[mask[i] & 0x0f];
    }
    return r;
}
