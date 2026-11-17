// PLIRON_SROA_FRAGCOPY_OUT: an aggregate that only direct stores write and
// that one copy moves out (here into a new Box) is built in place: sroa
// splits it, the copy becomes the stores into the box, and no stack slot is
// left. Shaped like regex-syntax's `Properties::empty` (`PropertiesI`, 80
// bytes of words, u32 sets and bools: FRAGCOPY's sub-word pieces).
#![allow(dead_code)]
use std::hint::black_box;

#[derive(Clone, Debug)]
struct Props {
    min_len: Option<usize>,
    max_len: Option<usize>,
    look: u32,
    look_pre: u32,
    look_suf: u32,
    look_pre_any: u32,
    look_suf_any: u32,
    utf8: bool,
    caps: usize,
    static_caps: Option<usize>,
    literal: bool,
    alt_literal: bool,
}

#[inline(never)]
fn make(n: usize, flag: bool) -> Box<Props> {
    Box::new(Props {
        min_len: Some(n),
        max_len: if flag { None } else { Some(n + 3) },
        look: n as u32,
        look_pre: (n >> 1) as u32,
        look_suf: 7,
        look_pre_any: 0,
        look_suf_any: (n as u32) ^ 5,
        utf8: flag,
        caps: n * 2,
        static_caps: Some(0),
        literal: !flag,
        alt_literal: true,
    })
}

fn main() {
    let mut h: u64 = 0;
    for i in 0..1000usize {
        let p = make(black_box(i), i % 3 == 0);
        let s = format!("{:?}", black_box(&p));
        for b in s.bytes() {
            h = (h ^ b as u64).wrapping_mul(1099511628211);
        }
    }
    println!("{h}");
}
