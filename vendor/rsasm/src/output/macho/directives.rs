//! The directives Darwin's assembler has and GNU as's ELF ports do not, and
//! the ones whose meaning changes in a Mach-O object.
//!
//! They are only looked up when the output is Mach-O: `.const` or `.zerofill`
//! mean nothing to an ELF object, and `.align 3` means eight bytes here but
//! three on x86 ELF.

use super::{BuildVersion, N_ALT_ENTRY, N_NO_DEAD_STRIP, N_WEAK_DEF, N_WEAK_REF, SectionInfo};
use crate::assembler::Assembler;
use crate::cursor::Cursor;
use crate::lexer::{Punct, TokKind};
use crate::parser::LabelDef;
use crate::section::{FragKind, Fragment, SectionFlags, SectionId, SectionKind};
use crate::source::Span;
use crate::symbol::{Binding, SymType, SymbolValue, Visibility};

/// Mach-O's fixed width for a segment or section name.
const NAME_MAX: usize = 16;

impl Assembler {
    /// The section `segment,section` names, created with `info` (or the
    /// type and attributes its name implies) if it is new.
    pub(crate) fn macho_section(
        &mut self,
        segment: &str,
        section: &str,
        info: Option<(u32, u32, u32)>,
    ) -> SectionId {
        let name = self.interner.intern(&format!("{segment},{section}"));
        if let Some(id) = self.sections.iter().find(|s| s.name == name).map(|s| s.id) {
            return id;
        }
        // A section llvm-mc knows from the start keeps what it knows of it.
        let (ty, attrs, reserved2) = match super::precreated(segment, section) {
            Some((ty, attrs)) => (ty, attrs, 0),
            None => info.unwrap_or((super::S_REGULAR, 0, 0)),
        };
        let zerofill = matches!(
            ty,
            super::S_ZEROFILL | super::S_GB_ZEROFILL | super::S_THREAD_LOCAL_ZEROFILL
        );
        // What the rest of the assembler needs to know of a section: whether
        // it takes bytes, and whether alignment in it pads with no-ops.
        let flags = SectionFlags {
            alloc: true,
            write: segment == "__DATA",
            exec: attrs & super::S_ATTR_PURE_INSTRUCTIONS != 0,
            ..SectionFlags::default()
        };
        let kind = if zerofill {
            SectionKind::Nobits
        } else {
            SectionKind::Progbits
        };
        let id = self.get_or_create_section(name, kind, flags, 1);
        self.macho
            .section_marks
            .insert(id, self.symbols.len() as u32);
        self.macho.sections.insert(
            id,
            SectionInfo {
                ty,
                attrs,
                reserved2,
            },
        );
        id
    }

    /// The value of `e`, where it is a difference of two labels that are a
    /// fixed distance apart where it is read, in a Mach-O object.
    ///
    /// llvm-mc folds such a difference as it reads the data directive, before
    /// it has cut the sections into atoms, and only later decides that a
    /// difference it could not fold then spans two atoms and needs a pair of
    /// relocations. So `.long _b - _a` is a number after both labels, with
    /// only fixed-size code between them, and a relocation pair before them.
    pub(crate) fn macho_fixed_difference(&self, e: crate::expr::ExprRef) -> Option<i64> {
        if self.options.format != crate::output::Format::MachO || !self.options.relocatable {
            return None;
        }
        let v = self.eval_ref(e).ok()?;
        let (Some(plus), Some(minus)) = (v.plus, v.minus) else {
            return None;
        };
        let position = |id| match self.symbols.get(id).value {
            SymbolValue::Label { section, frag } => Some((section, frag)),
            _ => None,
        };
        let d = crate::arch::fixed_distance(
            &self.sections,
            &self.exprs,
            &self.symbols,
            position(minus)?,
            position(plus)?,
        )?;
        Some(d + v.addend)
    }

