#!/usr/bin/env python3
"""Differential fuzzer for rsasm's m68k backend.

Random instructions are generated from GNU's own opcode table, read out of the
binutils source tree (opcodes/m68k-opc.c, with tools/tables/m68k.py) rather
than from rsasm's copy of it, for one CPU at a time and in GNU or Motorola
syntax. They are assembled by GNU as 2.47 (`--mri` for Motorola syntax) and by
rsasm, and the bytes, relocations and accept/reject decisions compared.

    tools/fuzz/m68k.py fuzz --cpu 68020 --syntax gas --count 20000
    tools/fuzz/m68k.py fuzz --cpu all --syntax all --count 100000
    tools/fuzz/m68k.py fuzz --cpu 68040 --syntax mot --only '^fmove' --seed 3
    tools/fuzz/m68k.py fuzz --cpu 68020 --syntax vasm --count 5000
    tools/fuzz/m68k.py corpus --cpu 68030 --syntax gas > lines.txt
    tools/fuzz/m68k.py check --cpu 68020 --syntax gas lines.txt

`fuzz` classifies each case:

    agree     the reference and rsasm produced the same result.
    rsasm     they differ. Each is an rsasm bug, or a deviation to document
              and leave out of generation (see `deviates` and `mri_skips`).
              Listed, grouped by form; the exit status is 1 when there are any.
    split     against vasm only: vasm and rsasm differ, and GNU as `--mri`,
              given the same source, agrees with rsasm. vasm and GNU as part
              ways by design often enough, and rsasm follows GNU as. Counted;
              `--splits` lists them.

`--syntax vasm` compares Motorola syntax against vasm (with GNU as `--mri` as
the tie-breaker above), bytes outside relocated fields only, since vasm
writes a RELA addend into its field. Mnemonics are spelled as vasm's own
opcode table has them, and what vasm reads differently by design is not
generated (see `Gen.vasm_ok`). This is where extended and packed float
immediates are checked, which GNU as gets wrong or refuses.

Generated cases leave out what rsasm deliberately does differently from
GNU as, since a line one assembler takes and the other refuses moves every
later label in its batch: see `deviates` and `mri_skips`.

`corpus` prints, for every form of every instruction the CPU has, one line
whose operands that form takes and no earlier form of the same mnemonic does,
so the line exercises that form. The corpora under tools/xas-diff were made
with it.

`--mutations` (default 0.15) is the fraction of cases deliberately given an
operand the form does not take, to compare rejections. `--all` fuzzes the
mnemonics rsasm encodes by hand too, where rsasm deliberately does not
substitute cheaper instructions the way GNU as does, so expect findings there.

Every case is labelled, and a case's bytes are those between its label and
the next one; one run of each assembler covers a batch (`--batch`, default
150), and a batch with errors is reassembled without the rejected cases. Runs
are seeded (`--seed`) and spread over the CPUs (`--jobs`).

Environment: RSASM (default target/debug/rsasm under the repository root),
RSASM_ORACLES (default target/oracles; GNU as and vasm are in its bin/).
"""

import argparse
import collections
import importlib.util
import os
import random
import re
import struct
import subprocess
import sys
import tempfile
from concurrent.futures import ProcessPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
ORACLES = os.environ.get("RSASM_ORACLES", os.path.join(ROOT, "target", "oracles"))
BIN = os.path.join(ORACLES, "bin")
RSASM = os.environ.get("RSASM", os.path.join(ROOT, "target", "debug", "rsasm"))
BINUTILS = os.path.join(ORACLES, "src", "binutils-2.47")

# The table generator, tools/tables/m68k.py, loaded by path: this script
# has the same name.
_spec = importlib.util.spec_from_file_location(
    "m68k_table", os.path.join(ROOT, "tools", "tables", "m68k.py")
)
gen = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(gen)

A = gen.ARCH_BITS
WIDE = A["m68020up"] | A["cpu32"] | A["fido_a"]

# CPU: (GNU as flags, rsasm architecture, vasm flags or None, table row name)
CPUS = {
    "68000": (["-m68000"], "68000", ["-m68000"], "68000"),
    "68010": (["-m68010"], "68010", ["-m68010"], "68010"),
    "68020": (["-m68020"], "m68k", ["-m68020", "-m68881", "-m68851"], "68020"),
    "68030": (["-m68030"], "68030", ["-m68030", "-m68881", "-m68851"], "68030"),
    "68040": (["-m68040"], "68040", ["-m68040"], "68040"),
    "68060": (["-m68060"], "68060", ["-m68060"], "68060"),
    "cpu32": (["-mcpu32"], "cpu32", ["-mcpu32", "-m68881"], "cpu32"),
    "fidoa": (["-mcpu=fidoa"], "fidoa", None, "fidoa"),
    "5206": (["-mcpu=5206"], "5206", None, "5206"),
    "5208": (["-mcpu=5208"], "5208", None, "5208"),
    "5407": (["-mcpu=5407"], "5407", None, "5407"),
    "54455": (["-mcpu=54455"], "54455", None, "54455"),
    "5475": (["-mcpu=5475"], "5475", None, "5475"),
}

# ============================================================================
# The table
# ============================================================================


class Form:
    __slots__ = ("name", "opcode", "words", "args", "arch", "index")

    def __init__(self, row, index):
        self.name, self.opcode, self.words, self.args, self.arch = row
        self.index = index

    def pairs(self):
        a = self.args
        return [(a[i], a[i + 1]) for i in range(0, len(a), 2)]


def load(include_hand):
    rows, aliases = gen.opcodes(BINUTILS)
    _, cpu_tables = gen.cpus(BINUTILS)
    archs = {name: arch for table in cpu_tables.values() for name, arch, _ in table}
    by_name = collections.OrderedDict()
    for i, row in enumerate(rows):
        if row[4] & gen.MAC:
            continue
        if not include_hand and gen.hand_written(row[0]):
            continue
        by_name.setdefault(row[0], []).append(Form(row, i))
    return by_name, archs


# ============================================================================
# Operands
# ============================================================================
#
# An operand is generated as a class, the way tc-m68k.c's matcher sees it,
# and then rendered in the syntax under test.

