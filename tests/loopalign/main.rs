#[inline(never)]
fn spin(n: u64, mut a: u64) -> u64 {
    let mut i = n;
    while i > 1 {
        a = a.wrapping_add(i.rotate_left(1) ^ 0x9e3779b97f4a7c15);
        i -= 1;
    }
    a
}

fn main() {
    let v = spin(std::hint::black_box(4096), 1);
    println!("{v}");
    assert_eq!(v, spin(4096, 1));
}
