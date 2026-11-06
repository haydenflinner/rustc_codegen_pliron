//! pliron-inc-ld: incremental Mach-O linker (aarch64-apple-darwin).
//!
//! Drop-in `-Clinker`. The first link execs `cc` with
//! `-Wl,-headerpad,8m` and `-sectcreate __DATA __incpad` injected and
//! `-dead_strip` stripped (so baseline .o sections stay contiguous),
//! then records a manifest beside the output. Later links diff the
//! direct `.o` inputs (rustc's leaf `*.rcgu.o`/`symbols.o`) by content:
//!
//!   * all inputs identical -> touch the output, done
//!   * only leaf objects    -> splice. Sections that verify
//!     changed                  byte-identical (reloc fields masked)
//!                              inside the image keep their addresses;
//!                              same-size changed sections are patched
//!                              in place (they keep dyld fixup
//!                              coverage); the rest are appended to the
//!                              `__TEXT` headerpad (code/rodata) or the
//!                              `__DATA,__incpad` writable pad
//!                              (pointer-bearing data). Moved funcs
//!                              get a `b` trampoline at the old head;
//!                              recorded references to moved symbols
//!                              are patched. Re-signs adhoc.
//!   * anything else        -> full `cc` fallback (always correct)
//!
//! ASLR-safety of appended data: the loader extends the rebase blob —
//! dyld requires rebase opcodes inside `__LINKEDIT`'s file range, so
//! the blob is copied, its `DONE` terminator and pad zeros trimmed,
//! per-slot `SET_SEG_AND_OFFSET`/`ADD_ADDR`/`DO_REBASE` opcodes
//! appended, re-terminated, written at EOF, `__LINKEDIT` grown, and
//! `LC_DYLD_INFO_ONLY` repointed. Pointer data goes to the writable
//! `__DATA` pad because dyld's `str` rebase writes cannot target
//! read-only `__TEXT`. Sections that match no image placement get a
//! reloc-slot fingerprint pass: pointer slots' resolved old-layout
//! addresses locate the section even when all its content is pointers.
//!
//! Detected fallbacks: non-arm64, TLS relocs, unresolved references,
//! pad exhaustion, dropped leaf objects, duplicate extern definitions,
//! fixup records other than classic `LC_DYLD_INFO(_ONLY)` rebases.
//! Unwinding metadata is not updated for appended code (fp-chain
//! fallback still applies).
//!
//! Env: PLIRON_INCLD=0 pass-through, =verbose prints decisions,
//! =full forces a baseline relink.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const PAD: &str = "0x800000";
const DPAD_SIZE: u64 = 0x400000;
const SMALL_FILE: u64 = 1 << 20;

fn env_is(k: &str, v: &str) -> bool {
    std::env::var(k).map(|x| x == v).unwrap_or(false)
}

fn verbose() -> bool {
    std::env::var("PLIRON_INCLD").as_deref() == Ok("verbose")
}
macro_rules! vlog {
    ($($a:tt)*) => { if verbose() { eprintln!("inc-ld: {}", format!($($a)*)); } };
}

// ---------- Mach-O model ----------

const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;
const LC_CODE_SIGNATURE: u32 = 0x1d;
const LC_DYLD_INFO: u32 = 0x22;
const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;
const S_ZEROFILL: u32 = 1;
const N_SECT: u8 = 0x0e;
const N_EXT: u8 = 0x01;
const N_PEXT: u8 = 0x10;

const R_UNSIGNED: u8 = 0;
const R_SUBTRACTOR: u8 = 1;
const R_BRANCH26: u8 = 2;
const R_PAGE21: u8 = 3;
const R_PAGEOFF12: u8 = 4;
const R_GOT_LOAD_PAGE21: u8 = 5;
const R_GOT_LOAD_PAGEOFF12: u8 = 6;
const R_POINTER_TO_GOT: u8 = 7;
/// resolve() marks targets that are pointer cells (keep load semantics);
/// unflagged GOT targets are the sym itself (relax ldr -> add).
const CELL_FLAG: u64 = 1 << 62;
const R_TLV_PAGE21: u8 = 8;
const R_TLV_PAGEOFF12: u8 = 9;

fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn wr64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}
fn wr32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes())
}
fn cstr(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).to_string()
}

#[derive(Clone)]
struct Sect {
    name: String,
    seg: String,
    addr: u64,
    size: u64,
    off: u32,
    alog: u32, // log2 alignment
    reloff: u32,
    nreloc: u32,
    flags: u32,
    resv1: u32, // indirect-table start index (stubs/got/la_symbol_ptr)
}
fn skippable(name: &str) -> bool {
    name.starts_with("__debug")
        || name == "__compact_unwind"
        || name == "__eh_frame"
        || name == "__unwind_info"
        || name == "__LD"
}

impl Sect {
    fn bytes<'a>(&self, b: &'a [u8]) -> &'a [u8] {
        if self.flags & 0xf == S_ZEROFILL || self.size == 0 {
            return &[];
        }
        &b[self.off as usize..self.off as usize + self.size as usize]
    }
    fn writable(&self) -> bool {
        // const-content section names are read-only even in __DATA —
        // appending them into the r-x pad is semantically sound
        self.seg.starts_with("__DATA")
            && !self.seg.contains("CONST")
            && !matches!(
                self.name.as_str(),
                "__const" | "__cstring" | "__ustring" | "__cfstring"
            )
    }
}

#[derive(Clone, Copy)]
struct Reloc {
    off: u64,
    rtype: u8,
    len: u8,
    pcrel: bool,
    ext: bool,
    num: u32, // ext: symnum; non-ext: 1-based section ordinal
}

struct MachO<'a> {
    b: &'a [u8],
    sects: Vec<Sect>,
    syms: Vec<(String, u8, u8, u64)>, // name, n_type, n_sect, n_value
    is_obj: bool,
    end_of_cmds: usize,
    indirect: Vec<u32>, // dysymtab indirect symbol table (sym indices)
    sig: (usize, usize), // LC_CODE_SIGNATURE dataoff/datasize
}

impl<'a> MachO<'a> {
    fn parse(b: &'a [u8]) -> Result<Self, String> {
        if b.len() < 32 || rd32(b, 0) != 0xfeedfacf {
            return Err("not Mach-O 64".into());
        }
        if rd32(b, 4) != 0x0100_000c {
            return Err("not arm64".into());
        }
        let filetype = rd32(b, 12);
        let ncmds = rd32(b, 16) as usize;
        let mut m = MachO {
            b,
            sects: vec![],
            syms: vec![],
            is_obj: filetype == 1,
            end_of_cmds: 0,
            indirect: vec![],
            sig: (0, 0),
        };
        let (mut symoff, mut nsyms, mut stroff) = (0usize, 0usize, 0usize);
        let (mut indoff, mut nind) = (0usize, 0usize);
        let mut off = 32usize;
        for _ in 0..ncmds {
            let size = rd32(b, off + 4) as usize;
            if size < 8 || off + size > b.len() {
                return Err("bad load command".into());
            }
            match rd32(b, off) {
                LC_SEGMENT_64 => {
                    for i in 0..rd32(b, off + 64) as usize {
                        let so = off + 72 + i * 80;
                        m.sects.push(Sect {
                            name: cstr(&b[so..so + 16]),
                            // the section's own segname — .o files carry
                            // everything under one empty-named segment cmd
                            seg: cstr(&b[so + 16..so + 32]),
                            addr: rd64(b, so + 32),
                            size: rd64(b, so + 40),
                            off: rd32(b, so + 48),
                            alog: rd32(b, so + 52),
                            reloff: rd32(b, so + 56),
                            nreloc: rd32(b, so + 60),
                            flags: rd32(b, so + 64),
                            resv1: rd32(b, so + 68),
                        });
                    }
                }
                LC_SYMTAB => {
                    symoff = rd32(b, off + 8) as usize;
                    nsyms = rd32(b, off + 12) as usize;
                    stroff = rd32(b, off + 16) as usize;
                }
                LC_DYSYMTAB => {
                    indoff = rd32(b, off + 56) as usize;
                    nind = rd32(b, off + 60) as usize;
                }
                LC_CODE_SIGNATURE => {
                    m.sig = (
                        rd32(b, off + 8) as usize,
                        rd32(b, off + 12) as usize,
                    );
                }
                _ => {}
            }
            off += size;
        }
        m.end_of_cmds = off;
        for i in 0..nind {
            m.indirect.push(rd32(b, indoff + i * 4));
        }
        for i in 0..nsyms {
            let o = symoff + i * 16;
            let strx = rd32(b, o) as usize;
            let name = if strx != 0 {
                cstr(&b[stroff + strx..])
            } else {
                String::new()
            };
            m.syms.push((name, b[o + 4], b[o + 5], rd64(b, o + 8)));
        }
        Ok(m)
    }
    fn relocs(&self, s: &Sect) -> Vec<Reloc> {
        let mut v = Vec::with_capacity(s.nreloc as usize);
        for i in 0..s.nreloc as usize {
            let o = s.reloff as usize + i * 8;
            let w = rd32(self.b, o + 4);
            v.push(Reloc {
                off: rd32(self.b, o) as u64,
                num: w & 0x00ff_ffff,
                pcrel: (w >> 24) & 1 == 1,
                len: ((w >> 25) & 3) as u8,
                ext: (w >> 27) & 1 == 1,
                rtype: ((w >> 28) & 0xf) as u8,
            });
        }
        v
    }
}

