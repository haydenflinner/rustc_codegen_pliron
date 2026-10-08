#!/usr/bin/env python3
"""Shared pieces for the AArch64 table generator: running llvm-mc, and the
operand grammar that both the sweep and the fitter speak.

Nothing here knows an encoding. Instruction words come from llvm-mc, either by
disassembling random words (which is how the forms are discovered) or by
assembling text (which is how each form's fields are measured).
"""

import os
import re
import subprocess

LLVM_MC = os.environ.get("LLVM_MC", "llvm-mc")

# Every extension llvm-mc 22 knows that this backend claims, so that a form is
# left out of the table only because llvm-mc cannot encode it at all.
MATTR = ",".join([
    "+v9.5a", "+sve2", "+sve2p1", "+sve2-aes", "+sve2-sha3", "+sve2-sm4",
    "+sve2-bitperm", "+sve-aes2", "+sve-b16b16", "+sve-bfscale",
    "+sve-f16f32mm", "+crypto", "+dotprod", "+i8mm", "+fullfp16", "+bf16",
    "+lse", "+rcpc", "+rand", "+memtag", "+pauth", "+fp16fml", "+flagm",
    "+sb", "+ssbs", "+predres", "+tme", "+ls64", "+f64mm", "+f32mm",
    "+jsconv", "+complxnum", "+rcpc3", "+cssc", "+the", "+d128", "+lut",
    "+faminmax", "+fp8", "+fp8fma", "+fp8dot2", "+fp8dot4", "+sme", "+sme2",
    "+sme2p1",
])

TRIPLE = "aarch64"


def _mc(args, text):
    return subprocess.run([LLVM_MC, "-triple=" + TRIPLE, "-mattr=" + MATTR] + args,
                          input=text, capture_output=True, text=True)


def disassemble(words):
    """[(word, text)] for the words llvm-mc decodes, in order.

    A word it refuses ("invalid instruction encoding") drops out; one it only
    warns about ("potentially undefined") is kept, since llvm-mc prints it.
    """
    txt = "\n".join("0x%02x 0x%02x 0x%02x 0x%02x" %
                    (w & 255, (w >> 8) & 255, (w >> 16) & 255, (w >> 24) & 255)
                    for w in words)
    p = _mc(["-disassemble"], txt)
    bad = set()
    for m in re.finditer(r"^<stdin>:(\d+):\d+: (?:error|warning: invalid instruction encoding)",
                         p.stderr, re.M):
        bad.add(int(m.group(1)))
    good = [i for i in range(1, len(words) + 1) if i not in bad]
    lines = [l.strip() for l in p.stdout.split("\n") if l.startswith("\t")]
    if len(lines) != len(good):
        raise RuntimeError("disassembly lost track: %d printed, %d expected" %
                           (len(lines), len(good)))
    return list(zip([words[i - 1] for i in good], lines))


def assemble(lines):
    """[word or None] for each source line, as llvm-mc encodes it."""
    if not lines:
        return []
    if any(l.startswith("movprfx") for l in lines):
        # llvm-mc refuses whatever follows a `movprfx` that does not use its
        # register, so each one gets a `nop` to take the blame.
        padded, where = [], []
        for l in lines:
            where.append(len(padded))
            padded.append(l)
            if l.startswith("movprfx"):
                padded.append("nop")
        got = _assemble_plain(padded)
        return [got[i] for i in where]
    return _assemble_plain(lines)


def _assemble_plain(lines):
    p = _mc(["-show-encoding"], "\n".join(lines) + "\n")
    bad = set()
    for m in re.finditer(r"^<stdin>:(\d+):\d+: error", p.stderr, re.M):
        bad.add(int(m.group(1)))
    enc = re.findall(r"encoding: \[([^\]]*)\]", p.stdout)
    good = [i for i in range(1, len(lines) + 1) if i not in bad]
    if len(enc) != len(good):
        # An alias that expands to more than one instruction, or a diagnostic
        # that named no line: fall back to one call per line.
        return [_assemble_one(l) if l != "nop" else None for l in lines]
    out = [None] * len(lines)
    for i, e in zip(good, enc):
        bs = [int(b, 16) for b in e.split(",")]
        if len(bs) != 4:
            out[i - 1] = None
        else:
            out[i - 1] = bs[0] | bs[1] << 8 | bs[2] << 16 | bs[3] << 24
    return out


def _assemble_one(line):
    p = _mc(["-show-encoding"], line + "\n")
    enc = re.findall(r"encoding: \[([^\]]*)\]", p.stdout)
    if len(enc) != 1:
        return None
    bs = [int(b, 16) for b in enc[0].split(",")]
    return bs[0] | bs[1] << 8 | bs[2] << 16 | bs[3] << 24 if len(bs) == 4 else None


