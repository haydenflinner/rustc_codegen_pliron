//! Runtime for `rustc_codegen_pliron` PGO-instrumented binaries
//! (docs/pgo.md). The per-CGU `.init_array` ctor calls
//! [`__pliron_prof_register`] with the CGU's header; an `atexit`
//! handler serializes every registered module's counters into a
//! `pliron-<pid>.prof` (or `$PLIRON_PROFILE_FILE`), merging counts
//! into any file already there.
//!
//! Link `libpliron_prof.a` into an instrumented binary
//! (`-C link-arg=.../libpliron_prof.a` or `-L ... -l pliron_prof`).
//! The same format code backs the `pliron-prof` CLI (`merge`, `dump`).

use std::collections::HashMap;
use std::sync::Mutex;

/// `.prof` magic: "PLPROF01" little-endian — same as pliron-ir-opt's.
pub const MAGIC: u64 = u64::from_le_bytes(*b"PLPROF01");

/// Per-function record: `(sym_hash, cfg_fingerprint, blocks, edges)`;
/// edges are `(pred_block_idx, succ_slot, count)`.
pub type ProfileMap = HashMap<(u64, u64), (Vec<u64>, Vec<(u16, u16, u64)>)>;

/// Header every instrumented CGU emits (see pgo.rs `instrument`):
/// `{magic, ctrs, nctrs, recs_len, recs[..]}` — `recs` trails inline.
#[repr(C)]
pub struct Hdr {
    pub magic: u64,
    pub ctrs: *const u64,
    pub nctrs: u64,
    pub recs_len: u64,
}

/// One serialized function record inside `hdr.recs`:
/// `{sh, fp, ctr_base, nblocks, nedges, pad, edge_keys[nedges]}`.
/// On-disk the header is 36 bytes (no tail padding), so reads go
/// through explicit offsets, not `size_of`.
#[repr(C)]
struct Rec {
    sh: u64,
    fp: u64,
    ctr_base: u64,
    nblocks: u32,
    nedges: u32,
    pad: u32,
}
const REC_LEN: usize = 36;

static HDRS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static REGISTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn flush() {
    let hdrs = HDRS.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: ProfileMap = HashMap::new();
    for &h in hdrs.iter() {
        unsafe { collect(h as *const Hdr, &mut out) };
    }
    if out.is_empty() {
        return;
    }
    let path = std::env::var("PLIRON_PROFILE_FILE")
        .unwrap_or_else(|_| format!("pliron-{}.prof", std::process::id()));
    // Merge-on-write: several runs share one profile file.
    if let Ok(old) = std::fs::read(&path) {
        if let Some(prev) = parse(&old) {
            for (k, (b, e)) in prev {
                let (ob, oe) = out.entry(k).or_default();
                ob.resize(ob.len().max(b.len()), 0);
                for (i, c) in b.iter().enumerate() {
                    ob[i] += c;
                }
                for &(p, s, c) in &e {
                    if let Some(x) = oe.iter_mut().find(|x| x.0 == p && x.1 == s) {
                        x.2 += c;
                    } else {
                        oe.push((p, s, c));
                    }
                }
            }
        }
    }
    let _ = std::fs::write(&path, serialize(&out));
}

/// Read one CGU's counters into `out` (merge-add).
unsafe fn collect(h: *const Hdr, out: &mut ProfileMap) {
    let hdr = &*h;
    if hdr.magic != MAGIC {
        return;
    }
    let recs = (h as *const u8).add(std::mem::size_of::<Hdr>());
    let end = recs.add(hdr.recs_len as usize);
    let mut p = recs;
    while p < end {
        let r = &*(p as *const Rec);
        let nb = r.nblocks as usize;
        let ne = r.nedges as usize;
        let mut keys = Vec::with_capacity(ne);
        let k = (p as *const u8).add(REC_LEN) as *const u32;
        for i in 0..ne {
            let e = *k.add(i);
            keys.push(((e >> 16) as u16, (e & 0xffff) as u16));
        }
        p = p.add((REC_LEN + ne * 4 + 7) & !7);
        let (b, e) = out.entry((r.sh, r.fp)).or_default();
        b.resize(b.len().max(nb), 0);
        for i in 0..nb {
            b[i] += *hdr.ctrs.add(r.ctr_base as usize + i);
        }
        for (i, &(pk, sk)) in keys.iter().enumerate() {
            let c = *hdr.ctrs.add(r.ctr_base as usize + nb + i);
            if let Some(x) = e.iter_mut().find(|x| x.0 == pk && x.1 == sk) {
                x.2 += c;
            } else {
                e.push((pk, sk, c));
            }
        }
    }
}

