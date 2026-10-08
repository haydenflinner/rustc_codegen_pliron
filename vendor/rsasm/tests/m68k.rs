//! Motorola 68000-family encoding tests.
//!
//! Every expected byte string here was produced by a reference assembler, not
//! written from the manual: `m68k-elf-as --mri` for Motorola syntax,
//! `m68k-elf-as` for GNU syntax (both GNU binutils 2.47, which assume a 68020
//! with a 68881 and a 68851 unless told `-m68000`, `-m68040`, `-mcpu=5475`
//! and so on, as the tests for other CPUs were), and `vasmm68k_mot -no-opt
//! -devpac` (with `-m68020 -m68881 -m68851` for the FPU) for the cases where
//! rsasm deliberately follows vasm instead. The tables at the bottom are the
//! `tools/xas-diff` corpora with the reference output attached.
//!
//! Expected bytes come from relocatable objects, as the harness compares
//! them, so a field that carries a relocation reads as zero.

#![cfg(feature = "m68k")]

mod common;
use common::*;
use rsasm::arch;
use rsasm::lexer::Dialect::{self, Gas, Motorola};
use rsasm::section::SectionId;

/// The `.text` bytes of a relocatable object, panicking on any diagnostic.
#[track_caller]
fn obj(arch: &str, dialect: Dialect, src: &str) -> String {
    let asm = assemble_dialect(arch, dialect, src);
    assert!(
        !asm.diags.has_errors(),
        "assembly failed for `{arch}`:\n{}\nsource:\n{src}",
        asm.diags.render(&asm.sm, false)
    );
    hex(&asm.section_bytes(SectionId(0)))
}

#[track_caller]
fn mot(src: &str, want: &str) {
    let got = obj("m68k", Motorola, src);
    assert_eq!(got, want, "\nsource:\n{src}");
}

#[track_caller]
fn gas(src: &str, want: &str) {
    let got = obj("m68k", Gas, src);
    assert_eq!(got, want, "\nsource:\n{src}");
}

#[track_caller]
fn mot000(src: &str, want: &str) {
    let got = obj("68000", Motorola, src);
    assert_eq!(got, want, "\nsource:\n{src}");
}

#[track_caller]
fn err(arch: &str, dialect: Dialect, src: &str, needle: &str) {
    let e = errors_dialect(arch, dialect, src);
    assert!(
        e.contains(needle),
        "wanted `{needle}` in:\n{e}\nsource:\n{src}"
    );
}

/// Runs a table of one-line cases, reporting every mismatch at once.
#[track_caller]
fn table(arch: &str, dialect: Dialect, cases: &[(&str, &str)]) {
    table_as(arch, dialect, cases, false);
}