# ---------------------------------------------------------------------------
# The operand grammar
# ---------------------------------------------------------------------------
#
# An operand is an `Atom`: a kind, which is what the text has to look like, and
# the numbers inside it, which are what the encoding varies. `render` turns a
# kind and a fresh set of numbers back into text, so the fitter can ask
# llvm-mc what changing one number does to the word.

ARRANGEMENTS = ["8b", "16b", "4h", "8h", "2s", "4s", "1d", "2d", "1q", "2q", "4b", "2h",
                "2b"]
ELEMS = ["b", "h", "s", "d", "q"]

# Names whose encoded value the fitter measures rather than assumes.
PATTERNS = ["pow2", "vl1", "vl2", "vl3", "vl4", "vl5", "vl6", "vl7", "vl8",
            "vl16", "vl32", "vl64", "vl128", "vl256", "mul4", "mul3", "all"]
PREFETCHES = [p + l + t for p in ("pld", "pli", "pst") for l in ("l1", "l2", "l3")
              for t in ("keep", "strm")]
SVE_PREFETCHES = [p + l + t for p in ("pld", "pst") for l in ("l1", "l2", "l3")
                  for t in ("keep", "strm")]
CONDS = ["eq", "ne", "hs", "lo", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt",
         "gt", "le", "al", "nv"]

# name -> encoded value, measured by `ensure_codes`.
COND_CODES = {}
PAT_CODES = {}
PRF_CODES = {}
COND_NAMES = {}
PAT_NAMES = {}
PRF_NAMES = {}


def _derive_codes(template, names):
    """What each name in a set of keywords encodes as, from the one field of
    the word that changes with it."""
    words = assemble([template % n for n in names])
    ok = [(n, w) for n, w in zip(names, words) if w is not None]
    if len(ok) < 2:
        raise RuntimeError("cannot measure %r" % template)
    bits = 0
    for _, w in ok:
        bits |= w ^ ok[0][1]
    lsb = (bits & -bits).bit_length() - 1
    mask = bits >> lsb
    if mask & (mask + 1):
        raise RuntimeError("%r does not use one run of bits" % template)
    return {n: (w >> lsb) & mask for n, w in ok}


def ensure_codes():
    """Measures the keyword encodings once per process."""
    if COND_CODES:
        return
    COND_CODES.update(_derive_codes("fcsel s0, s0, s0, %s", CONDS))
    PAT_CODES.update(_derive_codes("cntb x0, %s", PATTERNS))
    PRF_CODES.update(_derive_codes("prfb %s, p0, [x0]", SVE_PREFETCHES))
    for src, dst in ((COND_CODES, COND_NAMES), (PAT_CODES, PAT_NAMES),
                     (PRF_CODES, PRF_NAMES)):
        for name, code in src.items():
            dst.setdefault(code, name)


class Atom:
    __slots__ = ("kind", "vals", "sp")

    def __init__(self, kind, vals=(), sp=False):
        self.kind = kind          # tuple: a syntactic shape
        self.vals = tuple(vals)   # the numbers in it
        # Register 31 spelled `sp`/`wsp` rather than `xzr`/`wzr`. Which of the
        # two a field takes is measured, not assumed, so this only says how the
        # text that was parsed spelled it.
        self.sp = sp

    def __repr__(self):
        return "%s%r" % (":".join(str(x) for x in self.kind), self.vals)

    def with_vals(self, vals, sp=None):
        return Atom(self.kind, vals, self.sp if sp is None else sp)


def split_ops(s):
    """Splits an operand list on commas, keeping `[...]` and `{...}` whole."""
    out, depth, cur = [], 0, ""
    for ch in s:
        if ch in "[{":
            depth += 1
        elif ch in "]}":
            depth -= 1
        if ch == "," and depth == 0:
            out.append(cur.strip())
            cur = ""
        else:
            cur += ch
    if cur.strip():
        out.append(cur.strip())
    return out


def _num(s):
    s = s.strip()
    neg = s.startswith("-")
    if neg:
        s = s[1:]
    v = int(s, 16) if s.startswith("0x") else int(s)
    return -v if neg else v


