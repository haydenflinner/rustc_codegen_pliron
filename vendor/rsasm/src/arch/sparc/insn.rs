//! The SPARC instruction table.
//!
//! SPARC has exactly three instruction formats, and every entry here says
//! which one an mnemonic uses and what goes in its opcode fields. The formats
//! are described in [`super::encode`]; in short, bits 31-30 (`op`) pick
//! between them:
//!
//! ```text
//! op=1  format 1   call, and nothing else: a 30-bit displacement
//! op=0  format 2   sethi and the branches: `op2` plus a 22- or 19-bit field
//! op=2  format 3   arithmetic, logic, jumps, the window ops: `op3`
//! op=3  format 3   loads and stores, same shape, different `op3` space
//! ```

/// A load or store. `op3` lives in the `op=3` opcode space.
#[derive(Copy, Clone, Debug)]
pub struct MemForm {
    pub op3: u8,
    /// The `op3` to use when the data register is a float register. `ld` and
    /// `st` pick their opcode from the register class rather than the
    /// mnemonic, so `ld [%o0], %f1` is a different instruction from
    /// `ld [%o0], %g1`.
    pub fop3: Option<u8>,
    /// True when the data register comes first: `st %g1, [%o0]`.
    pub store: bool,
    /// The data register must be a float register (`ldf`, `stdf`, ...).
    pub float_only: bool,
}

#[derive(Copy, Clone, Debug)]
pub enum Form {
    /// `op=2` three-operand: `rs1, reg_or_imm, rd`.
    Alu(u8),
    /// A shift. `x` selects the V9 64-bit variant, which sets bit 12 and
    /// widens the count field from five bits to six.
    Shift {
        op3: u8,
        x: bool,
    },
    Mem(MemForm),
    /// A conditional branch. `predicted` forces the V9 `BPcc` form even
    /// without a `%icc` operand.
    Branch {
        cond: u8,
        predicted: bool,
    },
    /// V9 branch on the contents of a register (`BPr`).
    BranchReg(u8),
    Call,
    Sethi,
    Jmpl,
    /// `save` / `restore`: the register-window instructions.
    Window(u8),
    /// V9 `return`, spelled `rett` in V8.
    Return,
    /// `FPop1` with two sources: `frs1, frs2, frd`.
    FpBin(u16),
    /// `FPop1` with one source: `frs2, frd`.
    FpUn(u16),
    /// `FPop2` compare: `frs1, frs2`.
    FpCmp(u16),
    /// V9 `mov<cc> %icc, reg_or_imm, rd`.
    MovCc(u8),
    /// V9 `movr<cond> rs1, reg_or_imm, rd`.
    MovReg(u8),
    /// `t<cc> software_trap_number`.
    Trap(u8),
    /// `rd %asr, rd`.
    ReadAsr,
    /// `wr rs1, reg_or_imm, %asr`.
    WriteAsr,
    Flush,
    Unimp,
}

#[derive(Copy, Clone, Debug)]
pub struct Def {
    pub form: Form,
    /// Only exists on V9; refused with a clear message on a V8 target.
    pub v9: bool,
}

const fn v8(form: Form) -> Def {
    Def { form, v9: false }
}

const fn v9(form: Form) -> Def {
    Def { form, v9: true }
}

/// The four-bit integer condition field, shared by `Bicc`, `BPcc`, `MOVcc`
/// and `Tcc`. The bottom three bits name the test and bit 3 inverts it, which
/// is why `be` (1) and `bne` (9) differ by eight.
pub fn cond_code(name: &str) -> Option<u8> {
    Some(match name {
        "n" => 0,
        "e" | "eq" | "z" => 1,
        "le" => 2,
        "l" | "lt" => 3,
        "leu" => 4,
        "cs" | "lu" => 5,
        "neg" => 6,
        "vs" => 7,
        "a" => 8,
        "ne" | "nz" => 9,
        "g" | "gt" => 10,
        "ge" => 11,
        "gu" => 12,
        "cc" | "geu" => 13,
        "pos" => 14,
        "vc" => 15,
        _ => return None,
    })
}

/// The three-bit `rcond` field of `BPr` and `MOVr`, which tests a whole
/// register against zero rather than the condition codes.
fn rcond_code(name: &str) -> Option<u8> {
    Some(match name {
        "z" => 1,
        "lez" => 2,
        "lz" => 3,
        "nz" => 5,
        "gz" => 6,
        "gez" => 7,
        _ => return None,
    })
}

fn mem(op3: u8, store: bool) -> Def {
    v8(Form::Mem(MemForm {
        op3,
        fop3: None,
        store,
        float_only: false,
    }))
}

/// `ld`, `st`, `ldd` and `std`, whose opcode depends on the register class.
fn mem_either(op3: u8, fop3: u8, store: bool) -> Def {
    v8(Form::Mem(MemForm {
        op3,
        fop3: Some(fop3),
        store,
        float_only: false,
    }))
}

/// `ldf`, `stdf` and friends: GNU as spellings that pin the float form.
fn mem_float(op3: u8, store: bool) -> Def {
    v8(Form::Mem(MemForm {
        op3,
        fop3: Some(op3),
        store,
        float_only: true,
    }))
}

