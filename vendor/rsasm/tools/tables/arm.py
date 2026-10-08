#!/usr/bin/env python3
"""Regenerate src/arch/arm/table.rs from GNU binutils.

The A32 and T32 instruction sets have, between them, some hundreds of forms
that are nothing but a fixed opcode word with register and immediate fields
punched into it: the saturating and packing instructions, the parallel
arithmetic, the bitfield moves, the load/store exclusives, the halfword
multiplies, the hint and barrier space, the coprocessor instructions and the
system forms. Retyping those from the architecture manual is how an
assembler ends up with a wrong bit, so rsasm's table is derived from the one
GNU's own disassembler carries:

    opcodes/arm-dis.c   arm_opcodes[], thumb_opcodes[], thumb32_opcodes[]
                        and generic_coprocessor_opcodes[]

Each row there is an opcode word, a mask and a format string that spells the
instruction out -- `"ssat%c\\t%12-15R, %{I:#%16-20W%}, %0-3R%s"` says the
mnemonic takes a condition, then a register at bits 12-15, then an immediate
at bits 16-20 written one greater than the field, then a register at bits
0-3, then an optional shift. Read the other way round that is exactly what an
assembler needs, so this turns the format strings into rsasm's `Op` lists
and writes the table; `super::generic` encodes from it.

    tools/tables/arm.py table     # rewrite src/arch/arm/table.rs
    tools/tables/arm.py check     # exit 1 if it is out of date
    tools/tables/arm.py audit     # print every row and where it went

Nothing is dropped silently. Every row of the four tables is accounted for:
it becomes a form, or its mnemonic is in HAND because a hand-written encoder
owns it -- the instructions whose encoding depends on more than the operand
list: the data-processing group with its Thumb width selection, the branches
and their relocations, `ldr`/`str` and the literal pool, `ldm`/`stm`, `it`,
`cbz`, `msr`/`mrs` -- or it is a spelling only the disassembler has (DIS), or
its architecture is one rsasm does not claim, which FEATURES decides. A row
that is none of those is an error.

The tree is the one tools/oracles/build.sh unpacks (RSASM_ORACLES, default
target/oracles).
"""

import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
ORACLES = os.environ.get("RSASM_ORACLES", os.path.join(ROOT, "target", "oracles"))
BINUTILS = os.path.join(ORACLES, "src", "binutils-2.47")
TABLE = os.path.join(ROOT, "src", "arch", "arm", "table.rs")
TC_ARM = os.path.join(BINUTILS, "gas", "config", "tc-arm.c")

# ============================================================================
# Reading arm-dis.c
# ============================================================================

TABLES = [
    ("static const struct opcode32 arm_opcodes[] =", "arm"),
    ("static const struct opcode16 thumb_opcodes[] =", "t16"),
    ("static const struct opcode32 thumb32_opcodes[] =", "t32"),
    ("static const struct sopcode32 generic_coprocessor_opcodes[] =", "cop"),
    ("static const struct sopcode32 coprocessor_opcodes[] =", "vfp"),
    ("static const struct opcode32 neon_opcodes[] =", "neon"),
]

ENTRY = re.compile(
    r"\{\s*(?:ANY\s*,\s*)?(ARM_FEATURE\w*\s*\([^)]*\)|[A-Za-z_0-9]+)\s*,\s*"
    r"(0[xX][0-9a-fA-F]+|\d+)\s*,\s*(0[xX][0-9a-fA-F]+|\d+)\s*,\s*"
    r'((?:"(?:[^"\\]|\\.)*"\s*)+)\}'
)

# The extensions an ARMv7-A/R/M core can have, which is what this backend
# claims. `-march=armv7ve` turns on all of them at once, and that is what
# tools/xas-diff checks the instructions gated behind SEC, VIRT and DIV
# against. Everything else -- the ARMv8 additions, ARMv8-M's security
# extension, ARMv8.1-M's low-overhead loops and MVE, and ARMv8.1-M PACBTI --
# is out of scope, and README says so.
FEATURES = {
    "ARM_EXT_V1", "ARM_EXT_V2", "ARM_EXT_V2S", "ARM_EXT_V3", "ARM_EXT_V3M",
    "ARM_EXT_V4", "ARM_EXT_V4T", "ARM_EXT_V5", "ARM_EXT_V5T", "ARM_EXT_V5E",
    "ARM_EXT_V5ExP", "ARM_EXT_V5J", "ARM_EXT_V6", "ARM_EXT_V6K",
    "ARM_EXT_V6T2", "ARM_EXT_V6Z", "ARM_EXT_V7", "ARM_EXT_DIV",
    "ARM_EXT_ADIV", "ARM_EXT_MP", "ARM_EXT_SEC", "ARM_EXT_VIRT",
    "ARM_EXT2_V6T2_V8M",
    # The floating-point unit, up to VFPv4: what `-mfpu=neon-vfpv4` gives.
    "FPU_VFP_EXT_V1xD", "FPU_VFP_EXT_V1", "FPU_VFP_EXT_V2", "FPU_VFP_EXT_V3",
    "FPU_VFP_EXT_V3xD", "FPU_VFP_EXT_FMA", "FPU_VFP_EXT_FP16",
    "FPU_NEON_EXT_V1", "FPU_NEON_EXT_FMA",
}

# Mnemonics a hand-written encoder owns, because the bytes depend on more
# than the operands do: which width a Thumb instruction takes, where a
# literal pool went, what a branch is relocated as, or an addressing mode
# shared with the literal load.
HAND = set("""
    and eor sub rsb add adc sbc rsc tst teq cmp cmn orr mov bic mvn orn neg
    lsl lsr asr ror rrx addw subw movw movt mul mla mls
    umull umlal smull smlal
    ldr str ldrb strb ldrh strh ldrsb ldrsh ldrd strd
    ldrt strt ldrbt strbt ldrht strht ldrsbt ldrsht
    ldm ldmia ldmib ldmda ldmdb stm stmia stmib stmda stmdb
    ldmfd ldmfa ldmed ldmea stmfd stmfa stmed stmea push pop
    b bl bx blx cbz cbnz adr it
    msr mrs pld pldw pli
""".split())
# ...and the flag-setting spellings of the same, which the mnemonic patterns
# expand to.
HAND |= {n + "s" for n in HAND}
# The NEON instructions whose operands are more than fields: the modified
# immediate, whose `cmode` the assembler picks from the value written, and the
# structure transfers, whose register list, alignment and index are one
# tangle of fields. `super::neon` owns those.
NEON_HAND = ("%E", "%A", "%B", "%C")

# `tstp` and its relatives set the flags into the PSR on an ARMv2; GNU as has
# no syntax for them and only the disassembler prints them.
DIS = {"tstp", "teqp", "cmpp", "cmnp"}

# Rows the disassembler has that spell no instruction GNU as assembles: the
# second copy of the unallocated hint space, the `mov rd, rd` a pre-ARMv6T2
# core used as a no-op, and the one-register `push`/`pop` printed for a
# plain store.
DIS_ROWS = [
    ("arm", 0x0320F000, 0x0FFFFF00),  # nop {imm}, twice over
    ("t16", 0x0000BF00, 0x0000FF0F),  # nop {imm}, which Thumb has no syntax for
    ("t32", 0xF3AF8000, 0xFFFFFF00),  # nop.w {imm}, likewise
    ("arm", 0xE1A00000, 0xFFFFFFFF),  # mov r0, r0
    ("t16", 0x000046C0, 0x0000FFFF),  # mov r8, r8
    ("arm", 0x052D0004, 0x0FFF0FFF),  # push {rt} as a store
    ("arm", 0x049D0004, 0x0FFF0FFF),  # pop {rt} as a load
]

