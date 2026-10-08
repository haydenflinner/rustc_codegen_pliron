//! The MIPS instruction table.
//!
//! Every instruction is one 32-bit word, so a definition is just the fixed
//! bits (opcode, function code, and any register field the opcode nails down)
//! plus a [`Form`] saying where the operands go. The encoder ORs the operand
//! fields into the fixed word; the two never overlap.
//!
//! Field positions, once, for the whole file:
//!
//! ```text
//!  31..26  25..21  20..16  15..11  10..6   5..0
//!   op      rs      rt      rd      sa     funct     R-format
//!   op      rs      rt      immediate(15..0)         I-format
//!   op      target(25..0)                            J-format
//! ```

/// Where an instruction's operands land in the word.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Form {
    /// `op rd, rs, rt` — the R-format arithmetic order, destination first.
    RdRsRt,
    /// `op rd, rt, sa` — constant shifts. The value shifted is `rt`, not `rs`.
    RdRtSa,
    /// `op rd, rt, rs` — variable shifts. The shift *count* is `rs`, and it is
    /// written last, so the register order differs from `RdRsRt`.
    RdRtRs,
    /// `op rs, rt` — `mult` and the multiply-accumulate group.
    RsRt,
    /// `op rs, rt` / `op $zero, rs, rt` — the divides. The three-operand
    /// spelling names the destination field, which a divide architecturally
    /// ignores because its results go to HI and LO.
    Div,
    /// `op rs, rt` / `op rs, rt, code` — conditional traps, whose optional
    /// 10-bit code sits in bits 15..6 for the exception handler to read.
    Trap,
    /// `op rd` — `mfhi` / `mflo`.
    Rd,
    /// `op rs` — `mthi` / `mtlo` / `jr`.
    Rs,
    /// `op rd, rs` — `jalr`, whose one-operand form links through `$ra`.
    Jalr,
    /// `op rt, rs, imm16`.
    RtRsImm,
    /// `op rt, imm16` — `lui`.
    RtImm,
    /// `op rt, off(base)` — integer loads and stores.
    RtMem,
    /// `op ft, off(base)` — FPU loads and stores.
    FtMem,
    /// `op rs, rt, target` — `beq` / `bne`.
    RsRtOff,
    /// `op rs, target` — the `rt` field is part of the opcode.
    RsOff,
    /// `op target` — `b` / `bal` / `bc1f` / `bc1t`: every register field fixed.
    Off,
    /// `op target` — `j` / `jal`, a 26-bit index into the current 256 MB
    /// region.
    Off26,
    /// No operands.
    Nullary,
    /// `op` / `op code` / `op code1, code2` — `break`.
    Break,
    /// `op` / `op code` — `syscall`, a single 20-bit field at bits 25..6.
    Code20,
    /// `op` / `op stype` — `sync`, a 5-bit field at bits 10..6.
    Sync,
    /// `op fd, fs, ft` — FPU arithmetic.
    FdFsFt,
    /// `op fd, fs` — FPU unary operations and conversions.
    FdFs,
    /// `op fs, ft` — `c.cond.fmt`, whose result goes to a condition flag.
    FsFt,
    /// `op rt, fs` — `mfc1` / `mtc1` and their doubleword forms.
    RtFs,
    /// `op rt, rd` / `op rt, rd, sel` — coprocessor-0 moves.
    RtRdSel,
}

#[derive(Copy, Clone, Debug)]
pub struct Def {
    pub name: &'static str,
    pub form: Form,
    /// The bits the mnemonic itself fixes.
    pub word: u32,
    /// True for instructions that only exist on 64-bit implementations.
    pub is64: bool,
}

const fn d(name: &'static str, form: Form, word: u32) -> Def {
    Def {
        name,
        form,
        word,
        is64: false,
    }
}

/// A MIPS III / MIPS64 instruction, rejected in 32-bit mode.
const fn d64(name: &'static str, form: Form, word: u32) -> Def {
    Def {
        name,
        form,
        word,
        is64: true,
    }
}

