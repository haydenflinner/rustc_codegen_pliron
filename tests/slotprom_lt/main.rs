// PLIRON_SLOTPROM_LT / PLIRON_SLOTPROM_SYNCCOST: stack slots that slotprom
// promotes (locals whose address only reaches `&mut` calls, so SROA keeps
// them in memory) with their storage windows inside loops.
//
// - LT ends a promoted field's value at the slot's lifetime markers: a
//   per-iteration local read on one path and stored on another must not
//   see an earlier iteration's value where the source reads it, and a
//   local carried across iterations (declared outside the loop) must keep
//   it.
// - SYNCCOST keeps a slot in memory when its `&mut` calls would cost more
//   writebacks and reloads than promotion saves (regex-syntax `Ast::fmt`'s
//   heap-visitor stack).
//
// Every printed value must match the default build (and stock rustc).

use std::hint::black_box;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Acc {
    sum: u64,
    max: u64,
    last: u64,
    n: u32,
}

#[inline(never)]
fn bump(a: &mut Acc, x: u64) {
    a.sum = a.sum.wrapping_add(x);
    if x > a.max {
        a.max = x;
    }
    a.last = x;
    a.n += 1;
}

#[inline(never)]
fn maybe(a: &mut Acc, x: u64) -> bool {
    if x % 3 == 0 {
        bump(a, x);
        true
    } else {
        false
    }
}

// A fresh `Acc` per iteration (its lifetime starts and ends inside the
// loop), filled through `&mut` calls on some paths only; `outer` lives
// across all iterations.
#[inline(never)]
pub fn per_iter(xs: &[u64]) -> (u64, Acc) {
    let mut outer = Acc::default();
    let mut h = 0u64;
    for &x in xs {
        let mut a = Acc::default();
        if maybe(&mut a, x) {
            bump(&mut a, x + 1);
        }
        if x & 1 == 1 {
            bump(&mut outer, a.sum);
        }
        h = h.wrapping_mul(31).wrapping_add(a.sum ^ a.max ^ u64::from(a.n));
        if a.n == 2 {
            h ^= a.last;
        }
    }
    (h, outer)
}

#[derive(Debug)]
pub enum Node {
    Leaf(u64),
    Cat(Vec<Node>),
    Alt(Vec<Node>),
}

#[inline(never)]
fn emit(out: &mut String, s: &str) {
    out.push_str(s);
}

// `Ast::fmt`-shaped: an explicit heap stack pushed and popped around
// `&mut` calls, so its `Vec` headers sync at every call when promoted.
#[inline(never)]
pub fn walk(root: &Node, out: &mut String) -> usize {
    let mut stack: Vec<(&Node, usize)> = Vec::new();
    let mut done: Vec<usize> = Vec::new();
    stack.push((root, 0));
    let mut depth_max = 0;
    while let Some((n, d)) = stack.pop() {
        depth_max = depth_max.max(d);
        match n {
            Node::Leaf(x) => {
                emit(out, &x.to_string());
                done.push(d);
            }
            Node::Cat(v) | Node::Alt(v) => {
                emit(out, if matches!(n, Node::Cat(_)) { "(" } else { "[" });
                for c in v.iter().rev() {
                    stack.push((c, d + 1));
                }
                done.push(v.len());
            }
        }
        if stack.len() > 64 {
            break;
        }
    }
    depth_max + done.iter().sum::<usize>() + done.len()
}

fn tree(seed: u64, depth: u32) -> Node {
    if depth == 0 || seed % 5 == 0 {
        return Node::Leaf(seed % 97);
    }
    let k = (seed % 4 + 1) as usize;
    let kids = (0..k).map(|i| tree(seed.wrapping_mul(6364136223846793005).wrapping_add(i as u64 + 1) >> 7, depth - 1)).collect();
    if seed & 1 == 0 { Node::Cat(kids) } else { Node::Alt(kids) }
}

fn main() {
    let xs: Vec<u64> = (0..200u64).map(|i| i.wrapping_mul(2654435761) % 1000).collect();
    let (h, outer) = per_iter(black_box(&xs));
    println!("per_iter {h} {outer:?}");
    for seed in [1u64, 7, 42, 1234567] {
        let t = tree(seed, 5);
        let mut s = String::new();
        let r = walk(black_box(&t), &mut s);
        println!("walk {seed} {r} {} {}", s.len(), &s[..s.len().min(60)]);
    }
}