# Where a row and the instruction GNU as assembles part company, and what
# this writes instead. The disassembler need not print an operand it can
# work out, and need not read a field it does not use.
OVERRIDE = {
    # `ldrexd`/`strexd` name both halves of the pair; only the first is
    # encoded, and GNU as takes the spelling that leaves the second out too.
    ("ldrexd", "Arm"): (("Reg", 12, 4), ("Next",), ("Base", 16, 4)),
    ("strexd", "Arm"): (("Reg", 12, 4), ("Reg", 0, 4), ("Next",),
                        ("Base", 16, 4)),
    # `smc` takes a four-bit immediate, which the disassembler prints out of
    # a wider field.
    ("smc", "Arm"): (("Imm", ((0, 4),), 1, 0),),
    ("smc", "T32"): (("Imm", ((16, 4),), 1, 0),),
    # `nop`'s hint number has to be written in braces; the opcode fields of
    # `cdp` and `mcr`, which the disassembler prints the same way, do not.
    ("nop", "Arm"): (("Hint", ((0, 8),)),),
    # The A32 bitfield extracts print the width as a plain field; it is the
    # same `#lsb, #width` pair the Thumb rows spell with `%F`, and the width
    # has to fit above the bit position.
    ("sbfx", "Arm"): (("Reg", 12, 4), ("Reg", 0, 4), ("Lsb", ((7, 5),)),
                      ("Width", ((16, 5),))),
    ("ubfx", "Arm"): (("Reg", 12, 4), ("Reg", 0, 4), ("Lsb", ((7, 5),)),
                      ("Width", ((16, 5),))),
    # The halfword saturations take a four-bit position, which the T32 rows
    # print out of the five-bit field their word-sized siblings use.
    ("ssat16", "T32"): (("Reg", 8, 4), ("Imm", ((0, 4),), 1, 1), ("Reg", 16, 4)),
    ("usat16", "T32"): (("Reg", 8, 4), ("Imm", ((0, 4),), 1, 0), ("Reg", 16, 4)),
}
# `pkhtb`'s shift is `asr` alone, and its type bit -- 6 in A32, hw2's 5 in
# T32 -- is the one a shift of zero clears, which turns it into a `pkhbt`.
OVERRIDE[("pkhtb", "Arm")] = (
    ("Reg", 12, 4), ("Reg", 16, 4), ("Reg", 0, 4),
    ("SatShift", 7, 5, 64 + 6, 255, 0),
)
OVERRIDE[("pkhtb", "T32")] = (
    ("Reg", 8, 4), ("Reg", 16, 4), ("Reg", 0, 4),
    ("SatShift", 6, 2, 64 + 5, 12, 3),
)

# `do_strex` refuses a status register that is also one of the transfer
# registers or the base. The Thumb encoder makes that check for the byte,
# halfword and doubleword forms only; `do_t_strex` has no such constraint.
STREX = {
    "strex": ("Arm",),
    "strexb": ("Arm", "T32"),
    "strexh": ("Arm", "T32"),
    "strexd": ("Arm", "T32"),
}

# `do_t_ldrexd` moves into two registers, which have to differ.
OVERRIDE[("ldrexd", "T32")] = (
    ("Reg", 12, 4), ("Reg", 8, 4), ("Distinct",), ("Base", 16, 4),
)

# `mrrc` moves into two registers, which `do_mrrc` requires to differ.
for _m in ("mrrc", "mrrc2"):
    OVERRIDE[(_m, "*")] = (
        ("Coproc", 8), ("Imm", ((4, 4),), 1, 0), ("Reg", 12, 4),
        ("Reg", 16, 4), ("Distinct",), ("CReg", 0),
    )

# `srs` writes its base register as `sp`, or leaves it out.
for _srs in ("srsia", "srsib", "srsda", "srsdb"):
    OVERRIDE[(_srs, "*")] = (("SpBase", 16, 21), ("Imm", ((0, 5),), 1, 0))

# Bits a row leaves out because the disassembler works them out for itself.
# `cps` is printed from a row whose mask leaves the M bit free, since the
# forms with and without a mode share it; GNU as sets it, this row being the
# one that takes a mode.
WORD_FIX = {("cps", "Arm"): 1 << 17}

# `vqshrun` and `vqrshrun` have the unsigned bit set, which their rows do not
# say: the disassembler prints them from a row that matches either value of
# it. The bit is 24 in A32 and 28 in T32, since a T32 NEON instruction is the
# A32 word with its top byte translated rather than copied.
for _un in ("vqshrun", "vqrshrun"):
    for _size in (16, 32, 64):
        WORD_FIX[("%s.s%d" % (_un, _size), "Arm")] = 1 << 24
        WORD_FIX[("%s.s%d" % (_un, _size), "T32")] = 1 << 28

# The T32 forms of the one-register operations hold `Rm` twice, and the
# disassembler reads only the copy at bits 16-19.
for _dup in ("rev", "rev16", "revsh", "rbit", "clz"):
    OVERRIDE[(_dup, "T32")] = (("Reg", 8, 4), ("RegTwice", 16, 0))

# Forms GNU as assembles that no row of the disassembler's table describes,
# because it prints them as another instruction. `do_pkhtb` turns a `pkhtb`
# with no shift into `pkhbt rd, rm, rn`, in both instruction sets.
# The NEON registers of a three-register instruction.
VD = ((12, 4), (22, 1))
VN = ((16, 4), (7, 1))
VM = ((0, 4), (5, 1))

EXTRA = [
    ("pkhtb", "Arm", 0x06800010, True,
     (("Reg", 12, 4), ("Reg", 0, 4), ("Reg", 16, 4))),
    ("pkhtb", "T32", 0xEAC00000, True,
     (("Reg", 8, 4), ("Reg", 0, 4), ("Reg", 16, 4))),
    # `sdiv rd, rm` divides the destination, which GNU as writes as the
    # optional middle operand of `(RR, oRR, RR)`.
    ("sdiv", "Arm", 0x0710F010, True, (("RegTwice", 16, 0), ("Reg", 8, 4))),
    ("udiv", "Arm", 0x0730F010, True, (("RegTwice", 16, 0), ("Reg", 8, 4))),
    ("sdiv", "T32", 0xFB90F0F0, True, (("RegTwice", 8, 16), ("Reg", 0, 4))),
    ("udiv", "T32", 0xFBB0F0F0, True, (("RegTwice", 8, 16), ("Reg", 0, 4))),
]

# Where an encoder checks a register GNU as's operand kind leaves open:
# `do_div` refuses the PC in all three of its registers, which `RR` does not
# say. The value is the least restrictive class the form's registers take.
CLASS_FIX = {("sdiv", "Arm"): 1, ("udiv", "Arm"): 1}

# ============================================================================
# Taking a format string apart
# ============================================================================

# `%<lo>-<hi><code>` or `%<bit><code>`, the two spellings of a bitfield.
FIELD = re.compile(r"%(\d+)(?:-(\d+))?(.)")


class Unsupported(Exception):
    pass


def read_entries(src):
    """Every row of the four tables, in order."""
    lines = open(src).read().split("\n")
    out = []
    for head, which in TABLES:
        i = next(n for n, l in enumerate(lines) if l.startswith(head))
        j = i
        while not lines[j].startswith("};"):
            j += 1
        body = re.sub(r"/\*.*?\*/", "", "\n".join(lines[i + 1:j]), flags=re.S)
        for m in ENTRY.finditer(body):
            fmt = "".join(re.findall(r'"((?:[^"\\]|\\.)*)"', m.group(4)))
            out.append(
                {
                    "val": int(m.group(2), 0),
                    "mask": int(m.group(3), 0),
                    "fmt": fmt,
                    "set": which,
                    "feats": set(re.findall(r"(?:ARM_EXT|ARM_CEXT|FPU_)\w*", m.group(1))),
                }
            )
    return out


def split_fmt(fmt):
    """The mnemonic half of a format string and the operand half, with the
    disassembler's trailing `@ ...` commentary dropped."""
    parts = fmt.split("\\t")
    ops = parts[1] if len(parts) > 1 else ""
    return parts[0], ops.split("@")[0].strip()


