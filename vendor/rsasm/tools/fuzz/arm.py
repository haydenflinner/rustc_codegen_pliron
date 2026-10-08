#!/usr/bin/env python3
"""Differential fuzzer for rsasm's ARM and Thumb backends.

Random instructions are generated from GNU binutils' own instruction
tables in `opcodes/arm-dis.c`, read at run time for its mnemonics and
operand *syntax* only -- never its encodings -- assembled by GNU as,
llvm-mc and rsasm, and compared byte for byte, relocations and
accept/reject status included. Every case goes into its own section, so
one object holds a whole batch.

    tools/fuzz/arm.py fuzz --count 20000
    tools/fuzz/arm.py fuzz --target thumb --only '^ldrd' --seed 7
    tools/fuzz/arm.py check --target arm <file>

The instructions whose syntax a disassembler's format string does not
spell -- the data-processing group's second operand, the addressing
modes, the register lists, the branches, `msr` and `mrs` -- come from
SHAPES below instead, written out by hand.

`fuzz` classifies each case:

    agree       all three assemblers produced the same result.
    rsasm       GNU as and llvm-mc agree and rsasm does not (or rsasm
                panicked). These are the findings.
    deviation   the references agree and rsasm differs on purpose, as a
                rule in DEVIATIONS explains.
    split       the references disagree. Listed with the one rsasm
                follows (gas, mc or neither). Where a rule in
                KNOWN_SPLITS explains the disagreement and rsasm follows
                the side the rule prefers, the case is counted but not
                listed; following neither side is an rsasm finding
                whatever the rule.

The report groups findings by mnemonic, mutation and accept/reject
pattern, and shows the shortest example of each. The exit status is 1
when there are rsasm findings.

Some cases are deliberately invalid (`--mutations`, the fraction
mutated): out-of-range and misaligned immediates, high registers where
only low ones fit, an odd register pair, a missing or extra operand, and
a width suffix the form cannot take.

Environment: RSASM (default target/debug/rsasm under the repository
root), RSASM_ORACLES (default target/oracles), which holds GNU as and
the binutils source; GAS and LLVM_MC override the assemblers.
"""

import collections
import os
import random
import re
import struct
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
ORACLES = os.environ.get("RSASM_ORACLES", os.path.join(ROOT, "target", "oracles"))
RSASM = os.environ.get("RSASM", os.path.join(ROOT, "target", "debug", "rsasm"))
GAS = os.environ.get("GAS", os.path.join(ORACLES, "bin", "arm-none-eabi-as"))
LLVM_MC = os.environ.get("LLVM_MC", "llvm-mc")
DIS = os.path.join(ORACLES, "src", "binutils-2.47", "opcodes", "arm-dis.c")

# `-march=armv7ve` is ARMv7-A with the security, virtualization and divide
# extensions, which is the whole set this backend claims; llvm-mc needs the
# same features named one at a time.
MC_ATTRS = ("+v7,+sec,+virtualization,+hwdiv,+hwdiv-arm,+dsp,+mp,"
            "+neon,+vfp4,+fp16")
FPU = "-mfpu=neon-vfpv4"
TARGETS = {
    "arm": (["-march=armv7ve", FPU], "armv7", "arm", False),
    "thumb": (["-march=armv7ve", FPU, "-mthumb"], "thumbv7", "thumb", True),
}
PRELUDE = ".syntax unified\n"

# The extensions this backend claims; `tools/tables/arm.py` has the same list.
FEATURES = {
    "ARM_EXT_V1", "ARM_EXT_V2", "ARM_EXT_V2S", "ARM_EXT_V3", "ARM_EXT_V3M",
    "ARM_EXT_V4", "ARM_EXT_V4T", "ARM_EXT_V5", "ARM_EXT_V5T", "ARM_EXT_V5E",
    "ARM_EXT_V5ExP", "ARM_EXT_V5J", "ARM_EXT_V6", "ARM_EXT_V6K",
    "ARM_EXT_V6T2", "ARM_EXT_V6Z", "ARM_EXT_V7", "ARM_EXT_DIV",
    "ARM_EXT_ADIV", "ARM_EXT_MP", "ARM_EXT_SEC", "ARM_EXT_VIRT",
    "ARM_EXT2_V6T2_V8M",
    # The floating-point unit and NEON, up to VFPv4: `-mfpu=neon-vfpv4`.
    "FPU_VFP_EXT_V1xD", "FPU_VFP_EXT_V1", "FPU_VFP_EXT_V2", "FPU_VFP_EXT_V3",
    "FPU_VFP_EXT_V3xD", "FPU_VFP_EXT_FMA", "FPU_VFP_EXT_FP16",
    "FPU_NEON_EXT_V1", "FPU_NEON_EXT_FMA",
}

# Mnemonics whose syntax SHAPES spells out, so the rows for them are skipped.
BY_HAND = set("""
    and eor sub rsb add adc sbc rsc tst teq cmp cmn orr mov bic mvn orn neg
    lsl lsr asr ror rrx addw subw movw movt mul mla mls
    umull umlal smull smlal
    ldr str ldrb strb ldrh strh ldrsb ldrsh ldrd strd
    ldrt strt ldrbt strbt ldrht strht ldrsbt ldrsht
    ldm ldmia ldmib ldmda ldmdb stm stmia stmib stmda stmdb
    ldmfd ldmfa ldmed ldmea stmfd stmfa stmed stmea push pop
    b bl bx blx cbz cbnz adr it msr mrs pld pldw pli
    tstp teqp cmpp cmnp nop
""".split())

# ============================================================================
# Reading the instruction tables
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
FIELD = re.compile(r"%(\d+)(?:-(\d+))?(.)")
# `%12-15,22D`: a value the encoding holds in several pieces.
MULTI = re.compile(r"%(\d+(?:-\d+)?(?:,\d+(?:-\d+)?)+)(.)")


class Unsupported(Exception):
    pass


def read_rows(path):
    lines = open(path).read().split("\n")
    out = []
    for head, which in TABLES:
        i = next(n for n, l in enumerate(lines) if l.startswith(head))
        j = i
        while not lines[j].startswith("};"):
            j += 1
        body = re.sub(r"/\*.*?\*/", "", "\n".join(lines[i + 1:j]), flags=re.S)
        for m in ENTRY.finditer(body):
            fmt = "".join(re.findall(r'"((?:[^"\\]|\\.)*)"', m.group(4)))
            feats = set(re.findall(r"(?:ARM_EXT|ARM_CEXT|FPU_)\w*", m.group(1)))
            if feats and not (feats & FEATURES):
                continue
            out.append((which, fmt))
    return out


