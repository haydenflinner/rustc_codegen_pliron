//! RL78 encoding tests.
//!
//! Every expected byte string and relocation here was produced by
//! `rl78-elf-as` from GNU binutils 2.47, the reference behind
//! `tools/xas-diff/run.sh rl78`. The `corpus_*` tables and `programs` are
//! `tools/xas-diff/rl78.txt` and `rl78-programs.txt` with the reference's
//! bytes beside each case; the G14 instructions, which that harness cannot run
//! because the reference rejects them without `-mg14`, were assembled by hand
//! with that flag. None of the expectations came from rsasm.

#![cfg(feature = "rl78")]

mod common;
use common::*;
use rsasm::arch;
use rsasm::lexer::Dialect;

/// Asserts that `src` assembles to `want`, written as hex bytes.
#[track_caller]
fn enc(src: &str, want: &str) {
    let got = match try_text_for("rl78", src) {
        Ok(b) => hex(&b),
        Err(e) => e,
    };
    assert_eq!(got, want, "\nsource: {src}\n  want: {want}\n   got: {got}");
}

/// Checks a whole table, reporting every mismatch rather than the first.
#[track_caller]
fn check(cases: &[(&str, &str)]) {
    let mut failures = Vec::new();
    for (src, want) in cases {
        let got = match try_text_for("rl78", src) {
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
        let got = match try_text_for("rl78", src) {
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
        ("nop", "00"),
        ("brk", "61 cc"),
        ("brk1", "ff"),
        ("halt", "61 ed"),
        ("stop", "61 fd"),
        ("ret", "d7"),
        ("reti", "61 fc"),
        ("retb", "61 ec"),
        ("ei", "71 7a fa"),
        ("di", "71 7b fa"),
        ("skc", "61 c8"),
        ("skh", "61 e3"),
        ("sknc", "61 d8"),
        ("sknh", "61 f3"),
        ("sknz", "61 f8"),
        ("skz", "61 e8"),
        ("mulu x", "d6"),
        ("MULU X", "d6"),
    ]);
}

#[test]
fn corpus_mov_register_and_immediate() {
    check(&[
        ("mov a, #0x10", "51 10"),
        ("mov a, #10H", "51 10"),
        ("mov a, #0ffh", "51 ff"),
        ("mov a, #101B", "51 05"),
        ("mov a, #17o", "51 0f"),
        ("mov a, #99d", "51 63"),
        ("mov a, #010", "51 0a"),
        ("mov a, #-1", "51 ff"),
        ("mov a, #'a'", "51 61"),
        ("mov a, #1+2*3", "51 07"),
        ("MOV A, #1", "51 01"),
        ("mov x, #1", "50 01"),
        ("mov c, #2", "52 02"),
        ("mov b, #3", "53 03"),
        ("mov e, #4", "54 04"),
        ("mov d, #5", "55 05"),
        ("mov l, #6", "56 06"),
        ("mov h, #7", "57 07"),
        ("mov r0, #1", "50 01"),
        ("mov r7, #1", "57 01"),
        ("mov a, x", "60"),
        ("mov a, c", "62"),
        ("mov a, h", "67"),
        ("mov x, a", "70"),
        ("mov b, a", "73"),
        ("mov l, a", "76"),
        ("mov r3, r1", "73"),
    ]);
}

#[test]
fn corpus_mov_special_registers() {
    check(&[
        ("mov spl, #5", "ce f8 05"),
        ("mov sph, #5", "ce f9 05"),
        ("mov psw, #5", "ce fa 05"),
        ("mov cs, #5", "ce fc 05"),
        ("mov es, #5", "41 05"),
        ("mov pmc, #5", "ce fe 05"),
        ("mov mem, #5", "ce ff 05"),
        ("mov a, psw", "8e fa"),
        ("mov a, es", "8e fd"),
        ("mov a, cs", "8e fc"),
        ("mov a, spl", "8e f8"),
        ("mov psw, a", "9e fa"),
        ("mov es, a", "9e fd"),
        ("mov cs, a", "9e fc"),
        ("mov es, 0xffe30", "61 b8 30"),
    ]);
}

#[test]
fn corpus_mov_direct_addresses() {
    check(&[
        ("mov 0xffe20, #5", "cd 20 05"),
        ("mov 0xfff1f, #5", "ce 1f 05"),
        ("mov 0xffe30, #5", "cd 30 05"),
        ("mov 0xfff20, #5", "ce 20 05"),
        ("mov 0xfff10, #5", "ce 10 05"),
        ("mov 0xfffff, #5", "ce ff 05"),
        ("mov a, 0xffe30", "8d 30"),
        ("mov a, 0xfff20", "8e 20"),
        ("mov a, 0xfff10", "8d 10"),
        ("mov 0xffe30, a", "9d 30"),
        ("mov 0xfff20, a", "9e 20"),
        ("mov 0xfff10, a", "9e 10"),
        ("mov x, 0xffe30", "d8 30"),
        ("mov b, 0xffe30", "e8 30"),
        ("mov c, 0xffe30", "f8 30"),
        ("mov !0x1234, #5", "cf 34 12 05"),
        ("mov es:!0x1234, #5", "11 cf 34 12 05"),
        ("mov a, !0x1234", "8f 34 12"),
        ("mov a, es:!0x1234", "11 8f 34 12"),
        ("mov !0x1234, a", "9f 34 12"),
        ("mov es:!0x1234, a", "11 9f 34 12"),
        ("mov x, !0x1234", "d9 34 12"),
        ("mov b, !0x1234", "e9 34 12"),
        ("mov c, !0x1234", "f9 34 12"),
        ("mov c, es:!0x1234", "11 f9 34 12"),
        ("mov a, !0xffff", "8f ff ff"),
        ("mov a, !0", "8f 00 00"),
    ]);
}

#[test]
fn corpus_mov_indirect_and_based() {
    check(&[
        ("mov a, [de]", "89"),
        ("mov a, es:[de]", "11 89"),
        ("mov [de], a", "99"),
        ("mov es:[de], a", "11 99"),
        ("mov a, [de+5]", "8a 05"),
        ("mov a, es:[de+5]", "11 8a 05"),
        ("mov [de+5], a", "9a 05"),
        ("mov [de+5], #9", "ca 05 09"),
        ("mov es:[de+5], #9", "11 ca 05 09"),
        ("mov a, [hl]", "8b"),
        ("mov a, es:[hl]", "11 8b"),
        ("mov [hl], a", "9b"),
        ("mov a, [hl+5]", "8c 05"),
        ("mov a, [hl + 5]", "8c 05"),
        ("mov a, [ hl ]", "8b"),
        ("mov a, [hl+5+3]", "8c 08"),
        ("mov a, [hl+(5)]", "8c 05"),
        ("mov a, [hl+0]", "8c 00"),
        ("mov a, [de+0]", "8a 00"),
        ("mov [hl+0xff], a", "9c ff"),
        ("mov [hl+5], #9", "cc 05 09"),
        ("mov es:[hl+5], #9", "11 cc 05 09"),
        ("mov a, [hl+b]", "61 c9"),
        ("mov a, es:[hl+b]", "11 61 c9"),
        ("mov [hl+b], a", "61 d9"),
        ("mov a, [hl+c]", "61 e9"),
        ("mov [hl+c], a", "61 f9"),
        ("mov es:[hl+c], a", "11 61 f9"),
        ("mov a, 0x1234[b]", "09 34 12"),
        ("mov a, es:0x1234[b]", "11 09 34 12"),
        ("mov 0x1234[b], a", "18 34 12"),
        ("mov 0x1234[b], #9", "19 34 12 09"),
        ("mov a, 0x1234[c]", "29 34 12"),
        ("mov 0x1234[c], a", "28 34 12"),
        ("mov 0x1234[c], #9", "38 34 12 09"),
        ("mov a, 0x1234[bc]", "49 34 12"),
        ("mov 0x1234[bc], a", "48 34 12"),
        ("mov 0x1234[bc], #9", "39 34 12 09"),
        ("mov es:0x1234[bc], #9", "11 39 34 12 09"),
        ("mov a, [bc]", "49 00 00"),
        ("mov [bc], a", "48 00 00"),
        ("mov [bc], #9", "39 00 00 09"),
        ("mov es:[bc], #9", "11 39 00 00 09"),
        ("mov a, [sp+4]", "88 04"),
        ("mov a, [sp]", "88 00"),
        ("mov [sp+4], a", "98 04"),
        ("mov [sp], a", "98 00"),
        ("mov [sp+4], #9", "c8 04 09"),
        ("mov [sp], #9", "c8 00 09"),
    ]);
}

#[test]
fn corpus_xch() {
    check(&[
        ("xch a, x", "08"),
        ("xch a, c", "61 8a"),
        ("xch a, b", "61 8b"),
        ("xch a, e", "61 8c"),
        ("xch a, d", "61 8d"),
        ("xch a, l", "61 8e"),
        ("xch a, h", "61 8f"),
        ("xch a, !0x1234", "61 aa 34 12"),
        ("xch a, es:!0x1234", "11 61 aa 34 12"),
        ("xch a, [de]", "61 ae"),
        ("xch a, es:[de]", "11 61 ae"),
        ("xch a, [de+3]", "61 af 03"),
        ("xch a, [hl]", "61 ac"),
        ("xch a, [hl+3]", "61 ad 03"),
        ("xch a, es:[hl+3]", "11 61 ad 03"),
        ("xch a, [hl+b]", "61 b9"),
        ("xch a, [hl+c]", "61 a9"),
        ("xch a, 0xffe30", "61 a8 30"),
        ("xch a, 0xfff20", "61 ab 20"),
        ("xch a, 0xfff10", "61 ab 10"),
    ]);
}

#[test]
fn corpus_movw() {
    check(&[
        ("movw ax, #0x1234", "30 34 12"),
        ("movw bc, #0x1234", "32 34 12"),
        ("movw de, #0x1234", "34 34 12"),
        ("movw hl, #0x1234", "36 34 12"),
        ("movw rp2, #1", "34 01 00"),
        ("movw 0xffe30, #0x1234", "c9 30 34 12"),
        ("movw 0xfff20, #0x1234", "cb 20 34 12"),
        ("movw 0xfff10, #0x1234", "c9 10 34 12"),
        ("movw ax, 0xffe30", "ad 30"),
        ("movw ax, 0xfff20", "ae 20"),
        ("movw ax, 0xfff10", "ad 10"),
        ("movw 0xffe30, ax", "bd 30"),
        ("movw 0xfff20, ax", "be 20"),
        ("movw 0xfff10, ax", "bd 10"),
        ("movw ax, bc", "13"),
        ("movw ax, de", "15"),
        ("movw ax, hl", "17"),
        ("movw bc, ax", "12"),
        ("movw de, ax", "14"),
        ("movw hl, ax", "16"),
        ("movw ax, !0x1234", "af 34 12"),
        ("movw ax, es:!0x1234", "11 af 34 12"),
        ("movw !0x1234, ax", "bf 34 12"),
        ("movw es:!0x1234, ax", "11 bf 34 12"),
        ("movw ax, [de]", "a9"),
        ("movw [de], ax", "b9"),
        ("movw ax, [de+2]", "aa 02"),
        ("movw [de+2], ax", "ba 02"),
        ("movw ax, [hl]", "ab"),
        ("movw es:[hl], ax", "11 bb"),
        ("movw ax, [hl+2]", "ac 02"),
        ("movw [hl+2], ax", "bc 02"),
        ("movw ax, 0x1234[b]", "59 34 12"),
        ("movw 0x1234[b], ax", "58 34 12"),
        ("movw ax, 0x1234[c]", "69 34 12"),
        ("movw 0x1234[c], ax", "68 34 12"),
        ("movw ax, 0x1234[bc]", "79 34 12"),
        ("movw ax, [bc]", "79 00 00"),
        ("movw 0x1234[bc], ax", "78 34 12"),
        ("movw [bc], ax", "78 00 00"),
        ("movw es:[bc], ax", "11 78 00 00"),
        ("movw ax, [sp+2]", "a8 02"),
        ("movw ax, [sp]", "a8 00"),
        ("movw [sp+2], ax", "b8 02"),
        ("movw [sp], ax", "b8 00"),
        ("movw bc, 0xffe30", "da 30"),
        ("movw de, 0xffe30", "ea 30"),
        ("movw hl, 0xffe30", "fa 30"),
        ("movw bc, !0x1234", "db 34 12"),
        ("movw de, es:!0x1234", "11 eb 34 12"),
        ("movw hl, !0x1234", "fb 34 12"),
        ("movw sp, #0x1234", "cb f8 34 12"),
        ("movw sp, ax", "be f8"),
        ("movw ax, sp", "ae f8"),
        ("movw bc, sp", "db f8 ff"),
        ("movw de, sp", "eb f8 ff"),
        ("movw hl, sp", "fb f8 ff"),
    ]);
}

#[test]
fn corpus_xchw() {
    check(&[
        ("xchw ax, bc", "33"),
        ("xchw ax, de", "35"),
        ("xchw ax, hl", "37"),
    ]);
}

#[test]
fn corpus_oneb_clrb_onew_clrw_cmp0() {
    check(&[
        ("oneb a", "e1"),
        ("oneb x", "e0"),
        ("oneb b", "e3"),
        ("oneb c", "e2"),
        ("oneb 0xffe30", "e4 30"),
        ("oneb !0x1234", "e5 34 12"),
        ("oneb es:!0x1234", "11 e5 34 12"),
        ("clrb a", "f1"),
        ("clrb x", "f0"),
        ("clrb b", "f3"),
        ("clrb c", "f2"),
        ("clrb 0xffe30", "f4 30"),
        ("clrb !0x1234", "f5 34 12"),
        ("onew ax", "e6"),
        ("onew bc", "e7"),
        ("clrw ax", "f6"),
        ("clrw bc", "f7"),
        ("cmp0 a", "d1"),
        ("cmp0 x", "d0"),
        ("cmp0 b", "d3"),
        ("cmp0 c", "d2"),
        ("cmp0 0xffe30", "d4 30"),
        ("cmp0 !0x1234", "d5 34 12"),
        ("cmp0 es:!0x1234", "11 d5 34 12"),
    ]);
}

#[test]
fn corpus_movs_cmps() {
    check(&[
        ("movs [hl+4], x", "61 ce 04"),
        ("movs es:[hl+4], x", "11 61 ce 04"),
        ("cmps x, [hl+4]", "61 de 04"),
        ("cmps x, es:[hl+4]", "11 61 de 04"),
    ]);
}

#[test]
fn corpus_8_bit_arithmetic() {
    check(&[
        ("add a, #1", "0c 01"),
        ("addc a, #1", "1c 01"),
        ("sub a, #1", "2c 01"),
        ("subc a, #1", "3c 01"),
        ("cmp a, #1", "4c 01"),
        ("and a, #1", "5c 01"),
        ("or a, #1", "6c 01"),
        ("xor a, #1", "7c 01"),
        ("add 0xffe30, #1", "0a 30 01"),
        ("cmp 0xffe30, #1", "4a 30 01"),
        ("xor 0xffe30, #1", "7a 30 01"),
        ("add a, a", "61 01"),
        ("sub a, a", "61 21"),
        ("xor a, a", "61 71"),
        ("add a, x", "61 08"),
        ("add a, c", "61 0a"),
        ("add a, h", "61 0f"),
        ("sub a, b", "61 2b"),
        ("or a, l", "61 6e"),
        ("add x, a", "61 00"),
        ("add c, a", "61 02"),
        ("and h, a", "61 57"),
        ("cmp e, a", "61 44"),
        ("add a, 0xffe30", "0b 30"),
        ("subc a, 0xffe30", "3b 30"),
        ("add a, !0x1234", "0f 34 12"),
        ("add a, es:!0x1234", "11 0f 34 12"),
        ("and a, !0x1234", "5f 34 12"),
        ("add a, [hl]", "0d"),
        ("add a, es:[hl]", "11 0d"),
        ("or a, [hl]", "6d"),
        ("add a, [hl+7]", "0e 07"),
        ("xor a, es:[hl+7]", "11 7e 07"),
        ("add a, [hl+b]", "61 80"),
        ("add a, [hl+c]", "61 82"),
        ("cmp a, [hl+b]", "61 c0"),
        ("xor a, es:[hl+c]", "11 61 f2"),
        ("cmp !0x1234, #1", "40 34 12 01"),
        ("cmp es:!0x1234, #1", "11 40 34 12 01"),
        ("addc x, a", "61 10"),
        ("subc c, a", "61 32"),
    ]);
}

#[test]
fn corpus_16_bit_arithmetic() {
    check(&[
        ("addw ax, #0x1234", "04 34 12"),
        ("subw ax, #0x1234", "24 34 12"),
        ("cmpw ax, #0x1234", "44 34 12"),
        ("addw ax, ax", "01"),
        ("addw ax, bc", "03"),
        ("addw ax, de", "05"),
        ("addw ax, hl", "07"),
        ("subw ax, bc", "23"),
        ("cmpw ax, hl", "47"),
        ("addw ax, 0xffe30", "06 30"),
        ("subw ax, 0xffe30", "26 30"),
        ("cmpw ax, 0xffe30", "46 30"),
        ("addw ax, !0x1234", "02 34 12"),
        ("subw ax, es:!0x1234", "11 22 34 12"),
        ("cmpw ax, !0x1234", "42 34 12"),
        ("addw ax, [hl+2]", "61 09 02"),
        ("subw ax, es:[hl+2]", "11 61 29 02"),
        ("cmpw ax, [hl+2]", "61 49 02"),
        ("addw ax, [hl]", "61 09 00"),
        ("cmpw ax, es:[hl]", "11 61 49 00"),
        ("addw sp, #4", "10 04"),
        ("subw sp, #4", "20 04"),
    ]);
}

#[test]
fn corpus_inc_dec() {
    check(&[
        ("inc x", "80"),
        ("inc a", "81"),
        ("inc c", "82"),
        ("inc b", "83"),
        ("inc e", "84"),
        ("inc d", "85"),
        ("inc l", "86"),
        ("inc h", "87"),
        ("dec x", "90"),
        ("dec a", "91"),
        ("dec h", "97"),
        ("inc 0xffe30", "a4 30"),
        ("dec 0xffe30", "b4 30"),
        ("inc !0x1234", "a0 34 12"),
        ("dec !0x1234", "b0 34 12"),
        ("inc es:!0x1234", "11 a0 34 12"),
        ("dec es:!0x1234", "11 b0 34 12"),
        ("inc [hl+3]", "61 59 03"),
        ("dec [hl+3]", "61 69 03"),
        ("inc es:[hl+3]", "11 61 59 03"),
        ("dec es:[hl+3]", "11 61 69 03"),
        ("incw ax", "a1"),
        ("incw bc", "a3"),
        ("incw de", "a5"),
        ("incw hl", "a7"),
        ("decw ax", "b1"),
        ("decw hl", "b7"),
        ("incw 0xffe30", "a6 30"),
        ("decw 0xffe30", "b6 30"),
        ("incw !0x1234", "a2 34 12"),
        ("decw es:!0x1234", "11 b2 34 12"),
        ("incw [hl+3]", "61 79 03"),
        ("decw es:[hl+3]", "11 61 89 03"),
    ]);
}

#[test]
fn corpus_shifts_and_rotates() {
    check(&[
        ("rol a, 1", "61 eb"),
        ("rolc a, 1", "61 dc"),
        ("rolwc ax, 1", "61 ee"),
        ("rolwc bc, 1", "61 fe"),
        ("ror a, 1", "61 db"),
        ("rorc a, 1", "61 fb"),
        ("sar a, 1", "31 1b"),
        ("sar a, 7", "31 7b"),
        ("sarw ax, 1", "31 1f"),
        ("sarw ax, 15", "31 ff"),
        ("shl a, 1", "31 19"),
        ("shl a, 7", "31 79"),
        ("shl b, 3", "31 38"),
        ("shl c, 2", "31 27"),
        ("shlw ax, 4", "31 4d"),
        ("shlw bc, 15", "31 fc"),
        ("shr a, 6", "31 6a"),
        ("shrw ax, 9", "31 9e"),
    ]);
}

#[test]
fn corpus_bit_operations() {
    check(&[
        ("set1 cy", "71 80"),
        ("clr1 cy", "71 88"),
        ("not1 cy", "71 c0"),
        ("set1 psw.7", "71 7a fa"),
        ("clr1 psw.0", "71 0b fa"),
        ("set1 es.3", "71 3a fd"),
        ("set1 0xffe30.3", "71 32 30"),
        ("clr1 0xffe30.3", "71 33 30"),
        ("set1 0xfff20.1", "71 1a 20"),
        ("clr1 0xfff20.1", "71 1b 20"),
        ("set1 0xfff10.2", "71 2a 10"),
        ("set1 a.5", "71 da"),
        ("clr1 a.5", "71 db"),
        ("set1 !0x1234.6", "71 60 34 12"),
        ("clr1 !0x1234.6", "71 68 34 12"),
        ("set1 es:!0x1234.6", "11 71 60 34 12"),
        ("set1 [hl].4", "71 c2"),
        ("clr1 [hl].4", "71 c3"),
        ("clr1 es:[hl].4", "11 71 c3"),
        ("mov1 cy, 0xffe30.2", "71 24 30"),
        ("mov1 cy, 0xfff20.2", "71 2c 20"),
        ("mov1 cy, 0xfff10.2", "71 24 10"),
        ("mov1 cy, a.2", "71 ac"),
        ("mov1 cy, psw.2", "71 2c fa"),
        ("mov1 cy, [hl].2", "71 a4"),
        ("mov1 cy, es:[hl].2", "11 71 a4"),
        ("mov1 0xffe30.2, cy", "71 21 30"),
        ("mov1 0xfff20.2, cy", "71 29 20"),
        ("mov1 0xfff10.2, cy", "71 21 10"),
        ("mov1 a.2, cy", "71 a9"),
        ("mov1 psw.2, cy", "71 29 fa"),
        ("mov1 [hl].2, cy", "71 a1"),
        ("mov1 es:[hl].2, cy", "11 71 a1"),
        ("and1 cy, psw.1", "71 1d fa"),
        ("or1 cy, psw.1", "71 1e fa"),
        ("xor1 cy, psw.1", "71 1f fa"),
        ("and1 cy, 0xfff20.1", "71 1d 20"),
        ("or1 cy, 0xffe30.1", "71 16 30"),
        ("xor1 cy, 0xfff10.1", "71 1f 10"),
        ("and1 cy, a.3", "71 bd"),
        ("or1 cy, a.3", "71 be"),
        ("xor1 cy, a.3", "71 bf"),
        ("and1 cy, [hl].3", "71 b5"),
        ("or1 cy, es:[hl].3", "11 71 b6"),
        ("xor1 cy, [hl].0", "71 87"),
    ]);
}

#[test]
fn corpus_branches_and_calls_with_absolute_targets() {
    check(&[
        ("br ax", "61 cb"),
        ("br !0x1234", "ed 34 12"),
        ("br !!0x12345", "ec 45 23 01"),
        ("call ax", "61 ca"),
        ("call bc", "61 da"),
        ("call de", "61 ea"),
        ("call hl", "61 fa"),
        ("call !0x1234", "fd 34 12"),
        ("call !!0x12345", "fc 45 23 01"),
        ("callt [0x80]", "61 84"),
        ("callt [0x82]", "61 94"),
        ("callt [0x90]", "61 85"),
        ("callt [0xbe]", "61 f7"),
        ("push ax", "c1"),
        ("push bc", "c3"),
        ("push de", "c5"),
        ("push hl", "c7"),
        ("push psw", "61 dd"),
        ("pop ax", "c0"),
        ("pop bc", "c2"),
        ("pop de", "c4"),
        ("pop hl", "c6"),
        ("pop psw", "61 cd"),
        ("sel rb0", "61 cf"),
        ("sel rb1", "61 df"),
        ("sel RB2", "61 ef"),
        ("sel rb3", "61 ff"),
    ]);
}

#[test]
fn corpus_operand_spellings() {
    check(&[
        ("MOV A, 0FFE30H", "8d 30"),
        ("MOV 0FFF20H, #0FFH", "ce 20 ff"),
        ("SET1 PSW.7", "71 7a fa"),
        ("CLR1 0FFE30H.0", "71 03 30"),
        ("AND1 CY, 0FFF20H.7", "71 7d 20"),
        ("movw rp1, rp0", "12"),
        ("movw rp3, #0ffffh", "36 ff ff"),
        ("xchw rp0, rp2", "35"),
        ("mov r2, r1", "72"),
        ("add r1, #1", "0c 01"),
        ("set1 a . 3", "71 ba"),
        ("set1 psw . 2", "71 2a fa"),
        ("set1 !0x1234 . 2", "71 20 34 12"),
        ("mov1 cy, 0xfff20 . 2", "71 2c 20"),
        ("set1 (0xfff20).3", "71 3a 20"),
        ("set1 (0xffe20+0x10).4", "71 42 30"),
        ("mov a, [hl+-1]", "8c ff"),
        ("movw ax, #-1", "30 ff ff"),
        ("movw ax, #0xffff", "30 ff ff"),
        ("mov a, #255", "51 ff"),
        ("mov a, #-128", "51 80"),
        ("mov a, [hl+255]", "8c ff"),
        ("mov a, #(1<<7)|1", "51 81"),
        ("mov a, [ de + 2 ]", "8a 02"),
        ("mov a , [hl]", "8b"),
        ("xch a,[hl+c]", "61 a9"),
        ("mov a, !0xf1234", "8f 34 12"),
        ("mov a, es:!0xf1234", "11 8f 34 12"),
        ("movw ax, 0xffffa", "ae fa"),
        ("movw 0xfff20, #-1", "cb 20 ff ff"),
    ]);
}

#[test]
fn corpus_symbolic_operands() {
    check(&[
        ("mov a, #ext", "51 00"),
        ("movw ax, #ext", "30 00 00"),
        ("mov a, !ext", "8f 00 00"),
        ("mov a, es:!ext", "11 8f 00 00"),
        ("mov a, ext", "8d 00"),
        ("mov ext, #1", "cd 00 01"),
        ("mov a, [hl+ext]", "8c 00"),
        ("mov a, ext[b]", "09 00 00"),
        ("movw ax, ext", "ad 00"),
        ("br !ext", "ed 00 00"),
        ("br !!ext", "ec 00 00 00"),
        ("call !ext", "fd 00 00"),
        ("call !!ext", "fc 00 00 00"),
        ("set1 ext.3", "71 32 00"),
        ("set1 !ext.3", "71 30 00 00"),
        ("bt a.1, $.+3", "31 13 00"),
        ("mov1 cy, sym.2", "71 24 00"),
        ("xch a, sym", "61 a8 00"),
        ("movw bc, sym", "da 00"),
    ]);
}

#[test]
fn corpus_data() {
    check(&[
        (".byte 1, 2, 0xff", "01 02 ff"),
        (".short 0x1234", "34 12"),
        (".hword 0x1234", "34 12"),
        (".word 0x1234", "34 12 00 00"),
        (".int 0x12345678", "78 56 34 12"),
        (".long 0x12345678", "78 56 34 12"),
    ]);
}

#[test]
fn programs() {
    check_programs(&[
        (
            "short conditional branches, backward and forward",
            "top:\n\tbc $top\n\tbnc $top\n\tbz $fwd\n\tbnz $fwd\n\tbh $top\n\tbnh $fwd\nfwd:\n\tnop\n",
            "dc fe de fc dd 08 df 06 61 c3 f5 61 d3 00 00",
        ),
        (
            "unconditional relative branches and calls",
            "start:\n\tbr $start\n\tbr $!start\n\tcall $!start\n\tbr $!later\n\tcall $!later\n\tbr $later\nlater:\n\tret\n",
            "ef fe ee fb ff fe f8 ff ee 05 00 fe 02 00 ef 00 d7",
        ),
        (
            "bit tests with short displacements",
            "here:\n\tbt a.0, $here\n\tbf a.7, $here\n\tbtclr a.1, $here\n\tbt 0xffe30.2, $here\n\tbf 0xfff20.3, $here\n\tbtclr 0xfff10.4, $here\n\tbt psw.5, $there\n\tbf [hl].6, $there\n\tbtclr es:[hl].1, $there\n\tbt es:[hl].2, $there\nthere:\n\tnop\n",
            "31 03 fd 31 75 fa 31 11 f7 31 22 30 f3 31 b4 20 ef 31 c0 10 eb 31 d2 fa 0b 31 e5 08 11 31 91 04 11 31 a3 00 00",
        ),
        (
            "conditional branch relaxed to the long form",
            "\tbc $far\n\tbnc $far\n\tbz $far\n\tbnz $far\n\tbh $far\n\tbnh $far\n\t.space 300\nfar:\n\tnop\n",
            "de 03 ee 47 01 dc 03 ee 42 01 df 03 ee 3d 01 dd 03 ee 38 01 61 d3 03 ee 32 01 61 c3 03 ee 2c 01 00*301",
        ),
        (
            "bit tests relaxed to the long form",
            "\tbt a.0, $far\n\tbf a.1, $far\n\tbt 0xffe30.2, $far\n\tbf 0xfff20.3, $far\n\tbt psw.4, $far\n\tbt [hl].5, $far\n\tbf es:[hl].6, $far\n\t.space 300\nfar:\n\tnop\n",
            "31 05 03 ee 54 01 31 13 03 ee 4e 01 31 24 30 03 ee 47 01 31 b2 20 03 ee 40 01 31 c4 fa 03 ee 39 01 31 d5 03 ee 33 01 11 31 e3 03 ee 2c 01 00*301",
        ),
        (
            "backward long branches",
            "back:\n\t.space 300\n\tbc $back\n\tbnh $back\n\tbt a.3, $back\n\tbr $!back\n",
            "00*300 de 03 ee cf fe 61 c3 03 ee c9 fe 31 35 03 ee c3 fe ee c0 fe",
        ),
        (
            "edge of the short range, forward, as the reference measures it",
            "\tbc $edge\n\t.space 125\nedge:\n\tnop\n",
            "dc 7d 00*126",
        ),
        (
            "one past the short range, forward",
            "\tbc $edge\n\t.space 128\nedge:\n\tnop\n",
            "de 03 ee 80 00*130",
        ),
        (
            "edge of the short range, backward",
            "edge:\n\t.space 126\n\tbz $edge\n",
            "00*126 dd 80",
        ),
        (
            "data and alignment",
            "\tmov a, #1\n\t.byte 1, 2, 3\n\t.short 0x1234\n\t.long 0x12345678\n\t.balign 4\n\tret\n\tnop\n\tnop\n\tnop\n",
            "51 01 01 02 03 34 12 78 56 34 12 00 d7 00 00 00",
        ),
        (
            "label differences as immediates and displacements",
            "start:\n\tnop\n\tnop\n\tnop\nend:\n\tmovw ax, #end - start\n\tmov a, #end - start\n\tmov a, [hl+end-start]\n\tmovw ax, end-start[bc]\n",
            "00 00 00 30 03 00 51 03 8c 03 79 03 00",
        ),
        (
            "a relative branch to a label defined by .set",
            "\t.set target, .\n\tnop\n\tbnz $target\n",
            "00 df fd",
        ),
        (
            "equated constants pick the direct-address form",
            "\t.set PORT, 0xfff20\n\t.set FLAGS, 0xffe30\n\tmov PORT, #1\n\tmov a, FLAGS\n\tset1 PORT.3\n\tbt FLAGS.1, $done\ndone:\n\tret\n",
            "ce 20 01 8d 30 71 3a 20 31 12 30 00 d7",
        ),
        (
            "comments and statement separators",
            "\tmov a, #1 ; a comment\n# a line comment\n\tmov x, #2 @ mov b, #3\n\tnop /* block */\n",
            "51 01 50 02 53 03 00",
        ),
    ]);
}

// ---- instruction-set variants --------------------------------------------------

#[test]
fn the_g14_multiply_and_divide_group() {
    // `rl78-elf-as -mg14`. `divwu` is `0b`, not the `04` of some manuals.
    check(&[
        ("mulhu", "ce fb 01"),
        ("mulh", "ce fb 02"),
        ("divhu", "ce fb 03"),
        ("divwu", "ce fb 0b"),
        ("machu", "ce fb 05"),
        ("mach", "ce fb 06"),
    ]);
    assert_eq!(text_for("rl78g14", "mulhu"), text_for("rl78", "mulhu"));
}

#[test]
fn the_g13_has_no_hardware_multiply_or_divide() {
    for m in ["mulhu", "mulh", "divhu", "divwu", "machu", "mach"] {
        let e = errors_for("rl78g13", m);
        assert!(e.contains("G14"), "`{m}` on the G13: {e}");
    }
    // `mulu` is in every core.
    assert_eq!(text_for("rl78g13", "mulu x"), vec![0xd6]);
}

// ---- target properties ---------------------------------------------------------

#[test]
fn target_properties_match_the_reference_objects() {
    let a = arch::lookup("rl78").expect("rl78 backend");
    // `rl78-elf-readelf -h`: "Class: ELF32", "Machine: Renesas RL78".
    assert_eq!(a.elf_machine(), 197);
    assert_eq!(a.pointer_bytes(&a.initial_state()), 4);
    // `nop` is `00`, and that is what `.balign` pads code with.
    assert_eq!(text_for("rl78", "nop"), vec![0x00]);
    assert_eq!(a.nop_fill(&a.initial_state(), 3), vec![0, 0, 0]);
    // `.byte`/`.short`/`.3byte`/`.long` of an external symbol, read back with
    // `rl78-elf-readelf -r`.
    assert_eq!(a.data_reloc(1, false), Some(0x08)); // R_RL78_DIR8S
    assert_eq!(a.data_reloc(2, false), Some(0x05)); // R_RL78_DIR16S
    assert_eq!(a.data_reloc(3, false), Some(0x02)); // R_RL78_DIR24S
    assert_eq!(a.data_reloc(4, false), Some(0x01)); // R_RL78_DIR32
    // The reference builds `.short sym - .` from a stack of `R_RL78_OP*`
    // relocations, which have no single-relocation equivalent.
    assert_eq!(a.data_reloc(2, true), None);
    for alias in ["rl78g13", "rl78g14", "RL78"] {
        assert!(arch::lookup(alias).is_some(), "{alias}");
    }
}

#[test]
fn unresolved_operands_get_the_reference_relocations() {
    // The same source through `rl78-elf-as` and `rl78-elf-readelf -rW` gives
    // these offsets, types and addends. The reference relocates `call !start`
    // against the local symbol `start`; rsasm uses the section symbol, which
    // is the same address.
    let src = "\t.text\nstart:\n\
               \tmov a, #ext\n\
               \tmovw ax, #ext+2\n\
               \tmov a, !ext\n\
               \tbr !!ext\n\
               \tmov a, ext\n\
               \tmov a, [hl+ext]\n\
               \tmov a, ext[b]\n\
               \tcall !start\n\
               \tbnz $start\n\
               \t.byte ext\n\
               \t.short ext\n\
               \t.long ext+5\n";
    let asm = assemble_for("rl78", src);
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
            (0x01, 0x08, 0), // R_RL78_DIR8S     ext
            (0x03, 0x05, 2), // R_RL78_DIR16S    ext + 2
            (0x06, 0x05, 0), // R_RL78_DIR16S    ext
            (0x09, 0x02, 0), // R_RL78_DIR24S    ext
            (0x0d, 0x2f, 0), // R_RL78_RH_SADDR  ext
            (0x0f, 0x08, 0), // R_RL78_DIR8S     ext
            (0x11, 0x05, 0), // R_RL78_DIR16S    ext
            (0x14, 0x05, 0), // R_RL78_DIR16S    start
            (0x18, 0x08, 0), // R_RL78_DIR8S     ext
            (0x19, 0x05, 0), // R_RL78_DIR16S    ext
            (0x1b, 0x01, 5), // R_RL78_DIR32     ext + 5
        ]
    );
    // `bnz $start`, 0x16 to 0: -0x18 from the end of the instruction.
    let bytes = section(&asm, ".text");
    assert_eq!(hex(&bytes[0x16..0x18]), "df e8");
}

