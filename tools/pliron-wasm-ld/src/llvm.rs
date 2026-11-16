//! Ingest LLVM relocatable wasm objects (`linking` + `reloc.*`) into the
//! linker's `Obj` model. Clang/`cc` emits these for C deps (zstd, lz4, psm);
//! pliron's own objects use `pliron.link` instead and take the other path.
//!
//! Index relocs (`FUNCTION`/`GLOBAL`/`TYPE_INDEX`, `TABLE_NUMBER`) are written
//! as object-local indices into the code bytes up front so the rest of the
//! linker (GC, `Mapper`) sees ordinary wasm. Memory addresses and table slots
//! stay as late patches (`code_abs` / data relocs) until layout is known.
//!
//! Native ELF/Mach-O LLVM objects are out of scope here; the same shape of
//! "foreign relocatable → our symbol model" will apply there later.

use wasmparser as wp;

use crate::{CodeAbsReloc, DataObj, Obj, DEFAULT_PRIO, GLOBAL, USED};

/// Write a 5-byte padded unsigned LEB128 (LLVM reloc width).
pub fn write_uleb5(buf: &mut [u8], mut v: u32) {
    for i in 0..5 {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if i < 4 {
            b |= 0x80;
        }
        buf[i] = b;
    }
}

/// Write a 5-byte padded signed LEB128 (LLVM reloc width).
pub fn write_sleb5(buf: &mut [u8], mut v: i32) {
    for i in 0..5 {
        let mut b = (v as u32 & 0x7f) as u8;
        v >>= 7;
        if i < 4 {
            b |= 0x80;
        }
        buf[i] = b;
    }
}

fn strip_env_prefix(name: &str) -> &str {
    name.strip_prefix("env.").unwrap_or(name)
}

/// Rewrite FUNCTION/GLOBAL/TYPE/TABLE_NUMBER index immediates in place for
/// LLVM relocatable objects. Must run before any `FunctionBody` borrows the
/// buffer. No-op for pliron objects (`pliron.link` present) or plain wasm.
pub fn prepatch(bytes: &mut [u8]) -> Result<(), String> {
    let patches = {
        let mut has_pliron = false;
        let mut linking = None;
        let mut code_relocs = None;
        let mut code_payload_start = None;
        let mut fimports: Vec<(String, String, u32)> = Vec::new();
        let mut gimports: Vec<(String, String)> = Vec::new();

        for p in wp::Parser::new(0).parse_all(&*bytes) {
            let p = p.map_err(|e| e.to_string())?;
            match p {
                wp::Payload::ImportSection(r) => {
                    for imp in r.into_imports() {
                        let imp = imp.map_err(|e| e.to_string())?;
                        match imp.ty {
                            wp::TypeRef::Func(t) => {
                                fimports.push((imp.module.into(), imp.name.into(), t))
                            }
                            wp::TypeRef::Global(_) => {
                                gimports.push((imp.module.into(), imp.name.into()))
                            }
                            _ => {}
                        }
                    }
                }
                wp::Payload::CodeSectionStart { range, .. } => {
                    code_payload_start = Some(range.start);
                }
                wp::Payload::CustomSection(c) => match (c.name(), c.as_known()) {
                    ("pliron.link", _) => has_pliron = true,
                    ("linking", wp::KnownCustom::Linking(l)) => linking = Some(l),
                    ("reloc.CODE", wp::KnownCustom::Reloc(r)) => code_relocs = Some(r),
                    _ => {}
                },
                _ => {}
            }
        }
        if has_pliron || linking.is_none() {
            return Ok(());
        }
        let linking = linking.unwrap();
        // Data-only members have linking metadata but no code to patch.
        let Some(code_start) = code_payload_start else {
            return Ok(());
        };
        let mut syms = Vec::new();
        for sub in linking.subsections() {
            let sub = sub.map_err(|e| e.to_string())?;
            if let wp::Linking::SymbolTable(r) = sub {
                for s in r {
                    let s = s.map_err(|e| e.to_string())?;
                    syms.push(Sym::from_wp(s, &fimports, &gimports));
                }
            }
        }
        let Some(r) = code_relocs else {
            return Ok(());
        };
        let mut patches = Vec::new();
        for e in r.entries() {
            let e = e.map_err(|e| e.to_string())?;
            let file_off = code_start + e.offset as usize;
            use wp::RelocationType::*;
            let v = match e.ty {
                FunctionIndexLeb => syms
                    .get(e.index as usize)
                    .and_then(|s| s.func_index())
                    .ok_or_else(|| format!("FUNCTION_INDEX bad sym {}", e.index))?,
                GlobalIndexLeb => syms
                    .get(e.index as usize)
                    .and_then(|s| s.global_index())
                    .ok_or_else(|| format!("GLOBAL_INDEX bad sym {}", e.index))?,
                TypeIndexLeb => e.index,
                TableNumberLeb => 0,
                _ => continue,
            };
            patches.push((file_off, v));
        }
        patches
    };
    for (file_off, v) in patches {
        write_uleb5(&mut bytes[file_off..file_off + 5], v);
    }
    Ok(())
}

