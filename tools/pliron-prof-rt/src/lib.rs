//! `no_std` runtime for `rustc_codegen_pliron` PGO-instrumented
//! binaries (docs/pgo.md). Must not link `std` — this archive is pulled
//! into links that already carry a Rust std (rustc, proc-macro .so's)
//! and bundled std objects collide on `__rdl_*`/`__rust_*` symbols. It
//! must also not reference `core`/`compiler_builtins` members for the
//! same reason: every operation below is raw-pointer code with no
//! indexing (no bounds-check panics), no formatting, no allocation —
//! the archive's only object should end up needing just the libc
//! externs declared here.
//!
//! The per-CGU `.init_array` ctor calls [`__pliron_prof_register`] with
//! the CGU header; an `atexit` handler writes a raw dump to
//! `pliron-<pid>.prof` (or `$PLIRON_PROFILE_FILE`):
//!
//! ```text
//! "PLRAW01" (8 bytes)
//! per CGU: u64 payload_len | Hdr (32B) | recs (recs_len B) | ctrs (nctrs*8 B)
//! ```
//!
//! `pliron-prof merge` folds raw dumps (and `.prof` files) into a
//! profile — merging lives in the CLI so the runtime never allocates.

#![no_std]

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// `.prof` magic the headers carry ("PLPROF01" little-endian).
const MAGIC: u64 = u64::from_le_bytes(*b"PLPROF01");

/// Registered CGU headers. rustc links ~20k CGUs; overflow is
/// dropped, never fatal.
const MAX: usize = 1 << 18;
static mut HDRS: [usize; MAX] = [0; MAX];
static NHDRS: AtomicUsize = AtomicUsize::new(0);
static REGISTERED: AtomicBool = AtomicBool::new(false);

/// `{magic, ctrs, nctrs, recs_len}` then `recs` bytes inline —
/// see pliron-ir-opt/src/pgo.rs `instrument`.
#[repr(C)]
pub struct Hdr {
    pub magic: u64,
    pub ctrs: *const u64,
    pub nctrs: u64,
    pub recs_len: u64,
}

#[cfg(windows)]
unsafe extern "C" {
    // msvc spells the POSIX calls `_open`/`_write`/... — `_O_BINARY` in
    // OPEN_FLAGS keeps CRLF translation out of the binary dump.
    #[link_name = "_open"]
    fn open(path: *const u8, flags: i32, ...) -> i32;
    #[link_name = "_write"]
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
    #[link_name = "_close"]
    fn close(fd: i32) -> i32;
    #[link_name = "_getpid"]
    fn getpid() -> i32;
    fn getenv(name: *const u8) -> *const u8;
    fn atexit(cb: extern "C" fn()) -> i32;
    fn abort() -> !;
}

#[cfg(not(windows))]
unsafe extern "C" {
    // `open` is variadic (mode only matters with O_CREAT) — declaring the
    // mode as a fixed arg lands it in a register slot libc never reads on
    // targets where variadic args pass on the stack (aarch64 Darwin),
    // creating files with a garbage mode.
    fn open(path: *const u8, flags: i32, ...) -> i32;
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn getpid() -> i32;
    fn getenv(name: *const u8) -> *const u8;
    fn atexit(cb: extern "C" fn()) -> i32;
    fn abort() -> !;
}

// O_WRONLY | O_CREAT | O_TRUNC on Linux; _O_WRONLY|_O_CREAT|_O_TRUNC|
// _O_BINARY on msvc.
#[cfg(windows)]
const OPEN_FLAGS: i32 = 0x1 | 0x100 | 0x200 | 0x8000;
#[cfg(not(windows))]
const OPEN_FLAGS: i32 = 0o1 | 0o100 | 0o1000;

unsafe fn w(fd: i32, mut p: *const u8, mut n: usize) {
    while n > 0 {
        let k = write(fd, p, n);
        if k <= 0 {
            return;
        }
        p = p.add(k as usize);
        n -= k as usize;
    }
}

unsafe fn puts(fd: i32, s: &[u8]) {
    w(fd, s.as_ptr(), s.len());
}

extern "C" fn flush() {
    let n = NHDRS.load(Ordering::Relaxed).min(MAX);
    if n == 0 {
        return;
    }
    let mut name = [0u8; 128];
    let env = unsafe { getenv(b"PLIRON_PROFILE_FILE\0".as_ptr()) };
    let name = if env.is_null() {
        // "pliron-<pid>.prof"
        let mut i = 0usize;
        for &b in b"pliron-".iter() {
            unsafe { *name.as_mut_ptr().add(i) = b };
            i += 1;
        }
        let mut v = unsafe { getpid() } as u64;
        let mut d = [0u8; 20];
        let mut nd = 0usize;
        loop {
            unsafe { *d.as_mut_ptr().add(nd) = b'0' + (v % 10) as u8 };
            v /= 10;
            nd += 1;
            if v == 0 {
                break;
            }
        }
        while nd > 0 {
            nd -= 1;
            unsafe { *name.as_mut_ptr().add(i) = *d.as_ptr().add(nd) };
            i += 1;
        }
        for &b in b".prof".iter() {
            unsafe { *name.as_mut_ptr().add(i) = b };
            i += 1;
        }
        name.as_ptr()
    } else {
        env
    };
    unsafe {
        let fd = open(name, OPEN_FLAGS, 0o666);
        if fd < 0 {
            return;
        }
        puts(fd, b"PLRAW01\0");
        let hdrs = core::ptr::addr_of!(HDRS) as *const usize;
        for i in 0..n {
            let h = *hdrs.add(i) as *const Hdr;
            if (*h).magic != MAGIC {
                continue;
            }
            let recs_len = (*h).recs_len as usize;
            let nctrs = (*h).nctrs as usize;
            let total = core::mem::size_of::<Hdr>() as u64
                + recs_len as u64
                + nctrs as u64 * 8;
            w(fd, &total as *const u64 as *const u8, 8);
            w(fd, h as *const u8, core::mem::size_of::<Hdr>());
            w(fd, (h as *const u8).add(core::mem::size_of::<Hdr>()), recs_len);
            w(fd, (*h).ctrs as *const u8, nctrs * 8);
        }
        close(fd);
    }
}

/// Called from each instrumented CGU's `.init_array` ctor.
#[no_mangle]
pub extern "C" fn __pliron_prof_register(hdr: *const Hdr) {
    if !REGISTERED.swap(true, Ordering::Relaxed) {
        unsafe {
            atexit(flush);
        }
    }
    let i = NHDRS.fetch_add(1, Ordering::Relaxed);
    if i < MAX {
        unsafe {
            *(core::ptr::addr_of_mut!(HDRS) as *mut usize).add(i) = hdr as usize;
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { abort() }
}
