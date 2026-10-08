#!/usr/bin/env python3
"""Differential fuzzer for rsasm's MSP430 backend.

Random instructions are generated from the MSP430 and MSP430X addressing
modes and instruction formats as TI's family user's guides describe them (not
from rsasm's tables), assembled by msp430-elf-as and rsasm for the 430, 430X
and 430Xv2 instruction sets, and compared: each case's bytes, its relocations
and whether each assembler accepted it at all.

    tools/fuzz/msp430.py fuzz --count 20000
    tools/fuzz/msp430.py fuzz --isa 430x --only '^(mova|calla)$' --seed 7
    tools/fuzz/msp430.py check --isa 430 lines.txt

`check` compares the statements in a file, one case per line (`{` separates
statements within a case, as it does in MSP430 source).

Every case goes into a code section of its own, so one run of each assembler
covers a batch; a batch with errors is reassembled without the cases either
assembler rejected. GNU as is run with `-mP`, which only enables the
polymorphic branches in a code section. A case may define a label `L` after
its instructions and refer to it, and refer to the undefined `ext`, so
relocations against both kinds of symbol are compared, named the way a
linker reads them (a local label as its section plus an offset).

Some cases are deliberately invalid (`--mutations`): an MSP430X instruction
on the 430, out-of-range values, addressing modes an instruction does not
have, wrong operand counts and bad size suffixes. The exit status is 1 when
rsasm and GNU as disagree on any case the deviations in `DEVIATIONS` do not
explain.

Environment: RSASM (default target/debug/rsasm under the repository root),
RSASM_ORACLES (default target/oracles, for bin/msp430-elf-as).
"""

import collections
import os
import random
import re
import struct
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
RSASM = os.environ.get("RSASM", os.path.join(ROOT, "target/debug/rsasm"))
ORACLES = os.environ.get("RSASM_ORACLES", os.path.join(ROOT, "target/oracles"))
GAS = os.path.join(ORACLES, "bin/msp430-elf-as")

ISAS = {"430": ("msp430", "-mcpu=430"), "430x": ("msp430x", "-mcpu=430x"),
        "430xv2": ("msp430xv2", "-mcpu=430xv2")}

# Where rsasm refuses what GNU as accepts, on purpose (see src/arch/msp430).
DEVIATIONS = [
    # GNU as makes a meaningless opcode of a register count outside 1 to 16.
    (re.compile(r"^\s*(push|pop)m(\.\w)?\s+#(0|-|1[7-9]|[2-9]\d)"), "pushm count"),
    # rsasm refuses a polymorphic branch to anything but a label; GNU as drops
    # the rest.
    (re.compile(r"^\s*(jump|beq|bne|blt|bltu|bge|bgeu|bltn|bgt|bgtu|bleu|ble)\s+[#$]?L[+-]"), "polymorph addend"),
]

# Where rsasm accepts what GNU as refuses, on purpose: a number that names a
# register is read by its value, where GNU as reads its spelling and takes
# only `0`, not `0x0` or `00`, as `r0` (see src/arch/msp430/reg.rs).
ACCEPTED = [
    (re.compile(r"(^|[\s,@(])0(x0+|0+)\b"), "register zero spelled otherwise"),
]

# ---- generation ------------------------------------------------------------

DOUBLE = ["mov", "add", "addc", "sub", "subc", "cmp", "dadd", "bit", "bic", "bis", "xor", "and"]
SINGLE = ["rrc", "swpb", "rra", "sxt", "push", "call"]
EMUL_DST = ["inv", "dadc", "tst", "decd", "dec", "sbc", "adc", "incd", "inc", "clr", "pop", "rla", "rlc"]
IMPLIED = ["nop", "ret", "reti", "setc", "setz", "setn", "clrc", "clrz", "clrn", "dint", "eint"]
JUMPS = ["jmp", "jl", "jge", "jn", "jc", "jhs", "jnc", "jlo", "jz", "jeq", "jnz", "jne"]
POLY = ["jump", "beq", "bne", "blt", "bltu", "bge", "bgeu", "bltn", "bgt", "bgtu", "bleu", "ble"]
XDOUBLE = [m + "x" for m in DOUBLE]
XSINGLE = ["pushx", "rrax", "rrcx", "rrux", "swpbx", "sxtx"]
XEMUL = ["adcx", "clra", "clrx", "dadcx", "decx", "decda", "decdx", "incx", "incda", "incdx",
         "invx", "popx", "rlax", "rlcx", "sbcx", "tsta", "tstx"]
