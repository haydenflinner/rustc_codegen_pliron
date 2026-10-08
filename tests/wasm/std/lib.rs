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
