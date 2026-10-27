// `Ordering` / `Option<Ordering>` tests after three-way compares
// (`PLIRON_CMP3`): every `==`/`!=` against each variant, `is_lt`..`is_ne`,
// `matches!`, `is_some_and`, the derived lexicographic operators of a
// two-field struct (the join-block case: one spaceship per field merged
// into a param), and sort/binary_search callers. Signed, unsigned and
// float operands at the boundaries; NaN gives `partial_cmp` None, so the
// `== -2` niche test must stay live there. Every result is checked
// against a truth table indexed by a reference ordering built from plain
// `<`/`>`/`==`, and a checksum pinned from stock rustc is printed.
use std::cmp::Ordering::{self, Equal, Greater, Less};
use std::hint::black_box;

// Reference outcome: 0 = Less, 1 = Equal, 2 = Greater, 3 = None.
fn code<T: PartialOrd>(a: &T, b: &T) -> usize {
    if a < b {
        0
    } else if a > b {
        2
    } else if a == b {
        1
    } else {
        3
    }
}

macro_rules! pforms {
    ($($name:ident: |$o:ident| $e:expr, [$l:expr, $q:expr, $g:expr, $n:expr];)*) => {
        $(
            #[inline(never)]
            fn $name<T: PartialOrd>(a: T, b: T) -> bool {
                let $o: Option<Ordering> = a.partial_cmp(&b);
                $e
            }
        )*
        fn pcheck<T: PartialOrd + Copy>(a: T, b: T, sum: &mut u64) {
            let c = code(&a, &b);
            let mut k = 0u64;
            $(
                let r = $name(black_box(a), black_box(b));
                let want = [$l, $q, $g, $n][c];
                assert_eq!(r, want, "{} code {}", stringify!($name), c);
                k = k.wrapping_mul(3).wrapping_add(r as u64);
            )*
            *sum = sum.wrapping_mul(1_000_003).wrapping_add(k ^ c as u64);
        }
    };
}

pforms! {
    p_eq_l: |o| o == Some(Less), [true, false, false, false];
    p_eq_e: |o| o == Some(Equal), [false, true, false, false];
    p_eq_g: |o| o == Some(Greater), [false, false, true, false];
    p_eq_n: |o| o == None, [false, false, false, true];
    p_ne_l: |o| o != Some(Less), [false, true, true, true];
    p_ne_e: |o| o != Some(Equal), [true, false, true, true];
    p_ne_g: |o| o != Some(Greater), [true, true, false, true];
    p_ne_n: |o| o != None, [true, true, true, false];
    p_m_l: |o| matches!(o, Some(Less)), [true, false, false, false];
    p_m_le: |o| matches!(o, Some(Less | Equal)), [true, true, false, false];
    p_m_ge: |o| matches!(o, Some(Greater | Equal)), [false, true, true, false];
    p_m_ln: |o| matches!(o, Some(Less) | None), [true, false, false, true];
    p_lt: |o| o.is_some_and(Ordering::is_lt), [true, false, false, false];
    p_le: |o| o.is_some_and(Ordering::is_le), [true, true, false, false];
    p_gt: |o| o.is_some_and(Ordering::is_gt), [false, false, true, false];
    p_ge: |o| o.is_some_and(Ordering::is_ge), [false, true, true, false];
    p_iseq: |o| o.is_some_and(Ordering::is_eq), [false, true, false, false];
    p_isne: |o| o.is_some_and(Ordering::is_ne), [true, false, true, false];
    p_none: |o| o.is_none(), [false, false, false, true];
    p_unwrap_l: |o| o.unwrap_or(Equal) == Less, [true, false, false, false];
    p_map_ge: |o| o.map_or(true, |x| x != Less), [false, true, true, true];
    p_rev_l: |o| o.map(Ordering::reverse) == Some(Less), [false, false, true, false];
}