def expand_mnemonic(mnem, val, mask=0):
    """Every concrete spelling a mnemonic pattern stands for, as
    (name, opcode word, takes a condition).

    `%<bit>'c` prints `c` when the bit is one, `%<bit>`c` when it is zero and
    `%<field>?abc` selects a letter by the field's value, so each of those is
    two or more instructions sharing one row. A `.w` or `.n` in the name is
    the width the disassembler prints, which the encoder decides for itself."""
    # Each entry carries the bits the conditionals have settled, since a row
    # may test one bit twice (`%16?us%7?31%7?26` is `s16`, `s32`, `u16` and
    # `u32`, not sixteen spellings).
    out = [("", val, False, mask)]
    i = 0
    while i < len(mnem):
        if mnem[i] != "%":
            out = [(n + mnem[i], v, c, m) for n, v, c, m in out]
            i += 1
            continue
        if mnem[i:i + 2] == "%c":
            out = [(n, v, True, m) for n, v, _, m in out]
            i += 2
            continue
        # The letters the disassembler works out from the whole instruction
        # rather than from one field. Each stands for a set of spellings, all
        # of them instructions a hand-written encoder owns.
        if mnem[i + 1] in "CptwI":
            code, i = mnem[i + 1], i + 2
            tails = {
                # `%C` prints the condition, or `s` outside an `it` block.
                "C": ["", "s"],
                # `%p` prints `p` on the ARMv2 forms that set the PSR.
                "p": ["", "p"],
                # `%t` prints `t` on a post-indexed user-mode transfer.
                "t": ["", "t"],
                # `%w` prints a core load or store's width and signedness;
                # only a load can be signed.
                "w": ["", "b", "h", "sb", "sh"],
                # `%I` prints an `it` block's mask and condition.
                "I": [""],
            }[code]
            out = [
                (n + t, v, c or code == "C", m)
                for n, v, c, m in out
                for t in tails
                if code != "w" or n.endswith("ldr") or t in ("", "b", "h")
            ]
            continue
        m = MULTI.match(mnem, i)
        if m and m.group(2) == "?":
            # A selector whose bits are not next to each other: the NEON
            # fixed-point `vcvt`, whose direction is bit 24 over bit 8.
            pieces = multi_field(m.group(1))
            i = m.end()
            bits = sum(w for _, w in pieces)
            letters = mnem[i:i + (1 << bits)]
            i += 1 << bits
            ones = 0
            for lsb, width in pieces:
                ones |= ((1 << width) - 1) << lsb
            nxt = []
            for n, v, c, mk in out:
                if mk & ones == ones:
                    value, shift = 0, 0
                    for lsb, width in pieces:
                        value |= ((v >> lsb) & ((1 << width) - 1)) << shift
                        shift += width
                    nxt.append((n + letters[(1 << bits) - value - 1], v, c,
                                mk))
                    continue
                for value in range(1 << bits):
                    w, left = v, value
                    for lsb, width in pieces:
                        w |= (left & ((1 << width) - 1)) << lsb
                        left >>= width
                    nxt.append((n + letters[(1 << bits) - value - 1], w, c,
                                mk | ones))
            out = nxt
            continue
        m = FIELD.match(mnem, i)
        if not m:
            raise Unsupported("mnemonic %r" % mnem)
        lo = int(m.group(1))
        hi = int(m.group(2)) if m.group(2) else lo
        lo, hi = min(lo, hi), max(lo, hi)
        bits = hi - lo + 1
        code = m.group(3)
        i = m.end()
        if code in "'`":
            ch, i = mnem[i], i + 1
            ones = ((1 << bits) - 1) << lo
            on, off = (ones, 0) if code == "'" else (0, ones)
            nxt = []
            for n, v, c, m in out:
                if m & ones == ones:
                    nxt.append((n + ch if v & ones == on else n, v, c, m))
                else:
                    nxt.append((n + ch, v | on, c, m | ones))
                    nxt.append((n, v | off, c, m | ones))
            out = nxt
        elif code == "?":
            letters = mnem[i:i + (1 << bits)]
            i += 1 << bits
            ones = ((1 << bits) - 1) << lo
            nxt = []
            # print_insn_arm indexes c[(1 << width) - value], counting back
            # from the end of the letters.
            def letter(value):
                return letters[(1 << bits) - value - 1]
            for n, v, c, m in out:
                if m & ones == ones:
                    nxt.append((n + letter((v & ones) >> lo), v, c, m))
                    continue
                for value in range(1 << bits):
                    nxt.append((n + letter(value), v | (value << lo), c,
                                m | ones))
            out = nxt
        elif code in "STU":
            # `%<field>S<limit>`: the element width the field picks, written
            # into the mnemonic. `S` counts from 8 bits, `T` from 16 and `U`
            # from 32, and the limit digit says which values are legal --
            # its top two bits the lowest and its bottom two the highest.
            base = 8 << "STU".index(code)
            limit = int(mnem[i], 16)
            i += 1
            low, high = limit >> 2, limit & 3
            ones = ((1 << bits) - 1) << lo
            nxt = []
            for n, v, c, m in out:
                if m & ones == ones:
                    value = (v & ones) >> lo
                    if low <= value <= high:
                        nxt.append((n + str(base << value), v, c, m))
                    continue
                for value in range(low, high + 1):
                    nxt.append((n + str(base << value), v | (value << lo), c,
                                m | ones))
            out = nxt
        elif code == "c":
            # A condition in a field of its own, which the mnemonic carries
            # as a suffix either way.
            out = [(n, v, True, m) for n, v, _, m in out]
        else:
            raise Unsupported("mnemonic %r" % mnem)
    return [
        (n.replace(".w", "").replace(".n", "").strip(), v, c)
        for n, v, c, _ in out
    ]


def field(lo, hi):
    return ((lo, hi - lo + 1),)


# `%12-15,22D`: a value in several pieces, written from its own low bits up.
MULTI = re.compile(r"%(\d+(?:-\d+)?(?:,\d+(?:-\d+)?)+)(.)")


def multi_field(spec):
    out = []
    for part in spec.split(","):
        if "-" in part:
            lo, hi = (int(x) for x in part.split("-"))
            out.append((min(lo, hi), abs(hi - lo) + 1))
        else:
            out.append((int(part), 1))
    return tuple(out)


# Where each VFP register field lives. A single-precision number is the
# four-bit field with its low bit beside it; a double-precision one has the
# extra bit on top. `print_insn_coprocessor`'s `%y<n>` and `%z<n>` codes.
VFP_REG = {
    "y0": (0, ((5, 1), (0, 4))),
    "y1": (0, ((22, 1), (12, 4))),
    "y2": (0, ((7, 1), (16, 4))),
    "y4": (0, ((5, 1), (0, 4))),
    "z0": (1, ((0, 4), (5, 1))),
    "z1": (1, ((12, 4), (22, 1))),
    "z2": (1, ((16, 4), (7, 1))),
}
# A register list: the first register, then how many there are. A
# double-precision list counts in pairs of halves, so its length field starts
# one bit up and the bit below says whether this is the deprecated `fldmx`.
VFP_LIST = {
    "y3": (0, ((22, 1), (12, 4)), ((0, 8),)),
    "z3": (1, ((12, 4), (22, 1)), ((1, 7),)),
    "B": (1, ((12, 4), (22, 1)), ((1, 7),)),
}

# The lsb and width fields of `bfc`/`bfi` (%E) and `sbfx`/`ubfx` (%F). A32
# keeps each whole; T32 splits the lsb over hw2[14:12] and hw2[7:6].
LSB = {"arm": ((7, 5),), "t32": ((6, 2), (12, 3))}
MSB = {"arm": ((16, 5),), "t32": ((0, 5),)}