def expand_mnemonic(mnem):
    """The spellings a mnemonic pattern stands for, and whether it takes a
    condition. Only the names matter here, never the bits."""
    out, cond = [""], False
    i = 0
    while i < len(mnem):
        if mnem[i] != "%":
            out = [n + mnem[i] for n in out]
            i += 1
            continue
        if mnem[i:i + 2] == "%c":
            cond, i = True, i + 2
            continue
        if mnem[i + 1] in "CptwI":
            raise Unsupported("mnemonic %r" % mnem)
        if mnem[i:i + 2] == "%u":
            # The NEON rows that cannot be conditional in ARM state; all of
            # them are out of scope anyway.
            raise Unsupported("mnemonic %r" % mnem)
        m = MULTI.match(mnem, i)
        if m and m.group(2) == "?":
            pieces = [p.split("-") for p in m.group(1).split(",")]
            bits = sum(1 if len(q) == 1 else abs(int(q[0]) - int(q[1])) + 1
                       for q in pieces)
            i = m.end()
            letters, i = mnem[i:i + (1 << bits)], i + (1 << bits)
            out = [n + ch for ch in set(letters) for n in out]
            continue
        m = FIELD.match(mnem, i)
        if not m:
            raise Unsupported("mnemonic %r" % mnem)
        lo = int(m.group(1))
        hi = int(m.group(2)) if m.group(2) else lo
        bits = abs(hi - lo) + 1
        code, i = m.group(3), m.end()
        if code in "'`":
            ch, i = mnem[i], i + 1
            out = [n + ch for n in out] + list(out)
        elif code == "?":
            letters, i = mnem[i:i + (1 << bits)], i + (1 << bits)
            out = [n + ch for ch in set(letters) for n in out]
        elif code == "c":
            cond = True
        elif code in "STU":
            # The element width the field picks, written into the mnemonic:
            # `S` counts from 8 bits, `T` from 16 and `U` from 32, and the
            # limit digit says which of the values are legal.
            base = 8 << "STU".index(code)
            limit = int(mnem[i], 16)
            i += 1
            out = [n + str(base << v)
                   for v in range(limit >> 2, (limit & 3) + 1) for n in out]
        else:
            raise Unsupported("mnemonic %r" % mnem)
    return sorted({n.replace(".w", "").replace(".n", "").strip() for n in out}), cond


class Slot:
    """One thing the generator has to write: a register, an immediate, a
    keyword or a whole addressing mode."""

    __slots__ = ("kind", "lo", "hi", "step")

    def __init__(self, kind, lo=0, hi=0, step=1):
        self.kind, self.lo, self.hi, self.step = kind, lo, hi, step


def parse_syntax(text, which):
    """The operand half of a format string, as literal text with `{}` holes
    and the slots that fill them."""
    out, slots = [], []
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

    def imm_slot(body):
        """`#%<lo>-<hi><code>`, as the range it can hold."""
        f = FIELD.fullmatch(body)
        if not f:
            return Slot("fixed", int(body)) if re.fullmatch(r"\d+", body) else None
        lo = int(f.group(1))
        hi = int(f.group(2)) if f.group(2) else lo
        bits = abs(hi - lo) + 1
        code = f.group(3)
        if code == "W":
            return (Slot("imm", 1, 1 << bits) if which in ("arm", "cop")
                    else Slot("imm", 0, ((1 << bits) - 1) * 4, 4))
        if code == "D":
            return Slot("imm", 1, 1 << bits)
        if code == "H":
            return Slot("imm", 0, ((1 << bits) - 1) * 2, 2)
        if code in "dxX":
            return Slot("imm", 0, (1 << bits) - 1)
        return None

    # The coprocessor table's own names for a vector register: a single, a
    # double, a list of either, and the `%y4` pair written out as both.
    VFP = {"y0": "vfps", "y1": "vfps", "y2": "vfps", "y4": "vfpspair",
           "z0": "vfpd", "z1": "vfpd", "z2": "vfpd",
           "y3": "slist", "z3": "dlist"}

    while i < n:
        ch = text[i]
        if text[i:i + 2] == "%y" or text[i:i + 2] == "%z":
            code, i = text[i + 1:i + 3], i + 3
            if code not in VFP:
                raise Unsupported("register %%%s" % code)
            kind = VFP[code]
            m = re.compile(r"\[%\{I:%([\d,-]+)d%\}\]").match(text, i)
            if m:
                i = m.end()
                kind = "vfpdlane"
            slots.append(Slot(kind))
            out.append("{}")
            continue
        m = MULTI.match(text, i)
        if m and m.group(2) in "DQRr":
            i = m.end()
            slots.append(Slot({"D": "vfpd", "Q": "vfpq", "R": "vfpr",
                               "r": "reg"}[m.group(2)]))
            out.append("{}")
            continue
        if ch in ", []{}":
            out.append(ch)
            i += 1
            continue
        if text.startswith("ROR ", i):
            out.append("ror ")
            i += 4
            continue
        if text.startswith("%{", i):
            kind = text[i + 2]
            end = text.index("%}", i)
            body, i = text[i + 4:end], end + 2
            if kind == "I":
                if body == "#0.0":
                    # This assembler's lexer has no floating-point literal,
                    # so the zero `vcmp` compares against is written `#0`.
                    out.append("#0")
                    continue
                if re.fullmatch(r"#%[\d,-]+E", body):
                    # A VFP floating-point immediate, which needs a literal
                    # this assembler does not read.
                    raise Unsupported("a floating-point immediate")
                if re.fullmatch(r"#%[\d,-]+k", body):
                    slots.append(Slot("vfpfix"))
                    out.append("#{}")
                    continue
                m = re.fullmatch(r"#%(\d+)-(\d+)e", body)
                if m:
                    lo, hi = int(m.group(1)), int(m.group(2))
                    slots.append(Slot("imm", 1, 1 << (abs(hi - lo) + 1)))
                    out.append("#{}")
                    continue
                if re.fullmatch(r"#%\d+-\d+[STU][0-9a-f]", body):
                    slots.append(Slot("sizeimm"))
                    out.append("#{}")
                    continue
                if body == "#%e":
                    slots.append(Slot("imm", 0, 0xFFFF))
                    out.append("#{}")
                    continue
                if re.fullmatch(r"0x(%\d+-\d+X){4}", body):
                    slots.append(Slot("imm", 0, 0xFFFF))
                    out.append("#{}")
                    continue
                if not body.startswith("#"):
                    f = FIELD.fullmatch(body)
                    if not f:
                        raise Unsupported("immediate %r" % body)
                    lo = int(f.group(1))
                    if lo == 8:
                        slots.append(Slot("coproc"))
                        out.append("{}")
                        continue
                    hi = int(f.group(2)) if f.group(2) else lo
                    slots.append(Slot("imm", 0, (1 << (hi - lo + 1)) - 1))
                    out.append("{}")
                    continue
                s = imm_slot(body[1:])
                if s is None:
                    raise Unsupported("immediate %r" % body)
                if s.kind == "fixed":
                    out.append("#%d" % s.lo)
                else:
                    slots.append(s)
                    out.append("#{}")
                continue
            if kind == "R":
                if body == "APSR_nzcv":
                    out.append("APSR_nzcv")
                    continue
                if re.fullmatch(r"[a-z][a-z0-9_]*", body):
                    # A named system register: `fpscr` and its neighbours.
                    out.append(body)
                    continue
                m = re.fullmatch(r"s%([\d,-]+)d", body)
                if m:
                    slots.append(Slot("vfps"))
                    out.append("{}")
                    continue
                m = re.fullmatch(r"d%([\d,-]+)d", body)
                if m:
                    slots.append(Slot("vfpd"))
                    out.append("{}")
                    continue
                m = re.fullmatch(r"(?:d%[\d,-]+d|%[\d,-]+D)\[%([\d,-]+)d\]", body)
                if m:
                    slots.append(Slot("vfpdlane"))
                    out.append("{}")
                    continue
                m = re.fullmatch(r"cr%(\d+)-(\d+)d", body)
                if m:
                    slots.append(Slot("creg"))
                    out.append("{}")
                    continue
                m = re.fullmatch(r"r%(\d+)-(\d+)d", body)
                if m:
                    slots.append(Slot("reg", 4))
                    out.append("{}")
                    continue
                raise Unsupported("register %r" % body)
            if kind == "B":
                if body in ("lsl", "asr", "ror"):
                    out.append(body + " ")
                    continue
                if re.fullmatch(r"%(\d+)'a%(\d+)'i%(\d+)'f", body):
                    slots.append(Slot("iflags"))
                    out.append("{}")
                    continue
                if re.fullmatch(r"%(\d+)\?ble", body):
                    slots.append(Slot("endian"))
                    out.append("{}")
                    continue
            raise Unsupported("%%{%s:%s" % (kind, body))
        if ch != "%":
            raise Unsupported("literal %r in %r" % (ch, text))
        if text[i + 1].isdigit():
            lo, hi, code = bitfield()
            while i < n and text[i] in "uU":
                i += 1
            if code in "rRS":
                slots.append(Slot("reg", hi - lo + 1))
                out.append("{}")
            elif code == "T":
                slots.append(Slot("next"))
                out.append("{}")
            elif code in "dx":
                slots.append(Slot("imm", 0, (1 << (hi - lo + 1)) - 1))
                out.append("#{}")
            elif code == "'":
                ch, i = text[i], i + 1
                if ch != "!":
                    raise Unsupported("flag %r" % ch)
                slots.append(Slot("bang"))
                out.append("{}")
            else:
                raise Unsupported("field code %r" % code)
            continue
        code = text[i + 1]
        i += 2
        if code == "e":
            slots.append(Slot("imm", 0, 0xFFFF))
            out.append("#{}")
        elif code in "VHK":
            slots.append(Slot("imm", 0, 0xFFFF))
            out.append("#{}")
        elif code == "E":
            slots.append(Slot("bitfield"))
            out.append("{}")
        elif code == "F":
            slots.append(Slot("bitfield"))
            out.append("{}")
        elif code == "U":
            slots.append(Slot("barrier"))
            out.append("{}")
        elif code == "R" and which == "t32":
            slots.append(Slot("rotate"))
            out.append("{}")
        elif code == "s" and which == "t32":
            slots.append(Slot("satshift"))
            out.append("{}")
        elif code == "A" and which == "vfp":
            slots.append(Slot("vfpmem"))
            out.append("{}")
        elif code == "D" and which == "neon":
            slots.append(Slot("scalar"))
            out.append("{}")
        elif code == "F" and which == "neon":
            slots.append(Slot("tbllist"))
            out.append("{}")
        elif code == "B" and which == "vfp":
            slots.append(Slot("dlist"))
            out.append("{}")
        elif code in "ABCE" and which == "neon":
            # The modified immediate and the structure transfers, whose
            # syntax SHAPES spells out.
            raise Unsupported("code %%%s" % code)
        elif code == "A":
            slots.append(Slot("coprocmem"))
            out.append("{}")
        elif code in "xX":
            pass
        else:
            raise Unsupported("code %%%s" % code)
    return "".join(out), slots