def parse_operand(op, out):
    """Appends the atoms of one operand to `out`. Raises ValueError if the
    text is something this grammar does not model."""
    op = op.strip()
    m = re.fullmatch(r"v(\d+)\.(\w+)", op)
    if m and m.group(2) in ARRANGEMENTS:
        out.append(Atom(("v", m.group(2)), (int(m.group(1)),)))
        return
    m = re.fullmatch(r"v(\d+)\[(\d+)\]", op)
    if m:
        out.append(Atom(("vidx", ""), (int(m.group(1)), int(m.group(2)))))
        return
    m = re.fullmatch(r"v(\d+)\.(\w+)\[(\d+)\]", op)
    if m:
        t = m.group(2)
        if t in ELEMS:
            out.append(Atom(("vidx", t), (int(m.group(1)), int(m.group(3)))))
            return
        if t in ARRANGEMENTS:
            out.append(Atom(("vidxa", t), (int(m.group(1)), int(m.group(3)))))
            return
        raise ValueError(op)
    m = re.fullmatch(r"z(\d+)(?:\.([bhsdq]))?", op)
    if m:
        out.append(Atom(("z", m.group(2) or ""), (int(m.group(1)),)))
        return
    m = re.fullmatch(r"z(\d+)(?:\.([bhsdq]))?\[(\d+)\]", op)
    if m:
        # The lookup-table instructions index a register with no element
        # size: `luti2 z0.h, { z1.h }, z2[7]`.
        out.append(Atom(("zidx", m.group(2) or ""), (int(m.group(1)), int(m.group(3)))))
        return
    m = re.fullmatch(r"p(\d+)(?:\.([bhsdq]))?(?:/([mz]))?", op)
    if m:
        kind = "p" if not m.group(3) else "p" + m.group(3)
        out.append(Atom((kind, m.group(2) or ""), (int(m.group(1)),)))
        return
    m = re.fullmatch(r"([wx])(\d+)", op)
    if m:
        out.append(Atom(("g", m.group(1)), (int(m.group(2)),)))
        return
    if op in ("wzr", "xzr"):
        out.append(Atom(("g", op[0]), (31,)))
        return
    if op in ("sp", "wsp"):
        out.append(Atom(("g", "x" if op == "sp" else "w"), (31,), sp=True))
        return
    m = re.fullmatch(r"([bhsdq])(\d+)", op)
    if m:
        out.append(Atom(("s", m.group(1)), (int(m.group(2)),)))
        return
    if op.startswith("{"):
        return _parse_list(op, out)
    if op.startswith("["):
        return _parse_mem(op, out)
    if op.startswith("#"):
        body = op[1:]
        if re.fullmatch(r"-?\d+\.\d+(e[-+]?\d+)?", body):
            out.append(Atom(("fimm",), (float(body),)))
            return
        out.append(Atom(("imm",), (_num(body),)))
        return
    if op in COND_CODES:
        out.append(Atom(("cond",), (COND_CODES[op],)))
        return
    if op in PAT_CODES:
        out.append(Atom(("pat",), (PAT_CODES[op],)))
        return
    if op in PRF_CODES:
        out.append(Atom(("prf",), (PRF_CODES[op],)))
        return
    m = re.fullmatch(r"(lsl|msl|mul) #(-?\d+)", op)
    if m:
        out.append(Atom(("shift", m.group(1)), (int(m.group(2)),)))
        return
    raise ValueError(op)


def _parse_list(op, out):
    body = op[1:-1].strip()
    idx = None
    m = re.fullmatch(r"\{(.*)\}\[(\d+)\]", op)
    if m:
        body = m.group(1).strip()
        idx = int(m.group(2))
    parts = [p.strip() for p in body.split(",")]
    if len(parts) == 1 and " - " in parts[0]:
        a, b = [x.strip() for x in parts[0].split(" - ")]
        ma = re.fullmatch(r"([vzp])(\d+)(?:\.(\w+))?", a)
        mb = re.fullmatch(r"([vzp])(\d+)(?:\.(\w+))?", b)
        if not ma or not mb:
            raise ValueError(op)
        n = (int(mb.group(2)) - int(ma.group(2))) % 32 + 1
        first, cls, suffix = int(ma.group(2)), ma.group(1), ma.group(3) or ""
    else:
        regs = []
        cls = suffix = None
        for p in parts:
            m2 = re.fullmatch(r"([vzp])(\d+)(?:\.(\w+))?", p)
            if not m2:
                raise ValueError(op)
            regs.append(int(m2.group(2)))
            if cls is None:
                cls, suffix = m2.group(1), m2.group(3) or ""
            elif cls != m2.group(1) or suffix != (m2.group(3) or ""):
                raise ValueError(op)
        # A list is consecutive, wrapping at 32, except for the strided
        # multi-vector forms, which this grammar does not model.
        for i in range(1, len(regs)):
            if regs[i] != (regs[0] + i) % 32:
                raise ValueError(op)
        n, first = len(regs), regs[0]
    if cls == "v" and idx is not None:
        if suffix not in ELEMS:
            raise ValueError(op)
        out.append(Atom(("vlistidx", n, suffix), (first, idx)))
        return
    if idx is not None:
        raise ValueError(op)
    if cls == "v":
        if suffix not in ARRANGEMENTS:
            raise ValueError(op)
        out.append(Atom(("vlist", n, suffix), (first,)))
    elif cls == "z":
        out.append(Atom(("zlist", n, suffix), (first,)))
    else:
        out.append(Atom(("plist", n, suffix), (first,)))