// ---------- manifest ----------

#[derive(Serialize, Deserialize, Default)]
struct Manifest {
    stable: HashMap<String, (u64, u64)>,
    leaf: HashSet<String>,                      // .o content hashes
    addr: HashMap<String, u64>,                 // name -> current addr
    tramps: HashMap<String, Vec<u64>>,          // name -> stale addrs
    appended: HashMap<String, (u64, Vec<RR>)>,  // "sha:sect" -> (addr, relocs)
    pad_used: u64,
    #[serde(default)]
    dpad_used: u64,
    /// the binary image we last produced (rustc's -o scratch path)
    image: String,
    /// leaf object content hash -> its defined extern/private names
    defs: HashMap<String, Vec<String>>,
    /// synthesized GOT cells in the pad: name -> cell addr (holds sym addr)
    #[serde(default)]
    got_cells: HashMap<String, u64>,
    /// "<cgu>:<section>" -> (image addr, size, fxhash of live bytes) —
    /// lets pointer-bearing sections be patched in place (they keep
    /// dyld rebase coverage only at their original address)
    #[serde(default)]
    sect_addr: HashMap<String, (u64, u64, u64)>,
}

/// Recorded extern reloc inside an appended chunk, so a later round can
/// repatch it when its target moves again.
#[derive(Serialize, Deserialize, Clone)]
struct RR {
    off: u64,
    rtype: u8,
    len: u8,
    pcrel: bool,
    addend: i64,
    target: String,
}

fn ext_defs(mo: &MachO) -> Vec<String> {
    mo.syms
        .iter()
        .filter(|(n, t, _, _)| {
            !n.is_empty() && t & 0x0e == N_SECT && t & (N_EXT | N_PEXT) != 0
        })
        .map(|(n, _, _, _)| n.clone())
        .collect()
}

fn fxh(b: &[u8]) -> u64 {
    use rustc_hash::FxHasher;
    use std::hash::Hasher;
    let mut h = FxHasher::default();
    h.write(b);
    h.finish()
}

fn sha(b: &[u8]) -> String {
    format!("{:016x}", fxh(b))
}

/// CGU identity from a leaf .o path: `rtt.4x7nbjvdvr80rr7lvqyzo855h`
/// in `rtt.4x7nbjvdvr80rr7lvqyzo855h.1rhowlb.rcgu.o` — stable across
/// per-build suffixes, used to key in-place section patches.
fn cgu_key(path: &str) -> String {
    let base = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);
    let stem = base.strip_suffix(".rcgu.o").unwrap_or(base);
    match stem.rfind('.') {
        Some(i) => stem[..i].to_string(),
        None => stem.to_string(),
    }
}

/// Stable manifest location: rustc's -o lives in a per-build scratch
/// dir (build/<crate>/<hash>/out/x), so key the manifest off the
/// target dir instead.
fn manifest_path(out: &Path) -> PathBuf {
    let mut p: &Path = out;
    while let Some(par) = p.parent() {
        let name = par.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if matches!(name, "debug" | "release" | "deps")
            || par
                .file_name()
                .is_some_and(|n| n.to_str().is_some_and(|s| s.contains("target-")))
        {
            let stem = out
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("a.out")
                .to_string();
            return par.join(format!(".incld-{stem}.json"));
        }
        p = par;
    }
    PathBuf::from(format!("{}.incld.json", out.display()))
}

// ---------- args ----------

/// (leaf .o paths, stable inputs). `-o`/its value are excluded — the
/// manifest is keyed by that path anyway.
fn split_inputs(args: &[String]) -> (Vec<String>, Vec<String>) {
    let takes_val: HashSet<&str> = [
        "-framework", "-arch", "-isysroot", "-filelist",
        "-exported_symbols_list", "-u", "-rpath", "-install_name",
        "-segaddr", "-stack_size", "-pagezero_size", "-syslibroot",
        "-object_path_lto", "-cache_path_lto", "-temp_dir", "-order_file",
        "-dependency_info", "-sdk_version", "-platform_version",
        "-reexported_symbols_list", "-unexported_symbols_list",
        "-image_base", "-sectorder", "-e", "-sectcreate", "-sectalign",
        "-target", "-weak_reference_mismatches",
    ]
    .into_iter()
    .collect();
    let mut leaf = vec![];
    let mut stable = vec![];
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-o" {
            i += 2;
            continue;
        }
        if takes_val.contains(a.as_str()) {
            stable.push(a.clone());
            if i + 1 < args.len() {
                stable.push(args[i + 1].clone());
            }
            i += 2;
            continue;
        }
        if a.ends_with(".o") && Path::new(a).exists() {
            leaf.push(a.clone());
        } else {
            stable.push(a.clone());
        }
        i += 1;
    }
    (leaf, stable)
}

/// Content-hash small files (response files get rewritten each round);
/// key big ones by (mtime, size).
fn input_key(a: &str) -> (u64, u64) {
    let Ok(m) = Path::new(a).metadata() else {
        return (0, 0);
    };
    let len = m.len();
    if len <= SMALL_FILE && m.is_file() {
        if let Ok(b) = fs::read(a) {
            use rustc_hash::FxHasher;
            use std::hash::Hasher;
            let mut h = FxHasher::default();
            h.write(&b);
            return (u64::MAX, h.finish());
        }
    }
    (
        m.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0),
        len,
    )
}

// ---------- baseline ----------

fn baseline(args: &[String], out: &Path, mf_path: &Path) -> ! {
    // writable pad for appended pointer-bearing sections — dyld only
    // rebases into writable segments, so the __TEXT pad can't host them
    let dpad_file = mf_path.with_extension("dpad");
    if let Ok(f) = fs::File::create(&dpad_file) {
        let _ = f.set_len(DPAD_SIZE);
    }
    let st = Command::new("ld")
        .args(args)
        .args(["-headerpad", PAD])
        .args(["-sectcreate", "__DATA", "__incpad"])
        .arg(&dpad_file)
        .status()
        .expect("exec ld");
    if !st.success() {
        std::process::exit(st.code().unwrap_or(1));
    }
    let _ = fs::copy(out, mf_path.with_extension("bin"));
    match record_manifest(args, out, mf_path) {
        Ok(()) => vlog!("baseline + manifest"),
        Err(e) => eprintln!("pliron-inc-ld: no manifest ({e})"),
    }
    std::process::exit(0);
}

