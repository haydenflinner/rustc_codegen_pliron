//! ELF relocation numbers.
//!
//! PPC32 and PPC64 share the low end of the table — `R_PPC_ADDR32` and
//! `R_PPC64_ADDR32` are both 1, and so on up through `REL32` — which is why
//! one set of constants serves both. The numbers here were read back from
//! objects produced by `llvm-mc`, not from memory.

pub const ADDR32: u32 = 1;
pub const ADDR24: u32 = 2;
pub const ADDR16: u32 = 3;
pub const ADDR16_LO: u32 = 4;
pub const ADDR16_HI: u32 = 5;
pub const ADDR16_HA: u32 = 6;
pub const ADDR14: u32 = 7;
pub const REL24: u32 = 10;
pub const REL14: u32 = 11;
pub const REL32: u32 = 26;

/// 64-bit only.
pub const ADDR64: u32 = 38;
pub const REL64: u32 = 44;
/// The DS-form variants, whose low two bits belong to the opcode.
pub const ADDR16_DS: u32 = 56;
pub const ADDR16_LO_DS: u32 = 57;

/// The 34-bit field of a POWER10 prefixed instruction: absolute, relative to
/// the instruction (`@pcrel`), and the address of a GOT entry relative to the
/// instruction (`@got@pcrel`).
pub const D34: u32 = 128;
pub const PCREL34: u32 = 132;
pub const GOT_PCREL34: u32 = 133;

/// The relocation for a `size`-byte data reference, or `None` where the ABI
/// has none.
pub fn data(size: u8, pcrel: bool, bits64: bool) -> Option<u32> {
    Some(match (size, pcrel) {
        (8, false) if bits64 => ADDR64,
        (8, true) if bits64 => REL64,
        (4, false) => ADDR32,
        (4, true) => REL32,
        (2, false) => ADDR16,
        _ => return None,
    })
}
