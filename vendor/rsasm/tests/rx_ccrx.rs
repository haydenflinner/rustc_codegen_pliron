//! RX source in the syntax of Renesas CC-RX.
//!
//! No Renesas assembler is available to compare against, so the syntax
//! follows the *CC-RX Compiler User's Manual*, R20UT3248EJ0115, and every
//! encoding comes from GNU as: each program in `PAIRS` is a pair in
//! `tools/xas-diff/rx-ccrx-pairs.txt`, where it sits beside a GNU-syntax
//! program meaning the same thing, and the expected bytes are what
//! `rx-elf-as` (GNU binutils 2.47) made of that GNU half.
//! `tools/xas-diff/run.sh rx-ccrx` checks the pairing again. The data-section
//! tests below quote their GNU source the same way.

#![cfg(feature = "rx")]

mod common;
use common::*;
use rsasm::assembler::Assembler;
use rsasm::lexer::Dialect::CcRx;
use rsasm::section::SectionKind;

fn asm(src: &str) -> Assembler {
    assemble_dialect("rx", CcRx, src)
}

/// The bytes of section `name` of a relocatable assembly, or the
/// diagnostics.
fn section(src: &str, name: &str) -> Result<String, String> {
    let asm = asm(src);
    if asm.diags.has_errors() {
        return Err(asm.diags.render(&asm.sm, false));
    }
    let s = asm
        .sections
        .iter()
        .find(|s| asm.interner.get(s.name) == name)
        .unwrap_or_else(|| panic!("no section `{name}`"));
    Ok(hex(&asm.section_bytes(s.id)))
}

/// The first section's bytes, or the diagnostics.
fn ccrx(src: &str) -> Result<String, String> {
    section(src, ".text")
}

#[track_caller]
fn fails(src: &str, needle: &str) {
    match ccrx(src) {
        Ok(bytes) => panic!("expected an error mentioning `{needle}`, got {bytes}\nsource:\n{src}"),
        Err(e) => assert!(
            e.contains(needle),
            "`{needle}` not in:\n{e}\nsource:\n{src}"
        ),
    }
}

#[track_caller]
fn warns(src: &str, needle: &str) {
    let asm = asm(src);
    let text = asm.diags.render(&asm.sm, false);
    assert!(!asm.diags.has_errors(), "{text}");
    assert!(text.contains(needle), "`{needle}` not in:\n{text}");
}

