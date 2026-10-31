// r[verify native.pgo.ctor]
#[inline(never)]
fn hot(n: u64) -> u64 {
    let mut x = 0;
    for i in 0..n {
        x += i * 3 + 1;
    }
    x
}

#[inline(never)]
fn cold_fn(x: u64) -> u64 {
    x * 7 + 11
}

#[inline(never)]
fn biased(x: u64) -> u64 {
    if x % 16 == 0 { 1 } else { 0 }
}

fn main() {
    let mut s = 0u64;
    for i in 0..10_000u64 {
        s = s.wrapping_add(hot(i % 97));
    }
    s += biased(15);
    if std::env::var("PGO_NEVER").is_ok() {
        cold_fn(s);
    }
    println!("{s}");
}
