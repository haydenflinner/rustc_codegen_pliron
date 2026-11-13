#![crate_type="lib"]
// lol_html's `Bytes::as_lowercase_string`: `[u8]::to_ascii_lowercase`
// (`iter().map(u8::to_ascii_lowercase).collect()`), mostly on tag and
// attribute names of a few bytes.
#[no_mangle]
pub fn lower(b: &[u8]) -> Vec<u8> { b.to_ascii_lowercase() }
