// bcheck's loop versioning must only fold a bound check whose unsafe edge
// dies — a panic/trap path. `is_cold` alone is not enough: the not-found
// exit of a scan under a cold caller is cold but live, and folding the
// loop's own `iv < bound` control test drops the exit, turning the byte
// scan into an out-of-bounds read. In stage2 rustc this hit
// rustc_interface::parse_cfg's `s.contains('=')` — every string scanned
// "contained" '=' — and `--cfg a{` printed the wrong diagnostic.

#[cold]
#[inline(never)]
fn found() -> bool {
    true
}
#[cold]
#[inline(never)]
fn missing() -> bool {
    false
}

#[inline(never)]
fn scan(s: &[u8], k: u8) -> bool {
    let mut i = 0;
    loop {
        if i >= s.len() {
            return missing();
        }
        if s[i] == k {
            return found();
        }
        i += 1;
    }
}

fn main() {
    // '=' sits at index 4 inside the Vec's capacity but past `head`'s
    // len: a scan whose bound was folded finds it — the broken backend
    // reports true, LLVM says false.
    let mut v = Vec::with_capacity(64);
    v.extend_from_slice(b"noeq");
    v.push(b'=');
    let head = &v[..4];
    assert_eq!(scan(head, b'='), false);
    assert_eq!(scan(b"a=b", b'='), true);
    assert_eq!(scan(b"a{", b'='), false);
    assert_eq!(scan(b"", b'='), false);
    // Same shape through the library path (`position`/`contains`).
    assert!(!head.contains(&b'='));
    assert_eq!(head.iter().position(|&b| b == b'='), None);

    // The loop's own trip test must never fold even when its exit edge is
    // provably dead — dropping `i < n` makes the loop infinite, not just
    // removes a check. Under emulated EH a plain post-loop call site is
    // `call; load tls_flag; brif flag`: `dead_end` read the `call; flag`
    // part as a throw and admitted every return downstream, so the
    // inlined `reverse`'s trip test folded away and the swap walked off
    // the end of the buffer until a wild store faulted.
    let n = 200_000u64;
    let mut w: Vec<u64> = (0..n).collect();
    w.reverse();
    eprintln!("reversed");
    assert_eq!(w[0], n - 1);
    assert_eq!(w[n as usize - 1], 0);
    println!("bcheck ok");
}
