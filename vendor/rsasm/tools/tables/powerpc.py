#!/usr/bin/env python3
"""Derives rsasm's PowerPC vector table from binutils' opcode table.

src/arch/powerpc/vector.rs, the AltiVec, VSX and POWER8-10 instructions, is
not written by hand. This reads `opcodes/ppc-opc.c` from the GNU binutils
source tools/oracles/build.sh unpacks, evaluates each opcode and operand
expression with the file's own macros through cpp, and writes the table in
rsasm's form: the instruction word through the same form constructors the
ISA names (`vx`, `xx3`, `pfx`), and each operand as the rsasm field whose bit
positions match binutils' insert function or mask and shift.

    tools/tables/powerpc.py table     # rewrite src/arch/powerpc/vector.rs
    tools/tables/powerpc.py corpus    # rewrite the generated corpus blocks
    tools/tables/powerpc.py check     # exit 1 if either is out of date

`corpus` writes each instruction twice, with small operands and with large
ones, plus its record form, between the `generated` markers in
tools/mc-diff/powerpc{,64,64le}.txt and tools/xas-diff/powerpc64.txt. The
lines llvm-mc refuses (mnemonics only GNU as knows) go only to the GNU as
corpus; llvm-mc on PATH decides which those are. Nothing here says what the
bytes should be: the harnesses take those from llvm-mc and GNU as.

Environment: RSASM_ORACLES (default target/oracles), CPP (default cpp),
LLVM_MC (default llvm-mc).
"""

import os
import re
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
ORACLES = os.environ.get("RSASM_ORACLES", os.path.join(ROOT, "target", "oracles"))
BINUTILS = os.path.join(ORACLES, "src", "binutils-2.47")
CPP = os.environ.get("CPP", "cpp")
LLVM_MC = os.environ.get("LLVM_MC", "llvm-mc")
TABLE = os.path.join(ROOT, "src", "arch", "powerpc", "vector.rs")
BASE = os.path.join(ROOT, "src", "arch", "powerpc", "insn.rs")

# ============================================================================
# Reading ppc-opc.c
# ============================================================================


