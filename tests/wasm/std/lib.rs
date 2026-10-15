use std::collections::{BTreeMap, HashMap};

#[unsafe(no_mangle)]
pub extern "C" fn words(n: u32) -> u32 {
    let text: String = (0..n).map(|i| format!("w{} ", i % 17)).collect();
    let mut m: HashMap<&str, u32> = HashMap::new();
    for w in text.split_whitespace() {
        *m.entry(w).or_default() += 1;
    }
    let b: BTreeMap<_, _> = m.into_iter().collect();
    b.values().copied().max().unwrap_or(0) * 1000 + b.len() as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn vec_sort(n: u32) -> u64 {
    let mut v: Vec<u64> = (0..n as u64)
        .map(|i| i.wrapping_mul(0x9e3779b97f4a7c15) >> 40)
        .collect();
    v.sort();
    v.dedup();
    v.iter().sum::<u64>() ^ v.len() as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn float_fmt() -> u32 {
    let s = format!(
        "{:.5} {:e} {}",
        std::f64::consts::PI,
        1.5e300,
        0.1f32 + 0.2f32
    );
    s.len() as u32
}

/// Forces dlmalloc to grow linear memory (`memory.grow`).
#[unsafe(no_mangle)]
pub extern "C" fn big_alloc(mb: u32) -> u32 {
    let v = vec![1u8; (mb as usize) << 20];
    v.iter().map(|&x| x as u32).sum()
}

/// HashMap<u64, _> grown past its initial capacity — forces hashbrown's
/// `reserve_rehash`, which calls the hasher closure through a vtable
/// funcref (regression: a zeroed table slot trapped here).
#[unsafe(no_mangle)]
pub extern "C" fn hm_u64(n: u32) -> u32 {
    let mut m: HashMap<u64, u32> = HashMap::with_capacity(64);
    let mut x = 1u64;
    for _ in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *m.entry(x).or_insert(0) += 1;
    }
    let mut miss = 0u32;
    let mut y = 1u64;
    for _ in 0..n {
        y ^= y << 13;
        y ^= y >> 7;
        y ^= y << 17;
        if m.get(&y).is_none() {
            miss += 1;
        }
    }
    m.len() as u32 * 1000 + miss
}

/// `RandomState` hashing must depend on contents, not pointer identity,
/// and `str` equality must both accept and reject correctly.
#[unsafe(no_mangle)]
pub extern "C" fn hash_stable() -> u32 {
    use std::hash::{BuildHasher, Hash, Hasher};
    let rs = std::hash::RandomState::new();
    let text = String::from("w0 w1 w0 w1");
    let a = &text[0..2];
    let b = &text[6..8];
    let h1 = rs.hash_one(a);
    let h2 = rs.hash_one(b);
    let h3 = rs.hash_one(&*a);
    let h4 = {
        let mut h = rs.build_hasher();
        a.hash(&mut h);
        h.finish()
    };
    let neq = (&text[0..2] != &text[3..5]) as u32;
    let eqp = (&text[0..2] == &text[6..8]) as u32;
    (h1 == h2) as u32 + 2 * (h1 == h3) as u32 + 4 * (h1 == h4) as u32 + 8 * neq + 16 * eqp
}
