//! Flat binaries against a linked reference.
//!
//! A flat image has to hold what a linker would have written for every
//! relocation: the page arithmetic of an AArch64 `adrp`, a PowerPC `@ha`, the
//! `auipc` that a RISC-V `%pcrel_lo` names, the distance between labels in
//! different sections. Each case in the tables below was assembled by the
//! reference and linked at `base` with the sections laid end to end, and the
//! image is GNU ld's: the tables are `FLAT_DIFF_SHOW=1 tools/flat-diff/run.sh`
//! output, pasted in and formatted. An image is its length plus the runs of
//! 16-byte lines that are not all zero. A case with no image is one the linker
//! refuses, and rsasm has to refuse it too.

mod common;
#[allow(unused_imports)]
use common::*;

#[allow(dead_code)]
struct Case {
    name: &'static str,
    base: u64,
    src: &'static str,
    image: Option<(usize, &'static [(usize, &'static str)])>,
}

#[allow(dead_code)]
fn check(arch: &str, cases: &[Case]) {
    for case in cases {
        let asm = assemble_flat_for(arch, case.src, case.base);
        let errors = asm.diags.render(&asm.sm, false);
        let Some((len, runs)) = case.image else {
            assert!(
                asm.diags.has_errors(),
                "[{arch}] {}: the reference refuses this, rsasm did not",
                case.name
            );
            continue;
        };
        assert!(!asm.diags.has_errors(), "[{arch}] {}:\n{errors}", case.name);
        let image = rsasm::output::raw::build(&asm).expect("flat output");
        let mut want = vec![0u8; len];
        for (at, bytes) in runs {
            for (i, b) in bytes.split(' ').enumerate() {
                want[at + i] = u8::from_str_radix(b, 16).unwrap();
            }
        }
        assert_eq!(image.len(), len, "[{arch}] {}: image length", case.name);
        for line in (0..len).step_by(16) {
            let end = (line + 16).min(len);
            assert_eq!(
                hex(&image[line..end]),
                hex(&want[line..end]),
                "[{arch}] {}: bytes at {:#x}",
                case.name,
                case.base + line as u64
            );
        }
    }
}

// ---- what only a linker can supply ---------------------------------------
//
// These have no image to compare: the linker builds a GOT or a PLT to satisfy
// them, which a flat binary cannot contain. Refusing is the only answer that
// is not wrong.

#[allow(dead_code)]
fn flat_errors(arch: &str, src: &str) -> String {
    let asm = assemble_flat_for(arch, src, 0x10000);
    assert!(asm.diags.has_errors(), "[{arch}] expected an error:\n{src}");
    asm.diags.render(&asm.sm, false)
}

#[cfg(feature = "aarch64")]
#[test]
fn aarch64_got_references_are_refused() {
    let e = flat_errors(
        "aarch64",
        "adrp x0, :got:var\nldr x0, [x0, :got_lo12:var]\nvar: .quad 0\n",
    );
    assert_eq!(e.matches("this refers to a GOT entry").count(), 2, "{e}");
}

#[cfg(feature = "x86")]
#[test]
fn x86_got_modifiers_are_refused() {
    let e = flat_errors(
        "x86-64",
        "movq var@GOTPCREL(%rip), %rax\n.long var@GOT\nvar: .quad 0\n",
    );
    assert!(
        e.contains("`@gotpcrel` names something only a linker"),
        "{e}"
    );
    assert!(e.contains("`@got` names something only a linker"), "{e}");
}

#[cfg(feature = "superh")]
#[test]
fn superh_got_modifiers_are_refused() {
    let e = flat_errors("sh", ".long var@GOT\n.long var@GOTOFF\nvar: .long 0\n");
    assert!(e.contains("`@got` names something only a linker"), "{e}");
    assert!(e.contains("`@gotoff` names something only a linker"), "{e}");
}

#[cfg(feature = "riscv")]
#[test]
fn riscv_pcrel_lo_must_name_its_auipc() {
    // GNU ld refuses both: an addend, and a label on anything but the
    // instruction carrying the high half.
    let e = flat_errors(
        "riscv64",
        "1: auipc a0, %pcrel_hi(var)\n addi a0, a0, %pcrel_lo(1b + 4)\n\
         2: nop\n addi a0, a0, %pcrel_lo(2b)\nvar: .word 0\n",
    );
    assert_eq!(
        e.matches("must name the label on its high half").count(),
        2,
        "{e}"
    );
}

#[cfg(feature = "x86")]
#[test]
fn x86_64_images_match_a_linked_reference() {
    check("x86-64", X86_64);
}

/// Assembled with GNU as and GNU ld.
#[cfg(feature = "x86")]
const X86_64: &[Case] = &[
    Case {
        name: "rip-relative and absolute references into another section",
        base: 0x401000,
        src: r#"        .text
        leaq    msg(%rip), %rsi
        movl    $msg, %eax
        movabsq $msg, %rdx
        movq    msg(%rip), %rcx
        ret
        .data
        .space  0x100
msg:    .quad   1

"#,
        image: Some((
            294,
            &[
                (
                    0,
                    "48 8d 35 17 01 00 00 b8 1e 11 40 00 48 ba 1e 11 40 00 00 00 00 00 48 8b 0d 01 01 00 00 c3 00 00",
                ),
                (0x110, "00 00 00 00 00 00 00 00 00 00 00 00 00 00 01 00"),
            ],
        )),
    },
    Case {
        name: "calls into another section",
        base: 0x401000,
        src: r#"        .text
entry:
        call    helper
        call    helper@PLT
        .section .text.helper, "ax"
helper:
        ret

"#,
        image: Some((11, &[(0, "e8 05 00 00 00 e8 00 00 00 00 c3")])),
    },
    Case {
        name: "data across sections",
        base: 0x401000,
        src: r#"        .text
entry:
        ret
        .data
        .quad   entry
        .long   entry
        .long   entry - .
        .quad   entry - .
        .long   entry@PLT
"#,
        image: Some((
            29,
            &[(
                0,
                "c3 00 10 40 00 00 00 00 00 00 10 40 00 f3 ff ff ff ef ff ff ff ff ff ff ff e7 ff ff ff",
            )],
        )),
    },
];

#[cfg(feature = "x86")]
#[test]
fn i386_images_match_a_linked_reference() {
    check("i386", I386);
}

/// Assembled with GNU as and GNU ld.
#[cfg(feature = "x86")]
const I386: &[Case] = &[
    Case {
        name: "absolute references into another section",
        base: 0x8048000,
        src: r#"        .text
        movl    $msg, %eax
        movl    msg, %ecx
        leal    msg + 4, %edx
        ret
        .data
        .space  0x100
msg:    .long   1

"#,
        image: Some((
            278,
            &[
                (
                    0,
                    "b8 12 81 04 08 8b 0d 12 81 04 08 8d 15 16 81 04 08 c3 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
                ),
                (0x110, "00 00 01 00 00 00"),
            ],
        )),
    },
    Case {
        name: "calls into another section",
        base: 0x8048000,
        src: r#"        .text
entry:
        call    helper
        call    helper@PLT
        .section .text.helper, "ax"
helper:
        ret

"#,
        image: Some((11, &[(0, "e8 05 00 00 00 e8 00 00 00 00 c3")])),
    },
    Case {
        name: "data across sections",
        base: 0x8048000,
        src: r#"        .text
entry:
        ret
        .data
        .long   entry
        .long   entry - .
        .long   entry@PLT
"#,
        image: Some((13, &[(0, "c3 00 80 04 08 fb ff ff ff f7 ff ff ff")])),
    },
    Case {
        name: "the location counter in each item of a data list",
        base: 0x8048000,
        src: r#"        .text
entry:
        ret
        .long   ., ., . - entry
        .byte   . - entry, . - entry
"#,
        image: Some((15, &[(0, "c3 01 80 04 08 05 80 04 08 09 00 00 00 0d 0e")])),
    },
];

#[cfg(feature = "aarch64")]
#[test]
fn aarch64_images_match_a_linked_reference() {
    check("aarch64", AARCH64);
}

/// Assembled with llvm-mc and GNU ld.
#[cfg(feature = "aarch64")]
const AARCH64: &[Case] = &[
    Case {
        name: "adrp to a label on the same page",
        base: 0x400000,
        src: r#"start:
        adrp    x0, here
        add     x0, x0, :lo12:here
here:
        ret

"#,
        image: Some((12, &[(0, "00 00 00 90 00 20 00 91 c0 03 5f d6")])),
    },
    Case {
        name: "adrp to a later page in the same section",
        base: 0x400000,
        src: r#"        adrp    x0, far
        add     x0, x0, :lo12:far
        ret
        .space  0x2000 - 12
        nop
far:
        ret

"#,
        image: Some((
            8200,
            &[
                (0, "00 00 00 d0 00 10 00 91 c0 03 5f d6 00 00 00 00"),
                (0x2000, "1f 20 03 d5 c0 03 5f d6"),
            ],
        )),
    },
    Case {
        name: "adrp back to an earlier page",
        base: 0x400000,
        src: r#"back:
        ret
        .space  0x3000
        adrp    x1, back
        add     x1, x1, :lo12:back

"#,
        image: Some((
            12300,
            &[
                (0, "c0 03 5f d6 00 00 00 00 00 00 00 00 00 00 00 00"),
                (0x3000, "00 00 00 00 e1 ff ff b0 21 00 00 91"),
            ],
        )),
    },
    Case {
        name: "adrp from a page boundary",
        base: 0x400ff8,
        src: r#"        nop
        adrp    x0, next
        adrp    x1, next
        add     x1, x1, :lo12:next
next:
        ret

"#,
        image: Some((
            20,
            &[(
                0,
                "1f 20 03 d5 00 00 00 b0 01 00 00 90 21 20 00 91 c0 03 5f d6",
            )],
        )),
    },
    Case {
        name: "adrp into another section",
        base: 0x400000,
        src: r#"        .text
        adrp    x0, msg
        add     x0, x0, :lo12:msg
        ldrb    w1, [x0, :lo12:msg]
        ret
        .data
        .space  0x1234
msg:    .ascii  "hi"

"#,
        image: Some((
            4678,
            &[
                (0, "00 00 00 b0 00 10 09 91 01 10 49 39 c0 03 5f d6"),
                (0x1240, "00 00 00 00 68 69"),
            ],
        )),
    },
    Case {
        name: "scaled :lo12: loads into another section",
        base: 0x400000,
        src: r#"        .text
        adrp    x0, vars
        ldr     x1, [x0, :lo12:vars]
        ldr     w2, [x0, :lo12:vars + 8]
        ldrh    w3, [x0, :lo12:vars + 12]
        ldr     q4, [x0, :lo12:vars + 16]
        str     x5, [x0, :lo12:vars]
        ret
        .data
        .p2align 4
        .space  0xff0
vars:   .quad   1, 2, 3, 4

"#,
        image: Some((
            4144,
            &[
                (
                    0,
                    "00 00 00 b0 01 08 40 f9 02 18 40 b9 03 38 40 79 04 08 c0 3d 05 08 00 f9 c0 03 5f d6 00 00 00 00",
                ),
                (
                    0x1010,
                    "01 00 00 00 00 00 00 00 02 00 00 00 00 00 00 00 03 00 00 00 00 00 00 00 04 00 00 00 00 00 00 00",
                ),
            ],
        )),
    },
    Case {
        name: "adrp with an addend",
        base: 0x400000,
        src: r#"        .text
        adrp    x0, table + 0x1000
        add     x0, x0, :lo12:table + 0x1000
        adrp    x1, table - 4
        add     x1, x1, :lo12:table - 4
        ret
        .data
table:  .quad   0

"#,
        image: Some((
            28,
            &[(
                0,
                "00 00 00 b0 00 50 00 91 01 00 00 90 21 40 00 91 c0 03 5f d6 00 00 00 00 00 00 00 00",
            )],
        )),
    },
    Case {
        name: "adr and literal loads into another section",
        base: 0x400000,
        src: r#"        .text
        adr     x0, value
        ldr     x1, value
        ldr     w2, value
        b       .
        .data
value:  .quad   0x1122334455667788

"#,
        image: Some((
            24,
            &[(
                0,
                "80 00 00 10 61 00 00 58 42 00 00 18 00 00 00 14 88 77 66 55 44 33 22 11",
            )],
        )),
    },
    Case {
        name: "branches and data across sections",
        base: 0x400000,
        src: r#"        .text
entry:
        bl      helper
        b       helper
        .section .text.helper, "ax"
helper:
        ret
        .data
        .quad   entry
        .4byte  helper - .
        .8byte  entry - .
"#,
        image: Some((
            32,
            &[(
                0,
                "02 00 00 94 01 00 00 14 c0 03 5f d6 00 00 40 00 00 00 00 00 f4 ff ff ff e8 ff ff ff ff ff ff ff",
            )],
        )),
    },
    Case {
        name: "executable sections start on an instruction boundary",
        base: 0x400000,
        src: r#"        .text
        .byte   1
        .section .text.b,"ax"
        .byte   2
        .data
        .byte   3
"#,
        image: Some((6, &[(0, "01 00 00 00 02 03")])),
    },
];

