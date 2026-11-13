// Profile-guided field order (`PLIRON_FIELD_ORDER`, docs/pgo.md): sizes,
// alignments and every value must be the same whatever the order file
// says; only the `offset_of` lines may differ. tests/field_order/run.sh
// builds this plain, with PLIRON_FIELD_HEAT, and with order files that
// force permutations, and diffs the output.
use std::hint::black_box;
use std::mem::{align_of, offset_of, size_of};
use std::num::NonZeroU32;

#[derive(Clone, Debug, PartialEq)]
struct Mixed {
    a: u8,
    b: u16,
    c: u32,
    d: u64,
    e: u8,
    f: u16,
    g: u32,
    h: u64,
    p: usize,
    q: [u8; 3],
    r: [u16; 5],
}

#[derive(Clone, Debug, PartialEq)]
struct Inner {
    x: u32,
    y: u32,
    z: u64,
    t: [u8; 6],
    u: u16,
}

#[derive(Clone, Debug, PartialEq)]
struct Outer {
    i1: Inner,
    k: u64,
    i2: Inner,
    arr: [u32; 3],
    small: u8,
    opt: Option<Box<Inner>>,
    name: String,
    unit: (),
}

#[derive(Clone, Debug, PartialEq)]
struct Gen<T, U> {
    a: T,
    b: U,
    c: T,
    d: u64,
    e: U,
}

#[derive(Clone, Debug, PartialEq)]
struct Niched<'a> {
    n: NonZeroU32,
    v: u32,
    flag: bool,
    w: u8,
    r: &'a u64,
    q: u64,
    m: u32,
}

#[derive(Debug)]
struct Tail<T: ?Sized> {
    a: u32,
    b: u32,
    c: u64,
    t: T,
}

#[derive(Clone, Debug, PartialEq)]
struct Elem {
    id: u32,
    w: u32,
    x: u64,
    y: u64,
    tag: u8,
    k: u16,
}

#[derive(Clone, Debug, PartialEq)]
struct Tup(u8, u64, u16, u64, u32, u8, u32);

#[inline(never)]
fn mix(m: &mut Mixed, i: u64) {
    m.a = m.a.wrapping_add(i as u8);
    m.b = m.b.wrapping_mul(3).wrapping_add(i as u16);
    m.c ^= (i as u32) << 3;
    m.d = m.d.wrapping_add(m.c as u64 * 7);
    m.e = m.e.rotate_left(1) ^ m.a;
    m.f = m.f.wrapping_sub(m.b);
    m.g = m.g.wrapping_add(m.c);
    m.h ^= m.d.rotate_left(i as u32 & 31);
    m.p = m.p.wrapping_add(m.h as usize & 0xff);
    m.q[(i % 3) as usize] ^= m.e;
    m.r[(i % 5) as usize] = m.r[(i % 5) as usize].wrapping_add(m.f);
}

#[inline(never)]
fn outer_step(o: &mut Outer, i: u64) {
    o.i1.x = o.i1.x.wrapping_add(i as u32);
    o.i1.z ^= o.k;
    o.i1.t[(i % 6) as usize] = i as u8;
    o.i2.y = o.i2.y.wrapping_mul(5).wrapping_add(1);
    o.i2.u = o.i2.u.wrapping_add(o.small as u16);
    o.k = o.k.wrapping_add(o.i2.y as u64);
    o.arr[(i % 3) as usize] += 1;
    o.small = o.small.wrapping_add(3);
    if i % 4 == 0 {
        o.opt = Some(Box::new(o.i1.clone()));
    } else if let Some(b) = &mut o.opt {
        b.z = b.z.wrapping_add(i);
    }
    if i % 7 == 0 {
        o.name.push((b'a' + (i % 26) as u8) as char);
    }
}

#[inline(never)]
fn gen_step<T, U>(g: &mut Gen<T, U>, i: u64) {
    if i % 2 == 0 {
        std::mem::swap(&mut g.a, &mut g.c);
    } else {
        std::mem::swap(&mut g.b, &mut g.e);
    }
    g.d = g.d.wrapping_mul(31).wrapping_add(i);
}

#[inline(never)]
fn niche_step<'a>(n: &mut Niched<'a>, other: &'a u64, i: u64) -> Option<Niched<'a>> {
    n.v = n.v.wrapping_add(n.n.get());
    n.n = NonZeroU32::new(n.n.get().wrapping_mul(3) | 1).unwrap();
    n.flag = !n.flag;
    n.w = n.w.wrapping_add(1);
    n.q ^= *n.r;
    n.m = n.m.wrapping_add(i as u32);
    if i % 3 == 0 {
        n.r = other;
    }
    if n.flag { Some(n.clone()) } else { None }
}

#[inline(never)]
fn tail_sum(t: &Tail<[u8]>) -> u64 {
    t.a as u64 + t.b as u64 * 3 + t.c + t.t.iter().map(|&x| x as u64).sum::<u64>() + t.t.len() as u64
}

#[inline(never)]
fn elems(v: &mut Vec<Elem>, i: u64) {
    v.push(Elem { id: i as u32, w: (i * 3) as u32, x: i * i, y: !i, tag: i as u8, k: (i * 7) as u16 });
    for e in v.iter_mut() {
        e.x = e.x.wrapping_add(e.id as u64);
        e.y ^= e.w as u64;
        e.tag = e.tag.wrapping_add(1);
        e.k = e.k.wrapping_add(e.tag as u16);
    }
    if i % 5 == 4 {
        v.swap_remove(0);
    }
    v.sort_by_key(|e| (e.k, e.id));
}

