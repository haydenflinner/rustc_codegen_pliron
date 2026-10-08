# Generated instruction tables

Several backends have tables no one wrote: they are derived from a reference
and checked against it.

- `arm.py` reads the five instruction tables in binutils'
  `opcodes/arm-dis.c` -- A32, 16-bit Thumb, 32-bit Thumb, coprocessor and
  NEON -- and writes `src/arch/arm/table.rs`. Every row is accounted for: it
  becomes a form, a hand-written encoder owns its mnemonic, it is a spelling
  only the disassembler prints, or its architecture is out of scope; a row
  that is none of those is an error. `tools/tables/arm.py audit` prints one
  line per row saying which. What the disassembler's table does not say --
  which registers an operand may hold, and the element sizes a NEON type
  has -- comes from the operand kinds of gas's own `insns[]` in
  `gas/config/tc-arm.c`, or is written out in the script with the reason.
- `powerpc.py` reads binutils' `opcodes/ppc-opc.c` and writes
  `src/arch/powerpc/vector.rs` and the generated blocks of the PowerPC
  corpora; see its own `--help`.
- `aarch64.py` measures the AArch64 SIMD, floating-point and SVE instruction
  set against llvm-mc and writes `src/arch/aarch64/table_data.rs`,
  `table_names.rs` and the corpora `tools/mc-diff/aarch64-simd-words.txt` and
  `aarch64-sve-words.txt`, as below.
- `aarch64-sys.py` does the same for the system instructions against the
  other reference: the names come from binutils' `aarch64-sys-regs.def` and
  the `aarch64_sys_regs_*` tables in `aarch64-opc.c`, and each encoding from
  assembling that name with `aarch64-elf-as`. It writes
  `src/arch/aarch64/sysreg_data.rs` and a line per name to
  `tools/mc-diff/aarch64-sys-words.txt` (where llvm-mc agrees) or
  `tools/xas-diff/aarch64.txt` (where it does not know the name).

```console
$ tools/tables/arm.py table                     # rewrite src/arch/arm/table.rs
$ tools/tables/arm.py check                     # exit 1 if it is out of date
$ tools/tables/arm.py audit                     # one line per row of the tables
$ tools/tables/aarch64.py table --jobs 32       # about ten minutes
$ tools/tables/aarch64.py check                 # exit 1 if it is out of date
$ tools/tables/aarch64.py fit --only '^fmov$' --dump forms.txt
$ tools/tables/aarch64-probe.py 'sshr v0.8b, v1.8b, #3'   # one line, measured
$ tools/tables/aarch64-sys.py table             # a few seconds
$ tools/tables/aarch64-sys.py check
```

## How a form is found

llvm-mc disassembles:

- every value of the top 22 bits of a word, with the low ten bits zero, with
  bits 5-9 set (the SVE `all` pattern) and twice random;
- every value of the top 16 bits, with 64 random fills of the rest;
- eight million random words;
- every single-bit neighbour of four words of each form found so far, until
  that finds nothing new.

Each line it prints is parsed by the operand grammar in `a64.py` into a
mnemonic and operands, and the lines are grouped by *shape*: the mnemonic and
what each operand looks like, `v0.4s` and `v1.8b` being different shapes but
`v0.4s` and `v3.4s` the same. A shape is kept if it has a SIMD, floating-point
or SVE operand, or if the mnemonic has a shape that does and the handwritten
encoders in `insn.rs` do not know it (`ldapur x0, [x1]` goes with
`ldapur d0, [x1]`, `cntd x0` with the rest of SVE). SME beyond `smstart`,
`smstop` and `zero {za}` is left out.

Spellings llvm-mc accepts but never prints (`uxtl`, `cmle` with three
registers, `mov z0.b, w0`) are listed in `ALIASES`, as syntax only.

## How a form is measured

For each shape, one line is assembled with every register zero, and then
again with each number in it changed on its own: every register, every lane
index, and for an immediate a dense run around the printed value, the powers
of two and their neighbours, and so on outwards while llvm-mc keeps accepting
it. From the words that come back:

- a number that flips its own bits of the word is a field, or a scatter of
  bits (a vector element index, `movi`'s split `imm8`, `mov z0.d, z1.d`
  writing `z1` twice);
- one that moves a run of bits by a constant per step is affine: right shifts
  count down from the element width, `mul vl` offsets are signed;
- an immediate the word holds a function of is the bitmask immediate, the
  bitmask immediate of its complement (`bic`, `orn`, `eon`), the 8-bit
  floating-point immediate or a byte mask, whichever explains most values;
- a handful of values with codes of their own (`#90`/`#270`) is a choice;
- two registers that only move together are tied: `add z0.b, p0/m, z0.b,
  z1.b`.

Two things make ranges hard to read off llvm-mc. It takes out-of-range
immediates and truncates them (`ext v0.8b, v1.8b, v2.8b, #8` is `#0`), so a
range is grown from the printed value only while llvm-mc's word keeps
agreeing with the model, and kept only if most of its values are ones the
disassembler prints back: that tells `scvtf`'s `#1`..`#32` from the `#0`
llvm-mc also takes. And one printed shape can be several encodings: `mov
z0.s, #imm` is `dup` for `#-128`..`#127`, `dup` shifted for multiples of 256,
and `dupm` for bitmask immediates. Each shape is therefore fitted from its
sample with the smallest numbers, then from the smallest sample that fit does
not encode, and so on; the forms are tried in that order, narrowest first,
and every value a later form's transform was seen to get wrong must reach an
earlier form. A form whose signed immediate llvm-mc also takes as the
element's bits (`mov z0.h, #0xfff0` for `#-16`, `mov z0.b, #-241` for `#15`)
is marked to wrap that way, once llvm-mc has been seen to.

Every form is then assembled with random operands, the ends of each range
included, and compared with what the fit predicts; one that does not match is
reported rather than written. Finally the whole table is checked in the order
the backend tries it, on every line assembled so far.

## Files

- `a64.py` runs llvm-mc and holds the operand grammar, shared with
  `tools/fuzz/aarch64.py`.
- `aarch64.py` sweeps, fits, checks and writes the table and corpora.
- `aarch64-probe.py` fits one line and prints what it measured, for looking
  into a form.
