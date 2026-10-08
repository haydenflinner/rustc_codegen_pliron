#!/usr/bin/env python3
"""Differential fuzzer for rsasm's x86 backend.

Random instructions are generated from a table of valid forms written from the
Intel SDM (not from rsasm's own tables), assembled by GNU as, llvm-mc and
rsasm, and compared byte for byte, relocations and accept/reject status
included. Every case goes into its own section, so one object holds a whole
batch and a byte difference is attributed to the case that caused it.

    tools/fuzz/x86.py fuzz --count 20000 --mode all --syntax all
    tools/fuzz/x86.py fuzz --mode 32 --syntax att --only '^f' --seed 7
    tools/fuzz/x86.py check --mode 16 [--intel] [--all] <file>

`check` compares the instructions in a file, one per line.

`fuzz` classifies each case:

    agree             all three assemblers produced the same result.
    rsasm             GNU as and llvm-mc agree and rsasm does not (or rsasm
                      panicked). These are the findings that matter: each is an
                      rsasm bug or a deliberate deviation to record in a corpus.
    convention        the references disagree in a way KNOWN_SPLITS explains
                      (the order of legacy prefixes, say) and rsasm matches one
                      of them. Matching neither counts as an rsasm finding.
    convention-other  as above, but rsasm follows the reference the rule marks
                      as the wrong one to copy. Listed.
    split             the references disagree otherwise. Listed, with which
                      one rsasm follows (gas, mc or neither), for a person to
                      judge.
    ignored           64-bit instructions both references read as APX, which
                      rsasm does not implement.

The report groups findings by table row, mutation and prefix, most frequent
first, and shows the shortest example of each. `--out` writes every listed
case, tab-separated. The exit status is 1 when there are rsasm findings.

A result is either the section's bytes plus its relocations, or ERROR; GNU as
warnings count as acceptance. A rejected case is an error in the reference, so
"both references reject and rsasm accepts" is an rsasm finding too.

Some generated cases are deliberately invalid (`--mutations`, the fraction of
cases that are mutated): mismatched register sizes, REX registers outside
64-bit mode, instructions from the wrong mode, impossible 16-bit addressing,
out-of-range immediates, wrong operand counts, bad or missing size suffixes.

Environment: RSASM (default target/debug/rsasm under the repository root),
GAS (default `as`), LLVM_MC (default `llvm-mc`). 16-bit mode is assembled as
32-bit ELF with `.code16` at the top of the source, for all three tools.
"""

import collections
import os
import random
import re
import struct
import subprocess
import sys
import tempfile

import simd

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
RSASM = os.environ.get("RSASM", os.path.join(ROOT, "target", "debug", "rsasm"))
GAS = os.environ.get("GAS", "as")
LLVM_MC = os.environ.get("LLVM_MC", "llvm-mc")

MODES = {
    # mode: (gas flags, llvm-mc triple, rsasm arch). Neither llvm-mc nor rsasm
    # writes an object for an i8086 target, so 16-bit code is i386 plus `.code16`.
    64: (["--64"], "x86_64", "x86-64"),
    32: (["--32"], "i386", "i386"),
    16: (["--32"], "i386", "i386"),
}

# ============================================================================
# Running the assemblers
# ============================================================================


