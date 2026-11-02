// `PLIRON_XCGU_HOME`: `work` is copied into every CGU that calls it (big
// `PLIRON_XCGU_SIZE`) but is `#[inline(never)]`, so every copy stays
// called. It also calls the `#[cold]` `note`, which is never copied, so
// each copy reaches it only through a call: a degraded copy. With HOME the
// copies call the one home definition again; the results and the
// function's address must not change.

mod a {
    #[cold]
    #[inline(never)]
    pub fn note(h: u64) -> u64 {
        h ^ 0x5555
    }

    #[inline(never)]
    pub fn work(xs: &[u64], k: u64) -> u64 {
        let mut h = k;
        for (i, &x) in xs.iter().enumerate() {
            h = h.rotate_left(5) ^ x.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            if i % 3 == 0 {
                h = h.wrapping_add(i as u64);
            }
            if x == 7 {
                h = note(h);
            }
        }
        h
    }
}

mod m1 {
    pub fn go(v: &[u64]) -> u64 {
        crate::a::work(v, 1)
    }
    pub fn ptr() -> fn(&[u64], u64) -> u64 {
        crate::a::work
    }
}
mod m2 {
    pub fn go(v: &[u64]) -> u64 {
        crate::a::work(v, 2) ^ crate::a::work(&v[1..], 3)
    }
    pub fn ptr() -> fn(&[u64], u64) -> u64 {
        crate::a::work
    }
}
mod m3 {
    pub fn go(v: &[u64]) -> u64 {
        (0..4).map(|k| crate::a::work(v, k)).fold(0, |a, b| a.wrapping_add(b))
    }
}

fn main() {
    let v: Vec<u64> = (0..1000u64).map(|i| i * i + 7).collect();
    let r = [m1::go(&v), m2::go(&v), m3::go(&v)];
    println!("{:x} {:x} {:x}", r[0], r[1], r[2]);
    assert!(m1::ptr() as usize == m2::ptr() as usize || std::env::var("XCGU_PTR_ANY").is_ok());
    assert_eq!(r[0], m1::ptr()(&v, 1));
}
