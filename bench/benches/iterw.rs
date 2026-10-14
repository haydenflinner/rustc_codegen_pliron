pub fn setup() -> Vec<i32> {
    (0..4_000_000i32).collect()
}

pub fn run(a: &[i32]) -> i64 {
    let mut acc = 0i64;
    for _ in 0..20 {
        acc = acc.wrapping_add(
            a.iter().filter(|&&x| x % 3 != 0).map(|&x| x as i64 * x as i64).sum::<i64>(),
        );
    }
    acc
}
