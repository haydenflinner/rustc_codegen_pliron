// rangefold: `[lo,hi)` bound-pair tests become one `x-lo u<w` offset
// compare, and two same-width ranges a power-of-two apart union into
// `(x|d)-k_hi u<w` — only when the bit-d partition is exact. These
// shapes come from rustc's `is_ascii_*` expansion; the assertions pin
// down every window boundary the mask must get right.

#[inline(never)]
fn lower(c: u8) -> bool {
    c.is_ascii_lowercase()
}

#[inline(never)]
fn alpha(c: u8) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_uppercase()
}

#[inline(never)]
fn pair_lo_hi(x: u32) -> bool {
    x >= 100 && x < 132
}

// Widths differ (26 vs 13) — not unionable; bound_pairs alone must
// still fold each pair, and nothing may merge them.
#[inline(never)]
fn mismatched(x: u32) -> bool {
    (x >= 97 && x < 123) || (x >= 200 && x < 213)
}

// Same width, distance 32, but the hi range straddles a 64-window
// boundary ([160,186): 160%64=32 ≥32 but 32+26=58 ≤64 — exact). Use a
// deliberately *inexact* pair instead: [33,59) ∪ [65,91): d=32, but
// 33%64+26=58 >32 — bit-d-set values leak in. Must NOT union.
#[inline(never)]
fn inexact(x: u32) -> bool {
    (x >= 33 && x < 59) || (x >= 65 && x < 91)
}

fn main() {
    // Bound-pair + union: the a-z/A-Z case fold.
    for c in 0u32..=255 {
        let c = c as u8;
        let want_lower = (b'a'..=b'z').contains(&c);
        let want_alpha = want_lower || (b'A'..=b'Z').contains(&c);
        assert_eq!(lower(c), want_lower, "lower {c}");
        assert_eq!(alpha(c), want_alpha, "alpha {c}");
        let x = c as u32;
        assert_eq!(pair_lo_hi(x), (100..132).contains(&x), "pair {x}");
        assert_eq!(
            mismatched(x),
            (97..123).contains(&x) || (200..213).contains(&x),
            "mismatched {x}"
        );
        assert_eq!(
            inexact(x),
            (33..59).contains(&x) || (65..91).contains(&x),
            "inexact {x}"
        );
    }
    // The union trick's danger zone: bit-5-set bytes just below 'a'
    // (0x60 '`', 0x61 itself) and just past 'z'.
    for c in [0x40u8, 0x5b, 0x5f, 0x60, 0x61, 0x7a, 0x7b, 0x80, 0x9f, 0xe1] {
        assert_eq!(alpha(c), c.is_ascii_alphabetic(), "alpha boundary {c:#x}");
    }
    println!("rangefold ok");
}
