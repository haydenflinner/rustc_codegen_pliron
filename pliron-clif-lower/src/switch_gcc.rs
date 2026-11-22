//! GCC-style switch clustering (xoranth.net/gcc-switch).
//!
//! Pipeline on contiguous same-dest ranges:
//! 1. **Bit-test clusters** — span < 64, ≤3 unique dests, profitable
//!    by GCC's uniq/count thresholds.
//! 2. **Jump-table clusters** — DP splits; density `span ≤ ratio ×
//!    comparison_count` (ratio 8 for speed); at least `jt_min` cases.
//! 3. Leftovers stay as **leaf ranges** for a balanced decision tree.
//!
//! Emission is separate ([`crate::lower`]); this module only partitions.

#![allow(dead_code)] // dest type is opaque to tests

/// Contiguous `case lo..=hi → dest` (GCC `simple_cluster`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range<D: Copy> {
    pub lo: u64,
    pub hi: u64,
    pub dest: D,
}

impl<D: Copy> Range<D> {
    pub fn cmp_count(&self) -> u64 {
        if self.lo == self.hi {
            1
        } else {
            2
        }
    }
    pub fn n_labels(&self) -> u64 {
        self.hi - self.lo + 1
    }
}

/// One lowering unit after clustering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cluster<D: Copy> {
    /// `bt`-style: `(x - lo)` in `0..=(hi-lo)`, then bit `mask`.
    /// `edges[i]` = (bitset of offsets that go to `dest`, dest). ≤3 edges.
    BitTest {
        lo: u64,
        hi: u64,
        edges: Vec<(u64, D)>,
    },
    /// Dense table over `lo..=hi`; `None` slots are the switch default.
    JumpTable {
        lo: u64,
        hi: u64,
        slots: Vec<Option<D>>,
    },
    /// Single range → two compares (or one if `lo == hi`).
    Leaf(Range<D>),
}

/// Collapse sorted `(value, dest)` cases into contiguous same-dest ranges.
pub fn collapse_ranges<D: Copy + Eq>(vals: &[(u64, D)]) -> Vec<Range<D>> {
    if vals.is_empty() {
        return Vec::new();
    }
    let mut sorted = vals.to_vec();
    sorted.sort_by_key(|p| p.0);
    let mut out = Vec::new();
    let (mut lo, mut hi, mut dest) = (sorted[0].0, sorted[0].0, sorted[0].1);
    for &(v, d) in &sorted[1..] {
        if d == dest && v == hi.wrapping_add(1) {
            hi = v;
        } else {
            out.push(Range { lo, hi, dest });
            lo = v;
            hi = v;
            dest = d;
        }
    }
    out.push(Range { lo, hi, dest });
    out
}

/// GCC `bit_test_cluster::is_beneficial`.
fn bit_beneficial(count: usize, uniq: usize) -> bool {
    (uniq == 1 && count >= 3) || (uniq == 2 && count >= 5) || (uniq == 3 && count >= 6)
}

fn bit_can_handle<D: Copy + Eq>(ranges: &[Range<D>], start: usize, end: usize) -> bool {
    if start > end || end >= ranges.len() {
        return false;
    }
    let lo = ranges[start].lo;
    let hi = ranges[end].hi;
    if hi.saturating_sub(lo) >= 64 {
        return false;
    }
    let mut dests = Vec::with_capacity(3);
    for r in &ranges[start..=end] {
        if !dests.iter().any(|&d| d == r.dest) {
            if dests.len() >= 3 {
                return false;
            }
            dests.push(r.dest);
        }
    }
    true
}

fn bit_uniq_count<D: Copy + Eq>(ranges: &[Range<D>], start: usize, end: usize) -> (usize, usize) {
    // LLVM's `isSuitableForBitTests` counts compares (two per range), GCC
    // counts ranges. lol_html's whitespace / `/` / `>` byte switch is five
    // ranges, seven compares: one LLVM bit-test cluster, a GCC tree of five
    // leaves (`PLIRON_SWITCH_BT_GCC=1` restores GCC's count).
    static GCC: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| pliron_cfg::env_is("PLIRON_SWITCH_BT_GCC", "1"));
    let count = if *GCC {
        end - start + 1
    } else {
        ranges[start..=end].iter().map(|r| r.cmp_count() as usize).sum()
    };
    let mut dests = Vec::new();
    for r in &ranges[start..=end] {
        if !dests.iter().any(|&d| d == r.dest) {
            dests.push(r.dest);
        }
    }
    (count, dests.len())
}