def parse_ops(text, which, val=0, mask=0):
    """The operand half of a format string, as a list of `Op` constructors."""
    ops = []
    i, n = 0, len(text)

    def bitfield():
        nonlocal i
        m = FIELD.match(text, i)
        if not m:
            raise Unsupported("field at %r" % text[i:])
        lo = int(m.group(1))
        hi = int(m.group(2)) if m.group(2) else lo
        i = m.end()
        return min(lo, hi), max(lo, hi), m.group(3)

    def skip_unique():
        nonlocal i
        # `u` and `U` only say that two register fields must differ, which
        # the disassembler warns about and an assembler need not.
        while i < n and text[i] in "uU":
            i += 1

    while i < n:
        if text[i] in ", ":
            i += 1
            continue
        if text[i] == "!":
            # A `!` the row writes out is a writeback the form always does,
            # and whose bit its own word already holds.
            ops.append(("Writeback", 255))
            i += 1
            continue
        if text[i] == "[":
            i = parse_mem(text, i, ops, which)
            continue
        if text.startswith("%{", i):
            i = parse_braced(text, i, ops, which)
            continue
        if text[i] == "{":
            # `{imm}`: the coprocessor opcode of `cdp` and `mcr`, and the
            # hint number of `nop`, both of which may be left out.
            m = re.compile(r"\{%\{I:%(\d+)-(\d+)d%\}\}").match(text, i)
            if not m:
                raise Unsupported("braces %r" % text[i:])
            i = m.end()
            ops.append(("OptImm", field(int(m.group(1)), int(m.group(2)))))
            continue
        if text.startswith("ROR ", i):
            # One `uxtab16` row spells the rotation in plain capitals where
            # its siblings use the styling wrapper.
            ops.append(("Shift", "ror"))
            i += 4
            continue
        if text[i] != "%":
            raise Unsupported("literal %r in %r" % (text[i], text))
        m = MULTI.match(text, i)
        if m:
            i = m.end()
            pieces, code = multi_field(m.group(1)), m.group(2)
            if code in "DQR":
                ops.append(("Vfp", {"D": 1, "Q": 2, "R": 3}[code], pieces))
            elif code == "r":
                ops.append(("Reg", pieces[0][0], pieces[0][1]))
            else:
                raise Unsupported("multi-field %%%s" % code)
            continue
        if text[i + 1] in "yz" and text[i + 2].isdigit():
            code, i = text[i + 1:i + 3], i + 3
            if code in VFP_LIST:
                kind, first, count = VFP_LIST[code]
                ops.append(("VfpList", kind, first, count))
            else:
                kind, pieces = VFP_REG[code]
                ops.append(("Vfp", kind, pieces))
                if code == "y4":
                    # The `%y4` pair is written out as both registers.
                    ops.append(("VfpNext",))
            # A lane may follow, written `[imm]`.
            m = re.compile(r"\[%\{I:%([\d,-]+)d%\}\]").match(text, i)
            if m:
                i = m.end()
                ops[-1] = ("VfpLane", kind, pieces, multi_field(m.group(1)))
            continue
        if text[i + 1].isdigit():
            lo, hi, code = bitfield()
            if code in "STU" and which == "neon":
                # The element width as an operand: `vshll.s8 q0, d0, #8`
                # takes the width the type suffix already named.
                limit = text[i]
                i += 1
                ops.append(("SizeImm", field(lo, hi), 8 << "STU".index(code)))
                continue
            skip_unique()
            if code == "e" and which == "neon":
                # A shift amount the field holds counting down: `vshr.s8 d0,
                # d1, #1` is the largest value the field can take.
                ops.append(("NegImm", field(lo, hi)))
                continue
            if code in "rRS":
                ops.append(("Reg", lo, hi - lo + 1))
            elif code == "T":
                ops.append(("Next",))
            elif code in "dx":
                ops.append(("Imm", field(lo, hi), 1, 0))
            elif code == "'":
                ch, i = text[i], i + 1
                if ch != "!":
                    raise Unsupported("flag %r" % ch)
                ops.append(("Writeback", lo))
            else:
                raise Unsupported("field code %r" % code)
            continue
        code = text[i + 1]
        i += 2
        if which == "neon" and code in "DFE":
            if code == "D":
                # A scalar, `d0[1]`, whose register and lane share one
                # field: the element size says where the line falls.
                ops.append(("Scalar",))
            elif code == "F":
                # `vtbl`'s table: a run of `d` registers, one to four.
                ops.append(("TblList", ((16, 4), (7, 1)), ((8, 2),)))
            else:
                # The modified immediate, which `super::neon` owns.
                raise Unsupported("the NEON modified immediate")
        elif code == "e":
            # `smc`, `hvc` and A32 `udf`: bits 8-19 above bits 0-3.
            ops.append(("Imm", ((0, 4), (8, 12)), 1, 0))
        elif code == "V":
            ops.append(("Imm", ((0, 12), (16, 4)), 1, 0))
        elif code == "H":
            # T32 `udf.w`: the four bits of hw1 above the twelve of hw2,
            # which is where `%V` puts them too.
            ops.append(("Imm", ((0, 12), (16, 4)), 1, 0))
        elif code == "K":
            # T32 `smc`: hw2[3:0], hw1[3:0], hw2[11:4].
            ops.append(("Imm", ((4, 8), (16, 4), (0, 4)), 1, 0))
        elif code == "E":
            ops.append(("Lsb", LSB[which]))
            ops.append(("Msb", MSB[which]))
        elif code == "F":
            ops.append(("Lsb", LSB[which]))
            ops.append(("Width", MSB[which]))
        elif code == "U":
            ops.append(("Barrier",))
        elif code == "R" and which == "t32":
            ops.append(("Rotate", 4))
        elif code == "s" and which == "t32":
            ops.append(("SatShift", 6, 2, 21, 12, 3))
        elif code == "S" and which == "t32":
            # A shifted register. Where the row's mask fixes the two type
            # bits, as it does for `pkhbt` and `pkhtb`, only that one shift
            # is allowed and the amount is all that is written.
            if mask & 0x30 == 0x30:
                ops.append(("Reg", 0, 4))
                # The type bit is hw2's bit 5, which a zero shift clears.
                ops.append(("SatShift", 6, 2, 64 + 5 if val & 0x20 else 255, 12, 3))
            else:
                ops.append(("Shifted",))
        elif code == "A":
            ops.append(("VfpMem",) if which == "vfp" else ("CoprocMem",))
        elif code == "B" and which == "vfp":
            kind, first, count = VFP_LIST["B"]
            ops.append(("VfpList", kind, first, count))
        elif code in "xX":
            pass  # a disassembler warning, not an operand
        else:
            raise Unsupported("code %%%s" % code)
    return ops


def parse_mem(text, i, ops, which):
    """A bracketed address the table can hold whole: `[rn]`, `[rn, rm]`,
    `[rn, rm, lsl #1]` and `[rn, #imm]`."""
    end = text.index("]", i)
    body = text[i + 1:end]
    i = end + 1
    parts = [p.strip() for p in body.split(",")]
    m = FIELD.fullmatch(re.sub(r"[uU]+$", "", parts[0]))
    if not m or m.group(3) not in "rRS":
        raise Unsupported("base %r" % parts[0])
    base = int(m.group(1))
    if len(parts) == 1:
        ops.append(("Base", base, 4))
        return i
    m = FIELD.fullmatch(parts[1])
    if m and m.group(3) in "rRS":
        shift = 0
        if len(parts) == 3:
            if parts[2] != "%{B:lsl%} %{I:#1%}":
                raise Unsupported("index shift %r" % parts[2])
            shift = 1
        ops.append(("IdxMem", base, int(m.group(1)), shift))
        return i
    if len(parts) != 2:
        raise Unsupported("address %r" % body)
    kind, lo, hi, scale, bias = braced_imm(parts[1], which)
    if kind != "Imm":
        raise Unsupported("address %r" % body)
    ops.append(("OffMem", base, field(lo, hi), scale))
    return i


def braced_imm(body, which):
    """`%{I:#%<lo>-<hi><code>%}`, as (kind, lo, hi, scale, bias)."""
    m = re.fullmatch(r"%\{I:#(.*)%\}", body)
    if not m:
        raise Unsupported("immediate %r" % body)
    f = FIELD.fullmatch(m.group(1))
    if not f:
        return ("Fixed", int(m.group(1)), 0, 0, 0)
    lo = int(f.group(1))
    hi = int(f.group(2)) if f.group(2) else lo
    lo, hi = min(lo, hi), max(lo, hi)
    scale, bias = 1, 0
    code = f.group(3)
    if code == "W":
        # A32 prints the field plus one; Thumb prints it times four.
        bias, scale = (1, 1) if which in ("arm", "cop") else (0, 4)
    elif code == "D":
        bias = 1
    elif code == "H":
        scale = 2
    elif code not in "dxX":
        raise Unsupported("immediate code %r" % code)
    return ("Imm", lo, hi, scale, bias)


def parse_braced(text, i, ops, which):
    """`%{I:...%}`, `%{R:...%}` and `%{B:...%}`, the disassembler's styling
    wrappers, which hold an immediate, a register name or a keyword."""
    kind = text[i + 2]
    end = text.index("%}", i)
    body, i = text[i + 4:end], end + 2
    if kind == "I":
        if which == "neon":
            m = re.fullmatch(r"#%(\d+)-(\d+)([STU])([0-9a-f])", body)
            if m:
                # The element width, which the type suffix named too.
                lo, hi = int(m.group(1)), int(m.group(2))
                ops.append(("SizeImm", field(min(lo, hi), max(lo, hi)),
                            8 << "STU".index(m.group(3))))
                return i
            m = re.fullmatch(r"#%(\d+)-(\d+)e", body)
            if m:
                # A shift the field counts down from its own width.
                lo, hi = int(m.group(1)), int(m.group(2))
                ops.append(("NegImm", field(min(lo, hi), max(lo, hi))))
                return i
        if body == "#0.0":
            ops.append(("Zero",))
            return i
        m = re.fullmatch(r"#%([\d,-]+)E", body)
        if m:
            ops.append(("VfpImm", multi_field(m.group(1))))
            return i
        m = re.fullmatch(r"#%([\d,-]+)k", body)
        if m:
            ops.append(("VfpFix", multi_field(m.group(1))))
            return i
        if body == "#%e":
            ops.append(("Imm", ((0, 4), (8, 12)), 1, 0))
            return i
        m = re.fullmatch(r"0x(%\d+-\d+X){4}", body)
        if m:
            # `bkpt` and `hlt`, whose immediate is printed a nibble at a time.
            ops.append(("Imm", ((0, 4), (8, 12)), 1, 0))
            return i
        f = FIELD.fullmatch(body)
        if f and f.group(3) == "d" and not body.startswith("#"):
            # A number printed with no `#`. At bits 8-11 it is the
            # coprocessor, written `p15`; the rest are the opcode fields of
            # `cdp`, `mcr` and their relatives, written as plain numbers.
            lo = int(f.group(1))
            hi = int(f.group(2)) if f.group(2) else lo
            if lo == 8:
                ops.append(("Coproc", lo))
            else:
                ops.append(("Imm", field(lo, hi), 1, 0))
            return i
        kind, lo, hi, scale, bias = braced_imm("%{I:" + body + "%}", which)
        if kind == "Fixed":
            ops.append(("Fixed", lo))
        else:
            ops.append(("Imm", field(lo, hi), scale, bias))
        return i
    if kind == "R":
        m = re.fullmatch(r"d%([\d,-]+)d\[%([\d,-]+)d\]", body)
        if m:
            ops.append(("VfpLane", 1, multi_field(m.group(1)),
                        multi_field(m.group(2))))
            return i
        m = re.fullmatch(r"%([\d,-]+)D\[%([\d,-]+)d\]", body)
        if m:
            ops.append(("VfpLane", 1, multi_field(m.group(1)),
                        multi_field(m.group(2))))
            return i
        if body in ("fpsid", "fpscr", "fpexc", "fpinst", "fpinst2",
                    "mvfr0", "mvfr1", "mvfr2"):
            ops.append(("Named", body))
            return i
        m = re.fullmatch(r"cr%(\d+)-(\d+)d", body)
        if m:
            ops.append(("CReg", int(m.group(1))))
            return i
        if body == "APSR_nzcv":
            ops.append(("ApsrNzcv",))
            return i
        m = re.fullmatch(r"r%(\d+)-(\d+)d", body)
        if m:
            lo, hi = int(m.group(1)), int(m.group(2))
            ops.append(("Reg", lo, hi - lo + 1))
            return i
        raise Unsupported("register %r" % body)
    if kind == "B":
        if body in ("lsl", "asr", "ror"):
            ops.append(("Shift", body))
            return i
        m = re.fullmatch(r"%(\d+)'a%(\d+)'i%(\d+)'f", body)
        if m:
            ops.append(("IntFlags", int(m.group(3))))
            return i
        m = re.fullmatch(r"%(\d+)\?ble", body)
        if m:
            ops.append(("Endian", int(m.group(1))))
            return i
        raise Unsupported("keyword %r" % body)
    raise Unsupported("%%{%s:" % kind)