/// (what the pair shows, CC-RX source, the GNU half's bytes from GNU as)
const PAIRS: &[(&str, &str, &str)] = &[
    (
        "comments and labels",
        "; a comment line\nLABEL1:\tMOV.L\t[R1], R2\t; Example of a mnemonic.\n\tNOP\n",
        "ec 12 03",
    ),
    (
        "numbers: B, O and H suffixes, and decimal",
        "\t.LWORD\t1011000B, 1011000b, 60702O, 60702o, 9243\n\t.LWORD\t0A5FH, 5FH, 0a5fh, 5fh, 0FFFFFF81H\n",
        "58 00 00 00 58 00 00 00 c2 61 00 00 c2 61 00 00 1b 24 00 00 5f 0a 00 00 5f 00 00 00 5f 0a 00 00 5f 00 00 00 81 ff ff ff",
    ),
    (
        "characters and strings in .BYTE",
        "\t.BYTE\t\"abcd\", 'x', \"A\", 1, 2, 3\n",
        "61 62 63 64 78 41 01 02 03",
    ),
    (
        "widths of .BYTE, .WORD and .LWORD",
        "\t.BYTE\t0FFH, -1\n\t.WORD\t1, 1234H\n\t.LWORD\t11111111H, 22222222H\n",
        "ff ff 01 00 34 12 11 11 11 11 22 22 22 22",
    ),
    (
        "operator precedence: shifts below `+`, `&` above `|` and `^`",
        "\t.LWORD\t1+2<<1, 2&1+1, 6|1^3, 1<<2+1, 7&3|8\n\t.LWORD\t10%4*2, ~0&0FFH, 2+3*4, 16>>2-1, -(3-5)\n",
        "06 00 00 00 02 00 00 00 04 00 00 00 08 00 00 00 0b 00 00 00 04 00 00 00 ff 00 00 00 0e 00 00 00 08 00 00 00 02 00 00 00",
    ),
    (
        ".EQU symbols in operands and expressions",
        "symbol\t.EQU\t1\nsymbol1\t.EQU\tsymbol+symbol\nCNT\t.EQU\t0FFH\n\tMOV.L\t#symbol1, R1\n\tMOV.L\t#CNT, R2\n\tADD\t#symbol+2, R3\n",
        "66 21 75 42 ff 62 33",
    ),
    (
        "names with `$`, `_` and `.`",
        "\t.GLB\tname1, name2\n\t.GLB\tname4\nlab$1:\tNOP\n_x.y:\tNOP\n\tBRA\tlab$1\n\tBRA\t_x.y\n",
        "03 03 2e fe 2e fd",
    ),
    (
        "`$` is the location symbol",
        "\tBRA\t$\n\t.LWORD\t$-lab\nlab:\tNOP\n",
        "2e 00 fc ff ff ff 03",
    ),
    (
        "temporary labels `?:`, `?+` and `?-`",
        "?:\n\tBRA\t?+\n\tNOP\n\tNOP\n\tNOP\n?:\n\tBRA\t?-\n\tBRA\t?-\n",
        "0c 03 03 03 2e 00 2e fe",
    ),
    (
        "size specifiers",
        "\tMOV.B\t#0, [R3]\n\tMOV.W\tR1, R2\n\tMOV.L\tR1, R2\n\tmov.b\t[r1], r2\n\tMOVU.W\t4[R1], R2\n",
        "f8 34 00 df 12 ef 12 cc 12 b8 92",
    ),
    (
        "branch distance specifiers",
        "L0:\tBRA.S\tL1\n\tBEQ.S\tL1\n\tNOP\n\tNOP\nL1:\tBRA.B\tL0\n\tBRA.W\tL0\n\tBRA.A\tL0\n\tBRA.L\tR1\n\tBSR.W\tL0\n\tBSR.A\tL0\n\tBSR.L\tR2\n\tBNE.W\tL0\n\tBGT.B\tL0\n\tRTS\n",
        "0c 13 03 03 2e fc 38 fa ff 04 f7 ff ff 7f 41 39 f1 ff 05 ee ff ff 7f 52 3b e8 ff 2a e5 02",
    ),
    (
        "branches without a specifier take the shortest distance",
        "L0:\tBRA\tL1\n\tBEQ\tL1\n\tBNE\tL0\n\tBSR\tL0\nL1:\tRTS\n",
        "0f 16 21 fe 39 fc ff 02",
    ),
    (
        "the addressing modes of §5.1.5 (2)",
        "\tADD\tR1, R2\n\tADD\t[R1], R2\n\tADD\t400[R1], R2\n\tMOV.L\t#-100, R2\n\tRACW\t#1\n\tBSET\t#7, R10\n\tADD\t#15, R8\n\tBSET\t#31, R10\n\tMOV.L\tR3, 124[R1]\n\tMOV.L\t[R3+], R1\n\tMOV.L\t[-R3], R1\n\tMOV.L\t[R3,R1], R2\n\tMOV.L\tR3, [R1,R2]\n\tMVFC\tPSW, R2\n\tCLRPSW\tU\n\tADD\t[R1].B, R2\n\tAND\t125[R1].UB, R2\n\tMOV.L\tSP, R1\n",
        "4b 12 06 88 12 06 89 12 64 fb 26 9c fd 18 00 78 7a 62 f8 79 fa a7 9b fd 2a 31 fd 2e 31 fe 63 12 fe 21 23 fd 6a 02 7f b9 06 08 12 51 12 7d ef 01",
    ),
    (
        "bit length specifiers naming the shortest form",
        "\tMOV.L\t#5:4, R1\n\tMOV.L\t#-100:8, R2\n\tMOV.L\t#200 :8, R2\n\tDIV\t#5:8, R1\n\tADD\t#1000:16, R1\n\tADD\t#-100000:24, R1\n\tMOV.L\t#12345678H:32, R1\n\tMOV.B\t#200:8, [R1]\n\tMOV.W\t#40000:16, [R1]\n\tMOV.L\tR3, 124:5[R1]\n\tADD\t400:8[R1], R2\n\tADD\t4000:16[R1], R2\n\tMOV.L\t#20:8, 4:5[R1]\n\tBSET\t#7:3, [R1].B\n\tBSET\t#31:5, R10\n\tSHLR\t#3:5, R4\n\tMVTIPL\t#9:4\n\tRACW\t#2:1\n",
        "66 51 fb 26 9c 75 42 c8 fd 74 81 05 72 11 e8 03 73 11 60 79 fe fb 12 78 56 34 12 f8 14 c8 f8 19 40 9c a7 9b 06 89 12 64 06 8a 12 e8 03 3e 11 14 f0 17 79 fa 68 34 75 70 09 fd 18 10",
    ),
    (
        "bit length specifiers on a folded difference and a symbol",
        "S:\t.BYTE\t1, 2, 3\nE:\tMOV.L\t#E-S:4, R1\n\tMOV.L\t#ext:32, R2\n\tADD\t#ext:32, R3\n",
        "01 02 03 66 31 fb 22 00 00 00 00 70 33 00 00 00 00",
    ),
    (
        "substitute register names for the PID function",
        "\tMOV.L\t__PID_R13, __PID_R1\n\tADD\t#4, __PID_R15, R6\n\tMOV.L\t4[__PID_R2], R3\n",
        "ef d1 71 f6 04 a8 2b",
    ),
    (
        ".MACRO with arguments, from the manual's example (page 486)",
        "mac\t.MACRO\tp1,p2,p3\n\t.IF ..MACPARA == 3\n\t.IF 'p1' == 'byte'\n\tMOV.B #p2,[p3]\n\t.ELSE\n\tMOV.W #p2,[p3]\n\t.ENDIF\n\t.ELIF ..MACPARA == 2\n\t.IF 'p1' == 'byte'\n\tMOV.B #p2,[R3]\n\t.ELSE\n\tMOV.W #p2,[R3]\n\t.ENDIF\n\t.ELSE\n\tMOV.W R3,R1\n\t.ENDIF\n\t.ENDM\n\tmac word,10,R3\n\tmac byte,20\n\tmac byte\n",
        "f8 35 0a f8 34 14 df 31",
    ),
    (
        "`@` concatenates in a macro body (page 497)",
        "mov_nibble .MACRO p1,src,dest\n\tMOV.@p1 src,dest\n\t.ENDM\n\tmov_nibble W,R1,R2\n\tmov_nibble L,R3,R4\n",
        "df 12 ef 34",
    ),
    (
        ".EXITM and .LOCAL",
        "data1\t.MACRO value\n\t.IF value == 0\n\t.EXITM\n\t.ENDIF\n\t.LOCAL m1\nm1:\tNOP\n\tBRA m1\n\t.ENDM\n\tdata1 0\n\tdata1 1\n\tdata1 2\n",
        "03 2e ff 03 2e ff",
    ),
    (
        ".MREPEAT and ..MACREP, from the manual's example (page 490)",
        "mac\t.MACRO value,reg\n\t.MREPEAT value\n\tMOV.B #0,..MACREP[reg]\n\t.ENDR\n\t.ENDM\n\tmac 3,R3\n",
        "3c 31 00 3c 32 00 3c 33 00",
    ),
    (
        ".IF, .ELIF and .ELSE; an undefined symbol is 0",
        "TYPE\t.EQU\t1\n\t.IF TYPE==0\n\t.BYTE \"Proto Type Mode\"\n\t.ELIF TYPE>0\n\t.BYTE \"Mass Production Mode\"\n\t.ELSE\n\t.BYTE \"Debug Mode\"\n\t.ENDIF\n\t.IF UNDEFINED == 0\n\tNOP\n\t.ENDIF\n\t.IF 'A' < 'B'\n\tRTS\n\t.ENDIF\n",
        "4d 61 73 73 20 50 72 6f 64 75 63 74 69 6f 6e 20 4d 6f 64 65 03 02",
    ),
    (
        ".DEFINE replaces a name with a string (page 499)",
        "X_HI\t.DEFINE\tR1\nARGS\t.DEFINE\t\"#2, R3\"\n\tMOV.L\t#0, X_HI\n\tMOV.L\tARGS\n\t.IF __RENESAS__ == 1\n\tNOP\n\t.ENDIF\n",
        "66 01 66 23 03",
    ),
    (
        ".ALIGN pads code with NOP (03H), which GNU as spells as `.skip`",
        "\tMOV.L\tR1, R2\n\t.ALIGN\t4\n\tRTS\n\t.ALIGN\t2, FILL=0\n\tNOP\n\tRTS\n",
        "ef 12 03 03 02 03 03 02",
    ),
    (
        ".OFFSET pads code with NOP (03H)",
        "\tNOP\n\t.OFFSET\t8\n\tRTS\n",
        "03 03 03 03 03 03 03 03 02",
    ),
    (
        ".END ends the source",
        "\tNOP\n\t.END\n\tthis is not assembled\n",
        "03",
    ),
];

