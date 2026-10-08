//! Encoding tests for the Intel 8051 (MCS-51) backend.
//!
//! Every expected image here was produced by the Macro Assembler AS (`asl
//! -cpu 8051` with `p2bin`, after its own `stddef51.inc`), or where a case
//! says so by SDCC's sdas8051 with sdld, from the same source, the way
//! `tools/xas-diff/run.sh` runs them. Every refusal was checked to be one in
//! AS too, except the few marked as rsasm's own, which the backend documents.
//! The corpora there are the full check; these keep the rules that are
//! easiest to break in the hermetic suite.

#![cfg(feature = "retro")]

mod common;

use common::hex;
use rsasm::arch;
use rsasm::arch::retro::mcs51;
use rsasm::assembler::{Assembler, Options};
use rsasm::section::SectionId;
use std::collections::BTreeMap;

/// Assembles `src` the way `rsasm -a 8051 -f bin` does.
fn assemble(src: &str) -> Assembler {
    let a = arch::lookup("8051").expect("backend is compiled in");
    let options = Options::new()
        .with_dialect(a.default_dialect())
        .with_relocatable(false);
    let mut asm = Assembler::new(a, options);
    asm.assemble_str("test.s", src);
    asm.finish();
    asm
}

#[track_caller]
fn image(src: &str) -> String {
    let asm = assemble(src);
    assert!(
        !asm.diags.has_errors(),
        "{src}\n{}",
        asm.diags.render(&asm.sm, false)
    );
    hex(&asm.section_bytes(SectionId(0)))
}

#[track_caller]
fn check(src: &str, want: &str) {
    let got = image(src);
    assert_eq!(got, want, "\n{src}\n  want: {want}\n   got: {got}");
}

#[track_caller]
fn refused(src: &str, message: &str) {
    let asm = assemble(src);
    let text = asm.diags.render(&asm.sm, false);
    assert!(asm.diags.has_errors(), "not refused:\n{src}");
    assert!(
        text.contains(message),
        "\n{src}\n  want an error containing: {message}\n  got: {text}"
    );
}

