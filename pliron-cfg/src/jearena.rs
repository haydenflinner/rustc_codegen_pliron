//! `PLIRON_JEARENA` (default on; `=0` off): backend-owned jemalloc arenas
//! with immediate purging.
//!
//! rustc links jemalloc (exported as `_rjem_mallctl`), whose default
//! arenas keep freed pages dirty for 10 s (`dirty_decay_ms`) and then
//! muzzy for another 10 s. A pliron compile frees most of what it
//! allocates within the same second (DEADFREE'd inliner copies, pass
//! scratch, each CGU's whole `Context`), and every thread has its own
//! arena, so a page one CGU worker freed is not reused by another and
//! stays resident until the process exits: on syn, jemalloc decay is
//! ~30% of the peak (docs/history/2026-10-27-memory2.md).
//!
//! With the flag, the first backend thread to call [`bind_thread`]
//! creates `PLIRON_JEARENA_N` (default 4) arenas with dirty and muzzy
//! decay set to `PLIRON_JEARENA_DECAY` ms (default 0: purge at once),
//! and each backend thread (CGU workers, the phase-(B) fn pool) binds to
//! one of them round-robin. rustc's own threads keep their arenas and
//! their decay. Placement never reaches output (docs/determinism.md R6).
//!
//! A no-op when the host has no `_rjem_mallctl` (a rustc built without
//! jemalloc, Windows) or when a `mallctl` call fails.

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

type Mallctl =
    unsafe extern "C" fn(*const c_char, *mut c_void, *mut usize, *mut c_void, usize) -> c_int;

#[cfg(unix)]
fn find_mallctl() -> Option<Mallctl> {
    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
    // RTLD_DEFAULT: the global symbol scope (rustc's executable included).
    #[cfg(target_os = "macos")]
    let default = -2isize as *mut c_void;
    #[cfg(not(target_os = "macos"))]
    let default = std::ptr::null_mut();
    // SAFETY: dlsym with a NUL-terminated name; a non-null result for
    // this name is jemalloc's `mallctl`, whose signature `Mallctl` is.
    let p = unsafe { dlsym(default, c"_rjem_mallctl".as_ptr()) };
    (!p.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, Mallctl>(p) })
}

#[cfg(not(unix))]
fn find_mallctl() -> Option<Mallctl> {
    None
}

fn mallctl() -> Option<Mallctl> {
    static F: OnceLock<Option<Mallctl>> = OnceLock::new();
    *F.get_or_init(find_mallctl)
}

/// The backend's arenas: empty when the flag is off or setup failed.
fn arenas() -> &'static [u32] {
    static A: OnceLock<Vec<u32>> = OnceLock::new();
    A.get_or_init(|| {
        if !crate::env_is("PLIRON_JEARENA", "1") {
            return Vec::new();
        }
        let Some(ctl) = mallctl() else {
            return Vec::new();
        };
        let n = crate::env_get("PLIRON_JEARENA_N")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(4)
            .clamp(1, 64);
        let decay: isize = crate::env_get("PLIRON_JEARENA_DECAY")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let mut ind: u32 = 0;
            let mut len = std::mem::size_of::<u32>();
            // SAFETY: `arenas.create` writes an `unsigned` arena index;
            // a null new value selects the default extent hooks.
            let r = unsafe {
                ctl(
                    c"arenas.create".as_ptr(),
                    (&raw mut ind).cast(),
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if r != 0 {
                break;
            }
            for k in ["dirty_decay_ms", "muzzy_decay_ms"] {
                let name = format!("arena.{ind}.{k}\0");
                let mut d = decay;
                // SAFETY: both knobs take an `ssize_t`.
                unsafe {
                    ctl(
                        name.as_ptr().cast(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        (&raw mut d).cast(),
                        std::mem::size_of::<isize>(),
                    )
                };
            }
            out.push(ind);
        }
        out
    })
}

thread_local! {
    static BOUND: Cell<bool> = const { Cell::new(false) };
}

/// Bind the calling backend thread to one of the backend arenas (once per
/// thread; cheap to call on every job). No-op with the flag off.
pub fn bind_thread() {
    if BOUND.with(|b| b.replace(true)) {
        return;
    }
    let a = arenas();
    if a.is_empty() {
        return;
    }
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let mut ind = a[NEXT.fetch_add(1, Ordering::Relaxed) % a.len()];
    let Some(ctl) = mallctl() else {
        return;
    };
    // SAFETY: `thread.arena` takes an `unsigned` arena index.
    unsafe {
        ctl(
            c"thread.arena".as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            (&raw mut ind).cast(),
            std::mem::size_of::<u32>(),
        )
    };
}

/// The calling thread's current arena index, for tests and diagnostics.
pub fn thread_arena() -> Option<u32> {
    let ctl = mallctl()?;
    let mut ind: u32 = 0;
    let mut len = std::mem::size_of::<u32>();
    // SAFETY: reading `thread.arena` writes an `unsigned`.
    let r = unsafe {
        ctl(
            c"thread.arena".as_ptr(),
            (&raw mut ind).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (r == 0).then_some(ind)
}
