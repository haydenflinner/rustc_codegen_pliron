// Compiled with PLIRON_OPT_BISECT=0 (tests/wasm/smoke.sh): code as lowered,
// no optimizer. Locals live in stack slots here, so every read is a load
// and every assignment a store: the shapes the wasm stages (waffle's
// trees, the byte peepholes) see only in unoptimized builds.
#![no_std]

/// itoa's `divmod100(remain as u32)` with the result stored back into
/// `remain` between the two uses of the narrowed value. waffle once
/// re-read `remain` for the second use, after the store (99 printed "00").
#[unsafe(no_mangle)]
pub extern "C" fn raw_divmod(n: u64) -> u32 {
    let mut remain = n;
    let v = remain as u32;
    let div = (v.wrapping_mul(5243)) >> 19;
    remain = div as u64;
    let rem = v.wrapping_sub(div.wrapping_mul(100));
    (remain as u32) << 8 | rem
}

/// Decimal digits of `n`, least significant pair first, packed four bits
/// a digit: the same loop shape as itoa's formatter without the table.
#[unsafe(no_mangle)]
pub extern "C" fn raw_digits(n: u64) -> u64 {
    let mut remain = n;
    let mut out = 0u64;
    let mut shift = 0u32;
    while remain > 99 {
        let quad = remain % 100;
        remain /= 100;
        let v = quad as u32;
        let hi = (v.wrapping_mul(205)) >> 11;
        let lo = v.wrapping_sub(hi.wrapping_mul(10));
        out |= ((hi << 4 | lo) as u64) << shift;
        shift += 8;
    }
    let v = remain as u32;
    let hi = (v.wrapping_mul(205)) >> 11;
    remain = hi as u64;
    let lo = v.wrapping_sub(hi.wrapping_mul(10));
    out |= ((remain as u32) << 4 | lo) as u64 >> 0 << shift;
    out
}