    /// Runs a directive that means something different, or only something, in
    /// a Mach-O object. Returns whether it was one.
    pub(crate) fn macho_directive(&mut self, text: &str, cur: &mut Cursor<'_>, span: Span) -> bool {
        match text {
            ".section" => self.macho_dir_section(cur, span),
            ".zerofill" => self.macho_dir_zerofill(cur, span),
            ".lcomm" => self.macho_dir_lcomm(cur, span),
            ".comm" => self.macho_dir_comm(cur, span),
            // Darwin counts `.align` in bits on every machine.
            ".align" => self.dir_align(cur, span, true),
            ".rodata" => {
                self.diags.error(
                    span,
                    "Mach-O has no `.rodata`; read-only data goes in `.const` \
                     (`__TEXT,__const`)",
                );
                true
            }
            _ if super::shorthand(text).is_some() => {
                let id = self.standard_section(text);
                self.set_section(id);
                true
            }
            ".private_extern" => self.macho_symbols(cur, |asm, id| {
                let sym = asm.symbols.get_mut(id);
                sym.binding = Binding::Global;
                sym.visibility = Visibility::Hidden;
            }),
            ".weak_definition" => self.macho_symbols(cur, |asm, id| {
                *asm.macho.symbol_desc.entry(id).or_default() |= N_WEAK_DEF;
            }),
            ".weak_reference" => self.macho_symbols(cur, |asm, id| {
                *asm.macho.symbol_desc.entry(id).or_default() |= N_WEAK_REF;
                asm.symbols.get_mut(id).used = true;
            }),
            ".alt_entry" => self.macho_symbols(cur, |asm, id| {
                *asm.macho.symbol_desc.entry(id).or_default() |= N_ALT_ENTRY;
            }),
            ".no_dead_strip" => self.macho_symbols(cur, |asm, id| {
                *asm.macho.symbol_desc.entry(id).or_default() |= N_NO_DEAD_STRIP;
            }),
            ".subsections_via_symbols" => {
                self.macho.subsections_via_symbols = true;
                true
            }
            ".build_version" => self.macho_dir_build_version(cur, span),
            ".data_region" => self.macho_dir_data_region(cur, span),
            ".end_data_region" => {
                match self.macho.data_regions.last_mut() {
                    Some(region) if region.end.is_none() => {
                        let end = self.anon_label(span);
                        if let Some(region) = self.macho.data_regions.last_mut() {
                            region.end = Some(end);
                        }
                    }
                    _ => self
                        .diags
                        .error(span, "`.end_data_region` without a `.data_region`"),
                }
                true
            }
            ".set" | ".equ" | ".equiv" => {
                if let Some(n) = cur.peek().ident() {
                    let id = self.symbols.intern(n, span);
                    self.macho.set_constants.insert(id);
                }
                self.dir_set(cur, span, text == ".equiv")
            }
            _ => false,
        }
    }

    /// Applies `f` to each symbol of a comma-separated list.
    fn macho_symbols(
        &mut self,
        cur: &mut Cursor<'_>,
        f: impl Fn(&mut Assembler, crate::symbol::SymbolId),
    ) -> bool {
        loop {
            let Some((name, span)) = self.expect_name(cur) else {
                return true;
            };
            let id = self.symbols.intern(name, span);
            f(self, id);
            if cur.eat_punct(Punct::Comma).is_none() {
                return true;
            }
        }
    }

    /// A segment or section name: an identifier, at most sixteen bytes.
    fn macho_name(&mut self, cur: &mut Cursor<'_>, what: &str) -> Option<String> {
        let tok = cur.peek();
        let Some(n) = tok.ident() else {
            self.diags
                .error(tok.span, format!("expected a {what} name"));
            return None;
        };
        cur.advance();
        let name = self.interner.get(n).to_string();
        if name.len() > NAME_MAX {
            self.diags.error(
                tok.span,
                format!(
                    "a Mach-O {what} name is at most {NAME_MAX} characters, and `{name}` is longer"
                ),
            );
            return None;
        }
        Some(name)
    }