DREG, AREG, FREG, CTL, ABS, IMM, BIG, IND, INC, DEC, DISP, DISPPC, BASE, BASEPC, \
    MEMIND, LIST = range(16)


class Op:
    def __init__(self, cls, text, **kw):
        self.cls = cls
        self.text = text
        self.pc = kw.get("pc", False)
        self.value = kw.get("value")  # constant immediates
        self.reg = kw.get("reg")      # register number / GNU register name
        self.mask = kw.get("mask", 0)
        self.memind = cls == MEMIND
        self.full = kw.get("full", False)  # needs 68020 addressing
        self.scaled = kw.get("scaled", False)
        self.noindexsize = kw.get("noindexsize", False)
        self.index_reg = kw.get("index_reg")
        self.float = kw.get("float", False)

    def __repr__(self):
        return self.text


def fits(k, p, op):
    """tc-m68k.c's matching switch, for the kinds the table uses."""
    c = op.cls
    pc = op.pc and c in (DISP, DISPPC, BASE, BASEPC, MEMIND)
    imm = c in (IMM, BIG)
    ctl = lambda *names: c == CTL and op.reg in names
    if k == "!":
        return c not in (DREG, AREG, FREG, CTL, INC, DEC, LIST) and not imm
    if k == "<":
        return c not in (DREG, AREG, FREG, CTL, DEC, LIST) and not imm
    if k == ">":
        if c in (DREG, AREG, FREG, CTL, IMM, BIG, INC, LIST):
            return False
        return c == ABS or not pc
    if k == "b":
        return c not in (IMM, BIG, ABS, AREG, FREG, CTL, LIST, MEMIND)
    if k == "p":
        return c in (DREG, AREG, IND, INC, DEC) or (c in (DISP, DISPPC) and not pc)
    if k == "q":
        return c in (DREG, IND, INC, DEC) or (c in (DISP, DISPPC) and not pc)
    if k == "v":
        return c in (DREG, IND, INC, DEC, ABS) or (c in (DISP, DISPPC) and not pc)
    if k == "w":
        return c not in (IMM, BIG, ABS, AREG, DREG, FREG, CTL, LIST, MEMIND)
    if k == "y":
        return c == IND or (c == DISP and not pc)
    if k == "z":
        return c in (IND, DISP, DISPPC)
    if k == "#":
        if c == IMM:
            if op.value is None:
                return True
            v = (op.value + 2**31) % 2**32 - 2**31
            return {"b": -255 <= v <= 255, "B": -128 <= v <= 127,
                    "w": -65535 <= v <= 65535, "W": -32768 <= v <= 32767}.get(p, True)
        return c == BIG and p not in "bBwW"
    if k in "^Tk":
        return imm
    if k == "$":
        return c not in (AREG, CTL, FREG, LIST) and not imm and not (c != ABS and pc)
    if k == "%":
        return c not in (CTL, FREG, LIST) and not imm and not (c != ABS and pc)
    if k == "&":
        if c in (DREG, AREG, FREG, CTL, IMM, BIG, INC, DEC, LIST):
            return False
        return c == ABS or not pc
    if k == "*":
        return c not in (CTL, FREG, LIST)
    if k == "+":
        return c == INC
    if k == "-":
        return c == DEC
    if k == "/":
        return c not in (AREG, CTL, FREG, INC, DEC, LIST) and not imm
    if k == ";":
        return c not in (AREG, CTL, FREG, LIST)
    if k == "?":
        if c in (AREG, CTL, FREG, INC, DEC, IMM, BIG, LIST):
            return False
        return c == ABS or not pc
    if k == "@":
        return c not in (AREG, CTL, FREG, LIST) and not imm
    if k == "~":
        if c in (DREG, AREG, CTL, FREG, IMM, BIG, LIST):
            return False
        return c == ABS or not pc
    if k == "|":
        return c not in (CTL, FREG, DREG, AREG, LIST) and not imm
    if k == "3":
        return ctl("tt0", "tt1")
    if k == "A":
        return c == AREG
    if k == "a":
        return c == IND
    if k in "B_":
        return c == ABS
    if k == "C":
        return ctl("ccr")
    if k == "d":
        return c == DISP
    if k == "D":
        return c == DREG
    if k == "F":
        return c == FREG
    if k in "lL":
        if c in (DREG, AREG, FREG):
            return p != "8"
        if ctl("fpiar", "fpsr", "fpcr"):
            return p == "8"
        if c == LIST:
            if p == "8":
                return op.mask & 0x0FFFFFF == 0
            if p == "3":
                return op.mask & 0x7000000 == 0
            return True
        return False
    if k == "M":
        return c == IMM and op.value is not None and -128 <= op.value <= 127
    if k == "R":
        return c in (DREG, AREG)
    if k == "r":
        return c == IND or (c == BASE and op.index_reg is not None and not op.scaled
                            and op.noindexsize)
    if k == "s":
        return ctl("fpiar", "fpsr", "fpcr")
    if k == "S":
        return ctl("sr")
    if k == "t":
        return c == IMM and op.value is not None and 0 <= op.value % 2**32 <= 7
    if k == "U":
        return ctl("usp")
    if k == "x":
        return c == IMM and op.value is not None and (op.value % 2**32 == 0xFFFFFFFF
                                                      or 1 <= op.value % 2**32 <= 7)
    if k == "f":
        return ctl("sfc", "dfc")
    if k == "0":
        return ctl("tc")
    if k == "1":
        return ctl("ac")
    if k == "2":
        return ctl("cal", "val", "scc")
    if k == "V":
        return ctl("val")
    if k == "W":
        return ctl("drp", "srp", "crp")
    if k == "X":
        return c == CTL and re.match(r"^ba[cd][0-7]$", op.reg or "") is not None
    if k == "Y":
        return ctl("psr")
    if k == "Z":
        return ctl("pcsr")
    if k == "c":
        return ctl("nc", "ic", "dc", "bc")
    return False


# Stands for the label of the case it is in.
SELF = "SELF_LABEL"

