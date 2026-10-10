// Edge specialization must not thread an edge past a dispatch block whose
// params it would leave stale. `SplitWhitespace::next` keeps the cursor in a
// loop-carried block param rebound on the "not whitespace" edge; folding the
// `x <= 132` arm straight through used to skip that rebind, so the cursor
// never advanced and the iterator looped forever on any non-ASCII-tail word.
// The iteration count is large enough that the old bug reliably hung; the
// assertions keep it a real correctness check too.

fn main() {
    let s: String = "the quick brown fox jumps over the lazy dog\n".repeat(100_000);
    let mut n = 0usize;
    let mut last = "";
    for w in s.split_whitespace() {
        n += 1;
        last = w;
    }
    assert_eq!(n, 900_000);
    assert_eq!(last, "dog");

    // The fold must also leave chars()/bytes() filter plumbing correct.
    let a = s.chars().filter(|c| c.is_alphabetic()).count();
    assert_eq!(a, 3_500_000);
    let b = s.bytes().filter(|b| b.is_ascii_alphabetic()).count();
    assert_eq!(b, 3_500_000);

    // Whitespace-only and empty inputs stay correct.
    assert_eq!("   \t\n".split_whitespace().count(), 0);
    assert_eq!("".split_whitespace().count(), 0);
    // Multibyte UTF-8 between words: 'é' (2 bytes) and '\u{3000}' ideographic
    // space exercise the >132 page path the bitmask guard rejects.
    let t = "a\u{3000}b é  c";
    let toks: Vec<&str> = t.split_whitespace().collect();
    assert_eq!(toks, ["a", "b", "é", "c"]);

    println!("edgespec ok");
}