macro_rules! oforms {
    ($($name:ident: |$o:ident| $e:expr, [$l:expr, $q:expr, $g:expr];)*) => {
        $(
            #[inline(never)]
            fn $name<T: Ord>(a: T, b: T) -> bool {
                let $o: Ordering = a.cmp(&b);
                $e
            }
        )*
        fn ocheck<T: Ord + Copy>(a: T, b: T, sum: &mut u64) {
            let c = code(&a, &b);
            let mut k = 0u64;
            $(
                let r = $name(black_box(a), black_box(b));
                let want = [$l, $q, $g][c];
                assert_eq!(r, want, "{} code {}", stringify!($name), c);
                k = k.wrapping_mul(3).wrapping_add(r as u64);
            )*
            let raw = raw_ord(black_box(a), black_box(b));
            assert_eq!(raw, c as i8 - 1);
            *sum = sum.wrapping_mul(1_000_003).wrapping_add(k ^ c as u64);
        }
    };
}

#[inline(never)]
fn raw_ord<T: Ord>(a: T, b: T) -> i8 {
    a.cmp(&b) as i8
}

oforms! {
    o_eq_l: |o| o == Less, [true, false, false];
    o_eq_e: |o| o == Equal, [false, true, false];
    o_eq_g: |o| o == Greater, [false, false, true];
    o_ne_l: |o| o != Less, [false, true, true];
    o_ne_e: |o| o != Equal, [true, false, true];
    o_ne_g: |o| o != Greater, [true, true, false];
    o_lt: |o| o.is_lt(), [true, false, false];
    o_le: |o| o.is_le(), [true, true, false];
    o_gt: |o| o.is_gt(), [false, false, true];
    o_ge: |o| o.is_ge(), [false, true, true];
    o_iseq: |o| o.is_eq(), [false, true, false];
    o_isne: |o| o.is_ne(), [true, false, true];
    o_m_le: |o| matches!(o, Less | Equal), [true, true, false];
    o_m_lg: |o| matches!(o, Less | Greater), [true, false, true];
    o_rev_g: |o| o.reverse() == Greater, [true, false, false];
    o_then: |o| o.then(Greater) == Greater, [false, true, true];
}

// Operators: derived lexicographic structs reach them through a join.
macro_rules! ops {
    ($($name:ident: $op:tt, [$l:expr, $q:expr, $g:expr, $n:expr];)*) => {
        $(
            #[inline(never)]
            fn $name<T: PartialOrd>(a: T, b: T) -> bool {
                a $op b
            }
        )*
        fn opcheck<T: PartialOrd + Copy>(a: T, b: T, c: usize, sum: &mut u64) {
            let mut k = 0u64;
            $(
                let r = $name(black_box(a), black_box(b));
                assert_eq!(r, [$l, $q, $g, $n][c], "{} code {}", stringify!($name), c);
                k = k.wrapping_mul(3).wrapping_add(r as u64);
            )*
            *sum = sum.wrapping_mul(1_000_003).wrapping_add(k);
        }
    };
}

ops! {
    op_lt: <, [true, false, false, false];
    op_le: <=, [true, true, false, false];
    op_gt: >, [false, false, true, false];
    op_ge: >=, [false, true, true, false];
}