fn record_manifest(args: &[String], out: &Path, mf_path: &Path) -> Result<(), String> {
    let bin = fs::read(out).map_err(|e| e.to_string())?;
    let mo = MachO::parse(&bin)?;
    let mut mf = Manifest::default();
    let (leaf, stable) = split_inputs(args);
    for a in &stable {
        mf.stable
            .insert(a.clone(), if Path::new(a).exists() { input_key(a) } else { (0, 0) });
    }
    for (n, t, _s, v) in &mo.syms {
        if !n.is_empty() && t & 0x0e == N_SECT {
            mf.addr.insert(n.clone(), *v);
        }
    }
    for lp in &leaf {
        let ob = fs::read(lp).map_err(|e| format!("{lp}: {e}"))?;
        let mo = MachO::parse(&ob)?;
        let h = sha(&ob);
        mf.leaf.insert(h.clone());
        mf.defs.insert(h, ext_defs(&mo));
    }
    // where did each leaf object's sections land? enables in-place
    // patching of pointer-bearing sections on later edits
    if let Ok(img) = load_image(out) {
        for lp in &leaf {
            let ob = fs::read(lp).map_err(|e| format!("{lp}: {e}"))?;
            let mo = MachO::parse(&ob)?;
            let cgu = cgu_key(lp);
            for (si, s) in mo.sects.iter().enumerate() {
                if s.size == 0 || s.flags & 0xf == S_ZEROFILL {
                    continue;
                }
                if skippable(&s.name) || s.seg == "__DWARF" {
                    continue;
                }
                let content = s.bytes(&ob);
                if content.len() < 8 {
                    continue;
                }
                let rels = mo.relocs(s);
                let mask = mask_of(content.len(), &rels);
                // all-pointer sections have no stable bytes to match —
                // place them via a defined sym's image address instead
                let placed: Option<u64> = if mask.iter().filter(|&&x| x).count() >= 8 {
                    img.find(&s.name, content, &mask, 0).first().copied()
                } else {
                    mo.syms.iter().find_map(|(n, t, sidx, v)| {
                        if n.is_empty() || t & 0x0e != N_SECT || *sidx == 0 {
                            return None;
                        }
                        if *sidx as usize - 1 != si {
                            return None;
                        }
                        mf.addr
                            .get(n)
                            .map(|&ia| ia.wrapping_sub(v.wrapping_sub(s.addr)))
                    })
                };
                if let Some(a) = placed {
                    if let Some(fo) = img.off_of(a) {
                        mf.sect_addr.insert(
                            format!("{cgu}:{},{}", s.seg, s.name),
                            (a, s.size, fxh(&img.bytes[fo..fo + s.size as usize])),
                        );
                    }
                }
            }
            // pass 2: all-pointer sections have neither fixed bytes to
            // match nor surviving local names — place them by the
            // resolved values of their own pointer slots
            for (_si, s) in mo.sects.iter().enumerate() {
                let key = format!("{cgu}:{},{}", s.seg, s.name);
                if s.size == 0 || mf.sect_addr.contains_key(&key) {
                    continue;
                }
                if skippable(&s.name) || s.seg == "__DWARF" {
                    continue;
                }
                let content = s.bytes(&ob);
                if content.is_empty() {
                    continue;
                }
                let rels = mo.relocs(s);
                let old_sect_base = |ti: usize| -> Option<u64> {
                    let ts = &mo.sects[ti];
                    let k = format!("{cgu}:{},{}", ts.seg, ts.name);
                    mf.sect_addr.get(&k).map(|&(a, _, _)| a)
                };
                let slots = ptr_slots(
                    &mo.syms,
                    &mo.sects,
                    &old_sect_base,
                    &mf.addr,
                    content,
                    &rels,
                );
                if slots.is_empty() {
                    continue;
                }
                let mask = mask_of(content.len(), &rels);
                if let Some(a) =
                    fingerprint(&img, &s.name, content, &mask, s.alog, &slots)
                {
                    if let Some(fo) = img.off_of(a) {
                        mf.sect_addr.insert(
                            key,
                            (a, s.size, fxh(&img.bytes[fo..fo + s.size as usize])),
                        );
                    }
                }
            }
        }
    }
    mf.image = out.to_string_lossy().to_string();
    fs::write(mf_path, serde_json::to_vec(&mf).unwrap()).map_err(|e| e.to_string())
}

// ---------- image ----------

struct Image {
    bytes: Vec<u8>,
    sects: Vec<Sect>,
    segs: Vec<(u64, u64, u64)>, // (vmaddr, filesize, fileoff)
    pad_fileoff: u64,
    pad_size: u64,
    text_delta: u64,
    stubs: HashMap<String, u64>,
    gots: HashMap<String, u64>,
    lazys: HashMap<String, u64>,
    sig: (usize, usize),
    /// (load-cmd file offset, rebase_off, rebase_size) — the classic
    /// dyld rebase blob we extend for appended pointer slots
    rebase: Option<(usize, u32, u32)>,
    /// LC_SEGMENT_64 cmd offset of __LINKEDIT (filesize/vmsize grow
    /// when we append the extended rebase blob at EOF)
    linkedit_cmd: Option<usize>,
    /// (addr, fileoff, size) of the writable __DATA,__incpad pad —
    /// appended pointer-bearing sections live here so dyld can write
    /// rebased values (it will not touch read-only __TEXT)
    dpad: Option<(u64, u64, u64)>,
    /// MH_PIE not set — absolute pointers never slide
    no_pie: bool,
}

fn load_image(out: &Path) -> Result<Image, String> {
    let bytes = fs::read(out).map_err(|e| e.to_string())?;
    let mo = MachO::parse(&bytes)?;
    if mo.is_obj {
        return Err("output is object".into());
    }
    // MH_PIE = 0x200000 in mach_header.flags
    let no_pie = rd32(&bytes, 24) & 0x20_0000 == 0;
    let mut segs = vec![];
    let mut first_sect = u64::MAX;
    let mut text_delta = 0u64;
    let mut off = 32usize;
    let mut rebase = None;
    let mut linkedit_cmd = None;
    for _ in 0..rd32(&bytes, 16) {
        let size = rd32(&bytes, off + 4) as usize;
        let cmd = rd32(&bytes, off);
        if cmd == LC_DYLD_INFO || cmd == LC_DYLD_INFO_ONLY {
            rebase = Some((off, rd32(&bytes, off + 8), rd32(&bytes, off + 12)));
        }
        if cmd == LC_SEGMENT_64 {
            let (va, fo, fs) = (
                rd64(&bytes, off + 24),
                rd64(&bytes, off + 40),
                rd64(&bytes, off + 48),
            );
            segs.push((va, fs, fo));
            let segname = cstr(&bytes[off + 8..off + 24]);
            if segname == "__TEXT" && fo == 0 {
                text_delta = va;
            }
            if segname == "__LINKEDIT" {
                linkedit_cmd = Some(off);
            }
            for i in 0..rd32(&bytes, off + 64) as usize {
                let sfo = rd32(&bytes, off + 72 + i * 80 + 48) as u64;
                if sfo != 0 {
                    first_sect = first_sect.min(sfo);
                }
            }
        }
        off += size;
    }
    if first_sect <= mo.end_of_cmds as u64 {
        return Err("no headerpad".into());
    }
    let end = mo.end_of_cmds as u64;
    let sig = mo.sig;
    let mut stubs = HashMap::new();
    let mut gots = HashMap::new();
    let mut lazys = HashMap::new();
    for s in &mo.sects {
        let (map, esz) = match s.name.as_str() {
            "__stubs" => (&mut stubs, 12u64),
            "__got" => (&mut gots, 8),
            "__la_symbol_ptr" => (&mut lazys, 8),
            _ => continue,
        };
        for i in 0..s.size / esz {
            let sym = mo
                .indirect
                .get(s.resv1 as usize + i as usize)
                .and_then(|&si| mo.syms.get(si as usize).map(|t| t.0.clone()));
            if let Some(n) = sym {
                map.insert(n, s.addr + i * esz);
            }
        }
    }
    let dpad = mo
        .sects
        .iter()
        .find(|s| s.seg == "__DATA" && s.name == "__incpad")
        .map(|s| (s.addr, s.off as u64, s.size));
    let sects = mo.sects;
    Ok(Image {
        bytes,
        sects,
        segs,
        pad_fileoff: end,
        pad_size: first_sect - end,
        text_delta,
        stubs,
        gots,
        lazys,
        sig,
        rebase,
        linkedit_cmd,
        dpad,
        no_pie,
    })
}

