#![crate_type="lib"]
// pulldown-cmark's `scanners::unescape`: a byte loop with several
// `continue` arms (so several back edges), some of which call out and
// keep many values live across the loop.
#[inline(never)]
fn entity(b: &[u8]) -> (usize, Option<&'static str>) {
    if b.len() > 3 && b[1] == b'a' { (4, Some("&")) } else { (0, None) }
}
fn punct(c: u8) -> bool { c.is_ascii_punctuation() }
#[no_mangle]
pub fn unescape(input: &str, in_table: bool) -> String {
    let mut result = String::new();
    let mut mark = 0;
    let mut i = 0;
    let bytes = input.as_bytes();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_table && i + 2 < bytes.len() && bytes[i + 1] == b'\\' && bytes[i + 2] == b'|' => {
                result.push_str(&input[mark..i]);
                mark = i + 2;
                i += 3;
            }
            b'\\' if i + 1 < bytes.len() && punct(bytes[i + 1]) => {
                result.push_str(&input[mark..i]);
                mark = i + 1;
                i += 2;
            }
            b'&' => match entity(&bytes[i..]) {
                (n, Some(value)) => {
                    result.push_str(&input[mark..i]);
                    result.push_str(value);
                    i += n;
                    mark = i;
                }
                _ => i += 1,
            },
            b'\r' => {
                result.push_str(&input[mark..i]);
                i += 1;
                mark = i;
            }
            _ => i += 1,
        }
    }
    if mark == 0 { input.to_string() } else { result.push_str(&input[mark..]); result }
}