fn main() {
    macro_rules! sz {
        ($($t:ty),* $(,)?) => {$(
            println!("{}: size {} align {}", stringify!($t), size_of::<$t>(), align_of::<$t>());
        )*};
    }
    sz!(
        Mixed, Inner, Outer, Gen<u8, u32>, Gen<u64, u16>, Gen<Inner, u8>, Niched<'static>,
        Option<Niched<'static>>, Option<Outer>, Tail<[u8; 4]>, Tail<[u8; 9]>, Elem, Tup,
        Option<Gen<bool, u64>>, Vec<Elem>,
    );
    println!(
        "offset_of Mixed a {} d {} h {} q {} r {}",
        offset_of!(Mixed, a),
        offset_of!(Mixed, d),
        offset_of!(Mixed, h),
        offset_of!(Mixed, q),
        offset_of!(Mixed, r)
    );
    println!(
        "offset_of Outer i1 {} k {} i2 {} opt {} | Elem id {} x {} k {} | Niched n {} r {} flag {}",
        offset_of!(Outer, i1),
        offset_of!(Outer, k),
        offset_of!(Outer, i2),
        offset_of!(Outer, opt),
        offset_of!(Elem, id),
        offset_of!(Elem, x),
        offset_of!(Elem, k),
        offset_of!(Niched<'static>, n),
        offset_of!(Niched<'static>, r),
        offset_of!(Niched<'static>, flag)
    );
    println!(
        "offset_of Tail<[u8;4]> a {} b {} c {} t {} | Tup 0 {} 1 {} 6 {}",
        offset_of!(Tail<[u8; 4]>, a),
        offset_of!(Tail<[u8; 4]>, b),
        offset_of!(Tail<[u8; 4]>, c),
        offset_of!(Tail<[u8; 4]>, t),
        offset_of!(Tup, 0),
        offset_of!(Tup, 1),
        offset_of!(Tup, 6)
    );

    let n = black_box(100u64);
    let mut m = Mixed { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, p: 9, q: [1, 2, 3], r: [1, 2, 3, 4, 5] };
    let inner = Inner { x: 10, y: 20, z: 30, t: [1, 2, 3, 4, 5, 6], u: 40 };
    let mut o = Outer {
        i1: inner.clone(),
        k: 77,
        i2: Inner { x: 1, ..inner.clone() },
        arr: [5, 6, 7],
        small: 9,
        opt: None,
        name: String::from("s"),
        unit: (),
    };
    let mut g1 = Gen { a: 1u8, b: 2u32, c: 3u8, d: 4, e: 5u32 };
    let mut g2 = Gen { a: 11u64, b: 12u16, c: 13u64, d: 14, e: 15u16 };
    let mut g3 = Gen { a: inner.clone(), b: 1u8, c: Inner { z: 99, ..inner.clone() }, d: 5, e: 2u8 };
    let mut g4: Option<Gen<bool, u64>> = None;
    let (r1, r2) = (black_box(1234u64), black_box(99u64));
    let mut nc = Niched { n: NonZeroU32::new(5).unwrap(), v: 1, flag: false, w: 2, r: &r1, q: 3, m: 4 };
    let mut last: Option<Niched> = None;
    let mut somes = 0;
    let mut v = Vec::new();
    let mut tup = Tup(1, 2, 3, 4, 5, 6, 7);
    let mut tails = 0u64;
    for i in 0..n {
        mix(&mut m, i);
        outer_step(&mut o, i);
        gen_step(&mut g1, i);
        gen_step(&mut g2, i);
        gen_step(&mut g3, i);
        g4 = match g4 {
            None => Some(Gen { a: i % 2 == 0, b: i, c: i % 3 == 0, d: i * 2, e: !i }),
            Some(mut g) => {
                gen_step(&mut g, i);
                if g.a && g.c { None } else { Some(g) }
            }
        };
        if let Some(x) = niche_step(&mut nc, &r2, i) {
            somes += 1;
            last = Some(x);
        }
        elems(&mut v, i);
        tup.1 = tup.1.wrapping_add(tup.0 as u64 + tup.4 as u64);
        tup.0 = tup.0.wrapping_add(tup.5);
        tup.2 ^= i as u16;
        tup.3 = tup.3.rotate_left(3) ^ tup.1;
        tup.4 = tup.4.wrapping_add(tup.6);
        tup.6 = tup.6.wrapping_mul(3);
        let sized = Tail { a: i as u32, b: 2, c: i * 5, t: [i as u8, 1, 2, 3] };
        let boxed: Box<Tail<[u8]>> = Box::new(sized);
        tails = tails.wrapping_add(tail_sum(&boxed));
        let on_stack = Tail { a: 7, b: i as u32, c: 1, t: [9u8; 9] };
        let dynref: &Tail<[u8]> = &on_stack;
        tails = tails.wrapping_add(tail_sum(dynref));
    }
    println!("{m:?}");
    println!("{o:?}");
    println!("{g1:?} {g2:?}");
    println!("{g3:?} {g4:?}");
    println!("{nc:?} somes {somes} last {last:?}");
    println!("{} elems, first {:?} last {:?}", v.len(), v.first(), v.last());
    println!("{tup:?} tails {tails}");
    let t: Box<Tail<[u8]>> = Box::new(Tail { a: 1, b: 2, c: 3, t: [4u8, 5, 6, 7] });
    println!("{t:?} size_of_val {}", std::mem::size_of_val(&*t));
    println!("eq {} {} {}", m == m.clone(), o == o.clone(), last == Some(nc.clone()));
}
