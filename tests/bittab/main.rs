// bittab's sparse-set fold emitted `ushr mask, d; icmp ne 0` — nonzero
// whenever ANY accepted bit sits at-or-above d, i.e. the whole [lo, max]
// interval rather than the sparse set. A contiguous mask hides this
// (x <= max IS membership there); {21,28,35} does not: values below the
// lowest bit (12) and in the holes (22..27, 29..34) wrongly matched.
// rustc's parse_constness_ hit it: `kw::Fn`=12 "matched" {Move,Static,Use}
// in check_const_closure, so `const fn` was rejected at stage2.

#[inline(never)]
fn member(x: u32, y: u32) -> u32 {
    // Sparse small-domain set in the icmp/brif tree shape bittab folds;
    // the conditional hit arm keeps jumpthread from select-collapsing it.
    if x == 21 || x == 28 || x == 35 {
        if y > 5 { 1 } else { 2 }
    } else {
        0
    }
}

fn main() {
    let mut hit = 0u32;
    let mut miss = 0u32;
    for x in 0u32..70 {
        let r = member(x, 9);
        match x {
            21 | 28 | 35 => {
                hit += 1;
                assert_eq!(r, 1, "{x}");
            }
            _ => {
                miss += 1;
                assert_eq!(r, 0, "{x} wrongly accepted");
            }
        }
    }
    assert_eq!(hit, 3);
    assert_eq!(miss, 67);
}