CTL_NAMES = ["sr", "ccr", "usp", "fpcr", "fpsr", "fpiar", "tc", "ac", "cal", "val", "scc",
             "drp", "srp", "crp", "psr", "pcsr", "tt0", "tt1", "sfc", "dfc", "nc", "ic", "dc",
             "bc", "bad0", "bad5", "bac3", "bac7"]


class Gen:
    def __init__(self, rng, cpu_arch, syntax):
        self.r = rng
        self.arch = cpu_arch
        self.syntax = syntax
        self.gnu = syntax == "gas"
        self.wide = bool(cpu_arch & WIDE) and not cpu_arch & A["mcfisa_a"]

    # ---- spelling ------------------------------------------------------------

    def reg(self, name):
        return ("%" if self.gnu else "") + name

    def hexn(self, v):
        if v < 0:
            return "-" + self.hexn(-v)
        return ("0x%x" if self.gnu else "$%x") % v

    def num(self, v):
        return str(v) if self.r.random() < 0.5 or v < 0 else self.hexn(v)

    def sym(self):
        return self.r.choice(["ext", "ext+4", "ext-2"])

    # ---- classes -------------------------------------------------------------

    def dreg(self):
        n = self.r.randrange(8)
        return Op(DREG, self.reg("d%d" % n), reg=n)

    def areg(self):
        n = self.r.randrange(8)
        name = "sp" if n == 7 and self.r.random() < 0.3 else "a%d" % n
        return Op(AREG, self.reg(name), reg=n)

    def freg(self):
        n = self.r.randrange(8)
        return Op(FREG, self.reg("fp%d" % n), reg=n)

    def ctl(self, names=None):
        name = self.r.choice(names or CTL_NAMES)
        return Op(CTL, self.reg(name), reg=name)

    def an(self):
        return self.reg("a%d" % self.r.randrange(8))

    def ind(self):
        a = self.an()
        if self.gnu and self.r.random() < 0.3:
            return Op(IND, "%s@" % a)
        return Op(IND, "(%s)" % a)

    def inc(self):
        a = self.an()
        if self.gnu and self.r.random() < 0.3:
            return Op(INC, "%s@+" % a)
        return Op(INC, "(%s)+" % a)

    def dec(self):
        a = self.an()
        if self.gnu and self.r.random() < 0.3:
            return Op(DEC, "%s@-" % a)
        return Op(DEC, "-(%s)" % a)

    def disp(self):
        d = self.r.choice([self.r.randint(-32768, 32767), self.r.randint(1, 127), -8])
        if d == 0:
            d = 2
        a = self.an()
        if self.gnu and self.r.random() < 0.3:
            return Op(DISP, "%s@(%s)" % (a, self.num(d)))
        return Op(DISP, "%s(%s)" % (self.num(d), a))

    def index(self, allow_scale=True):
        n = self.r.randrange(8)
        reg = self.reg(("d%d" if self.r.random() < 0.7 else "a%d") % n)
        size = self.r.choice(["w", "l"])
        scale = self.r.choice([1, 1, 2, 4, 8]) if allow_scale else 1
        text = reg
        if self.gnu and self.r.random() < 0.4:
            text += ":" + size + (":%d" % scale if scale != 1 else "")
        else:
            text += "." + size + ("*%d" % scale if scale != 1 else "")
        return text, scale != 1

    def base(self):
        """d8(An,Xn): the brief extension word."""
        d = self.r.randint(-128, 127)
        x, scaled = self.index(self.arch & (WIDE | A["mcfisa_a"]))
        return Op(BASE, "%s(%s,%s)" % (self.num(d), self.an(), x), scaled=scaled)

    def full(self):
        """68020 modes: a wide base displacement, no base, memory indirect."""
        r = self.r.random()
        x, scaled = self.index()
        bd = self.r.choice([0x12345, -40000, 0x7fff, 1000])
        if r < 0.35:
            return Op(BASE, "(%s,%s,%s)" % (self.num(bd), self.an(), x), full=True,
                      scaled=scaled)
        if r < 0.5:
            return Op(BASE, "(%s,%s)" % (self.num(bd), x), full=True, scaled=scaled)
        od = self.r.choice([0, 4, 0x12345])
        if r < 0.75:
            return Op(MEMIND, "([%s,%s],%s,%s)" % (self.num(bd), self.an(), x, self.num(od)),
                      full=True)
        return Op(MEMIND, "([%s,%s,%s],%s)" % (self.num(bd), self.an(), x, self.num(od)),
                  full=True)

    def pcrel(self):
        # A constant `n(pc)` means a displacement to GNU as and an address in
        # Motorola syntax, so only symbols are written.
        s = self.sym()
        if self.r.random() < 0.5:
            return Op(DISPPC, "%s(%s)" % (s, self.reg("pc")), pc=True)
        x, scaled = self.index(self.arch & (WIDE | A["mcfisa_a"]))
        return Op(BASEPC, "%s(%s,%s)" % (s, self.reg("pc"), x), pc=True, scaled=scaled)

    def absolute(self):
        r = self.r.random()
        if r < 0.3:
            return Op(ABS, self.hexn(self.r.randint(0, 0x7fff)))
        if r < 0.6:
            return Op(ABS, self.hexn(self.r.randint(0x10000, 0x7fffffff)))
        return Op(ABS, self.sym())

    def imm(self, place):
        lo, hi = {"b": (-128, 255), "B": (-128, 127), "w": (-32768, 65535),
                  "W": (-32768, 32767)}.get(place, (-2**31, 2**32 - 1))
        r = self.r.random()
        if r < 0.1 and place not in "fFxp3C":
            return Op(IMM, "#" + self.sym())
        if r < 0.3:
            v = self.r.choice([lo, hi, 0, 1, -1 if lo < 0 else 0])
        elif r < 0.6:
            v = self.r.randint(0, min(hi, 15))
        else:
            v = self.r.randint(lo, hi)
        return Op(IMM, "#" + self.num(v), value=v)

    def fimm(self):
        v = self.r.choice(["1.5", "0.1", "-2.5", "3.14159", "1e10", "-1.0e-5", "12345.678",
                           "0.0"])
        if self.gnu:
            text = "#" + ("-0r" + v[1:] if v.startswith("-") else "0r" + v)
        else:
            text = "#" + (v if "." in v else v.replace("e", ".0e"))
        return Op(BIG, text, float=True)

    def reglist(self, fp=True):
        if fp and self.r.random() < 0.3:
            names = self.r.sample(["fpcr", "fpsr", "fpiar"], self.r.randint(1, 3))
            mask = sum({"fpiar": 1 << 24, "fpsr": 1 << 25, "fpcr": 1 << 26}[n] for n in names)
            return Op(LIST, "/".join(self.reg(n) for n in names), mask=mask)
        prefix, base = ("fp", 16) if fp else (self.r.choice(["d", "a"]), 0 if True else 8)
        if not fp and prefix == "a":
            base = 8
        lo = self.r.randrange(7)
        hi = self.r.randrange(lo + 1, 8)
        mask = sum(1 << (base + i) for i in range(lo, hi + 1))
        text = "%s-%s" % (self.reg("%s%d" % (prefix, lo)), self.reg("%s%d" % (prefix, hi)))
        if self.r.random() < 0.4:
            extra = self.r.randrange(8)
            text += "/" + self.reg("%s%d" % (prefix, extra))
            mask |= 1 << (base + extra)
        return Op(LIST, text, mask=mask)

    # ---- a whole operand for a (kind, place) ----------------------------------

    def general(self, k, p):
        """Some addressing mode the matcher gives kind `k`."""
        makers = [self.dreg, self.areg, self.ind, self.inc, self.dec, self.disp, self.base,
                  self.absolute, self.pcrel]
        # GNU as has no size for an immediate in a place without one, and
        # stops with an internal error.
        if p in "bwlBWfFxp":
            makers.append(lambda: self.imm(p))
        if self.wide:
            makers.append(self.full)
        if p in ("fFxp" if self.syntax == "vasm" else "fF"):
            makers.append(self.fimm)
        for _ in range(40):
            op = self.r.choice(makers)()
            if fits(k, p, op):
                return op
        return None

    def operand(self, k, p):
        r = self.r
        if k in "*~%;@!&$?/<>bpqvwyz|":
            return self.general(k, p)
        if k in "#^":
            if p == "3":
                return Op(IMM, "#" + self.num(r.randrange(256)), value=0)
            if p == "C":
                v = r.randrange(128)
                return Op(IMM, "#" + self.num(v), value=v)
            return self.imm(p)
        if k == "T":
            v = r.randrange(16)
            return Op(IMM, "#%d" % v, value=v)
        if k == "t":
            v = r.randrange(8)
            return Op(IMM, "#%d" % v, value=v)
        if k == "k":
            v = r.randint(-64, 63)
            return Op(IMM, "#%d" % v, value=v)
        if k == "x":
            v = r.choice([-1, 1, 2, 3, 4, 5, 6, 7])
            return Op(IMM, "#%d" % v, value=v)
        if k == "M":
            v = r.randint(-128, 127)
            return Op(IMM, "#%d" % v, value=v)
        if k == "D":
            return self.dreg()
        if k == "A":
            return self.areg()
        if k == "F":
            return self.freg()
        if k == "R":
            return r.choice([self.dreg, self.areg])()
        if k == "+":
            return self.inc()
        if k == "-":
            return self.dec()
        if k == "a":
            return self.ind()
        if k == "d":
            return self.disp()
        if k == "r":
            if r.random() < 0.5:
                return Op(IND, "(%s)" % self.an())
            n = r.randrange(8)
            reg = ("d%d" if r.random() < 0.5 else "a%d") % n
            return Op(BASE, "(%s)" % self.reg(reg), index_reg=reg, noindexsize=True)
        if k in "B":
            if self.syntax == "vasm":
                # A branch to itself, so that a case vasm writes at another
                # length (by design, as `pbbs ext`) moves no other target.
                return Op(ABS, r.choice(["ext", SELF]))
            return Op(ABS, r.choice(["ext", "ext+6", "c0", "cend"]))
        if k == "_":
            return self.absolute() if r.random() < 0.8 else Op(ABS, "ext")
        if k in "lL":
            if p == "8":
                return self.reglist(True) if r.random() < 0.7 else self.ctl(
                    ["fpcr", "fpsr", "fpiar"])
            if r.random() < 0.3:
                return self.freg()
            op = self.reglist(True)
            while op.mask & 0x7000000:
                op = self.reglist(True)
            return op
        table = {"s": ["fpcr", "fpsr", "fpiar"], "c": ["nc", "ic", "dc", "bc"],
                 "f": ["sfc", "dfc"], "0": ["tc"], "1": ["ac"], "2": ["cal", "val", "scc"],
                 "3": ["tt0", "tt1"], "W": ["drp", "srp", "crp"], "Y": ["psr"],
                 "Z": ["pcsr"], "V": ["val"], "C": ["ccr"], "S": ["sr"], "U": ["usp"],
                 "X": ["bad%d" % i for i in range(8)] + ["bac%d" % i for i in range(8)]}
        if k in table:
            return self.ctl(table[k])
        return None

    def mutant(self):
        # Constants only: a symbol GNU as would take in some other form of the
        # mnemonic (a register mask, say) is not what a mutation is testing.
        def imm():
            # Small enough for a trap vector or a level, which GNU as would
            # otherwise take with a warning in some other form.
            v = self.r.randint(0, 7)
            return Op(IMM, "#%d" % v, value=v)
        return self.r.choice([self.dreg, self.areg, self.freg, self.ctl, self.ind, self.inc,
                              self.dec, self.disp, self.absolute, imm,
                              self.fimm, lambda: self.reglist(True)])()

    def vasm_ok(self, ops, pairs):
        """What vasm reads differently from GNU as and rsasm, by design: MIT
        syntax it has no reading of; `n(pc)`, an address to it; with `-no-opt`
        it keeps every absolute address and base displacement at 32 bits; an
        integer immediate in a floating-point operand it converts to a float,
        where GNU as and rsasm write its bits."""
        for (k, p), o in zip(pairs, ops):
            if "(pc" in o.text or "@" in o.text:
                return False
            if o.full or (o.cls == ABS and k != "_" and not re.match(r"^[a-z]", o.text)):
                return False
            if o.cls == IMM and p in "fFxp":
                return False
        return True


