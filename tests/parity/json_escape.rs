#![crate_type="lib"]
// serde_json's `format_escaped_str_contents` (ser.rs:2091): a byte walk
// over a slice that restarts after each escape.
static ESCAPE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 32 { t[i] = b'u'; i += 1; }
    t[b'"' as usize] = b'"';
    t[b'\\' as usize] = b'\\';
    t
};
#[no_mangle]
pub fn escaped(writer: &mut Vec<u8>, value: &str) {
    let mut bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let (string_run, rest) = bytes.split_at(i);
        let (&byte, rest) = rest.split_first().unwrap();
        let escape = ESCAPE[byte as usize];
        i += 1;
        if escape == 0 {
            continue;
        }
        bytes = rest;
        i = 0;
        if !string_run.is_empty() {
            writer.extend_from_slice(string_run);
        }
        writer.extend_from_slice(&[b'\\', escape]);
    }
    if bytes.is_empty() {
        return;
    }
    writer.extend_from_slice(bytes);
}
