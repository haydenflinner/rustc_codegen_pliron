//! In-process loader for rustc_codegen_pliron hot patches.
//!
//! The base binary is built with `PLIRON_HOT=<crate>` so each of that crate's
//! functions is a thunk through a `__hot_slot.<sym>` pointer. A patch is the
//! same crate recompiled normally to one relocatable object. The loader maps it
//! near the executable, resolves undefined symbols against the running
//! executable's own symbol table (then `dlsym`), binds writable statics to the
//! live copies so state survives, registers its `.eh_frame`, and repoints the
//! slots of the functions whose code changed since the previous object
//! (`base.ref` in the patch dir for the first patch). Unchanged functions keep
//! running the old code, and the patch calls them through their thunks.

use object::elf::*;
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol, RelocationFlags, RelocationTarget, SectionIndex, SectionKind, SymbolKind};
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn pliron_hot_anchor() {}

fn exe_path() -> Result<std::path::PathBuf, String> {
    #[cfg(target_os = "macos")]
    #[allow(deprecated)]
    unsafe {
        let p = libc::_dyld_get_image_name(0);
        if !p.is_null() {
            return Ok(std::path::PathBuf::from(
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned(),
            ));
        }
    }
    Ok(std::path::PathBuf::from("/proc/self/exe"))
}

struct BaseSym {
    addr: usize,
    writable: bool,
    /// Only one definition of this name, so a patch static can safely alias it.
    unique: bool,
}

pub struct Base {
    syms: HashMap<String, BaseSym>,
    end: usize,
    /// Runtime extent of the executable's `__hot_thunks` section (arm64
    /// Mach-O): the only pages `direct` may rewrite.
    thunks: (usize, usize),
}

impl Base {
    pub fn load() -> Result<Base, String> {
        let path = exe_path()?;
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        let f = object::File::parse(&*bytes).map_err(|e| e.to_string())?;
        let mut raw = HashMap::new();
        for s in f.symbols() {
            if !s.is_definition() || s.kind() == SymbolKind::Tls {
                continue;
            }
            let Ok(name) = s.name() else { continue };
            let writable = s
                .section_index()
                .and_then(|i| f.section_by_index(i).ok())
                .is_some_and(|sec| matches!(sec.kind(), SectionKind::Data | SectionKind::UninitializedData));
            let e = raw.entry(name.to_string()).or_insert((s.address() as usize, writable, s.is_global(), 0));
            e.3 += 1;
            if s.is_global() && !e.2 {
                *e = (s.address() as usize, writable, true, e.3);
            }
        }
        let anchor = raw
            .get("pliron_hot_anchor")
            .or_else(|| raw.get("_pliron_hot_anchor"))
            .ok_or("executable has no symbol table")?
            .0;
        let slide = (pliron_hot_anchor as *const () as usize).wrapping_sub(anchor);
        let end = f.segments().map(|s| (s.address() + s.size()) as usize).max().unwrap_or(0).wrapping_add(slide);
        let syms = raw
            .into_iter()
            .map(|(k, (a, w, _, n))| (k, BaseSym { addr: a.wrapping_add(slide), writable: w, unique: n == 1 }))
            .collect();
        let thunks = f
            .sections()
            .find(|s| s.name() == Ok("__hot_thunks"))
            .map_or((0, 0), |s| {
                let a = (s.address() as usize).wrapping_add(slide);
                (a, a + s.size() as usize)
            });
        Ok(Base { syms, end, thunks })
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        if let Some(s) = self.syms.get(name) {
            return Some(s.addr);
        }
        // dlsym wants the C spelling: Mach-O file names carry one extra `_`.
        #[cfg(target_os = "macos")]
        let name = name.strip_prefix('_').unwrap_or(name);
        let c = std::ffi::CString::new(name).ok()?;
        let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
        (!p.is_null()).then_some(p as usize)
    }
}

fn is_mangled(n: &str) -> bool {
    // ELF spells Rust names `_R*`/`_ZN*`; Mach-O adds one more leading `_`.
    let t = n.strip_prefix('_').unwrap_or(n);
    n.starts_with("_R") || n.starts_with("_ZN") || t.starts_with("_R") || t.starts_with("_ZN")
}

fn align(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

/// Map `len` RWX bytes within +-2GiB of the executable so PC32 relocations reach it.
fn map_near(end: usize, len: usize) -> Result<usize, String> {
    let len = align(len, 4096);
    let mut hint = align(end, 1 << 21) + (64 << 20);
    for _ in 0..24 {
        #[cfg(target_os = "linux")]
        let p = unsafe {
            libc::mmap(
                hint as *mut _,
                len,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
        };
        // Darwin has no MAP_FIXED_NOREPLACE: map at the hint, then unmap and
        // advance when the kernel picked a spot beyond PC32 reach.
        #[cfg(not(target_os = "linux"))]
        let p = {
            // Darwin arm64 rejects plain RWX maps; MAP_JIT pages may be
            // written and executed on unsigned binaries.
            let p = unsafe {
                libc::mmap(
                    hint as *mut _,
                    len,
                    libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_JIT,
                    -1,
                    0,
                )
            };
            if p != libc::MAP_FAILED && (p as i64 - end as i64).unsigned_abs() >= (1 << 31) {
                unsafe { libc::munmap(p, len) };
                libc::MAP_FAILED
            } else {
                p
            }
        };
        if p != libc::MAP_FAILED {
            return Ok(p as usize);
        }
        hint += 64 << 20;
    }
    Err("no address space near the executable".into())
}

/// Hash of each defined function's code and relocation targets. Anonymous
/// targets (`__rcg_alloc.N` counters differ between builds) hash by content.
pub type Prints = HashMap<String, u64>;

/// Pre-parsed symbol: `symbol_by_index` re-decodes the nlist/symtab entry on
/// every call, which dominates `fingerprints` on leaf crates with tens of
/// thousands of relocations. Decode once into a table.
struct Sym<'a> {
    name: &'a str,
    kind: SymbolKind,
    sec: Option<SectionIndex>,
    addr: u64,
    size: u64,
    def: bool,
    undef: bool,
    weak: bool,
}

/// Symbols of `f` indexed by `SymbolIndex`.
fn symtab<'d, 'f>(f: &'f object::File<'d, &'d [u8]>) -> Vec<Option<Sym<'d>>> {
    let mut v: Vec<Option<Sym<'d>>> = Vec::new();
    for s in f.symbols() {
        let i = s.index().0;
        if i >= v.len() {
            v.resize_with(i + 1, || None);
        }
        v[i] = Some(Sym {
            name: s.name().unwrap_or(""),
            kind: s.kind(),
            sec: s.section_index(),
            addr: s.address(),
            size: s.size(),
            def: s.is_definition(),
            undef: s.is_undefined(),
            weak: s.is_weak(),
        });
    }
    v
}

/// Pre-parsed section.
struct Sec<'d> {
    data: &'d [u8],
    size: u64,
    align: u64,
    kind: SectionKind,
}

/// One parse of a patch object, shared by `fingerprints` and the loaders:
/// decoding the symtab and bucketing relocs once instead of per caller
/// roughly halves apply time on large leaf crates.
struct Parsed<'d> {
    syms: Vec<Option<Sym<'d>>>,
    secs: HashMap<SectionIndex, Sec<'d>>,
    /// Relocations per section, sorted by offset.
    relocs: HashMap<SectionIndex, Vec<(u64, object::Relocation)>>,
    /// Sorted defined-symbol addresses per section (function extents).
    by_sec: HashMap<SectionIndex, Vec<u64>>,
}

