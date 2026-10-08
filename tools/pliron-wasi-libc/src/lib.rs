//! The slice of wasi-libc that std's `wasm32-wasip1` backend links against,
//! written in Rust on top of the `wasip1` syscall bindings. Single-threaded.
#![no_std]
#![allow(clippy::missing_safety_doc, static_mut_refs, non_upper_case_globals)]

use core::ffi::{CStr, c_char, c_int, c_long, c_void};
use core::ptr::{copy_nonoverlapping, null_mut};
use libc::{dirent, iovec, mode_t, off_t, size_t, ssize_t, stat as Stat, timespec};
use wasip1 as w;

#[unsafe(no_mangle)]
pub static mut errno: c_int = 0;

// This #[panic_handler] also defines `rust_begin_unwind`; when the binary's
// real panic_impl comes from std/panic_abort, pliron-wasm-ld keeps the
// earliest input object's definition (std precedes libc.a on the link line).
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

fn fail(e: w::Errno) -> c_int {
    unsafe { errno = e.raw() as c_int };
    -1
}
fn ret<T>(r: Result<T, w::Errno>) -> c_int {
    match r {
        Ok(_) => 0,
        Err(e) => fail(e),
    }
}
fn retn(r: Result<usize, w::Errno>) -> ssize_t {
    match r {
        Ok(n) => n as ssize_t,
        Err(e) => fail(e) as ssize_t,
    }
}
unsafe fn cstr<'a>(p: *const c_char) -> &'a str {
    unsafe { core::str::from_utf8_unchecked(CStr::from_ptr(p).to_bytes()) }
}

// ---- entry ----

