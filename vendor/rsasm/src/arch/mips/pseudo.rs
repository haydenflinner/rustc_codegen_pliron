//! Macro instructions.
//!
//! Most hand-written MIPS is made of these. They are not encodings the CPU
//! has: `move` is an `or` with `$zero`, `li` is one or two real instructions
//! depending on the constant, and the ordered branches are a `slt` followed by
//! a branch on the result. GNU as calls the multi-instruction ones *macros*
//! and lets `.set nomacro` warn about them; this backend always expands them,
//! because refusing would reject nearly every real source file.
//!
//! Expansions that need a scratch register use `$at`, exactly as GNU as and
//! llvm-mc do.

use super::encode::{Args, Words, branch_fixup, imm, place_imm16, rd, rs, rt};
use super::operand::{Imm, RelocMod};
use super::reg::{self, Reg};
use crate::arch::{AsmCtx, Endian};
use crate::section::Variant;

/// SPECIAL function codes the expansions build on.
const OR: u32 = 0x25;
const NOR: u32 = 0x27;
const SUB: u32 = 0x22;
const SUBU: u32 = 0x23;
const SLT: u32 = 0x2a;
const SLTU: u32 = 0x2b;

const ADDIU: u32 = 0x09 << 26;
const DADDIU: u32 = 0x19 << 26;
const ORI: u32 = 0x0d << 26;
const LUI: u32 = 0x0f << 26;
const BEQ: u32 = 0x04 << 26;
const BNE: u32 = 0x05 << 26;
const BLEZ: u32 = 0x06 << 26;
const BGTZ: u32 = 0x07 << 26;
const REGIMM: u32 = 0x01 << 26;
const BLTZ: u32 = REGIMM;
const BGEZ: u32 = REGIMM | (1 << 16);
const BGEZAL: u32 = REGIMM | (0x11 << 16);

/// True if `name` is a macro this module expands, so the caller can try it
/// before the instruction table.
pub fn is_pseudo(name: &str) -> bool {
    matches!(
        name,
        "move"
            | "not"
            | "neg"
            | "negu"
            | "li"
            | "la"
            | "b"
            | "bal"
            | "beqz"
            | "bnez"
            | "bge"
            | "bgt"
            | "ble"
            | "blt"
            | "bgeu"
            | "bgtu"
            | "bleu"
            | "bltu"
    )
}

pub fn expand(
    cx: &mut AsmCtx<'_>,
    name: &str,
    a: &Args<'_>,
    endian: Endian,
    is64: bool,
) -> Option<Variant> {
    let mut w = Words::new(endian);
    match name {
        // Three register-to-register aliases, each an arithmetic instruction
        // with `$zero` in one slot.
        "move" | "not" | "neg" | "negu" => {
            a.arity(cx, 2)?;
            let (d, s) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            w.push(match name {
                "move" => rd(d.num) | rs(s.num) | OR,
                "not" => rd(d.num) | rs(s.num) | NOR,
                // Negation subtracts *from* zero, so the source is `rt`.
                "neg" => rd(d.num) | rt(s.num) | SUB,
                _ => rd(d.num) | rt(s.num) | SUBU,
            });
        }

        "li" => {
            a.arity(cx, 2)?;
            let d = a.gpr(cx, 0)?;
            let v = a.imm(cx, 1)?;
            // `li` loads a word even on a 64-bit target, so its narrow form
            // stays `addiu`; `la` below is the one that widens.
            load_constant(cx, &mut w, d, v, ADDIU)?;
        }

        "la" => {
            a.arity(cx, 2)?;
            let d = a.gpr(cx, 0)?;
            let v = a.imm(cx, 1)?;
            // On a 64-bit target an address fills the whole register, so the
            // narrow form has to sign-extend through all 64 bits: `daddiu`,
            // not `addiu`.
            let narrow = if is64 { DADDIU } else { ADDIU };
            if cx.constant(v.expr).is_some() {
                // An address that is already a number is just a constant.
                load_constant(cx, &mut w, d, v, narrow)?;
            } else {
                if v.modifier != RelocMod::None {
                    cx.error(
                        v.span,
                        "`la` already splits its operand into %hi and %lo halves",
                    );
                    return None;
                }
                let hi = Imm {
                    modifier: RelocMod::Hi,
                    ..v
                };
                let lo = Imm {
                    modifier: RelocMod::Lo,
                    ..v
                };
                place_imm16(cx, &mut w, LUI | rt(d.num), hi, "address")?;
                place_imm16(cx, &mut w, narrow | rt(d.num) | rs(d.num), lo, "address")?;
            }
        }

        // Unconditional branches. `b` is `beq $zero, $zero`; `bal` is the
        // always-true `bgezal $zero`, which is why it links.
        "b" | "bal" => {
            a.arity(cx, 1)?;
            let target = a.imm(cx, 0)?;
            let word = if name == "b" { BEQ } else { BGEZAL };
            w.push_fixup(word, target.expr, branch_fixup(), target.span);
        }

        "beqz" | "bnez" => {
            a.arity(cx, 2)?;
            let s = a.gpr(cx, 0)?;
            let target = a.imm(cx, 1)?;
            let word = if name == "beqz" { BEQ } else { BNE };
            w.push_fixup(word | rs(s.num), target.expr, branch_fixup(), target.span);
        }

        "bge" | "bgt" | "ble" | "blt" | "bgeu" | "bgtu" | "bleu" | "bltu" => {
            a.arity(cx, 3)?;
            let (x, y) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            let target = a.imm(cx, 2)?;
            ordered_branch(&mut w, name, x, y, target);
        }

        _ => return None,
    }
    Some(w.finish())
}

