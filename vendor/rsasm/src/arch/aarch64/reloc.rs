//! ELF relocation types for AArch64 (`R_AARCH64_*`).

pub const ABS64: u32 = 257;
pub const ABS32: u32 = 258;
pub const ABS16: u32 = 259;
pub const PREL64: u32 = 260;
pub const PREL32: u32 = 261;
pub const PREL16: u32 = 262;
pub const LD_PREL_LO19: u32 = 273;
pub const ADR_PREL_LO21: u32 = 274;
pub const ADR_PREL_PG_HI21: u32 = 275;
pub const ADD_ABS_LO12_NC: u32 = 277;
pub const LDST8_ABS_LO12_NC: u32 = 278;
pub const TSTBR14: u32 = 279;
pub const CONDBR19: u32 = 280;
pub const JUMP26: u32 = 282;
pub const CALL26: u32 = 283;
pub const LDST16_ABS_LO12_NC: u32 = 284;
pub const LDST32_ABS_LO12_NC: u32 = 285;
pub const LDST64_ABS_LO12_NC: u32 = 286;
pub const LDST128_ABS_LO12_NC: u32 = 299;
pub const ADR_GOT_PAGE: u32 = 311;
pub const LD64_GOT_LO12_NC: u32 = 312;

/// The absolute relocation for an `n`-byte data field.
pub fn abs(n: u8) -> Option<u32> {
    Some(match n {
        2 => ABS16,
        4 => ABS32,
        8 => ABS64,
        _ => return None,
    })
}

/// The PC-relative relocation for an `n`-byte data field.
pub fn pcrel(n: u8) -> Option<u32> {
    Some(match n {
        2 => PREL16,
        4 => PREL32,
        8 => PREL64,
        _ => return None,
    })
}
