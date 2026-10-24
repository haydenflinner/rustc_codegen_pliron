// String parsing: chars, byte offsets, &str slices.
use std::hint::black_box;

#[inline(never)]
fn tokenize(src: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in src.char_indices() {
        let is_word = c.is_alphanumeric() || c == '_';
        match (start, is_word) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                let tok = &src[s..i];
                out.push((s, tok));
                if out.len() == 3 {
                    sink(tok); // BREAK
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, &src[s..]));
    }
    out
}

#[inline(never)]
fn checksum(s: &str) -> u32 {
    let bytes = s.as_bytes();
    let mut h = 2166136261u32;
    for &b in bytes {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    let upper = s.to_uppercase();
    sink(&upper); // BREAK
    h
}

fn main() {
    let toks = tokenize("let x_1 = foo(bar, 42);");
    println!("{}", toks.len());
    println!("{}", checksum("hello world"));
}

/// The breakpoint lines' call: real code on the line in every build, and
/// opaque, so the value passed is computed but not forced into memory.
#[inline(never)]
fn sink<T>(x: T) {
    black_box(x);
}
