// Stack-move: a local a callee fills through `&mut` (dropped if that
// call unwinds) is then moved whole into another local, which outlives
// it. rustc's own lowering (lower_item_kind) is full of these 300-byte
// pairs.
use std::hint::black_box;

pub struct Big {
    words: [u64; 38],
    tag: Vec<u8>,
}

impl Big {
    fn new() -> Big {
        Big { words: [0; 38], tag: Vec::new() }
    }
}

#[inline(never)]
fn fill(b: &mut Big, seed: u64) {
    if seed == 7777 {
        panic!("unlucky seed");
    }
    for (i, w) in b.words.iter_mut().enumerate() {
        *w = seed.wrapping_mul(i as u64 + 1);
    }
    b.tag.push(seed as u8);
}

#[inline(never)]
fn consume(out: &mut Vec<Big>, d: Big) {
    out.push(d);
}

#[inline(never)]
fn build(out: &mut Vec<Big>, seed: u64) {
    let mut s = Big::new();
    fill(&mut s, seed);
    let d = s;
    consume(out, d);
}

fn main() {
    let mut out = Vec::new();
    for seed in 1..6 {
        build(&mut out, black_box(seed));
    }
    let r = std::panic::catch_unwind(|| {
        let mut v = Vec::new();
        build(&mut v, black_box(7777));
        v.len()
    });
    let sum: u64 = out.iter().map(|b| b.words.iter().sum::<u64>() + b.tag[0] as u64).sum();
    println!("{} {} {}", out.len(), sum, r.is_err());
}
