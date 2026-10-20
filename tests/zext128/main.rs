// Regression: `iadd.i128`/`isub.i128` rules fused with a `uextend`
// operand took the narrow operand's raw register. Narrow ops lower to
// 32-bit `lsl`/`eor`/`mul` on aarch64, leaving garbage in bits 16..32 of
// the containing X register, so `x - (t as u16 as i128)` saw e.g.
// 0x12340 instead of 0x2340 (rustlantis seed 24: -5439487 for -65535).
// The fused rules must zero-extend the operand (put_in_reg_zext64).

use std::hint::black_box;

// Producers that leave dirty bits above the value width in their W reg.
#[inline(never)]
fn shl16(a: i16, s: u32) -> i16 {
    a.wrapping_shl(s)
}
#[inline(never)]
fn notshl16(a: i16, s: u32) -> i16 {
    !a.wrapping_shl(s)
}
#[inline(never)]
fn mul16(a: i16, b: i16) -> i16 {
    a.wrapping_mul(b)
}
#[inline(never)]
fn shl8(a: i8, s: u32) -> i8 {
    a.wrapping_shl(s)
}

// Consumers: all four fused-rule shapes, plus an i8 operand.
#[inline(never)]
fn x_sub(x: i128, t: i16) -> i128 {
    x - (t as u16 as i128)
}
#[inline(never)]
fn sub_x(x: i128, t: i16) -> i128 {
    (t as u16 as i128) - x
}
#[inline(never)]
fn x_add(x: i128, t: i16) -> i128 {
    x + (t as u16 as i128)
}
#[inline(never)]
fn add_x(x: i128, t: i16) -> i128 {
    (t as u16 as i128) + x
}
#[inline(never)]
fn x_sub8(x: i128, t: i8) -> i128 {
    x - (t as u8 as i128)
}

fn main() {
    let x = black_box(0x1_0000i128);

    // 0x1234 << 4 -> reg 0x12340, i16 value 0x2340 = 9024.
    let t = shl16(0x1234, black_box(4));
    assert_eq!(x_sub(x, t), x - 9024);
    assert_eq!(sub_x(x, t), 9024 - x);
    assert_eq!(x_add(x, t), x + 9024);
    assert_eq!(add_x(x, t), x + 9024);

    // !(0x2AA << 7) -> reg 0x1AAFF keeps shl's dirty bit 16; i16 = 0xAAFF.
    let t = notshl16(0x2AA, black_box(7));
    let want = !(0x2AAu16.wrapping_shl(7)) as i16 as u16 as i128;
    assert_eq!(x_sub(x, t), x - want);
    assert_eq!(sub_x(x, t), want - x);
    assert_eq!(x_add(x, t), x + want);

    // 0x1234 * 0x45 = 0x4E574 -> reg, i16 value 0xE574.
    let t = mul16(0x1234, black_box(0x45));
    let want = 0x1234u16.wrapping_mul(0x45) as i128;
    assert_eq!(x_sub(x, t), x - want);
    assert_eq!(x_add(x, t), x + want);

    // i8: 0x35 << 4 -> reg 0x350, i8 value 0x50 = 80.
    let t = shl8(0x35, black_box(4));
    assert_eq!(x_sub8(x, t), x - 80);

    // Loop-carried variant: the seed-24 shape was a promoted block param.
    let mut acc = 0i128;
    for i in 0..64u32 {
        let t = shl16(-21555, i % 8); // 0xABCD
        acc += x_sub(x, t) + x_add(x, t);
    }
    let mut want_acc = 0i128;
    for i in 0..64u32 {
        let want = 0xABCDu16.wrapping_shl(i % 8) as i128;
        want_acc += (x - want) + (x + want);
    }
    assert_eq!(acc, want_acc);

    println!("zext128 ok");
}