pub fn lookup(name: &str) -> Option<Def> {
    if let Some(d) = alu(name) {
        return Some(d);
    }
    if let Some(d) = memory(name) {
        return Some(d);
    }
    if let Some(d) = control(name) {
        return Some(d);
    }
    if let Some(d) = float(name) {
        return Some(d);
    }
    None
}

#[rustfmt::skip]
fn alu(name: &str) -> Option<Def> {
    use Form::*;
    Some(match name {
        // The `cc` suffix is bit 4 of op3: `add` 0x00, `addcc` 0x10.
        "add"    => v8(Alu(0x00)), "addcc"    => v8(Alu(0x10)),
        // V8 calls the carry-propagating forms `addx`/`subx`; V9 renamed them
        // `addc`/`subc`. Both spellings assemble to the same opcode.
        "addx" | "addc"     => v8(Alu(0x08)),
        "addxcc" | "addccc" => v8(Alu(0x18)),
        "sub"    => v8(Alu(0x04)), "subcc"    => v8(Alu(0x14)),
        "subx" | "subc"     => v8(Alu(0x0c)),
        "subxcc" | "subccc" => v8(Alu(0x1c)),
        "and"    => v8(Alu(0x01)), "andcc"    => v8(Alu(0x11)),
        "andn"   => v8(Alu(0x05)), "andncc"   => v8(Alu(0x15)),
        "or"     => v8(Alu(0x02)), "orcc"     => v8(Alu(0x12)),
        "orn"    => v8(Alu(0x06)), "orncc"    => v8(Alu(0x16)),
        "xor"    => v8(Alu(0x03)), "xorcc"    => v8(Alu(0x13)),
        "xnor"   => v8(Alu(0x07)), "xnorcc"   => v8(Alu(0x17)),
        "umul"   => v8(Alu(0x0a)), "umulcc"   => v8(Alu(0x1a)),
        "smul"   => v8(Alu(0x0b)), "smulcc"   => v8(Alu(0x1b)),
        "udiv"   => v8(Alu(0x0e)), "udivcc"   => v8(Alu(0x1e)),
        "sdiv"   => v8(Alu(0x0f)), "sdivcc"   => v8(Alu(0x1f)),
        "mulscc" => v8(Alu(0x24)),
        "taddcc" => v8(Alu(0x20)), "tsubcc"   => v8(Alu(0x21)),
        "mulx"   => v9(Alu(0x09)),
        "udivx"  => v9(Alu(0x0d)), "sdivx"    => v9(Alu(0x2d)),

        "sll"  => v8(Shift { op3: 0x25, x: false }),
        "srl"  => v8(Shift { op3: 0x26, x: false }),
        "sra"  => v8(Shift { op3: 0x27, x: false }),
        "sllx" => v9(Shift { op3: 0x25, x: true }),
        "srlx" => v9(Shift { op3: 0x26, x: true }),
        "srax" => v9(Shift { op3: 0x27, x: true }),

        // `save` allocates a fresh register window: the caller's `%o`
        // registers become the callee's `%i` registers and `%l` is fresh, so
        // a leaf's locals never touch memory. `restore` rotates it back. Both
        // also add their operands, which is how the stack pointer is bumped
        // in the same instruction: `save %sp, -96, %sp`.
        "save"    => v8(Window(0x3c)),
        "restore" => v8(Window(0x3d)),

        "rd" => v8(ReadAsr),
        "wr" => v8(WriteAsr),
        _ => return None,
    })
}

#[rustfmt::skip]
fn memory(name: &str) -> Option<Def> {
    Some(match name {
        "ld"   => mem_either(0x00, 0x20, false),
        "lduw" => mem_either(0x00, 0x20, false),
        "ldub" => mem(0x01, false),
        "lduh" => mem(0x02, false),
        "ldd"  => mem_either(0x03, 0x23, false),
        "ldsb" => mem(0x09, false),
        "ldsh" => mem(0x0a, false),
        "st" | "stw" => mem_either(0x04, 0x24, true),
        "stb"  => mem(0x05, true),
        "sth"  => mem(0x06, true),
        "std"  => mem_either(0x07, 0x27, true),
        "ldf"  => mem_float(0x20, false),
        "lddf" => mem_float(0x23, false),
        "stf"  => mem_float(0x24, true),
        "stdf" => mem_float(0x27, true),
        // V9 widened the integer registers to 64 bits and added the opcodes
        // that move all of them.
        "ldsw" => Def { v9: true, ..mem(0x08, false) },
        "ldx"  => Def { v9: true, ..mem(0x0b, false) },
        "stx"  => Def { v9: true, ..mem(0x0e, true) },
        _ => return None,
    })
}

