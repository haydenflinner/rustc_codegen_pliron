// icmp_eqfold: `x >=u 1`/`x <u 1` are equality tests, and
// `eq/ne (x + c), k` shifts the constant across the wrapping add —
// together peeling's countdown `end - k >=u 1` becomes `end != k`.
// The parse-style countdown below exercises the folded shape end to end.

#[inline(never)]
fn countdown(n: usize, ptr: &[u8]) -> usize {
    // `rem - k >=u 1` per peeled step; result counts bytes until a '0'.
    let mut i = 0usize;
    while n - i >= 1 {
        if ptr[i] == b'0' {
            break;
        }
        i += 1;
    }
    i
}

#[inline(never)]
fn adds(x: u64) -> u64 {
    let mut a = 0u64;
    if x.wrapping_add(7) == 100 {
        a += 1;
    }
    if x.wrapping_sub(9) != 50 {
        a += 2;
    }
    if x >= 1 {
        a += 4;
    }
    if x < 1 {
        a += 8;
    }
    a
}

fn main() {
    let buf = *b"hello0world";
    assert_eq!(countdown(11, &buf), 5);
    assert_eq!(countdown(0, &buf), 0);
    assert_eq!(countdown(3, &buf), 3);
    // (93+7)==100 → +1; (93-9)!=50 → +2; 93>=1 → +4; not <1 → total 7
    assert_eq!(adds(93), 7);
    // 0 wraps: (0+7)!=100, (0-9)!=50, 0>=1 no, 0<1 yes → 2+8 = 10
    assert_eq!(adds(0), 10);
    // boundary: (43+7-9)!=50? 43-9=34≠50 → +2; 43>=1 → +4 → 6
    assert_eq!(adds(43), 6);
    println!("icmpfold ok");
}