def deviates(k, p, op, g, form):
    """Operands where rsasm refuses what GNU as accepts with a warning or
    writes something no one would want, all documented in the backend:

    - a float literal anywhere but a floating-point operand, which GNU as
      writes as single-precision bits (or 0, with a warning); and, against GNU
      as, in an extended or packed operand at all (see src/arch/m68k/float.rs);
    - a symbol where only a constant can go (a trap vector, a level, a
      k-factor, a mask), which GNU as replaces by its addend with a warning.

    They are left out of generated cases, since a line one assembler rejects
    and the other accepts shifts every later label in its batch."""
    syntax = g.syntax
    if form.name == "linkw" and op.cls == IMM and (op.value or 0) > 32767:
        # rsasm's `link.w` takes a signed displacement, where GNU as also
        # takes 32768 to 65535 as the same bits (with `--all` only).
        return True
    if op.cls == BIG:
        return not (k in "*~%;@!&$?/<>bpqvwyz|" and p in ("fFxp" if syntax == "vasm" else "fF"))
    if op.cls == BASEPC and not g.wide:
        # An 8-bit PC displacement to an undefined symbol, which GNU as
        # refuses once the field is 128 bytes into its section.
        return True
    if op.cls == IMM and op.value is not None:
        # Out of range where GNU as warns and writes 0 or 1 instead.
        v = op.value
        return {"T": not 0 <= v <= 15, "t": not 0 <= v <= 7, "k": not -64 <= v <= 63,
                "x": not (v == -1 or 1 <= v <= 7), "M": not -128 <= v <= 127}.get(k, False) \
            or (k in "#^" and p == "C" and not 0 <= v <= 127) \
            or (k == "#" and p == "j" and not 0 <= v <= 4095)
    if k == "B" and op.cls == ABS and not re.match(r"^[a-z]", op.text):
        # A branch to a number: GNU as writes a `DBcc` displacement to one as
        # zero with no relocation, and refuses a word displacement to one out
        # of reach of where the branch is in its section; rsasm relocates
        # both (src/arch/m68k/branch.rs).
        return True
    if op.cls == IMM and op.value is None:
        # A symbol in a floating-point operand stops GNU as with an internal
        # error.
        return k in "TtkxM" or (k in "#^" and p in "3Cj") or p in "fFxp"
    return False


