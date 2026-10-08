//! Tests for the NEC/Renesas 78K0 backend, in CA78K0 syntax.
//!
//! There is no assembler to check against: CA78K0 is proprietary and Windows
//! only, and neither binutils nor LLVM knows the 78K0. So the tests are built
//! to make a mistake in the transcribed code table loud instead:
//!
//! 1. **Every row must parse.** `form::parse_row` rejects a row whose operand
//!    column and code columns disagree, and `every_row_of_the_code_table_parses`
//!    requires all 227 to be accepted.
//! 2. **A second document fixes the set and the lengths.** `KX2_OPERATION_LIST`
//!    is the operation list of a *different* Renesas manual, with its own byte
//!    counts; the table must contain exactly those instruction forms, each of
//!    exactly that length.
//! 3. **The whole opcode space is walked.** Every form is expanded over every
//!    register, bit and bank it can take, and no two expansions may decode
//!    alike — except the aliases the manual itself documents (`PSW` is the
//!    short direct address FF1EH, `SP` is FF1CH, `EI` is `SET1 PSW.7`), which
//!    are required to be exact. The first bytes left unassigned are pinned.
//! 4. **The front end is walked too.** Every expansion is written out as
//!    CA78K0 source, in each spelling of its register names, assembled, and
//!    compared with the table's bytes; the `addr11`, `addr5` and short direct
//!    fields are swept over their whole ranges, both as constants and as
//!    forward references.
//!
//! During development all 782 bindings were also disassembled by an
//! independent decoder: MAME's `src/devices/cpu/upd78k/upd78k0d.cpp`, built
//! locally outside the repository (the corpus comes from
//! `dump_corpus_for_an_external_disassembler` below). 764 agree on mnemonic,
//! operands and length. The other 18 are MAME bugs, each contradicted by both
//! NEC documents: it decodes the sixteen `ADD`..`CMP A,[HL+B]` and
//! `A,[HL+C]` forms on the `31H` page as undefined (its test for them sits
//! inside a branch that excludes them), and gives `RETI` and `RETB` two bytes
//! rather than one. MAME agrees with the table on the six unassigned first
//! bytes, and on which second bytes of the `31H`, `61H` and `71H` pages are
//! defined, apart from those same sixteen.

#![cfg(feature = "k78")]

mod common;

use common::*;
use rsasm::arch::k78::form::{Code, Field, Form, Slot, Var, forms, parse_row};
use rsasm::arch::k78::table::ROWS;
use rsasm::lexer::Dialect::Renesas;
use rsasm::section::SectionId;
use std::collections::{BTreeMap, BTreeSet};

// ---- helpers ----------------------------------------------------------------

fn assemble(src: &str) -> Result<Vec<u8>, String> {
    let asm = assemble_flat_dialect("78k0", Renesas, src, 0);
    if asm.diags.has_errors() {
        return Err(asm.diags.render(&asm.sm, false));
    }
    Ok(asm.section_bytes(SectionId(0)))
}

#[track_caller]
fn bytes(src: &str) -> Vec<u8> {
    match assemble(src) {
        Ok(b) => b,
        Err(e) => panic!("assembly failed:\n{e}\nsource:\n{src}"),
    }
}

#[track_caller]
fn enc(src: &str, want: &str) {
    let got = hex(&bytes(src));
    assert_eq!(
        got, want,
        "\n  source: {src}\n  want: {want}\n   got: {got}"
    );
}

#[track_caller]
fn error(src: &str) -> String {
    match assemble(src) {
        Ok(b) => panic!(
            "expected an error, but assembly succeeded with {}:\n{src}",
            hex(&b)
        ),
        Err(e) => e,
    }
}

#[track_caller]
fn error_mentions(src: &str, needle: &str) {
    let e = error(src);
    assert!(
        e.contains(needle),
        "the diagnostic for `{src}` should mention `{needle}`:\n{e}"
    );
}

fn label(f: &Form) -> String {
    format!("{} {}", f.row.mnemonic, f.row.operands)
        .trim()
        .to_string()
}

/// Every binding of the variables a form uses, as `(var, value)` lists.
fn bindings(f: &Form) -> Vec<Vec<(Var, u32)>> {
    let mut out = vec![Vec::new()];
    for var in [Var::R, Var::P, Var::B, Var::N, Var::F, Var::T] {
        if !f.uses(var) {
            continue;
        }
        out = out
            .into_iter()
            .flat_map(|b| {
                f.values(var).into_iter().map(move |v| {
                    let mut b = b.clone();
                    b.push((var, v));
                    b
                })
            })
            .collect();
    }
    out
}

fn value_of(b: &[(Var, u32)], var: Var) -> u32 {
    b.iter().find(|(v, _)| *v == var).map_or(0, |(_, x)| *x)
}

// ---- 1. the table parses ----------------------------------------------------

