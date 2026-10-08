//! Writing the DWARF sections once layout has settled.

use super::line::{FileEntry, View};
use super::{Flavor, Pos, push_sleb, push_uleb};
use crate::arch::Endian;
use crate::assembler::Assembler;
use crate::expr::{BinOp, ExprKind, ExprRef};
use crate::section::{
    FixupKind, FragKind, Fragment, SectionFlags, SectionId, SectionKind, Variant,
};
use crate::source::Span;
use crate::symbol::SymbolValue;

const DW_LNS_COPY: u8 = 1;
const DW_LNS_ADVANCE_PC: u8 = 2;
const DW_LNS_ADVANCE_LINE: u8 = 3;
const DW_LNS_SET_FILE: u8 = 4;
const DW_LNS_SET_COLUMN: u8 = 5;
const DW_LNS_NEGATE_STMT: u8 = 6;
const DW_LNS_SET_BASIC_BLOCK: u8 = 7;
const DW_LNS_CONST_ADD_PC: u8 = 8;
const DW_LNS_FIXED_ADVANCE_PC: u8 = 9;
const DW_LNS_SET_PROLOGUE_END: u8 = 10;
const DW_LNS_SET_EPILOGUE_BEGIN: u8 = 11;
const DW_LNS_SET_ISA: u8 = 12;
const DW_LNE_END_SEQUENCE: u8 = 1;
const DW_LNE_SET_ADDRESS: u8 = 2;
const DW_LNE_SET_DISCRIMINATOR: u8 = 4;

const DW_LNCT_PATH: u64 = 1;
const DW_LNCT_DIRECTORY_INDEX: u64 = 2;
const DW_LNCT_MD5: u64 = 5;
const DW_FORM_DATA16: u64 = 0x1e;
const DW_FORM_LINE_STRP: u64 = 0x1f;
const DW_FORM_UDATA: u64 = 0x0f;

/// Both references use GNU as's original special-opcode parameters.
const LINE_BASE: i64 = -5;
const LINE_RANGE: i64 = 14;

/// The first special opcode: 13, after the twelve standard opcodes, except
/// in GNU as's version 2 tables, which stop at `DW_LNS_fixed_advance_pc`.
fn opcode_base(flavor: Flavor, version: u16) -> i64 {
    if flavor == Flavor::Gnu && version == 2 {
        10
    } else {
        13
    }
}

/// Bytes being built for a generated section, with the fixups for the fields
/// only a relocation can fill.
pub(crate) struct Blob {
    pub bytes: Vec<u8>,
    pub fixups: Vec<(u32, u8, ExprRef, FixupKind)>,
    pub endian: Endian,
}

impl Blob {
    pub fn new(endian: Endian) -> Blob {
        Blob {
            bytes: Vec::new(),
            fixups: Vec::new(),
            endian,
        }
    }

    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    pub fn u8(&mut self, v: u8) {
        self.bytes.push(v);
    }

    pub fn int(&mut self, v: u64, size: usize) {
        let b = self.endian.bytes(v, size);
        self.bytes.extend_from_slice(&b);
    }

    /// Overwrites a field written earlier, such as a length.
    pub fn patch(&mut self, at: usize, v: u64, size: usize) {
        self.endian.write(&mut self.bytes[at..at + size], v);
    }

    pub fn uleb(&mut self, v: u64) {
        push_uleb(&mut self.bytes, v);
    }

    pub fn sleb(&mut self, v: i64) {
        push_sleb(&mut self.bytes, v);
    }

    pub fn str(&mut self, s: &str) {
        self.bytes.extend_from_slice(s.as_bytes());
        self.bytes.push(0);
    }

    /// A field of `size` bytes that a fixup fills.
    pub fn fixup(&mut self, size: u8, expr: ExprRef, kind: FixupKind) {
        self.fixups
            .push((self.bytes.len() as u32, size, expr, kind));
        self.bytes.resize(self.bytes.len() + size as usize, 0);
    }

    /// Pads with `fill` to a multiple of `align`, counting from `base`.
    pub fn align(&mut self, base: u64, align: u64, fill: u8) {
        while !(base + self.len()).is_multiple_of(align) {
            self.bytes.push(fill);
        }
    }
}

/// What every sequence of a line program is written with.
struct ProgramCx {
    target: super::DwarfTarget,
    version: u16,
    ptr: u8,
    opcode_base: i64,
    /// An address advance that is not a whole number of instructions has
    /// been reported.
    unaligned: bool,
}

