#![crate_type="lib"]
// encoding_rs `Encoding::for_bom`: prefix tests against two- and
// three-byte literals.
#[no_mangle]
pub fn for_bom(buffer: &[u8]) -> usize {
    if buffer.starts_with(b"\xEF\xBB\xBF") {
        3
    } else if buffer.starts_with(b"\xFF\xFE") {
        2
    } else if buffer.starts_with(b"\xFE\xFF") {
        1
    } else {
        0
    }
}
