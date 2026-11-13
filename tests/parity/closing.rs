#![crate_type="lib"]
fn scan_while<F: FnMut(u8) -> bool>(data: &[u8], mut f: F) -> usize {
    data.iter().take_while(|&&c| f(c)).count()
}
fn scan_ch_repeat(data: &[u8], c: u8) -> usize { scan_while(data, |x| x == c) }
fn scan_eol(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() { return Some(0); }
    match bytes[0] { b'\n' => Some(1), b'\r' => Some(if bytes.get(1) == Some(&b'\n') { 2 } else { 1 }), _ => None }
}
#[no_mangle]
pub fn closing(bytes: &[u8], fence_char: u8, n_fence_char: usize) -> Option<usize> {
    if bytes.is_empty() { return Some(0); }
    let mut i = 0;
    let found = scan_ch_repeat(&bytes[i..], fence_char);
    if found < n_fence_char { return None; }
    i += found;
    let sp = scan_ch_repeat(&bytes[i..], b' ');
    i += sp;
    scan_eol(&bytes[i..]).map(|_| i)
}
