//! Literal pools, and the other things a backend asks the core to do to a
//! section.
//!
//! `ldr r0, =0x12345678` loads a constant no instruction can hold, so the
//! assembler puts the constant in a pool of data near the code and assembles
//! a PC-relative load from it. The instruction is encoded as its statement
//! is read; the pool is written later, at `.ltorg` or at the end of the
//! section, and until then each use refers to its entry by a label that is
//! defined when the pool is written.
//!
//! Where entries go follows GNU as, whose rules the source that uses pools
//! was written against: one pool per section, collecting every literal since
//! the last `.ltorg`; an entry shared by every use of the same number, or of
//! the same symbol plus the same addend; the entries of each width kept
//! together, narrowest first, each run aligned to its width with zeros, not
//! no-ops; and at most 1024 entries. The widths only matter on AArch64,
//! where a pool holds four-, eight- and sixteen-byte entries; ARM's are all
//! four bytes.

use crate::arch::{Literal, LiteralRequest, Request};
use crate::assembler::Assembler;
use crate::section::{FragKind, Fragment};
use crate::source::Span;
use crate::symbol::{SymbolId, SymbolValue};

/// The most entries one pool may hold, as in GNU as.
const MAX_ENTRIES: usize = 1024;

/// What makes two literals the same entry.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Key {
    /// A number known when the instruction was read.
    Const(i64),
    /// A symbol plus an addend.
    Symbol(SymbolId, i64),
    /// Anything else, which GNU as never shares; the index keeps it apart.
    Unique(usize),
}

struct Entry {
    key: Key,
    value: Literal,
    size: u8,
    labels: Vec<crate::intern::Name>,
    span: Span,
}

impl Assembler {
    /// Carries out what a backend asked for while assembling a statement.
    pub(crate) fn run_requests(&mut self, requests: Vec<Request>, span: Span) {
        for r in requests {
            match r {
                Request::AlignZero(align) => {
                    if align > 1 {
                        if self.arch.align_padding_is_code() {
                            self.map_code();
                        } else {
                            self.map_data_frag();
                        }
                        self.cur_section().push(Fragment::new(
                            FragKind::Align {
                                align,
                                fill: vec![0],
                                max_skip: None,
                                pad: 0,
                                nop_state: None,
                            },
                            span,
                        ));
                    }
                }
                Request::RecordAlign(align) => {
                    let s = self.cur_section();
                    s.align = s.align.max(align);
                }
                Request::AlignCode { align, max_skip } => {
                    let state = self
                        .cur_section()
                        .nop_state
                        .clone()
                        .unwrap_or_else(|| self.arch_state.clone());
                    self.map_code_align(&state);
                    self.cur_section().push(Fragment::new(
                        FragKind::Align {
                            align,
                            fill: Vec::new(),
                            max_skip: Some(max_skip),
                            pad: 0,
                            nop_state: Some(state),
                        },
                        span,
                    ));
                }
                Request::Literal(lit) => self.literal_pools.entry(self.cur).or_default().push(lit),
                Request::FlushLiterals => self.flush_literals(span),
            }
        }
    }

    /// Writes every pool that is still open at the end of its section.
    pub(crate) fn flush_all_literals(&mut self) {
        let mut open: Vec<_> = self.literal_pools.keys().copied().collect();
        open.sort();
        let saved = self.cur;
        for id in open {
            self.cur = id;
            self.flush_literals(Span::DUMMY);
        }
        self.cur = saved;
    }

    /// Writes the current section's pool here, and starts a new one.
    pub(crate) fn flush_literals(&mut self, span: Span) {
        let Some(requests) = self.literal_pools.remove(&self.cur) else {
            return;
        };
        let mut entries: Vec<Entry> = Vec::new();
        for (i, r) in requests.into_iter().enumerate() {
            let key = self.literal_key(&r, i);
            if let Some(e) = entries
                .iter_mut()
                .find(|e| e.key == key && e.size == r.size)
            {
                e.labels.push(r.label);
                continue;
            }
            if entries.len() == MAX_ENTRIES {
                self.diags.error(
                    r.span,
                    format!("literal pool overflow: a pool holds at most {MAX_ENTRIES} entries"),
                );
                continue;
            }
            entries.push(Entry {
                key,
                value: r.value,
                size: r.size,
                labels: vec![r.label],
                span: r.span,
            });
        }
        if entries.is_empty() {
            return;
        }
        // GNU as keeps a pool per entry width and writes them narrowest
        // first, each aligned with zeros even in code, and marks the padding
        // and the entries as data.
        entries.sort_by_key(|e| e.size);
        let section = self.cur;
        let mut written = 0u8;
        for e in entries {
            if e.size != written {
                written = e.size;
                let align = u64::from(e.size);
                self.run_requests(vec![Request::AlignZero(align)], span);
                let s = self.cur_section();
                s.align = s.align.max(align);
                if let Some((_, _, data)) =
                    crate::mapping::mapping_names(self.arch.as_ref(), &self.arch_state)
                {
                    self.map_transition(data, true);
                }
            }
            self.cur_section().seal();
            let frag = self.cur_section().next_frag_index();
            for label in e.labels {
                let id = self.symbols.intern(label, e.span);
                let sym = self.symbols.get_mut(id);
                sym.value = SymbolValue::Label { section, frag };
                sym.def_span = e.span;
                self.symbols.mark_defined(id);
            }
            match e.value {
                Literal::Const(v) => {
                    let endian = self.arch.endian();
                    let mut bytes = endian.bytes(v as u64, usize::from(e.size).min(8));
                    if usize::from(e.size) > bytes.len() {
                        // A sixteen-byte entry holds the value sign-extended,
                        // as GNU as writes it: `ldr q0, =-1` is sixteen 0xff
                        // bytes.
                        let fill = vec![u8::from(v < 0) * 0xff; usize::from(e.size) - bytes.len()];
                        match endian {
                            crate::arch::Endian::Little => bytes.extend(fill),
                            crate::arch::Endian::Big => {
                                bytes.splice(0..0, fill);
                            }
                        }
                    }
                    self.cur_section().emit_bytes(&bytes, e.span);
                }
                Literal::Expr(x) => self.emit_value(e.size, x, e.span),
            }
        }
    }

    /// Which entry a literal shares, by GNU as's rule: the same number, or
    /// the same symbol and addend.
    fn literal_key(&mut self, r: &LiteralRequest, index: usize) -> Key {
        match r.value {
            Literal::Const(v) => Key::Const(v),
            Literal::Expr(e) => match self.eval(e) {
                Ok(v) if v.minus.is_none() => match v.plus {
                    Some(p) => Key::Symbol(p, v.addend),
                    None => Key::Unique(index),
                },
                _ => Key::Unique(index),
            },
        }
    }
}
