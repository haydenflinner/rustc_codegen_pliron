# Differential testing against cross assemblers

For targets that neither `tools/gas-diff` (the host's GNU as) nor
`tools/mc-diff` (llvm-mc) can assemble: m68k, V850/RH850, RL78, RX, SuperH,
AVR, MSP430, and the 8-bit Z80, 6502, 8080 and 8051.
And for ARM and Thumb whole objects, where GNU as is the reference that matters
and llvm-mc answers differently; see [ARM](#arm). AArch64 is here for the
same reason, its literal pools and its system instructions; see
[AArch64](#aarch64).

```console
$ tools/oracles/build.sh          # once: builds the pinned references
$ tools/xas-diff/run.sh           # every target with a corpus
$ tools/xas-diff/run.sh m68k-mot  # just one
```

The references are GNU binutils 2.47, vasm, cc65 2.19's ca65, the Macro
Assembler AS and SDCC 4.4.0's sdas8051 and sdld, built into `target/oracles/`,
or wherever `RSASM_ORACLES` points — useful for sharing one build between
worktrees, since binutils takes minutes per target.
See `tools/oracles/build.sh` for why the versions are pinned.

## Targets

| Key | rsasm | Reference |
|---|---|---|
| `m68k` | `m68k`, GNU syntax | `m68k-elf-as` |
| `m68k-mot` | `m68k`, Motorola syntax | `m68k-elf-as --mri` |
| `m68k-vasm` | `m68k`, Motorola syntax | `vasmm68k_mot -no-opt -devpac` |
| `m68k-vasm-020` | `m68k`, Motorola syntax | `vasmm68k_mot -no-opt -devpac -m68020 -m68881 -m68851` |
| `m68k-000` | `68000`, GNU syntax | `m68k-elf-as -m68000` |
| `m68k-020`, `-030`, `-040`, `-060` | `m68k`, `68030`, `68040`, `68060`, GNU syntax | `m68k-elf-as -m68020` and so on |
| `m68k-cpu32`, `-fido` | `cpu32`, `fidoa`, GNU syntax | `m68k-elf-as -mcpu32`, `-mcpu=fidoa` |
| `m68k-5475`, `-54455`, `-5208` | those ColdFire parts, GNU syntax | `m68k-elf-as -mcpu=5475` and so on |
| each of those with `-mot` | the same, Motorola syntax | the same with `--mri` |
| `v850` | `v850` | `v850-elf-as` |
| `rh850` | `rh850` | `v850-elf-as -mv850e3v5` |
| `rl78` | `rl78` | `rl78-elf-as` |
| `rx` | `rx` | `rx-elf-as` (code is in section `P`) |
| `sh` / `shl` | `sh` / `shl` | `sh-elf-as` / `sh-elf-as -little` |
| `msp430` / `msp430x` | `msp430` / `msp430x` | `msp430-elf-as -mcpu=430` / `-mcpu=430x` |
| `msp430xv2` | `msp430xv2` | `msp430-elf-as -mcpu=430xv2`, on `msp430.txt` |
| `msp430-poly` / `msp430x-poly` | `msp430` / `msp430x` | the same with `-mP`, for the polymorphic branches |
| `rl78-ccrl` | `rl78`, CC-RL syntax | `rl78-elf-as`, on the GNU half of each pair |
| `rh850-ccrh` | `rh850`, CC-RH syntax | `v850-elf-as -mv850e3v5`, likewise |
| `rx-ccrx` | `rx`, CC-RX syntax | `rx-elf-as`, likewise |
| `6502` | `6502`, 8-bit syntax | `ca65`, laid out by `ld65` |
| `6502-vasm` | `6502`, 8-bit syntax | `vasm6502_oldstyle` |
| `z80` | `z80`, 8-bit syntax | `z80-elf-as`, linked at 0 by `z80-elf-ld` |
| `z80-gas` | `z80`, GNU syntax | `z80-elf-as`, on `z80.txt` |
| `z80-vasm` | `z80`, 8-bit syntax | `vasmz80_oldstyle`, on `z80.txt` and its own programs |
| `i8080` | `i8080`, 8-bit syntax | `asl -cpu 8080`, converted by `p2bin` |
| `powerpc64` / `powerpc64le` | `powerpc64` / `powerpc64le` | `powerpc64-linux-gnu-as -a64 -mfuture` with `-mbig` / `-mlittle`, both on `powerpc64.txt`; see [PowerPC](#powerpc) |
| `powerpc` | `powerpc`, relocations only | `powerpc64-linux-gnu-as -a32 -mfuture` |
| `i8051` | `8051`, 8-bit syntax | `asl -cpu 8051` after its `stddef51.inc`, converted by `p2bin` |
| `i8051-sdas` | `8051`, 8-bit syntax | `sdas8051`, linked by `sdld` into Intel HEX |
| `i8051-hex` | `8051`, 8-bit syntax, `-f ihex` | `asl -cpu 8051`, converted by `p2hex`; the text is compared |
| `avr` | `avr` | `avr-elf-as`, with no `-mmcu`: the AVR2 set |
| `avr51` | `avr51` | `avr-elf-as -mmcu=avr51` |
| `avrxmega` | `atxmega128a1u` | `avr-elf-as -mmcu=atxmega128a1u`, which has the read-modify-write instructions |
| `avrtiny` | `avrtiny` | `avr-elf-as -mmcu=avrtiny` |

The m68k keys named after a CPU hold corpora generated from GNU's opcode table
by `tools/fuzz/m68k.py corpus --first`: every form of every instruction, once,
on the first CPU in its list that has the form, with operands that form takes
and no earlier form of the same mnemonic does. Two kinds of form are missing
from them, and say so in a comment: the `fmovem` forms with a dynamic register
list, which GNU as can only reach as `fmovemx` because an earlier `fmovem` form
matches first and then refuses the operand, and in Motorola syntax the forms
GNU as `--mri` reads its own way (see `tools/fuzz/m68k.py`, `mri_skips`).
Extended and packed float immediates are checked against vasm, in
`m68k-vasm-020`: GNU as writes the first without the 68881 format's 16 zero
bits and refuses the second.

## Comparing objects

`<key>-relocs.txt` holds snippets compared as whole objects — sections,
global symbols and relocations — with `tools/mc-diff/canon.sh`, as
`tools/mc-diff` compares its own (see its README). Each walks a symbol of
every binding through the target's calls, branches and data, and checks the
alignment of the standard sections. RX's reference is run with
`-muse-conventional-section-names` for these, since by default it renames
`.text`, `.data` and `.bss` to Renesas's `P`, `D_1` and `B_1`, which rsasm
does not.

They leave out what rsasm deliberately writes differently:

- A relocation against a local symbol in RL78 or RX data. GNU as keeps the
  symbol and writes its value into the field as well; rsasm relocates against
  the section, like every other target, and leaves the field zero. A linker
  reads both the same way.
- `sym - .` where `sym` is outside the section: GNU as for RL78 and RX writes
  a stack of relocations, which rsasm has no support for and refuses, and GNU
  as for V850 drops the `- .` (see `src/arch/v850/reloc.rs`).
- A conditional branch on RX or V850 that is left to the linker: GNU as keeps
  it short, trusting the linker to reach; rsasm takes the longest form (see
  `src/arch/rx/branch.rs` and `src/arch/v850/branch.rs`).
- On MSP430, what the header of `msp430-relocs.txt` lists: the addend GNU as
  gives the last `R_MSP430_SYM_DIFF` in a section, where `.section .data`
  puts its `__crt0_movedata` among the undefined symbols, and a number
  `.set` after its use, which GNU as relocates against the symbol.

AVR objects are compared with their `e_flags` too (`canon.sh --flags`), which
name the core and carry `EF_AVR_LINKRELAX_PREPARED`, and with `.avr.prop`,
which is not allocated but is what the linker relaxes the code by. Their
local symbols are not compared: GNU as names each label a relocation needs,
`.L1^B1` for a `1:`, and rsasm names the same labels in its own way.
| `arm` / `thumb` | `arm` / `thumb`, statements and whole objects | `arm-none-eabi-as -march=armv7ve -mfpu=neon-vfpv4` (`-mthumb`) |
| `aarch64` | `aarch64`, and whole objects for the pools | `aarch64-elf-as` with every extension it names; see [AArch64](#aarch64) |

## ARM

llvm-mc checks ARM and Thumb encodings in `tools/mc-diff`, but the source
people write for ARM was written against GNU as, and for literal pools,
mapping symbols and interworking the two disagree. So `arm.txt` and
`thumb.txt` here are GNU as's own opinion of the encodings -- the
instructions the security, virtualization and divide extensions add, the
banked registers, the shifted operands llvm-mc wants a `#` on, and the whole
floating-point and NEON set -- and the `arm-relocs.txt` and
`thumb-relocs.txt` corpora compare whole objects
against GNU as with `tools/mc-diff/canon.sh --full`: as for every other
target, each allocated section's header and bytes and every relocation, and
also `e_flags` and every symbol, local ones and mapping symbols included. A
snippet named `refused: ...` matches when both assemblers reject it.

GNU as is run with `-march=armv7ve -mfpu=neon-vfpv4`: without a `-march` it
assumes a CPU with no Thumb-2 and no `blx`, and `armv7ve` is ARMv7-A with
the security, virtualization and divide extensions, which together are the
whole set this backend claims. The FPU option adds VFPv4 and NEON. Every snippet starts with `.syntax unified`, because
GNU as reads Thumb in the older divided syntax unless told otherwise, and
rsasm only knows the unified one.

Where the two still differ, on purpose:

- **Alignment padding in Thumb code.** GNU as for ARMv7 pads with 32-bit
  `nop.w`, after one 16-bit `nop` if the count is odd; rsasm, like llvm-mc,
  uses 16-bit ones throughout. Snippets pad Thumb code with zeros, or not at
  all.
- **A three-operand Thumb immediate on one register.** GNU as assembles
  `adds r0, r0, #1` (and `suble r0, r0, #1` in an `it` block) with the 8-bit
  `adds r0, #1` form; rsasm, like llvm-mc, keeps the 3-bit form the spelling
  asks for, which `tools/mc-diff` checks. Snippets write the two-operand form.
- **`-mthumb-interwork`.** GNU as sets the low bit of a Thumb function's
  address in an ARM `adr` only with that option, which rsasm does not have;
  so the snippets, assembled without it, check that it does not.

Where GNU as and llvm-mc disagree and GNU as is followed, as seen in these
corpora: mapping symbols (llvm-mc marks neither alignment padding nor the
zeros that align a literal pool), padding the end of a code section to a
word (llvm-mc does not), which branches are left to the linker (llvm-mc
relocates an ARM `bl` even to a label in the same section, and converts no
`bl` to `blx` itself), and the size of a relaxable Thumb instruction (GNU as
picks each afresh on every pass against the growth so far, and llvm-mc can
widen one that GNU as keeps at 16 bits).

## AArch64

llvm-mc checks the AArch64 encodings in `tools/mc-diff`, including the SIMD
and SVE table and the 3,037 system-instruction lines the two references
agree on. Two things are GNU as's alone, and are here.

**Literal pools.** `ldr x0, =0x123456789` puts the value in a pool and loads
it from there, and where the pool goes is decided across a whole section, so
`aarch64-relocs.txt` compares whole objects with `tools/mc-diff/canon.sh
--full`: sections, bytes, relocations, `e_flags` and every symbol, the
`$x`/`$d` mapping symbols among them. llvm-mc decides differently — it turns
`ldr x0, =1` into `mov x0, #1`, and writes its entries in the order they were
used, not grouped by width — and GNU as on AArch64, unlike its own ARM port,
never substitutes a `mov` at all. rsasm follows GNU as. `tools/flat-diff`
links the same programs and compares the images.

**System instructions.** `aarch64.txt` holds a line for every operand name in
GNU's tables that llvm-mc does not know or encodes differently: most of the
newer system registers, the nXS TLB maintenance names, `plbi`, `stshh`.
`tools/tables/aarch64-sys.py` generates it and the mc-diff half from the same
run, so regenerate rather than edit. The CPU string in `run.sh` names every
extension GNU as has a name for, since it refuses a system register for the
CPU rather than reporting an unknown name; the generator builds the same
string from GNU as's own list.

Where the two references differ and GNU as is followed: `msr ctr_el0, x0`,
writing a read-only register, is a warning in both GNU as and rsasm, and an
error in llvm-mc.

## PowerPC

llvm-mc is the reference for PowerPC, in `tools/mc-diff`. GNU as is here as a
second opinion on the AltiVec, VSX and POWER8-10 instructions, whose table
rsasm derives from binutils' own, and for what only GNU as accepts. It runs
with `-mfuture`, since a few VSX instructions both references know
(`xvadduhm`, `lxvrl` and their relatives) are enabled only there.

The two references disagree, and rsasm takes the wider of the two where the
encoding is not in doubt:

- **Mnemonics only GNU as knows:** `xxmr`, `xxlnot`, `pnop`, `fmrgew`,
  `fmrgow`, and `vcfpsxws`, `vcfpuxws`, `vcsxwfp` and `vcuxwfp` (spellings of
  `vctsxs`, `vctuxs`, `vcfsx` and `vcfux`); and the R operand of `pla` and
  `psubi`.
- **Ranges only GNU as accepts:** a negative byte in `xxspltib` or immediate in
  `mtvsrbmi`, `subpcis` outside -32768 to 32767, and `xxgenpcv*m` modes 16-31.
- **Mnemonics only llvm-mc knows** are not in the table: the POWER11
  `xxaes*` and `xxgfmul128*` instructions, which GNU as 2.47 refuses short of
  `-mfuture` and which llvm-mc encodes with the 192- and 256-bit key forms
  swapped.
- **Relocations.** Only GNU as writes `R_PPC64_D34`, for an absolute symbol in
  a prefixed instruction; llvm-mc refuses it. A PC-relative prefixed
  reference to a label in its own section is resolved by GNU as, which also
  writes a relocation's addend into the field; llvm-mc leaves it to the
  linker with the field zero, and rsasm follows llvm-mc, so
  `powerpc64-relocs.txt` here holds only absolute references.
- **Relocations in 32-bit code.** For a symbol in a DS- or DQ-form
  displacement, llvm-mc writes the PowerPC64 relocation numbers, which
  `R_PPC_*` does not define, and GNU as the plain `R_PPC_ADDR16` and
  `R_PPC_ADDR16_LO`; rsasm follows GNU as, which `powerpc-relocs.txt` checks.
  A 34-bit field cannot be relocated there at all.
- **`@pcrel` with the R operand 0**, where the instruction would read a
  PC-relative value as an offset from `rA`: GNU as writes it; llvm-mc refuses
  it on `paddi` and mis-encodes it on a load with R left out; rsasm refuses it.

## Vendor syntax no reference reads

No Renesas assembler can be run here, so CC-RL, CC-RH and CC-RX source cannot be
compared against the assembler it was written for. Their corpora,
`<key>-pairs.txt`, hold pairs instead:

```text
=== what the pair shows
	MOV	[DE], #1		; CC-RL, assembled by rsasm -d ccrl
--- gnu
	mov	[de+0], #1		; the same thing in GNU syntax, assembled by GNU as
```

The syntax rules come from the Renesas manuals, and the pairing — that the
GNU half means what the manual says the vendor half means — is the claim each
case makes; the bytes of both halves must then agree. A GNU half the
reference refuses counts as a failure, never a match. The pairs are also
the source of the expected bytes in `tests/rl78_ccrl.rs`,
`tests/rh850_ccrh.rs` and `tests/rx_ccrx.rs`.

The vendor half is assembled without a section directive, because only the
first section's bytes are compared; CC-RX's `.SECTION` and data-section
padding are tested in `tests/rx_ccrx.rs` instead.

## Which m68k reference to trust

GNU as `--mri` is the reference for Motorola *encodings*. vasm is a secondary
check, and only for cases with no size choice in them, because it disagrees
with GNU as for reasons that are not rsasm's to copy:

- By default vasm is an **optimizing** assembler. It rewrites instructions —
  `move.l #1,d0` becomes `moveq #1,d0` — and deletes a branch to the next
  instruction. GNU as and rsasm assemble what is written.
- With `-no-opt` it stops choosing absolute-short addressing, so `$0400`
  becomes a 32-bit address where GNU as uses the 16-bit form.

On *syntax* the two agree on every rule we checked, including the one that
surprises people: in Motorola source a word in the first column is a label,
so `rts` written in column 0 assembles to nothing.

## The 8-bit references

Each 8-bit target has the assembler its source is usually written for as its
reference, and a second where one reads the same syntax:

- **6502: ca65.** cc65's assembler is what most 6502 source today is written
  for. It assembles in one pass, so a forward reference is absolute, and its
  `.org` sets the location counter without padding; the corpus keeps to a
  leading `.org`. vasm is the second opinion, on programs only: it picks zero
  page for any address that turns out to fit, and has no `z:`/`a:` or dotted
  directives in oldstyle syntax.
- **Z80: GNU as.** Linked at address 0, since an unlinked object leaves
  absolute addresses to the linker. GNU as has no `org` directive of its own
  (`.org` pads), reads `add b` as something other than `add a,b`, and gives
  -1 for a true comparison; those cases are in the vasm programs or nowhere.
  vasm reads the whole one-line corpus identically.
- **8080: AS.** Neither GNU as nor vasm reads Intel's 8080 syntax faithfully,
  and AS does, with two gaps of its own: it has no `D` radix suffix or `AND`-
  style word operators, and its `$` in a data list is the statement's
  address rather than the item's.
- **8051: AS, and sdas8051.** AS reads ASM51's syntax and defines no register
  names of its own, so every `i8051` snippet is assembled after the
  `stddef51.inc` AS ships, which is where rsasm's predefined names come from.
  SDCC's sdas8051 is the second: an asxxxx assembler with its own directives,
  `0x` numbers and `.` for the location counter, so it has a corpus of its
  own in the spelling both read, `i8051-sdas.txt`, generated with the AS one
  by `tools/fuzz/mcs51.py corpus`. It needs an absolute area before an `.org`,
  which the harness supplies; it writes `.dw` high byte first, where AS's
  `DW` is low byte first, so its corpus has no words; and sdld refuses a
  numeric `AJMP` target outside block 0, so its programs use labels. Where
  the two disagree rsasm follows AS, apart from what `i8051-pairs.txt` pairs
  with the AS source that means the same: ASM51's `DATA`, `IDATA`, `XDATA`
  and `CODE`, which AS lacks and are its `EQU`; and an `AJMP` or `ACALL` in
  the last two bytes of a 2 KiB block, which both references check against
  the instruction's own address and the CPU, and rsasm, against the address
  after it — the pair is AS's generic `JMP`, which uses that address too.

A reference that refuses a case never matches, so the corpora hold only
source every reference for the key accepts.

## 78K0

No reference exists — CA78K0 is a proprietary Windows tool and neither GNU
binutils nor LLVM supports the 78K0 — so it has no corpus here. Its tables are
verified by tests that walk the whole opcode space.
