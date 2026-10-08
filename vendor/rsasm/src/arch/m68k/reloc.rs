//! ELF relocation numbers for `EM_68K`, as `m68k-elf-objdump -r` names them.

pub const R_68K_32: u32 = 1;
pub const R_68K_16: u32 = 2;
pub const R_68K_8: u32 = 3;
pub const R_68K_PC32: u32 = 4;
pub const R_68K_PC16: u32 = 5;
pub const R_68K_PC8: u32 = 6;

/// The relocation for a `size`-byte reference.
pub fn data(size: u8, pcrel: bool) -> Option<u32> {
    Some(match (size, pcrel) {
        (4, false) => R_68K_32,
        (2, false) => R_68K_16,
        (1, false) => R_68K_8,
        (4, true) => R_68K_PC32,
        (2, true) => R_68K_PC16,
        (1, true) => R_68K_PC8,
        _ => return None,
    })
}
