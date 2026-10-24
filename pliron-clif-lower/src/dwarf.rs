//! DWARF for `-g` (ELF and Mach-O objects): one compile unit per object,
//! a line table, and a `DW_TAG_subprogram` per defined function with its
//! inlined-subroutine tree. Input is what cranelift reports per function
//! — machine-code ranges tagged with the CLIF srcloc, which lowering set
//! to the op's `pliron_ir_opt::dbg` location id.
//!
//! Relocations follow clang: on ELF every address and cross-section
//! offset is relocated against a symbol; on Mach-O (whose linker leaves
//! DWARF in the objects for dsymutil/lldb) offsets are plain and
//! addresses are section-relative, the stored value being the address
//! in the object (`fix_macho_text_addrs` corrects that after layout).

use cranelift_module::FuncId;
use cranelift_object::ObjectProduct;
use gimli::write::{
    Address, AttributeValue, DirectoryId, DwarfUnit, EndianVec, Expression, FileId, LineProgram,
    LineString, Location, LocationList, Range, RangeList, Sections, UnitEntryId, Writer,
};
use gimli::{Encoding, Format, LineEncoding, RunTimeEndian, SectionId};
use object::write::{Relocation, SymbolSection};
use object::{BinaryFormat, RelocationEncoding, RelocationFlags, RelocationKind, SectionKind};
use pliron_ir_opt::dbg::{DbgInfo, Piece, Type};
use rustc_hash::FxHashMap;

/// One compiled function: srcloc ranges `(start, end, loc id)` relative to
/// the function start, sorted by start.
pub struct FnLines {
    pub id: FuncId,
    pub sym: String,
    pub size: u32,
    pub ranges: Vec<(u32, u32, u32)>,
    /// Variable-holding slots: (decl group, offset from the frame pointer).
    pub vars: Vec<(u32, i64)>,
    /// Value labels (`pliron_ir_opt::dbg::Piece` ids) and where each one
    /// lives: `(start, end, location)` code ranges, sorted by start.
    pub labels: Vec<(u32, Vec<(u32, u32, ValLoc)>)>,
}

/// Where a value label is, over one code range.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValLoc {
    /// A DWARF register number.
    Reg(u16),
    /// A spill slot: this offset from the canonical frame address.
    Cfa(i64),
    /// A constant (no storage).
    Const(u64),
}

#[derive(Clone)]
enum Target {
    /// `Address::Symbol` index: a function of `FnLines`.
    Fn(usize),
    Section(SectionId),
}

#[derive(Clone)]
struct Reloc {
    offset: u32,
    size: u8,
    target: Target,
    addend: i64,
}

#[derive(Clone)]
struct W {
    w: EndianVec<RunTimeEndian>,
    relocs: Vec<Reloc>,
    /// Mach-O: cross-section offsets need no relocation.
    macho: bool,
}

impl Writer for W {
    type Endian = RunTimeEndian;
    fn endian(&self) -> RunTimeEndian {
        self.w.endian()
    }
    fn len(&self) -> usize {
        self.w.len()
    }
    fn write(&mut self, b: &[u8]) -> gimli::write::Result<()> {
        self.w.write(b)
    }
    fn write_at(&mut self, o: usize, b: &[u8]) -> gimli::write::Result<()> {
        self.w.write_at(o, b)
    }
    fn write_address(&mut self, a: Address, size: u8) -> gimli::write::Result<()> {
        match a {
            Address::Constant(v) => self.write_udata(v, size),
            Address::Symbol { symbol, addend } => {
                let offset = self.len() as u32;
                self.relocs.push(Reloc { offset, size, target: Target::Fn(symbol), addend });
                self.write_udata(0, size)
            }
        }
    }
    fn write_offset(&mut self, v: usize, s: SectionId, size: u8) -> gimli::write::Result<()> {
        if !self.macho {
            let offset = self.len() as u32;
            self.relocs.push(Reloc { offset, size, target: Target::Section(s), addend: v as i64 });
            return self.write_udata(0, size);
        }
        self.write_udata(v as u64, size)
    }
    fn write_offset_at(
        &mut self,
        o: usize,
        v: usize,
        s: SectionId,
        size: u8,
    ) -> gimli::write::Result<()> {
        if !self.macho {
            self.relocs.push(Reloc { offset: o as u32, size, target: Target::Section(s), addend: v as i64 });
            return self.write_udata_at(o, 0, size);
        }
        self.write_udata_at(o, v as u64, size)
    }
}