/// Build a bit-test cluster covering `ranges[start..=end]`.
fn make_bit_test<D: Copy + Eq>(ranges: &[Range<D>], start: usize, end: usize) -> Cluster<D> {
    let lo = ranges[start].lo;
    let hi = ranges[end].hi;
    let mut by_dest: Vec<(D, u64)> = Vec::new();
    for r in &ranges[start..=end] {
        let mut bits = 0u64;
        for v in r.lo..=r.hi {
            bits |= 1u64 << (v - lo);
        }
        match by_dest.iter_mut().find(|(d, _)| *d == r.dest) {
            Some((_, m)) => *m |= bits,
            None => by_dest.push((r.dest, bits)),
        }
    }
    // Most cases first (LLVM's `buildBitTests` without a profile): the
    // last edge's test is the one the default path runs past.
    by_dest.sort_by_key(|&(_, m)| core::cmp::Reverse(m.count_ones()));
    Cluster::BitTest {
        lo,
        hi,
        edges: by_dest.into_iter().map(|(d, m)| (m, d)).collect(),
    }
}

/// GCC-style DP: pack ranges into bit-test clusters where profitable.
pub fn find_bit_tests<D: Copy + Eq>(ranges: &[Range<D>]) -> Vec<Cluster<D>> {
    let l = ranges.len();
    if l == 0 {
        return Vec::new();
    }
    // min[i] = (best bitset count for ranges[0..i), start of last bitset)
    let mut min_count = vec![usize::MAX; l + 1];
    let mut min_start = vec![0usize; l + 1];
    min_count[0] = 0;
    for i in 1..=l {
        for j in 0..i {
            if min_count[j] == usize::MAX {
                continue;
            }
            if !bit_can_handle(ranges, j, i - 1) {
                continue;
            }
            let c = min_count[j] + 1;
            if c < min_count[i] {
                min_count[i] = c;
                min_start[i] = j;
            }
        }
        // Always possible as singleton bitsets that we'll reject later.
        if min_count[i] == usize::MAX {
            min_count[i] = min_count[i - 1].saturating_add(1);
            min_start[i] = i - 1;
        }
    }
    let mut out = Vec::new();
    let mut end = l;
    while end > 0 {
        let start = min_start[end];
        let (count, uniq) = bit_uniq_count(ranges, start, end - 1);
        if bit_beneficial(count, uniq) && bit_can_handle(ranges, start, end - 1) {
            out.push(make_bit_test(ranges, start, end - 1));
        } else {
            for i in (start..end).rev() {
                out.push(Cluster::Leaf(ranges[i].clone()));
            }
        }
        end = start;
    }
    out.reverse();
    out
}

fn jt_can_handle<D: Copy>(
    ranges: &[Range<D>],
    start: usize,
    end: usize,
    max_ratio: u64,
) -> bool {
    if start > end {
        return false;
    }
    if start == end {
        return true; // algorithm needs this; pruned by is_beneficial
    }
    let lo = ranges[start].lo;
    let hi = ranges[end].hi;
    let span = hi.saturating_sub(lo).saturating_add(1);
    let mut cmp = 0u64;
    for r in &ranges[start..=end] {
        cmp += r.cmp_count();
    }
    // Avoid absurd tables.
    if span > 4096 {
        return false;
    }
    span <= max_ratio.saturating_mul(cmp)
}

fn jt_beneficial(start: usize, end: usize, jt_min: usize) -> bool {
    if start == end {
        return false;
    }
    end - start + 1 >= jt_min
}

