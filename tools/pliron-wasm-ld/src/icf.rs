//! Identical-code folding on the linked module (`PLIRON_WASM_ICF=1`).
//!
//! The backend emits many byte-identical bodies under different names:
//! one generic instance per crate that needs it (`quicksort<Hap>` 19
//! times in the dsa module), local copies the same-name fold
//! (`PLIRON_LDEDUP`) cannot see because they are not exported. This pass
//! keeps one body per class of equal functions and points every
//! reference at it.
//!
//! Equality, by partition refinement (LLD's ICF, without its
//! address-significance exceptions, see below):
//!
//! 1. Each defined function gets a key: its type index, its body bytes
//!    with the function-index immediate of every `call`, `return_call`
//!    and `ref.func` cut out, the positions of those cuts, and the
//!    targets that are imports (literal indices; imports are never
//!    folded). Equal keys start in one class. Every other byte of a body
//!    names something module-global (types, globals, tables, memory,
//!    data, locals), so equal bytes mean equal behaviour once the
//!    defined-function targets are equal too.
//! 2. Refine: a function's next class is its class plus the classes of
//!    its defined targets, in order. Classes only ever split, so the
//!    round in which no class splits is the fixpoint. Starting from the
//!    optimistic partition gives the largest one: mutually recursive
//!    identical pairs fold, and callers fold once their callees do.
//! 3. The lowest-index member of each class is kept; the others are
//!    removed. Calls, `ref.func`, element segments, exports, the start
//!    function and global initializers point at the kept member.
//!
//! Function addresses: Rust does not promise distinct addresses for
//! distinct functions (`fn` pointer equality is documented as
//! unreliable, `ptr::fn_addr_eq`), and the backend compares no function
//! pointers of its own, so two table slots or two `ref.func`s may become
//! one function. Two exports may name the same body: each keeps its
//! name.
//!
//! Byte offsets: a kept body is copied byte for byte, and each patched
//! index is written back in the same number of LEB128 bytes (a kept
//! index is never larger than the one it replaces), so offsets inside a
//! body never move. Bodies that leave shift later bodies, which is why
//! this runs before the other post-link passes (`addroff`, `brhint`).
//!
//! Names: the name section keeps the kept member's names (function and
//! local names); the removed members' names go.
//!
//! The pass gives up (returns `None`) on input it does not handle: a
//! module with DWARF (`.debug_*`, code offsets would go stale) or a name
//! subsection other than module, function and local names. That is never
//! a reason the module is wrong, only unfolded.

use rustc_hash::FxHashMap;
use wasm_encoder as we;
use wasm_encoder::reencode::Reencode;
use wasmparser as wp;

/// What one run did.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub defined: usize,
    pub removed: usize,
    pub classes_folded: usize,
    pub rounds: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

/// The refinement's round cap: each round must split a class, so a
/// module with n functions converges in fewer than n rounds; real
/// modules take a handful. Past the cap nothing is folded.
const MAX_ROUNDS: usize = 1000;

/// A function-index immediate inside a body.
struct Site {
    /// Offset of the LEB128 within the body bytes.
    at: usize,
    len: usize,
    target: u32,
}

struct Body {
    /// Absolute range of the body (locals and code) in the module.
    range: std::ops::Range<usize>,
    sites: Vec<Site>,
}

fn leb_len(b: &[u8], at: usize) -> Option<usize> {
    (0..5).find(|&k| b.get(at + k).is_some_and(|&c| c & 0x80 == 0)).map(|k| k + 1)
}

/// `v` in exactly `len` LEB128 bytes (continuation bits on all but the
/// last). `v` must fit.
fn put_leb_padded(out: &mut [u8], mut v: u32, len: usize) {
    for (k, b) in out.iter_mut().take(len).enumerate() {
        let x = (v & 0x7f) as u8;
        v >>= 7;
        *b = if k + 1 < len { x | 0x80 } else { x };
    }
    debug_assert_eq!(v, 0, "padded LEB overflow");
}

fn put_leb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        out.push(if v != 0 { b | 0x80 } else { b });
        if v == 0 {
            break;
        }
    }
}

struct Remap<'a> {
    map: &'a [u32],
}

impl Reencode for Remap<'_> {
    type Error = std::convert::Infallible;
    fn function_index(&mut self, f: u32) -> Result<u32, we::reencode::Error<Self::Error>> {
        Ok(self.map[f as usize])
    }
}