unsafe extern "C" {
    fn __wasm_call_ctors();
    fn __main_void() -> c_int;
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    unsafe { __wasm_call_ctors() };
    let r = unsafe { __main_void() };
    if r != 0 {
        unsafe { w::proc_exit(r as u32) }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn exit(code: c_int) -> ! {
    unsafe { w::proc_exit(code as u32) };
    core::arch::wasm32::unreachable()
}

#[unsafe(no_mangle)]
pub extern "C" fn abort() -> ! {
    core::arch::wasm32::unreachable()
}

// ---- memory ----

static mut HEAP: dlmalloc::Dlmalloc = dlmalloc::Dlmalloc::new();
const HDR: usize = 16;

/// Each block is preceded by (base, total size, align) so free/realloc need no size.
unsafe fn alloc(size: usize, align: usize, zero: bool) -> *mut c_void {
    let align = align.max(HDR);
    let total = match size.checked_add(align) {
        Some(t) => t,
        None => return null_mut(),
    };
    let base = unsafe {
        if zero {
            HEAP.calloc(total, align)
        } else {
            HEAP.malloc(total, align)
        }
    };
    if base.is_null() {
        unsafe { errno = libc::ENOMEM };
        return null_mut();
    }
    unsafe {
        let user = base.add(align);
        let h = user.cast::<usize>().sub(3);
        h.write(base as usize);
        h.add(1).write(total);
        h.add(2).write(size);
        user.cast()
    }
}
unsafe fn header(p: *mut c_void) -> (*mut u8, usize, usize) {
    unsafe {
        let h = p.cast::<usize>().sub(3);
        (h.read() as *mut u8, h.add(1).read(), h.add(2).read())
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn malloc(n: size_t) -> *mut c_void {
    unsafe { alloc(n, HDR, false) }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn calloc(a: size_t, b: size_t) -> *mut c_void {
    match a.checked_mul(b) {
        Some(n) => unsafe { alloc(n, HDR, true) },
        None => null_mut(),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_memalign(out: *mut *mut c_void, align: size_t, n: size_t) -> c_int {
    if !align.is_power_of_two() || align % size_of::<usize>() != 0 {
        return libc::EINVAL;
    }
    let p = unsafe { alloc(n, align, false) };
    if p.is_null() {
        return libc::ENOMEM;
    }
    unsafe { *out = p };
    0
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn free(p: *mut c_void) {
    if !p.is_null() {
        unsafe {
            let (base, total, _) = header(p);
            HEAP.free(base, total, p as usize - base as usize);
        }
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn realloc(p: *mut c_void, n: size_t) -> *mut c_void {
    if p.is_null() {
        return unsafe { malloc(n) };
    }
    unsafe {
        let (base, _, old) = header(p);
        let align = p as usize - base as usize;
        let q = alloc(n, align, false);
        if !q.is_null() {
            copy_nonoverlapping(p.cast::<u8>(), q.cast::<u8>(), old.min(n));
            free(p);
        }
        q
    }
}

// ---- strings ----

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strlen(s: *const c_char) -> size_t {
    let mut n = 0;
    while unsafe { *s.add(n) } != 0 {
        n += 1;
    }
    n
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: size_t) -> c_int {
    let (a, b) = (a.cast::<u8>(), b.cast::<u8>());
    for i in 0..n {
        let (x, y) = unsafe { (*a.add(i), *b.add(i)) };
        if x != y {
            return x as c_int - y as c_int;
        }
    }
    0
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strerror_r(e: c_int, buf: *mut c_char, len: size_t) -> c_int {
    let mut tmp = [0u8; 24];
    let msg = b"wasi errno ";
    tmp[..msg.len()].copy_from_slice(msg);
    let mut i = msg.len();
    let mut digits = [0u8; 10];
    let (mut v, mut d) = (e.unsigned_abs(), 0);
    loop {
        digits[d] = b'0' + (v % 10) as u8;
        d += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    while d > 0 {
        d -= 1;
        tmp[i] = digits[d];
        i += 1;
    }
    if len == 0 {
        return libc::ERANGE;
    }
    let n = i.min(len - 1);
    unsafe {
        copy_nonoverlapping(tmp.as_ptr(), buf.cast(), n);
        *buf.add(n) = 0;
    }
    0
}

// ---- environment ----

static mut ENVIRON: *mut *mut c_char = null_mut();
static mut ENV_LEN: usize = 0;

unsafe fn env_init() {
    unsafe {
        if !ENVIRON.is_null() {
            return;
        }
        let (count, size) = w::environ_sizes_get().unwrap_or((0, 0));
        let buf = malloc(size.max(1)).cast::<u8>();
        ENVIRON = malloc((count + 1) * size_of::<usize>()).cast();
        if count > 0 {
            let _ = w::environ_get(ENVIRON.cast(), buf);
        }
        *ENVIRON.add(count) = null_mut();
        ENV_LEN = count;
    }
}
unsafe fn env_find(k: &[u8]) -> Option<usize> {
    unsafe {
        env_init();
        (0..ENV_LEN).find(|&i| {
            let e = CStr::from_ptr(*ENVIRON.add(i)).to_bytes();
            e.len() > k.len() && e[k.len()] == b'=' && &e[..k.len()] == k
        })
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wasilibc_get_environ() -> *mut *mut c_char {
    unsafe {
        env_init();
        ENVIRON
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getenv(k: *const c_char) -> *mut c_char {
    unsafe {
        let k = CStr::from_ptr(k).to_bytes();
        match env_find(k) {
            Some(i) => (*ENVIRON.add(i)).add(k.len() + 1),
            None => null_mut(),
        }
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setenv(k: *const c_char, v: *const c_char, overwrite: c_int) -> c_int {
    unsafe {
        let (kb, vb) = (CStr::from_ptr(k).to_bytes(), CStr::from_ptr(v).to_bytes());
        let found = env_find(kb);
        if found.is_some() && overwrite == 0 {
            return 0;
        }
        let s = malloc(kb.len() + vb.len() + 2).cast::<u8>();
        copy_nonoverlapping(kb.as_ptr(), s, kb.len());
        *s.add(kb.len()) = b'=';
        copy_nonoverlapping(vb.as_ptr(), s.add(kb.len() + 1), vb.len());
        *s.add(kb.len() + 1 + vb.len()) = 0;
        match found {
            Some(i) => *ENVIRON.add(i) = s.cast(),
            None => {
                ENVIRON = realloc(ENVIRON.cast(), (ENV_LEN + 2) * size_of::<usize>()).cast();
                *ENVIRON.add(ENV_LEN) = s.cast();
                ENV_LEN += 1;
                *ENVIRON.add(ENV_LEN) = null_mut();
            }
        }
        0
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unsetenv(k: *const c_char) -> c_int {
    unsafe {
        if let Some(i) = env_find(CStr::from_ptr(k).to_bytes()) {
            ENV_LEN -= 1;
            *ENVIRON.add(i) = *ENVIRON.add(ENV_LEN);
            *ENVIRON.add(ENV_LEN) = null_mut();
        }
        0
    }
}

// ---- paths: preopens + cwd ----

const MAXP: usize = 1024;
struct Path {
    buf: [u8; MAXP],
    len: usize,
}
impl Path {
    const fn new() -> Self {
        Path {
            buf: [0; MAXP],
            len: 0,
        }
    }
    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
    fn push(&mut self, b: u8) -> bool {
        if self.len == MAXP {
            return false;
        }
        self.buf[self.len] = b;
        self.len += 1;
        true
    }
    /// Appends `p` to this absolute path, resolving `.` and `..`.
    fn join(&mut self, p: &[u8]) -> bool {
        if p.first() == Some(&b'/') {
            self.len = 0;
        }
        for c in p.split(|&b| b == b'/') {
            match c {
                b"" | b"." => {}
                b".." => {
                    while self.len > 0 && self.buf[self.len - 1] != b'/' {
                        self.len -= 1;
                    }
                    self.len = self.len.saturating_sub(1);
                }
                c => {
                    if !self.push(b'/') || !c.iter().all(|&b| self.push(b)) {
                        return false;
                    }
                }
            }
        }
        true
    }
}

struct Preopen {
    fd: u32,
    path: Path,
}
static mut PREOPENS: [Option<Preopen>; 16] = [const { None }; 16];
static mut PREOPENS_INIT: bool = false;
static mut CWD: Path = Path::new();

unsafe fn preopens() -> &'static [Option<Preopen>; 16] {
    unsafe {
        if !PREOPENS_INIT {
            PREOPENS_INIT = true;
            let mut n = 0;
            for fd in 3..64u32 {
                let Ok(ps) = w::fd_prestat_get(fd) else { break };
                if ps.tag != w::PREOPENTYPE_DIR.raw() || n == PREOPENS.len() {
                    continue;
                }
                let mut name = [0u8; MAXP];
                let len = ps.u.dir.pr_name_len.min(MAXP);
                if w::fd_prestat_dir_name(fd, name.as_mut_ptr(), len).is_err() {
                    continue;
                }
                let mut path = Path::new();
                path.join(&name[..len]);
                PREOPENS[n] = Some(Preopen { fd, path });
                n += 1;
            }
        }
        &PREOPENS
    }
}

/// Resolves `p` (relative to `dirfd`, or to the cwd for AT_FDCWD) to a
/// directory fd and a path relative to it, into `out`.
unsafe fn resolve<'a>(
    dirfd: c_int,
    p: *const c_char,
    out: &'a mut Path,
) -> Result<(u32, &'a str), w::Errno> {
    unsafe {
        let p = CStr::from_ptr(p).to_bytes();
        if dirfd != libc::AT_FDCWD {
            out.len = 0;
            p.iter().all(|&b| out.push(b));
            return Ok((dirfd as u32, cstr_bytes(out.bytes())));
        }
        out.len = 0;
        out.join(CWD.bytes());
        if !out.join(p) {
            return Err(w::ERRNO_NAMETOOLONG);
        }
        let abs = out.bytes();
        let mut best: Option<(u32, usize)> = None;
        for po in preopens().iter().flatten() {
            let pre = po.path.bytes();
            let m = abs.starts_with(pre)
                && (abs.len() == pre.len() || pre.is_empty() || abs[pre.len()] == b'/');
            if m && best.is_none_or(|(_, l)| pre.len() > l) {
                best = Some((po.fd, pre.len()));
            }
        }
        let (fd, l) = best.ok_or(w::ERRNO_NOENT)?;
        let rest = &abs[l..];
        let rest = rest.strip_prefix(b"/").unwrap_or(rest);
        Ok((
            fd,
            if rest.is_empty() {
                "."
            } else {
                cstr_bytes(rest)
            },
        ))
    }
}
fn cstr_bytes(b: &[u8]) -> &str {
    unsafe { core::str::from_utf8_unchecked(b) }
}

macro_rules! at {
    ($dirfd:expr, $p:expr, |$fd:ident, $rel:ident| $body:expr) => {{
        let mut buf = Path::new();
        match unsafe { resolve($dirfd, $p, &mut buf) } {
            Ok(($fd, $rel)) => ret(unsafe { $body }),
            Err(e) => fail(e),
        }
    }};
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn chdir(p: *const c_char) -> c_int {
    let mut st: Stat = unsafe { core::mem::zeroed() };
    if unsafe { stat(p, &mut st) } != 0 {
        return -1;
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return fail(w::ERRNO_NOTDIR);
    }
    unsafe { CWD.join(CStr::from_ptr(p).to_bytes()) };
    0
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getcwd(buf: *mut c_char, size: size_t) -> *mut c_char {
    unsafe {
        let c = if CWD.len == 0 {
            b"/" as &[u8]
        } else {
            CWD.bytes()
        };
        if c.len() + 1 > size {
            errno = libc::ERANGE;
            return null_mut();
        }
        copy_nonoverlapping(c.as_ptr(), buf.cast(), c.len());
        *buf.add(c.len()) = 0;
        buf
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn realpath(p: *const c_char, out: *mut c_char) -> *mut c_char {
    unsafe {
        let mut abs = Path::new();
        abs.join(CWD.bytes());
        if !abs.join(CStr::from_ptr(p).to_bytes()) {
            errno = libc::ENAMETOOLONG;
            return null_mut();
        }
        let b = if abs.len == 0 {
            b"/" as &[u8]
        } else {
            abs.bytes()
        };
        let out = if out.is_null() {
            malloc(b.len() + 1).cast()
        } else {
            out
        };
        copy_nonoverlapping(b.as_ptr(), out.cast(), b.len());
        *out.add(b.len()) = 0;
        out
    }
}

// ---- files ----

const R_READ: u64 = 1 << 1 | 1 << 14; // fd_read, fd_readdir
const R_WRITE: u64 = 1 << 6 | 1 << 0 | 1 << 8 | 1 << 22; // fd_write, datasync, allocate, set_size

#[unsafe(no_mangle)]
/// Variadic in C: `_va` points at the optional mode.
pub unsafe extern "C" fn openat(
    dirfd: c_int,
    p: *const c_char,
    flags: c_int,
    _va: *const c_int,
) -> c_int {
    let mut buf = Path::new();
    let (fd, rel) = match unsafe { resolve(dirfd, p, &mut buf) } {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let inh = match unsafe { w::fd_fdstat_get(fd) } {
        Ok(s) => s.fs_rights_inheriting,
        Err(e) => return fail(e),
    };
    let mut rights = inh;
    if flags & libc::O_RDONLY == 0 {
        rights &= !R_READ;
    }
    if flags & libc::O_WRONLY == 0 {
        rights &= !R_WRITE;
    }
    let lookup = if flags & libc::O_NOFOLLOW != 0 {
        0
    } else {
        w::LOOKUPFLAGS_SYMLINK_FOLLOW
    };
    let oflags = ((flags >> 12) & 0xfff) as u16;
    let fdflags = (flags & 0xfff) as u16;
    match unsafe { w::path_open(fd, lookup, rel, oflags, rights, inh, fdflags) } {
        Ok(fd) => fd as c_int,
        Err(e) => fail(e),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn open(p: *const c_char, flags: c_int, va: *const c_int) -> c_int {
    unsafe { openat(libc::AT_FDCWD, p, flags, va) }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    ret(unsafe { w::fd_close(fd as u32) })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn read(fd: c_int, b: *mut c_void, n: size_t) -> ssize_t {
    retn(unsafe {
        w::fd_read(
            fd as u32,
            &[w::Iovec {
                buf: b.cast(),
                buf_len: n,
            }],
        )
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn write(fd: c_int, b: *const c_void, n: size_t) -> ssize_t {
    retn(unsafe {
        w::fd_write(
            fd as u32,
            &[w::Ciovec {
                buf: b.cast(),
                buf_len: n,
            }],
        )
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn readv(fd: c_int, iov: *const iovec, n: c_int) -> ssize_t {
    retn(unsafe {
        w::fd_read(
            fd as u32,
            core::slice::from_raw_parts(iov.cast(), n as usize),
        )
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn writev(fd: c_int, iov: *const iovec, n: c_int) -> ssize_t {
    retn(unsafe {
        w::fd_write(
            fd as u32,
            core::slice::from_raw_parts(iov.cast(), n as usize),
        )
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pread(fd: c_int, b: *mut c_void, n: size_t, off: off_t) -> ssize_t {
    retn(unsafe {
        w::fd_pread(
            fd as u32,
            &[w::Iovec {
                buf: b.cast(),
                buf_len: n,
            }],
            off as u64,
        )
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwrite(fd: c_int, b: *const c_void, n: size_t, off: off_t) -> ssize_t {
    retn(unsafe {
        w::fd_pwrite(
            fd as u32,
            &[w::Ciovec {
                buf: b.cast(),
                buf_len: n,
            }],
            off as u64,
        )
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lseek(fd: c_int, off: off_t, whence: c_int) -> off_t {
    let wh = match whence {
        libc::SEEK_SET => w::WHENCE_SET,
        libc::SEEK_CUR => w::WHENCE_CUR,
        _ => w::WHENCE_END,
    };
    match unsafe { w::fd_seek(fd as u32, off, wh) } {
        Ok(p) => p as off_t,
        Err(e) => fail(e) as off_t,
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsync(fd: c_int) -> c_int {
    ret(unsafe { w::fd_sync(fd as u32) })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ftruncate(fd: c_int, len: off_t) -> c_int {
    ret(unsafe { w::fd_filestat_set_size(fd as u32, len as u64) })
}
#[unsafe(no_mangle)]
/// Variadic in C: `va` points at the optional argument.
pub unsafe extern "C" fn fcntl(fd: c_int, cmd: c_int, va: *const c_int) -> c_int {
    match cmd {
        libc::F_GETFD | libc::F_SETFD => 0,
        libc::F_GETFL => match unsafe { w::fd_fdstat_get(fd as u32) } {
            Ok(s) => {
                let mut f = s.fs_flags as c_int;
                if s.fs_rights_base & R_READ != 0 {
                    f |= libc::O_RDONLY;
                }
                if s.fs_rights_base & (1 << 6) != 0 {
                    f |= libc::O_WRONLY;
                }
                f
            }
            Err(e) => fail(e),
        },
        libc::F_SETFL => ret(unsafe { w::fd_fdstat_set_flags(fd as u32, (*va & 0xfff) as u16) }),
        _ => fail(w::ERRNO_INVAL),
    }
}

fn to_stat(f: w::Filestat, st: &mut Stat) {
    use libc::*;
    *st = unsafe { core::mem::zeroed() };
    st.st_dev = f.dev;
    st.st_ino = f.ino;
    st.st_nlink = f.nlink;
    st.st_size = f.size as off_t;
    st.st_mode = match f.filetype {
        w::FILETYPE_DIRECTORY => S_IFDIR,
        w::FILETYPE_REGULAR_FILE => S_IFREG,
        w::FILETYPE_SYMBOLIC_LINK => S_IFLNK,
        w::FILETYPE_BLOCK_DEVICE => S_IFBLK,
        w::FILETYPE_CHARACTER_DEVICE => S_IFCHR,
        w::FILETYPE_SOCKET_DGRAM | w::FILETYPE_SOCKET_STREAM => S_IFSOCK,
        _ => 0,
    };
    // WASI has no permission bits; report rw (rwx for dirs) so std's `readonly()` is false.
    st.st_mode |= if st.st_mode == S_IFDIR { 0o755 } else { 0o644 };
    let ts = |n: u64| timespec {
        tv_sec: (n / 1_000_000_000) as _,
        tv_nsec: (n % 1_000_000_000) as _,
    };
    st.st_atim = ts(f.atim);
    st.st_mtim = ts(f.mtim);
    st.st_ctim = ts(f.ctim);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstat(fd: c_int, st: *mut Stat) -> c_int {
    match unsafe { w::fd_filestat_get(fd as u32) } {
        Ok(f) => {
            to_stat(f, unsafe { &mut *st });
            0
        }
        Err(e) => fail(e),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstatat(
    dirfd: c_int,
    p: *const c_char,
    st: *mut Stat,
    flags: c_int,
) -> c_int {
    let lookup = if flags & libc::AT_SYMLINK_NOFOLLOW != 0 {
        0
    } else {
        w::LOOKUPFLAGS_SYMLINK_FOLLOW
    };
    let mut buf = Path::new();
    let r = unsafe { resolve(dirfd, p, &mut buf) }
        .and_then(|(fd, rel)| unsafe { w::path_filestat_get(fd, lookup, rel) });
    match r {
        Ok(f) => {
            to_stat(f, unsafe { &mut *st });
            0
        }
        Err(e) => fail(e),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stat(p: *const c_char, st: *mut Stat) -> c_int {
    unsafe { fstatat(libc::AT_FDCWD, p, st, 0) }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lstat(p: *const c_char, st: *mut Stat) -> c_int {
    unsafe { fstatat(libc::AT_FDCWD, p, st, libc::AT_SYMLINK_NOFOLLOW) }
}

fn fst(times: *const timespec) -> (u64, u64, u16) {
    if times.is_null() {
        return (0, 0, (w::FSTFLAGS_ATIM_NOW | w::FSTFLAGS_MTIM_NOW) as u16);
    }
    let t = unsafe { core::slice::from_raw_parts(times, 2) };
    let mut flags = 0;
    let mut v = [0u64; 2];
    for (i, (set, now)) in [
        (w::FSTFLAGS_ATIM, w::FSTFLAGS_ATIM_NOW),
        (w::FSTFLAGS_MTIM, w::FSTFLAGS_MTIM_NOW),
    ]
    .into_iter()
    .enumerate()
    {
        match t[i].tv_nsec {
            libc::UTIME_OMIT => {}
            libc::UTIME_NOW => flags |= now,
            n => {
                flags |= set;
                v[i] = t[i].tv_sec as u64 * 1_000_000_000 + n as u64;
            }
        }
    }
    (v[0], v[1], flags as u16)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn futimens(fd: c_int, times: *const timespec) -> c_int {
    let (a, m, f) = fst(times);
    ret(unsafe { w::fd_filestat_set_times(fd as u32, a, m, f) })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn utimensat(
    dirfd: c_int,
    p: *const c_char,
    times: *const timespec,
    flags: c_int,
) -> c_int {
    let (a, m, f) = fst(times);
    let lookup = if flags & libc::AT_SYMLINK_NOFOLLOW != 0 {
        0
    } else {
        w::LOOKUPFLAGS_SYMLINK_FOLLOW
    };
    at!(dirfd, p, |fd, rel| w::path_filestat_set_times(
        fd, lookup, rel, a, m, f
    ))
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn chmod(_p: *const c_char, _m: mode_t) -> c_int {
    fail(w::ERRNO_NOTSUP)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fchmod(_fd: c_int, _m: mode_t) -> c_int {
    fail(w::ERRNO_NOTSUP)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mkdirat(dirfd: c_int, p: *const c_char, _m: mode_t) -> c_int {
    at!(dirfd, p, |fd, rel| w::path_create_directory(fd, rel))
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mkdir(p: *const c_char, m: mode_t) -> c_int {
    unsafe { mkdirat(libc::AT_FDCWD, p, m) }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlinkat(dirfd: c_int, p: *const c_char, flags: c_int) -> c_int {
    if flags & libc::AT_REMOVEDIR != 0 {
        at!(dirfd, p, |fd, rel| w::path_remove_directory(fd, rel))
    } else {
        at!(dirfd, p, |fd, rel| w::path_unlink_file(fd, rel))
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlink(p: *const c_char) -> c_int {
    unsafe { unlinkat(libc::AT_FDCWD, p, 0) }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rmdir(p: *const c_char) -> c_int {
    unsafe { unlinkat(libc::AT_FDCWD, p, libc::AT_REMOVEDIR) }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn renameat(
    od: c_int,
    op: *const c_char,
    nd: c_int,
    np: *const c_char,
) -> c_int {
    let mut b = Path::new();
    match unsafe { resolve(nd, np, &mut b) } {
        Ok((nfd, nrel)) => at!(od, op, |ofd, orel| w::path_rename(ofd, orel, nfd, nrel)),
        Err(e) => fail(e),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rename(a: *const c_char, b: *const c_char) -> c_int {
    unsafe { renameat(libc::AT_FDCWD, a, libc::AT_FDCWD, b) }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn linkat(
    od: c_int,
    op: *const c_char,
    nd: c_int,
    np: *const c_char,
    flags: c_int,
) -> c_int {
    let lookup = if flags & libc::AT_SYMLINK_FOLLOW != 0 {
        w::LOOKUPFLAGS_SYMLINK_FOLLOW
    } else {
        0
    };
    let mut b = Path::new();
    match unsafe { resolve(nd, np, &mut b) } {
        Ok((nfd, nrel)) => at!(od, op, |ofd, orel| w::path_link(
            ofd, lookup, orel, nfd, nrel
        )),
        Err(e) => fail(e),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn symlink(target: *const c_char, p: *const c_char) -> c_int {
    let t = unsafe { cstr(target) };
    at!(libc::AT_FDCWD, p, |fd, rel| w::path_symlink(t, fd, rel))
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn readlink(p: *const c_char, out: *mut c_char, n: size_t) -> ssize_t {
    let mut b = Path::new();
    retn(
        unsafe { resolve(libc::AT_FDCWD, p, &mut b) }
            .and_then(|(fd, rel)| unsafe { w::path_readlink(fd, rel, out.cast(), n) }),
    )
}

// ---- directories ----

const DBUF: usize = 4096;
#[repr(C)]
pub struct Dir {
    fd: c_int,
    cookie: u64,
    pos: usize,
    len: usize,
    eof: bool,
    buf: [u8; DBUF],
    ent: Ent,
}

// std reads the returned `dirent` in place, so it needs `dirent`'s alignment.
#[repr(C, align(8))]
struct Ent([u8; 16 + 512]);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fdopendir(fd: c_int) -> *mut Dir {
    unsafe {
        let d = calloc(1, size_of::<Dir>()).cast::<Dir>();
        if !d.is_null() {
            (*d).fd = fd;
        }
        d
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn opendir(p: *const c_char) -> *mut Dir {
    let fd = unsafe {
        openat(
            libc::AT_FDCWD,
            p,
            libc::O_RDONLY | libc::O_DIRECTORY,
            null_mut(),
        )
    };
    if fd < 0 {
        null_mut()
    } else {
        unsafe { fdopendir(fd) }
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn closedir(d: *mut Dir) -> c_int {
    unsafe {
        let r = close((*d).fd);
        free(d.cast());
        r
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dirfd(d: *mut Dir) -> c_int {
    unsafe { (*d).fd }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn readdir(d: *mut Dir) -> *mut dirent {
    const H: usize = 24;
    let d = unsafe { &mut *d };
    loop {
        let avail = d.len - d.pos;
        if avail >= H {
            let h = &d.buf[d.pos..];
            let next = u64::from_le_bytes(h[0..8].try_into().unwrap());
            let ino = u64::from_le_bytes(h[8..16].try_into().unwrap());
            let namlen = u32::from_le_bytes(h[16..20].try_into().unwrap()) as usize;
            let ty = h[20];
            if avail >= H + namlen {
                let name = &d.buf[d.pos + H..d.pos + H + namlen];
                d.pos += H + namlen;
                d.cookie = next;
                let n = namlen.min(d.ent.0.len() - 10);
                d.ent.0[..8].copy_from_slice(&ino.to_le_bytes());
                d.ent.0[8] = ty;
                d.ent.0[9..9 + n].copy_from_slice(&name[..n]);
                d.ent.0[9 + n] = 0;
                return d.ent.0.as_mut_ptr().cast();
            }
        }
        if d.eof && avail < H {
            return null_mut();
        }
        // Refill from the first unconsumed entry.
        match unsafe { w::fd_readdir(d.fd as u32, d.buf.as_mut_ptr(), DBUF, d.cookie) } {
            Ok(n) => {
                d.pos = 0;
                d.len = n;
                d.eof = n < DBUF;
                if n == 0 {
                    return null_mut();
                }
            }
            Err(e) => {
                fail(e);
                return null_mut();
            }
        }
    }
}

// ---- time, scheduling, misc ----

// wasi-libc's clockid_t is a pointer to one of these.
#[unsafe(no_mangle)]
pub static _CLOCK_REALTIME: u32 = 0;
#[unsafe(no_mangle)]
pub static _CLOCK_MONOTONIC: u32 = 1;
#[unsafe(no_mangle)]
pub static _CLOCK_PROCESS_CPUTIME_ID: u32 = 2;
#[unsafe(no_mangle)]
pub static _CLOCK_THREAD_CPUTIME_ID: u32 = 3;

fn clock(id: *const u32) -> w::Clockid {
    if unsafe { *id } == 0 {
        w::CLOCKID_REALTIME
    } else {
        w::CLOCKID_MONOTONIC
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_gettime(id: *const u32, ts: *mut timespec) -> c_int {
    let clk = clock(id);
    match unsafe { w::clock_time_get(clk, 1) } {
        Ok(n) => {
            unsafe {
                *ts = timespec {
                    tv_sec: (n / 1_000_000_000) as _,
                    tv_nsec: (n % 1_000_000_000) as _,
                }
            };
            0
        }
        Err(e) => fail(e),
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_nanosleep(
    id: *const u32,
    flags: c_int,
    t: *const timespec,
    _rem: *mut timespec,
) -> c_int {
    let t = unsafe { &*t };
    let sub = w::Subscription {
        userdata: 0,
        u: w::SubscriptionU {
            tag: w::EVENTTYPE_CLOCK.raw(),
            u: w::SubscriptionUU {
                clock: w::SubscriptionClock {
                    id: clock(id),
                    timeout: t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64,
                    precision: 0,
                    flags: if flags & libc::TIMER_ABSTIME != 0 {
                        w::SUBCLOCKFLAGS_SUBSCRIPTION_CLOCK_ABSTIME
                    } else {
                        0
                    },
                },
            },
        },
    };
    let mut ev: w::Event = unsafe { core::mem::zeroed() };
    match unsafe { w::poll_oneoff(&sub, &mut ev, 1) } {
        Ok(_) => 0,
        Err(e) => e.raw() as c_int,
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sched_yield() -> c_int {
    ret(unsafe { w::sched_yield() })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sysconf(name: c_int) -> c_long {
    match name {
        libc::_SC_PAGESIZE => 65536,
        libc::_SC_NPROCESSORS_CONF | libc::_SC_NPROCESSORS_ONLN => 1,
        _ => fail(w::ERRNO_INVAL) as c_long,
    }
}

// No threads on wasm32-wasip1: spawning fails, everything else is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_create(
    _: *mut c_void,
    _: *const c_void,
    _: *const c_void,
    _: *mut c_void,
) -> c_int {
    libc::EAGAIN
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_join(_: usize, _: *mut *mut c_void) -> c_int {
    libc::ESRCH
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_detach(_: usize) -> c_int {
    0
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_init(_: *mut c_void) -> c_int {
    0
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_destroy(_: *mut c_void) -> c_int {
    0
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_setstacksize(_: *mut c_void, _: size_t) -> c_int {
    0
}

// ---- misc posix used by rustc ----

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __errno_location() -> *mut c_int {
    &raw mut errno
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn isatty(fd: c_int) -> c_int {
    match unsafe { w::fd_fdstat_get(fd as u32) } {
        Ok(st) if st.fs_filetype == w::FILETYPE_CHARACTER_DEVICE => 1,
        Ok(_) => {
            unsafe { errno = libc::ENOTTY };
            0
        }
        Err(e) => {
            fail(e);
            0
        }
    }
}
/// Variadic in C: `va` points at the optional argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ioctl(_fd: c_int, _req: c_int, _va: *mut c_void) -> c_int {
    fail(w::ERRNO_NOTTY)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn readlinkat(
    dirfd: c_int,
    p: *const c_char,
    out: *mut c_char,
    n: size_t,
) -> ssize_t {
    let mut b = Path::new();
    retn(
        unsafe { resolve(dirfd, p, &mut b) }
            .and_then(|(fd, rel)| unsafe { w::path_readlink(fd, rel, out.cast(), n) }),
    )
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn link(a: *const c_char, b: *const c_char) -> c_int {
    unsafe { linkat(libc::AT_FDCWD, a, libc::AT_FDCWD, b, 0) }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn symlinkat(target: *const c_char, dirfd: c_int, p: *const c_char) -> c_int {
    let t = unsafe { cstr(target) };
    at!(dirfd, p, |fd, rel| w::path_symlink(t, fd, rel))
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faccessat(
    dirfd: c_int,
    p: *const c_char,
    _mode: c_int,
    flags: c_int,
) -> c_int {
    let lookup = if flags & libc::AT_SYMLINK_NOFOLLOW != 0 {
        0
    } else {
        w::LOOKUPFLAGS_SYMLINK_FOLLOW
    };
    at!(dirfd, p, |fd, rel| w::path_filestat_get(fd, lookup, rel))
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn access(p: *const c_char, mode: c_int) -> c_int {
    unsafe { faccessat(libc::AT_FDCWD, p, mode, 0) }
}
/// Returns the error number directly, like POSIX.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_fadvise(fd: c_int, off: off_t, len: off_t, advice: c_int) -> c_int {
    let advice = match advice {
        libc::POSIX_FADV_SEQUENTIAL => w::ADVICE_SEQUENTIAL,
        libc::POSIX_FADV_RANDOM => w::ADVICE_RANDOM,
        libc::POSIX_FADV_WILLNEED => w::ADVICE_WILLNEED,
        libc::POSIX_FADV_DONTNEED => w::ADVICE_DONTNEED,
        libc::POSIX_FADV_NOREUSE => w::ADVICE_NOREUSE,
        _ => w::ADVICE_NORMAL,
    };
    match unsafe { w::fd_advise(fd as u32, off as u64, len as u64, advice) } {
        Ok(()) => 0,
        Err(e) => e.raw() as c_int,
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_fallocate(fd: c_int, off: off_t, len: off_t) -> c_int {
    match unsafe { w::fd_allocate(fd as u32, off as u64, len as u64) } {
        Ok(()) => 0,
        Err(e) => e.raw() as c_int,
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fdatasync(fd: c_int) -> c_int {
    ret(unsafe { w::fd_datasync(fd as u32) })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn preadv(fd: c_int, iov: *const iovec, n: c_int, off: off_t) -> ssize_t {
    retn(unsafe {
        w::fd_pread(
            fd as u32,
            core::slice::from_raw_parts(iov.cast(), n as usize),
            off as u64,
        )
    })
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwritev(fd: c_int, iov: *const iovec, n: c_int, off: off_t) -> ssize_t {
    retn(unsafe {
        w::fd_pwrite(
            fd as u32,
            core::slice::from_raw_parts(iov.cast(), n as usize),
            off as u64,
        )
    })
}
// C math for std's `f64::exp` etc. (wasi-libc normally provides these), forwarded to `libm`.
macro_rules! libm_c {
    ($($name:ident($($a:ident: $t:ty),*) -> $r:ty;)*) => {$(
        #[unsafe(no_mangle)]
        pub extern "C" fn $name($($a: $t),*) -> $r {
            libm::$name($($a),*)
        }
    )*};
}
libm_c! {
    acos(x: f64) -> f64; asin(x: f64) -> f64; atan(x: f64) -> f64; atan2(y: f64, x: f64) -> f64;
    cbrt(x: f64) -> f64; ceil(x: f64) -> f64; copysign(x: f64, y: f64) -> f64; cos(x: f64) -> f64;
    cosh(x: f64) -> f64; erf(x: f64) -> f64; erfc(x: f64) -> f64; exp(x: f64) -> f64;
    exp10(x: f64) -> f64; exp2(x: f64) -> f64; expm1(x: f64) -> f64; fabs(x: f64) -> f64;
    fdim(x: f64, y: f64) -> f64; floor(x: f64) -> f64; fma(x: f64, y: f64, z: f64) -> f64;
    fmax(x: f64, y: f64) -> f64; fmin(x: f64, y: f64) -> f64; fmod(x: f64, y: f64) -> f64;
    hypot(x: f64, y: f64) -> f64; ldexp(x: f64, n: i32) -> f64; lgamma(x: f64) -> f64;
    log(x: f64) -> f64; log10(x: f64) -> f64; log1p(x: f64) -> f64; log2(x: f64) -> f64;
    nextafter(x: f64, y: f64) -> f64; pow(x: f64, y: f64) -> f64; remainder(x: f64, y: f64) -> f64;
    rint(x: f64) -> f64; round(x: f64) -> f64; scalbn(x: f64, n: i32) -> f64; sin(x: f64) -> f64;
    sinh(x: f64) -> f64; sqrt(x: f64) -> f64; tan(x: f64) -> f64; tanh(x: f64) -> f64;
    tgamma(x: f64) -> f64; trunc(x: f64) -> f64;
    acosf(x: f32) -> f32; asinf(x: f32) -> f32; atanf(x: f32) -> f32; atan2f(y: f32, x: f32) -> f32;
    cbrtf(x: f32) -> f32; ceilf(x: f32) -> f32; copysignf(x: f32, y: f32) -> f32; cosf(x: f32) -> f32;
    coshf(x: f32) -> f32; erff(x: f32) -> f32; erfcf(x: f32) -> f32; expf(x: f32) -> f32;
    exp10f(x: f32) -> f32; exp2f(x: f32) -> f32; expm1f(x: f32) -> f32; fabsf(x: f32) -> f32;
    fdimf(x: f32, y: f32) -> f32; floorf(x: f32) -> f32; fmaf(x: f32, y: f32, z: f32) -> f32;
    fmaxf(x: f32, y: f32) -> f32; fminf(x: f32, y: f32) -> f32; fmodf(x: f32, y: f32) -> f32;
    hypotf(x: f32, y: f32) -> f32; ldexpf(x: f32, n: i32) -> f32; lgammaf(x: f32) -> f32;
    logf(x: f32) -> f32; log10f(x: f32) -> f32; log1pf(x: f32) -> f32; log2f(x: f32) -> f32;
    nextafterf(x: f32, y: f32) -> f32; powf(x: f32, y: f32) -> f32; remainderf(x: f32, y: f32) -> f32;
    rintf(x: f32) -> f32; roundf(x: f32) -> f32; scalbnf(x: f32, n: i32) -> f32; sinf(x: f32) -> f32;
    sinhf(x: f32) -> f32; sqrtf(x: f32) -> f32; tanf(x: f32) -> f32; tanhf(x: f32) -> f32;
    tgammaf(x: f32) -> f32; truncf(x: f32) -> f32;
    acosh(x: f64) -> f64; asinh(x: f64) -> f64; atanh(x: f64) -> f64;
    ilogb(x: f64) -> i32; j0(x: f64) -> f64; j1(x: f64) -> f64; jn(n: i32, x: f64) -> f64;
    y0(x: f64) -> f64; y1(x: f64) -> f64; yn(n: i32, x: f64) -> f64;
    acoshf(x: f32) -> f32; asinhf(x: f32) -> f32; atanhf(x: f32) -> f32;
    ilogbf(x: f32) -> i32; j0f(x: f32) -> f32; j1f(x: f32) -> f32; jnf(n: i32, x: f32) -> f32;
    y0f(x: f32) -> f32; y1f(x: f32) -> f32; ynf(n: i32, x: f32) -> f32;
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lgamma_r(x: f64, sign: *mut i32) -> f64 {
    let (r, s) = libm::lgamma_r(x);
    unsafe { *sign = s };
    r
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lgammaf_r(x: f32, sign: *mut i32) -> f32 {
    let (r, s) = libm::lgammaf_r(x);
    unsafe { *sign = s };
    r
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frexp(x: f64, e: *mut i32) -> f64 {
    let (r, n) = libm::frexp(x);
    unsafe { *e = n };
    r
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frexpf(x: f32, e: *mut i32) -> f32 {
    let (r, n) = libm::frexpf(x);
    unsafe { *e = n };
    r
}

// _Unwind API over the backend's emulated EH (__pliron_eh flag+exn slot).
// RaiseException/Resume stash the exception and set the flag; the backend's
// per-call checks propagate the unwind up the stack.
#[repr(C)]
pub struct _Unwind_Exception {
    pub exception_class: u64,
    pub exception_cleanup: Option<unsafe extern "C" fn(u32, *mut _Unwind_Exception)>,
    pub private: [usize; 2],
}

unsafe extern "C" {
    static __pliron_eh: u64;
}

unsafe fn eh_raise(exn: *mut u8) {
    let p = &raw const __pliron_eh as *mut u8;
    unsafe {
        core::ptr::write_volatile(p.add(4).cast::<u32>(), exn as u32);
        core::ptr::write_volatile(p.cast::<u32>(), 1);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn _Unwind_RaiseException(exn: *mut _Unwind_Exception) -> u32 {
    unsafe { eh_raise(exn.cast()) };
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn _Unwind_Resume(exn: *mut _Unwind_Exception) -> ! {
    unsafe { eh_raise(exn.cast()) };
    loop {
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_DeleteException(exn: *mut _Unwind_Exception) {
    if let Some(cleanup) = unsafe { (*exn).exception_cleanup } {
        unsafe { cleanup(3 /* _URC_FOREIGN_EXCEPTION_CAUGHT */, exn) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_GetIP(_ctx: *mut u8) -> usize {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_GetIPInfo(_ctx: *mut u8, ip_before_insn: *mut i32) -> usize {
    unsafe { *ip_before_insn = 0 };
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_SetIP(_ctx: *mut u8, _v: usize) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_GetGR(_ctx: *mut u8, _r: i32) -> usize {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_SetGR(_ctx: *mut u8, _r: i32, _v: usize) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_GetLanguageSpecificData(_ctx: *mut u8) -> usize {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_GetRegionStart(_ctx: *mut u8) -> usize {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_GetTextRelBase(_ctx: *mut u8) -> usize {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_GetDataRelBase(_ctx: *mut u8) -> usize {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_FindEnclosingFunction(_pc: *mut u8) -> usize {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Unwind_Backtrace(
    _cb: unsafe extern "C" fn(*mut u8, *mut u8) -> u32,
    _a: *mut u8,
) -> u32 {
    5 // _URC_END_OF_STACK
}
