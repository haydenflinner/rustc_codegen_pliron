//! Built-in assembler directives.
//!
//! Anything the table here does not claim is offered to the current
//! architecture backend, which is how `.code64` and `.intel_syntax` are
//! handled without the core knowing about x86.

use crate::assembler::{Assembler, Cond};
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::intern::Name;
use crate::lexer::{Dialect, Punct, TokKind};
use crate::parser::Statement;
use crate::section::{FragKind, Fragment, SectionFlags, SectionKind};
use crate::source::Span;
use crate::symbol::{Binding, SymType, SymbolValue, Visibility};
use std::path::PathBuf;

impl Assembler {
    pub(crate) fn directive(&mut self, stmt: &Statement, name: Name) {
        let text = self.interner.get(name).to_string();

        // Renesas's newer assemblers dot their directives (`.DB`, `.CSEG`).
        // Those are the vendor table's words, and take priority over a GNU as
        // directive of the same name, which would read their arguments wrongly.
        if matches!(self.options.dialect, Dialect::Motorola | Dialect::Renesas)
            && let Some(bare) = text.strip_prefix('.')
            && let Some(alias) = crate::dialect::lookup(self.options.dialect, bare)
            && !matches!(alias, crate::dialect::Alias::Gas(_))
        {
            self.run_alias(stmt, alias);
            return;
        }
        // In the 8-bit dialect a dotted word is ca65's, whose meaning can
        // differ from the GNU as directive of the same name (`.org`).
        if let Some(bare) = text.strip_prefix('.')
            && let Some(alias) = crate::dialect::lookup_dotted(self.options.dialect, bare)
        {
            self.run_alias(stmt, alias);
            return;
        }
        // CC-RL, CC-RH and CC-RX have tables of their own; the `$` control
        // instructions of the first two keep the `$` in their name.
        if self.options.dialect.renesas_cc()
            && let Some(alias) = crate::dialect::lookup(
                self.options.dialect,
                text.strip_prefix('.').unwrap_or(&text),
            )
        {
            self.run_alias(stmt, alias);
            return;
        }
        self.builtin_directive(stmt, name);
    }