#[test]
fn relative_branches_out_of_their_section_carry_unbiased_relocations() {
    // The GNU linker computes `R_RL78_DIR8S_PCREL` and `DIR16S_PCREL` as
    // `S + A - (P + size)`, so GNU as writes an addend of 0. Offsets, types
    // and addends below are what `rl78-elf-as` produces for the same source.
    for (src, bytes, kind, offset) in [
        ("br $ext", "ef 00", R_DIR8S_PCREL, 1),
        ("br $!ext", "ee 00 00", R_DIR16S_PCREL, 1),
        ("call $!ext", "fe 00 00", R_DIR16S_PCREL, 1),
    ] {
        let asm = assemble_for("rl78", src);
        assert!(
            !asm.diags.has_errors(),
            "`{src}`: {}",
            asm.diags.render(&asm.sm, false)
        );
        assert_eq!(hex(&section(&asm, ".text")), bytes, "`{src}`");
        assert_eq!(asm.relocs.len(), 1, "`{src}`");
        let r = &asm.relocs[0];
        assert_eq!((r.kind, r.offset, r.addend), (kind, offset, 0), "`{src}`");
    }
}

#[test]
fn a_conditional_branch_to_an_unknown_target_takes_the_long_form() {
    // A deliberate difference from GNU as, which keeps the two-byte form with
    // an 8-bit relocation that fails to link if the target lands more than 127
    // bytes away. Nothing is known about an external symbol's distance, so
    // rsasm takes the form that always reaches: the inverted condition skipping
    // over `br $!ext`.
    let asm = assemble_for("rl78", "bc $ext");
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(hex(&section(&asm, ".text")), "de 03 ee 00 00");
    let r = &asm.relocs[0];
    assert_eq!((r.kind, r.offset, r.addend), (R_DIR16S_PCREL, 3, 0));
}