impl Image {
    fn off_of(&self, addr: u64) -> Option<usize> {
        for &(va, fs, fo) in &self.segs {
            if addr >= va && addr < va + fs {
                return Some((addr - va + fo) as usize);
            }
        }
        None
    }
    /// Locate `content` (reloc bytes masked off) in same-named sections
    /// and the used pad region. Returns image addresses.
    fn find(&self, name: &str, content: &[u8], mask: &[bool], pad_used: u64) -> Vec<u64> {
        let mut hits = vec![];
        let mut ranges: Vec<(u64, usize, usize, u64)> = vec![]; // va, fileoff, len, step
        for s in &self.sects {
            if s.name == name && s.off != 0 {
                let step = 1u64 << s.alog.min(2);
                ranges.push((s.addr, s.off as usize, s.size as usize, step));
            }
        }
        if pad_used > 0 {
            ranges.push((
                self.text_delta + self.pad_fileoff,
                self.pad_fileoff as usize,
                pad_used as usize,
                4,
            ));
        }
        for (base, fo, len, step) in ranges {
            if len < content.len() || fo + len > self.bytes.len() {
                continue;
            }
            let hay = &self.bytes[fo..fo + len];
            let mut pos = 0usize;
            while pos + content.len() <= hay.len() {
                let w = &hay[pos..pos + content.len()];
                let mut ok = true;
                for i in 0..content.len() {
                    if mask[i] && w[i] != content[i] {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    hits.push(base + pos as u64);
                }
                pos += step as usize;
            }
        }
        hits
    }
}

fn mask_of(len: usize, rels: &[Reloc]) -> Vec<bool> {
    let mut m = vec![true; len];
    for r in rels {
        for i in 0..(1usize << r.len).min(8).max(4) {
            let ix = r.off as usize + i;
            if ix < len {
                m[ix] = false;
            }
        }
    }
    m
}

fn is_ldr(i: u32) -> bool {
    (i & 0x3b00_0000) == 0x3900_0000
}

/// The addend encoded in the reloc field of .o `content`.
fn addend(content: &[u8], r: &Reloc) -> i64 {
    let o = r.off as usize;
    match r.rtype {
        R_BRANCH26 => (((rd32(content, o) & 0x03ff_ffff) << 6) as i32 >> 4) as i64,
        R_PAGE21 | R_GOT_LOAD_PAGE21 => {
            let i = rd32(content, o) as i32;
            let imm = ((i >> 29) & 3) | ((i >> 3) & 0x1ffffc);
            ((imm << 10) >> 10) as i64 * 4096
        }
        R_PAGEOFF12 | R_GOT_LOAD_PAGEOFF12 => {
            let i = rd32(content, o);
            let f = ((i >> 10) & 0xfff) as i64;
            if is_ldr(i) {
                f << ((i >> 30) & 3)
            } else {
                f
            }
        }
        _ => {
            let n = (1usize << r.len).min(8);
            let mut v = 0u64;
            for i in 0..n {
                v |= (content[o + i] as u64) << (i * 8);
            }
            let bits = (n * 8) as u32;
            if bits < 64 && v >> (bits - 1) & 1 == 1 {
                (v as i64) | !((1i64 << bits) - 1)
            } else {
                v as i64
            }
        }
    }
}

/// Write the reloc field at `off` in `content`. `t` = final target
/// (symbol + addend), `site` = place address.
fn apply(content: &mut [u8], off: usize, rtype: u8, len: u8, site: u64, t: i64) -> Result<(), String> {
    if rtype == R_GOT_LOAD_PAGEOFF12 && t as u64 & CELL_FLAG == 0 {
        // relax: the GOT entry is a defined sym — rewrite `ldr` to
        // `add` (idempotent: a repatched site is already an `add`)
        let i = rd32(content, off);
        if (i >> 30) & 3 == 3 && is_ldr(i) {
            wr32(
                content,
                off,
                (i & 0x001f_ffff & !(0xfff << 10)) | 0x9100_0000 | ((t as u32 & 0xfff) << 10),
            );
            return Ok(());
        }
        if i & 0xff00_0000 == 0x9100_0000 {
            // `add xN, xM, #imm` (64-bit, unshifted): rewrite imm12
            wr32(content, off, (i & !(0xfff << 10)) | ((t as u32 & 0xfff) << 10));
            return Ok(());
        }
        return Err(format!("GOT relax: not ldr/add: {i:#010x} at {site:#x}"));
    }
    let t = t & !(CELL_FLAG as i64);
    match rtype {
        R_BRANCH26 => {
            let d = t - site as i64;
            if d & 3 != 0 || d < -(1 << 27) || d >= 1 << 27 {
                return Err("b26 range".into());
            }
            let i = rd32(content, off) & !0x03ff_ffff;
            wr32(content, off, i | (((d >> 2) as u32) & 0x03ff_ffff));
        }
        R_PAGE21 | R_GOT_LOAD_PAGE21 => {
            let d = (t & !0xfff) - (site as i64 & !0xfff);
            let imm = d >> 12;
            if !(-(1 << 20)..1 << 20).contains(&imm) {
                return Err("page21 range".into());
            }
            // immlo at bits[30:29], immhi at bits[23:5] — preserve rd[4:0]
            let i = rd32(content, off) & !((3 << 29) | 0xffffe0);
            wr32(content, off, i | (((imm as u32) & 3) << 29) | (((imm as u32) & 0x1ffffc) << 3));
        }
        R_PAGEOFF12 | R_GOT_LOAD_PAGEOFF12 => {
            let i = rd32(content, off);
            let o12 = (t & 0xfff) as u32;
            if is_ldr(i) {
                let sz = (i >> 30) & 3;
                if o12 & ((1 << sz) - 1) != 0 {
                    return Err("pageoff12 align".into());
                }
                wr32(content, off, (i & !(0xfff << 10)) | ((o12 >> sz) << 10));
            } else {
                wr32(content, off, (i & !(0xfff << 10)) | (o12 << 10));
            }
        }
        R_UNSIGNED => {
            let n = (1usize << len).min(8);
            for i in 0..n {
                content[off + i] = (t as u64 >> (i * 8)) as u8;
            }
        }
        t => return Err(format!("reloc {t}")),
    }
    Ok(())
}

// ---------- main ----------

fn fallback(args: &[String], out: &Path, mf_path: &Path, why: &str) -> ! {
    eprintln!("pliron-inc-ld: full link ({why})");
    baseline(args, out, mf_path)
}

fn main() {
    let t0 = std::time::Instant::now();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if verbose() {
        fs::write("/tmp/incld-args.txt", args.join("\n")).ok();
    }
    if std::env::var("PLIRON_INCLD").as_deref() == Ok("0") {
        let st = Command::new("ld").args(&args).status().expect("exec ld");
        std::process::exit(st.code().unwrap_or(1));
    }
    let Some(out) = args
        .iter()
        .position(|a| a == "-o")
        .map(|i| PathBuf::from(&args[i + 1]))
    else {
        let st = Command::new("ld").args(&args).status().expect("exec ld");
        std::process::exit(st.code().unwrap_or(1));
    };
    let mf_path = manifest_path(&out);
    let bin_path = mf_path.with_extension("bin");
    if env_is("PLIRON_INCLD", "full") {
        baseline(&args, &out, &mf_path);
    }
    if !mf_path.exists() || !bin_path.exists() {
        baseline(&args, &out, &mf_path);
    }
    let mf: Manifest =
        serde_json::from_slice(&fs::read(&mf_path).unwrap_or_default()).unwrap_or_default();
    if mf.leaf.is_empty() {
        eprintln!("inc-ld: manifest empty/unreadable -> baseline");
        baseline(&args, &out, &mf_path);
    }
    vlog!("manifest ok: {} leaf, {} stable", mf.leaf.len(), mf.stable.len());

    vlog!("manifest read: {:?}", t0.elapsed());
    let (leaf, stable) = split_inputs(&args);
    for a in &stable {
        let cur = if Path::new(a).exists() { input_key(a) } else { (0, 0) };
        if mf.stable.get(a) != Some(&cur) {
            eprintln!("inc-ld: stable changed: {a} (was {:?} now {cur:?})", mf.stable.get(a));
            fallback(&args, &out, &mf_path, &format!("input changed: {a}"));
        }
    }
    let mut changed: Vec<(String, String, Vec<u8>)> = vec![];
    let mut same: Vec<Vec<u8>> = vec![];
    let mut present: HashSet<String> = HashSet::new();
    for lp in &leaf {
        let ob = match fs::read(lp) {
            Ok(b) => b,
            Err(_) => fallback(&args, &out, &mf_path, "missing .o"),
        };
        let h = sha(&ob);
        present.insert(h.clone());
        if mf.leaf.contains(&h) {
            same.push(ob);
        } else {
            changed.push((h.clone(), lp.clone(), ob));
        }
    }
    vlog!("input hashing: {:?} ({} leaf objs)", t0.elapsed(), leaf.len());
    if verbose() {
        for lp in &leaf {
            eprintln!("  leaf {lp}");
        }
        for (h, p, _) in &changed {
            eprintln!("  changed {h} {p}");
        }
    }
    let dropped: HashSet<String> = mf.leaf.difference(&present).cloned().collect();
    if changed.is_empty() && dropped.is_empty() {
        if let Ok(f) = fs::OpenOptions::new().write(true).open(&out) {
            f.set_modified(std::time::SystemTime::now()).ok();
        }
        vlog!("no-op");
        std::process::exit(0);
    }
    match splice(&mf, &bin_path, &out, &changed, &same, &dropped) {
        Ok(nmf) => {
            fs::write(&mf_path, serde_json::to_vec(&nmf).unwrap()).ok();
            vlog!("spliced {} objects in {:?}", changed.len(), t0.elapsed());
            if verbose() {
                let b = fs::read(&out).unwrap_or_default();
                vlog!("out-at-exit: len={} sha={}", b.len(), sha(&b));
                vlog!("verify: {}", verify_sig(&b));
            }
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("inc-ld: splice failed: {e}");
            fallback(&args, &out, &mf_path, &e)
        }
    }
}

// ---------- splice ----------

struct CObj<'a> {
    h: String,
    /// cgu identity for in-place section patching
    cgu: String,
    mo: MachO<'a>,
    rels: HashMap<usize, Vec<Reloc>>, // section index -> relocs
    append: Vec<usize>,
    /// (section idx, existing image addr) — rewritten in place, keeps
    /// dyld rebase coverage for absolute pointers
    inplace: Vec<(usize, u64)>,
    /// find-miss sections awaiting fingerprint placement (phase 1b)
    deferred: Vec<usize>,
    sect_base: HashMap<usize, u64>,
}