/// Fill `o.funcs` / `o.exports` / `o.data` / reloc fields from LLVM `linking`
/// + `reloc.CODE`/`reloc.DATA`. No-op when `pliron.link` already populated
/// `funcs` (pliron objects win).
pub fn ingest(o: &mut Obj<'_>, bytes: &[u8]) -> Result<(), String> {
    if !o.funcs.is_empty() {
        return Ok(());
    }

    let mut linking = None;
    let mut code_relocs = None;
    let mut data_relocs = None;
    let mut code_payload_start = None;
    let mut data_payload_start = None;
    let mut data_segs: Vec<SegBytes> = Vec::new();

    for p in wp::Parser::new(0).parse_all(bytes) {
        let p = p.map_err(|e| format!("{}: {e}", o.name))?;
        match p {
            wp::Payload::CodeSectionStart { range, .. } => {
                code_payload_start = Some(range.start);
            }
            wp::Payload::DataSection(r) => {
                data_payload_start = Some(r.range().start);
                data_segs = parse_data_segs(bytes, r.range().start)
                    .map_err(|e| format!("{}: {e}", o.name))?;
            }
            wp::Payload::CustomSection(c) => match (c.name(), c.as_known()) {
                ("linking", wp::KnownCustom::Linking(l)) => linking = Some(l),
                ("reloc.CODE", wp::KnownCustom::Reloc(r)) => code_relocs = Some(r),
                ("reloc.DATA", wp::KnownCustom::Reloc(r)) => data_relocs = Some(r),
                _ => {}
            },
            _ => {}
        }
    }
    let Some(linking) = linking else {
        return Ok(());
    };
    let mut syms: Vec<Sym> = Vec::new();
    let mut seg_meta: Vec<(String, u32)> = Vec::new();
    let mut init_funcs: Vec<(u32, u32)> = Vec::new();
    for sub in linking.subsections() {
        let sub = sub.map_err(|e| format!("{}: linking: {e}", o.name))?;
        match sub {
            wp::Linking::SymbolTable(r) => {
                for s in r {
                    let s = s.map_err(|e| format!("{}: symtab: {e}", o.name))?;
                    syms.push(Sym::from_wp(s, &o.fimports, &o.gimports));
                }
            }
            wp::Linking::SegmentInfo(r) => {
                for s in r {
                    let s = s.map_err(|e| format!("{}: seginfo: {e}", o.name))?;
                    seg_meta.push((s.name.to_string(), 1u32 << s.alignment.min(31)));
                }
            }
            wp::Linking::InitFuncs(r) => {
                for f in r {
                    let f = f.map_err(|e| format!("{}: init funcs: {e}", o.name))?;
                    init_funcs.push((f.priority, f.symbol_index));
                }
            }
            wp::Linking::ComdatInfo(_) | wp::Linking::Unknown { .. } => {}
        }
    }
    // Constructors. LLVM's object writer turns each `.init_array[.N]`
    // pointer into a `WASM_INIT_FUNCS` record (priority, function symbol)
    // and leaves the section out of the data, unless a symbol refers to
    // it; then the segment is ordinary data here, not a second list.
    for (prio, si) in init_funcs {
        match syms.get(si as usize) {
            Some(Sym::Func { name, .. }) => o.init_funcs.push((prio, name.clone())),
            _ => return Err(format!("{}: init function: bad symbol {si}", o.name)),
        }
    }

    // One DataObj per wasm data segment; named data symbols alias into them.
    for (i, seg) in data_segs.iter().enumerate() {
        let (name, align) = seg_meta
            .get(i)
            .cloned()
            .unwrap_or_else(|| (format!("__llvm_seg{i}"), 1));
        o.data_names.entry(name.clone()).or_insert(o.data.len());
        o.data.push(DataObj {
            name,
            flags: 0,
            prio: DEFAULT_PRIO,
            align,
            bytes: seg.bytes.clone(),
            relocs: vec![],
        });
    }

    for s in &syms {
        if let Sym::Data {
            name,
            flags,
            segment,
            offset,
            ..
        } = s
            && !flags.contains(wp::SymbolFlags::UNDEFINED)
        {
            let di = *segment as usize;
            if di >= o.data.len() {
                return Err(format!("{}: data symbol {name} bad segment {di}", o.name));
            }
            o.data_names.entry(name.clone()).or_insert(di);
            o.data_sym_off.insert(name.clone(), *offset);
            if flags.contains(wp::SymbolFlags::NO_STRIP) {
                o.data[di].flags |= USED;
            }
            if !flags.contains(wp::SymbolFlags::BINDING_LOCAL) {
                o.data[di].flags |= GLOBAL;
            }
        }
    }

    for s in &syms {
        if let Sym::Func { name, flags, index } = s {
            if flags.contains(wp::SymbolFlags::UNDEFINED) {
                continue;
            }
            let name = name.clone();
            o.exports.insert(name.clone(), *index);
            let mut f = 0u8;
            if !flags.contains(wp::SymbolFlags::BINDING_LOCAL) {
                f |= GLOBAL;
            }
            o.funcs.push((name, f));
        }
    }

    // Index relocs were applied by `prepatch`; only absolute/table patches remain.
    if let (Some(r), Some(code_start)) = (code_relocs, code_payload_start) {
        for e in r.entries() {
            let e = e.map_err(|e| format!("{}: reloc.CODE: {e}", o.name))?;
            let file_off = code_start + e.offset as usize;
            apply_code_abs_reloc(o, &syms, file_off, e)?;
        }
    }

    if let Some(r) = data_relocs {
        let _data_start = data_payload_start
            .ok_or_else(|| format!("{}: reloc.DATA but no data section", o.name))?;
        for e in r.entries() {
            let e = e.map_err(|e| format!("{}: reloc.DATA: {e}", o.name))?;
            apply_data_reloc(o, &syms, &data_segs, e)?;
        }
    }

    o.llvm = true;
    Ok(())
}