impl Assembler {
    /// Writes `.debug_line`, `.eh_frame` and `.debug_frame` from what the
    /// source asked for, now that layout has settled. Returns whether it added
    /// anything, which means layout has to run again.
    pub(crate) fn emit_dwarf(&mut self) -> bool {
        // A flat image has no room for sections nobody loads, and the only
        // consumer of the others is a linker it will never see.
        if !self.options.relocatable {
            return false;
        }
        let mut added = false;
        if self.dwarf.line.has_views {
            self.assign_views();
        }
        let version = self.dwarf_line_version();
        self.number_generated_files(version);
        match self.dwarf_target().flavor {
            // GNU as's `dwarf2_finish`: no table without a row, unless the
            // source has a unit of its own for one; and a unit to go with
            // the table unless the source has one.
            Flavor::Gnu => {
                let rows = !self.dwarf.line.sequences.is_empty();
                let has_info = self.section_has_bytes(".debug_info");
                let has_line = self.section_has_bytes(".debug_line");
                if rows && has_line && self.dwarf.line.used {
                    let span = self.dwarf.line.current.span;
                    self.diags.error(span, "duplicate .debug_line sections");
                } else if (rows || has_info) && !(has_info && has_line) {
                    if !has_line {
                        self.emit_debug_line();
                    }
                    if !has_info {
                        self.gnu_unit(version);
                    }
                    added = true;
                }
            }
            // llvm-mc writes a table for a numbered `.file` alone.
            Flavor::Llvm => {
                if self.dwarf.line.is_used() {
                    self.emit_debug_line();
                    if self.dwarf.line.source.on {
                        self.llvm_unit(version);
                    }
                    added = true;
                }
            }
        }
        if !self.dwarf.cfi.fdes.is_empty() {
            self.emit_frames();
            added = true;
        }
        added
    }

    /// Finds or creates a section for generated DWARF, whose bytes are the
    /// object's target's, whatever `.arch` was last active.
    pub(crate) fn dwarf_section(
        &mut self,
        name: &str,
        flags: SectionFlags,
        entsize: u64,
        align: u64,
    ) -> SectionId {
        let n = self.interner.intern(name);
        let id = self.get_or_create_section(n, SectionKind::Progbits, flags, 1);
        let s = self.section_mut(id);
        s.align = s.align.max(align);
        if entsize != 0 {
            s.entsize = entsize;
        }
        s.mark_arch(0);
        id
    }

    /// Appends `blob` to a section as one fragment, returning its position.
    pub(crate) fn push_blob(&mut self, section: SectionId, blob: Blob, span: Span) -> Pos {
        let fixups = blob
            .fixups
            .into_iter()
            .map(|(offset, _, expr, kind)| crate::section::Fixup {
                offset,
                expr,
                kind,
                span,
            })
            .collect();
        let s = self.section_mut(section);
        s.seal();
        let idx = s.push(Fragment::new(
            FragKind::Bytes {
                variants: vec![Variant {
                    bytes: blob.bytes,
                    fixups,
                }],
                chosen: 0,
            },
            span,
        ));
        (section, idx)
    }

    /// The position the next fragment of `section` will have.
    pub(crate) fn next_pos(&mut self, section: SectionId) -> Pos {
        let s = self.section_mut(section);
        s.seal();
        (section, s.next_frag_index())
    }

    /// An expression for a label at `pos` plus `offset`.
    pub(crate) fn pos_expr(&mut self, pos: Pos, offset: u64) -> ExprRef {
        let label = self.dwarf_label(pos, Span::DUMMY);
        let l = self.exprs.alloc(ExprKind::SymId(label), Span::DUMMY);
        if offset == 0 {
            return l;
        }
        let o = self.exprs.int(offset, Span::DUMMY);
        self.exprs
            .alloc(ExprKind::Binary(BinOp::Add, l, o), Span::DUMMY)
    }

    /// An absolute data field of `size` bytes in the target's relocation.
    pub(crate) fn abs_kind(&self, size: u8) -> FixupKind {
        let reloc = self.target().data_reloc(size, false).unwrap_or(0);
        FixupKind::data(size).with_reloc(reloc)
    }

