// PLIRON_LTUNDEF: a promoted (SROA'd) local's variables restart at zero at
// each `llvm.lifetime` marker, so a dead value no longer rides a loop's
// block params into the next iteration. The rewrite is only sound if no
// read of a local survives past its storage window. These shapes put a
// local's lifetime inside a loop and read it right up to the end, or keep
// one live across iterations from outside: every printed value must match
// the default build's output (and stock rustc's, which the asserts pin).
// PLIRON_REMATCALL (cheap constants rematerialized into loops that contain
// calls) runs on the same program, and t_spills checks `ltu_visit`'s spill
// bytes drop under it.
//
// `ltu_visit` follows regex-syntax's NestLimiter::check (a heap-stack
// visitor with an inlined inner class loop, `?` on a `Result<(), Error>`
// whose `Ok` writes only the niche word), the function PLIRON_LTUNDEF
// shrank from 912 to 416 frame bytes.

use std::hint::black_box;

#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    kind: u32,
    depth: u32,
    span: [u64; 6],
    msg: String,
    notes: Vec<u64>,
    extra: [u64; 4],
}

#[derive(Clone)]
pub enum Node {
    Leaf(u64),
    Group(Box<Node>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Class(Vec<Item>),
}

#[derive(Clone)]
pub enum Item {
    Lit(u64),
    Union(Vec<Item>),
}

pub struct Limiter<'a> {
    limit: &'a u32,
    depth: u32,
    seen: u64,
}

impl<'a> Limiter<'a> {
    #[inline(never)]
    fn error(&self, kind: u32, span: u64) -> Error {
        Error {
            kind,
            depth: self.depth,
            span: [span, span + 1, span + 2, span + 3, span + 4, span + 5],
            msg: format!("nest {kind} at {span}"),
            notes: vec![span; (kind % 3) as usize],
            extra: [self.seen, 1, 2, 3],
        }
    }

    fn inc(&mut self, span: u64) -> Result<(), Error> {
        let new = self.depth.checked_add(1).ok_or_else(|| self.error(u32::MAX, span))?;
        if new > *black_box(self.limit) {
            return Err(self.error(new, span));
        }
        self.depth = new;
        Ok(())
    }

    fn dec(&mut self) {
        self.depth = self.depth.checked_sub(1).unwrap();
    }

    #[inline(never)]
    fn note(&mut self, x: u64) -> Result<(), Error> {
        self.seen = self.seen.wrapping_mul(31).wrapping_add(x);
        if x == 0xdead { Err(self.error(7, x)) } else { Ok(()) }
    }
}

/// regex-syntax's `visit_class`: an inner heap-stack loop with its own `?`s,
/// inlined into the outer loop and itself `?`-ed — its `Result` local lives
/// inside the outer loop body.
#[inline(always)]
fn visit_class<'n>(v: &mut Limiter, items: &'n [Item], stack: &mut Vec<&'n [Item]>) -> Result<(), Error> {
    let mut cur = items;
    loop {
        let Some((first, rest)) = cur.split_first() else {
            match stack.pop() {
                None => return Ok(()),
                Some(r) => {
                    v.dec();
                    cur = r;
                    continue;
                }
            }
        };
        match first {
            Item::Lit(x) => v.note(*x ^ 0x40)?,
            Item::Union(xs) => {
                v.inc(xs.len() as u64)?;
                stack.push(rest);
                cur = xs;
                continue;
            }
        }
        cur = rest;
    }
}

enum Frame<'n> {
    Group,
    List { rest: &'n [Node], alt: bool },
}

#[inline(never)]
pub fn ltu_visit(root: &Node, limit: u32) -> Result<u64, Error> {
    let lim = limit;
    let mut v = Limiter { limit: &lim, depth: 0, seen: 1 };
    let mut stack: Vec<(&Node, Frame)> = Vec::new();
    let mut cstack: Vec<&[Item]> = Vec::new();
    let mut node = root;
    'outer: loop {
        match node {
            Node::Leaf(x) => v.note(*x)?,
            Node::Class(items) => {
                v.inc(9)?;
                visit_class(&mut v, items, &mut cstack)?;
                v.dec();
            }
            Node::Group(inner) => {
                v.inc(1)?;
                stack.push((node, Frame::Group));
                node = inner;
                continue;
            }
            Node::Concat(xs) | Node::Alt(xs) => {
                v.inc(xs.len() as u64 + 2)?;
                if let Some((first, rest)) = xs.split_first() {
                    let alt = matches!(node, Node::Alt(_));
                    stack.push((node, Frame::List { rest, alt }));
                    node = first;
                    continue;
                }
                v.dec();
            }
        }
        loop {
            let Some((parent, frame)) = stack.pop() else { break 'outer };
            match frame {
                Frame::Group => {
                    v.note(3)?;
                    v.dec();
                }
                Frame::List { rest, alt } => {
                    if let Some((next, rest)) = rest.split_first() {
                        if alt {
                            v.note(5)?;
                        }
                        stack.push((parent, Frame::List { rest, alt }));
                        node = next;
                        continue 'outer;
                    }
                    v.dec();
                }
            }
        }
    }
    Ok(v.seen ^ v.depth as u64)
}