def read_binutils():
    """The operand table, name -> {mask, shift, insert, flags}, and the opcode
    entries, each {name, word, mask, cpu, ops}, with every expression
    evaluated."""
    src = os.path.join(BINUTILS, "opcodes", "ppc-opc.c")
    hdr = os.path.join(BINUTILS, "include", "opcode", "ppc.h")
    text = open(src).read()
    lines = text.split("\n")

    def find(prefix):
        return next(i for i, line in enumerate(lines) if line.startswith(prefix))

    op_start = find("const struct powerpc_operand powerpc_operands[] =")
    op_end = find("const unsigned int num_powerpc_operands")
    tab_start = find("const struct powerpc_opcode powerpc_opcodes[] = {")
    pfx_start = find("const struct powerpc_opcode prefix_opcodes[] = {")
    vle_start = find("const struct powerpc_opcode vle_opcodes[] = {")

    # Each operand is `#define NAME PREV + 1`, perhaps with aliases, then its
    # `{ mask, shift, insert, extract, flags }` initialiser.
    operands, entries, pending = {}, [], []
    body = re.sub(r"/\*.*?\*/", "", "\n".join(lines[op_start + 1:op_end]), flags=re.S)
    for m in re.finditer(r"#define\s+(\w+)\s+([^\n]*)|\{([^{}]*)\}", body):
        if m.group(1):
            name, val = m.group(1), m.group(2).strip()
            if re.fullmatch(r"\w+", val) and val in operands:
                operands[name] = operands[val]
            elif re.fullmatch(r"\w+(\s*\+\s*1)?", val):
                pending.append(name)
            continue
        f = [x.strip() for x in m.group(3).split(",")]
        entry = {"mask": f[0], "shift": f[1], "insert": f[2], "flags": ",".join(f[4:])}
        entries.append(entry)
        for name in pending:
            operands[name] = entry
        pending = []

    def opcodes(start, end):
        out = []
        blob = re.sub(r"/\*.*?\*/", "", "\n".join(lines[start + 1:end]), flags=re.S)
        for m in re.finditer(r'\{\s*"([^"]*)"\s*,(.*?)\{([^{}]*)\}\s*\}', blob, re.S):
            parts, depth, cur = [], 0, ""
            for ch in m.group(2):
                depth += ch == "("
                depth -= ch == ")"
                if ch == "," and depth == 0:
                    parts.append(cur.strip())
                    cur = ""
                else:
                    cur += ch
            parts = [p for p in parts + [cur.strip()] if p]
            if len(parts) >= 4:
                ops = [o.strip() for o in m.group(3).split(",") if o.strip() not in ("", "0")]
                out.append({"name": m.group(1), "word": parts[0], "mask": parts[1],
                            "cpu": parts[2], "ops": ops})
        return out

    ops = opcodes(tab_start, pfx_start) + opcodes(pfx_start, vle_start)

    # Evaluate every opcode, mask and operand expression with the file's own
    # macro definitions.
    exprs = [e[k] for e in ops for k in ("word", "mask")] + \
        [e[k] for e in entries for k in ("mask", "shift")]
    macros = "\n".join(line for line in open(hdr).read().split("\n") + lines[:tab_start]
                       if not line.startswith("#include"))
    probe = macros + "\n" + "\n".join(f"@@@ {e}" for e in exprs) + "\n"
    out = subprocess.run([CPP, "-P", "-"], input=probe, capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit(out.stderr[:2000])
    values = []
    for chunk in out.stdout.split("@@@ ")[1:]:
        expr = chunk.replace("\n", " ")
        expr = re.sub(r"\(\s*(uint64_t|int64_t|unsigned\s+long|unsigned|long\s+long|long|int)\s*\)",
                      "", expr)
        expr = re.sub(r"\b(0[xX][0-9a-fA-F]+|\d+)[uUlL]+", r"\1", expr)
        try:
            values.append(eval(expr, {"UINT64_C": lambda v: v}) & (2**64 - 1))
        except (NameError, SyntaxError):
            values.append(None)
    assert len(values) == len(exprs)
    it = iter(values)
    for e in ops:
        e["word"], e["mask"] = next(it), next(it)
    for e in entries:
        e["mask"], e["shift"] = next(it), next(it)
    return operands, ops


# ============================================================================
# Choosing and translating the entries
# ============================================================================

VEC = {"PPCVEC", "PPCVEC2", "PPCVEC3"}
VSX = {"PPCVSX", "PPCVSX2", "PPCVSX3", "PPCVSXF", "PPCVSX4"}
CPUS = VEC | VSX | {"POWER8", "POWER9", "POWER10"}

# The POWER8-10 scalar instructions a user program may execute that the base
# table in insn.rs does not already have. Privileged, hypervisor, cache and
# synchronisation instructions are left out, as README.md says.
SCALAR = set("""maddhd maddhdu maddld addpcis subpcis lnia cntlzdm cnttzdm brw brd
brh pdepd pextd cfuged addex cmprb cmpeqb setb setbc setbcr setnbc setnbcr
modud moduw modsd modsw cnttzw cnttzd extswsli copy paste. darn mcrxrx
mffsce mffscdrn mffscdrni mffscrn mffscrni mffsl scv fmrgew fmrgow""".split())

# Vector-related instructions whose operands are not vector registers: the
# AltiVec data-stream hints, VRSAVE, and the VSX moves spelled with an FPR.
VECTOR_OTHER = set("""dst dstt dstst dststt dss dssall mfvrsave mtvrsave
mtfprd mtfprwa mtfprwz mffprd mffprwz""".split())

# The doubleword members of the new word/doubleword pairs, and the prefixed
# forms of `ld`, `std` and `lwa`: 64-bit only, as their counterparts in the
# base table are.
P64 = set("""maddhd maddhdu maddld modsd modud extswsli cnttzd cntlzdm
cnttzdm pdepd pextd cfuged brd mfvsrd mtvsrd mfvsrld mtvsrdd pld pstd
plwa""".split())

# Prefixed instructions left out: the quadword loads and a FUTURE spelling.
PREFIXED_SKIP = {"plq", "pstq", "plis", "pstfsx"}

# Operands naming what rsasm does not reach: the MMA accumulators and their
# masks, quadword GPR pairs, the hash operands, POWER11's AES key size and
# Galois-field polynomial.
UNSUPPORTED = {"ACC", "DMR", "DMRAB", "DMRATp", "XMSK", "XMSK8", "YMSK", "YMSK2",
               "PMSK2", "PMSK4", "PMSK8", "RTQ", "RSQ", "RAX", "RBX", "RAS", "DW",
               "XA6a", "XB6a", "XA6ap", "PRAQ", "AESM", "PGF1", "XTOP2", "SVP64"}

# binutils operand -> rsasm field. Anything not named here is read off the
# operand table: a plain immediate becomes Uim or Sim with the table's width
# and shift.
FIELD = {
    "VD": "Vt", "VS": "Vt", "VA": "Va", "VB": "Vb", "VC": "Vc", "VAB": "VaVb",
    "XT6": "Xt", "XS6": "Xt", "XA6": "Xa", "XB6": "Xb", "XC6": "Xc",
    "XAB6": "Xab", "XTQ6": "Xtq", "XSQ6": "Xtq", "XTS": "Xts",
    "XTP": "Xtp", "XSP": "Xtp", "XA5p": "Xap", "XB5p": "Xbp", "XTOP": "Xtop",
    "RT": "Rt", "RS": "Rt", "RA": "Ra", "RA0": "Ra", "RB": "Rb", "RC": "Rc",
    "FRT": "Ft", "FRS": "Ft", "FRA": "Fa", "FRB": "Fb", "FRC": "Fc",
    "BF": "CrfD", "OBF": "CrfD", "BFA": "CrfS", "BI": "Bi", "BO": "Bo",
    "BH": "Bh", "L": "L", "SH6": "Sh6", "SIMM": "Sim(5, 16)",
    "IMM8": "SimU(8, 11)", "DXD": "Dx", "NDXD": "NegDx", "DCMXS": "Dcmxs",
    "DMEX": "DmEx", "SI34": "Simm34", "NSI34": "NegSimm34", "IMM32": "Imm32",
    "PCREL": "Pcrel", "PCREL1": "Pcrel", "SVC_LEV": "Lev", "L1OPT": "L1",
}
# Displacements, and the memory field each becomes with the base after it.
MEM = {"D": "MemD", "DS": "MemDS", "DQ": "MemDQ", "D34": "MemD34"}
BASES = {"RA0", "PRA0", "RA", "PRAQ"}


def choose(operands, opcodes):
    def optional(name):
        return name in operands and "PPC_OPERAND_OPTIONAL" in operands[name]["flags"]

    def field(name):
        if name in FIELD:
            return FIELD[name]
        en = operands.get(name)
        if en is None or en["insert"] != "NULL" or en["mask"] is None or en["shift"] is None:
            return None
        if any(f"PPC_OPERAND_{k}" in en["flags"] for k in ("GPR", "FPR", "VR", "VSR", "CR")):
            return None
        mask = en["mask"]
        if mask & (mask + 1):
            return None
        kind = "Sim" if "PPC_OPERAND_SIGNED" in en["flags"] else "Uim"
        return "%s(%d, %d)" % (kind, mask.bit_length(), en["shift"])

    def pattern(ops):
        out, i = [], 0
        while i < len(ops):
            if ops[i] in UNSUPPORTED:
                return None
            if ops[i] in MEM and i + 1 < len(ops) and ops[i + 1] in BASES:
                out.append(MEM[ops[i]])
                i += 2
                continue
            f = field(ops[i])
            if f is None:
                return None
            out.append(f)
            i += 1
        flags = ["OPTL"] if ops and optional(ops[-1]) else \
            ["OPT1"] if ops and optional(ops[0]) else []
        return out, flags

    base_names = set(re.findall(r'd\("([^"]+)"', open(BASE).read()))
    chosen = {}
    for e in opcodes:
        name, ops = e["name"], e["ops"]
        if e["cpu"] not in CPUS or name in base_names or name.endswith(("-", "+")):
            continue
        # POWER11's AES and Galois-field instructions: GNU as 2.47 only takes
        # them with -mfuture, and llvm-mc swaps the 192- and 256-bit forms.
        if name.startswith(("xxaes", "xxgfmul")):
            continue
        prefixed = e["word"] >> 32 != 0
        vector = any(o in FIELD and FIELD[o][0] in "VX" for o in ops)
        if not (vector or name in SCALAR or name in VECTOR_OTHER
                or (prefixed and name not in PREFIXED_SKIP)):
            continue
        p = pattern(ops)
        if p is None:
            continue
        e = dict(e, pat=p[0], pflags=p[1], prefixed=prefixed, p64=name in P64)
        # `paste.`'s L reads 1 when left out: the word carries the bit and the
        # field clears it.
        if "L1OPT" in ops:
            e["word"] |= 1 << 21
        old = chosen.get(name)
        # The first of two entries for one mnemonic wins, unless the second is
        # the same instruction with an optional operand added.
        if old is None or (e["word"] == old["word"] and ops[:len(old["ops"])] == old["ops"]
                           and all(optional(o) for o in ops[len(old["ops"]):])):
            chosen[name] = e

    # A record form binutils lists apart becomes a flag, as `add.` is.
    dotted = {}
    for e in opcodes:
        if e["name"].endswith("."):
            dotted.setdefault(e["name"], []).append(e)
    for name, e in list(chosen.items()):
        for alt in dotted.get(name + ".", []):
            if alt["ops"] != e["ops"]:
                continue
            delta = alt["word"] ^ e["word"]
            if delta in (1, 0x400):
                e["rc"] = "RC" if delta == 1 else "VRC"
                chosen.pop(name + ".", None)
    return sorted(chosen.values(), key=lambda e: (section(e), e["name"]))


# ============================================================================
# The table
# ============================================================================

SECTIONS = [
    ("""    // ---- AltiVec loads and stores ----------------------------------------
    // A vector load or store is X-form and ignores the low four bits of the
    // address, so there is no displaced form to go with it.""", "AltiVec loads and stores"),
    ("""    // ---- AltiVec, VX and VA form -----------------------------------------
    // Opcode 4 with an eleven-bit extended opcode, or six bits and a third
    // register. The compares also have a record form, and it sets CR6 from
    // bit 21 rather than CR0 from bit 31, which is what `VRC` says.""",
     "AltiVec arithmetic and logic"),
    ("""    // ---- VSX loads, stores and register moves ------------------------------""",
     "VSX loads, stores and moves"),
    ("""    // ---- VSX computation ---------------------------------------------------
    // Opcode 60 with a two-, three- or four-register form, plus the
    // quad-precision instructions, which live in opcode 63 and take their
    // operands from the AltiVec half of the bank.""", "VSX computation"),
    ("""    // ---- POWER9 and POWER10 scalar additions -------------------------------""",
     "POWER9 and POWER10 scalar"),
    ("""    // ---- prefixed (POWER10) ------------------------------------------------
    // Eight bytes: a prefix word carrying the top of a 34-bit displacement
    // and the R bit, then an ordinary instruction word.""", "prefixed (POWER10)"),
]


def section(e):
    name, primary = e["name"], e["word"] >> 26
    if e["word"] >> 32:
        return 5
    if name.startswith(("lv", "stv")) or name in (
            "mfvscr", "mtvscr", "dst", "dstt", "dstst", "dststt", "dss", "dssall",
            "mfvrsave", "mtvrsave"):
        return 0
    if primary == 4:
        return 1
    if name in SCALAR:
        return 4
    if primary in (31, 57, 61) and name.startswith(("l", "st", "mfv", "mtv", "mff", "mtf")):
        return 2
    return 3


SLOTS = [(0, 0), (1, 5), (6, 10), (11, 15), (16, 20), (21, 25)]


def slots(v):
    """Fixed bits outside the form's opcode fields, one term per operand slot
    they sit in."""
    out, rest = [], v
    for lo, hi in SLOTS:
        piece = (rest >> lo) & ((1 << (hi - lo + 1)) - 1)
        if piece:
            out.append("%d" % piece if lo == 0 else "(%d << %d)" % (piece, lo))
            rest &= ~(piece << lo)
    return ["0x%08x" % v] if rest else out


def suffix_expr(w, mask=0, rc=None):
    """A 32-bit instruction word through the constructor for its form."""
    primary = w >> 26
    if primary == 4:
        form = ("vxr", 0x3FF, 0) if rc == "VRC" else \
            ("vxa", 0x3F, 0) if mask & 0x7FF == 0x3F else ("vx", 0x7FF, 0)
    elif primary in (57, 58, 61, 62) and w & 3:
        form = ("op", 0, 0)                  # DS- and DQ-form sub-opcodes
    elif primary in (60, 61) and mask & 0x7FC == 0x7FC:
        form = ("xx2", 0x1FF, 2)
    elif primary in (60, 61) and mask & 0x7F8 == 0x7F8:
        form = ("xx3", 0xFF, 3)
    elif primary in (60, 61) and mask & 0x30 == 0x30:
        form = ("xx4", 0x3, 4)
    else:
        form = ("x", 0x3FF, 1)
    fn, fmask, shift = form
    ext = (w >> shift) & fmask
    if fn == "op" or (ext == 0 and fn == "x"):
        head, extra = "op(%d)" % primary, w ^ (primary << 26)
    else:
        head, extra = "%s(%d, %d)" % (fn, primary, ext), w ^ ((primary << 26) | (ext << shift))
    return " | ".join([head] + slots(extra))


def word_expr(e):
    w = e["word"]
    if not e["prefixed"]:
        return suffix_expr(w, e["mask"], e.get("rc"))
    prefix, suffix = w >> 32, w & 0xFFFFFFFF
    form = ["P8LS", "P8RR", "PMLS", "PMRR"][(prefix >> 24) & 3]
    rest = prefix & ~0x07000000
    p = form if rest == 0 else "%s | 0x%08x" % (form, rest)
    s = "op(%d)" % (suffix >> 26) if suffix & 0x03FFFFFF == 0 else suffix_expr(suffix)
    return "pfx(%s, %s)" % (p, s)


def render_entry(e):
    flags = ([e["rc"]] if "rc" in e else []) + e["pflags"] + (["P64"] if e["p64"] else [])
    return 'd("%s", %s, &[%s], %s),' % (e["name"], word_expr(e), ", ".join(e["pat"]),
                                         " | ".join(flags) or "0")


HEAD = '''//! The AltiVec, VSX and POWER10 instruction table.
//!
//! The same shape as [`super::insn`]'s table, and read by the same code; it
//! is here because the vector instructions outnumber everything else and
//! bring their own forms with them. The fields they add are the vector and
//! VSX register slots and a generic immediate,
//! [`F::Uim`](super::insn::F::Uim), that stands in for the two dozen one-off
//! immediates these forms carry.
//!
//! Three things are worth knowing before reading the table:
//!
//! - **A VSX register number is six bits.** Five fit in the same slot a
//!   vector register would use; the sixth is one of four bits at the bottom
//!   of the word (TX, AX, BX, CX). So `xxlor 0, 32, 63` and `xxlor 0, 0, 31`
//!   differ in bits nothing else touches, which is what
//!   [`F::Xt`](super::insn::F::Xt) and its relatives place.
//! - **The vector record bit is not Rc.** `vcmpequb.` sets bit 21 and writes
//!   CR6, where `add.` sets bit 31 and writes CR0; the table flags those
//!   forms [`VRC`].
//! - **A prefixed instruction is eight bytes**, a prefix word followed by a
//!   suffix that looks like an ordinary instruction. The table keeps both in
//!   one 64-bit value, prefix first, and so does the encoder.
//!
//! This file is written by `tools/tables/powerpc.py table` from the opcode
//! table in GNU binutils' `ppc-opc.c`, not by hand: change the script, not the
//! entries. Every form in it is in the `tools/mc-diff` and `tools/xas-diff`
//! corpora, where llvm-mc and GNU as check the bytes.

use super::insn::{Def, F::*, OPTL, P64, RC, VRC, d, op, x};

/// VX form: opcode 4 with an eleven-bit extended opcode in bits 21:31.
const fn vx(primary: u32, ext: u32) -> u64 {
    op(primary) | ext as u64
}

/// VXR form: the vector compares, whose extended opcode is ten bits, leaving
/// bit 21 for the record bit.
const fn vxr(primary: u32, ext: u32) -> u64 {
    op(primary) | ext as u64
}

/// VA form: a six-bit extended opcode in bits 26:31, since a fourth register
/// takes the space above it.
const fn vxa(primary: u32, ext: u32) -> u64 {
    op(primary) | ext as u64
}

/// XX2, XX3 and XX4 forms: opcode 60 with a nine-, eight- or two-bit extended
/// opcode, the rest of the low bits being the VSX register extensions.
const fn xx2(primary: u32, ext: u32) -> u64 {
    op(primary) | ((ext as u64) << 2)
}

const fn xx3(primary: u32, ext: u32) -> u64 {
    op(primary) | ((ext as u64) << 3)
}

const fn xx4(primary: u32, ext: u32) -> u64 {
    op(primary) | ((ext as u64) << 4)
}

/// A prefixed instruction: the prefix word in the upper half, the suffix in
/// the lower.
const fn pfx(prefix: u32, suffix: u64) -> u64 {
    ((prefix as u64) << 32) | suffix
}

/// The four prefix forms, in bits 6:7 of the prefix word: eight-byte load and
/// store, eight-byte register to register, and the two "modified" forms that
/// add to a register.
const P8LS: u32 = op(1) as u32;
const P8RR: u32 = op(1) as u32 | (1 << 24);
const PMLS: u32 = op(1) as u32 | (2 << 24);
const PMRR: u32 = op(1) as u32 | (3 << 24);

#[rustfmt::skip]
pub static DEFS: &[Def] = &[
'''


def render_table(entries):
    out, last = [HEAD], None
    for e in entries:
        if section(e) != last:
            last = section(e)
            out.append(SECTIONS[last][0] + "\n")
        out.append("    " + render_entry(e) + "\n")
    return "".join(out) + "];\n"


# ============================================================================
# The corpora
# ============================================================================

# Two operand values for each field: small ones, then ones that set the high
# bits — a VSX register above 31, the top of an immediate, a negative
# displacement. The R operand stays 0, since it can only be 1 with no base
# register; the hand-written cases after the generated block cover that.
SAMPLE = {
    "Vt": ("2", "30"), "Va": ("3", "1"), "Vb": ("4", "31"), "Vc": ("5", "0"),
    "VaVb": ("3", "31"),
    "Xt": ("1", "63"), "Xa": ("2", "40"), "Xb": ("3", "52"), "Xc": ("4", "33"),
    "Xab": ("5", "45"), "Xtq": ("3", "35"), "Xts": ("3", "35"),
    "Xtop": ("3", "35"), "Xtp": ("2", "34"), "Xap": ("4", "36"), "Xbp": ("6", "38"),
    "Rt": ("3", "31"), "Ra": ("4", "0"), "Rb": ("5", "31"), "Rc": ("6", "0"),
    "L1": ("0", "1"),
    "Ft": ("1", "31"), "Fa": ("2", "0"), "Fb": ("3", "31"), "Fc": ("4", "0"),
    "CrfD": ("0", "5"), "CrfS": ("1", "7"), "Bi": ("5", "31"), "L": ("0", "1"),
    "Sh6": ("5", "63"), "Bh": ("0", "3"), "Lev": ("1", "0"),
    "Dx": ("100", "-32768"), "NegDx": ("100", "32767"), "Dcmxs": ("3", "127"),
    "DmEx": ("0", "1"),
    "MemD": ("8(4)", "-4(0)"), "MemDS": ("8(4)", "-8(0)"),
    "MemDQ": ("16(4)", "-32(0)"), "MemD34": ("100(4)", "-8589934592(0)"),
    "Simm34": ("100", "-8589934592"), "NegSimm34": ("100", "-100"),
    "Imm32": ("305419896", "-1"), "Pcrel": ("0", "0"),
}

# Where llvm-mc's range for a field is narrower than GNU as's, stay inside
# both; the wider values are in the hand-written GNU as cases.
OVERRIDE = {
    ("mtvsrbmi", 1): {1: "65535"},
    ("xxgenpcvbm", 1): {2: "15"}, ("xxgenpcvhm", 1): {2: "15"},
    ("xxgenpcvwm", 1): {2: "15"}, ("xxgenpcvdm", 1): {2: "15"},
}


def sample(f, which):
    if f in SAMPLE:
        return SAMPLE[f][which]
    kind, bits, _lsb = re.match(r"(SimU|Uim|Sim)\((\d+), (\d+)\)", f).groups()
    bits = int(bits)
    if kind == "Uim":
        return ("1", str((1 << bits) - 1))[which] if bits > 1 else ("0", "1")[which]
    if kind == "Sim":
        return ("1", str(-(1 << (bits - 1))))[which]
    return ("1", str((1 << bits) - 1))[which]


def corpus_lines(entries, bits):
    out, last = [], None
    for e in entries:
        if bits == 32 and e["p64"]:
            continue
        if section(e) != last:
            last = section(e)
            out.append("")
            out.append("# ---- " + SECTIONS[last][1])
        forms = [(e, 0), (e, 1)] + ([(dict(e, name=e["name"] + "."), 1)] if "rc" in e else [])
        for f, which in forms:
            ops = [sample(p, which) for p in f["pat"]]
            for i, v in OVERRIDE.get((f["name"], which), {}).items():
                ops[i] = v
            if "OPTL" in f["pflags"] and which == 1:
                ops.pop()
            out.append(f"{f['name']} {', '.join(ops)}" if ops else f["name"])
    return out


def refused_by_llvm_mc(lines):
    """The indices of the lines llvm-mc will not assemble."""
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "in.s")
        with open(path, "w") as f:
            f.write("\n".join(lines) + "\n")
        p = subprocess.run([LLVM_MC, "-triple=powerpc64", "-filetype=obj", "-o",
                            os.path.join(d, "out.o"), path], capture_output=True, text=True)
    return {int(m.group(1)) - 1 for m in re.finditer(r":(\d+):\d+: error", p.stderr)}