    /// A field of `size` bytes holding its distance to the target.
    pub(crate) fn pcrel_kind(&self, size: u8) -> FixupKind {
        let reloc = self.target().data_reloc(size, true).unwrap_or(0);
        FixupKind::pcrel(size, 0).with_reloc(reloc)
    }

    /// Numbers GNU as's views: a row at the same address as the one before
    /// it in its section counts one up from it, and any other row, or one
    /// written `view -0`, starts again at 0. A `view` symbol is defined as its
    /// row's number, and `view 0` has to be 0.
    fn assign_views(&mut self) {
        let mut defs = Vec::new();
        let mut mismatches = Vec::new();
        for (_, rows) in &self.dwarf.line.sequences {
            let mut prev: Option<(u64, u64)> = None;
            for row in rows {
                let addr = self.row_addr(row);
                let view = match prev {
                    Some((paddr, pview)) if addr <= paddr && row.loc.view != Some(View::Reset) => {
                        pview + 1
                    }
                    _ => 0,
                };
                match row.loc.view {
                    Some(View::Sym(id)) => defs.push((id, view)),
                    Some(View::Zero) if view != 0 => mismatches.push(row.loc.span),
                    _ => {}
                }
                prev = Some((addr, view));
            }
        }
        for (id, view) in defs {
            let span = self.symbols.get(id).def_span;
            let e = self.exprs.int(view, span);
            self.symbols.get_mut(id).value = SymbolValue::Expr(e);
        }
        for span in mismatches {
            self.diags.error(span, "view number mismatch");
        }
    }

    // ---- .debug_line -------------------------------------------------------