extern "C" {
    fn atexit(cb: extern "C" fn()) -> i32;
}

/// Called from each instrumented CGU's `.init_array` ctor.
#[no_mangle]
pub extern "C" fn __pliron_prof_register(hdr: *const Hdr) {
    if !REGISTERED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        unsafe { atexit(flush) };
    }
    HDRS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(hdr as usize);
}

// ---------------------------------------------------------------------------
// `.prof` IO — same layout as pliron-ir-opt/src/pgo.rs::serialize/parse
// ---------------------------------------------------------------------------

fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn rd_u32(b: &[u8], p: &mut usize) -> Option<u32> {
    let v = u32::from_le_bytes(b.get(*p..*p + 4)?.try_into().unwrap());
    *p += 4;
    Some(v)
}
fn rd_u64(b: &[u8], p: &mut usize) -> Option<u64> {
    let v = u64::from_le_bytes(b.get(*p..*p + 8)?.try_into().unwrap());
    *p += 8;
    Some(v)
}

pub fn serialize(m: &ProfileMap) -> Vec<u8> {
    let mut out = Vec::new();
    put_u64(&mut out, MAGIC);
    put_u64(&mut out, m.len() as u64);
    for (&(sh, fp), (blocks, edges)) in m {
        put_u64(&mut out, sh);
        put_u64(&mut out, fp);
        put_u32(&mut out, blocks.len() as u32);
        put_u32(&mut out, edges.len() as u32);
        for &c in blocks {
            put_u64(&mut out, c);
        }
        for &(_, _, c) in edges {
            put_u64(&mut out, c);
        }
        for &(p, s, _) in edges {
            put_u32(&mut out, ((p as u32) << 16) | s as u32);
        }
    }
    out
}

pub fn parse(b: &[u8]) -> Option<ProfileMap> {
    let mut p = 0usize;
    if rd_u64(b, &mut p)? != MAGIC {
        return None;
    }
    let n = rd_u64(b, &mut p)?;
    let mut m: ProfileMap = HashMap::new();
    for _ in 0..n {
        let sh = rd_u64(b, &mut p)?;
        let fp = rd_u64(b, &mut p)?;
        let nb = rd_u32(b, &mut p)? as usize;
        let ne = rd_u32(b, &mut p)? as usize;
        let mut blocks = Vec::with_capacity(nb);
        for _ in 0..nb {
            blocks.push(rd_u64(b, &mut p)?);
        }
        let mut edges = Vec::with_capacity(ne);
        let mut counts = Vec::with_capacity(ne);
        for _ in 0..ne {
            counts.push(rd_u64(b, &mut p)?);
        }
        for c in counts {
            let e = rd_u32(b, &mut p)?;
            edges.push(((e >> 16) as u16, (e & 0xffff) as u16, c));
        }
        let (ob, oe) = m.entry((sh, fp)).or_default();
        ob.resize(ob.len().max(nb), 0);
        for (i, c) in blocks.iter().enumerate() {
            ob[i] += c;
        }
        for &(pe, se, c) in &edges {
            if let Some(x) = oe.iter_mut().find(|x| x.0 == pe && x.1 == se) {
                x.2 += c;
            } else {
                oe.push((pe, se, c));
            }
        }
    }
    Some(m)
}