def _parse_mem(op, out):
    m = re.fullmatch(r"\[(.*)\](!?)", op, re.S)
    if not m:
        raise ValueError(op)
    out.append(Atom(("open",)))
    parts = split_ops(m.group(1))
    for i, p in enumerate(parts):
        p = p.strip()
        if p == "mul vl":
            out.append(Atom(("mulvl",)))
            continue
        m2 = re.fullmatch(r"(uxtw|sxtw|sxtx|uxtx|lsl)(?: #(\d+))?", p)
        if m2 and i > 0:
            if m2.group(2) is None:
                out.append(Atom(("ext", m2.group(1))))
            else:
                out.append(Atom(("exta", m2.group(1)), (int(m2.group(2)),)))
            continue
        parse_operand(p, out)
    out.append(Atom(("close_wb",) if m.group(2) else ("close",)))


def parse_line(text):
    """(mnemonic, [Atom]) for one line of assembly."""
    text = text.split("//")[0].strip()
    parts = text.split(None, 1)
    mn = parts[0]
    atoms = []
    if len(parts) > 1:
        for op in split_ops(parts[1]):
            parse_operand(op, atoms)
    return mn, atoms


# ---- rendering -------------------------------------------------------------

def render_atom(a):
    k = a.kind
    t = k[0]
    if t == "v":
        return "v%d.%s" % (a.vals[0], k[1])
    if t in ("vidx", "vidxa"):
        dot = "." if k[1] else ""
        return "v%d%s%s[%d]" % (a.vals[0], dot, k[1], a.vals[1])
    if t == "z":
        return "z%d" % a.vals[0] + ("." + k[1] if k[1] else "")
    if t == "zidx":
        dot = "." if k[1] else ""
        return "z%d%s%s[%d]" % (a.vals[0], dot, k[1], a.vals[1])
    if t in ("p", "pm", "pz"):
        s = "p%d" % a.vals[0] + ("." + k[1] if k[1] else "")
        return s + ("/m" if t == "pm" else "/z" if t == "pz" else "")
    if t == "g":
        if a.vals[0] == 31:
            if a.sp:
                return "sp" if k[1] == "x" else "wsp"
            return "xzr" if k[1] == "x" else "wzr"
        return "%s%d" % (k[1], a.vals[0])
    if t == "s":
        return "%s%d" % (k[1], a.vals[0])
    if t == "vlist":
        return "{ %s }" % ", ".join("v%d.%s" % ((a.vals[0] + i) % 32, k[2]) for i in range(k[1]))
    if t == "vlistidx":
        return "{ %s }[%d]" % (", ".join("v%d.%s" % ((a.vals[0] + i) % 32, k[2])
                                         for i in range(k[1])), a.vals[1])
    if t == "zlist":
        return "{ %s }" % ", ".join("z%d.%s" % ((a.vals[0] + i) % 32, k[2]) for i in range(k[1]))
    if t == "plist":
        return "{ %s }" % ", ".join("p%d.%s" % ((a.vals[0] + i) % 16, k[2]) for i in range(k[1]))
    if t == "imm":
        return "#%d" % a.vals[0]
    if t == "fimm":
        return "#%.8f" % a.vals[0]
    if t == "cond":
        return COND_NAMES[a.vals[0]]
    if t == "pat":
        return PAT_NAMES[a.vals[0]]
    if t == "prf":
        return PRF_NAMES[a.vals[0]]
    if t == "shift":
        return "%s #%d" % (k[1], a.vals[0])
    if t == "mulvl":
        return "mul vl"
    if t == "ext":
        return k[1]
    if t == "exta":
        return "%s #%d" % (k[1], a.vals[0])
    raise ValueError(k)


def render(mn, atoms):
    """Assembly text for a mnemonic and its atoms."""
    ops = []
    i = 0
    while i < len(atoms):
        a = atoms[i]
        if a.kind[0] == "open":
            inner = []
            i += 1
            while atoms[i].kind[0] not in ("close", "close_wb"):
                inner.append(render_atom(atoms[i]))
                i += 1
            close = "]!" if atoms[i].kind[0] == "close_wb" else "]"
            ops.append("[" + ", ".join(inner) + close)
            i += 1
            continue
        ops.append(render_atom(a))
        i += 1
    return mn + ("\t" + ", ".join(ops) if ops else "")
