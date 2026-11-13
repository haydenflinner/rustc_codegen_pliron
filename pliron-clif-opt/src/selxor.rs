//! `select (x & 1), !y, y` is `y ^ -(x & 1)` (default on;
//! `PLIRON_SELXOR=0` off).
//!
//! Shape: zigzag decoding, `let mut n = un >> 1; if un & 1 != 0 { n = !n }`.
//! If-converted it is a select between a value and its complement on
//! one bit of another value:
//!
//! ```text
//! b = band un, 1
//! c = icmp ne b, 0
//! n = ushr un, 1
//! r = select c, (bnot n), n          ; lsr; and; mvn; cmp; csel
//! ```
//!
//! Evidence: regex 1.7's `read_vari32` under `InstPtrs::next`
//! (`dfa.rs:1855`, `tests/parity/instptrs.rs`), run once per NFA state
//! of every DFA state built: five instructions where LLVM has two
//! (`sbfx; eor ..., lsr #1`).
//!
//! Rule: when the condition is bit 0 of a value of the select's own
//! type (directly, or through `icmp ne _, 0`), the select becomes
//! `bxor y, (ineg b)`: all-ones exactly when the bit is set. Three
//! instructions on aarch64 (`and; neg; eor`). Any other condition
//! stays a select — `cmp; csetm; eor` would be no shorter.
//!
//! The encoding direction tests the sign instead of a bit,
//! `let mut un = (n as u32) << 1; if n < 0 { un = !un }` (regex's
//! `write_vari32`, once per instruction pointer in `Fsm::cached_state`,
//! `tests/parity/statekey.rs`): `select (icmp slt n, 0), !y, y` is
//! `y ^ (n >> bits-1)` with an arithmetic shift, the mask all-ones
//! exactly when `n` is negative. `lsl; mvn; cmp; csel` becomes
//! `lsl; eor ..., asr #31`, LLVM's two instructions
//! (`PLIRON_SELXOR_SIGN=0` leaves this form alone).

use cranelift_codegen::cursor::{Cursor, FuncCursor};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{Function, Inst, InstBuilder, InstructionData, Opcode, Value};

fn def(func: &Function, v: Value) -> Option<Inst> {
    func.dfg.value_def(func.dfg.resolve_aliases(v)).inst()
}

fn iconst(func: &Function, v: Value) -> Option<i64> {
    match func.dfg.insts[def(func, v)?] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => Some(imm.bits()),
        _ => None,
    }
}

/// `v` as `band x, 1`: returns `v` itself (the 0/1 value).
fn low_bit(func: &Function, v: Value) -> Option<Value> {
    let v = func.dfg.resolve_aliases(v);
    match func.dfg.insts[def(func, v)?] {
        InstructionData::Binary { opcode: Opcode::Band, args }
            if iconst(func, args[1]) == Some(1) || iconst(func, args[0]) == Some(1) =>
        {
            Some(v)
        }
        _ => None,
    }
}