impl<'d> Parsed<'d> {
    fn load(f: &object::File<'d, &'d [u8]>) -> Self {
        // Symtab decode and section/reloc collection are independent —
        // they are the two halves of parse time on large leaf crates.
        let (mut p, sec_parts) = std::thread::scope(|sc| {
            let sym_h = sc.spawn(|| symtab(f));
            let sec_h = sc.spawn(|| {
                let mut secs = HashMap::new();
                let mut relocs = HashMap::new();
                for s in f.sections() {
                    secs.insert(
                        s.index(),
                        Sec {
                            data: s.data().unwrap_or(&[]),
                            size: s.size(),
                            align: s.align(),
                            kind: s.kind(),
                        },
                    );
                    let mut rs: Vec<_> = s.relocations().collect();
                    rs.sort_unstable_by_key(|&(o, _)| o);
                    relocs.insert(s.index(), rs);
                }
                (secs, relocs)
            });
            (
                Parsed {
                    syms: sym_h.join().unwrap(),
                    secs: HashMap::new(),
                    relocs: HashMap::new(),
                    by_sec: HashMap::new(),
                },
                sec_h.join().unwrap(),
            )
        });
        let (secs, relocs) = sec_parts;
        p.secs = secs;
        p.relocs = relocs;
        for s in p.syms.iter().flatten() {
            if let (Some(si), true) = (s.sec, s.def) {
                p.by_sec.entry(si).or_default().push(s.addr);
            }
        }
        for v in p.by_sec.values_mut() {
            v.sort_unstable();
        }
        p
    }

    /// A symbol's byte range inside its section: explicit size, else up to
    /// the next symbol in the same section (Mach-O nlist symbols carry no
    /// size).
    fn extent(&self, si: SectionIndex, a: u64, size: u64) -> (u64, u64) {
        if size > 0 {
            return (a, a + size);
        }
        let next = self
            .by_sec
            .get(&si)
            .map(|v| v.partition_point(|&x| x <= a))
            .and_then(|i| self.by_sec[&si].get(i).copied())
            .unwrap_or_else(|| self.secs.get(&si).map(|s| s.size).unwrap_or(a));
        (a, next.max(a))
    }

    /// Hash a function's byte range plus the relocations inside it. `deep`
    /// follows data-section targets one level (per-function string/const
    /// blobs) — relocatable objects keep symbol values and reloc offsets
    /// section-relative, so `lo`/`hi` index `sec_data` directly.
    fn hash_range(&self, si: SectionIndex, lo: u64, hi: u64, h: &mut DefaultHasher, deep: bool) {
        if let Some(d) = self.secs.get(&si).map(|s| s.data) {
            let (i0, i1) = (lo as usize, hi as usize);
            if i1 <= d.len() && i0 <= i1 {
                d[i0..i1].hash(h);
            }
        }
        let Some(rs) = self.relocs.get(&si) else { return };
        let i0 = rs.partition_point(|&(o, _)| o < lo);
        for &(o, ref r) in &rs[i0..] {
            if o >= hi {
                break;
            }
            // Hash the offset relative to the function start: a function
            // that moved because a neighbour grew keeps its fingerprint,
            // avoiding spurious repoints of identical code.
            (o - lo).hash(h);
            r.addend().hash(h);
            match r.flags() {
                RelocationFlags::Elf { r_type } => r_type.hash(h),
                RelocationFlags::MachO { r_type, r_pcrel, r_length } => {
                    (r_type, r_pcrel, r_length).hash(h)
                }
                _ => {}
            }
            match r.target() {
                RelocationTarget::Symbol(i) => {
                    let Some(Some(t)) = self.syms.get(i.0) else { continue };
                    if t.undef || t.kind == SymbolKind::Text || is_mangled(t.name) {
                        t.name.hash(h);
                    } else if let (Some(ti), true) = (t.sec, deep) {
                        // By content only, not by address: an anonymous
                        // blob moves whenever an earlier one grows, and
                        // hashing where it sits made every function that
                        // names a constant look changed after an edit
                        // elsewhere (tests-on-save reran everything).
                        let (tlo, thi) = self.extent(ti, t.addr, t.size);
                        self.hash_range(ti, tlo, thi, h, false);
                    }
                }
                RelocationTarget::Section(ti) if deep => {
                    let end = self.secs.get(&ti).map(|s| s.size).unwrap_or(0);
                    self.hash_range(ti, 0, end, h, false);
                }
                _ => {}
            }
        }
    }
}

pub fn fingerprints(f: &object::File) -> Prints {
    fingerprints_of(&Parsed::load(f))
}

/// Fingerprints over an already-parsed object. Each function's extent is its
/// explicit size or up to the next symbol in its section; the hash covers its
/// code bytes and the relocations inside it (anonymous targets by content —
/// `__rcg_alloc.N` counters differ between builds). Work is split across
/// threads since hashing dominates on large leaf crates.
fn fingerprints_of(t: &Parsed) -> Prints {
    let mut texts = Vec::new();
    for (i, s) in t.syms.iter().enumerate() {
        let Some(s) = s else { continue };
        if s.def && s.kind == SymbolKind::Text && s.sec.is_some() {
            texts.push((i, s.sec.unwrap()));
        }
    }

    let mut out = HashMap::new();
    let hash_fns = |chunk: &[(usize, SectionIndex)]| -> Vec<(String, u64)> {
        let mut v = Vec::with_capacity(chunk.len());
        for &(i, si) in chunk {
            let s = t.syms[i].as_ref().unwrap();
            let (lo, hi) = t.extent(si, s.addr, s.size);
            let mut h = DefaultHasher::new();
            t.hash_range(si, lo, hi, &mut h, true);
            v.push((s.name.to_string(), h.finish()));
        }
        v
    };
    let nthreads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    if texts.len() < 512 || nthreads == 1 {
        out.extend(hash_fns(&texts));
    } else {
        let chunk = texts.len().div_ceil(nthreads);
        let mut results: Vec<Vec<(String, u64)>> = Vec::new();
        std::thread::scope(|sc| {
            let mut hs = Vec::new();
            for c in texts.chunks(chunk) {
                hs.push(sc.spawn(|| hash_fns(c)));
            }
            for h in hs {
                results.push(h.join().unwrap());
            }
        });
        out.extend(results.into_iter().flatten());
    }
    out
}

