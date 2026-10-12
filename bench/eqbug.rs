use std::hint::black_box;

// i8/i16 eq/ne compares on add/sub expressions — the icmp_eqfold shift
// patterns that miscompiled strwork when applied at narrow widths.
fn f8(x: i8, k: i8) -> u64 {
    let mut a = 0u64;
    if x.wrapping_add(3) == k {
        a += 1;
    }
    if x.wrapping_sub(5) != k {
        a += 2;
    }
    if x.wrapping_add(-7) != 10 {
        a += 4;
    }
    a
}

fn f16(x: i16, k: i16) -> u64 {
    let mut a = 0u64;
    if x.wrapping_add(100) == k {
        a += 1;
    }
    if x.wrapping_sub(-200) != k {
        a += 2;
    }
    if x.wrapping_add(300) == -50 {
        a += 4;
    }
    a
}

fn fu8(x: u8, k: u8) -> u64 {
    let mut a = 0u64;
    if x.wrapping_add(200) == k {
        a += 1;
    }
    if x.wrapping_sub(100) != k {
        a += 2;
    }
    if x.wrapping_add(250) == 7 {
        a += 4;
    }
    a
}

fn main() {
    let mut acc = 0u64;
    for i in 0..1000i64 {
        let x = black_box(i as i8);
        let k = black_box((i * 3) as i8);
        acc = acc.wrapping_add(f8(x, k));
        let x = black_box(i as i16);
        let k = black_box((i * 7) as i16);
        acc = acc.wrapping_add(f16(x, k));
        let x = black_box(i as u8);
        let k = black_box((i * 5) as u8);
        acc = acc.wrapping_add(fu8(x, k));
    }
    println!("{}", acc);
}
