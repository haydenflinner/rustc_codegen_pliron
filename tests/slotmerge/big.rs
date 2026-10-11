#![crate_type="rlib"]

#[derive(Clone)]
pub struct Big { a: [u64; 8], b: [u64; 8], c: u64 }

#[inline(never)]
fn make(i: u64) -> Big { Big { a: [i; 8], b: [i + 1; 8], c: i } }

#[inline(never)]
fn useit(b: &Big) -> u64 { b.a[0] + b.c }

pub fn dispatch(sel: u64, x: u64) -> u64 {
    match sel {
        0 => { let a = make(x); let b = make(x + 1); useit(&a) + useit(&b) }
        1 => { let a = make(x + 2); let b = make(x + 3); useit(&a) + useit(&b) }
        2 => { let a = make(x + 4); let b = make(x + 5); useit(&a) + useit(&b) }
        3 => { let a = make(x + 6); let b = make(x + 7); useit(&a) + useit(&b) }
        4 => { let a = make(x + 8); let b = make(x + 9); useit(&a) + useit(&b) }
        5 => { let a = make(x + 10); let b = make(x + 11); useit(&a) + useit(&b) }
        6 => { let a = make(x + 12); let b = make(x + 13); useit(&a) + useit(&b) }
        7 => { let a = make(x + 14); let b = make(x + 15); useit(&a) + useit(&b) }
        8 => { let a = make(x + 16); let b = make(x + 17); useit(&a) + useit(&b) }
        9 => { let a = make(x + 18); let b = make(x + 19); useit(&a) + useit(&b) }
        10 => { let a = make(x + 20); let b = make(x + 21); useit(&a) + useit(&b) }
        11 => { let a = make(x + 22); let b = make(x + 23); useit(&a) + useit(&b) }
        12 => { let a = make(x + 24); let b = make(x + 25); useit(&a) + useit(&b) }
        13 => { let a = make(x + 26); let b = make(x + 27); useit(&a) + useit(&b) }
        14 => { let a = make(x + 28); let b = make(x + 29); useit(&a) + useit(&b) }
        15 => { let a = make(x + 30); let b = make(x + 31); useit(&a) + useit(&b) }
        16 => { let a = make(x + 32); let b = make(x + 33); useit(&a) + useit(&b) }
        17 => { let a = make(x + 34); let b = make(x + 35); useit(&a) + useit(&b) }
        18 => { let a = make(x + 36); let b = make(x + 37); useit(&a) + useit(&b) }
        19 => { let a = make(x + 38); let b = make(x + 39); useit(&a) + useit(&b) }
        _ => x,
    }
}
