// PLIRON_MEMCPYOPT_BIGTMP: `driftsort_main` fills a 4 KiB `AlignedStorage`
// scratch through an inlined `memset` and copies it whole into the slot
// `drift::sort` receives. The temporary is too big for sroa, so memcpyopt
// sinks its writer into the destination and the frame keeps one 4 KiB slot.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Span {
    start: (u32, u32, u32),
    end: (u32, u32, u32),
}

#[inline(never)]
fn sort_spans(v: &mut Vec<Span>) {
    v.sort();
}

fn main() {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut v = Vec::new();
    for _ in 0..300 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let a = (x % 97) as u32;
        let b = ((x >> 20) % 89) as u32;
        v.push(Span { start: (a, b, 1), end: (b, a, 2) });
    }
    sort_spans(&mut v);
    let mut h: u64 = 1469598103934665603;
    for s in &v {
        for k in [s.start.0, s.start.1, s.end.0, s.end.1] {
            h = (h ^ k as u64).wrapping_mul(1099511628211);
        }
    }
    println!("{} {}", v.len(), h);
}