    /// Runs a directive from the GNU as table, or the backend's.
    pub(crate) fn builtin_directive(&mut self, stmt: &Statement, name: Name) {
        let text = self.interner.get(name).to_string();
        let span = stmt.span;
        let mut cur = stmt.arg_cursor();

        // A Mach-O object has directives of its own, and gives some of the
        // common ones another meaning.
        if self.options.format == crate::output::Format::MachO
            && self.macho_directive(&text, &mut cur, span)
        {
            self.expect_end(&mut cur);
            return;
        }

        let handled = match text.as_str() {
            // ---- COFF ------------------------------------------------------
            // First, because `.type` means one thing on its own and another
            // between `.def` and `.endef`.
            _ if crate::coff::is_directive(&text)
                || (self.coff.in_def() && crate::coff::is_def_field(&text)) =>
            {
                self.coff_directive(&text, &mut cur, span)
            }

            // ---- sections -------------------------------------------------
            ".text" | ".data" | ".bss" | ".rodata" => {
                let id = self.standard_section(&text);
                self.set_section(id);
                true
            }
            ".section" => self.dir_section(&mut cur, span, false),
            ".pushsection" => self.dir_section(&mut cur, span, true),
            ".popsection" => {
                match self.pop_section() {
                    Some(id) => {
                        if id != self.cur {
                            self.dwarf_section_switch();
                        }
                        self.cur = id
                    }
                    None => self
                        .diags
                        .error(span, "`.popsection` without a matching `.pushsection`"),
                }
                true
            }
            ".previous" => {
                self.swap_previous();
                true
            }

            // ---- data -----------------------------------------------------
            // The `.Nbyte` spellings are never aligned, even on a target whose
            // other data directives are; see `Architecture::aligns_data`.
            ".byte" => self.dir_data(&mut cur, 1, span, false),
            // `.value` is x86's spelling, which GCC writes in debug sections.
            ".short" | ".hword" | ".half" | ".value" => self.dir_data(&mut cur, 2, span, true),
            ".2byte" => self.dir_data(&mut cur, 2, span, false),
            // `.word` is the one data directive whose width depends on the
            // target, so it asks the backend rather than assuming x86.
            ".word" => {
                let w = self.arch.word_bytes();
                self.dir_data(&mut cur, w, span, true)
            }
            // `.3byte` is the RL78 and RX ports' 24-bit address; like
            // `.dword` below, it means nothing else anywhere.
            ".3byte" => self.dir_data(&mut cur, 3, span, false),
            ".int" | ".long" => self.dir_data(&mut cur, 4, span, true),
            ".4byte" => self.dir_data(&mut cur, 4, span, false),
            // `.dword` (MIPS, RISC-V) and `.xword` (AArch64, SPARC V9) both
            // mean eight bytes; accepting them everywhere is harmless, since
            // neither has a different meaning on any other target.
            ".quad" => self.dir_data(&mut cur, 8, span, true),
            ".8byte" | ".dword" | ".xword" => self.dir_data(&mut cur, 8, span, false),
            // SuperH's GNU as names its unaligned `.word`, `.long` and
            // `.quad` these; they mean nothing to a target that aligns no data.
            ".uaword" if self.arch.aligns_data() => self.dir_data(&mut cur, 2, span, false),
            ".ualong" if self.arch.aligns_data() => self.dir_data(&mut cur, 4, span, false),
            ".uaquad" if self.arch.aligns_data() => self.dir_data(&mut cur, 8, span, false),
            ".ascii" => self.dir_ascii(&mut cur, false, span),
            ".asciz" | ".string" | ".asciiz" => self.dir_ascii(&mut cur, true, span),
            ".sleb128" => self.dir_leb(&mut cur, true, span),
            ".uleb128" => self.dir_leb(&mut cur, false, span),
            ".zero" => self.dir_space(&mut cur, span, true),
            ".space" | ".skip" => self.dir_space(&mut cur, span, false),
            ".fill" => self.dir_fill(&mut cur, span),
            ".incbin" => self.dir_incbin(&mut cur, span),

            // ---- layout ---------------------------------------------------
            ".align" => {
                let log2 = self.arch.align_is_log2();
                self.dir_align(&mut cur, span, log2)
            }
            ".balign" => self.dir_align(&mut cur, span, false),
            ".p2align" => self.dir_align(&mut cur, span, true),
            ".org" => {
                if let Some(e) = self.parse_expr(&mut cur) {
                    let fill = self.optional_byte(&mut cur, 0);
                    self.push_frag(
                        FragKind::Org {
                            target: e,
                            fill,
                            size: 0,
                        },
                        span,
                    );
                }
                true
            }

            // ---- symbols --------------------------------------------------
            ".globl" | ".global" => self.dir_binding(&mut cur, Binding::Global, span),
            ".weak" => self.dir_binding(&mut cur, Binding::Weak, span),
            ".local" => self.dir_binding(&mut cur, Binding::Local, span),
            ".hidden" => self.dir_visibility(&mut cur, Visibility::Hidden, span),
            ".protected" => self.dir_visibility(&mut cur, Visibility::Protected, span),
            ".internal" => self.dir_visibility(&mut cur, Visibility::Internal, span),
            // `.set word` with nothing after it is not an assignment: MIPS
            // uses it for assembler options (`.set noreorder`). Only `.set`
            // has that second meaning, so the other spellings go straight
            // to the assignment.
            ".set" if Self::is_set_option(&cur) => false,
            ".set" | ".equ" | ".equiv" => self.dir_set(&mut cur, span, text == ".equiv"),
            ".size" => self.dir_size(&mut cur, span),
            ".type" => self.dir_type(&mut cur, span),
            ".comm" => self.dir_comm(&mut cur, span, false),
            ".lcomm" => self.dir_comm(&mut cur, span, true),

            // ---- conditionals ---------------------------------------------
            ".if" | ".ifeq" | ".ifne" | ".ifdef" | ".ifndef" | ".ifb" | ".ifnb" => {
                self.dir_if(&mut cur, &text, span)
            }
            ".elseif" | ".elif" => self.dir_elseif(&mut cur, span),
            ".else" => {
                self.dir_else(span);
                true
            }
            ".endif" => {
                if self.pop_cond().is_none() {
                    self.diags.error(span, "`.endif` without a matching `.if`");
                }
                true
            }

            // ---- diagnostics ----------------------------------------------
            ".error" | ".err" => {
                let msg = self
                    .optional_string(&mut cur)
                    .unwrap_or_else(|| ".error directive".into());
                self.diags.error(span, msg);
                true
            }
            ".warning" => {
                let msg = self
                    .optional_string(&mut cur)
                    .unwrap_or_else(|| ".warning directive".into());
                self.diags.warning(span, msg);
                true
            }

            // ---- macros ---------------------------------------------------
            // `.macro`, `.endm`, `.exitm`, `.rept`, `.irp`, `.irpc` and
            // `.endr` never reach here: the statement walker intercepts them
            // because they consume the statements that follow. Only `.purgem`
            // is an ordinary directive.
            ".purgem" => self.dir_purgem(&mut cur),

            // ---- files and configuration ----------------------------------
            ".include" => self.dir_include(&mut cur, span),
            ".arch" | ".cpu" => self.dir_arch(&mut cur, span),
            // ---- debugging information ------------------------------------
            // A COFF object records the source file name as a symbol of its
            // own; the numbered form is DWARF's either way.
            ".file" if self.options.format.is_coff() && !Self::is_numbered_file(&cur) => {
                if let Some(s) = self.expect_string(&mut cur, "a file name") {
                    let name = String::from_utf8_lossy(&s).into_owned();
                    self.coff.files.push(name);
                }
                true
            }
            ".file" => {
                self.dir_dwarf_file(&mut cur, span);
                true
            }
            // DWARF in a COFF object needs section-relative relocations and
            // conventions of its own, which rsasm does not write yet; a table
            // a linker would misread is worse than none.
            ".loc" | ".loc_mark_labels" if self.options.format.is_coff() => {
                self.coff_refuse_dwarf(&text, span);
                cur.set_pos(cur.all().len());
                true
            }
            _ if text.starts_with(".cfi_") && self.options.format.is_coff() => {
                self.coff_refuse_dwarf(&text, span);
                cur.set_pos(cur.all().len());
                true
            }
            ".loc" => {
                self.dir_loc(&mut cur, span);
                true
            }
            ".loc_mark_labels" => {
                self.dir_loc_mark_labels(&mut cur, span);
                true
            }
            _ if text.starts_with(".cfi_") => self.dir_cfi(&text, &mut cur, span),
            // Recognised and ignored: they carry no information this assembler
            // acts on yet, and rejecting them would break real-world input.
            ".ident" | ".version" | ".line" => {
                cur.set_pos(cur.all().len());
                true
            }
            _ => false,
        };

        if handled {
            self.expect_end(&mut cur);
            return;
        }

        // Give the architecture a chance before reporting it unknown.
        if self.arch_directive(stmt, &text) {
            return;
        }

        if text == ".set" {
            let opt = match stmt.arg_cursor().rest().first().map(|t| t.kind) {
                Some(TokKind::Ident(n)) => self.interner.get(n).to_string(),
                _ => String::new(),
            };
            self.diags.emit(
                crate::diag::Diagnostic::error(
                    span,
                    format!(
                        "`.set {opt}` is not an option the `{}` backend understands",
                        self.arch.name()
                    ),
                )
                .with_help("to define a symbol, write `.set name, value`"),
            );
            return;
        }
        self.diags
            .error(span, format!("unknown directive `{text}`"));
    }