    /// `.section SEGMENT,SECTION[,TYPE[,ATTRIBUTE+...[,STUB_SIZE]]]`.
    fn macho_dir_section(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some((segment, section)) = self.macho_pair(cur) else {
            return true;
        };
        let mut info = None;
        if cur.eat_punct(Punct::Comma).is_some() {
            let Some(tname) = self.macho_word(cur, "section type") else {
                return true;
            };
            let Some(ty) = super::section_type(&tname) else {
                self.diags
                    .error(span, format!("unknown Mach-O section type `{tname}`"));
                return true;
            };
            let mut attrs = 0;
            let mut reserved2 = 0;
            if cur.eat_punct(Punct::Comma).is_some() {
                loop {
                    let Some(a) = self.macho_word(cur, "section attribute") else {
                        return true;
                    };
                    let Some(bit) = super::section_attribute(&a) else {
                        self.diags
                            .error(span, format!("unknown Mach-O section attribute `{a}`"));
                        return true;
                    };
                    attrs |= bit;
                    if cur.eat_punct(Punct::Plus).is_none() {
                        break;
                    }
                }
                if cur.eat_punct(Punct::Comma).is_some() {
                    let Some(e) = self.parse_expr(cur) else {
                        return true;
                    };
                    reserved2 = self.eval_absolute(e, "stub size").unwrap_or(0) as u32;
                }
            }
            if ty == super::S_SYMBOL_STUBS && reserved2 == 0 {
                self.diags
                    .error(span, "a `symbol_stubs` section needs a stub size");
                return true;
            }
            info = Some((ty, attrs, reserved2));
        }
        let id = self.macho_section(&segment, &section, info);
        self.set_section(id);
        true
    }

    /// `SEGMENT,SECTION`.
    fn macho_pair(&mut self, cur: &mut Cursor<'_>) -> Option<(String, String)> {
        let segment = self.macho_name(cur, "segment")?;
        if cur.eat_punct(Punct::Comma).is_none() {
            let tok = cur.peek();
            self.diags.error(
                tok.span,
                "a Mach-O section is named `SEGMENT,SECTION`, as in `__TEXT,__text`",
            );
            return None;
        }
        let section = self.macho_name(cur, "section")?;
        Some((segment, section))
    }

    fn macho_word(&mut self, cur: &mut Cursor<'_>, what: &str) -> Option<String> {
        let tok = cur.peek();
        match tok.kind {
            TokKind::Ident(n) => {
                cur.advance();
                Some(self.interner.get(n).to_string())
            }
            // A number-led word such as `4byte_literals` does not lex as an
            // identifier; its text is still the word.
            TokKind::BadNumber(n) => {
                cur.advance();
                Some(self.interner.get(n).to_string())
            }
            _ => {
                self.diags.error(tok.span, format!("expected a {what}"));
                None
            }
        }
    }

    /// Reserves `size` bytes for `name` in a zero-filled section, aligned to
    /// `1 << align`, without leaving the current section.
    fn macho_reserve(
        &mut self,
        section: SectionId,
        name: crate::intern::Name,
        nspan: Span,
        size: i64,
        align: u64,
    ) {
        let saved = self.cur;
        self.cur = section;
        self.align_to(align, nspan);
        self.define_label(&LabelDef::Named(name, nspan));
        let size = self.exprs.int(size as u64, nspan);
        let fill = self.exprs.int(0, nspan);
        self.cur_section().push(Fragment::new(
            FragKind::Space {
                size,
                fill,
                resolved: 0,
            },
            nspan,
        ));
        self.cur = saved;
    }