/// One source line for each of the 255 opcodes, with the bytes AS gives it:
/// the first line of `tools/xas-diff/i8051.txt` that produces each opcode.
const OPCODES: &[(&str, &str)] = &[
    ("nop", "00"),
    ("ajmp 0H", "01 00"),
    ("ljmp 0H", "02 00 00"),
    ("rr A", "03"),
    ("inc A", "04"),
    ("inc 0H", "05 00"),
    ("inc @R0", "06"),
    ("inc @R1", "07"),
    ("inc R0", "08"),
    ("inc R1", "09"),
    ("inc R2", "0a"),
    ("inc R3", "0b"),
    ("inc R4", "0c"),
    ("inc R5", "0d"),
    ("inc R6", "0e"),
    ("inc R7", "0f"),
    ("jbc 0H,$", "10 00 fd"),
    ("acall 0H", "11 00"),
    ("lcall 0H", "12 00 00"),
    ("rrc A", "13"),
    ("dec A", "14"),
    ("dec 0H", "15 00"),
    ("dec @R0", "16"),
    ("dec @R1", "17"),
    ("dec R0", "18"),
    ("dec R1", "19"),
    ("dec R2", "1a"),
    ("dec R3", "1b"),
    ("dec R4", "1c"),
    ("dec R5", "1d"),
    ("dec R6", "1e"),
    ("dec R7", "1f"),
    ("jb 0H,$", "20 00 fd"),
    ("ajmp 1FFH", "21 ff"),
    ("ret", "22"),
    ("rl A", "23"),
    ("add A,#0H", "24 00"),
    ("add A,0H", "25 00"),
    ("add A,@R0", "26"),
    ("add A,@R1", "27"),
    ("add A,R0", "28"),
    ("add A,R1", "29"),
    ("add A,R2", "2a"),
    ("add A,R3", "2b"),
    ("add A,R4", "2c"),
    ("add A,R5", "2d"),
    ("add A,R6", "2e"),
    ("add A,R7", "2f"),
    ("jnb 0H,$", "30 00 fd"),
    ("acall 1FFH", "31 ff"),
    ("reti", "32"),
    ("rlc A", "33"),
    ("addc A,#0H", "34 00"),
    ("addc A,0H", "35 00"),
    ("addc A,@R0", "36"),
    ("addc A,@R1", "37"),
    ("addc A,R0", "38"),
    ("addc A,R1", "39"),
    ("addc A,R2", "3a"),
    ("addc A,R3", "3b"),
    ("addc A,R4", "3c"),
    ("addc A,R5", "3d"),
    ("addc A,R6", "3e"),
    ("addc A,R7", "3f"),
    ("jc $", "40 fe"),
    ("ajmp 2AAH", "41 aa"),
    ("orl 0H,A", "42 00"),
    ("orl 0H,#0H", "43 00 00"),
    ("orl A,#0H", "44 00"),
    ("orl A,0H", "45 00"),
    ("orl A,@R0", "46"),
    ("orl A,@R1", "47"),
    ("orl A,R0", "48"),
    ("orl A,R1", "49"),
    ("orl A,R2", "4a"),
    ("orl A,R3", "4b"),
    ("orl A,R4", "4c"),
    ("orl A,R5", "4d"),
    ("orl A,R6", "4e"),
    ("orl A,R7", "4f"),
    ("jnc $", "50 fe"),
    ("acall 2AAH", "51 aa"),
    ("anl 0H,A", "52 00"),
    ("anl 0H,#0H", "53 00 00"),
    ("anl A,#0H", "54 00"),
    ("anl A,0H", "55 00"),
    ("anl A,@R0", "56"),
    ("anl A,@R1", "57"),
    ("anl A,R0", "58"),
    ("anl A,R1", "59"),
    ("anl A,R2", "5a"),
    ("anl A,R3", "5b"),
    ("anl A,R4", "5c"),
    ("anl A,R5", "5d"),
    ("anl A,R6", "5e"),
    ("anl A,R7", "5f"),
    ("jz $", "60 fe"),
    ("ajmp 355H", "61 55"),
    ("xrl 0H,A", "62 00"),
    ("xrl 0H,#0H", "63 00 00"),
    ("xrl A,#0H", "64 00"),
    ("xrl A,0H", "65 00"),
    ("xrl A,@R0", "66"),
    ("xrl A,@R1", "67"),
    ("xrl A,R0", "68"),
    ("xrl A,R1", "69"),
    ("xrl A,R2", "6a"),
    ("xrl A,R3", "6b"),
    ("xrl A,R4", "6c"),
    ("xrl A,R5", "6d"),
    ("xrl A,R6", "6e"),
    ("xrl A,R7", "6f"),
    ("jnz $", "70 fe"),
    ("acall 355H", "71 55"),
    ("orl C,0H", "72 00"),
    ("jmp @A+DPTR", "73"),
    ("mov A,#0H", "74 00"),
    ("mov 0H,#0H", "75 00 00"),
    ("mov @R0,#0H", "76 00"),
    ("mov @R1,#0H", "77 00"),
    ("mov R0,#0H", "78 00"),
    ("mov R1,#0H", "79 00"),
    ("mov R2,#0H", "7a 00"),
    ("mov R3,#0H", "7b 00"),
    ("mov R4,#0H", "7c 00"),
    ("mov R5,#0H", "7d 00"),
    ("mov R6,#0H", "7e 00"),
    ("mov R7,#0H", "7f 00"),
    ("sjmp $", "80 fe"),
    ("ajmp 400H", "81 00"),
    ("anl C,0H", "82 00"),
    ("movc A,@A+PC", "83"),
    ("div AB", "84"),
    ("mov 0H,0H", "85 00 00"),
    ("mov 0H,@R0", "86 00"),
    ("mov 0H,@R1", "87 00"),
    ("mov 0H,R0", "88 00"),
    ("mov 0H,R1", "89 00"),
    ("mov 0H,R2", "8a 00"),
    ("mov 0H,R3", "8b 00"),
    ("mov 0H,R4", "8c 00"),
    ("mov 0H,R5", "8d 00"),
    ("mov 0H,R6", "8e 00"),
    ("mov 0H,R7", "8f 00"),
    ("mov DPTR,#0H", "90 00 00"),
    ("acall 400H", "91 00"),
    ("mov 0H,C", "92 00"),
    ("movc A,@A+DPTR", "93"),
    ("subb A,#0H", "94 00"),
    ("subb A,0H", "95 00"),
    ("subb A,@R0", "96"),
    ("subb A,@R1", "97"),
    ("subb A,R0", "98"),
    ("subb A,R1", "99"),
    ("subb A,R2", "9a"),
    ("subb A,R3", "9b"),
    ("subb A,R4", "9c"),
    ("subb A,R5", "9d"),
    ("subb A,R6", "9e"),
    ("subb A,R7", "9f"),
    ("orl C,/0H", "a0 00"),
    ("ajmp 5FFH", "a1 ff"),
    ("mov C,0H", "a2 00"),
    ("inc DPTR", "a3"),
    ("mul AB", "a4"),
    ("mov @R0,0H", "a6 00"),
    ("mov @R1,0H", "a7 00"),
    ("mov R0,0H", "a8 00"),
    ("mov R1,0H", "a9 00"),
    ("mov R2,0H", "aa 00"),
    ("mov R3,0H", "ab 00"),
    ("mov R4,0H", "ac 00"),
    ("mov R5,0H", "ad 00"),
    ("mov R6,0H", "ae 00"),
    ("mov R7,0H", "af 00"),
    ("anl C,/0H", "b0 00"),
    ("acall 5FFH", "b1 ff"),
    ("cpl 0H", "b2 00"),
    ("cpl C", "b3"),
    ("cjne A,#0H,$", "b4 00 fd"),
    ("cjne A,0H,$", "b5 00 fd"),
    ("cjne @R0,#0H,$", "b6 00 fd"),
    ("cjne @R1,#0H,$", "b7 00 fd"),
    ("cjne R0,#0H,$", "b8 00 fd"),
    ("cjne R1,#0H,$", "b9 00 fd"),
    ("cjne R2,#0H,$", "ba 00 fd"),
    ("cjne R3,#0H,$", "bb 00 fd"),
    ("cjne R4,#0H,$", "bc 00 fd"),
    ("cjne R5,#0H,$", "bd 00 fd"),
    ("cjne R6,#0H,$", "be 00 fd"),
    ("cjne R7,#0H,$", "bf 00 fd"),
    ("push 0H", "c0 00"),
    ("ajmp 6C3H", "c1 c3"),
    ("clr 0H", "c2 00"),
    ("clr C", "c3"),
    ("swap A", "c4"),
    ("xch A,0H", "c5 00"),
    ("xch A,@R0", "c6"),
    ("xch A,@R1", "c7"),
    ("xch A,R0", "c8"),
    ("xch A,R1", "c9"),
    ("xch A,R2", "ca"),
    ("xch A,R3", "cb"),
    ("xch A,R4", "cc"),
    ("xch A,R5", "cd"),
    ("xch A,R6", "ce"),
    ("xch A,R7", "cf"),
    ("pop 0H", "d0 00"),
    ("acall 6C3H", "d1 c3"),
    ("setb 0H", "d2 00"),
    ("setb C", "d3"),
    ("da A", "d4"),
    ("djnz 0H,$", "d5 00 fd"),
    ("xchd A,@R0", "d6"),
    ("xchd A,@R1", "d7"),
    ("djnz R0,$", "d8 fe"),
    ("djnz R1,$", "d9 fe"),
    ("djnz R2,$", "da fe"),
    ("djnz R3,$", "db fe"),
    ("djnz R4,$", "dc fe"),
    ("djnz R5,$", "dd fe"),
    ("djnz R6,$", "de fe"),
    ("djnz R7,$", "df fe"),
    ("movx A,@DPTR", "e0"),
    ("ajmp 7FFH", "e1 ff"),
    ("movx A,@R0", "e2"),
    ("movx A,@R1", "e3"),
    ("clr A", "e4"),
    ("mov A,0H", "e5 00"),
    ("mov A,@R0", "e6"),
    ("mov A,@R1", "e7"),
    ("mov A,R0", "e8"),
    ("mov A,R1", "e9"),
    ("mov A,R2", "ea"),
    ("mov A,R3", "eb"),
    ("mov A,R4", "ec"),
    ("mov A,R5", "ed"),
    ("mov A,R6", "ee"),
    ("mov A,R7", "ef"),
    ("movx @DPTR,A", "f0"),
    ("acall 7FFH", "f1 ff"),
    ("movx @R0,A", "f2"),
    ("movx @R1,A", "f3"),
    ("cpl A", "f4"),
    ("mov 0H,A", "f5 00"),
    ("mov @R0,A", "f6"),
    ("mov @R1,A", "f7"),
    ("mov R0,A", "f8"),
    ("mov R1,A", "f9"),
    ("mov R2,A", "fa"),
    ("mov R3,A", "fb"),
    ("mov R4,A", "fc"),
    ("mov R5,A", "fd"),
    ("mov R6,A", "fe"),
    ("mov R7,A", "ff"),
];

