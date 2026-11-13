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

/// Raw per-process dump magic written by the `no_std` runtime
/// (`tools/pliron-prof-rt`): "PLRAW01" little-endian.
pub const RAW_MAGIC: u64 = u64::from_le_bytes(*b"PLRAW01\0");

/// Per-function record: `(sym_hash, cfg_fingerprint, blocks, edges)`;
/// edges are `(pred_block_idx, succ_slot, count)`.
pub type ProfileMap = HashMap<(u64, u64), (Vec<u64>, Vec<(u16, u16, u64)>)>;

/// Header magic of a CGU with the indirect-call extension, and the
/// `.prof` trailer magic for the sites (pgo.rs).
pub const MAGIC2: u64 = u64::from_le_bytes(*b"PLPROF02");
pub const ICALL_MAGIC: u64 = u64::from_le_bytes(*b"PLICALL2");
pub const SLOTS: usize = 4;
const SITE_BYTES: usize = (SLOTS * 2 + 1) * 8;
const EXT_BYTES: usize = 72;

/// One callee an indirect-call site reached (pgo.rs `ProfTarget`).
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub name: String,
    pub crate_id: u64,
    pub crate_hash: u64,
    pub cgu: u64,
    pub flags: u32,
    pub count: u64,
}

/// One site: calls that found no slot or no name, and the named targets.
#[derive(Clone, Debug, Default)]
pub struct Site {
    pub other: u64,
    pub targets: Vec<Target>,
}

/// Per-function indirect-call sites, keyed like [`ProfileMap`].
pub type IcallMap = HashMap<(u64, u64), Vec<Site>>;

fn merge_sites(into: &mut Vec<Site>, from: &[Site]) {
    if into.len() < from.len() {
        into.resize(from.len(), Site::default());
    }
    for (i, s) in from.iter().enumerate() {
        let d = &mut into[i];
        d.other += s.other;
        for t in &s.targets {
            match d
                .targets
                .iter_mut()
                .find(|x| x.name == t.name && x.crate_id == t.crate_id && x.cgu == t.cgu)
            {
                Some(x) => {
                    x.count += t.count;
                    x.flags |= t.flags;
                }
                None => d.targets.push(t.clone()),
            }
        }
    }
}

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
#[allow(dead_code)]
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
    // Same bytes the `no_std` runtime writes, folded right here.
    let mut raw = RAW_MAGIC.to_le_bytes().to_vec();
    for &h in hdrs.iter() {
        unsafe { dump(h as *const Hdr, &mut raw) };
    }
    let mut out: ProfileMap = HashMap::new();
    let mut ic: IcallMap = HashMap::new();
    parse_raw_all(&raw, &mut out, &mut ic);
    if out.is_empty() {
        return;
    }
    let path = std::env::var("PLIRON_PROFILE_FILE")
        .unwrap_or_else(|_| format!("pliron-{}.prof", std::process::id()));
    // Merge-on-write: several runs share one profile file.
    if let Ok(old) = std::fs::read(&path) {
        load_all(&old, &mut out, &mut ic);
    }
    let _ = std::fs::write(&path, serialize_all(&out, &ic));
}

/// Append one CGU's raw payload (see pliron-prof-rt) to `out`.
unsafe fn dump(h: *const Hdr, out: &mut Vec<u8>) {
    let hdr = &*h;
    let v2 = hdr.magic == MAGIC2;
    if hdr.magic != MAGIC && !v2 {
        return;
    }
    let hb = std::mem::size_of::<Hdr>();
    let recs_len = hdr.recs_len as usize;
    let mut p: Vec<u8> = Vec::new();
    p.extend_from_slice(std::slice::from_raw_parts(h as *const u8, hb + recs_len));
    p.extend_from_slice(std::slice::from_raw_parts(hdr.ctrs as *const u8, hdr.nctrs as usize * 8));
    if v2 {
        let ext = (h as *const u8).add(hb + recs_len) as *const u64;
        let w = |i: usize| *ext.add(i);
        p.extend_from_slice(std::slice::from_raw_parts(ext as *const u8, EXT_BYTES));
        let mut part = |ptr: u64, len: usize| {
            if ptr != 0 && len > 0 {
                p.extend_from_slice(std::slice::from_raw_parts(ptr as *const u8, len));
            }
        };
        part(w(2), w(3) as usize * SITE_BYTES);
        part(w(4), w(5) as usize * 24);
        part(w(6), w(7) as usize);
    }
    out.extend_from_slice(&(p.len() as u64).to_le_bytes());
    out.extend_from_slice(&p);
}