# ============================================================================
# Reading gas's own table, for the registers each operand may hold
# ============================================================================

# `gas/config/tc-arm.c`'s `insns[]` names each operand's kind, which is where
# the register restrictions live: `RRnpc` is any register but the PC, and
# `RRnpcsp` -- gas's `BadReg` -- neither the PC nor the stack pointer. The
# disassembler's table cannot say this, since an UNPREDICTABLE encoding still
# has to print.
GAS_CLASS = {
    "RR": 0, "APSR_RR": 0,
    "RRnpc": 1, "RRnpcb": 1, "RRw": 1, "RRnpctw": 3, "RRe": 1,
    "RRnpc_npcsp": 1, "RRnpc_I0": 1,
    "RRnpcsp": 2, "RRo": 2, "RRnpcsp_I32": 2,
    # `vmrs` takes `apsr_nzcv` where the program counter would be, so the
    # register spelling of it is one the PC is not.
    "APSR_RR": 1,
}
# `RRnpctw` -- the base of a VFP block transfer -- takes the PC in A32 as
# long as it is not written back; in Thumb it does not take it at all, which
# THUMB_BAD says.
# In Thumb, `do_t_*` puts nearly every register operand through
# `reject_bad_reg`, which refuses the stack pointer as well as the PC, whatever
# the table's own kind says. `rfe`'s base is the exception the table records
# itself, and `RRw` keeps its meaning.
THUMB_BAD = {"RR", "RRnpc", "RRnpcb", "RRnpcsp", "RRnpc_npcsp", "RRnpc_I0",
             "APSR_RR"}
# ...and `RRnpctw` refuses the PC in Thumb, but not the stack pointer.
THUMB_NO_PC = {"RRnpctw"}


def read_insns(src):
    """Mnemonic -> the operand kinds of its first row in `insns[]`."""
    text = open(src).read()
    start = text.index("static const struct asm_opcode insns[] =")
    end = text.index("\n};", start)
    body = re.sub(r"/\*.*?\*/", "", text[start:end], flags=re.S)
    out = {}
    row = re.compile(
        r"^[ \t]*\w+\s*\(\s*\"?([\w.]+)\"?\s*,(?:[^()\n]|\n)*?"
        r",\s*\d+\s*,\s*\(([^()]*)\)", re.M)
    for m in row.finditer(body):
        name = m.group(1)
        kinds = [k.strip() for k in m.group(2).split(",") if k.strip()]
        out.setdefault(name, kinds)
    return out


def reading_ops(ops):
    """The operands of a form that are written in the source, in order."""
    return [o for o in ops
            if o[0] not in ("Writeback", "Distinct", "FirstDistinct")]


def gas_kinds(name, ops, insns):
    """The operand kinds `insns[]` gives this form, one per written operand,
    or None where the two do not line up. A vector mnemonic carries a type
    suffix that gas's table does not."""
    kinds = insns.get(name) or insns.get(name.split(".")[0])
    if kinds is None:
        return None
    reading = reading_ops(ops)
    # An operand GNU as marks optional may simply not be in the form: the
    # 16-bit Thumb extends have no rotation to write, and `sdiv rd, rm`
    # leaves out the middle register.
    while len(kinds) > len(reading) and any(k.startswith("o") for k in kinds):
        drop = max(i for i, k in enumerate(kinds) if k.startswith("o"))
        kinds = kinds[:drop] + kinds[drop + 1:]
    return kinds if len(kinds) == len(reading) else None


def bare_kind(kind):
    """An operand kind with the marks that do not change which registers it
    holds taken off: the `o` of an optional operand, the `MQ` of an MVE
    register, and the alternatives after an underscore."""
    if kind.startswith("o") and len(kind) > 1:
        kind = kind[1:]
    kind = kind.split("_")[0]
    for tail in ("MQR", "MQ"):
        if kind.endswith(tail) and kind != tail:
            kind = kind[:-len(tail)]
    return kind


def reg_classes(name, ops, thumb, insns):
    """One class per operand the form reads, in the order they are written."""
    kinds = gas_kinds(name, ops, insns)
    if kinds is None:
        return None
    reading = reading_ops(ops)
    out = []
    which = "T32" if thumb else "Arm"
    least = CLASS_FIX.get((name, which),
                          VEC_CLASS_FIX.get((name.split(".")[0], which), 0))
    for op, kind in zip(reading, kinds):
        bare = kind[1:] if kind.startswith("o") and len(kind) > 1 else kind
        if op[0] in ("Base", "OffMem"):
            out.append(1 if thumb else GAS_CLASS.get(bare, 255))
        elif op[0] in ("Reg", "RegTwice", "Next"):
            if thumb and bare in THUMB_NO_PC:
                out.append(1)
            elif thumb and bare in THUMB_BAD:
                out.append(2)
            else:
                out.append(max(least, GAS_CLASS.get(bare, 255)) if
                           GAS_CLASS.get(bare, 255) != 255 else 255)
        else:
            out.append(255)
    return out if any(c != 255 for c in out) else None


# ============================================================================
# Merging the rows that spell one instruction
# ============================================================================


def merge(forms):
    """Several rows of the disassembler's table are one instruction as an
    assembler sees it, because a fixed field it prints is an operand that
    may be left out: the rotation of `sxtab`, the shift of `ssat` and
    `pkhbt`, the offset of a Thumb `ldrex`. Each group is folded into the
    row that has no such operand, with the operand added back."""
    out, by_key = [], {}
    for f in forms:
        trail = trailing_shift(f["ops"])
        key = (f["name"], f["set"], tuple(f["ops"][:len(f["ops"]) - len(trail)]))
        by_key.setdefault(key, []).append((f, trail))
    for key, group in by_key.items():
        base = group[0][0]
        trails = [t for _, t in group]
        if all(not t for t in trails):
            for f, _ in group:
                out.append(f)
            continue
        vals = [f["word"] for f, _ in group]
        common = 0
        for v in vals:
            common |= v ^ vals[0]
        lo = (common & -common).bit_length() - 1
        bits = common.bit_length() - lo
        kinds = {t[0][1] for t in trails if t}
        word = min(vals)
        ops = list(base["ops"][:len(base["ops"]) - len(trails[0])]) if trails[0] else list(base["ops"])
        # `sxtab r0, r1, r2, ror #8` and its three siblings: one instruction
        # with an optional rotation.
        if kinds == {"ror"}:
            ops.append(("Rotate", lo))
        elif kinds <= {"lsl", "asr"}:
            # `ssat`, `usat` and `pkhbt`: an optional shift whose kind is one
            # bit and whose amount is a field. A group that allows only `asr`
            # cannot say which bit means it, and `pkhtb`, the one instruction
            # that has such a group, is written out in OVERRIDE instead.
            amt = [t[1] for t in trails if len(t) > 1 and t[1][0] == "Imm"]
            if not amt:
                raise Unsupported("shift group %r" % (key,))
            f = amt[0][1]
            if kinds == {"asr"}:
                raise Unsupported("`asr`-only shift group %r" % (key,))
            asr = lo if "asr" in kinds else 255
            ops.append(("SatShift", f[0][0], f[0][1], asr, 255, 0))
        else:
            raise Unsupported("shift group %r %r" % (key, kinds))
        out.append(dict(base, word=word, ops=tuple(ops)))
    return out


def trailing_shift(ops):
    """The `, lsl #n` / `, ror #8` tail of an operand list, if it has one."""
    for i, op in enumerate(ops):
        if op[0] == "Shift":
            return tuple(ops[i:])
    return ()


# ============================================================================
# Building the table
# ============================================================================

# Which instruction sets a row's table serves. The coprocessor instructions
# are one table for both, because a T32 coprocessor instruction is the A32
# word with the condition field left at `al`, halfword by halfword.
SETS = {
    "arm": ["Arm"], "t16": ["T16"], "t32": ["T32"], "cop": ["Arm", "T32"],
    "vfp": ["Arm", "T32"], "neon": ["Arm", "T32"],
}

