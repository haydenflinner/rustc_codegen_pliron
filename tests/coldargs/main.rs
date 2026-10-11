#[cold]
#[inline(never)]
fn cold_sink(x: i128) {
    std::hint::black_box(x);
}

fn main() {
    let mut a: i128 = std::env::args().len() as i128;
    for i in 0..10 {
        a = a.wrapping_mul(3).wrapping_add(i);
    }
    if a > 0 {
        cold_sink(a);
    }
    println!("{a}");
}
