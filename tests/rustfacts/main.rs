//! rustc's facts the builder keeps (`pliron_ir_opt::facts`) and their
//! consumers: `exact` signed division by 24, 12 and 8 bytes (`offset_from`,
//! a slice iterator's `len()`), both signs of the distance. Stanza
//! `rustfacts` pins the output to stock rustc with `PLIRON_EXACT_DIV`,
//! `PLIRON_DISJOINT_ADD` and `PLIRON_COLD_SWITCH` off and on.
#[derive(Clone, Copy)] pub struct S24 { a: u64, b: u64, c: u64 }
#[derive(Clone, Copy)] pub struct S12 { a: u32, b: u32, c: u32 }
#[inline(never)] fn d24(a: *const S24, b: *const S24) -> isize { unsafe { b.offset_from(a) } }
#[inline(never)] fn d12(a: *const S12, b: *const S12) -> isize { unsafe { b.offset_from(a) } }
#[inline(never)] fn d8(a: *const u64, b: *const u64) -> isize { unsafe { b.offset_from(a) } }
#[inline(never)] fn len24(v: &[S24]) -> usize { v.iter().len() }
/// Narrowing round trips run under `PLIRON_TRUNC_FLAGS`. rustc emits
/// `unchecked_utrunc` only for unchecked shift amounts and bool switch
/// discriminants, so these lower to a compare and select and do not reach
/// the round-trip rule; they check the knob changes nothing else.
#[inline(never)] fn rt(a: u64) -> u64 { let h: u32 = unsafe { a.try_into().unwrap_unchecked() }; h as u64 }
#[inline(never)] fn rts(a: i64) -> i64 { let h: i16 = unsafe { a.try_into().unwrap_unchecked() }; h as i64 }
#[inline(never)] fn len12(v: &[S12]) -> usize { v.iter().skip(1).len() }
fn main() {
    let n: usize = std::env::args().count() * 37;
    let v24 = vec![S24 { a: 1, b: 2, c: 3 }; n];
    let v12 = vec![S12 { a: 1, b: 2, c: 3 }; n];
    let v8 = vec![7u64; n];
    let mut acc: i64 = 0;
    for i in 0..n { for j in [0, i / 2, n - 1] {
        acc = acc.wrapping_mul(31).wrapping_add(d24(&v24[i], &v24[j]) as i64);
        acc = acc.wrapping_mul(31).wrapping_add(d12(&v12[j], &v12[i]) as i64);
        acc = acc.wrapping_mul(31).wrapping_add(d8(&v8[i], &v8[j]) as i64);
    }}
    let mut t: u64 = 0;
    for i in 0..n as u64 { t = t.wrapping_mul(7).wrapping_add(rt(i * 1_000_003 % 4_000_000_000)).wrapping_add(rts(i as i64 * 97 % 30_000 - 15_000) as u64); }
    println!("{} {} {} {} {}", t, acc, len24(&v24[3..]), len12(&v12[5..]), v24[0].a + v24[0].b + v24[0].c + v12[0].a as u64 + v12[0].b as u64 + v12[0].c as u64);
}
