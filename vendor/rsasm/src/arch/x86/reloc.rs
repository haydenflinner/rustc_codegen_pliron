//! ELF relocation types for the two x86 psABIs.
//!
//! Two independent things decide a relocation, and they are easy to conflate:
//!
//! - **The numbering follows the object.** i386 and x86-64 number their
//!   relocations differently, and a `.code32` stretch inside an x86-64 object
//!   still uses `R_X86_64_*`, because the object is ELF64. That is [`Abi`].
//! - **Some choices follow the mode.** A plain `call` gets `PLT32` in 64-bit
//!   mode but `PC32` in 32-bit mode — even inside an x86-64 object — so those
//!   decisions look at the current `bits` instead.
//!
//! Both were checked against GNU as. The numbering agrees between the ABIs
//! only by coincidence for `PC32` (2) and `PLT32` (4); the absolute sizes
//! differ, which is why using one table for both produced i386 objects that a
//! linker could not make sense of.

/// Which psABI's relocation numbering an object uses.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Abi {
    I386,
    X86_64,
}

pub mod x86_64 {
    pub const ABS64: u32 = 1;
    pub const PC32: u32 = 2;
    pub const GOT32: u32 = 3;
    pub const PLT32: u32 = 4;
    pub const GOTPCREL: u32 = 9;
    pub const ABS32: u32 = 10;
    pub const ABS32S: u32 = 11;
    pub const ABS16: u32 = 12;
    pub const PC16: u32 = 13;
    pub const ABS8: u32 = 14;
    pub const PC8: u32 = 15;
    pub const DTPMOD64: u32 = 16;
    pub const DTPOFF64: u32 = 17;
    pub const TPOFF64: u32 = 18;
    pub const TLSGD: u32 = 19;
    pub const TLSLD: u32 = 20;
    pub const DTPOFF32: u32 = 21;
    pub const GOTTPOFF: u32 = 22;
    pub const TPOFF32: u32 = 23;
    pub const GOTOFF64: u32 = 25;
    pub const GOTPC32: u32 = 26;
    pub const GOT64: u32 = 27;
    pub const PC64: u32 = 24;
}

pub mod i386 {
    pub const ABS32: u32 = 1;
    pub const PC32: u32 = 2;
    pub const GOT32: u32 = 3;
    pub const PLT32: u32 = 4;
    pub const GOTOFF: u32 = 9;
    pub const GOTPC: u32 = 10;
    pub const TLS_IE: u32 = 15;
    pub const TLS_GOTIE: u32 = 16;
    pub const TLS_LE: u32 = 17;
    pub const TLS_GD: u32 = 18;
    pub const TLS_LDM: u32 = 19;
    pub const ABS16: u32 = 20;
    pub const PC16: u32 = 21;
    pub const ABS8: u32 = 22;
    pub const PC8: u32 = 23;
    pub const TLS_LDO_32: u32 = 32;
    pub const TLS_IE_32: u32 = 33;
    pub const TLS_LE_32: u32 = 34;
    pub const SIZE32: u32 = 38;
    pub const GOT32X: u32 = 43;
}

impl Abi {
    /// The ABI of an object whose default mode is `bits` — the same thing
    /// `elf_machine` keys on, so the two can never disagree.
    pub fn for_object_bits(bits: u8) -> Abi {
        if bits == 64 { Abi::X86_64 } else { Abi::I386 }
    }

    /// The absolute relocation for an `n`-byte field.
    pub fn abs(self, n: u8) -> Option<u32> {
        Some(match (self, n) {
            (Abi::X86_64, 1) => x86_64::ABS8,
            (Abi::X86_64, 2) => x86_64::ABS16,
            (Abi::X86_64, 4) => x86_64::ABS32,
            (Abi::X86_64, 8) => x86_64::ABS64,
            (Abi::I386, 1) => i386::ABS8,
            (Abi::I386, 2) => i386::ABS16,
            (Abi::I386, 4) => i386::ABS32,
            // i386 has no 64-bit relocation; a `.quad` of a symbol there is an
            // error rather than something to approximate.
            _ => return None,
        })
    }

    /// The PC-relative relocation for an `n`-byte field.
    pub fn pcrel(self, n: u8) -> Option<u32> {
        Some(match (self, n) {
            (Abi::X86_64, 1) => x86_64::PC8,
            (Abi::X86_64, 2) => x86_64::PC16,
            (Abi::X86_64, 4) => x86_64::PC32,
            (Abi::X86_64, 8) => x86_64::PC64,
            (Abi::I386, 1) => i386::PC8,
            (Abi::I386, 2) => i386::PC16,
            (Abi::I386, 4) => i386::PC32,
            _ => return None,
        })
    }