    fn emit_debug_line(&mut self) {
        let target = self.dwarf_target();
        let flavor = target.flavor;
        let version = self.dwarf_line_version();
        let endian = self.target().endian();
        let ptr = self.target().pointer_bytes(&self.target().initial_state());
        self.check_file_table(flavor, version);

        let line_sec = self.dwarf_section(".debug_line", SectionFlags::default(), 0, 1);
        let str_sec = (version >= 5).then(|| {
            let flags = SectionFlags {
                merge: true,
                strings: true,
                ..SectionFlags::default()
            };
            self.dwarf_section(".debug_line_str", flags, 1, 1)
        });
        let mut strs = Blob::new(endian);
        // Offsets of the strings `strs` holds, for llvm-mc, which writes each
        // one once.
        let mut seen: Vec<(String, u64)> = Vec::new();
        let str_pos = str_sec.map(|s| self.next_pos(s));

        let mut b = Blob::new(endian);
        b.int(0, 4); // unit_length, patched below
        b.int(version as u64, 2);
        if version >= 5 {
            b.u8(ptr);
            b.u8(0);
        }
        let header_len_at = b.bytes.len();
        b.int(0, 4);
        let header_start = b.bytes.len();
        b.u8(target.min_insn_length);
        if version >= 4 {
            b.u8(1);
        }
        b.u8(1); // default_is_stmt
        b.u8(LINE_BASE as i8 as u8);
        b.u8(LINE_RANGE as u8);
        let opcode_base = opcode_base(flavor, version);
        b.u8(opcode_base as u8);
        let lengths = [0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1];
        b.bytes
            .extend_from_slice(&lengths[..opcode_base as usize - 1]);

        // A path in `.debug_line_str`, or inline before DWARF 5. Returns
        // the offset of the string.
        let mut path = |asm: &mut Assembler, b: &mut Blob, s: &str| -> u64 {
            let (Some(pos), Some(_)) = (str_pos, str_sec) else {
                b.str(s);
                return 0;
            };
            let off = match seen.iter().find(|(t, _)| t == s) {
                Some((_, o)) if flavor == Flavor::Llvm => *o,
                _ => {
                    let o = strs.len();
                    strs.str(s);
                    seen.push((s.to_string(), o));
                    o
                }
            };
            let e = asm.pos_expr(pos, off);
            let kind = asm.abs_kind(4);
            b.fixup(4, e, kind);
            off
        };

        match (flavor, version >= 5) {
            (Flavor::Gnu, true) => {
                let pwd = super::line::current_dir();
                let t = std::mem::take(&mut self.dwarf.line.gnu);
                let dirs: Vec<String> = if t.dirs.is_empty() {
                    vec![pwd.clone()]
                } else {
                    t.dirs
                        .iter()
                        .enumerate()
                        .map(|(i, d)| match d {
                            Some(d) => d.clone(),
                            None if i == 0 => pwd.clone(),
                            None => String::new(),
                        })
                        .collect()
                };
                b.u8(1);
                b.uleb(DW_LNCT_PATH);
                b.uleb(DW_FORM_LINE_STRP);
                b.uleb(dirs.len() as u64);
                for d in &dirs {
                    path(self, &mut b, d);
                }
                let mut files = t.files.clone();
                if files.is_empty() {
                    files.push(None);
                }
                // File 0 given no `.file 0` is file 1, and the two share
                // the one string: GNU as shares a string only where the
                // two slots hold the same pointer, which only this makes.
                let shared = files[0].is_none() && matches!(files.get(1), Some(Some(_)));
                if files[0].is_none() {
                    files[0] = Some(match files.get(1).cloned().flatten() {
                        Some(f) => f,
                        None => FileEntry {
                            name: String::new(),
                            dir: 0,
                            md5: None,
                        },
                    });
                }
                let md5 = files.iter().flatten().any(|f| f.md5.is_some());
                b.u8(if md5 { 3 } else { 2 });
                b.uleb(DW_LNCT_PATH);
                b.uleb(DW_FORM_LINE_STRP);
                b.uleb(DW_LNCT_DIRECTORY_INDEX);
                b.uleb(DW_FORM_UDATA);
                if md5 {
                    b.uleb(DW_LNCT_MD5);
                    b.uleb(DW_FORM_DATA16);
                }
                b.uleb(files.len() as u64);
                let mut first = 0;
                for (i, f) in files.iter().flatten().enumerate() {
                    if shared && i == 1 {
                        let e = self.pos_expr(str_pos.expect("DWARF 5 has line strings"), first);
                        let kind = self.abs_kind(4);
                        b.fixup(4, e, kind);
                    } else {
                        first = path(self, &mut b, &f.name);
                    }
                    b.uleb(f.dir as u64);
                    if md5 {
                        // GNU as writes the number in the target's byte
                        // order, which on a little-endian target reverses
                        // the checksum as written.
                        let v = f.md5.unwrap_or(0);
                        b.bytes.extend_from_slice(&match endian {
                            Endian::Little => v.to_le_bytes(),
                            Endian::Big => v.to_be_bytes(),
                        });
                    }
                }
                self.dwarf.line.gnu = t;
            }
            (Flavor::Gnu, false) => {
                let t = &self.dwarf.line.gnu;
                for d in t.dirs.iter().skip(1) {
                    b.str(d.as_deref().unwrap_or(""));
                }
                b.u8(0);
                for f in t.files.iter().skip(1).flatten() {
                    b.str(&f.name);
                    b.uleb(f.dir as u64);
                    b.uleb(0);
                    b.uleb(0);
                }
                b.u8(0);
            }
            (Flavor::Llvm, true) => {
                let t = std::mem::take(&mut self.dwarf.line.llvm);
                let comp = t.comp_dir.clone().unwrap_or_else(super::line::current_dir);
                b.u8(1);
                b.uleb(DW_LNCT_PATH);
                b.uleb(DW_FORM_LINE_STRP);
                b.uleb(t.dirs.len() as u64 + 1);
                path(self, &mut b, &comp);
                for d in &t.dirs {
                    path(self, &mut b, d);
                }
                let md5 = t.all_md5;
                b.u8(if md5 { 3 } else { 2 });
                b.uleb(DW_LNCT_PATH);
                b.uleb(DW_FORM_LINE_STRP);
                b.uleb(DW_LNCT_DIRECTORY_INDEX);
                b.uleb(DW_FORM_UDATA);
                if md5 {
                    b.uleb(DW_LNCT_MD5);
                    b.uleb(DW_FORM_DATA16);
                }
                b.uleb(if t.files.is_empty() {
                    1
                } else {
                    t.files.len() as u64
                });
                let root = t.root.clone().or_else(|| t.files.get(1).cloned().flatten());
                let entries = root.iter().chain(t.files.iter().skip(1).flatten());
                for f in entries {
                    path(self, &mut b, &f.name);
                    b.uleb(f.dir as u64);
                    if md5 {
                        let v = f.md5.unwrap_or(0);
                        b.bytes.extend_from_slice(&v.to_be_bytes());
                    }
                }
                self.dwarf.line.llvm = t;
            }
            (Flavor::Llvm, false) => {
                let t = &self.dwarf.line.llvm;
                for d in &t.dirs {
                    b.str(d);
                }
                b.u8(0);
                for f in t.files.iter().skip(1).flatten() {
                    b.str(&f.name);
                    b.uleb(f.dir as u64);
                    b.u8(0);
                    b.u8(0);
                }
                b.u8(0);
            }
        }
        let header_len = b.bytes.len() - header_start;
        b.patch(header_len_at, header_len as u64, 4);

        let line_pos = self.next_pos(line_sec);
        let sequences = std::mem::take(&mut self.dwarf.line.sequences);
        let mut cx = ProgramCx {
            target,
            version,
            ptr,
            opcode_base,
            unaligned: false,
        };
        // llvm-mc ends a fragment with the address advance that ends each
        // sequence, which matters where the offset of the next sequence's
        // address in its fragment picks the relocation (SPARC's unaligned
        // ones); so the table is cut into fragments at the same places.
        let mut pieces = Vec::new();
        for (section, rows) in &sequences {
            self.line_program(&mut b, &mut cx, *section, rows);
            if flavor == Flavor::Llvm {
                pieces.push(std::mem::replace(&mut b, Blob::new(endian)));
            }
        }
        pieces.push(b);
        self.dwarf.line.sequences = sequences;
        if flavor == Flavor::Llvm {
            self.relocs_by_fragment.push(line_sec);
        }

        let total = pieces.iter().map(|p| p.bytes.len() as u64).sum::<u64>() - 4;
        pieces[0].patch(0, total, 4);
        for (i, piece) in pieces.into_iter().enumerate() {
            if i > 0 && piece.bytes.is_empty() {
                continue;
            }
            let pushed = self.push_blob(line_sec, piece, Span::DUMMY);
            debug_assert!(i > 0 || pushed == line_pos);
        }
        if let (Some(s), Some(pos)) = (str_sec, str_pos)
            && !strs.bytes.is_empty()
        {
            let pushed = self.push_blob(s, strs, Span::DUMMY);
            debug_assert_eq!(pushed, pos);
        }
    }