const R_DIR16S_PCREL: u32 = 0x0a;
const R_DIR8S_PCREL: u32 = 0x0b;

// ---- layout ------------------------------------------------------------------------

#[test]
fn a_short_direct_label_resolves_to_its_low_byte_in_a_flat_image() {
    // `mov a, 0xffe20` is `8d 20` in the reference; a label that lands on
    // 0xFFE20 must encode the same.
    let asm = assemble_flat_for("rl78", "here:\n\tmov a, here\n", 0xffe20);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(hex(&section(&asm, ".text")), "8d 20");
    enc("mov a, 0xffe20", "8d 20");
}

#[test]
fn relaxation_uses_the_exact_reach_of_the_short_form() {
    // 127 bytes back is a displacement of -129 from the end of a 2-byte `bz`.
    // The reference keeps the short form anyway and writes `dd 7f`, which
    // branches forward; the long form is required. Its shape, `df 03 ee`
    // and a 16-bit displacement from the end, is the reference's own for 129
    // bytes back (`df 03 ee 7a ff`).
    let b = text_for("rl78", "edge:\n\t.space 127\n\tbz $edge\n");
    assert_eq!(b.len(), 127 + 5);
    assert_eq!(hex(&b[127..130]), "df 03 ee");
    assert_eq!(i16::from_le_bytes([b[130], b[131]]), -(127 + 5));
    // 126 back fits exactly: the reference agrees (`dd 80`, in `programs`).
    let b = text_for("rl78", "edge:\n\t.space 126\n\tbz $edge\n");
    assert_eq!(hex(&b[126..]), "dd 80");
}