ROTM = ["rrcm", "rram", "rlam", "rrum"]
ADDRA = ["adda", "cmpa", "suba"]

INTERESTING = [0, 1, 2, 3, 4, 5, 7, 8, 9, -1, -2, -3, 0x7f, 0x80, 0xff, 0x100, 0x7fff, 0x8000,
               0xfffe, 0xffff, 0x10000, -0x8000, -0x8001, 0x12345, 0x7ffff, 0x80000, 0xfffff,
               0x100000, -0x7ffff, -0x80000, -0x80001, 0x1234, 0x200]


def reg(rng):
    r = rng.randrange(16)
    style = rng.random()
    if r == 0 and style < 0.3:
        return "pc"
    if r == 1 and style < 0.3:
        return "sp"
    if r == 2 and style < 0.3:
        return "sr"
    return ("R" if style > 0.95 else "r") + str(r)


def value(rng):
    v = rng.choice(INTERESTING) if rng.random() < 0.7 else rng.randrange(-0x90000, 0x110000)
    if v < 0:
        return str(v)
    r = rng.random()
    if r < 0.5:
        return hex(v)
    if r < 0.6:
        return "0%xh" % v
    if r < 0.65:
        return "(%d+%d)" % (v - 1, 1)
    return str(v)


def symbol(rng):
    base = rng.choice(["L", "ext"])
    r = rng.random()
    if r < 0.15:
        return base + "+" + str(rng.randrange(1, 8))
    if r < 0.2:
        return base + "-2"
    return base


def expr(rng):
    return symbol(rng) if rng.random() < 0.35 else value(rng)


def src(rng):
    k = rng.randrange(9)
    if k == 0:
        return reg(rng)
    if k == 1:
        e = expr(rng)
        r = rng.random()
        if r < 0.2:
            e = rng.choice(["lo", "hi", "llo", "lhi", "hlo", "hhi"]) + "(" + e + ")"
        return "#" + e
    if k == 8:
        # What the reference reads as a register followed by junk it ignores.
        return rng.choice([reg(rng) + "+" + str(rng.randrange(1, 4)), str(rng.randrange(16)) + "*2",
                           "0(" + reg(rng) + ")", "-2(" + reg(rng) + ")", "(1+1)(" + reg(rng) + ")"])
    if k == 2:
        return "&" + expr(rng)
    if k == 3:
        return expr(rng) + "(" + reg(rng) + ")"
    if k == 4:
        return "@" + reg(rng)
    if k == 5:
        return "@" + reg(rng) + "+"
    if k == 6:
        return symbol(rng)
    return "#" + rng.choice(["0", "1", "2", "4", "8", "-1", "0xffff"])


def dst(rng):
    k = rng.randrange(5)
    if k == 0:
        return reg(rng)
    if k == 1:
        return "&" + expr(rng)
    if k == 2:
        return expr(rng) + "(" + reg(rng) + ")"
    if k == 3:
        return symbol(rng)
    return src(rng)  # sometimes invalid


def size(rng, wide):
    r = rng.random()
    if r < 0.5:
        return ""
    if r < 0.7:
        return ".b"
    if r < 0.8:
        return ".w"
    if wide and r < 0.95:
        return ".a"
    return ".a" if r < 0.97 else rng.choice([".q", "."])