    /// Reads `, size[, align]`, with the alignment as a power of two.
    fn macho_size_align(&mut self, cur: &mut Cursor<'_>, span: Span) -> Option<(i64, u64)> {
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags.error(span, "expected `,` and a size");
            return None;
        }
        let e = self.parse_expr(cur)?;
        let size = self.eval_absolute(e, "size")?;
        if size < 0 {
            self.diags.error(span, "the size must not be negative");
            return None;
        }
        let mut align = 0;
        if cur.eat_punct(Punct::Comma).is_some() {
            let e = self.parse_expr(cur)?;
            align = self.eval_absolute(e, "alignment")?;
            if !(0..=15).contains(&align) {
                self.diags.error(
                    span,
                    format!("an alignment of 2^{align} is out of range; Mach-O allows up to 2^15"),
                );
                return None;
            }
        }
        Some((size, 1u64 << align))
    }

    /// `.zerofill SEGMENT,SECTION[,symbol,size[,align]]`.
    fn macho_dir_zerofill(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some((segment, section)) = self.macho_pair(cur) else {
            return true;
        };
        let id = self.macho_section(&segment, &section, Some((super::S_ZEROFILL, 0, 0)));
        if cur.eat_punct(Punct::Comma).is_none() {
            return true;
        }
        let Some((name, nspan)) = self.expect_name(cur) else {
            return true;
        };
        let Some((size, align)) = self.macho_size_align(cur, span) else {
            return true;
        };
        self.macho_reserve(id, name, nspan, size, align);
        true
    }

    /// `.lcomm symbol,size[,align]`: zero-filled space in `__DATA,__bss`.
    fn macho_dir_lcomm(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some((name, nspan)) = self.expect_name(cur) else {
            return true;
        };
        let Some((size, align)) = self.macho_size_align(cur, span) else {
            return true;
        };
        let id = self.macho_section("__DATA", "__bss", None);
        self.macho_reserve(id, name, nspan, size, align);
        true
    }

    /// `.comm symbol,size[,align]`, whose alignment Darwin gives as a power
    /// of two.
    fn macho_dir_comm(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some((name, nspan)) = self.expect_name(cur) else {
            return true;
        };
        let Some((size, align)) = self.macho_size_align(cur, span) else {
            return true;
        };
        let id = self.symbols.intern(name, nspan);
        let sym = self.symbols.get_mut(id);
        sym.value = SymbolValue::Common {
            size: size as u64,
            align,
        };
        sym.def_span = nspan;
        sym.ty = SymType::Object;
        sym.binding = Binding::Global;
        true
    }

    /// `.data_region [jt8|jt16|jt32]`: data, or a jump table, in code.
    fn macho_dir_data_region(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let kind = match cur.peek().ident().map(|n| self.interner.get(n).to_string()) {
            None => 1,
            Some(word) => {
                cur.advance();
                match word.as_str() {
                    "jt8" => 2,
                    "jt16" => 3,
                    "jt32" => 4,
                    _ => {
                        self.diags
                            .error(span, format!("unknown data region kind `{word}`"));
                        return true;
                    }
                }
            }
        };
        if self
            .macho
            .data_regions
            .last()
            .is_some_and(|r| r.end.is_none())
        {
            self.diags.error(
                span,
                "a `.data_region` is already open; end it with `.end_data_region` first",
            );
            return true;
        }
        let start = self.anon_label(span);
        self.macho.data_regions.push(super::DataRegion {
            kind,
            start,
            end: None,
            span,
        });
        true
    }

    /// `.build_version PLATFORM, MAJOR, MINOR[, UPDATE] [sdk_version MAJOR,
    /// MINOR[, UPDATE]]`.
    fn macho_dir_build_version(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some(pname) = self.macho_word(cur, "platform name") else {
            return true;
        };
        let Some(platform) = platform(&pname) else {
            self.diags
                .error(span, format!("unknown Mach-O platform `{pname}`"));
            return true;
        };
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags.error(span, "expected `,` and a version");
            return true;
        }
        let Some(minos) = self.macho_version(cur, span) else {
            return true;
        };
        let mut sdk = 0;
        if let Some(n) = cur.peek().ident()
            && self.interner.get(n) == "sdk_version"
        {
            cur.advance();
            let Some(v) = self.macho_version(cur, span) else {
                return true;
            };
            sdk = v;
        }
        self.macho.build_version = Some(BuildVersion {
            platform,
            minos,
            sdk,
        });
        true
    }

    /// `MAJOR, MINOR[, UPDATE]`, packed as Mach-O stores a version.
    fn macho_version(&mut self, cur: &mut Cursor<'_>, span: Span) -> Option<u32> {
        let mut parts = [0u32; 3];
        for (i, part) in parts.iter_mut().enumerate() {
            if i > 0 && cur.eat_punct(Punct::Comma).is_none() {
                if i == 1 {
                    self.diags.error(span, "expected `,` and a minor version");
                    return None;
                }
                break;
            }
            let e = self.parse_expr(cur)?;
            let v = self.eval_absolute(e, "version number")?;
            let limit = if i == 0 { 0xffff } else { 0xff };
            if !(0..=limit).contains(&v) {
                self.diags
                    .error(span, format!("version number {v} is out of range"));
                return None;
            }
            *part = v as u32;
        }
        Some((parts[0] << 16) | (parts[1] << 8) | parts[2])
    }
}

/// The `PLATFORM_*` number a `.build_version` platform name stands for.
fn platform(name: &str) -> Option<u32> {
    Some(match name {
        "macos" => 1,
        "ios" => 2,
        "tvos" => 3,
        "watchos" => 4,
        "bridgeos" => 5,
        "macCatalyst" => 6,
        "iossimulator" => 7,
        "tvossimulator" => 8,
        "watchossimulator" => 9,
        "driverkit" => 10,
        "xros" => 11,
        "xrsimulator" => 12,
        _ => return None,
    })
}