fn splice(
    mf: &Manifest,
    image: &Path,
    out: &Path,
    changed: &[(String, String, Vec<u8>)],
    same: &[Vec<u8>],
    dropped: &HashSet<String>,
) -> Result<Manifest, String> {
    let t0 = std::time::Instant::now();
    let mut img = load_image(image)?;
    vlog!("load_image: {:?}", t0.elapsed());
    let text_delta = img.text_delta;
    let pad_va = text_delta + img.pad_fileoff;
    let mut pad_used = mf.pad_used;
    if pad_used > img.pad_size {
        return Err("pad accounting".into());
    }
    let mut addr = mf.addr.clone();
    let mut tramps = mf.tramps.clone();
    let mut dirty: Vec<(usize, usize)> = vec![];

    // extern-def names per object, for duplicate-def detection
    let mut def_owner: HashMap<String, usize> = HashMap::new();
    let mut same_mos: Vec<MachO> = Vec::new();
    for bytes in same {
        let mo = MachO::parse(bytes)?;
        for n in ext_defs(&mo) {
            *def_owner.entry(n).or_default() += 1;
        }
        same_mos.push(mo);
    }

    // Phase 1: parse changed objects; place each section (find or append).
    let mut cobjs: Vec<CObj> = Vec::new();
    for (h, path, bytes) in changed {
        let mo = MachO::parse(bytes)?;
        let cgu_key = cgu_key(path);
        if !mo.is_obj {
            return Err("leaf not an object".into());
        }
        for n in ext_defs(&mo) {
            *def_owner.entry(n).or_default() += 1;
        }
        let mut rels = HashMap::new();
        let append = vec![];
        let inplace: Vec<(usize, u64)> = vec![];
        let mut deferred = vec![];
        let mut sb = HashMap::new();
        for (si, s) in mo.sects.iter().enumerate() {
            rels.insert(si, mo.relocs(s));
            if s.size == 0 || s.flags & 0xf == S_ZEROFILL {
                continue;
            }
            if skippable(&s.name) || s.seg == "__DWARF" {
                continue;
            }
            let content = s.bytes(bytes);
            if content.is_empty() {
                continue;
            }
            let mask = mask_of(content.len(), &rels[&si]);
            let hits = if mask.iter().filter(|&&x| x).count() >= 8 {
                img.find(&s.name, content, &mask, pad_used)
            } else {
                vec![]
            };
            match hits.first() {
                Some(&b) => {
                    sb.insert(si, b);
                }
                None => deferred.push(si),
            }
        }
        cobjs.push(CObj {
            h: h.clone(),
            cgu: cgu_key,
            mo,
            rels,
            append,
            inplace,
            deferred,
            sect_base: sb,
        });
    }

    // dropped objects: their defs are dead unless still referenced
    // and not redefined by the current leaf set
    if !dropped.is_empty() {
        let mut cur_defs: HashSet<String> = HashSet::new();
        let mut refs: HashSet<String> = HashSet::new();
        for (h, _, _) in changed {
            if let Some(ds) = mf.defs.get(h) {
                cur_defs.extend(ds.iter().cloned());
            }
        }
        for c in &cobjs {
            for n in ext_defs(&c.mo) {
                cur_defs.insert(n);
            }
        }
        for mo in &same_mos {
            for n in ext_defs(mo) {
                cur_defs.insert(n);
            }
            for rel in mo.sects.iter().flat_map(|s| mo.relocs(s)) {
                if rel.ext {
                    refs.insert(mo.syms[rel.num as usize].0.clone());
                }
            }
        }
        for c in &cobjs {
            for rel in c.rels.values().flatten() {
                if rel.ext {
                    refs.insert(c.mo.syms[rel.num as usize].0.clone());
                }
            }
        }
        for dh in dropped {
            if let Some(ds) = mf.defs.get(dh) {
                for n in ds {
                    if refs.contains(n) && !cur_defs.contains(n) {
                        return Err(format!("dropped def {n} still referenced"));
                    }
                }
            }
        }
    }

    // Phase 1b: deferred sections get placed by reloc fingerprint —
    // all-pointer tables have too few fixed bytes for content
    // matching, but their slots' resolved values (under the OLD
    // layout) pinpoint where ld put them. A hit means the linked
    // bytes are already in the image unchanged, so keep the section
    // there; a miss falls through to in-place/append below.
    for ci in 0..cobjs.len() {
        let mut hits_found: Vec<(usize, u64)> = vec![];
        {
            let c = &cobjs[ci];
            for &si in &c.deferred {
                let s = &c.mo.sects[si];
                let content = s.bytes(c.mo.b);
                let rels = &c.rels[&si];
                // old-layout base of a target's section: its kept
                // address, or this cgu's recorded placement
                let old_sect_base = |ti: usize| -> Option<u64> {
                    let ts = &c.mo.sects[ti];
                    c.sect_base.get(&ti).copied().or_else(|| {
                        let k = format!("{}:{},{}", c.cgu, ts.seg, ts.name);
                        mf.sect_addr.get(&k).map(|&(a, _, _)| a)
                    })
                };
                let slots = ptr_slots(
                    &c.mo.syms,
                    &c.mo.sects,
                    &old_sect_base,
                    &mf.addr,
                    content,
                    rels,
                );
                if slots.is_empty() {
                    continue;
                }
                let mask = mask_of(content.len(), rels);
                if let Some(b) = fingerprint(&img, &s.name, content, &mask, s.alog, &slots) {
                    hits_found.push((si, b));
                }
            }
        }
        for (si, b) in hits_found {
            let c = &mut cobjs[ci];
            c.sect_base.insert(si, b);
            c.deferred.retain(|&x| x != si);
        }
    }

    // Phase 1c: still-deferred sections — pointer-bearing ones patch
    // in place at their recorded address; the rest append to the pad.
    for c in &mut cobjs {
        for si in std::mem::take(&mut c.deferred) {
            let s = &c.mo.sects[si];
            // pointer slots are appendable when we can extend the
            // rebase info (or the image never slides)
            if !c_abs_ptr(&c.rels[&si]) || img.no_pie || img.rebase.is_some() {
                c.append.push(si);
                continue;
            }
            // pointer-bearing sections keep dyld rebase coverage only
            // at their original address — patch in place if known
            let key = format!("{}:{},{}", c.cgu, s.seg, s.name);
            match mf.sect_addr.get(&key) {
                Some(&(a, sz, _)) if sz == s.size && img.off_of(a).is_some() => {
                    c.inplace.push((si, a));
                }
                prev => {
                    let why = match prev {
                        None => format!("no {key}"),
                        Some(&(a, sz, _)) => {
                            format!("{key}: sz {sz}v{} off {:?}", s.size, img.off_of(a))
                        }
                    };
                    return Err(format!(
                        "{}: absolute pointer section unplaced ({why})",
                        s.name
                    ));
                }
            }
        }
    }

    // Phase 2: assign append addresses. Pointer-bearing sections and
    // __DATA-seg sections go to the writable data pad; code and
    // read-only data go to the __TEXT header pad.
    let (dpad_va, dpad_size) = img.dpad.map(|(a, _, s)| (a, s)).unwrap_or((0, 0));
    let mut dpad_used = mf.dpad_used;
    for c in &mut cobjs {
        for &si in &c.append {
            let s = &c.mo.sects[si];
            let ptrs = c_abs_ptr(&c.rels[&si]);
            if s.writable() && !ptrs {
                return Err(format!("{}: writable data moved", s.name));
            }
            let data = s.seg.starts_with("__DATA") || ptrs;
            if data {
                if img.dpad.is_none() {
                    return Err(format!("{}: needs data pad", s.name));
                }
                dpad_used = (dpad_used + 15) & !15;
                if dpad_used + s.size > dpad_size {
                    return Err("dpad exhausted".into());
                }
                c.sect_base.insert(si, dpad_va + dpad_used);
                dpad_used += s.size;
            } else {
                pad_used = (pad_used + 15) & !15;
                if pad_used + s.size > img.pad_size {
                    return Err("pad exhausted".into());
                }
                c.sect_base.insert(si, pad_va + pad_used);
                pad_used += s.size;
            }
        }
    }

    // Phase 3: compute symbol moves (all defs in appended sections)
    // before relocating anything so cross-object refs resolve new addrs.
    let mut moved: HashMap<String, u64> = HashMap::new();
    for c in &cobjs {
        for (n, t, sidx, v) in &c.mo.syms {
            if n.is_empty() || t & 0x0e != N_SECT || t & (N_EXT | N_PEXT) == 0 {
                continue;
            }
            let si = *sidx as usize - 1;
            let is_append = c.append.contains(&si);
            if !is_append && !c.inplace.iter().any(|&(x, _)| x == si) {
                continue;
            }
            let s = &c.mo.sects[si];
            let new_a = c.sect_base[&si] + (*v - s.addr);
            let old_a = addr.get(n).copied().unwrap_or(0);
            if def_owner.get(n).copied().unwrap_or(0) > 1 && old_a != 0 {
                return Err(format!("{n}: duplicate definition moved"));
            }
            if old_a != new_a {
                moved.insert(n.clone(), new_a);
                if is_append {
                    tramps.entry(n.clone()).or_default().push(old_a);
                }
            }
            addr.insert(n.clone(), new_a);
        }
    }

    let got_cells = mf.got_cells.clone();

    // S of an extern reloc target (symbol + addend is added by caller).
    // A sym defined in this object resolves through its section's base
    // (kept or appended) — that covers locals and moved globals alike.
    let resolve = |c: &CObj, r: &Reloc| -> Result<u64, String> {
        let (n, t, sidx, v) = &c.mo.syms[r.num as usize];
        if r.rtype == R_POINTER_TO_GOT {
            return img
                .gots
                .get(n)
                .copied()
                .ok_or_else(|| format!("pointer-to-got: {n}"));
        }
        if matches!(r.rtype, R_GOT_LOAD_PAGE21 | R_GOT_LOAD_PAGEOFF12) {
            // an existing GOT slot stays a memory load (flagged); else
            // resolve to the sym and the caller relaxes ldr -> add
            if let Some(&a) = img.gots.get(n) {
                return Ok(a | CELL_FLAG);
            }
        }
        if t & 0x0e == N_SECT && *sidx > 0 {
            let si = *sidx as usize - 1;
            if let Some(&base) = c.sect_base.get(&si) {
                return Ok(base + (*v - c.mo.sects[si].addr));
            }
        }
        if let Some(&a) = moved.get(n).or_else(|| addr.get(n)) {
            return Ok(a);
        }
        // undefined external: calls go through its stub, data refs
        // through its GOT slot / lazy pointer
        let (map, fl) = match r.rtype {
            R_GOT_LOAD_PAGE21 | R_GOT_LOAD_PAGEOFF12 => (&img.gots, CELL_FLAG),
            R_UNSIGNED => (&img.lazys, 0),
            _ => (&img.stubs, 0),
        };
        map.get(n)
            .map(|&a| a | fl)
            .ok_or_else(|| format!("unresolved extern: {n}"))
    };

    // "<cgu>:<section>" in-place patch key: image base of that section minus its .o addr
    let sect_s = |c: &CObj, num: u32| -> Result<i64, String> {
        let si = num as usize - 1;
        let base = *c
            .sect_base
            .get(&si)
            .ok_or_else(|| format!("sect {num} unplaced"))?;
        Ok(base as i64 - c.mo.sects[si].addr as i64)
    };
    let s_of = |c: &CObj, r: &Reloc| -> Result<i64, String> {
        if r.ext {
            resolve(c, r).map(|x| x as i64)
        } else {
            sect_s(c, r.num)
        }
    };

    // Phase 4: relocate appended and in-place sections.
    let mut rebase_adds: Vec<u64> = vec![];
    let mut emits: Vec<(u64, Vec<u8>)> = vec![];
    let mut new_appended: Vec<(String, u64, Vec<RR>)> = vec![];
    for c in &cobjs {
        let sites: Vec<(usize, bool)> = c
            .append
            .iter()
            .map(|&si| (si, false))
            .chain(c.inplace.iter().map(|&(si, _)| (si, true)))
            .collect();
        for (si, is_inplace) in sites {
            let s = &c.mo.sects[si];
            let base = c.sect_base[&si];
            let mut content = s.bytes(c.mo.b).to_vec();
            let mut recs = vec![];
            let mut done: HashSet<usize> = HashSet::new();
            // mark UNSIGNED relocs paired with a SUBTRACTOR regardless of
            // table order so they are only consumed by the pair logic
            for (ri, r) in c.rels[&si].iter().enumerate() {
                if r.rtype != R_SUBTRACTOR {
                    continue;
                }
                if let Some((uj, _)) = c.rels[&si].iter().enumerate().find(|(j, x)| {
                    *j != ri && x.off == r.off && x.rtype == R_UNSIGNED
                }) {
                    done.insert(uj);
                }
            }
            for (ri, r) in c.rels[&si].iter().enumerate() {
                if done.contains(&ri) {
                    continue;
                }
                match r.rtype {
                    R_TLV_PAGE21 | R_TLV_PAGEOFF12 => return Err("TLS reloc".into()),
                    R_SUBTRACTOR => {
                        let pair = c.rels[&si].iter().enumerate().find(|(j, x)| {
                            *j != ri && x.off == r.off && x.rtype == R_UNSIGNED
                        });
                        let Some((uj, ur)) = pair else {
                            return Err("unpaired SUBTRACTOR".into());
                        };
                        done.insert(uj);
                        // value = S_u + A_u (-P if pcrel) - (S_s + A_s)
                        let mut v = s_of(c, ur)? + addend(&content, ur);
                        if ur.pcrel {
                            v -= (base + r.off) as i64;
                        }
                        v -= s_of(c, r)? + addend(&content, r);
                        let n = (1usize << ur.len).min(8);
                        for i in 0..n {
                            content[r.off as usize + i] = (v as u64 >> (i * 8)) as u8;
                        }
                        continue;
                    }
                    R_UNSIGNED if !r.pcrel && !is_inplace => {
                        if img.no_pie {
                            // fixed addrs — slot stays correct forever
                        } else if img.rebase.is_some() {
                            // slot gets rebase coverage below
                            rebase_adds.push(base + r.off);
                        } else {
                            return Err(format!("{}: absolute pointer", s.name));
                        }
                    }
                    _ => {}
                }
                let a = addend(&content, r);
                let t = s_of(c, r)? + a;
                if r.ext {
                    recs.push(RR {
                        off: r.off,
                        rtype: r.rtype,
                        len: r.len,
                        pcrel: r.pcrel,
                        addend: a,
                        target: c.mo.syms[r.num as usize].0.clone(),
                    });
                }
                apply(&mut content, r.off as usize, r.rtype, r.len, base + r.off, t)?;
            }
            emits.push((base, content));
            new_appended.push((format!("{}:{}", c.h, s.name), base, recs));
        }
    }

    // Phase 5a: repatch earlier appended chunks whose extern targets
    // moved again this round.
    for (_k, (caddr, recs)) in &mf.appended {
        for r in recs {
            let Some(&na) = moved.get(&r.target) else {
                continue;
            };
            let site = caddr + r.off;
            let fo = img
                .off_of(site)
                .ok_or_else(|| format!("appended site {site:#x}"))?;
            let n = (1usize << r.len).min(8).max(4);
            let mut tmp = img.bytes[fo..fo + n].to_vec();
            apply(&mut tmp, 0, r.rtype, r.len, site, na as i64 + r.addend)?;
            img.bytes[fo..fo + n].copy_from_slice(&tmp);
            dirty.push((fo, n));
        }
    }
    // 5b0: patch refs inside CHANGED objects' kept sections: extern
    // refs to moved syms, and section refs into sections that appended.
    for c in &cobjs {
        for (si, s) in c.mo.sects.iter().enumerate() {
            if c.append.contains(&si) {
                continue;
            }
            let Some(&base) = c.sect_base.get(&si) else {
                continue;
            };
            for r in &c.rels[&si] {
                let site = base + r.off;
                let t = if r.ext {
                    match resolve(c, r) {
                        Ok(sa) => sa as i64 + addend(s.bytes(c.mo.b), r),
                        Err(_) => continue,
                    }
                } else {
                    let ti = r.num as usize - 1;
                    if !c.append.contains(&ti) {
                        continue;
                    }
                    c.sect_base[&ti] as i64 - c.mo.sects[ti].addr as i64
                        + addend(s.bytes(c.mo.b), r)
                };
                if matches!(r.rtype, R_TLV_PAGE21 | R_TLV_PAGEOFF12) {
                    return Err("TLS reloc".into());
                }
                let fo = img
                    .off_of(site)
                    .ok_or_else(|| format!("ref site {site:#x}"))?;
                let n = (1usize << r.len).min(8).max(4);
                let mut tmp = img.bytes[fo..fo + n].to_vec();
                apply(&mut tmp, 0, r.rtype, r.len, site, t)?;
                img.bytes[fo..fo + n].copy_from_slice(&tmp);
                dirty.push((fo, n));
            }
        }
    }

    // 5b: patch recorded refs in unchanged objects to moved syms.
    for (bytes, mo) in same.iter().zip(&same_mos) {
        let h = sha(bytes);
        for s in &mo.sects {
            if s.size == 0 || s.name.starts_with("__debug") || s.flags & 0xf == S_ZEROFILL {
                continue;
            }
            let content = s.bytes(bytes);
            if content.is_empty() {
                continue;
            }
            let rels = mo.relocs(s);
            let needs_patch = rels
                .iter()
                .any(|r| r.ext && moved.contains_key(&mo.syms[r.num as usize].0));
            if !needs_patch {
                continue;
            }
            let mask = mask_of(content.len(), &rels);
            let bases = if mask.iter().filter(|&&x| x).count() >= 8 {
                img.find(&s.name, content, &mask, mf.pad_used)
            } else {
                // find base via appended record (was appended earlier)
                mf.appended
                    .get(&format!("{h}:{}", s.name))
                    .map(|(a, _)| vec![*a])
                    .unwrap_or_default()
            };
            if bases.is_empty() {
                return Err(format!("{}: unchanged section unplaced", s.name));
            }
            for base in bases {
                for r in &rels {
                    if !r.ext {
                        continue;
                    }
                    let name = &mo.syms[r.num as usize].0;
                    let Some(&na) = moved.get(name) else {
                        continue;
                    };
                    let site = base + r.off;
                    let fo = img
                        .off_of(site)
                        .ok_or_else(|| format!("ref site {site:#x}"))?;
                    let a = addend(content, r);
                    let n = (1usize << r.len).min(8).max(4);
                    let mut tmp = img.bytes[fo..fo + n].to_vec();
                    apply(&mut tmp, 0, r.rtype, r.len, site, na as i64 + a)?;
                    img.bytes[fo..fo + n].copy_from_slice(&tmp);
                    dirty.push((fo, n));
                }
            }
        }
    }
    // 5c: GOT slots pointing at moved syms.
    let mut stale: Vec<(u64, u64)> = vec![];
    for (n, &new_a) in &moved {
        for &o in tramps.get(n).into_iter().flatten() {
            if o != 0 {
                stale.push((o, new_a));
            }
        }
    }
    if !stale.is_empty() {
        for s in &img.sects {
            if s.name != "__got" {
                continue;
            }
            let mut fo = s.off as usize;
            while fo + 8 <= s.off as usize + s.size as usize {
                let cur = rd64(&img.bytes, fo);
                if let Some(&(_, na)) = stale.iter().find(|(o, _)| *o == cur) {
                    img.bytes[fo..fo + 8].copy_from_slice(&na.to_le_bytes());
                    dirty.push((fo, 8));
                }
                fo += 8;
            }
        }
    }
    // 5d: trampolines at stale addrs of moved code syms.
    for (n, &new_a) in &moved {
        for &old_a in tramps.get(n).into_iter().flatten() {
            if old_a == 0 || old_a == new_a {
                continue;
            }
            match img
                .sects
                .iter()
                .find(|s| old_a >= s.addr && old_a < s.addr + s.size && s.off != 0)
            {
                Some(sec) => {
                    let code = sec.name == "__text"
                        || sec.name == "__stubs"
                        || sec.flags & 0x8000_0000 != 0;
                    if !code {
                        continue; // data: refs already patched
                    }
                    let fo = img.off_of(old_a).ok_or("tramp unmapped")?;
                    write_tramp(&mut img.bytes, fo, old_a, new_a)?;
                    dirty.push((fo, 8));
                }
                None => {
                    // previously appended addr lives in the pad
                    let fo = img
                        .off_of(old_a)
                        .filter(|&f| f >= img.pad_fileoff as usize)
                        .ok_or_else(|| format!("{n}: stale addr unmapped"))?;
                    write_tramp(&mut img.bytes, fo, old_a, new_a)?;
                    dirty.push((fo, 8));
                }
            }
        }
    }
    vlog!("splice phases 1-5: {:?}", t0.elapsed());
    // Phase 6: write appended bytes, out, resign.
    for (base, content) in &emits {
        let fo = (*base - text_delta) as usize;
        img.bytes[fo..fo + content.len()].copy_from_slice(content);
        dirty.push((fo, content.len()));
    }
    // appended pointer slots need dyld rebase coverage: copy the
    // existing rebase blob into the pad, append ops for the new slots,
    // and repoint LC_DYLD_INFO at it
    if !rebase_adds.is_empty() {
        let Some((cmdfo, roff, rsize)) = img.rebase else {
            return Err("no rebase info for pointer append".into());
        };
        rebase_adds.sort_unstable();
        rebase_adds.dedup();
        // the blob must live inside __LINKEDIT's file range — append
        // it at EOF (after the code signature, which keeps its dataoff
        // and codeLimit, so in-process resign still covers [0, sig))
        // the old stream ends with REBASE_OPCODE_DONE (0x00) then pad
        // zeros — trim them so our ops continue the live stream, then
        // terminate with our own DONE
        let old = &img.bytes[roff as usize..roff as usize + rsize as usize];
        let live = old.iter().rposition(|&b| b != 0).map(|i| i + 1).unwrap_or(0);
        if live == old.len() {
            return Err("rebase blob lacks terminator pad".into());
        }
        let mut blob = old[..live].to_vec();
        enc_rebase(&img.segs, &rebase_adds, &mut blob);
        blob.push(0); // REBASE_OPCODE_DONE
        let bfo = (img.bytes.len() + 7) & !7;
        img.bytes.resize(bfo + blob.len(), 0);
        img.bytes[bfo..bfo + blob.len()].copy_from_slice(&blob);
        dirty.push((bfo, blob.len()));
        wr32(&mut img.bytes, cmdfo + 8, bfo as u32);
        wr32(&mut img.bytes, cmdfo + 12, blob.len() as u32);
        dirty.push((cmdfo + 8, 8));
        // grow __LINKEDIT to cover the appended region
        let le = img.linkedit_cmd.ok_or("no __LINKEDIT")?;
        let le_off = rd64(&img.bytes, le + 40);
        let newfs = img.bytes.len() as u64 - le_off;
        wr64(&mut img.bytes, le + 48, newfs);
        wr64(&mut img.bytes, le + 32, (newfs + 0x3fff) & !0x3fff);
        dirty.push((le + 32, 24));
        vlog!("rebase: +{} slots -> {:#x} ({}B)", rebase_adds.len(), bfo, blob.len());
    }
    // recompute code-directory hashes for the pages we touched, then
    // sparse write: clone the pristine image and pwrite dirty regions
    let fast_sign = match resign(&mut img, &mut dirty) {
        Ok(()) => true,
        Err(e) => {
            vlog!("resign: {e}");
            false
        }
    };
    fs::copy(image, out).map_err(|e| e.to_string())?;
    {
        use std::os::unix::fs::FileExt;
        let f = fs::File::options()
            .write(true)
            .open(out)
            .map_err(|e| e.to_string())?;
        for &(fo, n) in &dirty {
            f.write_all_at(&img.bytes[fo..fo + n], fo as u64)
                .map_err(|e| e.to_string())?;
        }
    }
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(out, fs::Permissions::from_mode(0o755));
    }
    vlog!("write-out: {:?} ({} dirty regions)", t0.elapsed(), dirty.len());
    if !fast_sign {
        sign(out)?;
    }
    vlog!("sign: {:?} (in-process: {})", t0.elapsed(), fast_sign);
    // self-check: an invalid signature must never ship. If the fast
    // resign left bad slots, subprocess-sign; if that still fails, mark
    // the manifest so the next invocation re-baselines.
    {
        let b = fs::read(out).map_err(|e| e.to_string())?;
        let v = verify_sig(&b);
        if v != "ok" {
            vlog!("verify after write: {v}; subprocess sign");
            sign(out)?;
            let b = fs::read(out).map_err(|e| e.to_string())?;
            let v = verify_sig(&b);
            if v != "ok" {
                vlog!("verify still bad: {v}; manifest invalidated");
                return Err(format!("signature: {v}"));
            }
        }
    }
    // refresh our copy of the image for the next round
    let mfp = manifest_path(out);
    fs::copy(out, mfp.with_extension("bin")).ok();

    let mut appended = mf.appended.clone();
    for (k, a, recs) in new_appended {
        appended.insert(k, (a, recs));
    }
    // remember where every changed object's sections landed so later
    // rounds can patch pointer-bearing sections in place
    let mut sect_addr = mf.sect_addr.clone();
    for c in &cobjs {
        for (si, s) in c.mo.sects.iter().enumerate() {
            if !c.sect_base.contains_key(&si) {
                continue;
            }
            let &base = &c.sect_base[&si];
            let Some(fo) = img.off_of(base) else {
                continue;
            };
            let sz = s.size as usize;
            if fo + sz > img.bytes.len() {
                continue;
            }
            sect_addr.insert(
                format!("{}:{},{}", c.cgu, s.seg, s.name),
                (base, s.size, fxh(&img.bytes[fo..fo + sz])),
            );
        }
    }
    Ok(Manifest {
        sect_addr,
        stable: mf.stable.clone(),
        leaf: {
            let mut s = mf.leaf.clone();
            for (h, _, _) in changed {
                s.insert(h.clone());
            }
            s
        },
        addr,
        tramps,
        appended,
        got_cells,
        pad_used,
        dpad_used,
        image: out.to_string_lossy().to_string(),
        defs: {
            let mut d = mf.defs.clone();
            for (h, _, bytes) in changed {
                if let Ok(mo) = MachO::parse(bytes) {
                    d.insert(h.clone(), ext_defs(&mo));
                }
            }
            d
        },
    })
}