/// An inline instance: (call-site loc, inlined callee subprogram).
type InstKey = (u32, u32);

#[derive(Default)]
struct Inst {
    ranges: Vec<(u32, u32)>,
    kids: Vec<(InstKey, Inst)>,
    /// Its `DW_TAG_inlined_subroutine`, once written.
    die: Option<UnitEntryId>,
}

impl Inst {
    fn add(&mut self, path: &[InstKey], s: u32, e: u32) {
        match self.ranges.last_mut() {
            Some(r) if r.1 == s => r.1 = e,
            _ => self.ranges.push((s, e)),
        }
        let Some((&k, rest)) = path.split_first() else {
            return;
        };
        let i = match self.kids.iter().position(|(kk, _)| *kk == k) {
            Some(i) => i,
            None => {
                self.kids.push((k, Inst::default()));
                self.kids.len() - 1
            }
        };
        self.kids[i].1.add(rest, s, e);
    }

    fn find(&self, path: &[InstKey]) -> Option<&Inst> {
        let Some((k, rest)) = path.split_first() else {
            return Some(self);
        };
        self.kids.iter().find(|(kk, _)| kk == k)?.1.find(rest)
    }
}

struct Cx<'a> {
    d: &'a DbgInfo,
    dw: DwarfUnit,
    files: FxHashMap<u32, FileId>,
    dirs: FxHashMap<String, DirectoryId>,
    abstract_subs: FxHashMap<u32, UnitEntryId>,
    types: FxHashMap<u32, UnitEntryId>,
}