    /// Reports gaps in the file table that the references refuse.
    fn check_file_table(&mut self, flavor: Flavor, version: u16) {
        let span = self
            .dwarf
            .line
            .file_spans
            .last()
            .map_or(Span::DUMMY, |s| s.1);
        let missing: Vec<usize> = match flavor {
            Flavor::Gnu => {
                let files = &self.dwarf.line.gnu.files;
                (1..files.len()).filter(|&i| files[i].is_none()).collect()
            }
            Flavor::Llvm => {
                let files = &self.dwarf.line.llvm.files;
                (1..files.len()).filter(|&i| files[i].is_none()).collect()
            }
        };
        for i in missing {
            self.diags.error(
                span,
                format!("unassigned file number {i} for .file directives"),
            );
        }
        let _ = version;
    }

    /// The rows of one section, as a sequence.
    fn line_program(
        &mut self,
        b: &mut Blob,
        cx: &mut ProgramCx,
        section: SectionId,
        rows: &[super::line::Row],
    ) {
        let (target, version, ptr) = (&cx.target, cx.version, cx.ptr);
        let flavor = target.flavor;
        let fixed = target.fixed_advance_pc;
        let min = target.min_insn_length.max(1) as u64;
        let (mut file, mut line, mut column, mut isa) = (1u32, 1i64, 0u32, 0u32);
        let mut is_stmt = true;
        let mut last: Option<u64> = None;
        let mut last_at: Option<(Pos, u64)> = None;
        for row in rows {
            let loc = &row.loc;
            if file != loc.file {
                file = loc.file;
                b.u8(DW_LNS_SET_FILE);
                b.uleb(file as u64);
            }
            if column != loc.column {
                column = loc.column;
                b.u8(DW_LNS_SET_COLUMN);
                b.uleb(column as u64);
            }
            // GNU as writes a discriminator whatever the version; llvm-mc
            // only from version 4.
            if loc.discriminator != 0 && (flavor == Flavor::Gnu || version >= 4) {
                let mut d = Vec::new();
                push_uleb(&mut d, loc.discriminator as u64);
                b.u8(0);
                b.uleb(d.len() as u64 + 1);
                b.u8(DW_LNE_SET_DISCRIMINATOR);
                b.bytes.extend_from_slice(&d);
            }
            if isa != loc.isa {
                isa = loc.isa;
                b.u8(DW_LNS_SET_ISA);
                b.uleb(isa as u64);
            }
            if is_stmt != loc.is_stmt {
                is_stmt = loc.is_stmt;
                b.u8(DW_LNS_NEGATE_STMT);
            }
            if loc.basic_block {
                b.u8(DW_LNS_SET_BASIC_BLOCK);
            }
            if loc.prologue_end {
                b.u8(DW_LNS_SET_PROLOGUE_END);
            }
            if loc.epilogue_begin {
                b.u8(DW_LNS_SET_EPILOGUE_BEGIN);
            }
            let line_delta = loc.line as i64 - line;
            let addr = self.row_addr(row);
            let at = (row.pos, addr - self.pos_offset(row.pos));
            match last {
                // A `view -0` row at the address of the one before it gets an
                // address of its own, so that consumers restart the count.
                Some(prev) if !(loc.view == Some(View::Reset) && prev == addr) => {
                    let delta = addr.saturating_sub(prev);
                    if fixed {
                        let from = last_at.unwrap_or(at);
                        self.fixed_advance(b, Some(line_delta), delta, from, at, ptr);
                    } else {
                        // GNU as says so once, however many rows are off.
                        if delta % min != 0 && flavor == Flavor::Gnu && !cx.unaligned {
                            cx.unaligned = true;
                            self.diags.error(
                                loc.span,
                                "unaligned opcodes detected in executable segment",
                            );
                        }
                        special_advance(b, Some(line_delta), delta / min, cx.opcode_base);
                    }
                }
                _ => {
                    self.set_address(b, at, ptr);
                    special_advance(b, Some(line_delta), 0, cx.opcode_base);
                }
            }
            line = loc.line as i64;
            last = Some(addr);
            last_at = Some(at);
        }
        let end = self.sequence_end(section);
        let prev = last.unwrap_or(end);
        let delta = end.saturating_sub(prev);
        if fixed {
            let end_at = ((section, self.section(section).frags.len() as u32), 0);
            let from = last_at.unwrap_or(end_at);
            self.fixed_advance(b, None, delta, from, end_at, ptr);
        } else {
            special_advance(b, None, delta / min, cx.opcode_base);
        }
    }