struct SegBytes {
    bytes: Vec<u8>,
    /// Offset of this segment's data payload within the Data section contents.
    data_off: u32,
}

fn parse_data_segs(bytes: &[u8], payload_start: usize) -> Result<Vec<SegBytes>, String> {
    let mut r = wp::BinaryReader::new(&bytes[payload_start..], payload_start);
    let n = r.read_var_u32().map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let flags = r.read_var_u32().map_err(|e| e.to_string())?;
        if flags & 0x1 == 0 {
            if flags & 0x2 != 0 {
                let _mem = r.read_var_u32().map_err(|e| e.to_string())?;
            }
            loop {
                let op = r.read_u8().map_err(|e| e.to_string())?;
                if op == 0x0b {
                    break;
                }
                if op == 0x41 {
                    let _ = r.read_var_i32().map_err(|e| e.to_string())?;
                } else if op == 0x42 {
                    let _ = r.read_var_i64().map_err(|e| e.to_string())?;
                } else {
                    return Err(format!("unsupported data segment init opcode {op:#x}"));
                }
            }
        }
        let size = r.read_var_u32().map_err(|e| e.to_string())? as usize;
        let data_off = (r.original_position() - payload_start) as u32;
        let data = r
            .read_bytes(size)
            .map_err(|e| e.to_string())?
            .to_vec();
        out.push(SegBytes {
            bytes: data,
            data_off,
        });
    }
    Ok(out)
}

enum Sym {
    Func {
        name: String,
        flags: wp::SymbolFlags,
        index: u32,
    },
    Data {
        name: String,
        flags: wp::SymbolFlags,
        segment: u32,
        offset: u32,
        #[allow(dead_code)]
        size: u32,
    },
    Global {
        name: String,
        index: u32,
    },
    Table {
        name: String,
    },
    Other,
}

