// A `&mut` temporary in a `static mut` initializer is interned as a
// nested static, a synthetic def with no `type_of`; debug info for it
// ICEd (regex-automata's integration tests, 2026-10-24).
static mut BUF: &mut [u8] = &mut [1, 2, 3];
static WORDS: &[&str] = &["a", "bc"];

fn main() {
    // SAFETY: single-threaded.
    let b = unsafe { &mut *std::ptr::addr_of_mut!(BUF) };
    b[0] += 4;
    println!("{} {}", b.iter().map(|&x| x as u32).sum::<u32>(), WORDS.len() + WORDS[1].len());
}