    /// Where a section's sequence ends: its end, before any padding GNU as
    /// adds to round the section up only after the line table is written.
    fn sequence_end(&self, section: SectionId) -> u64 {
        let s = self.section(section);
        match s.frags.last() {
            Some(f) if self.tail_pads.contains(&section) => f.offset,
            _ => s.size,
        }
    }

    fn set_address(&mut self, b: &mut Blob, at: (Pos, u64), ptr: u8) {
        b.u8(0);
        b.uleb(ptr as u64 + 1);
        b.u8(DW_LNE_SET_ADDRESS);
        let e = self.pos_expr(at.0, at.1);
        let kind = self.abs_kind(ptr);
        b.fixup(ptr, e, kind);
    }

    /// GNU as's `emit_fixed_inc_line_addr`, for targets whose linker may move
    /// code: an explicit 16-bit advance, or a new address past 50000 bytes.
    /// `line_delta` of `None` ends the sequence.
    fn fixed_advance(
        &mut self,
        b: &mut Blob,
        line_delta: Option<i64>,
        delta: u64,
        from: (Pos, u64),
        at: (Pos, u64),
        ptr: u8,
    ) {
        // GNU as knows how far the last row is from the end of its frag, so
        // the advance that ends a sequence is always a number.
        let Some(line_delta) = line_delta else {
            b.u8(DW_LNS_FIXED_ADVANCE_PC);
            b.int(delta, 2);
            b.u8(0);
            b.u8(1);
            b.u8(DW_LNE_END_SEQUENCE);
            return;
        };
        // Even an advance of 0: GNU as sizes the row before it knows the
        // delta, and always writes one.
        b.u8(DW_LNS_ADVANCE_LINE);
        b.sleb(line_delta);
        if delta > 50000 {
            self.set_address(b, at, ptr);
        } else {
            b.u8(DW_LNS_FIXED_ADVANCE_PC);
            self.advance_field(b, from, at);
        }
        b.u8(DW_LNS_COPY);
    }

