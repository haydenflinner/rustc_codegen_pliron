// `PLIRON_SIBCALL=1`: `call; return` becomes `return_call` on aarch64's
// platform ABIs and the internal `tail` one. The output is pinned by stock
// rustc; test.sh also checks that the state functions branch to each other.
//
// Shapes that must keep the call: a slot address handed to the callee, a
// slot address stored in memory the callee reads, stack arguments, and a
// callee that returns through `sret`. Shapes that may convert: a state
// machine whose states are reached through function pointers (the platform
// ABI, as lol_html's tokenizer), float and `u128` arguments, a caller that
// stores to its own slots and hands the callee nothing of them.
use std::hint::black_box;

type State = fn(&mut Machine, &[u8], usize) -> u64;

struct Machine {
    acc: u64,
    tags: u64,
    states: [State; 3],
}

#[inline(never)]
fn data_state(m: &mut Machine, s: &[u8], i: usize) -> u64 {
    if i >= s.len() {
        return m.acc;
    }
    m.acc = m.acc.wrapping_mul(31).wrapping_add(s[i] as u64);
    let next = if s[i] == b'<' { m.states[1] } else { m.states[0] };
    next(m, s, i + 1)
}

#[inline(never)]
fn tag_state(m: &mut Machine, s: &[u8], i: usize) -> u64 {
    if i >= s.len() {
        return m.acc ^ m.tags;
    }
    m.tags += 1;
    let next = if s[i] == b'>' { m.states[0] } else if s[i] == b'"' { m.states[2] } else { m.states[1] };
    next(m, s, i + 1)
}

#[inline(never)]
fn attr_state(m: &mut Machine, s: &[u8], i: usize) -> u64 {
    if i >= s.len() {
        return m.acc.rotate_left(7);
    }
    m.acc ^= (s[i] as u64) << (i % 13);
    let next = if s[i] == b'"' { m.states[1] } else { m.states[2] };
    next(m, s, i + 1)
}

// Direct sibling calls between `pub` (platform-ABI) functions.
#[inline(never)]
pub fn walk_even(n: u64, acc: u64) -> u64 {
    if n == 0 { acc } else { walk_odd(n - 1, acc.wrapping_add(n * 3)) }
}

#[inline(never)]
pub fn walk_odd(n: u64, acc: u64) -> u64 {
    if n == 0 { acc ^ 1 } else { walk_even(n - 1, acc ^ (n << 1)) }
}

#[inline(never)]
fn read_ptr(p: *const u64) -> u64 {
    unsafe { black_box(*black_box(p)) }
}

// Must keep the call: the callee reads this frame through the argument.
#[inline(never)]
pub fn slot_arg(x: u64) -> u64 {
    let v = black_box(x * 5);
    read_ptr(&v)
}

struct Holder {
    p: *const u64,
}

#[inline(never)]
fn read_held(h: &Holder) -> u64 {
    unsafe { *black_box(h.p) }
}

// Must keep the call: the frame's address is stored in `h`, which lives
// on the heap, and the callee reads through it.
#[inline(never)]
pub fn slot_stored(x: u64) -> u64 {
    let v = black_box(x + 11);
    let h = Box::new(Holder { p: &v });
    let r = read_held(black_box(&*h));
    r
}

#[inline(never)]
pub fn ten(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, i: u64, j: u64) -> u64 {
    black_box(a ^ b.rotate_left(1) ^ c.rotate_left(2) ^ d ^ e ^ f ^ g ^ h ^ i.wrapping_mul(j))
}

// Must keep the call: two arguments go on the stack.
#[inline(never)]
pub fn stack_args(x: u64) -> u64 {
    ten(x, x + 1, x + 2, x + 3, x + 4, x + 5, x + 6, x + 7, x + 8, x + 9)
}

#[derive(Clone, Copy)]
pub struct Big {
    w: [u64; 6],
}

#[inline(never)]
pub fn make_big(x: u64) -> Big {
    let mut w = [0u64; 6];
    for (k, s) in w.iter_mut().enumerate() {
        *s = black_box(x.wrapping_mul(k as u64 + 1));
    }
    Big { w }
}

// Returned through `sret`: either the call stays or the pointer passes on.
#[inline(never)]
pub fn big_of(x: u64) -> Big {
    make_big(x ^ 0x55)
}

#[inline(never)]
pub fn fmix(a: f64, b: f32, c: u128) -> f64 {
    black_box(a * b as f64 + (c >> 64) as f64 + (c as u64) as f64)
}

#[inline(never)]
pub fn fwd_float(a: f64, b: f32, c: u128) -> f64 {
    fmix(a + 1.0, b * 2.0, c.wrapping_mul(3))
}

// Stores into its own slot, reads it back, hands the callee only a value.
#[inline(never)]
pub fn local_then_call(x: u64) -> u64 {
    let mut a = [x; 4];
    for k in 0..4 {
        a[k] = a[k].wrapping_mul(k as u64 + black_box(3));
    }
    walk_even(black_box(a.iter().sum::<u64>() % 64), 7)
}

// Values live across a call to a function that tail-calls a leaf
// (FASTCC must count the leaf's writes in the middle function's set).
#[inline(never)]
fn leaf_mix(x: u64) -> u64 {
    x.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (x >> 29)
}

#[inline(never)]
fn middle(x: u64) -> u64 {
    leaf_mix(x + 1)
}

#[inline(never)]
pub fn across_middle(xs: &[u64]) -> u64 {
    let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (1u64, 2u64, 3u64, 4u64, 5u64, 6u64, 7u64, 8u64);
    for &x in xs {
        let m = middle(x);
        a = a.wrapping_add(m);
        b ^= a.rotate_left(3);
        c = c.wrapping_mul(b | 1);
        d ^= c >> 7;
        e = e.wrapping_add(d);
        f ^= e.rotate_left(11);
        g = g.wrapping_add(f ^ x);
        h ^= g;
    }
    a ^ b ^ c ^ d ^ e ^ f ^ g ^ h
}

fn main() {
    let mut text = Vec::new();
    for k in 0..200_000u32 {
        text.push(b"ab<cd e=\"f\">g"[(k % 13) as usize]);
    }
    let mut m = Machine { acc: 0, tags: 0, states: [data_state, tag_state, attr_state] };
    let mut total = 0u64;
    // Deep enough that a frame per transition would show (each state is a
    // frame without sibling calls), shallow enough for the default stack.
    for start in 0..4 {
        let n = black_box(20_000usize);
        total = total.wrapping_add((m.states[0])(&mut m, &text[..n], start));
    }
    println!("machine {total} tags {}", m.tags);
    println!("walk {}", walk_even(black_box(10_000), 0));
    println!("slot_arg {} slot_stored {}", slot_arg(black_box(9)), slot_stored(black_box(9)));
    println!("stack_args {}", stack_args(black_box(100)));
    let b = big_of(black_box(12345));
    println!("big {:?}", b.w);
    println!("float {}", fwd_float(black_box(1.5), black_box(2.25), black_box(7u128 << 70 | 99)));
    println!("local {}", local_then_call(black_box(77)));
    let xs: Vec<u64> = (0..1000).map(|i| i * 7 + 1).collect();
    println!("across {}", across_middle(black_box(&xs)));
}