def elf_sections(data):
    """Section name -> [bytes, [(offset, type, symbol, addend)]] of an ELF object."""
    cls = data[4]
    if cls == 2:
        (shoff,) = struct.unpack_from("<Q", data, 0x28)
        shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x3A)
    else:
        (shoff,) = struct.unpack_from("<I", data, 0x20)
        shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 0x2E)
    secs = []
    for i in range(shnum):
        o = shoff + i * shentsize
        fmt_ = "<IIQQQQIIQQ" if cls == 2 else "<IIIIIIIIII"
        name, typ, _flags, _addr, off, size, link, info, _al, entsize = struct.unpack_from(
            fmt_, data, o)
        secs.append((name, typ, off, size, link, info, entsize))
    strtab = secs[shstrndx]

    def cstr(tab_off, idx):
        end = data.index(b"\0", tab_off + idx)
        return data[tab_off + idx:end].decode()

    names = [cstr(strtab[2], s[0]) for s in secs]
    out = {}
    for i, s in enumerate(secs):
        if s[1] in (1, 8):  # PROGBITS, NOBITS
            out[names[i]] = [bytes(data[s[2]:s[2] + s[3]]) if s[1] == 1 else b"", []]
    for i, s in enumerate(secs):
        if s[1] not in (4, 9):  # RELA, REL
            continue
        target = names[s[5]]
        symtab = secs[s[4]]
        symstr = secs[symtab[4]]
        rela = s[1] == 4
        relocs = []
        for k in range(s[3] // s[6]):
            o = s[2] + k * s[6]
            if cls == 2:
                if rela:
                    off, info, addend = struct.unpack_from("<QQq", data, o)
                else:
                    (off, info), addend = struct.unpack_from("<QQ", data, o), 0
                sym, typ = info >> 32, info & 0xFFFFFFFF
                so = symtab[2] + sym * 24
                sname, sinfo = struct.unpack_from("<IB", data, so)
                shndx = struct.unpack_from("<H", data, so + 6)[0]
            else:
                if rela:
                    off, info, addend = struct.unpack_from("<IIi", data, o)
                else:
                    (off, info), addend = struct.unpack_from("<II", data, o), 0
                sym, typ = info >> 8, info & 0xFF
                so = symtab[2] + sym * 16
                sname = struct.unpack_from("<I", data, so)[0]
                sinfo = data[so + 12]
                shndx = struct.unpack_from("<H", data, so + 14)[0]
            if sinfo & 0xF == 3:  # STT_SECTION
                label = names[shndx] if shndx < len(names) else "?"
            else:
                label = cstr(symstr[2], sname)
            relocs.append((off, typ, label, addend))
        if target in out:
            out[target][1] = sorted(relocs)
    return out


LINE_RE = {
    "gas": re.compile(r"^[^:]*\.s:(\d+): (Error|Warning): (.*)$"),
    "mc": re.compile(r"^[^:]*\.s:(\d+):\d+: (error|warning): (.*)$"),
    "rsasm": re.compile(r"^\s*--> [^:]*\.s:(\d+):\d+"),
}


def run_tool(tool, mode, source, workdir):
    """Assembles `source`; returns (object bytes or None, {line: [errors]}, {line: [warnings]})."""
    src = os.path.join(workdir, f"{tool}.s")
    obj = os.path.join(workdir, f"{tool}.o")
    with open(src, "w") as f:
        f.write(source)
    gasflags, triple, arch = MODES[mode]
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
        return None, {0: ["TIMEOUT"]}, {}
    errors, warnings = {}, {}
    if tool == "rsasm":
        # Diagnostics are `error: msg` followed by ` --> file:line:col`.
        last = None
        for ln in p.stderr.splitlines():
            if ln.startswith("error: ") or ln.startswith("warning: "):
                last = ln
            m = LINE_RE["rsasm"].match(ln)
            if m and last:
                bucket = errors if last.startswith("error") else warnings
                bucket.setdefault(int(m.group(1)), []).append(last.split(": ", 1)[1])
                last = None
        if "panicked" in p.stderr:
            return None, {0: ["PANIC: " + p.stderr.strip().splitlines()[0][:200]]}, {}
    else:
        for ln in p.stderr.splitlines():
            m = LINE_RE[tool].match(ln)
            if m:
                bucket = errors if m.group(2).lower() == "error" else warnings
                bucket.setdefault(int(m.group(1)), []).append(m.group(3))
    if p.returncode != 0:
        if not errors:
            errors[0] = [p.stderr.strip()[:200] or f"exit {p.returncode}"]
        return None, errors, warnings
    with open(obj, "rb") as f:
        return f.read(), errors, warnings


def assemble_batch(tool, mode, syntax, cases, workdir):
    """Assembles each case (a list of source lines) in its own section.

    Returns one result per case: ("ok", (bytes, relocs, warnings)) or
    ("err", message). A batch with errors is assembled again without the
    cases that were rejected, until it goes through.
    """
    header = []
    if mode == 16:
        header.append(".code16")
    if syntax == "intel":
        header.append(".intel_syntax noprefix")
    rejected = {}
    for _round in range(24):
        lines = list(header)
        owner = {}
        for i, case in enumerate(cases):
            lines.append(f'.section .t{i},"ax",@progbits')
            if i in rejected:
                continue
            for text in case:
                lines.append(text)
                owner[len(lines)] = i
        obj, errors, warnings = run_tool(tool, mode, "\n".join(lines) + "\n", workdir)
        if obj is not None:
            secs = elf_sections(obj)
            warn_by_case = {}
            for ln, msgs in warnings.items():
                if ln in owner:
                    warn_by_case.setdefault(owner[ln], []).extend(msgs)
            results = []
            for i in range(len(cases)):
                if i in rejected:
                    results.append(("err", rejected[i]))
                else:
                    data, relocs = secs.get(f".t{i}", [b"", []])
                    results.append(("ok", (data, relocs, warn_by_case.get(i, []))))
            return results
        new = False
        for ln, msgs in errors.items():
            if ln in owner and owner[ln] not in rejected:
                rejected[owner[ln]] = "; ".join(msgs)
                new = True
        if not new:
            break
    # An error not attributable to a line (a crash, say): split the batch.
    if len(cases) == 1:
        return [("err", "; ".join(sum(errors.values(), [])))]
    mid = len(cases) // 2
    return assemble_batch(tool, mode, syntax, cases[:mid], workdir) + \
        assemble_batch(tool, mode, syntax, cases[mid:], workdir)


def fmt(res, with_error=False):
    status, payload = res
    if status == "err":
        if payload.startswith("PANIC"):
            return payload
        return f"ERROR ({payload})" if with_error else "ERROR"
    data, relocs, _w = payload
    s = data.hex(" ") if data else "(empty)"
    if relocs:
        s += " " + " ".join(f"[{o:x}:{t}:{n}{a:+d}]" for o, t, n, a in relocs)
    return s


def compare(mode, syntax, cases, tools=("gas", "mc", "rsasm")):
    with tempfile.TemporaryDirectory() as d:
        return {t: assemble_batch(t, mode, syntax, cases, d) for t in tools}


# ============================================================================
# Instruction forms
# ============================================================================
#
# Operands are listed in Intel order. Kinds:
#
#   r8 r16 r32 r64          general-purpose register
#   rm8 ... rm64            register or memory of that width
#   m8 m16 m32 m64 m80      memory of that width (m32/m64/m80 are also x87)
#   m128 m256               vector-width memory
#   m                       memory whose size the instruction does not take
#   mfar                    far pointer in memory (`fword ptr`, `ljmp *`)
#   i8 i8s i8u i16 i16u i32 i32s i64    immediates (s: sign-extended to the
#                           operation, u: an unsigned count or port)
#   one                     the constant 1 of `shl $1`
#   =name                   a fixed register (`=al`, `=dx`, `=cl`, `=es`, `=st`)
#   sreg cr dr              segment, control and debug registers
#   sti                     `st(i)`
#   xmm ymm mm              vector registers; xm32 xm64 xm128 ym256 mm64 are
#                           register-or-memory
#   rel rel8                branch target (rel8: short-only instructions)
#   far                     direct far pointer `$seg, $off` / `seg:off`
#   *rm16 *rm32 *rm64       indirect branch target (AT&T writes `*`)

ALL, LEG, X64 = frozenset((16, 32, 64)), frozenset((16, 32)), frozenset((64,))
SFX = {8: "b", 16: "w", 32: "l", 64: "q"}
KIND_SIZE_RE = re.compile(r"^\*?(?:rm|r|m)(8|16|32|64)$")


class Form:
    __slots__ = ("mnem", "ops", "modes", "att", "intel", "sfx", "flags", "group", "syntaxes")

    def __init__(self, mnem, ops, modes=None, att=None, intel=None, sfx="auto", flags="",
                 group=None, syntaxes=("att", "intel")):
        self.mnem = mnem
        self.ops = ops.split(",") if isinstance(ops, str) and ops else list(ops or [])
        if modes is None:
            is64 = any(k in ("r64", "rm64", "*rm64", "=rax", "i64") for k in self.ops)
            modes = X64 if is64 else ALL
        self.modes = modes
        self.att = [att] if isinstance(att, str) else att
        self.intel = [intel] if isinstance(intel, str) else intel
        if sfx == "auto":
            sfx = None
            for k in self.ops:
                m = KIND_SIZE_RE.match(k)
                if m:
                    sfx = SFX[int(m.group(1))]
                    break
                if k in ("=al", "=ax", "=eax", "=rax"):
                    sfx = {"=al": "b", "=ax": "w", "=eax": "l", "=rax": "q"}[k]
                    break
        self.sfx = sfx or None
        # `noswap`: AT&T keeps the Intel operand order (enter, bound).
        # `lock`/`rep`: may take that prefix. `far`: far branch.
        self.flags = flags
        self.group = group or mnem
        self.syntaxes = syntaxes

    def __repr__(self):
        return f"{self.mnem} {','.join(self.ops)}"


def build_forms():
    F = []

    def add(*a, **k):
        F.append(Form(*a, **k))

    # ---- arithmetic -------------------------------------------------------
    for m in ("add", "or", "adc", "sbb", "and", "sub", "xor", "cmp"):
        lock = "" if m == "cmp" else "lock"
        for n in (8, 16, 32, 64):
            add(m, f"rm{n},r{n}", flags=lock)
            add(m, f"r{n},rm{n}")
            imm = {8: "i8", 16: "i16", 32: "i32", 64: "i32s"}[n]
            add(m, f"rm{n},{imm}", flags=lock)
            if n > 8:
                add(m, f"rm{n},i8s", flags=lock)
        add(m, "=al,i8")
        add(m, "=ax,i16")
        add(m, "=eax,i32")
        add(m, "=rax,i32s")
    for m in ("rol", "ror", "rcl", "rcr", "shl", "sal", "shr", "sar"):
        for n in (8, 16, 32, 64):
            add(m, f"rm{n},one")
            add(m, f"rm{n},=cl")
            add(m, f"rm{n},i8u")
            add(m, f"rm{n}")
    for m in ("not", "neg", "mul", "div", "idiv", "imul", "inc", "dec"):
        for n in (8, 16, 32, 64):
            add(m, f"rm{n}", flags="" if m in ("mul", "div", "idiv", "imul") else "lock")
    for n in (16, 32, 64):
        add("imul", f"r{n},rm{n}")
        add("imul", f"r{n},rm{n},i8s")
        add("imul", f"r{n},rm{n},{'i16' if n == 16 else 'i32s' if n == 64 else 'i32'}")
        add("imul", f"r{n},i8s")
    for n in (8, 16, 32, 64):
        add("test", f"rm{n},r{n}")
        add("test", f"r{n},rm{n}")
        add("test", f"rm{n},{ {8: 'i8', 16: 'i16', 32: 'i32', 64: 'i32s'}[n] }")
    add("test", "=al,i8")
    add("test", "=eax,i32")

    # ---- moves ------------------------------------------------------------
    for n in (8, 16, 32, 64):
        add("mov", f"rm{n},r{n}")
        add("mov", f"r{n},rm{n}")
        add("mov", f"r{n},{ {8: 'i8', 16: 'i16', 32: 'i32', 64: 'i64'}[n] }")
        add("mov", f"rm{n},{ {8: 'i8', 16: 'i16', 32: 'i32', 64: 'i32s'}[n] }")
    add("movabs", "r64,i64", att=["movabsq", "movabs"])
    add("movabs", "=al,moffs", X64, sfx=None)
    add("movabs", "=rax,moffs", sfx=None)
    add("mov", "rm16,sreg", sfx=None)
    add("mov", "sreg,rm16", sfx=None)
    add("mov", "r32,sreg", sfx=None)
    add("mov", "r32,cr", LEG, sfx=None)
    add("mov", "cr,r32", LEG, sfx=None)
    add("mov", "r32,dr", LEG, sfx=None)
    add("mov", "dr,r32", LEG, sfx=None)
    add("mov", "r64,cr", sfx=None)
    add("mov", "cr,r64", sfx=None)
    add("mov", "r64,dr", sfx=None)
    for z, s in (("movzx", "movz"), ("movsx", "movs")):
        for dst, src in ((16, 8), (32, 8), (64, 8), (32, 16), (64, 16)):
            modes = X64 if dst == 64 else ALL
            add(z, f"r{dst},rm{src}", modes, att=[f"{s}{SFX[src]}{SFX[dst]}", z + "?"], sfx=None)
    # llvm-mc spells it `movslq` only, in AT&T syntax.
    add("movsxd", "r64,rm32", att="movslq", sfx=None)
    for n in (16, 32, 64):
        add("lea", f"r{n},m")
    for n in (8, 16, 32, 64):
        add("xchg", f"rm{n},r{n}", flags="lock")
        add("xchg", f"r{n},rm{n}")
    add("xchg", "=ax,r16")
    add("xchg", "r32,=eax")
    add("xchg", "=rax,r64")
    for n in (16, 32, 64):
        add("movbe", f"r{n},m{n}")
        add("movbe", f"m{n},r{n}")
        for m in ("popcnt", "lzcnt", "tzcnt", "bsf", "bsr"):
            add(m, f"r{n},rm{n}")
        for m in ("bt", "bts", "btr", "btc"):
            add(m, f"rm{n},r{n}", flags="" if m == "bt" else "lock")
            add(m, f"rm{n},i8u", flags="" if m == "bt" else "lock")
        for m in ("shld", "shrd"):
            add(m, f"rm{n},r{n},i8u")
            add(m, f"rm{n},r{n},=cl")
            add(m, f"rm{n},r{n}")
        for cc in CONDITIONS:
            add("cmov" + cc, f"r{n},rm{n}", group="cmovcc")
        # GNU as takes no suffix on these; llvm-mc does.
        add("rdrand", f"r{n}", sfx=None)
        add("rdseed", f"r{n}", sfx=None)
        add("lar", f"r{n},rm16", sfx=None)
        add("lsl", f"r{n},rm16", sfx=None)
    for n in (8, 16, 32, 64):
        add("xadd", f"rm{n},r{n}", flags="lock")
        add("cmpxchg", f"rm{n},r{n}", flags="lock")
    add("cmpxchg8b", "m64", sfx=None, flags="lock")
    add("bswap", "r32")
    add("bswap", "r64")
    for sz, at in ((8, "b"), (16, "w"), (32, "l"), (64, "q")):
        add("crc32", f"r32,rm{sz}" if sz < 64 else "r64,rm64", att=["crc32" + at], sfx=None)
    add("crc32", "r64,rm8", att=["crc32b"], sfx=None)
    for cc in CONDITIONS:
        add("set" + cc, "rm8", sfx=None, group="setcc")

    # ---- stack ------------------------------------------------------------
    add("push", "r16")
    add("push", "r32", LEG)
    add("push", "r64")
    add("push", "rm16")
    add("push", "rm32", LEG)
    add("push", "rm64")
    add("push", "i8s", sfx=None)
    add("push", "i16", sfx="w")
    add("push", "i32", LEG, sfx="l")
    add("push", "i32s", X64, sfx="q")
    add("pop", "r16")
    add("pop", "r32", LEG)
    add("pop", "r64")
    add("pop", "rm16")
    add("pop", "rm32", LEG)
    add("pop", "rm64")
    for s in ("es", "cs", "ss", "ds"):
        add("push", f"={s}", LEG, sfx=None)
        if s != "cs":
            add("pop", f"={s}", LEG, sfx=None)
    for s in ("fs", "gs"):
        add("push", f"={s}", sfx=None)
        add("pop", f"={s}", sfx=None)
    for m, modes in (("pusha", LEG), ("popa", LEG), ("pushaw", LEG), ("popaw", LEG),
                     ("pushf", ALL), ("popf", ALL), ("pushfw", ALL), ("popfw", ALL),
                     ("pushfq", X64), ("popfq", X64)):
        add(m, "", modes, sfx=None)
    add("pushal", "", LEG, intel="pushad", sfx=None)
    add("popal", "", LEG, intel="popad", sfx=None)
    add("pushfl", "", LEG, intel="pushfd", sfx=None)
    add("popfl", "", LEG, intel="popfd", sfx=None)
    add("enter", "i16u,i8u", sfx=None, flags="noswap")
    add("leave", "", sfx=None)
    # `leavew`/`leavel` are gas-only spellings; llvm-mc knows `leaveq`.
    add("leaveq", "", X64, sfx=None, syntaxes=("att",))

    # ---- one-byte odds and ends --------------------------------------------
    for m in ("lahf", "sahf", "cbw", "cwd", "cwde", "cdq", "hlt", "cli", "sti", "clc",
              "stc", "cmc", "cld", "std", "nop", "ud2", "pause", "int3", "cpuid",
              "rdtsc", "rdtscp", "rdmsr", "wrmsr", "rdpmc", "sysenter",
              "syscall", "xgetbv", "xsetbv", "clts", "iret", "xlatb",
              "insb", "insw", "outsb", "outsw", "movsb", "movsw", "cmpsb", "cmpsw",
              "scasb", "scasw", "lodsb", "lodsw", "stosb", "stosw", "emms", "ret",
              "sysret"):
        flags = "rep" if re.match(r"^(ins|outs|movs|cmps|scas|lods|stos)[bw]$", m) else ""
        add(m, "", ALL, sfx=None, flags=flags)
    # AT&T spellings, which llvm-mc does not take in Intel syntax.
    for m in ("cbtw", "cwtl", "cwtd", "cltd", "iretw", "xlat", "lret", "retw", "lretw"):
        add(m, "", ALL, sfx=None, syntaxes=("att",))
    for m in ("cdqe", "cqo", "swapgs", "iretq", "movsq", "cmpsq", "scasq", "lodsq", "stosq"):
        add(m, "", X64, sfx=None, flags="rep" if m.endswith("sq") else "")
    for m in ("cltq", "cqto", "retq", "lretq"):
        add(m, "", X64, sfx=None, syntaxes=("att",))
    for m in ("insl", "outsl", "movsl", "cmpsl", "scasl", "lodsl", "stosl"):
        add(m, "", ALL, sfx=None, flags="rep", intel=m[:-1] + "d")
    add("iretl", "", ALL, sfx=None, intel="iretd")
    # llvm-mc has no `int1`, no AT&T `retf`, and gas calls 64-bit `sysexit`
    # ambiguous in Intel syntax.
    add("retf", "", ALL, sfx=None, syntaxes=("intel",))
    add("sysexit", "", LEG, sfx=None)
    add("retl", "", LEG, sfx=None, syntaxes=("att",))
    for m in ("daa", "das", "aaa", "aas", "aam", "aad", "into", "salc"):
        add(m, "", LEG, sfx=None)
    add("aam", "i8u", LEG, sfx=None)
    add("aad", "i8u", LEG, sfx=None)
    add("int", "i8u", sfx=None)
    add("ret", "i16u", sfx=None)
    add("lret", "i16u", sfx=None, intel="retf")
    add("bound", "r16,m", LEG, sfx=None, flags="noswap")
    add("bound", "r32,m", LEG, sfx=None, flags="noswap")
    add("arpl", "rm16,r16", LEG, sfx=None)
    for w in (16, 32):
        add("nop", f"rm{w}")
    add("nop", "rm64")

    # ---- string instructions with operands -------------------------------
    add("movs", "strmov", sfx=None, flags="rep")
    add("stos", "strdi", sfx=None, flags="rep")
    add("scas", "strdi", sfx=None, flags="rep")
    add("lods", "strsi", sfx=None, flags="rep")
    add("cmps", "strcmp", sfx=None, flags="rep")
    add("xlat", "strxlat", sfx=None)

    # ---- I/O ----------------------------------------------------------------
    for r in ("=al", "=ax", "=eax"):
        add("in", f"{r},i8u")
        add("in", f"{r},=dx")
        add("out", f"i8u,{r}", sfx={"=al": "b", "=ax": "w", "=eax": "l"}[r])
        add("out", f"=dx,{r}", sfx={"=al": "b", "=ax": "w", "=eax": "l"}[r])

    # ---- branches -----------------------------------------------------------
    add("jmp", "rel", sfx=None)
    add("call", "rel", sfx=None)
    add("jmp", "*rm16", LEG)
    add("jmp", "*rm32", LEG)
    add("jmp", "*rm64")
    add("call", "*rm16", LEG)
    add("call", "*rm32", LEG)
    add("call", "*rm64")
    add("ljmp", "far", LEG, sfx=None, intel=["jmp", "ljmp"])
    add("lcall", "far", LEG, sfx=None, intel=["call", "lcall"])
    # gas takes `jmp fword ptr` in Intel syntax but not `ljmp fword ptr`.
    add("ljmp", "*mfar", sfx=None, att=["ljmp", "ljmpl"], intel="jmp")
    add("lcall", "*mfar", sfx=None, att=["lcall", "lcalll"], intel="call")
    for cc in CONDITIONS:
        add("j" + cc, "rel", sfx=None, group="jcc")
    add("jcxz", "rel8", LEG, sfx=None)
    add("jecxz", "rel8", sfx=None)
    add("jrcxz", "rel8", X64, sfx=None)
    for m in ("loop", "loope", "loopz", "loopne", "loopnz"):
        add(m, "rel8", sfx=None)

    # ---- segment and system -------------------------------------------------
    for m in ("les", "lds"):
        add(m, "r16,m", LEG)
        add(m, "r32,m", LEG)
    # The REX.W forms are Intel-only; gas refuses them unless `-mintel64`.
    for m in ("lss", "lfs", "lgs"):
        add(m, "r16,m")
        add(m, "r32,m")
    for m in ("sgdt", "sidt", "lgdt", "lidt", "invlpg", "fxsave", "fxrstor", "xsave",
              "xrstor", "xsaveopt"):
        add(m, "m", sfx=None)
    for m in ("sldt", "str", "smsw"):
        add(m, "rm16", sfx=None)
        add(m, "r32", sfx=None)
    for m in ("lldt", "ltr", "verr", "verw", "lmsw"):
        add(m, "rm16", sfx=None)

    # ---- x87 ----------------------------------------------------------------
    for m in ("f2xm1", "fabs", "fchs", "fclex", "fnclex", "fcompp", "fdecstp", "fincstp",
              "finit", "fninit", "fld1", "fldl2t", "fldl2e", "fldlg2", "fldln2", "fldpi",
              "fldz", "fnop", "fpatan", "fprem", "fprem1", "fptan", "frndint", "fscale",
              "fsin", "fsincos", "fsqrt", "ftst", "fucompp", "fwait", "fxam", "fxtract",
              "fyl2x", "fyl2xp1", "fcos"):
        add(m, "", sfx=None, group="x87-0op")

    def x87(m, ops, att, syntaxes=("att", "intel")):
        add(m, ops, sfx=None, att=att, group="x87", syntaxes=syntaxes)

    x87("fld", "m32", "flds")
    x87("fld", "m64", "fldl")
    x87("fld", "m80", "fldt")
    x87("fld", "sti", None)
    x87("fst", "m32", "fsts")
    x87("fst", "m64", "fstl")
    x87("fst", "sti", None)
    x87("fstp", "m32", "fstps")
    x87("fstp", "m64", "fstpl")
    x87("fstp", "m80", "fstpt")
    x87("fstp", "sti", None)
    x87("fild", "m16", "filds")
    x87("fild", "m32", "fildl")
    x87("fild", "m64", ["fildll", "fildq"])
    x87("fist", "m16", "fists")
    x87("fist", "m32", "fistl")
    x87("fistp", "m16", "fistps")
    x87("fistp", "m32", "fistpl")
    x87("fistp", "m64", ["fistpll", "fistpq"])
    x87("fisttp", "m16", "fisttps")
    x87("fisttp", "m32", "fisttpl")
    x87("fisttp", "m64", "fisttpll")
    x87("fbld", "m80", "fbld")
    x87("fbstp", "m80", "fbstp")
    for m in ("fadd", "fsub", "fsubr", "fmul", "fdiv", "fdivr"):
        x87(m, "m32", m + "s")
        x87(m, "m64", m + "l")
        x87(m, "=st,sti", None)
        x87(m, "sti,=st", None)
        x87(m, "sti", None)
        # With no operand GNU as refuses these in Intel syntax.
        x87(m, "", None, ("att",))
        x87(m + "p", "sti,=st", None)
        x87(m + "p", "sti", None)
        x87(m + "p", "", None, ("att",))
        x87("fi" + m[1:], "m16", "fi" + m[1:] + "s")
        x87("fi" + m[1:], "m32", "fi" + m[1:] + "l")
    for m in ("fcom", "fcomp"):
        x87(m, "m32", m + "s")
        x87(m, "m64", m + "l")
        x87(m, "sti", None)
        x87(m, "", None)
        x87("fi" + m[1:], "m16", "fi" + m[1:] + "s")
        x87("fi" + m[1:], "m32", "fi" + m[1:] + "l")
    for m in ("fucom", "fucomp", "fxch", "ffree", "ffreep"):
        x87(m, "sti", None)
    x87("fucom", "", None)
    x87("fxch", "", None)
    for m in ("fcomi", "fcomip", "fucomi", "fucomip"):
        x87(m, "=st,sti", None)
        x87(m, "sti", None)
    for cc in ("b", "e", "be", "u", "nb", "ne", "nbe", "nu"):
        x87("fcmov" + cc, "=st,sti", None)
    for m in ("fldcw", "fnstcw", "fstcw", "fnstsw", "fstsw"):
        x87(m, "m16", m)
    x87("fnstsw", "=ax", None)
    x87("fstsw", "=ax", None)
    for m in ("fldenv", "fnstenv", "fstenv", "frstor", "fnsave", "fsave"):
        x87(m, "m", m)

    # ---- a slice of SIMD ------------------------------------------------------
    def v(m, ops, att=None, modes=None):
        add(m, ops, modes, sfx=None, att=att, group="simd")

    for m in ("addps", "addpd", "subps", "mulpd", "divps", "andps", "xorpd", "orps",
              "sqrtps", "movaps", "movdqa", "movdqu", "paddb", "paddd", "pxor",
              "pcmpeqb", "unpcklps", "punpckhbw", "pmaddwd"):
        v(m, "xmm,xm128")
    v("movaps", "m128,xmm")
    v("movdqu", "m128,xmm")
    v("addss", "xmm,xm32")
    v("addsd", "xmm,xm64")
    v("comiss", "xmm,xm32")
    v("ucomisd", "xmm,xm64")
    for m in ("pshufd", "shufps", "cmpps"):
        v(m, "xmm,xm128,i8u")
    v("movd", "xmm,rm32")
    v("movd", "rm32,xmm")
    v("movq", "xmm,xm64")
    v("movq", "m64,xmm")
    v("movq", "xmm,r64")
    v("paddb", "mm,mm64")
    v("movq", "mm,mm64")
    v("cvtsi2sd", "xmm,rm32", att=["cvtsi2sdl", "cvtsi2sd?"])
    v("cvttsd2si", "r32,xm64")
    v("pinsrw", "xmm,r32,i8u")
    v("pextrw", "r32,xmm,i8u")
    v("pmovmskb", "r32,xmm")
    v("movmskps", "r32,xmm")
    v("ldmxcsr", "m32")
    v("stmxcsr", "m32")
    for m in ("vaddps", "vxorps", "vpxor", "vpaddd", "vandpd"):
        v(m, "xmm,xmm,xm128")
        v(m, "ymm,ymm,ym256")
    for m in ("vmovaps", "vmovdqu"):
        v(m, "xmm,xm128")
        v(m, "ymm,ym256")
        v(m, "m256,ymm")
    v("vpshufd", "ymm,ym256,i8u")
    v("vmovd", "xmm,rm32")
    v("vcvtsi2sd", "xmm,xmm,rm32", att=["vcvtsi2sdl", "vcvtsi2sd?"])
    v("vzeroupper", "")
    return F


CONDITIONS = ["o", "no", "b", "c", "nae", "ae", "nb", "nc", "e", "z", "ne", "nz", "be", "na",
              "a", "nbe", "s", "ns", "p", "pe", "np", "po", "l", "nge", "ge", "nl", "le",
              "ng", "g", "nle"]

# ============================================================================
# Operand generation
# ============================================================================

GPR = {
    8: ["al", "cl", "dl", "bl"],
    16: ["ax", "cx", "dx", "bx", "sp", "bp", "si", "di"],
    32: ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi"],
    64: ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi"],
}
HIGH8 = ["ah", "ch", "dh", "bh"]
REX8 = ["spl", "bpl", "sil", "dil"] + [f"r{i}b" for i in range(8, 16)]
REXN = {16: [f"r{i}w" for i in range(8, 16)], 32: [f"r{i}d" for i in range(8, 16)],
        64: [f"r{i}" for i in range(8, 16)]}
SREG = ["es", "cs", "ss", "ds", "fs", "gs"]

IMM = {
    "i8": [0, 1, 2, 0x7F, 0x80, 0xFF, -1, -0x80, 0x10, 0x55],
    "i8s": [0, 1, -1, 0x7F, -0x80, 0x40, -0x41, 5],
    "i8u": [0, 1, 3, 0x7F, 0x80, 0xFF, 0x21, 8],
    "i16": [0, 1, -1, 0x7F, 0x80, -0x80, -0x81, 0x100, 0x7FFF, 0x8000, 0xFFFF, -0x8000, 0x1234],
    "i16u": [0, 1, 0x10, 0x1234, 0xFFFF, 0x100],
    "i32": [0, 1, -1, 0x7F, 0x80, -0x81, 0x7FFF, 0x8000, 0xFFFF, 0x10000, 0x7FFFFFFF,
            0x80000000, 0xFFFFFFFF, -0x80000000, 0x12345678],
    "i32s": [0, 1, -1, 0x7F, 0x80, -0x81, 0x7FFF, 0x10000, 0x7FFFFFFF, -0x80000000, 0x12345678],
    "i64": [0, 1, -1, 0x7FFFFFFF, 0x80000000, 0xFFFFFFFF, 0x100000000, 0x1122334455667788,
            0x7FFFFFFFFFFFFFFF, -0x80000001],
}
OUT_OF_RANGE = {
    "i8": [0x100, -0x81, 0x1FF], "i8s": [0x100, -0x81], "i8u": [0x100, -0x81, 0x1000],
    "i16": [0x10000, -0x8001], "i16u": [0x10000, -1], "i32": [0x100000000, -0x80000001],
    "i32s": [0x80000000, 0xFFFFFFFF, 0x100000000], "i64": [0x10000000000000000],
}
SYMS = ["sym", "sym+4", "sym-1", "sym+0x1000"]


def num(rng, v):
    if v < 0:
        return "-" + num(rng, -v)
    return hex(v) if rng.random() < 0.6 else str(v)


class Opnd:
    """One generated operand, rendered per syntax at the end."""

    def __init__(self, kind, **kw):
        self.kind = kind  # reg, mem, imm, raw
        self.__dict__.update(kw)

    def shape(self):
        if self.kind == "reg":
            return self.cls
        if self.kind == "mem":
            s = f"m{self.size}/a{self.addr}" + ("rip" if self.rip else "") + ("s" if self.index else "")
            return s + (":seg" if self.seg else "")
        if self.kind == "imm":
            return "sym" if isinstance(self.value, str) else "imm"
        return self.shape_name


class Gen:
    def __init__(self, rng, mode, syntax):
        self.rng = rng
        self.mode = mode
        self.syntax = syntax
        # In 64-bit mode an instruction either may use REX registers or may use
        # ah-bh, never both.
        self.rex = mode == 64 and rng.random() < 0.75

    def choice(self, seq):
        return self.rng.choice(seq)

    def reg(self, size):
        pool = list(GPR[size])
        if size == 8:
            pool += REX8 if self.rex else HIGH8
        elif size == 64 or self.rex:
            pool += REXN.get(size, []) if self.rex else []
        name = self.choice(pool)
        return Opnd("reg", name=name, size=size, cls=f"r{size}")

    def disp(self, width):
        r = self.rng.random()
        if r < 0.15:
            return self.choice(SYMS)
        if width == 8:
            return num(self.rng, self.choice([0, 1, 4, 0x7F, -0x80, -8, 0x10]))
        if width == 16:
            return num(self.rng, self.choice([0x80, -0x81, 0x1234, 0x7FFF, -0x8000, 0xFFFF, 0x100]))
        return num(self.rng, self.choice([0x80, -0x81, 0x12345678, -0x80000000, 0x7FFFFFFF,
                                          0x10000, 0xFFFFFFFF]))

    def mem(self, size):
        rng = self.rng
        r = rng.random()
        if self.mode == 16:
            addr = 16 if r < 0.8 else 32
        elif self.mode == 32:
            addr = 32 if r < 0.85 else 16
        else:
            addr = 64 if r < 0.8 else 32
        base = index = None
        scale = 1
        rip = False
        disp = None
        if addr == 16:
            combos = [("bx", "si"), ("bx", "di"), ("bp", "si"), ("bp", "di"), ("si", None),
                      ("di", None), ("bp", None), ("bx", None), (None, None)]
            base, index = self.choice(combos)
            if base is None:
                disp = self.disp(16)
            else:
                k = rng.random()
                disp = None if k < 0.35 else self.disp(8 if k < 0.7 else 16)
        else:
            regs = list(GPR[addr]) + (REXN[addr] if self.rex else [])
            if addr == 64 and rng.random() < 0.15:
                rip = True
                disp = self.choice(SYMS + ["0", "4", "-8", "0x1000"])
            else:
                shape = rng.random()
                if shape < 0.1:
                    disp = self.disp(32)
                else:
                    if shape < 0.85:
                        base = self.choice(regs)
                    if shape >= 0.5:
                        index = self.choice([x for x in regs if x not in ("esp", "rsp")])
                        scale = self.choice([1, 1, 2, 4, 8])
                    k = rng.random()
                    if base is None:
                        disp = self.disp(32) if k < 0.7 else None
                    else:
                        disp = None if k < 0.35 else self.disp(8 if k < 0.7 else 32)
        if self.mode == 64 and disp in ("0xffffffff", "4294967295"):
            # Not a 64-bit address: GNU as refuses what the CPU would sign-extend.
            disp = "0x7fffffff"
        seg = self.choice(SREG) if rng.random() < 0.12 else None
        return Opnd("mem", size=size, addr=addr, base=base, index=index, scale=scale, disp=disp,
                    rip=rip, seg=seg, explicit_scale=rng.random() < 0.5, intel_swap=rng.random() < 0.2)

    def imm(self, kind):
        vals = IMM[kind]
        if kind in ("i16", "i32", "i32s", "i64") and self.rng.random() < 0.12:
            return Opnd("imm", value=self.choice(SYMS), width=kind)
        return Opnd("imm", value=self.choice(vals), width=kind)

    def operand(self, kind, form):
        star = kind.startswith("*")
        k = kind.lstrip("*")
        o = self._operand(k, form)
        o.star = star
        return o

    def _operand(self, k, form):
        rng = self.rng
        m = re.match(r"^(rm|r|m)(8|16|32|64|80|128|256)$", k)
        if m:
            size = int(m.group(2))
            if m.group(1) == "r" or (m.group(1) == "rm" and rng.random() < 0.45):
                return self.reg(size)
            return self.mem(size)
        if k == "m":
            return self.mem(None)
        if k == "mfar":
            return self.mem("far")
        if k in IMM:
            return self.imm(k)
        if k == "one":
            return Opnd("imm", value=1, width="one")
        if k.startswith("="):
            name = k[1:]
            if name == "st":
                return Opnd("raw", att="%st", intel="st", shape_name="st", cls="st")
            size = {"al": 8, "cl": 8, "ax": 16, "dx": 16, "eax": 32, "rax": 64}.get(name)
            return Opnd("reg", name=name, size=size, cls=name, fixed=True)
        if k == "sreg":
            return Opnd("reg", name=self.choice(SREG), size=None, cls="sreg")
        if k == "cr":
            return Opnd("reg", name=self.choice(["cr0", "cr2", "cr3", "cr4"] +
                                                (["cr8"] if self.mode == 64 else [])),
                        size=None, cls="cr")
        if k == "dr":
            return Opnd("reg", name=self.choice(["dr0", "dr1", "dr2", "dr3", "dr6", "dr7"]),
                        size=None, cls="dr")
        if k == "sti":
            i = rng.randrange(8)
            return Opnd("raw", att=f"%st({i})", intel=f"st({i})", shape_name="sti", cls="sti")
        if k in ("xmm", "ymm", "mm"):
            n = 16 if (self.mode == 64 and k != "mm" and self.rex) else 8
            name = f"{k}{rng.randrange(n)}"
            return Opnd("reg", name=name, size=None, cls=k)
        vm = re.match(r"^(xm|ym|mm)(32|64|128|256)$", k)
        if vm:
            if rng.random() < 0.5:
                return self._operand({"xm": "xmm", "ym": "ymm", "mm": "mm"}[vm.group(1)], form)
            return self.mem(int(vm.group(2)))
        if k in ("rel", "rel8"):
            return Opnd("rel", target=self.choice(["fwd", "fwd", "back", "far", "farback", "ext"]),
                        shape_name=k)
        if k == "far":
            seg = num(rng, self.choice([0, 0x10, 0xFFFF, 0x1234]))
            off = self.choice([0, 0x1000, 0xFFFF] + ([0x12345678] if self.mode != 16 else []))
            off = num(rng, off) if rng.random() < 0.85 else "sym"
            return Opnd("raw", att=f"${seg}, ${off}", intel=f"{seg}:{off}", shape_name="far")
        if k == "moffs":
            v = self.choice([0x1122334455667788, 0x1000, "sym"])
            text = v if isinstance(v, str) else hex(v)
            return Opnd("raw", att=text, intel=f"[{text}]", shape_name="moffs")
        if k.startswith("str"):
            return self.string_operand(k)
        raise ValueError(f"unknown operand kind {k}")

    def string_operand(self, k):
        rng = self.rng
        a = {16: ("si", "di", "bx"), 32: ("esi", "edi", "ebx"), 64: ("rsi", "rdi", "rbx")}[self.mode]
        sz = self.choice(["byte", "word", "dword"] + (["qword"] if self.mode == 64 else []))
        att_sfx = {"byte": "b", "word": "w", "dword": "l", "qword": "q"}[sz]
        si, di, bx = a
        if k == "strmov":
            att = f"%ds:(%{si}), %es:(%{di})" if rng.random() < 0.5 else f"(%{si}), (%{di})"
            intel = f"{sz} ptr es:[{di}], {sz} ptr ds:[{si}]" if rng.random() < 0.5 else \
                f"{sz} ptr [{di}], [{si}]"
        elif k == "strcmp":
            att = f"%es:(%{di}), %ds:(%{si})"
            intel = f"{sz} ptr [{si}], {sz} ptr es:[{di}]"
        elif k == "strdi":
            att = f"%es:(%{di})"
            intel = f"{sz} ptr es:[{di}]"
        elif k == "strsi":
            att = f"(%{si})"
            intel = f"{sz} ptr [{si}]"
        else:  # xlat
            att = f"(%{bx})"
            intel = f"byte ptr [{bx}]"
            att_sfx = "b"
        return Opnd("raw", att=att, intel=intel, shape_name=k, att_sfx=att_sfx)


# ============================================================================
# Rendering
# ============================================================================

INTEL_PTR = {8: "byte", 16: "word", 32: "dword", 64: "qword", 80: "tbyte", 128: "xmmword",
             256: "ymmword", "far": "fword"}


def render_mem_att(o):
    s = f"%{o.seg}:" if o.seg else ""
    if o.rip:
        return s + f"{o.disp}(%rip)"
    d = o.disp or ""
    if o.base is None and o.index is None:
        return s + (d or "0")
    inner = f"%{o.base}" if o.base else ""
    if o.index:
        inner += f",%{o.index}"
        if o.scale != 1 or o.explicit_scale:
            inner += f",{o.scale}"
    return s + f"{d}({inner})"


def render_mem_intel(o, with_ptr):
    pre = f"{INTEL_PTR[o.size]} ptr " if with_ptr and o.size is not None else ""
    seg = f"{o.seg}:" if o.seg else ""
    terms = []
    if o.rip:
        terms = ["rip"]
    else:
        if o.base:
            terms.append(o.base)
        if o.index:
            if o.addr == 16 or (o.scale == 1 and not o.explicit_scale):
                terms.append(o.index)
            else:
                terms.append(f"{o.index}*{o.scale}")
        if o.intel_swap and len(terms) == 2:
            terms.reverse()
    body = "+".join(terms)
    if o.disp is not None:
        d = o.disp
        if not body:
            body = d
        elif d.startswith("-"):
            body += d
        else:
            body += "+" + d
    return f"{pre}{seg}[{body}]"


class Case:
    """A generated instruction: the form, its operands and how to spell it."""

    def __init__(self, form, ops, mode, syntax, rng):
        self.form = form
        self.ops = ops
        self.mode = mode
        self.syntax = syntax
        self.rng = rng
        self.prefix = None
        self.mutation = None
        self.suffix_choice = rng.random()  # decides optional suffixes and ptrs
        self.force_bad_suffix = None
        self.drop_size = False

    def size_fixed_by_register(self, size):
        return any(o.kind == "reg" and o.cls.startswith("r") and o.size == size
                   and not getattr(o, "fixed", False) for o in self.ops) or \
            any(o.kind == "reg" and getattr(o, "fixed", False) and o.size == size for o in self.ops)

    def has_mem(self):
        return any(o.kind == "mem" for o in self.ops)

    def mnemonic(self):
        f = self.form
        if self.syntax == "intel":
            m = self.rng.choice(f.intel) if f.intel else f.mnem
            if self.force_bad_suffix:
                m += self.force_bad_suffix
            return m
        if f.att:
            cands = [a for a in f.att if not a.endswith("?")]
            if not self.has_mem():
                cands += [a[:-1] for a in f.att if a.endswith("?")]
            m = cands[int(self.suffix_choice * len(cands))]
        else:
            m = f.mnem
            if f.sfx:
                size = {"b": 8, "w": 16, "l": 32, "q": 64}[f.sfx]
                optional = not self.has_mem() or self.size_fixed_by_register(size)
                if self.drop_size:
                    pass
                elif not optional or self.suffix_choice < 0.5:
                    m += f.sfx
            for o in self.ops:
                if o.kind == "raw" and getattr(o, "att_sfx", None) and f.mnem in (
                        "movs", "stos", "scas", "lods", "cmps") and self.suffix_choice < 0.7:
                    m += o.att_sfx
        if self.force_bad_suffix:
            m += self.force_bad_suffix
        return m

    def operand_text(self, o):
        if o.kind == "reg":
            return ("%" + o.name if self.syntax == "att" else o.name)
        if o.kind == "imm":
            if isinstance(o.value, str):
                # A bare symbol is a memory reference in Intel syntax.
                return "$" + o.value if self.syntax == "att" else "offset " + o.value
            v = num(self.rng, o.value)
            return ("$" + v) if self.syntax == "att" else v
        if o.kind == "mem":
            if self.syntax == "att":
                return render_mem_att(o)
            ptr = o.size is not None and not self.drop_size
            if ptr and o.size in (8, 16, 32, 64, 128, 256) and self.suffix_choice < 0.5:
                # A register of the same width makes the keyword optional, but
                # not the count of a shift or the source of `crc32`.
                if self.form.mnem != "crc32" and any(
                        x.kind == "reg" and x.size == o.size and not getattr(x, "fixed", False)
                        and x.cls != "=cl" for x in self.ops):
                    ptr = False
                if any(x.kind == "reg" and x.cls == {128: "xmm", 256: "ymm"}.get(o.size)
                       for x in self.ops):
                    ptr = False
            return render_mem_intel(o, ptr)
        if o.kind == "rel":
            return {"fwd": "1f", "far": "1f", "back": "1b", "farback": "1b", "ext": "sym"}[o.target]
        return o.att if self.syntax == "att" else o.intel

    def lines(self):
        ops = list(self.ops)
        texts = []
        for o in ops:
            t = self.operand_text(o)
            if getattr(o, "star", False) and self.syntax == "att":
                t = "*" + t
            texts.append(t)
        if self.syntax == "att" and "noswap" not in self.form.flags:
            texts.reverse()
        insn = self.mnemonic()
        if texts:
            insn += " " + ", ".join(texts)
        if self.prefix:
            insn = f"{self.prefix} {insn}"
        rel = next((o for o in ops if o.kind == "rel"), None)
        if rel is None:
            return [insn]
        return {
            "fwd": [insn, "1:"],
            "far": [insn, ".space 200", "1:"],
            "back": ["1:", "nop", insn],
            "farback": ["1:", ".space 200", insn],
            "ext": [insn],
        }[rel.target]

    def form_key(self):
        """The table row plus what was done to it: the grouping for findings."""
        return f"{self.form!r}" + (f" <{self.mutation}>" if self.mutation else "") \
            + (f" [{self.prefix}]" if self.prefix else "")

    def signature(self):
        shapes = [o.shape() if o.kind != "rel" else f"rel:{o.target}" for o in self.ops]
        return f"{self.form.mnem} {','.join(shapes)}" + (f" <{self.mutation}>" if self.mutation else "") \
            + (f" [{self.prefix}]" if self.prefix else "")


# ============================================================================
# Mutations
# ============================================================================

def mutate(case, gen):
    """Applies one mutation meant to make the case invalid. Returns False if none applied."""
    rng = case.rng
    choices = ["regsize", "immrange", "arity", "suffix", "nosize", "badmem", "espindex"]
    if case.mode != 64:
        choices.append("rexreg")
    rng.shuffle(choices)
    for m in choices:
        if apply_mutation(m, case, gen):
            case.mutation = m
            return True
    return False


def apply_mutation(m, case, gen):
    rng = case.rng
    regs = [o for o in case.ops if o.kind == "reg" and o.cls in ("r8", "r16", "r32", "r64")]
    mems = [o for o in case.ops if o.kind == "mem"]
    imms = [o for o in case.ops if o.kind == "imm" and o.width in OUT_OF_RANGE
            and not isinstance(o.value, str)]
    if m == "regsize" and regs:
        o = rng.choice(regs)
        sizes = [s for s in (8, 16, 32, 64) if s != o.size and (s != 64 or case.mode == 64)]
        o.name = gen.reg(rng.choice(sizes)).name
        return True
    if m == "rexreg" and (regs or mems):
        if regs and (not mems or rng.random() < 0.5):
            o = rng.choice(regs)
            o.name = rng.choice(REX8 if o.size == 8 else REXN[o.size] + ["rax", "rsp"])
        else:
            o = rng.choice(mems)
            if o.addr == 16 or o.rip:
                return False
            o.base = rng.choice(["r8d", "r12d", "rax", "rbp"] if o.addr == 32 else REXN[64])
        return True
    if m == "immrange" and imms:
        o = rng.choice(imms)
        o.value = rng.choice(OUT_OF_RANGE[o.width])
        return True
    if m == "arity" and case.ops:
        if rng.random() < 0.5 or len(case.ops) == 1:
            case.ops.append(case.ops[0])
        else:
            case.ops.pop()
        return True
    if m == "suffix":
        case.force_bad_suffix = rng.choice(["b", "w", "l", "q", "s", "t", "x"])
        return True
    if m == "nosize" and mems and not regs and case.form.sfx:
        case.drop_size = True
        return True
    if m == "badmem" and mems and case.mode != 64:
        o = rng.choice(mems)
        o.addr = 16
        o.rip = False
        o.seg = None
        o.base, o.index, o.scale = rng.choice([("bx", "ax", 1), ("si", "di", 1), ("bx", "si", 2),
                                               ("sp", None, 1), ("ax", None, 1), ("bp", "bx", 1),
                                               (None, "si", 1)])
        o.explicit_scale = o.scale != 1
        return True
    if m == "espindex" and mems and case.mode != 16:
        o = rng.choice(mems)
        if o.addr == 16 or o.rip:
            return False
        o.index = "esp" if o.addr == 32 else "rsp"
        o.scale = 2
        o.explicit_scale = True
        return True
    return False


# ============================================================================
# Generation
# ============================================================================

SEG_PREFIXES = ["es", "cs", "ss", "ds", "fs", "gs"]


def generate_simd(rng, mode, syntax, forms, mutation_ratio):
    """A case from the SIMD forms read from GNU's table (see simd.py)."""
    for _ in range(100):
        form = rng.choice(forms)
        try:
            return form.generate(rng, mode, syntax, mutation_ratio)
        except simd.Unusable:
            continue
    raise RuntimeError("no usable SIMD form")


def generate(rng, mode, syntax, forms, mutation_ratio):
    gen = Gen(rng, mode, syntax)
    mutate_this = rng.random() < mutation_ratio
    forms = [f for f in forms if syntax in f.syntaxes]
    valid = [f for f in forms if mode in f.modes]
    if mutate_this and rng.random() < 0.2:
        # An instruction from another mode.
        other = [f for f in forms if mode not in f.modes]
        form = rng.choice(other or valid)
        wrong_mode = form in other
    else:
        form = rng.choice(valid)
        wrong_mode = False
    if any(k.lstrip("*") in ("r64", "rm64", "=rax") for k in form.ops):
        gen.rex = mode == 64  # REX.W rules out ah-bh anyway
    ops = [gen.operand(k, form) for k in form.ops]
    case = Case(form, ops, mode, syntax, rng)
    if wrong_mode:
        case.mutation = "wrongmode"
    elif mutate_this:
        mutate(case, gen)
    r = rng.random()
    if "lock" in form.flags and case.has_mem() and r < 0.2:
        case.prefix = "lock"
    elif "rep" in form.flags and r < 0.5:
        case.prefix = rng.choice(["rep", "repe", "repne", "repz", "repnz"])
    elif r < 0.03 and not any(o.kind == "mem" and o.seg for o in ops):
        # GNU as refuses the null segments as prefixes in 64-bit mode, and a
        # second segment override anywhere.
        case.prefix = rng.choice(SEG_PREFIXES if mode != 64 else ["fs", "gs"])
    elif r < 0.05 and (not form.ops or "rep" in form.flags) and form.group != "simd":
        # A size prefix on an instruction that already has one is refused by
        # gas and doubled by llvm-mc, so these go only where they are news:
        # operand-less instructions and string operations.
        case.prefix = rng.choice(["data16", "data32", "addr16", "addr32"])
    return case


# ============================================================================
# Classification
# ============================================================================

LEGACY_PREFIXES = {0x66, 0x67, 0xF0, 0xF2, 0xF3, 0x26, 0x2E, 0x36, 0x3E, 0x64, 0x65}


def split_prefixes(data):
    i = 0
    while i < len(data) and data[i] in LEGACY_PREFIXES:
        i += 1
    return data[:i], data[i:]


def ok_parts(res):
    return res[1] if res[0] == "ok" else None


def opcode_start(data):
    """Index of the opcode: past legacy prefixes and a REX byte, if any."""
    i = len(split_prefixes(data)[0])
    return i + 1 if data[i:i + 1] and 0x40 <= data[i] <= 0x4F else i


def same_relocs(a, b):
    """Relocation lists equal but for R_X86_64_32 against R_X86_64_32S; see addr32_reloc."""
    norm = lambda rs: [(o, 11 if t == 10 else t, n, ad) for o, t, n, ad in rs]  # noqa: E731
    return norm(a) == norm(b)


def without_byte(res, i):
    """A result's bytes and relocations with byte `i` removed."""
    data, relocs, _w = res
    return data[:i] + data[i + 1:], [(o - 1 if o > i else o, t, n, a) for o, t, n, a in relocs]


def prefix_order(g, m, ctx):
    """Legacy prefixes in another order: `rep stosw` is `66 f3 ab` in gas, `f3 66 ab` in mc,
    and `fs fsave` is `9b 64 dd ..` in gas, `64 9b dd ..` in mc."""
    gp, mp = ok_parts(g), ok_parts(m)
    if not gp or not mp:
        return False
    gdata, mdata = gp[0], mp[0]
    if gdata[:1] == b"\x9b" and b"\x9b" in mdata[:4]:
        # `fsave`, `finit` and friends: gas puts `fwait` before a segment
        # prefix, llvm-mc after it.
        gdata, mdata = gdata[1:], mdata.replace(b"\x9b", b"", 1)
    gpre, grest = split_prefixes(gdata)
    mpre, mrest = split_prefixes(mdata)
    return grest == mrest and sorted(gpre) == sorted(mpre) \
        and [r[1:] for r in gp[1]] == [r[1:] for r in mp[1]]


SEGMENT_PREFIX_BYTE = {"es": 0x26, "cs": 0x2E, "ss": 0x36, "ds": 0x3E, "fs": 0x64, "gs": 0x65}
SEGMENT_PREFIXES = set(SEGMENT_PREFIX_BYTE.values())


def redundant_segment(g, m, ctx):
    """gas drops a segment override that names the default segment (`%ds:(%ebx)`,
    `%ss:(%ebp)`) or that a branch cannot use; llvm-mc keeps it."""
    gp, mp = ok_parts(g), ok_parts(m)
    if not gp or not mp or len(mp[0]) != len(gp[0]) + 1:
        return False
    branch = re.search(r"\b(es|cs|ss|ds|fs|gs) (j[a-z]+|loop[a-z]*|call)\b", " ".join(ctx["lines"]))
    if branch and mp[0].count(bytes([SEGMENT_PREFIX_BYTE[branch.group(1)]])) == \
            gp[0].count(bytes([SEGMENT_PREFIX_BYTE[branch.group(1)]])) + 1:
        # On a branch the displacement moves with the dropped byte, so only the
        # prefix itself can be compared.
        return True
    # Within the first few bytes, since `fwait` can come before the prefix.
    for i, b in enumerate(mp[0][:5]):
        if b in SEGMENT_PREFIXES:
            data, relocs = without_byte(mp, i)
            if data == gp[0] and same_relocs(relocs, gp[1]):
                return True
    return False


def mc_intel_opsize(g, m, ctx):
    """llvm-mc in Intel syntax gives some instructions without an explicit size the
    other mode's operand size: `push ds`, `ret`, `iret` and `retf` gain a 0x66, and
    a 16-bit `call` becomes a 32-bit one."""
    gp, mp = ok_parts(g), ok_parts(m)
    if ctx["syntax"] != "intel" or not gp or not mp:
        return False
    i = 0
    while i < min(len(gp[0]), len(mp[0])) and gp[0][i] == mp[0][i]:
        i += 1
    return mp[0][i:i + 1] == b"\x66" and mp[0][i + 1:i + 2] == gp[0][i:i + 1] \
        and not re.search(r"\b(byte|word|dword|qword) ptr", " ".join(ctx["lines"]))


def gas_data_prefix(g, m, ctx):
    """An explicit `data16`/`data32`, or a segment register moved into a 32-bit register:
    gas emits the 0x66, llvm-mc leaves it out."""
    gp, mp = ok_parts(g), ok_parts(m)
    if not gp or not mp or len(gp[0]) != len(mp[0]) + 1:
        return False
    return any(b == 0x66 and without_byte(gp, i) == (mp[0], list(mp[1]))
               for i, b in enumerate(split_prefixes(gp[0])[0]))


def mc_addr32_in_16(g, m, ctx):
    """llvm-mc refuses `addr32` in 16-bit code, as if it needed 64-bit mode."""
    return ctx["mode"] == 16 and g[0] == "ok" and m[0] == "err" and "64-bit mode" in m[1] \
        and "addr32" in " ".join(ctx["lines"])


def unsized_memory(g, m, ctx):
    """An AT&T memory operand with no suffix and no register to size it: gas warns and
    uses the default size, llvm-mc refuses it."""
    return g[0] == "ok" and any("no instruction mnemonic suffix" in w for w in g[1][2]) \
        and m[0] == "err" and "ambiguous" in m[1]


REGISTER_NAME = re.compile(r"[re]?[a-d][xhl]|[re]?[sd]il?|[re]?[sb]pl?|r\d+[bwd]?|[re]?ip")


def register_as_symbol(g, m, ctx):
    """In Intel syntax gas reads a register the mode lacks (`r8d` in 32-bit code) as a
    symbol name; llvm-mc refuses it."""
    gp = ok_parts(g)
    return gp is not None and m[0] == "err" and "only available in 64-bit mode" in m[1] \
        and any(REGISTER_NAME.fullmatch(n) for _o, _t, n, _a in gp[1])


def intel_suffix(g, m, ctx):
    """gas takes an AT&T size suffix on a mnemonic in Intel syntax (`addw bp, di`)."""
    return ctx["syntax"] == "intel" and g[0] == "ok" and m[0] == "err" \
        and "invalid instruction mnemonic" in m[1]


def intel_far_direct(g, m, ctx):
    """A direct far branch in Intel syntax, `jmp 0x10:0x1000`: gas takes it, llvm-mc
    does not parse it."""
    return ctx["syntax"] == "intel" and g[0] == "ok" and m[0] == "err" \
        and re.search(r"\b(l?jmp|l?call) \w+:\w+", " ".join(ctx["lines"])) is not None


def xchg_order(g, m, ctx):
    """`xchg %cl, %bl` can put either register in ModRM.reg; gas and llvm-mc differ."""
    gp, mp = ok_parts(g), ok_parts(m)
    return bool(gp and mp and ctx["mnem"] == "xchg" and len(gp[0]) == len(mp[0]))


def addr32_reloc(g, m, ctx):
    """A 32-bit address in 64-bit code: gas relocates the displacement with R_X86_64_32,
    llvm-mc with R_X86_64_32S."""
    gp, mp = ok_parts(g), ok_parts(m)
    if not gp or not mp or gp[0] != mp[0] or len(gp[1]) != len(mp[1]):
        return False
    diff = [(a, b) for a, b in zip(gp[1], mp[1]) if a != b]
    return bool(diff) and all(a[1] == 10 and b[1] == 11 and a[0] == b[0] and a[2:] == b[2:]
                              for a, b in diff)


def disp16_wrap(g, m, ctx):
    """A 16-bit address with displacement 0xffff: gas wraps it to -1 and uses disp8,
    llvm-mc keeps disp16."""
    gp, mp = ok_parts(g), ok_parts(m)
    src = " ".join(ctx["lines"])
    return bool(gp and mp and len(mp[0]) == len(gp[0]) + 1
                and re.search(r"\b(0xffff|65535)\b", src)
                and re.search(r"%?\b(bx|bp|si|di)\b", src))


ACC_IMM_OPCODES = {0x05, 0x0D, 0x15, 0x1D, 0x25, 0x2D, 0x35, 0x3D, 0xA9}


def symbolic_accumulator(g, m, ctx):
    """`cmp $sym, %eax`: gas uses the one-byte-shorter accumulator opcode, llvm-mc the
    ModRM form."""
    gp, mp = ok_parts(g), ok_parts(m)
    if not gp or not mp or len(mp[0]) != len(gp[0]) + 1 or not gp[1]:
        return False
    gi, mi = opcode_start(gp[0]), opcode_start(mp[0])
    return gi < len(gp[0]) and mi < len(mp[0]) and gp[0][gi] in ACC_IMM_OPCODES \
        and mp[0][mi] in (0x81, 0xF7) and gp[0][:gi] == mp[0][:mi]


def vex_commute(g, m, ctx):
    """llvm-mc swaps the sources of a commutative AVX operation to fit the two-byte VEX
    prefix; gas keeps the operands where they were written."""
    gp, mp = ok_parts(g), ok_parts(m)
    return bool(gp and mp and gp[0][:1] == b"\xc4" and mp[0][:1] == b"\xc5"
                and len(mp[0]) + 1 == len(gp[0]))


def gas_error(res, text):
    return res[0] == "err" and text in res[1]


def mc_lone_zeroing(g, m, ctx):
    """llvm-mc drops a `{z}` written without a writemask; GNU as refuses it."""
    line = " ".join(ctx["lines"])
    refused = gas_error(g, "zeroing-masking only allowed with write mask") or \
        gas_error(g, "junk `{z}'")
    return bool(refused and ok_parts(m) and "{z}" in line
                and not re.search(r"\{%?k[0-7]\}", line))


def unsized_gpr_scalar(g, m, ctx):
    """An Intel `vcvtsi2ss xmm, xmm, [mem]` outside 64-bit mode: GNU as reads the
    memory as 32 bits, the only size there, where llvm-mc calls it ambiguous."""
    return bool(ctx["syntax"] == "intel" and ctx["mode"] != 64 and ok_parts(g)
                and m[0] == "err" and "ambiguous" in m[1]
                and re.match(r"vcvtu?si2s[sdh]\b", ctx["lines"][0]))


def mc_16bit_vsib(g, m, ctx):
    """In 16-bit mode llvm-mc takes a gather's index-only VSIB address, with its
    32-bit displacement; GNU as refuses it."""
    return bool(ctx["mode"] == 16 and gas_error(g, "is not a valid base/index expression")
                and ok_parts(m))


def vp2intersect_group(g, m, ctx):
    """`vp2intersectd` writes a pair of masks named by an even `k`; given an odd one
    GNU as encodes it as written, with a warning, and llvm-mc rounds it down."""
    gp, mp = ok_parts(g), ok_parts(m)
    return bool(gp and mp and ctx["lines"][0].startswith("vp2intersect")
                and len(gp[0]) == len(mp[0])
                and sum(a != b for a, b in zip(gp[0], mp[0])) == 1)


def mc_length_suffix(g, m, ctx):
    """llvm-mc knows the AT&T `x`/`y`/`z` length spellings of the half-precision and
    bfloat16 conversions only for some operands; GNU as takes them all."""
    word = ctx["lines"][0].split()[0]
    return bool(ok_parts(g) and m[0] == "err"
                and re.fullmatch(r"v(cvt\w+|fpclass\w+)[xyz]", word)
                and re.search(r"(ph|bf16|phx|[bh]f8s?|u?dqs)[xyz]$", word))


def mc_intel_refused(g, m, ctx):
    """Forms llvm-mc's Intel parser refuses outright: the Xeon Phi gather and scatter
    prefetches, and `rstorssp`/`clrssbsy` with a `qword ptr`."""
    word = ctx["lines"][0].split()[0]
    return bool(ctx["syntax"] == "intel" and ok_parts(g) and m[0] == "err"
                and (re.match(r"v(gather|scatter)pf", word) or word in ("rstorssp", "clrssbsy")))


def mc_ymm_rounding(g, m, ctx):
    """llvm-mc still takes `{sae}` on some 256-bit AVX10.2 conversions, from the draft
    of AVX10.2 that had rounding at every length; GNU as refuses it."""
    line = ctx["lines"][0]
    return bool(g[0] == "err" and ok_parts(m) and re.search(r"\{(r[nduz]-)?sae\}", line)
                and "ymm" in line and "zmm" not in line)


def distinct_dest(g, m, ctx):
    """The FP16 complex multiplications: GNU as refuses a destination that is also a
    source; llvm-mc assembles it."""
    return bool(gas_error(g, "must be distinct") and ok_parts(m))


def evex_vmovq_load(g, m, ctx):
    """For an EVEX `vmovq` to or from memory GNU as picks `66 W1 6E`/`7E` where llvm-mc
    picks `F3 7E`/`66 D6`. Both are valid; rsasm follows llvm-mc there."""
    gp, mp = ok_parts(g), ok_parts(m)
    return bool(gp and mp and ctx["lines"][0].startswith("vmovq") and b"\x62" in gp[0][:2])


# Differences between the two references that are conventions or quirks rather
# than something to fix. Each is (name, predicate(gas, mc, context), preferred):
# the context has the case's mode, syntax, mnemonic group and source lines, and
# `preferred` names the reference whose answer is the one to follow when one of
# them is plainly wrong, or None when both are defensible. rsasm matching
# either is not a finding, but following the non-preferred side is listed.
KNOWN_SPLITS = [
    ("prefix-order", prefix_order, None),
    ("redundant-segment", redundant_segment, None),
    ("mc-intel-opsize", mc_intel_opsize, "gas"),
    ("gas-data-prefix", gas_data_prefix, None),
    ("mc-addr32-in-16", mc_addr32_in_16, "gas"),
    ("unsized-memory", unsized_memory, None),
    ("register-as-symbol", register_as_symbol, "gas"),
    ("intel-suffix", intel_suffix, None),
    ("intel-far-direct", intel_far_direct, "gas"),
    ("xchg-order", xchg_order, None),
    ("addr32-reloc", addr32_reloc, None),
    ("disp16-wrap", disp16_wrap, None),
    ("symbolic-accumulator", symbolic_accumulator, None),
    ("vex-commute", vex_commute, "gas"),
    ("mc-lone-zeroing", mc_lone_zeroing, "gas"),
    ("unsized-gpr-scalar", unsized_gpr_scalar, "gas"),
    ("mc-16bit-vsib", mc_16bit_vsib, "gas"),
    ("vp2intersect-group", vp2intersect_group, "gas"),
    ("mc-length-suffix", mc_length_suffix, "gas"),
    ("mc-intel-refused", mc_intel_refused, "gas"),
    ("distinct-dest", distinct_dest, "gas"),
    ("mc-ymm-rounding", mc_ymm_rounding, "gas"),
    ("evex-vmovq", evex_vmovq_load, None),
]


def key(res):
    status, payload = res
    if status == "err":
        return ("err",)
    data, relocs, _w = payload
    return ("ok", data, tuple(relocs))


def is_apx(res, ctx):
    """64-bit GPR instructions that both references read as APX (an EVEX `62` where a
    legacy opcode belongs): `setb %ebx`, `rol %ecx, %ecx`. rsasm has no APX."""
    p = ok_parts(res)
    return bool(p and ctx["mode"] == 64 and not ctx["mnem"].startswith("simd")
                and p[0][opcode_start(p[0]):][:1] == b"\x62")


def classify(g, m, r, ctx):
    """Returns (class, detail); see the module docstring for the classes."""
    if r[0] == "err" and r[1].startswith("PANIC"):
        return "rsasm", "panic"
    if is_apx(g, ctx) or is_apx(m, ctx):
        return "ignored", "apx"
    kg, km, kr = key(g), key(m), key(r)
    if kg == km:
        return ("agree", None) if kr == kg else ("rsasm", None)
    follows = "gas" if kr == kg else "mc" if kr == km else "neither"
    for name, pred, preferred in KNOWN_SPLITS:
        if pred(g, m, ctx):
            if follows == "neither":
                return "rsasm", name
            if preferred and follows != preferred:
                return "convention-other", f"{name}:{follows}"
            return "convention", f"{name}:{follows}"
    return "split", follows


def run_batch(job):
    mode, syntax, cases = job
    texts = [c[0] for c in cases]
    with tempfile.TemporaryDirectory() as d:
        res = {t: assemble_batch(t, mode, syntax, texts, d) for t in ("gas", "mc", "rsasm")}
    out = []
    for i, (lines, sig, mnem, fkey) in enumerate(cases):
        g, m, r = res["gas"][i], res["mc"][i], res["rsasm"][i]
        ctx = {"mode": mode, "syntax": syntax, "mnem": mnem, "lines": lines}
        cls, detail = classify(g, m, r, ctx)
        out.append((mode, syntax, lines, sig, mnem, fkey, cls, detail,
                    fmt(g, True), fmt(m, True), fmt(r, True)))
    return out


def fuzz(args):
    import multiprocessing

    forms = build_forms() if args.forms != "simd" else []
    extra = simd.forms() if args.forms != "base" else []
    if args.forms == "simd" and not extra:
        sys.exit("--forms simd needs the binutils source from tools/oracles/build.sh")
    if args.only:
        rx = re.compile(args.only)
        forms = [f for f in forms if rx.search(f.mnem) or rx.search(f.group)]
        extra = [f for f in extra if rx.search(f.mnem) or rx.search(f.group)]
        if not forms and not extra:
            sys.exit(f"--only {args.only!r} matches no form")
    modes = [16, 32, 64] if args.mode == "all" else [int(args.mode)]
    syntaxes = ["att", "intel"] if args.syntax == "all" else [args.syntax]
    rng = random.Random(args.seed)
    jobs = []
    per = max(1, args.count // (len(modes) * len(syntaxes)))
    for mode in modes:
        for syntax in syntaxes:
            cases = []
            # The SIMD forms outnumber the rest several times over, so the two
            # sets share the cases evenly rather than by form count.
            simd_here = [f for f in extra if mode in f.modes and syntax in f.syntaxes]
            for _ in range(per):
                if simd_here and (not forms or rng.random() < 0.5):
                    c = generate_simd(rng, mode, syntax, simd_here, args.mutations)
                else:
                    c = generate(rng, mode, syntax, forms, args.mutations)
                cases.append((c.lines(), c.signature(), c.form.group, c.form_key()))
            for i in range(0, len(cases), args.batch):
                jobs.append((mode, syntax, cases[i:i + args.batch]))
    workers = args.jobs or max(1, (os.cpu_count() or 2) - 2)
    results = []
    with multiprocessing.Pool(workers) as pool:
        for batch in pool.imap(run_batch, jobs):
            results.extend(batch)
    return report(results, args)


def short(text, limit=160):
    return text if len(text) <= limit else text[:limit] + f" ... ({len(text)} chars)"


LISTED = {
    "rsasm": "rsasm differs from both references",
    "convention-other": "known reference splits where rsasm follows the non-preferred side",
    "split": "references disagree",
}


def report(results, args):
    totals = collections.Counter()
    by_mode = collections.defaultdict(collections.Counter)
    follow = collections.Counter()
    conventions = collections.Counter()
    by_mnem = {cls: collections.Counter() for cls in LISTED}
    buckets = collections.OrderedDict()
    for (mode, syntax, lines, sig, mnem, fkey, cls, detail, g, m, r) in results:
        totals[cls] += 1
        by_mode[(mode, syntax)][cls] += 1
        if cls == "split":
            follow[detail] += 1
        if cls.startswith("convention") or cls == "ignored":
            conventions[detail] += 1
        if cls in LISTED:
            by_mnem[cls][mnem] += 1
            # Cases that fail the same way are one finding: same mnemonic, operand
            # shapes and mutation, and the same accept/reject pattern.
            k = (cls, detail, mode, syntax, fkey, g.startswith("ERROR"), m.startswith("ERROR"),
                 r.startswith("ERROR"))
            b = buckets.setdefault(k, [0, None])
            b[0] += 1
            # Keep the shortest example as the one to show.
            if b[1] is None or len(" ".join(lines)) < len(" ".join(b[1][0])):
                b[1] = (lines, g, m, r, sig)

    out = []
    p = out.append
    p(f"=== {len(results)} cases: " + ", ".join(f"{k} {v}" for k, v in sorted(totals.items())))
    for (mode, syntax), c in sorted(by_mode.items()):
        p(f"  [{mode} {syntax}] " + ", ".join(f"{k} {v}" for k, v in sorted(c.items())))
    if follow:
        p("  unexplained reference splits, rsasm follows: " + ", ".join(
            f"{k} {v}" for k, v in sorted(follow.items())))
    if conventions:
        p("  known splits (name:rsasm follows): " + ", ".join(
            f"{k} {v}" for k, v in sorted(conventions.items())))
    for cls, title in LISTED.items():
        if by_mnem[cls]:
            p(f"  {cls} by mnemonic group: " + ", ".join(
                f"{k} {v}" for k, v in by_mnem[cls].most_common()))

    for cls, title in LISTED.items():
        if cls == "split" and args.no_splits:
            continue
        rows = [(k, v) for k, v in buckets.items() if k[0] == cls]
        if not rows:
            continue
        p(f"--- {title}: {len(rows)} distinct")
        rows.sort(key=lambda kv: (-kv[1][0], kv[0][2], kv[0][3], kv[0][4]))
        for k, (count, (lines, g, m, r, sig)) in rows[: args.limit]:
            _cls, detail, mode, syntax = k[:4]
            tag = f"[{mode} {syntax}]" + (f" (x{count})" if count > 1 else "")
            extra = f" follows={detail}" if cls == "split" else (f" ({detail})" if detail else "")
            p(f"{tag} {' ; '.join(lines)}{extra}    # {sig}\n    gas:   {short(g)}\n"
              f"    mc:    {short(m)}\n    rsasm: {short(r)}")
        if len(rows) > args.limit:
            p(f"    ... {len(rows) - args.limit} more (raise --limit or use --out)")
    print("\n".join(out))
    if args.out:
        with open(args.out, "w") as f:
            for (mode, syntax, lines, sig, mnem, fkey, cls, detail, g, m, r) in results:
                if cls in LISTED:
                    f.write(f"{cls}\t{detail or ''}\t{mode}\t{syntax}\t{' ; '.join(lines)}\t"
                            f"gas={g}\tmc={m}\trsasm={r}\n")
    # A non-zero exit when rsasm disagrees with both references, for scripts.
    return 1 if totals["rsasm"] else 0


def main():
    import argparse

    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("check", help="compare instructions given one per line")
    c.add_argument("--mode", type=int, default=64, choices=(16, 32, 64))
    c.add_argument("--intel", action="store_true")
    c.add_argument("--all", action="store_true", help="also print matching cases")
    c.add_argument("file", nargs="?")
    z = sub.add_parser("fuzz", help="generate random instructions and compare")
    z.add_argument("--seed", type=int, default=1)
    z.add_argument("--count", type=int, default=6000, help="total cases, split across modes")
    z.add_argument("--mode", default="all", choices=("16", "32", "64", "all"))
    z.add_argument("--syntax", default="all", choices=("att", "intel", "all"))
    z.add_argument("--only", help="regex on the mnemonic (or group: jcc, setcc, x87, simd, "
                   "simd:avx512_fp16 ...)")
    z.add_argument("--forms", default="all", choices=("base", "simd", "all"),
                   help="the hand-written forms, the SIMD forms read from GNU's table, or both")
    z.add_argument("--mutations", type=float, default=0.25,
                   help="fraction of cases mutated to be invalid")
    z.add_argument("--batch", type=int, default=200)
    z.add_argument("--jobs", type=int, default=0)
    z.add_argument("--limit", type=int, default=200, help="distinct cases printed per list")
    z.add_argument("--no-splits", action="store_true", help="omit the reference-split list")
    z.add_argument("--out", help="write every non-agreeing case, tab-separated")
    z.add_argument("--print-cases", action="store_true", help="only print the generated source")
    args = ap.parse_args()
    if args.cmd == "check":
        src = open(args.file) if args.file else sys.stdin
        cases = [[ln.rstrip("\n")] for ln in src if ln.strip() and not ln.startswith("#")]
        syntax = "intel" if args.intel else "att"
        res = compare(args.mode, syntax, cases)
        bad = 0
        for i, case in enumerate(cases):
            g, m, r = (fmt(res[t][i]) for t in ("gas", "mc", "rsasm"))
            same = g == r
            if not same:
                bad += 1
            if not same or args.all:
                flag = "  " if same else "!!"
                mc_note = "" if m == g else f"\n      mc:    {m}"
                print(f"{flag} {case[0]}\n      gas:   {g}{mc_note}\n      rsasm: {r}")
        print(f"--- {len(cases) - bad} matched gas, {bad} differed")
    elif args.print_cases:
        forms = build_forms()
        if args.only:
            forms = [f for f in forms if re.search(args.only, f.mnem) or re.search(args.only, f.group)]
        rng = random.Random(args.seed)
        modes = [16, 32, 64] if args.mode == "all" else [int(args.mode)]
        syntaxes = ["att", "intel"] if args.syntax == "all" else [args.syntax]
        for _ in range(args.count):
            mode, syntax = rng.choice(modes), rng.choice(syntaxes)
            c = generate(rng, mode, syntax, forms, args.mutations)
            print(f"[{mode} {syntax}] {' ; '.join(c.lines())}    # {c.signature()}")
    else:
        sys.exit(fuzz(args))


if __name__ == "__main__":
    main()