#[test]
fn every_row_of_the_code_table_parses() {
    let failures: Vec<String> = ROWS
        .iter()
        .filter_map(|r| {
            parse_row(r)
                .err()
                .map(|e| format!("page {}: {} {}: {e}", r.page, r.mnemonic, r.operands))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(ROWS.len(), 227);
    assert_eq!(forms().len(), ROWS.len());
}

#[test]
fn rows_cite_the_pages_of_the_code_list() {
    // Section 4.2.2 of U12326EJ4V0UM runs from page 39 to page 45, and the
    // table is kept in the manual's order.
    let pages: Vec<u16> = ROWS.iter().map(|r| r.page).collect();
    assert!(pages.windows(2).all(|w| w[0] <= w[1]));
    assert_eq!(pages.first(), Some(&39));
    assert_eq!(pages.last(), Some(&45));
}

// ---- 2. the second document -------------------------------------------------

/// The operation list of the *78K0/Kx2 User's Manual: Hardware*,
/// R01UH0008EJ0401 (Rev. 4.01), section 29.2, pages 761–768: mnemonic, operand
/// column (spaces removed) and the "Bytes" column, in that document's order.
///
/// This is a separate Renesas publication from the one the code table was
/// transcribed from, and it lists lengths rather than codes, so it checks the
/// table's shape without sharing its transcription.
const KX2_OPERATION_LIST: &[(&str, &str, usize)] = &[
    ("MOV", "r,#byte", 2),
    ("MOV", "saddr,#byte", 3),
    ("MOV", "sfr,#byte", 3),
    ("MOV", "A,r", 1),
    ("MOV", "r,A", 1),
    ("MOV", "A,saddr", 2),
    ("MOV", "saddr,A", 2),
    ("MOV", "A,sfr", 2),
    ("MOV", "sfr,A", 2),
    ("MOV", "A,!addr16", 3),
    ("MOV", "!addr16,A", 3),
    ("MOV", "PSW,#byte", 3),
    ("MOV", "A,PSW", 2),
    ("MOV", "PSW,A", 2),
    ("MOV", "A,[DE]", 1),
    ("MOV", "[DE],A", 1),
    ("MOV", "A,[HL]", 1),
    ("MOV", "[HL],A", 1),
    ("MOV", "A,[HL+byte]", 2),
    ("MOV", "[HL+byte],A", 2),
    ("MOV", "A,[HL+B]", 1),
    ("MOV", "[HL+B],A", 1),
    ("MOV", "A,[HL+C]", 1),
    ("MOV", "[HL+C],A", 1),
    ("XCH", "A,r", 1),
    ("XCH", "A,saddr", 2),
    ("XCH", "A,sfr", 2),
    ("XCH", "A,!addr16", 3),
    ("XCH", "A,[DE]", 1),
    ("XCH", "A,[HL]", 1),
    ("XCH", "A,[HL+byte]", 2),
    ("XCH", "A,[HL+B]", 2),
    ("XCH", "A,[HL+C]", 2),
    ("MOVW", "rp,#word", 3),
    ("MOVW", "saddrp,#word", 4),
    ("MOVW", "sfrp,#word", 4),
    ("MOVW", "AX,saddrp", 2),
    ("MOVW", "saddrp,AX", 2),
    ("MOVW", "AX,sfrp", 2),
    ("MOVW", "sfrp,AX", 2),
    ("MOVW", "AX,rp", 1),
    ("MOVW", "rp,AX", 1),
    ("MOVW", "AX,!addr16", 3),
    ("MOVW", "!addr16,AX", 3),
    ("XCHW", "AX,rp", 1),
    ("ADD", "A,#byte", 2),
    ("ADD", "saddr,#byte", 3),
    ("ADD", "A,r", 2),
    ("ADD", "r,A", 2),
    ("ADD", "A,saddr", 2),
    ("ADD", "A,!addr16", 3),
    ("ADD", "A,[HL]", 1),
    ("ADD", "A,[HL+byte]", 2),
    ("ADD", "A,[HL+B]", 2),
    ("ADD", "A,[HL+C]", 2),
    ("ADDC", "A,#byte", 2),
    ("ADDC", "saddr,#byte", 3),
    ("ADDC", "A,r", 2),
    ("ADDC", "r,A", 2),
    ("ADDC", "A,saddr", 2),
    ("ADDC", "A,!addr16", 3),
    ("ADDC", "A,[HL]", 1),
    ("ADDC", "A,[HL+byte]", 2),
    ("ADDC", "A,[HL+B]", 2),
    ("ADDC", "A,[HL+C]", 2),
    ("SUB", "A,#byte", 2),
    ("SUB", "saddr,#byte", 3),
    ("SUB", "A,r", 2),
    ("SUB", "r,A", 2),
    ("SUB", "A,saddr", 2),
    ("SUB", "A,!addr16", 3),
    ("SUB", "A,[HL]", 1),
    ("SUB", "A,[HL+byte]", 2),
    ("SUB", "A,[HL+B]", 2),
    ("SUB", "A,[HL+C]", 2),
    ("SUBC", "A,#byte", 2),
    ("SUBC", "saddr,#byte", 3),
    ("SUBC", "A,r", 2),
    ("SUBC", "r,A", 2),
    ("SUBC", "A,saddr", 2),
    ("SUBC", "A,!addr16", 3),
    ("SUBC", "A,[HL]", 1),
    ("SUBC", "A,[HL+byte]", 2),
    ("SUBC", "A,[HL+B]", 2),
    ("SUBC", "A,[HL+C]", 2),
    ("AND", "A,#byte", 2),
    ("AND", "saddr,#byte", 3),
    ("AND", "A,r", 2),
    ("AND", "r,A", 2),
    ("AND", "A,saddr", 2),
    ("AND", "A,!addr16", 3),
    ("AND", "A,[HL]", 1),
    ("AND", "A,[HL+byte]", 2),
    ("AND", "A,[HL+B]", 2),
    ("AND", "A,[HL+C]", 2),
    ("OR", "A,#byte", 2),
    ("OR", "saddr,#byte", 3),
    ("OR", "A,r", 2),
    ("OR", "r,A", 2),
    ("OR", "A,saddr", 2),
    ("OR", "A,!addr16", 3),
    ("OR", "A,[HL]", 1),
    ("OR", "A,[HL+byte]", 2),
    ("OR", "A,[HL+B]", 2),
    ("OR", "A,[HL+C]", 2),
    ("XOR", "A,#byte", 2),
    ("XOR", "saddr,#byte", 3),
    ("XOR", "A,r", 2),
    ("XOR", "r,A", 2),
    ("XOR", "A,saddr", 2),
    ("XOR", "A,!addr16", 3),
    ("XOR", "A,[HL]", 1),
    ("XOR", "A,[HL+byte]", 2),
    ("XOR", "A,[HL+B]", 2),
    ("XOR", "A,[HL+C]", 2),
    ("CMP", "A,#byte", 2),
    ("CMP", "saddr,#byte", 3),
    ("CMP", "A,r", 2),
    ("CMP", "r,A", 2),
    ("CMP", "A,saddr", 2),
    ("CMP", "A,!addr16", 3),
    ("CMP", "A,[HL]", 1),
    ("CMP", "A,[HL+byte]", 2),
    ("CMP", "A,[HL+B]", 2),
    ("CMP", "A,[HL+C]", 2),
    ("ADDW", "AX,#word", 3),
    ("SUBW", "AX,#word", 3),
    ("CMPW", "AX,#word", 3),
    ("MULU", "X", 2),
    ("DIVUW", "C", 2),
    ("INC", "r", 1),
    ("INC", "saddr", 2),
    ("DEC", "r", 1),
    ("DEC", "saddr", 2),
    ("INCW", "rp", 1),
    ("DECW", "rp", 1),
    ("ROR", "A,1", 1),
    ("ROL", "A,1", 1),
    ("RORC", "A,1", 1),
    ("ROLC", "A,1", 1),
    ("ROR4", "[HL]", 2),
    ("ROL4", "[HL]", 2),
    ("ADJBA", "", 2),
    ("ADJBS", "", 2),
    ("MOV1", "CY,saddr.bit", 3),
    ("MOV1", "CY,sfr.bit", 3),
    ("MOV1", "CY,A.bit", 2),
    ("MOV1", "CY,PSW.bit", 3),
    ("MOV1", "CY,[HL].bit", 2),
    ("MOV1", "saddr.bit,CY", 3),
    ("MOV1", "sfr.bit,CY", 3),
    ("MOV1", "A.bit,CY", 2),
    ("MOV1", "PSW.bit,CY", 3),
    ("MOV1", "[HL].bit,CY", 2),
    ("AND1", "CY,saddr.bit", 3),
    ("AND1", "CY,sfr.bit", 3),
    ("AND1", "CY,A.bit", 2),
    ("AND1", "CY,PSW.bit", 3),
    ("AND1", "CY,[HL].bit", 2),
    ("OR1", "CY,saddr.bit", 3),
    ("OR1", "CY,sfr.bit", 3),
    ("OR1", "CY,A.bit", 2),
    ("OR1", "CY,PSW.bit", 3),
    ("OR1", "CY,[HL].bit", 2),
    ("XOR1", "CY,saddr.bit", 3),
    ("XOR1", "CY,sfr.bit", 3),
    ("XOR1", "CY,A.bit", 2),
    ("XOR1", "CY,PSW.bit", 3),
    ("XOR1", "CY,[HL].bit", 2),
    ("SET1", "saddr.bit", 2),
    ("SET1", "sfr.bit", 3),
    ("SET1", "A.bit", 2),
    ("SET1", "PSW.bit", 2),
    ("SET1", "[HL].bit", 2),
    ("CLR1", "saddr.bit", 2),
    ("CLR1", "sfr.bit", 3),
    ("CLR1", "A.bit", 2),
    ("CLR1", "PSW.bit", 2),
    ("CLR1", "[HL].bit", 2),
    ("SET1", "CY", 1),
    ("CLR1", "CY", 1),
    ("NOT1", "CY", 1),
    ("CALL", "!addr16", 3),
    ("CALLF", "!addr11", 2),
    ("CALLT", "[addr5]", 1),
    ("BRK", "", 1),
    ("RET", "", 1),
    ("RETI", "", 1),
    ("RETB", "", 1),
    ("PUSH", "PSW", 1),
    ("PUSH", "rp", 1),
    ("POP", "PSW", 1),
    ("POP", "rp", 1),
    ("MOVW", "SP,#word", 4),
    ("MOVW", "SP,AX", 2),
    ("MOVW", "AX,SP", 2),
    ("BR", "!addr16", 3),
    ("BR", "$addr16", 2),
    ("BR", "AX", 2),
    ("BC", "$addr16", 2),
    ("BNC", "$addr16", 2),
    ("BZ", "$addr16", 2),
    ("BNZ", "$addr16", 2),
    ("BT", "saddr.bit,$addr16", 3),
    ("BT", "sfr.bit,$addr16", 4),
    ("BT", "A.bit,$addr16", 3),
    ("BT", "PSW.bit,$addr16", 3),
    ("BT", "[HL].bit,$addr16", 3),
    ("BF", "saddr.bit,$addr16", 4),
    ("BF", "sfr.bit,$addr16", 4),
    ("BF", "A.bit,$addr16", 3),
    ("BF", "PSW.bit,$addr16", 4),
    ("BF", "[HL].bit,$addr16", 3),
    ("BTCLR", "saddr.bit,$addr16", 4),
    ("BTCLR", "sfr.bit,$addr16", 4),
    ("BTCLR", "A.bit,$addr16", 3),
    ("BTCLR", "PSW.bit,$addr16", 4),
    ("BTCLR", "[HL].bit,$addr16", 3),
    ("DBNZ", "B,$addr16", 2),
    ("DBNZ", "C,$addr16", 2),
    ("DBNZ", "saddr,$addr16", 3),
    ("SEL", "RBn", 2),
    ("NOP", "", 1),
    ("EI", "", 2),
    ("DI", "", 2),
    ("HALT", "", 2),
    ("STOP", "", 2),
];

#[test]
fn the_table_is_exactly_the_kx2_operation_list() {
    let ours: BTreeMap<(&str, &str), usize> = forms()
        .iter()
        .map(|f| ((f.row.mnemonic, f.row.operands), f.len()))
        .collect();
    let theirs: BTreeMap<(&str, &str), usize> = KX2_OPERATION_LIST
        .iter()
        .map(|(m, o, n)| ((*m, *o), *n))
        .collect();
    // No duplicate rows on either side.
    assert_eq!(ours.len(), forms().len());
    assert_eq!(theirs.len(), KX2_OPERATION_LIST.len());

    let only_ours: Vec<_> = ours.keys().filter(|k| !theirs.contains_key(k)).collect();
    let only_theirs: Vec<_> = theirs.keys().filter(|k| !ours.contains_key(k)).collect();
    assert!(
        only_ours.is_empty() && only_theirs.is_empty(),
        "forms only in the code table: {only_ours:?}\nforms only in the Kx2 list: {only_theirs:?}"
    );
    let wrong: Vec<String> = ours
        .iter()
        .filter(|(k, n)| theirs[*k] != **n)
        .map(|((m, o), n)| format!("{m} {o}: table {n} bytes, Kx2 list {}", theirs[&(*m, *o)]))
        .collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

// ---- 3. the opcode space ----------------------------------------------------

/// One form with its variables bound: the bytes a decoder would see, with
/// operand fields as wildcards.
struct Instance {
    form: &'static Form,
    binding: Vec<(Var, u32)>,
    pattern: Vec<Option<u8>>,
}

fn instances() -> Vec<Instance> {
    let mut out = Vec::new();
    for f in forms() {
        for b in bindings(f) {
            let pattern = f.pattern(|v| value_of(&b, v));
            out.push(Instance {
                form: f,
                binding: b,
                pattern,
            });
        }
    }
    out
}

/// Whether one byte string could begin both patterns.
fn overlap(a: &[Option<u8>], b: &[Option<u8>]) -> bool {
    a.iter()
        .zip(b)
        .all(|(x, y)| x.is_none() || y.is_none() || x == y)
}

/// A collision the manual documents: the same length, and wherever the two
/// differ, one has a short direct offset field and the other names `PSW`
/// (offset 1EH, FF1EH) or `SP` (offset 1CH, FF1CH) in its place. `EI` and `DI`
/// are the only forms allowed to match another byte for byte, being
/// `SET1 PSW.7` and `CLR1 PSW.7` under another name.
fn is_documented_alias(a: &Instance, b: &Instance) -> bool {
    if a.pattern.len() != b.pattern.len() {
        return false;
    }
    let names_psw = |f: &Form| {
        f.slots.contains(&Slot::Psw)
            || f.slots.contains(&Slot::PswBit)
            || matches!(f.row.mnemonic, "EI" | "DI")
    };
    let mut differs = false;
    let ok = (0..a.pattern.len()).all(|i| {
        let (x, y) = (a.pattern[i], b.pattern[i]);
        if x == y {
            return true;
        }
        differs = true;
        let (field_side, fixed_side, fixed) = match (x, y) {
            (None, Some(v)) => (a, b, v),
            (Some(v), None) => (b, a, v),
            _ => return false,
        };
        let named = match fixed {
            0x1e => names_psw(fixed_side.form),
            0x1c => fixed_side.form.slots.contains(&Slot::Sp),
            _ => false,
        };
        named && field_side.form.codes[i] == Code::Field(Field::SaddrOffset)
    });
    ok && (differs
        || [a, b]
            .iter()
            .any(|x| matches!(x.form.row.mnemonic, "EI" | "DI")))
}

#[test]
fn the_opcode_space_has_no_collisions_but_the_documented_aliases() {
    let all = instances();
    let mut aliases: BTreeSet<(String, String)> = BTreeSet::new();
    let mut bad = Vec::new();
    for i in 0..all.len() {
        for j in i + 1..all.len() {
            let (a, b) = (&all[i], &all[j]);
            if !overlap(&a.pattern, &b.pattern) {
                continue;
            }
            if is_documented_alias(a, b) {
                aliases.insert((label(a.form), label(b.form)));
                // `EI` and `DI` are bit 7 of PSW, and nothing else.
                for (x, y) in [(a, b), (b, a)] {
                    if matches!(x.form.row.mnemonic, "EI" | "DI") {
                        assert_eq!(
                            value_of(&y.binding, Var::B),
                            7,
                            "{} vs {}",
                            label(x.form),
                            label(y.form)
                        );
                    }
                }
            } else {
                bad.push(format!(
                    "{} {:?} overlaps {} {:?}",
                    label(a.form),
                    a.binding,
                    label(b.form),
                    b.binding
                ));
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));

    let want: BTreeSet<(String, String)> = [
        ("MOV saddr,#byte", "MOV PSW,#byte"),
        ("MOV A,saddr", "MOV A,PSW"),
        ("MOV saddr,A", "MOV PSW,A"),
        ("MOVW saddrp,#word", "MOVW SP,#word"),
        ("MOVW AX,saddrp", "MOVW AX,SP"),
        ("MOVW saddrp,AX", "MOVW SP,AX"),
        ("MOV1 CY,saddr.bit", "MOV1 CY,PSW.bit"),
        ("MOV1 saddr.bit,CY", "MOV1 PSW.bit,CY"),
        ("AND1 CY,saddr.bit", "AND1 CY,PSW.bit"),
        ("OR1 CY,saddr.bit", "OR1 CY,PSW.bit"),
        ("XOR1 CY,saddr.bit", "XOR1 CY,PSW.bit"),
        ("SET1 saddr.bit", "SET1 PSW.bit"),
        ("CLR1 saddr.bit", "CLR1 PSW.bit"),
        ("SET1 saddr.bit", "EI"),
        ("SET1 PSW.bit", "EI"),
        ("CLR1 saddr.bit", "DI"),
        ("CLR1 PSW.bit", "DI"),
        ("BT saddr.bit,$addr16", "BT PSW.bit,$addr16"),
        ("BF saddr.bit,$addr16", "BF PSW.bit,$addr16"),
        ("BTCLR saddr.bit,$addr16", "BTCLR PSW.bit,$addr16"),
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    assert_eq!(aliases, want);
}

#[test]
fn the_unassigned_first_bytes_are_exactly_the_manuals_holes() {
    let mut used = [false; 256];
    for inst in instances() {
        match inst.pattern[0] {
            Some(b) => used[b as usize] = true,
            None => panic!("{} has no fixed first byte", label(inst.form)),
        }
    }
    let free: Vec<u8> = (0..=255u8).filter(|b| !used[*b as usize]).collect();
    // 06H and 15H/17H sit between documented forms; C0H, D0H and E0H are
    // `MOVW AX,AX`, `MOVW AX,AX` and `XCHW AX,AX`, excluded by "Only when
    // rp = BC, DE or HL". (The "Except r = A" codes 31H, 61H and 71H are the
    // prefix bytes, so they are in use.) MAME's disassembler agrees.
    assert_eq!(free, vec![0x06, 0x15, 0x17, 0xc0, 0xd0, 0xe0]);
}

// The second bytes the table defines after each prefix, as opcode maps: row
// is the high nibble, column the low nibble, `#` defined. A code moved into
// an unused slot changes neither the length check nor the collision check,
// but it does change one of these.
//
// Cross-checked against MAME's disassembler, which defines exactly these but
// for the sixteen `31H 0xH`..`7xH` `A,[HL+B]`/`A,[HL+C]` codes in columns A
// and B of rows 0x-7x, where it has the bug described at the top of this file.
const PAGE_31H: [&str; 16] = [
    ".#.#.###..##.###", // 0x
    ".#.#.###..##.###", // 1x
    ".#.#.###..##.###", // 2x
    ".#.#.###..##.###", // 3x
    ".#.#.###..##.###", // 4x
    ".#.#.###..##.###", // 5x
    ".#.#.###..##.###", // 6x
    ".#.#.###..##.###", // 7x
    "#.#..####.##....", // 8x
    "#....####.......", // 9x
    ".....###........", // Ax
    ".....###........", // Bx
    ".....###........", // Cx
    ".....###........", // Dx
    ".....###........", // Ex
    ".....###........", // Fx
];
const PAGE_61H: [&str; 16] = [
    "#########.######", // 0x
    "#########.######", // 1x
    "#########.######", // 2x
    "#########.######", // 3x
    "#########.######", // 4x
    "#########.######", // 5x
    "#########.######", // 6x
    "#########.######", // 7x
    "#........#######", // 8x
    "#........#######", // 9x
    ".........#######", // Ax
    ".........#######", // Bx
    ".........#######", // Cx
    "#.......########", // Dx
    ".........#######", // Ex
    "#.......########", // Fx
];
const PAGE_71H: [&str; 16] = [
    "##..####.#######", // 0x
    "##..####.#######", // 1x
    ".#..####.#######", // 2x
    ".#..####.#######", // 3x
    ".#..####.#######", // 4x
    ".#..####.#######", // 5x
    ".#..####.#######", // 6x
    ".#..####.#######", // 7x
    ".#######........", // 8x
    ".#######........", // 9x
    ".#######........", // Ax
    ".#######........", // Bx
    ".#######........", // Cx
    ".#######........", // Dx
    ".#######........", // Ex
    ".#######........", // Fx
];

#[test]
fn the_prefix_pages_are_exactly_the_documented_maps() {
    for (prefix, map) in [(0x31u8, PAGE_31H), (0x61, PAGE_61H), (0x71, PAGE_71H)] {
        let mut used = [false; 256];
        for inst in instances() {
            if inst.pattern[0] == Some(prefix) {
                match inst.pattern.get(1).copied().flatten() {
                    Some(b) => used[b as usize] = true,
                    None => panic!("{} has no fixed second byte", label(inst.form)),
                }
            }
        }
        let got: Vec<String> = (0..16)
            .map(|hi| {
                (0..16)
                    .map(|lo| if used[hi << 4 | lo] { '#' } else { '.' })
                    .collect()
            })
            .collect();
        assert_eq!(got, map, "the {prefix:02X}H page");
    }
}

#[test]
fn nop_is_zero_and_pads_code() {
    let nop = forms().iter().find(|f| f.row.mnemonic == "NOP").unwrap();
    assert_eq!(nop.pattern(|_| 0), vec![Some(0x00)]);
    let arch = rsasm::arch::lookup("78k0").unwrap();
    let state = arch.initial_state();
    assert_eq!(arch.nop_fill(&state, 3), vec![0, 0, 0]);
    assert_eq!(arch.pointer_bytes(&state), 2);
    assert_eq!(arch.elf_machine(), 0);
    assert_eq!(arch.data_reloc(2, false), None);
    assert_eq!(arch.default_dialect(), Renesas);
}

// ---- 4. every form through the front end --------------------------------------

const R_FN: [&str; 8] = ["X", "A", "C", "B", "E", "D", "L", "H"];
const RP_FN: [&str; 4] = ["AX", "BC", "DE", "HL"];

/// Register-name spellings: function names, absolute names, lowercase.
#[derive(Copy, Clone)]
enum Spelling {
    Function,
    Absolute,
    Lower,
}

fn reg8_name(r: u32, s: Spelling) -> String {
    match s {
        Spelling::Function => R_FN[r as usize].to_string(),
        Spelling::Absolute => format!("R{r}"),
        Spelling::Lower => R_FN[r as usize].to_ascii_lowercase(),
    }
}

fn reg16_name(p: u32, s: Spelling) -> String {
    match s {
        Spelling::Function => RP_FN[p as usize].to_string(),
        Spelling::Absolute => format!("RP{p}"),
        Spelling::Lower => RP_FN[p as usize].to_ascii_lowercase(),
    }
}

fn word(w: &str, s: Spelling) -> String {
    match s {
        Spelling::Lower => w.to_ascii_lowercase(),
        _ => w.to_string(),
    }
}

// The operand values each instance is written with, and the bytes they must
// produce.
const IMM8: u8 = 0x12;
const HL_DISP: u8 = 0xab;
const IMM16: u16 = 0x3456;
const SADDR: u16 = 0xfe34;
const SADDRP: u16 = 0xfe36;
const SFR: u16 = 0xff56;
const SFRP: u16 = 0xff58;
const ADDR16: u16 = 0x789a;
const FA_LOW: u16 = 0xbc;
/// Written as `$$+7`: the target is 7 bytes past the instruction's start.
const REL_TARGET: i64 = 7;

fn source_and_bytes(inst: &Instance, s: Spelling) -> (String, Vec<u8>) {
    let f = inst.form;
    let bv = |v| value_of(&inst.binding, v);
    let ops: Vec<String> = f
        .slots
        .iter()
        .map(|slot| match slot {
            Slot::R => reg8_name(bv(Var::R), s),
            Slot::Rp => reg16_name(bv(Var::P), s),
            Slot::Reg(c) => reg8_name(*c as u32, s),
            Slot::Ax => reg16_name(0, s),
            Slot::Sp => word("SP", s),
            Slot::Psw => word("PSW", s),
            Slot::Cy => word("CY", s),
            Slot::One => "1".into(),
            Slot::Byte => format!("#{IMM8:X}H"),
            Slot::Word => format!("#{IMM16:X}H"),
            Slot::Saddr => format!("0{SADDR:X}H"),
            Slot::Saddrp => format!("0{SADDRP:X}H"),
            Slot::Sfr => format!("0{SFR:X}H"),
            Slot::Sfrp => format!("0{SFRP:X}H"),
            Slot::Addr16 => format!("!{ADDR16:X}H"),
            Slot::Addr11 => format!("!0{:X}H", 0x800 | (bv(Var::F) as u16) << 8 | FA_LOW),
            Slot::Addr5 => format!("[{:X}H]", 0x40 + 2 * bv(Var::T)),
            Slot::Rel => format!("$$+{REL_TARGET}"),
            Slot::De => word("[DE]", s),
            Slot::Hl => word("[HL]", s),
            Slot::HlByte => format!("{}+0{HL_DISP:X}H]", word("[HL", s)),
            Slot::HlB => word("[HL+B]", s),
            Slot::HlC => word("[HL+C]", s),
            Slot::SaddrBit => format!("0{SADDR:X}H.{}", bv(Var::B)),
            Slot::SfrBit => format!("0{SFR:X}H.{}", bv(Var::B)),
            // Bit terms name the register `A` only (RA78K0 Table 2-16).
            Slot::ABit => format!("{}.{}", word("A", s), bv(Var::B)),
            Slot::PswBit => format!("{}.{}", word("PSW", s), bv(Var::B)),
            Slot::HlBit => format!("{}.{}", word("[HL]", s), bv(Var::B)),
            Slot::Bank => format!("{}{}", word("RB", s), bv(Var::N)),
        })
        .collect();
    let src = format!("\t{}\t{}", word(f.row.mnemonic, s), ops.join(","));

    let mut want = Vec::new();
    for (i, (code, p)) in f.codes.iter().zip(&inst.pattern).enumerate() {
        let byte = match (code, p) {
            (_, Some(b)) => *b,
            (Code::Field(field), None) => match field {
                Field::Data if f.slots.contains(&Slot::HlByte) => HL_DISP,
                Field::Data => IMM8,
                Field::LowByte => IMM16 as u8,
                Field::HighByte => (IMM16 >> 8) as u8,
                Field::SaddrOffset if f.slots.contains(&Slot::Saddrp) => SADDRP as u8,
                Field::SaddrOffset => SADDR as u8,
                Field::SfrOffset if f.slots.contains(&Slot::Sfrp) => SFRP as u8,
                Field::SfrOffset => SFR as u8,
                Field::LowAddr => ADDR16 as u8,
                Field::HighAddr => (ADDR16 >> 8) as u8,
                Field::Jdisp => {
                    assert_eq!(i + 1, f.len());
                    (REL_TARGET - f.len() as i64) as u8
                }
                Field::Fa7_0 => FA_LOW as u8,
            },
            (Code::Bits { .. }, None) => unreachable!(),
        };
        want.push(byte);
    }
    (src, want)
}

#[test]
fn every_form_assembles_to_the_tables_bytes_in_every_spelling() {
    let all = instances();
    let mut checked = 0;
    let mut failures = Vec::new();
    for s in [Spelling::Function, Spelling::Absolute, Spelling::Lower] {
        // One source per form keeps the failure report specific without
        // paying for an assembler per instance.
        let mut by_form: BTreeMap<usize, Vec<(String, Vec<u8>)>> = BTreeMap::new();
        for inst in &all {
            let idx = forms()
                .iter()
                .position(|f| std::ptr::eq(f, inst.form))
                .unwrap();
            by_form
                .entry(idx)
                .or_default()
                .push(source_and_bytes(inst, s));
        }
        for lines in by_form.values() {
            let src: String = lines.iter().map(|(l, _)| format!("{l}\n")).collect();
            let want: Vec<u8> = lines.iter().flat_map(|(_, b)| b.clone()).collect();
            match assemble(&src) {
                Ok(got) if got == want => checked += lines.len(),
                _ => {
                    for (line, want) in lines {
                        match assemble(line) {
                            Ok(got) if &got == want => {}
                            Ok(got) => failures.push(format!(
                                "{line}: want {} got {}",
                                hex(want),
                                hex(&got)
                            )),
                            Err(e) => failures.push(format!("{line}: {e}")),
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // 782 bindings of 227 forms, three spellings each.
    assert_eq!(checked, 3 * all.len());
    assert_eq!(all.len(), 782);
}

/// Writes every instance as `hex<TAB>source` lines to the file named by
/// `K78_CORPUS`, for feeding an external disassembler. Not a check by itself.
#[test]
#[ignore = "writes a corpus file; run with K78_CORPUS=<path> --ignored"]
fn dump_corpus_for_an_external_disassembler() {
    let Ok(path) = std::env::var("K78_CORPUS") else {
        return;
    };
    let mut out = String::new();
    for inst in instances() {
        let (src, want) = source_and_bytes(&inst, Spelling::Function);
        assert_eq!(bytes(&src), want, "{src}");
        out.push_str(&format!("{}\t{}\n", hex(&want), src.trim()));
    }
    std::fs::write(path, out).unwrap();
}

#[test]
fn callf_reaches_its_whole_range_as_constants_and_forward_references() {
    let mut consts = String::new();
    let mut fwd = String::new();
    let mut defs = String::new();
    let mut want = Vec::new();
    for addr in 0x800u16..=0xfff {
        let fa = addr - 0x800;
        consts.push_str(&format!("\tCALLF !0{addr:X}H\n"));
        fwd.push_str(&format!("\tCALLF !T{addr:X}\n"));
        defs.push_str(&format!("T{addr:X} EQU 0{addr:X}H\n"));
        want.push(0x0c | ((fa >> 8) as u8) << 4);
        want.push(fa as u8);
    }
    assert_eq!(bytes(&consts), want);
    assert_eq!(bytes(&format!("{fwd}{defs}")), want);
}

#[test]
fn callt_reaches_its_whole_table_as_constants_and_forward_references() {
    let mut consts = String::new();
    let mut fwd = String::new();
    let mut defs = String::new();
    let mut want = Vec::new();
    for ta in 0u8..32 {
        let addr = 0x40 + 2 * ta;
        consts.push_str(&format!("\tCALLT [{addr:X}H]\n"));
        fwd.push_str(&format!("\tCALLT [T{addr:X}]\n"));
        defs.push_str(&format!("T{addr:X} EQU {addr:X}H\n"));
        want.push(0xc1 | ta << 1);
    }
    assert_eq!(bytes(&consts), want);
    assert_eq!(bytes(&format!("{fwd}{defs}")), want);
}

#[test]
fn short_direct_addressing_covers_fe20h_to_ff1fh_as_constants_and_forward_references() {
    let mut consts = String::new();
    let mut fwd = String::new();
    let mut defs = String::new();
    let mut want = Vec::new();
    for addr in 0xfe20u16..=0xff1f {
        consts.push_str(&format!("\tINC 0{addr:X}H\n"));
        fwd.push_str(&format!("\tINC V{addr:X}\n"));
        defs.push_str(&format!("V{addr:X} EQU 0{addr:X}H\n"));
        want.extend([0x81, addr as u8]);
    }
    assert_eq!(bytes(&consts), want);
    assert_eq!(bytes(&format!("{fwd}{defs}")), want);
}

// ---- operand selection -------------------------------------------------------

#[test]
fn an_address_picks_short_direct_or_sfr_addressing_by_value() {
    // U12326EJ4V0UM pages 27 and 28 give these two as worked examples.
    enc("MOV 0FE30H,#50H", "11 30 50");
    enc("MOV 0FF20H,A", "f6 20");
    // FF00H-FF1FH is both; short direct addressing wins (RA78K0 Table 2-21
    // note 2).
    enc("MOV A,0FF05H", "f0 05");
    enc("MOV A,0FF30H", "f4 30");
    enc("MOVW AX,0FF10H", "89 10");
    enc("MOVW AX,0FF12H", "89 12");
    enc("MOVW AX,0FFB0H", "a9 b0");
    enc("SET1 0FF02H.1", "1a 02");
    enc("SET1 0FF30H.1", "71 1a 30");
    enc("BT 0FF30H.1,$$+4", "31 16 30 00");
    enc("BT 0FE30H.1,$$+3", "9c 30 00");
    // An `EQU` defined before use is a constant, so it is classified too.
    enc("PM0 EQU 0FF20H\nMOV PM0,A\nCLR1 PM0.3", "f6 20 71 3b 20");
    // `!` always means a 16-bit address, even in short direct range.
    enc("MOV A,!0FE30H", "8e 30 fe");
}

#[test]
fn psw_and_sp_are_their_short_direct_addresses() {
    enc("MOV A,PSW", "f0 1e");
    enc("MOV A,0FF1EH", "f0 1e");
    enc("MOVW SP,#0FE00H", "ee 1c 00 fe");
    enc("MOVW AX,SP", "89 1c");
    enc("EI\nSET1 PSW.7\nDI\nCLR1 PSW.7", "7a 1e 7a 1e 7b 1e 7b 1e");
}

#[test]
fn bit_terms_follow_the_manuals_precedence_rule() {
    // RA78K0 language manual, section 2.5, page 67.
    enc("SET1 1 + 0FE30H.3", "3a 31");
    enc("CLR1 0FE40H.4 + 2", "6b 40");
    enc("FLAGS EQU 0FE20H\nBIT EQU 5\nSET1 FLAGS.BIT", "5a 20");
    enc(
        "SET1 A.5\nCLR1 [HL].7\nMOV1 CY,PSW.0",
        "61 da 71 f3 71 04 1e",
    );
    enc("SET1 A . 5", "61 da");
}

#[test]
fn register_names_take_either_spelling_in_any_case() {
    enc("MOV A,C", "62");
    enc("MOV R1,R2", "62");
    enc("mov a,c", "62");
    enc("INCW DE", "84");
    enc("INCW RP2", "84");
    enc("PUSH AX\nPOP rp3", "b1 b6");
    enc("SEL RB3", "61 f8");
}

#[test]
fn except_r_a_and_only_bc_de_hl_are_enforced() {
    // `ADD A,A` has no `A,r` code, but it is `r,A` with r = A.
    enc("ADD A,A", "61 01");
    error_mentions("MOV A,A", "does not take A");
    error_mentions("XCH A,A", "does not take A");
    error_mentions("MOVW AX,AX", "BC, DE or HL");
    error_mentions("XCHW AX,AX", "BC, DE or HL");
}

// ---- branches ---------------------------------------------------------------

#[test]
fn relative_branches_count_from_the_next_instruction() {
    enc("LP: BR $LP", "fa fe");
    enc("BC $NX\nNX:", "8d 00");
    enc("LP: DBNZ B,$LP", "8b fe");
    enc("LP: DBNZ 0FE20H,$LP", "04 20 fd");
    enc("LP: BTCLR 0FE20H.0,$LP", "31 01 20 fc");
    enc("BR $$-1", "fa fd");
    enc("BR !$+100H", "9b 00 01");
}

#[test]
fn a_relative_branch_out_of_range_names_its_limit() {
    let far = "BR $LP\nDS 128\nLP:";
    error_mentions(far, "-128 to 127");
    let back = "LP: DS 127\nBNZ $LP";
    error_mentions(back, "-128 to 127");
    // The extremes fit.
    assert_eq!(bytes("BR $LP\nDS 127\nLP:").len(), 129);
    assert_eq!(bytes("LP: DS 126\nBZ $LP")[127], 0x80);
}

#[test]
fn br_without_a_sigil_picks_the_short_form_when_it_reaches() {
    // RA78K0 language manual, section 3.7, pages 115-116.
    enc("LP: BR LP", "fa fe");
    let near = bytes("BR LP\nDS 127\nLP:");
    assert_eq!(&near[..2], &[0xfa, 0x7f]);
    let far = bytes("BR LP\nDS 128\nLP:");
    assert_eq!(&far[..3], &[0x9b, 0x83, 0x00]);
    enc("BR 1234H", "9b 34 12");
}

#[test]
fn calls_take_their_sigils() {
    enc("CALL !1234H", "9a 34 12");
    enc("CALLF !0FFFH", "7c ff");
    enc("CALLT [40H]", "c1");
    enc("BR AX", "31 98");
    error_mentions("CALL 1234H", "!addr16");
}

// ---- diagnostics --------------------------------------------------------------

#[test]
fn range_violations_name_their_limits() {
    error_mentions("INC 0FD00H", "0FE20H to 0FF1FH");
    error_mentions("MOV A,0FD00H", "!0FD00H");
    error_mentions("MOVW AX,0FE21H", "must be even");
    error_mentions("MOVW AX,0FF31H", "must be even");
    error_mentions("CALLF !1000H", "0800H to 0FFFH");
    error_mentions("CALLT [41H]", "40H to 7EH");
    error_mentions("CALLT [80H]", "40H to 7EH");
    error_mentions("SET1 A.8", "0 to 7");
    error_mentions("MOV A,#100H", "-128 to 255");
    error_mentions("MOV A,!10000H", "0H to 0FFFFH");
    error_mentions("ROR A,2", "exactly 1");
    // A forward reference is checked when it resolves.
    error_mentions("INC V\nV EQU 0FE1FH", "out of range");
    error_mentions("CALLF !T\nT EQU 7FFH", "out of range");
}

#[test]
fn sfr_addressing_needs_an_absolute_address_known_before_use() {
    // RA78K0 language manual, Table 2-21: `sfr` accepts only an absolute
    // expression, backward-referenced. A forward reference is short direct
    // addressing, which FF30H is not.
    enc("S EQU 0FF30H\nMOV A,S", "f4 30");
    error_mentions("MOV A,S\nS EQU 0FF30H", "out of range");
}

#[test]
fn malformed_operands_are_diagnosed() {
    error_mentions("FOO A", "unknown 78K0 instruction");
    error_mentions("MOV A,[DE+1]", "[HL+byte]");
    error_mentions("MOV A,#B", "register name");
    // Register names are reserved words, so a label cannot be called `L`.
    error_mentions("BR $L", "register name");
    error_mentions("SET1 X.1", "no addressable bits");
    error_mentions("SET1 [DE].1", "[HL]");
    error_mentions("SET1 A.1.2", "exactly one `.`");
    error_mentions("SET1 0FE20H.N\nN EQU 1", "absolute number");
    error_mentions("NOP A", "no operands");
    error_mentions("MOV A", "invalid operands for `MOV`");
}

#[test]
fn malformed_input_never_panics() {
    let cases = [
        "MOV",
        "MOV A,",
        "MOV ,A",
        "MOV A,#",
        "BR $",
        "BR !",
        "BR",
        "SET1 .3",
        "SET1 A.",
        "SET1 A..3",
        "SET1 .",
        "MOV A,[",
        "MOV A,[HL+",
        "MOV A,[HL+]",
        "MOV A,[]",
        "MOV A,]",
        "CALLT [",
        "CALLT []",
        "BT A.0",
        "BT A.0,",
        "BT ,$L",
        "MOV1 CY,[HL].",
        "MOV1 CY,[DE].1",
        "SEL RB4",
        "SEL",
        "XCH A,[HL+B+1]",
        "MOV A,1.2.3",
        "SET1 [HL]..3",
        "INC 0.",
        "MOV A,$",
        "DBNZ $,$",
        "MOV1 .,.",
        "CLR1 0FE20H.-1",
        "MOV A,#A",
        "SET1 0FE20H.99999999999",
        "MOV A,0FFFFFFFFFFFFFFFFH",
        "CALLF !-1",
        "CALLT [-2]",
        "MOVW AX,#(",
        "MOV [HL].1,A",
        "BT [HL],$L",
        "MOV1 CY,A.0,CY",
        "SET1 1.",
        "SET1 .1",
        "SET1 A.0FH",
        "SET1 A.9ZZ",
        "SET1 0FE20H.1H",
        "ROR4 [HL].1",
        "MOV A,[HL+C",
        "MOV A,HL+C]",
        "INC [[HL]]",
    ];
    for c in cases {
        // Only the absence of a panic is being tested; most are errors.
        let _ = assemble(c);
    }
}

// ---- a realistic program ------------------------------------------------------

#[test]
fn a_small_ca78k0_program() {
    let src = "\
; Toggle an LED on P1.0 with a busy-wait between toggles.
        NAME    BLINK
P1      EQU     0FF01H          ; port 1 (short direct area)
PM1     EQU     0FF21H          ; port mode register 1 (SFR area)
COUNT   EQU     0FE80H          ; delay counter in high-speed RAM
LED     EQU     0

        CSEG
START:  DI
        MOVW    SP,#0FE7EH
        CLR1    PM1.LED         ; P1.0 is an output
        MOV     A,#0
LOOP:   XOR     A,#01H
        MOV     P1,A
        CALL    !DELAY
        BR      $LOOP

DELAY:  MOV     COUNT,#10
WAIT:   DBNZ    COUNT,$WAIT
        BT      P1.LED,$DONE
        NOP
DONE:   RET
        END
";
    let asm = assemble_flat_dialect("78k0", Renesas, src, 0);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    // Worked by hand from U12326EJ4V0UM pages 39-45:
    //   00 DI                 7B 1E
    //   02 MOVW SP,#0FE7EH    EE 1C 7E FE
    //   06 CLR1 PM1.0         71 0B 21     (sfr.bit)
    //   09 MOV A,#0           A1 00
    //   0B XOR A,#01H         7D 01
    //   0D MOV P1,A           F2 01        (saddr: FF01H is short direct)
    //   0F CALL !DELAY        9A 14 00
    //   12 BR $LOOP           FA F7        (0BH - 14H)
    //   14 MOV COUNT,#10      11 80 0A
    //   17 DBNZ COUNT,$WAIT   04 80 FD     (17H - 1AH)
    //   1A BT P1.0,$DONE      8C 01 01     (1EH - 1DH)
    //   1D NOP                00
    //   1E RET                AF
    assert_eq!(
        hex(&section(&asm, ".text")),
        "7b 1e ee 1c 7e fe 71 0b 21 a1 00 7d 01 f2 01 9a 14 00 fa f7 \
         11 80 0a 04 80 fd 8c 01 01 00 af"
    );
}