const fn op(o: u32) -> u32 {
    o << 26
}

/// SPECIAL2, which holds the MIPS32 multiply-accumulate group.
const SPECIAL2: u32 = op(0x1c);
/// REGIMM, where the `rt` field selects the instruction.
const REGIMM: u32 = op(0x01);
const COP0: u32 = op(0x10);
const COP1: u32 = op(0x11);

/// FPU format codes, occupying the `rs` field of a COP1 instruction.
const FMT_S: u32 = 16;
const FMT_D: u32 = 17;
const FMT_W: u32 = 20;
const FMT_L: u32 = 21;

const fn cop1(fmt: u32, funct: u32) -> u32 {
    COP1 | (fmt << 21) | funct
}

#[rustfmt::skip]
static TABLE: &[Def] = &[
    // ---- SPECIAL: three-register arithmetic and logic --------------------
    d("add",    Form::RdRsRt, 0x20),
    d("addu",   Form::RdRsRt, 0x21),
    d("sub",    Form::RdRsRt, 0x22),
    d("subu",   Form::RdRsRt, 0x23),
    d("and",    Form::RdRsRt, 0x24),
    d("or",     Form::RdRsRt, 0x25),
    d("xor",    Form::RdRsRt, 0x26),
    d("nor",    Form::RdRsRt, 0x27),
    d("slt",    Form::RdRsRt, 0x2a),
    d("sltu",   Form::RdRsRt, 0x2b),
    d("movz",   Form::RdRsRt, 0x0a),
    d("movn",   Form::RdRsRt, 0x0b),
    d64("dadd",  Form::RdRsRt, 0x2c),
    d64("daddu", Form::RdRsRt, 0x2d),
    d64("dsub",  Form::RdRsRt, 0x2e),
    d64("dsubu", Form::RdRsRt, 0x2f),

    // ---- SPECIAL: shifts --------------------------------------------------
    d("sll",    Form::RdRtSa, 0x00),
    d("srl",    Form::RdRtSa, 0x02),
    d("sra",    Form::RdRtSa, 0x03),
    d("sllv",   Form::RdRtRs, 0x04),
    d("srlv",   Form::RdRtRs, 0x06),
    d("srav",   Form::RdRtRs, 0x07),
    // The doubleword shifts split their 6-bit range across two mnemonics:
    // `dsll` covers 0-31 and `dsll32` adds 32 to whatever is written.
    d64("dsll",   Form::RdRtSa, 0x38),
    d64("dsrl",   Form::RdRtSa, 0x3a),
    d64("dsra",   Form::RdRtSa, 0x3b),
    d64("dsll32", Form::RdRtSa, 0x3c),
    d64("dsrl32", Form::RdRtSa, 0x3e),
    d64("dsra32", Form::RdRtSa, 0x3f),
    d64("dsllv",  Form::RdRtRs, 0x14),
    d64("dsrlv",  Form::RdRtRs, 0x16),
    d64("dsrav",  Form::RdRtRs, 0x17),

    // ---- SPECIAL: multiply, divide and the HI/LO pair --------------------
    d("mult",   Form::RsRt, 0x18),
    d("multu",  Form::RsRt, 0x19),
    d("div",    Form::Div, 0x1a),
    d("divu",   Form::Div, 0x1b),
    d64("dmult",  Form::RsRt, 0x1c),
    d64("dmultu", Form::RsRt, 0x1d),
    d64("ddiv",   Form::Div, 0x1e),
    d64("ddivu",  Form::Div, 0x1f),
    d("mfhi",   Form::Rd, 0x10),
    d("mthi",   Form::Rs, 0x11),
    d("mflo",   Form::Rd, 0x12),
    d("mtlo",   Form::Rs, 0x13),

    // ---- SPECIAL2 ---------------------------------------------------------
    d("mul",    Form::RdRsRt, SPECIAL2 | 0x02),
    d("madd",   Form::RsRt,   SPECIAL2), // function code 0
    d("maddu",  Form::RsRt,   SPECIAL2 | 0x01),
    d("msub",   Form::RsRt,   SPECIAL2 | 0x04),
    d("msubu",  Form::RsRt,   SPECIAL2 | 0x05),

    // ---- SPECIAL: jumps through a register --------------------------------
    d("jr",     Form::Rs,   0x08),
    d("jalr",   Form::Jalr, 0x09),

    // ---- SPECIAL: traps and system ----------------------------------------
    d("tge",    Form::Trap, 0x30),
    d("tgeu",   Form::Trap, 0x31),
    d("tlt",    Form::Trap, 0x32),
    d("tltu",   Form::Trap, 0x33),
    d("teq",    Form::Trap, 0x34),
    d("tne",    Form::Trap, 0x36),
    d("syscall", Form::Code20, 0x0c),
    d("break",  Form::Break, 0x0d),
    d("sync",   Form::Sync,  0x0f),
    // MIPS `nop` is architecturally `sll $zero, $zero, 0`, which is the
    // all-zero word. That is also why zero fill is valid instruction padding.
    d("nop",    Form::Nullary, 0x0000_0000),
    d("ssnop",  Form::Nullary, 0x0000_0040),
    d("eret",   Form::Nullary, COP0 | (1 << 25) | 0x18),

    // ---- I-format arithmetic ----------------------------------------------
    d("addi",   Form::RtRsImm, op(0x08)),
    d("addiu",  Form::RtRsImm, op(0x09)),
    d("slti",   Form::RtRsImm, op(0x0a)),
    d("sltiu",  Form::RtRsImm, op(0x0b)),
    d("andi",   Form::RtRsImm, op(0x0c)),
    d("ori",    Form::RtRsImm, op(0x0d)),
    d("xori",   Form::RtRsImm, op(0x0e)),
    d("lui",    Form::RtImm,   op(0x0f)),
    d64("daddi",  Form::RtRsImm, op(0x18)),
    d64("daddiu", Form::RtRsImm, op(0x19)),

    // ---- loads and stores -------------------------------------------------
    d("lb",     Form::RtMem, op(0x20)),
    d("lh",     Form::RtMem, op(0x21)),
    d("lwl",    Form::RtMem, op(0x22)),
    d("lw",     Form::RtMem, op(0x23)),
    d("lbu",    Form::RtMem, op(0x24)),
    d("lhu",    Form::RtMem, op(0x25)),
    d("lwr",    Form::RtMem, op(0x26)),
    d("sb",     Form::RtMem, op(0x28)),
    d("sh",     Form::RtMem, op(0x29)),
    d("swl",    Form::RtMem, op(0x2a)),
    d("sw",     Form::RtMem, op(0x2b)),
    d("swr",    Form::RtMem, op(0x2e)),
    d("ll",     Form::RtMem, op(0x30)),
    d("sc",     Form::RtMem, op(0x38)),
    d64("lwu",  Form::RtMem, op(0x27)),
    d64("ld",   Form::RtMem, op(0x37)),
    d64("sd",   Form::RtMem, op(0x3f)),
    d64("ldl",  Form::RtMem, op(0x1a)),
    d64("ldr",  Form::RtMem, op(0x1b)),
    d64("sdl",  Form::RtMem, op(0x2c)),
    d64("sdr",  Form::RtMem, op(0x2d)),
    d("lwc1",   Form::FtMem, op(0x31)),
    d("ldc1",   Form::FtMem, op(0x35)),
    d("swc1",   Form::FtMem, op(0x39)),
    d("sdc1",   Form::FtMem, op(0x3d)),

    // ---- branches and jumps -----------------------------------------------
    d("beq",    Form::RsRtOff, op(0x04)),
    d("bne",    Form::RsRtOff, op(0x05)),
    d("blez",   Form::RsOff,   op(0x06)),
    d("bgtz",   Form::RsOff,   op(0x07)),
    // REGIMM selects on `rt`, so `bltz` is the one whose selector is zero.
    d("bltz",   Form::RsOff,   REGIMM),
    d("bgez",   Form::RsOff,   REGIMM | (0x01 << 16)),
    d("bltzal", Form::RsOff,   REGIMM | (0x10 << 16)),
    d("bgezal", Form::RsOff,   REGIMM | (0x11 << 16)),
    d("j",      Form::Off26,   op(0x02)),
    d("jal",    Form::Off26,   op(0x03)),

    // ---- coprocessor 0 ----------------------------------------------------
    // The COP0 `rs` field selects the direction: 0 moves from, 4 moves to.
    d("mfc0",   Form::RtRdSel, COP0),
    d("mtc0",   Form::RtRdSel, COP0 | (4 << 21)),

    // ---- coprocessor 1 moves and branches ---------------------------------
    // Same layout on COP1, plus 1 and 5 for the doubleword pair.
    d("mfc1",   Form::RtFs, COP1),
    d("mtc1",   Form::RtFs, COP1 | (4 << 21)),
    d64("dmfc1", Form::RtFs, COP1 | (1 << 21)),
    d64("dmtc1", Form::RtFs, COP1 | (5 << 21)),
    // The `nd`/`tf` bits live in the rt field: 0 tests false, 1 tests true.
    d("bc1f",   Form::Off, COP1 | (8 << 21)),
    d("bc1t",   Form::Off, COP1 | (8 << 21) | (1 << 16)),
];

