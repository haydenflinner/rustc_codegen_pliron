//! Renesas RX encoding tests.
//!
//! Every expected byte string and relocation here was produced by `rx-elf-as`
//! from GNU binutils 2.47 with its default options, the reference behind
//! `tools/xas-diff/run.sh rx`, and read back from its code section `P`. The
//! `corpus_*` tables and `programs` are `tools/xas-diff/rx.txt` and
//! `rx-programs.txt` with the reference's bytes beside each case. None of the
//! expectations came from rsasm. The few tests that cannot have a reference
//! answer — `sp`, which GNU as does not accept, and rsasm's own diagnostics —
//! compare rsasm against itself or against the reference's spelling, and say
//! so.

#![cfg(feature = "rx")]

mod common;
use common::*;
use rsasm::arch;
use rsasm::lexer::Dialect;

/// Checks a whole table, reporting every mismatch rather than the first.
#[track_caller]
fn check(cases: &[(&str, &str)]) {
    let mut failures = Vec::new();
    for (src, want) in cases {
        let got = match try_text_for("rx", src) {
            Ok(b) => hex(&b),
            Err(e) => format!("error: {e}"),
        };
        if got != *want {
            failures.push(format!("{src}\n  want: {want}\n   got: {got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

/// Expands the `xx*n` run notation used for long stretches of `.space`.
fn expand(compact: &str) -> String {
    let mut out = Vec::new();
    for tok in compact.split_whitespace() {
        match tok.split_once('*') {
            Some((byte, n)) => {
                let n: usize = n.parse().expect("run length");
                out.extend(std::iter::repeat_n(byte, n));
            }
            None => out.push(tok),
        }
    }
    out.join(" ")
}

#[track_caller]
fn check_programs(cases: &[(&str, &str, &str)]) {
    let mut failures = Vec::new();
    for (name, src, want) in cases {
        let want = expand(want);
        let got = match try_text_for("rx", src) {
            Ok(b) => hex(&b),
            Err(e) => format!("error: {e}"),
        };
        if got != want {
            failures.push(format!("[{name}]\n{src}\n  want: {want}\n   got: {got}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} programs differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

// ---- the differential corpus -------------------------------------------------

#[test]
fn corpus_no_operands() {
    check(&[
        ("brk", "00"),
        ("dbt", "01"),
        ("rts", "02"),
        ("nop", "03"),
        ("rtfi", "7f 94"),
        ("rte", "7f 95"),
        ("wait", "7f 96"),
        ("satr", "7f 93"),
        ("scmpu", "7f 83"),
        ("smovu", "7f 87"),
        ("smovb", "7f 8b"),
        ("smovf", "7f 8f"),
        ("suntil", "7f 82"),
        ("suntil.b", "7f 80"),
        ("suntil.w", "7f 81"),
        ("suntil.l", "7f 82"),
        ("swhile.b", "7f 84"),
        ("swhile.w", "7f 85"),
        ("swhile", "7f 86"),
        ("sstr.b", "7f 88"),
        ("sstr.w", "7f 89"),
        ("sstr", "7f 8a"),
        ("rmpa.b", "7f 8c"),
        ("rmpa.w", "7f 8d"),
        ("rmpa.l", "7f 8e"),
    ]);
}

#[test]
fn corpus_registers_case_and_statement_syntax() {
    check(&[
        ("mov r0, r15", "ef 0f"),
        ("mov r15, r0", "ef f0"),
        ("MOV R1, R2", "ef 12"),
        ("Mov.L r3, r4", "ef 34"),
        ("nop ! nop", "03 03"),
        ("nop ; a comment", "03"),
    ]);
}

#[test]
fn corpus_mov_immediate_to_register_by_size_class() {
    check(&[
        ("mov #0, r1", "66 01"),
        ("mov #1, r1", "66 11"),
        ("mov #15, r15", "66 ff"),
        ("mov #16, r1", "75 41 10"),
        ("mov #255, r7", "75 47 ff"),
        ("mov #256, r1", "fb 1a 00 01"),
        ("mov #-1, r1", "fb 16 ff"),
        ("mov #-128, r1", "fb 16 80"),
        ("mov #-129, r1", "fb 1a 7f ff"),
        ("mov #127, r2", "75 42 7f"),
        ("mov #0x7fff, r2", "fb 2a ff 7f"),
        ("mov #0x8000, r2", "fb 2e 00 80 00"),
        ("mov #-32768, r2", "fb 2a 00 80"),
        ("mov #-32769, r2", "fb 2e ff 7f ff"),
        ("mov #0x7fffff, r3", "fb 3e ff ff 7f"),
        ("mov #0x800000, r3", "fb 32 00 00 80 00"),
        ("mov #-0x800000, r3", "fb 3e 00 00 80"),
        ("mov #-0x800001, r3", "fb 32 ff ff 7f ff"),
        ("mov #0x7fffffff, r3", "fb 32 ff ff ff 7f"),
        ("mov #0x80000000, r3", "fb 32 00 00 00 80"),
        ("mov #0xffffffff, r3", "fb 36 ff"),
        ("mov #0xfffffffe, r3", "fb 36 fe"),
        ("mov.l #1, r1", "66 11"),
        ("mov.l #1000, r9", "fb 9a e8 03"),
        ("mov #ext, r1", "fb 12 00 00 00 00"),
        ("mov #ext+4, r2", "fb 22 00 00 00 00"),
    ]);
}

#[test]
fn corpus_mov_register_to_register() {
    check(&[
        ("mov.b r1, r2", "cf 12"),
        ("mov.w r1, r2", "df 12"),
        ("mov.l r1, r2", "ef 12"),
    ]);
}

#[test]
fn corpus_mov_register_indirect_and_displacement() {
    check(&[
        ("mov.b r1, [r2]", "c3 21"),
        ("mov.w r1, [r2]", "d3 21"),
        ("mov.l r1, [r2]", "e3 21"),
        ("mov.l r8, [r9]", "e3 98"),
        ("mov.b [r1], r2", "cc 12"),
        ("mov.w [r1], r2", "dc 12"),
        ("mov.l [r1], r2", "ec 12"),
        ("mov.l [r15], r14", "ec fe"),
        ("mov.b r1, 0[r2]", "80 21"),
        ("mov.b r1, 1[r2]", "80 29"),
        ("mov.b r1, 31[r2]", "87 a9"),
        ("mov.b r1, 32[r2]", "c7 21 20"),
        ("mov.b r1, 255[r2]", "c7 21 ff"),
        ("mov.b r1, 256[r2]", "cb 21 00 01"),
        ("mov.b r1, 65535[r2]", "cb 21 ff ff"),
        ("mov.w r1, 2[r2]", "90 29"),
        ("mov.w r1, 62[r2]", "97 a9"),
        ("mov.w r1, 64[r2]", "d7 21 20"),
        ("mov.w r1, 510[r2]", "d7 21 ff"),
        ("mov.w r1, 512[r2]", "db 21 00 01"),
        ("mov.w r1, 131070[r2]", "db 21 ff ff"),
        ("mov.l r1, 4[r2]", "a0 29"),
        ("mov.l r1, 124[r2]", "a7 a9"),
        ("mov.l r1, 128[r2]", "e7 21 20"),
        ("mov.l r1, 1020[r2]", "e7 21 ff"),
        ("mov.l r1, 1024[r2]", "eb 21 00 01"),
        ("mov.l r1, 262140[r2]", "eb 21 ff ff"),
        ("mov.l r8, 4[r2]", "e7 28 01"),
        ("mov.l r1, 4[r8]", "e7 81 01"),
        ("mov.b 4[r1], r2", "89 12"),
        ("mov.w 4[r1], r2", "98 92"),
        ("mov.l 4[r1], r2", "a8 1a"),
        ("mov.l 0[r1], r2", "a8 12"),
        ("mov.l 8[r7], r7", "a8 f7"),
        ("mov.l 8[r7], r8", "ed 78 02"),
        ("mov.l 400[r7], r3", "ed 73 64"),
        ("mov.b 300[r1], r2", "ce 12 2c 01"),
        ("mov.l 70000[r1], r2", "ee 12 5c 44"),
        ("mov 4[r1], r2", "a8 1a"),
        ("mov r1, 4[r2]", "a0 29"),
    ]);
}

#[test]
fn corpus_mov_memory_to_memory() {
    check(&[
        ("mov.b [r1], [r2]", "c0 12"),
        ("mov.w [r1], 2[r2]", "d4 12 01"),
        ("mov.l 4[r1], [r2]", "e1 12 01"),
        ("mov.l 4[r1], 8[r2]", "e5 12 01 02"),
        ("mov.b 300[r1], 1000[r2]", "ca 12 2c 01 e8 03"),
        ("mov.l [r10], [r11]", "e0 ab"),
    ]);
}

#[test]
fn corpus_mov_post_increment_pre_decrement_indexed() {
    check(&[
        ("mov.b r1, [r2+]", "fd 20 21"),
        ("mov.w r1, [r2+]", "fd 21 21"),
        ("mov.l r1, [r2+]", "fd 22 21"),
        ("mov.b r1, [-r2]", "fd 24 21"),
        ("mov.w r1, [-r2]", "fd 25 21"),
        ("mov.l r1, [-r2]", "fd 26 21"),
        ("mov.b [r1+], r2", "fd 28 12"),
        ("mov.w [r1+], r2", "fd 29 12"),
        ("mov.l [r1+], r2", "fd 2a 12"),
        ("mov.b [-r1], r2", "fd 2c 12"),
        ("mov.w [-r1], r2", "fd 2d 12"),
        ("mov.l [-r1], r2", "fd 2e 12"),
        ("mov.b r1, [ r2 + ]", "fd 20 21"),
        ("mov.b r1, [ - r2 ]", "fd 24 21"),
        ("mov.b r3, [r1, r2]", "fe 01 23"),
        ("mov.w r3, [r1, r2]", "fe 11 23"),
        ("mov.l r3, [r1, r2]", "fe 21 23"),
        ("mov.b [r1, r2], r3", "fe 41 23"),
        ("mov.w [r1, r2], r3", "fe 51 23"),
        ("mov.l [r1, r2], r3", "fe 61 23"),
        ("mov [r14, r15], r13", "fe 6e fd"),
    ]);
}

#[test]
fn corpus_mov_immediate_to_memory() {
    check(&[
        ("mov.b #1, [r1]", "f8 14 01"),
        ("mov.w #1, [r1]", "f8 15 01"),
        ("mov.l #1, [r1]", "f8 16 01"),
        ("mov.b #255, [r1]", "f8 14 ff"),
        ("mov.b #-1, [r1]", "f8 14 ff"),
        ("mov.w #0x1234, [r1]", "f8 19 34 12"),
        ("mov.w #-1, [r1]", "f8 15 ff"),
        ("mov.w #0xffff, [r1]", "f8 15 ff"),
        ("mov.l #0x12345678, [r1]", "f8 12 78 56 34 12"),
        ("mov.l #-1, [r1]", "f8 16 ff"),
        ("mov.b #1, 0[r1]", "3c 10 01"),
        ("mov.b #1, 4[r1]", "3c 14 01"),
        ("mov.b #200, 31[r7]", "3c ff c8"),
        ("mov.b #1, 32[r1]", "f9 14 20 01"),
        ("mov.b #1, 4[r8]", "f9 84 04 01"),
        ("mov.b #-1, 4[r1]", "f9 14 04 ff"),
        ("mov.w #1, 4[r1]", "3d 12 01"),
        ("mov.w #1, 62[r1]", "3d 9f 01"),
        ("mov.w #1, 64[r1]", "f9 15 20 01"),
        ("mov.w #0x1234, 4[r1]", "f9 19 02 34 12"),
        ("mov.w #256, 4[r1]", "f9 19 02 00 01"),
        ("mov.l #1, 4[r1]", "3e 11 01"),
        ("mov.l #1, 124[r1]", "3e 9f 01"),
        ("mov.l #1, 128[r1]", "f9 16 20 01"),
        ("mov.l #0x12345678, 8[r3]", "f9 32 02 78 56 34 12"),
        ("mov.l #-1, 8[r3]", "f9 36 02 ff"),
        ("mov.l #300, 8[r12]", "f9 ca 02 2c 01"),
        ("mov.b #ext, 4[r1]", "f9 14 04 00"),
        ("mov.w #ext, [r1]", "f8 11 00 00 00 00"),
        ("mov.l #ext, 4[r1]", "f9 12 01 00 00 00 00"),
    ]);
}

#[test]
fn corpus_movu() {
    check(&[
        ("movu.b r1, r2", "5b 12"),
        ("movu.w r1, r2", "5f 12"),
        ("movu r1, r2", "5f 12"),
        ("movu.b [r1], r2", "58 12"),
        ("movu.w [r1], r2", "5c 12"),
        ("movu.b 4[r1], r2", "b1 12"),
        ("movu.w 4[r1], r2", "b8 92"),
        ("movu.b 31[r7], r7", "b7 ff"),
        ("movu.b 32[r1], r2", "59 12 20"),
        ("movu.w 62[r1], r2", "bf 9a"),
        ("movu.w 64[r1], r2", "5d 12 20"),
        ("movu.b 4[r8], r2", "59 82 04"),
        ("movu.w 1000[r1], r2", "5e 12 f4 01"),
        ("movu.b [r1+], r2", "fd 38 12"),
        ("movu.w [r1+], r2", "fd 39 12"),
        ("movu.b [-r1], r2", "fd 3c 12"),
        ("movu.w [-r1], r2", "fd 3d 12"),
        ("movu.b [r1, r2], r3", "fe c1 23"),
        ("movu.w [r1, r2], r3", "fe d1 23"),
    ]);
}

#[test]
fn corpus_stack() {
    check(&[
        ("push r1", "7e a1"),
        ("push.b r1", "7e 81"),
        ("push.w r1", "7e 91"),
        ("push.l r15", "7e af"),
        ("push.b [r1]", "f4 18"),
        ("push.w 2[r1]", "f5 19 01"),
        ("push.l 400[r1]", "f5 1a 64"),
        ("push 4[r2]", "f5 2a 01"),
        ("pop r1", "7e b1"),
        ("pop r15", "7e bf"),
        ("pushc psw", "7e c0"),
        ("pushc pc", "7e c1"),
        ("pushc usp", "7e c2"),
        ("pushc fpsw", "7e c3"),
        ("pushc bpsw", "7e c8"),
        ("pushc bpc", "7e c9"),
        ("pushc isp", "7e ca"),
        ("pushc fintv", "7e cb"),
        ("pushc intb", "7e cc"),
        ("popc psw", "7e e0"),
        ("popc usp", "7e e2"),
        ("popc isp", "7e ea"),
        ("popc intb", "7e ec"),
        ("pushm r1-r1", "7e a1"),
        ("pushm r1-r2", "6e 12"),
        ("pushm r1-r15", "6e 1f"),
        ("pushm r6-r13", "6e 6d"),
        ("popm r1-r1", "7e b1"),
        ("popm r1-r15", "6f 1f"),
        ("popm r6-r13", "6f 6d"),
        ("rtsd #0", "67 00"),
        ("rtsd #8", "67 02"),
        ("rtsd #1020", "67 ff"),
        ("rtsd #8, r1-r7", "3f 17 02"),
        ("rtsd #1020, r15-r15", "3f ff ff"),
    ]);
}

#[test]
fn corpus_exchange_and_conversion() {
    check(&[
        ("xchg r1, r2", "fc 43 12"),
        ("xchg [r1].ub, r2", "fc 40 12"),
        ("xchg 4[r1].ub, r2", "fc 41 12 04"),
        ("xchg [r1], r2", "06 a0 10 12"),
        ("xchg [r1].b, r2", "06 20 10 12"),
        ("xchg 2[r1].w, r2", "06 61 10 12 01"),
        ("xchg 4[r1].l, r2", "06 a1 10 12 01"),
        ("xchg 4[r1], r2", "06 a1 10 12 01"),
        ("xchg 2[r1].uw, r2", "06 e1 10 12 01"),
        ("xchg 400[r1].l, r2", "06 a1 10 12 64"),
        ("itof r1, r2", "fc 47 12"),
        ("itof 4[r1].ub, r2", "fc 45 12 04"),
        ("itof 4[r1], r2", "06 a1 11 12 01"),
        ("itof 2[r1].w, r2", "06 61 11 12 01"),
        ("utof r1, r2", "fc 57 12"),
        ("utof [r1].ub, r2", "fc 54 12"),
        ("utof 4[r1].l, r2", "06 a1 15 12 01"),
    ]);
}

#[test]
fn corpus_add_sub_mul_and_or() {
    check(&[
        ("add r1, r2", "4b 12"),
        ("sub r1, r2", "43 12"),
        ("mul r1, r2", "4f 12"),
        ("and r1, r2", "53 12"),
        ("or r1, r2", "57 12"),
        ("add [r1].ub, r2", "48 12"),
        ("sub 4[r1].ub, r2", "41 12 04"),
        ("mul 300[r1].ub, r2", "4e 12 2c 01"),
        ("and [r1].ub, r2", "50 12"),
        ("or 1[r1].ub, r2", "55 12 01"),
        ("add [r1], r2", "06 88 12"),
        ("add [r1].b, r2", "06 08 12"),
        ("add 4[r1].w, r2", "06 49 12 02"),
        ("add 8[r1].l, r2", "06 89 12 02"),
        ("add 2[r1].uw, r2", "06 c9 12 01"),
        ("sub 8[r1], r2", "06 81 12 02"),
        ("mul 8[r1].w, r2", "06 4d 12 04"),
        ("and 400[r1].l, r2", "06 91 12 64"),
        ("or 4[r1].b, r2", "06 15 12 04"),
        ("add r1, r2, r3", "ff 23 12"),
        ("sub r1, r2, r3", "ff 03 12"),
        ("mul r1, r2, r3", "ff 33 12"),
        ("and r1, r2, r3", "ff 43 12"),
        ("or r1, r2, r3", "ff 53 12"),
        ("add #0, r1", "62 01"),
        ("add #15, r1", "62 f1"),
        ("add #16, r1", "71 11 10"),
        ("add #-1, r1", "71 11 ff"),
        ("add #127, r1", "71 11 7f"),
        ("add #128, r1", "72 11 80 00"),
        ("add #0x7fff, r1", "72 11 ff 7f"),
        ("add #0x8000, r1", "73 11 00 80 00"),
        ("add #0x800000, r1", "70 11 00 00 80 00"),
        ("add #0x80000000, r1", "70 11 00 00 00 80"),
        ("add #ext, r1", "70 11 00 00 00 00"),
        ("add #1, r1, r2", "71 12 01"),
        ("add #0, r1, r2", "71 12 00"),
        ("add #1000, r1, r2", "72 12 e8 03"),
        ("add #0x12345678, r1, r2", "70 12 78 56 34 12"),
        ("sub #0, r1", "60 01"),
        ("sub #15, r1", "60 f1"),
        ("sub #16, r1", "71 11 f0"),
        ("sub #128, r1", "71 11 80"),
        ("sub #129, r1", "72 11 7f ff"),
        ("sub #-1, r1", "71 11 01"),
        ("sub #0x10000, r1", "73 11 00 00 ff"),
        ("mul #0, r1", "63 01"),
        ("mul #15, r1", "63 f1"),
        ("mul #16, r1", "75 11 10"),
        ("mul #-1, r1", "75 11 ff"),
        ("mul #1000, r1", "76 11 e8 03"),
        ("and #15, r1", "64 f1"),
        ("and #0xff, r1", "76 21 ff 00"),
        ("and #0xffff, r1", "77 21 ff ff 00"),
        ("and #-16, r1", "75 21 f0"),
        ("or #1, r1", "65 11"),
        ("or #0x100, r1", "76 31 00 01"),
        ("or #0x80000000, r1", "74 31 00 00 00 80"),
        ("mul #ext, r1", "74 11 00 00 00 00"),
        ("and #ext, r1", "74 21 00 00 00 00"),
        ("or #ext, r1", "74 31 00 00 00 00"),
    ]);
}

#[test]
fn corpus_cmp() {
    check(&[
        ("cmp r1, r2", "47 12"),
        ("cmp [r1].ub, r2", "44 12"),
        ("cmp 4[r1].ub, r2", "45 12 04"),
        ("cmp [r1], r2", "06 84 12"),
        ("cmp 4[r1].b, r2", "06 05 12 04"),
        ("cmp 4[r1].w, r2", "06 45 12 02"),
        ("cmp 4[r1].l, r2", "06 85 12 01"),
        ("cmp 2[r1].uw, r2", "06 c5 12 01"),
        ("cmp #0, r1", "61 01"),
        ("cmp #15, r1", "61 f1"),
        ("cmp #16, r1", "75 51 10"),
        ("cmp #255, r1", "75 51 ff"),
        ("cmp #256, r1", "76 01 00 01"),
        ("cmp #-1, r1", "75 01 ff"),
        ("cmp #-128, r1", "75 01 80"),
        ("cmp #0x12345, r1", "77 01 45 23 01"),
        ("cmp #ext, r1", "74 01 00 00 00 00"),
    ]);
}

#[test]
fn corpus_adc_sbb() {
    check(&[
        ("adc r1, r2", "fc 0b 12"),
        ("adc [r1], r2", "06 a0 02 12"),
        ("adc 4[r1], r2", "06 a1 02 12 01"),
        ("adc 4[r1].l, r2", "06 a1 02 12 01"),
        ("adc #1, r1", "fd 74 21 01"),
        ("adc #1000, r1", "fd 78 21 e8 03"),
        ("adc #ext, r1", "fd 70 21 00 00 00 00"),
        ("sbb r1, r2", "fc 03 12"),
        ("sbb [r1], r2", "06 a0 00 12"),
        ("sbb 400[r1].l, r2", "06 a1 00 12 64"),
        ("sbb #1, r1", "fd 74 21 fe"),
        ("sbb #0, r1", "fd 74 21 ff"),
        ("sbb #-1, r1", "fd 74 21 00"),
        ("sbb #1000, r1", "fd 78 21 17 fc"),
    ]);
}

#[test]
fn corpus_neg_not_abs() {
    check(&[
        ("neg r1", "7e 11"),
        ("neg r1, r2", "fc 07 12"),
        ("not r1", "7e 01"),
        ("not r1, r2", "fc 3b 12"),
        ("abs r1", "7e 21"),
        ("abs r1, r2", "fc 0f 12"),
    ]);
}

#[test]
fn corpus_max_min_emul_emulu_div_divu_tst_xor() {
    check(&[
        ("max r1, r2", "fc 13 12"),
        ("min r1, r2", "fc 17 12"),
        ("emul r1, r2", "fc 1b 12"),
        ("emulu r1, r2", "fc 1f 12"),
        ("div r1, r2", "fc 23 12"),
        ("divu r1, r2", "fc 27 12"),
        ("tst r1, r2", "fc 33 12"),
        ("xor r1, r2", "fc 37 12"),
        ("max [r1].ub, r2", "fc 10 12"),
        ("min 4[r1].ub, r2", "fc 15 12 04"),
        ("emul [r1], r2", "06 a0 06 12"),
        ("emulu 4[r1].w, r2", "06 61 07 12 02"),
        ("div 8[r1].l, r2", "06 a1 08 12 02"),
        ("divu 2[r1].uw, r2", "06 e1 09 12 01"),
        ("tst 4[r1].b, r2", "06 21 0c 12 04"),
        ("xor 400[r1], r2", "06 a1 0d 12 64"),
        ("max #1, r1", "fd 74 41 01"),
        ("min #-1, r1", "fd 74 51 ff"),
        ("emul #1000, r1", "fd 78 61 e8 03"),
        ("emulu #0x12345678, r1", "fd 70 71 78 56 34 12"),
        ("div #127, r1", "fd 74 81 7f"),
        ("divu #128, r1", "fd 78 91 80 00"),
        ("tst #0xff, r1", "fd 78 c1 ff 00"),
        ("xor #0x8000, r1", "fd 7c d1 00 80 00"),
        ("max #ext, r1", "fd 70 41 00 00 00 00"),
        ("tst #ext, r1", "fd 70 c1 00 00 00 00"),
    ]);
}

#[test]
fn corpus_stz_stnz() {
    check(&[
        ("stz #1, r1", "fd 74 e1 01"),
        ("stz #1000, r2", "fd 78 e2 e8 03"),
        ("stnz #-1, r3", "fd 74 f3 ff"),
        ("stnz #0x12345678, r4", "fd 70 f4 78 56 34 12"),
        ("stz #ext, r1", "fd 70 e1 00 00 00 00"),
    ]);
}

#[test]
fn corpus_shifts_and_rotates() {
    check(&[
        ("shlr #0, r1", "68 01"),
        ("shlr #1, r1", "68 11"),
        ("shlr #31, r15", "69 ff"),
        ("shar #1, r1", "6a 11"),
        ("shar #16, r2", "6b 02"),
        ("shll #1, r1", "6c 11"),
        ("shll #31, r2", "6d f2"),
        ("shlr #3, r1, r2", "fd 83 12"),
        ("shar #3, r1, r2", "fd a3 12"),
        ("shll #31, r1, r2", "fd df 12"),
        ("shlr r1, r2", "fd 60 12"),
        ("shar r1, r2", "fd 61 12"),
        ("shll r1, r2", "fd 62 12"),
        ("rotl #0, r1", "fd 6e 01"),
        ("rotl #1, r1", "fd 6e 11"),
        ("rotl #31, r2", "fd 6f f2"),
        ("rotr #1, r1", "fd 6c 11"),
        ("rotr #17, r2", "fd 6d 12"),
        ("rotl r1, r2", "fd 66 12"),
        ("rotr r1, r2", "fd 64 12"),
        ("revw r1, r2", "fd 65 12"),
        ("revl r1, r2", "fd 67 12"),
        ("rolc r1", "7e 51"),
        ("rorc r2", "7e 42"),
        ("sat r3", "7e 33"),
    ]);
}

#[test]
fn corpus_bit_operations() {
    check(&[
        ("bset #0, r1", "78 01"),
        ("bset #31, r15", "79 ff"),
        ("bclr #1, r2", "7a 12"),
        ("bclr #16, r2", "7b 02"),
        ("btst #5, r3", "7c 53"),
        ("btst #31, r3", "7d f3"),
        ("bnot #0, r1", "fd e0 f1"),
        ("bnot #31, r2", "fd ff f2"),
        ("bset #1, [r1].b", "f0 11"),
        ("bset #7, 4[r1].b", "f1 17 04"),
        ("bclr #0, 300[r2].b", "f2 28 2c 01"),
        ("btst #3, 4[r1].b", "f5 13 04"),
        ("bnot #1, 4[r1]", "fc e5 1f 04"),
        ("bnot #7, [r1].b", "fc fc 1f"),
        ("bset r1, r2", "fc 63 21"),
        ("bclr r1, r2", "fc 67 21"),
        ("btst r1, r2", "fc 6b 21"),
        ("bnot r1, r2", "fc 6f 21"),
        ("bset r1, [r2]", "fc 60 21"),
        ("bclr r1, 4[r2]", "fc 65 21 04"),
        ("btst r1, 4[r2].b", "fc 69 21 04"),
        ("bnot r1, 300[r2]", "fc 6e 21 2c 01"),
        ("bmc #1, r2", "fd e1 22"),
        ("bmnc #31, r2", "fd ff 32"),
        ("bmeq #3, r1", "fd e3 01"),
        ("bmne #3, r1", "fd e3 11"),
        ("bmz #0, r1", "fd e0 01"),
        ("bmnz #0, r1", "fd e0 11"),
        ("bmgeu #1, r1", "fd e1 21"),
        ("bmltu #1, r1", "fd e1 31"),
        ("bmgtu #1, r1", "fd e1 41"),
        ("bmleu #1, r1", "fd e1 51"),
        ("bmpz #1, r1", "fd e1 61"),
        ("bmn #1, r1", "fd e1 71"),
        ("bmge #1, r1", "fd e1 81"),
        ("bmlt #1, r1", "fd e1 91"),
        ("bmgt #1, r1", "fd e1 a1"),
        ("bmle #1, r1", "fd e1 b1"),
        ("bmo #1, r1", "fd e1 c1"),
        ("bmno #1, r1", "fd e1 d1"),
        ("bmc #1, 4[r1]", "fc e5 12 04"),
        ("bmeq #7, [r1].b", "fc fc 10"),
        ("bmne #0, 300[r2]", "fc e2 21 2c 01"),
    ]);
}

#[test]
fn corpus_store_condition() {
    check(&[
        ("sceq.l r1", "fc db 10"),
        ("scne.l r2", "fc db 21"),
        ("scgt.l r3", "fc db 3a"),
        ("sceq [r1]", "fc d8 10"),
        ("sceq.b [r1]", "fc d0 10"),
        ("sceq.w 2[r1]", "fc d5 10 01"),
        ("sceq.l 4[r1]", "fc d9 10 01"),
        ("scle.b 4[r1]", "fc d1 1b 04"),
        ("scltu.w 400[r1]", "fc d5 13 c8"),
    ]);
}

#[test]
fn corpus_system_and_register_transfer() {
    check(&[
        ("int #0", "75 60 00"),
        ("int #255", "75 60 ff"),
        ("int #ext", "75 60 00"),
        ("mvtipl #0", "75 70 00"),
        ("mvtipl #15", "75 70 0f"),
        ("setpsw c", "7f a0"),
        ("setpsw z", "7f a1"),
        ("setpsw s", "7f a2"),
        ("setpsw o", "7f a3"),
        ("setpsw i", "7f a8"),
        ("setpsw u", "7f a9"),
        ("clrpsw c", "7f b0"),
        ("clrpsw i", "7f b8"),
        ("clrpsw u", "7f b9"),
        ("mvtc r1, psw", "fd 68 10"),
        ("mvtc r2, usp", "fd 68 22"),
        ("mvtc r3, isp", "fd 68 3a"),
        ("mvtc r4, intb", "fd 68 4c"),
        ("mvtc r5, bpc", "fd 68 59"),
        ("mvtc r6, fintv", "fd 68 6b"),
        ("mvtc r7, fpsw", "fd 68 73"),
        ("mvtc #1, psw", "fd 77 00 01"),
        ("mvtc #0x10000, intb", "fd 7f 0c 00 00 01"),
        ("mvtc #-1, usp", "fd 77 02 ff"),
        ("mvtc #ext, intb", "fd 73 0c 00 00 00 00"),
        ("mvfc psw, r1", "fd 6a 01"),
        ("mvfc pc, r2", "fd 6a 12"),
        ("mvfc usp, r3", "fd 6a 23"),
        ("mvfc isp, r4", "fd 6a a4"),
        ("mvfc intb, r5", "fd 6a c5"),
        ("mvfc bpsw, r6", "fd 6a 86"),
    ]);
}

#[test]
fn corpus_jumps_and_register_branches() {
    check(&[
        ("jmp r1", "7f 01"),
        ("jmp r15", "7f 0f"),
        ("jsr r1", "7f 11"),
        ("jsr r15", "7f 1f"),
        ("bra r1", "7f 41"),
        ("bra.l r2", "7f 42"),
        ("bsr r3", "7f 53"),
        ("bsr.l r4", "7f 54"),
    ]);
}

#[test]
fn corpus_branches_to_external_symbols_take_their_widest_form() {
    check(&[
        ("bra ext", "04 00 00 00"),
        ("bsr ext", "05 00 00 00"),
        ("bra.a ext", "04 00 00 00"),
        ("bsr.a ext", "05 00 00 00"),
        ("bra.b ext", "2e 00"),
        ("bra.w ext", "38 00 00"),
        ("bsr.w ext", "39 00 00"),
        ("beq.b ext", "20 00"),
        ("bne.b ext", "21 00"),
        ("bgt.b ext", "2a 00"),
        ("beq.w ext", "3a 00 00"),
        ("bne.w ext", "3b 00 00"),
        ("bra.s ext", "08"),
        ("beq.s ext", "10"),
        ("bne.s ext", "18"),
    ]);
}

#[test]
fn corpus_floating_point() {
    check(&[
        ("fadd r1, r2", "fc 8b 12"),
        ("fsub r1, r2", "fc 83 12"),
        ("fmul r1, r2", "fc 8f 12"),
        ("fdiv r1, r2", "fc 93 12"),
        ("fcmp r1, r2", "fc 87 12"),
        ("ftoi r1, r2", "fc 97 12"),
        ("round r1, r2", "fc 9b 12"),
        ("fsqrt r1, r2", "fc a3 12"),
        ("ftou r1, r2", "fc a7 12"),
        ("fadd [r1], r2", "fc 88 12"),
        ("fsub 4[r1], r2", "fc 81 12 01"),
        ("fmul 4[r1].l, r2", "fc 8d 12 01"),
        ("fdiv 400[r1], r2", "fc 91 12 64"),
        ("fcmp [r1].l, r2", "fc 84 12"),
        ("ftoi 8[r1], r2", "fc 95 12 02"),
        ("round [r1], r2", "fc 98 12"),
        ("fadd #0x3fc00000, r1", "fd 72 21 00 00 c0 3f"),
        ("fsub #1, r2", "fd 72 02 01 00 00 00"),
        ("fmul #ext, r3", "fd 72 33 00 00 00 00"),
        ("fdiv #0, r4", "fd 72 44 00 00 00 00"),
        ("fcmp #-1, r5", "fd 72 15 ff ff ff ff"),
    ]);
}

#[test]
fn corpus_dsp() {
    check(&[
        ("mulhi r1, r2", "fd 00 12"),
        ("mullo r1, r2", "fd 01 12"),
        ("machi r1, r2", "fd 04 12"),
        ("maclo r1, r2", "fd 05 12"),
        ("mvtachi r1", "fd 17 01"),
        ("mvtaclo r2", "fd 17 12"),
        ("mvfachi r3", "fd 1f 03"),
        ("mvfaclo r4", "fd 1f 14"),
        ("mvfacmi r5", "fd 1f 25"),
        ("racw #1", "fd 18 00"),
        ("racw #2", "fd 18 10"),
    ]);
}

#[test]
fn corpus_data() {
    check(&[
        (".byte 1, 2, 0xff", "01 02 ff"),
        (".short 0x1234", "34 12"),
        (".word 0x12345678", "78 56 34 12"),
        (".long 0x12345678", "78 56 34 12"),
        (".byte ext", "00"),
        (".short ext", "00 00"),
        (".long ext", "00 00 00 00"),
    ]);
}

/// Relaxation as GNU as's RX port does it: sizes are re-picked every pass, so
/// they can shrink as well as grow, in its walk order and with its limits.
/// Expected bytes are `rx-elf-as` output (the same programs are in
/// `tools/xas-diff/rx-programs.txt`).
#[test]
fn relaxation_follows_gnu_as() {
    check_programs(&[
        (
            "a branch too close for .s at first shrinks back once the code grows",
            "\tbne 1f\n\tbra x\n1:\trts\n",
            "1d 04 00 00 00 02",
        ),
        (
            "two branches that both settle on their short forms",
            "\tbne 1f\n\tbra 2f\n1:\trts\n2:\trts\n",
            "1b 2e 03 02 02",
        ),
        (
            "bra.s over two bsr that grow to bsr.a",
            "\tbra 1f\n\tbsr x\n\tbsr x\n1:\trts\n",
            "09 05 00 00 00 05 00 00 00 02",
        ),
        (
            "a target carried past the branch by earlier growth is not moved",
            "\tbra L1\n\tbeq L2\nL2:\n\tbo L0\n\tbsr ext\n\tbc L0\nL0:\n\tbne L4\n\t.balign 4\nL4:\n\trts\n\t.space 114\nL1:\n",
            "38 83 00 20 02 2c 08 05 00 00 00 22 02 21 03 03 02 00*114 03",
        ),
        (
            "the bra.w pair stops three bytes short of its field's reach",
            "\tbgt L0\n\t.space 32762\nL0:\n",
            "2b 05 38 fd 7f 00*32762",
        ),
        (
            "one byte further takes the bra.a pair",
            "\tbgt L0\n\t.space 32763\nL0:\n",
            "2b 06 04 ff 7f 00*32764",
        ),
        (
            "sizes that flip around an alignment settle as GNU as's do",
            "\tnop\n\tbsr ext\n\tblt L0\n\tbsr ext\n\tbno L0\n\tbc L0\n\t.space 123\n\t.balign 4\nL0:\n\tbge L0\n\tadd #1,r3\n\tbsr ext\n\tmov.l r1,r2\n\tbo L0\n\tbsr L0\n\tbn L0\n\tbno L0\n\t.space 32760\n\t.space 32763\n\tbsr ext\n\tble L0\n\tbra L0\n\trts\n\t.space 1\n\t.space 8\n",
            "03 05 00 00 00 28 05 38 8d 00 05 00 00 00 2c 05 38 84 00 23 05 38 7f 00*124 03 28 00 62 13 05 00 00 00 ef 12 2c f6 39 f4 ff 27 f1 2d ef 00*65523 05 00 00 00 2a 06 04 f4 ff fe 04 f0 ff fe 02 00*9 ef 00",
        ),
    ]);
}

/// Differences of labels as GNU as reads them: a constant, with the short
/// forms, where its expression parser could fold one, which it cannot across
/// anything that may change size; otherwise an immediate relaxed with its own
/// rules, or a 16-bit displacement. Expected bytes are `rx-elf-as` output (the
/// same programs are in `tools/xas-diff/rx-programs.txt`).
#[test]
fn label_differences_follow_gnu_as() {
    check_programs(&[
        (
            "a difference across an instruction with a displacement is not folded",
            "s:\n\tadd 4[r1], r2\ne:\n\tmov #e-s, r1\n",
            "06 89 12 01 fb 16 04",
        ),
        (
            "a difference across a plain [reg] operand is folded",
            "s:\n\tmov.l [r1], r2\ne:\n\tmov #e-s, r1\n",
            "ec 12 66 21",
        ),
        (
            "a displacement written as zero still stops folding",
            "s:\n\tmov.l r1, 0[r8]\ne:\n\tmov #e-s, r1\n",
            "e3 81 fb 16 02",
        ),
        (
            "a zero displacement the short mov takes does not stop folding",
            "s:\n\tmov.l 0[r1], r2\ne:\n\tmov #e-s, r1\n",
            "a8 12 66 21",
        ),
        (
            "the short mov to a displacement does not stop folding",
            "s:\n\tmov.b #1, 4[r1]\ne:\n\tcmp #e-s, r1\n",
            "3c 14 01 61 31",
        ),
        (
            "a difference across an alignment is not folded",
            "s:\n\tnop\n\t.balign 4\ne:\n\tmov #e-s, r1\n",
            "03 fc 13 00 fb 16 04 03",
        ),
        (
            "constant LEB128 values do not stop folding",
            "s:\n\t.uleb128 300\n\t.sleb128 -1000\ne:\n\tmov #e-s, r1\n",
            "ac 02 98 78 66 41",
        ),
        (
            "a difference across a symbolic immediate is not folded",
            "s:\n\tmov #ext, r1\ne:\n\tmov #e-s, r2\n",
            "fb 12 00 00 00 00 fb 26 06",
        ),
        (
            "a symbol minus itself is folded before it is defined",
            "\tmov #x-x, r1\n\tcmp #x-x+20, r2\nx:\n",
            "66 01 75 52 14",
        ),
        (
            "a difference from . folds like one from a label",
            "s:\n\t.space 20\n\tmov #.-s, r1\n",
            "00*20 75 41 14",
        ),
        (
            "a difference of labels as a displacement is 16 bits and not divided",
            "\tmov.l (e-s)[r1], r2\n\tmov.b (e-s)[r1], r2\n\tadd (e-s)[r1].w, r2\ns:\n\t.space 200\ne:\n",
            "ee 12 c8 00 ce 12 c8 00 06 4a 12 c8 00*201",
        ),
        (
            "a folded difference as a displacement is divided and can be short",
            "s:\n\t.space 8\ne:\n\tmov.l (e-s)[r1], r2\n\tadd (e-s)[r3].w, r4\n",
            "00*8 a8 92 06 49 34 04",
        ),
        (
            "an immediate and a displacement that are both differences relax together",
            "\tmov.l #e-s, (e-s)[r1]\ns:\n\t.space 200\ne:\n",
            "fa 1a c8 00 c8 00*201",
        ),
        (
            "a byte-sized difference wraps where GNU as's fixup lets it",
            "\tint #s-e\n\tmov.b #s-e, 4[r1]\ns:\n\t.space 200\ne:\n",
            "75 60 38 f9 14 04 38 00*200",
        ),
        (
            "sbb, shift counts and rtsd take folded differences",
            "s:\n\t.space 4\ne:\n\tsbb #e-s, r1\n\tshlr #e-s, r2\n\trtsd #e-s\n",
            "00 00 00 00 fd 74 21 fb 68 42 67 01",
        ),
        (
            "a difference with a global label is 32 bits",
            "\t.global g\n\tmov #g-s, r1\ns:\n\t.space 20\ng:\n",
            "fb 12 14 00*23",
        ),
        (
            "a difference of labels in another section is 32 bits",
            "\tmov #e-s, r1\n\t.data\ns:\n\t.long 1, 2\ne:\n",
            "fb 12 08 00 00 00",
        ),
        (
            "a difference from . in data resolves in the section",
            "\t.long e - .\n\t.short e - .\n\t.byte e - .\n\tbra e\ne:\n",
            "09 00 00 00 05 00 03 2e 02",
        ),
        (
            "a difference straddling a branch is sized once the branch is",
            "\tmov #e-s, r1\ns:\n\tbne far\n\t.space 125\ne:\n\t.space 32768\nfar:\n",
            "fb 1a 82 00 15 04 81 80 00*32894",
        ),
        (
            "a symbolic immediate after a displacement starts at one byte, as GNU as estimates it",
            "\tmov.l #e-s, 12[r3]\n\t.space 6\n\tbra e\n\t.balign 4\ns:\ne:\n",
            "f9 32 03 00*10 2e 03 03",
        ),
        (
            "growth before an immediate is added to a negative difference ahead of it",
            "\tmov.l #a-b, 4[r1]\n\tmov #b-c, r1\nb:\n\t.balign 2\na:\n\t.space 128\nc:\n",
            "f9 12 01 00 00 00 00 fb 16 80 00*128",
        ),
        (
            "a symbol set to a folded difference is a constant",
            "msg:\n\t.ascii \"hello\"\nlen = . - msg\n\tmov #len, r1\n\tcmp #len+1, r2\n",
            "68 65 6c 6c 6f 66 51 61 62",
        ),
        (
            "a symbol set to a difference that does not fold is 32 bits",
            "s:\n\tbra far\ne:\nd = e - s\n\tmov #d, r3\n\t.space 200\nfar:\n",
            "38 d1 00 fb 32 03 00*203",
        ),
        (
            "a symbol set before its labels are defined is 32 bits",
            "size = e - s\ns:\n\t.space 3\ne:\n\tmov #size, r1\n",
            "00 00 00 fb 12 03 00 00 00",
        ),
        (
            "sub of a symbol set later is a negated 32-bit add",
            "\tsub #n, r3\nn = 300\n",
            "70 33 d4 fe ff ff",
        ),
    ]);
}

/// The one place rsasm departs from that on purpose: GNU as gives a target
/// 32,769 bytes back the `bra.w` pair, whose field then wraps to +32,767.
#[test]
fn a_conditional_just_out_of_bra_w_reach_backwards_still_reaches() {
    let bytes = text_for("rx", "L0:\n\t.space 32767\n\tbgt L0\n");
    let at = 32767;
    // `ble .+6` over `bra.a`, measured from the `bra.a` opcode.
    assert_eq!(&bytes[at..at + 3], &[0x2b, 0x06, 0x04]);
    let field = &bytes[at + 3..at + 6];
    let disp = i32::from_le_bytes([field[0], field[1], field[2], 0]) << 8 >> 8;
    assert_eq!(disp, -(at as i32 + 2));
}

#[test]
fn programs() {
    check_programs(&[
        (
            "bra to the next instruction is bra.b",
            "\tbra next\nnext:\n\tnop\n",
            "2e 02 03",
        ),
        (
            "bra over two bytes is still bra.b",
            "\tbra next\n\tnop\n\tnop\nnext:\n\tnop\n",
            "0b 03 03 03",
        ),
        (
            "bra.s reaches three bytes",
            "\tbra next\n\tnop\n\tnop\nnext:\n\trts\n",
            "0b 03 03 02",
        ),
        (
            "bra.s at its longest, ten bytes",
            "\tbra next\n\tmov.l #0x12345678, [r1]\n\tnop\n\tnop\n\tnop\nnext:\n\trts\n",
            "0a f8 12 78 56 34 12 03 03 03 02",
        ),
        (
            "bra.b just past bra.s",
            "\tbra next\n\tmov.l #0x12345678, [r1]\n\tnop\n\tnop\n\tnop\n\tnop\nnext:\n\trts\n",
            "2e 0c f8 12 78 56 34 12 03 03 03 03 02",
        ),
        (
            "bra backwards is bra.b",
            "top:\n\tnop\n\tbra top\n",
            "03 2e ff",
        ),
        (
            "bra.b at its forward limit",
            "\tbra next\n\t.space 125\nnext:\n\trts\n",
            "2e 7f 00*125 02",
        ),
        (
            "bra.w just past bra.b forwards",
            "\tbra next\n\t.space 126\nnext:\n\trts\n",
            "38 81 00*127 02",
        ),
        (
            "bra.b at its backward limit",
            "top:\n\t.space 128\n\tbra top\n",
            "00*128 2e 80",
        ),
        (
            "bra.w just past bra.b backwards",
            "top:\n\t.space 129\n\tbra top\n",
            "00*129 38 7f ff",
        ),
        (
            "bra.w at its forward limit",
            "\tbra next\n\t.space 32764\nnext:\n\trts\n",
            "38 ff 7f 00*32764 02",
        ),
        (
            "bra.a just past bra.w",
            "\tbra next\n\t.space 32765\nnext:\n\trts\n",
            "04 01 80 00*32766 02",
        ),
        (
            "bra.a backwards",
            "top:\n\t.space 40000\n\tbra top\n",
            "00*40000 04 c0 63 ff",
        ),
        (
            "explicit branch sizes to a label",
            "\tbra.b next\n\tbra.w next\n\tbra.a next\n\tbsr.w next\n\tbsr.a next\n\tbeq.b next\n\tbne.w next\n\tbgt.b next\nnext:\n\trts\n",
            "2e 17 38 15 00 04 12 00 00 39 0e 00 05 0b 00 00 20 07 3b 05 00 2a 02 02",
        ),
        (
            "explicit short branches",
            "\tbra.s next\n\tbeq.s next\n\tbne.s next\n\tnop\n\tnop\n\tnop\nnext:\n\trts\n",
            "0e 15 1c 03 03 03 02",
        ),
        (
            "bsr is at least bsr.w",
            "\tbsr func\n\trts\nfunc:\n\trts\n",
            "39 04 00 02 02",
        ),
        ("bsr backwards", "func:\n\trts\n\tbsr func\n", "02 39 ff ff"),
        (
            "bsr.a past 32K",
            "\tbsr func\n\t.space 40000\nfunc:\n\trts\n",
            "05 44 9c 00*40001 02",
        ),
        (
            "beq and bne to the next instruction",
            "\tbeq a\na:\n\tbne b\nb:\n\trts\n",
            "20 02 21 02 02",
        ),
        (
            "beq.s and bne.s",
            "\tbeq a\n\tnop\n\tnop\na:\n\tbne b\n\tmov.l #0x12345678, [r1]\n\tnop\n\tnop\n\tnop\nb:\n\trts\n",
            "13 03 03 1a f8 12 78 56 34 12 03 03 03 02",
        ),
        (
            "beq.w",
            "\tbeq far\n\t.space 200\nfar:\n\trts\n",
            "3a cb 00*201 02",
        ),
        (
            "bne.w backwards",
            "far:\n\t.space 200\n\tbne far\n",
            "00*200 3b 38 ff",
        ),
        (
            "beq past 32K becomes bne.s over bra.a",
            "\tbeq far\n\t.space 40000\nfar:\n\trts\n",
            "1d 04 44 9c 00*40001 02",
        ),
        (
            "bne past 32K becomes beq.s over bra.a",
            "\tbne far\n\t.space 40000\nfar:\n\trts\n",
            "15 04 44 9c 00*40001 02",
        ),
        (
            "beq backwards past 32K",
            "far:\n\t.space 40000\n\tbeq far\n",
            "00*40000 1d 04 bf 63 ff",
        ),
        (
            "every condition, near",
            "\tbgeu t\n\tbltu t\n\tbgtu t\n\tbleu t\n\tbpz t\n\tbn t\n\tbge t\n\tblt t\n\tbgt t\n\tble t\n\tbo t\n\tbno t\n\tbc t\n\tbnc t\n\tbz t\n\tbnz t\n\tnop\n\tnop\nt:\n\trts\n",
            "22 20 23 1e 24 1c 25 1a 26 18 27 16 28 14 29 12 2a 10 2b 0e 2c 0c 2d 0a 22 08 23 06 14 1b 03 03 02",
        ),
        (
            "bgt past 127 is an inverted bgt.b over bra.w",
            "\tbgt far\n\t.space 200\nfar:\n\trts\n",
            "2b 05 38 cb 00*201 02",
        ),
        (
            "ble backwards past 128",
            "far:\n\t.space 200\n\tble far\n",
            "00*200 2a 05 38 36 ff",
        ),
        (
            "bltu past 32K is an inverted branch over bra.a",
            "\tbltu far\n\t.space 40000\nfar:\n\trts\n",
            "22 06 04 44 9c 00*40001 02",
        ),
        (
            "bgeu backwards past 32K",
            "far:\n\t.space 40000\n\tbgeu far\n",
            "00*40000 23 06 04 be 63 ff",
        ),
        (
            "a relaxation chain: growing one branch pushes another out of range",
            "\tbra b\n\tbra c\n\t.space 120\nb:\n\t.space 3\nc:\n\trts\n",
            "2e 7c 2e 7d 00*123 02",
        ),
        (
            "a loop",
            "\tmov #10, r1\nloop:\n\tsub #1, r1\n\tbne loop\n\trts\n",
            "66 a1 60 11 21 fe 02",
        ),
        (
            "numeric local labels",
            "1:\n\tnop\n\tbra 1b\n\tbra 2f\n2:\n\trts\n",
            "03 2e ff 2e 02 02",
        ),
        (
            "a difference of labels already defined folds to the short forms",
            "s:\n\tnop\n\tnop\ne:\n\tmov #e-s, r1\n",
            "03 03 66 21",
        ),
        (
            "a folded difference past 15 takes the uimm8 form",
            "s:\n\t.space 20\ne:\n\tmov #e-s, r1\n",
            "00*20 75 41 14",
        ),
        (
            "a folded difference past 255 takes a 16-bit immediate",
            "s:\n\t.space 300\ne:\n\tmov #e-s, r1\n",
            "00*300 fb 1a 2c 01",
        ),
        (
            "a folded difference in cmp",
            "s:\n\t.space 200\ne:\n\tcmp #e-s, r1\n\tadd #e-s, r2\n\tsub #e-s, r3\n",
            "00*200 75 51 c8 72 22 c8 00 72 33 38 ff",
        ),
        (
            "a folded difference in another section",
            "\t.data\ns:\n\t.long 1, 2, 3\ne:\n\t.text\n\tmov #e-s, r1\n\tcmp #e-s, r2\n",
            "66 c1 61 c2",
        ),
        (
            "a negative folded difference",
            "s:\n\t.space 3\ne:\n\tmov #s-e, r1\n",
            "00 00 00 fb 16 fd",
        ),
        (
            "a forward difference relaxes the general form only",
            "\tmov #e-s, r1\ns:\n\tnop\ne:\n\trts\n",
            "fb 16 01 03 02",
        ),
        (
            "a forward difference of 200",
            "\tmov #e-s, r1\ns:\n\t.space 200\ne:\n\trts\n",
            "fb 1a c8 00*201 02",
        ),
        (
            "a forward difference past 16 bits",
            "\tmov #e-s, r1\ns:\n\t.space 70000\ne:\n\trts\n",
            "fb 1e 70 11 01 00*70000 02",
        ),
        (
            "a negative forward difference",
            "\tmov #s-e, r1\ns:\n\t.space 200\ne:\n\trts\n",
            "fb 1a 38 ff 00*200 02",
        ),
        (
            "sub of a forward difference is a negated 32-bit add",
            "\tsub #e-s, r1\ns:\n\t.space 200\ne:\n\trts\n",
            "70 11 38 ff ff ff 00*200 02",
        ),
        (
            "a forward difference in a word store",
            "\tmov.w #e-s, [r1]\ns:\n\t.space 200\ne:\n\trts\n",
            "f8 19 c8 00*201 02",
        ),
        (
            "a set constant takes the short form",
            "n = 5\n\tmov #n, r1\n\tcmp #n, r2\n",
            "66 51 61 52",
        ),
        (
            "a set constant defined later is a 32-bit immediate",
            "\tmov #n, r1\nn = 5\n",
            "fb 12 05 00 00 00",
        ),
        (
            "alignment with nops, one to seven bytes",
            "\tnop\n\t.align 8\n\tnop\n\tnop\n\t.align 8\n\tnop\n\tnop\n\tnop\n\t.align 8\n\tnop\n\tnop\n\tnop\n\tnop\n\t.align 8\n\tnop\n\t.align 4\n\tnop\n\t.align 8\n\trts\n\t.align 8\n",
            "03 fd 70 40 00 00 00 80 03 03 74 10 01 00 00 00 03 03 03 77 10 01 00 00 03 03 03 03 76 10 01 00 03 fc 13 00 03 fc 13 00 02 fd 70 40 00 00 00 80",
        ),
        (
            "alignment with bra.b over the padding",
            "\tnop\n\tnop\n\t.align 32\n\trts\n\t.align 32\n",
            "03 03 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 02 2e 1f 2e 1f 2e 1f 2e 1f 2e 1f 2e 1f 2e 1f 2e 1f 2e 1f 2e 1f 2e 1f 2e 1f 2e 1f 2e 1f 2e 1f 00",
        ),
        (
            "symbols in data",
            "\t.long ext\n\t.long ext + 8\n\t.word ext\n\t.short ext\n\t.byte ext\n",
            "00*15",
        ),
        (
            "small folded differences in every immediate form",
            "s:\n\t.byte 1, 2, 3, 4, 5\ne:\n\tmov.b #e-s, 4[r1]\n\tmov.l #e-s, [r1]\n\tint #e-s\n\tadd #e-s, r1, r2\n\tmov.w #e-s, 4[r2]\n\tcmp #s-e, r3\n\tsub #s-e, r4\n\tmvtc #e-s, intb\n\tmov.b #s-e, 4[r1]\n\tand #e-s, r5\n\ttst #e-s, r6\n",
            "01 02 03 04 05 3c 14 05 f8 16 05 75 60 05 71 12 05 3d 22 05 75 03 fb 71 44 05 fd 77 0c 05 f9 14 04 fb 64 55 fd 74 c6 05",
        ),
        (
            "larger folded differences in every immediate form",
            "s:\n\t.space 200\ne:\n\tmov.b #e-s, 4[r1]\n\tmov.l #e-s, 4[r1]\n\tmov.l #e-s, [r1]\n\tsub #e-s, r1\n\tcmp #e-s, r2\n\tmul #e-s, r3\n\tmov.w #e-s, 4[r2]\n\tmvtc #e-s, intb\n",
            "00*200 3c 14 c8 3e 11 c8 f8 1a c8 00 72 11 38 ff 75 52 c8 76 13 c8 00 3d 22 c8 fd 7b 0c c8 00",
        ),
        (
            "forward differences in every immediate form",
            "\tmov.b #e-s, 4[r1]\n\tmov.l #e-s, [r1]\n\tcmp #e-s, r3\n\tadd #e-s, r1, r2\n\tmov.w #e-s, 4[r2]\n\tint #e-s\n\tmvtc #e-s, intb\n\tstz #e-s, r3\ns:\n\t.space 5\ne:\n\trts\n",
            "f9 14 04 05 f8 16 05 75 03 05 71 12 05 f9 21 02 05 00 00 00 75 60 05 fd 77 0c 05 fd 74 e3 05 00 00 00 00 00 02",
        ),
    ]);
}

// ---- target properties and relocations -----------------------------------------------

#[test]
fn target_properties_match_the_reference_objects() {
    let a = arch::lookup("rx").expect("rx backend");
    // `rx-elf-readelf -h`: "Class: ELF32", "Data: little endian",
    // "Machine: Renesas RX", which binutils' `elf/common.h` numbers 173.
    assert_eq!(a.elf_machine(), 173);
    assert_eq!(a.pointer_bytes(&a.initial_state()), 4);
    // `.word 0x12345678` is four bytes in the reference.
    assert_eq!(a.word_bytes(), 4);
    // `.byte`/`.short`/`.long` of an external symbol, read back with
    // `rx-elf-readelf -r`.
    assert_eq!(a.data_reloc(1, false), Some(0x08)); // R_RX_DIR8S
    assert_eq!(a.data_reloc(2, false), Some(0x05)); // R_RX_DIR16S
    assert_eq!(a.data_reloc(3, false), Some(0x02)); // R_RX_DIR24S
    assert_eq!(a.data_reloc(4, false), Some(0x01)); // R_RX_DIR32
    // `.long sym - .` is a stack of `R_RX_SYM`/`R_RX_OPsub`/`R_RX_ABS32` in
    // the reference, which one relocation cannot express.
    assert_eq!(a.data_reloc(4, true), None);
    for alias in ["RX", "rxv1", "rx600"] {
        assert!(arch::lookup(alias).is_some(), "{alias}");
    }
}

#[test]
fn alignment_pads_with_the_references_single_cycle_no_ops() {
    // Each is what `rx-elf-as` put between the instructions of a
    // `.align` test: `nop` then `.align 4` gives `03 fc 13 00`, and so on.
    let a = arch::lookup("rx").expect("rx backend");
    let st = a.initial_state();
    assert_eq!(hex(&a.nop_fill(&st, 1)), "03");
    assert_eq!(hex(&a.nop_fill(&st, 2)), "ef 00");
    assert_eq!(hex(&a.nop_fill(&st, 3)), "fc 13 00");
    assert_eq!(hex(&a.nop_fill(&st, 5)), "77 10 01 00 00");
    assert_eq!(hex(&a.nop_fill(&st, 6)), "74 10 01 00 00 00");
    assert_eq!(hex(&a.nop_fill(&st, 7)), "fd 70 40 00 00 00 80");
    // Thirty bytes: `bra.b .+30`, repeated.
    assert_eq!(
        hex(&a.nop_fill(&st, 30)),
        expand(
            "2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e 2e 1e"
        )
    );
    for n in 0..300 {
        assert_eq!(a.nop_fill(&st, n).len() as u64, n);
    }
}

#[test]
fn unresolved_operands_get_the_reference_relocations() {
    // The same source through `rx-elf-as` and `rx-elf-readelf -r` gives these
    // offsets, types and addends, and the same code bytes. In particular the
    // PC-relative relocations carry the plain addend: the RX linker measures
    // them from the opcode itself.
    let src = "\tmov #ext, r1\n\
               \tmov.l #ext+8, 4[r2]\n\
               \tmov.b #ext, 4[r1]\n\
               \tint #ext\n\
               \tfadd #ext, r3\n\
               \tadd #ext, r1\n\
               \tbra ext\n\
               \tbsr ext+4\n\
               \tbra.b ext\n\
               \tbra.w ext\n\
               \tbeq.b ext\n\
               \tbne.w ext\n\
               \tbra.s ext\n\
               \tbeq.s ext+3\n\
               \t.byte ext\n\
               \t.short ext\n\
               \t.word ext\n\
               \t.long ext+5\n";
    let asm = assemble_for("rx", src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let got: Vec<(u64, u32, i64)> = asm
        .relocs
        .iter()
        .map(|r| (r.offset, r.kind, r.addend))
        .collect();
    assert_eq!(
        got,
        vec![
            (0x02, 0x01, 0), // R_RX_DIR32        ext
            (0x09, 0x01, 8), // R_RX_DIR32        ext + 8
            (0x10, 0x08, 0), // R_RX_DIR8S        ext
            (0x13, 0x07, 0), // R_RX_DIR8U        ext
            (0x17, 0x01, 0), // R_RX_DIR32        ext
            (0x1d, 0x01, 0), // R_RX_DIR32        ext
            (0x22, 0x09, 0), // R_RX_DIR24S_PCREL ext
            (0x26, 0x09, 4), // R_RX_DIR24S_PCREL ext + 4
            (0x2a, 0x0b, 0), // R_RX_DIR8S_PCREL  ext
            (0x2c, 0x0a, 0), // R_RX_DIR16S_PCREL ext
            (0x2f, 0x0b, 0), // R_RX_DIR8S_PCREL  ext
            (0x31, 0x0a, 0), // R_RX_DIR16S_PCREL ext
            (0x33, 0x12, 0), // R_RX_DIR3U_PCREL  ext
            (0x34, 0x12, 3), // R_RX_DIR3U_PCREL  ext + 3
            (0x35, 0x08, 0), // R_RX_DIR8S        ext
            (0x36, 0x05, 0), // R_RX_DIR16S       ext
            (0x38, 0x01, 0), // R_RX_DIR32        ext
            (0x3c, 0x01, 5), // R_RX_DIR32        ext + 5
        ]
    );
    assert_eq!(
        hex(&section(&asm, ".text")[..0x30]),
        "fb 12 00 00 00 00 f9 22 01 00 00 00 00 f9 14 04 \
         00 75 60 00 fd 72 23 00 00 00 00 70 11 00 00 00 \
         00 04 00 00 00 05 00 00 00 2e 00 38 00 00 20 00"
    );
}

#[test]
fn a_branch_to_another_section_is_relocated_from_the_opcode() {
    // `rx-elf-as` gives `R_RX_DIR24S_PCREL other + 0` at offset 1 and
    // `other + 2` at offset 5. rsasm relocates against the section symbol,
    // and `other` is at its start, so the addends are the same.
    let src = "\tbsr other\n\tbra other+2\n\t.section .t2,\"ax\"\nother:\n\trts\n";
    let asm = assemble_for("rx", src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let got: Vec<(u64, u32, i64)> = asm
        .relocs
        .iter()
        .map(|r| (r.offset, r.kind, r.addend))
        .collect();
    assert_eq!(got, vec![(1, 0x09, 0), (5, 0x09, 2)]);
}

#[test]
fn a_branch_resolved_in_a_flat_image_counts_from_its_opcode() {
    // `bra.a` from 0x1000 to 0x1010: a displacement of 16, as the reference
    // writes `bra.a` for a label 16 bytes on (`04 10 00 00`).
    let asm = assemble_flat_for("rx", "\tbra.a next\n\t.space 12\nnext:\n\trts\n", 0x1000);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(hex(&section(&asm, ".text")[..4]), "04 10 00 00");
    check(&[("bra.a 1f\n.space 12\n1:", &expand("04 10 00 00 00*12"))]);
}

// ---- spellings the reference does not have ---------------------------------------------

#[test]
fn sp_is_r0() {
    // GNU as rejects `sp`; Renesas documentation uses it for r0.
    for (with_sp, with_r0) in [
        ("mov sp, r1", "mov r0, r1"),
        ("mov.l 4[sp], r2", "mov.l 4[r0], r2"),
        ("add #4, sp", "add #4, r0"),
        ("mov r1, [-sp]", "mov r1, [-r0]"),
    ] {
        assert_eq!(
            text_for("rx", with_sp),
            text_for("rx", with_r0),
            "{with_sp}"
        );
    }
}

#[test]
fn bit_operations_on_memory_may_leave_out_the_byte_suffix() {
    // GNU as insists on `.b` for `bset`/`bclr`/`btst #n, mem` but not for
    // `bnot`/`bmCnd`, or for any of them with a register bit number; rsasm
    // accepts all of them either way. Upper case is the reference's too.
    assert_eq!(
        text_for("rx", "MOV.W R1, R2"),
        text_for("rx", "mov.w r1, r2")
    );
    assert_eq!(
        text_for("rx", "bset #1, 4[r1]"),
        text_for("rx", "bset #1, 4[r1].b")
    );
}

#[test]
fn renesas_dialect_source_assembles_to_the_same_bytes() {
    // The core's Renesas dialect gives `H` suffixes, `;` comments and bare
    // data directives; the operand syntax stays GNU's.
    let renesas = "\tMOV.L #10H, R1 ; load\n\tDB 1, 2\nL1:\tBEQ L1\n\tRTS\n";
    let gnu = "\tmov.l #0x10, r1\n\t.byte 1, 2\nl1:\tbeq l1\n\trts\n";
    assert_eq!(
        text_dialect("rx", Dialect::Renesas, renesas),
        text_for("rx", gnu),
    );
    // And the GNU version is what the reference assembles.
    check(&[(gnu, "75 41 10 01 02 20 00 02")]);
}

// ---- diagnostics --------------------------------------------------------------------------

#[track_caller]
fn fails(src: &str, needle: &str) {
    let e = errors_for("rx", src);
    assert!(
        e.contains(needle),
        "`{src}` should mention `{needle}`:\n{e}"
    );
}

#[test]
fn range_violations_name_their_limit() {
    fails("int #256", "0 to 255");
    fails("mov.b #256, [r1]", "-128 to 255");
    fails("mov.w #65536, [r1]", "-32768 to 65535");
    fails("mov #0x100000000, r1", "4294967295");
    fails("shlr #32, r1", "0 to 31");
    fails("rotl #32, r1", "0 to 31");
    fails("bset #32, r1", "0 to 31");
    fails("bset #8, [r1].b", "0 to 7");
    fails("mvtipl #16", "0 to 15");
    fails("rtsd #1024", "0 to 1020");
    fails("rtsd #6", "multiple of 4");
    fails("racw #3", "1 to 2");
    fails("mov.b r1, 65536[r2]", "65535");
    fails("mov.w r1, 131072[r2]", "131070");
    fails("mov.l r1, 262144[r2]", "262140");
    fails("mov.l 5[r1], r2", "multiple of 4");
    fails("mov.w 3[r1], r2", "multiple of 2");
    fails("mov.l -4[r1], r2", "negative");
    // Branches: `.b` reaches 127 bytes forward, `.s` 3 to 10.
    fails("bra.b far\n.space 200\nfar:", "out of range (-128 to 127)");
    fails("bgt.b far\n.space 200\nfar:", "out of range");
    fails("bra.s next\nnext:", "out of range");
}

#[test]
fn what_the_reference_rejects_is_rejected() {
    // Each of these is an error from `rx-elf-as` too.
    fails("mov.b #1, r1", "mov.b");
    fails("mov #1, [r1]", "needs a size");
    fails("add.l r1, r2", "size suffix");
    fails("pushm r0-r3", "r0");
    fails("popm r5-r3", "backwards");
    fails("rtsd #8, r0-r3", "r0");
    fails("mov.l ext[r1], r2", "displacements must be constants");
    fails("bgt.w foo", "only `beq` and `bne`");
    fails("bgt.s foo", "only `beq` and `bne`");
    fails("bsr.b foo", "`bsr` is `.w` or `.a`");
    fails("sbb #ext, r1", "symbolic");
    fails("rtsd #ext", "constant");
    fails("pushc extb", "RXv2");
    fails("stz r1, r2", "RXv2");
    fails("emula r1, r2, a0", "RXv2");
    fails("xor r1, r2, r3", "RXv3");
    fails("sceq r1", "sceq.l");
    fails("setpsw q", "flag");
    fails("frob r1", "unknown instruction");
    fails("mov r1", "invalid operands");
    fails("mov [r1", "expected `]`");
    fails("mov r1, 4[r2].q", "size suffix");
    fails("adc 4[r1].w, r2", "long");
}

#[test]
fn errors_do_not_stop_later_statements() {
    let e = errors_for("rx", "frob\nmov r1\nnop\nint #300\n");
    assert!(e.contains("frob"), "{e}");
    assert!(e.contains("invalid operands"), "{e}");
    assert!(e.contains("0 to 255"), "{e}");
}

#[test]
fn malformed_input_never_panics() {
    const MNEMONICS: &[&str] = &[
        "nop", "mov", "mov.b", "mov.w", "mov.l", "mov.q", "movu", "movu.l", "add", "sub", "cmp",
        "and", "adc", "sbb", "neg", "abs", "max", "div", "tst", "xor", "stz", "xchg", "itof",
        "shlr", "shll", "rotl", "revw", "bset", "bclr", "bnot", "bmc", "bmeq", "sceq", "sceq.l",
        "push", "pop", "pushc", "pushm", "popm", "rtsd", "bra", "bra.s", "bra.b", "bra.w", "bra.a",
        "bra.x", "bsr", "beq", "beq.s", "bgt", "bgt.w", "jmp", "jsr", "int", "mvtipl", "setpsw",
        "mvtc", "mvfc", "fadd", "fcmp", "ftoi", "racw", "mvtachi", "suntil", "rmpa.b",
    ];
    const FRAGMENTS: &[&str] = &[
        "",
        "r0",
        "r15",
        "r16",
        "sp",
        "psw",
        "extb",
        "pbp",
        "c",
        "#",
        "#0",
        "#-1",
        "#99999999999999999999",
        "#ext",
        "#a-b",
        "[r1]",
        "[r1",
        "r1]",
        "[]",
        "[r1+]",
        "[-r1]",
        "[+r1]",
        "[r1-]",
        "[r1,r2]",
        "[r1,]",
        "[,r1]",
        "[r1,r2,r3]",
        "4[r1]",
        "-4[r1]",
        "5[r1].l",
        "4[r1].ub",
        "4[r1].uw",
        "4[r1].x",
        "[r1].b.b",
        "ext[r1]",
        "70000[r1]",
        "99999999999999999999[r1]",
        "r1-r3",
        "r3-r1",
        "r0-r15",
        "r1-",
        "-r1",
        "ext",
        "1f",
        "(",
        ")",
        "%",
        "'",
        "\"s\"",
        ".b",
        ".",
    ];
    for m in MNEMONICS {
        for a in FRAGMENTS {
            let _ = try_text_for("rx", &format!("{m} {a}"));
            for b in FRAGMENTS {
                let _ = try_text_for("rx", &format!("l:\n\t{m} {a}, {b}"));
            }
        }
    }
    for src in [
        ",",
        "mov ,",
        "mov r1,,",
        "mov #1, r1, r2, r3",
        "mov\u{80}",
        "bset #1, \u{1F600}[r1]",
        "bra 0x7fffffffffffffff",
        "mov #e-s, r1\ns:\ne:",
        "s:\ne:\nsub #e-s, r1\nmov.b #s-e, 4[r1]",
    ] {
        let _ = try_text_for("rx", src);
    }
}