fn tree(seed: u64, depth: u32) -> Node {
    let k = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    if depth == 0 || k % 7 == 0 {
        return Node::Leaf(if k % 101 == 0 { 0xdead } else { k >> 40 });
    }
    match k % 4 {
        3 => Node::Class(items(k, depth)),
        0 => Node::Group(Box::new(tree(k, depth - 1))),
        1 => Node::Concat((0..(k % 4)).map(|i| tree(k ^ i, depth - 1)).collect()),
        _ => Node::Alt((0..(k % 3 + 1)).map(|i| tree(k + i, depth - 1)).collect()),
    }
}

fn items(seed: u64, depth: u32) -> Vec<Item> {
    (0..seed % 4)
        .map(|i| {
            let k = (seed ^ i).wrapping_mul(2862933555777941757).wrapping_add(3037000493);
            if depth == 0 || k % 3 == 0 {
                Item::Lit(if k % 89 == 0 { 0xdead ^ 0x40 } else { k >> 44 })
            } else {
                Item::Union(items(k, depth - 1))
            }
        })
        .collect()
}

/// A local declared inside the loop whose `Ok` writes only part of it,
/// collected on `Err`: the dead payload of iteration i must not leak into
/// iteration i+1's reads.
#[inline(never)]
fn collect_errs(n: u64) -> (Vec<Error>, u64) {
    let lim = 2u32;
    let mut v = Limiter { limit: &lim, depth: 0, seen: 0 };
    let mut errs = Vec::new();
    let mut oks = 0u64;
    for i in 0..n {
        let r = if i % 3 == 0 { v.note(0xdead) } else { v.note(i) };
        match r {
            Ok(()) => oks += 1,
            Err(e) => errs.push(e),
        }
        let r2 = v.inc(i);
        if let Err(e) = r2 {
            errs.push(e);
            v.depth = 0;
        }
    }
    (errs, oks)
}

/// An aggregate local that lives across iterations (declared outside the
/// loop) and is partially overwritten inside it: its other fields must
/// survive the inner locals' markers.
#[inline(never)]
fn carried(n: u64) -> u64 {
    let mut acc: Option<[u64; 6]> = None;
    let mut sum = 0u64;
    for i in 0..n {
        let step: Option<[u64; 6]> = if black_box(i) % 4 == 1 { None } else { Some([i, i * 2, i * 3, i ^ 7, i + 9, !i]) };
        match (acc, step) {
            (Some(a), Some(s)) => acc = Some([a[0] + s[0], a[1] ^ s[1], a[2], a[3] + s[3], a[4], s[5]]),
            (None, s) => acc = s,
            (Some(a), None) => sum = sum.wrapping_add(a.iter().fold(0, |x, y| x.wrapping_mul(3) ^ y)),
        }
    }
    sum ^ acc.map_or(0, |a| a[0] ^ a[5])
}

fn main() {
    let mut h = 0u64;
    for s in 0..200u64 {
        let t = tree(s, 6);
        for lim in [1u32, 3, 8, 100] {
            match ltu_visit(black_box(&t), lim) {
                Ok(x) => h = h.wrapping_mul(1000003) ^ x,
                Err(e) => {
                    h = h.wrapping_mul(1000003) ^ (e.kind as u64) ^ (e.depth as u64) << 32;
                    h ^= e.span.iter().sum::<u64>() ^ e.notes.len() as u64 ^ e.extra[0];
                    h ^= e.msg.len() as u64;
                }
            }
        }
    }
    let (errs, oks) = collect_errs(black_box(40));
    assert_eq!(oks, 26);
    assert_eq!(errs.len(), 14 + 13);
    for e in &errs {
        assert_eq!(e.span[5], e.span[0] + 5);
        assert_eq!(e.msg, format!("nest {} at {}", e.kind, e.span[0]));
        assert_eq!(e.notes.len(), (e.kind % 3) as usize);
    }
    let c = carried(black_box(1000));
    println!("visit {h:x}");
    println!("errs {} oks {oks} first {:?} last {:?}", errs.len(), errs[0].kind, errs[errs.len() - 1].span);
    println!("carried {c:x}");
}