/// `PLIRON_HOT_DIRECT=1` (arm64 macOS): after the slots of a patch are
/// repointed, rewrite each patched function's thunk from
/// `adrp x16, slot; ldr x16, [x16]; br x16` to `b <new body>`, so calls
/// cost a direct jump instead of a load and an indirect branch (relax's
/// thunk head). The slot is always updated too: a thread already past the
/// first instruction still loads the right target.
///
/// Thunks live in their own page-padded section (`__TEXT,__hot_thunks`),
/// because the write needs the page read-write and Apple Silicon has no
/// writable+executable pages: for the moment a page is flipped nothing
/// on it can run. Every other thread is suspended around the flip, and
/// the patcher's own code is never on a thunk page. Returns how many
/// thunks now jump directly.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn direct(base: &Base, targets: &[(usize, usize)]) -> usize {
    use std::sync::atomic::AtomicU32;
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn vm_protect(task: u32, addr: usize, size: usize, set_max: i32, prot: i32) -> i32;
        fn task_threads(task: u32, list: *mut *mut u32, n: *mut u32) -> i32;
        fn thread_suspend(t: u32) -> i32;
        fn thread_resume(t: u32) -> i32;
        fn mach_thread_self() -> u32;
        fn mach_port_deallocate(task: u32, name: u32) -> i32;
        fn vm_deallocate(task: u32, addr: usize, size: usize) -> i32;
    }
    const PAGE: usize = 0x4000;
    let (lo, hi) = base.thunks;
    // (page, instruction address, new instruction), decided before any
    // thread stops: nothing below may allocate or take a lock.
    let mut work: Vec<(usize, usize, u32)> = Vec::new();
    for &(thunk, target) in targets {
        if thunk < lo || thunk + 28 > hi || (lo & (PAGE - 1)) != 0 || (hi & (PAGE - 1)) != 0 {
            continue;
        }
        // The slot load is the thunk's first instruction, or its fifth
        // behind a reach mark; a thunk patched before starts with `b`.
        let at = (0..5).map(|i| thunk + 4 * i).find(|&a| {
            let w = unsafe { (*(a as *const AtomicU32)).load(Ordering::Relaxed) };
            (w & 0x9f00_001f) == 0x9000_0010 || (w & 0xfc00_0000) == 0x1400_0000
        });
        let Some(at) = at else { continue };
        let d = target as isize - at as isize;
        if d & 3 != 0 || !(-(1 << 27)..(1 << 27)).contains(&d) {
            continue;
        }
        work.push((at & !(PAGE - 1), at, 0x1400_0000 | (((d >> 2) as u32) & 0x03ff_ffff)));
    }
    if work.is_empty() {
        return 0;
    }
    work.sort();
    let mut done = 0;
    unsafe {
        let task = mach_task_self_;
        let me = mach_thread_self();
        let (mut list, mut n) = (std::ptr::null_mut::<u32>(), 0u32);
        if task_threads(task, &mut list, &mut n) != 0 {
            return 0;
        }
        let threads = std::slice::from_raw_parts(list, n as usize);
        for &t in threads {
            if t != me {
                thread_suspend(t);
            }
        }
        let mut i = 0;
        while i < work.len() {
            let page = work[i].0;
            let mut j = i;
            while j < work.len() && work[j].0 == page {
                j += 1;
            }
            // 1|2|0x10: read, write, copy (a private copy of the file page).
            if vm_protect(task, page, PAGE, 0, 1 | 2 | 0x10) == 0 {
                for &(_, at, insn) in &work[i..j] {
                    (*(at as *const AtomicU32)).store(insn, Ordering::Release);
                }
                done += j - i;
            }
            // Back to read+execute whether or not the write happened.
            vm_protect(task, page, PAGE, 0, 1 | 4);
            sys_icache_invalidate(page as *const u8, PAGE);
            i = j;
        }
        for &t in threads {
            if t != me {
                thread_resume(t);
            }
            mach_port_deallocate(task, t);
        }
        mach_port_deallocate(task, me);
        vm_deallocate(task, list as usize, n as usize * 4);
    }
    done
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn direct(_base: &Base, _targets: &[(usize, usize)]) -> usize {
    0
}

/// The functions of the hot crate this process has entered so far, as the
/// executable spells them (`PLIRON_HOT_REACH=1` builds: every thunk sets
/// its function's byte in `__hot_marks` on the way in).
pub fn reach(base: &Base) -> Vec<String> {
    let mut v: Vec<String> = base
        .syms
        .iter()
        .filter_map(|(k, s)| {
            let name = k.strip_prefix("__hot_mark.")?;
            (unsafe { *(s.addr as *const u8) } != 0).then(|| name.to_string())
        })
        .collect();
    v.sort();
    v
}