# A T32 NEON instruction is the A32 word with its top byte translated, which
# is the mapping print_insn_neon undoes to share one table between the two.
NEON_T32 = {0xF2: 0xEF, 0xF3: 0xFF, 0xF4: 0xF9}

# Spellings GNU as takes as another instruction in the table: the stack
# orders of `rfe` and `srs`, which are `ia` and `db` under other names, and
# `swi`, the pre-UAL name of `svc`.
ALIASES = [
    ("rfe", "rfeia"), ("rfeea", "rfedb"), ("rfeed", "rfeib"),
    ("rfefa", "rfeda"), ("rfefd", "rfeia"),
    ("srs", "srsia"), ("srsea", "srsia"), ("srsed", "srsda"),
    ("srsfa", "srsib"), ("srsfd", "srsdb"),
    ("swi", "svc"),
]


# NEON spellings GNU as takes that the disassembler prints as another
# instruction. Each is derived from the form it shares an encoding with, so
# the bits still come from the disassembler's own table.
def derived(forms):
    """The extra forms, given the ones the tables gave."""
    out = []
    for f in forms:
        name, ops = f["name"], f["ops"]
        stem, _, size = name.partition(".")
        kinds = tuple(o[0] for o in ops)
        # `vcle` is `vcge` with its sources the other way round, and so are
        # `vclt`, `vacle` and `vaclt`. GNU as swaps the operands and the
        # disassembler prints the instruction it made.
        swap = {"vcge": "vcle", "vcgt": "vclt",
                "vacge": "vacle", "vacgt": "vaclt"}
        if stem in swap and kinds == ("Vfp", "Vfp", "Vfp"):
            a, b = ops[1], ops[2]
            out.append(dict(f, name="%s.%s" % (swap[stem], size),
                            ops=(ops[0], (a[0], a[1], b[2]),
                                 (b[0], b[1], a[2]))))
        # `vshll` by less than the element size shifts by at least one: a
        # shift of none would be the widening move `vmovl`, which has an
        # encoding of its own.
        if stem == "vshll" and any(o[0] == "Imm" for o in ops):
            f["ops"] = tuple(("PosImm", o[1]) if o[0] == "Imm" else o
                             for o in ops)
        # The immediate shift left is one encoding whatever the type suffix
        # says, and `vshll` by the element size likewise.
        if stem == "vshl" and size.startswith("s") and "Imm" in kinds:
            for letter in "iu":
                out.append(dict(f, name="vshl.%s%s" % (letter, size[1:])))
        if stem == "vshll" and size.startswith("i") and "SizeImm" in kinds:
            for letter in "su":
                out.append(dict(f, name="vshll.%s%s" % (letter, size[1:])))
        # `vmov s0, s1` needs no type: a single-precision register says
        # which move it is. The `d` spelling is the NEON `vorr` instead.
        if name == "vmov.f32" and all(o[0] == "Vfp" and o[1] == 0 for o in ops):
            out.append(dict(f, name="vmov"))
        # `vzip.32` and `vuzp.32` on `d` registers are both `vtrn.32`, which
        # is what GNU as assembles them as.
        if name == "vtrn.32" and kinds == ("Vfp", "Vfp") and ops[0][1] == 3:
            for other in ("vzip.32", "vuzp.32"):
                out.append(dict(f, name=other,
                                ops=tuple((o[0], 1, o[2]) for o in ops)))
    return out


# The modified immediate: one encoding for `vmov`, `vmvn`, `vorr`, `vbic`
# and the two pseudo-instructions `vand` and `vorn`, whose `cmode` GNU as
# works out from the value written. The base word is the one the
# disassembler's rows share with `cmode` and `op` cleared, and
# `super::generic` fills those in the way `neon_cmode_for_move_imm` and
# `neon_cmode_for_logic_imm` do.
for _which, _base in (("Arm", 0xF2800010), ("T32", 0xEF800010)):
    for _class, _name in enumerate(
            ("vmov", "vmvn", "vorr", "vbic", "vand", "vorn")):
        for _size in range(4):
            _spell = "%s.i%d" % (_name, 8 << _size)
            EXTRA.append((_spell, _which, _base, False,
                          (("Vfp", 3, VD), ("NeonImm", _class, 8 << _size))))
            if _class >= 2:
                # The logic instructions also take the destination twice.
                EXTRA.append((_spell, _which, _base, False,
                              (("Vfp", 3, VD), ("VfpSame", 3, VD),
                               ("NeonImm", _class, 8 << _size))))
    # `vmov dd, dm` is `vorr dd, dm, dm`, which is what GNU as assembles it
    # as and what the disassembler prints it back as.
    EXTRA.append(("vmov", _which, (_base & 0xFF000000) | 0x00200110, False,
                  (("Vfp", 3, VD), ("VfpTwice", 3, VN, VM))))


# The structure transfers, `vld1` to `vld4` and `vst1` to `vst4`. Each has
# three encodings -- a run of whole registers, one element of each register,
# and one element copied over every lane -- which GNU as picks by the shape
# of the register list. The base words are the ones gas's own table carries
# (`NEON_ENC_TAB`); `super::generic` fills in the list length, the register
# stride, the element size, the lane and the alignment the way
# `do_neon_ldx_stx` does.
for _which, _top in (("Arm", 0xF4000000), ("T32", 0xF9000000)):
    for _n in (1, 2, 3, 4):
        for _load in (True, False):
            _stem = "%s%d" % ("vld" if _load else "vst", _n)
            for _size in (8, 16, 32, 64):
                _name = "%s.%d" % (_stem, _size)
                EXTRA.append((_name, _which,
                              _top | (0x00200000 if _load else 0), False,
                              (("NeonStruct", 0, _n, _size),)))
                if _size == 64:
                    continue
                EXTRA.append((_name, _which,
                              _top | (0x00A00000 if _load else 0x00800000)
                              | ((_n - 1) << 8), False,
                              (("NeonStruct", 1, _n, _size),)))
                if _load:
                    EXTRA.append((_name, _which,
                                  _top | 0x00A00C00 | ((_n - 1) << 8), False,
                                  (("NeonStruct", 2, _n, _size),)))


# `vmsr` refuses the program counter in both instruction sets, and the
# stack pointer in Thumb too, which `do_vmsr` checks by hand rather than
# through the table's own operand kinds.
VEC_CLASS_FIX = {
    ("vmsr", "Arm"): 1, ("vmsr", "T32"): 2,
}


def vector_operands(forms, insns):
    """Narrow the `d`-or-`q` operands GNU as says are one or the other, and
    add the two-operand spelling of the instructions whose middle operand it
    marks optional -- `vadd.i8 d0, d1` is `vadd.i8 d0, d0, d1`."""
    extra = []
    for form in forms:
        if form["set"] == "T16":
            continue
        ops = list(form["ops"])
        kinds = gas_kinds(form["name"], ops, insns)
        if kinds is None:
            continue
        reading = reading_ops(ops)
        where = {id(o): i for i, o in enumerate(ops)}
        for op, kind in zip(reading, kinds):
            if op[0] != "Vfp" or op[1] != 3:
                continue
            bare = bare_kind(kind)
            if bare == "RND":
                ops[where[id(op)]] = ("Vfp", 1, op[2])
            elif bare == "RNQ":
                ops[where[id(op)]] = ("Vfp", 2, op[2])
        form["ops"] = tuple(ops)
        # The middle operand of a vector instruction may be left out, and
        # then it is the destination: the two have to be the same kind of
        # register for that to mean anything.
        if (len(kinds) >= 3 and kinds[1].startswith("o")
                and ops[0][0] == "Vfp" and ops[1][0] == "Vfp"
                and ops[0][1] == ops[1][1]):
            short = (("VfpTwice", ops[0][1], ops[0][2], ops[1][2]),) \
                + tuple(ops[2:])
            extra.append(dict(form, ops=short))
    forms += extra


# The element sizes a NEON type has, which a row of the disassembler's table
# does not say: its two fields are independent there, so the rows spell
# `vneg.f8` and `vmul.p16` as readily as `vneg.f32` and `vmul.p8`. Within
# this backend's scope a NEON float is 32 bits (16-bit floats need the
# half-precision extension) and a polynomial is 8 (`p64` is ARMv8 crypto).
def bad_type(name):
    m = re.fullmatch(r"[a-z0-9]+\.([fp])(\d+)", name)
    if m:
        return m.group(2) != {"f": "32", "p": "8"}[m.group(1)]
    # The reversals fill a region with elements smaller than it is:
    # `vrev16` reverses bytes, `vrev32` bytes or halfwords.
    m = re.fullmatch(r"vrev(16|32|64)\.(\d+)", name)
    return bool(m) and int(m.group(2)) >= int(m.group(1))