/// scan image sections named `name` for a window where every pointer
/// slot holds its expected value and every non-reloc byte matches.
/// Returns the image address of a unique hit.
fn fingerprint(
    img: &Image,
    name: &str,
    content: &[u8],
    mask: &[bool],
    alog: u32,
    slots: &[(u64, u64)],
) -> Option<u64> {
    let step = 1usize << alog.min(2);
    let mut cand: Option<u64> = None;
    for is in &img.sects {
        if is.name != name || is.off == 0 || is.size < content.len() as u64 {
            continue;
        }
        let (mut pos, end) = (0usize, (is.size - content.len() as u64) as usize);
        while pos <= end {
            let fo = is.off as usize + pos;
            if fo + content.len() > img.bytes.len() {
                break;
            }
            if slots
                .iter()
                .all(|&(ro, v)| rd64(&img.bytes, fo + ro as usize) == v)
            {
                let hay = &img.bytes[fo..fo + content.len()];
                if mask
                    .iter()
                    .enumerate()
                    .all(|(i, &m)| !m || hay[i] == content[i])
                {
                    let base = is.addr + pos as u64;
                    if cand.replace(base).is_some() {
                        return None; // ambiguous
                    }
                }
            }
            pos += step;
        }
    }
    cand
}

/// expected stored value (old layout) for one pointer slot, or None
/// if its target's old address is unknown. `old_sect_base` maps an
/// object section index to its old image address.
fn slot_expect(
    c_syms: &[(String, u8, u8, u64)],
    c_sects: &[Sect],
    old_sect_base: &dyn Fn(usize) -> Option<u64>,
    mf_addr: &HashMap<String, u64>,
    content: &[u8],
    r: &Reloc,
) -> Option<u64> {
    let t = if r.ext {
        let (n, ty, sidx, v) = &c_syms[r.num as usize];
        mf_addr.get(n).copied().or_else(|| {
            if ty & 0x0e != N_SECT || *sidx == 0 {
                return None;
            }
            let ti = *sidx as usize - 1;
            old_sect_base(ti)
                .map(|b| b.wrapping_add(v.wrapping_sub(c_sects[ti].addr)))
        })
    } else {
        old_sect_base(r.num.wrapping_sub(1) as usize)
    };
    t.map(|t| (t as i64 + addend(content, r)) as u64)
}

