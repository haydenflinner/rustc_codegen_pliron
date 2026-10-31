// r[verify iropt.ipo.ret]
// Return-value IPO attributes (PLIRON_NNRET / PLIRON_RETARG).
//
// nnret: a body whose returns are all provably non-null marks its callers'
// results non-null for jumpthread's null-test folds. retarg: a body that
// returns exactly parameter k lets the call result fold to the k-th operand.
// Both must fire where claimed (checked via PLIRON_STATS in test.sh) and
// must NOT fire where the proof fails — a wrongly inferred attribute flips
// these asserts.

static S: u8 = 9;
static INNER: u8 = 7;
static OUTER: &u8 = &INNER;

// --- nnret: should be inferred ---

#[inline(never)]
fn nn_static() -> *const u8 {
    &raw const S
}

#[inline(never)]
fn nn_load() -> *const u8 {
    // Loading the `&u8` static carries `!nonnull` (references can't be
    // null), so the loaded pointer is provably non-null.
    OUTER as *const u8
}

#[inline(never)]
fn nn_wrap() -> *const u8 {
    // `return call_result` stays a direct value use: transitive nnret.
    nn_static()
}

#[inline(never)]
fn nn_chain(s: bool) -> *const u8 {
    // Missed on purpose: rustc routes the if/else through a stack temp
    // (store + load), which a body scan can't forward soundly. The result
    // is still non-null at runtime.
    if s {
        return nn_static();
    }
    nn_load()
}

// --- nnret: must NOT be inferred ---

#[inline(never)]
fn nn_arg(p: *const u8) -> *const u8 {
    p // also `returned(0)`, but the param isn't provably non-null
}

#[inline(never)]
fn nn_or_null(s: bool) -> *const u8 {
    if s { &raw const S } else { core::ptr::null() }
}

#[inline(never)]
fn nn_null() -> *const u8 {
    core::ptr::null()
}

// --- retarg: should be inferred ---

#[inline(never)]
fn id_i32(x: i32) -> i32 {
    x
}

#[inline(never)]
fn wrap_i32(x: i32) -> i32 {
    id_i32(x) // returns its own arg transitively through a `returned(0)` callee
}

#[inline(never)]
fn sel_same(c: bool, x: i32) -> i32 {
    // Both select arms are the same param: provably `returned(1)`.
    if c { x } else { x }
}

// --- retarg: must NOT be inferred ---

#[inline(never)]
fn plus1(x: i32) -> i32 {
    x + 1
}

#[inline(never)]
fn sel_diff(c: bool, a: i32, b: i32) -> i32 {
    if c { a } else { b }
}

fn main() {
    // A runtime (non-constant) argument keeps `spec` from constant-folding
    // the callee params away, so the transitive `returned` chain survives.
    let rt = std::env::args().count() as i32;

    // Inference fires: results are genuinely non-null / identity.
    assert!(!nn_static().is_null());
    assert!(!nn_load().is_null());
    assert!(!nn_wrap().is_null());
    assert!(!nn_chain(true).is_null());
    assert!(!nn_chain(false).is_null());
    assert_eq!(id_i32(41), 41);
    assert_eq!(wrap_i32(rt), rt);
    assert_eq!(sel_same(rt > 0, rt), rt);

    // Inference must NOT fire: these results are null / changed, and a lying
    // attribute would fold the tests the wrong way.
    assert!(nn_arg(core::ptr::null()).is_null());
    assert!(nn_or_null(false).is_null());
    assert!(nn_null().is_null());
    assert!(!nn_arg(&raw const S).is_null());
    assert_eq!(plus1(1), 2);
    assert_eq!(sel_diff(true, 5, 9), 5);
    assert_eq!(sel_diff(false, 5, 9), 9);
    println!("retattr ok");
}
