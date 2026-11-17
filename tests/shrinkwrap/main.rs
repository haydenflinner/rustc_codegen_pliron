// PLIRON_SHRINKWRAP=1: the callee saves move from the entry to a save block
// that dominates the blocks needing them. Two emission-time rewrites used
// to bypass that block: EDGEHOIST retargeted the entry's branch past a save
// block that held only regalloc moves and a jump, and edge merging could
// drop or alias it. The epilogue then restored callee-saved registers that
// were never saved (regex's `IntervalSet::canonicalize` crashed). This is
// that function's shape: an early return when the set is already
// canonical (no frame needed), otherwise a sort, a merge loop that pushes,
// and a drain; the caller keeps values in callee-saved registers across
// the calls and checks them afterwards.
use std::hint::black_box;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Range {
    lo: u32,
    hi: u32,
}

impl Range {
    fn contiguous(&self, o: &Range) -> bool {
        self.lo.max(o.lo) <= self.hi.min(o.hi).saturating_add(1)
    }
    fn union(&self, o: &Range) -> Option<Range> {
        if !self.contiguous(o) {
            return None;
        }
        Some(Range { lo: self.lo.min(o.lo), hi: self.hi.max(o.hi) })
    }
}

struct Set {
    ranges: Vec<Range>,
}

impl Set {
    fn is_canonical(&self) -> bool {
        for p in self.ranges.windows(2) {
            if p[0] >= p[1] || p[0].contiguous(&p[1]) {
                return false;
            }
        }
        true
    }

    #[inline(never)]
    fn canonicalize(&mut self) {
        if self.is_canonical() {
            return;
        }
        self.ranges.sort();
        assert!(!self.ranges.is_empty());
        let drain_end = self.ranges.len();
        for oldi in 0..drain_end {
            if self.ranges.len() > drain_end {
                let (last, rest) = self.ranges.split_last_mut().unwrap();
                if let Some(u) = last.union(&rest[oldi]) {
                    *last = u;
                    continue;
                }
            }
            let r = self.ranges[oldi];
            self.ranges.push(r);
        }
        self.ranges.drain(..drain_end);
    }
}

#[inline(never)]
fn run(seed: u64) -> u64 {
    // Values the caller keeps live across `canonicalize` (callee-saved).
    let a = black_box(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let b = black_box(seed ^ 0xdead_beef);
    let c = black_box(seed.rotate_left(17));
    let mut h = 0u64;
    let mut x = seed | 1;
    for round in 0..200u64 {
        let n = (round % 13) as usize;
        let mut ranges = Vec::with_capacity(n);
        for _ in 0..n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let lo = (x % 500) as u32;
            let hi = lo + (x >> 32) as u32 % 20;
            ranges.push(Range { lo, hi });
        }
        if round % 3 == 0 {
            // Already canonical: the early return, with no frame.
            ranges.sort();
            ranges.dedup_by(|b, a| a.contiguous(b));
            let mut prev: Option<Range> = None;
            ranges.retain(|r| {
                let ok = prev.is_none_or(|p| p.hi.saturating_add(1) < r.lo);
                if ok {
                    prev = Some(*r);
                }
                ok
            });
        }
        let mut s = Set { ranges };
        if !s.ranges.is_empty() {
            s.canonicalize();
        }
        for r in &s.ranges {
            h = (h ^ (r.lo as u64) << 32 ^ r.hi as u64).wrapping_mul(0x100_0000_01b3);
        }
        h = h.wrapping_add(s.ranges.len() as u64);
    }
    h ^ a.wrapping_add(b).wrapping_add(c)
}

fn main() {
    let mut t = 0u64;
    for seed in 1..40u64 {
        t = t.wrapping_mul(31).wrapping_add(run(seed));
    }
    println!("{t}");
}