fn make_jt<D: Copy>(ranges: &[Range<D>], start: usize, end: usize) -> Cluster<D> {
    let lo = ranges[start].lo;
    let hi = ranges[end].hi;
    let span = (hi - lo + 1) as usize;
    let mut slots = vec![None; span];
    for r in &ranges[start..=end] {
        for v in r.lo..=r.hi {
            slots[(v - lo) as usize] = Some(r.dest);
        }
    }
    Cluster::JumpTable { lo, hi, slots }
}

/// GCC-style DP: jump-table clusters among a run of leaf/simple ranges.
/// `max_ratio` is 8 (speed) or 2 (size); `jt_min` is usually 4–5.
pub fn find_jump_tables<D: Copy + Eq>(
    ranges: &[Range<D>],
    max_ratio: u64,
    jt_min: usize,
) -> Vec<Cluster<D>> {
    let l = ranges.len();
    if l == 0 {
        return Vec::new();
    }
    // Only operate on Leaf-shaped input: callers pass simple ranges.
    let mut min_count = vec![usize::MAX; l + 1];
    let mut min_start = vec![0usize; l + 1];
    let mut min_unhandled = vec![usize::MAX; l + 1];
    min_count[0] = 0;
    min_unhandled[0] = 0;
    for i in 1..=l {
        for j in 0..i {
            if min_count[j] == usize::MAX {
                continue;
            }
            if !jt_can_handle(ranges, j, i - 1, max_ratio) {
                continue;
            }
            let mut s = min_unhandled[j];
            let ncases = i - j;
            if ncases < jt_min {
                s = s.saturating_add(ncases);
            }
            let c = min_count[j] + 1;
            if c < min_count[i] || (c == min_count[i] && s < min_unhandled[i]) {
                min_count[i] = c;
                min_start[i] = j;
                min_unhandled[i] = s;
            }
        }
        if min_count[i] == usize::MAX {
            min_count[i] = min_count[i - 1].saturating_add(1);
            min_start[i] = i - 1;
            min_unhandled[i] = min_unhandled[i - 1].saturating_add(1);
        }
    }
    let mut out = Vec::new();
    let mut end = l;
    while end > 0 {
        let start = min_start[end];
        if jt_beneficial(start, end - 1, jt_min)
            && jt_can_handle(ranges, start, end - 1, max_ratio)
        {
            out.push(make_jt(ranges, start, end - 1));
        } else {
            for i in (start..end).rev() {
                out.push(Cluster::Leaf(ranges[i].clone()));
            }
        }
        end = start;
    }
    out.reverse();
    out
}

/// Full GCC pipeline on value/dest pairs: bit tests, then jump tables on
/// remaining simple runs, leaves intact for a decision tree.
pub fn clusterize<D: Copy + Eq>(
    vals: &[(u64, D)],
    max_ratio: u64,
    jt_min: usize,
) -> Vec<Cluster<D>> {
    let ranges = collapse_ranges(vals);
    let after_bits = find_bit_tests(&ranges);
    // Jump-table DP only over maximal runs of Leaf clusters.
    let mut out = Vec::new();
    let mut leaf_run: Vec<Range<D>> = Vec::new();
    let flush = |leaf_run: &mut Vec<Range<D>>, out: &mut Vec<Cluster<D>>| {
        if leaf_run.is_empty() {
            return;
        }
        out.extend(find_jump_tables(leaf_run, max_ratio, jt_min));
        leaf_run.clear();
    };
    for c in after_bits {
        match c {
            Cluster::Leaf(r) => leaf_run.push(r),
            other => {
                flush(&mut leaf_run, &mut out);
                out.push(other);
            }
        }
    }
    flush(&mut leaf_run, &mut out);
    out
}

/// Balanced decision-tree order: indices into `clusters` as an in-order
/// binary tree by cumulative leaf weight (uniform per case label).
/// Returns `(pivot_index, left_indices, right_indices)` recursively via
/// a flat emit plan: list of (lo_bound_exclusive_for_right, cluster_idx)
/// is overkill — emit side uses recursive split on sorted clusters.
pub fn cluster_weight<D: Copy>(c: &Cluster<D>) -> u64 {
    match c {
        Cluster::BitTest { lo, hi, .. } => hi - lo + 1,
        Cluster::JumpTable { slots, .. } => slots.iter().filter(|s| s.is_some()).count() as u64,
        Cluster::Leaf(r) => r.n_labels(),
    }
}