/// `li` / `la` of a value that is already known.
///
/// The split follows GNU as and llvm-mc exactly: a value that fits a
/// sign-extended 16-bit field is one `addiu`, one that fits a zero-extended
/// one is one `ori`, a value with no low half is one `lui`, and only the rest
/// take two instructions. Note the *signed 32-bit* reading: `li $a0,
/// 0xffffffff` is `addiu $a0, $zero, -1`, not a two-instruction sequence.
fn load_constant(cx: &mut AsmCtx<'_>, w: &mut Words, dest: Reg, v: Imm, narrow: u32) -> Option<()> {
    let Some(n) = cx.constant(v.expr) else {
        cx.error(
            v.span,
            "`li` needs a value known at assembly time; use `la` for an address",
        );
        return None;
    };
    if !(-0x8000_0000..=0xffff_ffff).contains(&n) {
        cx.error(
            v.span,
            format!("{n} does not fit in 32 bits; MIPS `li` loads at most a word"),
        );
        return None;
    }
    let n32 = n as i32 as i64;
    let lo = n32 & 0xffff;
    let hi = (n32 >> 16) & 0xffff;
    if (-0x8000..=0x7fff).contains(&n32) {
        w.push(narrow | rt(dest.num) | imm(n32));
    } else if (0..=0xffff).contains(&n32) {
        w.push(ORI | rt(dest.num) | imm(n32));
    } else if lo == 0 {
        w.push(LUI | rt(dest.num) | imm(hi));
    } else {
        w.push(LUI | rt(dest.num) | imm(hi));
        w.push(ORI | rt(dest.num) | rs(dest.num) | imm(lo));
    }
    Some(())
}

/// `blt` and friends: compute the predicate into `$at`, then branch on it.
///
/// Comparing against `$zero` has a one-instruction form for the signed
/// orderings, since `bltz`/`bgez`/`bgtz`/`blez` already test a register's
/// sign. The unsigned orderings have no such shortcut — an unsigned value is
/// never less than zero — so they always go through `sltu`.
fn ordered_branch(w: &mut Words, name: &str, x: Reg, y: Reg, target: Imm) {
    let unsigned = name.ends_with('u');
    if !unsigned && y == reg::ZERO {
        let word = match name {
            "bge" => BGEZ,
            "blt" => BLTZ,
            "bgt" => BGTZ,
            _ => BLEZ,
        };
        w.push_fixup(word | rs(x.num), target.expr, branch_fixup(), target.span);
        return;
    }
    let set = if unsigned { SLTU } else { SLT };
    // `x < y` for the strict-less orderings, `y < x` for the others; then
    // branch when the flag is set or clear as the ordering requires.
    let (lhs, rhs, branch_if_set) = match name.trim_end_matches('u') {
        "blt" => (x, y, true),
        "bge" => (x, y, false),
        "bgt" => (y, x, true),
        _ => (y, x, false),
    };
    w.push(rd(reg::AT.num) | rs(lhs.num) | rt(rhs.num) | set);
    let branch = if branch_if_set { BNE } else { BEQ };
    w.push_fixup(
        branch | rs(reg::AT.num),
        target.expr,
        branch_fixup(),
        target.span,
    );
}