#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
struct P2<T> {
    a: T,
    b: T,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct O3<T> {
    a: T,
    b: T,
    c: u8,
}

// Nested: a struct of structs (inner joins feeding the outer one).
#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
struct Span<T> {
    lo: P2<T>,
    hi: P2<T>,
}

fn pref<T: PartialOrd>(x: &P2<T>, y: &P2<T>) -> usize {
    match code(&x.a, &y.a) {
        1 => code(&x.b, &y.b),
        c => c,
    }
}

fn run_partial<T: PartialOrd + Copy>(vals: &[T], sum: &mut u64) {
    for &a in vals {
        for &b in vals {
            pcheck(a, b, sum);
            opcheck(a, b, code(&a, &b), sum);
        }
    }
    // Two-field derives: every field combination (join-block case).
    let n = vals.len().min(6);
    for &a1 in &vals[..n] {
        for &b1 in &vals[..n] {
            for &a2 in &vals[..n] {
                for &b2 in &vals[..n] {
                    let x = P2 { a: a1, b: a2 };
                    let y = P2 { a: b1, b: b2 };
                    let c = pref(&x, &y);
                    assert_eq!(code(&x, &y), c);
                    pcheck(x, y, sum);
                    opcheck(x, y, c, sum);
                    let s1 = Span { lo: x, hi: y };
                    let s2 = Span { lo: x, hi: x };
                    let cs = match pref(&x, &x) {
                        1 => pref(&y, &x),
                        c => c,
                    };
                    assert_eq!(code(&s1, &s2), cs);
                    pcheck(s1, s2, sum);
                    opcheck(s1, s2, cs, sum);
                }
            }
        }
    }
}

fn run_total<T: Ord + Copy + std::fmt::Debug>(vals: &[T], sum: &mut u64) {
    for &a in vals {
        for &b in vals {
            ocheck(a, b, sum);
        }
    }
    let n = vals.len().min(5);
    let mut v: Vec<O3<T>> = Vec::new();
    for &a1 in &vals[..n] {
        for &b1 in &vals[..n] {
            for c in [0u8, 1, 255] {
                v.push(O3 { a: a1, b: b1, c });
            }
        }
    }
    for x in &v {
        for y in &v {
            ocheck(*x, *y, sum);
        }
    }
    // Sort / binary_search callers.
    let mut s = v.clone();
    s.reverse();
    let third = s.len() / 3;
    s.rotate_left(third);
    let mut r = s.clone();
    // Reference: insertion sort with an explicit lexicographic test.
    let less = |x: &O3<T>, y: &O3<T>| {
        x.a < y.a || (x.a == y.a && (x.b < y.b || (x.b == y.b && x.c < y.c)))
    };
    for i in 1..r.len() {
        let mut j = i;
        while j > 0 && less(&r[j], &r[j - 1]) {
            r.swap(j, j - 1);
            j -= 1;
        }
    }
    let mut s1 = s.clone();
    s1.sort();
    assert_eq!(s1, r);
    let mut s2 = s.clone();
    s2.sort_unstable_by(|x, y| x.cmp(y));
    assert_eq!(s2, r);
    let mut s3 = s.clone();
    s3.sort_by(|x, y| y.cmp(x));
    s3.reverse();
    assert_eq!(s3, r);
    for (i, x) in r.iter().enumerate() {
        assert_eq!(r.binary_search(x), Ok(i));
        let k = black_box(*x);
        let lt = r.iter().filter(|y| less(y, &k)).count();
        assert_eq!(r.partition_point(|y| y < &k), lt);
        *sum = sum.wrapping_mul(31).wrapping_add(lt as u64);
    }
    let mx = r.iter().max().copied();
    let mn = r.iter().min().copied();
    assert_eq!(mx, r.last().copied());
    assert_eq!(mn, r.first().copied());
}

fn main() {
    let mut sum = 0u64;
    run_total(&[0u8, 1, 2, 127, 128, 254, 255], &mut sum);
    run_total(&[i8::MIN, i8::MIN + 1, -2, -1, 0, 1, i8::MAX], &mut sum);
    run_total(&[0u32, 1, 0x7fff_ffff, 0x8000_0000, u32::MAX - 1, u32::MAX], &mut sum);
    run_total(&[i32::MIN, -1, 0, 1, i32::MAX - 1, i32::MAX], &mut sum);
    run_total(&[0u64, 1, 2, i64::MAX as u64, 1 << 63, u64::MAX], &mut sum);
    run_total(&[i64::MIN, i64::MIN + 1, -2, -1, 0, 1, i64::MAX], &mut sum);
    run_total(&[0u128, 1, u64::MAX as u128, 1 << 127, u128::MAX], &mut sum);
    run_total(&[i128::MIN, -1, 0, 1, i128::MAX], &mut sum);
    run_total(&['\0', 'a', 'z', '\u{10ffff}'], &mut sum);
    run_total(&[false, true], &mut sum);
    run_partial(&[i8::MIN, -1, 0, 1, i8::MAX], &mut sum);
    run_partial(&[0u16, 1, 0x8000, u16::MAX], &mut sum);
    run_partial(&[i64::MIN, -1, 0, 1, i64::MAX], &mut sum);
    run_partial(&[0usize, 1, usize::MAX], &mut sum);
    let f = [f64::NAN, f64::NEG_INFINITY, -1.0, -0.0, 0.0, f64::MIN_POSITIVE, 1.0, f64::INFINITY];
    run_partial(&f, &mut sum);
    let g = [f32::NAN, -f32::MAX, -0.0f32, 0.0, 1.5, f32::INFINITY];
    run_partial(&g, &mut sum);
    println!("{sum}");
}