#[test]
fn every_opcode_assembles_as_as_assembles_it() {
    for (src, want) in OPCODES {
        check(&format!("\t{src}\n"), want);
    }
}

#[test]
fn the_instruction_set_is_exactly_255_opcodes() {
    let mut seen: BTreeMap<u8, Vec<&str>> = BTreeMap::new();
    mcs51::for_each_opcode(|m, _, op| seen.entry(op).or_default().push(m));
    let duplicated: Vec<_> = seen.iter().filter(|(_, v)| v.len() > 1).collect();
    assert!(duplicated.is_empty(), "listed twice: {duplicated:02x?}");
    assert_eq!(seen.len(), 255);
    assert!(!seen.contains_key(&0xa5), "A5H is the one undefined opcode");
    // And the table agrees with what the corpus line for each opcode gives.
    let from_corpus: Vec<u8> = OPCODES
        .iter()
        .map(|(_, b)| u8::from_str_radix(&b[..2], 16).unwrap())
        .collect();
    assert_eq!(from_corpus, seen.keys().copied().collect::<Vec<_>>());
    for (src, _) in OPCODES {
        let m = src.split_whitespace().next().unwrap();
        assert!(mcs51::is_mnemonic(m), "{m}");
    }
}

#[test]
fn operand_bytes_follow_the_opcode() {
    // `MOV direct,direct` carries its source first.
    check("\tMOV 30H,31H\n\tMOV ACC,ACC\n", "85 31 30 85 e0 e0");
    // 16-bit fields are high byte first; `DW` is low byte first.
    check("\tLJMP 1234H\n\tMOV DPTR,#1234H\n", "02 12 34 90 12 34");
    check(
        "\tDW 1234H\n\tDB 'AB',0\n\tDW $,LBL\nLBL:\tNOP\n",
        "34 12 41 42 00 05 00 09 00 00",
    );
    check(
        "\tLCALL -1\n\tMOV DPTR,#-1\n\tMOV A,#-128\n",
        "12 ff ff 90 ff ff 74 80",
    );
}

