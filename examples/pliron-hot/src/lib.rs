//! In-process loader for rustc_codegen_pliron hot patches.
//!
//! The base binary is built with `PLIRON_HOT=<crate>` so each of that crate's
//! functions is a thunk through a `__hot_slot.<sym>` pointer. A patch is the
//! same crate recompiled normally to one relocatable object. The loader maps it
//! near the executable, resolves undefined symbols against the running
//! executable's own symbol table (then `dlsym`), binds writable statics to the
//! live copies so state survives, registers its `.eh_frame`, and repoints the
//! slots of every function it defines.

use object::elf::*;
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol, RelocationFlags, RelocationTarget, SectionIndex, SectionKind, SymbolKind};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn pliron_hot_anchor() {}

struct BaseSym {
    addr: usize,
    writable: bool,
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
            raw.insert(name.to_string(), (s.address() as usize, writable));
        }
        let anchor = raw.get("pliron_hot_anchor").ok_or("executable has no symbol table")?.0;
        let slide = (pliron_hot_anchor as *const () as usize).wrapping_sub(anchor);
        let end = f.segments().map(|s| (s.address() + s.size()) as usize).max().unwrap_or(0).wrapping_add(slide);
        let syms = raw.into_iter().map(|(k, (a, w))| (k, BaseSym { addr: a.wrapping_add(slide), writable: w })).collect();
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

/// Load one patch object; returns the number of functions repointed.
pub fn apply(base: &Base, path: &Path) -> Result<usize, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let f = object::File::parse(&*bytes).map_err(|e| e.to_string())?;

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
                Some(b) if b.writable && sym.kind() == SymbolKind::Data => b.addr,
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
        if let Some(slot) = base.syms.get(&format!("__hot_slot.{name}")) {
            let new = mem + place[&sym.section_index().unwrap()] + sym.address() as usize;
            unsafe { (*(slot.addr as *const AtomicUsize)).store(new, Ordering::Release) };
            n += 1;
        }
    }
    Ok(n)
}

/// If `PLIRON_HOT_DIR` is set, apply every `*.o` that appears in it, in name order.
pub fn start() {
    let Some(dir) = std::env::var_os("PLIRON_HOT_DIR") else { return };
    std::thread::spawn(move || {
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
                match apply(&base, &p) {
                    Ok(n) => eprintln!("[hot] {}: {n} functions patched in {:?}", p.display(), t.elapsed()),
                    Err(e) => eprintln!("[hot] {}: {e}", p.display()),
                }
                seen.insert(p);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
}