# ============================================================================
# Cases
# ============================================================================


def join_operands(form, ops, gnu):
    """Writes the operand list, gluing what GNU as reads as separate operands:
    `{offset:width}` and k-factors, and the two sides of a colon."""
    pairs = form.pairs()
    skip = 1 if pairs and pairs[0][0] == "I" else 0
    kinds = pairs[skip:]
    texts = [o.text for o in ops]
    name = form.name
    if name.startswith("fsincos") and len(texts) == 3:
        return "%s,%s:%s" % tuple(texts)
    if name.startswith("cas2") and len(texts) == 6:
        return "%s:%s,%s:%s,%s:%s" % tuple(texts)
    if name in ("remsl", "remul") and len(texts) == 3:
        return "%s,%s:%s" % tuple(texts)
    if name.startswith("fmovep") and len(texts) == 3 and kinds[2][0] in "kD":
        k = texts[2]
        return "%s,%s{%s}" % (texts[0], texts[1], k)
    return ",".join(texts)


VASM = {}


def vasm_spelling(name):
    """How vasm spells a mnemonic of GNU's table, if it has it: vasm splits a
    size off with a dot where it takes one, and has no aliases of GNU's
    (`fjeq`, `pbbsw`). Read from vasm's own opcode table."""
    if not VASM:
        sizes = {"UNS": "", "B": "b", "W": "w", "L": "l", "Q": "d", "SBWL": "sbwl", "BW": "bw",
                 "WL": "wl", "BWL": "bwl", "WQ": "wd", "QW": "wd", "CFWL": "wl", "CFBWL": "bwl",
                 "ANY": "bwlsdxp", "CFANY": "bwlsd", "FX": "x", "FD": "d"}
        src = os.path.join(ORACLES, "src", "vasm", "cpus", "m68k", "opcodes.h")
        row = re.compile(r'^\s*"(\w+)",.*?\},\s*\{[^}]*\},\s*\d\|([\w|]+),\s*([\w|]+)\s*\},')
        for line in open(src):
            m = row.match(line)
            if not m or m.group(3) == "apollo" or "mgas" in m.group(3):
                continue
            letters = "".join(sizes.get(t, "") for t in m.group(2).split("|"))
            VASM.setdefault(m.group(1), set()).update(letters or " ")
    stem, c = name[:-1], name[-1]
    if stem in VASM and c in VASM[stem]:
        return stem + "." + c
    if name in VASM:
        return name
    return None


NAMES = set()


def spell_mnemonic(name, rng, gnu):
    """`faddx` as `fadd.x` (always in Motorola syntax, mostly in GNU's).

    GNU as only removes the dot, so any split would do; this one splits off a
    size letter where the stem is a mnemonic too or has other sizes, which
    leaves `lpstop`, `pflushs` and `cinvl` alone."""
    if not NAMES:
        rows, aliases = gen.opcodes(BINUTILS)
        NAMES.update(r[0] for r in rows)
        NAMES.update(a for a, _ in aliases)
    sizes = "bwlsdxp" if name.startswith("f") else "bwl"
    stem, c = name[:-1], name[-1]
    sized = c in sizes and (stem in NAMES or any(stem + o in NAMES for o in sizes if o != c))
    if sized and rng.random() < (0.7 if gnu else 1.0):
        return stem + "." + c
    return name


class Case:
    __slots__ = ("form", "text", "mutated")

    def __init__(self, form, text, mutated):
        self.form, self.text, self.mutated = form, text, mutated