pub fn cluster_lo<D: Copy>(c: &Cluster<D>) -> u64 {
    match c {
        Cluster::BitTest { lo, .. } | Cluster::JumpTable { lo, .. } => *lo,
        Cluster::Leaf(r) => r.lo,
    }
}

pub fn cluster_hi<D: Copy>(c: &Cluster<D>) -> u64 {
    match c {
        Cluster::BitTest { hi, .. } | Cluster::JumpTable { hi, .. } => *hi,
        Cluster::Leaf(r) => r.hi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_merges_contiguous() {
        let v = [(1u64, 'a'), (2, 'a'), (3, 'a'), (5, 'b'), (6, 'b')];
        let r = collapse_ranges(&v);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], Range { lo: 1, hi: 3, dest: 'a' });
        assert_eq!(r[1], Range { lo: 5, hi: 6, dest: 'b' });
    }

    #[test]
    fn dense_becomes_jump_table() {
        // cases 0..4 → five dests, span 5, cmp 5 → 5 ≤ 8*5
        let vals: Vec<(u64, i32)> = (0..5).map(|i| (i, i as i32)).collect();
        let c = clusterize(&vals, 8, 4);
        assert!(
            matches!(&c[..], [Cluster::JumpTable { lo: 0, hi: 4, .. }]),
            "{c:?}"
        );
    }

    #[test]
    fn outlier_peels_to_jt_plus_leaf() {
        // 1..=5 dense + 100 outlier (GCC "outlier" example shape)
        let mut vals: Vec<(u64, i32)> = (1..=5).map(|i| (i, i as i32)).collect();
        vals.push((100, 99));
        let c = clusterize(&vals, 8, 4);
        assert!(
            c.iter().any(|x| matches!(x, Cluster::JumpTable { .. })),
            "expected a JT for the dense run: {c:?}"
        );
        assert!(
            c.iter().any(|x| matches!(x, Cluster::Leaf(Range { lo: 100, .. }))),
            "expected leaf for outlier: {c:?}"
        );
    }

    #[test]
    fn sparse_same_dest_becomes_bitset() {
        // ASCII digits/letters like the article's bittest — one dest
        let chars = [b'0', b'2', b'4', b'5', b'7', b'9', b'A', b'C', b'E'];
        let vals: Vec<(u64, i32)> = chars.iter().map(|&c| (c as u64, 1)).collect();
        let c = clusterize(&vals, 8, 5);
        assert!(
            matches!(&c[..], [Cluster::BitTest { .. }]),
            "expected one bit test: {c:?}"
        );
        if let Cluster::BitTest { lo, edges, .. } = &c[0] {
            assert_eq!(*lo, b'0' as u64);
            assert_eq!(edges.len(), 1);
            assert_eq!(edges[0].1, 1);
        }
    }

    #[test]
    fn very_sparse_stays_leaves() {
        // spacing 8 with 5 cases — GCC used a tree, not JT (ratio edge)
        let vals = [(0u64, 0i32), (8, 1), (16, 2), (24, 3), (40, 4)];
        let c = clusterize(&vals, 8, 5);
        // span 41, cmp 5 → 41 ≤ 40? no → leaves / maybe bit if range ok
        // hi-lo=40 < 64, uniq=5 > 3 → not bit; not JT → all leaves
        assert!(
            c.iter().all(|x| matches!(x, Cluster::Leaf(_))),
            "{c:?}"
        );
        // Decision tree balances over these leaves by weight.
        assert_eq!(c.len(), 5);
        let total: u64 = c.iter().map(cluster_weight).sum();
        assert_eq!(total, 5);
    }

    #[test]
    fn contiguous_range_is_one_leaf() {
        let vals: Vec<(u64, i32)> = (10..=20).map(|i| (i, 7)).collect();
        let c = clusterize(&vals, 8, 5);
        assert_eq!(
            c,
            vec![Cluster::Leaf(Range {
                lo: 10,
                hi: 20,
                dest: 7
            })]
        );
    }
}