/// Fold identical functions in the linked module `m`. `None`: nothing to
/// fold, or a module this pass leaves alone.
pub fn fold(m: &[u8]) -> Result<Option<(Vec<u8>, Stats)>, String> {
    let err = |e: wp::BinaryReaderError| e.to_string();
    let mut nimp = 0u32;
    let mut ftypes: Vec<u32> = Vec::new();
    let mut bodies: Vec<Body> = Vec::new();
    for p in wp::Parser::new(0).parse_all(m) {
        match p.map_err(err)? {
            wp::Payload::ImportSection(r) => {
                for imp in r.into_imports() {
                    if matches!(imp.map_err(err)?.ty, wp::TypeRef::Func(_)) {
                        nimp += 1;
                    }
                }
            }
            wp::Payload::FunctionSection(r) => {
                for t in r {
                    ftypes.push(t.map_err(err)?);
                }
            }
            wp::Payload::CodeSectionEntry(b) => {
                let range = b.range();
                let mut sites = Vec::new();
                let mut ops = b.get_operators_reader().map_err(err)?;
                while !ops.eof() {
                    let (op, off) = ops.read_with_offset().map_err(err)?;
                    let target = match op {
                        wp::Operator::Call { function_index }
                        | wp::Operator::ReturnCall { function_index }
                        | wp::Operator::RefFunc { function_index } => function_index,
                        _ => continue,
                    };
                    // All three opcodes are one byte; the index follows.
                    let len = leb_len(m, off + 1).ok_or("icf: bad LEB")?;
                    sites.push(Site { at: off + 1 - range.start, len, target });
                }
                bodies.push(Body { range, sites });
            }
            wp::Payload::CustomSection(c) => {
                if c.name().starts_with(".debug") {
                    return Ok(None);
                }
                if c.name() == "name" {
                    if let wp::KnownCustom::Name(r) = c.as_known() {
                        for s in r {
                            match s.map_err(err)? {
                                wp::Name::Module { .. } | wp::Name::Function(_) | wp::Name::Local(_) => {}
                                _ => return Ok(None),
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let n = bodies.len();
    if n != ftypes.len() || n < 2 {
        return Ok(None);
    }

    // 1. Initial classes: type, normalized bytes, cut positions, import targets.
    let mut cls: Vec<u32> = Vec::with_capacity(n);
    {
        let mut intern: FxHashMap<(u32, Vec<u8>, Vec<u32>, Vec<u32>), u32> = FxHashMap::default();
        for (i, b) in bodies.iter().enumerate() {
            let bytes = &m[b.range.clone()];
            let mut norm = Vec::with_capacity(bytes.len());
            let mut cuts = Vec::with_capacity(b.sites.len());
            let mut imps = Vec::new();
            let mut p = 0;
            for s in &b.sites {
                norm.extend_from_slice(&bytes[p..s.at]);
                cuts.push(norm.len() as u32);
                p = s.at + s.len;
                // Imports by index (u32::MAX: a defined target, refined below).
                imps.push(if s.target < nimp { s.target } else { u32::MAX });
            }
            norm.extend_from_slice(&bytes[p..]);
            let next = intern.len() as u32;
            cls.push(*intern.entry((ftypes[i], norm, cuts, imps)).or_insert(next));
        }
    }
    let mut ncls = cls.iter().copied().max().map_or(0, |c| c as usize + 1);
    if ncls == n {
        return Ok(None);
    }

    // 2. Refine by the classes of defined targets until no class splits.
    let mut rounds = 0;
    loop {
        rounds += 1;
        if rounds > MAX_ROUNDS {
            return Ok(None);
        }
        let mut intern: FxHashMap<(u32, Vec<u32>), u32> = FxHashMap::default();
        let next: Vec<u32> = bodies
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let key = (
                    cls[i],
                    b.sites
                        .iter()
                        .filter(|s| s.target >= nimp)
                        .map(|s| cls[(s.target - nimp) as usize])
                        .collect(),
                );
                let k = intern.len() as u32;
                *intern.entry(key).or_insert(k)
            })
            .collect();
        let nn = intern.len();
        cls = next;
        if nn == ncls {
            break;
        }
        ncls = nn;
        if ncls == n {
            return Ok(None);
        }
    }

    // 3. Keep the lowest index of each class.
    let mut rep: FxHashMap<u32, u32> = FxHashMap::default();
    for (i, &c) in cls.iter().enumerate() {
        rep.entry(c).or_insert(i as u32);
    }
    // `PLIRON_WASM_ICF_MAX=N`: fold only the first N removable functions
    // (by index); the rest stay. For bisecting a folded module.
    let max = std::env::var("PLIRON_WASM_ICF_MAX").ok().and_then(|v| v.parse::<usize>().ok());
    let mut seen = 0usize;
    let rep_of: Vec<u32> = cls
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let r = rep[c];
            if r == i as u32 {
                return r;
            }
            seen += 1;
            if max.is_some_and(|m| seen > m) { i as u32 } else { r }
        })
        .collect();
    let keep: Vec<bool> = rep_of.iter().enumerate().map(|(i, &r)| r == i as u32).collect();
    let mut new_def = vec![u32::MAX; n];
    let mut k = nimp;
    for i in 0..n {
        if keep[i] {
            new_def[i] = k;
            k += 1;
        }
    }
    // Old function index -> new index of its class's kept member.
    let map: Vec<u32> = (0..nimp)
        .chain((0..n).map(|i| new_def[rep_of[i] as usize]))
        .collect();
    let removed = keep.iter().filter(|&&x| !x).count();
    let mut folded_cls: FxHashMap<u32, usize> = FxHashMap::default();
    for (i, &c) in cls.iter().enumerate() {
        if !keep[i] {
            *folded_cls.entry(c).or_default() += 1;
        }
    }

    // Emit: every section in order, rebuilding the ones that name functions.
    let mut out = we::Module::new();
    let mut re = Remap { map: &map };
    let rerr = |e: we::reencode::Error| format!("icf: {e:?}");
    for p in wp::Parser::new(0).parse_all(m) {
        let p = p.map_err(err)?;
        match p {
            wp::Payload::FunctionSection(_) => {
                let mut fs = we::FunctionSection::new();
                for i in 0..n {
                    if keep[i] {
                        fs.function(ftypes[i]);
                    }
                }
                out.section(&fs);
            }
            wp::Payload::ExportSection(r) => {
                let mut es = we::ExportSection::new();
                re.parse_export_section(&mut es, r).map_err(rerr)?;
                out.section(&es);
            }
            wp::Payload::ElementSection(r) => {
                let mut es = we::ElementSection::new();
                re.parse_element_section(&mut es, r).map_err(rerr)?;
                out.section(&es);
            }
            wp::Payload::GlobalSection(r) => {
                let mut gs = we::GlobalSection::new();
                re.parse_global_section(&mut gs, r).map_err(rerr)?;
                out.section(&gs);
            }
            wp::Payload::StartSection { func, .. } => {
                out.section(&we::StartSection { function_index: map[func as usize] });
            }
            wp::Payload::CodeSectionStart { .. } => {
                let mut data = Vec::new();
                put_leb(&mut data, (n - removed) as u32);
                for (i, b) in bodies.iter().enumerate() {
                    if !keep[i] {
                        continue;
                    }
                    let mut bytes = m[b.range.clone()].to_vec();
                    for s in &b.sites {
                        put_leb_padded(&mut bytes[s.at..], map[s.target as usize], s.len);
                    }
                    put_leb(&mut data, bytes.len() as u32);
                    data.extend_from_slice(&bytes);
                }
                out.section(&we::RawSection { id: 10, data: &data });
            }
            wp::Payload::CodeSectionEntry(_) => {}
            wp::Payload::CustomSection(c) if c.name() == "name" => {
                let wp::KnownCustom::Name(r) = c.as_known() else {
                    unreachable!("checked above");
                };
                let mut ns = we::NameSection::new();
                for s in r {
                    match s.map_err(err)? {
                        wp::Name::Module { name, .. } => ns.module(name),
                        wp::Name::Function(map_r) => {
                            let mut nm = we::NameMap::new();
                            for e in map_r {
                                let e = e.map_err(err)?;
                                let f = e.index;
                                if f < nimp || keep.get((f - nimp) as usize).copied().unwrap_or(false) {
                                    nm.append(map[f as usize], e.name);
                                }
                            }
                            ns.functions(&nm);
                        }
                        wp::Name::Local(map_r) => {
                            let mut im = we::IndirectNameMap::new();
                            for e in map_r {
                                let e = e.map_err(err)?;
                                let f = e.index;
                                if f < nimp || keep.get((f - nimp) as usize).copied().unwrap_or(false) {
                                    let mut nm = we::NameMap::new();
                                    for l in e.names {
                                        let l = l.map_err(err)?;
                                        nm.append(l.index, l.name);
                                    }
                                    im.append(map[f as usize], &nm);
                                }
                            }
                            ns.locals(&im);
                        }
                        _ => unreachable!("checked above"),
                    }
                }
                out.section(&ns);
            }
            wp::Payload::CustomSection(c) => {
                out.section(&we::CustomSection { name: c.name().into(), data: c.data().into() });
            }
            wp::Payload::Version { .. } | wp::Payload::End(_) => {}
            other => {
                if let Some((id, range)) = other.as_section() {
                    out.section(&we::RawSection { id, data: &m[range] });
                }
            }
        }
    }
    let bytes = out.finish();
    let stats = Stats {
        defined: n,
        removed,
        classes_folded: folded_cls.len(),
        rounds,
        bytes_before: m.len(),
        bytes_after: bytes.len(),
    };
    Ok(Some((bytes, stats)))
}