#[test]
fn register_and_bit_names_are_predefined_in_either_case() {
    check("\tMOV A,acc\n\tmov b,A\n\tSETB tr0\n", "e5 e0 f5 f0 d2 8c");
}

#[test]
fn bits_are_written_as_byte_dot_bit() {
    check(
        "\tMOV C,P1.3\n\tMOV ACC.7,C\n\tANL C,/B.0\n\tORL C,PSW.1\n\tCPL 20H.0\n\tCLR 2FH.7\n",
        "a2 93 92 e7 b0 f0 72 d1 b2 00 c2 7f",
    );
    // The `.` splits the whole operand, as AS splits it at its last `.`.
    check("\tSETB 20H+1.3\n\tCPL P1.1+2\n", "d2 0b b2 93");
}

#[test]
fn bit_symbols_are_defined_with_bit() {
    check(
        "FLAG BIT 21H.7\nLED BIT P1.3\n\tSETB FLAG\n\tCLR LED\n\tJB FLAG,$\n",
        "d2 0f c2 93 20 0f fd",
    );
    check("\tSETB LATER\nLATER BIT TCON.6\n", "d2 8e");
}

#[test]
fn cy_is_the_carry_where_c_could_stand() {
    check(
        "\tCLR CY\n\tSETB CY\n\tCPL CY\n\tMOV CY,21H.0\n\tMOV 21H.1,CY\n\tANL CY,/CY\n\tJNB CY,$\n",
        "c3 d3 b3 a2 08 92 09 b0 d7 30 d7 fd",
    );
}

#[test]
fn using_selects_the_bank_the_ar_names_refer_to() {
    check(
        "\tUSING 2\n\tPUSH AR1\n\tPOP AR6\n\tMOV A,AR7\n",
        "c0 11 d0 16 e5 17",
    );
}

#[test]
fn generic_jumps_take_the_form_that_reaches() {
    check(
        "\tORG 7F0H\n\tJMP 810H\n\tCALL 810H\n\tJMP 900H\n\tCALL 7F0H\n",
        "80 1e 12 08 10 02 09 00 f1 f0",
    );
    // The jump grows, which moves the call after it to 7FDH, where the
    // block after it still holds its target.
    let zeros = |n: usize| vec!["00"; n].join(" ");
    check(
        "\tORG 7C0H\n\tJMP FAR\n\tDS 3AH\n\tCALL NEAR\nNEAR:\tRET\n\tORG 1000H\nFAR:\tRET\n",
        &format!("02 10 00 {} f1 ff 22 {} 22", zeros(0x3a), zeros(0x800)),
    );
    check("\tORG 0FFF0H\n\tJMP 0\n", "02 00 00");
}