/// `flat` assembles an image at address 0, to compare with vasm's `-Fbin`.
fn table_as(arch: &str, dialect: Dialect, cases: &[(&str, &str)], flat: bool) {
    let mut bad = Vec::new();
    for (src, want) in cases {
        // Motorola source is column-sensitive: indent the instruction.
        let line = format!(" {src}\n");
        let asm = if flat {
            assemble_flat_dialect(arch, dialect, &line, 0)
        } else {
            assemble_dialect(arch, dialect, &line)
        };
        let got = if asm.diags.has_errors() {
            asm.diags.render(&asm.sm, false)
        } else {
            hex(&asm.section_bytes(SectionId(0)))
        };
        if got != *want {
            bad.push(format!("  {src}\n    want: {want}\n     got: {got}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} differ:\n{}",
        bad.len(),
        cases.len(),
        bad.join("\n")
    );
}

// ---- the acceptance test ----------------------------------------------------

#[test]
fn the_amiga_dmacon_write_assembles() {
    // `move.w #$7fff,$DFF096`: disable every DMA channel. A 16-bit immediate
    // and a 32-bit absolute address, since $DFF096 does not survive
    // sign-extension from 16 bits.
    mot(" move.w #$7fff,$DFF096\n", "33 fc 7f ff 00 df f0 96");
    gas("movew #0x7fff,0xdff096\n", "33 fc 7f ff 00 df f0 96");
    // And in a flat binary.
    assert_eq!(
        hex(&text_dialect("m68k", Motorola, " move.w #$7fff,$DFF096\n")),
        "33 fc 7f ff 00 df f0 96"
    );
}

// ---- addressing modes -------------------------------------------------------

#[test]
fn absolute_short_is_chosen_for_constants_that_sign_extend() {
    mot(" move.w d0,$0400\n", "31 c0 04 00");
    mot(" move.w d0,$DFF096\n", "33 c0 00 df f0 96");
    mot(" move.l d0,$7ffe\n", "21 c0 7f fe");
    mot(" move.l d0,$8000\n", "23 c0 00 00 80 00");
    // The top of the address space reads as a negative 16-bit address.
    mot(" move.l d0,$ffff8000\n", "21 c0 80 00");
    mot(" move.l d0,-2\n", "21 c0 ff fe");
    // An explicit size wins.
    mot(" move.w $400.l,d0\n", "30 39 00 00 04 00");
    mot(" move.w d0,($400).w\n", "31 c0 04 00");
    gas("movew %d0,0x400:l\n", "33 c0 00 00 04 00");
}

#[test]
fn a_value_not_yet_known_takes_the_form_gnu_as_gives_it() {
    // A forward `equ` is not known when the instruction is read: `abs.L`, a
    // 32-bit base displacement on the 68020, and with an index a 16-bit one
    // under `--mri` but a 32-bit one natively.
    mot(
        " move.w d0,FWD\n move.l d0,FWDL\n jsr FWD(a6)\n move.w FWD(a0,d0.w),d1\n \
         move.w #FWD,d2\nFWD equ -30\nFWDL equ $12345\n",
        "33 c0 ff ff ff e2 23 c0 00 01 23 45 4e b6 01 70 ff ff ff e2 32 30 01 20 ff e2 \
         34 3c ff e2",
    );
    gas(
        " movew %a0@(FWD,%d0:w),%d1\n jsr %a6@(FWD)\n .set FWD, -30\n",
        "32 30 01 30 ff ff ff e2 4e b6 01 70 ff ff ff e2",
    );
    // A 68000 has only the 16- and 8-bit fields.
    mot000(
        " jsr FWD(a6)\n move.w FWD(a0,d0.w),d1\nFWD equ -30\n",
        "4e ae ff e2 32 30 00 e2",
    );
}

#[test]
fn constants_defined_first_get_their_shortest_form() {
    mot(
        "BASE equ $dff000\nDMACON equ $96\nLVO equ -198\nSIZE equ 16\n \
         move.w #$7fff,BASE+DMACON\n lea BASE,a5\n move.w #$8200,DMACON(a5)\n \
         jsr LVO(a6)\n moveq #SIZE,d0\n lea SIZE(a0,d0.w),a1\n",
        "33 fc 7f ff 00 df f0 96 4b f9 00 df f0 00 3b 7c 82 00 00 96 4e ae ff 3a 70 10 \
         43 f0 00 10",
    );
}

#[test]
fn a_zero_displacement_is_dropped() {
    mot(" lea 0(a0),a1\n", "43 d0");
    gas("lea %a0@(0),%a1\n", "43 d0");
    // ...but not from an indexed mode, which has no shorter form.
    mot(" move.w 0(a0,d0.w),d1\n", "32 30 00 00");
}

#[test]
fn registers_in_both_syntaxes() {
    mot(" move.l sp,a6\n", "2c 4f");
    mot(" move.l SP,A0\n", "20 4f");
    mot(" move.w %d0,%d1\n", "32 00");
    gas("movew %sp@,%d0\n", "30 17");
    gas("movew %fp@,%d0\n", "30 16");
    // In GNU syntax a register needs its `%`; `d0` alone is a symbol.
    let asm = assemble_dialect("m68k", Gas, "movew d0,d1\n");
    assert!(!asm.diags.has_errors());
    assert_eq!(
        hex(&asm.section_bytes(SectionId(0))),
        "33 f9 00 00 00 00 00 00 00 00"
    );
}

#[test]
fn index_registers_and_scales() {
    mot(" move.w 4(a1,d2.w),d0\n", "30 31 20 04");
    mot(" move.w 4(a1,d2.l),d0\n", "30 31 28 04");
    mot(" move.w (8,a0,d1.l*4),d0\n", "30 30 1c 08");
    gas("movew %d0,%a0@(8,%d1:w:4)\n", "31 80 14 08");
    gas("movew %d0,8(%a0,%d1)\n", "31 80 18 08");
}

#[test]
fn motorola_index_size_defaults_to_a_word() {
    // vasm and Devpac read `(a0,d1)` as `d1.w`, as Motorola specifies; GNU as
    // makes it a long even under `--mri`. The upper word of `d1` changes
    // which address this is, so Motorola source keeps Motorola's meaning.
    // Expected bytes from vasm.
    mot(" move.w 8(a0,d1),d0\n", "30 30 10 08");
    // GNU syntax keeps GNU as's long.
    gas("movew %d0,8(%a0,%d1)\n", "31 80 18 08");
}

#[test]
fn pc_relative_operands_measure_from_their_extension_word() {
    mot(
        " lea data(pc),a0\n move.w data(pc),d0\n move.w data(pc,d1.w),d2\n \
         pea back(pc)\n jsr back(pc)\n jmp data(pc,d0.l)\nback rts\ndata dc.w 1,2,3\n",
        "41 fa 00 18 30 3a 00 14 34 3b 10 10 48 7a 00 0a 4e ba 00 06 4e fb 08 04 4e 75 \
         00 01 00 02 00 03",
    );
    // In GNU syntax a constant is the displacement itself.
    gas("movew %pc@(8),%d0\n", "30 3a 00 08");
    // In Motorola syntax it is the address; from vasm, in a flat image at 0.
    let asm = assemble_flat_dialect("m68k", Motorola, " move.w 8(pc),d0\n", 0);
    assert_eq!(hex(&asm.section_bytes(SectionId(0))), "30 3a 00 06");
}

#[test]
fn pc_relative_operands_relax_to_a_full_extension_word() {
    let asm = assemble_dialect(
        "m68k",
        Motorola,
        " lea far(pc),a0\n move.w far(pc,d0.w),d1\n ds.b 40000\nfar dc.l 0\n",
    );
    assert!(!asm.diags.has_errors());
    let bytes = hex(&asm.section_bytes(SectionId(0)));
    assert!(
        bytes.starts_with("41 fb 01 70 00 00 9c 4e 32 3b 01 30 00 00 9c 46 00"),
        "{}",
        &bytes[..60]
    );
}

#[test]
fn memory_indirect_and_suppressed_base() {
    mot(" move.l ([8,a0],d1.w,4),d0\n", "20 30 11 26 00 08 00 04");
    mot(" move.l ([8,a0,d1.w],4),d0\n", "20 30 11 22 00 08 00 04");
    mot(" move.w (8,d1.w),d0\n", "30 30 11 a0 00 08");
    gas("movew %d0,%a0@(8)@(4)\n", "31 80 01 62 00 08 00 04");
}

// ---- instruction selection ----------------------------------------------------

#[test]
fn what_is_written_is_what_is_assembled() {
    // GNU as turns these into `moveq`, `addq` and `cmpi`. vasm -no-opt, like
    // rsasm, assembles the instruction written; these bytes are vasm's.
    mot(" move.l #1,d0\n", "20 3c 00 00 00 01");
    mot(" add.w #1,d0\n", "d0 7c 00 01");
    mot(" add.l #1,a0\n", "d1 fc 00 00 00 01");
    mot(" cmp.w #1,d0\n", "b0 7c 00 01");
    mot(" and.w #1,d0\n", "c0 7c 00 01");
    // ...and the quick forms are there for whoever writes them.
    mot(" moveq #1,d0\n", "70 01");
    mot(" addq.w #1,d0\n", "52 40");
}

#[test]
fn one_mnemonic_two_opcodes_is_decided_by_operand() {
    // `add` to an address register is `ADDA`; `add #imm` to memory is `ADDI`.
    mot(" add.l d0,a0\n", "d1 c0");
    mot(" add.l #$10000,4(a0)\n", "06 a8 00 01 00 00 00 04");
    mot(" cmp.w #1,(a0)\n", "0c 50 00 01");
    mot(" cmp.w d0,a0\n", "b0 c0");
    mot(" eor.w #1,d0\n", "0a 40 00 01");
    mot(" and.w #$f8ff,sr\n", "02 7c f8 ff");
    mot(" or.b #1,ccr\n", "00 3c 00 01");
    mot(" move.l d0,a0\n", "20 40");
}

#[test]
fn movem_reverses_its_mask_for_predecrement() {
    // Pushing, the CPU walks the registers from a7 down, so bit 0 is a7.
    mot(" movem.l d0-d3/a0-a2,-(sp)\n", "48 e7 f0 e0");
    // Everywhere else bit 0 is d0.
    mot(" movem.l (sp)+,d0-d3/a0-a2\n", "4c df 07 0f");
    mot(" movem.l a0-a1/d0-d1,(a0)\n", "48 d0 03 03");
    mot(" movem.l d3-d0,(a0)\n", "48 d0 00 0f");
    mot(" movem.l d0,-(sp)\n", "48 e7 80 00");
    mot(" movem.l #$0003,(a0)\n", "48 d0 00 03");
    gas("moveml %d0-%d3/%a0-%a2,%sp@-\n", "48 e7 f0 e0");
}

#[test]
fn byte_immediates_occupy_a_word() {
    mot(" move.b #$ff,d0\n", "10 3c 00 ff");
    // GNU as sign-extends into the ignored high byte.
    mot(" move.b #-128,d0\n", "10 3c ff 80");
    mot(" btst d0,#5\n", "01 3c 00 05");
}

#[test]
fn sized_mnemonics_both_ways() {
    mot(" movew #1,d0\n", "30 3c 00 01");
    gas("move.w %d0,%d1\n", "32 00");
    gas("movl %d0,%d1\n", "22 00");
    // A trailing letter is a size only where the rest is a mnemonic taking
    // it: `bls` is a condition, `divsl` is `divs.l`, `extbl` is `extb.l`.
    mot(" sls d0\n", "53 c0");
    gas("divsl %d0,%d2:%d1\n", "4c 40 1c 02");
    gas("divsll %d0,%d2:%d1\n", "4c 40 18 02");
    gas("extbl %d0\n", "49 c0");
}

// ---- branches -----------------------------------------------------------------

#[test]
fn a_branch_to_the_next_instruction_cannot_be_short() {
    // An 8-bit displacement of 0 is the escape for "16 bits follow".
    mot(" bra next\nnext nop\n", "60 00 00 02 4e 71");
    mot(
        " bsr next\nnext beq next2\nnext2 rts\n",
        "61 00 00 02 67 00 00 02 4e 75",
    );
    gas(" bra next\nnext: nop\n", "60 00 00 02 4e 71");
    // Written short, it is an error — in both references too.
    err("m68k", Motorola, " bra.s next\nnext rts\n", "out of range");
}

#[test]
fn short_branches_both_ways() {
    mot(
        "back nop\n bra back\n bne fwd\n nop\nfwd rts\n",
        "4e 71 60 fc 66 02 4e 71 4e 75",
    );
    mot(" bra *\n dbf d0,*\n", "60 fe 51 c8 ff fe");
}

#[test]
fn branches_relax_through_their_sizes() {
    let z = format!(" dc.l {}\n", ["0"; 31].join(","));
    // 124 and 126 bytes on reach with 8 bits; 128 needs 16.
    let near = obj("m68k", Motorola, &format!(" bra near\n{z}near rts\n"));
    assert!(near.starts_with("60 7c 00"), "{near}");
    let near = obj(
        "m68k",
        Motorola,
        &format!(" bra near\n{z} dc.w 0\nnear rts\n"),
    );
    assert!(near.starts_with("60 7e 00"), "{near}");
    let far = obj(
        "m68k",
        Motorola,
        &format!(" bra far\n{z} dc.w 0,0\nfar rts\n"),
    );
    assert!(far.starts_with("60 00 00 82 00"), "{far}");
    // -128 is the last short displacement backwards.
    let back = obj("m68k", Motorola, "back ds.b 126\n bra back\n bra back\n");
    assert!(back.ends_with("60 80 60 00 ff 7e"), "{back}");
    // A branch that grows can push an earlier one out of reach.
    let push = obj(
        "m68k",
        Motorola,
        &format!(" bra t\n bra far\n{z}t rts\n{z}far rts\n"),
    );
    assert!(push.starts_with("60 00 00 82 60 00 00 fc 00"), "{push}");
    // Past 16 bits a 68020 takes 32, with `$FF` as the escape byte.
    let long = obj(
        "m68k",
        Motorola,
        " beq far\n bsr far\n bra far\n ds.b 40000\nfar rts\n",
    );
    assert!(
        long.starts_with("67 ff 00 00 9c 50 61 ff 00 00 9c 4a 60 ff 00 00 9c 44 00"),
        "{}",
        &long[..60]
    );
}

#[test]
fn a_68000_reaches_far_with_an_absolute_jump() {
    // `bcc` over a `jmp` on the opposite condition; `jmp` and `jsr` for
    // `bra` and `bsr`. From `m68k-elf-as --mri -m68000`.
    let far = obj(
        "68000",
        Motorola,
        " beq far\n bsr far\n bra far\n bne near\n ds.b 200\nnear ds.b 40000\nfar rts\n",
    );
    assert!(
        far.starts_with(
            "66 06 4e f9 00 00 00 00 4e b9 00 00 00 00 4e f9 00 00 00 00 66 00 00 ca 00"
        ),
        "{}",
        &far[..80]
    );
    mot000(
        " bsr ext\n bra ext\n beq ext\n lea ext(pc),a0\n",
        "4e b9 00 00 00 00 4e f9 00 00 00 00 66 06 4e f9 00 00 00 00 41 fa 00 00",
    );
    mot000(
        " lea back(pc),a0\n move.w back(pc,d0.w),d1\nback rts\n",
        "41 fa 00 06 32 3b 00 02 4e 75",
    );
}

#[test]
fn undefined_targets_take_the_widest_form() {
    mot(
        " bsr ext\n bra ext\n bne ext\n jsr ext\n jmp ext\n lea ext(pc),a0\n \
         move.w ext(pc,d0.w),d1\n",
        "61 ff 00 00 00 00 60 ff 00 00 00 00 66 ff 00 00 00 00 4e b9 00 00 00 00 \
         4e f9 00 00 00 00 41 fb 01 70 00 00 00 00 32 3b 01 30 00 00 00 00",
    );
}

#[test]
fn gnu_syntax_relaxes_only_its_j_spellings() {
    gas(
        "back: nop\n bra back\n bsr fwd\n beq back\n nop\nfwd: rts\n",
        "4e 71 60 00 ff fc 61 00 00 08 67 00 ff f4 4e 71 4e 75",
    );
    gas(
        "top: nop\n jeq top\n jne top\n jbsr top\n jra top\n",
        "4e 71 67 fc 66 fa 61 f8 60 f6",
    );
    gas(
        "1: nop\n jra 2f\n jra 1b\n2: rts\n",
        "4e 71 60 02 60 fa 4e 75",
    );
    let far = obj(
        "m68k",
        Gas,
        " jra mid\n jbsr far\n .space 200\nmid: .space 40000\nfar: rts\n",
    );
    assert!(far.starts_with("60 00 00 d0 61 ff 00 00 9d 0c 00"), "{far}");
    let far = obj(
        "68000",
        Gas,
        " jra far\n jbsr far\n .space 40000\nfar: rts\n",
    );
    assert!(
        far.starts_with("4e f9 00 00 00 00 4e b9 00 00 00 00 00"),
        "{far}"
    );
}

#[test]
fn dbcc_counts_down_a_loop() {
    mot(
        " moveq #9,d0\nloop move.b (a0)+,(a1)+\n dbf d0,loop\n rts\n",
        "70 09 12 d8 51 c8 ff fc 4e 75",
    );
}

// ---- relocations --------------------------------------------------------------

/// `(offset, type, addend)` for every relocation, in order.
fn relocs(arch: &str, dialect: Dialect, src: &str) -> Vec<(u64, u32, i64)> {
    let asm = assemble_dialect(arch, dialect, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    let mut v: Vec<_> = asm
        .relocs
        .iter()
        .map(|r| (r.offset, r.kind, r.addend))
        .collect();
    v.sort();
    v
}

#[test]
fn pc_relative_relocations_carry_the_distance_to_their_base_in_the_addend() {
    const R32: u32 = 1;
    const R16: u32 = 2;
    const PC32: u32 = 4;
    const PC16: u32 = 5;
    const PC8: u32 = 6;
    // `m68k-elf-readelf -r` on the `--mri` object: a field that sits after the
    // point the CPU measures from has that distance subtracted, so a short
    // branch's byte gets -1 and a full extension word's displacement +2.
    assert_eq!(
        relocs(
            "m68k",
            Motorola,
            " bsr ext\n bsr.w ext\n jsr ext\n lea ext(pc),a0\n move.w ext(pc,d0.w),d1\n \
             lea ext+8(pc),a0\n move.w d0,ext\n move.w d0,ext.w\n",
        ),
        vec![
            (0x02, PC32, 0),
            (0x08, PC16, 0),
            (0x0c, R32, 0),
            (0x14, PC32, 2),
            (0x1c, PC32, 2),
            (0x24, PC32, 10),
            (0x2a, R32, 0),
            (0x30, R16, 0),
        ]
    );
    // And from `-m68000`, where the brief extension word's byte is +1.
    assert_eq!(
        relocs(
            "68000",
            Motorola,
            " bsr.w ext\n lea ext(pc),a0\n move.w ext(pc,d0.w),d1\n lea ext+8(pc),a0\n",
        ),
        vec![
            (0x02, PC16, 0),
            (0x06, PC16, 0),
            (0x0b, PC8, 1),
            (0x0e, PC16, 8)
        ]
    );
    // GNU syntax, and a reference into another section.
    assert_eq!(
        relocs(
            "m68k",
            Gas,
            " bsr ext\n lea dat(%pc),%a0\n lea dat+4(%pc),%a0\n .data\ndat: .long 0\n",
        ),
        vec![(0x02, PC16, 0), (0x08, PC32, 2), (0x10, PC32, 6)]
    );
}

// ---- the 68000 and 68010 ------------------------------------------------------

#[test]
fn the_68000_rejects_what_it_lacks() {
    for (src, needle) in [
        (" extb.l d0\n", "needs a 68020"),
        (" mulu.l d0,d1\n", "needs a 68020"),
        (" move.w (a0,d1.w*2),d0\n", "scaled index needs a 68020"),
        (" move.w 40000(a0),d1\n", "wider displacements need a 68020"),
        (" move.w 1000(a0,d0.w),d1\n", "8-bit"),
        (" bra.l far\nfar rts\n", "`bra.l` needs a 68020 or later"),
        (
            " move.l ([a0]),d0\n",
            "memory-indirect addressing needs a 68020",
        ),
        (" bfextu d0{1:8},d1\n", "needs a 68020"),
        (" link.l a6,#-100000\n", "needs a 68020"),
        (" tst.l a0\n", "cannot take an address register"),
        (" rtd #4\n", "needs a 68010"),
        (" move ccr,d0\n", "needs a 68010"),
        (" movec d0,vbr\n", "needs a 68010"),
    ] {
        err("68000", Motorola, src, needle);
    }
    // The 68010 has `movec` and `rtd`, but not the 68020's control registers.
    assert_eq!(
        hex(&text_dialect("68010", Motorola, " movec d0,vbr\n rtd #4\n")),
        "4e 7b 08 01 4e 74 00 04"
    );
    err(
        "68010",
        Motorola,
        " movec d0,cacr\n",
        "`cacr` is not a control register `movec` reaches on a 68010",
    );
    // `.arch` switches the instruction set mid-file.
    err("m68k", Gas, " .arch mc68000\n extbl %d0\n", "needs a 68020");
}

// ---- diagnostics --------------------------------------------------------------

#[test]
fn diagnostics_name_the_limit() {
    for (src, needle) in [
        (" moveq #200,d0\n", "`moveq` takes -128 to 127"),
        (" addq #9,d0\n", "out of range (1 to 8)"),
        (" asl #9,d0\n", "shift count 9 is out of range"),
        (" trap #16\n", "trap vector 16"),
        (" move.w #$10000,d0\n", "does not fit in a word"),
        (" move.b #256,d0\n", "does not fit in a byte"),
        (
            " move.b a0,d0\n",
            "address register cannot be used as a byte",
        ),
        (" lea (a0)+,a1\n", "`lea` cannot take `(An)+`"),
        (
            " move.w d0,#1\n",
            "cannot take an immediate as its destination",
        ),
        (" frob d0\n", "unknown instruction `frob`"),
        (" lea.b (a0),a1\n", "does not take a `.b` size"),
        (" move.w (a0,d1.w*3),d0\n", "scale must be 1, 2, 4 or 8"),
        (" movem.l d0,d1\n", "register list to or from memory"),
        (" bra (a0)\n", "branches to a label"),
        (" move.w d0,$12345678.w\n", "cannot be reached with `abs.W`"),
        (" move.w d0{1:2},d1\n", "only the bit-field instructions"),
        (" rts d0\n", "takes no operands"),
    ] {
        err("m68k", Motorola, src, needle);
    }
}

/// Nonsense of every kind, in both syntaxes. The only requirement is a
/// diagnostic rather than a panic.
const MALFORMED: &[&str] = &[
    "move",
    "move ,",
    "move d0,",
    "move ,d0",
    "move #,d0",
    "move #",
    "move (",
    "move )",
    "move (a0",
    "move a0)",
    "move -(",
    "move -(a0",
    "move (a0)+,(",
    "move ()",
    "move (,)",
    "move (,,,)",
    "move ([",
    "move ([]),d0",
    "move ([a0]",
    "move ([a0],",
    "move ([a0],,),d0",
    "move ([a0,a1,a2,a3]),d0",
    "move (a0,a1,a2),d0",
    "move -(d0),d1",
    "move (d0)+,d1",
    "move (pc)+,d0",
    "move -(pc),d0",
    "move (a0,d1*),d0",
    "move (a0,d1*16),d0",
    "move (a0,d1:),d0",
    "move (a0,sr),d0",
    "move d0,d1,d2",
    "move #1 2,d0",
    "move.x d0,d1",
    "move.ww d0,d1",
    ".w d0",
    "movem d0-,(a0)",
    "movem -d0,(a0)",
    "movem d0//d1,(a0)",
    "movem d0-d1-d2,(a0)",
    "movem d0/,(a0)",
    "movem d0/sr,(a0)",
    "movem d0,d1",
    "movem (a0),(a1)",
    "moveq #,d0",
    "moveq d0,d1",
    "lea d0,a0",
    "lea (a0),d0",
    "exg d0",
    "exg d0,(a0)",
    "divs.l d0,d1:",
    "divs.l d0,:d1",
    "divs.l d0,a1:a2",
    "mulu.l d0,(a0)",
    "bfextu d0{",
    "bfextu d0{}",
    "bfextu d0{1},d1",
    "bfextu d0{1:},d1",
    "bfextu d0{:8},d1",
    "bfextu d0{1:8}",
    "bfextu d0,d1",
    "bfextu d0{a0:8},d1",
    "bfins d1{1:2},d0{1:2}",
    "bra",
    "bra #1",
    "bra.x label",
    "dbra label",
    "dbra d0",
    "dbra a0,label",
    "trap d0",
    "trap #1,#2",
    "link a6",
    "link a6,d0",
    "link d0,#4",
    "stop",
    "stop d0",
    "movec d0,d1",
    "movec vbr,vbr",
    "asl",
    "asl #1,(a0)",
    "asl (a0),d0",
    "asl #1,#2,#3",
    "btst",
    "btst #1",
    "btst (a0),d0",
    "btst #256,d0",
    "addq d0,d1",
    "addq #1",
    "addx d0,(a0)",
    "cmpm d0,d1",
    "and a0,d0",
    "eor d0,a0",
    "tst",
    "clr #1",
    "jsr (a0)+",
    "pea #1",
    "swap a0",
    "ext.b d0",
    "chk2 d0,d1",
    "cmp2 (a0),(a1)",
    "move 1/0,d0",
    "move (1/0)(a0),d0",
    "move 99999999999999999999,d0",
    "move #$,d0",
    "move #%2,d0",
    "move #@9,d0",
    "move d0,%",
    "move d0,%%",
    "move %d9,d0",
    "move d0,@",
    "move d0,@(",
    "move %a0@(,d0",
    "move %a0@(1,2,3),%d0",
    "move %a0@(1)@,%d0",
    "move %a0@(1)@(2)@(3),%d0",
    "move %d0@,%d1",
    "move %sr@,%d1",
    "move %a0@+x,%d0",
    "move %a0@(%d1:x),%d0",
    "move %a0@(%d1:w:3),%d0",
    "move %a0@(%d1:w:),%d0",
    "move %a0@(%d1,%d2),%d0",
    "move {,d0",
    "move d0}",
    "move [a0],d0",
    "bra.s",
    "bra.s *+1000",
    "bra.w *+100000",
    "jbsr",
    "moveq #-129,d0",
];

/// Misused registers, which need each syntax's own spelling: in GNU syntax a
/// bare `d0` is a symbol, and `move d0,usp` is a fine absolute address.
const MALFORMED_MOTOROLA: &[&str] = &[
    "move pc,d0",
    "move d0.w,d1",
    "move (sr),d0",
    "bra d0",
    "jmp d0",
    "andi #1,a0",
    "addi #1,ccr",
    "move.b d0,ccr",
    "move usp,d0",
    "move d0,usp",
    "move sr,sr",
];

const MALFORMED_GNU: &[&str] = &[
    "movew %pc,%d0",
    "movew %d0.w,%d1",
    "movew (%sr),%d0",
    "bra %d0",
    "jmp %d0",
    "andiw #1,%a0",
    "addib #1,%ccr",
    "moveb %d0,%ccr",
    "movel %usp,%d0",
    "movel %d0,%usp",
    "movew %sr,%sr",
    "movew % d0,%d1",
    "movew %d0@,%d1",
    "movew %a0@(1)@,%d0",
    "movew %a0@(%d1:q),%d0",
];

#[test]
fn malformed_input_reports_and_never_panics() {
    let mut silent = Vec::new();
    for (dialect, own) in [(Motorola, MALFORMED_MOTOROLA), (Gas, MALFORMED_GNU)] {
        for arch in ["m68k", "68000"] {
            for line in MALFORMED.iter().chain(own) {
                // Indented for Motorola, whose first column holds labels; GNU
                // as does not mind either way.
                let src = format!(" {line}\n");
                if !assemble_dialect(arch, dialect, &src).diags.has_errors() {
                    silent.push(format!("{arch}, {dialect:?}: `{line}`"));
                }
            }
        }
    }
    assert!(
        silent.is_empty(),
        "no diagnostic for:\n{}",
        silent.join("\n")
    );
}

#[test]
fn token_soup_never_panics() {
    // Operand parsing slices token lists by bracket positions, so stress it
    // with every arrangement a small generator reaches. Nothing is asserted
    // beyond returning.
    const PIECES: &[&str] = &[
        "d0", "a7", "%d1", "%a0", "sp", "pc", "sr", "ccr", "usp", "vbr", "(", ")", "[", "]", "{",
        "}", ",", ":", "@", "+", "-", "*", "/", "#", ".w", ".l", "d1.w", "a2.l", "4", "$ff",
        "label", "label.w", "0x10", "8", "w", "l", "%", " ", "'a'",
    ];
    const MNEMONICS: &[&str] = &[
        "move.w", "movem.l", "lea", "bra", "dbf", "btst", "asl", "divs.l", "bfextu", "movec",
        "cmp2.l", "link", "moveq", "addq", "exg", "jmp", "movew", "jra",
    ];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    for _ in 0..3000 {
        let mut line = format!(" {} ", MNEMONICS[next(MNEMONICS.len())]);
        for _ in 0..next(12) {
            line.push_str(PIECES[next(PIECES.len())]);
        }
        line.push('\n');
        for dialect in [Motorola, Gas] {
            let _ = assemble_dialect("m68k", dialect, &line);
            let _ = assemble_dialect("68000", dialect, &line);
        }
    }
}

// ---- target conventions -------------------------------------------------------

#[test]
fn target_conventions() {
    let a = arch::lookup("m68k").expect("m68k backend");
    assert_eq!(a.elf_machine(), 4);
    assert_eq!(a.align_unit(), 2);
    assert_eq!(a.default_dialect(), Motorola);
    assert_eq!(a.data_reloc(4, false), Some(1));
    assert_eq!(a.data_reloc(2, false), Some(2));
    assert_eq!(a.data_reloc(1, false), Some(3));
    assert_eq!(a.data_reloc(4, true), Some(4));
    assert_eq!(a.data_reloc(2, true), Some(5));
    assert_eq!(a.data_reloc(1, true), Some(6));
    assert_eq!(a.data_reloc(8, false), None);
    for name in ["68000", "68010", "68020", "68030", "mc68000", "M68K"] {
        assert!(
            arch::lookup(name).is_some(),
            "`{name}` should name the m68k backend"
        );
    }
    assert!(arch::available().contains(&"m68k"));

    // Code alignment pads with zeroes, as both references do.
    let s = a.initial_state();
    assert_eq!(a.nop_fill(&s, 1), vec![0x00]);
    assert_eq!(a.nop_fill(&s, 4), vec![0x00; 4]);
}

#[test]
fn pc_relative_references_to_plain_addresses_are_relocated() {
    // m68k-elf-as: `bsrw` with R_68K_PC16 `*ABS*+0x2000`, and `jsr` through
    // a 32-bit PC displacement with R_68K_PC32 `*ABS*+0x2002`.
    let asm = assemble_dialect("m68k", Gas, " nop\n bsr F\n jsr (F,%pc)\nF = 0x2000\n");
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(
        hex(&section(&asm, ".text")),
        "4e 71 61 00 00 00 4e bb 01 70 00 00 00 00"
    );
    let got: Vec<(u64, u32, Option<_>, i64)> = asm
        .relocs
        .iter()
        .map(|r| (r.offset, r.kind, r.symbol, r.addend))
        .collect();
    // R_68K_PC16 = 5, R_68K_PC32 = 4.
    assert_eq!(got, vec![(4, 5, None, 0x2000), (10, 4, None, 0x2002)]);
}

#[test]
fn arch_takes_numeric_cpu_names() {
    let e = errors_dialect("m68k", Gas, ".arch 68000\n extb.l %d0\n");
    assert!(e.contains("needs a 68020"), "{e}");
    assert_eq!(
        hex(&text_dialect("68000", Gas, ".arch 68020\n extb.l %d0\n")),
        "49 c0"
    );
}

#[test]
fn code_alignment_pads_with_zeroes() {
    // m68k-elf-as for the GNU spelling, vasm `-devpac` for `cnop`.
    gas(
        " .byte 1\n .balign 8\n rts\n .balign 8\n",
        "01 00 00 00 00 00 00 00 4e 75 00 00 00 00 00 00",
    );
    mot(
        " dc.b 1\n cnop 0,8\n rts\n cnop 0,8\n",
        "01 00 00 00 00 00 00 00 4e 75 00 00 00 00 00 00",
    );
}

#[test]
fn motorola_syntax_aligns_code_and_gnu_syntax_does_not() {
    mot(
        " dc.b 1\n rts\n dc.b 1,2,3\n move.w d0,d1\n ds.b 1\n nop\n",
        "01 00 4e 75 01 02 03 00 32 00 00 00 4e 71",
    );
    gas(
        " .byte 1\n rts\n .byte 2\n movew %d0,%d1\n",
        "01 4e 75 02 32 00",
    );
}

#[test]
fn gnu_comments_and_separators() {
    gas(
        "# a hash comment in the first column\n movew #1,%d0 | a trailing comment\n \
         movew #2,%d1 ; movew #3,%d2\n /* a block comment */ nop\n",
        "30 3c 00 01 32 3c 00 02 34 3c 00 03 4e 71",
    );
}

// ---- the rest of the family ---------------------------------------------------

#[test]
fn fpu_instructions() {
    // m68k-elf-as, which assumes a 68881 beside the 68020.
    gas("fadd.x %fp0,%fp1\n", "f2 00 00 a2");
    gas("faddx %a0@(8,%d1:l:4),%fp2\n", "f2 30 49 22 1c 08");
    gas("fmove.l %fpcr,%d0\n", "f2 00 b0 00");
    gas("fmovem.x %fp0-%fp3,-(%sp)\n", "f2 27 e0 0f");
    gas("fmovem.x (%sp)+,%fp0-%fp3\n", "f2 1f d0 f0");
    gas("fmovem.l %fpcr/%fpsr,(%a0)\n", "f2 10 b8 00");
    gas("fsincos.x %fp0,%fp1:%fp2\n", "f2 00 01 31");
    gas("fmove.p %fp0,(%a0){#5}\n", "f2 10 6c 05");
    gas("fmove.p %fp0,(%a0){%d1}\n", "f2 10 7c 10");
    gas("fmovecr #0x0e,%fp0\n", "f2 00 5c 0e");
    gas("ftrapeq.w #1\n", "f2 7a 00 01 00 01");
    mot(" fmovem.x fp0/fp2/fp4,(a0)\n", "f2 10 f0 a8");
    mot(" fsincos.x (a0),fp3:fp4\n", "f2 10 4a 33");
}

#[test]
fn float_immediates_in_each_format() {
    // Single and double: m68k-elf-as and vasm agree.
    gas("fadd.s #0r1.5,%fp1\n", "f2 3c 44 a2 3f c0 00 00");
    gas(
        "fadd.d #0r1.5,%fp1\n",
        "f2 3c 54 a2 3f f8 00 00 00 00 00 00",
    );
    gas(
        "fmove.d #0d-3.5,%fp0\n",
        "f2 3c 54 00 c0 0c 00 00 00 00 00 00",
    );
    mot(" fmove.s #-0.1,fp1\n", "f2 3c 44 80 bd cc cc cd");
    // An integer where a float is wanted is its own bits, as GNU as writes
    // it (vasm converts it to a float instead).
    gas("fadd.s #5,%fp1\n", "f2 3c 44 a2 00 00 00 05");
    // Extended and packed follow vasm (`-m68881`): the 68881 format with
    // its 16 zero bits, which GNU as leaves out, and packed decimal, which
    // GNU as refuses.
    mot(
        " fmove.x #1.5,fp0\n",
        "f2 3c 48 00 3f ff 00 00 c0 00 00 00 00 00 00 00",
    );
    mot(
        " fmove.x #0.1,fp1\n",
        "f2 3c 48 80 3f fb 00 00 cc cc cc cc cc cc d0 00",
    );
    mot(
        " fmove.p #12345.678,fp3\n",
        "f2 3c 4d 80 00 04 00 01 23 45 67 80 00 00 00 00",
    );
    err(
        "m68k",
        Motorola,
        " fmove.l #1.5,d0\n",
        "cannot take a floating-point immediate",
    );
    err(
        "m68k",
        Gas,
        "ftrapeq.l #0r1.5\n",
        "needs a floating-point size",
    );
}

#[test]
fn mmu_instructions() {
    // m68k-elf-as, whose default CPU has a 68851.
    gas("pmove %tc,(%a0)\n", "f0 10 42 00");
    gas("pmove %crp,(%a0)\n", "f0 10 4e 00");
    gas("pmove %bad3,%d0\n", "f0 00 72 0c");
    gas("pflush #1,#2,(%a0)\n", "f0 10 38 51");
    gas("ptestr #1,(%a0),#3,%a2\n", "f0 10 8f 51");
    gas("pvalid %val,(%a0)\n", "f0 10 28 00");
    gas("pbbs .\n", "f0 80 ff fe");
    gas("pdbbs %d0,.\n", "f0 48 00 00 ff fc");
    gas("ptrapbs.w #1\n", "f0 7a 00 00 00 01");
}

#[test]
fn integer_instructions_the_68020_added() {
    // m68k-elf-as.
    gas("cas.b %d0,%d1,(%a0)\n", "0a d0 00 40");
    gas("cas2.w %d0:%d1,%d2:%d3,(%a0):(%a1)\n", "0c fc 80 80 90 c1");
    gas("cas2.l %d0:%d1,%d2:%d3,(%d4):(%a5)\n", "0e fc 40 80 d0 c1");
    gas("callm #3,(%a0)\n", "06 d0 00 03");
    gas("rtm %a3\n", "06 cb");
    gas("pack %d0,%d1,#5\n", "83 40 00 05");
    gas("unpk -(%a0),-(%a1),#0x1234\n", "83 88 12 34");
    gas("trapeq\n", "57 fc");
    gas("traphi.l #0x12345678\n", "52 fb 12 34 56 78");
    gas("movep.w %d0,4(%a0)\n", "01 88 00 04");
    gas("moves.l %a2,-(%a3)\n", "0e a3 a8 00");
}

#[test]
fn cpu_models_have_their_own_instructions() {
    table("68040", Gas, GNU_68040);
    table("68060", Gas, GNU_68060);
    table("cpu32", Gas, GNU_CPU32);
    table("fidoa", Gas, GNU_FIDO);
    table("5475", Gas, GNU_5475);
    table("54455", Gas, GNU_54455);
}

#[test]
fn cpus_refuse_what_they_lack_and_say_what_it_needs() {
    for (arch, src, needle) in [
        ("68000", "fadd.x %fp0,%fp1\n", "needs a 68040 or later"),
        ("68000", "fadd.x %fp0,%fp1\n", "or a 68881/68882 FPU"),
        ("68000", "fadd.x %fp0,%fp1\n", "this target is a 68000"),
        ("68040", "pmove %tc,(%a0)\n", "a 68851 MMU"),
        ("68030", "move16 (%a0)+,(%a1)+\n", "needs a 68040 or later"),
        ("68030", "callm #0,(%a0)\n", "needs a 68020;"),
        ("m68k", "bgnd\n", "needs a CPU32 or a Fido"),
        ("68060", "mvsw %d0,%d1\n", "ColdFire"),
        (
            "5475",
            "bitrev %d0\n",
            "or a ColdFire ISA_C; this target is a 5475",
        ),
        ("cpu32", "movel ([4,%a0]),%d0\n", "not available on CPU32"),
        (
            "5475",
            "movew 4(%a0,%d1:w),%d2\n",
            "ColdFire index register is a long",
        ),
        ("68010", "movec %d0,%cacr\n", "on a 68010"),
        ("68040", "movec %d0,%caar\n", "on a 68040"),
    ] {
        err(arch, Gas, src, needle);
    }
    // An extension after a comma adds to the CPU, and names it.
    err(
        "m68k",
        Gas,
        ".arch 68000,68851\n faddx %fp0,%fp1\n",
        "a 68000 with 68851",
    );
    assert_eq!(
        hex(&text_dialect(
            "m68k",
            Gas,
            ".arch 68000,68881\n faddx %fp0,%fp1\n"
        )),
        "f2 00 00 a2"
    );
}

#[test]
fn coldfire_refuses_the_68000_modes_it_dropped() {
    // m68k-elf-as -mcpu=5475 refuses each of these; the 68000 encodings
    // rsasm would otherwise write are not ColdFire instructions.
    for (src, needle) in [
        ("addil #5,%a0@\n", "is not an instruction a 5475 has"),
        ("seq %a0@\n", "is not an instruction a 5475 has"),
        ("moveml %d0-%d3,%sp@-\n", "is not an instruction a 5475 has"),
        ("addb %d0,%d1\n", "this target is a 5475"),
    ] {
        err("5475", Gas, src, needle);
    }
    // And what it kept assembles as it does on a 68000.
    assert_eq!(
        hex(&text_dialect(
            "5475",
            Gas,
            " moveml %d0-%d3/%a2,%sp@\n addl %d0,%a1@(8)\n"
        )),
        "48 d7 04 0f d1 a9 00 08"
    );
    // m68k-elf-as -mcpu=54455: ISA_C has 32-bit conditional branches and
    // bsr, but reaches far with bra only by an absolute jump; `jra` to an
    // address register is `jmp`.
    let src = "top: nop\n jbne far\n jbsr far\n jra far\n jra %a0@\n .space 40000\nfar: rts\n";
    let asm = assemble_dialect("54455", Gas, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(
        hex(&asm.section_bytes(SectionId(0))[..0x16]),
        "4e 71 66 ff 00 00 9c 52 61 ff 00 00 9c 4c 4e f9 00 00 00 00 4e d0"
    );
}

#[test]
fn every_cpu_name_gnu_as_takes() {
    for name in [
        "68000",
        "68ec000",
        "68hc001",
        "68302",
        "68010",
        "68020",
        "68k",
        "68030",
        "68ec030",
        "68040",
        "68060",
        "cpu32",
        "68332",
        "fidoa",
        "fido",
        "isaa",
        "isaaplus",
        "isab",
        "isac",
        "cfv4",
        "cfv4e",
        "5200",
        "5206e",
        "5208",
        "5407",
        "54455",
        "5475",
        "547x",
        "m68030",
        "mc68060",
        "68000,68881",
        "68020,no-68851",
        "5208,float",
    ] {
        assert!(
            arch::lookup(name).is_some(),
            "`{name}` should be an m68k CPU"
        );
    }
    for name in ["68070", "5476", "68000,68882x", "cpu33"] {
        assert!(arch::lookup(name).is_none(), "`{name}` is not a CPU");
    }
}

#[test]
fn elf_flags_record_the_cpu() {
    // `readelf -h` on m68k-elf-as objects for each `-mcpu`/`-march`.
    for (name, flags) in [
        ("m68k", 0),
        ("68000", 0x0100_0000),
        ("68010", 0x0100_0000),
        ("68040", 0),
        ("cpu32", 0x0081_0000),
        ("fidoa", 0x0200_0000),
        ("5475", 0x8065),
        ("5206", 0x1),
        ("54455", 0x26),
        ("5208", 0x23),
        ("5407", 0x14),
        ("isaa", 0x2),
        ("isab", 0x5),
        ("isac", 0x6),
        ("cfv4", 0x15),
        ("cfv4e", 0x8065),
    ] {
        let a = arch::lookup(name).expect("an m68k CPU");
        assert_eq!(a.elf_flags(&a.initial_state()), flags, "{name}");
    }
}

#[test]
fn coprocessor_branches_and_far_dbcc_relax() {
    // m68k-elf-as: `fjeq` and unsized `pbbs` reach 16 or 32 bits, `fjne` to
    // an undefined symbol is 32, a DBcc out of reach is dbcc/bra.s/bra.l,
    // and `jra` to a number jumps there absolutely.
    let src = "top: fnop\n fjeq top\n pbbs far\n fjne ext\n dbra %d0,far\n jra 0x1000\n \
               .space 40000\nfar: rts\n";
    let asm = assemble_dialect("m68k", Gas, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(
        hex(&asm.section_bytes(SectionId(0))[..0x26]),
        "f2 80 00 00 f2 81 ff fa f0 c0 00 00 9c 5c f2 ce 00 00 00 00 51 c8 00 04 60 06 60 ff \
         00 00 9c 4a 4e f9 00 00 10 00"
    );
    // A 68000 has no 32-bit branch, and jumps absolutely instead.
    let src = "top: nop\n dbra %d0,far\n jra far\n .space 40000\nfar: rts\n";
    let asm = assemble_dialect("68000", Gas, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    assert_eq!(
        hex(&asm.section_bytes(SectionId(0))[..0x14]),
        "4e 71 51 c8 00 04 60 06 4e f9 00 00 00 00 4e f9 00 00 00 00"
    );
}

#[test]
fn fpu_and_mmu_corpus_matches_gnu_as() {
    table("m68k", Gas, GNU_FPU);
    table("m68k", Gas, GNU_FPU_MORE);
}

#[test]
fn fpu_and_mmu_corpus_matches_gnu_as_mri() {
    table("m68k", Motorola, MOTOROLA_FPU);
}

#[test]
fn fpu_corpus_matches_vasm() {
    table_as("m68k", Motorola, VASM_FPU, true);
}

// ---- the corpora, with reference output ---------------------------------------

#[test]
fn motorola_corpus_matches_gnu_as_mri() {
    table("m68k", Motorola, MOTOROLA);
}

#[test]
fn gnu_corpus_matches_gnu_as() {
    table("m68k", Gas, GNU);
}

#[test]
fn vasm_corpus_matches_vasm() {
    table_as("m68k", Motorola, VASM, true);
}

const MOTOROLA: &[(&str, &str)] = &[
    (r#"move.w #$7fff,$DFF096"#, "33 fc 7f ff 00 df f0 96"),
    (r#"MOVE.W #$7FFF,$DFF096"#, "33 fc 7f ff 00 df f0 96"),
    (r#"move.w #$8200,$dff096"#, "33 fc 82 00 00 df f0 96"),
    (r#"move.w d1,d0"#, "30 01"),
    (r#"move.w a1,d0"#, "30 09"),
    (r#"move.w (a1),d0"#, "30 11"),
    (r#"move.w (a1)+,d0"#, "30 19"),
    (r#"move.w -(a1),d0"#, "30 21"),
    (r#"move.w 4(a1),d0"#, "30 29 00 04"),
    (r#"move.w -4(a1),d0"#, "30 29 ff fc"),
    (r#"move.w (4,a1),d0"#, "30 29 00 04"),
    (r#"move.w 4(a1,d2.w),d0"#, "30 31 20 04"),
    (r#"move.w 4(a1,d2.l),d0"#, "30 31 28 04"),
    (r#"move.w -128(a1,a2.w),d0"#, "30 31 a0 80"),
    (r#"move.w 127(a1,a2.l),d0"#, "30 31 a8 7f"),
    (r#"move.w (a1,d2.w),d0"#, "30 31 20 00"),
    (r#"move.w $400,d0"#, "30 38 04 00"),
    (r#"move.w $400.w,d0"#, "30 38 04 00"),
    (r#"move.w $400.l,d0"#, "30 39 00 00 04 00"),
    (r#"move.w $DFF006,d0"#, "30 39 00 df f0 06"),
    (r#"move.w #1234,d0"#, "30 3c 04 d2"),
    (r#"move.w (pc,d1.w),d0"#, "30 3b 10 00"),
    (r#"move.l d0,d7"#, "2e 00"),
    (r#"move.l d0,(a7)"#, "2e 80"),
    (r#"move.l d0,(a7)+"#, "2e c0"),
    (r#"move.l d0,-(a7)"#, "2f 00"),
    (r#"move.l d0,-(sp)"#, "2f 00"),
    (r#"move.l (sp)+,d0"#, "20 1f"),
    (r#"move.l d0,32767(a6)"#, "2d 40 7f ff"),
    (r#"move.l d0,-32768(a6)"#, "2d 40 80 00"),
    (r#"move.l d0,10(a5,d3.w)"#, "2b 80 30 0a"),
    (r#"move.l d0,10(a5,d3.l)"#, "2b 80 38 0a"),
    (r#"move.l d0,$7ffe"#, "21 c0 7f fe"),
    (r#"move.l d0,$8000"#, "23 c0 00 00 80 00"),
    (r#"move.l d0,$ffff8000"#, "21 c0 80 00"),
    (r#"move.l d0,-2"#, "21 c0 ff fe"),
    (r#"move.l d0,$12345678"#, "23 c0 12 34 56 78"),
    (r#"move.b d0,d1"#, "12 00"),
    (r#"move.b #$ff,d0"#, "10 3c 00 ff"),
    (r#"move.b #-128,d0"#, "10 3c ff 80"),
    (r#"move.b #'A',d0"#, "10 3c 00 41"),
    (r#"move.w #'AB',d0"#, "30 3c 41 42"),
    (r#"move.l #'ABCD',d0"#, "20 3c 41 42 43 44"),
    (r#"move #1,d0"#, "30 3c 00 01"),
    (r#"movew #1,d0"#, "30 3c 00 01"),
    (r#"movel d0,d1"#, "22 00"),
    (r#"moveb d0,d1"#, "12 00"),
    (r#"move.w #-32768,d0"#, "30 3c 80 00"),
    (r#"move.w #$ffff,d0"#, "30 3c ff ff"),
    (r#"move.l #-2147483648,d1"#, "22 3c 80 00 00 00"),
    (r#"move.l #$12345678,(a0)"#, "20 bc 12 34 56 78"),
    (r#"move.l d0,a0"#, "20 40"),
    (r#"move.w d0,a0"#, "30 40"),
    (r#"move.l (a0)+,a1"#, "22 58"),
    (r#"movea.l d0,a0"#, "20 40"),
    (r#"movea.w (a0),a1"#, "32 50"),
    (r#"movea d0,a1"#, "32 40"),
    (r#"move.l a7,a0"#, "20 4f"),
    (r#"move.l sp,a6"#, "2c 4f"),
    (r#"move.l #$12345678,a0"#, "20 7c 12 34 56 78"),
    (r#"move.w #1,a0"#, "30 7c 00 01"),
    (r#"moveq #0,d0"#, "70 00"),
    (r#"moveq #1,d0"#, "70 01"),
    (r#"moveq #-1,d7"#, "7e ff"),
    (r#"moveq #127,d3"#, "76 7f"),
    (r#"moveq #-128,d4"#, "78 80"),
    (r#"moveq.l #42,d2"#, "74 2a"),
    (r#"move sr,d0"#, "40 c0"),
    (r#"move.w sr,(a0)"#, "40 d0"),
    (r#"move d0,sr"#, "46 c0"),
    (r#"move.w #$2700,sr"#, "46 fc 27 00"),
    (r#"move #0,ccr"#, "44 fc 00 00"),
    (r#"move (a0),ccr"#, "44 d0"),
    (r#"move.w d0,ccr"#, "44 c0"),
    (r#"move ccr,d0"#, "42 c0"),
    (r#"move usp,a0"#, "4e 68"),
    (r#"move a0,usp"#, "4e 60"),
    (r#"move.l a7,usp"#, "4e 67"),
    (r#"move.l usp,a7"#, "4e 6f"),
    (r#"movem.l d0-d3/a0-a2,-(sp)"#, "48 e7 f0 e0"),
    (r#"movem.l (sp)+,d0-d3/a0-a2"#, "4c df 07 0f"),
    (r#"movem.l d0,-(sp)"#, "48 e7 80 00"),
    (r#"movem.l (sp)+,d0"#, "4c df 00 01"),
    (r#"movem.w d0/d2/a5,(a0)"#, "48 90 20 05"),
    (r#"movem.l 8(a0),d0-a6"#, "4c e8 7f ff 00 08"),
    (r#"movem.l d0-d7/a0-a6,-(a7)"#, "48 e7 ff fe"),
    (r#"movem.l (a7)+,d0-d7/a0-a6"#, "4c df 7f ff"),
    (r#"movem.l #$0003,(a0)"#, "48 d0 00 03"),
    (r#"movem #$0003,(a0)"#, "48 90 00 03"),
    (r#"movem.l (a0),#$0003"#, "4c d0 00 03"),
    (r#"movem.l a0-a1/d0-d1,(a0)"#, "48 d0 03 03"),
    (r#"movem.l d3-d0,(a0)"#, "48 d0 00 0f"),
    (r#"movem.l a7,-(sp)"#, "48 e7 00 01"),
    (r#"movem.l d0-a0,-(sp)"#, "48 e7 ff 80"),
    (r#"movem.w (a0)+,d0/a1"#, "4c 98 02 01"),
    (r#"movem.l d2-d7/a2-a6,-(sp)"#, "48 e7 3f 3e"),
    (r#"movem.l (sp)+,d2-d7/a2-a6"#, "4c df 7c fc"),
    (r#"lea (a0),a1"#, "43 d0"),
    (r#"lea 4(a0),a1"#, "43 e8 00 04"),
    (r#"lea 0(a0),a1"#, "43 d0"),
    (r#"lea -4(a0,d0.w),a1"#, "43 f0 00 fc"),
    (r#"lea $400.w,a1"#, "43 f8 04 00"),
    (r#"lea $12345678,a1"#, "43 f9 12 34 56 78"),
    (r#"pea (a0)"#, "48 50"),
    (r#"pea 4(a0)"#, "48 68 00 04"),
    (r#"pea $400"#, "48 78 04 00"),
    (r#"exg d0,d1"#, "c1 41"),
    (r#"exg a0,a1"#, "c1 49"),
    (r#"exg d0,a1"#, "c1 89"),
    (r#"exg a1,d0"#, "c1 89"),
    (r#"exg sp,d0"#, "c1 8f"),
    (r#"swap d0"#, "48 40"),
    (r#"swap d7"#, "48 47"),
    (r#"clr.b d0"#, "42 00"),
    (r#"clr.w (a0)"#, "42 50"),
    (r#"clr.l -(a0)"#, "42 a0"),
    (r#"clr (a0)+"#, "42 58"),
    (r#"clr.l $400.w"#, "42 b8 04 00"),
    (r#"ext.w d0"#, "48 80"),
    (r#"ext.l d0"#, "48 c0"),
    (r#"ext d0"#, "48 80"),
    (r#"extb.l d0"#, "49 c0"),
    (r#"add.b d0,d1"#, "d2 00"),
    (r#"add.w (a0),d1"#, "d2 50"),
    (r#"add.l d0,(a0)"#, "d1 90"),
    (r#"add.w d0,-(a7)"#, "d1 67"),
    (r#"add.w a0,d0"#, "d0 48"),
    (r#"add.l d0,a0"#, "d1 c0"),
    (r#"add.l a0,d0"#, "d0 88"),
    (r#"add.w d0,a0"#, "d0 c0"),
    (r#"add.l #$10000,4(a0)"#, "06 a8 00 01 00 00 00 04"),
    (r#"adda.l d0,a0"#, "d1 c0"),
    (r#"adda.w d0,a0"#, "d0 c0"),
    (r#"adda.w #1,a0"#, "d0 fc 00 01"),
    (r#"adda.l #$10000,a0"#, "d1 fc 00 01 00 00"),
    (r#"adda d0,a0"#, "d0 c0"),
    (r#"addi.b #1,d0"#, "06 00 00 01"),
    (r#"addi.w #1,(a0)"#, "06 50 00 01"),
    (r#"addi.l #1,-(sp)"#, "06 a7 00 00 00 01"),
    (r#"addi #1,d0"#, "06 40 00 01"),
    (r#"addq.w #1,d0"#, "52 40"),
    (r#"addq.l #8,a0"#, "50 88"),
    (r#"addq #1,(a0)"#, "52 50"),
    (r#"addq.b #5,$400.w"#, "5a 38 04 00"),
    (r#"addx.b d0,d1"#, "d3 00"),
    (r#"addx.l -(a0),-(a1)"#, "d3 88"),
    (r#"addx d2,d3"#, "d7 42"),
    (r#"sub.b d0,d1"#, "92 00"),
    (r#"sub.l (a0)+,d1"#, "92 98"),
    (r#"sub.w d1,(a0)"#, "93 50"),
    (r#"sub.l a0,a1"#, "93 c8"),
    (r#"suba.l a1,a0"#, "91 c9"),
    (r#"suba.w (a0),a0"#, "90 d0"),
    (r#"subi.w #1,4(a0)"#, "04 68 00 01 00 04"),
    (r#"subi.l #$12345678,d0"#, "04 80 12 34 56 78"),
    (r#"sub.w #$100,(a1)"#, "04 51 01 00"),
    (r#"subq.b #8,d7"#, "51 07"),
    (r#"subq.l #1,a7"#, "53 8f"),
    (r#"subq #2,(a0)+"#, "55 58"),
    (r#"subx.w d0,d1"#, "93 40"),
    (r#"subx.w -(a0),-(a1)"#, "93 48"),
    (r#"cmp.b (a0),d0"#, "b0 10"),
    (r#"cmp.l d0,d1"#, "b2 80"),
    (r#"cmp.l a0,d1"#, "b2 88"),
    (r#"cmp.w d0,a0"#, "b0 c0"),
    (r#"cmp.l a0,a1"#, "b3 c8"),
    (r#"cmp.w #1,(a0)"#, "0c 50 00 01"),
    (r#"cmpa.l a0,a1"#, "b3 c8"),
    (r#"cmpa.w #1,a0"#, "b0 fc 00 01"),
    (r#"cmpi.b #1,d0"#, "0c 00 00 01"),
    (r#"cmpi.l #1,(a0)"#, "0c 90 00 00 00 01"),
    (r#"cmpi.w #$7fff,$400.w"#, "0c 78 7f ff 04 00"),
    (r#"cmpm.b (a0)+,(a1)+"#, "b3 08"),
    (r#"cmpm (a0)+,(a1)+"#, "b3 48"),
    (r#"cmpm.l (a2)+,(a3)+"#, "b7 8a"),
    (r#"and.w d0,d1"#, "c2 40"),
    (r#"and.w d0,(a0)"#, "c1 50"),
    (r#"and.w (a0),d0"#, "c0 50"),
    (r#"and.b #$0f,(a0)"#, "02 10 00 0f"),
    (r#"andi.b #1,d0"#, "02 00 00 01"),
    (r#"andi.w #1,(a0)"#, "02 50 00 01"),
    (r#"or.b d0,(a0)+"#, "81 18"),
    (r#"or.l (a0)+,d0"#, "80 98"),
    (r#"or.w #$8000,(a0)"#, "00 50 80 00"),
    (r#"ori.l #1,d0"#, "00 80 00 00 00 01"),
    (r#"eor.l d0,d1"#, "b1 81"),
    (r#"eor.w d0,(a0)"#, "b1 50"),
    (r#"eor.w #1,d0"#, "0a 40 00 01"),
    (r#"eor.b #$ff,(a0)"#, "0a 10 00 ff"),
    (r#"eori.b #1,(a0)"#, "0a 10 00 01"),
    (r#"not.b d0"#, "46 00"),
    (r#"not.w (a0)"#, "46 50"),
    (r#"not.l d7"#, "46 87"),
    (r#"neg.l d0"#, "44 80"),
    (r#"neg.b (a0)+"#, "44 18"),
    (r#"negx.w d0"#, "40 40"),
    (r#"negx.l -(a0)"#, "40 a0"),
    (r#"abcd d0,d1"#, "c3 00"),
    (r#"abcd -(a0),-(a1)"#, "c3 08"),
    (r#"sbcd d0,d1"#, "83 00"),
    (r#"sbcd -(a2),-(a3)"#, "87 0a"),
    (r#"nbcd d0"#, "48 00"),
    (r#"nbcd (a0)"#, "48 10"),
    (r#"mulu d0,d1"#, "c2 c0"),
    (r#"mulu.w (a0),d1"#, "c2 d0"),
    (r#"muls.w #3,d1"#, "c3 fc 00 03"),
    (r#"muls d2,d3"#, "c7 c2"),
    (r#"divu d0,d1"#, "82 c0"),
    (r#"divu.w #10,d0"#, "80 fc 00 0a"),
    (r#"divs.w #3,d1"#, "83 fc 00 03"),
    (r#"divs 4(a0),d2"#, "85 e8 00 04"),
    (r#"mulu.l d0,d1"#, "4c 00 10 00"),
    (r#"muls.l d0,d1"#, "4c 00 18 00"),
    (r#"mulu.l d0,d2:d1"#, "4c 00 14 02"),
    (r#"muls.l (a0),d3:d4"#, "4c 10 4c 03"),
    (r#"divu.l d0,d1"#, "4c 40 10 01"),
    (r#"divs.l d0,d1"#, "4c 40 18 01"),
    (r#"divs.l d0,d2:d1"#, "4c 40 1c 02"),
    (r#"divul.l d0,d2:d1"#, "4c 40 10 02"),
    (r#"divsl.l d0,d2:d1"#, "4c 40 18 02"),
    (r#"divsl d0,d2:d1"#, "4c 40 1c 02"),
    (r#"divul d0,d2:d1"#, "4c 40 14 02"),
    (r#"divsl.l d0,d1"#, "4c 40 18 01"),
    (r#"tst.b d0"#, "4a 00"),
    (r#"tst.w (a0)"#, "4a 50"),
    (r#"tst.l -(sp)"#, "4a a7"),
    (r#"tst.l a0"#, "4a 88"),
    (r#"tst $400.w"#, "4a 78 04 00"),
    (r#"tas d0"#, "4a c0"),
    (r#"tas (a0)"#, "4a d0"),
    (r#"chk d0,d1"#, "43 80"),
    (r#"chk.w (a0),d1"#, "43 90"),
    (r#"chk.l d0,d1"#, "43 00"),
    (r#"chk #100,d2"#, "45 bc 00 64"),
    (r#"asl.b #1,d0"#, "e3 00"),
    (r#"asl.w #8,d1"#, "e1 41"),
    (r#"asl.l #3,d7"#, "e7 87"),
    (r#"asl.w d1,d0"#, "e3 60"),
    (r#"asl.l d2,d3"#, "e5 a3"),
    (r#"asl.w (a0)"#, "e1 d0"),
    (r#"asl (a0)"#, "e1 d0"),
    (r#"asr.b #1,d0"#, "e2 00"),
    (r#"asr.w d1,d0"#, "e2 60"),
    (r#"asr.w (a0)"#, "e0 d0"),
    (r#"asr.l #8,d0"#, "e0 80"),
    (r#"lsl.l #1,d0"#, "e3 88"),
    (r#"lsl.w d0,d1"#, "e1 69"),
    (r#"lsl.w 4(a0)"#, "e3 e8 00 04"),
    (r#"lsr.b #2,d0"#, "e4 08"),
    (r#"lsr.l d0,d1"#, "e0 a9"),
    (r#"lsr.w -(a0)"#, "e2 e0"),
    (r#"rol.w #1,d0"#, "e3 58"),
    (r#"rol.l d0,d1"#, "e1 b9"),
    (r#"rol.w (a0)+"#, "e7 d8"),
    (r#"ror.b #8,d0"#, "e0 18"),
    (r#"ror.w d0,d1"#, "e0 79"),
    (r#"ror.w $400"#, "e6 f8 04 00"),
    (r#"roxl.w #1,d0"#, "e3 50"),
    (r#"roxl.b d0,d1"#, "e1 31"),
    (r#"roxl.w (a0)"#, "e5 d0"),
    (r#"roxr.l #5,d0"#, "ea 90"),
    (r#"roxr.w d0,d1"#, "e0 71"),
    (r#"roxr.w $1234.w"#, "e4 f8 12 34"),
    (r#"btst #3,d0"#, "08 00 00 03"),
    (r#"btst #31,d0"#, "08 00 00 1f"),
    (r#"btst #3,(a0)"#, "08 10 00 03"),
    (r#"btst d1,d0"#, "03 00"),
    (r#"btst d1,(a0)"#, "03 10"),
    (r#"btst d0,#5"#, "01 3c 00 05"),
    (r#"btst #7,$bfe001"#, "08 39 00 07 00 bf e0 01"),
    (r#"bset #7,d0"#, "08 c0 00 07"),
    (r#"bset d1,-(a0)"#, "03 e0"),
    (r#"bclr #0,(a0)+"#, "08 98 00 00"),
    (r#"bclr d2,d3"#, "05 83"),
    (r#"bchg #5,$400.w"#, "08 78 00 05 04 00"),
    (r#"bchg d0,d1"#, "01 41"),
    (r#"btst.b #3,(a0)"#, "08 10 00 03"),
    (r#"btst.l #3,d0"#, "08 00 00 03"),
    (r#"bset.b #7,(a0)"#, "08 d0 00 07"),
    (r#"bclr.l #1,d7"#, "08 87 00 01"),
    (r#"st d0"#, "50 c0"),
    (r#"sf d0"#, "51 c0"),
    (r#"shi d0"#, "52 c0"),
    (r#"sls d0"#, "53 c0"),
    (r#"scc d0"#, "54 c0"),
    (r#"scs (a0)"#, "55 d0"),
    (r#"sne d0"#, "56 c0"),
    (r#"seq d0"#, "57 c0"),
    (r#"svc d0"#, "58 c0"),
    (r#"svs d0"#, "59 c0"),
    (r#"spl d0"#, "5a c0"),
    (r#"smi d0"#, "5b c0"),
    (r#"sge d0"#, "5c c0"),
    (r#"slt d0"#, "5d c0"),
    (r#"sgt d0"#, "5e c0"),
    (r#"sle -(a0)"#, "5f e0"),
    (r#"seq.b d0"#, "57 c0"),
    (r#"jmp (a0)"#, "4e d0"),
    (r#"jmp 4(a0)"#, "4e e8 00 04"),
    (r#"jmp $400.w"#, "4e f8 04 00"),
    (r#"jmp 4(a0,d0.w)"#, "4e f0 00 04"),
    (r#"jmp $fc0000"#, "4e f9 00 fc 00 00"),
    (r#"jsr (a0)"#, "4e 90"),
    (r#"jsr -198(a6)"#, "4e ae ff 3a"),
    (r#"jsr -552(a6)"#, "4e ae fd d8"),
    (r#"jsr $12345678"#, "4e b9 12 34 56 78"),
    (r#"rts"#, "4e 75"),
    (r#"rte"#, "4e 73"),
    (r#"rtr"#, "4e 77"),
    (r#"rtd #4"#, "4e 74 00 04"),
    (r#"trap #0"#, "4e 40"),
    (r#"trap #15"#, "4e 4f"),
    (r#"trapv"#, "4e 76"),
    (r#"illegal"#, "4a fc"),
    (r#"nop"#, "4e 71"),
    (r#"reset"#, "4e 70"),
    (r#"stop #$2700"#, "4e 72 27 00"),
    (r#"bkpt #1"#, "48 49"),
    (r#"bkpt #7"#, "48 4f"),
    (r#"link a6,#-4"#, "4e 56 ff fc"),
    (r#"link a5,#0"#, "4e 55 00 00"),
    (r#"link.w a6,#-4"#, "4e 56 ff fc"),
    (r#"link.l a6,#-100000"#, "48 0e ff fe 79 60"),
    (r#"link a6,#-100000"#, "48 0e ff fe 79 60"),
    (r#"link a6,#32768"#, "48 0e 00 00 80 00"),
    (r#"unlk a6"#, "4e 5e"),
    (r#"unlk a5"#, "4e 5d"),
    (r#"andi #$f8ff,sr"#, "02 7c f8 ff"),
    (r#"andi.w #$f8ff,sr"#, "02 7c f8 ff"),
    (r#"andi #$fe,ccr"#, "02 3c 00 fe"),
    (r#"andi.b #$fe,ccr"#, "02 3c 00 fe"),
    (r#"ori #$0700,sr"#, "00 7c 07 00"),
    (r#"ori #1,ccr"#, "00 3c 00 01"),
    (r#"eori #1,ccr"#, "0a 3c 00 01"),
    (r#"eori #$2000,sr"#, "0a 7c 20 00"),
    (r#"and.w #$f8ff,sr"#, "02 7c f8 ff"),
    (r#"or.b #1,ccr"#, "00 3c 00 01"),
    (r#"movec d0,vbr"#, "4e 7b 08 01"),
    (r#"movec vbr,a0"#, "4e 7a 88 01"),
    (r#"movec.l sfc,d1"#, "4e 7a 10 00"),
    (r#"movec cacr,d0"#, "4e 7a 00 02"),
    (r#"movec a1,usp"#, "4e 7b 98 00"),
    (r#"move.w (a0,d1.w*2),d0"#, "30 30 12 00"),
    (r#"move.w (8,a0,d1.l*4),d0"#, "30 30 1c 08"),
    (r#"move.w (a0,d1.w*8),d0"#, "30 30 16 00"),
    (r#"move.w 1000(a0,d0.w),d1"#, "32 30 01 20 03 e8"),
    (r#"move.w 40000(a0),d1"#, "32 30 01 70 00 00 9c 40"),
    (r#"move.w (40000,a0,d1.w),d0"#, "30 30 11 30 00 00 9c 40"),
    (r#"move.w (8,d1.w),d0"#, "30 30 11 a0 00 08"),
    (r#"move.w (d1.w),d0"#, "30 30 11 90"),
    (r#"move.w (d1.l*4),d0"#, "30 30 1d 90"),
    (r#"move.l ([8,a0],d1.w,4),d0"#, "20 30 11 26 00 08 00 04"),
    (r#"move.l ([8,a0,d1.w],4),d0"#, "20 30 11 22 00 08 00 04"),
    (r#"move.l ([a0]),d0"#, "20 30 01 51"),
    (r#"move.l ([4,a0]),d0"#, "20 30 01 61 00 04"),
    (r#"bftst d0{1:8}"#, "e8 c0 00 48"),
    (r#"bfextu d0{1:8},d1"#, "e9 c0 10 48"),
    (r#"bfexts (a0){d1:d2},d3"#, "eb d0 38 62"),
    (r#"bfins d1,(a0){0:32}"#, "ef d0 10 00"),
    (r#"bfset 4(a0){3:5}"#, "ee e8 00 c5 00 04"),
    (r#"bfclr d7{31:1}"#, "ec c7 07 c1"),
    (r#"bfchg (a0){d0:8}"#, "ea d0 08 08"),
    (r#"bfffo d2{0:32},d3"#, "ed c2 30 00"),
    (r#"cmp2.l (a0),d0"#, "04 d0 00 00"),
    (r#"chk2.w (a0),d0"#, "02 d0 08 00"),
    (r#"cmp2.b 4(a0),a1"#, "00 e8 90 00 00 04"),
];
const GNU: &[(&str, &str)] = &[
    (r#"movew #0x7fff,0xdff096"#, "33 fc 7f ff 00 df f0 96"),
    (r#"movw #0x7fff,0xDFF096"#, "33 fc 7f ff 00 df f0 96"),
    (r#"move.w #0x7fff,0xdff096"#, "33 fc 7f ff 00 df f0 96"),
    (r#"movew %d0,%d1"#, "32 00"),
    (r#"move.w %d0,%d1"#, "32 00"),
    (r#"movw %d0,%d1"#, "32 00"),
    (r#"movl %d0,%d1"#, "22 00"),
    (r#"moveb %d0,%d1"#, "12 00"),
    (r#"move %d0,%d1"#, "32 00"),
    (r#"movb %d0,%d1"#, "12 00"),
    (r#"movew %sp@,%d0"#, "30 17"),
    (r#"movew %fp@,%d0"#, "30 16"),
    (r#"movew %a6@,%d0"#, "30 16"),
    (r#"movew %a7@,%d0"#, "30 17"),
    (r#"movel %d0,%a1"#, "22 40"),
    (r#"movew %d0,%a1"#, "32 40"),
    (r#"moveal %d0,%a1"#, "22 40"),
    (r#"movea %d0,%a1"#, "32 40"),
    (r#"movew #1,%d0 | a comment"#, "30 3c 00 01"),
    (r#"movew #1+2*3,%d0"#, "30 3c 00 07"),
    (r#"movew #-1,%d0"#, "30 3c ff ff"),
    (r#"movew #0xffff,%d0"#, "30 3c ff ff"),
    (r#"moveb #0xff,%d0"#, "10 3c 00 ff"),
    (r#"moveb #-128,%d0"#, "10 3c ff 80"),
    (r#"movew #'A',%d0"#, "30 3c 00 41"),
    (r#"movel #0x12345678,%d7"#, "2e 3c 12 34 56 78"),
    (r#"movew %d0,%a0@"#, "30 80"),
    (r#"movew %d0,%a0@+"#, "30 c0"),
    (r#"movew %d0,%a0@-"#, "31 00"),
    (r#"movew %d0,%a0@(8)"#, "31 40 00 08"),
    (r#"movew %d0,%a0@(-8)"#, "31 40 ff f8"),
    (r#"movew %d0,%a0@(8,%d1:w)"#, "31 80 10 08"),
    (r#"movew %d0,%a0@(8,%d1:l)"#, "31 80 18 08"),
    (r#"movew %d0,%a0@(8,%d1:w:4)"#, "31 80 14 08"),
    (r#"movew %d0,%a0@(8,%d1:l:4)"#, "31 80 1c 08"),
    (r#"movew %d0,%a0@(%d1:w)"#, "31 80 10 00"),
    (r#"movew %d0,%a1@(0,%a2:l)"#, "33 80 a8 00"),
    (r#"movew %a0@(8),%d0"#, "30 28 00 08"),
    (r#"movew %a0@+,%d0"#, "30 18"),
    (r#"movew %a0@-,%d0"#, "30 20"),
    (r#"movew %pc@(8),%d0"#, "30 3a 00 08"),
    (r#"movew %pc@(8,%d0:w),%d0"#, "30 3b 00 08"),
    (r#"movew %d0,(%a0)"#, "30 80"),
    (r#"movew %d0,(%a0)+"#, "30 c0"),
    (r#"movew %d0,-(%a0)"#, "31 00"),
    (r#"movew %d0,8(%a0,%d1.w)"#, "31 80 10 08"),
    (r#"movew %d0,8(%a0,%d1)"#, "31 80 18 08"),
    (r#"movew %d0,(8,%a0,%d1.l*4)"#, "31 80 1c 08"),
    (r#"movew %d0,(%d1.w,%a0)"#, "31 80 10 00"),
    (r#"movew %d0,(%a0,%d1.w)"#, "31 80 10 00"),
    (r#"movew %d0,(%a1,%a2.l)"#, "33 80 a8 00"),
    (r#"movew 8(%pc),%d0"#, "30 3a 00 08"),
    (r#"movew %d0,-(%sp)"#, "3f 00"),
    (r#"movel %sp@+,%d0"#, "20 1f"),
    (r#"movew %d0,0x400"#, "31 c0 04 00"),
    (r#"movew %d0,0x400:w"#, "31 c0 04 00"),
    (r#"movew %d0,(0x400).w"#, "31 c0 04 00"),
    (r#"movew %d0,(0x400).l"#, "33 c0 00 00 04 00"),
    (r#"movew %d0,0x400:l"#, "33 c0 00 00 04 00"),
    (r#"movew %d0,0x8000"#, "33 c0 00 00 80 00"),
    (r#"movew %d0,0xffff8000"#, "31 c0 80 00"),
    (r#"movew %d0,-2"#, "31 c0 ff fe"),
    (r#"movel 0x12345678,%d0"#, "20 39 12 34 56 78"),
    (r#"moveq #1,%d0"#, "70 01"),
    (r#"moveql #-128,%d0"#, "70 80"),
    (r#"moveq #127,%d7"#, "7e 7f"),
    (r#"moveml %d0-%d3/%a0-%a2,%sp@-"#, "48 e7 f0 e0"),
    (r#"moveml %d0-%d3/%a0-%a2,-(%sp)"#, "48 e7 f0 e0"),
    (r#"movem.l %d0-%d3/%a0-%a2,%sp@-"#, "48 e7 f0 e0"),
    (r#"moveml %sp@+,%d0-%d3/%a0-%a2"#, "4c df 07 0f"),
    (r#"moveml #0x3,%a0@"#, "48 d0 00 03"),
    (r#"movemw %d0/%d2,%a0@"#, "48 90 00 05"),
    (r#"moveml %a0@(8),%d0-%a6"#, "4c e8 7f ff 00 08"),
    (r#"movel %usp,%a0"#, "4e 68"),
    (r#"movel %a0,%usp"#, "4e 60"),
    (r#"movew %sr,%d0"#, "40 c0"),
    (r#"movew %d0,%ccr"#, "44 c0"),
    (r#"movew %d0,%sr"#, "46 c0"),
    (r#"movew #0x2700,%sr"#, "46 fc 27 00"),
    (r#"movew %ccr,%d0"#, "42 c0"),
    (r#"movec %d0,%vbr"#, "4e 7b 08 01"),
    (r#"movecl %vbr,%a0"#, "4e 7a 88 01"),
    (r#"leal %a0@,%a1"#, "43 d0"),
    (r#"lea %a0@(4),%a1"#, "43 e8 00 04"),
    (r#"lea %a0@(0),%a1"#, "43 d0"),
    (r#"pea %a0@"#, "48 50"),
    (r#"peal %a0@(-4)"#, "48 68 ff fc"),
    (r#"exg %d0,%d1"#, "c1 41"),
    (r#"exg %a0,%a1"#, "c1 49"),
    (r#"exg %a1,%d0"#, "c1 89"),
    (r#"swap %d0"#, "48 40"),
    (r#"swapw %d0"#, "48 40"),
    (r#"clrb %d0"#, "42 00"),
    (r#"clrw %d0"#, "42 40"),
    (r#"clrl %a0@"#, "42 90"),
    (r#"clr %d0"#, "42 40"),
    (r#"extw %d0"#, "48 80"),
    (r#"extl %d0"#, "48 c0"),
    (r#"extbl %d0"#, "49 c0"),
    (r#"addw %d0,%d1"#, "d2 40"),
    (r#"addl %d0,%a1"#, "d3 c0"),
    (r#"addal %d0,%a1"#, "d3 c0"),
    (r#"addaw %d0,%a1"#, "d2 c0"),
    (r#"addib #1,%d0"#, "06 00 00 01"),
    (r#"addiw #1,%a0@"#, "06 50 00 01"),
    (r#"addil #0x10000,%a0@(4)"#, "06 a8 00 01 00 00 00 04"),
    (r#"addqw #1,%d0"#, "52 40"),
    (r#"addqb #1,%d0"#, "52 00"),
    (r#"addq #8,%a0@+"#, "50 58"),
    (r#"addql #1,%a7"#, "52 8f"),
    (r#"addxb %d0,%d1"#, "d3 00"),
    (r#"addxl %a0@-,%a1@-"#, "d3 88"),
    (r#"addxl -(%a0),-(%a1)"#, "d3 88"),
    (r#"subw %d0,%d1"#, "92 40"),
    (r#"subl %a0@+,%d1"#, "92 98"),
    (r#"subw %d1,%a0@"#, "93 50"),
    (r#"subqw #1,%d0"#, "53 40"),
    (r#"subil #1,%d0"#, "04 80 00 00 00 01"),
    (r#"subxw %d0,%d1"#, "93 40"),
    (r#"subaw %a0@,%a0"#, "90 d0"),
    (r#"cmpw %d0,%d1"#, "b2 40"),
    (r#"cmpb %a0@,%d0"#, "b0 10"),
    (r#"cmpal %a0,%a1"#, "b3 c8"),
    (r#"cmpl %a0,%d1"#, "b2 88"),
    (r#"cmpmb %a0@+,%a1@+"#, "b3 08"),
    (r#"cmpib #1,%d0"#, "0c 00 00 01"),
    (r#"cmpiw #1,%a0@"#, "0c 50 00 01"),
    (r#"andw %d0,%d1"#, "c2 40"),
    (r#"andiw #1,%d0"#, "02 40 00 01"),
    (r#"andw %d0,%a0@"#, "c1 50"),
    (r#"orl %d0,%d1"#, "82 80"),
    (r#"oriw #1,%d0"#, "00 40 00 01"),
    (r#"orb #1,%a0@"#, "00 10 00 01"),
    (r#"eorw %d0,%d1"#, "b1 41"),
    (r#"eoriw #1,%d0"#, "0a 40 00 01"),
    (r#"eorl #1,%d0"#, "0a 80 00 00 00 01"),
    (r#"notw %d0"#, "46 40"),
    (r#"negl %d0"#, "44 80"),
    (r#"negxb %d0"#, "40 00"),
    (r#"abcd %d0,%d1"#, "c3 00"),
    (r#"sbcd %a0@-,%a1@-"#, "83 08"),
    (r#"nbcd %a0@"#, "48 10"),
    (r#"muls %d0,%d1"#, "c3 c0"),
    (r#"mulsw %d0,%d1"#, "c3 c0"),
    (r#"mulul %d0,%d1"#, "4c 00 10 00"),
    (r#"mulsl %d0,%d2:%d1"#, "4c 00 1c 02"),
    (r#"divu %d0,%d1"#, "82 c0"),
    (r#"divuw %d0,%d1"#, "82 c0"),
    (r#"divul %d0,%d1"#, "4c 40 10 01"),
    (r#"divsl %d0,%d1"#, "4c 40 18 01"),
    (r#"divsl %d0,%d2:%d1"#, "4c 40 1c 02"),
    (r#"divsll %d0,%d2:%d1"#, "4c 40 18 02"),
    (r#"tstw %d0"#, "4a 40"),
    (r#"tstl %a0"#, "4a 88"),
    (r#"tstb %a0@(1)"#, "4a 28 00 01"),
    (r#"tas %d0"#, "4a c0"),
    (r#"tasb %d0"#, "4a c0"),
    (r#"chkw %d0,%d1"#, "43 80"),
    (r#"chkl %d0,%d1"#, "43 00"),
    (r#"chk %d0,%d1"#, "43 80"),
    (r#"aslw #1,%d0"#, "e3 40"),
    (r#"lsll %d1,%d0"#, "e3 a8"),
    (r#"lsrw %a0@"#, "e2 d0"),
    (r#"rolb #1,%d0"#, "e3 18"),
    (r#"roxrw %d0,%d1"#, "e0 71"),
    (r#"lsl #1,%d0"#, "e3 48"),
    (r#"asrl #8,%d7"#, "e0 87"),
    (r#"rorw %a0@(2)"#, "e6 e8 00 02"),
    (r#"btst #3,%d0"#, "08 00 00 03"),
    (r#"btstb #3,%a0@"#, "08 10 00 03"),
    (r#"btstl #3,%d0"#, "08 00 00 03"),
    (r#"bset %d1,%d0"#, "03 c0"),
    (r#"bclr #1,%a0@"#, "08 90 00 01"),
    (r#"bchg %d0,%a0@"#, "01 50"),
    (r#"bset #31,%d7"#, "08 c7 00 1f"),
    (r#"seq %d0"#, "57 c0"),
    (r#"seqb %d0"#, "57 c0"),
    (r#"st %d0"#, "50 c0"),
    (r#"sf %d0"#, "51 c0"),
    (r#"scc %d0"#, "54 c0"),
    (r#"sne %a0@"#, "56 d0"),
    (r#"jmp %a0@"#, "4e d0"),
    (r#"jmp %a0@(4)"#, "4e e8 00 04"),
    (r#"jmp 0x400:w"#, "4e f8 04 00"),
    (r#"jsr %a6@(-198)"#, "4e ae ff 3a"),
    (r#"jsr -198(%a6)"#, "4e ae ff 3a"),
    (r#"jsr 0x12345678"#, "4e b9 12 34 56 78"),
    (r#"rts"#, "4e 75"),
    (r#"rte"#, "4e 73"),
    (r#"rtr"#, "4e 77"),
    (r#"rtd #4"#, "4e 74 00 04"),
    (r#"trap #15"#, "4e 4f"),
    (r#"trapv"#, "4e 76"),
    (r#"link %a6,#-4"#, "4e 56 ff fc"),
    (r#"linkw %a6,#-4"#, "4e 56 ff fc"),
    (r#"linkl %a6,#-4"#, "48 0e ff ff ff fc"),
    (r#"unlk %a6"#, "4e 5e"),
    (r#"nop"#, "4e 71"),
    (r#"reset"#, "4e 70"),
    (r#"stop #0x2700"#, "4e 72 27 00"),
    (r#"illegal"#, "4a fc"),
    (r#"bkpt #1"#, "48 49"),
    (r#"andiw #0xf8ff,%sr"#, "02 7c f8 ff"),
    (r#"andi #0xf8ff,%sr"#, "02 7c f8 ff"),
    (r#"andib #0xfe,%ccr"#, "02 3c 00 fe"),
    (r#"andi #0xfe,%ccr"#, "02 3c 00 fe"),
    (r#"oriw #0x700,%sr"#, "00 7c 07 00"),
    (r#"eorib #1,%ccr"#, "0a 3c 00 01"),
    (r#"cmp2l %a0@,%d0"#, "04 d0 00 00"),
    (r#"chk2w %a0@,%d0"#, "02 d0 08 00"),
    (r#"bfextu %d0{#1:#8},%d1"#, "e9 c0 10 48"),
    (r#"bftst %a0@{%d1:%d2}"#, "e8 d0 08 62"),
    (r#"bfins %d1,%a0@{#0:#32}"#, "ef d0 10 00"),
    (r#"bfset %a0@(4){#3:#5}"#, "ee e8 00 c5 00 04"),
    (r#"movew %d0,%a0@(8:w,%d1:w)"#, "31 80 11 20 00 08"),
    (
        r#"movew %d0,%a0@(0x10000,%d1:w)"#,
        "31 80 11 30 00 01 00 00",
    ),
    (r#"movew %d0,%a0@(8)@(4)"#, "31 80 01 62 00 08 00 04"),
    (r#"movew %d0,([8,%a0],%d1.w,4)"#, "31 80 11 26 00 08 00 04"),
    (r#"movew %d0,([8,%a0,%d1.w],4)"#, "31 80 11 22 00 08 00 04"),
    (r#"movew %d0,(8,%d1.w)"#, "31 80 11 a0 00 08"),
    (r#"movew (%d1),%d0"#, "30 30 19 90"),
    (r#"movew %d0,(%d1.w*2)"#, "31 80 13 90"),
    (r#"movew %a0@(0x12345),%d0"#, "30 30 01 70 00 01 23 45"),
];
const VASM: &[(&str, &str)] = &[
    (r#"move.w #$7fff,$DFF096"#, "33 fc 7f ff 00 df f0 96"),
    (r#"move.w #$8200,$dff096"#, "33 fc 82 00 00 df f0 96"),
    (r#"move.l #1,d0"#, "20 3c 00 00 00 01"),
    (r#"move.l #-1,d0"#, "20 3c ff ff ff ff"),
    (r#"move.l #0,d0"#, "20 3c 00 00 00 00"),
    (r#"move.l #1,d7"#, "2e 3c 00 00 00 01"),
    (r#"add.w #1,d0"#, "d0 7c 00 01"),
    (r#"add.l #1,d0"#, "d0 bc 00 00 00 01"),
    (r#"add.w #8,d0"#, "d0 7c 00 08"),
    (r#"add.w #9,d0"#, "d0 7c 00 09"),
    (r#"sub.l #1,d0"#, "90 bc 00 00 00 01"),
    (r#"add.l #1,a0"#, "d1 fc 00 00 00 01"),
    (r#"sub.w #2,a1"#, "92 fc 00 02"),
    (r#"cmp.w #1,d0"#, "b0 7c 00 01"),
    (r#"cmp.l #0,d0"#, "b0 bc 00 00 00 00"),
    (r#"cmp.b #$ff,d0"#, "b0 3c 00 ff"),
    (r#"and.w #1,d0"#, "c0 7c 00 01"),
    (r#"or.w #1,d0"#, "80 7c 00 01"),
    (r#"or.l #$80000000,d3"#, "86 bc 80 00 00 00"),
    (r#"move.w 8(a0,d1),d0"#, "30 30 10 08"),
    (r#"lea 4(a1,a2),a3"#, "47 f1 a0 04"),
    (r#"move.l (a0,d1),d0"#, "20 30 10 00"),
    (r#"move.w 8(pc,d0),d1"#, "32 3b 00 06"),
    (r#"move.w 8(pc),d0"#, "30 3a 00 06"),
    (r#"move.w (8,pc),d0"#, "30 3a 00 06"),
    (r#"jmp (4,pc)"#, "4e fa 00 02"),
    (r#"move.w (8,pc,d0.w),d0"#, "30 3b 00 06"),
    (r#"btst #1,4(pc)"#, "08 3a 00 01 00 00"),
    (r#"movem.l 4(pc),d0"#, "4c fa 00 01 00 00"),
    (r#"asl d0"#, "e3 40"),
    (r#"lsr.l d1"#, "e2 89"),
    (r#"shs d0"#, "54 c0"),
    (r#"slo d0"#, "55 c0"),
    (r#"move.w d1,d0"#, "30 01"),
    (r#"move.w a1,d0"#, "30 09"),
    (r#"move.w (a1),d0"#, "30 11"),
    (r#"move.w (a1)+,d0"#, "30 19"),
    (r#"move.w -(a1),d0"#, "30 21"),
    (r#"move.w 4(a1),d0"#, "30 29 00 04"),
    (r#"move.w -4(a1),d0"#, "30 29 ff fc"),
    (r#"move.w 4(a1,d2.w),d0"#, "30 31 20 04"),
    (r#"move.w 4(a1,d2.l),d0"#, "30 31 28 04"),
    (r#"move.w (a1,d2.w),d0"#, "30 31 20 00"),
    (r#"move.w $400.w,d0"#, "30 38 04 00"),
    (r#"move.w $400.l,d0"#, "30 39 00 00 04 00"),
    (r#"move.w #1234,d0"#, "30 3c 04 d2"),
    (r#"move.l d0,-(sp)"#, "2f 00"),
    (r#"move.l (sp)+,d0"#, "20 1f"),
    (r#"move.l d0,32767(a6)"#, "2d 40 7f ff"),
    (r#"move.l d0,10(a5,d3.w)"#, "2b 80 30 0a"),
    (r#"move.l d0,$12345678"#, "23 c0 12 34 56 78"),
    (r#"move.w ($400).w,d0"#, "30 38 04 00"),
    (r#"move.b d0,d1"#, "12 00"),
    (r#"move.b #$ff,d0"#, "10 3c 00 ff"),
    (r#"move.w #'AB',d0"#, "30 3c 41 42"),
    (r#"move.l #'ABCD',d0"#, "20 3c 41 42 43 44"),
    (r#"move.l d0,a0"#, "20 40"),
    (r#"move.w d0,a0"#, "30 40"),
    (r#"movea.l d0,a0"#, "20 40"),
    (r#"movea.w (a0),a1"#, "32 50"),
    (r#"moveq #1,d0"#, "70 01"),
    (r#"moveq #-128,d0"#, "70 80"),
    (r#"moveq #127,d7"#, "7e 7f"),
    (r#"move sr,d0"#, "40 c0"),
    (r#"move d0,sr"#, "46 c0"),
    (r#"move.w #$2700,sr"#, "46 fc 27 00"),
    (r#"move #0,ccr"#, "44 fc 00 00"),
    (r#"move usp,a0"#, "4e 68"),
    (r#"move a0,usp"#, "4e 60"),
    (r#"movem.l d0-d3/a0-a2,-(sp)"#, "48 e7 f0 e0"),
    (r#"movem.l (sp)+,d0-d3/a0-a2"#, "4c df 07 0f"),
    (r#"movem.l d0,-(sp)"#, "48 e7 80 00"),
    (r#"movem.w d0/d2/a5,(a0)"#, "48 90 20 05"),
    (r#"movem.l 8(a0),d0-a6"#, "4c e8 7f ff 00 08"),
    (r#"movem.l #$0003,(a0)"#, "48 d0 00 03"),
    (r#"movem.l a0-a1/d0-d1,(a0)"#, "48 d0 03 03"),
    (r#"movem.l d0-d7/a0-a7,-(sp)"#, "48 e7 ff ff"),
    (r#"lea (a0),a1"#, "43 d0"),
    (r#"lea 4(a0),a1"#, "43 e8 00 04"),
    (r#"lea $400.w,a1"#, "43 f8 04 00"),
    (r#"lea $12345678,a1"#, "43 f9 12 34 56 78"),
    (r#"pea (a0)"#, "48 50"),
    (r#"pea 4(a0)"#, "48 68 00 04"),
    (r#"exg d0,d1"#, "c1 41"),
    (r#"exg a0,a1"#, "c1 49"),
    (r#"exg a1,d0"#, "c1 89"),
    (r#"swap d0"#, "48 40"),
    (r#"clr.b d0"#, "42 00"),
    (r#"clr.w (a0)"#, "42 50"),
    (r#"clr.l -(a0)"#, "42 a0"),
    (r#"ext.w d0"#, "48 80"),
    (r#"ext.l d0"#, "48 c0"),
    (r#"add.b d0,d1"#, "d2 00"),
    (r#"add.w (a0),d1"#, "d2 50"),
    (r#"add.l d0,(a0)"#, "d1 90"),
    (r#"add.l d0,a0"#, "d1 c0"),
    (r#"add.w #1,(a0)"#, "06 50 00 01"),
    (r#"adda.w #1,a0"#, "d0 fc 00 01"),
    (r#"addi.b #1,d0"#, "06 00 00 01"),
    (r#"addi.l #1,-(sp)"#, "06 a7 00 00 00 01"),
    (r#"addq.w #1,d0"#, "52 40"),
    (r#"addq.l #8,a0"#, "50 88"),
    (r#"subq.b #8,d7"#, "51 07"),
    (r#"addx.b d0,d1"#, "d3 00"),
    (r#"addx.l -(a0),-(a1)"#, "d3 88"),
    (r#"subx.w -(a0),-(a1)"#, "93 48"),
    (r#"sub.l a0,a1"#, "93 c8"),
    (r#"subi.w #1,4(a0)"#, "04 68 00 01 00 04"),
    (r#"cmpa.l a0,a1"#, "b3 c8"),
    (r#"cmpi.b #1,d0"#, "0c 00 00 01"),
    (r#"cmpm.b (a0)+,(a1)+"#, "b3 08"),
    (r#"cmp.b (a0),d0"#, "b0 10"),
    (r#"andi.w #1,(a0)"#, "02 50 00 01"),
    (r#"ori.l #1,d0"#, "00 80 00 00 00 01"),
    (r#"eori.b #1,(a0)"#, "0a 10 00 01"),
    (r#"eor.l d0,d1"#, "b1 81"),
    (r#"eor.w #1,d0"#, "0a 40 00 01"),
    (r#"and.w d0,(a0)"#, "c1 50"),
    (r#"or.l (a0)+,d0"#, "80 98"),
    (r#"not.b d0"#, "46 00"),
    (r#"neg.l d0"#, "44 80"),
    (r#"negx.w d0"#, "40 40"),
    (r#"abcd -(a0),-(a1)"#, "c3 08"),
    (r#"sbcd d0,d1"#, "83 00"),
    (r#"nbcd (a0)"#, "48 10"),
    (r#"mulu d0,d1"#, "c2 c0"),
    (r#"muls.w #3,d1"#, "c3 fc 00 03"),
    (r#"divu d0,d1"#, "82 c0"),
    (r#"divs.w #3,d1"#, "83 fc 00 03"),
    (r#"tst.b d0"#, "4a 00"),
    (r#"tst.w (a0)"#, "4a 50"),
    (r#"tas (a0)"#, "4a d0"),
    (r#"chk d0,d1"#, "43 80"),
    (r#"asl.w #8,d1"#, "e1 41"),
    (r#"asr.w d1,d0"#, "e2 60"),
    (r#"lsl.w 4(a0)"#, "e3 e8 00 04"),
    (r#"lsr.b #2,d0"#, "e4 08"),
    (r#"rol.w (a0)+"#, "e7 d8"),
    (r#"ror.b #8,d0"#, "e0 18"),
    (r#"roxl.b d0,d1"#, "e1 31"),
    (r#"roxr.w $1234.w"#, "e4 f8 12 34"),
    (r#"btst #3,d0"#, "08 00 00 03"),
    (r#"btst d1,(a0)"#, "03 10"),
    (r#"btst d0,#5"#, "01 3c 00 05"),
    (r#"bset #7,d0"#, "08 c0 00 07"),
    (r#"bclr #0,(a0)+"#, "08 98 00 00"),
    (r#"bchg d0,d1"#, "01 41"),
    (r#"seq d0"#, "57 c0"),
    (r#"sne (a0)"#, "56 d0"),
    (r#"st d0"#, "50 c0"),
    (r#"jmp (a0)"#, "4e d0"),
    (r#"jmp 4(a0)"#, "4e e8 00 04"),
    (r#"jmp $400.w"#, "4e f8 04 00"),
    (r#"jsr -198(a6)"#, "4e ae ff 3a"),
    (r#"jsr $12345678"#, "4e b9 12 34 56 78"),
    (r#"rts"#, "4e 75"),
    (r#"rte"#, "4e 73"),
    (r#"rtr"#, "4e 77"),
    (r#"trap #15"#, "4e 4f"),
    (r#"trapv"#, "4e 76"),
    (r#"link a6,#-4"#, "4e 56 ff fc"),
    (r#"unlk a6"#, "4e 5e"),
    (r#"nop"#, "4e 71"),
    (r#"reset"#, "4e 70"),
    (r#"stop #$2700"#, "4e 72 27 00"),
    (r#"illegal"#, "4a fc"),
    (r#"andi #$f8ff,sr"#, "02 7c f8 ff"),
    (r#"andi #$fe,ccr"#, "02 3c 00 fe"),
    (r#"ori #$0700,sr"#, "00 7c 07 00"),
    (r#"eori #1,ccr"#, "0a 3c 00 01"),
];

const GNU_FPU: &[(&str, &str)] = &[
    (r#"fmove.x %fp0,%fp1"#, "f2 00 00 80"),
    (r#"fmove.x %fp7,%fp0"#, "f2 00 1c 00"),
    (r#"fadd.x %fp1,%fp2"#, "f2 00 05 22"),
    (r#"fsub.x %fp3,%fp4"#, "f2 00 0e 28"),
    (r#"fmul.x %fp5,%fp6"#, "f2 00 17 23"),
    (r#"fdiv.x %fp7,%fp0"#, "f2 00 1c 20"),
    (r#"fsqrt.x %fp0,%fp1"#, "f2 00 00 84"),
    (r#"fabs.x %fp2"#, "f2 00 09 18"),
    (r#"fneg.x %fp3,%fp3"#, "f2 00 0d 9a"),
    (r#"fmove.l %d0,%fp0"#, "f2 00 40 00"),
    (r#"fmove.w %d1,%fp2"#, "f2 01 51 00"),
    (r#"fmove.b %d2,%fp3"#, "f2 02 59 80"),
    (r#"fmove.l %fp0,%d0"#, "f2 00 60 00"),
    (r#"fmove.w %fp1,%d1"#, "f2 01 70 80"),
    (r#"fmove.b %fp2,%d2"#, "f2 02 79 00"),
    (r#"fmove.x (%a0),%fp0"#, "f2 10 48 00"),
    (r#"fmove.x %fp0,(%a0)"#, "f2 10 68 00"),
    (r#"fmove.x (%a0)+,%fp1"#, "f2 18 48 80"),
    (r#"fmove.x %fp1,-(%a7)"#, "f2 27 68 80"),
    (r#"fmove.d 0x10(%a0),%fp2"#, "f2 28 55 00 00 10"),
    (r#"fmove.s %fp3,0x20(%a1)"#, "f2 29 65 80 00 20"),
    (r#"fmove.x 4(%a0,%d0.w),%fp4"#, "f2 30 4a 00 00 04"),
    (r#"fmove.x 4(%a0,%d0.l*4),%fp5"#, "f2 30 4a 80 0c 04"),
    (r#"fmove.l #1,%fp0"#, "f2 3c 40 00 00 00 00 01"),
    (r#"fmove.w #0x1234,%fp0"#, "f2 3c 50 00 12 34"),
    (r#"fmove.b #0x12,%fp0"#, "f2 3c 58 00 00 12"),
    (r#"fmove.l %fpcr,%d0"#, "f2 00 b0 00"),
    (r#"fmove.l %d0,%fpcr"#, "f2 00 90 00"),
    (r#"fmove.l %fpsr,(%a0)"#, "f2 10 a8 00"),
    (r#"fmove.l %fpiar,%d1"#, "f2 01 a4 00"),
    (r#"fmovem.x %fp0-%fp3,-(%a7)"#, "f2 27 e0 0f"),
    (r#"fmovem.x (%a7)+,%fp0-%fp3"#, "f2 1f d0 f0"),
    (r#"fmovem.x %fp0/%fp2/%fp4,(%a0)"#, "f2 10 f0 a8"),
    (r#"fmovem.x (%a0),%fp1/%fp3"#, "f2 10 d0 50"),
    (r#"fmovem.l %fpcr/%fpsr,-(%a7)"#, "f2 27 b8 00"),
    (r#"fmovem.l (%a7)+,%fpcr/%fpsr"#, "f2 1f 98 00"),
    (r#"fmovem.l %fpcr/%fpsr/%fpiar,(%a0)"#, "f2 10 bc 00"),
    (r#"fmovecr #0x32,%fp0"#, "f2 00 5c 32"),
    (r#"fmovecr #0,%fp1"#, "f2 00 5c 80"),
    (r#"fsincos.x %fp0,%fp1:%fp2"#, "f2 00 01 31"),
    (r#"fsincos.x (%a0),%fp3:%fp4"#, "f2 10 4a 33"),
    (r#"ftst.x %fp0"#, "f2 00 00 3a"),
    (r#"ftst.l %d0"#, "f2 00 40 3a"),
    (r#"ftst.x (%a0)"#, "f2 10 48 3a"),
    (r#"fint.x %fp0,%fp1"#, "f2 00 00 81"),
    (r#"fintrz.x %fp2"#, "f2 00 09 03"),
    (r#"fetox.x %fp0,%fp1"#, "f2 00 00 90"),
    (r#"flogn.x %fp2,%fp3"#, "f2 00 09 94"),
    (r#"fsin.x %fp0"#, "f2 00 00 0e"),
    (r#"fcos.x %fp1"#, "f2 00 04 9d"),
    (r#"ftan.x %fp2"#, "f2 00 09 0f"),
    (r#"fatan.x %fp3"#, "f2 00 0d 8a"),
    (r#"fgetexp.x %fp0,%fp1"#, "f2 00 00 9e"),
    (r#"fgetman.x %fp2,%fp3"#, "f2 00 09 9f"),
    (r#"fscale.l %d0,%fp0"#, "f2 00 40 26"),
    (r#"fmod.x %fp1,%fp2"#, "f2 00 05 21"),
    (r#"frem.x %fp3,%fp4"#, "f2 00 0e 25"),
    (r#"fsgldiv.x %fp0,%fp1"#, "f2 00 00 a4"),
    (r#"fsglmul.x %fp2,%fp3"#, "f2 00 09 a7"),
    (r#"fnop"#, "f2 80 00 00"),
    (r#"fsave -(%a7)"#, "f3 27"),
    (r#"frestore (%a7)+"#, "f3 5f"),
    (r#"fseq %d0"#, "f2 40 00 01"),
    (r#"fsne (%a0)"#, "f2 50 00 0e"),
    (r#"fsgt -(%a7)"#, "f2 67 00 12"),
    (r#"fsf %d1"#, "f2 41 00 00"),
    (r#"fst %d2"#, "f2 42 00 0f"),
    (r#"ftrapeq"#, "f2 7c 00 01"),
    (r#"ftrapne.w #1"#, "f2 7a 00 0e 00 01"),
    (r#"ftrapgt.l #0x12345678"#, "f2 7b 00 12 12 34 56 78"),
    (r#"fmove.p %fp0,(%a0){#5}"#, "f2 10 6c 05"),
    (r#"fmove.p %fp1,(%a1){%d2}"#, "f2 11 7c a0"),
    (r#"pmove %tc,(%a0)"#, "f0 10 42 00"),
    (r#"pmove (%a0),%tc"#, "f0 10 40 00"),
    (r#"pmove %crp,(%a1)"#, "f0 11 4e 00"),
    (r#"pmove %srp,-(%a7)"#, "f0 27 4a 00"),
    (r#"pmove %psr,%d0"#, "f0 00 62 00"),
    (r#"pmove %d0,%psr"#, "f0 00 60 00"),
    (r#"pflush #1,#2"#, "f0 00 30 51"),
    (r#"pflush #1,#2,(%a0)"#, "f0 10 38 51"),
    (r#"ptestr #1,(%a0),#7"#, "f0 10 9e 11"),
    (r#"ptestw #1,(%a0),#7,%a1"#, "f0 10 9d 31"),
    (r#"pvalid %val,(%a0)"#, "f0 10 28 00"),
    (r#"psave -(%a7)"#, "f1 27"),
    (r#"prestore (%a7)+"#, "f1 5f"),
    (r#"cas.b %d0,%d1,(%a0)"#, "0a d0 00 40"),
    (r#"cas.w %d2,%d3,(%a1)"#, "0c d1 00 c2"),
    (r#"cas.l %d4,%d5,(%a2)"#, "0e d2 01 44"),
    (r#"cas2.w %d0:%d1,%d2:%d3,(%a0):(%a1)"#, "0c fc 80 80 90 c1"),
    (r#"cas2.l %d0:%d1,%d2:%d3,(%a0):(%a1)"#, "0e fc 80 80 90 c1"),
    (r#"callm #0,(%a0)"#, "06 d0 00 00"),
    (r#"rtm %d0"#, "06 c0"),
    (r#"pack %d0,%d1,#0"#, "83 40 00 00"),
    (r#"pack -(%a0),-(%a1),#0x30"#, "83 48 00 30"),
    (r#"unpk %d0,%d1,#0x3030"#, "83 80 30 30"),
    (r#"unpk -(%a0),-(%a1),#0"#, "83 88 00 00"),
    (r#"trapeq"#, "57 fc"),
    (r#"trapne.w #1"#, "56 fa 00 01"),
    (r#"traphi.l #0x12345678"#, "52 fb 12 34 56 78"),
    (r#"movep.w %d0,0x10(%a0)"#, "01 88 00 10"),
    (r#"movep.l 0x20(%a1),%d1"#, "03 49 00 20"),
    (r#"moves.b (%a0),%d0"#, "0e 10 00 00"),
    (r#"moves.w %d1,(%a1)"#, "0e 51 18 00"),
    (r#"moves.l (%a2)+,%a3"#, "0e 9a b0 00"),
    (r#"fmove.s #0r1.5,%fp0"#, "f2 3c 44 00 3f c0 00 00"),
    (
        r#"fmove.d #0r1.5,%fp0"#,
        "f2 3c 54 00 3f f8 00 00 00 00 00 00",
    ),
    (r#"fmove.s #-0r0.1,%fp1"#, "f2 3c 44 80 bd cc cc cd"),
    (
        r#"fmove.d #0r2.718281828,%fp1"#,
        "f2 3c 54 80 40 05 bf 0a 8b 04 91 9b",
    ),
    (r#"fadd.s #0r0.5,%fp1"#, "f2 3c 44 a2 3f 00 00 00"),
    (
        r#"fsub.d #0r100.25,%fp2"#,
        "f2 3c 55 28 40 59 10 00 00 00 00 00",
    ),
];

const GNU_FPU_MORE: &[(&str, &str)] = &[
    (r#"faddx %fp0,%fp1"#, "f2 00 00 a2"),
    (r#"fmovex %a0@,%fp0"#, "f2 10 48 00"),
    (r#"fmovex %fp0,%a1@-"#, "f2 21 68 00"),
    (r#"fmovemx %fp0-%fp3,%sp@-"#, "f2 27 e0 0f"),
    (r#"fmovemx %sp@+,%fp0-%fp3"#, "f2 1f d0 f0"),
    (r#"faddd %a0@(8,%d1:l:4),%fp2"#, "f2 30 55 22 1c 08"),
    (r#"fmovel %fpcr,%a0@+"#, "f2 18 b0 00"),
    (r#"fmoves #0e1.5,%fp0"#, "f2 3c 44 00 3f c0 00 00"),
    (r#"fmoves #0f2.25,%fp0"#, "f2 3c 44 00 40 10 00 00"),
    (
        r#"fmoved #0s1.0,%fp0"#,
        "f2 3c 54 00 3f f0 00 00 00 00 00 00",
    ),
    (
        r#"fmoved #0d-3.5,%fp0"#,
        "f2 3c 54 00 c0 0c 00 00 00 00 00 00",
    ),
    (
        r#"fmoved #-0r3.5,%fp0"#,
        "f2 3c 54 00 c0 0c 00 00 00 00 00 00",
    ),
    (r#"fmovecrx #0x0f,%fp7"#, "f2 00 5f 8f"),
    (r#"fsincosx %a0@(4),%fp6:%fp7"#, "f2 28 4b b6 00 04"),
    (r#"fmovep %fp0,%a0@(12){#-3}"#, "f2 28 6c 7d 00 0c"),
    (r#"fmovemx %d7,%a2@"#, "f2 12 f8 70"),
    (r#"fmovemx %a3@+,%d6"#, "f2 1b d8 60"),
    (r#"fmovem %fp1,%a0@"#, "f2 10 f0 40"),
    (r#"fmovem.x #0x80,%a0@"#, "f2 10 f0 80"),
    (r#"fmovem.x %d1,-(%sp)"#, "f2 27 e8 10"),
    (r#"fmovem.x (%sp)+,%d1"#, "f2 1f d8 10"),
    (r#"ptestr %sfc,%a0@,#3,%a4"#, "f0 10 8f 80"),
    (r#"pmove %bad2,%a0@"#, "f0 10 72 08"),
    (r#"pmove %a0@,%bac5"#, "f0 10 74 14"),
    (r#"pmove %cal,%d1"#, "f0 01 52 00"),
    (r#"pmove %scc,%d2"#, "f0 02 5a 00"),
    (r#"pmove %ac,%a0@"#, "f0 10 5e 00"),
    (r#"pmove %a0@,%val"#, "f0 10 54 00"),
    (r#"pflushr %a0@"#, "f0 10 a0 00"),
    (r#"pflushs #3,#7"#, "f0 00 34 f3"),
    (r#"ploadw %dfc,%a0@"#, "f0 10 20 01"),
    (r#"pdbbc %d7,."#, "f0 4f 00 01 ff fc"),
    (r#"cas2w %d0:%d1,%d2:%d3,%a0@:%a1@"#, "0c fc 80 80 90 c1"),
];

const MOTOROLA_FPU: &[(&str, &str)] = &[
    (r#"fmove.x fp0,fp1"#, "f2 00 00 80"),
    (r#"fmove.x fp7,fp0"#, "f2 00 1c 00"),
    (r#"fadd.x fp1,fp2"#, "f2 00 05 22"),
    (r#"fsub.x fp3,fp4"#, "f2 00 0e 28"),
    (r#"fmul.x fp5,fp6"#, "f2 00 17 23"),
    (r#"fdiv.x fp7,fp0"#, "f2 00 1c 20"),
    (r#"fsqrt.x fp0,fp1"#, "f2 00 00 84"),
    (r#"fabs.x fp2"#, "f2 00 09 18"),
    (r#"fneg.x fp3,fp3"#, "f2 00 0d 9a"),
    (r#"fmove.l d0,fp0"#, "f2 00 40 00"),
    (r#"fmove.w d1,fp2"#, "f2 01 51 00"),
    (r#"fmove.b d2,fp3"#, "f2 02 59 80"),
    (r#"fmove.l fp0,d0"#, "f2 00 60 00"),
    (r#"fmove.w fp1,d1"#, "f2 01 70 80"),
    (r#"fmove.b fp2,d2"#, "f2 02 79 00"),
    (r#"fmove.x (a0),fp0"#, "f2 10 48 00"),
    (r#"fmove.x fp0,(a0)"#, "f2 10 68 00"),
    (r#"fmove.x (a0)+,fp1"#, "f2 18 48 80"),
    (r#"fmove.x fp1,-(a7)"#, "f2 27 68 80"),
    (r#"fmove.d $10(a0),fp2"#, "f2 28 55 00 00 10"),
    (r#"fmove.s fp3,$20(a1)"#, "f2 29 65 80 00 20"),
    (r#"fmove.x 4(a0,d0.w),fp4"#, "f2 30 4a 00 00 04"),
    (r#"fmove.x 4(a0,d0.l*4),fp5"#, "f2 30 4a 80 0c 04"),
    (r#"fmove.l #1,fp0"#, "f2 3c 40 00 00 00 00 01"),
    (r#"fmove.w #$1234,fp0"#, "f2 3c 50 00 12 34"),
    (r#"fmove.b #$12,fp0"#, "f2 3c 58 00 00 12"),
    (r#"fmove.l fpcr,d0"#, "f2 00 b0 00"),
    (r#"fmove.l d0,fpcr"#, "f2 00 90 00"),
    (r#"fmove.l fpsr,(a0)"#, "f2 10 a8 00"),
    (r#"fmove.l fpiar,d1"#, "f2 01 a4 00"),
    (r#"fmovem.x fp0-fp3,-(a7)"#, "f2 27 e0 0f"),
    (r#"fmovem.x (a7)+,fp0-fp3"#, "f2 1f d0 f0"),
    (r#"fmovem.x fp0/fp2/fp4,(a0)"#, "f2 10 f0 a8"),
    (r#"fmovem.x (a0),fp1/fp3"#, "f2 10 d0 50"),
    (r#"fmovem.l fpcr/fpsr,-(a7)"#, "f2 27 b8 00"),
    (r#"fmovem.l (a7)+,fpcr/fpsr"#, "f2 1f 98 00"),
    (r#"fmovem.l fpcr/fpsr/fpiar,(a0)"#, "f2 10 bc 00"),
    (r#"fmovecr #$32,fp0"#, "f2 00 5c 32"),
    (r#"fmovecr #0,fp1"#, "f2 00 5c 80"),
    (r#"fsincos.x fp0,fp1:fp2"#, "f2 00 01 31"),
    (r#"fsincos.x (a0),fp3:fp4"#, "f2 10 4a 33"),
    (r#"ftst.x fp0"#, "f2 00 00 3a"),
    (r#"ftst.l d0"#, "f2 00 40 3a"),
    (r#"ftst.x (a0)"#, "f2 10 48 3a"),
    (r#"fint.x fp0,fp1"#, "f2 00 00 81"),
    (r#"fintrz.x fp2"#, "f2 00 09 03"),
    (r#"fetox.x fp0,fp1"#, "f2 00 00 90"),
    (r#"flogn.x fp2,fp3"#, "f2 00 09 94"),
    (r#"fsin.x fp0"#, "f2 00 00 0e"),
    (r#"fcos.x fp1"#, "f2 00 04 9d"),
    (r#"ftan.x fp2"#, "f2 00 09 0f"),
    (r#"fatan.x fp3"#, "f2 00 0d 8a"),
    (r#"fgetexp.x fp0,fp1"#, "f2 00 00 9e"),
    (r#"fgetman.x fp2,fp3"#, "f2 00 09 9f"),
    (r#"fscale.l d0,fp0"#, "f2 00 40 26"),
    (r#"fmod.x fp1,fp2"#, "f2 00 05 21"),
    (r#"frem.x fp3,fp4"#, "f2 00 0e 25"),
    (r#"fsgldiv.x fp0,fp1"#, "f2 00 00 a4"),
    (r#"fsglmul.x fp2,fp3"#, "f2 00 09 a7"),
    (r#"fnop"#, "f2 80 00 00"),
    (r#"fsave -(a7)"#, "f3 27"),
    (r#"frestore (a7)+"#, "f3 5f"),
    (r#"fseq d0"#, "f2 40 00 01"),
    (r#"fsne (a0)"#, "f2 50 00 0e"),
    (r#"fsgt -(a7)"#, "f2 67 00 12"),
    (r#"fsf d1"#, "f2 41 00 00"),
    (r#"fst d2"#, "f2 42 00 0f"),
    (r#"ftrapeq"#, "f2 7c 00 01"),
    (r#"ftrapne.w #1"#, "f2 7a 00 0e 00 01"),
    (r#"ftrapgt.l #$12345678"#, "f2 7b 00 12 12 34 56 78"),
    (r#"fmove.p fp0,(a0){#5}"#, "f2 10 6c 05"),
    (r#"fmove.p fp1,(a1){d2}"#, "f2 11 7c a0"),
    (r#"pmove tc,(a0)"#, "f0 10 42 00"),
    (r#"pmove (a0),tc"#, "f0 10 40 00"),
    (r#"pmove crp,(a1)"#, "f0 11 4e 00"),
    (r#"pmove srp,-(a7)"#, "f0 27 4a 00"),
    (r#"pmove psr,d0"#, "f0 00 62 00"),
    (r#"pmove d0,psr"#, "f0 00 60 00"),
    (r#"pflush #1,#2"#, "f0 00 30 51"),
    (r#"pflush #1,#2,(a0)"#, "f0 10 38 51"),
    (r#"ptestr #1,(a0),#7"#, "f0 10 9e 11"),
    (r#"ptestw #1,(a0),#7,a1"#, "f0 10 9d 31"),
    (r#"pvalid val,(a0)"#, "f0 10 28 00"),
    (r#"psave -(a7)"#, "f1 27"),
    (r#"prestore (a7)+"#, "f1 5f"),
    (r#"cas.b d0,d1,(a0)"#, "0a d0 00 40"),
    (r#"cas.w d2,d3,(a1)"#, "0c d1 00 c2"),
    (r#"cas.l d4,d5,(a2)"#, "0e d2 01 44"),
    (r#"cas2.w d0:d1,d2:d3,(a0):(a1)"#, "0c fc 80 80 90 c1"),
    (r#"cas2.l d0:d1,d2:d3,(a0):(a1)"#, "0e fc 80 80 90 c1"),
    (r#"callm #0,(a0)"#, "06 d0 00 00"),
    (r#"rtm d0"#, "06 c0"),
    (r#"pack d0,d1,#0"#, "83 40 00 00"),
    (r#"pack -(a0),-(a1),#$30"#, "83 48 00 30"),
    (r#"unpk d0,d1,#$3030"#, "83 80 30 30"),
    (r#"unpk -(a0),-(a1),#0"#, "83 88 00 00"),
    (r#"trapeq"#, "57 fc"),
    (r#"trapne.w #1"#, "56 fa 00 01"),
    (r#"traphi.l #$12345678"#, "52 fb 12 34 56 78"),
    (r#"movep.w d0,$10(a0)"#, "01 88 00 10"),
    (r#"movep.l $20(a1),d1"#, "03 49 00 20"),
    (r#"moves.b (a0),d0"#, "0e 10 00 00"),
    (r#"moves.w d1,(a1)"#, "0e 51 18 00"),
    (r#"moves.l (a2)+,a3"#, "0e 9a b0 00"),
    (r#"fmove.s #1.5,fp0"#, "f2 3c 44 00 3f c0 00 00"),
    (r#"fmove.d #1.5,fp0"#, "f2 3c 54 00 3f f8 00 00 00 00 00 00"),
    (r#"fmove.s #-0.1,fp1"#, "f2 3c 44 80 bd cc cc cd"),
    (
        r#"fmove.d #2.718281828,fp1"#,
        "f2 3c 54 80 40 05 bf 0a 8b 04 91 9b",
    ),
    (r#"fadd.s #0.5,fp1"#, "f2 3c 44 a2 3f 00 00 00"),
    (
        r#"fsub.d #100.25,fp2"#,
        "f2 3c 55 28 40 59 10 00 00 00 00 00",
    ),
];

const VASM_FPU: &[(&str, &str)] = &[
    (r#"fmove.x fp0,fp1"#, "f2 00 00 80"),
    (r#"fmove.x fp7,fp0"#, "f2 00 1c 00"),
    (r#"fadd.x fp1,fp2"#, "f2 00 05 22"),
    (r#"fsub.x fp3,fp4"#, "f2 00 0e 28"),
    (r#"fmul.x fp5,fp6"#, "f2 00 17 23"),
    (r#"fdiv.x fp7,fp0"#, "f2 00 1c 20"),
    (r#"fsqrt.x fp0,fp1"#, "f2 00 00 84"),
    (r#"fabs.x fp2"#, "f2 00 09 18"),
    (r#"fneg.x fp3,fp3"#, "f2 00 0d 9a"),
    (r#"fmove.s #1.5,fp0"#, "f2 3c 44 00 3f c0 00 00"),
    (r#"fmove.d #1.5,fp0"#, "f2 3c 54 00 3f f8 00 00 00 00 00 00"),
    (
        r#"fmove.x #1.5,fp0"#,
        "f2 3c 48 00 3f ff 00 00 c0 00 00 00 00 00 00 00",
    ),
    (
        r#"fmove.p #1.5,fp0"#,
        "f2 3c 4c 00 00 00 00 01 50 00 00 00 00 00 00 00",
    ),
    (
        r#"fmove.x #0.1,fp1"#,
        "f2 3c 48 80 3f fb 00 00 cc cc cc cc cc cc d0 00",
    ),
    (
        r#"fmove.x #-2.5,fp2"#,
        "f2 3c 49 00 c0 00 00 00 a0 00 00 00 00 00 00 00",
    ),
    (
        r#"fmove.x #3.14159,fp3"#,
        "f2 3c 49 80 40 00 00 00 c9 0f cf 80 dc 33 70 00",
    ),
    (
        r#"fmove.x #1.0e10,fp4"#,
        "f2 3c 4a 00 40 20 00 00 95 02 f9 00 00 00 00 00",
    ),
    (
        r#"fmove.x #-1.0e-5,fp5"#,
        "f2 3c 4a 80 bf ee 00 00 a7 c5 ac 47 1b 47 88 00",
    ),
    (
        r#"fmove.p #0.1,fp1"#,
        "f2 3c 4c 80 40 01 00 01 00 00 00 00 00 00 00 01",
    ),
    (
        r#"fmove.p #-1.0e-5,fp2"#,
        "f2 3c 4d 00 c0 05 00 01 00 00 00 00 00 00 00 01",
    ),
    (
        r#"fmove.p #12345.678,fp3"#,
        "f2 3c 4d 80 00 04 00 01 23 45 67 80 00 00 00 00",
    ),
    (
        r#"fmove.p #3.14159,fp4"#,
        "f2 3c 4e 00 00 00 00 03 14 15 89 99 99 99 99 99",
    ),
    (r#"fmove.s #-0.1,fp1"#, "f2 3c 44 80 bd cc cc cd"),
    (
        r#"fmove.d #2.718281828,fp1"#,
        "f2 3c 54 80 40 05 bf 0a 8b 04 91 9b",
    ),
    (r#"fadd.s #0.5,fp1"#, "f2 3c 44 a2 3f 00 00 00"),
    (
        r#"fsub.d #100.25,fp2"#,
        "f2 3c 55 28 40 59 10 00 00 00 00 00",
    ),
    (
        r#"fcmp.x #1.0,fp0"#,
        "f2 3c 48 38 3f ff 00 00 80 00 00 00 00 00 00 00",
    ),
    (r#"fmove.l d0,fp0"#, "f2 00 40 00"),
    (r#"fmove.w d1,fp2"#, "f2 01 51 00"),
    (r#"fmove.b d2,fp3"#, "f2 02 59 80"),
    (r#"fmove.l fp0,d0"#, "f2 00 60 00"),
    (r#"fmove.w fp1,d1"#, "f2 01 70 80"),
    (r#"fmove.b fp2,d2"#, "f2 02 79 00"),
    (r#"fmove.x (a0),fp0"#, "f2 10 48 00"),
    (r#"fmove.x fp0,(a0)"#, "f2 10 68 00"),
    (r#"fmove.x (a0)+,fp1"#, "f2 18 48 80"),
    (r#"fmove.x fp1,-(a7)"#, "f2 27 68 80"),
    (r#"fmove.d $10(a0),fp2"#, "f2 28 55 00 00 10"),
    (r#"fmove.s fp3,$20(a1)"#, "f2 29 65 80 00 20"),
    (r#"fmove.x 4(a0,d0.w),fp4"#, "f2 30 4a 00 00 04"),
    (r#"fmove.x 4(a0,d0.l*4),fp5"#, "f2 30 4a 80 0c 04"),
    (r#"fmove.l #1,fp0"#, "f2 3c 40 00 00 00 00 01"),
    (r#"fmove.w #$1234,fp0"#, "f2 3c 50 00 12 34"),
    (r#"fmove.b #$12,fp0"#, "f2 3c 58 00 00 12"),
    (r#"fmove.l fpcr,d0"#, "f2 00 b0 00"),
    (r#"fmove.l d0,fpcr"#, "f2 00 90 00"),
    (r#"fmove.l fpsr,(a0)"#, "f2 10 a8 00"),
    (r#"fmove.l fpiar,d1"#, "f2 01 a4 00"),
    (r#"fmovem.x fp0-fp3,-(a7)"#, "f2 27 e0 0f"),
    (r#"fmovem.x (a7)+,fp0-fp3"#, "f2 1f d0 f0"),
    (r#"fmovem.x fp0/fp2/fp4,(a0)"#, "f2 10 f0 a8"),
    (r#"fmovem.x (a0),fp1/fp3"#, "f2 10 d0 50"),
    (r#"fmovem.l fpcr/fpsr,-(a7)"#, "f2 27 b8 00"),
    (r#"fmovem.l (a7)+,fpcr/fpsr"#, "f2 1f 98 00"),
    (r#"fmovem.l fpcr/fpsr/fpiar,(a0)"#, "f2 10 bc 00"),
    (r#"fmovecr #$32,fp0"#, "f2 00 5c 32"),
    (r#"fmovecr #0,fp1"#, "f2 00 5c 80"),
    (r#"fsincos.x fp0,fp1:fp2"#, "f2 00 01 31"),
    (r#"fsincos.x (a0),fp3:fp4"#, "f2 10 4a 33"),
    (r#"ftst.x fp0"#, "f2 00 00 3a"),
    (r#"ftst.l d0"#, "f2 00 40 3a"),
    (r#"ftst.x (a0)"#, "f2 10 48 3a"),
    (r#"fint.x fp0,fp1"#, "f2 00 00 81"),
    (r#"fintrz.x fp2"#, "f2 00 09 03"),
    (r#"fetox.x fp0,fp1"#, "f2 00 00 90"),
    (r#"flogn.x fp2,fp3"#, "f2 00 09 94"),
    (r#"fsin.x fp0"#, "f2 00 00 0e"),
    (r#"fcos.x fp1"#, "f2 00 04 9d"),
    (r#"ftan.x fp2"#, "f2 00 09 0f"),
    (r#"fatan.x fp3"#, "f2 00 0d 8a"),
    (r#"fgetexp.x fp0,fp1"#, "f2 00 00 9e"),
    (r#"fgetman.x fp2,fp3"#, "f2 00 09 9f"),
    (r#"fscale.l d0,fp0"#, "f2 00 40 26"),
    (r#"fmod.x fp1,fp2"#, "f2 00 05 21"),
    (r#"frem.x fp3,fp4"#, "f2 00 0e 25"),
    (r#"fsgldiv.x fp0,fp1"#, "f2 00 00 a4"),
    (r#"fsglmul.x fp2,fp3"#, "f2 00 09 a7"),
    (r#"fnop"#, "f2 80 00 00"),
    (r#"fsave -(a7)"#, "f3 27"),
    (r#"frestore (a7)+"#, "f3 5f"),
    (r#"fseq d0"#, "f2 40 00 01"),
    (r#"fsne (a0)"#, "f2 50 00 0e"),
    (r#"fsgt -(a7)"#, "f2 67 00 12"),
    (r#"fsf d1"#, "f2 41 00 00"),
    (r#"fst d2"#, "f2 42 00 0f"),
    (r#"ftrapeq"#, "f2 7c 00 01"),
    (r#"ftrapne.w #1"#, "f2 7a 00 0e 00 01"),
    (r#"ftrapgt.l #$12345678"#, "f2 7b 00 12 12 34 56 78"),
    (r#"fmove.p fp0,(a0){#5}"#, "f2 10 6c 05"),
    (r#"fmove.p fp1,(a1){d2}"#, "f2 11 7c a0"),
    (r#"pmove tc,(a0)"#, "f0 10 42 00"),
    (r#"pmove (a0),tc"#, "f0 10 40 00"),
    (r#"pmove crp,(a1)"#, "f0 11 4e 00"),
    (r#"pmove psr,d0"#, "f0 00 62 00"),
    (r#"pmove d0,psr"#, "f0 00 60 00"),
    (r#"pflusha"#, "f0 00 24 00"),
    (r#"pflush #1,#2"#, "f0 00 30 51"),
    (r#"pflush #1,#2,(a0)"#, "f0 10 38 51"),
    (r#"ptestr #1,(a0),#7"#, "f0 10 9e 11"),
    (r#"ptestw #1,(a0),#7,a1"#, "f0 10 9d 31"),
    (r#"pvalid val,(a0)"#, "f0 10 28 00"),
    (r#"psave -(a7)"#, "f1 27"),
    (r#"prestore (a7)+"#, "f1 5f"),
    (r#"cas.b d0,d1,(a0)"#, "0a d0 00 40"),
    (r#"cas.w d2,d3,(a1)"#, "0c d1 00 c2"),
    (r#"cas.l d4,d5,(a2)"#, "0e d2 01 44"),
    (r#"cas2.w d0:d1,d2:d3,(a0):(a1)"#, "0c fc 80 80 90 c1"),
    (r#"cas2.l d0:d1,d2:d3,(a0):(a1)"#, "0e fc 80 80 90 c1"),
    (r#"callm #0,(a0)"#, "06 d0 00 00"),
    (r#"rtm d0"#, "06 c0"),
    (r#"pack d0,d1,#0"#, "83 40 00 00"),
    (r#"pack -(a0),-(a1),#$30"#, "83 48 00 30"),
    (r#"unpk d0,d1,#$3030"#, "83 80 30 30"),
    (r#"unpk -(a0),-(a1),#0"#, "83 88 00 00"),
    (r#"trapeq"#, "57 fc"),
    (r#"trapne.w #1"#, "56 fa 00 01"),
    (r#"traphi.l #$12345678"#, "52 fb 12 34 56 78"),
    (r#"movep.w d0,$10(a0)"#, "01 88 00 10"),
    (r#"movep.l $20(a1),d1"#, "03 49 00 20"),
    (r#"moves.b (a0),d0"#, "0e 10 00 00"),
    (r#"moves.w d1,(a1)"#, "0e 51 18 00"),
    (r#"moves.l (a2)+,a3"#, "0e 9a b0 00"),
];

const GNU_68040: &[(&str, &str)] = &[
    (r#"move16 (%a0)+,(%a1)+"#, "f6 20 90 00"),
    (r#"move16 0x12345678,(%a2)"#, "f6 1a 12 34 56 78"),
    (r#"move16 (%a3),0x1000"#, "f6 13 00 00 10 00"),
    (r#"move16 %a4@+,0x8000"#, "f6 04 00 00 80 00"),
    (r#"move16 0x20000,%a5@+"#, "f6 0d 00 02 00 00"),
    (r#"cinva %ic"#, "f4 98"),
    (r#"cinva %bc"#, "f4 d8"),
    (r#"cinvl %dc,(%a0)"#, "f4 48"),
    (r#"cinvp %nc,%a1@"#, "f4 11"),
    (r#"cpusha %dc"#, "f4 78"),
    (r#"cpushl %ic,(%a2)"#, "f4 aa"),
    (r#"cpushp %bc,(%a3)"#, "f4 f3"),
    (r#"pflush (%a0)"#, "f5 08"),
    (r#"pflushn (%a1)"#, "f5 01"),
    (r#"pflusha"#, "f5 18"),
    (r#"pflushan"#, "f5 10"),
    (r#"ptestr (%a2)"#, "f5 6a"),
    (r#"ptestw (%a3)"#, "f5 4b"),
    (r#"fsmovex %fp0,%fp1"#, "f2 00 00 c0"),
    (r#"fdmoved %d0,%fp2"#, "f2 00 55 44"),
    (r#"fsaddx (%a0),%fp3"#, "f2 10 49 e2"),
    (r#"fdaddl %d1,%fp4"#, "f2 01 42 66"),
    (r#"fssubs #0r1.5,%fp5"#, "f2 3c 46 e8 3f c0 00 00"),
    (r#"fdsubx %fp6,%fp7"#, "f2 00 1b ec"),
    (r#"fsmulw (%a1)+,%fp0"#, "f2 19 50 63"),
    (r#"fddivb -(%a2),%fp1"#, "f2 22 58 e4"),
    (r#"fsabsx %fp2"#, "f2 00 09 58"),
    (r#"fdnegx %fp3,%fp4"#, "f2 00 0e 5e"),
    (r#"fssqrtd 8(%a3),%fp5"#, "f2 2b 56 c1 00 08"),
    (r#"movec %d0,%tc"#, "4e 7b 00 03"),
    (r#"movec %itt0,%d1"#, "4e 7a 10 04"),
    (r#"movec %a0,%urp"#, "4e 7b 88 06"),
    (r#"movec %mmusr,%a1"#, "4e 7a 98 05"),
];

const GNU_68060: &[(&str, &str)] = &[
    (r#"plpar (%a0)"#, "f5 c8"),
    (r#"plpaw (%a7)"#, "f5 8f"),
    (r#"lpstop #0x2000"#, "f8 00 01 c0 20 00"),
    (r#"halt"#, "4a c8"),
    (r#"pulse"#, "4a cc"),
    (r#"movec %d0,%pcr"#, "4e 7b 08 08"),
    (r#"movec %buscr,%d1"#, "4e 7a 10 08"),
    (r#"fsaddx %fp0,%fp1"#, "f2 00 00 e2"),
    (r#"move16 (%a0)+,(%a1)+"#, "f6 20 90 00"),
];

const GNU_CPU32: &[(&str, &str)] = &[
    (r#"bgnd"#, "4a fa"),
    (r#"lpstop #0x2700"#, "f8 00 01 c0 27 00"),
    (r#"fmovex %fp0,%fp1"#, "f2 00 00 80"),
    (r#"movec %d0,%vbr"#, "4e 7b 08 01"),
    (r#"movew %d0,(8,%a0,%d1.l*4)"#, "31 80 1c 08"),
    (r#"movel %a0@(0x12345),%d0"#, "20 30 01 70 00 01 23 45"),
];

const GNU_FIDO: &[(&str, &str)] = &[
    (r#"sleep"#, "4e 78"),
    (r#"trapx #3"#, "4e 33"),
    (r#"bgnd"#, "4a fa"),
    (r#"movec %d0,%cac"#, "4e 7b 0f fe"),
    (r#"movec %mbo,%d1"#, "4e 7a 1f ff"),
];

const GNU_5475: &[(&str, &str)] = &[
    (r#"mvsb %d0,%d1"#, "73 00"),
    (r#"mvsw (%a0),%d2"#, "75 50"),
    (r#"mvzb 8(%a1),%d3"#, "77 a9 00 08"),
    (r#"mvzw #0x1234,%d4"#, "79 fc 12 34"),
    (r#"mov3ql #-1,%d5"#, "a1 45"),
    (r#"mov3ql #7,(%a0)"#, "af 50"),
    (r#"mov3ql #1,-(%sp)"#, "a3 67"),
    (r#"satsl %d6"#, "4c 86"),
    (r#"remsl %d0,%d1:%d2"#, "4c 40 28 01"),
    (r#"remul (%a0),%d3:%d4"#, "4c 50 40 03"),
    (r#"tpf"#, "51 fc"),
    (r#"tpfw #1"#, "51 fa 00 01"),
    (r#"tpfl #0x12345"#, "51 fb 00 01 23 45"),
    (r#"wdebug (%a0)"#, "fb d0 00 03"),
    (r#"wddatab (%a1)"#, "fb 11"),
    (r#"halt"#, "4a c8"),
    (r#"pulse"#, "4a cc"),
    (r#"intouch %a3"#, "f4 2b"),
    (r#"fmoved %fp0,%fp1"#, "f2 00 00 80"),
    (r#"fmoved (%a0),%fp2"#, "f2 10 55 00"),
    (r#"fmoved %fp3,8(%a1)"#, "f2 29 75 80 00 08"),
    (r#"fmovel %d1,%fp5"#, "f2 01 42 80"),
    (r#"fmoves (%a2),%fp6"#, "f2 12 47 00"),
    (r#"fmovemd %fp0-%fp3,(%a7)"#, "f2 17 f0 f0"),
    (r#"fmovemd (%a7),%fp0-%fp3"#, "f2 17 d0 f0"),
    (r#"fsqrtd %fp1,%fp2"#, "f2 00 05 04"),
    (r#"fbeq ."#, "f2 81 ff fe"),
];

const GNU_54455: &[(&str, &str)] = &[
    (r#"bitrev %d0"#, "00 c0"),
    (r#"byterev %d1"#, "02 c1"),
    (r#"ff1 %d2"#, "04 c2"),
    (r#"mvsb (%a0)+,%d3"#, "77 18"),
    (r#"mov3ql #3,%d4"#, "a7 44"),
];
