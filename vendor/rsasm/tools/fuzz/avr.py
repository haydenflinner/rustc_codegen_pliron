#!/usr/bin/env python3
"""Differential fuzzer for rsasm's AVR backend: whole random programs.

Each case is a program: labels in several sections, instructions drawn from
every row of GNU binutils' AVR opcode table with operands of every shape
(registers in each spelling, pointer modes, the `lo8()` family of modifiers on
numbers, labels and undefined symbols, branches forward and back across
`.skip`s that put some targets out of reach), data with and without
modifiers, alignment and `.org`. It is assembled for a random core by
avr-elf-as and by rsasm, and the two objects are compared: `e_flags`, every
allocated section and `.avr.prop` byte for byte, the global, weak and
undefined symbols, and every relocation, read the way a linker reads it. A
program with nothing undefined is also linked by avr-elf-ld at address 0,
sections laid end to end (and, for the cores with a 22-bit program counter,
without the trampolines GNU ld would build for `gs()`), and the image compared
with `rsasm -f bin`, which is what checks every displacement and every byte a
relocation stands for.

    tools/fuzz/avr.py fuzz --count 2000 --seed 1
    tools/fuzz/avr.py fuzz --core avr51 --count 500 --mutations 0.5
    tools/fuzz/avr.py check --core avr5 prog.s

A case is:

    agree     both accepted with the same object (and image), or both refused.
    rsasm     they differ. These are the findings; the exit status is 1 when
              there are any. Each is shown reduced: statements are removed
              while the difference stays the same kind.
    known     they differ in a way rsasm means to: GNU as keeps the low bits
              of a value with only a warning where rsasm refuses it (an `ldi`
              constant below -255, an AVR-tiny `lds`/`sts` address outside
              0x40-0xbf, a `call` target past 22 bits, `pm()` of an odd
              number), and GNU as stops on `lo8(gs(...))` of a number.

`--mutations` is the fraction of programs given one statement meant to be
refused: a register of the wrong class, a constant out of range, an odd
branch target, an operand too many or too few, a modifier where there is
none.

The operand rules come from `include/opcode/avr.h` and
`gas/config/tc-avr.c`, not from rsasm's tables. Symbols are declared at the
top of each program, so both assemblers put them in the symbol table in the
same order; which order an undeclared symbol lands in says nothing to a
linker.

Environment: RSASM (default target/debug/rsasm under the repository root) and
RSASM_ORACLES (default target/oracles), whose bin directory has avr-elf-as,
avr-elf-ld and avr-elf-objcopy from tools/oracles/build.sh.
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
RSASM = os.environ.get("RSASM", os.path.join(ROOT, "target", "debug", "rsasm"))
ORACLES = os.environ.get("RSASM_ORACLES", os.path.join(ROOT, "target", "oracles"))
BIN = os.path.join(ORACLES, "bin")

# The instruction-set bits of include/opcode/avr.h, and the sets of the cores
# below (mcu_types in gas/config/tc-avr.c). They only steer generation towards
# instructions a core has; whether a core has one is still the assemblers' to
# say.
I1200, LPM, LPMX, SRAM, TINY, MEGA, MUL, ELPM, ELPMX, SPM, BRK, EIND, MOVW, SPMX, DES, RMW = (
    1 << i for i in range(16))
TINY1 = I1200 | LPM
X2XXX = TINY1 | SRAM
X2XXXA = I1200 | SRAM
M603 = X2XXX | MEGA
M161 = M603 | MUL | MOVW | LPMX | SPM
M128 = M161 | BRK | ELPM | ELPMX
XMEGA = M128 | EIND | SPMX | DES
AVR25 = X2XXX | MOVW | LPMX | SPM | BRK
AVR35 = M603 | MOVW | LPMX | SPM | BRK
AVR4 = X2XXX | MUL | MOVW | LPMX | SPM | BRK
AVR6 = I1200 | LPM | LPMX | SRAM | MEGA | MUL | ELPM | ELPMX | SPM | BRK | EIND | MOVW

# rsasm target | avr-elf-as -mmcu (None for none) | avr-elf-ld emulation | set
CORES = [
    ("avr", None, "avr2", X2XXX),
    ("avr1", "avr1", "avr1", TINY1),
    ("avr2", "avr2", "avr2", AVR25),
    ("avr25", "avr25", "avr25", AVR25),
    ("avr3", "avr3", "avr3", M603 | ELPM | AVR35),
    ("avr31", "avr31", "avr31", M603 | ELPM),
    ("avr35", "avr35", "avr35", AVR35),
    ("avr4", "avr4", "avr4", AVR4),
    ("avr5", "avr5", "avr5", M128),
    ("avr51", "avr51", "avr51", M128),
    ("avr6", "avr6", "avr6", AVR6),
    ("avrxmega2", "avrxmega2", "avrxmega2", XMEGA),
    ("avrxmega3", "avrxmega3", "avrxmega3", XMEGA),
    ("avrxmega6", "avrxmega6", "avrxmega6", XMEGA),
    ("avrxmega7", "avrxmega7", "avrxmega7", XMEGA),
    ("avrtiny", "avrtiny", "avrtiny", I1200 | BRK | SRAM | TINY),
    ("atmega328p", "atmega328p", "avr5", M161 | BRK),
    ("attiny13", "attiny13", "avr25", AVR25),
    ("attiny26", "attiny26", "avr2", X2XXX | LPMX),
    ("at90s1200", "at90s1200", "avr1", I1200),
    ("atxmega128a1u", "atxmega128a1u", "avrxmega7", XMEGA | RMW),
]

# Every row of include/opcode/avr.h as (mnemonic, constraints, set). The
# one-operand `clr`, `lsl`, `rol` and `tst` are its `r=r` rows.
FORMS = [
    ("clc", "", I1200), ("clh", "", I1200), ("cli", "", I1200), ("cln", "", I1200),
    ("cls", "", I1200), ("clt", "", I1200), ("clv", "", I1200), ("clz", "", I1200),
    ("sec", "", I1200), ("seh", "", I1200), ("sei", "", I1200), ("sen", "", I1200),
    ("ses", "", I1200), ("set", "", I1200), ("sev", "", I1200), ("sez", "", I1200),
    ("bclr", "S", I1200), ("bset", "S", I1200), ("icall", "", X2XXXA), ("ijmp", "", X2XXXA),
    ("lpm", "", TINY1), ("lpm", "r,z", LPMX), ("elpm", "", ELPM), ("elpm", "r,z", ELPMX),
    ("nop", "", I1200), ("ret", "", I1200), ("reti", "", I1200), ("sleep", "", I1200),
    ("break", "", BRK), ("wdr", "", I1200), ("spm", "", SPM), ("spm", "z", SPMX),
    ("adc", "r,r", I1200), ("add", "r,r", I1200), ("and", "r,r", I1200), ("cp", "r,r", I1200),
    ("cpc", "r,r", I1200), ("cpse", "r,r", I1200), ("eor", "r,r", I1200),
    ("mov", "r,r", I1200), ("mul", "r,r", MUL), ("or", "r,r", I1200), ("sbc", "r,r", I1200),
    ("sub", "r,r", I1200), ("clr", "r", I1200), ("lsl", "r", I1200), ("rol", "r", I1200),
    ("tst", "r", I1200), ("andi", "d,M", I1200), ("cbr", "d,n", I1200), ("ldi", "d,M", I1200),
    ("ser", "d", I1200), ("ori", "d,M", I1200), ("sbr", "d,M", I1200), ("cpi", "d,M", I1200),
    ("sbci", "d,M", I1200), ("subi", "d,M", I1200), ("sbrc", "r,s", I1200),
    ("sbrs", "r,s", I1200), ("bld", "r,s", I1200), ("bst", "r,s", I1200),
    ("in", "r,P", I1200), ("out", "P,r", I1200), ("adiw", "w,K", X2XXX), ("sbiw", "w,K", X2XXX),
    ("cbi", "p,s", I1200), ("sbi", "p,s", I1200), ("sbic", "p,s", I1200), ("sbis", "p,s", I1200),
    ("brcc", "l", I1200), ("brcs", "l", I1200), ("breq", "l", I1200), ("brge", "l", I1200),
    ("brhc", "l", I1200), ("brhs", "l", I1200), ("brid", "l", I1200), ("brie", "l", I1200),
    ("brlo", "l", I1200), ("brlt", "l", I1200), ("brmi", "l", I1200), ("brne", "l", I1200),
    ("brpl", "l", I1200), ("brsh", "l", I1200), ("brtc", "l", I1200), ("brts", "l", I1200),
    ("brvc", "l", I1200), ("brvs", "l", I1200), ("brbc", "s,l", I1200), ("brbs", "s,l", I1200),
    ("rcall", "L", I1200), ("rjmp", "L", I1200), ("call", "h", MEGA), ("jmp", "h", MEGA),
    ("asr", "r", I1200), ("com", "r", I1200), ("dec", "r", I1200), ("inc", "r", I1200),
    ("lsr", "r", I1200), ("neg", "r", I1200), ("pop", "r", X2XXXA), ("push", "r", X2XXXA),
    ("ror", "r", I1200), ("swap", "r", I1200),
    ("xch", "z,r", RMW), ("las", "z,r", RMW), ("lac", "z,r", RMW), ("lat", "z,r", RMW),
    ("movw", "v,v", MOVW), ("muls", "d,d", MUL), ("mulsu", "a,a", MUL), ("fmul", "a,a", MUL),
    ("fmuls", "a,a", MUL), ("fmulsu", "a,a", MUL),
    ("sts", "j,d", TINY), ("sts", "i,r", X2XXX), ("lds", "d,j", TINY), ("lds", "r,i", X2XXX),
    ("ldd", "r,b", X2XXX), ("ld", "r,e", I1200), ("std", "b,r", X2XXX), ("st", "e,r", I1200),
    ("eicall", "", EIND), ("eijmp", "", EIND), ("des", "E", DES),
]

# `ldi` modifiers, with whether a `pm(`/`gs(` may go inside (avr_ldi_expression).
LDI_MODS = [("lo8", True), ("hi8", True), ("hh8", True), ("hlo8", False), ("hhi8", False),
            ("pm_lo8", False), ("pm_hi8", False), ("pm_hh8", False)]

# ============================================================================
# Reading objects
# ============================================================================

R_AVR = {0: "NONE", 1: "32", 2: "7_PCREL", 3: "13_PCREL", 4: "16", 5: "16_PM", 6: "LO8_LDI",
         7: "HI8_LDI", 8: "HH8_LDI", 9: "LO8_LDI_NEG", 10: "HI8_LDI_NEG", 11: "HH8_LDI_NEG",
         12: "LO8_LDI_PM", 13: "HI8_LDI_PM", 14: "HH8_LDI_PM", 15: "LO8_LDI_PM_NEG",
         16: "HI8_LDI_PM_NEG", 17: "HH8_LDI_PM_NEG", 18: "CALL", 19: "LDI", 20: "6",
         21: "6_ADIW", 22: "MS8_LDI", 23: "MS8_LDI_NEG", 24: "LO8_LDI_GS", 25: "HI8_LDI_GS",
         26: "8", 27: "8_LO8", 28: "8_HI8", 29: "8_HLO8", 30: "DIFF8", 31: "DIFF16",
         32: "DIFF32", 33: "LDS_STS_16", 34: "PORT6", 35: "PORT5", 36: "32_PCREL"}


def canon(path):
    """What two AVR objects have to agree on, as tools/mc-diff/canon.sh reads
    it, with the symbols sorted: which order undeclared symbols come in is not
    what this fuzzer is for."""
    data = open(path, "rb").read()
    (e_flags,) = struct.unpack_from("<I", data, 36)
    shoff, = struct.unpack_from("<I", data, 32)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 46)
    secs = []
    for i in range(shnum):
        name, ty, flags, _addr, off, size, link, info, align, entsize = struct.unpack_from(
            "<IIIIIIIIII", data, shoff + i * shentsize)
        secs.append(dict(name=name, ty=ty, flags=flags, off=off, size=size, link=link,
                         info=info, align=align, entsize=entsize))
    strtab = secs[shstrndx]

    def cstr(sec, off):
        start = sec["off"] + off
        return data[start:data.index(b"\0", start)].decode()

    for s in secs:
        s["name"] = cstr(strtab, s["name"])
    out = [f"flags {e_flags:#x}"]
    for s in sorted(secs, key=lambda s: s["name"]):
        if s["size"] == 0 or not (s["flags"] & 2 or s["name"] == ".avr.prop"):
            continue
        out.append(f"section {s['name']} type={s['ty']} flags={s['flags']:#x} "
                   f"size={s['size']:#x} align={s['align']}")
        if s["ty"] != 8:
            out.append("  " + data[s["off"]:s["off"] + s["size"]].hex())
    syms = []
    symtab = next((s for s in secs if s["ty"] == 2), None)
    if symtab:
        names = secs[symtab["link"]]
        for i in range(symtab["size"] // 16):
            nm, value, size, info, other, shndx = struct.unpack_from(
                "<IIIBBH", data, symtab["off"] + i * 16)
            sec = ("UND" if shndx == 0 else "ABS" if shndx == 0xfff1 else
                   "COM" if shndx == 0xfff2 else secs[shndx]["name"])
            syms.append(dict(name=cstr(names, nm), value=value, bind=info >> 4, ty=info & 15,
                             vis=other & 3, sec=sec))
        for s in sorted(syms[1:], key=lambda s: s["name"]):
            if s["bind"] != 0 or s["sec"] == "UND":
                out.append(f"symbol {s['name']} bind={s['bind']} type={s['ty']} vis={s['vis']} "
                           f"{s['sec']}+{s['value']:#x}")
    relocs = []
    for s in secs:
        if s["ty"] != 4:
            continue
        target = secs[s["info"]]["name"]
        for i in range(s["size"] // 12):
            off, info, addend = struct.unpack_from("<IIi", data, s["off"] + i * 12)
            sym = syms[info >> 8]
            if (info >> 8) == 0:
                where = f"*ABS*{addend:+#x}"
            elif sym["bind"] != 0 or sym["sec"] == "UND":
                where = f"{sym['name']}{addend:+#x}"
            else:
                where = f"{sym['sec']}+{sym['value'] + addend:#x}"
            relocs.append((target, off, f"reloc {target} {off:#x} "
                           f"R_AVR_{R_AVR.get(info & 255, info & 255)} {where}"))
    relocs.sort(key=lambda r: (r[0], r[1]))
    out.extend(r[2] for r in relocs)
    return "\n".join(out)


# ============================================================================
# Running the tools
# ============================================================================


def run(cmd, cwd):
    try:
        p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=60)
    except subprocess.TimeoutExpired:
        return 124, "timeout"
    return p.returncode, p.stdout + p.stderr


def first_error(text):
    for line in text.splitlines():
        if "rror" in line or "atal" in line or "panicked" in line:
            return line.strip()
    return text.strip().splitlines()[0] if text.strip() else ""


def assemble(source, core, workdir):
    """Both results for one program: (gas, rsasm), each a dict with ok, text
    (the canonical object or the first error) and, where it was linked, img."""
    arch, mmcu, emul, _ = core
    src = os.path.join(workdir, "in.s")
    with open(src, "w") as f:
        f.write(source)
    res = {}
    flags = [f"-mmcu={mmcu}"] if mmcu else []
    rc, log = run([os.path.join(BIN, "avr-elf-as")] + flags + ["-o", "ref.o", "in.s"], workdir)
    if rc == 0 and "Error" not in log:
        res["gas"] = dict(ok=True, text=canon(os.path.join(workdir, "ref.o")), log=log)
    else:
        res["gas"] = dict(ok=False, text=first_error(log), log=log)
    rc, log = run([RSASM, "-a", arch, "-o", "rs.o", "in.s"], workdir)
    if rc == 0:
        res["rsasm"] = dict(ok=True, text=canon(os.path.join(workdir, "rs.o")), log=log)
    elif rc in (0, 1):
        res["rsasm"] = dict(ok=False, text=first_error(log), log=log)
    else:
        res["rsasm"] = dict(ok=False, text=f"CRASH {rc}: {first_error(log)}", log=log)
    if res["gas"]["ok"] and res["rsasm"]["ok"] and " UND+" not in res["gas"]["text"]:
        link(res, core, workdir)
    return res["gas"], res["rsasm"]


def link(res, core, workdir):
    """Links GNU as's object at 0 and assembles rsasm's flat image, as
    tools/flat-diff/run.sh does."""
    arch, _mmcu, emul, _ = core
    sections = []
    data = open(os.path.join(workdir, "ref.o"), "rb").read()
    shoff, = struct.unpack_from("<I", data, 32)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 46)
    hdrs = [struct.unpack_from("<IIIIIIIIII", data, shoff + i * shentsize) for i in range(shnum)]
    stroff = hdrs[shstrndx][4]
    for h in hdrs:
        name = data[stroff + h[0]:data.index(b"\0", stroff + h[0])].decode()
        if h[1] == 1 and h[2] & 2:
            sections.append(name)
    with open(os.path.join(workdir, "link.ld"), "w") as f:
        f.write("SECTIONS {\n  . = 0;\n")
        for s in sections:
            f.write(f"  {s} : {{ *({s}) }}\n")
        f.write("  .rest : { *(*) }\n}\n")
    # The cores with a 22-bit program counter get trampolines for `gs()` from
    # GNU ld, which a flat image has no linker to build.
    stubs = ["--no-stubs"] if emul in ("avr6", "avrxmega6", "avrxmega7") else []
    rc, log = run([os.path.join(BIN, "avr-elf-ld"), "-m", emul] + stubs + ["-e", "0", "-T",
                   "link.ld", "-o", "ref.elf", "ref.o"], workdir)
    if rc == 0:
        only = [f"--only-section={s}" for s in sections]
        run([os.path.join(BIN, "avr-elf-objcopy"), "-O", "binary"] + only + ["ref.elf", "ref.bin"],
            workdir)
        path = os.path.join(workdir, "ref.bin")
        res["gas"]["img"] = open(path, "rb").read().hex() if os.path.exists(path) else ""
    else:
        res["gas"]["img"] = "LINK-ERROR " + first_error(log)
    rc, log = run([RSASM, "-a", arch, "-f", "bin", "--base", "0", "-o", "rs.bin", "in.s"], workdir)
    if rc == 0:
        res["rsasm"]["img"] = open(os.path.join(workdir, "rs.bin"), "rb").read().hex()
    else:
        res["rsasm"]["img"] = "ERROR " + first_error(log)


# ============================================================================
# Programs
# ============================================================================


class Program:
    def __init__(self, rng, core, mutate):
        self.rng = rng
        self.core = core
        self.tiny = core[0] == "avrtiny"
        self.header = []
        self.stmts = []
        self.code_labels = []
        self.data_labels = []
        # Half the programs refer to nothing undefined, so that they can be
        # linked and compared as images too.
        self.externs = [f"ext{i}" for i in range(3)] if rng.random() < 0.5 else []
        self.globals = []
        self.weaks = []
        self.equs = {"PORTB": 0x18, "SREG": 0x3f, "BIT": 3, "OFF": 12, "ADDR": 0x100}
        self.mutation = None
        self.build(mutate)

    # ---- operands ----------------------------------------------------------

    def reg(self, lo=0, hi=31, even=False):
        r = self.rng
        if self.tiny:
            lo = max(lo, 16)
        n = r.randint(lo, hi)
        if even:
            n &= ~1
            if n < lo:
                n += 2
        spell = r.random()
        if spell < 0.08 and n >= 26:
            return ["xl", "xh", "yl", "yh", "zl", "zh"][n - 26]
        if spell < 0.12:
            return f"R{n}"
        if spell < 0.15:
            return str(n)
        return f"r{n}"

    def value(self, lo, hi):
        r = self.rng
        pick = r.random()
        if pick < 0.3:
            return str(r.choice([lo, hi, (lo + hi) // 2]))
        if pick < 0.4:
            return hex(r.randint(lo, hi))
        return str(r.randint(lo, hi))

    def symbol(self, code=True):
        r = self.rng
        pool = (self.code_labels if code else self.data_labels) + self.externs
        if not pool:
            pool = self.code_labels + self.data_labels + self.externs
        if not pool:
            return hex(r.randint(0, 0x7fff) * 2)
        name = r.choice(pool)
        k = r.random()
        if k < 0.15:
            return f"{name} + {r.choice([1, 2, 4, 0x100])}"
        if k < 0.2:
            return f"{name} - {r.choice([2, 6])}"
        return name

    def extern(self, otherwise):
        return self.rng.choice(self.externs) if self.externs else otherwise

    def ldi_operand(self):
        r = self.rng
        k = r.random()
        if k < 0.35:
            return self.value(0, 255)
        if k < 0.45:
            return str(r.randint(-128, -1))
        if k < 0.5:
            return r.choice(["BIT", "OFF", "(1 << 3) | 1", "'A'", "~0x0f & 0xff"])
        mod, pm_ok = r.choice(LDI_MODS)
        inner = self.symbol(code=r.random() < 0.6) if r.random() < 0.7 else hex(
            r.randint(0, 0x3ffff) & ~1)
        if pm_ok and r.random() < 0.3:
            fn = r.choice(["pm", "gs"])
            if r.random() < 0.3:
                return f"{mod}(-({fn}({inner})))"
            return f"{mod}({fn}({inner}))"
        if r.random() < 0.2:
            return f"{mod}(-({inner}))"
        return f"{mod}({inner})"

    def target(self, near):
        r = self.rng
        k = r.random()
        if k < 0.6 and self.code_labels:
            return r.choice(self.code_labels) if not near else r.choice(self.code_labels[-6:])
        if k < 0.7:
            return r.choice([".", ". + 2", ". - 4", f". + {r.randint(-60, 60) * 2}"])
        if k < 0.85:
            return str(r.randint(-40, 40) * 2)
        return self.symbol()

    def operand(self, c):
        r = self.rng
        if c == "r":
            return self.reg()
        if c == "d":
            return self.reg(16)
        if c == "a":
            return self.reg(16, 23)
        if c == "v":
            return r.choice(["x", "y", "z"]) if r.random() < 0.15 else self.reg(0, 30, even=True)
        if c == "w":
            return r.choice(["x", "y", "z", "r24", "r26", "r28", "r30", "X"])
        if c == "e":
            return r.choice(["X", "X+", "-X", "Y", "Y+", "-Y", "Z", "Z+", "-Z", "x", "y+", "-z"])
        if c == "b":
            base = r.choice(["Y", "Z", "y", "z"])
            return f"{base}+{r.choice([self.value(0, 63), 'OFF', '(2*3)'])}"
        if c == "z":
            return r.choice(["Z", "Z+", "z", "z+"])
        if c == "M":
            return self.ldi_operand()
        if c == "n":
            return self.value(0, 255)
        if c in "sS":
            return r.choice([self.value(0, 7), "BIT"])
        if c == "E":
            return self.value(0, 15)
        if c == "P":
            return r.choice([self.value(0, 63), "SREG", "PORTB + 1", self.extern("PORTB")])
        if c == "p":
            return r.choice([self.value(0, 31), "PORTB", self.extern("BIT")])
        if c == "K":
            return r.choice([self.value(0, 63), "OFF", self.extern("OFF")])
        if c == "i":
            return r.choice([self.value(0, 0xffff), "ADDR", self.symbol(code=False)])
        if c == "j":
            return r.choice([self.value(0x40, 0xbf), self.extern("0x60")])
        if c == "l":
            return self.target(near=True)
        if c == "L":
            return self.target(near=False)
        if c == "h":
            if r.random() < 0.3:
                return hex(r.randint(0, 0x3fffff) & ~1)
            return self.target(near=False) if r.random() < 0.3 else self.symbol()
        raise ValueError(c)

    def instruction(self):
        isa = self.core[3]
        have = [f for f in FORMS if f[2] & isa == f[2]]
        # Now and then a form the core may not have, to see both refuse it.
        pool = FORMS if self.rng.random() < 0.03 else have
        name, ops, _ = self.rng.choice(pool)
        letters = [c for c in ops if c != ","]
        return (name + " " + ", ".join(self.operand(c) for c in letters)).rstrip()

    # ---- mutations ---------------------------------------------------------

    MUTATIONS = ["low-register", "high-constant", "odd-target", "extra-operand",
                 "missing-operand", "bad-pointer", "modifier-in-data", "bad-modifier"]

    def mutated(self):
        r = self.rng
        m = r.choice(self.MUTATIONS)
        self.mutation = m
        if m == "low-register":
            return r.choice(["ldi r15, 1", "andi r3, 0x0f", "adiw r25, 1", "movw r1, r2",
                             "fmul r24, r16", "ser r0"])
        if m == "high-constant":
            return r.choice(["ldi r16, 256", "in r0, 64", "cbi 32, 1", "sbrc r0, 8",
                             "ldd r0, Y+64", "adiw r24, 64", "des 16", "bset 8"])
        if m == "odd-target":
            return r.choice(["rjmp 3", "breq 1", "call 5", "brbs 1, 7"])
        if m == "extra-operand":
            return r.choice(["nop r0", "inc r0, r1", "ret 1", "ldi r16, 1, 2"])
        if m == "missing-operand":
            return r.choice(["add r0", "ldi r16", "sbi 3", "brbs 1"])
        if m == "bad-pointer":
            return r.choice(["ld r0, -X+", "ldd r0, X+1", "ldd r0, Y", "lpm r0, -Z", "st W, r0"])
        if m == "modifier-in-data":
            return r.choice([".byte pm(0x10)", ".word lo8(0x10)", ".long hi8(0x10)"])
        return r.choice(["ldi r16, LO8(0x10)", "ldi r16, pm_lo8(pm(0x10))",
                         "ldi r16, lo8(0x10) + 1", "ldi r16, hlo8(gs(0x10))"])

    # ---- the program -------------------------------------------------------

    def build(self, mutate):
        r = self.rng
        n = r.randint(4, 40)
        # Sections GNU as creates before reading anything come first, so both
        # lay the flat image out in the same order.
        self.header += ["\t.text", "\t.data", "\t.text"]
        for k, v in self.equs.items():
            self.header.append(f"\t.equ\t{k}, {v:#x}")
        section = ".text"
        labels = 0
        mutate_at = r.randrange(n) if mutate else -1
        for i in range(n):
            if i == mutate_at:
                self.stmts.append("\t" + self.mutated())
                continue
            k = r.random()
            if k < 0.08:
                section = r.choice([".text", ".text", ".data", ".section .init0,\"ax\",@progbits",
                                    ".section .progmem.data,\"a\",@progbits"])
                self.stmts.append("\t" + section)
                continue
            if k < 0.2:
                labels += 1
                name = f"l{labels}"
                code = "data" not in section
                (self.code_labels if code else self.data_labels).append(name)
                roll = r.random()
                if roll < 0.2:
                    self.globals.append(name)
                elif roll < 0.25:
                    self.weaks.append(name)
                self.stmts.append(f"{name}:")
                continue
            if k < 0.3:
                self.stmts.append("\t" + self.data())
                continue
            if k < 0.34:
                self.stmts.append("\t" + r.choice([
                    ".balign 2", ".balign 4", ".p2align 3", ".balign 8, 0x55", ".align 1",
                    f".skip {r.choice([2, 4, 120, 130, 4090, 4100])}",
                ]))
                continue
            self.stmts.append("\t" + self.instruction())
        # A label used but never defined would make every case the same
        # error; define them all at the end of .text.
        self.stmts.append("\t.text")
        used = set(re.findall(r"\bl\d+\b", "\n".join(self.stmts)))
        defined = {s[:-1] for s in self.stmts if s.endswith(":")}
        for name in sorted(used - defined):
            self.stmts.append(f"{name}:\tnop")
        for name in self.globals:
            self.header.append(f"\t.globl\t{name}")
        for name in self.weaks:
            self.header.append(f"\t.weak\t{name}")
        if any(e in "\n".join(self.stmts) for e in self.externs):
            self.header.append("\t.globl\t" + ", ".join(self.externs))

    def data(self):
        r = self.rng
        k = r.random()
        if k < 0.3:
            return f".byte {self.value(0, 255)}, {r.choice(['lo8', 'hi8', 'hlo8', 'hh8'])}({self.symbol(code=r.random() < 0.5)})"
        if k < 0.6:
            return f".word {r.choice(['pm', 'gs'])}({self.symbol()}), {self.symbol(code=False)}"
        if k < 0.8:
            return f".long {self.symbol(code=r.random() < 0.5)}"
        return f".word {self.value(0, 0xffff)}, {r.choice(['pm', 'gs'])}({hex(r.randint(0, 0x7fff) * 2)})"

    def source(self, stmts=None):
        return "\n".join(self.header + (self.stmts if stmts is None else stmts)) + "\n"


# ============================================================================
# Comparing
# ============================================================================


def known(gas, rsasm):
    """Why a disagreement is one rsasm means, or None."""
    if gas["ok"] and not rsasm["ok"]:
        warn = gas["log"]
        msg = rsasm["text"]
        if "constant out of 8-bit range" in warn and "out of range (-255 to 255)" in msg:
            return "ldi below -255"
        if "out of range (64 to 191)" in msg:
            return "AVR-tiny lds/sts address"
        if "is not a multiple of 2" in msg and ("pm" in msg or "gs" in msg or "value" in msg):
            return "odd program-memory number"
        if "out of range (-4194304 to 8388607)" in msg:
            return "call beyond 22 bits"
    if not gas["ok"] and rsasm["ok"] and "unknown relocation type" in gas["text"]:
        return "lo8(gs()) of a number"
    return None


def classify(gas, rsasm):
    if gas["ok"] != rsasm["ok"]:
        why = known(gas, rsasm)
        return ("known", why) if why else ("rsasm", "accepts" if rsasm["ok"] else "refuses")
    if not gas["ok"]:
        return "agree", None
    if gas["text"] != rsasm["text"]:
        return "rsasm", "object"
    if "img" in gas:
        g, r = gas["img"], rsasm["img"]
        if g.startswith("LINK-ERROR") and r.startswith("ERROR"):
            return "agree", None
        if g != r:
            return "rsasm", "image"
    return "agree", None


def reduce(prog, kind, core, workdir):
    """Drops statements while the case stays a finding of the same kind."""
    stmts = list(prog.stmts)
    i = len(stmts) - 1
    while i >= 0:
        trial = stmts[:i] + stmts[i + 1:]
        g, r = assemble(prog.source(trial), core, workdir)
        if classify(g, r) == ("rsasm", kind):
            stmts = trial
        i -= 1
    return stmts


def run_case(job):
    seed, core_index, mutate = job
    rng = random.Random(seed)
    core = CORES[core_index]
    prog = Program(rng, core, mutate)
    with tempfile.TemporaryDirectory() as d:
        g, r = assemble(prog.source(), core, d)
        cls, detail = classify(g, r)
        shown = prog.source()
        if cls == "rsasm":
            shown = prog.source(reduce(prog, detail, core, d))
            g, r = assemble(shown, core, d)
    return dict(seed=seed, core=core[0], cls=cls, detail=detail, mutation=prog.mutation,
                source=shown, gas=g, rsasm=r)


def diff_text(a, b):
    import difflib
    lines = list(difflib.unified_diff(a.splitlines(), b.splitlines(), "gas", "rsasm", n=0,
                                      lineterm=""))
    return "\n".join(lines[2:40])


def fuzz(args):
    import multiprocessing

    cores = [i for i, c in enumerate(CORES) if not args.core or c[0] == args.core]
    if not cores:
        sys.exit(f"--core {args.core!r} is not one of " + ", ".join(c[0] for c in CORES))
    rng = random.Random(args.seed)
    jobs = [(rng.randrange(1 << 30), rng.choice(cores), rng.random() < args.mutations)
            for _ in range(args.count)]
    workers = args.jobs or max(1, (os.cpu_count() or 2) - 2)
    results = []
    with multiprocessing.Pool(workers) as pool:
        for res in pool.imap_unordered(run_case, jobs, chunksize=4):
            results.append(res)
    totals = collections.Counter(r["cls"] for r in results)
    print(f"=== {len(results)} programs: " + ", ".join(f"{k} {v}" for k, v in sorted(totals.items())))
    accepted = sum(1 for r in results if r["cls"] == "agree" and r["gas"]["ok"])
    print(f"  agreed on an object: {accepted}, agreed on refusing: {totals['agree'] - accepted}")
    linked = sum(1 for r in results if "img" in r["gas"])
    print(f"  linked and compared as images: {linked}")
    reasons = collections.Counter(r["detail"] for r in results if r["cls"] == "known")
    if reasons:
        print("  known: " + ", ".join(f"{k} {v}" for k, v in reasons.most_common()))
    findings = [r for r in results if r["cls"] == "rsasm"]
    findings.sort(key=lambda r: len(r["source"]))
    for f in findings[: args.limit]:
        print(f"--- [{f['core']}] seed {f['seed']}: {f['detail']}"
              + (f" (mutation {f['mutation']})" if f["mutation"] else ""))
        print("    " + f["source"].rstrip().replace("\n", "\n    "))
        g, r = f["gas"], f["rsasm"]
        if f["detail"] == "object":
            print(diff_text(g["text"], r["text"]))
        elif f["detail"] == "image":
            print(f"  gas+ld: {g['img'][:300]}\n  rsasm:  {r['img'][:300]}")
        else:
            print(f"  gas:   {g['text'][:300]}\n  rsasm: {r['text'][:300]}")
    if len(findings) > args.limit:
        print(f"... {len(findings) - args.limit} more findings")
    return 1 if findings else 0


def check(args):
    core = next((c for c in CORES if c[0] == args.core), None)
    if core is None:
        sys.exit(f"--core {args.core!r} is not one of " + ", ".join(c[0] for c in CORES))
    source = open(args.file).read()
    with tempfile.TemporaryDirectory() as d:
        g, r = assemble(source, core, d)
    cls, detail = classify(g, r)
    print(f"{cls}" + (f" ({detail})" if detail else ""))
    if cls != "agree":
        if detail == "object":
            print(diff_text(g["text"], r["text"]))
        else:
            print(f"gas:   {g['text'][:500]}\nrsasm: {r['text'][:500]}")
            if "img" in g:
                print(f"gas+ld: {g['img'][:300]}\nrsasm:  {r['img'][:300]}")
    return 0 if cls != "rsasm" else 1


def main():
    import argparse

    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    z = sub.add_parser("fuzz", help="generate random programs and compare")
    z.add_argument("--seed", type=int, default=1)
    z.add_argument("--count", type=int, default=1000)
    z.add_argument("--core", help="only this core (default: a random one per program)")
    z.add_argument("--mutations", type=float, default=0.25)
    z.add_argument("--jobs", type=int, default=0)
    z.add_argument("--limit", type=int, default=20, help="findings shown")
    c = sub.add_parser("check", help="compare one program")
    c.add_argument("--core", default="avr")
    c.add_argument("file")
    args = ap.parse_args()
    if not os.path.exists(os.path.join(BIN, "avr-elf-as")):
        sys.exit(f"no avr-elf-as in {BIN}; run tools/oracles/build.sh")
    sys.exit(fuzz(args) if args.cmd == "fuzz" else check(args))


if __name__ == "__main__":
    main()
