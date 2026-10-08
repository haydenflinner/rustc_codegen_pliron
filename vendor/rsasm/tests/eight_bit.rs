//! The 8-bit dialect, which the 6502, Z80 and 8080 backends read by default.
//!
//! Every expected image here was produced by the reference assembler named in
//! its case, from the same source, by `tools/xas-diff/run.sh`'s recipes:
//! cc65's ca65 with ld65 for the 6502, GNU as for the Z80 linked at address 0,
//! vasm's oldstyle syntax for both, and the Macro Assembler AS with p2bin for
//! the 8080. The corpora there are the full check; these keep the rules that
//! are easiest to break in the hermetic suite.

#![cfg(feature = "retro")]

mod common;

use common::hex;
use rsasm::arch;
use rsasm::assembler::{Assembler, Options};
use rsasm::section::SectionId;

struct Case {
    name: &'static str,
    /// Which assembler produced `bytes`.
    reference: &'static str,
    src: &'static str,
    bytes: &'static str,
}

/// Assembles `src` the way `rsasm -a <arch> -f bin` does: in the backend's
/// default dialect, as a flat image at address 0.
fn assemble(archname: &str, src: &str) -> Assembler {
    let a = arch::lookup(archname).expect("backend is compiled in");
    let options = Options::new()
        .with_dialect(a.default_dialect())
        .with_relocatable(false);
    let mut asm = Assembler::new(a, options);
    asm.assemble_str("test.s", src);
    asm.finish();
    asm
}

#[track_caller]
fn check(archname: &str, cases: &[Case]) {
    for case in cases {
        let asm = assemble(archname, case.src);
        assert!(
            !asm.diags.has_errors(),
            "{}: {}",
            case.name,
            asm.diags.render(&asm.sm, false)
        );
        let got = hex(&asm.section_bytes(SectionId(0)));
        assert_eq!(
            got, case.bytes,
            "\n  {archname}: {}\n  want ({}): {}\n   got: {got}",
            case.name, case.reference, case.bytes
        );
    }
}

#[track_caller]
fn errors(archname: &str, src: &str) -> String {
    let asm = assemble(archname, src);
    assert!(asm.diags.has_errors(), "expected an error:\n{src}");
    asm.diags.render(&asm.sm, false)
}

#[test]
fn mos6502_source_matches_its_references() {
    check(
        "6502",
        &[
            Case {
                name: "immediates, zero page and absolute",
                reference: "ca65 2.19",
                src: r#"        lda #$12
        lda $12
        lda $1234,x
        lda ($12),y
        ldx $12,y
        jmp ($1234)
        asl a
"#,
                bytes: "a9 12 a5 12 bd 34 12 b1 12 b6 12 6c 34 12 0a",
            },
            Case {
                name: "zero page chosen as ca65 chooses it",
                reference: "ca65 2.19",
                src: r#"ptr     = $80
        .org $0200
start:  lda ptr
        lda (ptr),y
        lda later
        lda a:ptr
        ldx z:later
        lda <start
        lda #>start
        jmp start
later   = $40
"#,
                bytes: "a5 80 b1 80 ad 40 00 ad 80 00 a6 40 a5 00 a9 02 4c 00 02",
            },
            Case {
                name: "a label org placed in the zero page",
                reference: "ca65 2.19",
                src: r#"        .org $0010
var:    .byte 0
        lda var
        lda fwd
fwd:    rts
"#,
                bytes: "00 a5 10 ad 16 00 60",
            },
            Case {
                name: "ca65 data directives",
                reference: "ca65 2.19",
                src: r#"        .org $c000
        .byte 1, $02, %11, 'a', "bc"
        .word $1234, *, *
        .dbyt $1234
        .res 2, $ff
        .asciiz "ok"
        .lobytes main
        .hibytes main
main:   rts
"#,
                bytes: "01 02 03 61 62 63 34 12 08 c0 0a c0 12 34 ff ff 6f 6b 00 15 c0 60",
            },
            Case {
                name: "a ca65 macro",
                reference: "ca65 2.19",
                src: r#"        .macro  load val, addr
        lda #val
        sta addr
        .endmacro
        load 1, $0200
lda #2
"#,
                bytes: "a9 01 8d 00 02 a9 02",
            },
            Case {
                name: "vasm labels without colons",
                reference: "vasm oldstyle",
                src: r#"        org $c000
reset   ldx #0
loop    lda msg,x
        beq done
        inx
        bne loop
done    rts
msg     byte "HI",0
"#,
                bytes: "a2 00 bd 0b c0 f0 03 e8 d0 f8 60 48 49 00",
            },
        ],
    );
}

#[test]
fn z80_source_matches_its_references() {
    check(
        "z80",
        &[
            Case {
                name: "Zilog operands and numbers",
                reference: "GNU as 2.47",
                src: r#"        ld a,(ix+5)
        ld (iy-3),0FFh
        ex af,af'
        ld hl,$1234
        ld de,1234h
        ld a,%1010
        jp (ix)
        rst 38h
        in a,(c)
        ld a,ixh
        LD B,(IX+1)
"#,
                bytes: "dd 7e 05 fd 36 fd ff 08 21 34 12 11 34 12 3e 0a dd e9 ff ed 78 dd 7c dd 46 01",
            },
            Case {
                name: "Zilog data directives and DEFL",
                reference: "GNU as 2.47",
                src: r#"n       defl 1
        ld a,n
n       defl n+1
        ld a,n
        db 1,"ab",'c'
        dw msg,$
        defs 2,$aa
msg:    defm "xy"
"#,
                bytes: "3e 01 3e 02 01 61 62 63 0e 00 0a 00 aa aa 78 79",
            },
            Case {
                name: "a CP/M program with org and colonless labels",
                reference: "vasm oldstyle",
                src: r#"bdos    equ 5
        org 100h
start   ld de,msg
        ld c,9
        call bdos
        ret
msg     db 'Hi$'
"#,
                bytes: "11 09 01 0e 09 cd 05 00 c9 48 69 24",
            },
            Case {
                name: "org pads between pieces of a ROM",
                reference: "vasm oldstyle",
                src: r#"        org 0
        jp reset
        org 8
reset   di
        out (c),0
"#,
                bytes: "c3 08 00 00 00 00 00 00 f3 ed 71",
            },
        ],
    );
}

