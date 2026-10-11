// cparm folds a join param to a shared `iconst` when every incoming edge
// feeds it the same constant through carried (self-passthrough) param
// chains. The mid-chain params' uses must get a const materialized in
// *their* home block — one canonical const in the join block only
// dominates the join's own uses. `Vec::<u8>::into_boxed_slice`'s shrink
// path produced exactly that shape (a `1` tag carried to the realloc arm
// through sibling branch arms); the stale-block const fed edges on
// non-dominated arms, the egraph remapped them to the reserved value, and
// codegen aborted on `payload < (1 << 30)` in BlockArg::encode_as_value.

fn main() {
    // cap > len exercises the realloc-shrink path that carried the flag.
    let mut v: Vec<u8> = Vec::with_capacity(64);
    v.extend(0u8..40);
    let b = v.into_boxed_slice();
    assert_eq!(b.len(), 40);
    assert_eq!(b[0], 0);
    assert_eq!(b[39], 39);

    // Same generic body on other element shapes and exact-fit vectors.
    let w: Vec<u64> = (0..33).collect();
    let mut w = w;
    w.reserve(100);
    let wb = w.into_boxed_slice();
    assert_eq!(wb.len(), 33);
    assert_eq!(wb[32], 32);

    let s = String::with_capacity(128) + "hello";
    assert_eq!(s.into_boxed_str().len(), 5);

    // Nested Option/Result chains through match arms: more carried-const
    // param chains landing on a join from non-dominating siblings.
    fn carry(x: Option<Result<u8, u8>>) -> u8 {
        let t = x.and_then(|r| r.ok());
        match t {
            Some(v) if v > 10 => v - 10,
            Some(v) => v + 1,
            None => 1,
        }
    }
    assert_eq!(carry(Some(Ok(20))), 10);
    assert_eq!(carry(Some(Ok(3))), 4);
    assert_eq!(carry(None), 1);
    assert_eq!(carry(Some(Err(7))), 1);

    println!("cparm ok");
}