/// COP1 operations that exist for both `.s` and `.d`, with their function
/// codes. Spelled out here rather than in `TABLE` so the format suffix is
/// handled in one place.
#[rustfmt::skip]
static FP_BINARY: &[(&str, u32)] = &[
    ("add", 0x00), ("sub", 0x01), ("mul", 0x02), ("div", 0x03),
];

#[rustfmt::skip]
static FP_UNARY: &[(&str, u32)] = &[
    ("sqrt", 0x04), ("abs", 0x05), ("mov", 0x06), ("neg", 0x07),
    ("round.l", 0x08), ("trunc.l", 0x09), ("ceil.l", 0x0a), ("floor.l", 0x0b),
    ("round.w", 0x0c), ("trunc.w", 0x0d), ("ceil.w", 0x0e), ("floor.w", 0x0f),
];

/// `cvt.<to>.<from>`: the destination format is part of the function code and
/// the source format goes in the `fmt` field.
#[rustfmt::skip]
static FP_CVT: &[(&str, u32)] = &[
    ("s", 0x20), ("d", 0x21), ("w", 0x24), ("l", 0x25),
];

/// The 16 `c.cond.fmt` predicates, in function-code order from 0x30.
#[rustfmt::skip]
static FP_COND: &[&str] = &[
    "f",   "un",  "eq",  "ueq", "olt", "ult", "ole", "ule",
    "sf",  "ngle", "seq", "ngl", "lt",  "nge", "le",  "ngt",
];