fn control(name: &str) -> Option<Def> {
    use Form::*;
    match name {
        "call" => return Some(v8(Call)),
        "sethi" => return Some(v8(Sethi)),
        "jmpl" => return Some(v8(Jmpl)),
        "flush" => return Some(v8(Flush)),
        "unimp" | "illtrap" => return Some(v8(Unimp)),
        "rett" => return Some(v8(Return)),
        "return" => return Some(v9(Return)),
        _ => {}
    }
    // `b<cc>` and the V9 `bp<cc>`, `br<cond>`, `mov<cc>`, `movr<cond>` and
    // `t<cc>` families are all "opcode plus condition name", so they are
    // decoded from the mnemonic rather than listed one by one.
    if let Some(rest) = name.strip_prefix("movr")
        && let Some(rcond) = rcond_code(rest)
    {
        return Some(v9(MovReg(rcond)));
    }
    if let Some(rest) = name.strip_prefix("mov")
        && let Some(cond) = cond_code(rest)
    {
        return Some(v9(MovCc(cond)));
    }
    if let Some(rest) = name.strip_prefix("br")
        && let Some(rcond) = rcond_code(rest)
    {
        return Some(v9(BranchReg(rcond)));
    }
    if let Some(rest) = name.strip_prefix("bp")
        && let Some(cond) = cond_code(rest)
    {
        return Some(v9(Branch {
            cond,
            predicted: true,
        }));
    }
    if let Some(rest) = name.strip_prefix("b")
        && let Some(cond) = cond_code(rest)
    {
        return Some(v8(Branch {
            cond,
            predicted: false,
        }));
    }
    if let Some(rest) = name.strip_prefix("t")
        && let Some(cond) = cond_code(rest)
    {
        return Some(v8(Trap(cond)));
    }
    None
}

#[rustfmt::skip]
fn float(name: &str) -> Option<Def> {
    use Form::*;
    Some(match name {
        "fadds" => v8(FpBin(0x41)), "faddd" => v8(FpBin(0x42)), "faddq" => v8(FpBin(0x43)),
        "fsubs" => v8(FpBin(0x45)), "fsubd" => v8(FpBin(0x46)), "fsubq" => v8(FpBin(0x47)),
        "fmuls" => v8(FpBin(0x49)), "fmuld" => v8(FpBin(0x4a)), "fmulq" => v8(FpBin(0x4b)),
        "fdivs" => v8(FpBin(0x4d)), "fdivd" => v8(FpBin(0x4e)), "fdivq" => v8(FpBin(0x4f)),
        "fsmuld" => v8(FpBin(0x69)),

        "fmovs" => v8(FpUn(0x01)), "fnegs" => v8(FpUn(0x05)), "fabss" => v8(FpUn(0x09)),
        // The double and quad register-to-register moves only exist on V9;
        // on V8 they are written as two or four `fmovs`.
        "fmovd" => v9(FpUn(0x02)), "fnegd" => v9(FpUn(0x06)), "fabsd" => v9(FpUn(0x0a)),
        "fmovq" => v9(FpUn(0x03)), "fnegq" => v9(FpUn(0x07)), "fabsq" => v9(FpUn(0x0b)),

        "fsqrts" => v8(FpUn(0x29)), "fsqrtd" => v8(FpUn(0x2a)), "fsqrtq" => v8(FpUn(0x2b)),
        "fitos" => v8(FpUn(0xc4)), "fitod" => v8(FpUn(0xc8)), "fitoq" => v8(FpUn(0xcc)),
        "fstoi" => v8(FpUn(0xd1)), "fdtoi" => v8(FpUn(0xd2)), "fqtoi" => v8(FpUn(0xd3)),
        "fstod" => v8(FpUn(0xc9)), "fstoq" => v8(FpUn(0xcd)),
        "fdtos" => v8(FpUn(0xc6)), "fdtoq" => v8(FpUn(0xce)),
        "fqtos" => v8(FpUn(0xc7)), "fqtod" => v8(FpUn(0xcb)),

        "fcmps" => v8(FpCmp(0x51)), "fcmpd" => v8(FpCmp(0x52)), "fcmpq" => v8(FpCmp(0x53)),
        "fcmpes" => v8(FpCmp(0x55)), "fcmped" => v8(FpCmp(0x56)), "fcmpeq" => v8(FpCmp(0x57)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_suffixes_do_not_swallow_whole_mnemonics() {
        // `bpos` is "branch on positive", not the predicted branch `bp` with
        // an `os` condition; `movrz` is MOVr, not MOVcc with an `rz` cond.
        assert!(matches!(
            lookup("bpos"),
            Some(Def {
                form: Form::Branch {
                    cond: 14,
                    predicted: false
                },
                ..
            })
        ));
        assert!(matches!(
            lookup("movrz"),
            Some(Def {
                form: Form::MovReg(1),
                ..
            })
        ));
        assert!(matches!(
            lookup("bpe"),
            Some(Def {
                form: Form::Branch {
                    cond: 1,
                    predicted: true
                },
                ..
            })
        ));
    }

    #[test]
    fn unknown_mnemonics_are_not_invented() {
        for bad in ["", "b", "t", "mov", "movr", "br", "bp", "zzz", "ldq"] {
            assert!(lookup(bad).is_none(), "`{bad}` should not resolve");
        }
    }
}
