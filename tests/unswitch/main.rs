// Loop unswitching: a `brif` on a loop-invariant condition (the
// `cap == 0` early-out HashMap::get leaves after inlining) runs the
// same test every iteration. Unswitching clones the loop so each
// version sees a constant outcome; a guard picks per call. Both
// versions must stay correct — including when the folded side leaves
// a self-referencing dead cycle sharing defs with live clones.
#[inline(never)]
fn probe(keys: &[u64], table: &[u64], cap: u64) -> u64 {
    let mut t = 0u64;
    for &k in keys {
        if cap == 0 {
            continue;
        }
        let h = (k % cap) as usize;
        if table[h] == k {
            t = t.wrapping_add(h as u64);
        }
    }
    t
}

fn main() {
    let keys: Vec<u64> = (0..2000u64).map(|i| i * 977 % 1000).collect();
    let mut table = vec![u64::MAX; 997];
    for i in 0..500u64 {
        table[(i * 7 % 997) as usize] = i * 7 % 997;
    }
    let hit = probe(&keys, &table, 997);
    let miss = probe(&keys, &table, 0);
    assert_eq!(miss, 0);
    let want: u64 = keys
        .iter()
        .filter(|&&k| table[(k % 997) as usize] == k)
        .map(|&k| (k % 997) as u64)
        .sum();
    assert_eq!(hit, want);
    assert!(hit != 0);
    println!("{hit} {miss}");
}
