#![crate_type="lib"]
// lol_html drops short byte buffers everywhere: `drop_glue::<RawVec<u8>>`
// is "if cap != 0 { dealloc(ptr, cap, 1) }" once its layout arithmetic
// folds, and LLVM inlines it at every site.
#[inline(never)]
fn sink(_: &[u8]) {}
#[no_mangle]
pub fn use_and_drop(v: Vec<u8>) -> usize { sink(&v); v.len() }
#[no_mangle]
pub fn make(n: usize) -> Vec<u8> { Vec::with_capacity(n) }
#[no_mangle]
pub fn two(a: Vec<u8>, b: String) -> usize { sink(&a); sink(b.as_bytes()); a.len() + b.len() }