    /// The operand of a `DW_LNS_fixed_advance_pc`: the distance between two
    /// rows, written as the difference of labels at them. Layout folds that
    /// to the number, unless the target's linker is to work it out, as GNU as
    /// for MSP430 has it do (see `Architecture::defers_difference`).
    fn advance_field(&mut self, b: &mut Blob, from: (Pos, u64), to: (Pos, u64)) {
        let t = self.pos_expr(to.0, to.1);
        let f = self.pos_expr(from.0, from.1);
        let e = self
            .exprs
            .alloc(ExprKind::Binary(BinOp::Sub, t, f), Span::DUMMY);
        let kind = self.abs_kind(2);
        b.fixup(2, e, kind);
    }
}

/// Advances the line by `line_delta` and the address by `addr_delta`
/// instructions, and makes a row; or, with no line delta, ends the sequence
/// there. This is GNU as's `emit_inc_line_addr`, which llvm-mc's
/// `MCDwarfLineAddr::encode` copies exactly.
fn special_advance(b: &mut Blob, line_delta: Option<i64>, addr_delta: u64, opcode_base: i64) {
    let max_special = ((255 - opcode_base) / LINE_RANGE) as u64;
    let Some(mut line_delta) = line_delta else {
        if addr_delta == max_special {
            b.u8(DW_LNS_CONST_ADD_PC);
        } else if addr_delta != 0 {
            b.u8(DW_LNS_ADVANCE_PC);
            b.uleb(addr_delta);
        }
        b.u8(0);
        b.u8(1);
        b.u8(DW_LNE_END_SEQUENCE);
        return;
    };
    let mut tmp = line_delta - LINE_BASE;
    let mut need_copy = false;
    if !(0..LINE_RANGE).contains(&tmp) {
        b.u8(DW_LNS_ADVANCE_LINE);
        b.sleb(line_delta);
        line_delta = 0;
        tmp = -LINE_BASE;
        need_copy = true;
    }
    if line_delta == 0 && addr_delta == 0 {
        b.u8(DW_LNS_COPY);
        return;
    }
    tmp += opcode_base;
    if addr_delta < 256 + max_special {
        let opcode = tmp + addr_delta as i64 * LINE_RANGE;
        if opcode <= 255 {
            b.u8(opcode as u8);
            return;
        }
        let opcode = tmp + (addr_delta as i64 - max_special as i64) * LINE_RANGE;
        if opcode <= 255 {
            b.u8(DW_LNS_CONST_ADD_PC);
            b.u8(opcode as u8);
            return;
        }
    }
    b.u8(DW_LNS_ADVANCE_PC);
    b.uleb(addr_delta);
    b.u8(if need_copy { DW_LNS_COPY } else { tmp as u8 });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advance(line: Option<i64>, addr: u64) -> Vec<u8> {
        let mut b = Blob::new(Endian::Little);
        special_advance(&mut b, line, addr, 13);
        b.bytes
    }

    #[test]
    fn advances_encode_as_the_references_write_them() {
        // Every vector is a piece of a line program from GNU as or llvm-mc.
        // Line +1 at address +1, +2, +4 and +8: special opcodes.
        assert_eq!(advance(Some(1), 1), vec![0x21]);
        assert_eq!(advance(Some(1), 2), vec![0x2f]);
        assert_eq!(advance(Some(1), 4), vec![0x4b]);
        assert_eq!(advance(Some(1), 8), vec![0x83]);
        // The first row of a sequence, at its `DW_LNE_set_address`.
        assert_eq!(advance(Some(0), 0), vec![DW_LNS_COPY]);
        // Out of range for a special opcode: line -398 at address +301, and
        // line +1998 at +1.
        assert_eq!(
            advance(Some(-398), 301),
            vec![
                DW_LNS_ADVANCE_LINE,
                0xf2,
                0x7c,
                DW_LNS_ADVANCE_PC,
                0xad,
                0x02,
                DW_LNS_COPY
            ]
        );
        assert_eq!(
            advance(Some(1998), 1),
            vec![DW_LNS_ADVANCE_LINE, 0xce, 0x0f, 0x20]
        );
        // The end of a sequence, one and four bytes on.
        assert_eq!(advance(None, 1), vec![DW_LNS_ADVANCE_PC, 1, 0, 1, 1]);
        assert_eq!(advance(None, 4), vec![DW_LNS_ADVANCE_PC, 4, 0, 1, 1]);
    }
}