def vext_split(forms):
    """`vext` takes four bits of immediate over quadword registers and three
    over double ones, which is one row in the disassembler's table because
    the fourth bit is simply zero there. The rows that spell out the
    combinations are dropped for the two an assembler needs."""
    out = []
    for form in forms:
        if form["name"] != "vext.8":
            out.append(form)
            continue
        # The row that fixes no immediate bit and no register width.
        if form["word"] & 0xF40:
            continue
        for wide in (False, True):
            ops = []
            for op in form["ops"]:
                if op[0] == "Vfp":
                    ops.append(("Vfp", 2 if wide else 1, op[2]))
                elif op[0] == "VfpTwice":
                    ops.append(("VfpTwice", 2 if wide else 1, op[2], op[3]))
                elif op[0] == "Imm":
                    ops.append(("Imm", ((8, 4 if wide else 3),), 1, 0))
                else:
                    ops.append(op)
            out.append(dict(form, word=form["word"] | (0x40 if wide else 0),
                            ops=tuple(ops)))
    return out


def build(entries, insns):
    """The forms, and the audit trail: one line per row saying where it
    went."""
    forms, audit = [], []
    for e in entries:
        mnem, optext = split_fmt(e["fmt"])
        why = None
        if any((e["set"], e["val"], e["mask"]) == row for row in DIS_ROWS):
            why = "disassembly only"
        elif "<" in optext:
            # The disassembler's angle brackets are not syntax: `vmsr <impl
            # def r0>` is how it prints a register GNU as has no name for.
            why = "disassembly only"
        elif e["feats"] and not (e["feats"] & FEATURES):
            why = "out of scope: %s" % ",".join(sorted(e["feats"]))
        elif e["set"] == "neon" and (optext in NEON_HAND
                                     or optext.endswith("%E")):
            why = "hand-written"
        if why:
            audit.append("%-4s %08x %-24s -- %s" % (e["set"], e["val"], mnem, why))
            continue
        try:
            names = expand_mnemonic(mnem, e["val"], e["mask"])
        except Unsupported as exc:
            audit.append("%-4s %08x %-24s -- ?? %s" % (e["set"], e["val"], mnem, exc))
            continue
        mine = [n for n in names if n[0] not in HAND and n[0] not in DIS
                and not (e["set"] == "neon" and bad_type(n[0]))]
        if not mine:
            kind = "hand-written" if any(n in HAND for n, _, _ in names) else (
                "disassembly only")
            audit.append("%-4s %08x %-24s -- %s" % (e["set"], e["val"], mnem, kind))
            continue
        ops = tuple(parse_ops(optext, e["set"], e["val"], e["mask"]))
        for name, word, cond in mine:
            for which in SETS[e["set"]]:
                w, has_cond = word, cond
                # An A32 word with `1111` in the condition field is one of
                # the unconditional instructions, whatever `%c` says: the
                # disassembler prints the field, and GNU as refuses a
                # condition on it.
                if which == "Arm" and w >> 28 == 0xF:
                    has_cond = False
                if e["set"] == "neon" and which == "T32":
                    # The conditional rows -- `vdup` from a core register --
                    # are the coprocessor space, where T32 leaves `al` in
                    # the condition field; the rest have a top byte of
                    # their own.
                    w, has_cond = NEON_T32.get(w >> 24, (w >> 24)
                                               | 0xE0) << 24 | (w & 0xFFFFFF), False
                elif e["set"] in ("cop", "vfp") and which == "T32" and cond:
                    # Thumb has no condition field: a T32 coprocessor
                    # instruction is the A32 word with `al` left in it.
                    w, has_cond = word | 0xE000_0000, False
                w |= WORD_FIX.get((name, which), 0)
                use = OVERRIDE.get((name, which), OVERRIDE.get((name, "*"), ops))
                forms.append(
                    {"name": name, "set": which, "word": w,
                     "cond": has_cond, "ops": use}
                )
        audit.append("%-4s %08x %-24s -> %s"
                     % (e["set"], e["val"], mnem,
                        " ".join(n for n, _, _ in mine)))
    for form in forms:
        # A vector register field an instruction holds twice is one operand
        # written twice, which has to be the same register both times.
        seen, ops = set(), []
        for op in form["ops"]:
            if op[0] == "Vfp" and op[2] in seen:
                ops.append(("VfpSame",) + op[1:])
            else:
                if op[0] == "Vfp":
                    seen.add(op[2])
                ops.append(op)
        form["ops"] = tuple(ops)
    for form in forms:
        if form["set"] in STREX.get(form["name"], ()):
            form["ops"] = tuple(form["ops"]) + (("FirstDistinct",),)
    forms += derived(forms)
    for name, which, word, cond, ops in EXTRA:
        forms.append({"name": name, "set": which, "word": word, "cond": cond,
                      "ops": ops})
    forms = merge(forms)
    vector_operands(forms, insns)
    forms = vext_split(forms)
    seen, out = set(), []
    for f in forms:
        key = (f["name"], f["set"], f["word"], tuple(f["ops"]))
        if key in seen:
            continue
        seen.add(key)
        out.append(f)
    order = {"T16": 0, "T32": 1, "Arm": 2}
    out.sort(key=lambda f: (f["name"], order[f["set"]], f["word"]))
    for form in out:
        form["regs"] = reg_classes(
            form["name"], form["ops"], form["set"] != "Arm", insns)
    return out, audit


# ============================================================================
# Writing the Rust
# ============================================================================