impl Sym {
    fn from_wp(
        s: wp::SymbolInfo<'_>,
        fimports: &[(String, String, u32)],
        gimports: &[(String, String)],
    ) -> Self {
        match s {
            wp::SymbolInfo::Func { flags, index, name } => {
                let name = name
                    .map(strip_env_prefix)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        fimports
                            .get(index as usize)
                            .map(|(_, n, _)| n.clone())
                            .unwrap_or_else(|| format!("__func{index}"))
                    });
                Sym::Func { name, flags, index }
            }
            wp::SymbolInfo::Data {
                flags,
                name,
                symbol,
            } => {
                let (segment, offset, size) = symbol
                    .map(|d| (d.index, d.offset, d.size))
                    .unwrap_or((0, 0, 0));
                Sym::Data {
                    name: strip_env_prefix(name).to_string(),
                    flags,
                    segment,
                    offset,
                    size,
                }
            }
            wp::SymbolInfo::Global { flags: _, index, name } => {
                let name = name
                    .map(strip_env_prefix)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        gimports
                            .get(index as usize)
                            .map(|(_, n)| n.clone())
                            .unwrap_or_else(|| format!("__global{index}"))
                    });
                Sym::Global { name, index }
            }
            wp::SymbolInfo::Table { name, .. } => Sym::Table {
                name: name.unwrap_or("").to_string(),
            },
            _ => Sym::Other,
        }
    }

    fn func_index(&self) -> Option<u32> {
        match self {
            Sym::Func { index, .. } => Some(*index),
            _ => None,
        }
    }
    fn global_index(&self) -> Option<u32> {
        match self {
            Sym::Global { index, .. } => Some(*index),
            _ => None,
        }
    }
    fn name(&self) -> Option<&str> {
        match self {
            Sym::Func { name, .. }
            | Sym::Data { name, .. }
            | Sym::Global { name, .. }
            | Sym::Table { name, .. } => Some(name),
            Sym::Other => None,
        }
    }
}

fn apply_code_abs_reloc(
    o: &mut Obj<'_>,
    syms: &[Sym],
    file_off: usize,
    e: wp::RelocationEntry,
) -> Result<(), String> {
    use wp::RelocationType::*;
    match e.ty {
        FunctionIndexLeb | GlobalIndexLeb | TypeIndexLeb | TableNumberLeb => Ok(()),
        MemoryAddrLeb | MemoryAddrSleb | MemoryAddrI32 | TableIndexSleb | TableIndexI32 => {
            let name = syms
                .get(e.index as usize)
                .and_then(|s| s.name())
                .ok_or_else(|| format!("{}: abs reloc bad sym {}", o.name, e.index))?
                .to_string();
            let body = body_index_at(o, file_off)
                .ok_or_else(|| format!("{}: reloc offset {file_off:#x} outside bodies", o.name))?;
            let body_start = o.bodies[body].range().start;
            let off = (file_off - body_start) as u32;
            let func = matches!(e.ty, TableIndexSleb | TableIndexI32);
            let sleb = matches!(e.ty, MemoryAddrSleb | MemoryAddrLeb | TableIndexSleb);
            o.code_abs.push(CodeAbsReloc {
                body: body as u32,
                off,
                func,
                sleb,
                sym: name,
                addend: e.addend as i32,
            });
            Ok(())
        }
        SectionOffsetI32 | FunctionOffsetI32 | FunctionOffsetI64 => Ok(()),
        MemoryAddrRelSleb
        | TableIndexRelSleb
        | MemoryAddrTlsSleb
        | MemoryAddrLeb64
        | MemoryAddrSleb64
        | MemoryAddrI64
        | MemoryAddrRelSleb64
        | TableIndexSleb64
        | TableIndexI64
        | TableIndexRelSleb64
        | MemoryAddrTlsSleb64
        | MemoryAddrLocrelI32
        | EventIndexLeb
        | GlobalIndexI32
        | FunctionIndexI32 => Err(format!(
            "{}: unsupported code reloc {:?}",
            o.name, e.ty
        )),
    }
}

fn body_index_at(o: &Obj<'_>, file_off: usize) -> Option<usize> {
    o.bodies.iter().position(|b| {
        let r = b.range();
        file_off >= r.start && file_off < r.end
    })
}

fn apply_data_reloc(
    o: &mut Obj<'_>,
    syms: &[Sym],
    segs: &[SegBytes],
    e: wp::RelocationEntry,
) -> Result<(), String> {
    use wp::RelocationType::*;
    let section_off = e.offset;
    let (di, local_off) = segs
        .iter()
        .enumerate()
        .find_map(|(i, s)| {
            let end = s.data_off + s.bytes.len() as u32;
            if section_off >= s.data_off && section_off + 4 <= end {
                Some((i, section_off - s.data_off))
            } else {
                None
            }
        })
        .ok_or_else(|| {
            format!(
                "{}: DATA reloc offset {section_off:#x} outside segment payloads",
                o.name
            )
        })?;
    match e.ty {
        MemoryAddrI32 | TableIndexI32 => {
            let name = syms
                .get(e.index as usize)
                .and_then(|s| s.name())
                .ok_or_else(|| format!("{}: DATA reloc bad sym {}", o.name, e.index))?
                .to_string();
            let func = matches!(e.ty, TableIndexI32);
            o.data[di]
                .relocs
                .push((local_off, func, name, e.addend as i32));
        }
        other => {
            return Err(format!("{}: unsupported DATA reloc {other:?}", o.name));
        }
    }
    Ok(())
}