def case(rng, isa):
    """Returns (name, source) for one case."""
    x = isa != "430" or rng.random() < 0.05
    kinds = ["double", "single", "emul", "implied", "jump", "br", "poly"]
    if x:
        kinds += ["xdouble", "xsingle", "xemul", "mova", "calla", "pushm", "rotm", "adda", "rpt"] * 2
    k = rng.choice(kinds)
    if rng.random() < 0.03:
        # `.a` on an instruction that may or may not have an address form.
        m = rng.choice(DOUBLE + SINGLE + EMUL_DST + ["br", "call", "ret", "push", "pop"])
        return m, f"{m}.a " + ", ".join(src(rng) for _ in range(arity(m)))
    if k == "double":
        m = rng.choice(DOUBLE)
        s = f"{m}{size(rng, False)} {src(rng)}, {dst(rng)}"
    elif k == "single":
        m = rng.choice(SINGLE)
        s = f"{m}{size(rng, False)} {src(rng)}"
    elif k == "emul":
        m = rng.choice(EMUL_DST)
        s = f"{m}{size(rng, False)} {dst(rng)}"
    elif k == "implied":
        m = rng.choice(IMPLIED)
        s = m
    elif k == "jump":
        m = rng.choice(JUMPS)
        t = rng.choice([symbol(rng), "$+" + str(rng.randrange(-4, 1100)), "$-" + str(rng.randrange(0, 1100)),
                        str(rng.randrange(-1100, 1100))])
        s = f"{m} {t}"
    elif k == "br":
        m = "br"
        s = f"br {src(rng)}"
    elif k == "poly":
        m = rng.choice(POLY)
        s = f"{m} {rng.choice(['L', 'ext', 'L+2', '#L', '$ext'])}"
    elif k == "xdouble":
        m = rng.choice(XDOUBLE)
        s = f"{m}{size(rng, True)} {src(rng)}, {dst(rng)}"
    elif k == "xsingle":
        m = rng.choice(XSINGLE)
        s = f"{m}{size(rng, True)} {src(rng)}"
    elif k == "xemul":
        m = rng.choice(XEMUL)
        s = f"{m}{size(rng, True)} {dst(rng)}"
    elif k == "mova":
        m = rng.choice(["mova", "bra", "reta", "mov.a", "br.a"])
        if m in ("bra", "br.a"):
            s = f"{m} {src(rng)}"
        elif m == "reta":
            s = m
        else:
            s = f"{m} {src(rng)}, {dst(rng)}"
    elif k == "calla":
        m = "calla"
        s = f"calla {src(rng)}"
    elif k == "pushm":
        m = rng.choice(["pushm", "popm"])
        s = f"{m}{rng.choice(['', '.a', '.w'])} #{rng.randrange(0, 18)}, {reg(rng)}"
    elif k == "rotm":
        m = rng.choice(ROTM)
        s = f"{m}{rng.choice(['', '.a', '.w'])} #{rng.randrange(0, 6)}, {reg(rng)}"
    elif k == "adda":
        m = rng.choice(ADDRA)
        first = "#" + expr(rng) if rng.random() < 0.6 else reg(rng)
        s = f"{m} {first}, {reg(rng)}"
    else:
        m = "rpt"
        n = "#" + str(rng.randrange(0, 18)) if rng.random() < 0.6 else reg(rng)
        s = f"rpt {n} {{ {rng.choice(XSINGLE + XEMUL + XDOUBLE[:3])} {reg(rng)}" + \
            ("" if rng.random() < 0.7 else f", {reg(rng)}")
    return m, s


def build(cases):
    """Assembly source with each case, given with its number, in a section of
    its own followed by its own `L`; and the case on each line."""
    out, where = [], {}
    for i, (_, s) in cases:
        out.append(f'\t.section .t{i}, "ax", @progbits')
        where[len(out) + 1] = i
        out.append("\t" + re.sub(r"\bL\b", f"L{i}", s))
        out.append(f"L{i}:")
        out.append("\tnop")
    return "\n".join(out) + "\n", where