class Form:
    __slots__ = ("mnem", "cond", "template", "slots", "targets")

    def __init__(self, mnem, cond, template, slots, targets):
        self.mnem, self.cond = mnem, cond
        self.template, self.slots, self.targets = template, slots, targets


SETS = {"arm": ("arm",), "t16": ("thumb",), "t32": ("thumb",),
        "cop": ("arm", "thumb"), "vfp": ("arm", "thumb"),
        "neon": ("arm", "thumb")}


def table_forms():
    forms, seen = [], set()
    for which, fmt in read_rows(DIS):
        parts = fmt.split("\\t")
        head = parts[0]
        body = (parts[1] if len(parts) > 1 else "").split("@")[0].strip()
        try:
            names, cond = expand_mnemonic(head)
        except Unsupported:
            continue
        names = [n for n in names if n not in BY_HAND]
        if not names:
            continue
        try:
            template, slots = parse_syntax(body, which)
        except Unsupported:
            continue
        for name in names:
            key = (name, which, template)
            if key in seen:
                continue
            seen.add(key)
            forms.append(Form(name, cond, template, slots, SETS[which]))
    return forms


# ============================================================================
# The instructions whose syntax has to be written out
# ============================================================================

# Each shape is (mnemonics, takes a condition, template, slot kinds, targets).
# `S` in a mnemonic is the flag-setting suffix the generator adds at random.
DP = "and eor sub rsb add adc sbc orr bic"
NEON_IMM_MOVE = " ".join("%s.i%d" % (m, s) for m in ("vmov", "vmvn")
                         for s in (8, 16, 32, 64))
NEON_IMM_LOGIC = " ".join("%s.i%d" % (m, s)
                          for m in ("vorr", "vbic", "vand", "vorn")
                          for s in (8, 16, 32, 64))
NEON_LOADS = " ".join("vld%d.%d" % (n, s) for n in (1, 2, 3, 4)
                      for s in (8, 16, 32, 64))
NEON_STRUCT = NEON_LOADS + " " + " ".join(
    "vst%d.%d" % (n, s) for n in (1, 2, 3, 4) for s in (8, 16, 32, 64))
SHAPES = [
    (DP + " rsc", True, "{}, {}, {}", ["regS", "reg", "op2"], ("arm",)),
    (DP + " rsc", True, "{}, {}", ["regS", "op2"], ("arm",)),
    (DP + " orn", True, "{}, {}, {}", ["regS", "reg", "op2t"], ("thumb",)),
    (DP + " orn", True, "{}, {}", ["regS", "op2t"], ("thumb",)),
    ("mov mvn", True, "{}, {}", ["regS", "op2"], ("arm",)),
    ("mov mvn", True, "{}, {}", ["regS", "op2t"], ("thumb",)),
    ("cmp cmn tst teq", True, "{}, {}", ["reg", "op2"], ("arm",)),
    ("cmp cmn tst teq", True, "{}, {}", ["reg", "op2t"], ("thumb",)),
    ("neg", True, "{}, {}", ["regS", "reg"], ("arm", "thumb")),
    ("lsl lsr asr ror", True, "{}, {}, {}", ["regS", "reg", "shamt"],
     ("arm", "thumb")),
    ("lsl lsr asr ror", True, "{}, {}", ["regS", "shamt"], ("arm", "thumb")),
    ("rrx", True, "{}, {}", ["regS", "reg"], ("arm", "thumb")),
    ("movw movt", True, "{}, #{}", ["reg", "imm16"], ("arm", "thumb")),
    ("addw subw", True, "{}, {}, #{}", ["reg", "reg", "imm12"], ("thumb",)),
    ("mul", True, "{}, {}, {}", ["regS", "reg", "reg"], ("arm", "thumb")),
    ("mla", True, "{}, {}, {}, {}", ["regS", "reg", "reg", "reg"],
     ("arm", "thumb")),
    ("mls", True, "{}, {}, {}, {}", ["reg", "reg", "reg", "reg"],
     ("arm", "thumb")),
    ("umull umlal smull smlal", True, "{}, {}, {}, {}",
     ["regS", "reg", "reg", "reg"], ("arm", "thumb")),
    # Loads and stores.
    ("ldr str ldrb strb ldrh strh ldrsb ldrsh", True, "{}, {}",
     ["reg", "mem"], ("arm", "thumb")),
    ("ldrt strt ldrbt strbt ldrht strht ldrsbt ldrsht", True, "{}, {}",
     ["reg", "memt"], ("arm", "thumb")),
    ("ldrd strd", True, "{}, {}, {}", ["evenreg", "next", "memd"],
     ("arm", "thumb")),
    ("ldrd strd", True, "{}, {}", ["evenreg", "memd"], ("arm", "thumb")),
    ("pld pldw pli", False, "{}", ["mem"], ("arm", "thumb")),
    ("ldm ldmia ldmib ldmda ldmdb stm stmia stmib stmda stmdb", True,
     "{}{}, {}", ["reg", "bang", "reglist"], ("arm", "thumb")),
    ("push pop", True, "{}", ["reglist"], ("arm", "thumb")),
    # Branches, which resolve against a label right after them.
    ("b bl", True, "{}", ["label"], ("arm", "thumb")),
    ("blx", False, "{}", ["label"], ("arm", "thumb")),
    ("bx blx", True, "{}", ["reg"], ("arm", "thumb")),
    ("cbz cbnz", False, "{}, {}", ["lowreg", "label"], ("thumb",)),
    ("adr", True, "{}, {}", ["reg", "label"], ("arm", "thumb")),
    ("adrl", True, "{}, {}", ["reg", "label"], ("arm",)),
    ("nop", True, "", [], ("arm", "thumb")),
    # The status registers.
    ("mrs", True, "{}, {}", ["reg", "psr"], ("arm", "thumb")),
    ("msr", True, "{}, {}", ["psrw", "reg"], ("arm", "thumb")),
    ("msr", True, "{}, #{}", ["psrw", "modimm"], ("arm",)),
    # The NEON modified immediate, whose `cmode` an assembler works out from
    # the value, and the register move that is `vorr rd, rm, rm`.
    (NEON_IMM_MOVE, False, "{}, #{}", ["vfpr", "neonimm"], ("arm", "thumb")),
    (NEON_IMM_LOGIC, False, "{}, #{}", ["vfpr", "neonimm"], ("arm", "thumb")),
    (NEON_IMM_LOGIC, False, "{}, {}, #{}", ["vfpr", "same", "neonimm"],
     ("arm", "thumb")),
    ("vmov", False, "{}, {}", ["vfpr", "vfpr"], ("arm", "thumb")),
    # The structure transfers, in each of their three shapes.
    (NEON_STRUCT, False, "{}, {}", ["structlist", "structaddr"],
     ("arm", "thumb")),
    (NEON_STRUCT, False, "{}, {}", ["lanelist", "structaddr"],
     ("arm", "thumb")),
    (NEON_LOADS, False, "{}, {}", ["duplist", "structaddr"],
     ("arm", "thumb")),
]