#[test]
fn relative_branches_reach_127_forward_and_128_back() {
    check(
        "\tORG 100H\n\tSJMP $+129\n\tSJMP $-126\n\tCJNE R3,#0,$-125\n\tDJNZ 30H,$+130\n",
        "80 7f 80 80 bb 00 80 d5 30 7f",
    );
}

#[test]
fn an_ajmp_takes_its_block_from_the_address_after_it() {
    // rsasm's own rule, the CPU's: the same bytes AS gives the generic `JMP`
    // and `CALL` here. AS and sdas8051 refuse the `AJMP` and `ACALL`.
    check("\tORG 7FEH\n\tAJMP 0FF0H\n", "e1 f0");
    check("\tORG 7FEH\n\tACALL 0FF0H\n", "f1 f0");
    // ...and refuse a target in the block being left, which they assemble.
    refused("\tORG 7FEH\n\tAJMP 7F0H\n", "outside the 2 KB region");
    refused("\tORG 7FFH\n\tACALL 0\n", "outside the 2 KB region");
}

#[test]
fn a_label_may_take_a_predefined_name() {
    // From sdas8051, which predefines the upper-case names only.
    check(
        "\tORG 0\n\tSJMP p1\n\tMOV A,P1\np1:\tSETB RI\nri:\tSJMP ri\n",
        "80 02 e5 90 d2 98 80 fe",
    );
}

#[test]
fn what_as_refuses_is_refused() {
    refused("\tORG 900H\n\tAJMP 0\n", "outside the 2 KB region");
    refused("\tORG 900H\n\tACALL 100H\n", "outside the 2 KB region");
    refused("\tSJMP $+130\n", "out of range");
    refused("\tORG 100H\n\tCJNE A,#1,$-126\n", "out of range");
    refused("\tSJMP $-5\n", "outside the 64 KB region");
    refused("\tJMP $-5\n", "outside the 64 KB region");
    refused("\tCALL -1\n", "outside the 64 KB region");
    refused("\tJMP 12345H\n", "outside the 64 KB region");
    refused("\tMOV A,#256\n", "out of range");
    refused("\tMOV A,#-129\n", "out of range");
    refused("\tMOV A,-1\n", "out of range");
    refused("\tMOV A,256\n", "out of range");
    refused("\tJB 100H,$\n", "out of range");
    refused("\tLJMP 10000H\n", "out of range");
    refused("\tMOV DPTR,#10000H\n", "out of range");
    refused("\tSETB P1.8\n", "a bit number must be 0 to 7");
    refused("\tMOV A,C\n", "invalid operands for `mov`");
    refused("\tMOV CY,A\n", "invalid operands for `mov`");
    refused("\tMOV A,CY\n", "invalid operands for `mov`");
    refused("\tXRL C,20H\n", "invalid operands for `xrl`");
    refused("\tDEC DPTR\n", "invalid operands for `dec`");
    refused("\tMUL A\n", "invalid operands for `mul`");
    refused("\tPUSH A\n", "invalid operands for `push`");
    refused("\tCPL DPTR\n", "invalid operands for `cpl`");
    refused("\tMOV A,@R2\n", "expected `@R0`");
    refused("\tMOVX A,@A+DPTR\n", "invalid operands for `movx`");
}

#[test]
fn what_rsasm_refuses_and_as_only_warns_about() {
    // AS warns and assembles a bit of another byte: 30H.1 is D2 81 there.
    refused("\tSETB 30H.1\n", "not bit addressable");
    refused("\tSETB 1FH.0\n", "not bit addressable");
    refused("\tSETB SBUF.1\n", "not bit addressable");
    // AS assembles `SETB A` as `DA A`.
    refused("\tSETB A\n", "invalid operands for `setb`");
}

#[test]
fn intel_hex_output_is_what_p2hex_writes() {
    let asm =
        assemble("\tORG 1235H\n\tLJMP 1235H\n\tDB 1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17\n");
    assert!(!asm.diags.has_errors());
    let text = String::from_utf8(rsasm::output::ihex::build(&asm).unwrap()).unwrap();
    assert_eq!(
        text,
        ":101235000212350102030405060708090A0B0C0D05\n\
         :041245000E0F101167\n\
         :00000001FF\n"
    );
}
