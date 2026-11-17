// memcpyopt and lifetime markers: a value built field by field early and
// moved into a struct whose storage starts late. Forwarding the copy puts
// the field stores into the struct ahead of its `lifetime.start`; if the
// marker stays, slotmerge takes the struct's slot for dead until then
// and gives it to the array that lives in between, which overwrites the
// moved fields (mimas `install_assoc`, 2026-11-16; the struct's other
// fields are written after the marker, which hides the stale marker from
// slotmerge's own check). `PLIRON_MEMCPYOPT_DSTMARK=0` puts the bug back.
use std::hint::black_box;

struct Holder {
    k: u64,
    hdr: [u64; 40],
    j: u64,
}

#[inline(never)]
fn mk(n: u64) -> [u64; 40] {
    std::array::from_fn(|k| n * 3 + k as u64)
}

#[inline(never)]
fn finish(h: &Holder, i: usize) -> u64 {
    h.hdr[i] * 1000 + h.k + h.j
}

#[inline(never)]
fn build(n: u64, i: usize, m: usize) -> u64 {
    let hdr = mk(n);
    let s = {
        let t = [n * 100, n * 100 + 1, n * 100 + 2, n * 100 + 3, n * 100 + 4, n, n, n, n];
        t[m] + t[m + 1]
    };
    let h = Holder { k: s, hdr, j: n };
    finish(&h, i)
}

fn main() {
    let mut total = 0;
    for n in 1..5u64 {
        for i in 0..40 {
            total += build(black_box(n * 7), black_box(i), black_box(i % 7));
        }
    }
    println!("{total}");
}