# ---- reading objects --------------------------------------------------------

def read_elf(path):
    """Per section named .tN: (bytes, [(offset, type, target, addend)])."""
    data = open(path, "rb").read()
    shoff, = struct.unpack_from("<I", data, 0x20)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x2e)
    shdrs = [struct.unpack_from("<IIIIIIIIII", data, shoff + i * shentsize) for i in range(shnum)]

    def cstr(off):
        return data[off:data.index(b"\0", off)].decode()

    names = [cstr(shdrs[shstrndx][4] + h[0]) for h in shdrs]
    syms = []
    for h in shdrs:
        if h[1] == 2:  # SHT_SYMTAB
            strtab = shdrs[h[6]]
            for j in range(h[5] // 16):
                name, value, sz, info, other, shndx = struct.unpack_from("<IIIBBH", data, h[4] + j * 16)
                syms.append((cstr(strtab[4] + name), value, info, shndx))
    out = {}
    for i, h in enumerate(shdrs):
        if names[i].startswith(".t") and h[1] == 1:
            out[names[i]] = [data[h[4]:h[4] + h[5]].hex(), []]
    for h in shdrs:
        if h[1] == 4:  # SHT_RELA
            target = names[h[7]]
            if target not in out:
                continue
            for j in range(h[5] // 12):
                off, info, addend = struct.unpack_from("<IIi", data, h[4] + j * 12)
                sym, ty = info >> 8, info & 0xff
                name, value, sinfo, shndx = syms[sym] if sym < len(syms) else ("?", 0, 0, 0)
                if sym == 0:
                    t = "*ABS*"
                elif sinfo >> 4 == 0 and shndx not in (0, 0xfff1):
                    t = f"{names[shndx]}+{value:#x}"
                else:
                    t = name
                out[target][1].append((off, ty, t, addend))
    return out


def error_lines(stderr, gnu):
    lines = set()
    if gnu:
        for m in re.finditer(r"in\.s:(\d+): Error", stderr):
            lines.add(int(m.group(1)))
    else:
        pending = False
        for line in stderr.split("\n"):
            if line.startswith("error"):
                pending = True
            m = re.search(r"-->\s.*in\.s:(\d+):", line)
            if m and pending:
                lines.add(int(m.group(1)))
                pending = False
    return lines


def run(cases, isa):
    """Result per case: ('ERROR',) or (bytes, relocs), for both assemblers."""
    arch, flag = ISAS[isa]
    results = {"gas": {}, "rsasm": {}}
    remaining = list(range(len(cases)))
    with tempfile.TemporaryDirectory() as d:
        src = os.path.join(d, "in.s")
        for tool in ("gas", "rsasm"):
            live = list(remaining)
            for _ in range(8):
                text, where = build([(i, cases[i]) for i in live])
                open(src, "w").write(text)
                obj = os.path.join(d, tool + ".o")
                if tool == "gas":
                    p = subprocess.run([GAS, flag, "-mP", "-o", obj, src], capture_output=True, text=True, cwd=d)
                    ok = p.returncode == 0 and "Error" not in p.stderr
                else:
                    p = subprocess.run([RSASM, "-a", arch, "-o", obj, src], capture_output=True, text=True, cwd=d)
                    ok = p.returncode == 0
                if ok:
                    objs = read_elf(obj)
                    for i in live:
                        data, relocs = objs.get(f".t{i}", ("", []))
                        results[tool][i] = (data, tuple(relocs))
                    break
                bad = {where[ln] for ln in error_lines(p.stderr, tool == "gas") if ln in where}
                if not bad:
                    for i in live:
                        results[tool][i] = ("ERROR", p.stderr.strip().split("\n")[-1][:100])
                    break
                for i in bad:
                    results[tool][i] = ("ERROR",)
                live = [i for i in live if i not in bad]
                if not live:
                    break
    return results


def arity(mnemonic):
    stem = mnemonic.split(".")[0]
    if stem in IMPLIED or stem == "reta":
        return 0
    if stem in DOUBLE + XDOUBLE + ROTM + ADDRA + ["mova", "pushm", "popm"]:
        return 2
    return 1


def deviation(source):
    for statement in source.split("{"):
        # GNU as ignores operands an instruction does not take.
        words = statement.split(None, 1)
        if words and len(words) > 1 and len(words[1].split(",")) > arity(words[0]):
            return "extra operands"
        for rx, why in DEVIATIONS:
            if rx.search(statement):
                return why
    return None


def compare(cases, isa):
    """The cases rsasm and GNU as disagree on, and a tally of the others."""
    res = run(cases, isa)
    findings = []
    tally = collections.Counter()
    for i, (m, s) in enumerate(cases):
        g, r = res["gas"].get(i, ("ERROR",)), res["rsasm"].get(i, ("ERROR",))
        gerr, rerr = g[0] == "ERROR", r[0] == "ERROR"
        if gerr and rerr:
            tally["refused by both"] += 1
        elif not gerr and not rerr and g == r:
            tally["identical"] += 1
        elif not gerr and rerr and deviation(s):
            tally["deviations"] += 1
        elif gerr and not rerr and any(rx.search(s) for rx, _ in ACCEPTED):
            tally["deviations"] += 1
        else:
            findings.append((isa, m, s, g, r))
    return findings, tally


def main():
    import argparse
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("check")
    c.add_argument("--isa", default="430x", choices=list(ISAS))
    c.add_argument("file")
    z = sub.add_parser("fuzz")
    z.add_argument("--seed", type=int, default=1)
    z.add_argument("--count", type=int, default=6000, help="total cases, split across ISAs")
    z.add_argument("--isa", default="all", choices=list(ISAS) + ["all"])
    z.add_argument("--only", help="regex on the mnemonic")
    z.add_argument("--batch", type=int, default=200)
    z.add_argument("--jobs", type=int, default=0)
    z.add_argument("--limit", type=int, default=40, help="distinct findings printed")
    args = ap.parse_args()

    if args.cmd == "check":
        cases = [(l.split()[0], l.strip()) for l in open(args.file) if l.strip() and not l.startswith("#")]
        findings, tally = compare(cases, args.isa)
    else:
        isas = list(ISAS) if args.isa == "all" else [args.isa]
        rng = random.Random(args.seed)
        batches = []
        per = args.count // len(isas)
        for isa in isas:
            cases = []
            while len(cases) < per:
                m, s = case(rng, isa)
                if args.only and not re.search(args.only, m):
                    continue
                cases.append((m, s))
            for b in range(0, len(cases), args.batch):
                batches.append((cases[b:b + args.batch], isa))
        jobs = args.jobs or os.cpu_count()
        findings, tally = [], collections.Counter()
        with ThreadPoolExecutor(jobs) as ex:
            for fs, t in ex.map(lambda b: compare(*b), batches):
                findings += fs
                tally += t

    groups = collections.defaultdict(list)
    for f in findings:
        groups[(f[0], f[1], f[3][0] == "ERROR", f[4][0] == "ERROR")].append(f)
    shown = 0
    for key, fs in sorted(groups.items(), key=lambda kv: -len(kv[1])):
        if shown >= args.__dict__.get("limit", 40):
            break
        shown += 1
        isa, m, s, g, r = min(fs, key=lambda f: len(f[2]))
        print(f"[{isa}] {m}: {len(fs)} case(s), e.g. `{s}`")
        print(f"    gas:   {g}")
        print(f"    rsasm: {r}")
    print("--- " + ", ".join(f"{n} {k}" for k, n in sorted(tally.items())) + f", {len(findings)} finding(s)")
    sys.exit(1 if findings else 0)


if __name__ == "__main__":
    main()