/// Called before every profiled indirect call (see pliron-prof-rt).
#[no_mangle]
pub unsafe extern "C" fn __pliron_prof_icall(site: *mut u64, target: u64) {
    for i in 0..SLOTS {
        let a = site.add(i * 2);
        if *a == target {
            *a.add(1) += 1;
            return;
        }
        if *a == 0 {
            *a = target;
            *a.add(1) = 1;
            return;
        }
    }
    *site.add(SLOTS * 2) += 1;
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

/// `serialize` plus the indirect-call trailer.
pub fn serialize_all(m: &ProfileMap, ic: &IcallMap) -> Vec<u8> {
    let mut out = serialize(m);
    let with: Vec<_> = ic.iter().filter(|(_, s)| !s.is_empty()).collect();
    if !with.is_empty() {
        put_u64(&mut out, ICALL_MAGIC);
        put_u64(&mut out, with.len() as u64);
        for (&(sh, fp), sites) in with {
            put_u64(&mut out, sh);
            put_u64(&mut out, fp);
            put_u32(&mut out, sites.len() as u32);
            for s in sites {
                put_u64(&mut out, s.other);
                put_u32(&mut out, s.targets.len() as u32);
                for t in &s.targets {
                    put_u64(&mut out, t.count);
                    put_u64(&mut out, t.crate_id);
                    put_u64(&mut out, t.crate_hash);
                    put_u64(&mut out, t.cgu);
                    put_u32(&mut out, t.flags);
                    put_u32(&mut out, t.name.len() as u32);
                    out.extend_from_slice(t.name.as_bytes());
                }
            }
        }
    }
    out
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
    parse_at(b).map(|(m, _)| m)
}

/// The indirect-call trailer of a `.prof` whose function records end at
/// `p`, merged into `ic`. A missing trailer is an old profile: no sites.
fn parse_icalls(b: &[u8], mut p: usize, ic: &mut IcallMap) -> Option<()> {
    let p = &mut p;
    if rd_u64(b, p)? != ICALL_MAGIC {
        return None;
    }
    let n = rd_u64(b, p)?;
    for _ in 0..n {
        let sh = rd_u64(b, p)?;
        let fp = rd_u64(b, p)?;
        let ns = rd_u32(b, p)? as usize;
        let mut sites = Vec::new();
        for _ in 0..ns {
            let other = rd_u64(b, p)?;
            let nt = rd_u32(b, p)? as usize;
            let mut targets = Vec::new();
            for _ in 0..nt {
                let count = rd_u64(b, p)?;
                let crate_id = rd_u64(b, p)?;
                let crate_hash = rd_u64(b, p)?;
                let cgu = rd_u64(b, p)?;
                let flags = rd_u32(b, p)?;
                let len = rd_u32(b, p)? as usize;
                let name = String::from_utf8(b.get(*p..*p + len)?.to_vec()).ok()?;
                *p += len;
                targets.push(Target { name, crate_id, crate_hash, cgu, flags, count });
            }
            sites.push(Site { other, targets });
        }
        merge_sites(ic.entry((sh, fp)).or_default(), &sites);
    }
    Some(())
}

fn parse_at(b: &[u8]) -> Option<(ProfileMap, usize)> {
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
    Some((m, p))
}

/// Fold a raw per-process dump into `m`. Layout (see pliron-prof-rt):
/// `"PLRAW01"` then per CGU `u64 payload_len | Hdr(32B) | recs | ctrs`.
/// Returns false when `b` isn't a raw dump.
pub fn parse_raw(b: &[u8], m: &mut ProfileMap) -> bool {
    parse_raw_all(b, m, &mut IcallMap::new())
}

/// The indirect-call part of one CGU's raw payload: `Ext` words, then
/// the site table, the `{sym hash, address, flags}` table and the names.
struct RawExt<'a> {
    crate_id: u64,
    crate_hash: u64,
    cgu: u64,
    sites: &'a [u8],
    fns: &'a [u8],
    names: &'a [u8],
}