impl Cx<'_> {
    /// The DIE for type id `t` (`None` for void), written on first use.
    fn ty(&mut self, t: u32) -> Option<UnitEntryId> {
        if t == 0 {
            return None;
        }
        if let Some(&e) = self.types.get(&t) {
            return Some(e);
        }
        let root = self.dw.unit.root();
        let ty = self.d.types[t as usize].clone();
        let tag = match &ty {
            Type::Base { .. } => gimli::DW_TAG_base_type,
            Type::Ptr { .. } => gimli::DW_TAG_pointer_type,
            Type::Struct { union: true, .. } => gimli::DW_TAG_union_type,
            Type::Struct { .. } | Type::Opaque { .. } => gimli::DW_TAG_structure_type,
            Type::Array { .. } => gimli::DW_TAG_array_type,
            Type::CEnum { .. } => gimli::DW_TAG_enumeration_type,
        };
        let die = self.dw.unit.add(root, tag);
        // Registered before its parts, so a type reaching itself through a
        // pointer refers back to this DIE.
        self.types.insert(t, die);
        let str = |s: &str| AttributeValue::String(s.as_bytes().to_vec());
        match ty {
            Type::Base { name, size, enc } => {
                let e = self.dw.unit.get_mut(die);
                e.set(gimli::DW_AT_name, str(&name));
                e.set(gimli::DW_AT_byte_size, AttributeValue::Udata(size as u64));
                e.set(gimli::DW_AT_encoding, AttributeValue::Encoding(gimli::DwAte(enc)));
            }
            Type::Ptr { name, pointee } => {
                let p = self.ty(pointee);
                let e = self.dw.unit.get_mut(die);
                e.set(gimli::DW_AT_name, str(&name));
                e.set(gimli::DW_AT_byte_size, AttributeValue::Udata(8));
                if let Some(p) = p {
                    e.set(gimli::DW_AT_type, AttributeValue::UnitRef(p));
                }
            }
            Type::Struct { name, size, align, fields, .. } => {
                let e = self.dw.unit.get_mut(die);
                e.set(gimli::DW_AT_name, str(&name));
                e.set(gimli::DW_AT_byte_size, AttributeValue::Udata(size as u64));
                e.set(gimli::DW_AT_alignment, AttributeValue::Udata(align as u64));
                for f in fields {
                    let ft = self.ty(f.ty);
                    let m = self.dw.unit.add(die, gimli::DW_TAG_member);
                    let e = self.dw.unit.get_mut(m);
                    e.set(gimli::DW_AT_name, str(&f.name));
                    if let Some(ft) = ft {
                        e.set(gimli::DW_AT_type, AttributeValue::UnitRef(ft));
                    }
                    e.set(gimli::DW_AT_data_member_location, AttributeValue::Udata(f.offset as u64));
                }
            }
            Type::Array { name, elem, len, size } => {
                let et = self.ty(elem);
                let e = self.dw.unit.get_mut(die);
                e.set(gimli::DW_AT_name, str(&name));
                e.set(gimli::DW_AT_byte_size, AttributeValue::Udata(size as u64));
                if let Some(et) = et {
                    e.set(gimli::DW_AT_type, AttributeValue::UnitRef(et));
                }
                let r = self.dw.unit.add(die, gimli::DW_TAG_subrange_type);
                self.dw.unit.get_mut(r).set(gimli::DW_AT_count, AttributeValue::Udata(len));
            }
            Type::CEnum { name, size, signed, variants } => {
                let e = self.dw.unit.get_mut(die);
                e.set(gimli::DW_AT_name, str(&name));
                e.set(gimli::DW_AT_byte_size, AttributeValue::Udata(size as u64));
                for (n, v) in variants {
                    let m = self.dw.unit.add(die, gimli::DW_TAG_enumerator);
                    let e = self.dw.unit.get_mut(m);
                    e.set(gimli::DW_AT_name, str(&n));
                    let v = if signed { AttributeValue::Sdata(v as i64) } else { AttributeValue::Udata(v as u64) };
                    e.set(gimli::DW_AT_const_value, v);
                }
            }
            Type::Opaque { name, size } => {
                let e = self.dw.unit.get_mut(die);
                e.set(gimli::DW_AT_name, str(&name));
                e.set(gimli::DW_AT_byte_size, AttributeValue::Udata(size as u64));
            }
        }
        Some(die)
    }

    fn file(&mut self, f: u32) -> FileId {
        if let Some(&id) = self.files.get(&f) {
            return id;
        }
        let path = &self.d.files[f as usize];
        let path = if path.is_empty() { "<unknown>" } else { path.as_str() };
        let (dir, base) = match path.rfind('/') {
            Some(i) if i > 0 => (&path[..i], &path[i + 1..]),
            _ => ("", path),
        };
        let lp = &mut self.dw.unit.line_program;
        let dir = if dir.is_empty() {
            lp.default_directory()
        } else {
            *self
                .dirs
                .entry(dir.to_string())
                .or_insert_with(|| lp.add_directory(LineString::String(dir.as_bytes().to_vec())))
        };
        let id = lp.add_file(LineString::String(base.as_bytes().to_vec()), dir, None);
        self.files.insert(f, id);
        id
    }

    fn name_attrs(&mut self, die: UnitEntryId, sub: u32) {
        let s = &self.d.subs[sub as usize];
        let (name, linkage, file, line) = (s.name.clone(), s.linkage.clone(), s.file, s.line);
        let fid = self.file(file);
        let e = self.dw.unit.get_mut(die);
        e.set(gimli::DW_AT_name, AttributeValue::String(name.into_bytes()));
        if !linkage.is_empty() {
            e.set(gimli::DW_AT_linkage_name, AttributeValue::String(linkage.into_bytes()));
        }
        e.set(gimli::DW_AT_decl_file, AttributeValue::FileIndex(Some(fid)));
        e.set(gimli::DW_AT_decl_line, AttributeValue::Udata(line as u64));
    }

    /// The abstract `DW_TAG_subprogram` inlined instances of `sub` point at.
    fn abstract_sub(&mut self, sub: u32) -> UnitEntryId {
        if let Some(&e) = self.abstract_subs.get(&sub) {
            return e;
        }
        let root = self.dw.unit.root();
        let die = self.dw.unit.add(root, gimli::DW_TAG_subprogram);
        self.name_attrs(die, sub);
        self.dw
            .unit
            .get_mut(die)
            .set(gimli::DW_AT_inline, AttributeValue::Inline(gimli::DW_INL_inlined));
        self.abstract_subs.insert(sub, die);
        die
    }

    fn ranges_attr(&mut self, die: UnitEntryId, sym: usize, ranges: &[(u32, u32)]) {
        let begin = |o: u32| Address::Symbol { symbol: sym, addend: o as i64 };
        if let [(s, e)] = ranges {
            let d = self.dw.unit.get_mut(die);
            d.set(gimli::DW_AT_low_pc, AttributeValue::Address(begin(*s)));
            d.set(gimli::DW_AT_high_pc, AttributeValue::Udata((e - s) as u64));
        } else {
            let list = RangeList(
                ranges
                    .iter()
                    .map(|&(s, e)| Range::StartLength { begin: begin(s), length: (e - s) as u64 })
                    .collect(),
            );
            let id = self.dw.unit.ranges.add(list);
            self.dw.unit.get_mut(die).set(gimli::DW_AT_ranges, AttributeValue::RangeListRef(id));
        }
    }

    fn inlined(&mut self, parent: UnitEntryId, sym: usize, kids: &mut [(InstKey, Inst)]) {
        for ((site, callee), inst) in kids {
            let (site, callee) = (*site, *callee);
            let origin = self.abstract_sub(callee);
            let die = self.dw.unit.add(parent, gimli::DW_TAG_inlined_subroutine);
            let l = self.d.locs[site as usize];
            let fid = self.file(l.file);
            let e = self.dw.unit.get_mut(die);
            e.set(gimli::DW_AT_abstract_origin, AttributeValue::UnitRef(origin));
            e.set(gimli::DW_AT_call_file, AttributeValue::FileIndex(Some(fid)));
            e.set(gimli::DW_AT_call_line, AttributeValue::Udata(l.line as u64));
            e.set(gimli::DW_AT_call_column, AttributeValue::Udata(l.col as u64));
            self.ranges_attr(die, sym, &inst.ranges);
            inst.die = Some(die);
            self.inlined(die, sym, &mut inst.kids);
        }
    }

    /// The function's variables, each under the subprogram or the inlined
    /// instance its declaration's location chain names; parameters first,
    /// in argument order.
    /// The DIE a variable declared at `loc` belongs under: the subprogram
    /// `die` or one of its inlined instances; `None` when `loc` belongs to
    /// another function or to an instance with no code.
    fn var_parent(&self, die: UnitEntryId, sub: u32, tree: &Inst, loc: u32) -> Option<UnitEntryId> {
        let mut path = Vec::new();
        let mut x = self.d.locs[loc as usize];
        while x.inlined_at != 0 {
            path.push((x.inlined_at, x.scope));
            x = self.d.locs[x.inlined_at as usize];
        }
        if loc != 0 && x.scope != sub {
            return None;
        }
        path.reverse();
        if path.is_empty() { Some(die) } else { tree.find(&path).and_then(|i| i.die) }
    }

    #[allow(clippy::too_many_arguments)]
    fn vars(
        &mut self,
        die: UnitEntryId,
        sub: u32,
        sym: usize,
        tree: &Inst,
        vars: &[(u32, i64)],
        labels: &[(u32, Vec<(u32, u32, ValLoc)>)],
        consts: &[(u32, [(u32, u32, ValLoc); 1])],
        fp_reg: u16,
    ) {
        let mut out: Vec<(UnitEntryId, u32, u32, AttributeValue)> = Vec::new();
        let mut seen = rustc_hash::FxHashSet::default();
        for &(g, fp_off) in vars {
            for &di in &self.d.groups[g as usize] {
                let decl = &self.d.decls[di as usize];
                let Some(parent) = self.var_parent(die, sub, tree, decl.loc) else {
                    continue;
                };
                if !seen.insert((parent, decl.var)) {
                    continue;
                }
                let mut loc = Expression::new();
                loc.op_breg(gimli::Register(fp_reg), fp_off + decl.offset as i64);
                for &o in &decl.derefs {
                    loc.op_deref();
                    if o != 0 {
                        loc.op_plus_uconst(o as u64);
                    }
                }
                out.push((parent, decl.var, self.d.vars[decl.var as usize].arg, AttributeValue::Exprloc(loc)));
            }
        }
        // Variables held in values: group the labels' pieces by variable
        // instance, then one location list per instance.
        let mut inst: FxHashMap<(UnitEntryId, u32), Vec<(Piece, &[(u32, u32, ValLoc)])>> = FxHashMap::default();
        let mut keys = Vec::new();
        {
            let tab = self.d.pieces.lock().unwrap();
            let lab = labels.iter().map(|(l, rs)| (*l, &rs[..]));
            let cst = consts.iter().filter(|(l, _)| !tab.labelled.contains(l)).map(|(l, r)| (*l, &r[..]));
            for (l, rs) in lab.chain(cst) {
                let Some(&pc) = tab.list.get(l as usize) else {
                    continue;
                };
                let Some(parent) = self.var_parent(die, sub, tree, pc.loc) else {
                    continue;
                };
                if seen.contains(&(parent, pc.var)) {
                    continue;
                }
                let e = inst.entry((parent, pc.var)).or_default();
                if e.is_empty() {
                    keys.push((parent, pc.var));
                }
                e.push((pc, rs));
            }
        }
        for k in keys {
            let pcs = &inst[&k];
            let vsize = self.d.type_size(self.d.vars[k.1 as usize].ty);
            let list = loc_list(sym, vsize, pcs, fp_reg);
            seen.insert(k);
            let id = self.dw.unit.locations.add(LocationList(list));
            out.push((k.0, k.1, self.d.vars[k.1 as usize].arg, AttributeValue::LocationListRef(id)));
        }
        // Parameters (arg > 0) in order, then locals in declaration order.
        out.sort_by_key(|&(_, v, arg, _)| if arg > 0 { (0, arg, v) } else { (1, 0, v) });
        for (parent, v, arg, loc) in out {
            let var = self.d.vars[v as usize].clone();
            let t = self.ty(var.ty);
            let fid = self.file(var.file);
            let tag = if arg > 0 { gimli::DW_TAG_formal_parameter } else { gimli::DW_TAG_variable };
            let vd = self.dw.unit.add(parent, tag);
            let e = self.dw.unit.get_mut(vd);
            e.set(gimli::DW_AT_name, AttributeValue::String(var.name.into_bytes()));
            if let Some(t) = t {
                e.set(gimli::DW_AT_type, AttributeValue::UnitRef(t));
            }
            e.set(gimli::DW_AT_decl_file, AttributeValue::FileIndex(Some(fid)));
            e.set(gimli::DW_AT_decl_line, AttributeValue::Udata(var.line as u64));
            e.set(gimli::DW_AT_location, loc);
        }
    }
}

