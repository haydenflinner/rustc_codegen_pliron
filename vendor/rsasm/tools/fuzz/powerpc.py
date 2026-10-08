#!/usr/bin/env python3
"""Differential fuzzer for rsasm's PowerPC vector and POWER8-10 instructions.

Random instructions are generated from the opcode table in GNU binutils'
`opcodes/ppc-opc.c`, read at run time for its mnemonics and operand kinds
only (never its encodings), assembled by GNU as, llvm-mc and rsasm, and
compared byte for byte, relocations and accept/reject status included. Every
case goes into its own section, so one object holds a whole batch.

    tools/fuzz/powerpc.py fuzz --count 20000
    tools/fuzz/powerpc.py fuzz --target powerpc64le --only '^xx' --seed 7
    tools/fuzz/powerpc.py check --target powerpc64 <file>

`fuzz` classifies each case:

    agree       all three assemblers produced the same result.
    rsasm       GNU as and llvm-mc agree and rsasm does not (or rsasm
                panicked). These are the findings.
    deviation   the references agree and rsasm differs on purpose, as a rule
                in DEVIATIONS explains: rsasm refuses a doubleword instruction
                in 32-bit code, say, where both references take it.
    split       the references disagree. Listed with the one rsasm follows
                (gas, mc or neither). Where a rule in KNOWN_SPLITS explains
                the disagreement and rsasm follows the side the rule prefers,
                the case is counted but not listed; following neither side is
                an rsasm finding whatever the rule.

The report groups findings by mnemonic, mutation and accept/reject pattern,
and shows the shortest example of each. The exit status is 1 when there are
rsasm findings.

Some cases are deliberately invalid (`--mutations`, the fraction mutated):
out-of-range and misaligned immediates, register numbers past the bank, a
register name of the wrong class, an odd register pair, a missing or extra
operand, and an R operand of 1 with a base register.

Environment: RSASM (default target/debug/rsasm under the repository root),
RSASM_ORACLES (default target/oracles), which holds GNU as and the binutils
source; GAS and LLVM_MC override the assemblers.
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
GAS = os.environ.get("GAS", os.path.join(ORACLES, "bin", "powerpc64-linux-gnu-as"))
LLVM_MC = os.environ.get("LLVM_MC", "llvm-mc")
OPCODES = os.path.join(ORACLES, "src", "binutils-2.47", "opcodes", "ppc-opc.c")

# target: (GNU as flags, llvm-mc triple, rsasm arch, bits). GNU as runs with
# -mfuture: a few VSX instructions both references know are enabled only there.
TARGETS = {
    "powerpc64": (["-a64", "-mbig", "-mfuture"], "powerpc64", "powerpc64", 64),
    "powerpc64le": (["-a64", "-mlittle", "-mfuture"], "powerpc64le", "powerpc64le", 64),
    "powerpc": (["-a32", "-mbig", "-mfuture"], "powerpc", "powerpc", 32),
}

# ============================================================================
# The binutils opcode table
# ============================================================================

FLAG = {
    "PPC_OPERAND_GPR": 0x1, "PPC_OPERAND_GPR_0": 0x2, "PPC_OPERAND_FPR": 0x4,
    "PPC_OPERAND_VR": 0x8, "PPC_OPERAND_VSR": 0x10, "PPC_OPERAND_ACC": 0x20,
    "PPC_OPERAND_DMR": 0x40, "PPC_OPERAND_CR_BIT": 0x80,
    "PPC_OPERAND_CR_REG": 0x100, "PPC_OPERAND_SPR": 0x200,
    "PPC_OPERAND_RELATIVE": 0x800, "PPC_OPERAND_ABSOLUTE": 0x1000,
    "PPC_OPERAND_SIGNED": 0x2000, "PPC_OPERAND_SIGNOPT": 0x4000,
    "PPC_OPERAND_PARENS": 0x8000, "PPC_OPERAND_DS": 0x10000,
    "PPC_OPERAND_DQ": 0x20000, "PPC_OPERAND_NEGATIVE": 0x40000,
    "PPC_OPERAND_PLUS1": 0x80000, "PPC_OPERAND_OPTIONAL": 0x100000,
    "PPC_OPERAND_NEXT": 0x200000, "PPC_OPERAND_OPTIONAL32": 0x400000,
    "PPC_OPERAND_NONZERO": 0x4000000,
}

# The CPU groups fuzzed: AltiVec and VSX with their POWER8-10 additions, and
# the POWER8-10 scalar instructions.
CPUS = {"PPCVEC", "PPCVEC2", "PPCVEC3", "PPCVSX", "PPCVSX2", "PPCVSX3", "PPCVSXF",
        "POWER8", "POWER9", "POWER10"}

# What rsasm does not implement, and says so in README.md: the MMA
# accumulators, POWER11's AES and Galois-field instructions, quadword atomics
# and loads, decimal floating point, the BCTAR branches with their hints, and
# the privileged, hypervisor, cache and synchronisation instructions.
NOT_IMPLEMENTED = re.compile(r"""^(
    x?x?v?[if]\d+gerx?\d*.* | dm.* | pmx.* | pmdm.* | xxm[ft]acc | xxsetaccz
  | xxaes.* | xxgfmul.* | lqarx | stqcx\. | plq | pstq
  | dcffixqq | dctfixqq | dtstsfiq? | b.*tar.* | mf.* | mt.* | wait.* | pause_short
  | .*sync | stop | urfid | rfscv | rfebb | exser | slb.* | msg.* | tlbie.*
  | hash.* | clrbhrb | cpabort | pbt\. | rmieg | dcb.* | lwat | ldat | stwat | stdat
)$""", re.X)
# The scalar and vector moves between registers are implemented, whatever the
# `mf`/`mt` rule above says.
IMPLEMENTED_MOVES = re.compile(
    r"^(m[ft]vsr.*|m[ft]fpr.*|m[ft]vr(d|wa|wz)|m[ft]vscr|m[ft]vrsave|mffs(ce|cdrni?|crni?|l)"
    r"|mtvsrbmi)$")


def parse_table(path):
    """Operand name -> (mask, shift, insert, flags), and the opcode entries as
    (name, cpu, [operand names])."""
    text = open(path).read()
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    start = text.index("const struct powerpc_operand powerpc_operands[] =")
    end = text.index("const unsigned int num_powerpc_operands")
    operands, pending = {}, []
    for m in re.finditer(r"#define\s+(\w+)\s+([^\n]*)|\{([^{}]*)\}", text[start:end]):
        if m.group(1):
            name, val = m.group(1), m.group(2).strip()
            if re.fullmatch(r"\w+", val) and val in operands:
                operands[name] = operands[val]
            elif re.fullmatch(r"\w+(\s*\+\s*1)?", val):
                pending.append(name)
            continue
        f = [x.strip() for x in m.group(3).split(",")]
        try:
            mask = int(re.sub(r"UINT64_C\((\w+)\)", r"\1", f[0]), 0)
        except ValueError:
            mask = None
        try:
            shift = int(f[1], 0)
        except ValueError:
            shift = None  # PPC_OPSHIFT_INV and friends: an insert function places it
        flags = 0
        for flag in re.findall(r"PPC_OPERAND_\w+", ",".join(f[4:])):
            flags |= FLAG.get(flag, 0)
        for name in pending:
            operands[name] = (mask, shift, f[2], flags)
        pending = []
    entries = []
    for table in ("powerpc_opcodes", "prefix_opcodes"):
        s = text.index(f"const struct powerpc_opcode {table}[] = {{")
        e = text.index("};", s)
        for m in re.finditer(r'\{"([^"]+)",(.*?)\{([^{}]*)\}\}', text[s:e], re.S):
            mid = [x.strip() for x in re.split(r",(?![^(]*\))", m.group(2)) if x.strip()]
            ops = [o.strip() for o in m.group(3).split(",") if o.strip() not in ("", "0")]
            entries.append((m.group(1), mid[2] if len(mid) > 2 else "", ops))
    return operands, entries


class Kind:
    """How to write one operand: a register bank, an immediate range, or a
    displacement with its base register."""
    __slots__ = ("what", "lo", "hi", "step", "optional", "name")

    def __init__(self, what, lo=0, hi=0, step=1, optional=False, name=""):
        self.what, self.lo, self.hi, self.step = what, lo, hi, step
        self.optional, self.name = optional, name


def operand_kind(name, spec):
    mask, shift, insert, flags = spec
    optional = bool(flags & FLAG["PPC_OPERAND_OPTIONAL"])
    k = lambda *a, **kw: Kind(*a, optional=optional, name=name, **kw)  # noqa: E731
    if flags & (FLAG["PPC_OPERAND_ACC"] | FLAG["PPC_OPERAND_DMR"]):
        return None
    if flags & FLAG["PPC_OPERAND_VSR"]:
        return k("vsp" if mask == 0x3E else "vs")
    if flags & FLAG["PPC_OPERAND_VR"]:
        return k("v")
    if flags & FLAG["PPC_OPERAND_FPR"]:
        return None if mask == 0x1E else k("f")
    if flags & (FLAG["PPC_OPERAND_GPR"] | FLAG["PPC_OPERAND_GPR_0"]):
        return None if mask == 0x1E or insert not in ("NULL",) else k("r")
    if flags & FLAG["PPC_OPERAND_CR_REG"]:
        return k("cr")
    if flags & FLAG["PPC_OPERAND_CR_BIT"]:
        return k("imm", 0, 31)
    if name in ("D34", "SI34", "NSI34"):
        lo, hi = -(1 << 33), (1 << 33) - 1
        return k("d34" if flags & FLAG["PPC_OPERAND_PARENS"] else "imm", lo, hi)
    if flags & FLAG["PPC_OPERAND_PARENS"]:
        step = 16 if mask == 0xFFF0 else 4 if mask == 0xFFFC else 1
        return k("disp", -32768, 32767, step) if shift == 0 else None
    if name in ("PCREL", "PCREL1"):
        return k("r1")
    if name == "IMM32":
        return k("imm", -(1 << 32), (1 << 33) - 1)
    if name in ("DXD", "NDXD"):
        return k("imm", -32768, 65535)
    if name == "DCMXS":
        return k("imm", 0, 127)
    if name == "DMEX":
        return k("imm", 0, 1)
    if name == "SH6":
        return k("imm", 0, 63)
    if mask is None or insert != "NULL" or mask & (mask + 1):
        return None
    bits = mask.bit_length()
    if flags & FLAG["PPC_OPERAND_SIGNED"]:
        return k("imm", -(1 << (bits - 1)), (1 << (bits - 1)) - 1)
    if flags & FLAG["PPC_OPERAND_SIGNOPT"]:
        return k("imm", -(1 << (bits - 1)), (1 << bits) - 1)
    return k("imm", 0, mask)


class Form:
    __slots__ = ("mnem", "kinds")

    def __init__(self, mnem, kinds):
        self.mnem, self.kinds = mnem, kinds


def build_forms():
    operands, entries = parse_table(OPCODES)
    forms, seen = [], set()
    for name, cpu, ops in entries:
        if cpu not in CPUS or name.endswith(("+", "-")) or (name, tuple(ops)) in seen:
            continue
        if NOT_IMPLEMENTED.match(name) and not IMPLEMENTED_MOVES.match(name):
            continue
        seen.add((name, tuple(ops)))
        kinds, i, ok = [], 0, True
        while i < len(ops):
            spec = operands.get(ops[i])
            kind = spec and operand_kind(ops[i], spec)
            if kind is None:
                ok = False
                break
            if kind.what in ("disp", "d34"):
                i += 1                         # the base register that follows
            kinds.append(kind)
            i += 1
        if ok:
            forms.append(Form(name, kinds))
    return forms


# ============================================================================
# Generating instructions
# ============================================================================

MUTATIONS = ["range", "register", "class", "count", "align", "pair", "r-with-base"]


def number(rng, lo, hi, step=1):
    pick = rng.random()
    if pick < 0.3:
        v = rng.choice([lo, hi, 0, 1, -1, lo + step, hi - step])
    else:
        v = rng.randint(lo, hi)
    v = max(lo, min(hi, v))
    return v - v % step if step > 1 else v


def spell(rng, v, prefix):
    return f"%{prefix}{v}" if rng.random() < 0.3 else str(v)


class Case:
    __slots__ = ("form", "text", "mutation")

    def lines(self):
        return [self.text]

    def signature(self):
        return self.form.mnem + ("" if not self.mutation else f" [{self.mutation}]")


def generate(rng, form, mutate):
    mutation = rng.choice(MUTATIONS) if mutate else None
    ops = []
    base_zero = rng.random() < 0.5
    victim = rng.randrange(len(form.kinds)) if form.kinds else -1
    for i, k in enumerate(form.kinds):
        hit = mutation is not None and i == victim
        if k.what in ("v", "vs", "vsp", "r", "f", "cr"):
            top = {"v": 31, "vs": 63, "vsp": 63, "r": 31, "f": 31, "cr": 7}[k.what]
            v = rng.randint(0, top)
            if k.what == "vsp":
                v &= ~1
                if hit and mutation == "pair":
                    v |= 1
            if hit and mutation == "register":
                v = top + 1
            prefix = {"v": "v", "vs": "vs", "vsp": "vs", "r": "r", "f": "f", "cr": "cr"}[k.what]
            if hit and mutation == "class":
                prefix = rng.choice([p for p in ("v", "vs", "r", "f") if p != prefix])
                ops.append(f"%{prefix}{min(v, 31)}")
            else:
                ops.append(spell(rng, v, prefix))
        elif k.what == "imm":
            v = number(rng, k.lo, k.hi)
            if hit and mutation == "range":
                v = rng.choice([k.hi + 1, k.lo - 1])
            ops.append(str(v))
        elif k.what in ("disp", "d34"):
            lo, hi = k.lo, k.hi
            d = number(rng, lo, hi, k.step)
            if hit and mutation == "range":
                d = rng.choice([hi + 1, lo - 1])
            if hit and mutation == "align" and k.step > 1:
                d += rng.randint(1, k.step - 1)
            base = 0 if base_zero else rng.randint(0, 31)
            if hit and mutation == "r-with-base":
                base = rng.randint(1, 31)
            if rng.random() < 0.1 and k.what == "disp":
                ops.append(f"sym@l({base})")
            elif rng.random() < 0.1 and k.what == "d34" and base == 0:
                ops.append(f"sym@pcrel({base})")
            else:
                ops.append(f"{d}({spell(rng, base, 'r')})")
        elif k.what == "r1":
            if k.optional and rng.random() < 0.3 and not ops[-1].startswith("sym@pcrel"):
                continue
            pcrel = base_zero and (rng.random() < 0.5 or "sym@pcrel" in ops[-1])
            if mutation == "r-with-base":
                pcrel = True
            ops.append("1" if pcrel else "0")
    # Optional trailing operands may be left out.
    while ops and form.kinds and len(ops) == len(form.kinds) and form.kinds[-1].optional \
            and form.kinds[-1].what != "r1" and rng.random() < 0.3:
        ops.pop()
    if mutation == "count":
        if ops and rng.random() < 0.5:
            ops.pop()
        else:
            ops.append("1")
    c = Case()
    c.form = form
    c.text = f"{form.mnem} {', '.join(ops)}".rstrip() if ops else form.mnem
    c.mutation = mutation
    return c


# ============================================================================
# Running the assemblers
# ============================================================================


def elf_sections(data):
    """Section name -> [bytes, sorted [(offset, type, symbol, addend)]], for an
    ELF object of either class and byte order."""
    cls, e = data[4], "<" if data[5] == 1 else ">"
    if cls == 2:
        (shoff,) = struct.unpack_from(e + "Q", data, 0x28)
        shentsize, shnum, shstrndx = struct.unpack_from(e + "HHH", data, 0x3A)
        shfmt = e + "IIQQQQIIQQ"
    else:
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
            if cls == 2:
                off, info = struct.unpack_from(e + "QQ", data, o)
                addend = struct.unpack_from(e + "q", data, o + 16)[0] if rela else 0
                sym, typ = info >> 32, info & 0xFFFFFFFF
                so = symtab[2] + sym * 24
                sname, sinfo = struct.unpack_from(e + "IB", data, so)
                shndx = struct.unpack_from(e + "H", data, so + 6)[0]
            else:
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
    gasflags, triple, arch, _bits = TARGETS[target]
    if tool == "gas":
        cmd = [GAS] + gasflags + ["-o", obj, src]
    elif tool == "mc":
        cmd = [LLVM_MC, f"-triple={triple}", "-filetype=obj", "-o", obj, src]
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


def assemble_batch(tool, target, cases, workdir):
    """One result per case, ("ok", (bytes, relocs)) or ("err", message); a batch
    with errors is assembled again without the cases that were rejected."""
    rejected = {}
    for _round in range(24):
        lines, owner = [], {}
        for i, text in enumerate(cases):
            lines.append(f'.section .t{i},"ax",@progbits')
            if i in rejected:
                continue
            lines.append(text)
            owner[len(lines)] = i
        obj, errors = run_tool(tool, target, "\n".join(lines) + "\n", workdir)
        if obj is not None:
            secs = elf_sections(obj)
            return [("err", rejected[i]) if i in rejected else
                    ("ok", tuple(secs.get(f".t{i}", [b"", []]))) for i in range(len(cases))]
        new = False
        for ln, msgs in errors.items():
            if ln in owner and owner[ln] not in rejected:
                rejected[owner[ln]] = "; ".join(msgs)
                new = True
        if not new:
            break
    if len(cases) == 1:
        return [("err", "; ".join(sum(errors.values(), [])))]
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
    return ("err",) if res[0] == "err" else ("ok",) + tuple(res[1][0:1]) + (tuple(res[1][1]),)


def mc_register_number(g, m, ctx):
    """llvm-mc takes a register number past the end of its bank in some slots;
    GNU as refuses it, and so does rsasm."""
    return ctx["mutation"] == "register" and g[0] == "err" and m[0] == "ok"


def mc_wider_range(g, m, ctx):
    """An immediate past the end of its field: llvm-mc takes some (`dst`'s
    stream number, `addpcis` below -32768) and keeps the bits that fit; GNU as
    refuses them, and so does rsasm."""
    return g[0] == "err" and "out of range" in g[1] and m[0] == "ok"


def reloc_in_32_bit(g, m, ctx):
    """A symbol in a DS- or DQ-form displacement, or `@pcrel`, in 32-bit code:
    llvm-mc writes relocation numbers `R_PPC_*` does not define (the PowerPC64
    ones), or none that exists; GNU as writes the plain halfword relocations
    or refuses the modifier. rsasm follows GNU as."""
    return ctx["bits"] == 32 and "sym" in ctx["text"]


def gas_only_spelling(g, m, ctx):
    """Mnemonics, optional operands and immediate ranges GNU as accepts and
    llvm-mc does not (`xxmr`, `pnop`, `pla` with its R operand, a negative
    `xxspltib` byte). rsasm accepts them too."""
    return g[0] == "ok" and m[0] == "err"


def pcrel_addend_in_field(g, m, ctx):
    """A PC-relative prefixed reference: GNU as writes the addend into the
    field as well as the relocation, llvm-mc leaves the field zero."""
    return g[0] == "ok" and m[0] == "ok" and "@pcrel" in ctx["text"] and \
        g[1][1] == m[1][1]


def gas_pcrel_without_r(g, m, ctx):
    """`@pcrel` with R left at 0: GNU as writes the relocation, llvm-mc refuses
    it or, on a load, writes garbage with no relocation; rsasm refuses it."""
    return "@pcrel" in ctx["text"] and not re.search(r",\s*1$", ctx["text"])


# (name, predicate, the side rsasm should follow, or None where both are fine)
KNOWN_SPLITS = [
    ("mc-register-number", mc_register_number, "gas"),
    ("mc-wider-range", mc_wider_range, "gas"),
    ("reloc-in-32-bit", reloc_in_32_bit, "gas"),
    ("pcrel-addend-in-field", pcrel_addend_in_field, "mc"),
    ("pcrel-without-r", gas_pcrel_without_r, None),
    ("gas-only-spelling", gas_only_spelling, "gas"),
]

# Where both references agree and rsasm deliberately does not.
DEVIATIONS = [
    ("64-bit-only", lambda g, m, r, ctx: ctx["bits"] == 32 and r[0] == "err"
     and "64-bit instruction" in r[1]),
    # A register name of another bank (`%vs3` where a GPR goes): both
    # references read its number, GNU as with a warning; rsasm refuses it.
    ("register-class", lambda g, m, r, ctx: ctx["mutation"] == "class" and r[0] == "err"
     and re.search(r"is not a|expected a .* register", r[1]) is not None),
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
            if name == "pcrel-without-r" and r[0] == "err":
                return "convention", name
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
        ctx = {"text": text, "mutation": mutation, "bits": TARGETS[target][3]}
        cls, detail = classify(g, m, r, ctx)
        out.append((target, text, sig, mnem, mutation, cls, detail,
                    fmt(g, True), fmt(m, True), fmt(r, True)))
    return out


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
        cases = []
        for _ in range(per):
            c = generate(rng, rng.choice(forms), rng.random() < args.mutations)
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


LISTED = {"rsasm": "rsasm differs from both references", "split": "references disagree"}


def report(results, args, nforms):
    totals, by_target, details = (collections.Counter(), collections.defaultdict(
        collections.Counter), collections.Counter())
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
    p = []
    p.append(f"=== {len(results)} cases from {nforms} forms: " +
             ", ".join(f"{k} {v}" for k, v in sorted(totals.items())))
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
            p.append(f"{tag} {text}{extra}    # {sig}\n    gas:   {g[:200]}\n"
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
    c.add_argument("--target", default="powerpc64", choices=list(TARGETS))
    c.add_argument("--all", action="store_true", help="also print matching cases")
    c.add_argument("file", nargs="?")
    z = sub.add_parser("fuzz", help="generate random instructions and compare")
    z.add_argument("--seed", type=int, default=1)
    z.add_argument("--count", type=int, default=6000, help="total cases, split across targets")
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
            res = {t: assemble_batch(t, args.target, cases, d) for t in ("gas", "mc", "rsasm")}
        bad = 0
        for i, text in enumerate(cases):
            g, m, r = (fmt(res[t][i], True) for t in ("gas", "mc", "rsasm"))
            same = key(res["rsasm"][i]) in (key(res["gas"][i]), key(res["mc"][i]))
            bad += not same
            if not same or args.all:
                print(f"{'  ' if same else '!!'} {text}\n      gas:   {g}\n      mc:    {m}\n"
                      f"      rsasm: {r}")
        print(f"--- {len(cases) - bad} matched a reference, {bad} matched neither")
        return 1 if bad else 0
    return fuzz(args)


if __name__ == "__main__":
    sys.exit(main())
