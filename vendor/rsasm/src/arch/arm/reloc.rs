//! `R_ARM_*` relocation numbers, from the ARM ELF ABI.

pub const ABS32: u32 = 2;
pub const REL32: u32 = 3;
pub const ABS16: u32 = 5;
pub const ABS8: u32 = 8;
pub const PREL31: u32 = 42;
pub const THM_CALL: u32 = 10;
pub const CALL: u32 = 28;
pub const JUMP24: u32 = 29;
pub const THM_JUMP24: u32 = 30;
pub const THM_JUMP19: u32 = 51;

/// Relocation for a data reference of `size` bytes.
pub fn data(size: u8, pcrel: bool) -> Option<u32> {
    Some(match (size, pcrel) {
        (4, false) => ABS32,
        (4, true) => REL32,
        (2, false) => ABS16,
        (1, false) => ABS8,
        _ => return None,
    })
}