/// One variable's location list from its pieces' label ranges: split the
/// code at every range boundary; in each segment, compose the located
/// pieces with `DW_OP_piece`, or use the plain location when one piece
/// covers the whole variable. Segments where some byte has no location
/// are left out (the variable reads as unavailable there).
fn loc_list(sym: usize, vsize: u32, pcs: &[(Piece, &[(u32, u32, ValLoc)])], fp_reg: u16) -> Vec<Location> {
    let mut cuts: Vec<u32> = pcs.iter().flat_map(|(_, rs)| rs.iter().flat_map(|r| [r.0, r.1])).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut order: Vec<usize> = (0..pcs.len()).collect();
    order.sort_by_key(|&i| (pcs[i].0.offset, std::cmp::Reverse(pcs[i].0.size)));
    let mut out: Vec<(u32, u32, Vec<u8>)> = Vec::new();
    let mut cur = vec![0usize; pcs.len()];
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let mut e = Vec::new();
        let mut at = 0u32;
        for &i in &order {
            let (pc, rs) = &pcs[i];
            if pc.offset < at {
                continue;
            }
            while cur[i] < rs.len() && rs[cur[i]].1 <= a {
                cur[i] += 1;
            }
            let Some(&(_, _, loc)) = rs.get(cur[i]).filter(|r| r.0 <= a) else {
                continue;
            };
            if pc.offset > at {
                break;
            }
            match loc {
                ValLoc::Reg(r) if r < 32 => e.push(gimli::DW_OP_reg0.0 + r as u8),
                ValLoc::Reg(r) => {
                    e.push(gimli::DW_OP_regx.0);
                    uleb(&mut e, r as u64);
                }
                // Every frame with spill slots has a frame pointer, 16
                // bytes below the CFA (aarch64 and x86_64 alike).
                ValLoc::Cfa(o) => {
                    e.push(gimli::DW_OP_breg0.0 + fp_reg as u8);
                    sleb(&mut e, o + 16);
                }
                ValLoc::Const(c) => {
                    e.push(gimli::DW_OP_constu.0);
                    uleb(&mut e, c);
                    e.push(gimli::DW_OP_stack_value.0);
                }
            }
            if pc.offset == 0 && pc.size >= vsize {
                at = vsize;
                break;
            }
            op_piece(&mut e, pc.size);
            at = pc.offset + pc.size;
        }
        // Only whole variables: lldb reads a missing piece as zero bytes,
        // so a half-known `&str` would show a null pointer, not "unknown".
        if e.is_empty() || at < vsize {
            continue;
        }
        match out.last_mut() {
            Some(l) if l.1 == a && l.2 == e => l.1 = b,
            _ => out.push((a, b, e)),
        }
    }
    out.into_iter()
        .map(|(a, b, bytes)| Location::StartLength {
            begin: Address::Symbol { symbol: sym, addend: a as i64 },
            length: (b - a) as u64,
            data: Expression::raw(bytes),
        })
        .collect()
}