#[cfg(feature = "aarch64")]
#[test]
fn aarch64_gas_images_match_a_linked_reference() {
    check("aarch64", AARCH64_GAS);
}

/// Assembled with GNU as and GNU ld.
#[cfg(feature = "aarch64")]
const AARCH64_GAS: &[Case] = &[Case {
    name: "adrp and :lo12: of absolute addresses",
    base: 0x400ff8,
    src: r#"        .set    konst, 0x12345
        .set    slot, 0x5008
        nop
        adrp    x0, konst
        add     x0, x0, :lo12:konst
        adrp    x1, 0x5000
        adrp    x4, -0x3000
        ldr     x2, [x1, :lo12:slot]
        adrp    x3, 0x400000
"#,
    image: Some((
        28,
        &[(
            0,
            "1f 20 03 d5 80 e0 ff d0 00 14 0d 91 21 00 00 b0 e4 ff ff b0 22 04 40 f9 03 20 00 90",
        )],
    )),
}];

#[cfg(feature = "arm")]
#[test]
fn arm_images_match_a_linked_reference() {
    check("arm", ARM);
}

/// Assembled with llvm-mc and GNU ld.
#[cfg(feature = "arm")]
const ARM: &[Case] = &[
    Case {
        name: "branches into another section",
        base: 0x8000,
        src: r#"        .text
        .p2align 2
entry:
        bl      helper
        b       helper
        beq     helper
        .section .text.helper, "ax"
        .p2align 2
helper:
        b       entry

"#,
        image: Some((
            16,
            &[(0, "01 00 00 eb 00 00 00 ea ff ff ff 0a fb ff ff ea")],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x8000,
        src: r#"        .text
        .p2align 2
entry:
        bx      lr
        .data
        .p2align 2
        .word   entry
        .word   entry - .
"#,
        image: Some((12, &[(0, "1e ff 2f e1 00 80 00 00 f8 ff ff ff")])),
    },
];

#[cfg(feature = "arm")]
#[test]
fn thumb_images_match_a_linked_reference() {
    check("thumb", THUMB);
}

/// Assembled with llvm-mc and GNU ld.
#[cfg(feature = "arm")]
const THUMB: &[Case] = &[
    Case {
        name: "branches into another section",
        base: 0x8000,
        src: r#"        .text
        .p2align 2
entry:
        bl      helper
        b.w     helper
        .section .text.helper, "ax"
        .p2align 2
helper:
        bl      entry
        bx      lr
        nop
"#,
        image: Some((
            16,
            &[(0, "00 f0 02 f8 00 f0 00 b8 ff f7 fa ff 70 47 00 bf")],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x8000,
        src: r#"        .text
        .p2align 2
entry:
        bx      lr
        nop
        .data
        .p2align 2
        .word   entry
        .word   entry - .
"#,
        image: Some((12, &[(0, "70 47 00 bf 00 80 00 00 f8 ff ff ff")])),
    },
];

#[cfg(feature = "arm")]
#[test]
fn arm_gas_images_match_a_linked_reference() {
    check("arm", ARM_GAS);
}

/// Assembled with GNU as and GNU ld.
#[cfg(feature = "arm")]
const ARM_GAS: &[Case] = &[
    Case {
        name: "literals loaded from a pool at the end of the section",
        base: 0x8000,
        src: r#"        .syntax unified
        .text
entry:  ldr     r0, =0x12345678
        ldr     r1, =msg
        ldr     r2, =msg + 3
        ldr     r3, =entry
        ldr     r4, =0x100
        bx      lr
        .data
msg:    .ascii  "hello"
"#,
        image: Some((
            45,
            &[(
                0,
                "10 00 9f e5 10 10 9f e5 10 20 9f e5 10 30 9f e5 01 4c a0 e3 1e ff 2f e1 78 56 34 12 28 80 00 00 2b 80 00 00 00 80 00 00 68 65 6c 6c 6f",
            )],
        )),
    },
    Case {
        name: "a pool placed by ltorg, and a second one after it",
        base: 0x8000,
        src: r#"        .syntax unified
entry:  ldr     r0, =table
        b       over
        .ltorg
over:   ldr     r1, =table + 4
        ldr     r2, =0xdeadbeef
        bx      lr
        .section .rodata
        .byte   1
table:  .word   1, 2, 3
"#,
        image: Some((
            45,
            &[(
                0,
                "00 00 1f e5 00 00 00 ea 21 80 00 00 04 10 9f e5 04 20 9f e5 1e ff 2f e1 25 80 00 00 ef be ad de 01 01 00 00 00 02 00 00 00 03 00 00 00",
            )],
        )),
    },
    Case {
        name: "adr and adrl within a section",
        base: 0x10000,
        src: r#"        .syntax unified
back:   adr     r0, back
        adr     r1, fwd
        adrl    r2, far
        adrl    r3, back
fwd:    nop
        .space  0x2000
far:    bx      lr
"#,
        image: Some((
            8224,
            &[
                (
                    0,
                    "08 00 4f e2 0c 10 8f e2 0c 20 8f e2 20 2c 82 e2 18 30 4f e2 00 00 a0 e1 00 f0 20 e3 00 00 00 00",
                ),
                (0x2010, "00 00 00 00 00 00 00 00 00 00 00 00 1e ff 2f e1"),
            ],
        )),
    },
    Case {
        name: "pools in two code sections",
        base: 0x8000,
        src: r#"        .syntax unified
        .text
start:  ldr     r0, =other
        bx      lr
        .section .text.other, "ax"
other:  ldr     r0, =0x11223344
        ldr     r1, =start
        bx      lr
"#,
        image: Some((
            32,
            &[(
                0,
                "00 00 1f e5 1e ff 2f e1 0c 80 00 00 04 00 9f e5 04 10 9f e5 1e ff 2f e1 44 33 22 11 00 80 00 00",
            )],
        )),
    },
    Case {
        name: "calls between ARM and Thumb functions in two sections",
        base: 0x8000,
        src: r#"        .syntax unified
        .text
        .global start
        .type   start, %function
start:  bl      tfunc
        blx     tfunc
        bl      afunc
        blx     afunc
        bl      local
        bx      lr
        .type   local, %function
local:  bx      lr
        .section .text.thumb, "ax"
        .thumb
        .thumb_func
tfunc:  bl      afunc
        blx     afunc
        bl      tfunc2
        blx     tfunc2
        bl      start
        bx      lr
        .thumb_func
tfunc2: bx      lr
        .section .text.arm, "ax"
        .arm
        .type   afunc, %function
afunc:  bx      lr
"#,
        image: Some((
            56,
            &[(
                0,
                "05 00 00 fa 04 00 00 fa 09 00 00 eb 08 00 00 eb 00 00 00 eb 1e ff 2f e1 1e ff 2f e1 00 f0 0a e8 00 f0 08 e8 00 f0 05 f8 00 f0 03 f8 ff f7 e8 ef 70 47 70 47 1e ff 2f e1",
            )],
        )),
    },
    Case {
        name: "addresses of Thumb functions in data and literal pools",
        base: 0x8000,
        src: r#"        .syntax unified
        .text
        ldr     r0, =tfunc
        ldr     r1, =afunc
        bx      lr
        .type   afunc, %function
afunc:  bx      lr
        .thumb
        .thumb_func
tfunc:  bx      lr
        .data
        .word   tfunc, afunc, tfunc + 4
"#,
        image: Some((
            40,
            &[(
                0,
                "0c 00 9f e5 0c 10 9f e5 1e ff 2f e1 1e ff 2f e1 70 47 00 00 11 80 00 00 0c 80 00 00 11 80 00 00 0c 80 00 00 15 80 00 00",
            )],
        )),
    },
    Case {
        name: "global Thumb functions called from the same section",
        base: 0x8000,
        src: r#"        .syntax unified
        .global g
        .thumb
        .thumb_func
g:      bx      lr
        .arm
        bl      g
        blx     g
        .thumb
        blx     g
        bl      g
        bx      lr
"#,
        image: Some((
            24,
            &[(
                0,
                "70 47 00 00 fd ff ff fa fc ff ff fa ff f7 f8 ff ff f7 f6 ff 70 47 00 bf",
            )],
        )),
    },
    Case {
        name: "a code section ending in Thumb is padded with Thumb no-ops",
        base: 0x8000,
        src: r#"        .syntax unified
        .text
        .type   fa, %function
fa:     bx      lr
        .thumb
        .thumb_func
fb:     bx      lr
        .section .text.b, "ax"
        .arm
        .type   fc, %function
fc:     bl      fb
        bx      lr
"#,
        image: Some((
            16,
            &[(0, "1e ff 2f e1 70 47 00 bf fd ff ff fa 1e ff 2f e1")],
        )),
    },
];

#[cfg(feature = "arm")]
#[test]
fn thumb_gas_images_match_a_linked_reference() {
    check("thumb", THUMB_GAS);
}

/// Assembled with GNU as and GNU ld.
#[cfg(feature = "arm")]
const THUMB_GAS: &[Case] = &[
    Case {
        name: "literals loaded from a pool at the end of the section",
        base: 0x8000,
        src: r#"        .syntax unified
        .text
entry:  ldr     r0, =0x12345678
        ldr     r1, =msg
        ldr     r8, =msg + 3
        ldr     r3, =0xff
        movs    r0, r0
        bx      lr
        .data
msg:    .ascii  "hello"
"#,
        image: Some((
            33,
            &[(
                0,
                "03 48 04 49 df f8 10 80 4f f0 ff 03 00 00 70 47 78 56 34 12 1c 80 00 00 1f 80 00 00 68 65 6c 6c 6f",
            )],
        )),
    },
    Case {
        name: "a load that needs 32 bits to reach its pool",
        base: 0x8000,
        src: r#"        .syntax unified
entry:  ldr     r0, =data
        ldr     r1, =data + 4
        .space  1020
        .ltorg
        bx      lr
        .data
        .space  3
data:   .word   0
"#,
        image: Some((
            1047,
            &[
                (0, "df f8 00 04 df f8 00 14 00 00 00 00 00 00 00 00"),
                (0x400, "00 00 00 00 13 84 00 00 17 84 00 00 70 47 00 bf"),
            ],
        )),
    },
    Case {
        name: "adr within a section",
        base: 0x9002,
        src: r#"        .syntax unified
back:   adr     r0, back
        adr     r1, fwd
        adr     r2, far
        .p2align 2, 0
fwd:    nop
        .space  0x800
        .p2align 2, 0
far:    bx      lr
"#,
        image: Some((
            2068,
            &[
                (0, "af f2 04 00 01 a1 0f f6 08 02 00 00 00 bf 00 00"),
                (0x810, "70 47 00 bf"),
            ],
        )),
    },
    Case {
        name: "a section a linker places at an odd halfword",
        base: 0x8000,
        src: r#"        .syntax unified
        .text
        nop
        .section .text.b, "ax"
        .thumb_func
fc:     push    {r4, lr}
        pop     {r4, pc}
        adr     r1, fc
        adr     r2, fwd
        blx     fwd
        bx      lr
        .thumb_func
fwd:    bx      lr
"#,
        image: Some((
            22,
            &[(
                0,
                "00 bf 10 b5 10 bd af f2 07 01 0f f2 07 02 00 f0 01 f8 70 47 70 47",
            )],
        )),
    },
];

#[cfg(feature = "riscv")]
#[test]
fn riscv32_images_match_a_linked_reference() {
    check("riscv32", RISCV32);
}

/// Assembled with llvm-mc and GNU ld.
#[cfg(feature = "riscv")]
const RISCV32: &[Case] = &[
    Case {
        name: "hi and lo of a label in another section",
        base: 0x10000,
        src: r#"        .text
        .p2align 2
        lui     a0, %hi(msg)
        addi    a0, a0, %lo(msg)
        lw      a1, %lo(msg)(a0)
        sw      a1, %lo(msg + 4)(a0)
        ret
        .data
        .p2align 2
        .space  0x1800
msg:    .word   1, 2

"#,
        image: Some((
            6172,
            &[
                (
                    0,
                    "37 25 01 00 13 05 45 81 83 25 45 81 23 2c b5 80 82 80 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
                ),
                (0x1810, "00 00 00 00 01 00 00 00 02 00 00 00"),
            ],
        )),
    },
    Case {
        name: "pcrel_hi and pcrel_lo in the same section",
        base: 0x10000,
        src: r#"        .text
        .p2align 2
        nop
1:      auipc   a0, %pcrel_hi(target)
        addi    a0, a0, %pcrel_lo(1b)
        lw      a1, %pcrel_lo(1b)(a0)
        sw      a1, %pcrel_lo(1b)(a0)
        ret
        .space  0x900
target: .word   0

"#,
        image: Some((
            2328,
            &[(
                0,
                "01 00 17 15 00 00 13 05 25 91 83 25 25 91 23 29 b5 90 82 80 00 00 00 00 00 00 00 00 00 00 00 00",
            )],
        )),
    },
    Case {
        name: "pcrel_hi and pcrel_lo into another section",
        base: 0x10000,
        src: r#"        .text
        .p2align 2
here:   auipc   a0, %pcrel_hi(msg)
        addi    a0, a0, %pcrel_lo(here)
        ret
        .data
        .p2align 2
        .space  0x2810
msg:    .word   1

"#,
        image: Some((
            10272,
            &[
                (0, "17 35 00 00 13 05 c5 81 82 80 00 00 00 00 00 00"),
                (0x2810, "00 00 00 00 00 00 00 00 00 00 00 00 01 00 00 00"),
            ],
        )),
    },
    Case {
        name: "pcrel_lo naming an auipc behind it",
        base: 0x10ffc,
        src: r#"        .text
        .p2align 2
back:   auipc   a0, %pcrel_hi(data)
        nop
        nop
        addi    a0, a0, %pcrel_lo(back)
        ret
        .data
        .p2align 2
data:   .word   1

"#,
        image: Some((
            20,
            &[(
                0,
                "17 05 00 00 01 00 01 00 13 05 05 01 82 80 00 00 01 00 00 00",
            )],
        )),
    },
    Case {
        name: "la, call and tail into another section",
        base: 0x10000,
        src: r#"        .data
        .text
        .p2align 2
entry:
        la      a0, msg
        call    helper
        tail    helper
        .section .text.helper, "ax"
        .p2align 2
helper:
        ret
        .data
        .p2align 2
        .space  0x1000
msg:    .word   1

"#,
        image: Some((
            4126,
            &[
                (
                    0,
                    "17 15 00 00 13 05 85 01 97 10 00 00 e7 80 40 01 17 13 00 00 67 00 c3 00 00 00 00 00 00 00 00 00",
                ),
                (0x1010, "00 00 00 00 00 00 00 00 01 00 00 00 82 80"),
            ],
        )),
    },
    Case {
        name: "jumps and branches into another section",
        base: 0x10000,
        src: r#"        .option norvc
        .text
        .p2align 2
entry:
        jal     helper
        j       helper
        .section .text.helper, "ax"
        .p2align 2
helper:
        j       entry

"#,
        image: Some((12, &[(0, "ef 00 80 00 6f 00 40 00 6f f0 9f ff")])),
    },
    Case {
        name: "data across sections",
        base: 0x10000,
        src: r#"        .text
        .p2align 2
entry:
        ret
        .data
        .p2align 2
        .word   entry
        .word   entry - .
"#,
        image: Some((12, &[(0, "82 80 00 00 00 00 01 00 f8 ff ff ff")])),
    },
];

#[cfg(feature = "riscv")]
#[test]
fn riscv64_images_match_a_linked_reference() {
    check("riscv64", RISCV64);
}

/// Assembled with llvm-mc and GNU ld.
#[cfg(feature = "riscv")]
const RISCV64: &[Case] = &[
    Case {
        name: "hi and lo of a label in another section",
        base: 0x10000,
        src: r#"        .text
        .p2align 2
        lui     a0, %hi(msg)
        addi    a0, a0, %lo(msg)
        lw      a1, %lo(msg)(a0)
        sw      a1, %lo(msg + 4)(a0)
        ret
        .data
        .p2align 2
        .space  0x1800
msg:    .word   1, 2

"#,
        image: Some((
            6172,
            &[
                (
                    0,
                    "37 25 01 00 13 05 45 81 83 25 45 81 23 2c b5 80 82 80 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
                ),
                (0x1810, "00 00 00 00 01 00 00 00 02 00 00 00"),
            ],
        )),
    },
    Case {
        name: "pcrel_hi and pcrel_lo in the same section",
        base: 0x10000,
        src: r#"        .text
        .p2align 2
        nop
1:      auipc   a0, %pcrel_hi(target)
        addi    a0, a0, %pcrel_lo(1b)
        lw      a1, %pcrel_lo(1b)(a0)
        sw      a1, %pcrel_lo(1b)(a0)
        ret
        .space  0x900
target: .word   0

"#,
        image: Some((
            2328,
            &[(
                0,
                "01 00 17 15 00 00 13 05 25 91 83 25 25 91 23 29 b5 90 82 80 00 00 00 00 00 00 00 00 00 00 00 00",
            )],
        )),
    },
    Case {
        name: "pcrel_hi and pcrel_lo into another section",
        base: 0x10000,
        src: r#"        .text
        .p2align 2
here:   auipc   a0, %pcrel_hi(msg)
        addi    a0, a0, %pcrel_lo(here)
        ret
        .data
        .p2align 2
        .space  0x2810
msg:    .word   1

"#,
        image: Some((
            10272,
            &[
                (0, "17 35 00 00 13 05 c5 81 82 80 00 00 00 00 00 00"),
                (0x2810, "00 00 00 00 00 00 00 00 00 00 00 00 01 00 00 00"),
            ],
        )),
    },
    Case {
        name: "pcrel_lo naming an auipc behind it",
        base: 0x10ffc,
        src: r#"        .text
        .p2align 2
back:   auipc   a0, %pcrel_hi(data)
        nop
        nop
        addi    a0, a0, %pcrel_lo(back)
        ret
        .data
        .p2align 2
data:   .word   1

"#,
        image: Some((
            20,
            &[(
                0,
                "17 05 00 00 01 00 01 00 13 05 05 01 82 80 00 00 01 00 00 00",
            )],
        )),
    },
    Case {
        name: "la, call and tail into another section",
        base: 0x10000,
        src: r#"        .data
        .text
        .p2align 2
entry:
        la      a0, msg
        call    helper
        tail    helper
        .section .text.helper, "ax"
        .p2align 2
helper:
        ret
        .data
        .p2align 2
        .space  0x1000
msg:    .word   1

"#,
        image: Some((
            4126,
            &[
                (
                    0,
                    "17 15 00 00 13 05 85 01 97 10 00 00 e7 80 40 01 17 13 00 00 67 00 c3 00 00 00 00 00 00 00 00 00",
                ),
                (0x1010, "00 00 00 00 00 00 00 00 01 00 00 00 82 80"),
            ],
        )),
    },
    Case {
        name: "jumps and branches into another section",
        base: 0x10000,
        src: r#"        .option norvc
        .text
        .p2align 2
entry:
        jal     helper
        j       helper
        .section .text.helper, "ax"
        .p2align 2
helper:
        j       entry

"#,
        image: Some((12, &[(0, "ef 00 80 00 6f 00 40 00 6f f0 9f ff")])),
    },
    Case {
        name: "data across sections",
        base: 0x10000,
        src: r#"        .text
        .p2align 2
entry:
        ret
        .data
        .p2align 2
        .word   entry
        .word   entry - .
"#,
        image: Some((12, &[(0, "82 80 00 00 00 00 01 00 f8 ff ff ff")])),
    },
];

#[cfg(feature = "powerpc")]
#[test]
fn powerpc_images_match_a_linked_reference() {
    check("powerpc", POWERPC);
}

/// Assembled with llvm-mc and GNU ld.
#[cfg(feature = "powerpc")]
const POWERPC: &[Case] = &[
    Case {
        name: "ha and l of a label in another section",
        base: 0x10000000,
        src: r#"	.text
	.p2align 2
	lis 3, msg@ha
	addi 3, 3, msg@l
	lwz 4, msg@l(3)
	stw 4, msg+4@l(3)
	blr
	.data
	.p2align 2
	.space 0x8010
msg:	.long 1, 2

"#,
        image: Some((
            32812,
            &[
                (
                    0,
                    "3c 60 10 01 38 63 80 24 80 83 80 24 90 83 80 28 4e 80 00 20 00 00 00 00 00 00 00 00 00 00 00 00",
                ),
                (0x8020, "00 00 00 00 00 00 00 01 00 00 00 02"),
            ],
        )),
    },
    Case {
        name: "h and ha differ where bit 15 is set",
        base: 0x10007ff0,
        src: r#"	.text
	.p2align 2
here:
	lis 3, there@h
	ori 3, 3, there@l
	lis 4, there@ha
	addi 4, 4, there@l
	blr
there:
	blr

"#,
        image: Some((
            24,
            &[(
                0,
                "3c 60 10 00 60 63 80 04 3c 80 10 01 38 84 80 04 4e 80 00 20 4e 80 00 20",
            )],
        )),
    },
    Case {
        name: "ha of a label below 0x8000 of its page",
        base: 0x1000fff0,
        src: r#"	.text
	.p2align 2
	lis 3, next@ha
	addi 3, 3, next@l
	lis 4, next@h
next:
	blr

"#,
        image: Some((
            16,
            &[(0, "3c 60 10 01 38 63 ff fc 3c 80 10 00 4e 80 00 20")],
        )),
    },
    Case {
        name: "ha and l with an addend",
        base: 0x10000000,
        src: r#"	.text
	.p2align 2
	lis 3, table+0x10000@ha
	addi 3, 3, table+0x10000@l
	lis 4, table-8@ha
	addi 4, 4, table-8@l
	blr
	.data
	.p2align 2
table:	.long 0

"#,
        image: Some((
            24,
            &[(
                0,
                "3c 60 10 01 38 63 00 14 3c 80 10 00 38 84 00 0c 4e 80 00 20 00 00 00 00",
            )],
        )),
    },
    Case {
        name: "branches into another section",
        base: 0x10000000,
        src: r#"	.text
	.p2align 2
entry:
	bl helper
	b helper
	beq helper
	.section .text.helper, "ax"
	.p2align 2
helper:
	b entry
	bdnz entry

"#,
        image: Some((
            20,
            &[(
                0,
                "48 00 00 0d 48 00 00 08 41 82 00 04 4b ff ff f4 42 00 ff f0",
            )],
        )),
    },
    Case {
        name: "absolute branches to labels",
        base: 0x1000,
        src: r#"	.text
	.p2align 2
entry:
	bla target
	ba target
	.section .text.helper, "ax"
	.p2align 2
target:
	blr
	.org 0x40

"#,
        image: Some((
            72,
            &[(0, "48 00 10 0b 48 00 10 0a 4e 80 00 20 00 00 00 00")],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x10000000,
        src: r#"	.text
	.p2align 2
entry:
	blr
	.data
	.p2align 2
	.long entry
	.long entry - .
"#,
        image: Some((12, &[(0, "4e 80 00 20 10 00 00 00 ff ff ff f8")])),
    },
];

#[cfg(feature = "powerpc")]
#[test]
fn powerpc64_images_match_a_linked_reference() {
    check("powerpc64", POWERPC64);
}

/// Assembled with llvm-mc and GNU ld.
#[cfg(feature = "powerpc")]
const POWERPC64: &[Case] = &[
    Case {
        name: "ha and l of a label in another section",
        base: 0x10000000,
        src: r#"	.text
	.p2align 4
	lis 3, msg@ha
	addi 3, 3, msg@l
	lwz 4, msg@l(3)
	ld 5, msg+8@l(3)
	std 5, msg+8@l(3)
	blr
	.data
	.p2align 4
	.space 0x8010
msg:	.quad 1, 2

"#,
        image: Some((
            32832,
            &[
                (
                    0,
                    "3c 60 10 01 38 63 80 30 80 83 80 30 e8 a3 80 38 f8 a3 80 38 4e 80 00 20 00 00 00 00 00 00 00 00",
                ),
                (0x8030, "00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 02"),
            ],
        )),
    },
    Case {
        name: "h and ha differ where bit 15 is set",
        base: 0x10007ff0,
        src: r#"	.text
	.p2align 4
here:
	lis 3, there@h
	ori 3, 3, there@l
	lis 4, there@ha
	addi 4, 4, there@l
	blr
there:
	blr

"#,
        image: Some((
            24,
            &[(
                0,
                "3c 60 10 00 60 63 80 04 3c 80 10 01 38 84 80 04 4e 80 00 20 4e 80 00 20",
            )],
        )),
    },
    Case {
        name: "ha and l with an addend",
        base: 0x10000000,
        src: r#"	.text
	.p2align 4
	lis 3, table+0x10000@ha
	addi 3, 3, table+0x10000@l
	lis 4, table-8@ha
	addi 4, 4, table-8@l
	blr
	.data
	.p2align 4
table:	.quad 0

"#,
        image: Some((
            40,
            &[(
                0,
                "3c 60 10 01 38 63 00 20 3c 80 10 00 38 84 00 18 4e 80 00 20 00 00 00 00 00 00 00 00 00 00 00 00",
            )],
        )),
    },
    Case {
        name: "branches into another section",
        base: 0x10000000,
        src: r#"	.text
	.p2align 4
entry:
	bl helper
	b helper
	beq helper
	.section .text.helper, "ax"
	.p2align 4
helper:
	b entry
	bdnz entry

"#,
        image: Some((
            24,
            &[(
                0,
                "48 00 00 11 48 00 00 0c 41 82 00 08 00 00 00 00 4b ff ff f0 42 00 ff ec",
            )],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x10000000,
        src: r#"	.text
	.p2align 4
entry:
	blr
	.data
	.p2align 4
	.quad entry
	.long entry - .
	.quad entry - .
"#,
        image: Some((
            36,
            &[(
                0,
                "4e 80 00 20 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 10 00 00 00 ff ff ff e8 ff ff ff ff ff ff ff e4",
            )],
        )),
    },
    Case {
        name: "prefixed PC-relative references to another section",
        base: 0x10000000,
        src: r#"	.text
	.p2align 4
	paddi 3, 0, msg@pcrel, 1
	pld 4, msg@pcrel+8(0), 1
	pstd 5, msg@pcrel(0), 1
	plxv 33, msg@pcrel+16(0), 1
	blr
	.data
	.p2align 4
	.space 0x8010
msg:	.quad 1, 2, 3, 4

"#,
        image: Some((
            32864,
            &[
                (
                    0,
                    "06 10 00 00 38 60 80 40 04 10 00 00 e4 80 80 40 04 10 00 00 f4 a0 80 30 04 10 00 00 cc 20 80 38 4e 80 00 20 00 00 00 00 00 00 00 00 00 00 00 00",
                ),
                (
                    0x8040,
                    "00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 02 00 00 00 00 00 00 00 03 00 00 00 00 00 00 00 04",
                ),
            ],
        )),
    },
    Case {
        name: "a prefixed PC-relative reference within its own section",
        base: 0x10000000,
        src: r#"	.text
	.p2align 4
	paddi 3, 0, there@pcrel, 1
	plwz 4, back@pcrel(0), 1
	nop
there:
	blr
back:	.long 7

"#,
        image: Some((
            28,
            &[(
                0,
                "06 10 00 00 38 60 00 14 06 10 00 00 80 80 00 10 60 00 00 00 4e 80 00 20 00 00 00 07",
            )],
        )),
    },
    Case {
        name: "a prefixed instruction padded off a 64-byte boundary",
        base: 0x10000000,
        src: r#"	.text
	.p2align 6
	.fill 15, 4, 0x60000000
here:	paddi 3, 0, here@pcrel, 1
	b here
"#,
        image: Some((
            76,
            &[(
                0,
                "60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 06 10 00 00 38 60 00 00 4b ff ff f8",
            )],
        )),
    },
];

#[cfg(feature = "powerpc")]
#[test]
fn powerpc64le_images_match_a_linked_reference() {
    check("powerpc64le", POWERPC64LE);
}

/// Assembled with llvm-mc and GNU ld.
#[cfg(feature = "powerpc")]
const POWERPC64LE: &[Case] = &[
    Case {
        name: "ha and l of a label in another section",
        base: 0x10000000,
        src: r#"	.text
	.p2align 4
	lis 3, msg@ha
	addi 3, 3, msg@l
	lwz 4, msg@l(3)
	ld 5, msg+8@l(3)
	std 5, msg+8@l(3)
	blr
	.data
	.p2align 4
	.space 0x8010
msg:	.quad 1, 2

"#,
        image: Some((
            32832,
            &[
                (
                    0,
                    "01 10 60 3c 30 80 63 38 30 80 83 80 38 80 a3 e8 38 80 a3 f8 20 00 80 4e 00 00 00 00 00 00 00 00",
                ),
                (0x8030, "01 00 00 00 00 00 00 00 02 00 00 00 00 00 00 00"),
            ],
        )),
    },
    Case {
        name: "h and ha differ where bit 15 is set",
        base: 0x10007ff0,
        src: r#"	.text
	.p2align 4
here:
	lis 3, there@h
	ori 3, 3, there@l
	lis 4, there@ha
	addi 4, 4, there@l
	blr
there:
	blr

"#,
        image: Some((
            24,
            &[(
                0,
                "00 10 60 3c 04 80 63 60 01 10 80 3c 04 80 84 38 20 00 80 4e 20 00 80 4e",
            )],
        )),
    },
    Case {
        name: "ha and l with an addend",
        base: 0x10000000,
        src: r#"	.text
	.p2align 4
	lis 3, table+0x10000@ha
	addi 3, 3, table+0x10000@l
	lis 4, table-8@ha
	addi 4, 4, table-8@l
	blr
	.data
	.p2align 4
table:	.quad 0

"#,
        image: Some((
            40,
            &[(
                0,
                "01 10 60 3c 20 00 63 38 00 10 80 3c 18 00 84 38 20 00 80 4e 00 00 00 00 00 00 00 00 00 00 00 00",
            )],
        )),
    },
    Case {
        name: "branches into another section",
        base: 0x10000000,
        src: r#"	.text
	.p2align 4
entry:
	bl helper
	b helper
	beq helper
	.section .text.helper, "ax"
	.p2align 4
helper:
	b entry
	bdnz entry

"#,
        image: Some((
            24,
            &[(
                0,
                "11 00 00 48 0c 00 00 48 08 00 82 41 00 00 00 00 f0 ff ff 4b ec ff 00 42",
            )],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x10000000,
        src: r#"	.text
	.p2align 4
entry:
	blr
	.data
	.p2align 4
	.quad entry
	.long entry - .
	.quad entry - .
"#,
        image: Some((
            36,
            &[(
                0,
                "20 00 80 4e 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 10 00 00 00 00 e8 ff ff ff e4 ff ff ff ff ff ff ff",
            )],
        )),
    },
    Case {
        name: "prefixed PC-relative references to another section",
        base: 0x10000000,
        src: r#"	.text
	.p2align 4
	paddi 3, 0, msg@pcrel, 1
	pld 4, msg@pcrel+8(0), 1
	pstd 5, msg@pcrel(0), 1
	plxv 33, msg@pcrel+16(0), 1
	blr
	.data
	.p2align 4
	.space 0x8010
msg:	.quad 1, 2, 3, 4

"#,
        image: Some((
            32864,
            &[
                (
                    0,
                    "00 00 10 06 40 80 60 38 00 00 10 04 40 80 80 e4 00 00 10 04 30 80 a0 f4 00 00 10 04 38 80 20 cc 20 00 80 4e 00 00 00 00 00 00 00 00 00 00 00 00",
                ),
                (
                    0x8040,
                    "01 00 00 00 00 00 00 00 02 00 00 00 00 00 00 00 03 00 00 00 00 00 00 00 04 00 00 00 00 00 00 00",
                ),
            ],
        )),
    },
    Case {
        name: "a prefixed PC-relative reference within its own section",
        base: 0x10000000,
        src: r#"	.text
	.p2align 4
	paddi 3, 0, there@pcrel, 1
	plwz 4, back@pcrel(0), 1
	nop
there:
	blr
back:	.long 7

"#,
        image: Some((
            28,
            &[(
                0,
                "00 00 10 06 14 00 60 38 00 00 10 06 10 00 80 80 00 00 00 60 20 00 80 4e 07 00 00 00",
            )],
        )),
    },
    Case {
        name: "a prefixed instruction padded off a 64-byte boundary",
        base: 0x10000000,
        src: r#"	.text
	.p2align 6
	.fill 15, 4, 0x60000000
here:	paddi 3, 0, here@pcrel, 1
	b here
"#,
        image: Some((
            76,
            &[(
                0,
                "00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 00 60 00 00 10 06 00 00 60 38 f8 ff ff 4b",
            )],
        )),
    },
];

#[cfg(feature = "mips")]
#[test]
fn mips_images_match_a_linked_reference() {
    check("mips", MIPS);
}

/// Assembled with llvm-mc and GNU ld.
#[cfg(feature = "mips")]
const MIPS: &[Case] = &[
    Case {
        name: "hi and lo of a label in another section",
        base: 0x400000,
        src: r#"        .text
        .p2align 4
        lui     $a0, %hi(msg)
        addiu   $a0, $a0, %lo(msg)
        lw      $a1, %lo(msg)($a0)
        sw      $a1, %lo(msg + 4)($a0)
        .p2align 4
        .data
        .p2align 4
        .space  0x8000
msg:    .word   1, 2
        .p2align 4

"#,
        image: Some((
            32800,
            &[
                (0, "3c 04 00 41 24 84 80 10 8c 85 80 10 ac 85 80 14"),
                (0x8010, "00 00 00 01 00 00 00 02 00 00 00 00 00 00 00 00"),
            ],
        )),
    },
    Case {
        name: "hi and lo where the low half is negative",
        base: 0x407ff0,
        src: r#"        .text
        .p2align 4
here:
        lui     $a0, %hi(there)
        addiu   $a0, $a0, %lo(there)
        lui     $a1, %hi(here)
        addiu   $a1, $a1, %lo(here)
there:
        .p2align 4

"#,
        image: Some((
            16,
            &[(0, "3c 04 00 41 24 84 80 00 3c 05 00 40 24 a5 7f f0")],
        )),
    },
    Case {
        name: "la of a label",
        base: 0x400000,
        src: r#"        .text
        .p2align 4
        la      $a0, value
        la      $a1, value + 0x10000
        .p2align 4
        .data
        .p2align 4
        .space  0x9000
value:  .word   0x11223344
        .p2align 4

"#,
        image: Some((
            36896,
            &[
                (0, "3c 04 00 41 24 84 90 10 3c 05 00 42 24 a5 90 10"),
                (0x9010, "11 22 33 44 00 00 00 00 00 00 00 00 00 00 00 00"),
            ],
        )),
    },
    Case {
        name: "jal and j to labels in another section",
        base: 0x400000,
        src: r#"        .text
        .p2align 4
start:
        jal     func
        .p2align 4
        .section .text.func, "ax"
        .p2align 4
func:
        j       start
        .p2align 4

"#,
        image: Some((
            32,
            &[(
                0,
                "0c 10 00 04 00 00 00 00 00 00 00 00 00 00 00 00 08 10 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            )],
        )),
    },
    Case {
        name: "a branch into another section",
        base: 0x400000,
        src: r#"        .text
        .p2align 4
entry:
        bal     helper
        .p2align 4
        .section .text.helper, "ax"
        .p2align 4
helper:
        bal     entry
        .p2align 4

"#,
        image: Some((
            32,
            &[(
                0,
                "04 11 00 03 00 00 00 00 00 00 00 00 00 00 00 00 04 11 ff fb 00 00 00 00 00 00 00 00 00 00 00 00",
            )],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x400000,
        src: r#"        .text
        .p2align 4
entry:
        .word   0
        .section .text.helper, "ax"
        .p2align 4
helper:
        .word   0
        .data
        .p2align 4
        .word   entry
        .word   helper - .

        .p2align 4

"#,
        image: Some((
            48,
            &[(0x20, "00 40 00 00 ff ff ff ec 00 00 00 00 00 00 00 00")],
        )),
    },
    Case {
        name: "refused: jal across a 256 MB region boundary",
        base: 0xffffff0,
        src: r#"        .text
        .p2align 4
        jal     far
        .p2align 4
        .section .text.far, "ax"
        .p2align 4
far:
        .word   0

"#,
        image: None,
    },
    Case {
        name: "jal to a number takes its region from the jump",
        base: 0xffffff0,
        src: r#"        .text
        .p2align 4
        jal     0x10000010
        .p2align 4
"#,
        image: Some((
            16,
            &[(0, "0c 00 00 04 00 00 00 00 00 00 00 00 00 00 00 00")],
        )),
    },
    Case {
        name: "sections start at their default alignment",
        base: 0x400000,
        src: r#"        .text
        .byte   1
        .data
        .byte   2
        .section .rodata
        .byte   3
"#,
        image: Some((
            18,
            &[(0, "01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 02 03")],
        )),
    },
];

#[cfg(feature = "mips")]
#[test]
fn mipsel_images_match_a_linked_reference() {
    check("mipsel", MIPSEL);
}

/// Assembled with llvm-mc and GNU ld.
#[cfg(feature = "mips")]
const MIPSEL: &[Case] = &[
    Case {
        name: "hi and lo of a label in another section",
        base: 0x400000,
        src: r#"        .text
        .p2align 4
        lui     $a0, %hi(msg)
        addiu   $a0, $a0, %lo(msg)
        lw      $a1, %lo(msg)($a0)
        sw      $a1, %lo(msg + 4)($a0)
        .p2align 4
        .data
        .p2align 4
        .space  0x8000
msg:    .word   1, 2
        .p2align 4

"#,
        image: Some((
            32800,
            &[
                (0, "41 00 04 3c 10 80 84 24 10 80 85 8c 14 80 85 ac"),
                (0x8010, "01 00 00 00 02 00 00 00 00 00 00 00 00 00 00 00"),
            ],
        )),
    },
    Case {
        name: "hi and lo where the low half is negative",
        base: 0x407ff0,
        src: r#"        .text
        .p2align 4
here:
        lui     $a0, %hi(there)
        addiu   $a0, $a0, %lo(there)
        lui     $a1, %hi(here)
        addiu   $a1, $a1, %lo(here)
there:
        .p2align 4

"#,
        image: Some((
            16,
            &[(0, "41 00 04 3c 00 80 84 24 40 00 05 3c f0 7f a5 24")],
        )),
    },
    Case {
        name: "la of a label",
        base: 0x400000,
        src: r#"        .text
        .p2align 4
        la      $a0, value
        la      $a1, value + 0x10000
        .p2align 4
        .data
        .p2align 4
        .space  0x9000
value:  .word   0x11223344
        .p2align 4

"#,
        image: Some((
            36896,
            &[
                (0, "41 00 04 3c 10 90 84 24 42 00 05 3c 10 90 a5 24"),
                (0x9010, "44 33 22 11 00 00 00 00 00 00 00 00 00 00 00 00"),
            ],
        )),
    },
    Case {
        name: "jal and j to labels in another section",
        base: 0x400000,
        src: r#"        .text
        .p2align 4
start:
        jal     func
        .p2align 4
        .section .text.func, "ax"
        .p2align 4
func:
        j       start
        .p2align 4

"#,
        image: Some((
            32,
            &[(
                0,
                "04 00 10 0c 00 00 00 00 00 00 00 00 00 00 00 00 00 00 10 08 00 00 00 00 00 00 00 00 00 00 00 00",
            )],
        )),
    },
    Case {
        name: "a branch into another section",
        base: 0x400000,
        src: r#"        .text
        .p2align 4
entry:
        bal     helper
        .p2align 4
        .section .text.helper, "ax"
        .p2align 4
helper:
        bal     entry
        .p2align 4

"#,
        image: Some((
            32,
            &[(
                0,
                "03 00 11 04 00 00 00 00 00 00 00 00 00 00 00 00 fb ff 11 04 00 00 00 00 00 00 00 00 00 00 00 00",
            )],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x400000,
        src: r#"        .text
        .p2align 4
entry:
        .word   0
        .section .text.helper, "ax"
        .p2align 4
helper:
        .word   0
        .data
        .p2align 4
        .word   entry
        .word   helper - .

        .p2align 4

"#,
        image: Some((
            48,
            &[(0x20, "00 00 40 00 ec ff ff ff 00 00 00 00 00 00 00 00")],
        )),
    },
    Case {
        name: "refused: jal across a 256 MB region boundary",
        base: 0xffffff0,
        src: r#"        .text
        .p2align 4
        jal     far
        .p2align 4
        .section .text.far, "ax"
        .p2align 4
far:
        .word   0

"#,
        image: None,
    },
    Case {
        name: "jal to a number takes its region from the jump",
        base: 0xffffff0,
        src: r#"        .text
        .p2align 4
        jal     0x10000010
        .p2align 4
"#,
        image: Some((
            16,
            &[(0, "04 00 00 0c 00 00 00 00 00 00 00 00 00 00 00 00")],
        )),
    },
    Case {
        name: "sections start at their default alignment",
        base: 0x400000,
        src: r#"        .text
        .byte   1
        .data
        .byte   2
        .section .rodata
        .byte   3
"#,
        image: Some((
            18,
            &[(0, "01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 02 03")],
        )),
    },
];

#[cfg(feature = "sparc")]
#[test]
fn sparc_images_match_a_linked_reference() {
    check("sparc", SPARC);
}

/// Assembled with llvm-mc and GNU ld.
#[cfg(feature = "sparc")]
const SPARC: &[Case] = &[
    Case {
        name: "hi and lo of a label in another section",
        base: 0x100000,
        src: r#"        .text
        .p2align 2
        sethi   %hi(msg), %o0
        or      %o0, %lo(msg), %o0
        ld      [%o0 + %lo(msg)], %o1
        retl
        nop
        .data
        .p2align 2
        .space  0x1400
msg:    .word   1

"#,
        image: Some((
            5144,
            &[
                (
                    0,
                    "11 00 04 05 90 12 20 14 d2 02 20 14 81 c3 e0 08 01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
                ),
                (0x1410, "00 00 00 00 00 00 00 01"),
            ],
        )),
    },
    Case {
        name: "hi and lo with an addend",
        base: 0x100000,
        src: r#"        .text
        .p2align 2
        sethi   %hi(msg + 0x3ff), %o0
        add     %o0, %lo(msg + 0x3ff), %o0
        sethi   %hi(msg - 1), %o1
        retl
        nop
        .data
        .p2align 2
msg:    .word   1

"#,
        image: Some((
            24,
            &[(
                0,
                "11 00 04 01 90 02 20 13 13 00 04 00 81 c3 e0 08 01 00 00 00 00 00 00 01",
            )],
        )),
    },
    Case {
        name: "call and branches into another section",
        base: 0x100000,
        src: r#"        .text
        .p2align 2
entry:
        call    helper
        nop
        ba      helper
        nop
        .section .text.helper, "ax"
        .p2align 2
helper:
        bne     entry
        nop

"#,
        image: Some((
            24,
            &[(
                0,
                "40 00 00 04 01 00 00 00 10 80 00 02 01 00 00 00 12 bf ff fc 01 00 00 00",
            )],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x100000,
        src: r#"        .text
        .p2align 2
entry:
        retl
        nop
        .data
        .p2align 2
        .word   entry
        .word   entry - .

"#,
        image: Some((
            16,
            &[(0, "81 c3 e0 08 01 00 00 00 00 10 00 00 ff ff ff f4")],
        )),
    },
    Case {
        name: "set of a label",
        base: 0x100000,
        src: r#"        .text
        .p2align 2
        set     msg, %g1
        set     msg + 0x400, %g2
        retl
        nop
        .data
        .p2align 2
        .space  0x2000
msg:    .word   1
"#,
        image: Some((
            8220,
            &[
                (
                    0,
                    "03 00 04 08 82 10 60 18 05 00 04 09 84 10 a0 18 81 c3 e0 08 01 00 00 00 00 00 00 00 00 00 00 00",
                ),
                (0x2010, "00 00 00 00 00 00 00 00 00 00 00 01"),
            ],
        )),
    },
];

#[cfg(feature = "sparc")]
#[test]
fn sparcv9_images_match_a_linked_reference() {
    check("sparcv9", SPARCV9);
}

/// Assembled with llvm-mc and GNU ld.
#[cfg(feature = "sparc")]
const SPARCV9: &[Case] = &[
    Case {
        name: "hi and lo of a label in another section",
        base: 0x100000,
        src: r#"        .text
        .p2align 2
        sethi   %hi(msg), %o0
        or      %o0, %lo(msg), %o0
        ld      [%o0 + %lo(msg)], %o1
        retl
        nop
        .data
        .p2align 2
        .space  0x1400
msg:    .word   1

"#,
        image: Some((
            5144,
            &[
                (
                    0,
                    "11 00 04 05 90 12 20 14 d2 02 20 14 81 c3 e0 08 01 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
                ),
                (0x1410, "00 00 00 00 00 00 00 01"),
            ],
        )),
    },
    Case {
        name: "hi and lo with an addend",
        base: 0x100000,
        src: r#"        .text
        .p2align 2
        sethi   %hi(msg + 0x3ff), %o0
        add     %o0, %lo(msg + 0x3ff), %o0
        sethi   %hi(msg - 1), %o1
        retl
        nop
        .data
        .p2align 2
msg:    .word   1

"#,
        image: Some((
            24,
            &[(
                0,
                "11 00 04 01 90 02 20 13 13 00 04 00 81 c3 e0 08 01 00 00 00 00 00 00 01",
            )],
        )),
    },
    Case {
        name: "call and branches into another section",
        base: 0x100000,
        src: r#"        .text
        .p2align 2
entry:
        call    helper
        nop
        ba      helper
        nop
        .section .text.helper, "ax"
        .p2align 2
helper:
        bne     entry
        nop

"#,
        image: Some((
            24,
            &[(
                0,
                "40 00 00 04 01 00 00 00 10 80 00 02 01 00 00 00 12 bf ff fc 01 00 00 00",
            )],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x100000,
        src: r#"        .text
        .p2align 2
entry:
        retl
        nop
        .data
        .p2align 2
        .word   entry
        .word   entry - .

"#,
        image: Some((
            16,
            &[(0, "81 c3 e0 08 01 00 00 00 00 10 00 00 ff ff ff f4")],
        )),
    },
    Case {
        name: "set of a label",
        base: 0x100000,
        src: r#"        .text
        .p2align 2
        set     msg, %g1
        set     msg + 0x400, %g2
        retl
        nop
        .data
        .p2align 2
        .space  0x2000
msg:    .word   1
"#,
        image: Some((
            8220,
            &[
                (
                    0,
                    "03 00 04 08 82 10 60 18 05 00 04 09 84 10 a0 18 81 c3 e0 08 01 00 00 00 00 00 00 00 00 00 00 00",
                ),
                (0x2010, "00 00 00 00 00 00 00 00 00 00 00 01"),
            ],
        )),
    },
];

#[cfg(feature = "m68k")]
#[test]
fn m68k_images_match_a_linked_reference() {
    check("m68k", M68K);
}

/// Assembled with GNU as and GNU ld.
#[cfg(feature = "m68k")]
const M68K: &[Case] = &[
    Case {
        name: "absolute references into another section",
        base: 0x10000,
        src: r#" .text
 .p2align 2
 lea msg,%a0
 movel #msg,%d0
 movel msg,%d1
 jsr sub
 rts
 .data
 .p2align 2
 .space 0x100
msg: .long 1
sub: rts

"#,
        image: Some((
            290,
            &[
                (
                    0,
                    "41 f9 00 01 01 1c 20 3c 00 01 01 1c 22 39 00 01 01 1c 4e b9 00 01 01 20 4e 75 00 00 00 00 00 00",
                ),
                (
                    0x110,
                    "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 01 4e 75",
                ),
            ],
        )),
    },
    Case {
        name: "pc-relative references into another section",
        base: 0x10000,
        src: r#" .text
 .p2align 2
 .cpu 68020
 lea (msg:l,%pc),%a1
 movew (msg:l,%pc),%d1
 bsrl sub
 rts
 .data
 .p2align 2
 .space 0x100
msg: .long 1
sub: rts

"#,
        image: Some((
            286,
            &[
                (
                    0,
                    "43 fb 01 70 00 00 01 16 32 3b 01 70 00 00 01 0e 61 ff 00 00 01 0a 4e 75 00 00 00 00 00 00 00 00",
                ),
                (0x110, "00 00 00 00 00 00 00 00 00 00 00 01 4e 75"),
            ],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x10000,
        src: r#" .text
 .p2align 2
entry:
 rts
 .data
 .p2align 2
 .long entry
 .long entry - .
 .word entry - .
"#,
        image: Some((14, &[(0, "4e 75 00 00 00 01 00 00 ff ff ff f8 ff f4")])),
    },
    Case {
        name: "sections start at their default alignment",
        base: 0x10000,
        src: r#"        .text
        .byte   1
        .data
        .byte   2
        .section .rodata
        .byte   3
"#,
        image: Some((6, &[(0, "01 00 00 00 02 03")])),
    },
];

#[cfg(feature = "superh")]
#[test]
fn sh_images_match_a_linked_reference() {
    check("sh", SH);
}

/// Assembled with GNU as and GNU ld.
#[cfg(feature = "superh")]
const SH: &[Case] = &[
    Case {
        name: "a literal pool pointing into another section",
        base: 0x10000,
        src: r#"        .text
        .p2align 2
        mov.l   lit, r0
        mov.l   @r0, r1
        rts
        nop
        .p2align 2
lit:    .long   msg
        .data
        .p2align 2
        .space  0x100
msg:    .long   1

"#,
        image: Some((
            272,
            &[
                (0, "d0 01 61 02 00 0b 00 09 00 01 01 0c 00 00 00 00"),
                (0x100, "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 01"),
            ],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x10000,
        src: r#"        .text
        .p2align 2
entry:
        rts
        nop
        .data
        .p2align 2
        .long   entry
        .long   entry - .
        .long   entry@PLT
        .long   entry@PCREL
"#,
        image: Some((
            20,
            &[(
                0,
                "00 0b 00 09 00 01 00 00 ff ff ff f8 ff ff ff f4 ff ff ff f0",
            )],
        )),
    },
];

#[cfg(feature = "superh")]
#[test]
fn shl_images_match_a_linked_reference() {
    check("shl", SHL);
}

/// Assembled with GNU as and GNU ld.
#[cfg(feature = "superh")]
const SHL: &[Case] = &[
    Case {
        name: "a literal pool pointing into another section",
        base: 0x10000,
        src: r#"        .text
        .p2align 2
        mov.l   lit, r0
        mov.l   @r0, r1
        rts
        nop
        .p2align 2
lit:    .long   msg
        .data
        .p2align 2
        .space  0x100
msg:    .long   1

"#,
        image: Some((
            272,
            &[
                (0, "01 d0 02 61 0b 00 09 00 0c 01 01 00 00 00 00 00"),
                (0x100, "00 00 00 00 00 00 00 00 00 00 00 00 01 00 00 00"),
            ],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x10000,
        src: r#"        .text
        .p2align 2
entry:
        rts
        nop
        .data
        .p2align 2
        .long   entry
        .long   entry - .
        .long   entry@PLT
        .long   entry@PCREL
"#,
        image: Some((
            20,
            &[(
                0,
                "0b 00 09 00 00 00 01 00 f8 ff ff ff f4 ff ff ff f0 ff ff ff",
            )],
        )),
    },
];

#[cfg(feature = "rx")]
#[test]
fn rx_images_match_a_linked_reference() {
    check("rx", RX);
}

/// Assembled with GNU as and GNU ld.
#[cfg(feature = "rx")]
const RX: &[Case] = &[
    Case {
        name: "absolute references into another section",
        base: 0x10000,
        src: r#"        .text
        mov     #msg, r1
        mov.l   #msg + 4, r2
        rts
        .data
        .space  0x100
msg:    .long   1

"#,
        image: Some((
            273,
            &[
                (0, "fb 12 0d 01 01 00 fb 22 11 01 01 00 02 00 00 00"),
                (0x100, "00 00 00 00 00 00 00 00 00 00 00 00 00 01 00 00"),
            ],
        )),
    },
    Case {
        name: "calls and branches into another section",
        base: 0x10000,
        src: r#"        .data
        .text
entry:
        bsr.a   helper
        bra.a   helper
        bra.w   helper
        .section .text.helper, "ax"
helper:
        bsr.a   entry
        bsr.w   entry
        rts

"#,
        image: Some((
            19,
            &[(
                0,
                "05 0b 00 00 04 07 00 00 38 03 00 05 f5 ff ff 39 f1 ff 02",
            )],
        )),
    },
    Case {
        name: "data across sections",
        base: 0x10000,
        src: r#"        .text
entry:
        rts
        .data
        .long   entry
        .long   entry - .
"#,
        image: Some((9, &[(0, "02 00 00 01 00 fb ff ff ff")])),
    },
];

#[cfg(feature = "rl78")]
#[test]
fn rl78_images_match_a_linked_reference() {
    check("rl78", RL78);
}

/// Assembled with GNU as and GNU ld.
#[cfg(feature = "rl78")]
const RL78: &[Case] = &[
    Case {
        name: "absolute references into another section",
        base: 0x2000,
        src: r#"        .data
        .text
        movw    ax, #msg
        movw    ax, !msg
        call    !!helper
        call    !helper
        ret
        .data
        .space  0x100
msg:    .word   1
        .section .text.helper, "ax"
helper:
        ret

"#,
        image: Some((
            275,
            &[
                (0, "30 0e 21 af 0e 21 fc 12 21 00 fd 12 21 d7 00 00"),
                (
                    0x100,
                    "00 00 00 00 00 00 00 00 00 00 00 00 00 00 01 00 00 00 d7",
                ),
            ],
        )),
    },
    Case {
        name: "relative calls and branches into another section",
        base: 0x2000,
        src: r#"        .text
entry:
        call    $!helper
        br      $!helper
        .section .text.helper, "ax"
helper:
        br      $!entry
        ret

"#,
        image: Some((10, &[(0, "fe 03 00 ee 00 00 ee f7 ff d7")])),
    },
    Case {
        name: "data across sections",
        base: 0x2000,
        src: r#"        .text
entry:
        ret
        .data
        .long   entry
        .long   entry - .
        .word   entry
"#,
        image: Some((13, &[(0, "d7 00 20 00 00 fb ff ff ff 00 20 00 00")])),
    },
];

#[cfg(feature = "v850")]
#[test]
fn v850_images_match_a_linked_reference() {
    check("v850", V850);
}

/// Assembled with GNU as and GNU ld.
#[cfg(feature = "v850")]
const V850: &[Case] = &[
    Case {
        name: "hi, lo and hi0 of a label in another section",
        base: 0x100000,
        src: r#"        .text
        .p2align 1
        movhi   hi(msg), r0, r1
        movea   lo(msg), r1, r1
        movhi   hi0(msg), r0, r2
        ori     lo(msg), r2, r2
        ld.w    lo(msg)[r1], r3
        jmp     [lp]
        .data
        .p2align 1
        .space  0x8010
msg:    .long   1

"#,
        image: Some((
            32810,
            &[
                (
                    0,
                    "40 0e 11 00 21 0e 26 80 40 16 10 00 82 16 26 80 21 1f 27 80 7f 00 00 00 00 00 00 00 00 00 00 00",
                ),
                (0x8020, "00 00 00 00 00 00 01 00 00 00"),
            ],
        )),
    },
    Case {
        name: "hi and lo where the low half is negative",
        base: 0x107ff0,
        src: r#"        .text
        .p2align 1
here:
        movhi   hi(there), r0, r1
        movea   lo(there), r1, r1
        movhi   hi(here), r0, r2
        movea   lo(here), r2, r2
        jmp     [lp]
there:
        jmp     [lp]

"#,
        image: Some((
            20,
            &[(
                0,
                "40 0e 11 00 21 0e 02 80 40 16 10 00 22 16 f0 7f 7f 00 7f 00",
            )],
        )),
    },
    Case {
        name: "jarl and jr into another section",
        base: 0x100000,
        src: r#"        .text
        .p2align 1
entry:
        jarl    helper, lp
        jr      helper
        .section .text.helper, "ax"
        .p2align 1
helper:
        jr      entry

"#,
        image: Some((12, &[(0, "80 ff 08 00 80 07 04 00 bf 07 f8 ff")])),
    },
    Case {
        name: "data across sections",
        base: 0x100000,
        src: r#"        .text
        .p2align 1
entry:
        jmp     [lp]
        .data
        .p2align 1
        .long   entry

"#,
        image: Some((6, &[(0, "7f 00 00 00 10 00")])),
    },
    Case {
        name: "refused: zdaoff of a label above 32 KiB",
        base: 0x100000,
        src: r#"        .text
        .p2align 1
        movea   zdaoff(msg), r0, r1
        jmp     [lp]
        .data
        .p2align 1
msg:    .long   1

"#,
        image: None,
    },
    Case {
        name: "zdaoff of a label in the bottom 32 KiB",
        base: 0x1000,
        src: r#"        .text
        .p2align 1
        movea   zdaoff(msg), r0, r1
        addi    zdaoff(msg), r1, r2
        jmp     [lp]
        .data
        .p2align 1
msg:    .long   1
"#,
        image: Some((14, &[(0, "20 0e 0a 10 01 16 0a 10 7f 00 01 00 00 00")])),
    },
];

#[cfg(feature = "v850")]
#[test]
fn rh850_images_match_a_linked_reference() {
    check("rh850", RH850);
}

/// Assembled with GNU as and GNU ld.
#[cfg(feature = "v850")]
const RH850: &[Case] = &[
    Case {
        name: "hi, lo and hi0 of a label in another section",
        base: 0x100000,
        src: r#"        .text
        .p2align 1
        movhi   hi(msg), r0, r1
        movea   lo(msg), r1, r1
        movhi   hi0(msg), r0, r2
        ori     lo(msg), r2, r2
        ld.w    lo(msg)[r1], r3
        jmp     [lp]
        .data
        .p2align 1
        .space  0x8010
msg:    .long   1

"#,
        image: Some((
            32810,
            &[
                (
                    0,
                    "40 0e 11 00 21 0e 26 80 40 16 10 00 82 16 26 80 21 1f 27 80 7f 00 00 00 00 00 00 00 00 00 00 00",
                ),
                (0x8020, "00 00 00 00 00 00 01 00 00 00"),
            ],
        )),
    },
    Case {
        name: "hi and lo where the low half is negative",
        base: 0x107ff0,
        src: r#"        .text
        .p2align 1
here:
        movhi   hi(there), r0, r1
        movea   lo(there), r1, r1
        movhi   hi(here), r0, r2
        movea   lo(here), r2, r2
        jmp     [lp]
there:
        jmp     [lp]

"#,
        image: Some((
            20,
            &[(
                0,
                "40 0e 11 00 21 0e 02 80 40 16 10 00 22 16 f0 7f 7f 00 7f 00",
            )],
        )),
    },
    Case {
        name: "jarl and jr into another section",
        base: 0x100000,
        src: r#"        .text
        .p2align 1
entry:
        jarl    helper, lp
        jr      helper
        .section .text.helper, "ax"
        .p2align 1
helper:
        jr      entry

"#,
        image: Some((12, &[(0, "80 ff 08 00 80 07 04 00 bf 07 f8 ff")])),
    },
    Case {
        name: "data across sections",
        base: 0x100000,
        src: r#"        .text
        .p2align 1
entry:
        jmp     [lp]
        .data
        .p2align 1
        .long   entry

"#,
        image: Some((6, &[(0, "7f 00 00 00 10 00")])),
    },
    Case {
        name: "refused: zdaoff of a label above 32 KiB",
        base: 0x100000,
        src: r#"        .text
        .p2align 1
        movea   zdaoff(msg), r0, r1
        jmp     [lp]
        .data
        .p2align 1
msg:    .long   1

"#,
        image: None,
    },
    Case {
        name: "zdaoff of a label in the bottom 32 KiB",
        base: 0x1000,
        src: r#"        .text
        .p2align 1
        movea   zdaoff(msg), r0, r1
        addi    zdaoff(msg), r1, r2
        jmp     [lp]
        .data
        .p2align 1
msg:    .long   1
"#,
        image: Some((14, &[(0, "20 0e 0a 10 01 16 0a 10 7f 00 01 00 00 00")])),
    },
    Case {
        name: "32-bit and 23-bit loads of a label",
        base: 0x100000,
        src: r#"        .text
        .p2align 1
        mov     hilo(msg), r1
        ld.b    lo23(msg)[r0], r2
        jmp     [lp]
        .data
        .p2align 1
        .space  0x100
msg:    .long   1
"#,
        image: Some((
            274,
            &[
                (0, "21 06 0e 01 10 00 80 07 e5 10 02 20 7f 00 00 00"),
                (0x100, "00 00 00 00 00 00 00 00 00 00 00 00 00 00 01 00"),
            ],
        )),
    },
];

#[cfg(feature = "avr")]
#[test]
fn avr_images_match_a_linked_reference() {
    check("avr", AVR);
}

/// Assembled with GNU as for its default core and linked with GNU ld.
#[cfg(feature = "avr")]
const AVR: &[Case] = &[
    Case {
        name: "relative branches in both directions",
        base: 0x0,
        src: r#"        .text
back:   nop
        breq    back
        brne    fwd
        brbs    3, back
        brbc    6, fwd
        rjmp    back
        rcall   fwd
        nop
fwd:    ret

"#,
        image: Some((
            18,
            &[(0, "00 00 f1 f3 29 f4 e3 f3 1e f4 fa cf 01 d0 00 00 08 95")],
        )),
    },
    Case {
        name: "the far ends of each branch's reach",
        base: 0x0,
        src: r#"        .text
start:  breq    far7
        .space  124
far7:   brne    start2
        .space  2
start2: nop
        rjmp    far13
        .space  4092
far13:  rcall   start
        nop

"#,
        image: Some((
            4230,
            &[
                (0, "f1 f1 00 00 00 00 00 00 00 00 00 00 00 00 00 00"),
                (
                    0x70,
                    "00 00 00 00 00 00 00 00 00 00 00 00 00 00 09 f4 00 00 00 00 fe c7 00 00 00 00 00 00 00 00 00 00",
                ),
                (0x1080, "00 00 be d7 00 00"),
            ],
        )),
    },
    Case {
        name: "data referring to code and to data",
        base: 0x0,
        src: r#"        .text
entry:  nop
        rjmp    entry
isr:    reti
        .data
        .byte   0
        .word   pm(isr), gs(entry)
        .word   entry, isr, var
        .long   isr, var
        .byte   lo8(isr), hi8(isr), hlo8(isr), hh8(isr)
        .byte   lo8(var), hi8(var)
        .long   entry - .
var:    .word   0

"#,
        image: Some((
            37,
            &[(
                0,
                "00 00 fe cf 18 95 00 02 00 00 00 00 00 04 00 23 00 04 00 00 00 23 00 00 00 04 00 00 00 23 00 e1 ff ff ff 00 00",
            )],
        )),
    },
    Case {
        name: "I/O addresses and small constants from symbols",
        base: 0x0,
        src: r#"        .text
        .set    PORTB, 0x18
        .set    SREG, 0x3f
        .set    off, 12
        in      r0, SREG
        out     PORTB, r1
        cbi     PORTB, 3
        sbi     PORTB + 1, 7
        sbic    PORTB - 2, 0
        adiw    r24, off
        sbiw    r28, off + 1
        ldd     r0, Y+off
        std     Z+off, r1
        lds     r0, var
        sts     var + 1, r2
        ret
        .data
var:    .word   0

"#,
        image: Some((
            30,
            &[(
                0,
                "0f b6 18 ba c3 98 cf 9a b0 99 0c 96 2d 97 0c 84 14 86 00 90 1c 00 20 92 1d 00 08 95 00 00",
            )],
        )),
    },
    Case {
        name: "a location counter in operands is past the instruction",
        base: 0x0,
        src: r#"        .text
        rjmp    .
        rjmp    . + 2
        breq    . - 4
        rcall   . + 0
        nop
        nop

"#,
        image: Some((12, &[(0, "00 c0 01 c0 f1 f3 00 d0 00 00 00 00")])),
    },
    Case {
        name: "alignment and .org between a branch and its target",
        base: 0x0,
        src: r#"        .text
        rjmp    1f
        nop
        .balign 8
1:      breq    2f
        .balign 16, 0xff
        .org    0x40
2:      ret

"#,
        image: Some((
            80,
            &[
                (0, "03 c0 00 00 00 00 00 00 d9 f0 ff ff ff ff ff ff"),
                (0x40, "08 95 00 00 00 00 00 00 00 00 00 00 00 00 00 00"),
            ],
        )),
    },
    Case {
        name: "a small device with no jmp: rjmp vectors and the 8K wrap-around",
        base: 0x0,
        src: r#"        .arch   attiny13
        .text
        rjmp    reset
        rjmp    reset
reset:  sbi     0x17, 0
loop:   sbi     0x18, 0
        cbi     0x18, 0
        sbiw    r24, 1
        brne    loop
        rjmp    reset

"#,
        image: Some((
            16,
            &[(0, "01 c0 00 c0 b8 9a c0 9a c0 98 01 97 e1 f7 fa cf")],
        )),
    },
    Case {
        name: "pm() of an odd address",
        base: 0x0,
        src: r#"        .text
        .data
        .byte   1
odd:    .byte   2
        .word   odd
        ldi     r16, lo8(odd)

"#,
        image: Some((6, &[(0, "01 02 01 00 01 e0")])),
    },
    Case {
        name: "refused: a branch out of reach of another section",
        base: 0x0,
        src: r#"        .text
        breq    other
        .section .text.far, "ax", @progbits
        .space  0x200
other:  ret

"#,
        image: None,
    },
    Case {
        name: "branches and calls between sections, in both directions",
        base: 0x0,
        src: r#"        .text
start:  rcall   helper
        breq    helper
        brcs    near
        rjmp    tail
        .section .text.helper, "ax", @progbits
helper: brne    start
near:   rjmp    start
tail:   ret

"#,
        image: Some((14, &[(0, "03 d0 11 f0 10 f0 02 c0 d9 f7 fa cf 08 95")])),
    },
    Case {
        name: "at a base address, with data after the code",
        base: 0x0,
        src: r#"        .text
entry:  ldi     r30, lo8(table)
        ldi     r31, hi8(table)
        ldi     r24, pm_lo8(entry)
        ldi     r25, pm_hi8(entry)
        ldi     r26, lo8(pm(isr))
        ldi     r27, hi8(pm(isr))
        rjmp    entry
isr:    reti
        .data
table:  .word   pm(entry), pm(isr), gs(isr)
        .word   table, entry
        .byte   lo8(table), hi8(table), hlo8(table)

"#,
        image: Some((
            29,
            &[(
                0,
                "e0 e1 f0 e0 80 e0 90 e0 a7 e0 b0 e0 f9 cf 18 95 00 00 07 00 07 00 10 00 00 00 10 00 00",
            )],
        )),
    },
    Case {
        name: "the same at a high base",
        base: 0x1f00,
        src: r#"        .text
entry:  ldi     r30, lo8(table)
        ldi     r31, hi8(table)
        ldi     r24, pm_lo8(entry)
        ldi     r25, pm_hi8(entry)
        ldi     r20, lo8(-(table))
        ldi     r21, hi8(-(table))
        ldi     r22, pm_lo8(-(isr))
        ldi     r23, pm_hi8(-(isr))
        rcall   isr
isr:    reti
        .data
table:  .word   pm(entry), pm(isr)
        .long   table, isr

"#,
        image: Some((
            32,
            &[(
                0,
                "e4 e1 ff e1 80 e8 9f e0 4c ee 50 ee 67 e7 70 ef 00 d0 18 95 80 0f 89 0f 14 1f 00 00 12 1f 00 00",
            )],
        )),
    },
    Case {
        name: "ldi modifiers on sums and differences",
        base: 0x0,
        src: r#"        .text
a:      ldi     r16, lo8(b - a)
        ldi     r17, hi8(b + 0x100)
        ldi     r18, lo8(pm(b + 2))
        ldi     r19, lo8(-(b - 2))
        subi    r30, lo8(-(b))
        sbci    r31, hi8(-(b))
        .space  0x300
b:      ret

"#,
        image: Some((
            782,
            &[
                (0, "0c e0 14 e0 27 e8 36 ef e4 5f fc 4f 00 00 00 00"),
                (0x300, "00 00 00 00 00 00 00 00 00 00 00 00 08 95"),
            ],
        )),
    },
    Case {
        name: "a data word referring to a label at an odd address",
        base: 0x0,
        src: r#"        .data
        .byte   1
odd:    .byte   2
        .word   odd
        .long   odd + 1

"#,
        image: Some((8, &[(0, "01 02 01 00 02 00 00 00")])),
    },
    Case {
        name: "refused: pm() of a label at an odd address",
        base: 0x0,
        src: r#"        .data
        .byte   1
odd:    .byte   2
        .word   pm(odd)

"#,
        image: None,
    },
    Case {
        name: "rcall out of reach wraps around, as GNU ld wraps it for avr2, avr25 and avr4",
        base: 0x0,
        src: r#"        .text
        rcall   far
        .space  0x1000
far:    ret
"#,
        image: Some((
            4100,
            &[
                (0, "00 d8 00 00 00 00 00 00 00 00 00 00 00 00 00 00"),
                (0x1000, "00 00 08 95"),
            ],
        )),
    },
];

#[cfg(feature = "avr")]
#[test]
fn avr5_images_match_a_linked_reference() {
    check("avr5", AVR5);
}

/// Assembled with GNU as `-mmcu=avr5` and linked with GNU ld `-m avr5`.
#[cfg(feature = "avr")]
const AVR5: &[Case] = &[
    Case {
        name: "an interrupt vector table",
        base: 0x0,
        src: r#"        .section .vectors, "ax", @progbits
        .globl  __vectors
__vectors:
        jmp     __init
        jmp     __bad_interrupt
        jmp     __vector_2
        jmp     __bad_interrupt
        .text
__init: clr     r1
        out     0x3f, r1
        ldi     r28, lo8(0x08ff)
        ldi     r29, hi8(0x08ff)
        out     0x3e, r29
        out     0x3d, r28
        call    main
        jmp     _exit
__bad_interrupt:
        jmp     __vectors
__vector_2:
        push    r0
        in      r0, 0x3f
        push    r0
        pop     r0
        out     0x3f, r0
        pop     r0
        reti
main:   ldi     r24, lo8(msg)
        ldi     r25, hi8(msg)
        rcall   puts
        ret
puts:   movw    r30, r24
1:      ld      r24, Z+
        tst     r24
        breq    2f
        rjmp    1b
2:      ret
_exit:  cli
3:      rjmp    3b
        .section .progmem.data, "a", @progbits
msg:    .asciz  "hello"

"#,
        image: Some((
            84,
            &[(
                0,
                "11 24 1f be cf ef d8 e0 de bf cd bf 0e 94 13 00 0c 94 1d 00 0c 94 1f 00 0f 92 0f b6 0f 92 0f 90 0f be 0f 90 18 95 8e e4 90 e0 01 d0 08 95 fc 01 81 91 88 23 09 f0 fc cf 08 95 f8 94 ff cf 0c 94 00 00 0c 94 0a 00 0c 94 0c 00 0c 94 0a 00 68 65 6c 6c 6f 00",
            )],
        )),
    },
    Case {
        name: "refused: rjmp out of reach on a device with more than 8K",
        base: 0x0,
        src: r#"        .text
        rjmp    far
        .space  0x1000
far:    ret
"#,
        image: None,
    },
];

#[cfg(feature = "avr")]
#[test]
fn avr51_images_match_a_linked_reference() {
    check("avr51", AVR51);
}

/// Assembled with GNU as `-mmcu=avr51` and linked with GNU ld `-m avr51`.
#[cfg(feature = "avr")]
const AVR51: &[Case] = &[
    Case {
        name: "jmp and call into another section",
        base: 0x0,
        src: r#"        .text
        .globl  _start
_start: jmp     main
        call    helper
        call    far + 2
        .section .text.helper, "ax", @progbits
helper: ret
main:   jmp     _start
far:    nop
        ret

"#,
        image: Some((
            22,
            &[(
                0,
                "0c 94 07 00 0e 94 06 00 0e 94 0a 00 08 95 0c 94 00 00 00 00 08 95",
            )],
        )),
    },
    Case {
        name: "ldi on every byte of a code and a data address",
        base: 0x0,
        src: r#"        .text
start:  ldi     r16, lo8(var)
        ldi     r17, hi8(var)
        ldi     r18, hh8(var)
        ldi     r19, hlo8(var)
        ldi     r20, hhi8(var)
        ldi     r21, lo8(-(var))
        ldi     r22, hi8(-(var))
        ldi     r23, hh8(-(var))
        ldi     r24, hhi8(-(var))
        ldi     r30, pm_lo8(func)
        ldi     r31, pm_hi8(func)
        ldi     r26, pm_hh8(func)
        ldi     r27, lo8(pm(func))
        ldi     r28, hi8(pm(func))
        ldi     r29, hh8(pm(func))
        ldi     r30, lo8(gs(func))
        ldi     r31, hi8(gs(func))
        ldi     r16, pm_lo8(-(func))
        ldi     r17, pm_hi8(-(func))
        ldi     r18, lo8(-(pm(func)))
        andi    r20, lo8(var + 3)
        cpi     r21, hi8(var - 1)
        ret
        .space  0x1234
func:   ret
        .data
        .space  0x52
var:    .byte   1

"#,
        image: Some((
            4791,
            &[
                (
                    0,
                    "06 eb 12 e1 20 e0 30 e0 40 e0 5a e4 6d ee 7f ef 8f ef e1 e3 f9 e0 a0 e0 b1 e3 c9 e0 d0 e0 e1 e3 f9 e0 0f ec 16 ef 2f ec 49 7b 52 31 08 95 00 00",
                ),
                (0x1260, "00 00 08 95 00 00 00 00 00 00 00 00 00 00 00 00"),
                (0x12b0, "00 00 00 00 00 00 01"),
            ],
        )),
    },
    Case {
        name: "refused: an ldi constant that is an address above 255",
        base: 0x0,
        src: r#"        .text
        ldi     r19, var
        .space  0x200
var:    ret

"#,
        image: None,
    },
    Case {
        name: "jmp and call beyond 64K",
        base: 0x0,
        src: r#"        .text
        jmp     high
        call    high + 2
        .space  0x12340
high:   ret
        nop

"#,
        image: Some((
            74572,
            &[
                (0, "0c 94 a4 91 0e 94 a5 91 00 00 00 00 00 00 00 00"),
                (0x12340, "00 00 00 00 00 00 00 00 08 95 00 00"),
            ],
        )),
    },
    Case {
        name: "the third byte of an address beyond 64K",
        base: 0x0,
        src: r#"        .text
        ldi     r16, hh8(high)
        ldi     r17, hh8(pm(high))
        ldi     r18, pm_hh8(high)
        ldi     r19, hlo8(high)
        ldi     r20, hh8(-(high))
        ldi     r21, pm_hh8(-(high))
        ret
        .space  0x2a000
high:   ret

"#,
        image: Some((
            172048,
            &[
                (0, "02 e0 11 e0 21 e0 32 e0 4d ef 5e ef 08 95 00 00"),
                (0x2a000, "00 00 00 00 00 00 00 00 00 00 00 00 00 00 08 95"),
            ],
        )),
    },
    Case {
        name: "refused: breq out of reach",
        base: 0x0,
        src: r#"        .text
        breq    far
        .space  0x80
far:    ret
"#,
        image: None,
    },
];

#[cfg(feature = "avr")]
#[test]
fn avr6_images_match_a_linked_reference() {
    check("avr6", AVR6);
}

/// Assembled with GNU as `-mmcu=avr6` and linked with GNU ld `-m avr6 --no-stubs`.
#[cfg(feature = "avr")]
const AVR6: &[Case] = &[Case {
    name: "jmp and call at a base address",
    base: 0x0,
    src: r#"        .text
        jmp     target
        call    target + 4
        .space  0x100
target: ret
        nop
        nop

"#,
    image: Some((
        270,
        &[
            (0, "0c 94 84 00 0e 94 86 00 00 00 00 00 00 00 00 00"),
            (0x100, "00 00 00 00 00 00 00 00 08 95 00 00 00 00"),
        ],
    )),
}];

#[cfg(feature = "avr")]
#[test]
fn avrtiny_images_match_a_linked_reference() {
    check("avrtiny", AVRTINY);
}

/// Assembled with GNU as `-mmcu=avrtiny` and linked with GNU ld `-m avrtiny`.
#[cfg(feature = "avr")]
const AVRTINY: &[Case] = &[
    Case {
        name: "AVR-tiny direct load and store",
        base: 0x0,
        src: r#"        .text
        .set    var, 0x60
        lds     r16, var
        sts     var + 1, r17
        lds     r31, 0xbf
        ret

"#,
        image: Some((8, &[(0, "00 a5 11 ad ff a6 08 95")])),
    },
    Case {
        name: "refused: rcall out of reach, which GNU ld does not wrap for this core",
        base: 0x0,
        src: r#"        .text
        rcall   far
        .space  0x1000
far:    ret
"#,
        image: None,
    },
];