#[test]
fn equated_constants_choose_encodings_like_literals() {
    assert_eq!(
        text_for("rl78", ".set PORT, 0xfff20\n\tmov PORT, #1\n\tset1 PORT.3"),
        text_for("rl78", "\tmov 0xfff20, #1\n\tset1 0xfff20.3"),
    );
    assert_eq!(
        text_for(
            "rl78",
            ".set N, 3\n.set V, 0x80 + N * 0x10 / 3\n\tshl b, N\n\tcallt [V]"
        ),
        text_for("rl78", "\tshl b, 3\n\tcallt [0x90]"),
    );
}

// ---- diagnostics ------------------------------------------------------------------

/// Asserts that `src` fails with a diagnostic containing `needle`.
#[track_caller]
fn fails(src: &str, needle: &str) {
    let e = errors_for("rl78", src);
    assert!(
        e.contains(needle),
        "`{src}` should report `{needle}`, got:\n{e}"
    );
}

#[test]
fn range_violations_name_their_limit() {
    // The reference keeps the low bits of all of these without a word.
    fails("mov a, #256", "(-128 to 255)");
    fails("mov a, [hl+256]", "(-128 to 255)");
    fails("movw ax, #0x10000", "(-32768 to 65535)");
    fails("mov a, !0x12345", "0 to 0xffff, or 0xf0000 to 0xfffff");
    fails("mov a, 0xfe20", "0xffe20 to 0xfff1f");
    fails(
        "add a, 0xfff20",
        "not a short direct address (0xffe20 to 0xfff1f)",
    );
    fails("mov 0x1234, #1", "nor an SFR (0xfff00 to 0xfffff)");
    fails("set1 a.8", "bit number 8 is out of range (0 to 7)");
    fails("shr a, 8", "(1 to 7)");
    fails("shrw ax, 16", "(1 to 15)");
    fails("sar a, 0", "(1 to 7)");
    fails("rol a, 2", "must be 1");
    fails("callt [0xc0]", "(0x80 to 0xbe)");
    fails("callt [0x81]", "must be even");
    fails("br $far\n\t.space 200\nfar:", "(-128 to 127)");
    fails("btclr a.1, $far\n\t.space 200\nfar:", "(-128 to 127)");
}