fn op_piece(e: &mut Vec<u8>, n: u32) {
    e.push(gimli::DW_OP_piece.0);
    uleb(e, n as u64);
}

fn uleb(e: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            e.push(b);
            return;
        }
        e.push(b | 0x80);
    }
}

fn sleb(e: &mut Vec<u8>, mut v: i64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0) {
            e.push(b);
            return;
        }
        e.push(b | 0x80);
    }
}

/// Write the debug sections for `fns` into `product`'s object.
pub fn emit(product: &mut ObjectProduct, d: &DbgInfo, fns: &[FnLines], fp_reg: u16, consts: &FxHashMap<u32, u64>) {
    let obj = &product.object;
    let macho = obj.format() == BinaryFormat::MachO;
    if !matches!(obj.format(), BinaryFormat::MachO | BinaryFormat::Elf) {
        return;
    }
    // Every target this backend emits DWARF for is little-endian.
    let endian = RunTimeEndian::Little;
    let encoding = Encoding { format: Format::Dwarf32, version: 4, address_size: 8 };
    let mut dw = DwarfUnit::new(encoding);
    let dir = if d.comp_dir.is_empty() { "." } else { d.comp_dir.as_str() };
    dw.unit.line_program = LineProgram::new(
        encoding,
        LineEncoding::default(),
        LineString::String(dir.as_bytes().to_vec()),
        None,
        LineString::String(d.cu_name.as_bytes().to_vec()),
        None,
    );
    let mut cx = Cx {
        d,
        dw,
        files: FxHashMap::default(),
        dirs: FxHashMap::default(),
        abstract_subs: FxHashMap::default(),
        types: FxHashMap::default(),
    };
    let root = cx.dw.unit.root();
    {
        let r = cx.dw.unit.get_mut(root);
        r.set(gimli::DW_AT_producer, AttributeValue::String(d.producer.as_bytes().to_vec()));
        r.set(gimli::DW_AT_language, AttributeValue::Language(gimli::DW_LANG_Rust));
        r.set(gimli::DW_AT_name, AttributeValue::String(d.cu_name.as_bytes().to_vec()));
        r.set(gimli::DW_AT_comp_dir, AttributeValue::String(dir.as_bytes().to_vec()));
        r.set(gimli::DW_AT_low_pc, AttributeValue::Address(Address::Constant(0)));
    }
    // Constant pieces by the function they belong to (their location
    // chain's outermost scope).
    let mut sub_consts: FxHashMap<u32, Vec<(u32, u64)>> = FxHashMap::default();
    {
        let tab = d.pieces.lock().unwrap();
        for (&id, &c) in consts {
            let mut x = d.locs[tab.list[id as usize].loc as usize];
            while x.inlined_at != 0 {
                x = d.locs[x.inlined_at as usize];
            }
            sub_consts.entry(x.scope).or_default().push((id, c));
        }
    }
    for v in sub_consts.values_mut() {
        v.sort_unstable();
    }
    let mut cu_ranges = Vec::new();
    for (k, f) in fns.iter().enumerate() {
        let Some(&sub) = d.fn_sub.get(&f.sym) else {
            continue;
        };
        if f.size == 0 {
            continue;
        }
        let addr = |o: u32| Address::Symbol { symbol: k, addend: o as i64 };
        cu_ranges.push(Range::StartLength { begin: addr(0), length: f.size as u64 });
        let die = cx.dw.unit.add(root, gimli::DW_TAG_subprogram);
        cx.name_attrs(die, sub);
        {
            let e = cx.dw.unit.get_mut(die);
            e.set(gimli::DW_AT_low_pc, AttributeValue::Address(addr(0)));
            e.set(gimli::DW_AT_high_pc, AttributeValue::Udata(f.size as u64));
            e.set(gimli::DW_AT_external, AttributeValue::Flag(true));
            let mut fb = Expression::new();
            fb.op_reg(gimli::Register(fp_reg));
            e.set(gimli::DW_AT_frame_base, AttributeValue::Exprloc(fb));
        }
        // Line rows, and the inline tree, from the srcloc ranges. A loc
        // whose chain doesn't end in this function's own scope came from
        // somewhere else (a location left current across bodies): skip it,
        // so the previous row covers its code.
        let s0 = &d.subs[sub as usize];
        let (f0, l0) = (s0.file, s0.line);
        let mut tree = Inst::default();
        let mut path: Vec<InstKey> = Vec::new();
        let fid0 = cx.file(f0);
        let lp = &mut cx.dw.unit.line_program;
        lp.begin_sequence(Some(addr(0)));
        lp.row().file = fid0;
        lp.row().line = l0 as u64;
        lp.generate_row();
        let mut cur = (f0, l0, 0u32);
        let mut prologue = true;
        for &(s, e, l) in &f.ranges {
            let Some(loc) = d.locs.get(l as usize).copied() else {
                continue;
            };
            path.clear();
            let mut x = loc;
            while x.inlined_at != 0 {
                path.push((x.inlined_at, x.scope));
                x = d.locs[x.inlined_at as usize];
            }
            if x.scope != sub {
                continue;
            }
            path.reverse();
            tree.add(&path, s, e);
            let key = (loc.file, loc.line, loc.col);
            if key != cur || prologue {
                let fid = cx.file(loc.file);
                let lp = &mut cx.dw.unit.line_program;
                let r = lp.row();
                r.address_offset = s as u64;
                r.file = fid;
                r.line = loc.line as u64;
                r.column = loc.col as u64;
                r.prologue_end = prologue;
                lp.generate_row();
                cur = key;
                prologue = false;
            }
        }
        cx.dw.unit.line_program.end_sequence(f.size as u64);
        cx.inlined(die, k, &mut tree.kids);
        let cs: Vec<(u32, [(u32, u32, ValLoc); 1])> = sub_consts
            .get(&sub)
            .map_or(&[][..], |v| &v[..])
            .iter()
            .map(|&(id, c)| (id, [(0, f.size, ValLoc::Const(c))]))
            .collect();
        cx.vars(die, sub, k, &tree, &f.vars, &f.labels, &cs, fp_reg);
    }
    if cu_ranges.is_empty() {
        return;
    }
    let id = cx.dw.unit.ranges.add(RangeList(cu_ranges));
    cx.dw.unit.get_mut(root).set(gimli::DW_AT_ranges, AttributeValue::RangeListRef(id));

    let mut sections = Sections::new(W { w: EndianVec::new(endian), relocs: Vec::new(), macho });
    cx.dw.write(&mut sections).expect("DWARF write");

    // Section ids first (relocations may target any of them), then data.
    let mut ids: FxHashMap<SectionId, object::write::SectionId> = FxHashMap::default();
    let fsyms: Vec<_> = fns.iter().map(|f| product.function_symbol(f.id)).collect();
    let obj = &mut product.object;
    sections
        .for_each(|id, w| -> Result<(), ()> {
            if w.w.len() > 0 {
                let name = if macho {
                    format!("__{}", &id.name()[1..])
                } else {
                    id.name().to_string()
                };
                let seg = if macho { b"__DWARF".to_vec() } else { Vec::new() };
                ids.insert(id, obj.add_section(seg, name.into_bytes(), SectionKind::Debug));
            }
            Ok(())
        })
        .unwrap();
    sections
        .for_each(|id, w| -> Result<(), ()> {
            let Some(&sec) = ids.get(&id) else {
                return Ok(());
            };
            obj.set_section_data(sec, w.w.slice().to_vec(), 1);
            for r in &w.relocs {
                let (symbol, addend) = match r.target {
                    Target::Fn(k) if macho => {
                        // Section-relative: the stored addend is the
                        // function's address in the object.
                        let s = obj.symbol(fsyms[k]);
                        let SymbolSection::Section(fsec) = s.section else {
                            continue;
                        };
                        let v = s.value as i64;
                        (obj.section_symbol(fsec), v + r.addend)
                    }
                    Target::Fn(k) => (fsyms[k], r.addend),
                    Target::Section(t) => (obj.section_symbol(ids[&t]), r.addend),
                };
                obj.add_relocation(
                    sec,
                    Relocation {
                        offset: r.offset as u64,
                        symbol,
                        addend,
                        flags: RelocationFlags::Generic {
                            kind: RelocationKind::Absolute,
                            encoding: RelocationEncoding::Generic,
                            size: r.size * 8,
                        },
                    },
                )
                .unwrap();
            }
            Ok(())
        })
        .unwrap();
}