/// expected slot values for all absolute-pointer relocs in a section
fn ptr_slots(
    c_syms: &[(String, u8, u8, u64)],
    c_sects: &[Sect],
    old_sect_base: &dyn Fn(usize) -> Option<u64>,
    mf_addr: &HashMap<String, u64>,
    content: &[u8],
    rels: &[Reloc],
) -> Vec<(u64, u64)> {
    rels.iter()
        .filter(|r| {
            r.rtype == R_UNSIGNED
                && !r.pcrel
                && r.len == 3
                && !rels
                    .iter()
                    .any(|x| x.rtype == R_SUBTRACTOR && x.off == r.off)
        })
        .filter_map(|r| {
            slot_expect(c_syms, c_sects, old_sect_base, mf_addr, content, r)
                .map(|v| (r.off, v))
        })
        .collect()
}

fn uleb(v: u64, out: &mut Vec<u8>) {
    let mut v = v;
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

/// encode rebase ops for `slots` (sorted image addrs): SET_TYPE +
/// SET_SEG_AND_OFFSET per segment, ADD_ADDR + DO_REBASE per slot
fn enc_rebase(segs: &[(u64, u64, u64)], slots: &[u64], out: &mut Vec<u8>) {
    out.push(0x10 | 1); // REBASE_OPCODE_SET_TYPE_IMM, POINTER
    let mut cur_seg = usize::MAX;
    let mut cur = 0u64; // position (offset within cur_seg)
    for &a in slots {
        let Some((idx, &(sva, _, _))) = segs
            .iter()
            .enumerate()
            .find(|(_, &(va, fs, _))| a >= va && a < va + fs)
        else {
            continue;
        };
        let off = a - sva;
        if idx != cur_seg {
            out.push(0x20 | idx as u8); // SET_SEG_AND_OFFSET_ULEB
            uleb(off, out);
            cur_seg = idx;
        } else {
            out.push(0x40); // ADD_ADDR_ULEB
            uleb(off - cur, out);
        }
        out.push(0x60 | 1); // DO_REBASE_IMM_TIMES(1) — advances pos by 8
        cur = off + 8;
    }
}

/// any R_UNSIGNED !pcrel reloc not consumed by a SUBTRACTOR pair —
/// such a slot needs dyld rebase coverage and cannot be appended
fn c_abs_ptr(rels: &[Reloc]) -> bool {
    rels.iter().any(|r| {
        r.rtype == R_UNSIGNED
            && !r.pcrel
            && !rels
                .iter()
                .any(|x| x.rtype == R_SUBTRACTOR && x.off == r.off)
    })
}

fn write_tramp(b: &mut [u8], fo: usize, from: u64, to: u64) -> Result<(), String> {
    let d = to as i64 - from as i64;
    if d & 3 != 0 || d < -(1 << 27) || d >= 1 << 27 {
        return Err("tramp range".into());
    }
    wr32(b, fo, 0x1400_0000 | (((d >> 2) as u32) & 0x03ff_ffff));
    Ok(())
}

/// Recompute CodeDirectory page hashes covering the dirty regions and
/// append the updated hash bytes to `dirty` so they reach `out`.
/// Ad-hoc signatures only: falls back to the codesign subprocess shape
/// errors, so callers should keep sign() as a fallback.
fn resign(img: &mut Image, dirty: &mut Vec<(usize, usize)>) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let (soff, ssize) = img.sig;
    if soff == 0 || soff + ssize > img.bytes.len() {
        return Err("no sig".into());
    }
    let rdbe = |o: usize| -> u32 { u32::from_be_bytes(img.bytes[o..o + 4].try_into().unwrap()) };
    if rdbe(soff) != 0xfade_0cc0 {
        return Err("no superblob".into());
    }
    let count = rdbe(soff + 8) as usize;
    let mut cd_off = None;
    for i in 0..count {
        let t = rdbe(soff + 12 + i * 8);
        let bo = rdbe(soff + 16 + i * 8) as usize;
        if t == 0 && rdbe(soff + bo) == 0xfade_0c02 {
            cd_off = Some(soff + bo);
        }
    }
    let cd = cd_off.ok_or("no codedirectory")?;
    let hash_off = rdbe(cd + 16) as usize;
    let n_code = rdbe(cd + 28) as usize;
    let code_lim = rdbe(cd + 32) as usize;
    let hash_size = img.bytes[cd + 36] as usize;
    let page_log = img.bytes[cd + 39] as usize;
    if hash_size != 32 || img.bytes[cd + 37] != 2 {
        return Err("cd not sha256".into());
    }
    let pgsz = 1usize << page_log;
    let mut pages: std::collections::BTreeSet<usize> = Default::default();
    for &(fo, n) in dirty.iter() {
        if fo >= code_lim {
            continue;
        }
        for p in fo / pgsz..=(fo + n - 1).min(code_lim - 1) / pgsz {
            pages.insert(p);
        }
    }
    for p in pages {
        if p >= n_code {
            return Err("dirty beyond codeLimit".into());
        }
        let start = p * pgsz;
        let end = (start + pgsz).min(code_lim);
        let h = Sha256::digest(&img.bytes[start..end]);
        let slot = cd + hash_off + p * hash_size;
        img.bytes[slot..slot + 32].copy_from_slice(&h);
        dirty.push((slot, 32));
    }
    Ok(())
}