#[test]
fn operand_errors_match_what_the_reference_rejects() {
    // Each of these is an error in `rl78-elf-as` too.
    fails("movw ax, 0xffe31", "even");
    fails("movw [sp+1], ax", "even");
    fails("mov a, es:0xffe30", "`es:`");
    fails("br es:!0x1234", "`es:`");
    fails("mov a, [hl-1]", "`+`");
    fails("mov h, !0x1234", "only `a`, `x`, `b` and `c`");
    fails("callt [ext]", "constant");
    fails("sar a, #3", "plain number");
    fails("mulhu x", "no operands");
    fails("mov a", "2 operands");
    fails("frobnicate a", "unknown RL78 instruction `frobnicate`");
    for src in [
        "mov a, a",
        "xch a, a",
        "mov x, [hl]",
        "mov [hl], #1",
        "mov [de], #1",
        "mov [hl+b], #1",
        "inc [hl]",
        "cmp x, #1",
        "cmpw sp, #1",
        "onew de",
        "movs [hl], x",
        "cmps x, [hl]",
        "not1 a.1",
        "push sp",
        "mov psw, x",
        "add a, psw",
        "xch a, psw",
        "and1 cy, !0x1234.1",
        "mov1 cy, !0x1234.3",
        "call $label",
        "bc label",
        "br label",
    ] {
        assert!(
            try_text_for("rl78", src).is_err(),
            "`{src}` should be rejected"
        );
    }
}

