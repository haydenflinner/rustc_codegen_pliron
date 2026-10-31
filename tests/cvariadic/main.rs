// C-variadic definitions on the nightly-2026-10-22 `VaList` (`ap: ...`,
// `next_arg`, `clone`). `va_start` wrote an x86-64 SysV `__va_list_tag`
// (24 bytes) into every target's VaList; on aarch64-apple (and Windows,
// wasm, ...) VaList is one pointer, so it smashed the frame
// (rust-lang/rust tests/ui/c-variadic/copy.rs SIGSEGV'd). Forwarding to
// libc's vsnprintf checks the list matches the platform va_list.
use std::ffi::{c_char, c_int, VaList};

unsafe extern "C" {
    fn vsnprintf(buf: *mut c_char, n: usize, fmt: *const c_char, ap: VaList) -> c_int;
}

unsafe extern "C" fn none(mut ap: ...) -> i32 {
    unsafe { ap.next_arg::<i32>() + 10 * ap.next_arg::<i32>() }
}

unsafe extern "C" fn copy(mut ap1: ...) {
    let mut ap2 = ap1.clone();
    unsafe {
        assert_eq!(ap1.next_arg::<i32>(), 1);
        assert_eq!(ap2.next_arg::<i32>(), 1);
        assert_eq!(ap2.next_arg::<i32>(), 2);
        assert_eq!(ap1.next_arg::<i32>(), 2);
        drop(ap1);
        assert_eq!(ap2.next_arg::<i32>(), 3);
    }
}

unsafe fn vsum(n: u32, mut ap: VaList<'_>) -> f64 {
    let mut s = 0.0;
    for i in 0..n {
        s += unsafe {
            match i % 3 {
                0 => ap.next_arg::<i32>() as f64,
                1 => ap.next_arg::<f64>(),
                _ => ap.next_arg::<i64>() as f64,
            }
        };
    }
    s
}

unsafe extern "C" fn mixed(n: u32, ap: ...) -> f64 {
    unsafe { vsum(n, ap) }
}

unsafe extern "C" fn fmt(out: *mut c_char, len: usize, f: *const c_char, ap: ...) -> c_int {
    unsafe { vsnprintf(out, len, f, ap) }
}

// r[verify front.abi.variadic]
fn main() {
    unsafe {
        assert_eq!(none(2, 3), 32);
        copy(1, 2, 3);
        let s = mixed(
            12, 1i32, 0.5f64, 3i64, 4i32, 0.25f64, 6i64, 7i32, 1.5f64, 9i64, 10i32, 2.0f64, 12i64,
        );
        assert_eq!(s, 1.0 + 0.5 + 3.0 + 4.0 + 0.25 + 6.0 + 7.0 + 1.5 + 9.0 + 10.0 + 2.0 + 12.0);
        let mut buf = [0 as c_char; 64];
        let n = fmt(
            buf.as_mut_ptr(),
            buf.len(),
            c"%d %s %.2f %ld %d".as_ptr(),
            42i32,
            c"hi".as_ptr(),
            2.5f64,
            -7i64,
            9i32,
        );
        let got = std::ffi::CStr::from_ptr(buf.as_ptr()).to_str().unwrap();
        assert_eq!((n, got), (15, "42 hi 2.50 -7 9"));
    }
    println!("ok");
}
