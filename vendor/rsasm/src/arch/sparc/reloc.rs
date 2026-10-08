//! ELF relocation types for SPARC (`R_SPARC_*`), shared by the 32- and 64-bit
//! ABIs: `elf32-sparc` and `elf64-sparc` number them the same way.

#[allow(dead_code)]
pub const NONE: u32 = 0;
pub const ABS8: u32 = 1;
pub const ABS16: u32 = 2;
pub const ABS32: u32 = 3;
pub const DISP8: u32 = 4;
pub const DISP16: u32 = 5;
pub const DISP32: u32 = 6;
/// `call`: a 30-bit field counted in instructions.
pub const WDISP30: u32 = 7;
/// `Bicc`: a 22-bit field counted in instructions.
pub const WDISP22: u32 = 8;
/// The high 22 bits of a value, as `sethi` takes them.
pub const HI22: u32 = 9;
/// A bare 22-bit field, for `sethi` given a plain expression.
pub const ABS22: u32 = 10;
/// A 13-bit signed immediate.
pub const ABS13: u32 = 13;
/// The low 10 bits of a value, as `%lo()` produces them.
pub const LO10: u32 = 12;
pub const ABS64: u32 = 32;
/// `R_SPARC_32` for a field that need not be aligned.
pub const UA32: u32 = 23;
/// `R_SPARC_64` for a field that need not be aligned.
pub const UA64: u32 = 54;
/// V9 branch on register: a 16-bit split field.
pub const WDISP16: u32 = 40;
/// V9 predicted branch: a 19-bit field.
pub const WDISP19: u32 = 41;
pub const DISP64: u32 = 46;

/// The absolute relocation for an `n`-byte data reference.
pub fn abs(n: u8) -> Option<u32> {
    Some(match n {
        1 => ABS8,
        2 => ABS16,
        4 => ABS32,
        8 => ABS64,
        _ => return None,
    })
}

/// The PC-relative relocation for an `n`-byte data reference.
pub fn pcrel(n: u8) -> Option<u32> {
    Some(match n {
        1 => DISP8,
        2 => DISP16,
        4 => DISP32,
        8 => DISP64,
        _ => return None,
    })
}