/// `PLIRON_HOT_REACH_OUT=<file>`: write `reach` there when the process
/// exits. Runs from a static constructor, so a test binary gets it without
/// a line of code — it only has to link this crate
/// (`#[cfg(test)] use pliron_hot as _;` plus the `-u` link argument
/// `examples/hot-test.sh` passes).
extern "C" fn reach_dump() {
    let Some(out) = std::env::var_os("PLIRON_HOT_REACH_OUT") else { return };
    if let Ok(base) = Base::load() {
        let _ = std::fs::write(out, reach(&base).join("\n"));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pliron_hot_reach_init() {
    if std::env::var_os("PLIRON_HOT_REACH_OUT").is_some() {
        unsafe { libc::atexit(reach_dump) };
    }
}

#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
#[cfg_attr(not(target_os = "macos"), unsafe(link_section = ".init_array"))]
static REACH_CTOR: extern "C" fn() = pliron_hot_reach_init;

/// `PLIRON_HOT_FREE=protect`: retired patch mappings are not unmapped but
/// made inaccessible, and a fault inside one says so. An unmapped range
/// can be handed out again, and a thread that was still in the old code
/// (a long-running loop, a frame parked across the grace window) would
/// run whatever landed there; protected, it stops at once with the
/// reason on stderr. The address space stays reserved.
static RETIRED: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());
static RETIRED_N: AtomicUsize = AtomicUsize::new(0);
/// Lock-free copy for the signal handler.
static RETIRED_TAB: [(AtomicUsize, AtomicUsize); 256] =
    [const { (AtomicUsize::new(0), AtomicUsize::new(0)) }; 256];

extern "C" fn retired_fault(sig: i32, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    let addr = unsafe { (*info).si_addr() } as usize;
    let n = RETIRED_N.load(Ordering::Acquire).min(RETIRED_TAB.len());
    if (0..n).any(|i| {
        let (lo, len) = (RETIRED_TAB[i].0.load(Ordering::Relaxed), RETIRED_TAB[i].1.load(Ordering::Relaxed));
        addr >= lo && addr < lo + len
    }) {
        let msg = b"[hot] a thread was still running code of a patch that has been retired (PLIRON_HOT_FREE=protect); raise PLIRON_HOT_GRACE or restart\n";
        unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()) };
    }
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Make a retired mapping inaccessible and remember it for the handler.
fn retire(mem: usize, len: usize) {
    static HANDLER: std::sync::Once = std::sync::Once::new();
    HANDLER.call_once(|| unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = retired_fault as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut());
    });
    unsafe { libc::mprotect(mem as *mut _, len, libc::PROT_NONE) };
    let mut v = RETIRED.lock().unwrap();
    if v.len() < RETIRED_TAB.len() {
        RETIRED_TAB[v.len()].0.store(mem, Ordering::Relaxed);
        RETIRED_TAB[v.len()].1.store(len, Ordering::Relaxed);
        v.push((mem, len));
        RETIRED_N.store(v.len(), Ordering::Release);
    }
}

/// Type layouts of one object: `path -> (size, align, hash)`, from the
/// table the backend writes under `PLIRON_HOT_LAYOUT=1`
/// (`src/hot_layout.rs`). `None` when the object carries no table.
pub type Layouts = HashMap<String, (u64, u64, String)>;

pub fn layout_table(f: &object::File) -> Option<Layouts> {
    let sec = f
        .sections()
        .find(|s| matches!(s.name(), Ok("__hot_layout" | ".pliron_hot_layout")))?;
    let data = sec.data().ok()?;
    let mut out = Layouts::new();
    for line in String::from_utf8_lossy(data).lines() {
        let mut it = line.split('\t');
        let (Some(name), Some(size), Some(align), Some(hash)) = (it.next(), it.next(), it.next(), it.next()) else {
            continue;
        };
        out.insert(name.to_string(), (size.parse().unwrap_or(0), align.parse().unwrap_or(0), hash.to_string()));
    }
    Some(out)
}

/// The types of `new` whose layout differs from the running process's
/// (`base`): new code would read values the old code built at the wrong
/// offsets. Types only one side defines are fine — nothing live has a new
/// type's layout, and nothing new names a removed one.
pub fn layout_changes(base: &Layouts, new: &Layouts) -> Vec<String> {
    let mut v: Vec<String> = new
        .iter()
        .filter_map(|(name, n)| {
            let b = base.get(name)?;
            (b != n).then(|| {
                if b.0 != n.0 || b.1 != n.1 {
                    format!("`{name}` ({} -> {} bytes, align {} -> {})", b.0, n.0, b.1, n.1)
                } else {
                    format!("`{name}` (fields changed)")
                }
            })
        })
        .collect();
    v.sort();
    v
}

/// Refuse a patch object whose type layouts differ from `base`'s.
pub fn check_layout(base: Option<&Layouts>, path: &Path) -> Result<(), String> {
    let Some(base) = base else { return Ok(()) };
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let f = object::File::parse(&*bytes).map_err(|e| e.to_string())?;
    let Some(new) = layout_table(&f) else { return Ok(()) };
    let ch = layout_changes(base, &new);
    if ch.is_empty() {
        return Ok(());
    }
    Err(format!(
        "not applied: the layout of {} changed, and values the running code built would be read at the wrong offsets; restart to pick up this edit",
        ch.join(", ")
    ))
}

/// A successfully applied patch: its fingerprints, the mapping it occupies
/// (for `PLIRON_HOT_FREE` reclamation) and the slots it repointed.
pub struct Applied {
    /// Functions whose slots were repointed.
    pub n: usize,
    /// Fingerprints of this object — diff base for the next patch.
    pub prints: Prints,
    /// Functions repointed by name, and the mapping's extent.
    fns: Vec<String>,
    /// (thunk address, new body) of each repointed function, for `direct`.
    pub targets: Vec<(usize, usize)>,
    mem: usize,
    len: usize,
}

