// r[verify iropt.inline.model]
// Inline cost-model coverage (`PLIRON_INLINE_MODEL`, on by default).
//
// `dispatch` is larger than the fixed op budget, but a constant `mode`
// actual folds its switch (and every arm's `mode | 1` factor), so the
// model inlines `drive`'s calls where the fixed thresholds cannot
// (PLIRON_INLINE_VERBOSE shows `accept` vs `reject (score)`).
// `giant` has no constant actual and must stay a call under both;
// `trip` is recursive and must never inline; `callheavy` is small but
// dense in non-inlinable calls, which the model prices extra.

macro_rules! dispatch_arms {
    ($m:ident, $x:ident, $($k:literal),+) => {
        match $m {
            $($k => $x
                .wrapping_mul($m | 1)
                .wrapping_add($k ^ 0x9e37_79b9_7f4a_7c15)
                ^ $x.rotate_left((($k + 5) % 13) as u32),)+
            _ => $x ^ $m.rotate_left(11),
        }
    };
}

fn dispatch(mode: u64, x: u64) -> u64 {
    dispatch_arms!(
        mode, x, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22,
        23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45,
        46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68,
        69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91,
        92, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111,
        112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129,
        130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147,
        148, 149
    )
}

// Two constant-mode calls in a loop callsite: the model folds both arms.
fn drive(x: u64) -> u64 {
    dispatch(3, x) ^ dispatch(5, x).rotate_left(7)
}

// Same computation with a runtime mode: the result the folded path must
// still produce, kept out of line so both are really computed.
#[inline(never)]
fn drive_dyn(mode: u64, x: u64) -> u64 {
    dispatch(mode, x) ^ dispatch(mode + 2, x).rotate_left(7)
}

// ~1000 ops of chained arithmetic, called twice with non-constant actuals:
// over every threshold and earning no savings — must not inline.
fn giant(mut s: u64) -> u64 {
    macro_rules! churn {
        ($($k:literal)+) => {
            $(s = s.wrapping_mul($k).wrapping_add($k * 7 + 1) ^ s.rotate_left(($k % 23) as u32);)+
        };
    }
    churn!(
        3 5 7 11 13 17 19 23 29 31 37 41 43 47 53 59 61 67 71 73 79 83 89 97 101 103 107 109 113 127
        131 137 139 149 151 157 163 167 173 179 181 191 193 197 199 211 223 227 229 233 239 241 251
        257 263 269 271 277 281 283 293 307 311 313 317 331 337 347 349 353 359 367 373 379 383 389
        397 401 409 419 421 431 433 439 443 449 457 461 463 467 479 487 491 499 503 509 521 523 541
        547 557 563 569 571 577 587 593 599 601 607 613 617 619 631 641 643 647 653 659 661 673 677
        683 691 701 709 719 727 733 739 743 751 757 761 769 773 787 797 809 811 821 823 827 829 839
        853 857 859 863 877 881 883 887 907 911 919 929 937 941 947 953 967 971 977 983 991 997
    );
    s
}

// Recursion is a hard legality gate, not a profitability one.
fn trip(n: u64) -> u64 {
    if n <= 1 {
        1
    } else {
        n.wrapping_add(trip(n - 1))
    }
}

#[inline(never)]
fn tiny(k: u64) -> u64 {
    k ^ k.rotate_left(7).wrapping_mul(0x100_0000_01b3)
}

// Small op count but ~50 non-inlinable calls: CALL_OP_W prices each extra,
// so the model keeps it a call where the fixed op count would inline it.
fn callheavy(mut s: u64) -> u64 {
    macro_rules! many {
        ($($k:literal)+) => {
            $(s = s.wrapping_add(tiny($k)).rotate_left(3) ^ tiny($k + 1);)+
        };
    }
    many!(
        1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28
    );
    s
}

fn main() {
    let seed = std::env::args().len() as u64;
    let mut acc = giant(seed) ^ giant(seed.rotate_left(9));
    for i in 0..16u64 {
        let a = drive(i ^ seed);
        assert_eq!(a, drive_dyn(3, i ^ seed));
        acc = acc.wrapping_add(a);
    }
    acc = acc.wrapping_add(trip(seed % 32 + 8));
    acc = acc.wrapping_add(callheavy(seed) ^ callheavy(seed ^ 0x5a5a));
    println!("inlinecm {acc}");
}
