//! pliron-wasm-ld: links wasm objects from rustc_codegen_pliron (see its
//! `src/wasm.rs` for the object format) into one module. Accepts wasm-ld
//! style arguments: objects, rlibs/archives, `-o`, `--export=<sym>`; other
//! flags are ignored. Archive members are pulled in only when they define a
//! symbol that is still undefined.
//!
//! Memory layout: [0, 1024) unused, then the stack (grows down to 1024),
//! then data, then `__heap_base` up to `__heap_end` (end of initial memory).

use std::collections::HashMap;

use wasm_encoder as we;
use wasm_encoder::reencode::{Error as ReError, Reencode};
use wasmparser as wp;

static STACK_SIZE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1 << 20);

fn stack() -> u32 {
    STACK_SIZE.load(std::sync::atomic::Ordering::Relaxed)
}

fn set_z_option(opt: &str) {
    if let Some(n) = opt
        .strip_prefix("stack-size=")
        .and_then(|n| n.parse::<u32>().ok())
    {
        STACK_SIZE.store(n.next_multiple_of(16), std::sync::atomic::Ordering::Relaxed);
    }
}
const GLOBAL: u8 = 1;
const INIT_ARRAY: u8 = 4;
const CALL_CTORS: &str = "__wasm_call_ctors";

struct DataObj {
    name: String,
    flags: u8,
    align: u32,
    bytes: Vec<u8>,
    relocs: Vec<(u32, bool, String, i32)>,
}

struct Obj<'a> {
    name: String,
    types: Vec<wp::FuncType>,
    /// Function imports: (module, name, type).
    fimports: Vec<(String, String, u32)>,
    /// Global imports: (module, name).
    gimports: Vec<(String, String)>,
    ftypes: Vec<u32>,
    exports: HashMap<String, u32>,
    bodies: Vec<wp::FunctionBody<'a>>,
    funcs: Vec<(String, u8)>,
    data: Vec<DataObj>,
}

struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn u32(&mut self) -> u32 {
        let (h, t) = self.0.split_at(4);
        self.0 = t;
        u32::from_le_bytes(h.try_into().unwrap())
    }
    fn u8(&mut self) -> u8 {
        let b = self.0[0];
        self.0 = &self.0[1..];
        b
    }
    fn bytes(&mut self) -> &'a [u8] {
        let n = self.u32() as usize;
        let (h, t) = self.0.split_at(n);
        self.0 = t;
        h
    }
    fn str(&mut self) -> String {
        String::from_utf8(self.bytes().to_vec()).unwrap()
    }
}