    /// A 32-bit field the CPU sign-extends to 64 bits, as a 64-bit-mode
    /// displacement or `mov $sym, %rax` immediate is. Only x86-64 has a
    /// distinct relocation for it; i386 has nothing to sign-extend into.
    pub fn abs32_signed(self) -> u32 {
        match self {
            Abi::X86_64 => x86_64::ABS32S,
            Abi::I386 => i386::ABS32,
        }
    }

    pub fn plt32(self) -> u32 {
        match self {
            Abi::X86_64 => x86_64::PLT32,
            Abi::I386 => i386::PLT32,
        }
    }

    /// `reloc`, with `PLT32` turned into `PC32`: what GNU as writes for a
    /// PC-relative reference to a local label, which has no PLT entry.
    pub fn plt_as_pc32(self, reloc: u32) -> u32 {
        if reloc == self.plt32() {
            self.pcrel(4).expect("both ABIs have PC32")
        } else {
            reloc
        }
    }

    #[allow(dead_code)]
    pub fn got32(self) -> u32 {
        match self {
            Abi::X86_64 => x86_64::GOT32,
            Abi::I386 => i386::GOT32,
        }
    }

    /// The relocation an i386 `@` modifier names for a 32-bit field, where
    /// every one of them is: `@GOTOFF`, the TLS models, `@SIZE`. `@PLT` and
    /// `@GOT` are shared with x86-64 and handled by their own methods.
    pub fn i386_modifier(name: &str) -> Option<u32> {
        Some(match name {
            "plt" => i386::PLT32,
            "got" => i386::GOT32,
            "gotoff" => i386::GOTOFF,
            "tlsgd" => i386::TLS_GD,
            "tlsldm" => i386::TLS_LDM,
            "dtpoff" => i386::TLS_LDO_32,
            "ntpoff" => i386::TLS_LE,
            "tpoff" => i386::TLS_LE_32,
            "gotntpoff" => i386::TLS_GOTIE,
            "indntpoff" => i386::TLS_IE,
            "gottpoff" => i386::TLS_IE_32,
            "size" => i386::SIZE32,
            _ => return None,
        })
    }

    /// `R_386_GOT32X`: a `@GOT` load the linker may turn into a direct one
    /// when the symbol resolves locally.
    pub const I386_GOT32X: u32 = i386::GOT32X;

    /// `R_386_GOTPC`: the distance from here to the GOT, which is what a
    /// reference to `_GLOBAL_OFFSET_TABLE_` means in i386 code.
    pub const I386_GOTPC: u32 = i386::GOTPC;

    /// `@GOTPCREL` is RIP-relative, so it only exists on x86-64.
    pub fn gotpcrel(self) -> Option<u32> {
        match self {
            Abi::X86_64 => Some(x86_64::GOTPCREL),
            Abi::I386 => None,
        }
    }

    /// The relocation NASM's `wrt ..got` selects: the address of the symbol's
    /// GOT slot as a 64-bit value (`GOT64`), its 32-bit form on i386
    /// (`GOT32`), or, on a RIP-relative field, the PC-relative `GOTPCREL`.
    pub fn got(self, size: u8, pcrel: bool) -> Option<u32> {
        match (self, pcrel, size) {
            (Abi::X86_64, true, _) => Some(x86_64::GOTPCREL),
            (Abi::X86_64, false, 8) => Some(x86_64::GOT64),
            (Abi::X86_64, false, 4) => Some(x86_64::GOT32),
            (Abi::I386, false, 4) => Some(i386::GOT32),
            _ => None,
        }
    }

    /// `wrt ..gotoff`, the offset of the symbol from the GOT base: 64-bit on
    /// x86-64, 32-bit on i386.
    pub fn gotoff(self, size: u8) -> Option<u32> {
        match (self, size) {
            (Abi::X86_64, 8) => Some(x86_64::GOTOFF64),
            (Abi::I386, 4) => Some(i386::GOTOFF),
            _ => None,
        }
    }

    /// `wrt ..gotpc`, the distance from the field to the GOT base.
    pub fn gotpc(self, size: u8) -> Option<u32> {
        match (self, size) {
            (Abi::X86_64, 4) => Some(x86_64::GOTPC32),
            (Abi::I386, 4) => Some(i386::GOTPC),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_abis_agree_only_where_the_psabis_do() {
        // Coincidence, not design: these two share a number.
        assert_eq!(Abi::I386.pcrel(4), Abi::X86_64.pcrel(4));
        assert_eq!(Abi::I386.plt32(), Abi::X86_64.plt32());
        // And this is the difference that broke i386 objects.
        assert_eq!(Abi::I386.abs(4), Some(1));
        assert_eq!(Abi::X86_64.abs(4), Some(10));
    }

    #[test]
    fn i386_has_no_64_bit_or_rip_relative_relocations() {
        assert_eq!(Abi::I386.abs(8), None);
        assert_eq!(Abi::I386.pcrel(8), None);
        assert_eq!(Abi::I386.gotpcrel(), None);
    }
}