/// Load one patch object, repointing the slots of functions that differ from
/// `prev`. Returns what was repointed and this object's fingerprints.
pub fn apply(base: &Base, path: &Path, prev: Option<&Prints>) -> Result<Applied, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let f = object::File::parse(&*bytes).map_err(|e| e.to_string())?;
    if matches!(f, object::File::MachO64(_)) {
        return apply_macho(base, &f, prev);
    }
    let prints = fingerprints(&f);
    let changed = |n: &str| prev.is_none_or(|p| p.get(n) != prints.get(n));

    let mut place: HashMap<SectionIndex, usize> = HashMap::new();
    let (mut off, mut eh, mut nrel) = (0usize, None, 0usize);
    for s in f.sections() {
        let object::SectionFlags::Elf { sh_flags } = s.flags() else { continue };
        if sh_flags & SHF_ALLOC as u64 == 0 || s.size() == 0 {
            continue;
        }
        if sh_flags & SHF_TLS as u64 != 0 {
            return Err("patch defines thread-locals; restart needed".into());
        }
        off = align(off, s.align().max(1) as usize);
        place.insert(s.index(), off);
        if s.name() == Ok(".eh_frame") {
            eh = Some(off);
            off += 4; // zero terminator for __register_frame
        }
        off += s.size() as usize;
        nrel += s.relocations().count();
    }
    let got0 = align(off, 8);
    let plt0 = got0 + 8 * nrel;
    let mem = map_near(base.end, plt0 + 8 * nrel)?;
    for s in f.sections() {
        let Some(&o) = place.get(&s.index()) else { continue };
        if s.kind() != SectionKind::UninitializedData {
            let d = s.data().map_err(|e| e.to_string())?;
            unsafe { std::ptr::copy_nonoverlapping(d.as_ptr(), (mem + o) as *mut u8, d.len()) };
        }
    }

    let mut missing = Vec::new();
    let mut resolve = |sym: &object::Symbol<'_, '_>| -> usize {
        if let Some(&o) = sym.section_index().and_then(|i| place.get(&i)) {
            let local = mem + o + sym.address() as usize;
            if sym.kind() == SymbolKind::Section {
                return local;
            }
            let name = sym.name().unwrap_or("");
            match base.syms.get(name) {
                Some(b) if sym.kind() == SymbolKind::Text && !changed(name) => b.addr,
                // Rust statics keep their live state; anonymous constants
                // (`__rcg_alloc.N`, per-CGU counters) always use the patch copy.
                Some(b) if b.writable && b.unique && sym.kind() == SymbolKind::Data && is_mangled(name) => b.addr,
                _ => local,
            }
        } else {
            let name = sym.name().unwrap_or("");
            base.lookup(name).unwrap_or_else(|| {
                if !sym.is_weak() {
                    missing.push(name.to_string());
                }
                0
            })
        }
    };
    let (mut got, mut plt): (HashMap<usize, usize>, HashMap<usize, usize>) = Default::default();
    let mut got_slot = |s: usize| -> usize {
        let n = got.len();
        *got.entry(s).or_insert_with(|| {
            let g = mem + got0 + 8 * n;
            unsafe { (g as *mut usize).write(s) };
            g
        })
    };
    for s in f.sections() {
        let Some(&so) = place.get(&s.index()) else { continue };
        for (o, r) in s.relocations() {
            let p = mem + so + o as usize;
            let tgt = match r.target() {
                RelocationTarget::Symbol(i) => resolve(&f.symbol_by_index(i).map_err(|e| e.to_string())?),
                RelocationTarget::Section(i) => mem + place.get(&i).copied().unwrap_or(0),
                _ => continue,
            };
            let a = r.addend();
            let RelocationFlags::Elf { r_type } = r.flags() else { continue };
            let pc32 = |t: usize| -> Result<(), String> {
                let v = (t as i64).wrapping_add(a).wrapping_sub(p as i64);
                let v = i32::try_from(v).map_err(|_| format!("PC32 out of range at {o:#x} in {:?}", s.name()))?;
                unsafe { (p as *mut i32).write_unaligned(v) };
                Ok(())
            };
            match r_type {
                R_X86_64_NONE => {}
                R_X86_64_64 => unsafe { (p as *mut u64).write_unaligned((tgt as i64).wrapping_add(a) as u64) },
                R_X86_64_PC64 => unsafe {
                    (p as *mut i64).write_unaligned((tgt as i64).wrapping_add(a).wrapping_sub(p as i64))
                },
                R_X86_64_PC32 => pc32(tgt)?,
                R_X86_64_PLT32 => {
                    let near = i32::try_from((tgt as i64).wrapping_add(a).wrapping_sub(p as i64)).is_ok();
                    let t = if near {
                        tgt
                    } else {
                        let g = got_slot(tgt);
                        let n = plt.len();
                        *plt.entry(tgt).or_insert_with(|| {
                            let stub = mem + plt0 + 8 * n;
                            let rel = (g as i64 - (stub as i64 + 6)) as i32;
                            unsafe {
                                std::ptr::copy_nonoverlapping([0xff, 0x25].as_ptr(), stub as *mut u8, 2);
                                ((stub + 2) as *mut i32).write_unaligned(rel);
                            }
                            stub
                        })
                    };
                    pc32(t)?
                }
                R_X86_64_GOTPCREL | R_X86_64_GOTPCRELX | R_X86_64_REX_GOTPCRELX => pc32(got_slot(tgt))?,
                t => return Err(format!("unsupported relocation type {t}")),
            }
        }
    }
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        return Err(format!("{} symbols not in the running binary, e.g. {:?}", missing.len(), &missing[..missing.len().min(5)]));
    }
    if let Some(eh) = eh {
        if let Some(reg) = base.lookup("__register_frame") {
            let reg: extern "C" fn(*const u8) = unsafe { std::mem::transmute(reg) };
            reg((mem + eh) as *const u8);
        }
    }
    let mut fns = Vec::new();
    for sym in f.symbols() {
        if sym.kind() != SymbolKind::Text || !sym.is_definition() {
            continue;
        }
        let Ok(name) = sym.name() else { continue };
        if !changed(name) {
            continue;
        }
        if let Some(slot) = base.syms.get(&format!("__hot_slot.{name}")) {
            let new = mem + place[&sym.section_index().unwrap()] + sym.address() as usize;
            unsafe { (*(slot.addr as *const AtomicUsize)).store(new, Ordering::Release) };
            fns.push(name.to_string());
        }
    }
    let n = fns.len();
    Ok(Applied { n, prints, fns, targets: Vec::new(), mem, len: plt0 + 8 * nrel })
}

// Darwin arm64: MAP_JIT pages obey a per-thread write/execute toggle and
// code writes must be made visible to the instruction cache.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_jit_write_protect_np(enabled: i32);
    fn sys_icache_invalidate(start: *const u8, size: usize);
}

#[cfg(target_os = "macos")]
fn jit_writeable<T>(len: usize, mem: usize, f: impl FnOnce() -> T) -> T {
    unsafe { pthread_jit_write_protect_np(0) };
    let r = f();
    unsafe {
        sys_icache_invalidate(mem as *const u8, len);
        pthread_jit_write_protect_np(1);
    }
    r
}

#[cfg(not(target_os = "macos"))]
fn jit_writeable<T>(_len: usize, _mem: usize, f: impl FnOnce() -> T) -> T {
    f()
}

/// Mach-O arm64 relocation types (mach-o/reloc.h).
mod a64 {
    pub const UNSIGNED: u8 = 0;
    pub const SUBTRACTOR: u8 = 1;
    pub const BRANCH26: u8 = 2;
    pub const PAGE21: u8 = 3;
    pub const PAGEOFF12: u8 = 4;
    pub const GOT_LOAD_PAGE21: u8 = 5;
    pub const GOT_LOAD_PAGEOFF12: u8 = 6;
    pub const POINTER_TO_GOT: u8 = 7;
    pub const TLV_PAGE21: u8 = 8;
    pub const TLV_PAGEOFF12: u8 = 9;
}

/// imm12 scale for an unsigned-immediate load/store: access size is in
/// insn[31:30] for GPR/FP, `ldr q` takes 16 bytes, `add` takes raw bytes.
fn imm12_shift(insn: u32) -> u32 {
    if insn & 0x3b00_0000 == 0x3d00_0000 {
        4
    } else if insn & 0x3b00_0000 == 0x3900_0000 {
        insn >> 30
    } else {
        0
    }
}