/// Mach-O, after layout: a section-relative relocation stores the target's
/// address in the object, but `emit` could only write its offset within the
/// target section (`object` assigns section addresses while writing). Add
/// each target section's address to the debug relocations' fields.
pub fn fix_macho_text_addrs(bytes: &mut [u8]) {
    use object::read::macho::{MachHeader, Section as _, Segment as _};
    let mut patches = Vec::new();
    {
        let data = &*bytes;
        let Ok(h) = object::macho::MachHeader64::<object::Endianness>::parse(data, 0) else {
            return;
        };
        let Ok(e) = h.endian() else {
            return;
        };
        let Ok(mut cmds) = h.load_commands(e, data, 0) else {
            return;
        };
        let mut secs: Vec<&object::macho::Section64<object::Endianness>> = Vec::new();
        while let Ok(Some(c)) = cmds.next() {
            if let Ok(Some((seg, sdata))) = c.segment_64() {
                if let Ok(s) = seg.sections(e, sdata) {
                    secs.extend(s.iter());
                }
            }
        }
        for s in &secs {
            if s.segment_name() != b"__DWARF" {
                continue;
            }
            let Ok(relocs) = s.relocations(e, data) else {
                continue;
            };
            for r in relocs {
                let i = r.info(e);
                if r.r_scattered(e, h.cputype(e)) || i.r_extern || i.r_length != 3 || i.r_pcrel {
                    continue;
                }
                let Some(t) = secs.get((i.r_symbolnum as usize).wrapping_sub(1)) else {
                    continue;
                };
                let a = t.addr(e);
                if a != 0 {
                    patches.push((s.offset(e) as usize + i.r_address as usize, a));
                }
            }
        }
    }
    for (off, a) in patches {
        let f = &mut bytes[off..off + 8];
        let v = u64::from_le_bytes(f.try_into().unwrap()).wrapping_add(a);
        f.copy_from_slice(&v.to_le_bytes());
    }
}
