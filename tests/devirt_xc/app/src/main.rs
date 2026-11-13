use std::hint::black_box;

use dvlib::Op;

struct Xor(u64);
impl Op for Xor {
    fn apply(&self, x: u64) -> u64 {
        x ^ self.0
    }
}

#[inline(never)]
fn chain(ops: &[Box<dyn Op>], x: u64) -> u64 {
    ops.iter().fold(x, |a, o| o.apply(a))
}

fn main() {
    // Downstream target: `dvlib::run` calls closures defined here.
    let k = black_box(3u32);
    let mut seen = 0u32;
    let a = dvlib::run(&mut |x| x.wrapping_mul(k), 2000);
    let b = dvlib::run(
        &mut |x| {
            seen += 1;
            x ^ seen
        },
        100,
    );
    println!("run: {a} {b} {seen}");

    // Upstream vtable and target: closures made in `dvlib`, called here.
    let fs: Vec<Box<dyn Fn(u32) -> u32>> = (0..4).map(|i| dvlib::make(black_box(i))).collect();
    let mut s = 0u32;
    for i in 0..3000u32 {
        s = s.wrapping_add(fs[(i % 4) as usize](i));
    }
    println!("make: {s}");
    let xs: Vec<u16> = (0..500).collect();
    println!("fold: {}", dvlib::fold(&*fs[0], &xs) ^ dvlib::fold(&*fs[1], &xs));

    // Upstream impls plus one defined here.
    let mut ops = dvlib::ops();
    ops.push(Box::new(Xor(0x55)));
    let mut x = 1u64;
    for _ in 0..1000 {
        x = chain(black_box(&ops), x);
    }
    println!("ops: {x}");
}