    /// Offers a directive to the architecture backend. Returns whether the
    /// backend claimed it.
    pub(crate) fn arch_directive(&mut self, stmt: &Statement, text: &str) -> bool {
        let args = &stmt.toks[stmt.args.min(stmt.toks.len())..];
        self.arch_directive_tokens(text, args)
    }

    /// [`Assembler::arch_directive`], for arguments that are not a statement's.
    pub(crate) fn arch_directive_tokens(
        &mut self,
        text: &str,
        args: &[crate::lexer::Token],
    ) -> bool {
        // Where a pool or padding the directive asks for is blamed.
        let span = match (args.first(), args.last()) {
            (Some(a), Some(b)) => a.span.to(b.span),
            _ => Span::DUMMY,
        };
        let mut cur = Cursor::new(args);
        let Assembler {
            arch,
            interner,
            exprs,
            diags,
            pool,
            symbols,
            arch_state,
            options,
            sections,
            cur: section,
            sm,
            ..
        } = self;
        let dialect = options.dialect;
        let bit_dot = arch.bit_addressing();
        let mut cx = crate::arch::AsmCtx {
            interner,
            exprs,
            diags,
            pool,
            symbols,
            state: arch_state,
            dialect,
            format: options.format,
            bit_dot,
            sections,
            section: *section,
            relaxable: false,
            requests: Vec::new(),
            sources: sm,
        };
        if arch.directive(&mut cx, text, &mut cur) {
            let requests = std::mem::take(&mut cx.requests);
            self.run_requests(requests, span);
            self.expect_end(&mut cur);
            return true;
        }
        false
    }

    // ---- helpers ----------------------------------------------------------

    fn push_frag(&mut self, kind: FragKind, span: Span) {
        self.cur_section().push(Fragment::new(kind, span));
    }

    /// Reads an optional `, expr` trailing byte value.
    fn optional_byte(&mut self, cur: &mut Cursor<'_>, default: u8) -> u8 {
        if cur.eat_punct(Punct::Comma).is_none() {
            return default;
        }
        match self.parse_expr(cur) {
            Some(e) => self
                .eval_absolute(e, "fill value")
                .unwrap_or(default as i64) as u8,
            None => default,
        }
    }

    fn optional_string(&mut self, cur: &mut Cursor<'_>) -> Option<String> {
        let TokKind::Str(i) = cur.peek().kind else {
            return None;
        };
        cur.advance();
        Some(String::from_utf8_lossy(self.pool.get(i)).into_owned())
    }

    pub(crate) fn expect_string(&mut self, cur: &mut Cursor<'_>, what: &str) -> Option<Vec<u8>> {
        let tok = cur.peek();
        let TokKind::Str(i) = tok.kind else {
            self.diags
                .error(tok.span, format!("expected a string {what}"));
            return None;
        };
        cur.advance();
        Some(self.pool.get(i).to_vec())
    }

    pub(crate) fn expect_name(&mut self, cur: &mut Cursor<'_>) -> Option<(Name, Span)> {
        let tok = cur.peek();
        match tok.ident() {
            Some(n) => {
                cur.advance();
                Some((n, tok.span))
            }
            None => {
                self.diags.error(tok.span, "expected a symbol name");
                None
            }
        }
    }

    // ---- data -------------------------------------------------------------

