// Vec, String, HashMap, slices.
use std::collections::HashMap;
use std::hint::black_box;

#[inline(never)]
fn words(text: &str) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let lower = text.to_lowercase();
    for w in lower.split_whitespace() {
        *counts.entry(w.to_string()).or_insert(0) += 1;
    }
    let total = counts.values().sum::<usize>();
    sink(total); // BREAK
    counts
}

#[inline(never)]
fn evens(v: &Vec<u32>) -> Vec<u32> {
    let mut out = Vec::with_capacity(v.len());
    for &x in v {
        if x % 2 == 0 {
            out.push(x * 10);
        }
    }
    let first = out.first().copied();
    let tail = &v[1..];
    sink((first, tail)); // BREAK
    out
}

#[inline(never)]
fn join(parts: &[&str], sep: char) -> String {
    let mut s = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            s.push(sep);
        }
        s.push_str(p);
    }
    let len = s.len();
    sink(len); // BREAK
    s
}

fn main() {
    let m = words("the cat The dog the end");
    println!("{}", m.len());
    let v: Vec<u32> = (1..=8).collect();
    println!("{:?}", evens(&v));
    println!("{}", join(&["a", "bc", "def"], '-'));
}

/// The breakpoint lines' call: real code on the line in every build, and
/// opaque, so the value passed is computed but not forced into memory.
#[inline(never)]
fn sink<T>(x: T) {
    black_box(x);
}