BEGIN = "# ---- begin: generated by tools/tables/powerpc.py corpus, do not edit"
END = "# ---- end: generated"


def splice(path, lines):
    text = open(path).read()
    start, end = text.index(BEGIN), text.index(END)
    return text[:start] + BEGIN + "\n" + "\n".join(lines).strip("\n") + "\n" + text[end:]


def corpora(entries):
    out = {}
    for bits, names in ((64, ("powerpc64", "powerpc64le")), (32, ("powerpc",))):
        lines = corpus_lines(entries, bits)
        refused = refused_by_llvm_mc(lines)
        mc = [line for i, line in enumerate(lines) if i not in refused]
        for name in names:
            path = os.path.join(ROOT, "tools", "mc-diff", f"{name}.txt")
            out[path] = splice(path, mc)
        if bits == 64:
            path = os.path.join(ROOT, "tools", "xas-diff", "powerpc64.txt")
            out[path] = splice(path, lines)
    return out


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd not in ("table", "corpus", "check"):
        sys.exit(__doc__)
    operands, opcodes = read_binutils()
    entries = choose(operands, opcodes)
    files = {}
    if cmd in ("table", "check"):
        files[TABLE] = render_table(entries)
    if cmd in ("corpus", "check"):
        files.update(corpora(entries))
    stale = [p for p, text in files.items() if open(p).read() != text]
    if cmd == "check":
        for p in stale:
            print(f"out of date: {os.path.relpath(p, ROOT)}")
        sys.exit(1 if stale else 0)
    for p in stale:
        with open(p, "w") as f:
            f.write(files[p])
    print(f"{len(entries)} instructions; rewrote {len(stale)} file(s)", file=sys.stderr)


if __name__ == "__main__":
    main()
