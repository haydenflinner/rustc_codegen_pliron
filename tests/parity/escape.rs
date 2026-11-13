#![crate_type="lib"]
// pulldown-cmark's `escape_html_scalar`: `bytes[i..].iter().position(..)`
// inside a loop, the slice start moving with `i`.
const fn create_html_escape_table() -> [u8; 256] {
    let mut table = [0; 256];
    table[b'"' as usize] = 1;
    table[b'&' as usize] = 2;
    table[b'<' as usize] = 3;
    table[b'>' as usize] = 4;
    table
}
static HTML_ESCAPE_TABLE: [u8; 256] = create_html_escape_table();
static HTML_ESCAPES: [&'static str; 5] = ["", "&quot;", "&amp;", "&lt;", "&gt;"];
#[no_mangle]
pub fn escape(w: &mut String, s: &str) {
    let bytes = s.as_bytes();
    let mut mark = 0;
    let mut i = 0;
    while i < s.len() {
        match bytes[i..].iter().position(|&c| HTML_ESCAPE_TABLE[c as usize] != 0) {
            Some(pos) => { i += pos; }
            None => break,
        }
        let c = bytes[i];
        let escape = HTML_ESCAPE_TABLE[c as usize];
        let escape_seq = HTML_ESCAPES[escape as usize];
        w.push_str(&s[mark..i]);
        w.push_str(escape_seq);
        i += 1;
        mark = i;
    }
    w.push_str(&s[mark..])
}