#[test]
fn programs_match_their_gnu_equivalents() {
    let mut failures = Vec::new();
    for (name, src, want) in PAIRS {
        let got = ccrx(src).unwrap_or_else(|e| e);
        if got != *want {
            failures.push(format!("{name}\n  want: {want}\n   got: {got}"));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

// ---- sections (R20UT3248EJ0115 §5.2.2 and §5.2.4, pages 473-485) ----------

/// (name, is code, holds no file bytes, alignment) of every section but an
/// unused `.text`.
fn sections(src: &str) -> Vec<(String, bool, bool, u64)> {
    let asm = asm(src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    asm.sections
        .iter()
        .filter(|s| s.size > 0 || asm.interner.get(s.name) != ".text")
        .map(|s| {
            (
                asm.interner.get(s.name).to_string(),
                s.flags.exec,
                s.kind == SectionKind::Nobits,
                s.align,
            )
        })
        .collect()
}

#[test]
fn section_attributes_are_code_romdata_and_data() {
    let got = sections(
        " .SECTION P\n NOP\n .SECTION C,ROMDATA\n .BYTE 1\n .SECTION B,DATA\n .BLKL 1\n \
         .SECTION D,ALIGN=4,ROMDATA\n .LWORD 1\n .SECTION P\n RTS\n",
    );
    let got: Vec<_> = got
        .iter()
        .map(|(n, x, b, a)| (n.as_str(), *x, *b, *a))
        .collect();
    // CODE is the default (page 473); a restarted section keeps what it was.
    assert_eq!(
        got,
        [
            ("P", true, false, 1),
            ("C", false, false, 1),
            ("B", false, true, 1),
            ("D", false, false, 4),
        ]
    );
}

#[test]
fn a_section_keeps_its_attribute() {
    fails(
        " .SECTION X,CODE\n .SECTION X,DATA\n",
        "different relocation attribute",
    );
    fails(" .SECTION X,CODE,DATA\n", "given twice");
    fails(" .SECTION X,ALIGN=16\n", "not 2, 4 or 8");
    fails(
        " .SECTION X,TEXT\n",
        "expected `CODE`, `ROMDATA`, `DATA` or `ALIGN=n`",
    );
}

#[test]
fn romdata_pads_with_fill_or_nop_code() {
    // GNU as: `.section C,"a"`, `.byte 1`, `.balign 4, 0xaa`, `.byte 2`,
    // `.balign 8, 3`, `.byte 3`, `.org 0x10, 0x55`, `.byte 4` (pages 478 and
    // 485).
    let src = " .SECTION C,ROMDATA,ALIGN=8\n .BYTE 1\n .ALIGN 4,FILL=0AAH\n .BYTE 2\n \
               .ALIGN 8\n .BYTE 3\n .OFFSET 10H,FILL=55H\n .BYTE 4\n";
    assert_eq!(
        section(src, "C").unwrap(),
        "01 aa aa aa 02 03 03 03 03 55 55 55 55 55 55 55 04 00 00 00 00 00 00 00"
    );
}

#[test]
fn org_makes_a_section_absolute() {
    // GNU as: `.section A,"a"`, `.ascii "ab"`, `.org 8, 0`, `.byte 1`, `.org
    // 12, 3`, `.byte 2`, and `.section P2,"ax"`, `nop`, `.org 4, 3`, `rts`
    // (page 477). The start address is the linker's to place.
    let src = " .SECTION A,ROMDATA\n .ORG 0FF00H\n .BYTE \"ab\"\n .ORG 0FF08H,FILL=0\n .BYTE 1\n \
               .ORG 0FF0CH\n .BYTE 2\n .SECTION P2,CODE\n .ORG 1000H\n NOP\n .ORG 1004H,FILL=0\n RTS\n";
    assert_eq!(
        section(src, "A").unwrap(),
        "61 62 00 00 00 00 00 00 01 03 03 03 02"
    );
    assert_eq!(section(src, "P2").unwrap(), "03 03 03 03 02");
}

#[test]
fn org_and_offset_belong_to_their_kind_of_section() {
    fails(
        " .SECTION P\n NOP\n .ORG 100H\n",
        "straight after `.SECTION`",
    );
    fails(
        " .SECTION P\n .ORG 100H\n .OFFSET 4\n",
        "absolute-addressing section",
    );
    fails(
        " .SECTION P\n .ORG 100H\n .ORG 0FFH\n",
        "below the section's start",
    );
    fails(" .SECTION P\n .ORG 100000000H\n", "out of range");
    fails(" .OFFSET 4,FILL=100H\n", "out of range (0 to 0FFH)");
    fails(" .OFFSET 4,FIL=1\n", "expected `FILL=value`");
}

#[test]
fn blk_directives_reserve_ram() {
    let asm = asm(" .SECTION B,DATA\nw1: .BLKB 1\nw2: .BLKW 2\nw3: .BLKL 1\n .BLKD 1\n");
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let b = asm
        .sections
        .iter()
        .find(|s| asm.interner.get(s.name) == "B")
        .unwrap();
    // 1 + 2 * 2 + 4 + 8 bytes (pages 479-481).
    assert_eq!(b.size, 17);
    fails(" .BLKW 1\n", "belongs in a `DATA` section");
    fails(" .SECTION B,DATA\n .BYTE 1\n", "allocates no file space");
}

#[test]
fn align_takes_a_power_of_two_and_warns_as_the_manual_does() {
    fails(" .ALIGN 3\n", "power of two from 2 to 65536");
    fails(" .ALIGN 131072\n", "power of two from 2 to 65536");
    // Page 485: a relative-addressing section without `ALIGN=`, or with a
    // smaller one.
    warns(" .SECTION P\n .ALIGN 4\n", "declared without `ALIGN=`");
    warns(
        " .SECTION P,ALIGN=2\n .ALIGN 4\n",
        "larger than the section's `ALIGN=2`",
    );
}

#[test]
fn endian_little_is_the_only_endian() {
    assert_eq!(
        section(" .SECTION C,ROMDATA\n .ENDIAN LITTLE\n .WORD 1234H\n", "C").unwrap(),
        "34 12"
    );
    fails(" .SECTION C,ROMDATA\n .ENDIAN BIG\n", "big-endian");
    fails(" .ENDIAN LITTLE\n", "cannot be used in a `CODE` section");
}

// ---- symbols, files and macros ---------------------------------------------

#[test]
fn glb_and_weak_set_the_binding() {
    let asm = asm(" .GLB START,EXT\n .WEAK W\nSTART: BSR EXT\nW: RTS\n");
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let binding = |n: &str| {
        let id = asm.symbols.lookup(asm.interner.lookup(n).unwrap()).unwrap();
        asm.symbols.get(id).binding
    };
    assert_eq!(binding("START"), rsasm::symbol::Binding::Global);
    assert_eq!(binding("EXT"), rsasm::symbol::Binding::Global);
    assert_eq!(binding("W"), rsasm::symbol::Binding::Weak);
}

#[test]
fn equ_symbols_are_not_redefined() {
    fails("X .EQU 1\nX .EQU 2\n", "symbol `X` is already defined");
}

#[test]
fn include_takes_an_unquoted_file_name() {
    let dir = std::env::temp_dir().join(format!("rsasm-ccrx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let inc = dir.join("initial.src");
    // `03` is `rx-elf-as`'s `nop`.
    std::fs::write(&inc, " NOP\n").unwrap();
    let got = ccrx(&format!(" .INCLUDE {}\n", inc.display()));
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(got.unwrap(), "03");
    fails(" .INCLUDE \"x.inc\"\n", "without quotes");
    fails(" .INCLUDE ..FILE@.inc\n", "`..FILE` is not supported");
    fails(" .INCLUDE nosuchfile.inc\n", "cannot find include file");
}

#[test]
fn define_strings_are_redefined_and_yield_to_equ() {
    // `66 01` and `66 02` are `rx-elf-as`'s `mov.l #0, r1` and `mov.l #0, r2`.
    assert_eq!(
        ccrx("R .DEFINE R1\n MOV.L #0, R\nR .DEFINE 'R2'\n MOV.L #0, R\n").unwrap(),
        "66 01 66 02"
    );
    warns("X .EQU 1\nX .DEFINE 2\n", "takes priority over `.DEFINE`");
    warns("X .DEFINE 2\nX .EQU 1\n", "takes priority over `.EQU`");
    assert_eq!(ccrx(" .IF __ASRX__ == 1\n NOP\n .ENDIF\n").unwrap(), "03");
    fails(" .DEFINE R1\n", "needs the name it defines");
}

#[test]
fn macro_argument_count_mismatches_are_warnings() {
    // Page 487: missing arguments are empty, extra ones unused, and either
    // is warned about.
    warns(
        "M .MACRO A\n NOP\n .ENDM\n M 1,2\n",
        "takes 1 argument(s), but 2 were given",
    );
    warns(
        "M .MACRO A,B\n NOP\n .ENDM\n M 1\n",
        "takes 2 argument(s), but 1 were given",
    );
}

#[test]
fn a_macro_left_by_exitm_closes_its_conditionals() {
    assert_eq!(
        ccrx("M .MACRO\n .IF 1\n .EXITM\n .ENDIF\n .ENDM\n .IF 1\n M\n NOP\n .ENDIF\n").unwrap(),
        "03"
    );
}

#[test]
fn macpara_and_macrep_are_zero_outside_macros() {
    // Page 490.
    assert_eq!(
        ccrx(" .IF ..MACPARA == 0 && ..MACREP == 0\n NOP\n .ENDIF\n").unwrap(),
        "03"
    );
}

// ---- what rsasm refuses, and says why --------------------------------------

#[test]
fn what_elf_cannot_carry_is_refused_with_a_reason() {
    fails(" .LWORD SIZEOF P\n", "optimizing linker");
    fails(" .LWORD TOPOF P\n", "optimizing linker");
    fails(" .RVECTOR 50,_f\n", "`C$VECT`");
    fails(" .FLOAT 5E2\n", "floating-point");
    fails(" .DOUBLE 5E2\n", "floating-point");
    fails(" .SWITCH\n", "compiler output");
    fails(" .BYTE .LEN{'abc'}\n", "string function `.LEN`");
    fails(" MOV.L __PID_REG, R1\n", "`-pid` option");
    fails(" .LOCAL A\n", "only allowed inside a macro");
}

#[test]
fn bit_length_specifiers_that_would_change_the_code_are_refused() {
    // Each of these is shorter without the specifier, which CC-RX would
    // honour and GNU as ignores (page 460).
    fails(
        " MOV.L #5:8, R1\n",
        "not the width of the shortest form for 5",
    );
    fails(
        " MOV.L 4:8[R1], R2\n",
        "not the width of the shortest form for 4",
    );
    fails(
        " ADD 0:8[R1], R2\n",
        "not the width of the shortest form for 0",
    );
    fails(
        " ADD #200:8, R1\n",
        "not the width of the shortest form for 200",
    );
    fails(
        " DIV #5:4, R1\n",
        "not the width of the shortest form for 5",
    );
    fails(
        " MOV.W #0FFFFH:16, [R1]\n",
        "not the width of the shortest form",
    );
    fails(" .GLB S\n MOV.L #S:16, R1\n", "not a constant");
    fails(" MOV.L #1:7, R1\n", "expected a bit length specifier");
    fails(" BSET #8:3, R1\n", "does not fit a `:3` immediate");
    fails(" RACW #3:1\n", "out of range");
}

/// Malformed CC-RX lines must produce diagnostics, never a panic.
const BAD: &[&str] = &[
    "?",
    "?:",
    " BRA ?",
    " BRA ?+",
    " BRA ?-",
    " .SECTION",
    " .SECTION ,",
    " .SECTION X,",
    " .SECTION X,ALIGN",
    " .SECTION X,ALIGN=",
    " .ORG",
    " .ORG X",
    " .ORG 1,",
    " .ORG 1,FILL",
    " .ORG 1,FILL=",
    " .OFFSET",
    " .OFFSET -1",
    " .ALIGN",
    " .ALIGN 0",
    " .ALIGN 4,FILL=",
    " .ENDIAN",
    " .SECTION C,ROMDATA\n .ENDIAN",
    " .INCLUDE",
    " .BLKB",
    " .SECTION B,DATA\n .BLKB",
    " .BYTE",
    " .BYTE ,",
    " .BYTE \"",
    " .WORD \"AB\"",
    " .GLB",
    " .GLB 1",
    "X .EQU",
    "X .DEFINE",
    "X .DEFINE X\n X",
    ".MACRO",
    "M .MACRO\n .LOCAL\n .ENDM\n M",
    " .MREPEAT",
    " .MREPEAT X\n .ENDR",
    " .MREPEAT -1\n .ENDR",
    " .ENDR",
    " .IF",
    " .ELIF 1",
    " MOV.L #:8, R1",
    " MOV.L #1:, R1",
    " MOV.L :8[R1], R2",
    " MOV.L 4:8[R1, R2",
    " MOV.L __PID_, R1",
    " MOV.L __PID_RX, R1",
    " MOV.@ R1, R2",
    "@",
    "..MACPARA",
    " .LEN{",
    "$",
    "'",
];

#[test]
fn malformed_lines_never_panic() {
    for src in BAD {
        let _ = ccrx(src);
    }
}