HEADER = '''//! The ARM instruction table, generated from GNU binutils 2.47.
//!
//! Do not edit: `tools/tables/arm.py table` writes this file from the
//! disassembler's own tables in `opcodes/arm-dis.c`. `super::generic`
//! encodes from it, and the script's doc comment says which instructions
//! are here and which a hand-written encoder owns.

/// Where a value's bits live in the instruction word, from the value's own
/// low bits up: `(lsb, width)` pieces. A 32-bit Thumb instruction is one
/// word with its first halfword on top, which is how `arm-dis.c` numbers
/// the bits too.
pub type Field = &'static [(u8, u8)];

/// Which instruction set a form belongs to, and how wide it is.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Set {
    /// A32, four bytes, with a condition field.
    Arm,
    /// T32, two bytes.
    T16,
    /// T32, four bytes.
    T32,
}

/// One operand of a form, and where its value goes.
#[derive(Copy, Clone, Debug)]
pub enum Op {
    /// A core register: `bits` bits at `lsb`.
    Reg(u8, u8),
    /// A core register whose four bits the encoding holds twice.
    RegTwice(u8, u8),
    /// The second half of a register pair, which must be the register after
    /// the one before it and carries no bits of its own.
    Next,
    /// A base register written `[rn]`.
    Base(u8, u8),
    /// An immediate `#v`, holding `v / scale - bias`.
    Imm(Field, u8, u8),
    /// A trailing `, {imm}`, which may be left out: the coprocessor opcode
    /// of `cdp`, `mcr` and `mrc`, written with or without the braces.
    OptImm(Field),
    /// `nop`'s hint number, which may be left out and needs its braces.
    Hint(Field),
    /// A VFP eight-bit floating-point immediate, which this assembler takes
    /// only as the number the field holds.
    VfpImm(Field),
    /// The `#lsb` of a bitfield instruction.
    Lsb(Field),
    /// Its `#width`, held as the most significant bit it reaches.
    Msb(Field),
    /// Its `#width`, held one less.
    Width(Field),
    /// An optional `, ror #8`, `#16` or `#24`, two bits at `lsb`.
    Rotate(u8),
    /// An optional shift: `amount` bits at `lsb`, and which kinds it may
    /// be -- under 32, the bit that means `asr` rather than `lsl`; 255,
    /// `lsl` alone; 64 plus a bit, `asr` alone, that bit being the one a
    /// shift of zero clears, since a zero shift is always `lsl`. A second
    /// field holds an amount the first cannot.
    SatShift(u8, u8, u8, u8, u8),
    /// A coprocessor number, `p0` to `p15`.
    Coproc(u8),
    /// A coprocessor register, `c0` to `c15`.
    CReg(u8),
    /// A barrier option, `sy` where none is written.
    Barrier,
    /// `APSR_nzcv`, which `mrc` takes in place of a register.
    ApsrNzcv,
    /// The two registers before it must be different, which is what the
    /// disassembler's `u` marker means where GNU as makes it an error.
    Distinct,
    /// The first register the form reads must differ from every other one:
    /// `do_strex`'s rule that the status register is none of the others.
    FirstDistinct,
    /// `!` on the register before it, the bit at `lsb`; 255 where the form
    /// always writes back and its own word says so.
    Writeback(u8),
    /// The `a`, `i` and `f` letters of `cpsie` and `cpsid`, the `f` bit at
    /// `lsb`.
    IntFlags(u8),
    /// `be` or `le`, the bit at `lsb`.
    Endian(u8),
    /// `[rn, rm]`, or `[rn, rm, lsl #1]` where the third field is 1: the
    /// table branches.
    IdxMem(u8, u8, u8),
    /// `[rn]` or `[rn, #imm]`, the base at the first position and the
    /// offset in the field, scaled.
    OffMem(u8, Field, u8),
    /// The addressing modes of `ldc` and `stc`.
    CoprocMem,
    /// A vector register: 0 single-precision, 1 double, 2 quadword, 3 either
    /// double or quadword by the bit the form's own encoding picks.
    Vfp(u8, Field),
    /// The same vector register as the operand that filled this field
    /// already: `vcvt.f32.s16 s0, s0, #4` writes it twice.
    VfpSame(u8, Field),
    /// The vector register after the one before it, which carries no bits:
    /// the second half of a `vmov` pair.
    VfpNext,
    /// A vector register with a lane index, `d0[1]`.
    VfpLane(u8, Field, Field),
    /// `{d0-d3}` or `{s0-s3}`: the first register, then how many.
    VfpList(u8, Field, Field),
    /// `[rn, #±imm8*4]`, the address of `vldr` and `vstr`.
    VfpMem,
    /// A `vcvt` fixed-point size, which the field holds as `32 - n` or
    /// `16 - n` by the bit that says which.
    VfpFix(Field),
    /// A shift amount the field counts down from its own width: `vshr.s8
    /// d0, d1, #1` fills the field, and `#8` leaves it empty.
    NegImm(Field),
    /// A shift amount of at least one, which the field holds as it is.
    PosImm(Field),
    /// The element width as an operand, which the field already holds: the
    /// value is `base << field`, `base` being 8, 16 or 32.
    SizeImm(Field, u8),
    /// A NEON scalar, `d0[1]`: bits 0-3 and bit 5 hold the register and the
    /// lane together, and the element size in bits 20-21 says where the
    /// line between them falls.
    Scalar,
    /// `vtbl`'s table list, `{d0-d3}`: the first register, then how many
    /// there are less one.
    TblList(Field, Field),
    /// An operand that has to be exactly this constant: the `#0` the NEON
    /// comparisons take.
    Fixed(u32),
    /// One vector register written into two fields: `vmov d0, d1` is
    /// `vorr d0, d1, d1`.
    VfpTwice(u8, Field, Field),
    /// The NEON modified immediate, as the instruction that takes it (0
    /// `vmov`, 1 `vmvn`, 2 `vorr`, 3 `vbic`, 4 `vand`, 5 `vorn`) and the
    /// element size in bits.
    NeonImm(u8, u8),
    /// A structure transfer's register list and address together: which of
    /// the three encodings this form is (0 whole registers, 1 one element
    /// of each, 2 one element over every lane), how many registers the
    /// structure has, and the element size in bits.
    NeonStruct(u8, u8, u8),
    /// The literal `#0.0` that `vcmp` compares against.
    Zero,
    /// A named system register: `fpscr` and its neighbours.
    Named(&'static str),
    /// `srs`'s base register, which may be left out and must be `sp`: the
    /// register goes at the first position and the `!` bit at the second.
    SpBase(u8, u8),
}

/// One way of writing one instruction.
pub struct Form {
    pub name: &'static str,
    pub set: Set,
    /// The opcode word, with every fixed bit already in place.
    pub word: u32,
    /// Whether the instruction may carry a condition. An A32 form that may
    /// not has its condition field in `word`.
    pub cond: bool,
    pub ops: &'static [Op],
    /// Which registers each operand may hold, in the order they are
    /// written: 0 any, 1 not the PC, 2 neither the PC nor the stack
    /// pointer, 3 the PC only where the operand is not written back, 255
    /// not a register at all. Empty where GNU as's own table says nothing.
    pub regs: &'static [u8],
}

const fn f(
    name: &'static str,
    set: Set,
    word: u32,
    cond: bool,
    ops: &'static [Op],
    regs: &'static [u8],
) -> Form {
    Form { name, set, word, cond, ops, regs }
}

/// Mnemonics GNU as takes as another spelling of one in the table.
pub static SPELLINGS: &[(&str, &str)] = &[
'''


def rust_field(f):
    return "&[%s]" % ", ".join("(%d, %d)" % p for p in f)


def rust_op(op):
    k = op[0]
    if k == "Reg":
        return "Op::Reg(%d, %d)" % (op[1], op[2])
    if k == "RegTwice":
        return "Op::RegTwice(%d, %d)" % (op[1], op[2])
    if k == "Base":
        return "Op::Base(%d, %d)" % (op[1], op[2])
    if k == "Next":
        return "Op::Next"
    if k == "Imm":
        return "Op::Imm(%s, %d, %d)" % (rust_field(op[1]), op[2], op[3])
    if k == "OptImm":
        return "Op::OptImm(%s)" % rust_field(op[1])
    if k == "Hint":
        return "Op::Hint(%s)" % rust_field(op[1])
    if k in ("Lsb", "Msb", "Width"):
        return "Op::%s(%s)" % (k, rust_field(op[1]))
    if k == "Rotate":
        return "Op::Rotate(%d)" % op[1]
    if k == "SatShift":
        return "Op::SatShift(%d, %d, %d, %d, %d)" % op[1:]
    if k in ("Coproc", "CReg", "Writeback", "IntFlags", "Endian"):
        return "Op::%s(%d)" % (k, op[1])
    if k == "NeonStruct":
        return "Op::NeonStruct(%d, %d, %d)" % (op[1], op[2], op[3])
    if k == "NeonImm":
        return "Op::NeonImm(%d, %d)" % (op[1], op[2])
    if k == "VfpTwice":
        return "Op::VfpTwice(%d, %s, %s)" % (op[1], rust_field(op[2]),
                                             rust_field(op[3]))
    if k in ("Vfp", "VfpSame"):
        return "Op::%s(%d, %s)" % (k, op[1], rust_field(op[2]))
    if k == "VfpLane":
        return "Op::VfpLane(%d, %s, %s)" % (op[1], rust_field(op[2]),
                                            rust_field(op[3]))
    if k == "VfpList":
        return "Op::VfpList(%d, %s, %s)" % (op[1], rust_field(op[2]),
                                            rust_field(op[3]))
    if k == "SizeImm":
        return "Op::SizeImm(%s, %d)" % (rust_field(op[1]), op[2])
    if k == "TblList":
        return "Op::TblList(%s, %s)" % (rust_field(op[1]), rust_field(op[2]))
    if k == "Fixed":
        return "Op::Fixed(%d)" % op[1]
    if k == "Scalar":
        return "Op::Scalar"
    if k in ("VfpFix", "VfpImm", "NegImm", "PosImm"):
        return "Op::%s(%s)" % (k, rust_field(op[1]))
    if k == "Named":
        return 'Op::Named("%s")' % op[1]
    if k in ("Barrier", "ApsrNzcv", "CoprocMem", "Distinct", "FirstDistinct",
             "VfpNext", "VfpMem", "Zero"):
        return "Op::%s" % k
    if k == "IdxMem":
        return "Op::IdxMem(%d, %d, %d)" % (op[1], op[2], op[3])
    if k == "SpBase":
        return "Op::SpBase(%d, %d)" % (op[1], op[2])
    if k == "OffMem":
        return "Op::OffMem(%d, %s, %d)" % (op[1], rust_field(op[2]), op[3])
    raise Unsupported("emit %r" % (op,))


def render(forms):
    out = [HEADER]
    for a, b in ALIASES:
        out.append('    ("%s", "%s"),\n' % (a, b))
    out.append("];\n\n")
    out.append("/// Every form, sorted by name.\n")
    out.append("pub static FORMS: &[Form] = &[\n")
    for form in forms:
        out.append(
            '    f("%s", Set::%s, %#010x, %s, &[%s], &[%s]),\n'
            % (form["name"], form["set"], form["word"],
               "true" if form["cond"] else "false",
               ", ".join(rust_op(o) for o in form["ops"]),
               ", ".join(str(c) for c in form["regs"] or ()))
        )
    out.append("];\n")
    text = "".join(out)
    try:
        text = subprocess.run(
            ["rustfmt", "--edition", "2024", "--emit", "stdout"],
            input=text, capture_output=True, text=True, check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as e:
        sys.exit("rustfmt failed (%s)" % e)
    return text


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd not in ("table", "check", "audit"):
        sys.exit(__doc__)
    entries = read_entries(os.path.join(BINUTILS, "opcodes", "arm-dis.c"))
    forms, audit = build(entries, read_insns(TC_ARM))
    if cmd == "audit":
        for line in audit:
            print(line)
        print("%d forms" % len(forms), file=sys.stderr)
        return
    text = render(forms)
    stale = not os.path.exists(TABLE) or open(TABLE).read() != text
    if cmd == "check":
        if stale:
            print("out of date: %s" % os.path.relpath(TABLE, ROOT))
        sys.exit(1 if stale else 0)
    if stale:
        with open(TABLE, "w") as fh:
            fh.write(text)
    print("rewrote %d file(s)" % stale, file=sys.stderr)


if __name__ == "__main__":
    main()
