// PLIRON_SCOPED_NOALIAS: an inlined callee's `noalias` parameter keeps
// its fact for the span of the inlined body. The shapes here are the
// ones that went wrong once the fact reached the passes: the callee's
// parameter and the caller's own pointer are two names for one object,
// and a store through either must be seen through the other.
//  - a `&self` method inlined into a `&mut self` loop that also grows the
//    same `Vec` (regex-syntax `HeapVisitor::visit`: a load of the `Vec`'s
//    pointer was hoisted out of the pushing loop);
//  - an in-place `into_iter().map().collect()` (source and destination
//    are one buffer);
//  - callee stores read back by the caller, caller stores read by the
//    callee, in loops and across calls that may or may not touch them.
// Output must match stock rustc's, with and without MIR inlining.
use std::hint::black_box as bb;

struct Stack {
    items: Vec<(u32, u32)>,
    seen: u64,
}

impl Stack {
    #[inline]
    fn top(&self) -> Option<&(u32, u32)> {
        self.items.last()
    }
    #[inline]
    fn depth(&self) -> usize {
        self.items.len()
    }
    #[inline]
    fn push(&mut self, x: (u32, u32)) {
        self.items.push(x);
        self.seen += 1;
    }
    // Reads through `&self` between pushes that reallocate.
    #[inline(never)]
    fn walk(&mut self, mut n: u32) -> u64 {
        let mut h = 0u64;
        while n > 0 {
            let d = self.depth() as u64;
            if let Some(&(a, b)) = self.top() {
                h = h.wrapping_mul(31).wrapping_add(u64::from(a ^ b) + d);
                if a % 3 == 0 {
                    self.items.pop();
                }
            }
            self.push((n, n.wrapping_mul(2654435761)));
            if n % 5 == 0 {
                self.push((n ^ 7, 1));
            }
            n -= 1;
        }
        h ^ self.seen ^ (self.depth() as u64) << 32
    }
}

#[derive(Clone, Copy)]
struct Pair {
    a: u64,
    b: u64,
}

#[inline]
fn bump(p: &mut Pair, by: u64) -> u64 {
    p.a = p.a.wrapping_add(by);
    p.b ^= p.a;
    p.a
}

#[inline]
fn read(p: &Pair) -> u64 {
    p.a.wrapping_mul(3) ^ p.b
}

#[inline]
fn set_through(p: &mut Pair, q: &mut u64, v: u64) -> u64 {
    p.a = v;
    *q = v ^ 1;
    p.a.wrapping_add(*q)
}

// The caller only has raw pointers: nothing function-wide to lean on.
#[inline(never)]
fn raw_loop(p: *mut Pair, q: *mut u64, n: u64) -> u64 {
    let mut h = 0u64;
    for i in 0..n {
        unsafe {
            (*p).b = (*p).b.wrapping_add(i);
            h ^= bump(&mut *p, i);
            h = h.wrapping_add((*p).a ^ (*p).b);
            h ^= set_through(&mut *p, &mut *q, h);
            h = h.wrapping_add(read(&*p)).wrapping_add(*q);
            (*p).a = (*p).a.rotate_left(3);
            h ^= read(&*p);
        }
    }
    h
}

struct Holder {
    left: Box<Pair>,
    right: Box<Pair>,
    log: Vec<u64>,
}

#[inline]
fn swap_add(x: &mut Pair, y: &mut Pair, out: &mut Vec<u64>) {
    let t = x.a;
    x.a = y.a.wrapping_add(1);
    out.push(x.a ^ t);
    y.a = t.wrapping_add(x.b);
    out.push(y.a ^ x.a);
    x.b = y.b ^ x.a;
}

#[inline(never)]
fn holder_loop(h: &mut Holder, n: u64) -> u64 {
    let mut s = 0u64;
    for i in 0..n {
        h.left.b = h.left.b.wrapping_add(i);
        swap_add(&mut h.left, &mut h.right, &mut h.log);
        s = s.wrapping_add(h.left.a ^ h.right.a ^ h.left.b);
        if i % 4 == 3 {
            h.log.clear();
        }
    }
    s ^ h.log.iter().fold(0u64, |a, &x| a.wrapping_mul(7) ^ x)
}

#[inline(never)]
fn in_place(v: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    v.into_iter().map(|(a, b)| (b.wrapping_add(1), a ^ b)).collect()
}

#[inline(never)]
fn in_place_narrow(v: Vec<u64>) -> Vec<u32> {
    v.into_iter().map(|x| (x >> 7) as u32).filter(|x| x % 3 != 0).collect()
}

fn main() {
    let mut st = Stack { items: Vec::new(), seen: 0 };
    println!("walk {:016x} {}", st.walk(bb(1000)), st.items.len());
    let mut p = Pair { a: 1, b: 2 };
    let mut q = 3u64;
    println!("raw {:016x} {} {} {}", raw_loop(&mut p, &mut q, bb(500)), p.a, p.b, q);
    let mut h = Holder {
        left: Box::new(Pair { a: 5, b: 6 }),
        right: Box::new(Pair { a: 7, b: 8 }),
        log: Vec::new(),
    };
    println!("holder {:016x} {} {}", holder_loop(&mut h, bb(300)), h.left.a, h.right.b);
    let v: Vec<(u32, u32)> = (0..bb(200u32)).map(|i| (i, i.wrapping_mul(40503))).collect();
    let w = in_place(v);
    println!("inplace {} {:?} {:?}", w.len(), w[0], w[199]);
    let n = in_place_narrow((0..bb(300u64)).map(|i| i.wrapping_mul(0x9E37_79B9)).collect());
    println!("narrow {} {:?}", n.len(), &n[..4]);
}