def mri_skips(g, form, forms_of_name):
    """What GNU as `--mri` reads differently by design, and rsasm does not
    copy: an instruction whose first form has no operands is assembled as
    that form whatever follows it and whatever the CPU (`pflusha` is the
    68040's on a 68030, `tpf` ColdFire's on a 68020), and `swbeg` is its
    ignored pseudo-op."""
    if g.syntax == "gas":
        return False
    if form.name == "swbeg":
        return True
    first = forms_of_name[0]
    return first.args == "" and (form is not first or not first.arch & g.arch)


def make_case(g, form, forms_of_name, rng, mutate=False, exclusive=False):
    pairs = form.pairs()
    skip = 1 if pairs and pairs[0][0] == "I" else 0
    if mri_skips(g, form, forms_of_name) or (mutate and g.syntax != "gas"
                                             and forms_of_name[0].args == ""):
        return None
    for _attempt in range(30):
        ops = []
        for k, p in pairs[skip:]:
            op = g.operand(k, p)
            if op is None:
                break
            ops.append(op)
        else:
            if mutate and ops:
                i = rng.randrange(len(ops))
                ops[i] = g.mutant()
            if any(deviates(k, p, o, g, form) for (k, p), o in zip(pairs[skip:], ops)):
                continue
            if exclusive and not mutate:
                # No earlier form of the mnemonic may take these operands.
                earlier = [f for f in forms_of_name if f.index < form.index
                           and f.arch & g.arch]
                if any(len(f.pairs()) - (1 if f.args.startswith("I") else 0) == len(ops)
                       and all(fits(k, p, o) for (k, p), o in
                               zip(f.pairs()[1 if f.args.startswith("I") else 0:], ops))
                       for f in earlier):
                    continue
            if g.syntax == "vasm" and not g.vasm_ok(ops, pairs[skip:]):
                continue
            mn = spell_mnemonic(form.name, rng, g.gnu)
            if g.syntax == "vasm":
                mn = vasm_spelling(form.name)
                if mn is None:
                    return None
            text = mn + ("\t" + join_operands(form, ops, g.gnu) if ops else "")
            return Case(form, text, mutate)
    return None


# ============================================================================
# Running the assemblers
# ============================================================================