#[test]
fn i8080_source_matches_its_references() {
    check(
        "i8080",
        &[
            Case {
                name: "an Intel program",
                reference: "AS 1.42 bld311",
                src: r#"BDOS    EQU     5
        ORG     100H
START:  LXI     D,MSG
        MVI     C,9
        CALL    BDOS
LOOP    DCR     C
        JNZ     LOOP
        RST     7
        RET
MSG:    DB      'Hi',0DH,0AH,'$'
"#,
                bytes: "11 0e 01 0e 09 cd 05 00 0d c2 08 01 ff c9 48 69 0d 0a 24",
            },
            Case {
                name: "an Intel macro and SET",
                reference: "AS 1.42 bld311",
                src: r#"LOAD    MACRO   REG,VAL
        MVI     REG,VAL
        ENDM
N       SET     1
        LOAD    A,N
N       SET     N+1
        LOAD    B,N
"#,
                bytes: "3e 01 06 02",
            },
        ],
    );
}

#[test]
fn a_first_column_word_is_a_label_unless_it_names_an_instruction() {
    // `done` without a colon is a label, as vasm has it, and `rts` in column
    // 0 is the instruction, as ca65 has it; so the source means the same as
    // the spelling both references read, whose bytes the cases above check.
    let bytes = |src: &str| {
        let asm = assemble("6502", src);
        assert!(
            !asm.diags.has_errors(),
            "{}",
            asm.diags.render(&asm.sm, false)
        );
        hex(&asm.section_bytes(SectionId(0)))
    };
    assert_eq!(
        bytes("done jmp done\nrts\n"),
        bytes("done:   jmp done\n        rts\n")
    );
    // `RES` is a Z80 instruction, so ca65's `res` is a directive only when
    // dotted. The bytes are GNU as's.
    let asm = assemble("z80", "        res 0,b\n");
    assert_eq!(hex(&asm.section_bytes(SectionId(0))), "cb 80");
}

#[test]
fn org_cannot_go_back_before_the_section_start() {
    let err = errors("z80", "        org 100h\n        nop\n        org 80h\n");
    assert!(
        err.contains("before the start of the section"),
        "got: {err}"
    );
}

#[test]
fn only_the_nmos_6502_instruction_set_is_offered() {
    let err = errors("6502", "        .setcpu \"65C02\"\n");
    assert!(err.contains("not supported"), "got: {err}");
    let asm = assemble(
        "6502",
        "        .setcpu \"6502\"\n        .p02\n        nop\n",
    );
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
}

#[test]
fn an_index_register_half_does_not_mix_with_h_l_or_memory() {
    for src in [
        "ld ixh,h",
        "ld l,iyl",
        "ld ixh,iyl",
        "ld ixh,(hl)",
        "ld (ix+1),ixl",
    ] {
        let err = errors("z80", &format!("        {src}\n"));
        assert!(err.contains("index register half"), "{src}: {err}");
    }
    assert!(errors("z80", "        rlc ixh\n").contains("invalid operands"));
    assert!(errors("z80", "        out (c),1\n").contains("takes a register or 0"));
}

/// Malformed 8-bit source must produce a diagnostic, never a panic. The
/// assertion is only that assembly terminated.
#[test]
fn malformed_8bit_input_never_panics() {
    let lines = [
        "*=",
        "* =",
        "*= $",
        "org",
        "org -1",
        "org label",
        "ORG 10H\nORG 10H",
        ".org",
        "equ",
        "x equ",
        "x defl",
        "x set",
        "x :=",
        ":= 1",
        ".segment",
        ".segment CODE",
        ".segment \"X\" :",
        ".setcpu",
        ".setcpu 6502",
        ".res",
        ".res ,",
        ".dbyt",
        ".lobytes",
        ".hibytes ,",
        ".asciiz 1",
        ".lobyte(",
        ".hibyte()",
        "lda #<",
        "lda >",
        "lda ^",
        "lda z:",
        "lda a:",
        "lda z:,x",
        "lda (z:1),y",
        "lda $",
        "lda *",
        "lda **",
        "lda #$",
        "lda %",
        "lda %2",
        "lda $g",
        "af'",
        "ex af,af'",
        "ex af',af",
        "ld a,af'",
        "'",
        "''",
        "db '",
        "db ''''",
        "ld ixh",
        "ld ixh,",
        "ld ixh,ixh",
        "inc ixl,1",
        "in f",
        "in f,(b)",
        "in (c),a",
        "out (c),",
        "out (c),x",
        ".macro",
        ".endmacro",
        "m macro\n endm\n m 1,2",
        ".repeat",
        ".endrep",
        "if",
        "endif",
        "LOAD MACRO",
        "0FFH",
        "0B00H",
        "$FF",
        "%101",
        "lda 0ffh,z",
        "end",
        "db",
        "dw",
        "ds",
        "defs 1,",
    ];
    for archname in ["6502", "z80", "i8080"] {
        for line in lines {
            for src in [line.to_string(), format!("        {line}\n")] {
                let _ = assemble(archname, &src).section_bytes(SectionId(0));
            }
        }
    }
}