#[test]
fn registers_are_not_symbols() {
    // The reference lexes these words as registers wherever they appear.
    fails("mov a, #a+1", "`a` is a register");
    fails("bt a.1, $x", "`x` is a register");
    fails("mov a, 0x12[de]", "`de` cannot index");
}

#[test]
fn malformed_input_never_panics() {
    const MNEMONICS: &[&str] = &[
        "nop", "mov", "movw", "xch", "xchw", "add", "addw", "cmp", "cmp0", "cmps", "movs", "inc",
        "incw", "oneb", "onew", "shl", "shlw", "rol", "rolwc", "set1", "not1", "mov1", "and1",
        "bt", "btclr", "bc", "bh", "br", "call", "callt", "push", "sel", "mulhu", "mulu",
    ];
    const FRAGMENTS: &[&str] = &[
        "",
        "a",
        "x",
        "ax",
        "bc",
        "sp",
        "cy",
        "psw",
        "es",
        "rb1",
        "#1",
        "#",
        "!",
        "!!",
        "$",
        "$!",
        "!0x1234",
        "!!0x12345",
        "$l",
        "0xffe30",
        "0xfff20",
        "[hl]",
        "[hl+",
        "[hl+b]",
        "[de+1]",
        "[sp]",
        "[bc]",
        "[0x80]",
        "[",
        "]",
        "[]",
        "es:",
        "es:[hl]",
        "es:!1",
        "1[b]",
        "[b]",
        "a.1",
        "a.",
        ".3",
        "psw.9",
        "[hl].2",
        "f.b.3",
        "0x10.",
        "a . ",
        "(",
        "%",
        "'",
        "\"s\"",
        "99999999999999999999",
        "a.99999999999999999999",
        "[hl+c+1]",
        "es:es:[hl]",
    ];
    for m in MNEMONICS {
        for a in FRAGMENTS {
            let _ = try_text_for("rl78", &format!("{m} {a}"));
            for b in FRAGMENTS {
                let _ = try_text_for("rl78", &format!("l:\n\t{m} {a}, {b}"));
            }
        }
    }
    for src in [
        ",",
        "mov ,",
        "mov a,,",
        "mov a, #1, #2, #3",
        "mov\u{80}",
        "set1 \u{1F600}.1",
    ] {
        let _ = try_text_for("rl78", src);
    }
}

// ---- dialects ------------------------------------------------------------------------

#[test]
fn renesas_dialect_source_assembles_to_the_same_bytes() {
    // CC-RL spelling as far as the core's Renesas dialect covers it: bare and
    // dotted data directives, `H` suffixes, `;` comments and upper case. The
    // operand syntax is the GNU one; see the module documentation.
    let renesas =
        "\tMOV A, #10H ; load\n\tDB 1, 2\n\t.DW 1234H\n\tBZ $L1\nL1:\tSET1 PSW.7\n\tRET\n";
    let gnu = "\tmov a, #0x10\n\t.byte 1, 2\n\t.short 0x1234\n\tbz $l1\nl1:\tset1 psw.7\n\tret\n";
    assert_eq!(
        text_dialect("rl78", Dialect::Renesas, renesas),
        text_for("rl78", gnu),
    );
}
