# rsasm

[![CI](https://github.com/KarpelesLab/rsasm/actions/workflows/ci.yml/badge.svg)](https://github.com/KarpelesLab/rsasm/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/rsasm.svg)](https://crates.io/crates/rsasm)
[![docs.rs](https://img.shields.io/docsrs/rsasm)](https://docs.rs/rsasm)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

An assembler written in Rust, aiming at three things at once: accept the asm
text people actually have, target many CPUs, and let one source file emit code
for more than one of them.

```console
$ cat hello.s
        .section .rodata
msg:    .ascii  "Hello from rsasm!\n"
msglen = . - msg

        .text
        .globl  _start
_start:
        movq    $1, %rax                # write
        movq    $1, %rdi                # stdout
        leaq    msg(%rip), %rsi
        movq    $msglen, %rdx
        syscall
        movq    $60, %rax               # exit
        xorq    %rdi, %rdi
        syscall

$ rsasm -o hello.o hello.s && ld -o hello hello.o && ./hello
Hello from rsasm!
```

## Install

```console
$ cargo install rsasm
```

Prebuilt binaries for Linux, macOS and Windows are attached to each
[release](https://github.com/KarpelesLab/rsasm/releases).

## Status

Early, but broad. The pipeline is complete end to end — lex, parse, encode,
lay out, relax, relocate, write — with sixteen backends behind it.

### Architectures

Every encoding claimed below is checked byte for byte against an independent
assembler, not against rsasm's own idea of the manual. See
[Verification](#verification).

| Target | Names | Checked against | Cases |
|---|---|---|---|
| x86-64, i386, i8086, with x87, MMX, 3DNow!, SSE–SSE4.2, AVX, AVX2, AVX-512 with every subset and FP16, AVX10.2, FMA4, XOP, BMI, AMX, CET, Key Locker | `x86-64` `i386` `i8086` | GNU as, llvm-mc | 17055 |
| AArch64, with AdvSIMD (NEON), the cryptographic extensions, SVE and SVE2, the system instructions and literal pools | `aarch64` | llvm-mc, GNU as | 21730 |
| ARM A32 / Thumb, with the floating-point unit (VFPv4) and NEON | `arm` `thumb` | llvm-mc, GNU as | 2952 |
| RISC-V RV32/RV64 IMAFDC | `riscv32` `riscv64` | llvm-mc | 530 |
| PowerPC 32/64, both endians, with AltiVec, VSX and POWER8–10 | `powerpc` `powerpc64` `powerpc64le` | llvm-mc, GNU as | 9488 |
| MIPS 32/64, both endians | `mips` `mipsel` `mips64` `mips64el` | llvm-mc | 669 |
| SPARC V8 / V9 | `sparc` `sparcv9` | llvm-mc | 190 |
| m68k: 68000–68060, CPU32, 68881/68882, 68851, ColdFire, GNU and Motorola syntax | `m68k` `68000` … `68060` `cpu32` `5475` … | GNU as, vasm | 3744 |
| SuperH SH-1 to SH-4A, both endians | `sh` `shl` | GNU as | 1280 |
| Renesas RX (RXv1), GNU and CC-RX syntax | `rx` | GNU as | 609 |
| Renesas RL78, GNU and CC-RL syntax | `rl78` | GNU as | 528 |
| TI MSP430 and MSP430X | `msp430` `msp430x` `msp430xv2` | GNU as | 3699 |
| NEC/Renesas V850 and RH850, GNU and CC-RH syntax | `v850` `rh850` | GNU as | 558 |
| NEC 78K0, in CA78K0 syntax | `78k0` | NEC code tables, MAME | — |
| Microchip AVR, every core GNU as knows | `avr` `avr1`–`avr6` `avrxmega2`–`avrxmega7` `avrtiny` | GNU as | 1935 |
| Zilog Z80, with the undocumented `IXH`/`IXL` forms, Zilog and GNU syntax | `z80` | GNU as, vasm | 2572 |
| MOS 6502, in ca65 syntax | `6502` | ca65, vasm | 551 |
| Intel 8080, in Intel mnemonics | `i8080` | AS | 278 |
| Intel 8051 (MCS-51), in Intel mnemonics | `8051` | AS, sdas8051 | 960 |

The 8-bit targets are checked against the assemblers their source is written
for: cc65's ca65 for the 6502, GNU as and vasm for the Z80, and the Macro
Assembler AS for the 8080 — GNU as has no Intel mnemonics, and vasm's `RST`
takes a Zilog address — and for the 8051, AS and SDCC's sdas8051. Tests also
walk each complete opcode space and assert that exactly the documented
encodings exist. They are for flat binaries, or Intel HEX; ELF has no class
for a 16-bit target. See [the 8-bit dialect](#the-8-bit-dialect).
The 78K0 has no freely available assembler: its table was extracted
from NEC's instruction manual, checked against the byte counts in a second NEC
manual, and cross-checked against MAME's disassembler, which agrees on all
but 18 forms where both manuals show MAME to be wrong.

### Everything else

**Working**

- AT&T and Intel syntax on x86, switchable mid-file; `.code16`/`.code32`/`.code64`
  and `.code16gcc`
- NASM source (`-d nasm`): its preprocessor (`%macro`, `%rep`, `%if`, `%define`,
  `%assign`, contexts, `%include`), `db`/`resb`/`times`/`equ`/`struc`, sections
  with attributes, `default rel`, and NASM's operand syntax and `wrt`
  relocations; see [Dialects](#dialects)
- several targets in one file, switched with `.arch`; see
  [Multi-architecture files](#multi-architecture-files)
- ELF relocatable objects, 32- and 64-bit, REL or RELA as each psABI requires,
  flat binaries, and flat images as Intel HEX (`-f ihex`)
- PE/COFF relocatable objects for x86-64, i386 and ARM64 (`-f coff`, or NASM's
  `-f win64` and `-f win32`): COMDAT sections, weak externals, `.def`, `.rva`,
  `.secrel32` and `@IMGREL`, and x86-64 unwind data from `.seh_*`; see
  [PE/COFF](#pecoff)
- Mach-O relocatable objects for x86-64 and arm64 (`-f macho`, or a Darwin
  triple such as `-a arm64-apple-macos`), with Darwin's section, symbol and
  data-in-code directives, byte for byte as llvm-mc writes them; see
  [Mach-O objects](#mach-o-objects)
- branch relaxation, alignment, `.org`, symbol arithmetic, conditionals
- macros: `.macro` with defaults, `:req` and `:vararg`, plus `.rept`, `.irp`,
  `.irpc`, `.exitm` and `.purgem`
- DWARF: line tables from `.file` and `.loc`, versions 2 to 5, call frame
  information from `.cfi_*` in `.eh_frame` or `.debug_frame`, and `-g` to
  describe the assembly source itself; see [Debug information](#debug-information)
- each target's own comment syntax, so ARM's `@`, AArch64's `//` and SPARC's
  `!` work, and `#` stays an immediate prefix where it is one
- the x86 instruction-set extensions both GNU as and llvm-mc assemble: AVX-512
  and all its subsets (BW, DQ, CD, IFMA, VBMI, VBMI2, VNNI, BITALG, VPOPCNTDQ,
  VP2INTERSECT, BF16, FP16, ER, PF) with writemasks, `{z}`, `{1toN}`, rounding
  and disp8\*N at every tuple type, AVX10.2, the VEX additions (F16C, FMA,
  GFNI, VAES, VPCLMULQDQ, SHA, SHA512, SM3, SM4, AVX-VNNI, AVX-IFMA,
  AVX-NE-CONVERT, AVX-VNNI-INT8/16), FMA4 and XOP, BMI1/2, TBM, LWP, AMX, CET,
  Key Locker, and the newer system instructions; the named compare predicates
  (`vcmpneq_oqps`, `vpcmpnltuq`), AT&T length spellings (`vcvtpd2psx`) and the
  `{vex}`, `{vex3}` and `{evex}` pseudo-prefixes
- ARM and Thumb as GNU as assembles them: literal pools (`ldr r0, =x`,
  `.ltorg`), `adr` and `adrl`, `it` blocks, `.thumb_func` and calls between
  the two instruction sets, and `$a`/`$t`/`$d` mapping symbols; the whole
  ARMv7-A/R/M instruction set with the security, virtualization and divide
  extensions, and with it the floating-point unit up to VFPv4 and NEON --
  the vector arithmetic over `d` and `q` registers, the shifts, the widening
  and narrowing forms, scalars and lanes, `vmov`'s modified immediate with
  the `cmode` GNU as picks for it, `vldm`/`vstm`/`vpush`/`vpop`, and the
  `vld1`-`vld4` and `vst1`-`vst4` structure transfers with their lists,
  alignments and lane indices
- AArch64 the same way: literal pools (`ldr x0, =0x123456789`, `ldr w0, =sym`,
  `.ltorg`, `.pool`) with `$x`/`$d` mapping symbols, and the system
  instructions with every operand name GNU as knows — `dc`, `ic`, `at`,
  `tlbi`, `sys`/`sysl`, 1,619 `mrs`/`msr` registers and the PSTATE fields.
  Mapping symbols are an ELF convention, so a COFF or Mach-O object has none
  of them, and nothing raises a section's alignment for them there
- the whole 680x0 family as GNU as knows it: the 68881/68882 FPU with float
  immediates in every size (`#1.5` in Motorola source, `#0r1.5` in GNU's), the
  68851 and on-chip MMUs, `cas2`, `callm`, `move16`, CPU32 and ColdFire,
  chosen by GNU as's CPU names (`-a 68040`, `.arch 5475`, `.arch 68000,68881`),
  with what the chosen CPU lacks refused by a message naming what it needs
- AVR as `avr-elf-as` assembles it: each core's own instruction set, chosen
  by family (`avr5`) or by device (`atmega328p`) with `-a` or `.arch`; the
  `lo8()`/`hi8()`/`pm()`/`gs()` modifiers in instructions and data; and
  objects prepared for linker relaxation, with every branch relocated, local
  labels in the relocations, `EF_AVR_LINKRELAX_PREPARED` in `e_flags`, and
  `.align` and `.org` in code recorded in `.avr.prop`
- MSP430 objects as GNU as writes them for a linker that relaxes code: every
  reference from code relocated, differences of code labels as
  `R_MSP430_SYM_DIFF` pairs (in the line table too), the `.MSP430.attributes`
  section and the `__crt0_*` references; and GNU as's polymorphic branches
  (`beq`, `bgt`, `jump`, …) in their long form
- diagnostics with source snippets that name the real limit, and assembly that
  continues past the first error

**Not yet**

- in NASM source: the multi-pass immediate-size optimizer for a value known
  only after layout, so `mov r64, len` where `len` is a label difference stays
  the sign-extending form rather than NASM's shorter 32-bit load (a constant or
  a symbol is optimized); x87, `enter`, far direct `jmp`/`call seg:off`, `[rip]`
  addressing (NASM uses `[rel]`), the `..gotpc`/`..gotoff`/`..tlsie` `wrt`
  targets and 16-bit object formats; `-f bin` follows NASM except that a
  trailing `.bss` is written as zeros rather than trimmed
- in CC-RL and CC-RH source: bit symbols, `$label`/`%label` gp- and
  ep-relative references, `STARTOF`/`SIZEOF`, and CC-RL's `HIGH`/`LOWW` of a
  relocatable label (all refused with the reason)
- in CC-RX source: `.FLOAT`/`.DOUBLE`, `.RVECTOR`, the `.LEN`/`.INSTR`/`.SUBSTR`
  string functions, `SIZEOF`/`TOPOF`, `__PID_REG`, big-endian sections, and
  bit length specifiers that ask for a longer form than the shortest (all
  refused with the reason)
- in PE/COFF objects: DWARF (`-g`, `.loc` and `.cfi_*` are refused with
  `-f coff`) and CodeView debug information, unwind data for ARM64 (its
  `.seh_*` directives are refused), i386 `.safeseh`, and unwind data for a
  function in a COMDAT section, which needs `.xdata` and `.pdata` sections
  associated with it (refused)
- in Mach-O objects: 32-bit machines (i386, armv7), thread-local variables
  (`@TLVP`, `@TLVPPAGE`), DWARF and call frame information (`-g`, `.loc` and
  `.cfi_*` are refused, and with them compact unwind), indirect symbol tables
  (`.indirect_symbol`), `LC_VERSION_MIN_*` and linker options
- x86: APX (`r16`–`r31`, REX2, the NDD and `{nf}` forms, `push2`/`pop2`,
  `ccmp`/`ctest`), the Xeon Phi 4FMAPS and 4VNNIW register-group
  instructions, the `{disp8}`/`{disp32}`/`{load}`/`{store}` pseudo-prefixes,
  and SGX, VMX, SVM, MPX and VIA PadLock
- DWARF: 64-bit DWARF, compressed debug sections, the `.cfi_*` directives
  beyond the common set (`.cfi_label`, `.cfi_val_encoded_addr`,
  `.cfi_inline_lsda`, `.cfi_fde_data` and llvm-mc's `.cfi_llvm_*`), and
  `.debug_macro`/`.debug_names`
- ARM: `-mimplicit-it`, so a conditional Thumb instruction needs an `it` block
  of its own, as with GNU as's default; `.thumb_set`; 8-byte (VFP) literal
  pool entries; and the divided Thumb syntax GNU as reads without
  `.syntax unified` (rsasm reads Thumb as unified syntax either way)
- ARM vectors: the floating-point immediate of `vmov.f32 s0, #1.0` and
  `vmov.f64 d0, #0.5`, which needs a literal this assembler's GAS-dialect
  lexer does not read (`vcmp.f32 s0, 0` against the integer zero works);
  half-precision arithmetic (the conversions `vcvt.f16.f32`, `vcvtb` and
  `vcvtt` are there); and everything past ARMv7 and VFPv4 -- the ARMv8-A
  additions (`vrint`, `vcvta`/`vcvtn`/`vcvtp`/`vcvtm`, `vmaxnm`, `vsel`, the
  cryptographic and CRC instructions), ARMv8-M's security extension,
  ARMv8.1-M's low-overhead loops and MVE, the custom datapath extension and
  PACBTI, and the M-profile special registers of `vmrs`/`vmsr`
- AArch64: SME beyond `smstart`, `smstop` and `zero {za}` — the ZA array and
  its tiles, `zt0`, the multi-vector and strided register lists, predicates as
  counters and `psel`; and the general-purpose instructions no SIMD mnemonic
  shares, which have never been there: the atomics (`ldxr`, `casp`,
  `ldadd`…), memory tagging, and the pointer-authentication instructions that
  name a register (`pacia x0, x1`) rather than the `paciasp`-style hints,
  which are there. `movprfx` is assembled but its sequence is not checked,
  where llvm-mc refuses an instruction that does not use the prefixed
  register and GNU as warns
- PowerPC: POWER10's matrix-multiply accelerator (`xvi8ger4` and the other
  MMA instructions), POWER11's `xxaes*` and `xxgfmul128*`, decimal floating
  point, the quadword `lqarx`, `stqcx.`, `plq` and `pstq`, the `bctar`
  branches, and the privileged, hypervisor, cache-hint and synchronisation
  instructions POWER8–10 added (`stop`, `slbieg`, `hashst`, `mfdscr` and the
  like); relocation modifiers other than `@l`, `@h`, `@ha`, `@pcrel` and
  `@got@pcrel` (`@toc@ha`, `@got`, `@higher`)
- MSP430: the large memory model (`-ml`), the interrupt-state `NOP`
  warnings and insertion, the silicon errata options, assembly-time
  relaxation (`-mQ`), and `.profiler`, `.refsym` and `.cpu`
- RISC-V: linker relaxation (`.option relax` is accepted, but objects come out
  as llvm-mc writes them without it, with no `R_RISCV_RELAX` or
  `R_RISCV_ALIGN`), and the TLS forms `la.tls.ie`, `la.tls.gd` and the
  `%tls_*` and `%got_pcrel_hi` modifiers
- 6502: the 65C02 and later instruction sets; in ca65 source, cheap local
  (`@loop`) and unnamed (`:`, `:-`) labels, `.proc`/`.scope`, `.struct`, and
  the `ZEROPAGE` segment's zero-page addressing for labels defined in it
- 8080: Intel's word operators (`AND`, `SHR`, `HIGH`, `MOD`), which AS does
  not read either
- 8051: the 8052's timer 2 names and the extended parts (80C320, 80C390,
  80251 and the rest); address spaces for `DATA`, `BIT`, `CODE` and the other
  defining words, which define plain values; and ASM51's controls
  (`$MOD51`, `$NOMOD51`), segments and relocatable output
- m68k: the ColdFire MAC and EMAC units; the suppressed registers `zpc`,
  `za0`-`za7` and `zd0`-`zd7`, and FPU coprocessor numbers other than 1
  (`.fopt id=`); vasm's `MACHINE`, `FPU` and `CHIP` directives (use `.arch`);
  vasm's sized `fbcc.w`, which GNU as does not take either (`fbcc` is 16
  bits, `fbcc.l` 32); and CPU32's `tbl*` table lookups, for which no reference
  here has an encoding. On ColdFire, an instruction as written that the core
  dropped is refused where GNU as substitutes one it kept (`addil #5,%a0@`,
  which GNU as writes as `addql`), since rsasm substitutes nothing
- Z80: the `DD CB d op,r` forms that also write a register, which vasm
  refuses; and in the GNU dialect, GNU as's `db`/`dw`/`ds` pseudo-ops (use
  `.byte`, `.word` and `.space`, or the 8-bit dialect)

- AVR: the `__gcc_isr` pseudo-instruction (`-mgcc-isr`), and Atmel's own
  AVRASM2 syntax, for which there is no free assembler to check against;
  `.arch` selects exactly the named core, where GNU as adds its instructions
  to those of an earlier one of the same machine

**Known wrong**

Anything that produces incorrect output rather than an error is listed here,
separately:

- `.lcomm`, and `.comm` of a symbol declared `.local`, write a local common
  symbol rather than allocating the space in `.bss` as both references do.

Where the references themselves disagree, rsasm follows the one whose harness
checks the target (see [Verification](#verification)) and says so in the
backend. Three such choices are worth knowing about:

- **Which references are left to the linker.** A PC-relative reference to a
  global or weak symbol is relocated even when the symbol is in the same
  section, since the linker may bind the name elsewhere; a local one, or a
  local `.set` alias of a global one, is resolved. That is what both
  references do on nearly every target. The exceptions follow GNU as for
  x86, m68k, SuperH, RL78, AVR and MSP430 (a jump GNU as relaxes to a global
  symbol is resolved on x86; only weak symbols are left to the linker on
  m68k; nothing in the same section is on SuperH and RL78; and everything is
  on AVR and MSP430, whose linkers may delete code between a branch and its
  target). On ARM GNU as is followed for
  whole objects: a `bl` to a local label is resolved, and made a `blx` where
  the label is a Thumb function, where llvm-mc relocates every `bl`.
- **Default section alignment.** Sections start with the alignment the
  reference gives them: 16 for MIPS `.text`, `.data` and `.bss` (llvm-mc; GNU
  as aligns only `.text`, to 4), 4 for `.text` on PowerPC and SPARC and for
  every executable section on AArch64 (llvm-mc; GNU as gives 1, or aligns
  once an instruction is assembled), on ARM what GNU as gives (a section is
  aligned by the first instruction in it, to 4 for ARM and 2 for Thumb), 2 or 4 for RISC-V `.text` depending on
  compressed instructions, 4 for m68k `.text`, `.data` and `.bss`, and 1
  otherwise, including on x86, where GNU as is followed and llvm-mc's `.text`
  is 4.
- **m68k floating-point immediates.** Both references write a single or
  double precision `#1.5` the same way. An extended-precision one GNU as 2.47
  writes without the 16 zero bits of the 68881 format — its own `.extend`
  directive and disassembler have them — and a packed-decimal one it refuses;
  vasm writes both correctly, from a C `double`, and so does rsasm. The m68k
  backend follows GNU as otherwise.

## Usage

```
rsasm [options] <input.s>...

  -o <file>          write output to <file> (default: a.out)
  -a, --arch <name>  target architecture (default: the host, if supported),
                     or a target triple: `x86_64-apple-macos` also picks
                     Mach-O output, `x86_64-pc-windows-msvc` PE/COFF
  -f, --format <fmt> output format: elf (default), elf32, elf64, coff,
                     win64, win32, macho, bin or ihex
  -s, --syntax <s>   initial operand syntax: att (default) or intel
  -d, --dialect <d>  source dialect: gas, nasm, motorola, renesas (CA78K0),
                     ccrl (Renesas CC-RL), ccrh (Renesas CC-RH),
                     ccrx (Renesas CC-RX) or 8bit (6502, Z80, 8080, 8051)
                     (default: the architecture's usual one)
  -I <dir>           add <dir> to the .include search path
  -D <sym>[=<val>]   define <sym> before assembling
      --base <addr>  base address for `bin` and `ihex` output
      --hex          print the output as hex instead of writing a file
  -g                 describe the assembly source in DWARF line information
      --gdwarf-<n>   the same, as DWARF version <n> (2 to 5); the version
                     also applies to `.loc` source
      --list-arch    list the architectures this build supports
```

Architectures are cargo features, all on by default — `x86`, `aarch64`, `arm`,
`riscv`, `powerpc`, `mips`, `sparc`, `retro` (the Z80, 6502, 8080 and 8051),
`m68k`, `superh`, `rx`, `rl78`, `v850`, `k78`, `avr` and `msp430`:

```console
$ cargo build --no-default-features --features x86,aarch64
```

### As a library

```rust
use rsasm::assembler::{Assembler, Options};
use rsasm::lexer::Dialect;
use rsasm::section::SectionId;
use rsasm::{arch, output};

let options = Options::new()
    .with_dialect(Dialect::Gas)
    .with_include_path("include");

let mut asm = Assembler::new(arch::lookup("x86-64").unwrap(), options);
asm.assemble_str("example.s", "movq %rbx, %rax\nret\n");
if !asm.finish() || asm.diags().has_errors() {
    eprint!("{}", asm.diags().render(asm.source_map(), false));
} else {
    let text = asm.section_bytes(SectionId(0));
    let object = output::elf::build(&asm).unwrap();
}
```

Only a small part of the crate is API: `Assembler` and the methods that drive
it, `Options` and its `with_*` builders, `output::Format` and the writers'
`build`, `arch::lookup` with the `Architecture` trait, `section::SectionId`,
the diagnostics types and `lexer::Dialect`. Everything else — the opcode
tables, the operand parsers, the expression arena, the macro engine, the
layout — is an implementation detail and is not documented on docs.rs; see
"What is public API" there for the exact list. `Options` and the other types
that will keep growing are `#[non_exhaustive]`, so build them with their
constructors rather than with a struct literal.

## Dialects

Source syntax is chosen with `-d`, or defaults to what each architecture's
source is normally written in.

| Dialect | Looks like | Default for |
|---|---|---|
| `gas` | `.byte 1`, `# comment`, `0x10` | most targets |
| `motorola` | `dc.b 1`, `; comment`, `$10`, `%1010` | m68k |
| `renesas` | `DB 'A',1`, `; comment`, `10H` | 78K0 |
| `ccrl` | `.DB "A",1`, `$IF`, `0x10` or `10H` (Renesas CC-RL) | — |
| `ccrh` | `.dw #label`, `$IF`, `0x10` (Renesas CC-RH) | — |
| `ccrx` | `.SECTION P,CODE`, `.LWORD 10H`, `#1:8` (Renesas CC-RX) | — |
| `8bit` | `lda #$12`, `ld a,(ix+5)`, `MVI A,12H`, `SETB P1.3`, `DB 1`, `; comment` | 6502, Z80, 8080, 8051 |
| `nasm` | `db 1`, `; comment`, `mov eax, [rel x]`, `%macro`, `0FFh` | — |

```console
$ cat intena.s
        move.w  #$7fff,$DFF096          ; disable all Amiga interrupts
$ rsasm -a m68k -f bin --hex intena.s
33 fc 7f ff 00 df f0 96
```

Motorola covers vasm, Devpac and ASM-One source and was checked against both
vasm and GNU as `--mri`. Three rules in it catch people out:

- **A word in the first column is a label**, with or without a colon, so
  instructions have to be indented. `rts` written in column 0 assembles to no
  code at all — in both reference assemblers, not just here.
- **Word and long data, and instructions, are aligned to an even address.**
  vasm on its own defaults leaves a `dc.w` after a `dc.b` at an odd address;
  Devpac, GNU as and vasm's `-devpac` mode align it, and a 68000 faults on the
  alternative. GNU as's own m68k syntax aligns nothing, so this is a property of
  the dialect.
- **Instructions are assembled as written.** vasm's default optimizer turns
  `move.l #1,d0` into `moveq #1,d0`; rsasm, like GNU as, only chooses the
  shortest encoding of the instruction you wrote.

### The 8-bit dialect

`8bit` reads the source people have for the 6502, the Z80, the 8080 and the
8051: ca65's for the 6502, Zilog's as GNU as and vasm read it, and Intel's as
AS reads it. Their spellings are one language — `$12`, `12H`, `%1010` and
`0x12` numbers, `$` and `*` for the location counter, `<`, `>` and `^` for
the bytes of an address, `DB`/`DEFB`/`.byte`, `DW`/`DEFW`/`.word`,
`DS`/`DEFS`/`.res`, `EQU`, `=`, `DEFL`/`SET`, `IF`/`ENDIF`, `MACRO`/`ENDM` or
`.macro`/`.endmacro`, ca65's `.segment` — and where the references disagree,
rsasm picks one and says so:

```console
$ cat hello.asm
bdos    equ 5
        org 100h                ; a CP/M program
start:  ld de,msg
        ld c,9
        call bdos
        ret
msg     db 'Hello$'
$ rsasm -a z80 -f bin --hex hello.asm
11 09 01 0e 09 cd 05 00 c9 48 65 6c 6c 6f 24
```

- **A word in the first column is a label, unless it is an instruction or a
  directive.** vasm and AS take any first-column word as a label; ca65 and
  GNU as want a colon and assemble `rts` written there. Both kinds of source
  work, except a colonless label spelled like a mnemonic, or a macro called
  from the first column.
- **`ORG` says where code is loaded.** The first `ORG` in a section is its
  address in the image, not padding: `ORG 100H` does not put 256 zeros in
  front of a CP/M program. A later `ORG` pads up to its address, as vasm and
  AS do; ca65 does not pad.
- **Zero page is chosen as ca65 chooses it:** for a constant or `ORG`-placed
  label known before its use, and for `<addr`; a forward reference is
  absolute, and `z:`/`a:` override either way. vasm, a multi-pass assembler,
  picks zero page for forward references too.
- **The location counter in a data list is each item's address**: `.word
  *, *` is two different values, as in ca65, vasm and GNU as. AS keeps the
  statement's address.
- A comparison is 1 when true, as in ca65 and AS; GNU as and vasm give -1.
  The operators have C's precedence, where ca65 binds `&` as tightly as `*`.

The 8051 adds what its source needs, with AS as the reference and SDCC's
sdas8051 as the second:

```console
$ cat blink.asm
LED     BIT     P1.0
        ORG     30H
MAIN:   MOV     TMOD,#01H
LOOP:   CPL     LED
        ACALL   DELAY
        SJMP    LOOP
DELAY:  DJNZ    R7,DELAY
        RET
$ rsasm -a 8051 -f ihex --hex blink.asm
:0C003000758901B290113980FADFFE22C0
:00000001FF
```

- **A bit is written `byte.bit`,** `P1.3` or `20H.5`, and the `.` splits the
  whole operand as AS splits it, so `20H+1.3` is bit 3 of 21H. Only 20H to
  2FH and the registers at a multiple of 8 have bits; AS warns about other
  bytes, or for 30H to 3FH says nothing, and assembles a bit of some other
  byte, where rsasm refuses them.
- **The register and bit names are predefined,** as AS's `stddef51.inc`
  defines them for the 8051, in upper and lower case, with its `USING` and
  the `AR0`–`AR7` names; a label or `EQU` may take one over. `BIT`, `DATA`,
  `IDATA`, `XDATA` and `CODE`, and AS's `SFR` and `SFRB`, define a name.
- **`CY` is the carry flag wherever `C` could stand,** as in AS: `CPL CY` is
  the one-byte `CPL C`, and `JB CY,$` tests bit D7H.
- **`JMP` and `CALL` become the shortest jump that reaches,** `SJMP`, then
  `AJMP` or `ACALL`, then `LJMP` or `LCALL`, with every size picked again on
  each pass, as AS picks them.
- **`AJMP` and `ACALL` reach the 2 KiB block of the address after them,**
  which is where the CPU takes the block from. AS and sdas8051 test the
  instruction's own address, and differ from rsasm, and the CPU, only for
  one in the last two bytes of a block.
- **`DW` is low byte first,** as in AS; sdas8051's `.dw` is high byte first.
  A 16-bit instruction operand, `LJMP 1234H` or `MOV DPTR,#1234H`, is high
  byte first in every assembler, as the CPU reads it.

### Renesas CC-RL, CC-RH and CC-RX

`ccrl`, `ccrh` and `ccrx` read source written for the assemblers of Renesas's
RL78, RH850 and RX compiler packages. GNU as stays the default for those
targets, because
GNU-syntax source would not always be refused in the vendor dialects — it
would sometimes mean something else — so the dialect has to be asked for:

```console
$ cat start.asm
        .CSEG   TEXT
_start: MOVW    SP, #LOWW(0xFFE00)
        MOV     [HL], #0                ; CC-RL's shorthand for [HL+0]
        BR      !!_main
$ rsasm -a rl78 -d ccrl -o start.o start.asm
```

What is covered, from the *CC-RL Compiler User's Manual* (R20UT3123EJ0115)
and the *CC-RH Compiler User's Manual* (R20UT3516EJ0113):

- comments, both number notations (CC-RL), escapes in strings, `@` in symbols
- each assembler's own operator precedence, 32-bit `>>`, and the `HIGH`,
  `LOW`, `HIGHW`, `LOWW` and `HIGHW1` separators
- `.CSEG`/`.DSEG`/`.SECTION` with their relocation attributes, default names
  and alignments, `.ORG`, `.OFFSET`, `.ALIGN`, the `.DB` family, `.EQU` and a
  redefinable `.SET`, `.PUBLIC`/`.EXTERN`/`.WEAK`
- the `$IF`/`$IFDEF`/`$ELSEIFN` family, `$INCLUDE`, `$BINCLUDE`, and macros with
  named parameters, `.LOCAL`, `?`/`~` concatenation, `.REPT` and `.IRP`
- CC-RL's `[DE]`/`[HL]` zero-displacement shorthand
- CC-RH's instruction expansions — `mov 0x10, r10` is a `movea`, `add
  0x12345, r10` a load into `r1` — its `#label`/`!label` references, condition
  suffixes (`setfgt`, `cmovz`, `cmpfeq.s`), `jr22`/`ld23.w`-style width
  spellings, `push`/`pushm`, and byte-sized `prepare`/`dispose` frames

CC-RX has a directive set of its own, covered from the *CC-RX Compiler User's
Manual* (R20UT3248EJ0115): `B`/`O`/`H` number suffixes, names with `$` and
`.`, `$` as the location symbol, its operator precedence, `.SECTION` with
`CODE`/`ROMDATA`/`DATA` and `ALIGN=`, `.ORG` and `.OFFSET` with the NOP code
or `FILL` as padding, `.ALIGN`, `.BLKB` to `.BLKD`, `.BYTE`/`.WORD`/`.LWORD`,
`.EQU`, `.GLB`/`.WEAK`, `.INCLUDE`, `.END`, `.IF`/`.ELIF`, `.DEFINE`, `?:`
temporary labels, the `__PID_R0`-`__PID_R15` register names, and macros with
`..MACPARA`, `.MREPEAT`/`..MACREP`, `.LOCAL` and `@` concatenation.

```console
$ cat reset.src
        .SECTION P,CODE
        .GLB    _start
_start: MOV.L   #0FFH:8, R1
        ADD     400[R1], R2
        BRA     ?+
        NOP
?:      RTS
        .END
$ rsasm -a rx -d ccrx -o reset.o reset.src
```

CC-RX honours a bit length specifier such as `#1:8` even where a shorter form
fits; GNU as ignores it and rsasm, like GNU as, assembles the shortest form.
So a specifier is accepted where it names the width of that form, and refused
where CC-RX would have produced something else.

No Renesas assembler can be run here, so rsasm's reading of the manuals is
checked the only way it can be: each case in `tools/xas-diff/rl78-ccrl-pairs.txt`,
`rh850-ccrh-pairs.txt` and `rx-ccrx-pairs.txt` pairs vendor source with the
GNU-syntax program it means, and `tools/xas-diff/run.sh` requires rsasm's
bytes for the first to equal GNU as's for the second. Placement is the
linker's: an `AT` attribute or `.ORG` names the section (CC-RL, CC-RH) or pads
it (CC-RX) as the manual says, but the start address is not recorded.

### NASM

`-d nasm` reads source written for NASM, the flat-binary and ELF assembler most
x86 hand-written code targets. The whole language people reach for is there:

- a preprocessor run a line at a time as NASM's is — `%define`/`%xdefine`/
  `%assign`/`%undef`, `%macro` with parameter ranges, defaults, greedy `+`
  params, `%0`, `%rotate`, `%%` labels and `%00` label capture, `%rep`/
  `%exitrep`, the `%if`/`%elif`/`%else` family (`%ifdef`, `%ifmacro`, `%ifidn`,
  `%ifnum`, `%ifstr`, `%ifctx` …), `%include`, `%strlen`/`%substr`/`%defstr`,
  `%push`/`%pop` contexts with `%$` locals, and `%error`/`%warning`
- `db`/`dw`/`dd`/`dq`/`dt` with single-, double- and backquoted strings, the
  `resb` family, `times n <stmt>` (including `times 510-($-$$) db 0`), `incbin`,
  `equ`, `struc`/`endstruc`/`istruc`/`at`/`iend`, `align`/`alignb`, and
  `absolute`
- `section`/`segment` with attributes (`progbits`, `nobits`, `alloc`, `exec`,
  `write`, `align=`), `bits 16/32/64`, `org`, `global`/`extern`/`common`/
  `static` with `:function`/`:data` and sizes, `default rel`/`abs`, `$`/`$$`
  and `.local`/`..@` labels
- NASM's operand syntax: the `byte`/`word`/`dword`/`qword` size keywords with
  no `ptr`, `[rel x]` and `[abs x]`, segment overrides `[es:di]`, the moffs
  accumulator forms, and 8086 16-bit addressing; and the `wrt ..plt`,
  `wrt ..got`, `wrt ..sym` and `wrt ..gotoff` ELF relocations

Much of what looks like NASM directive syntax — `section`, `global`, `struc`,
`align` — is macros in NASM's standard macro set wrapping a bracketed
primitive, `[section .data]`; rsasm defines the same macros, so `__SECT__` and
the rest behave as they do there.

```console
$ cat boot.asm
        org     0x7c00
        bits    16
start:  mov     ax, 0x1234
        jmp     start
        times   510-($-$$) db 0
        dw      0xaa55
$ rsasm -d nasm -f bin -o boot.bin boot.asm   # a 512-byte boot sector
```

`tools/nasm-diff/run.sh` assembles a corpus of whole programs with rsasm
`-d nasm` and with NASM 2.16.03 (built by `tools/oracles/build.sh`), and
compares the flat binaries byte for byte and the ELF objects section by
section, relocations and global symbols included. Local symbols are not
compared in ELF objects: NASM writes every label into the symbol table, where
rsasm, like GNU as, keeps them to itself, and a linker never sees the
difference. `-f win64` and `-f win32` objects are compared whole, as
`tools/coff-diff/canon.sh` prints them; see [PE/COFF](#pecoff). 403 of 403
match.

## Multi-architecture files

`.arch <name>` switches the target for everything after it, so one file can
hold, say, a boot stub for one CPU and the code it loads for another:

```console
$ cat two.s
        movl    $0x10000, %esp          # x86: `#` starts a comment
        .arch   m68k
        movew   #0x2700, %sr            | m68k: `#` is an immediate, `|` a comment
        .arch   sh
        mov     #1, r0                  ! SuperH: `!` is the comment
$ rsasm -a i386 -f bin --hex two.s
bc 00 00 01 00 46 fc 27 00 e0 01
```

- **Source is read as the target it is for.** The statement after an `.arch`
  is lexed by the new target's rules — its comment characters, and number
  spellings such as RL78's `10H` — whether the switch was in the file itself,
  in a macro expansion or in an included file. The rest of the `.arch` line,
  a trailing comment say, still belongs to the old target. The dialect (`-d`)
  does not change.
- **A macro body is read where it is expanded.** Only where it ends is found by
  the rules in force at `.macro`; the body is kept as text and lexed by the
  rules in force at each expansion, like an included file.
- **An `.arch` that is not assembled does nothing,** whether it is in a false
  conditional or in a macro that is never called.
- **Code keeps its target.** Byte order, branch displacements resolved at the
  end, and the no-ops that pad an alignment are those of the target the code
  was written for, not the one active at the end of the file.
- **The object is for the starting target.** The ELF class, machine and byte
  order are those of `-a`. Code for a different machine, byte order or word
  size can be in it, but cannot be relocated: a reference in that code has to
  resolve within the file.
  Switching to another CPU of the object's own machine (`.arch sh4` in an `sh`
  file) carries over what the ELF header records about the code so far.

Every case in `tools/multiarch-diff/programs.txt` is checked the only way it
can be: the file is split at its `.arch` lines, each part is assembled by its
own target's reference, and rsasm has to produce the concatenation.

## Debug information

`.file N "name"` (with a directory and `md5` in DWARF 5) and `.loc` with all of
its options, `view` included, write `.debug_line` and `.debug_line_str`; the
`.cfi_*` directives write `.eh_frame` or `.debug_frame`, as `.cfi_sections`
says. Where the source brings no `.debug_info`, the compilation unit an
assembler makes up for the table is written too. `-g` (or `--gdwarf-<n>`)
instead describes the assembly source: a row for each instruction, at its
line, and a unit naming the file.

The two references agree on the formats and disagree on nearly everything
inside them, so each target follows the one that checks its encodings: GNU as
for x86, m68k, SuperH, RX, RL78, V850, AVR and MSP430, and llvm-mc for the rest. That
decides, among other things, the default version (3 for GNU as, 4 for
llvm-mc, 5 for either once a `.file 0` appears), how a path splits into a
directory, whether a column carries over to the next `.loc`, which directives
end a pending `.loc`, how CIEs are shared, and how padding and relocations are
written. Each backend supplies its DWARF register numbers and names, return
address column, alignment factors, initial instructions and FDE encoding; RX,
RL78, V850 and MSP430, whose GNU as has no CFI, refuse `.cfi_*` as it does. For `-g`,
GNU as places an instruction from a macro on the line that called it, one
from `.rept` or `.irp` on its line in the block, and one from an included file
on its line there; llvm-mc puts every instruction in the main file at the
outermost line that expanded it, and describes each label as well.

Three differences remain:

- GNU as gives a `view -0` row an address of its own wherever its frag
  obstack happened to start a new chunk, which depends on the host's memory
  allocation; rsasm does so only where the row is at the address of the one
  before, which is when the view count needs it.
- On RL78, GNU as leaves every distance in the line table to the linker as a
  stack of relocation operations; rsasm writes the distances, which are final
  since it lays out the section itself. On AVR GNU as writes the distances
  too, and adds an `R_AVR_DIFF*` relocation to each, for linker relaxation;
  rsasm writes the distances alone.
- For `-g` on llvm-mc's targets, llvm-mc numbers the last statement of an
  included file against the file that included it, reading past that file's
  buffer; rsasm gives its line in the included file.

The producer named in the unit is `rsasm` and its version, or the value of
`DEBUG_PRODUCER`, which llvm-mc also reads.

## PE/COFF

`-f coff` writes a Windows object file for the target: an AMD64 object for
`x86-64`, I386 for `i386` and ARM64 for `aarch64`. `-f win64` and `-f win32`
are NASM's names for the same thing, and choose `x86-64` or `i386` when `-a`
does not:

```console
$ rsasm -f win64 -o hello.obj hello.s     # then link.exe, lld-link or mingw ld
```

What the source can say:

- sections with `.section name,"flags"`, in llvm-mc's reading of the flag
  letters (`x`, `r`, `d`, `w`, `b`, `n`, `s`, `y`, `i`, `D`), `$`-grouped
  names such as `.text$mn` and `.CRT$XCU`, and COMDATs: `.section
  name,"flags",<selection>,<symbol>` with `discard`, `one_only`, `same_size`,
  `same_contents`, `associative`, `largest` or `newest`, and `.linkonce`.
  Sections of one name told apart by their COMDAT symbol stay apart, as a
  compiler's one `.rdata` per folded constant needs
- symbols: `.def`/`.scl`/`.type`/`.endef`, `.weak` as a weak external,
  `.comm` (with its alignment as a power of two) and `.lcomm` (into `.bss`),
  absolute and `.set` symbols, `.file`, and names with `@` in them, such as
  MSVC's mangled ones
- relocations for all three machines, with COFF's convention of keeping the
  addend in the relocated bytes: `IMAGE_REL_AMD64_ADDR64`, `ADDR32`,
  `ADDR32NB`, `REL32`, `SECTION` and `SECREL`; `IMAGE_REL_I386_DIR32`,
  `DIR32NB`, `REL32`, `SECTION` and `SECREL`; and `IMAGE_REL_ARM64_BRANCH26`,
  `BRANCH19`, `BRANCH14`, `PAGEBASE_REL21`, `REL21`, `PAGEOFFSET_12A`,
  `PAGEOFFSET_12L`, `ADDR64`, `ADDR32`, `ADDR32NB`, `REL32`, `SECTION` and
  `SECREL`; `.rva`, `@IMGREL` and NASM's `wrt ..imagebase` for image-relative
  addresses, `.secrel32` and `@SECREL32`, and `.secidx`
- x86-64 unwind data: `.seh_proc`, `.seh_pushreg`, `.seh_stackalloc`,
  `.seh_setframe`, `.seh_savereg`, `.seh_savexmm`, `.seh_pushframe`,
  `.seh_handler`, `.seh_handlerdata`, `.seh_endprologue` and `.seh_endproc`
  write `.xdata` and `.pdata`, counting the prologue from the final lengths
  of its instructions

Backends choose relocations as ELF numbers, the one numbering all of them
share, and name in a `reloc::RelocClass` what a number cannot say;
`src/output/coff/reloc.rs` translates both into COFF's numbering, and says
where each COFF relocation measures its PC from, which is what the addend in
the field has to make up for. Nothing in a backend knows COFF exists, and ELF
output does not go through the translation. `@IMGREL`, `.rva`, `.secrel32`
and `.secidx` name something no psABI has a number for, so they travel as
classes of their own; they are refused outside COFF output.

llvm-mc is the reference: it writes COFF for all three machines, and it is
the assembler of the LLVM Windows toolchains. GNU as for mingw agrees with it
on relocations — which ones, where, of what type, against what, and what the
field holds — and on almost nothing else, so where the two differ rsasm
follows llvm-mc:

- `.text`, `.data` and `.bss` are always present and four-byte aligned, and a
  section is not padded at its end; GNU as aligns them to 16 and pads.
- Section symbols carry a checksum of the section, and there is no `.file`
  symbol unless the source has a `.file`; GNU as writes no checksum and a
  `.file` named `fake`.
- A relocation against a local label names the label, which is in the symbol
  table; GNU as names its section and puts the label's offset in the field. A
  linker reads the two the same.
- Nothing is preempted in a COFF object, so a reference to a symbol in its own
  section is resolved whatever its binding, except a call to a function
  (`.type 32`), which llvm-mc leaves to the linker for incremental linking and
  control flow guard; GNU as resolves that and relocates a call to a weak
  definition instead.
- A weak definition hides behind `.weak.<name>.default.<first global>`, where
  GNU as leaves out `.default`.
- A sign-extended 32-bit field is `IMAGE_REL_AMD64_ADDR32`, the only 32-bit
  absolute type the PE specification has; GNU as writes type 17.
- Code is padded with llvm-mc's no-ops: up to fifteen bytes at once for
  x86-64, one-byte `nop`s for i386, whose default Windows CPU has no `nopl`.
- On i386, a local label spelled with a leading `L` is private, as in
  llvm-mc's Microsoft conventions; elsewhere `.L` is.

Neither reference writes `IMAGE_REL_AMD64_REL32_1` to `_5`: both measure every
PC-relative field from four bytes past it and put the difference in the field,
so rsasm does the same. Two things differ on purpose: ELF's `.type
foo,@function` is accepted and says nothing, where llvm-mc refuses it, and a
`.comm` alignment past 32 bytes is refused, where llvm-mc 22 crashes.

In NASM source the object follows NASM's COFF writer rather than llvm-mc's:
its section words (`code`, `data`, `rdata`, `bss`, `info`, `align=`) and
characteristics, only the sections the source named or filled, no
checksums, its `.file`, `.absolut` and (for i386) `@feat.00` symbols, and
relocations against a defined symbol's section.

## Mach-O objects

`-f macho`, or a target triple for Darwin in `-a`, writes an `MH_OBJECT` for
x86-64 or arm64:

```console
$ rsasm -a arm64-apple-macos -o hello.o hello.s
```

The source is Darwin's assembly, as llvm-mc reads it for those triples:

- **Sections are segment and section pairs.** `.section __DATA,__data`, with
  an optional type, `+`-joined attributes and stub size
  (`.section __TEXT,__cstring,cstring_literals`), and the shorthands `.text`,
  `.data`, `.bss`, `.const`, `.const_data`, `.cstring`, `.literal4`,
  `.literal8`, `.literal16`, `.mod_init_func`, `.mod_term_func`,
  `.non_lazy_symbol_pointer`, `.lazy_symbol_pointer`, `.tdata`, `.tlv` and
  the rest of llvm-mc's list. `.zerofill`, `.lcomm` and `.comm` reserve
  zero-filled space, and their alignment, like `.align`'s, is a power of two.
  There is no `.rodata`.
- **Symbols.** A label whose name starts with `L` is the assembler's own;
  every other name, `l_.str` included, reaches the symbol table, with no
  underscore added. `.globl`, `.private_extern`, `.weak_definition`,
  `.weak_reference`, `.alt_entry`, `.no_dead_strip` and
  `.subsections_via_symbols` say what the linker may do with them.
- **Relocation modifiers** are Darwin's: `sym@GOTPCREL` on x86-64, and
  `sym@PAGE`, `sym@PAGEOFF`, `sym@GOTPAGE`, `sym@GOTPAGEOFF` and `sym@GOT` on
  arm64, where `:lo12:` is not accepted.
- `.build_version` writes `LC_BUILD_VERSION`, and `.data_region` with
  `.end_data_region` writes `LC_DATA_IN_CODE`. On arm64 `;` starts a comment.

**What is left to the linker is decided by atoms, not by binding.** A Mach-O
linker may move or drop the code from one linker-visible label to the next on
its own, so a reference from one of these atoms into another is relocated
however close the two are, even to a local symbol, and against the target's
atom with the distance as the addend; one within an atom is resolved. On arm64
llvm-mc resolves a branch to any label in the same section unless the file
has `.subsections_via_symbols`, which is what promises the linker real atoms,
and rsasm does the same. A difference of two labels is a `SUBTRACTOR` pair
unless both are in one atom — or, as llvm-mc folds it while reading a data
directive, a fixed distance apart there, already defined. Where Mach-O has no
relocation for something ELF can express — `adr` or a conditional branch to
another atom, a 32-bit absolute address on x86-64, a page reference without
`@PAGE` — the reference is refused, as llvm-mc refuses it.

`tools/macho-diff/run.sh` compares 1,573 cases against llvm-mc 22: single
statements and whole programs in Clang's style of its own, and the
`tools/mc-diff` corpora for both machines, every instruction of which has to
come out the same in a Mach-O object. Every header and load command, section,
symbol and relocation matches, and each of the 1,548 objects both assemblers
write is identical byte for byte; the other 25 cases are refused by both.
Three differences remain, and the corpora leave them out:

- x86-64 instructions are encoded as GNU as encodes them, in either format, so
  alignment padding in code uses GNU as's no-ops, and `addl $sym, %eax` is the
  short `05` form where llvm-mc writes `81 c0`.
- A negative addend on an arm64 branch or page reference is written as a
  24-bit two's-complement `ARM64_RELOC_ADDEND`. llvm-mc 22 writes the addend
  over the entry's type and length bits, which not even llvm-readobj can read
  back.
- A reference through the GOT on arm64 to a label some way into its atom is
  refused, since a GOT relocation has no addend; llvm-mc accepts it,
  relocating against the atom and writing the offset into the instruction.

## Verification

Nine differential harnesses assemble the same source with rsasm and with an
independent assembler, and compare the bytes:

- `tools/gas-diff/run.sh` against GNU as 2.47, for x86 in 64-, 32- and
  16-bit mode, in AT&T and Intel syntax. 8,651 of 8,651 match.
- `tools/mc-diff/run.sh` against llvm-mc 22, for x86 and the targets LLVM
  supports. 38,561 of 38,561 match across twenty-one target variants. For RISC-V
  it also compares whole objects, relocations included, since `la` and its
  relatives are only right if the linker is told the right things.
- `tools/xas-diff/run.sh` against cross GNU as 2.47 for m68k (for each CPU
  model, with corpora generated from GNU's own opcode table so that every
  form in it is assembled), SuperH, RX, RL78,
  V850/RH850, AVR, MSP430 and the Z80, vasm for Motorola syntax and for the Z80 and the
  6502, cc65's ca65 for the 6502, AS for the 8080 and AS and SDCC's sdas8051
  for the 8051 (its Intel HEX against AS's `p2hex`), plus CC-RL, CC-RH and
  CC-RX source paired with its GNU-syntax equivalent. For ARM and Thumb it
  compares whole objects, local and mapping symbols included, against GNU as,
  the reference for literal pools and interworking, and for AArch64's literal
  pools and system instructions; for PowerPC's vector and
  POWER8–10 instructions it is GNU as's second opinion, and the check on the
  forms only GNU as accepts. `tools/oracles/build.sh` builds the references
  from checksum-pinned sources. 24,003 of 24,003 match across fifty-six
  variants.
- `tools/flat-diff/run.sh` against a link, for flat binaries: the reference
  assembler's object, linked by GNU ld 2.47 at the same base address with the
  sections laid end to end, against `rsasm -f bin`. That is what checks the
  arithmetic a linker would otherwise do — `adrp` pages, `@ha`, `%pcrel_lo`,
  distances between sections. 205 of 205 match across thirty-one variants.
  `tools/oracles/build.sh` builds the linkers alongside the assemblers.
- `tools/nasm-diff/run.sh` against NASM 2.16.03, for the `nasm` dialect: whole
  programs compared as flat binaries, as ELF objects, relocations and global
  symbols included, and as `win64` and `win32` COFF objects. 403 of 403
  match. `tools/oracles/build.sh` builds NASM from a checksum-pinned source.
- `tools/multiarch-diff/run.sh` for files that switch targets with `.arch`,
  against the same references, one part at a time.
- `tools/dwarf-diff/run.sh` for [debug information](#debug-information),
  against GNU as 2.47 or llvm-mc 22, whichever the target follows: the line
  table, frame and compilation unit sections byte for byte with their
  relocations, from hand-written snippets, `-g` and whole files from GCC and
  Clang. 1,260 of 1,260 match across twenty-six target variants.
- `tools/coff-diff/run.sh` for [PE/COFF objects](#pecoff), against llvm-mc 22
  for x86-64, i386 and ARM64 as whole objects — every section's
  characteristics and bytes, every symbol with its auxiliary records, every
  relocation — from single statements, hand-written programs and Clang's
  output, and against GNU as 2.47 for mingw as relocations with the addends
  their fields hold. 292 of 292 comparisons match. `tools/oracles/build.sh`
  builds GNU as for mingw alongside the other cross assemblers.
- `tools/macho-diff/run.sh` for [Mach-O objects](#mach-o-objects), against
  llvm-mc 22 for x86-64 and arm64: header, load commands, sections, symbols
  and relocations as `llvm-readobj` reads them, over its own corpora and
  those of `tools/mc-diff`. 1,597 of 1,597 match, and every object both write
  is also identical byte for byte.

The x86 backend is also fuzzed: `tools/fuzz/x86.py` generates random
instructions, in all three modes and both syntaxes, some of them deliberately
invalid, and compares rsasm's bytes, relocations and accept/reject decision
with GNU as's and llvm-mc's. The general-purpose forms are written from the
Intel manual; the SIMD and newer extensions are read from GNU's expanded
opcode table, every row of them, with writemasks, broadcasts, rounding and
displacements at every disp8\*N scale. The x86 SIMD tables were derived from
that same table (`tools/fuzz/gnutbl.py` decodes it). Where the two references
disagree, rsasm follows GNU as, apart from the few cases the corpora note;
runs of 600,000 general-purpose and 240,000 mixed instructions find no case
where rsasm differs from both. The m68k backend is fuzzed the same way:
`tools/fuzz/m68k.py` draws instructions from GNU's own opcode table, read out
of the binutils source, for each CPU model in GNU and Motorola syntax against
GNU as, and in Motorola syntax against vasm; runs of 400,000 and 100,000
instructions find nothing. The table rsasm encodes those forms from,
`src/arch/m68k/table.rs`, is written from the same source by
`tools/tables/m68k.py`. `tools/fuzz/msp430.py` does the same against GNU as
alone for the MSP430's 430, 430X and 430Xv2 instruction sets, and 200,000
cases find no difference but the deviations the backend documents. The 8051
is fuzzed with whole programs:
`tools/fuzz/mcs51.py` assembles them with AS and sdas8051 too, and 80,000
programs find no case where rsasm differs from the references outside the
places this README describes. AVR is too: `tools/fuzz/avr.py` generates
programs from every row of GNU as's opcode table — labels in several
sections, branches near and out of reach, modifiers, data and alignment — on
twenty-one cores, compares whole objects with `avr-elf-as`'s and, for a
program with nothing undefined, the image `avr-elf-ld` links from it with
`rsasm -f bin`; 120,000 programs, 37,000 of them linked, find no case where
rsasm differs outside the deviations this README lists. See
`tools/fuzz/README.md`.

AArch64's SIMD, floating-point and SVE table is derived from llvm-mc rather
than written: `tools/tables/aarch64.py` disassembles random instruction words
to find every form llvm-mc prints, measures where each operand's bits go by
assembling the form with one operand changed at a time, and checks every form
against llvm-mc before writing `src/arch/aarch64/table_data.rs` (5,879 forms) and
the corpora that check it, `tools/mc-diff/aarch64-{simd,sve}-words.txt` (17,600
lines, compared a batch at a time). `tools/tables/aarch64.py check` says
whether they are still what llvm-mc gives. The backend is fuzzed by
`tools/fuzz/aarch64.py`, whose cases are llvm-mc's or GNU objdump's
disassembly of random words, a quarter of them mutated into likely-invalid
ones; runs of 500,000 instructions find no case where rsasm differs from both
references. Where the two disagree, rsasm follows llvm-mc for what an
instruction means and GNU as for what is out of range: llvm-mc takes
`ext v0.8b, v1.8b, v2.8b, #8` or `scvtf s0, w0, #33` and truncates them,
where GNU as and rsasm refuse them. It also refuses the SVE spellings only
llvm-mc reads — an unpredicated `and z0.s, z0.s, z1.s`, whose element size is
always `.d`, and an immediate outside the element's signed range, such as
`mov z0.h, #-65408` — and takes the ones only GNU as reads: `fcmp s0, 0` for
`#0.0`, and a register list `{z0.h - z1.s}` of two element sizes is refused
as llvm-mc refuses it. `smstart`, `smstop` and `zero {za}` are handwritten.

The system instructions are generated the same way from the other reference:
`tools/tables/aarch64-sys.py` takes the names from binutils' own tables —
`opcodes/aarch64-sys-regs.def` and the `aarch64_sys_regs_*` arrays — and
every encoding from a run of `aarch64-elf-as`, writing
`src/arch/aarch64/sysreg_data.rs` (1,619 `mrs`/`msr` registers with what each
allows, 12 PSTATE fields, 284 `dc`/`ic`/`at`/`tlbi` operand names and 73
aliases of `hint`) and a line per name to whichever corpus can check it:
`tools/mc-diff/aarch64-sys-words.txt` where llvm-mc gives the same word, and
`tools/xas-diff/aarch64.txt` where it does not know the name at all, which is
most of the newer ones. `msr` of a register the architecture says is
read-only warns, as GNU as warns; neither assembler refuses it.

Literal pools are GNU as's feature, so GNU as is the reference for them:
`tools/xas-diff/aarch64-relocs.txt` compares whole objects, mapping symbols
and relocations included, and `tools/flat-diff/aarch64-gas.txt` compares
linked images. Unlike its own ARM port, and unlike llvm-mc — which turns
`ldr x0, =1` into `mov x0, #1` and writes its entries in the order they were
used — GNU as on AArch64 always loads from the pool, groups the entries by
width, aligns each run and shares an entry between loads of the same value:
rsasm does what GNU as does.

PowerPC's AltiVec, VSX and POWER8–10 instructions are fuzzed the same way by
`tools/fuzz/powerpc.py`, which draws its forms from the operand kinds in GNU
binutils' opcode table and runs all three PowerPC targets. A run of 600,000
instructions finds no case where rsasm differs from both references other
than the two refusals it makes on purpose: a doubleword instruction in 32-bit
code, and a register name from another bank (`%vs3` where a general-purpose
register goes), which both read as its number. The instruction table behind
them, `src/arch/powerpc/vector.rs`, is written from that same opcode table by
`tools/tables/powerpc.py`, never by hand.

ARM and Thumb are derived and fuzzed the same way. `tools/tables/arm.py`
reads the five tables in binutils' `opcodes/arm-dis.c` — the A32, 16-bit
Thumb, 32-bit Thumb, coprocessor and NEON ones — and turns each row's format
string, which spells out where the disassembler finds every operand, into
the form an assembler encodes from, writing `src/arch/arm/table.rs` (2,523
forms under 712 spellings). Every row is accounted for: it becomes a form,
its mnemonic belongs to a hand-written encoder (the ones whose bytes depend
on more than the operands — the data-processing group with its Thumb width
selection, the branches, the literal pool, `ldm`/`stm`, `it`, `cbz`,
`msr`/`mrs`), it is a spelling only the disassembler prints, or its
architecture is out of scope; a row that is none of those is an error, and
`tools/tables/arm.py check` says whether the file is still what binutils
gives. Where the disassembler's table is looser than the instruction set —
it will print `vneg.f8`, a `vext` immediate too wide for its registers, or a
quadword register where only a double one goes — the restriction comes from
the operand kinds of gas's own `insns[]`, and what neither table says is
written out in the script with the reason. `tools/fuzz/arm.py` then
generates random instructions from those same format strings, read again and
independently, and compares rsasm against GNU as and llvm-mc in both
instruction sets; runs of 20,000 instructions find no case where rsasm
differs from both. GNU as takes a condition on `vaddl` and `vsubl`, alone of
the NEON instructions, which is the one recorded deviation.

The first three also compare whole objects for every ELF target, from the
`*-relocs.txt` corpora: each allocated section's type, flags, size, alignment
and bytes, the global, weak and undefined symbols, and every relocation, read
the way a linker reads it (`tools/mc-diff/canon.sh`). Bytes alone cannot show
a reference that should have been left to the linker, or one relocated
against the wrong symbol: the field is zero either way. Those corpora walk
each binding — local, global, weak, hidden and the other visibilities, `.set`
aliases either way round, `.globl` after use, another section, undefined —
through branches, calls, PC-relative loads and data.

All nine run in CI. The expected bytes in the hermetic tests under `tests/` were
taken from these runs rather than written by hand: a test that only checks
rsasm against rsasm can never find a wrong encoding.

Where rsasm and the reference legitimately differ, the corpus says so rather
than dropping the case. The standing example is alignment padding in
executable sections, where GNU as picks its no-op sequence by `-mtune`; only
the total length is fixed. In flat binaries it is branches between sections:
a reference assembler cannot know how far away another section will be and
takes the longest form, while rsasm, which lays the image out itself, takes
the shortest that reaches; the flat corpora write those widths out.

ARM has two references that disagree with each other. llvm-mc checks the
encodings; GNU as, which the source was written for, decides everything that
depends on more than one instruction: where literal pools go and what they
share, mapping symbols, which branches become `blx` and which are left to
the linker, how relaxation sizes Thumb instructions, and that a code
section's end is padded to a word. rsasm follows
GNU as there, and llvm-mc where the two only differ in spelling: Thumb
alignment padding uses 16-bit no-ops, and `adds r0, r0, #1` keeps the
three-operand form, where GNU as uses 32-bit no-ops and the 8-bit form. In
the vector instructions it is llvm-mc that is the looser of the two, and
rsasm follows GNU as: a condition, a width suffix or an immediate wider than
the element size is refused on a NEON instruction, as is a quadword register
where only a double one goes. The exception is the `al` condition, which
llvm-mc takes everywhere and GNU as refuses on anything unconditional --
except where some other form of the mnemonic is conditional, which is how it
comes to take `vnegal.f32 d0, d1`; rsasm takes it everywhere. See
`tools/xas-diff/README.md`.

## Design

The interesting problems in an assembler are mostly about *when* things are
known, and the design is shaped around that.

**Lexing depends on the target, not just the dialect.** `;` separates
statements in GAS and starts a comment in NASM; `1b` is a local-label reference
in GAS and the binary constant `2` in NASM. Within GAS, `#` is a comment on x86
but the immediate prefix on ARM, AArch64 and SPARC, where it is a comment only
in the first column — which is also what C preprocessor line markers look like.
So each backend supplies its comment syntax, and the lexer is configured from
it. A file is read one statement at a time, each after the one before it has
been assembled, which is what lets an `.arch` switch change how the next line
is spelled without a second pass or a guess.

**The parser stops at the statement level.** It finds labels, directives and
mnemonics; it does not look inside operands. `disp(base,index,scale)` and
`[base + index*scale + disp]` are x86's problem, and register lists are ARM's,
so the architecture backend gets the raw token tail and parses it itself.

**Expressions evaluate to relocatable values,** not numbers: `plus - minus +
addend`. That is what lets `end - start` be a constant while `end` alone is
not, and it is the same representation whether the answer comes out as bytes
or as a relocation.

**Positional references are bound where they are written.** `.` and `1f` mean
different things depending on where in the file they appear, which is
information that no longer exists by the time an expression is evaluated. Both
are rewritten into ordinary symbol references as soon as the statement
containing them is parsed.

**Macro expansion is textual, and re-lexed.** Substituting tokens cannot
express what macro bodies rely on: `.L\@_loop:` has to paste the invocation
counter into the middle of an identifier, and there is no token meaning "join
these". Expanding into text and re-lexing gives that for free, and it is what
GNU as does, so bodies written for it behave the same way. The expansion
becomes a real entry in the source map, which turns out to be a feature — a
diagnostic inside a macro points at the expanded line and names the macro it
came from.

**A relocation is described, not just numbered.** A backend picks the ELF
relocation number for each field it leaves to the linker, and also says what
the field computes — a branch, a load through the GOT, the page of an address
(`RelocClass`) — which the ELF writer has no need of. Mach-O numbers the same
meanings differently and splits some of them further, so its writer maps the
description instead of second-guessing ELF's numbers.

**A fragment that might change size carries every candidate.** An instruction
whose branch could be short or long is encoded *both* ways at parse time; the
layout pass picks an index into that list. Since the index only ever
increases, the loop terminates. This costs a little memory and buys a
relaxation pass with no re-entry into the parser.

### Adding an architecture

Implement `arch::Architecture` and register it in `arch::lookup`. The trait is
the whole seam:

```rust
fn assemble(&self, cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>) -> Option<Vec<Variant>>;
```

The backend receives a mnemonic and the tokens after it, and returns the
candidate encodings with their fixups. It never touches sections, symbols or
addresses — those belong to the core, which is why two backends can write into
the same file. `.arch` switches between them at any point.

Fixed-width encodings rarely have a contiguous displacement field, so a fixup
can carry a function that scatters the value through the instruction word,
together with the field's real width and alignment. That is what lets a 26-bit
AArch64 branch offset be range-checked as 26 bits rather than as the 4 bytes
it lives in.

A few conventions really are per target and have trait methods with defaults:
`comments` (which characters start one), `word_bytes` (how wide `.word` is —
2 on x86 and PowerPC, 4 on the other RISC targets), `section_align` (the
alignment a section starts with) and `defers_to_linker` (which references to
a symbol in their own section are still relocated).

Add a corpus under `tools/mc-diff/` for the new target and take the hermetic
tests' expected bytes from its runs.

## Building

```console
$ cargo build --release
$ cargo test
$ tools/gas-diff/run.sh     # needs binutils
$ tools/mc-diff/run.sh      # needs llvm-mc and llvm-objcopy
$ tools/flat-diff/run.sh    # needs cross binutils with ld, and llvm-mc
$ tools/xas-diff/run.sh     # needs tools/oracles/build.sh
$ tools/nasm-diff/run.sh    # needs NASM from tools/oracles/build.sh
$ tools/multiarch-diff/run.sh  # needs all of the above
$ tools/dwarf-diff/run.sh   # needs llvm-mc and tools/oracles/build.sh
$ tools/coff-diff/run.sh    # needs llvm-mc and tools/oracles/build.sh
$ tools/macho-diff/run.sh   # needs llvm-mc, llvm-readobj and llvm-objdump
```

## License

MIT — see [LICENSE](LICENSE).