/// Patch in a Mach-O object: copy changed functions and non-code sections
/// into one RWX mapping, apply the relocations inside those regions only,
/// repoint the `__hot_slot.*` of changed functions. Unchanged patch functions
/// are never mapped — references to them resolve to the live executable's
/// thunk (which forwards through the live slot), so apply cost and memory
/// grow with the size of the edit, not the crate.
fn apply_macho<'d>(
    base: &Base,
    f: &object::File<'d, &'d [u8]>,
    prev: Option<&Prints>,
) -> Result<Applied, String> {
    let dbg = std::env::var_os("PLIRON_HOT_DEBUG").is_some();
    let t0 = std::time::Instant::now();
    let p = Parsed::load(f);
    let tparse = t0.elapsed();
    let prints = fingerprints_of(&p);
    if dbg {
        eprintln!("[hot] parse {:?} fingerprints {:?}", tparse, t0.elapsed());
    }
    let changed = |n: &str| prev.is_none_or(|q| q.get(n) != prints.get(n));

    let syms = &p.syms;
    let relocs = &p.relocs;
    // Sections the loader must not map (debug/unwind metadata is not needed —
    // Mach-O compact unwind cannot be registered post-load).
    let mut skip: HashSet<SectionIndex> = HashSet::new();
    for s in f.sections() {
        let name = s.name().unwrap_or("");
        let seg = s.segment_name().unwrap_or(None).unwrap_or("");
        if name.starts_with("__thread") {
            return Err("patch defines thread-locals; restart needed".into());
        }
        if seg == "__DWARF" || seg == "__LD" || name == "__unwind_info" || name == "__eh_frame"
            || s.size() == 0
        {
            skip.insert(s.index());
        }
    }
    let mut place: HashMap<SectionIndex, usize> = HashMap::new();
    let mut text_secs: HashSet<SectionIndex> = HashSet::new();
    for (&si, s) in &p.secs {
        if skip.contains(&si) {
            continue;
        }
        if s.kind == SectionKind::Text {
            text_secs.insert(si);
        } else {
            place.insert(si, usize::MAX); // non-code sections: assigned below
        }
    }

    let extent = |si: SectionIndex, a: u64, size: u64| p.extent(si, a, size);

    // Live set: changed functions, plus the transitive closure of
    // patch-defined text symbols they reference that the base cannot
    // resolve (a live thunk forwards those that it can).
    let mut live: HashMap<usize, (u64, u64)> = HashMap::new(); // symidx -> (lo,hi)
    let mut wl: Vec<usize> = Vec::new();
    let want = |live: &mut HashMap<usize, (u64, u64)>, wl: &mut Vec<usize>, t: &Sym, ti: usize| {
        if t.def
            && t.kind == SymbolKind::Text
            && !live.contains_key(&ti)
            && !base.syms.contains_key(t.name)
        {
            let (tlo, thi) = extent(t.sec.unwrap(), t.addr, t.size);
            live.insert(ti, (tlo, thi));
            wl.push(ti);
        }
    };
    for (i, s) in syms.iter().enumerate() {
        let Some(s) = s else { continue };
        if s.def && s.kind == SymbolKind::Text && changed(s.name) {
            let (lo, hi) = extent(s.sec.unwrap(), s.addr, s.size);
            live.insert(i, (lo, hi));
            wl.push(i);
        }
    }
    // Only data reachable from live functions is copied: a probe-only patch
    // skips the crate's whole `__const` tables and their relocations. Data
    // sections may in turn reference patch-local functions (`__const`
    // fn-pointer tables) and other data, so the worklist covers both.
    let mut dsec: HashSet<SectionIndex> = HashSet::new();
    let mut dwl: Vec<SectionIndex> = Vec::new();
    loop {
        if let Some(i) = wl.pop() {
            let s = syms[i].as_ref().unwrap();
            let si = s.sec.unwrap();
            let (lo, hi) = live[&i];
            if let Some(rs) = relocs.get(&si) {
                let i0 = rs.partition_point(|&(o, _)| o < lo);
                for &(o, ref r) in &rs[i0..] {
                    if o >= hi {
                        break;
                    }
                    match r.target() {
                        RelocationTarget::Symbol(ti) => {
                            if let Some(Some(t)) = syms.get(ti.0) {
                                if t.kind == SymbolKind::Text {
                                    want(&mut live, &mut wl, t, ti.0);
                                } else if let Some(ti) = t.sec {
                                    if place.contains_key(&ti) && dsec.insert(ti) {
                                        dwl.push(ti);
                                    }
                                }
                            }
                        }
                        RelocationTarget::Section(ti) => {
                            if place.contains_key(&ti) && dsec.insert(ti) {
                                dwl.push(ti);
                            }
                        }
                        _ => {}
                    }
                }
            }
        } else if let Some(di) = dwl.pop() {
            if let Some(rs) = relocs.get(&di) {
                for (_, r) in rs {
                    match r.target() {
                        RelocationTarget::Symbol(ti) => {
                            if let Some(Some(t)) = syms.get(ti.0) {
                                if t.kind == SymbolKind::Text {
                                    want(&mut live, &mut wl, t, ti.0);
                                } else if let Some(ti) = t.sec {
                                    if place.contains_key(&ti) && dsec.insert(ti) {
                                        dwl.push(ti);
                                    }
                                }
                            }
                        }
                        RelocationTarget::Section(ti) => {
                            if place.contains_key(&ti) && dsec.insert(ti) {
                                dwl.push(ti);
                            }
                        }
                        _ => {}
                    }
                }
            }
        } else {
            break;
        }
    }

    // Layout: [live fn ranges, packed][non-code sections][GOT][stubs].
    let mut fndst: HashMap<usize, usize> = HashMap::new(); // symidx -> mem offset
    let (mut off, mut nrel) = (0usize, 0usize);
    let mut livev: Vec<(usize, u64, u64)> = live.iter().map(|(&i, &r)| (i, r.0, r.1)).collect();
    livev.sort_unstable();
    for &(i, lo, hi) in &livev {
        off = align(off, 16);
        fndst.insert(i, off);
        off += (hi - lo) as usize;
    }
    for &si in &dsec {
        let s = &p.secs[&si];
        off = align(off, s.align.max(1) as usize);
        place.insert(si, off);
        off += s.size as usize;
        nrel += relocs.get(&si).map_or(0, |r| r.len());
    }
    let got0 = align(off, 8);
    let stub0 = got0 + 8 * (nrel + 64);
    let t1 = std::time::Instant::now();
    if dbg {
        eprintln!("[hot] layout {:?}", t1.duration_since(t0));
    }
    let mem = map_near(base.end, stub0 + 16 * nrel)?;
    if dbg {
        eprintln!("[hot] map {:?}", t1.elapsed());
    }
    jit_writeable(stub0 + 16 * nrel, mem, || -> Result<(), String> {
        for &(i, lo, hi) in &livev {
            let s = &syms[i].as_ref().unwrap();
            let d = p.secs[&s.sec.unwrap()].data;
            unsafe {
                std::ptr::copy_nonoverlapping(
                    d.as_ptr().add(lo as usize),
                    (mem + fndst[&i]) as *mut u8,
                    (hi - lo) as usize,
                )
            };
        }
        for &si in &dsec {
            let s = &p.secs[&si];
            let o = place[&si];
            if s.kind == SectionKind::UninitializedData {
                continue;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(s.data.as_ptr(), (mem + o) as *mut u8, s.data.len())
            };
        }
        if dbg {
            eprintln!("[hot] copy {:?}", t1.elapsed());
        }

    let mut missing = Vec::new();
    let resolve = |i: usize, s: &Sym, missing: &mut Vec<String>| -> usize {
        match s.kind {
            SymbolKind::Section => s
                .sec
                .and_then(|si| place.get(&si))
                .filter(|&&o| o != usize::MAX)
                .map(|&o| mem + o + s.addr as usize)
                .unwrap_or_else(|| {
                    missing.push("<anonymous section symbol>".to_string());
                    0
                }),
            SymbolKind::Text if fndst.contains_key(&i) => mem + fndst[&i],
            _ => {
                let b = base.syms.get(s.name);
                match (s.kind, b) {
                    (SymbolKind::Text, Some(b)) => b.addr,
                    (SymbolKind::Data, Some(b)) if b.writable && b.unique && is_mangled(s.name) => {
                        b.addr
                    }
                    (_, Some(b)) if s.sec.is_none() => b.addr,
                    _ => s
                        .sec
                        .and_then(|si| place.get(&si))
                        .filter(|&&o| o != usize::MAX)
                        .map(|o| mem + o + s.addr as usize)
                        .unwrap_or_else(|| {
                            base.lookup(s.name).unwrap_or_else(|| {
                                if !s.weak {
                                    missing.push(s.name.to_string());
                                }
                                0
                            })
                        }),
                }
            }
        }
    };
    let mut got: HashMap<usize, usize> = Default::default();
    let mut got_slot = |s: usize| -> usize {
        let n = got.len();
        *got.entry(s).or_insert_with(|| {
            let g = mem + got0 + 8 * n;
            unsafe { (g as *mut usize).write(s) };
            g
        })
    };
    let mut nstub = 0usize;
    let mut stub = |tgt: usize| -> usize {
        // ldr x16, [pc, #8]; br x16; .quad tgt — 16 bytes, reaches anywhere
        let a = mem + stub0 + 16 * nstub;
        nstub += 1;
        unsafe {
            (a as *mut u32).write(0x5800_0050); // ldr x16, #8
            ((a + 4) as *mut u32).write(0xd61f_0200); // br x16
            ((a + 8) as *mut u64).write_unaligned(tgt as u64);
        }
        a
    };

    let apply_relocs = |si: SectionIndex,
                            lo: u64,
                            hi: u64,
                            base_addr: usize,
                            missing: &mut Vec<String>,
                            got_slot: &mut dyn FnMut(usize) -> usize,
                            stub: &mut dyn FnMut(usize) -> usize|
     -> Result<(), String> {
        let Some(rs) = relocs.get(&si) else { return Ok(()) };
        let i0 = rs.partition_point(|&(o, _)| o < lo);
        let mut napp = 0usize;
        for &(o, ref r) in &rs[i0..] {
            if o >= hi {
                break;
            }
            napp += 1;
            let p = base_addr + (o - lo) as usize;
            let mut tgt = match r.target() {
                RelocationTarget::Symbol(i) => match syms.get(i.0) {
                    Some(Some(sy)) => resolve(i.0, sy, missing),
                    _ => continue,
                },
                RelocationTarget::Section(i) => match place.get(&i).filter(|&&x| x != usize::MAX) {
                    Some(&x) => mem + x,
                    None => {
                        return Err(format!(
                            "section-target reloc into unmapped section {i:?}; restart needed"
                        ))
                    }
                },
                _ => continue,
            };
            let a = r.addend();
            let RelocationFlags::MachO { r_type, r_length, .. } = r.flags() else {
                continue;
            };
            match r_type {
                a64::UNSIGNED => match r_length {
                    3 => unsafe {
                        (p as *mut u64).write_unaligned(tgt.wrapping_add(a as usize) as u64)
                    },
                    2 => unsafe {
                        (p as *mut u32).write_unaligned(tgt.wrapping_add(a as usize) as u32)
                    },
                    _ => {}
                },
                a64::SUBTRACTOR => {
                    // `minuend - subtrahend`: the subtrahend is the paired
                    // UNSIGNED reloc object folds in as `subtractor()`.
                    let sub = r
                        .subtractor()
                        .and_then(|i| syms.get(i.0).and_then(|o| o.as_ref()).map(|s| (i.0, s)))
                        .map(|(si, sy)| resolve(si, sy, missing))
                        .unwrap_or(0);
                    tgt = tgt.wrapping_sub(sub).wrapping_add(a as usize);
                    match r_length {
                        3 => unsafe { (p as *mut u64).write_unaligned(tgt as u64) },
                        2 => unsafe { (p as *mut u32).write_unaligned(tgt as u32) },
                        _ => {}
                    }
                }
                a64::BRANCH26 => {
                    let a = (tgt as i64).wrapping_add(a).wrapping_sub(p as i64);
                    let t = if (-(1 << 27)..(1 << 27)).contains(&(a >> 2)) {
                        tgt
                    } else {
                        stub(tgt)
                    };
                    let a = (t as i64).wrapping_add(0).wrapping_sub(p as i64);
                    if (a & 3) != 0 || !(-(1 << 27)..(1 << 27)).contains(&(a >> 2)) {
                        return Err("BRANCH26 out of range".into());
                    }
                    unsafe {
                        let w = p as *mut u32;
                        *w = (*w & !0x03ff_ffff) | (((a >> 2) as u32) & 0x03ff_ffff);
                    }
                }
                a64::PAGE21 => {
                    let v = (((tgt as i64) + a) >> 12) - (p as i64 >> 12);
                    unsafe {
                        let w = p as *mut u32;
                        *w = (*w & !0x60ff_ffe0)
                            | (((v as u32) & 3) << 29)
                            | ((((v >> 2) as u32) & 0x7ffff) << 5);
                    }
                }
                a64::PAGEOFF12 => {
                    let off = (tgt as u64).wrapping_add(a as u64) & 0xfff;
                    unsafe {
                        let w = p as *mut u32;
                        let insn = *w;
                        // Unsigned-imm loads scale imm12 by the access size
                        // in insn[31:30]; `add`-style targets take raw bytes.
                        let shift = imm12_shift(insn);
                        *w = (insn & !0x003f_fc00) | (((off >> shift) as u32) << 10);
                    }
                }
                a64::GOT_LOAD_PAGE21 => {
                    let g = got_slot(tgt);
                    let v = ((g as i64) >> 12) - (p as i64 >> 12);
                    unsafe {
                        let w = p as *mut u32;
                        *w = (*w & !0x60ff_ffe0)
                            | (((v as u32) & 3) << 29)
                            | ((((v >> 2) as u32) & 0x7ffff) << 5);
                    }
                }
                a64::GOT_LOAD_PAGEOFF12 => {
                    let g = got_slot(tgt);
                    let off = g as u64 & 0xfff;
                    unsafe {
                        let w = p as *mut u32;
                        let insn = *w;
                        let shift = imm12_shift(insn);
                        *w = (insn & !0x003f_fc00) | (((off >> shift) as u32) << 10);
                    }
                }
                a64::POINTER_TO_GOT => {
                    let g = got_slot(tgt);
                    let v = g as i64 - p as i64;
                    let v = i32::try_from(v).map_err(|_| "POINTER_TO_GOT out of range")?;
                    unsafe { (p as *mut i32).write_unaligned(v) };
                }
                a64::TLV_PAGE21 | a64::TLV_PAGEOFF12 => {
                    return Err("patch uses thread-locals; restart needed".into())
                }
                t => return Err(format!("unsupported arm64 relocation type {t}")),
            }
        }
        if dbg {
            eprintln!("[hot]   range {si:?} lo={lo:#x} hi={hi:#x} applied {napp}");
        }
        Ok(())
    };
    for &(i, lo, hi) in &livev {
        apply_relocs(
            syms[i].as_ref().unwrap().sec.unwrap(),
            lo,
            hi,
            mem + fndst[&i],
            &mut missing,
            &mut got_slot,
            &mut stub,
        )?;
    }
        let tapp = t1.elapsed();
    for &si in &dsec {
        let o = place[&si];
        let end = p.secs.get(&si).map(|s| s.size).unwrap_or(0);
        apply_relocs(si, 0, end, mem + o, &mut missing, &mut got_slot, &mut stub)?;
    }
        if dbg {
            eprintln!(
                "[hot] apply {:?} (fns {} data {} relocs {tapp:?})",
                t1.elapsed(),
                livev.len(),
                dsec.len()
            );
        }
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            return Err(format!("{} symbols not in the running binary, e.g. {:?}", missing.len(), &missing[..missing.len().min(5)]));
        }
        Ok(())
    })?;
    if dbg {
        eprintln!("[hot] relocs {:?}", t0.elapsed());
    }
    let mut fns = Vec::new();
    let mut targets = Vec::new();
    for (i, s) in syms.iter().enumerate() {
        let Some(s) = s else { continue };
        if s.kind != SymbolKind::Text || !s.def || !changed(s.name) {
            continue;
        }
        if let Some(slot) = base.syms.get(&format!("__hot_slot.{}", s.name)) {
            unsafe { (*(slot.addr as *const AtomicUsize)).store(mem + fndst[&i], Ordering::Release) };
            if let Some(th) = base.syms.get(s.name) {
                targets.push((th.addr, mem + fndst[&i]));
            }
            if dbg {
                eprintln!("[hot] repoint {}", s.name);
            }
            fns.push(s.name.to_string());
        }
    }
    let n = fns.len();
    Ok(Applied { n, prints, fns, targets, mem, len: stub0 + 16 * nrel })
}

