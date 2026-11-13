#![crate_type="lib"]
fn scan_while<F: FnMut(u8) -> bool>(data: &[u8], mut f: F) -> usize {
    data.iter().take_while(|&&c| f(c)).count()
}
fn is_ws_no_nl(c: u8) -> bool { c == b'\t' || c == 0x0b || c == 0x0c || c == b' ' }
#[no_mangle]
pub fn scan_ws(data: &[u8]) -> usize { scan_while(data, is_ws_no_nl) }
#[no_mangle]
pub fn scan_ch(data: &[u8], c: u8) -> usize { scan_while(data, |x| x == c) }
