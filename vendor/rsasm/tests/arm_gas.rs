//! ARM and Thumb features whose reference is GNU as: literal pools, mapping
//! symbols, `adr`, interworking and `it` blocks.
//!
//! Every expected value here was taken from `arm-none-eabi-as -march=armv7-a`
//! 2.47, the reference `tools/xas-diff/run.sh arm thumb` compares whole
//! objects against; the cases are the corpora's, or cut down from them.

#![cfg(feature = "arm")]

mod common;
use common::*;

/// The mapping symbols of an assembled source, as `(section, offset, name)`.
fn mapping(arch: &str, src: &str) -> Vec<(String, u64, &'static str)> {
    let asm = assemble_for(arch, src);
    assert!(
        !asm.diags.has_errors(),
        "{}",
        asm.diags.render(&asm.sm, false)
    );
    asm.mapping_symbols
        .iter()
        .map(|m| {
            let name = asm.interner.get(asm.section(m.section).name).to_string();
            (name, m.offset, m.name)
        })
        .collect()
}

fn text(entries: &[(u64, &'static str)]) -> Vec<(String, u64, &'static str)> {
    entries
        .iter()
        .map(|&(o, n)| (".text".to_string(), o, n))
        .collect()
}

// ---- literal pools -----------------------------------------------------------

/// A pool collects the literals since the last `.ltorg`; equal numbers share
/// an entry, a number a `mov` or `mvn` can hold is moved instead, and a pool
/// still open at the end of the section is written there.
#[test]
fn arm_literal_pools() {
    let src = "f:      ldr     r0, =0x12345678
        mov     r0, r1
        .ltorg
        ldr     r0, =0x12345678
        ldreq   r1, =0x87654321
        ldrne   r2, =0x100
        ldr     r3, =f
        bx      lr
";
    assert_eq!(
        hex(&text_for("arm", src)),
        "00 00 1f e5 01 00 a0 e1 78 56 34 12 0c 00 9f e5 0c 10 9f 05 01 2c a0 13 \
         08 30 9f e5 1e ff 2f e1 78 56 34 12 21 43 65 87 00 00 00 00"
    );
    assert_eq!(
        mapping("arm", src),
        text(&[(0, "$a"), (8, "$d"), (0xc, "$a"), (0x20, "$d")])
    );
    // The entry for a label is relocated against its section.
    let asm = assemble_for("arm", src);
    assert_eq!(asm.relocs.len(), 1);
    assert_eq!((asm.relocs[0].offset, asm.relocs[0].kind), (0x28, 2));
}

/// Thumb moves a number with a 32-bit `mov.w`, `mvn.w` or `movw`, never a
/// flag-setting 16-bit `movs`, and aligns its pool with zeros, marking the
/// padding as data as well as the pool.
#[test]
fn thumb_literal_pools() {
    let src = "f:      ldr     r0, =0x12345678
        movs    r0, r0
        bx      lr
        .ltorg
        ldr     r1, =0xff
        ldr     r2, =0xffffff00
        ldr     r3, =0x1234
        ldr     r4, =0x12345678
        ldr.w   r5, =0x12345678
";
    assert_eq!(
        hex(&text_for("thumb", src)),
        "01 48 00 00 70 47 00 00 78 56 34 12 4f f0 ff 01 6f f0 ff 02 41 f2 34 23 \
         01 4c df f8 04 50 00 00 78 56 34 12"
    );
    assert_eq!(
        mapping("thumb", src),
        text(&[
            (0, "$t"),
            (6, "$d"),
            (8, "$d"),
            (0xc, "$t"),
            (0x1e, "$d"),
            (0x20, "$d")
        ])
    );
}

/// A 16-bit load reaches 1020 bytes forward; past that, or when the code
/// before the pool grows, layout takes the 32-bit form.
#[test]
fn thumb_literal_loads_grow_out_of_reach() {
    let near = text_for("thumb", "ldr r0, =0x12345678\n.space 1018\n.ltorg\n");
    assert_eq!(hex(&near[..2]), "fe 48");
    let nearest = text_for("thumb", "ldr r0, =0x12345678\n.space 1022\n.ltorg\n");
    assert_eq!(hex(&nearest[..2]), "ff 48");
    let far = text_for("thumb", "ldr r0, =0x12345678\n.space 1024\n.ltorg\n");
    assert_eq!(hex(&far[..4]), "df f8 00 04");
    let grown = text_for(
        "thumb",
        "ldr r0, =0x12345678\nldr r1, =0x12345679\n.space 1014\nb far\n.ltorg\n\
         .space 2048\nfar: bx lr\n",
    );
    assert_eq!(hex(&grown[..6]), "ff 48 df f8 00 14");
}

/// Sizes are picked walking the section, each against the growth so far, as
/// GNU as's ARM relaxation picks them: once the first `adr` has grown, the
/// label the second one needs is on a word boundary again, so it stays 16
/// bits. (llvm-mc, deciding from where things were, widens both.)
#[test]
fn thumb_relaxation_follows_the_growth_so_far() {
    assert_eq!(
        hex(&text_for(
            "thumb",
            "adr r3, l2\nadr r0, l0\nb l5\nldr r12, =l1\nl0: bx lr\nl1: nop\nl2: nop\n\
             l3: nop\nl4: bx lr\nl5: bx lr\n"
        )),
        "0f f2 0c 03 01 a0 06 e0 df f8 0c c0 70 47 00 bf 00 bf 00 bf 70 47 70 47 \
         0e 00 00 00"
    );
}

#[test]
fn a_pool_out_of_reach_is_refused() {
    let e = errors_for("arm", "ldr r0, =0x12345678\n.space 4096\nbx lr\n");
    assert!(e.contains("-4095 to 4095"), "{e}");
    assert!(e.contains("put an `.ltorg` nearer"), "{e}");
    let e = errors_for("thumb", "ldr.n r0, =0x12345678\n.space 1026\n.ltorg\n");
    assert!(e.contains("0 to 1020"), "{e}");
    assert!(errors_for("arm", "ldrb r0, =1").contains("only `ldr` can"));
    assert!(errors_for("arm", "ldr r0, =0x100000000").contains("32-bit word"));
}

// ---- adr and adrl ----------------------------------------------------------------

/// ARM `adr` is an `add` or `sub` from the PC; `adrl` adds a second one, or a
/// no-op where one reaches.
#[test]
fn arm_adr_and_adrl() {
    assert_eq!(
        hex(&text_for(
            "arm",
            "back: nop\nadr r0, back\nadr r1, fwd\nadreq r2, fwd\nadr lr, back + 4\nfwd: bx lr\n"
        )),
        "00 f0 20 e3 0c 00 4f e2 04 10 8f e2 00 20 8f 02 14 e0 4f e2 1e ff 2f e1"
    );
    let far = text_for(
        "arm",
        "back: adrl r0, near\nadrl r1, far\nadrl r2, back\nnear: nop\n.space 0x10000\n\
         far: adrl r3, back\nadrlne r4, near\nbx lr\n",
    );
    assert_eq!(
        hex(&far[..24]),
        "10 00 8f e2 00 00 a0 e1 0c 10 8f e2 01 18 81 e2 18 20 4f e2 00 00 a0 e1"
    );
    assert_eq!(
        hex(&far[0x1001c..0x1002c]),
        "24 30 4f e2 01 38 43 e2 14 40 4f 12 01 48 44 12"
    );
    let e = errors_for("arm", "adr r0, far\n.space 1014\nfar: bx lr\n");
    assert!(e.contains("try `adrl`"), "{e}");
}

/// Thumb `adr` is 16 bits for a low register and a word-aligned label up to
/// 1020 bytes ahead, and `addw`/`subw` from the PC otherwise.
#[test]
fn thumb_adr() {
    assert_eq!(
        hex(&text_for(
            "thumb",
            "back: nop\nadr r0, back\nadr r1, fwd\nadr r8, fwd\nadr.w r2, fwd\n\
             adr r3, odd\n.p2align 2, 0\nfwd: bx lr\nodd: bx lr\n"
        )),
        "00 bf af f2 04 00 03 a1 0f f2 08 08 0f f2 04 02 0f f2 02 03 70 47 70 47"
    );
}

// ---- mapping symbols -----------------------------------------------------------

/// Every change between code and data is marked, alignment padding with no-ops
/// is code, and an odd remainder of it is data.
#[test]
fn mapping_symbols_mark_code_and_data() {
    let src = "f:      mov     r0, r1
        .word   1, 2
        .byte   3
        .p2align 2
        mov     r1, r2
        .ascii  \"ab\"
        .balign 4, 0xaa
        bx      lr
";
    assert_eq!(
        hex(&text_for("arm", src)),
        "01 00 a0 e1 01 00 00 00 02 00 00 00 03 00 00 00 02 10 a0 e1 61 62 aa aa \
         1e ff 2f e1"
    );
    assert_eq!(
        mapping("arm", src),
        text(&[
            (0, "$a"),
            (4, "$d"),
            (0xd, "$d"),
            (0x10, "$a"),
            (0x14, "$d"),
            (0x18, "$a")
        ])
    );
}

/// Data before any code is marked from the start of the section once code
/// follows, but not in a section that never has code.
#[test]
fn data_before_code_is_marked_from_the_start() {
    assert_eq!(
        mapping("arm", ".byte 1\n.p2align 1\nmov r0, r0\n.data\n.word 1\n"),
        text(&[(0, "$d"), (1, "$d"), (2, "$a")])
    );
}

// ---- interworking ----------------------------------------------------------------

/// The ELF symbol table of an object, as `(name, value, st_info)`, without
/// the null, section and mapping symbols.
fn symbols(asm: &rsasm::assembler::Assembler) -> Vec<(String, u32, u8)> {
    let b = rsasm::output::elf::build(asm).expect("ELF output");
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as usize;
    let u32at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as usize;
    let (shoff, shnum) = (u32at(0x20), u16at(0x30));
    let header = |i: usize| shoff + i * 40;
    let symtab = (0..shnum).find(|&i| u32at(header(i) + 4) == 2).unwrap();
    let strtab = u32at(header(u32at(header(symtab) + 24)) + 16);
    let (off, size) = (u32at(header(symtab) + 16), u32at(header(symtab) + 20));
    (16..size)
        .step_by(16)
        .filter_map(|e| {
            let name_at = strtab + u32at(off + e);
            let end = b[name_at..].iter().position(|&c| c == 0).unwrap() + name_at;
            let name = String::from_utf8_lossy(&b[name_at..end]).into_owned();
            let info = b[off + e + 12];
            (!name.is_empty() && !name.starts_with('$'))
                .then(|| (name, u32at(off + e + 4) as u32, info))
        })
        .collect()
}

const CALLS: &str = "        .arm
        .global armf
        .type   armf, %function
armf:   bl      thumbf
        blx     thumbf
        bl      armf2
        blx     armf2
        bl      ext
        blx     ext
        b       thumbf
        bx      lr
armf2:  bx      lr
        .thumb
        .thumb_func
thumbf: bl      armf
        blx     armf
        bl      thumbf2
        blx     thumbf2
        bl      ext
        blx     ext
        b.w     armf
        bx      lr
        .type   thumbf2, %function
thumbf2:
        bx      lr
plain:  bx      lr
        .data
        .word   thumbf, armf, thumbf2, plain
";

/// A call into the other instruction set becomes `blx`, and a `blx` that
/// stays in its set a call, where GNU as resolves the branch; a jump into
/// the other set, and anything global, is left to the linker.
#[test]
fn calls_between_arm_and_thumb() {
    assert_eq!(
        hex(&text_for("arm", CALLS)),
        "07 00 00 fa 06 00 00 fa 04 00 00 eb 03 00 00 fa fe ff ff eb fe ff ff fa \
         fe ff ff ea 1e ff 2f e1 1e ff 2f e1 ff f7 fe ff ff f7 fe ef 00 f0 09 f8 \
         00 f0 07 f8 ff f7 fe ff ff f7 fe ef ff f7 fe bf 70 47 70 47 70 47 00 bf"
    );
    let asm = assemble_for("arm", CALLS);
    let relocs: Vec<(u64, u32, String)> = asm
        .relocs
        .iter()
        .map(|r| {
            let name = r.symbol.map(|s| asm.display_name(s)).unwrap_or_default();
            (r.offset, r.kind, name)
        })
        .collect();
    let want: Vec<(u64, u32, String)> = [
        (0x10, 28, "ext"),
        (0x14, 28, "ext"),
        // A jump into Thumb needs a veneer, so it names the function.
        (0x18, 29, "thumbf"),
        (0x24, 10, "armf"),
        (0x28, 10, "armf"),
        (0x34, 10, "ext"),
        (0x38, 10, "ext"),
        (0x3c, 30, "armf"),
        // Data naming a function names the function, not its section.
        (0, 2, "thumbf"),
        (4, 2, "armf"),
        (8, 2, "thumbf2"),
        (0xc, 2, ".text"),
    ]
    .iter()
    .map(|&(o, k, n)| (o, k, n.to_string()))
    .collect();
    assert_eq!(relocs, want);
    // A Thumb function's address has its low bit set and its type is
    // `STT_FUNC`, whether `.thumb_func` or `.type` made it one.
    let syms = symbols(&asm);
    assert!(syms.contains(&("thumbf".into(), 0x25, 0x02)), "{syms:?}");
    assert!(syms.contains(&("thumbf2".into(), 0x43, 0x02)), "{syms:?}");
    assert!(syms.contains(&("armf".into(), 0, 0x12)), "{syms:?}");
    assert!(syms.contains(&("plain".into(), 0x44, 0x00)), "{syms:?}");
}

/// Only an unconditional `bl` is a call; a conditional one is a jump, which
/// into Thumb is the linker's to make reach.
#[test]
fn conditional_branches_into_thumb_are_relocated() {
    let asm = assemble_for(
        "arm",
        "bleq tf\nbeq tf\nb tf\nbl tf\nbx lr\n.thumb\n.type tf, %function\ntf: bx lr\n",
    );
    assert_eq!(
        hex(&asm.section_bytes(rsasm::section::SectionId(0))[..16]),
        "fe ff ff 0b fe ff ff 0a fe ff ff ea 00 00 00 fa"
    );
    let kinds: Vec<u32> = asm.relocs.iter().map(|r| r.kind).collect();
    assert_eq!(kinds, [29, 29, 29]);
}

/// A 16-bit branch has no relocation, so one to an ARM function or to a
/// global symbol is 32 bits.
#[test]
fn thumb_branches_that_the_linker_must_resolve_are_32_bits() {
    let asm = assemble_for(
        "thumb",
        "b armf\nbeq armf\nb near\nnear: bx lr\n.arm\n.type armf, %function\narmf: bx lr\n",
    );
    assert_eq!(
        hex(&asm.section_bytes(rsasm::section::SectionId(0))[..12]),
        "ff f7 fe bf 3f f4 fe af ff e7 70 47"
    );
    let kinds: Vec<u32> = asm.relocs.iter().map(|r| r.kind).collect();
    assert_eq!(kinds, [30, 51]);
}

/// A flat binary makes the choice of `bl` or `blx` a linker would (see the
/// `arm-gas` cases in `tests/flat.rs`), but a jump into the other instruction
/// set takes a veneer only a linker builds.
#[test]
fn a_flat_binary_cannot_jump_into_thumb() {
    let src = "b tfunc\n.section .text.thumb, \"ax\"\n.thumb\n.thumb_func\ntfunc: bx lr\n";
    let asm = assemble_flat_for("arm", src, 0x8000);
    let e = asm.diags.render(&asm.sm, false);
    assert!(e.contains("ARM-to-Thumb veneer"), "{e}");
}

/// Thumb `adr` of a Thumb function sets the low bit, which takes the 32-bit
/// form, whether the function is defined before the `adr` or after it.
#[test]
fn thumb_adr_of_a_thumb_function() {
    assert_eq!(
        hex(&text_for(
            "thumb",
            ".thumb_func\nf: bx lr\nadr r0, f\nadr r1, g\n.p2align 2, 0\n.thumb_func\ng: bx lr\n"
        )),
        "70 47 af f2 03 00 0f f2 05 01 00 00 70 47 00 bf"
    );
}

// ---- it blocks -----------------------------------------------------------------

/// Instructions in an `it` block take its condition, and the 16-bit
/// data-processing forms that set the flags outside a block leave them
/// alone inside one, so `addeq r0, r1, r2` is 16 bits and `addseq` 32.
#[test]
fn it_blocks() {
    let src = "f:      it      eq
        addeq   r0, r1, r2
        itt     ne
        addne   r0, r1, r2
        addsne  r0, r1, r2
        ite     cs
        movcs   r0, #1
        movcc   r0, #2
        itete   gt
        addgt   r0, #1
        suble   r0, #1
        mulgt   r0, r1, r0
        lslle   r0, r1, #2
        adds    r0, r1, r2
        add     r0, r1, r2
        it      mi
        bmi     f
        it      lt
        bxlt    lr
        itt     eq
        moveq   r0, r1
        bleq    f
        bx      lr
";
    assert_eq!(
        hex(&text_for("thumb", src)),
        "08 bf 88 18 1c bf 88 18 11 eb 02 00 2c bf 01 20 02 20 cb bf 01 30 01 38 \
         48 43 88 00 88 18 01 eb 02 00 48 bf ec e7 b8 bf 70 47 04 bf 08 46 ff f7 \
         e7 ff 70 47"
    );
    // In ARM code `it` is accepted and emits nothing.
    assert_eq!(
        hex(&text_for("arm", "it eq\naddeq r0, r1, r2\n")),
        "02 00 81 00"
    );
}

/// GNU as's rules for what an `it` block may hold, each with its reason.
#[test]
fn it_block_mistakes_are_refused() {
    let e = errors_for("thumb", "it eq\nadd r0, r1, r2\n");
    assert!(e.contains("not allowed in an `it` block"), "{e}");
    let e = errors_for("thumb", "it eq\naddne r0, r1, r2\n");
    assert!(e.contains("expects `eq` here"), "{e}");
    let e = errors_for("thumb", "ite eq\naddeq r0, r1\naddeq r0, r1\n");
    assert!(e.contains("expects `ne` here"), "{e}");
    let e = errors_for("thumb", "addeq r0, r1, r2\n");
    assert!(e.contains("takes an `it` block"), "{e}");
    let e = errors_for("thumb", "itt eq\nbeq x\naddeq r0, r1\nx: bx lr\n");
    assert!(e.contains("must be the last instruction"), "{e}");
    let e = errors_for("thumb", "itt eq\nmoveq pc, lr\naddeq r0, r1\n");
    assert!(e.contains("must be the last instruction"), "{e}");
    let e = errors_for("thumb", "itt eq\nit eq\naddeq r0, r1\n");
    assert!(e.contains("previous `it` block"), "{e}");
    let e = errors_for("thumb", "it al\nadd r0, r1, r2\n");
    assert!(e.contains("not allowed in an `it` block"), "{e}");
}
