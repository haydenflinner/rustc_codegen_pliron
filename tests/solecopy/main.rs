// PLIRON_MEMCPYOPT_SOLECOPY: a temporary whose only reader is the copy into
// its destination is not protected by memcpyopt's "tmp could promote" guard,
// so its scalar writers write the destination directly. lol_html's selector
// VM builds each 144-byte `StackItem` in a temp and pushes it onto a stack
// whose storage a call (the push) receives. The program does the same with
// a 144-byte item and a second item copied twice, and folds every field.
#[derive(Clone, Copy)]
struct Item {
    a: [u64; 8],
    b: u32,
    c: u16,
    d: [u8; 6],
    e: (u64, u64, u64, u64, u64, u64, u64),
}

#[inline(never)]
fn make(x: u64) -> Item {
    let mut it = Item { a: [0; 8], b: 0, c: 0, d: [0; 6], e: (0, 0, 0, 0, 0, 0, 0) };
    for (i, s) in it.a.iter_mut().enumerate() {
        *s = x.wrapping_mul(i as u64 + 3);
    }
    it.b = x as u32 ^ 0x5a5a;
    it.c = (x >> 3) as u16;
    it.d = [x as u8, 1, 2, 3, 4, (x >> 8) as u8];
    it.e = (x, x + 1, x + 2, x * 3, x ^ 9, !x, x.rotate_left(7));
    it
}

#[inline(never)]
fn push_all(stack: &mut Vec<Item>, n: u64) {
    for x in 0..n {
        let it = make(x);
        let twice = it;
        stack.push(twice);
        if x % 3 == 0 {
            let mut other = make(x + 100);
            other.b = it.b.wrapping_add(1);
            stack.push(other);
        }
    }
}

fn main() {
    let mut stack = Vec::new();
    push_all(&mut stack, 500);
    let mut h: u64 = 1469598103934665603;
    for it in &stack {
        let mut fold = |v: u64| h = (h ^ v).wrapping_mul(1099511628211);
        for v in it.a {
            fold(v);
        }
        fold(it.b as u64);
        fold(it.c as u64);
        for v in it.d {
            fold(v as u64);
        }
        let e = it.e;
        for v in [e.0, e.1, e.2, e.3, e.4, e.5, e.6] {
            fold(v);
        }
    }
    println!("{} {}", stack.len(), h);
}