def shape_forms():
    out = []
    for mnems, cond, template, slots, targets in SHAPES:
        for m in mnems.split():
            out.append(Form(m, cond, template, [Slot(k) for k in slots], targets))
    return out


# ============================================================================
# Writing one instruction
# ============================================================================

CONDS = ["eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge",
         "lt", "gt", "le", "al", "hs", "lo"]
BARRIERS = ["sy", "st", "ish", "ishst", "nsh", "nshst", "osh", "oshst", "un",
            "unst"]
PSRS = ["cpsr", "spsr", "apsr", "sp_usr", "lr_usr", "r8_fiq", "r12_fiq",
        "spsr_fiq", "lr_irq", "sp_svc", "spsr_abt", "lr_und", "sp_mon",
        "elr_hyp", "sp_hyp", "spsr_hyp"]
PSR_WRITE = PSRS + ["cpsr_f", "cpsr_c", "cpsr_fsxc", "spsr_cxsf", "cpsr_all",
                    "cpsr_flg", "cpsr_ctl", "apsr_nzcvq", "apsr_g",
                    "apsr_nzcvqg"]
SHIFTS = ["lsl", "lsr", "asr", "ror"]
MODIMM = [0, 1, 0xFF, 0x100, 0xFF00, 0xFF000000, 0xF000000F, 0x1FE, 0x101,
          0xABABABAB, 0xAB00AB00, 0x00AB00AB, 300, 0x12C, -1, -2, -300]


class Case:
    __slots__ = ("text", "form", "mutation", "target")

    def signature(self):
        return f"{self.form.mnem} {self.form.template}"


def rnum(rng, lo, hi, step=1):
    return rng.randrange(lo, hi + 1, step)


def write_reg(rng, bits=4, low=False):
    n = rng.randrange(8 if low or bits == 3 else 16)
    return f"r{n}"


def operand2(rng, thumb):
    """A data-processing second operand, which may be shifted."""
    r = rng.random()
    if r < 0.35:
        return "#%d" % rng.choice(MODIMM)
    if r < 0.6:
        return write_reg(rng)
    shift = rng.choice(SHIFTS)
    if not thumb and rng.random() < 0.25:
        return f"{write_reg(rng)}, {shift} {write_reg(rng)}"
    if rng.random() < 0.1:
        return f"{write_reg(rng)}, rrx"
    hi = 32 if shift in ("lsr", "asr") else 31
    return f"{write_reg(rng)}, {shift} #{rnum(rng, 0, hi)}"


def address(rng, thumb, kind):
    """One of the addressing modes, in the spellings the sources use."""
    base = write_reg(rng)
    if kind == "memt":
        if thumb:
            return f"[{base}, #{rnum(rng, 0, 255)}]"
        return f"[{base}], #{rnum(rng, -255, 255)}"
    if kind == "memd":
        off = rnum(rng, -1020, 1020, 4)
        form = rng.random()
        if form < 0.3:
            return f"[{base}]"
        if form < 0.55:
            return f"[{base}, #{off}]"
        if form < 0.75:
            return f"[{base}, #{off}]!"
        if form < 0.9 or thumb:
            return f"[{base}], #{off}"
        return f"[{base}, {'-' if rng.random() < 0.3 else ''}{write_reg(rng)}]"
    form = rng.random()
    if form < 0.2:
        return f"[{base}]"
    if form < 0.45:
        return f"[{base}, #{rnum(rng, -255, 4095)}]"
    if form < 0.6:
        return f"[{base}, #{rnum(rng, -255, 255)}]!"
    if form < 0.75:
        return f"[{base}], #{rnum(rng, -255, 255)}"
    sign = "-" if not thumb and rng.random() < 0.3 else ""
    if form < 0.9:
        return f"[{base}, {sign}{write_reg(rng)}]"
    amount = rnum(rng, 0, 3) if thumb else rnum(rng, 1, 31)
    return f"[{base}, {sign}{write_reg(rng)}, lsl #{amount}]"


def reglist(rng, thumb, caret=True):
    mask = 0
    for _ in range(rng.randrange(1, 5)):
        mask |= 1 << rng.randrange(8 if rng.random() < 0.6 else 16)
    names, i = [], 0
    while i < 16:
        if mask & (1 << i):
            j = i
            while j + 1 < 16 and mask & (1 << (j + 1)):
                j += 1
            names.append(f"r{i}" if i == j else f"r{i}-r{j}")
            i = j
        i += 1
    tail = "^" if not thumb and caret and rng.random() < 0.15 else ""
    return "{" + ", ".join(names) + "}" + tail


# The values a NEON modified immediate is worth trying: the byte patterns
# the encoding holds, ones that only their complement holds, and a few that
# nothing holds.
NEON_IMMS = [0, 1, 0xFF, 0x100, 0xFF00, 0xFF0000, 0xFF000000, 0xFFFF,
             0xFFFFFF, 0x1FF, 0x1FFFF, 0xABAB, 0x12345678, 0xFF00FF00,
             0xFFFFFFFFFFFFFFFF, 0xFF00FF00FF00FF00, 0x7F, -1, -256]