fn raw_ext(x: &[u8]) -> Option<RawExt<'_>> {
    let w = |i: usize| Some(u64::from_le_bytes(x.get(i * 8..i * 8 + 8)?.try_into().unwrap()));
    let sb = (w(3)? as usize).checked_mul(SITE_BYTES)?;
    let fb = (w(5)? as usize).checked_mul(24)?;
    let nb = w(7)? as usize;
    let sites = x.get(EXT_BYTES..EXT_BYTES + sb)?;
    let fns = x.get(EXT_BYTES + sb..EXT_BYTES + sb + fb)?;
    let names = x.get(EXT_BYTES + sb + fb..EXT_BYTES + sb + fb + nb)?;
    Some(RawExt { crate_id: w(0)?, crate_hash: w(8)?, cgu: w(1)?, sites, fns, names })
}

/// `parse_raw`, also folding the indirect-call sites into `ic`: every
/// recorded address is looked up in the address tables of all CGUs in
/// the dump (one process, so addresses are comparable); an address no
/// table has — uninstrumented code — counts as `other`.
pub fn parse_raw_all(b: &[u8], m: &mut ProfileMap, ic: &mut IcallMap) -> bool {
    if b.len() < 8 || u64::from_le_bytes(b[..8].try_into().unwrap()) != RAW_MAGIC {
        return false;
    }
    let u64at = |s: &[u8], o: usize| u64::from_le_bytes(s[o..o + 8].try_into().unwrap());
    // Pass 1: address -> (sym hash, flags, crate, cgu), sym hash -> name.
    let mut addr: HashMap<u64, (u64, u32, (u64, u64), u64)> = HashMap::new();
    let mut names: HashMap<u64, String> = HashMap::new();
    let mut cgus: Vec<(&[u8], &[u8], Option<RawExt<'_>>)> = Vec::new();
    let mut p = 8usize;
    while p + 8 <= b.len() {
        let len = u64at(b, p) as usize;
        p += 8;
        if len < 32 || p + len > b.len() {
            break;
        }
        let pay = &b[p..p + len];
        p += len;
        let recs_len = u64at(pay, 24) as usize;
        let nctrs = u64at(pay, 16) as usize;
        let v2 = u64at(pay, 0) == MAGIC2;
        let (Some(recs), Some(rest)) = (pay.get(32..32 + recs_len), pay.get(32 + recs_len..)) else {
            continue;
        };
        // v1 payloads end with the counters, whatever `nctrs` says.
        let ctrs_b = if v2 { (nctrs * 8).min(rest.len()) } else { rest.len() };
        let ext = if v2 { raw_ext(&rest[ctrs_b..]) } else { None };
        if let Some(e) = &ext {
            for f in e.fns.chunks_exact(24) {
                addr.insert(
                    u64at(f, 8),
                    (u64at(f, 0), u64at(f, 16) as u32, (e.crate_id, e.crate_hash), e.cgu),
                );
            }
            let mut q = 0usize;
            while q + 10 <= e.names.len() {
                let h = u64at(e.names, q);
                let n = u16::from_le_bytes(e.names[q + 8..q + 10].try_into().unwrap()) as usize;
                if let Some(s) = e.names.get(q + 10..q + 10 + n) {
                    names.insert(h, String::from_utf8_lossy(s).into_owned());
                }
                q = (q + 10 + n + 7) & !7;
            }
        }
        cgus.push((recs, &rest[..ctrs_b], ext));
    }
    // Pass 2: counters and sites.
    for (recs, ctrs, ext) in &cgus {
        let mut site = 0usize;
        let mut r = 0usize;
        while r + REC_LEN <= recs.len() {
            let sh = u64at(recs, r);
            let fp = u64at(recs, r + 8);
            let ns = u32::from_le_bytes(recs[r + 32..r + 36].try_into().unwrap()) as usize;
            let ne = u32::from_le_bytes(recs[r + 28..r + 32].try_into().unwrap()) as usize;
            r += (REC_LEN + ne * 4 + 7) & !7;
            let Some(e) = ext else { continue };
            if ns == 0 {
                continue;
            }
            let mut sites = Vec::with_capacity(ns);
            for k in 0..ns {
                let mut s = Site::default();
                if let Some(raw) = e.sites.get((site + k) * SITE_BYTES..(site + k + 1) * SITE_BYTES) {
                    s.other = u64at(raw, SLOTS * 16);
                    for i in 0..SLOTS {
                        let (a, c) = (u64at(raw, i * 16), u64at(raw, i * 16 + 8));
                        if c == 0 {
                            continue;
                        }
                        match addr.get(&a).and_then(|&(h, fl, cr, cg)| Some((names.get(&h)?, fl, cr, cg))) {
                            Some((name, flags, (crate_id, crate_hash), cgu)) => s.targets.push(Target {
                                name: name.clone(),
                                crate_id,
                                crate_hash,
                                cgu,
                                flags,
                                count: c,
                            }),
                            None => s.other += c,
                        }
                    }
                }
                sites.push(s);
            }
            site += ns;
            merge_sites(ic.entry((sh, fp)).or_default(), &sites);
        }
        let _ = ctrs;
    }
    parse_raw_counts(b, m)
}

fn parse_raw_counts(b: &[u8], m: &mut ProfileMap) -> bool {
    if b.len() < 8 || u64::from_le_bytes(b[..8].try_into().unwrap()) != RAW_MAGIC {
        return false;
    }
    let mut p = 8usize;
    while p + 8 <= b.len() {
        let len = u64::from_le_bytes(b[p..p + 8].try_into().unwrap()) as usize;
        p += 8;
        if len < 32 || p + len > b.len() {
            break;
        }
        let hdr = &b[p..p + 32];
        let recs_len = u64::from_le_bytes(hdr[24..32].try_into().unwrap()) as usize;
        if p + 32 + recs_len > p + len {
            break;
        }
        let recs = &b[p + 32..p + 32 + recs_len];
        // A v2 payload continues after the counters.
        let nctrs = u64::from_le_bytes(hdr[16..24].try_into().unwrap()) as usize;
        let v2 = u64::from_le_bytes(hdr[0..8].try_into().unwrap()) == MAGIC2;
        let cend = if v2 { (p + 32 + recs_len + nctrs * 8).min(p + len) } else { p + len };
        let ctrs = &b[p + 32 + recs_len..cend];
        let mut r = 0usize;
        while r + REC_LEN <= recs.len() {
            let sh = u64::from_le_bytes(recs[r..r + 8].try_into().unwrap());
            let fp = u64::from_le_bytes(recs[r + 8..r + 16].try_into().unwrap());
            let base = u64::from_le_bytes(recs[r + 16..r + 24].try_into().unwrap()) as usize;
            let nb = u32::from_le_bytes(recs[r + 24..r + 28].try_into().unwrap()) as usize;
            let ne = u32::from_le_bytes(recs[r + 28..r + 32].try_into().unwrap()) as usize;
            let mut keys = Vec::with_capacity(ne);
            for i in 0..ne {
                let o = r + REC_LEN + i * 4;
                if o + 4 > recs.len() {
                    break;
                }
                let e = u32::from_le_bytes(recs[o..o + 4].try_into().unwrap());
                keys.push(((e >> 16) as u16, (e & 0xffff) as u16));
            }
            let (ob, oe) = m.entry((sh, fp)).or_default();
            ob.resize(ob.len().max(nb), 0);
            for i in 0..nb {
                let o = (base + i) * 8;
                if o + 8 <= ctrs.len() {
                    ob[i] += u64::from_le_bytes(ctrs[o..o + 8].try_into().unwrap());
                }
            }
            for (i, &(pk, sk)) in keys.iter().enumerate() {
                let o = (base + nb + i) * 8;
                if o + 8 > ctrs.len() {
                    break;
                }
                let c = u64::from_le_bytes(ctrs[o..o + 8].try_into().unwrap());
                if let Some(x) = oe.iter_mut().find(|x| x.0 == pk && x.1 == sk) {
                    x.2 += c;
                } else {
                    oe.push((pk, sk, c));
                }
            }
            r += (REC_LEN + ne * 4 + 7) & !7;
        }
        p += len;
    }
    true
}

/// Read a profile file of either on-disk form into `m`.
pub fn load(b: &[u8], m: &mut ProfileMap) -> bool {
    load_all(b, m, &mut IcallMap::new())
}

/// `load`, with the indirect-call sites into `ic`.
pub fn load_all(b: &[u8], m: &mut ProfileMap, ic: &mut IcallMap) -> bool {
    if parse_raw_all(b, m, ic) {
        return true;
    }
    if let Some((pm, end)) = parse_at(b) {
        let _ = parse_icalls(b, end, ic);
        for (k, (bs, e)) in pm {
            let (ob, oe) = m.entry(k).or_default();
            ob.resize(ob.len().max(bs.len()), 0);
            for (i, c) in bs.iter().enumerate() {
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
        return true;
    }
    false
}