/// The 0/1 bit a select condition tests, if that is all it tests.
fn cond_bit(func: &Function, c: Value) -> Option<Value> {
    if let Some(b) = low_bit(func, c) {
        return Some(b);
    }
    match func.dfg.insts[def(func, c)?] {
        InstructionData::IntCompare { cond: IntCC::NotEqual, args, .. } => {
            if iconst(func, args[1]) == Some(0) {
                low_bit(func, args[0])
            } else if iconst(func, args[0]) == Some(0) {
                low_bit(func, args[1])
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `c` is `icmp slt n, 0`: returns `n`.
fn sign_test(func: &Function, c: Value) -> Option<Value> {
    match func.dfg.insts[def(func, c)?] {
        InstructionData::IntCompare { cond: IntCC::SignedLessThan, args, .. } if iconst(func, args[1]) == Some(0) => {
            Some(func.dfg.resolve_aliases(args[0]))
        }
        InstructionData::IntCompare { cond: IntCC::SignedGreaterThan, args, .. } if iconst(func, args[0]) == Some(0) => {
            Some(func.dfg.resolve_aliases(args[1]))
        }
        _ => None,
    }
}

/// Returns the number of selects rewritten.
pub fn run(func: &mut Function) -> usize {
    let sign = !pliron_cfg::env_is("PLIRON_SELXOR_SIGN", "0");
    // (select, y, the condition's value, it is a sign test)
    let mut todo: Vec<(Inst, Value, Value, bool)> = Vec::new();
    for b in func.layout.blocks() {
        for i in func.layout.block_insts(b) {
            let InstructionData::Ternary { opcode: Opcode::Select, args } = func.dfg.insts[i] else {
                continue;
            };
            let ty = func.dfg.value_type(args[1]);
            if !ty.is_int() || ty.is_vector() || ty.bits() > 64 {
                continue;
            }
            let (t, e) = (func.dfg.resolve_aliases(args[1]), func.dfg.resolve_aliases(args[2]));
            // `!y` on the taken arm, `y` on the other.
            let Some(ti) = def(func, t) else { continue };
            let InstructionData::Unary { opcode: Opcode::Bnot, arg } = func.dfg.insts[ti] else {
                continue;
            };
            if func.dfg.resolve_aliases(arg) != e {
                continue;
            }
            if let Some(bit) = cond_bit(func, args[0]) {
                if func.dfg.value_type(bit) == ty {
                    todo.push((i, e, bit, false));
                }
            } else if sign
                && let Some(v) = sign_test(func, args[0])
                && func.dfg.value_type(v) == ty
            {
                todo.push((i, e, v, true));
            }
        }
    }
    let mut n = 0;
    for (i, y, v, is_sign) in todo {
        if !pliron_cfg::bisect("selxor") {
            continue;
        }
        let mut pos = FuncCursor::new(func).at_inst(i);
        let mask = if is_sign {
            let bits = pos.func.dfg.value_type(v).bits() as i64;
            pos.ins().sshr_imm(v, bits - 1)
        } else {
            pos.ins().ineg(v)
        };
        pos.func.replace(i).bxor(y, mask);
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_select_becomes_xor() {
        let mut f = cranelift_reader::parse_functions(
            "function %f(i32) -> i32 {
block0(v0: i32):
    v1 = iconst.i32 1
    v2 = band v0, v1
    v3 = iconst.i32 0
    v4 = icmp ne v2, v3
    v5 = ushr v0, v1
    v6 = bnot v5
    v7 = select v4, v6, v5
    return v7
}",
        )
        .unwrap()
        .remove(0);
        assert_eq!(run(&mut f), 1);
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        cranelift_codegen::verify_function(&f, &flags).unwrap();
        let text = f.display().to_string();
        assert!(text.contains("ineg") && text.contains("bxor") && !text.contains("select"), "{text}");
        // The arms the other way round are a different function.
        let mut g = cranelift_reader::parse_functions(
            "function %f(i32) -> i32 {
block0(v0: i32):
    v1 = iconst.i32 1
    v2 = band v0, v1
    v5 = ushr v0, v1
    v6 = bnot v5
    v7 = select v2, v5, v6
    return v7
}",
        )
        .unwrap()
        .remove(0);
        assert_eq!(run(&mut g), 0);
    }

    #[test]
    fn zigzag_encode_select_becomes_xor_of_the_sign() {
        let src = |cmp: &str, arms: &str| {
            format!(
                "function %f(i32) -> i32 {{
block0(v0: i32):
    v1 = iconst.i32 1
    v2 = ishl v0, v1
    v3 = iconst.i32 0
    v4 = icmp {cmp}
    v6 = bnot v2
    v7 = select v4, {arms}
    return v7
}}"
            )
        };
        let flags = cranelift_codegen::settings::Flags::new(cranelift_codegen::settings::builder());
        for cmp in ["slt v0, v3", "sgt v3, v0"] {
            let before = cranelift_reader::parse_functions(&src(cmp, "v6, v2")).unwrap().remove(0);
            let mut f = before.clone();
            assert_eq!(run(&mut f), 1, "{cmp}");
            cranelift_codegen::verify_function(&f, &flags).unwrap();
            let text = f.display().to_string();
            assert!(text.contains("sshr") && text.contains("bxor") && !text.contains("select"), "{text}");
            let cfg = crate::diffcheck::Config { trials: 256, ..Default::default() };
            let s = crate::diffcheck::check(&before, &f, &crate::diffcheck::Facts::default(), &cfg)
                .unwrap_or_else(|m| panic!("{m}\n{text}"));
            assert!(s.compared > 0);
        }
        // Other compares, the arms the other way round, and a test of a
        // value of another width stay selects.
        for (cmp, arms) in [("sle v0, v3", "v6, v2"), ("slt v0, v3", "v2, v6"), ("ult v0, v3", "v6, v2")] {
            let mut g = cranelift_reader::parse_functions(&src(cmp, arms)).unwrap().remove(0);
            assert_eq!(run(&mut g), 0, "{cmp} {arms}");
        }
        let mut h = cranelift_reader::parse_functions(
            "function %f(i64, i32) -> i32 {
block0(v0: i64, v2: i32):
    v3 = iconst.i64 0
    v4 = icmp slt v0, v3
    v6 = bnot v2
    v7 = select v4, v6, v2
    return v7
}",
        )
        .unwrap()
        .remove(0);
        assert_eq!(run(&mut h), 0);
    }
}