fn fmt_code(suffix: &str) -> Option<u32> {
    Some(match suffix {
        "s" => FMT_S,
        "d" => FMT_D,
        "w" => FMT_W,
        "l" => FMT_L,
        _ => return None,
    })
}

/// Looks up a mnemonic, including the FPU forms whose format suffix is part of
/// the name.
pub fn lookup(name: &str) -> Option<Def> {
    if let Some(def) = TABLE.iter().find(|d| d.name == name) {
        return Some(*def);
    }
    fpu(name)
}

/// Resolves `add.s`, `cvt.d.w`, `c.eq.s` and friends.
fn fpu(name: &str) -> Option<Def> {
    let (base, suffix) = name.rsplit_once('.')?;
    let fmt = fmt_code(suffix)?;

    if let Some(cond) = base.strip_prefix("c.") {
        let idx = FP_COND.iter().position(|c| *c == cond)?;
        // `.w` and `.l` are integer formats; only `.s` and `.d` can be
        // compared.
        if fmt != FMT_S && fmt != FMT_D {
            return None;
        }
        return Some(Def {
            name: "c.cond.fmt",
            form: Form::FsFt,
            word: cop1(fmt, 0x30 + idx as u32),
            is64: false,
        });
    }

    if let Some(to) = base.strip_prefix("cvt.")
        && let Some((_, funct)) = FP_CVT.iter().find(|(n, _)| *n == to)
    {
        return Some(Def {
            name: "cvt.fmt.fmt",
            form: Form::FdFs,
            // `.l` results need a 64-bit FPU; the 32-bit backends reject them.
            is64: to == "l",
            word: cop1(fmt, *funct),
        });
    }

    if let Some((_, funct)) = FP_BINARY.iter().find(|(n, _)| *n == base) {
        if fmt != FMT_S && fmt != FMT_D {
            return None;
        }
        return Some(Def {
            name: "fp.binary",
            form: Form::FdFsFt,
            word: cop1(fmt, *funct),
            is64: false,
        });
    }

    if let Some((_, funct)) = FP_UNARY.iter().find(|(n, _)| *n == base) {
        if fmt != FMT_S && fmt != FMT_D {
            return None;
        }
        return Some(Def {
            name: "fp.unary",
            form: Form::FdFs,
            word: cop1(fmt, *funct),
            // The `.l` rounding forms produce a 64-bit integer.
            is64: base.ends_with(".l"),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bits the encoder ORs in for each form. Used to prove no definition
    /// puts fixed bits where an operand will land.
    fn operand_mask(form: Form) -> u32 {
        match form {
            Form::RdRsRt | Form::RdRtRs | Form::Jalr => 0x03ff_f800,
            Form::RdRtSa | Form::FdFsFt => 0x001f_ffc0,
            Form::RsRt | Form::Div => 0x03ff_0000,
            Form::Trap => 0x03ff_ffc0,
            Form::Rd => 0x0000_f800,
            Form::Rs => 0x03e0_0000,
            Form::RtRsImm | Form::RtMem | Form::FtMem | Form::RsRtOff | Form::Off26 => 0x03ff_ffff,
            Form::RtImm => 0x001f_ffff,
            Form::RsOff => 0x03e0_ffff,
            Form::Off => 0x0000_ffff,
            Form::Nullary => 0,
            Form::Break | Form::Code20 => 0x03ff_ffc0,
            Form::Sync => 0x0000_07c0,
            Form::FdFs => 0x0000_ffc0,
            Form::FsFt | Form::RtFs => 0x001f_f800,
            Form::RtRdSel => 0x001f_f807,
        }
    }

    #[test]
    fn fixed_bits_never_land_in_an_operand_field() {
        for def in TABLE {
            assert_eq!(
                def.word & operand_mask(def.form),
                0,
                "`{}` fixes bits the encoder will overwrite",
                def.name
            );
        }
    }

    #[test]
    fn mnemonics_are_unique() {
        let mut names: Vec<&str> = TABLE.iter().map(|d| d.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate mnemonic in the table");
    }

    #[test]
    fn fpu_suffixes_resolve() {
        assert_eq!(lookup("add.s").expect("add.s").word, 0x4600_0000);
        assert_eq!(lookup("add.d").expect("add.d").word, 0x4620_0000);
        assert_eq!(lookup("cvt.s.w").expect("cvt.s.w").word, 0x4680_0020);
        assert_eq!(lookup("c.eq.s").expect("c.eq.s").word, 0x4600_0032);
        assert!(lookup("add.q").is_none());
        assert!(lookup("c.bogus.s").is_none());
    }

    #[test]
    fn unknown_mnemonics_are_rejected() {
        assert!(lookup("addx").is_none());
        assert!(lookup("").is_none());
        assert!(lookup(".").is_none());
    }
}