def elf(data):
    """(symbols {name: value}, text bytes, relocs [(off, type, sym, addend)])."""
    (shoff,) = struct.unpack_from(">I", data, 0x20)
    shentsize, shnum, shstrndx = struct.unpack_from(">HHH", data, 0x2E)
    secs = []
    for i in range(shnum):
        secs.append(struct.unpack_from(">IIIIIIIIII", data, shoff + i * shentsize))
    strtab = secs[shstrndx]

    def cstr(off, idx):
        end = data.index(b"\0", off + idx)
        return data[off + idx:end].decode()

    names = [cstr(strtab[4], s[0]) for s in secs]
    text_idx = next((i for i, n in enumerate(names) if n in (".text", "CODE")), None)
    text = b""
    if text_idx is not None and secs[text_idx][1] == 1:
        text = data[secs[text_idx][4]:secs[text_idx][4] + secs[text_idx][5]]
    syms, relocs = {}, []
    for i, s in enumerate(secs):
        if s[1] == 2:  # SYMTAB
            strs = secs[s[6]]
            for k in range(s[5] // 16):
                o = s[4] + k * 16
                nm, val, _sz, info, _oth, shndx = struct.unpack_from(">IIIBBH", data, o)
                if shndx == text_idx and info & 0xF != 3:
                    syms[cstr(strs[4], nm)] = val
    for i, s in enumerate(secs):
        if s[1] == 4 and s[7] == text_idx:  # RELA for the code
            symtab = secs[s[6]]
            strs = secs[symtab[6]]
            for k in range(s[5] // 12):
                off, info, addend = struct.unpack_from(">IIi", data, s[4] + k * 12)
                so = symtab[4] + (info >> 8) * 16
                nm = struct.unpack_from(">I", data, so)[0]
                sinfo = data[so + 12]
                shndx = struct.unpack_from(">H", data, so + 14)[0]
                label = names[shndx] if sinfo & 0xF == 3 else cstr(strs[4], nm)
                relocs.append((off, info & 0xFF, label, addend))
    return syms, text, sorted(relocs)


LINE_RE = re.compile(r"^[^:]*\.s:(\d+): (Error|Warning|Internal error)[: ]\s*(.*)$")
VASM_RE = re.compile(r"^(error|warning) \d+ in line (\d+) of \"[^\"]*\": (.*)$")
RSASM_RE = re.compile(r"^\s*--> [^:]*\.s:(\d+):\d+")


def run(tool, cpu, syntax, source, workdir):
    """(object or None, {line: [errors]})."""
    src = os.path.join(workdir, tool + ".s")
    obj = os.path.join(workdir, tool + ".o")
    with open(src, "w") as f:
        f.write(source)
    if os.path.exists(obj):
        os.unlink(obj)
    gas_flags, rs_arch, vasm_flags, _ = CPUS[cpu]
    if tool == "ref" and syntax == "vasm":
        cmd = [os.path.join(BIN, "vasmm68k_mot"), "-quiet", "-no-opt", "-devpac", "-Felf",
               "-o", obj] + vasm_flags + [src]
    elif tool in ("ref", "gnu"):
        cmd = [os.path.join(BIN, "m68k-elf-as")] + gas_flags + \
            (["--mri"] if syntax != "gas" else []) + ["-o", obj, src]
    else:
        cmd = [RSASM, "-a", rs_arch, "-d", "gas" if syntax == "gas" else "motorola", "-o",
               obj, src]
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    except subprocess.TimeoutExpired:
        return None, {0: ["TIMEOUT"]}
    errors = {}
    out = p.stderr + p.stdout
    if tool == "rsasm":
        last = None
        for ln in out.splitlines():
            if ln.startswith("error: "):
                last = ln
            m = RSASM_RE.match(ln)
            if m and last:
                errors.setdefault(int(m.group(1)), []).append(last[7:])
                last = None
        if "panicked" in out:
            return None, {0: ["PANIC: " + out.strip().splitlines()[-1][:200]]}
    elif syntax == "vasm" and tool == "ref":
        for ln in out.splitlines():
            m = VASM_RE.match(ln)
            if m and m.group(1) == "error":
                errors.setdefault(int(m.group(2)), []).append(m.group(3))
    else:
        for ln in out.splitlines():
            m = LINE_RE.match(ln)
            if m and m.group(2) != "Warning":
                errors.setdefault(int(m.group(1)), []).append(m.group(3))
    if p.returncode != 0 or errors or not os.path.exists(obj):
        if not errors:
            errors[0] = [out.strip()[:200] or "exit %d" % p.returncode]
        return None, errors
    with open(obj, "rb") as f:
        return f.read(), errors


def assemble(tool, cpu, syntax, cases, workdir, plan=None):
    """One result per case: ("ok", (bytes, relocs)) or ("err", message), and
    the sub-batches it was assembled in.

    A batch that fails for a reason no line is blamed for is split in two.
    Labels are counted per sub-batch, so `plan` makes a second assembler use
    the sub-batches the first one did."""
    if plan is not None and plan != [(0, len(cases))]:
        results, used = [], []
        for lo, hi in plan:
            r, u = assemble(tool, cpu, syntax, cases[lo:hi], workdir, None)
            results += r
            used += [(lo + a, lo + b) for a, b in u]
        return results, used
    rejected = {}
    for _round in range(30):
        lines = []
        owner = {}
        if syntax == "vasm":
            lines.append("\txdef " + ",".join("c%d" % i for i in range(len(cases))) + ",cend")
        for i, case in enumerate(cases):
            lines.append(("c%d:" if syntax == "gas" else "c%d") % i)
            if i in rejected:
                continue
            lines.append("\t" + case.text.replace(SELF, "c%d" % i))
            owner[len(lines)] = i
        lines.append("cend:" if syntax == "gas" else "cend")
        lines.append("\t" + ("rts" if syntax == "gas" else "rts"))
        obj, errors = run(tool, cpu, syntax, "\n".join(lines) + "\n", workdir)
        if obj is not None:
            syms, text, relocs = elf(obj)
            results = []
            for i in range(len(cases)):
                if i in rejected:
                    results.append(("err", rejected[i]))
                    continue
                lo = syms.get("c%d" % i, 0)
                hi = syms.get("c%d" % (i + 1), syms.get("cend", len(text)))
                rel = tuple((o - lo, t, s, a) for o, t, s, a in relocs if lo <= o < hi)
                results.append(("ok", (bytes(text[lo:hi]), rel)))
            return results, [(0, len(cases))]
        new = False
        for ln, msgs in errors.items():
            if ln in owner and owner[ln] not in rejected:
                rejected[owner[ln]] = "; ".join(msgs)
                new = True
        if not new:
            break
    if len(cases) == 1:
        return [("err", "; ".join(sum(errors.values(), [])))], [(0, 1)]
    mid = len(cases) // 2
    r1, u1 = assemble(tool, cpu, syntax, cases[:mid], workdir)
    r2, u2 = assemble(tool, cpu, syntax, cases[mid:], workdir)
    return r1 + r2, u1 + [(mid + a, mid + b) for a, b in u2]


def fmt(res, relocs=True):
    status, payload = res
    if status == "err":
        return "ERROR (%s)" % payload[:120]
    data, rel = payload
    s = data.hex(" ") if data else "(empty)"
    if relocs and rel:
        s += " " + " ".join("[%x:%d:%s%+d]" % r for r in rel)
    return s


RELOC_SIZE = {1: 4, 2: 2, 3: 1, 4: 4, 5: 2, 6: 1}


def masked(res):
    """vasm writes a relocation's addend into its field, where GNU as and
    rsasm leave it zero for RELA; the bytes are compared without them."""
    data, relocs = res
    data = bytearray(data)
    for off, typ, _sym, _add in relocs:
        for i in range(off, min(off + RELOC_SIZE.get(typ, 0), len(data))):
            data[i] = 0
    return bytes(data), tuple((o, t) for o, t, _s, _a in relocs)


def same(r, m, vasm):
    if r[0] == "err" and m[0] == "err":
        return True
    return r[0] == m[0] == "ok" and (masked(r[1]) == masked(m[1]) if vasm else r[1] == m[1])


def compare(cpu, syntax, cases):
    """Against vasm, the same source also goes to GNU as `--mri`: vasm and
    GNU as part ways by design often enough (which register names a 68000
    has, `movep` to `(An)`, `fsub.x fp1` with one operand), and where they
    do rsasm follows GNU as, so a case is only a finding when rsasm matches
    neither. The others are counted as `split`."""
    with tempfile.TemporaryDirectory() as d:
        ref, plan = assemble("ref", cpu, syntax, cases, d)
        mine, _ = assemble("rsasm", cpu, syntax, cases, d, plan)
        gnu = assemble("gnu", cpu, syntax, cases, d, plan)[0] if syntax == "vasm" else None
    out = []
    for i, (case, r, m) in enumerate(zip(cases, ref, mine)):
        if same(r, m, syntax == "vasm"):
            out.append(("agree", case, r, m))
        elif gnu is not None and same(gnu[i], m, False):
            out.append(("split", case, r, m))
        else:
            out.append(("rsasm", case, r, m))
    return out


# ============================================================================
# Commands
# ============================================================================


def cases_for(cpu, syntax, count, seed, mutations, only, include_hand, exclusive=False):
    by_name, archs = load(include_hand)
    arch = archs[CPUS[cpu][3]]
    rng = random.Random(seed)
    g = Gen(rng, arch, syntax)
    names = [n for n in by_name if not only or re.search(only, n)]
    pool = [f for n in names for f in by_name[n] if f.arch & arch]
    other = [f for n in names for f in by_name[n] if not f.arch & arch]
    out = []
    while len(out) < count and pool:
        if other and rng.random() < mutations / 3:
            form = rng.choice(other)
            case = make_case(g, form, by_name[form.name], rng)
            if case:
                case.mutated = True
        else:
            form = rng.choice(pool)
            case = make_case(g, form, by_name[form.name], rng,
                             mutate=rng.random() < mutations, exclusive=exclusive)
        if case:
            out.append(case)
    return out


def job(args):
    cpu, syntax, count, seed, mutations, only, include_hand = args
    cases = cases_for(cpu, syntax, count, seed, mutations, only, include_hand)
    return cpu, syntax, compare(cpu, syntax, cases)


def fuzz(a):
    cpus = list(CPUS) if a.cpu == "all" else a.cpu.split(",")
    syntaxes = ["gas", "mot"] if a.syntax == "all" else a.syntax.split(",")
    jobs = []
    seed = a.seed
    for cpu in cpus:
        for syntax in syntaxes:
            if syntax == "vasm" and CPUS[cpu][2] is None:
                continue
            left = a.count // (len(cpus) * len(syntaxes))
            while left > 0:
                n = min(a.batch, left)
                jobs.append((cpu, syntax, n, seed, a.mutations, a.only, a.all))
                seed += 1
                left -= n
    tally = collections.Counter()
    findings = collections.defaultdict(list)
    with ProcessPoolExecutor(a.jobs) as ex:
        for cpu, syntax, results in ex.map(job, jobs):
            for kind, case, r, m in results:
                tally[kind] += 1
                if kind == "rsasm" or (kind == "split" and a.splits):
                    key = (cpu, syntax, case.form.name, case.form.args, case.mutated,
                           r[0], m[0])
                    findings[key].append((case, r, m))
    print("--- %s" % ", ".join("%d %s" % (n, k) for k, n in sorted(tally.items())))
    shown = 0
    for key, items in sorted(findings.items(), key=lambda kv: -len(kv[1])):
        if shown >= a.limit:
            print("... %d more groups" % (len(findings) - shown))
            break
        shown += 1
        cpu, syntax, name, args, mutated, rk, mk = key
        case, r, m = min(items, key=lambda x: len(x[0].text))
        print("### [%s %s] %s %s%s x%d" % (cpu, syntax, name, args,
                                           " (mutated)" if mutated else "", len(items)))
        print("    %s" % case.text)
        print("  ref:   %s" % fmt(r))
        print("  rsasm: %s" % fmt(m))
    return 1 if tally["rsasm"] else 0


# The CPUs the corpora are for, in the order `corpus --first` gives each form
# to the first one that has it.
CORPUS_ORDER = ["68020", "68030", "68040", "68060", "cpu32", "fidoa", "5475", "54455", "5208",
                "5206", "5407"]


def corpus(a):
    """One line per form the CPU has, which that form and no earlier one takes.

    With `--first`, only the forms no CPU before this one in CORPUS_ORDER
    has, so the corpora for all of them cover each form once."""
    by_name, archs = load(a.all)
    arch = archs[CPUS[a.cpu][3]]
    earlier = 0
    if a.first:
        for cpu in CORPUS_ORDER[:CORPUS_ORDER.index(a.cpu)]:
            earlier |= archs[CPUS[cpu][3]]
    rng = random.Random(a.seed)
    g = Gen(rng, arch, a.syntax)
    todo = []
    lines = {}
    for name, forms in by_name.items():
        if a.only and not re.search(a.only, name):
            continue
        for form in forms:
            if not form.arch & arch or form.arch & earlier:
                continue
            todo.append((name, form, forms))
            if mri_skips(g, form, forms):
                lines[(name, form.index)] = "# %s %s: GNU as --mri reads it differently" % (
                    name, form.args)
    # Operands are drawn without regard to what the CPU can address (a
    # ColdFire index is a long, say), so a line GNU as refuses is drawn again.
    for _round in range(40):
        batch = []
        for item in todo:
            name, form, forms = item
            if (name, form.index) in lines:
                continue
            case = make_case(g, form, forms, rng, exclusive=True)
            if case is None:
                lines[(name, form.index)] = "# %s %s: no operands only this form takes" % (
                    name, form.args)
            else:
                batch.append(((name, form.index), case))
        if not batch:
            break
        with tempfile.TemporaryDirectory() as d:
            results, _ = assemble("ref", a.cpu, a.syntax, [c for _, c in batch], d)
        for (key, case), res in zip(batch, results):
            if res[0] == "ok":
                lines[key] = ("\t" if g.syntax != "gas" else "") + case.text
    for name, form, _ in todo:
        print(lines.get((name, form.index),
                        "# %s %s: no line GNU as takes was found" % (name, form.args)))
    return 0


def check(a):
    lines = [ln.rstrip("\n") for ln in open(a.file)]
    lines = [ln.strip() for ln in lines if ln.strip() and not ln.lstrip().startswith("#")]
    cases = [Case(Form(("?", 0, 1, "", 0), 0), ln, False) for ln in lines]
    bad = 0
    for kind, case, r, m in compare(a.cpu, a.syntax, cases):
        if kind != "agree" or a.verbose:
            bad += kind != "agree"
            print("%s %s\n  ref:   %s\n  rsasm: %s" % (kind, case.text, fmt(r), fmt(m)))
    print("--- %d lines, %d differ" % (len(cases), bad))
    return 1 if bad else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("fuzz", "corpus", "check"):
        p = sub.add_parser(name)
        p.add_argument("--cpu", default="68020")
        p.add_argument("--syntax", default="gas")
        p.add_argument("--seed", type=int, default=1)
        p.add_argument("--only", default=None)
        p.add_argument("--all", action="store_true")
        if name == "corpus":
            p.add_argument("--first", action="store_true")
        if name == "fuzz":
            p.add_argument("--count", type=int, default=10000)
            p.add_argument("--batch", type=int, default=150)
            p.add_argument("--mutations", type=float, default=0.15)
            p.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
            p.add_argument("--limit", type=int, default=40)
            p.add_argument("--splits", action="store_true",
                           help="list the cases vasm and GNU as part on, too")
        if name == "check":
            p.add_argument("--verbose", "-v", action="store_true")
            p.add_argument("file")
    a = ap.parse_args()
    return {"fuzz": fuzz, "corpus": corpus, "check": check}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
