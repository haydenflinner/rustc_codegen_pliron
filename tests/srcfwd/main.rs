// PLIRON_MEMCPYOPT_SRCFWD: a temporary that only a copy of an unaliased
// parameter writes reads the parameter instead. lol_html's selector VM moves
// its by-value `ExecutionCtx` into a local and its `stack_item` out of that
// local again. `plain` is the shape the rule takes; in `written` the
// parameter is written between the copy and the reads, in `captured` a call
// that kept the parameter's address writes it there, and in `dropped` the
// temporary holds an owned value an unwind path would drop: each must read
// the bytes the copy took.
use std::cell::Cell;

#[derive(Clone, Copy)]
struct Item {
    a: [u64; 8],
    b: u32,
    c: u16,
    d: [u8; 6],
    e: [u64; 7],
}

struct Ctx {
    item: Item,
    flag: bool,
    k: u64,
}

struct Owned {
    item: Item,
    name: String,
}

thread_local! {
    static KEEP: Cell<*mut Ctx> = const { Cell::new(std::ptr::null_mut()) };
}

#[inline(always)]
fn fold(it: &Item) -> u64 {
    let mut h: u64 = 1469598103934665603;
    let mut f = |v: u64| h = (h ^ v).wrapping_mul(1099511628211);
    for v in it.a {
        f(v);
    }
    f(it.b as u64);
    f(it.c as u64);
    for v in it.d {
        f(v as u64);
    }
    for v in it.e {
        f(v);
    }
    h
}

fn mk(x: u64) -> Item {
    let mut it = Item { a: [0; 8], b: x as u32 ^ 0x5a5a, c: (x >> 3) as u16, d: [x as u8, 1, 2, 3, 4, 5], e: [0; 7] };
    for i in 0..8 {
        it.a[i] = x.wrapping_mul(i as u64 + 3);
    }
    for i in 0..7 {
        it.e[i] = x.rotate_left(i as u32 + 1);
    }
    it
}

#[inline(never)]
fn stash(c: &mut Ctx) {
    KEEP.with(|k| k.set(c));
}

#[inline(never)]
fn poke(x: u64) {
    KEEP.with(|k| {
        let p = k.get();
        if !p.is_null() {
            unsafe { (*p).item.a[0] ^= x };
            unsafe { (*p).item.e[6] = x };
        }
    });
}

#[inline(never)]
fn plain(ctx: Ctx) -> u64 {
    let it = ctx.item;
    if ctx.flag { fold(&it) ^ ctx.k } else { fold(&it).wrapping_add(ctx.k) }
}

#[inline(never)]
fn written(mut ctx: Ctx, x: u64) -> u64 {
    let it = ctx.item;
    ctx.item.a[0] = x;
    ctx.item.e[3] = !x;
    fold(&it) ^ ctx.item.a[0] ^ ctx.item.e[3]
}

#[inline(never)]
fn captured(mut ctx: Ctx, x: u64) -> u64 {
    stash(&mut ctx);
    let it = ctx.item;
    poke(x);
    let r = fold(&it) ^ ctx.item.a[0];
    KEEP.with(|k| k.set(std::ptr::null_mut()));
    r
}

#[inline(never)]
fn dropped(o: Owned, x: u64) -> (u64, usize) {
    let it = o.item;
    let mut o = o;
    o.item.a[1] = x;
    (fold(&it) ^ o.item.a[1], o.name.len())
}

fn main() {
    let mut h = 0u64;
    for x in 0..2000u64 {
        let c = Ctx { item: mk(x), flag: x & 1 == 0, k: x * 7 };
        h = h.wrapping_mul(31).wrapping_add(plain(c));
        let c = Ctx { item: mk(x + 1), flag: false, k: x };
        h = h.wrapping_mul(31).wrapping_add(written(c, x ^ 0x77));
        let c = Ctx { item: mk(x + 2), flag: true, k: x };
        h = h.wrapping_mul(31).wrapping_add(captured(c, x | 1));
        let o = Owned { item: mk(x + 3), name: format!("n{x}") };
        let (a, b) = dropped(o, x ^ 5);
        h = h.wrapping_mul(31).wrapping_add(a).wrapping_add(b as u64);
    }
    println!("{h:016x}");
}
