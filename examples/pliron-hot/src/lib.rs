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

struct BaseSym {
    addr: usize,
    writable: bool,
    /// Only one definition of this name, so a patch static can safely alias it.
    unique: bool,
}

pub struct Base {
    syms: HashMap<String, BaseSym>,
    end: usize,
}

impl Base {
    pub fn load() -> Result<Base, String> {
        let bytes = std::fs::read("/proc/self/exe").map_err(|e| e.to_string())?;
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
        let anchor = raw.get("pliron_hot_anchor").ok_or("executable has no symbol table")?.0;
        let slide = (pliron_hot_anchor as *const () as usize).wrapping_sub(anchor);
        let end = f.segments().map(|s| (s.address() + s.size()) as usize).max().unwrap_or(0).wrapping_add(slide);
        let syms = raw
            .into_iter()
            .map(|(k, (a, w, _, n))| (k, BaseSym { addr: a.wrapping_add(slide), writable: w, unique: n == 1 }))
            .collect();
        Ok(Base { syms, end })
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        if let Some(s) = self.syms.get(name) {
            return Some(s.addr);
        }
        let c = std::ffi::CString::new(name).ok()?;
        let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
        (!p.is_null()).then_some(p as usize)
    }
}

fn is_mangled(n: &str) -> bool {
    n.starts_with("_R") || n.starts_with("_ZN")
}

fn align(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

/// Map `len` RWX bytes within +-2GiB of the executable so PC32 relocations reach it.
fn map_near(end: usize, len: usize) -> Result<usize, String> {
    let len = align(len, 4096);
    let mut hint = align(end, 1 << 21) + (64 << 20);
    for _ in 0..24 {
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

pub fn fingerprints(f: &object::File) -> Prints {
    fn sec_key(f: &object::File, si: SectionIndex, h: &mut DefaultHasher, deep: bool) {
        let Ok(s) = f.section_by_index(si) else { return };
        s.data().unwrap_or(&[]).hash(h);
        for (o, r) in s.relocations() {
            o.hash(h);
            r.addend().hash(h);
            if let RelocationFlags::Elf { r_type } = r.flags() {
                r_type.hash(h);
            }
            match r.target() {
                RelocationTarget::Symbol(i) => {
                    let Ok(t) = f.symbol_by_index(i) else { continue };
                    let name = t.name().unwrap_or("");
                    if t.is_undefined() || t.kind() == SymbolKind::Text || is_mangled(name) {
                        name.hash(h);
                    } else if let (Some(ti), true) = (t.section_index(), deep) {
                        t.address().hash(h);
                        sec_key(f, ti, h, false);
                    }
                }
                RelocationTarget::Section(ti) if deep => sec_key(f, ti, h, false),
                _ => {}
            }
        }
    }
    let mut out = HashMap::new();
    for sym in f.symbols() {
        if sym.kind() != SymbolKind::Text || !sym.is_definition() {
            continue;
        }
        let (Ok(name), Some(si)) = (sym.name(), sym.section_index()) else { continue };
        let mut h = DefaultHasher::new();
        sec_key(f, si, &mut h, true);
        out.insert(name.to_string(), h.finish());
    }
    out
}

/// Load one patch object, repointing the slots of functions that differ from
/// `prev`. Returns how many were repointed and this object's fingerprints.
pub fn apply(base: &Base, path: &Path, prev: Option<&Prints>) -> Result<(usize, Prints), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let f = object::File::parse(&*bytes).map_err(|e| e.to_string())?;
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
    let mut n = 0;
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
            n += 1;
        }
    }
    Ok((n, prints))
}

/// If `PLIRON_HOT_DIR` is set, apply every `*.o` that appears in it, in name order.
pub fn start() {
    let Some(dir) = std::env::var_os("PLIRON_HOT_DIR") else { return };
    let dir = std::path::PathBuf::from(dir);
    std::thread::spawn(move || {
        let mut prev = std::fs::read(dir.join("base.ref")).ok().and_then(|b| object::File::parse(&*b).ok().map(|f| fingerprints(&f)));
        let base = match Base::load() {
            Ok(b) => b,
            Err(e) => return eprintln!("[hot] disabled: {e}"),
        };
        let mut seen = HashSet::new();
        loop {
            let mut new: Vec<_> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "o") && !seen.contains(p))
                .collect();
            new.sort();
            for p in new {
                let t = std::time::Instant::now();
                match apply(&base, &p, prev.as_ref()) {
                    Ok((n, prints)) => {
                        prev = Some(prints);
                        eprintln!("[hot] {}: {n} functions patched in {:?}", p.display(), t.elapsed())
                    }
                    Err(e) => eprintln!("[hot] {}: {e}", p.display()),
                }
                seen.insert(p);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
}