/// If `PLIRON_HOT_DIR` is set, apply every `*.o` that appears in it, in name order.
pub fn start() {
    let Some(dir) = std::env::var_os("PLIRON_HOT_DIR") else { return };
    let dir = std::path::PathBuf::from(dir);
    std::thread::spawn(move || {
        let base_ref = std::fs::read(dir.join("base.ref")).ok();
        let base_obj = base_ref.as_deref().and_then(|b| object::File::parse(b).ok());
        let mut prev = base_obj.as_ref().map(fingerprints);
        // The live process's type layouts (`PLIRON_HOT_LAYOUT=1` builds).
        let base_layout = base_obj.as_ref().and_then(layout_table);
        let base = match Base::load() {
            Ok(b) => b,
            Err(e) => return eprintln!("[hot] disabled: {e}"),
        };
        let mut seen = HashSet::new();
        let free = std::env::var_os("PLIRON_HOT_FREE").is_some();
        let protect = std::env::var("PLIRON_HOT_FREE").as_deref() == Ok("protect");
        let want_direct = std::env::var("PLIRON_HOT_DIRECT").as_deref() == Ok("1");
        let grace: usize = std::env::var("PLIRON_HOT_GRACE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        let mut gnr = 0usize;
        let mut applied: Vec<(usize, usize, HashSet<String>, usize)> = Vec::new();
        let mut provider: HashMap<String, usize> = HashMap::new();
        loop {
            let mut new: Vec<_> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let n = p.file_name().unwrap_or_default().to_string_lossy();
                    p.extension().is_some_and(|x| x == "o")
                        && !n.contains(".rcgu.")
                        && !seen.contains(p)
                })
                .collect();
            new.sort();
            for p in new {
                let t = std::time::Instant::now();
                match check_layout(base_layout.as_ref(), &p).and_then(|()| apply(&base, &p, prev.as_ref())) {
                    Ok(ap) => {
                        gnr += 1;
                        for name in &ap.fns {
                            provider.insert(name.clone(), gnr);
                        }
                        let nd = if want_direct { direct(&base, &ap.targets) } else { 0 };
                        eprintln!(
                            "[hot] {}: {} functions patched in {:?}{}",
                            p.display(),
                            ap.n,
                            t.elapsed(),
                            if want_direct { format!(", {nd} thunks jump directly") } else { String::new() }
                        );
                        prev = Some(ap.prints);
                        applied.push((ap.mem, ap.len, ap.fns.iter().cloned().collect(), gnr));
                        if free {
                            applied.retain(|&(mem, len, ref fns, g)| {
                                let keep = g + grace > gnr
                                    || !fns.iter().all(|n| provider.get(n).is_some_and(|&pg| pg > g));
                                if !keep {
                                    if protect {
                                        retire(mem, len);
                                    } else {
                                        unsafe { libc::munmap(mem as *mut _, len) };
                                    }
                                }
                                keep
                            });
                        }
                    }
                    Err(e) => eprintln!("[hot] {}: {e}", p.display()),
                }
                seen.insert(p);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
}