def vec_list(rng, letter, count=None, lane=None, stride=1, dash=None):
    """`{d0-d3}`, `{d0, d2}` or `{d0[1], d1[1]}`, the shapes a vector list
    is written in."""
    count = count or rng.randrange(1, 5)
    first = rng.randrange(0, 32 - count * stride + 1)
    tail = "" if lane is None else ("[]" if lane == "all" else "[%d]" % lane)
    if dash is None:
        dash = stride == 1 and lane is None and rng.random() < 0.5
    if dash:
        return "{%s%d-%s%d}" % (letter, first, letter, first + count - 1)
    return "{%s}" % ", ".join(
        "%s%d%s" % (letter, first + i * stride, tail) for i in range(count))


def struct_address(rng):
    base = write_reg(rng)
    align = rng.choice([None, None, 16, 32, 64, 128, 256])
    inside = "%s%s" % (base, "" if align is None else ":%d" % align)
    form = rng.random()
    if form < 0.5:
        return "[%s]" % inside
    if form < 0.75:
        return "[%s]!" % inside
    return "[%s], %s" % (inside, write_reg(rng))


def fill(rng, form, target):
    thumb = TARGETS[target][3]
    # Every `d`-or-`q` operand of one instruction is the same width, and the
    # element size a structure transfer names says which lanes it has.
    quad = rng.random() < 0.5
    tail = re.match(r"[a-z]*(\d+)$", form.mnem.rpartition(".")[2])
    size = int(tail.group(1)) if tail else 8
    last_vec = 0
    args, last_reg = [], 0
    for slot in form.slots:
        k = slot.kind
        if k == "reg":
            bits = slot.lo if slot.lo else 4
            r = write_reg(rng, bits)
            last_reg = int(r[1:])
            args.append(r)
        elif k == "regS":
            r = write_reg(rng)
            last_reg = int(r[1:])
            args.append(r)
        elif k == "lowreg":
            args.append(write_reg(rng, low=True))
        elif k == "evenreg":
            last_reg = rng.randrange(0, 12, 2)
            args.append(f"r{last_reg}")
        elif k == "next":
            args.append(f"r{last_reg + 1}")
        elif k == "imm":
            args.append(str(rnum(rng, slot.lo, slot.hi, slot.step)))
        elif k == "imm16":
            args.append(str(rnum(rng, 0, 0xFFFF)))
        elif k == "imm12":
            args.append(str(rnum(rng, 0, 0xFFF)))
        elif k == "modimm":
            args.append(str(rng.choice(MODIMM)))
        elif k == "coproc":
            args.append(f"p{rng.randrange(16)}")
        elif k == "creg":
            args.append(f"c{rng.randrange(16)}")
        elif k == "barrier":
            args.append(rng.choice(BARRIERS) if rng.random() < 0.85 else "")
        elif k == "iflags":
            args.append("".join(c for c in "aif" if rng.random() < 0.6) or "if")
        elif k == "endian":
            args.append(rng.choice(["be", "le"]))
        elif k == "bang":
            args.append("!" if rng.random() < 0.6 else "")
        elif k == "rotate":
            args.append(f", ror #{rng.choice([0, 8, 16, 24])}"
                        if rng.random() < 0.5 else "")
        elif k == "satshift":
            if rng.random() < 0.5:
                args.append("")
            else:
                s = rng.choice(["lsl", "asr"])
                args.append(f", {s} #{rnum(rng, 1, 31)}")
        elif k == "bitfield":
            lsb = rnum(rng, 0, 31)
            args.append(f"#{lsb}, #{rnum(rng, 1, 32 - lsb)}")
        elif k == "coprocmem":
            base = write_reg(rng)
            form_ = rng.random()
            if form_ < 0.3:
                args.append(f"[{base}]")
            elif form_ < 0.55:
                args.append(f"[{base}, #{rnum(rng, -1020, 1020, 4)}]")
            elif form_ < 0.75:
                args.append(f"[{base}, #{rnum(rng, -1020, 1020, 4)}]!")
            elif form_ < 0.9:
                args.append(f"[{base}], #{rnum(rng, -1020, 1020, 4)}")
            else:
                args.append("[%s], {%d}" % (base, rnum(rng, 0, 255)))
        elif k in ("op2", "op2t"):
            args.append(operand2(rng, k == "op2t"))
        elif k == "shamt":
            if rng.random() < 0.4:
                args.append(write_reg(rng))
            else:
                args.append(f"#{rnum(rng, 0, 32)}")
        elif k in ("mem", "memt", "memd"):
            args.append(address(rng, thumb, k))
        elif k == "reglist":
            args.append(reglist(rng, thumb, form.mnem not in ("push", "pop")))
        elif k == "vfps":
            args.append("s%d" % rng.randrange(32))
        elif k == "vfpspair":
            n = rng.randrange(31)
            args.append("s%d, s%d" % (n, n + 1))
        elif k == "vfpd":
            args.append("d%d" % rng.randrange(32))
        elif k == "vfpq":
            args.append("q%d" % rng.randrange(16))
        elif k == "vfpr":
            last_vec = rng.randrange(16 if quad else 32)
            args.append("%s%d" % ("q" if quad else "d", last_vec))
        elif k == "same":
            args.append("%s%d" % ("q" if quad else "d", last_vec))
        elif k == "vfpdlane":
            args.append("d%d[%d]" % (rng.randrange(32), rng.randrange(8)))
        elif k == "scalar":
            args.append("d%d[%d]" % (rng.randrange(16), rng.randrange(4)))
        elif k == "slist":
            args.append(vec_list(rng, "s"))
        elif k in ("dlist", "tbllist"):
            args.append(vec_list(rng, "d"))
        elif k == "vfpmem":
            args.append("[%s, #%d]" % (write_reg(rng), rnum(rng, -1020, 1020, 4)))
        elif k == "vfpfix":
            args.append(str(rnum(rng, 1, 32)))
        elif k == "sizeimm":
            args.append(str(rng.choice([8, 16, 32])))
        elif k == "neonimm":
            args.append(str(rng.choice(NEON_IMMS)))
        elif k == "structlist":
            args.append(vec_list(rng, "d", stride=rng.choice([1, 1, 2])))
        elif k == "lanelist":
            n = int(form.mnem[3])
            lanes = max(1, 64 // size)
            args.append(vec_list(rng, "d", count=n, lane=rng.randrange(lanes),
                                 stride=rng.choice([1, 1, 2]), dash=False))
        elif k == "duplist":
            n = int(form.mnem[3])
            args.append(vec_list(rng, "d", count=rng.choice([n, n, 1, 2]),
                                 lane="all", stride=rng.choice([1, 1, 2]),
                                 dash=False))
        elif k == "structaddr":
            args.append(struct_address(rng))
        elif k == "label":
            args.append("1f")
        elif k == "psr":
            args.append(rng.choice(PSRS))
        elif k == "psrw":
            args.append(rng.choice(PSR_WRITE))
        else:
            raise Unsupported("slot %r" % k)
    return args


SUFFIX_S = set("and eor sub rsb add adc sbc rsc orr bic mov mvn orn neg lsl "
               "lsr asr ror rrx mul mla umull umlal smull smlal".split())


def generate(rng, form, target, mutate):
    c = Case()
    c.form, c.target, c.mutation = form, target, None
    mnem = form.mnem
    if mnem in SUFFIX_S and rng.random() < 0.4:
        mnem += "s"
    if form.cond and rng.random() < 0.25:
        # A condition goes on the mnemonic itself, in front of the type
        # suffix a vector instruction carries.
        stem, dot, rest = mnem.partition(".")
        mnem = stem + rng.choice(CONDS) + dot + rest
    width = ""
    if TARGETS[target][3] and rng.random() < 0.2:
        width = rng.choice([".n", ".w"])
    args = fill(rng, form, target)
    if mutate and args:
        c.mutation = mutate_args(rng, form, args)
    text = form.template.format(*args) if args else form.template
    text = re.sub(r"\s+", " ", f"{mnem}{width} {text}").strip()
    if "1f" in text:
        # A branch or `adr` resolves against a label just after it. The
        # whole case stays on one line, so that a reference's error message
        # can be traced back to it.
        text += "; .space %d; 1:" % rnum(rng, 0, 64, 4)
    c.text = text
    return c


MUTATIONS = ["big", "negative", "misaligned", "high", "drop", "extra"]


def mutate_args(rng, form, args):
    i = rng.randrange(len(args))
    what = rng.choice(MUTATIONS)
    if what == "drop":
        args.pop()
        args.append("")
        return "drop"
    if what == "extra":
        args[i] = args[i] + ", r3"
        return "extra"
    if args[i].lstrip("#-").isdigit():
        v = int(args[i].lstrip("#"))
        pre = "#" if args[i].startswith("#") else ""
        if what == "big":
            args[i] = f"{pre}{v + rng.choice([1 << 8, 1 << 12, 1 << 16])}"
        elif what == "negative":
            args[i] = f"{pre}-{abs(v) + 1}"
        else:
            args[i] = f"{pre}{v + 1}"
        return what
    if args[i].startswith("r") and args[i][1:].isdigit():
        args[i] = f"r{rng.randrange(8, 16)}"
        return "high"
    return None


# ============================================================================
# Running the assemblers
# ============================================================================


def elf_sections(data):
    """Section name -> [bytes, sorted [(offset, type, symbol, addend)]]."""
    e = "<" if data[5] == 1 else ">"
    (shoff,) = struct.unpack_from(e + "I", data, 0x20)
    shentsize, shnum, shstrndx = struct.unpack_from(e + "HHH", data, 0x2E)
    shfmt = e + "IIIIIIIIII"
    secs = []
    for i in range(shnum):
        name, typ, _f, _a, off, size, link, info, _al, entsize = struct.unpack_from(
            shfmt, data, shoff + i * shentsize)
        secs.append((name, typ, off, size, link, info, entsize))

    def cstr(tab, idx):
        end = data.index(b"\0", tab + idx)
        return data[tab + idx:end].decode()

    names = [cstr(secs[shstrndx][2], s[0]) for s in secs]
    out = {}
    for i, s in enumerate(secs):
        if s[1] in (1, 8):
            out[names[i]] = [bytes(data[s[2]:s[2] + s[3]]) if s[1] == 1 else b"", []]
    for s in secs:
        if s[1] not in (4, 9):
            continue
        symtab, rela = secs[s[4]], s[1] == 4
        relocs = []
        for k in range(s[3] // s[6]):
            o = s[2] + k * s[6]
            off, info = struct.unpack_from(e + "II", data, o)
            addend = struct.unpack_from(e + "i", data, o + 8)[0] if rela else 0
            sym, typ = info >> 8, info & 0xFF
            so = symtab[2] + sym * 16
            sname = struct.unpack_from(e + "I", data, so)[0]
            sinfo = data[so + 12]
            shndx = struct.unpack_from(e + "H", data, so + 14)[0]
            label = names[shndx] if sinfo & 0xF == 3 else cstr(secs[symtab[4]][2], sname)
            relocs.append((off, typ, label, addend))
        target = names[s[5]]
        if target in out:
            out[target][1] = sorted(relocs)
    return out


LINE_RE = {
    "gas": re.compile(r"^[^:]*\.s:(\d+): (Error|Warning): (.*)$"),
    "mc": re.compile(r"^[^:]*\.s:(\d+):\d+: (error|warning): (.*)$"),
    "rsasm": re.compile(r"^\s*--> [^:]*\.s:(\d+):\d+"),
}


def run_tool(tool, target, source, workdir):
    src = os.path.join(workdir, f"{tool}.s")
    obj = os.path.join(workdir, f"{tool}.o")
    with open(src, "w") as f:
        f.write(source)
    gasflags, triple, arch, thumb = TARGETS[target]
    if tool == "gas":
        cmd = [GAS] + gasflags + ["-o", obj, src]
    elif tool == "mc":
        cmd = [LLVM_MC, f"-triple={triple}", f"-mattr={MC_ATTRS}",
               "-filetype=obj", "-o", obj, src]
    else:
        cmd = [RSASM, "-a", arch, "-o", obj, src]
    if os.path.exists(obj):
        os.unlink(obj)
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    except subprocess.TimeoutExpired:
        return None, {0: ["TIMEOUT"]}
    errors = {}
    if tool == "rsasm":
        if "panicked" in p.stderr:
            return None, {0: ["PANIC: " + p.stderr.strip().splitlines()[0][:200]]}
        last = None
        for ln in p.stderr.splitlines():
            if ln.startswith("error: "):
                last = ln
            m = LINE_RE["rsasm"].match(ln)
            if m and last:
                errors.setdefault(int(m.group(1)), []).append(last[7:])
                last = None
    else:
        for ln in p.stderr.splitlines():
            m = LINE_RE[tool].match(ln)
            if m and m.group(2).lower() == "error":
                errors.setdefault(int(m.group(1)), []).append(m.group(3))
    if p.returncode != 0:
        if not errors:
            errors[0] = [p.stderr.strip()[:200] or f"exit {p.returncode}"]
        return None, errors
    with open(obj, "rb") as f:
        return f.read(), errors


def source_for(target, cases, skip):
    """One section per case, with the mode set once at the top."""
    lines = [PRELUDE.rstrip(), ".thumb" if TARGETS[target][3] else ".arm"]
    owner = {}
    for i, text in enumerate(cases):
        lines.append(f'.section .t{i},"ax",%progbits')
        lines.append(".thumb" if TARGETS[target][3] else ".arm")
        if i in skip:
            continue
        owner[len(lines) + 1] = i
        lines.append("\t" + text)
    return "\n".join(lines) + "\n", owner


def assemble_batch(tool, target, cases, workdir):
    """One result per case, ("ok", (bytes, relocs)) or ("err", message)."""
    rejected = {}
    errors = {}
    for _round in range(24):
        source, owner = source_for(target, cases, rejected)
        obj, errors = run_tool(tool, target, source, workdir)
        if obj is not None:
            secs = elf_sections(obj)
            return [("err", rejected[i]) if i in rejected else
                    ("ok", tuple(secs.get(f".t{i}", [b"", []])))
                    for i in range(len(cases))]
        new = False
        for ln, msgs in errors.items():
            i = owner.get(ln)
            if i is not None and i not in rejected:
                rejected[i] = "; ".join(msgs)
                new = True
        if not new:
            break
    if len(cases) == 1:
        return [("err", "; ".join(sum(errors.values(), [])) or "rejected")]
    mid = len(cases) // 2
    return assemble_batch(tool, target, cases[:mid], workdir) + \
        assemble_batch(tool, target, cases[mid:], workdir)


def fmt(res, with_error=False):
    status, payload = res
    if status == "err":
        return payload if payload.startswith("PANIC") else \
            (f"ERROR ({payload})" if with_error else "ERROR")
    data, relocs = payload
    s = data.hex(" ") if data else "(empty)"
    if relocs:
        s += " " + " ".join(f"[{o:x}:{t}:{n}{a:+d}]" for o, t, n, a in relocs)
    return s


# ============================================================================
# Classifying
# ============================================================================


def key(res):
    return ("err",) if res[0] == "err" else ("ok",) + tuple(res[1][0:1]) + (
        tuple(res[1][1]),)


def gas_negative_immediate(g, m, ctx):
    """`add rd, rn, #-1` in Thumb: GNU as encodes the value as written where
    it is an expandable constant, llvm-mc substitutes the opposite operation
    with the sign taken off, and rsasm follows llvm-mc."""
    return ctx["thumb"] and "#-" in ctx["text"]


def gas_movs_complement(g, m, ctx):
    """`movs rd, #x` where only `~x` is expandable: GNU as writes `mvns`,
    llvm-mc refuses it, and rsasm follows llvm-mc."""
    return re.match(r"^(movs|mvns)", ctx["text"]) is not None


def mc_deprecated(g, m, ctx):
    """llvm-mc refuses what the architecture deprecates -- `swp`, a `^`
    register list, `ldm sp!` -- where GNU as assembles it."""
    return g[0] == "ok" and m[0] == "err"


def gas_said(g, m, *what):
    return g[0] == "err" and m[0] == "ok" and any(w in g[1] for w in what)


def mc_takes_unpredictable(g, m, ctx):
    """llvm-mc assembles a register the architecture calls UNPREDICTABLE in
    that slot -- the PC as a multiply's destination, say -- where GNU as
    refuses it. rsasm refuses it too."""
    return gas_said(g, m, "not allowed here")


def mc_takes_the_wrong_width(g, m, ctx):
    """llvm-mc ignores a `.n` or `.w` it cannot honour; GNU as says so."""
    return gas_said(g, m, "cannot honor width suffix")


def mc_takes_a_shorthand_shift(g, m, ctx):
    """`add rd, rm, lsl #n`: llvm-mc reads the two-operand shorthand with a
    shift on it, where GNU as wants all three registers written."""
    return gas_said(g, m, "garbage following instruction", "undefined symbol")


def gas_refuses_an_always_condition(g, m, ctx):
    """`dmbal`, `pldal`, `vrev16al.8`: GNU as looks a mnemonic up before it
    knows which condition was written, so it refuses even `al`, the
    condition an unconditional instruction has anyway. llvm-mc takes it, and
    so does rsasm -- not least because GNU as itself takes it wherever some
    other form of the mnemonic is conditional (`vnegal.f32 d0, d1`)."""
    return (gas_said(g, m, "instruction cannot be conditional")
            and re.match(r"^\w*al(\.\S*)?(\s|$)", ctx["text"]) is not None)


def mc_takes_a_condition_on_a_vector_instruction(g, m, ctx):
    """llvm-mc reads a condition on any NEON instruction and drops it. A
    NEON instruction cannot be conditional, and GNU as says so."""
    return gas_said(g, m, "instruction cannot be conditional")


def mc_takes_a_width_on_a_typed_mnemonic(g, m, ctx):
    """`vmull.u32.n`: a mnemonic that carries a data type has no width
    suffix, and GNU as reads the `n` as part of the type. llvm-mc takes it
    and ignores it."""
    return gas_said(g, m, "in type specifier")


def mc_takes_a_wide_vector_immediate(g, m, ctx):
    """`vmov.i16 d0, #-1`: llvm-mc truncates an immediate to the element
    size, where GNU as refuses one with bits above it."""
    return gas_said(g, m, "bits set outside the operand size")


def gas_reads_p9_as_a_half_float(g, m, ctx):
    """`ldc`/`stc` on coprocessor 9 shares its encoding with the half-float
    `vldr`, and GNU as takes it for one: it halves the offset's scale and
    refuses anything past 510. llvm-mc, and rsasm, read it as a plain
    coprocessor transfer."""
    return re.match(r"^[ls]tc|^ldc", ctx["text"]) is not None and " p9," in ctx["text"]


def gas_rewrites_a_stack_transfer(g, m, ctx):
    """`ldm sp, {rt}` with one register: GNU as always writes the 16-bit
    stack-relative load or store, even where the register does not reach it,
    and refuses a `.w` outright. rsasm keeps the block form there, which is
    what llvm-mc writes for all of them."""
    return re.match(r"^(ldm|stm)\w*(\.[nw])? sp,", ctx["text"]) is not None


def gas_rewrites_a_single_register_transfer(g, m, ctx):
    """A block transfer of one register through anything but the stack
    pointer: GNU as writes the load or store it is the same as, and llvm-mc
    keeps the block form. rsasm follows GNU as."""
    return re.match(r"^(ldm|stm)\w*(\.[nw])? \w+!?, *\{\w+\}$",
                    ctx["text"]) is not None


def mc_narrows_a_complemented_move(g, m, ctx):
    """`mvns r0, #0xffffff00` is `movs r0, #255`: GNU as keeps the 32-bit
    encoding it had already chosen, and llvm-mc picks the 16-bit one for the
    instruction it ends up with. rsasm follows GNU as."""
    return (g[0] == "ok" and m[0] == "ok" and len(g[1][0]) == 4
            and len(m[1][0]) == 2
            and re.match(r"^(movs?|mvns?)\b", ctx["text"]) is not None)


def mc_drops_a_vfp_register_bit(g, m, ctx):
    """`fldmiax r6, {d31}`: llvm-mc writes the deprecated `fldmx` transfers
    without the top bit of the register number, so its d16 to d31 come out
    as d0 to d15. GNU as writes the bit."""
    return re.match(r"^(fldm|fstm)", ctx["text"]) is not None


def mc_relocates_every_branch(g, m, ctx):
    """A branch to a local label: GNU as resolves it and llvm-mc leaves it to
    the linker. README says rsasm follows GNU as for whole objects."""
    return (g[0] == "ok" and m[0] == "ok" and not g[1][1] and m[1][1])


KNOWN_SPLITS = [
    ("thumb-negative-immediate", gas_negative_immediate, "mc"),
    ("movs-complement", gas_movs_complement, "mc"),
    ("mc-refuses-deprecated", mc_deprecated, "gas"),
    ("mc-takes-unpredictable-register", mc_takes_unpredictable, "gas"),
    ("mc-takes-the-wrong-width", mc_takes_the_wrong_width, "gas"),
    ("mc-takes-a-shorthand-shift", mc_takes_a_shorthand_shift, "gas"),
    ("mc-relocates-every-branch", mc_relocates_every_branch, "gas"),
    ("gas-rewrites-a-stack-transfer", gas_rewrites_a_stack_transfer, "mc"),
    ("gas-rewrites-a-single-register-transfer",
     gas_rewrites_a_single_register_transfer, "gas"),
    ("mc-narrows-a-complemented-move", mc_narrows_a_complemented_move, "gas"),
    ("mc-drops-a-vfp-register-bit", mc_drops_a_vfp_register_bit, "gas"),
    ("gas-refuses-an-always-condition", gas_refuses_an_always_condition, "mc"),
    ("mc-takes-a-condition-on-a-vector-instruction",
     mc_takes_a_condition_on_a_vector_instruction, "gas"),
    ("mc-takes-a-width-on-a-typed-mnemonic",
     mc_takes_a_width_on_a_typed_mnemonic, "gas"),
    ("mc-takes-a-wide-vector-immediate", mc_takes_a_wide_vector_immediate,
     "gas"),
]

def gas_takes_a_condition_on_vaddl(g, m, ctx):
    """`vaddlne.s8` and `vsublne.s8`: a NEON instruction cannot be
    conditional, and GNU as says so for every one of them except these two,
    whose encoder clears the delayed diagnostic before it is printed.
    llvm-mc takes a condition on any NEON instruction and drops it. rsasm
    refuses it, as the architecture does and as GNU as does everywhere
    else."""
    return re.match(r"^v(addl|subl)(eq|ne|cs|cc|mi|pl|vs|vc|hi|ls|ge|lt|gt|le|hs|lo)\.",
                    ctx["text"]) is not None


DEVIATIONS = [
    ("a-condition-on-vaddl-or-vsubl",
     lambda g, m, r, ctx: gas_takes_a_condition_on_vaddl(g, m, ctx)),
    ("p9-is-a-plain-coprocessor-transfer",
     lambda g, m, r, ctx: gas_reads_p9_as_a_half_float(g, m, ctx)),
]


def classify(g, m, r, ctx):
    if r[0] == "err" and r[1].startswith("PANIC"):
        return "rsasm", "panic"
    kg, km, kr = key(g), key(m), key(r)
    if kg == km and kr == kg:
        return "agree", None
    if g[0] == "ok" or m[0] == "ok":
        for name, pred in DEVIATIONS:
            if pred(g, m, r, ctx):
                return "deviation", name
    if kg == km:
        return "rsasm", None
    follows = "gas" if kr == kg else "mc" if kr == km else "neither"
    for name, pred, preferred in KNOWN_SPLITS:
        if pred(g, m, ctx):
            if follows == "neither":
                return "rsasm", name
            if preferred and follows != preferred:
                return "split", f"{name}:{follows}"
            return "convention", f"{name}:{follows}"
    return "split", follows


def run_batch(job):
    target, cases = job
    texts = [c[0] for c in cases]
    with tempfile.TemporaryDirectory() as d:
        res = {t: assemble_batch(t, target, texts, d) for t in ("gas", "mc", "rsasm")}
    out = []
    for i, (text, sig, mnem, mutation) in enumerate(cases):
        g, m, r = res["gas"][i], res["mc"][i], res["rsasm"][i]
        ctx = {"text": text, "mutation": mutation, "thumb": TARGETS[target][3]}
        cls, detail = classify(g, m, r, ctx)
        out.append((target, text, sig, mnem, mutation, cls, detail,
                    fmt(g, True), fmt(m, True), fmt(r, True)))
    return out


def build_forms():
    return table_forms() + shape_forms()


def fuzz(args):
    import multiprocessing

    forms = build_forms()
    if args.only:
        forms = [f for f in forms if re.search(args.only, f.mnem)]
        if not forms:
            sys.exit(f"--only {args.only!r} matches no form")
    targets = list(TARGETS) if args.target == "all" else [args.target]
    rng = random.Random(args.seed)
    jobs, per = [], max(1, args.count // len(targets))
    for target in targets:
        here = [f for f in forms if target in f.targets]
        if not here:
            continue
        cases = []
        for _ in range(per):
            c = generate(rng, rng.choice(here), target, rng.random() < args.mutations)
            cases.append((c.text, c.signature(), c.form.mnem, c.mutation))
        if args.print_cases:
            print("\n".join(f"[{target}] {c[0]}" for c in cases))
            continue
        for i in range(0, len(cases), args.batch):
            jobs.append((target, cases[i:i + args.batch]))
    if args.print_cases:
        return 0
    workers = args.jobs or max(1, (os.cpu_count() or 2) - 2)
    results = []
    with multiprocessing.Pool(workers) as pool:
        for batch in pool.imap(run_batch, jobs):
            results.extend(batch)
    return report(results, args, len(forms))


LISTED = {"rsasm": "rsasm differs from both references",
          "split": "references disagree"}


def report(results, args, nforms):
    totals = collections.Counter()
    by_target = collections.defaultdict(collections.Counter)
    details = collections.Counter()
    buckets = collections.OrderedDict()
    for (target, text, sig, mnem, mutation, cls, detail, g, m, r) in results:
        totals[cls] += 1
        by_target[target][cls] += 1
        if cls not in ("agree", "rsasm"):
            details[f"{cls}:{detail}"] += 1
        if cls in LISTED:
            k = (cls, detail, target, mnem, mutation, g.startswith("ERROR"),
                 m.startswith("ERROR"), r.startswith("ERROR"))
            b = buckets.setdefault(k, [0, None])
            b[0] += 1
            if b[1] is None or len(text) < len(b[1][0]):
                b[1] = (text, g, m, r, sig)
    p = [f"=== {len(results)} cases from {nforms} forms: " +
         ", ".join(f"{k} {v}" for k, v in sorted(totals.items()))]
    for target, c in sorted(by_target.items()):
        p.append(f"  [{target}] " + ", ".join(f"{k} {v}" for k, v in sorted(c.items())))
    if details:
        p.append("  explained: " + ", ".join(f"{k} {v}" for k, v in sorted(details.items())))
    for cls, title in LISTED.items():
        if cls == "split" and args.no_splits:
            continue
        rows = [(k, v) for k, v in buckets.items() if k[0] == cls]
        if not rows:
            continue
        p.append(f"--- {title}: {len(rows)} distinct")
        rows.sort(key=lambda kv: (-kv[1][0], kv[0][2], kv[0][3]))
        for k, (count, (text, g, m, r, sig)) in rows[:args.limit]:
            tag = f"[{k[2]}]" + (f" (x{count})" if count > 1 else "")
            extra = f" ({k[1]})" if k[1] else ""
            one = text.replace("\n", " ; ")
            p.append(f"{tag} {one}{extra}    # {sig}\n    gas:   {g[:200]}\n"
                     f"    mc:    {m[:200]}\n    rsasm: {r[:200]}")
        if len(rows) > args.limit:
            p.append(f"    ... {len(rows) - args.limit} more (raise --limit)")
    print("\n".join(p))
    return 1 if totals["rsasm"] else 0


def main():
    import argparse

    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("check", help="compare instructions given one per line")
    c.add_argument("--target", default="arm", choices=list(TARGETS))
    c.add_argument("--all", action="store_true", help="also print matching cases")
    c.add_argument("file", nargs="?")
    z = sub.add_parser("fuzz", help="generate random instructions and compare")
    z.add_argument("--seed", type=int, default=1)
    z.add_argument("--count", type=int, default=6000)
    z.add_argument("--target", default="all", choices=list(TARGETS) + ["all"])
    z.add_argument("--only", help="regex on the mnemonic")
    z.add_argument("--mutations", type=float, default=0.25)
    z.add_argument("--batch", type=int, default=200)
    z.add_argument("--jobs", type=int, default=0)
    z.add_argument("--limit", type=int, default=100)
    z.add_argument("--no-splits", action="store_true")
    z.add_argument("--print-cases", action="store_true")
    args = ap.parse_args()
    if args.cmd == "check":
        src = open(args.file) if args.file else sys.stdin
        cases = [ln.rstrip("\n") for ln in src if ln.strip() and not ln.startswith("#")]
        with tempfile.TemporaryDirectory() as d:
            res = {t: assemble_batch(t, args.target, cases, d)
                   for t in ("gas", "mc", "rsasm")}
        bad = 0
        for i, text in enumerate(cases):
            g, m, r = (fmt(res[t][i], True) for t in ("gas", "mc", "rsasm"))
            same = key(res["rsasm"][i]) in (key(res["gas"][i]), key(res["mc"][i]))
            bad += not same
            if not same or args.all:
                print(f"{'  ' if same else '!!'} {text}\n      gas:   {g}\n"
                      f"      mc:    {m}\n      rsasm: {r}")
        print(f"--- {len(cases) - bad} matched a reference, {bad} matched neither")
        return 1 if bad else 0
    return fuzz(args)


if __name__ == "__main__":
    sys.exit(main())