fn parse<'a>(name: String, bytes: &'a [u8]) -> wp::Result<Obj<'a>> {
    let mut o = Obj {
        name,
        types: vec![],
        fimports: vec![],
        gimports: vec![],
        ftypes: vec![],
        exports: HashMap::new(),
        bodies: vec![],
        funcs: vec![],
        data: vec![],
    };
    for p in wp::Parser::new(0).parse_all(bytes) {
        match p? {
            wp::Payload::TypeSection(r) => {
                for t in r.into_iter_err_on_gc_types() {
                    o.types.push(t?);
                }
            }
            wp::Payload::ImportSection(r) => {
                for imp in r.into_imports() {
                    let imp = imp?;
                    match imp.ty {
                        wp::TypeRef::Func(t) => {
                            o.fimports.push((imp.module.into(), imp.name.into(), t))
                        }
                        wp::TypeRef::Global(_) => {
                            o.gimports.push((imp.module.into(), imp.name.into()))
                        }
                        _ => {}
                    }
                }
            }
            wp::Payload::FunctionSection(r) => {
                for t in r {
                    o.ftypes.push(t?);
                }
            }
            wp::Payload::ExportSection(r) => {
                for e in r {
                    let e = e?;
                    if e.kind == wp::ExternalKind::Func {
                        o.exports.insert(e.name.into(), e.index);
                    }
                }
            }
            wp::Payload::CodeSectionEntry(b) => o.bodies.push(b),
            wp::Payload::CustomSection(c) if c.name() == "pliron.link" => {
                let mut r = Rd(c.data());
                for _ in 0..r.u32() {
                    let n = r.str();
                    let f = r.u8();
                    o.funcs.push((n, f));
                }
                for _ in 0..r.u32() {
                    let name = r.str();
                    let flags = r.u8();
                    let align = r.u32();
                    let bytes = r.bytes().to_vec();
                    let relocs = (0..r.u32())
                        .map(|_| {
                            let off = r.u32();
                            let func = r.u8() != 0;
                            let sym = r.str();
                            (off, func, sym, r.u32() as i32)
                        })
                        .collect();
                    o.data.push(DataObj {
                        name,
                        flags,
                        align,
                        bytes,
                        relocs,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(o)
}

#[derive(Clone, Copy, Debug)]
enum Def {
    Func(usize, u32),
    Data(usize, usize),
}

impl Obj<'_> {
    fn local(&self, oi: usize, sym: &str) -> Option<Def> {
        if let Some(&f) = self.exports.get(sym) {
            return Some(Def::Func(oi, f));
        }
        self.data
            .iter()
            .position(|d| d.name == sym)
            .map(|i| Def::Data(oi, i))
    }

    fn globals(&self, oi: usize) -> impl Iterator<Item = (&str, Def)> {
        let fs = self
            .funcs
            .iter()
            .filter(|(_, f)| f & GLOBAL != 0)
            .map(move |(n, _)| (&n[..], Def::Func(oi, self.exports[n])));
        let ds = self
            .data
            .iter()
            .enumerate()
            .filter(|(_, d)| d.flags & GLOBAL != 0)
            .map(move |(i, d)| (&d.name[..], Def::Data(oi, i)));
        fs.chain(ds)
    }

    fn refs(&self) -> impl Iterator<Item = &str> {
        let fi = self
            .fimports
            .iter()
            .filter(|i| i.0 == "env")
            .map(|i| &i.1[..]);
        let gi = self
            .gimports
            .iter()
            .filter(|g| g.0.starts_with("GOT."))
            .map(|g| &g.1[..]);
        let dr = self
            .data
            .iter()
            .flat_map(|d| d.relocs.iter().map(|r| &r.2[..]));
        fi.chain(gi).chain(dr)
    }
}

struct Mapper<'m> {
    funcs: &'m [u32],
    globals: &'m [u32],
    types: &'m [u32],
}

impl Reencode for Mapper<'_> {
    type Error = std::convert::Infallible;
    fn function_index(&mut self, f: u32) -> Result<u32, ReError<Self::Error>> {
        Ok(self.funcs[f as usize])
    }
    fn global_index(&mut self, g: u32) -> Result<u32, ReError<Self::Error>> {
        Ok(self.globals[g as usize])
    }
    fn type_index(&mut self, t: u32) -> Result<u32, ReError<Self::Error>> {
        Ok(self.types[t as usize])
    }
    fn table_index(&mut self, _: u32) -> Result<u32, ReError<Self::Error>> {
        Ok(0)
    }
    fn memory_index(&mut self, _: u32) -> Result<u32, ReError<Self::Error>> {
        Ok(0)
    }
}

fn val_type(t: wp::ValType) -> we::ValType {
    match t {
        wp::ValType::I32 => we::ValType::I32,
        wp::ValType::I64 => we::ValType::I64,
        wp::ValType::F32 => we::ValType::F32,
        wp::ValType::F64 => we::ValType::F64,
        wp::ValType::V128 => we::ValType::V128,
        t => panic!("unsupported value type {t:?}"),
    }
}

/// Links with wasm-ld style `args` (without argv[0]). Also called in-process by rustc.wasm,
/// which cannot spawn a linker.
pub fn link(args: Vec<String>) -> Result<(), String> {
    STACK_SIZE.store(1 << 20, std::sync::atomic::Ordering::Relaxed);
    let mut out = None;
    let mut exports = Vec::new();
    let mut inputs = Vec::new();
    let (mut dirs, mut libs) = (Vec::<String>::new(), Vec::<String>::new());
    let mut entry = true;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => out = it.next().cloned(),
            "--export" => exports.extend(it.next().cloned()),
            "--no-entry" => entry = false,
            "-L" => dirs.extend(it.next().cloned()),
            "-l" => libs.extend(it.next().cloned()),
            "-z" => it.next().into_iter().for_each(|o| set_z_option(o)),
            "-m" | "-flavor" | "--sysroot" => {
                it.next();
            }
            s if s.starts_with("-z") => set_z_option(&s[2..]),
            s if s.starts_with("--export=") => exports.push(s["--export=".len()..].to_string()),
            s if s.starts_with("-L") => dirs.push(s[2..].to_string()),
            s if s.starts_with("-l") => libs.push(s[2..].to_string()),
            s if s.starts_with('-') => {}
            s => inputs.push(s.to_string()),
        }
    }
    let out = out.ok_or("missing -o")?;
    for l in &libs {
        let f = format!("lib{l}.a");
        let p = dirs
            .iter()
            .map(|d| std::path::Path::new(d).join(&f))
            .find(|p| p.exists())
            .ok_or_else(|| format!("cannot find -l{l}"))?;
        inputs.push(p.to_string_lossy().into_owned());
    }

    // Read inputs: direct objects are always linked; archive members on demand.
    let mut blobs: Vec<(String, Vec<u8>, bool)> = Vec::new();
    for path in &inputs {
        let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        if data.starts_with(b"!<arch>\n") {
            let ar = object::read::archive::ArchiveFile::parse(&*data)
                .map_err(|e| format!("{path}: {e}"))?;
            for m in ar.members() {
                let m = m.map_err(|e| format!("{path}: {e}"))?;
                let bytes = m.data(&*data).map_err(|e| format!("{path}: {e}"))?;
                if bytes.starts_with(b"\0asm") {
                    let n = format!("{path}({})", String::from_utf8_lossy(m.name()));
                    blobs.push((n, bytes.to_vec(), true));
                }
            }
        } else if data.starts_with(b"\0asm") {
            blobs.push((path.clone(), data, false));
        }
    }
    let mut objs = Vec::new();
    for (n, b, _) in &blobs {
        objs.push(parse(n.clone(), b).map_err(|e| format!("{n}: {e}"))?);
    }

    // Resolve: pull archive members until no undefined symbol can be satisfied.
    let mut provider: HashMap<&str, usize> = HashMap::new();
    for (oi, o) in objs.iter().enumerate() {
        if blobs[oi].2 {
            for (n, _) in o.globals(oi) {
                provider.entry(n).or_insert(oi);
            }
        }
    }
    let mut included: Vec<usize> = (0..objs.len()).filter(|&i| !blobs[i].2).collect();
    let mut is_in = vec![false; objs.len()];
    let mut defs: HashMap<String, Def> = HashMap::new();
    let mut k = 0;
    for &i in &included {
        is_in[i] = true;
    }
    // Explicit exports and the command entry point root archive members.
    let roots = exports
        .iter()
        .map(|s| &s[..])
        .chain(entry.then_some("_start"));
    for r in roots {
        if let Some(&p) = provider.get(r)
            && !is_in[p]
        {
            is_in[p] = true;
            included.push(p);
        }
    }
    while k < included.len() {
        let oi = included[k];
        for (n, d) in objs[oi].globals(oi) {
            defs.entry(n.to_string()).or_insert(d);
        }
        k += 1;
        if k == included.len() {
            let mut want = Vec::new();
            for &oi in &included {
                for r in objs[oi].refs() {
                    if objs[oi].local(oi, r).is_none() && !defs.contains_key(r) {
                        if let Some(&p) = provider.get(r) {
                            if !is_in[p] {
                                is_in[p] = true;
                                want.push(p);
                            }
                        }
                    }
                }
            }
            included.extend(want);
        }
    }
    let resolve = |oi: usize, sym: &str| objs[oi].local(oi, sym).or_else(|| defs.get(sym).copied());

    // Types.
    let mut types: Vec<wp::FuncType> = Vec::new();
    let mut tmaps: HashMap<usize, Vec<u32>> = HashMap::new();
    for &oi in &included {
        let m = objs[oi]
            .types
            .iter()
            .map(|t| match types.iter().position(|x| x == t) {
                Some(i) => i as u32,
                None => {
                    types.push(t.clone());
                    types.len() as u32 - 1
                }
            })
            .collect();
        tmaps.insert(oi, m);
    }

    // Functions: host imports first, then every included object's bodies.
    // Only `env` imports are link-time symbols; other modules (e.g.
    // `wasi_snapshot_preview1`) are always provided by the host.
    let mut host: Vec<(String, String, u32)> = Vec::new();
    for &oi in &included {
        for (m, n, t) in &objs[oi].fimports {
            let linked = m == "env" && (resolve(oi, n).is_some() || n == CALL_CTORS);
            if !linked && !host.iter().any(|h| &h.0 == m && &h.1 == n) {
                host.push((m.clone(), n.clone(), tmaps[&oi][*t as usize]));
            }
        }
    }
    let mut base: HashMap<usize, u32> = HashMap::new();
    let mut next = host.len() as u32;
    for &oi in &included {
        base.insert(oi, next);
        next += objs[oi].bodies.len() as u32;
    }
    // `__wasm_call_ctors` is synthesized after every object's bodies.
    let ctors_fn = next;
    let func_out = |d: Def| -> Option<u32> {
        match d {
            Def::Func(oi, f) => Some(base[&oi] + f - objs[oi].fimports.len() as u32),
            Def::Data(..) => None,
        }
    };
    let mut fmaps: HashMap<usize, Vec<u32>> = HashMap::new();
    for &oi in &included {
        let o = &objs[oi];
        let mut m = Vec::new();
        for (md, n, _) in &o.fimports {
            m.push(match resolve(oi, n).filter(|_| md == "env") {
                Some(d) => func_out(d)
                    .ok_or_else(|| format!("{}: {n} is data, called as a function", o.name))?,
                None if md == "env" && n == CALL_CTORS => ctors_fn,
                None => host.iter().position(|h| &h.0 == md && &h.1 == n).unwrap() as u32,
            });
        }
        let b = base[&oi];
        m.extend((0..o.bodies.len() as u32).map(|i| b + i));
        fmaps.insert(oi, m);
    }

    // Data layout.
    let mut addr: HashMap<(usize, usize), u32> = HashMap::new();
    let mut cur = 1024 + stack();
    for &oi in &included {
        for (di, d) in objs[oi].data.iter().enumerate() {
            cur = cur.next_multiple_of(d.align.max(1));
            addr.insert((oi, di), cur);
            cur += d.bytes.len() as u32;
        }
    }
    let data_end = cur;
    let heap_base = cur.next_multiple_of(16);
    let pages = (heap_base as u64).div_ceil(65536) + 1;
    let heap_end = (pages * 65536) as u32;

    // Function table: slot 0 is null.
    let mut table: Vec<u32> = Vec::new();
    let slot = |f: u32, table: &mut Vec<u32>| -> u32 {
        match table.iter().position(|&x| x == f) {
            Some(i) => i as u32 + 1,
            None => {
                table.push(f);
                table.len() as u32
            }
        }
    };
    let value_of =
        |oi: usize, sym: &str, func: bool, table: &mut Vec<u32>| -> Result<u32, String> {
            match (resolve(oi, sym), sym) {
                (Some(Def::Data(o, d)), _) if !func => Ok(addr[&(o, d)]),
                (Some(d @ Def::Func(..)), _) if func => Ok(slot(func_out(d).unwrap(), table)),
                (None, CALL_CTORS) if func => Ok(slot(ctors_fn, table)),
                (None, "__heap_base") => Ok(heap_base),
                (None, "__data_end") => Ok(data_end),
                (None, "__heap_end") => Ok(heap_end),
                (None, _) if func => {
                    let h = host
                        .iter()
                        .position(|h| h.1 == sym)
                        .ok_or_else(|| format!("undefined function {sym}"))?;
                    Ok(slot(h as u32, table))
                }
                _ => Err(format!(
                    "{}: undefined {} {sym}",
                    objs[oi].name,
                    if func { "function" } else { "symbol" }
                )),
            }
        };

    // Globals: 0 is the stack pointer; each GOT import becomes a constant.
    let mut gvals: Vec<u32> = Vec::new();
    let mut gmaps: HashMap<usize, Vec<u32>> = HashMap::new();
    for &oi in &included {
        let mut m = Vec::new();
        for (module, n) in &objs[oi].gimports {
            m.push(match (module.as_str(), n.as_str()) {
                ("env", "__stack_pointer") => 0,
                ("GOT.mem", s) | ("GOT.func", s) => {
                    let v = value_of(oi, s, module == "GOT.func", &mut table)?;
                    gvals.push(v);
                    gvals.len() as u32
                }
                (m, s) => return Err(format!("{}: unknown global import {m}.{s}", objs[oi].name)),
            });
        }
        gmaps.insert(oi, m);
    }

    // Data bytes with relocations applied.
    let mut segs: Vec<(u32, Vec<u8>)> = Vec::new();
    for &oi in &included {
        for (di, d) in objs[oi].data.iter().enumerate() {
            let mut bytes = d.bytes.clone();
            for (off, func, sym, add) in &d.relocs {
                let v = value_of(oi, sym, *func, &mut table)?.wrapping_add(*add as u32);
                bytes[*off as usize..*off as usize + 4].copy_from_slice(&v.to_le_bytes());
            }
            if bytes.iter().any(|&b| b != 0) {
                segs.push((addr[&(oi, di)], bytes));
            }
        }
    }

    // Constructors: every pointer in an `.init_array` entry, in link order.
    let mut ctors: Vec<u32> = Vec::new();
    for &oi in &included {
        for d in objs[oi].data.iter().filter(|d| d.flags & INIT_ARRAY != 0) {
            for (_, func, sym, _) in &d.relocs {
                let f = resolve(oi, sym)
                    .filter(|_| *func)
                    .and_then(func_out)
                    .ok_or_else(|| format!("{}: bad constructor {sym}", objs[oi].name))?;
                ctors.push(f);
            }
        }
    }
    let void_ty = match types
        .iter()
        .position(|t| t.params().is_empty() && t.results().is_empty())
    {
        Some(i) => i as u32,
        None => {
            types.push(wp::FuncType::new([], []));
            types.len() as u32 - 1
        }
    };

    // Exports.
    let mut ex: Vec<(String, u32)> = Vec::new();
    let names: Vec<String> = if exports.is_empty() {
        included
            .iter()
            .filter(|&&oi| !blobs[oi].2)
            .flat_map(|&oi| {
                objs[oi]
                    .funcs
                    .iter()
                    .filter(|f| f.1 & GLOBAL != 0)
                    .map(|f| f.0.clone())
            })
            .collect()
    } else {
        exports
    };
    let start = (entry && defs.contains_key("_start")).then(|| "_start".to_string());
    for n in names.into_iter().chain(start) {
        if let Some(f) = defs.get(&n).copied().and_then(func_out) {
            if !ex.iter().any(|e| e.0 == n) {
                ex.push((n, f));
            }
        }
    }

    // Encode.
    let mut module = we::Module::new();
    let mut ts = we::TypeSection::new();
    for t in &types {
        ts.ty().function(
            t.params().iter().map(|v| val_type(*v)),
            t.results().iter().map(|v| val_type(*v)),
        );
    }
    module.section(&ts);
    let mut is = we::ImportSection::new();
    for (m, n, t) in &host {
        is.import(m, n, we::EntityType::Function(*t));
    }
    module.section(&is);
    let mut fs = we::FunctionSection::new();
    for &oi in &included {
        for t in &objs[oi].ftypes {
            fs.function(tmaps[&oi][*t as usize]);
        }
    }
    fs.function(void_ty);
    module.section(&fs);
    let mut tab = we::TableSection::new();
    let tsize = table.len() as u64 + 1;
    tab.table(we::TableType {
        element_type: we::RefType::FUNCREF,
        table64: false,
        minimum: tsize,
        maximum: Some(tsize),
        shared: false,
    });
    module.section(&tab);
    let mut ms = we::MemorySection::new();
    ms.memory(we::MemoryType {
        minimum: pages,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&ms);
    let mut gs = we::GlobalSection::new();
    gs.global(
        we::GlobalType {
            val_type: we::ValType::I32,
            mutable: true,
            shared: false,
        },
        &we::ConstExpr::i32_const((1024 + stack()) as i32),
    );
    for v in &gvals {
        gs.global(
            we::GlobalType {
                val_type: we::ValType::I32,
                mutable: false,
                shared: false,
            },
            &we::ConstExpr::i32_const(*v as i32),
        );
    }
    module.section(&gs);
    let mut es = we::ExportSection::new();
    es.export("memory", we::ExportKind::Memory, 0);
    for (n, f) in &ex {
        es.export(n, we::ExportKind::Func, *f);
    }
    module.section(&es);
    let mut els = we::ElementSection::new();
    els.active(
        Some(0),
        &we::ConstExpr::i32_const(1),
        we::Elements::Functions(table.clone().into()),
    );
    module.section(&els);
    let mut code = we::CodeSection::new();
    for &oi in &included {
        let mut m = Mapper {
            funcs: &fmaps[&oi],
            globals: &gmaps[&oi],
            types: &tmaps[&oi],
        };
        for b in &objs[oi].bodies {
            m.parse_function_body(&mut code, b.clone())
                .map_err(|e| format!("{}: {e:?}", objs[oi].name))?;
        }
    }
    let mut call_ctors = we::Function::new([]);
    for f in &ctors {
        call_ctors.instruction(&we::Instruction::Call(*f));
    }
    call_ctors.instruction(&we::Instruction::End);
    code.function(&call_ctors);
    module.section(&code);
    let mut named: Vec<(u32, &str)> = host
        .iter()
        .enumerate()
        .map(|(i, h)| (i as u32, &h.1[..]))
        .collect();
    for &oi in &included {
        for (n, &f) in &objs[oi].exports {
            named.push((base[&oi] + f - objs[oi].fimports.len() as u32, n));
        }
    }
    named.push((ctors_fn, CALL_CTORS));
    named.sort();
    named.dedup_by_key(|x| x.0);
    let mut fnames = we::NameMap::new();
    for (i, n) in named {
        fnames.append(i, n);
    }
    // Engines cap the segment count (V8: 100k), so coalesce neighbours,
    // zero-filling small gaps.
    let mut merged: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut order: Vec<_> = segs.iter().collect();
    order.sort_by_key(|s| s.0);
    for (a, bytes) in order {
        let (a, bytes) = (*a as u64, bytes.as_slice());
        match merged.last_mut() {
            Some((m, buf)) if a >= *m + buf.len() as u64 && a - (*m + buf.len() as u64) <= 64 => {
                buf.resize((a - *m) as usize, 0);
                buf.extend_from_slice(bytes);
            }
            _ => merged.push((a, bytes.to_vec())),
        }
    }
    let mut ds = we::DataSection::new();
    for (a, bytes) in &merged {
        ds.active(
            0,
            &we::ConstExpr::i32_const(*a as i32),
            bytes.iter().copied(),
        );
    }
    module.section(&ds);
    let mut names = we::NameSection::new();
    names.functions(&fnames);
    module.section(&names);
    std::fs::write(&out, module.finish()).map_err(|e| format!("{out}: {e}"))?;
    if std::env::var_os("PLIRON_WASM_LD_VERBOSE").is_some() {
        eprintln!(
            "pliron-wasm-ld: {} objects ({} from archives), {} functions, {} host imports, {} bytes data",
            included.len(),
            included.iter().filter(|&&i| blobs[i].2).count(),
            next,
            host.len(),
            data_end - 1024 - stack()
        );
    }
    Ok(())
}