/// Verify every code slot of the adhoc signature in `b`.
fn verify_sig(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let Ok(mo) = MachO::parse(b) else {
        return "parse fail".into();
    };
    let (soff, _) = mo.sig;
    if soff == 0 {
        return "no sig".into();
    }
    let rdbe = |o: usize| -> u32 { u32::from_be_bytes(b[o..o + 4].try_into().unwrap()) };
    let count = rdbe(soff + 8) as usize;
    let mut cd = None;
    for i in 0..count {
        let t = rdbe(soff + 12 + i * 8);
        let bo = rdbe(soff + 16 + i * 8) as usize;
        if t == 0 && rdbe(soff + bo) == 0xfade_0c02 {
            cd = Some(soff + bo);
        }
    }
    let Some(cd) = cd else { return "no cd".into() };
    let hash_off = rdbe(cd + 16) as usize;
    let n_code = rdbe(cd + 28) as usize;
    let code_lim = rdbe(cd + 32) as usize;
    let hsz = b[cd + 36] as usize;
    let pg = 1usize << b[cd + 39];
    let mut bad = vec![];
    for p in 0..n_code {
        let start = p * pg;
        let end = (start + pg).min(code_lim).min(b.len());
        let h = Sha256::digest(&b[start..end]);
        if b[cd + hash_off + p * hsz..cd + hash_off + p * hsz + hsz] != h[..] {
            bad.push(p);
        }
    }
    if bad.is_empty() {
        "ok".into()
    } else {
        format!("bad slots: {:?}", &bad[..bad.len().min(12)])
    }
}

fn sign(out: &Path) -> Result<(), String> {
    let st = Command::new("codesign")
        .args(["--sign", "-", "--force"])
        .arg(out)
        .status()
        .map_err(|e| e.to_string())?;
    if st.success() {
        Ok(())
    } else {
        Err("codesign".into())
    }
}