    /// A data directive of `size`-byte values. `aligned` marks the ones that
    /// start on their own boundary where the target asks for that.
    fn dir_data(&mut self, cur: &mut Cursor<'_>, size: u8, span: Span, aligned: bool) -> bool {
        if cur.at_end() {
            return true;
        }
        self.map_data();
        if aligned && self.arch.aligns_data() {
            self.align_data(size as u64, span);
        }
        loop {
            let mark = self.exprs.len();
            let Some(e) = self.parse_data_expr(cur) else {
                return true;
            };
            if self.dwarf.line.pending {
                self.dwarf_data();
            }
            // `.` in each value is the address of that value, not of the
            // statement: `.long a - ., b - .` is two PC-relative values in
            // GNU as and llvm-mc alike.
            if self.here_sym.is_some() {
                self.here_sym = Some(self.anon_label(span));
                self.bind_positional(mark);
            }
            self.emit_value(size, e, span);
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        true
    }

    /// One value of a data directive. On a target whose relocation modifiers
    /// are written as a call around the whole value (AVR's `.word pm(main)`;
    /// see [`Architecture::expr_modifiers`]) that call is read here, as GNU
    /// as reads it in `avr_parse_cons_expression`: only directly after the
    /// directive or a comma, and only where a `(` follows the name.
    ///
    /// [`Architecture::expr_modifiers`]: crate::arch::Architecture::expr_modifiers
    fn parse_data_expr(&mut self, cur: &mut Cursor<'_>) -> Option<ExprRef> {
        let tok = cur.peek();
        let name = tok.ident().and_then(|n| {
            let text = self.interner.get(n);
            self.arch
                .expr_modifiers()
                .iter()
                .find(|m| text.eq_ignore_ascii_case(m))
                .copied()
        });
        let Some(name) = name.filter(|_| cur.nth(1).is_punct(Punct::LParen)) else {
            return self.parse_expr(cur);
        };
        cur.advance();
        let open = cur.advance();
        let inner = self.parse_expr(cur)?;
        let Some(close) = cur.eat_punct(Punct::RParen) else {
            self.diags.emit(
                crate::diag::Diagnostic::error(cur.peek().span, "expected `)`")
                    .with_note(open.span, "to match this `(`"),
            );
            return None;
        };
        let name = self.interner.intern(name);
        let kind = crate::expr::ExprKind::Modifier(name, inner);
        Some(self.exprs.alloc(kind, tok.span.to(close.span)))
    }

    /// Pads to a `size`-byte boundary ahead of data that must start on one,
    /// and remembers the padding so that layout can refuse any it needed; see
    /// [`Architecture::aligns_data`](crate::arch::Architecture::aligns_data).
    fn align_data(&mut self, size: u64, span: Span) {
        let frag = self.cur_section().push(Fragment::new(
            FragKind::Align {
                align: size,
                fill: vec![0],
                max_skip: None,
                pad: 0,
                nop_state: None,
            },
            span,
        ));
        self.align_tests.push((self.cur, frag));
        self.section_mut(self.cur).align = self.section(self.cur).align.max(size);
    }

    /// Emits `size` bytes for `e`, as literal bytes when it already folds to a
    /// constant and as a fixup otherwise.
    pub(crate) fn emit_value(&mut self, size: u8, e: ExprRef, span: Span) {
        // A zero takes space in a section with no contents, in GNU as and
        // llvm-mc alike: Clang writes a zero-initialised AVR global as
        // `.short 0` in `.bss`. Anything else has nowhere to go.
        if self.section(self.cur).kind == SectionKind::Nobits
            && self.eval_ref(e).ok().and_then(|v| v.as_abs()) == Some(0)
        {
            let size = self.exprs.int(size as u64, span);
            let fill = self.exprs.int(0, span);
            self.push_frag(
                FragKind::Space {
                    size,
                    fill,
                    resolved: 0,
                },
                span,
            );
            return;
        }
        if self.check_nobits(span) {
            return;
        }
        self.bind_here_to_item(e);
        let mut reloc = self.arch.data_reloc(size, false).unwrap_or(0);
        let mut kind = crate::section::FixupKind::data(size);
        // A modifier the target does not recognise used to fall back to the
        // plain data relocation, so `.long foo@got` quietly became an
        // absolute reference to `foo`. That is a different program, so it is
        // an error instead. The check comes before the constant fold below
        // because a modifier can be wrong for the field's width whatever the
        // value is: AVR has `pm()` for a `.word` and none for a `.byte`.
        // A Mach-O object checks its modifiers against its own
        // relocations, once it builds them.
        if let Some(m) = self.find_modifier(e)
            && !self.macho_object()
        {
            let name = self.interner.get(m).to_string();
            // COFF's own modifiers (`@IMGREL`) are the format's, not the
            // backend's: they name what the field holds, not a relocation
            // number; see `crate::coff::modifier_class`.
            let coff = self
                .options
                .format
                .is_coff()
                .then(|| crate::coff::modifier_class(&name))
                .flatten();
            if let Some(class) = coff {
                let kind = crate::section::FixupKind::data(size)
                    .with_reloc(reloc)
                    .with_class(class);
                let espan = self.exprs.span(e);
                self.cur_section().emit_fixup(size, e, kind, espan);
                return;
            }
            match self.arch.modifier_reloc(&name, size, false) {
                Some(r) => {
                    reloc = r;
                    // One that takes part of the value writes the field
                    // itself; the value it takes that part of is what has
                    // to fit.
                    if let crate::arch::FlatModifier::Field { write, unit } =
                        self.arch.flat_modifier(&name)
                    {
                        kind = kind.with_field(63, unit).scatter(write);
                    }
                }
                None => {
                    let espan = self.exprs.span(e);
                    // Spelled the way the target writes it: `lo8(x)` on AVR,
                    // `x@got` everywhere else.
                    let written = if self.arch.expr_modifiers().contains(&name.as_str()) {
                        format!("`{name}()`")
                    } else {
                        format!("`@{name}`")
                    };
                    self.diags.error(
                        espan,
                        format!(
                            "{written} is not a relocation modifier the `{}` backend supports \
                             in a {size}-byte data field",
                            self.arch.name()
                        ),
                    );
                    return;
                }
            }
        }
        // Resolve now if it already has a value: a `.set` symbol is a
        // snapshot at each use, so a later redefinition must not reach back
        // and change bytes that were already emitted.
        // In a Mach-O object a difference of labels already a fixed distance
        // apart is a value too; see `Assembler::macho_fixed_difference`.
        if let Some(v) = self
            .eval_ref(e)
            .ok()
            .and_then(|v| v.as_abs())
            .or_else(|| self.macho_fixed_difference(e))
        {
            if !kind.fits(v as i128) {
                let espan = self.exprs.span(e);
                let msg = if kind.value_align > 1 && v % kind.value_align as i64 != 0 {
                    format!("value {v} is not a multiple of {}", kind.value_align)
                } else {
                    format!("value {v} does not fit in {size} byte(s)")
                };
                self.diags.error(espan, msg);
                return;
            }
            let mut bytes = vec![0; size as usize];
            kind.write(self.arch.endian(), &mut bytes, v);
            self.cur_section().emit_bytes(&bytes, span);
            return;
        }
        let kind = kind.with_reloc(reloc);
        let espan = self.exprs.span(e);
        self.cur_section().emit_fixup(size, e, kind, espan);
    }

    fn dir_ascii(&mut self, cur: &mut Cursor<'_>, terminate: bool, span: Span) -> bool {
        if cur.at_end() {
            return true;
        }
        self.map_data();
        loop {
            let Some(mut bytes) = self.expect_string(cur, "literal") else {
                return true;
            };
            if terminate {
                bytes.push(0);
            }
            if self.dwarf.line.pending {
                self.dwarf_data();
            }
            self.emit_bytes(&bytes, span);
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        true
    }

    fn dir_leb(&mut self, cur: &mut Cursor<'_>, signed: bool, span: Span) -> bool {
        self.map_data();
        loop {
            let Some(e) = self.parse_expr(cur) else {
                return true;
            };
            // llvm-mc writes a constant as bytes, which consume a `.loc`.
            if self.dwarf.line.pending && self.eval_ref(e).is_ok_and(|v| v.is_absolute()) {
                self.dwarf_data();
            }
            if !self.check_nobits(span) {
                self.push_frag(
                    FragKind::Leb128 {
                        value: e,
                        signed,
                        encoded: vec![0],
                    },
                    span,
                );
            }
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        true
    }

    fn dir_space(&mut self, cur: &mut Cursor<'_>, span: Span, zero_only: bool) -> bool {
        let Some(size) = self.parse_expr(cur) else {
            return true;
        };
        let fill = if zero_only {
            self.exprs.int(0, span)
        } else if cur.eat_punct(Punct::Comma).is_some() {
            match self.parse_expr(cur) {
                Some(e) => e,
                None => return true,
            }
        } else {
            self.exprs.int(0, span)
        };
        // A `.space` of a known size is a fill fragment to GNU as, which
        // marks its own start; one it cannot size yet is not.
        self.map_data();
        if self
            .eval_ref(size)
            .ok()
            .and_then(|v| v.as_abs())
            .is_some_and(|n| n > 0)
        {
            self.map_data_frag();
        }
        self.push_frag(
            FragKind::Space {
                size,
                fill,
                resolved: 0,
            },
            span,
        );
        true
    }

    fn dir_fill(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some(count_e) = self.parse_expr(cur) else {
            return true;
        };
        let Some(count) = self.eval_absolute(count_e, "`.fill` count") else {
            return true;
        };
        let mut size: i64 = 1;
        let mut value: i64 = 0;
        if cur.eat_punct(Punct::Comma).is_some() {
            let Some(e) = self.parse_expr(cur) else {
                return true;
            };
            size = self.eval_absolute(e, "`.fill` size").unwrap_or(1);
            if cur.eat_punct(Punct::Comma).is_some() {
                let Some(e) = self.parse_expr(cur) else {
                    return true;
                };
                value = self.eval_absolute(e, "`.fill` value").unwrap_or(0);
            }
        }
        if count < 0 || size < 0 {
            self.diags
                .error(span, "`.fill` count and size must not be negative");
            return true;
        }
        if !(0..=8).contains(&size) {
            self.diags
                .error(span, "`.fill` size must be between 0 and 8");
            return true;
        }
        let total = (count as u64).saturating_mul(size as u64);
        if total > (1 << 28) {
            self.diags
                .error(span, "`.fill` would emit more than 256 MiB");
            return true;
        }
        self.map_data();
        if total > 0 {
            self.map_data_frag();
        }
        // llvm-mc writes `.fill` as values, which consume a `.loc`.
        if self.dwarf.line.pending {
            self.dwarf_data();
        }
        let unit = self.arch.endian().bytes(value as u64, size as usize);
        let mut bytes = Vec::with_capacity(total as usize);
        for _ in 0..count {
            bytes.extend_from_slice(&unit);
        }
        self.emit_bytes(&bytes, span);
        true
    }

    fn dir_incbin(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some(name) = self.expect_string(cur, "file name") else {
            return true;
        };
        let name = String::from_utf8_lossy(&name).into_owned();
        let Some(path) = self.find_include(&name) else {
            self.diags.error(span, format!("cannot find `{name}`"));
            return true;
        };
        match std::fs::read(&path) {
            Ok(data) => {
                self.map_data();
                if self.dwarf.line.pending {
                    self.dwarf_data();
                }
                self.emit_bytes(&data, span)
            }
            Err(e) => self
                .diags
                .error(span, format!("cannot read `{}`: {e}", path.display())),
        }
        true
    }

    // ---- layout -----------------------------------------------------------

    pub(crate) fn dir_align(
        &mut self,
        cur: &mut Cursor<'_>,
        span: Span,
        power_of_two: bool,
    ) -> bool {
        let Some(e) = self.parse_expr(cur) else {
            return true;
        };
        let Some(n) = self.eval_absolute(e, "alignment") else {
            return true;
        };
        if n < 0 || n > 60 && power_of_two {
            self.diags.error(span, "alignment is out of range");
            return true;
        }
        let align: u64 = if power_of_two {
            1u64 << n
        } else {
            if n == 0 {
                return true;
            }
            if !(n as u64).is_power_of_two() {
                self.diags
                    .error(span, format!("alignment {n} is not a power of two"));
                return true;
            }
            n as u64
        };

        let mut fill_expr = None;
        let mut max_skip = None;
        if cur.eat_punct(Punct::Comma).is_some() {
            if !cur.check_punct(Punct::Comma) {
                fill_expr = self.parse_expr(cur);
            }
            if cur.eat_punct(Punct::Comma).is_some()
                && let Some(e) = self.parse_expr(cur)
            {
                max_skip = self
                    .eval_absolute(e, "`.align` maximum skip")
                    .map(|v| v.max(0) as u64);
            }
        }

        // Executable sections pad with real no-ops so the padding stays
        // executable; everything else pads with zeroes.
        let fill = match fill_expr {
            Some(e) => vec![self.eval_absolute(e, "fill value").unwrap_or(0) as u8],
            None if self.section(self.cur).flags.exec => Vec::new(),
            None => vec![0],
        };
        // GNU as makes no fragment for an alignment of one byte, and marks
        // the start of any other: as code where no-ops pad it.
        // No-ops are for the instruction set of the last instruction, if one
        // has been assembled since the last fragment ended; see
        // `Section::nop_state`.
        let nop_state = self
            .section(self.cur)
            .nop_state
            .clone()
            .unwrap_or_else(|| self.arch_state.clone());
        if align > 1 {
            if fill.is_empty() {
                self.map_code_align(&nop_state);
            } else {
                self.map_data_frag();
            }
        }
        self.push_frag(
            FragKind::Align {
                align,
                nop_state: fill.is_empty().then_some(nop_state),
                fill,
                max_skip,
                pad: 0,
            },
            span,
        );
        self.section_mut(self.cur).align = self.section(self.cur).align.max(align);
        true
    }

    // ---- sections ---------------------------------------------------------

    fn dir_section(&mut self, cur: &mut Cursor<'_>, span: Span, push: bool) -> bool {
        let tok = cur.peek();
        let name = match tok.kind {
            // A name is everything up to a comma or a space, as GNU as reads
            // it: `.note.GNU-stack` is one name, not a subtraction.
            TokKind::Ident(n) => {
                let first = cur.advance();
                let mut last = first;
                while !cur.peek().is_eol()
                    && !cur.peek().is_punct(Punct::Comma)
                    && !cur.peek().preceded_by_space
                {
                    last = cur.advance();
                }
                if last.span == first.span {
                    n
                } else {
                    let text = self.sm.span_text(first.span.to(last.span)).to_string();
                    self.interner.intern(&text)
                }
            }
            TokKind::Str(i) => {
                cur.advance();
                let s = String::from_utf8_lossy(self.pool.get(i)).into_owned();
                self.interner.intern(&s)
            }
            _ => {
                self.diags.error(tok.span, "expected a section name");
                return true;
            }
        };

        let text = self.interner.get(name).to_string();
        // GNU as for MSP430 refers to the C runtime's set-up routine for the
        // section as soon as it is named; see `Architecture::section_symbols`.
        for sym in self.target().section_symbols(&text) {
            self.refer_to_symbol(sym, tok.span);
        }
        let mut kind = if text.starts_with(".bss") {
            SectionKind::Nobits
        } else {
            SectionKind::Progbits
        };
        let mut flags = default_flags_for(&text);
        let mut entsize = 0u64;

        // A COFF section's attributes are its own: flag letters that mean
        // different things, and a COMDAT selection where ELF has a type.
        if self.options.format.is_coff() {
            let mut info = None;
            let mut key = name;
            if cur.eat_punct(Punct::Comma).is_some() {
                let (characteristics, comdat, k) =
                    crate::coff::parse_section_attributes(self, &mut *cur, &text, span);
                flags = crate::coff::section_flags(characteristics);
                kind = k;
                info = Some(crate::coff::SectionInfo {
                    characteristics,
                    comdat,
                });
                // llvm-mc tells sections apart by name and COMDAT symbol.
                if let Some(sym) = comdat.and_then(|c| c.symbol) {
                    let sym = self.interner.get(self.symbols.get(sym).name).to_string();
                    key = self.interner.intern(&format!("{text}\u{0}{sym}"));
                }
            }
            let id = self.get_or_create_section(key, kind, flags, 1);
            // The first description of a section is the one that counts.
            if let Some(info) = info {
                self.coff.sections.entry(id).or_insert(info);
            }
            if push {
                self.push_section_stack();
            }
            self.set_section(id);
            return true;
        }

        if cur.eat_punct(Punct::Comma).is_some() {
            if let Some(s) = self.expect_string(cur, "of section flags") {
                flags = parse_flags(&String::from_utf8_lossy(&s));
            }
            // `,@progbits` or `,%progbits`
            if cur.eat_punct(Punct::Comma).is_some() {
                if cur.eat_punct(Punct::At).is_none() {
                    cur.eat_punct(Punct::Percent);
                }
                // A type may also be written as its number, as llvm-mc writes
                // `@0x7000001e` for MIPS's DWARF sections; any such section
                // holds bits.
                if let TokKind::Int(_) = cur.peek().kind {
                    cur.advance();
                } else if let Some((tn, _)) = self.expect_name(cur) {
                    let t = self.interner.get(tn).to_ascii_lowercase();
                    kind = match t.as_str() {
                        "nobits" => SectionKind::Nobits,
                        "note" => SectionKind::Note,
                        _ => SectionKind::Progbits,
                    };
                }
                if cur.eat_punct(Punct::Comma).is_some()
                    && let Some(e) = self.parse_expr(cur)
                {
                    entsize = self
                        .eval_absolute(e, "section entry size")
                        .unwrap_or(0)
                        .max(0) as u64;
                }
            }
        }

        let id = self.get_or_create_section(name, kind, flags, 1);
        self.section_mut(id).entsize = entsize;
        if push {
            self.push_section_stack();
        }
        self.set_section(id);
        if self.dwarf.line.source.on {
            self.dwarf_section_named(id);
        }
        true
    }

    // ---- symbols ----------------------------------------------------------

    fn dir_binding(&mut self, cur: &mut Cursor<'_>, binding: Binding, _span: Span) -> bool {
        loop {
            let Some((name, span)) = self.expect_name(cur) else {
                return true;
            };
            let id = self.symbols.intern(name, span);
            self.symbols.get_mut(id).binding = binding;
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        true
    }

    fn dir_visibility(&mut self, cur: &mut Cursor<'_>, vis: Visibility, _span: Span) -> bool {
        loop {
            let Some((name, span)) = self.expect_name(cur) else {
                return true;
            };
            let id = self.symbols.intern(name, span);
            self.symbols.get_mut(id).visibility = vis;
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        true
    }

    /// True for `.set word` — a single identifier and nothing else, which no
    /// assignment can be. Returning `false` from the table sends it on to the
    /// architecture's directive hook.
    /// Whether a `.file` is the numbered form, which is DWARF's whatever the
    /// object format; `.file "name"` alone is the source file's name.
    fn is_numbered_file(cur: &Cursor<'_>) -> bool {
        matches!(
            cur.peek().kind,
            TokKind::Int(_) | TokKind::Punct(Punct::Minus)
        )
    }

    fn is_set_option(cur: &Cursor<'_>) -> bool {
        let rest = cur.rest();
        rest.len() == 1 && matches!(rest[0].kind, TokKind::Ident(_))
    }

    pub(crate) fn dir_set(&mut self, cur: &mut Cursor<'_>, span: Span, once_only: bool) -> bool {
        let Some((name, nspan)) = self.expect_name(cur) else {
            return true;
        };
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags.error(span, "expected `,` after the symbol name");
            return true;
        }
        let Some(e) = self.parse_expr(cur) else {
            return true;
        };
        let id = self.symbols.intern(name, nspan);
        if once_only && self.symbols.get(id).is_defined() {
            let prev = self.symbols.get(id).def_span;
            let display = self.display_name(id);
            self.diags.emit(
                crate::diag::Diagnostic::error(span, format!("`{display}` is already defined"))
                    .with_note(prev, "previous definition is here"),
            );
            return true;
        }
        let sym = self.symbols.get_mut(id);
        sym.value = SymbolValue::Expr(e);
        sym.def_span = nspan;
        sym.redefinable = !once_only;
        self.symbols.mark_defined(id);
        true
    }

    fn dir_size(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some((name, nspan)) = self.expect_name(cur) else {
            return true;
        };
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags.error(span, "expected `,` after the symbol name");
            return true;
        }
        let Some(e) = self.parse_expr(cur) else {
            return true;
        };
        let id = self.symbols.intern(name, nspan);
        self.symbols.get_mut(id).size = Some(e);
        true
    }

    fn dir_type(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some((name, nspan)) = self.expect_name(cur) else {
            return true;
        };
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags.error(span, "expected `,` after the symbol name");
            return true;
        }
        // The type is written `@function`, `%function` or `STT_FUNC`.
        if cur.eat_punct(Punct::At).is_none() {
            cur.eat_punct(Punct::Percent);
        }
        let Some((tname, tspan)) = self.expect_name(cur) else {
            return true;
        };
        // Where `@` may start a name (COFF), `@function` is one word.
        let t = self.interner.get(tname).to_ascii_lowercase();
        let ty = match t.trim_start_matches('@').trim_start_matches("stt_") {
            "function" | "func" => SymType::Func,
            "object" => SymType::Object,
            "notype" => SymType::NoType,
            "tls_object" | "tls" => SymType::Tls,
            "common" => SymType::Object,
            other => {
                self.diags
                    .error(tspan, format!("unknown symbol type `{other}`"));
                return true;
            }
        };
        let id = self.symbols.intern(name, nspan);
        self.symbols.get_mut(id).ty = ty;
        let _ = span;
        true
    }

    fn dir_comm(&mut self, cur: &mut Cursor<'_>, span: Span, local: bool) -> bool {
        let Some((name, nspan)) = self.expect_name(cur) else {
            return true;
        };
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags.error(span, "expected `,` and a size");
            return true;
        }
        let Some(e) = self.parse_expr(cur) else {
            return true;
        };
        let Some(size) = self.eval_absolute(e, "`.comm` size") else {
            return true;
        };
        let mut align = 1u64;
        let mut given = None;
        if cur.eat_punct(Punct::Comma).is_some()
            && let Some(e) = self.parse_expr(cur)
        {
            let v = self.eval_absolute(e, "`.comm` alignment").unwrap_or(1);
            align = v.max(1) as u64;
            given = Some(v.max(0) as u64);
        }
        if size < 0 {
            self.diags.error(span, "`.comm` size must not be negative");
            return true;
        }
        // COFF has no local common block: `.lcomm` puts the object in
        // `.bss` instead, which is where llvm-mc and GNU as put it.
        if local && self.options.format.is_coff() {
            self.coff_lcomm(name, nspan, size as u64, align, span);
            return true;
        }
        let mut size = size as u64;
        // A COFF common block records only its size, and `.comm`'s alignment
        // is a power of two there, as both references read it. llvm-mc makes
        // the block at least that large, which is how the linker, placing it
        // at a boundary of its size, honours the alignment.
        if self.options.format.is_coff()
            && let Some(log2) = given
        {
            if log2 > 5 {
                self.diags.error(
                    span,
                    format!("a COFF common block can be aligned to at most 32 bytes, not 2^{log2}"),
                );
                return true;
            }
            align = 1 << log2;
            size = size.max(align);
        }
        let id = self.symbols.intern(name, nspan);
        let sym = self.symbols.get_mut(id);
        sym.value = SymbolValue::Common { size, align };
        sym.def_span = nspan;
        sym.ty = SymType::Object;
        if !local {
            sym.binding = Binding::Global;
        }
        for sym in self.target().common_symbols() {
            self.refer_to_symbol(sym, nspan);
        }
        true
    }

    // ---- conditionals -----------------------------------------------------

    fn dir_if(&mut self, cur: &mut Cursor<'_>, kind: &str, span: Span) -> bool {
        // A conditional nested inside a false branch is pushed inactive
        // without evaluating its condition, which may not even be resolvable.
        if !self.cond_active() {
            self.push_cond(Cond {
                active: false,
                taken: true,
                seen_else: false,
                span,
            });
            cur.set_pos(cur.all().len());
            return true;
        }
        let value = self.eval_condition(cur, kind);
        self.push_cond(Cond {
            active: value,
            taken: value,
            seen_else: false,
            span,
        });
        true
    }

    fn eval_condition(&mut self, cur: &mut Cursor<'_>, kind: &str) -> bool {
        match kind {
            ".ifdef" | ".ifndef" => {
                let defined = match self.expect_name(cur) {
                    Some((name, _)) => self
                        .symbols
                        .lookup(name)
                        .is_some_and(|id| self.symbols.get(id).is_defined()),
                    None => false,
                };
                if kind == ".ifdef" { defined } else { !defined }
            }
            ".ifb" | ".ifnb" => {
                let blank = cur.at_end() || cur.is_empty();
                cur.set_pos(cur.all().len());
                if kind == ".ifb" { blank } else { !blank }
            }
            _ => {
                let mark = self.exprs.len();
                let Some(e) = self.parse_expr(cur) else {
                    return false;
                };
                // CC-RX takes a symbol not defined yet as 0 here
                // (R20UT3248EJ0115 page 495).
                if self.options.dialect == Dialect::CcRx {
                    self.ccrx_undefined_as_zero(mark);
                }
                let Some(v) = self.eval_absolute(e, "`.if` condition") else {
                    return false;
                };
                match kind {
                    ".ifeq" => v == 0,
                    ".ifne" => v != 0,
                    _ => v != 0,
                }
            }
        }
    }

    fn dir_elseif(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        self.dir_elseif_kind(cur, span, ".if")
    }

    /// `.elseif`, with the test of the `.if` variant `kind`: CC-RL's
    /// `$ELSEIFN` is an `.elseif` that tests like `.ifeq`.
    pub(crate) fn dir_elseif_kind(&mut self, cur: &mut Cursor<'_>, span: Span, kind: &str) -> bool {
        let Some(state) = self.cond_top() else {
            self.diags.error(span, "`.elseif` without a matching `.if`");
            cur.set_pos(cur.all().len());
            return true;
        };
        if state.seen_else {
            self.diags.error(span, "`.elseif` after `.else`");
            cur.set_pos(cur.all().len());
            return true;
        }
        if state.taken {
            // An earlier branch already won; skip this condition entirely
            // rather than evaluating something that may not resolve.
            self.set_cond_active(false);
            cur.set_pos(cur.all().len());
            return true;
        }
        // Evaluating the condition needs the enclosing conditional to look
        // active, which it is: `taken` is false only when no branch ran.
        let outer_active = self.enclosing_cond_active();
        let value = outer_active && self.eval_condition(cur, kind);
        self.set_cond_active(value);
        if value {
            self.mark_cond_taken();
        }
        true
    }

    fn dir_else(&mut self, span: Span) {
        let Some(state) = self.cond_top() else {
            self.diags.error(span, "`.else` without a matching `.if`");
            return;
        };
        if state.seen_else {
            self.diags.error(span, "duplicate `.else`");
            return;
        }
        let taken = state.taken;
        let outer_active = self.enclosing_cond_active();
        self.mark_cond_else();
        self.set_cond_active(outer_active && !taken);
        if !taken {
            self.mark_cond_taken();
        }
    }

    // ---- files ------------------------------------------------------------

    pub(crate) fn find_include(&self, name: &str) -> Option<PathBuf> {
        let direct = PathBuf::from(name);
        if direct.is_absolute() && direct.exists() {
            return Some(direct);
        }
        for dir in &self.options.include_paths {
            let p = dir.join(name);
            if p.exists() {
                return Some(p);
            }
        }
        direct.exists().then_some(direct)
    }

    fn dir_purgem(&mut self, cur: &mut Cursor<'_>) -> bool {
        loop {
            let Some((name, span)) = self.expect_name(cur) else {
                return true;
            };
            let lowered = self.interner.get(name).to_ascii_lowercase();
            let key = self.interner.intern(&lowered);
            if self.macros.remove(&key).is_none() {
                self.diags
                    .error(span, format!("no macro named `{lowered}` to purge"));
            }
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        true
    }

    fn dir_include(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some(name) = self.expect_string(cur, "file name") else {
            return true;
        };
        let name = String::from_utf8_lossy(&name).into_owned();
        let Some(path) = self.find_include(&name) else {
            self.diags
                .error(span, format!("cannot find include file `{name}`"));
            return true;
        };
        self.include(&path, span);
        true
    }

    fn dir_arch(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let tok = cur.peek();
        let name = match tok.kind {
            TokKind::Str(i) => {
                cur.advance();
                String::from_utf8_lossy(self.pool.get(i)).into_owned()
            }
            // A bare name is whatever is written without spaces, since names
            // like `68000`, `78k0` and `x86-64` are not single identifiers.
            // GNU as for m68k lists extensions after commas, `68000,68881`,
            // so the whole run is offered first and the name before the first
            // comma after that.
            _ if !tok.is_eol() && !tok.is_punct(Punct::Comma) => {
                let mut first = None;
                let mut last = cur.advance();
                while !cur.peek().is_eol() && !cur.peek().preceded_by_space {
                    if cur.peek().is_punct(Punct::Comma) && first.is_none() {
                        first = Some((cur.pos(), last));
                    }
                    last = cur.advance();
                }
                let whole = self.sm.span_text(tok.span.to(last.span)).to_string();
                match first {
                    Some((pos, before)) if crate::arch::lookup(&whole).is_none() => {
                        cur.set_pos(pos);
                        self.sm.span_text(tok.span.to(before.span)).to_string()
                    }
                    _ => whole,
                }
            }
            _ => {
                self.diags.error(tok.span, "expected an architecture name");
                return true;
            }
        };
        match crate::arch::lookup(&name) {
            Some(a) => self.switch_arch(a),
            None => {
                let avail = crate::arch::available().join(", ");
                self.diags.emit(
                    crate::diag::Diagnostic::error(span, format!("unknown architecture `{name}`"))
                        .with_help(format!("this build supports: {avail}")),
                );
            }
        }
        true
    }
}

fn default_flags_for(name: &str) -> SectionFlags {
    if name.starts_with(".text") || name == ".init" || name == ".fini" {
        SectionFlags::text()
    } else if name.starts_with(".rodata") {
        SectionFlags::rodata()
    } else if name.starts_with(".data") || name.starts_with(".bss") {
        SectionFlags::data()
    } else {
        SectionFlags::default()
    }
}

fn parse_flags(s: &str) -> SectionFlags {
    let mut f = SectionFlags::default();
    for c in s.chars() {
        match c {
            'a' => f.alloc = true,
            'w' => f.write = true,
            'x' => f.exec = true,
            'M' => f.merge = true,
            'S' => f.strings = true,
            'T' => f.tls = true,
            'G' => f.group = true,
            _ => {}
        }
    }
    f
}
