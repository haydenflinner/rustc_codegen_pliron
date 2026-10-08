//! What the layout pass records of a fixup it leaves to the linker in a
//! Mach-O object.
//!
//! Resolution itself is decided in `layout` by the atom rules in the parent
//! module; this is the other half, called where an ELF object would build its
//! relocation. It records the reference as the source wrote it, and refuses
//! what Mach-O cannot relocate while the source position is still at hand.

use crate::assembler::{Assembler, Relocation};
use crate::expr::ExprRef;
use crate::reloc::{RelocClass, RelocDesc};
use crate::section::{FixupKind, SectionId};
use crate::source::Span;

impl Assembler {
    /// Whether the output is a Mach-O object, whose relocations and
    /// resolution follow rules of their own.
    pub(crate) fn macho_object(&self) -> bool {
        self.options.format == crate::output::Format::MachO && self.options.relocatable
    }

    /// The relocation that leaves a fixup to the linker in a Mach-O object:
    /// the target and addend as the source wrote them, and what the field
    /// computes. What the entry names and what the field holds depend on the
    /// atoms and addresses of the whole object, so the writer decides both;
    /// see `crate::output::macho`. Empty after reporting why there can be
    /// none.
    pub(crate) fn macho_relocation(
        &mut self,
        e: ExprRef,
        kind: &FixupKind,
        section: SectionId,
        fi: usize,
        at: u64,
        span: Span,
    ) -> Vec<Relocation> {
        let si = section.0 as usize;
        let mut v = match self.eval(e) {
            Ok(v) => v,
            Err(err) => {
                self.diags.emit(err.into_diagnostic());
                return Vec::new();
            }
        };
        let (arch, out) = (self.frag_arch(si, fi).0, self.target());
        if arch.elf_machine() != out.elf_machine() {
            let msg = format!(
                "this reference, in code for `{}`, needs a relocation, which an object \
                 for `{}` cannot hold",
                arch.name(),
                out.name()
            );
            self.diags.error(span, msg);
            return Vec::new();
        }
        let mut desc = RelocDesc::of(kind);
        if let Some(m) = self.find_modifier(e) {
            let name = self.interner.get(m).to_string();
            match arch.modifier_class(&name, kind) {
                Some(class) => desc.class = class,
                None => {
                    self.diags.error(
                        span,
                        format!("`@{name}` has no Mach-O relocation in this position"),
                    );
                    return Vec::new();
                }
            }
        }
        let cpu = super::Cpu::for_arch(out);
        let arm64 = cpu == Some(super::Cpu::Arm64);
        let page = matches!(
            desc.class,
            RelocClass::Page | RelocClass::PageOff | RelocClass::GotPage | RelocClass::GotPageOff
        );
        // Darwin writes the page halves of an address as `sym@PAGE` and
        // `sym@PAGEOFF`, and its assemblers take nothing else there.
        if page && self.find_modifier(e).is_none() {
            self.diags.error(
                span,
                "a Mach-O page reference is written `sym@PAGE` and `sym@PAGEOFF`, or \
                 `sym@GOTPAGE` and `sym@GOTPAGEOFF` through the GOT",
            );
            return Vec::new();
        }
        if matches!(desc.class, RelocClass::GotPage | RelocClass::GotPageOff) && v.addend != 0 {
            self.diags
                .error(span, "a reference through the GOT cannot have an addend");
            return Vec::new();
        }
        // `sym@GOT - .` is the slot relative to the field: one arm64
        // relocation, where anything else subtracted would need two.
        if arm64
            && desc.class == RelocClass::Got
            && let Some(minus) = v.minus
            && self.symbol_section(minus) == Some(section)
            && self.symbol_addr(minus) == Some(at as i64 + self.section(section).addr as i64)
        {
            v.minus = None;
            desc.pcrel = true;
        }
        if desc.class != RelocClass::Plain
            && desc.class != RelocClass::SignExtended
            && v.minus.is_some()
        {
            self.diags.error(
                span,
                "a relocation through a modifier cannot also subtract a symbol",
            );
            return Vec::new();
        }
        match (v.plus, v.minus) {
            (Some(_), Some(minus)) => {
                // A pair names both symbols. On x86-64 neither may be left
                // to another object, and on arm64 only the one added may;
                // llvm-mc refuses the same.
                let plus = v.plus.expect("matched");
                for s in [plus, minus] {
                    if arm64 && s == plus {
                        continue;
                    }
                    if !self.symbols.get(s).is_defined() {
                        let name = self.display_name(s);
                        self.diags.error(
                            span,
                            format!(
                                "`{name}` is undefined, and a Mach-O relocation can only \
                                 subtract symbols defined in the object"
                            ),
                        );
                        return Vec::new();
                    }
                }
                if kind.pcrel {
                    self.diags.error(
                        span,
                        "a difference of two symbols cannot be relocated PC-relative",
                    );
                    return Vec::new();
                }
                desc.subtrahend = Some(minus);
            }
            (Some(_), None) => {}
            _ => {
                self.diags.error(
                    span,
                    "a Mach-O relocation has to name a symbol, and this value has none",
                );
                return Vec::new();
            }
        }
        let r = Relocation {
            section,
            offset: at,
            symbol: v.plus,
            addend: v.addend,
            kind: kind.reloc,
            desc,
        };
        if let Some(cpu) = cpu
            && super::reloc_type(cpu, &r).is_none()
        {
            let msg = match (desc.class, desc.pcrel) {
                (RelocClass::SignExtended, _) => {
                    "a 64-bit Mach-O object has no relocation for a 32-bit absolute \
                     address; address the symbol RIP-relative"
                        .to_string()
                }
                (class, pcrel) => {
                    let what = match (class, pcrel) {
                        (RelocClass::Plain, true) => "PC-relative ",
                        (RelocClass::Plain, false) => "",
                        (RelocClass::Branch, _) => "branch ",
                        _ => "GOT or page ",
                    };
                    format!(
                        "a {}-byte {what}reference to another atom or object has no Mach-O \
                         relocation; the target has to be an assembler-local label in the \
                         same atom",
                        desc.size
                    )
                }
            };
            self.diags.error(span, msg);
            return Vec::new();
        }
        for s in [v.plus, v.minus].into_iter().flatten() {
            self.symbols.get_mut(s).used = true;
        }
        vec![r]
    }
}
