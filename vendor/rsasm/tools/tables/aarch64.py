#!/usr/bin/env python3
"""Derives rsasm's AArch64 SIMD/SVE encoding table from llvm-mc.

Three phases, none of which contains an encoding written by hand:

  discover  random instruction words are disassembled by llvm-mc; each
            distinct (mnemonic, operand shape) it prints becomes a candidate
            form. Alias spellings llvm-mc prints for no encoding are added
            from ALIASES, which is syntax only.
  fit       for each form, every operand is varied on its own and the line is
            reassembled, so the bits each operand reaches, the values it
            accepts and the opcode left when all of them are zero are measured
            rather than assumed.
  emit      the fitted forms are written as src/arch/aarch64/table_data.rs,
            and a line per form as a differential corpus.

Every fitted form is checked against llvm-mc on random operand values before
it is written out, so a form in the table is one this generator has seen
llvm-mc encode exactly that way.

    tools/tables/aarch64.py table --jobs 32      # about ten minutes
    tools/tables/aarch64.py check               # exit 1 if it is out of date
    tools/tables/aarch64.py fit --only '^fmov$' --dump forms.txt

`check` regenerates into a temporary directory and compares, so it needs
llvm-mc and as long as `table`; the sweeps are split into a fixed number of
pieces, so the answer depends on `--seed` and not on `--jobs`.
"""

import argparse
import collections
import itertools
import multiprocessing
import os
import pickle
import random
import re
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import a64
from a64 import Atom, assemble, disassemble, parse_line, render

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))

# ---------------------------------------------------------------------------
# What the table covers
# ---------------------------------------------------------------------------

# Instructions the handwritten backend encodes, left out of the table unless
# they take an SVE register (`ldr z0, [x0]` is the table's, `ldr x0, [x0]` is
# not). Branches and PC-relative forms need fixups, which a table row has no
# room for.
HANDWRITTEN = set("""
b bl br blr ret eret drps adr adrp cbz cbnz tbz tbnz svc hvc smc brk hlt
dcps1 dcps2 dcps3 nop yield wfe wfi sev sevl hint dmb dsb isb clrex mrs msr
ldr str ldrb strb ldrh strh ldrsb ldrsh ldrsw ldur stur ldurb sturb ldurh
sturh ldursb ldursh ldursw prfm prfum ldp stp ldpsw ldnp stnp
add adds sub subs cmp cmn neg negs adc adcs sbc sbcs ngc ngcs and ands orr
eor bic bics orn eon tst mvn mov movz movn movk sbfm ubfm bfm sbfx ubfx bfxil
sbfiz ubfiz bfi sxtb sxth sxtw uxtb uxth lsl lsr asr ror lslv lsrv asrv rorv
extr mul mneg smull umull smnegl umnegl smulh umulh madd msub smaddl umaddl
smsubl umsubl sdiv udiv rbit rev rev16 rev32 rev64 clz cls csel csinc csinv
csneg cset csetm cinc cinv cneg ccmp ccmn
""".split())

# The loads and stores among them, which the backend also encodes for the
# scalar SIMD registers (`ldr q0, [x0]`).
HANDWRITTEN_LOADS = set("""
ldr str ldrb strb ldrh strh ldrsb ldrsh ldrsw ldur stur ldurb sturb ldurh sturh
ldursb ldursh ldursw prfm prfum ldp stp ldpsw ldnp stnp
""".split())

# Operand kinds that make a line this table's business.
SIMD_KINDS = {"v", "vidx", "vidxa", "s", "z", "zidx", "p", "pm", "pz",
              "vlist", "vlistidx", "zlist", "plist", "fimm"}

# SME beyond `smstart`/`zero {za}`, which the backend spells out: the ZA array,
# lookup tables, predicate-as-counter and multi-vector groups.
SKIP_TEXT = re.compile(r"\b(za\b|za\d|zt0|pn\d+|vgx\d|\{\s*z\d+\.\w+,\s*z\d+\.\w+\s*\}"
                       r"|mode|sm\b|everything)")

# Spellings llvm-mc accepts but never prints, so the sweep cannot find them:
# the preferred alias is what it prints for their encodings. Syntax only: what
# each one encodes is measured like everything else. `<a|b|c>` stands for each
# alternative in turn, and groups with as many alternatives go together; a
# line llvm-mc refuses is dropped.
ALIASES = """
sxtl v0.<8h|4s|2d>, v0.<8b|4h|2s>
uxtl v0.<8h|4s|2d>, v0.<8b|4h|2s>
sxtl2 v0.<8h|4s|2d>, v0.<16b|8h|4s>
uxtl2 v0.<8h|4s|2d>, v0.<16b|8h|4s>
ins v0.<b|h|s>[0], w0
ins v0.d[0], x0
ins v0.<b|h|s|d>[0], v0.<b|h|s|d>[0]
mov v0.<b|h|s|d>[0], v0.<b|h|s|d>[0]
mov v0.<b|h|s>[0], w0
mov v0.d[0], x0
umov w0, v0.<b|h|s>[0]
umov x0, v0.d[0]
mov w0, v0.s[0]
mov x0, v0.d[0]
mov <b|h|s|d>0, v0.<b|h|s|d>[0]
not v0.<8b|16b>, v0.<8b|16b>
mvn v0.<8b|16b>, v0.<8b|16b>
<bic|orr> v0.<4h|8h|2s|4s>, #0
<bic|orr> v0.<4h|8h|2s|4s>, #0, lsl #0
<movi|mvni> v0.<4h|8h|2s|4s>, #0
<movi|mvni> v0.<4h|8h|2s|4s>, #0, lsl #0
movi v0.<8b|16b>, #0, lsl #0
movi d0, #0
fmov <h|s|d>0, #<1.0|0.0>
fmov v0.<4h|8h|2s|4s|2d>, #1.0
<cmle|cmlt|cmls|cmlo> v0.<8b|16b|4h|8h|2s|4s|2d>, v0.<8b|16b|4h|8h|2s|4s|2d>, v0.<8b|16b|4h|8h|2s|4s|2d>
<cmle|cmlt|cmls|cmlo> d0, d0, d0
<fcmle|fcmlt|facle|faclt> v0.<4h|8h|2s|4s|2d>, v0.<4h|8h|2s|4s|2d>, v0.<4h|8h|2s|4s|2d>
<fcmle|fcmlt|facle|faclt> <h|s|d>0, <h|s|d>0, <h|s|d>0
mov z0.<b|h|s|d>, z0.<b|h|s|d>
mov z0.<b|h|s|d>, p0/<m|z>, z0.<b|h|s|d>
mov z0.<b|h|s>, w0
mov z0.d, x0
mov z0.<b|h|s|d>, <b|h|s|d>0
mov z0.<b|h|s|d>, #0
mov z0.<b|h|s|d>, p0/<m|z>, #0
mov z0.<b|h|s>, p0/m, w0
mov z0.d, p0/m, x0
mov z0.<b|h|s|d>, p0/m, <b|h|s|d>0
mov z0.<b|h|s|d>, z0.<b|h|s|d>[0]
mov p0.b, p0.b
mov p0.b, p0/<m|z>, p0.b
movs p0.b, p0.b
movs p0.b, p0/z, p0.b
<not|nots> p0.b, p0/z, p0.b
fmov z0.<h|s|d>, #<1.0|0.0>
fmov z0.<h|s|d>, p0/m, #<1.0|0.0>
fcpy z0.<h|s|d>, p0/m, #1.0
<cmple|cmplt|cmplo|cmpls> p0.<b|h|s|d>, p0/z, z0.<b|h|s|d>, z0.<b|h|s|d>
<fcmle|fcmlt|facle|faclt> p0.<h|s|d>, p0/z, z0.<h|s|d>, z0.<h|s|d>
dup z0.<b|h|s|d>, #<1|0>
dup z0.<h|s|d>, #<256|-256>
dup z0.<h|s|d>, #1, lsl #8
dupm z0.<b|h|s|d>, #<1|0x3f>
cpy z0.<b|h|s|d>, p0/<m|z>, #1
cpy z0.<h|s|d>, p0/<m|z>, #<256|-256>
cpy z0.<h|s|d>, p0/<m|z>, #1, lsl #8
cpy z0.<b|h|s>, p0/m, <w0|wsp>
cpy z0.d, p0/m, <x0|sp>
cpy z0.<b|h|s|d>, p0/m, <b|h|s|d>0
fdup z0.<h|s|d>, #1.0
fcpy z0.<h|s|d>, p0/m, #1.0
<orn|eon|bic> z0.<b|h|s|d>, z0.<b|h|s|d>, #1
sel z0.<b|h|s|d>, p0, z0.<b|h|s|d>, z0.<b|h|s|d>
pmov p0.<h|s|d>, z0
pmov z0, p0.<h|s|d>
"""


def expand_aliases(text):
    """Each template line of ALIASES, with its `<a|b>` groups expanded."""
    out = []
    for line in text.strip().split("\n"):
        line = line.strip()
        if not line:
            continue
        groups = re.findall(r"<([^>]*)>", line)
        if not groups:
            out.append(line)
            continue
        alts = [g.split("|") for g in groups]
        sizes = sorted({len(a) for a in alts})
        # Groups of one size vary together; groups of different sizes vary
        # independently of each other.
        for combo in itertools.product(*[range(n) for n in sizes]):
            pick = dict(zip(sizes, combo))
            text_ = line
            for a in alts:
                text_ = re.sub(r"<[^>]*>", a[pick[len(a)]], text_, count=1)
            out.append(text_)
    return out


# ---------------------------------------------------------------------------
# Discovery
# ---------------------------------------------------------------------------

def interesting(mn, atoms, text, word):
    if SKIP_TEXT.search(text):
        return False
    kinds = {a.kind[0] for a in atoms}
    if sum(1 for a in atoms if a.kind[0] in ("zlist", "vlist")) > 1:
        return False
    if mn in HANDWRITTEN_LOADS and not (kinds & {"z", "zidx", "zlist", "plist", "p", "pm", "pz"}):
        return False
    if mn.startswith("b.") or mn.startswith("bc."):
        return False
    # A general-purpose form of an instruction the backend does not write by
    # hand is kept too, and dropped after the sweep unless the same mnemonic
    # has a SIMD form: `ldapur x0, [x1]` goes with `ldapur d0, [x1]`.
    return bool(kinds & SIMD_KINDS) or mn not in HANDWRITTEN or sve_space(word)


def sve_space(word):
    """True for a word in the SVE encoding group, bits 28-25 `0010`: `cntd
    x0` and `addvl sp, sp, #1` are SVE though they name no SVE register."""
    return (word >> 25) & 0xf == 0b0010


def shape_key(mn, atoms):
    return (mn, tuple(a.kind for a in atoms))


def sweep(words):
    """{shape key: [(word, atoms)]} for one batch of random words."""
    a64.ensure_codes()
    out = collections.defaultdict(list)
    for w, text in disassemble(words):
        try:
            mn, atoms = parse_line(text)
        except ValueError:
            continue
        if not interesting(mn, atoms, text, w):
            continue
        out[shape_key(mn, atoms)].append((w, atoms))
    return out


SAMPLES = 64

# How many pieces each sweep is split into, whatever `--jobs` says, so that
# the table a run writes depends on `--seed` alone.
CHUNKS = 128


class Reservoir:
    """Up to SAMPLES samples of each shape, a uniform choice of all seen, so
    that the first prefixes swept do not decide which operand values a shape
    is fitted from."""

    def __init__(self, seed):
        self.rng = random.Random(seed)
        self.found = {}
        self.seen = collections.Counter()

    def add(self, key, sample):
        self.seen[key] += 1
        cur = self.found.setdefault(key, [])
        if len(cur) < SAMPLES:
            cur.append(sample)
        else:
            j = self.rng.randrange(self.seen[key])
            if j < SAMPLES:
                cur[j] = sample

    def add_all(self, part):
        for k, v in part.items():
            for smp in v:
                self.add(k, smp)


def words_job(args):
    """Given words."""
    words, seed = args
    res = Reservoir(seed)
    for i in range(0, len(words), 50000):
        try:
            res.add_all(sweep(words[i:i + 50000]))
        except RuntimeError as e:
            print("sweep: %s" % e, file=sys.stderr)
    return res.found, res.seen


def sweep_job(args):
    """Random words."""
    seed, n = args
    rng = random.Random(seed)
    res = Reservoir(seed)
    done = 0
    while done < n:
        batch = min(50000, n - done)
        done += batch
        try:
            res.add_all(sweep([rng.getrandbits(32) for _ in range(batch)]))
        except RuntimeError as e:
            print("sweep: %s" % e, file=sys.stderr)
    return res.found, res.seen


# ---------------------------------------------------------------------------
# Fitting
# ---------------------------------------------------------------------------

REG_KINDS = {"v", "vidx", "vidxa", "s", "z", "zidx", "p", "pm", "pz",
             "vlist", "vlistidx", "zlist", "plist", "g"}

# Slot encodings, as they reach the Rust table.
#   ("fixed", v)                     the only value this form accepts
#   ("field", lsb, width, max)       an unsigned value, placed as it is
#   ("scatter", [(vbit, wbit)], max) an unsigned value, spread through the word
#   ("affine", lsb, width, sign, step, min, max)
#                                    ((v - min) / step) * sign, from the base
#   ("logimm", lsb, esize)           the bitmask immediate
#   ("fpimm", lsb)                   the 8-bit floating-point immediate
#   ("tied", slot)                   must equal another slot; encodes nothing


def fp_imm8(value):
    """The imm8 an A64 floating-point immediate encodes as, or None.

    The ARM ARM's `VFPExpandImm`: a sign, an exponent of -3..4 and a fraction
    of sixteenths, so the 64 positive values are 2^e * (1 + n/16).
    """
    for imm8 in range(256):
        if fp_imm8_value(imm8) == value:
            return imm8
    return None


def fp_imm8_value(imm8):
    """What an 8-bit floating-point immediate `abcdefgh` stands for."""
    sign = -1.0 if imm8 & 0x80 else 1.0
    b = (imm8 >> 6) & 1
    cd = (imm8 >> 4) & 3
    frac = imm8 & 15
    exp = cd - 3 if b else cd + 1
    return sign * (2.0 ** exp) * (1.0 + frac / 16.0)


def logical_imm(value, reg_bits):
    """(N, immr, imms) for an A64 bitmask immediate, or None."""
    imm = value & ((1 << 64) - 1)
    if imm == 0 or imm == (1 << 64) - 1:
        return None
    if reg_bits != 64:
        if imm >> reg_bits != 0 or imm == ((1 << 64) - 1) >> (64 - reg_bits):
            return None
    size = reg_bits
    while True:
        size //= 2
        mask = (1 << size) - 1
        if (imm & mask) != ((imm >> size) & mask):
            size *= 2
            break
        if size <= 2:
            break
    mask = ((1 << 64) - 1) >> (64 - size)
    imm &= mask

    def trailing_zeros(v):
        return (v & -v).bit_length() - 1 if v else 64

    def trailing_ones(v):
        return trailing_zeros(~v & ((1 << 64) - 1))

    def is_mask(v):
        return v != 0 and ((v + 1) & v) == 0

    def is_shifted_mask(v):
        return v != 0 and is_mask(((v - 1) | v) & ((1 << 64) - 1))

    if is_shifted_mask(imm):
        i = trailing_zeros(imm)
        rotation, run = i, trailing_ones(imm >> i)
    else:
        imm |= ~mask & ((1 << 64) - 1)
        inv = ~imm & ((1 << 64) - 1)
        if not is_shifted_mask(inv):
            return None
        leading_ones = 64 - inv.bit_length()
        rotation = 64 - leading_ones
        run = leading_ones + trailing_ones(imm) - (64 - size)
    immr = (size - rotation) % size
    nimms = ((~(size - 1) << 1) | (run - 1)) & 0x1fff
    n = ((nimms >> 6) & 1) ^ 1
    return n, immr & 0x3f, nimms & 0x3f


def esize_of(kind):
    """The element width a `.b`/`.h`/`.s`/`.d` suffix names."""
    t = {"b": 8, "h": 16, "s": 32, "d": 64, "q": 128}
    if kind[0] in ("z", "zidx", "vidx", "p", "pm", "pz") and len(kind) > 1 and kind[1]:
        return t.get(kind[1])
    if kind[0] == "v":
        m = re.fullmatch(r"(\d+)([bhsdq])", kind[1])
        if m:
            return t[m.group(2)]
    return None


# ---------------------------------------------------------------------------
# Probing
# ---------------------------------------------------------------------------
#
# Each number in an operand is varied on its own and the line reassembled, so
# what the word does with it is measured. llvm-mc is generous with immediates
# it cannot hold — `ext v0.8b, v1.8b, v2.8b, #8` is taken and truncated — so a
# range is never read off the values it accepted: a model is fitted near the
# value the disassembler printed and grown outwards for as long as llvm-mc's
# word keeps agreeing with it.

BYTEMASKS = [0xff << (8 * k) for k in range(8)]
DENSE = 150


def with_val(atoms, ai, vi, v, sp=False):
    """A copy of `atoms` with one number in one operand replaced."""
    out = list(atoms)
    vals = list(out[ai].vals)
    vals[vi] = v
    out[ai] = out[ai].with_vals(vals, sp=sp)
    return out


def is_reg_slot(atom, vi):
    """A register number, a lane index or a keyword: never negative, and every
    value worth trying is tried at once."""
    return (vi == 0 and atom.kind[0] in REG_KINDS) or vi == 1 or \
        atom.kind[0] in ("cond", "pat", "prf")


def candidates(atom, vi, b0, svals):
    """The values worth trying first for one number of one operand."""
    k = atom.kind[0]
    if vi == 1:
        return list(range(64))
    if k in ("p", "pm", "pz", "plist"):
        return list(range(16))
    if k in REG_KINDS:
        return list(range(32))
    if k in ("cond", "pat", "prf"):
        names = {"cond": a64.COND_NAMES, "pat": a64.PAT_NAMES, "prf": a64.PRF_NAMES}[k]
        return sorted(names)
    if k == "fimm":
        return sorted({fp_imm8_value(i) for i in range(256)} | {0.0, 1.0, 2.0})
    out = set(range(b0 - DENSE, b0 + DENSE + 1))
    out.update(range(-DENSE, DENSE + 1))
    for j in range(1, 64):
        for d in (-1, 0, 1):
            out.add((1 << j) + d)
            out.add(-(1 << j) + d)
    out.update(BYTEMASKS)
    for v in set(svals) | {b0}:
        out.update({v, v - 1, v + 1, v - 2, v + 2, -v})
        out.update(v ^ (1 << j) for j in range(25))
        for d in (16, 32, 64, 90, 128):
            out.update(v + d * k for k in range(-16, 17))
        for d in (256, 4096, 65536):
            out.update(v + d * k for k in range(-8, 9))
        if _bytemask(v) is not None:
            out.update(v ^ (0xff << (8 * j)) for j in range(8))
    return sorted(out)


# ---- models ---------------------------------------------------------------
#
# A model gives the word for a value, or None where the value has no encoding.

def nbits(bits):
    """How many value bits a scatter list places."""
    return max(vb for vb, _ in bits) + 1


def scatter_model(bits, base_word, b0):
    def f(v):
        w = base_word
        for vb, wb in bits:
            if ((int(v) ^ int(b0)) >> vb) & 1:
                w ^= 1 << wb
        return w
    return f


def affine_model(base_word, b0, lsb, width, sign, step):
    mask = (1 << width) - 1
    f0 = (base_word >> lsb) & mask

    def f(v):
        if (v - b0) % step:
            return None
        n = (v - b0) // step
        field = (f0 + sign * n) & mask
        return (base_word & ~(mask << lsb) & 0xffffffff) | (field << lsb)
    return f


def xform_model(xf, place, base_word, b0):
    """A model for a value the word holds a function of: the bitmask
    immediate, the 8-bit floating-point immediate, or a byte mask."""
    f0 = xf(b0)
    if f0 is None:
        return None
    if place[0] == "field":
        lsb, width = place[1], place[2]
        mask = (1 << width) - 1

        def f(v):
            x = xf(v)
            if x is None or x > mask:
                return None
            return (base_word & ~(mask << lsb) & 0xffffffff) | (x << lsb)
    else:
        bits = place[1]

        def f(v):
            x = xf(v)
            if x is None or x >> nbits(bits):
                return None
            w = base_word
            for vb, wb in bits:
                if ((x ^ f0) >> vb) & 1:
                    w ^= 1 << wb
            return w
    return f


def bits_from_probes(accepted, b0, base_word, value_of, top, partial=False):
    """Which word bits each bit of a value reaches, if flipping one bit of the
    value flips bits of the word no other value bit does. With `partial`, the
    low bits that behave are enough: the range is grown from them afterwards.
    One value bit may reach several word bits: `mov z0.d, z1.d` is
    `orr z0.d, z1.d, z1.d`."""
    seen = {}
    for v, w in accepted.items():
        x = value_of(v)
        if x is not None and x not in seen:
            seen[x] = w
    x0 = value_of(b0)
    if x0 is None:
        return None
    bits, used, j = [], 0, 0
    while (1 << j) <= top:
        x = x0 ^ (1 << j)
        pair = next((u for u in seen if u >= 0 and u ^ (1 << j) in seen), None)
        if x in seen:
            diff = seen[x] ^ seen.get(x0, base_word)
        elif pair is not None:
            # Any two values that differ in this bit alone: the SVE patterns
            # are named for codes 0-13 and 29-31, so bit 4 is `vl256` against
            # `all`.
            diff = seen[pair] ^ seen[pair ^ (1 << j)]
        else:
            # The top bit of a signed field cannot be flipped from a value
            # near zero without leaving the range: `#-3` with bit 8 flipped is
            # `#-259`. Two values that agree below bit j and differ at it do
            # as well, `#255` and `#-1` for a 9-bit field.
            diff = 0
            for u in range(0, 1 << j):
                if u in seen and u - (1 << j) in seen:
                    diff = seen[u] ^ seen[u - (1 << j)]
                    break
        if diff == 0 or diff & used:
            if partial and bits:
                break
            return None
        used |= diff
        bits.extend((j, wb) for wb in range(32) if diff >> wb & 1)
        j += 1
    return bits or None


def grow(accepted, model, b0, step, lo_limit=None, hi_limit=None, most=None, printed=(),
         skip=()):
    """The run of values around `b0` that llvm-mc encodes as the model says,
    no more than `most` of them.

    llvm-mc takes `scvtf s0, w0, #33` as `#1` and `ext v0.16b, v1.16b, v2.16b,
    #-1` as `#15`, so a field's values repeat and the run can be longer than
    the field holds. The window kept is the one holding most of the values the
    disassembler printed, then one that starts at zero or is centred on it as
    a signed field is, then the one nearest zero: `#0`..`#15` for `ext`,
    `#1`..`#32` for `scvtf`.

    A value in `skip` belongs to a form tried before this one, and neither
    ends the run nor counts against it."""
    def agrees(v):
        return v in accepted and (v in skip or model(v) == accepted[v])
    lo = hi = b0
    v = b0 + step
    while (hi_limit is None or v <= hi_limit) and agrees(v):
        hi = v
        v += step
    v = b0 - step
    while (lo_limit is None or v >= lo_limit) and agrees(v):
        lo = v
        v -= step
    if most is not None and (hi - lo) // step + 1 > most:
        best = None
        # Windows need not hold `b0`: a sample of zero for `scvtf` is itself
        # an alias.
        starts = range(lo, hi - (most - 1) * step + step, step)
        if len(starts) > 4096:
            first = max(lo, b0 - (most - 1) * step)
            starts = range(first, b0 + step, step)
        for start in starts:
            end = start + (most - 1) * step
            if end > hi:
                break
            # Only the values on this form's ladder count either way.
            taken = sum(1 for v in skip if start <= v <= end and (v - start) % step == 0)
            held = sum(1 for p in printed
                       if start <= p <= end and (p - start) % step == 0 and p not in skip)
            aligned = start == 0 or start == -(most // 2) * step
            key = (taken, -held, not aligned, max(abs(start), abs(end)), abs(start))
            if best is None or key < best[0]:
                best = (key, start, end)
        lo, hi = best[1], best[2]
    return lo, hi


def fit_number(atom, vi, b0, base_word, accepted, esize_hint, claimed=None, printed=(),
               forbidden=0):
    """How one number reaches the word, as (encoding, values), or None.

    Every model that fits is measured by how many of the values llvm-mc took
    it explains, and the best wins; `values` lists them for a transform, whose
    domain is not a range. `claimed` holds values an earlier form of the same
    shape encodes, which are no evidence against this one."""
    claimed = claimed or set()
    everything = accepted
    if claimed:
        accepted = {v: w for v, w in accepted.items() if v not in claimed or v == b0}
    if all(w == base_word for w in accepted.values()):
        return ("fixed", b0), None, ()
    kind = atom.kind[0]
    reg = is_reg_slot(atom, vi)
    ints = kind != "fimm" and isinstance(b0, int)
    options = []  # (explained, preference, encoding, values)

    # Which values the disassembler prints back tells a range of values from a
    # range of llvm-mc's aliases for them: `scvtf d0, w0, #0` is taken as
    # `#32`, and measured from there, bit 0 moves five bits of the word. So a
    # range has to be mostly values that are printed back. That only means
    # something in a shape most encodings of which print back at all; an
    # alias spelling (`mov z0.h, #1, lsl #8`, printed `#256`) has few or none.
    words_all = {w for v, w in everything.items() if isinstance(v, int)}
    words_printed = {everything[v] for v in printed if v in everything}
    canon = set(printed) if printed and kind == "imm" and \
        len(words_printed) * 2 >= len(words_all) else None

    def mostly_printed(lo, hi, step):
        if canon is None:
            return True
        n = (hi - lo) // step + 1
        return sum(1 for v in canon if lo <= v <= hi and (v - lo) % step == 0) * 2 > n

    ranged, ranged_all = accepted, everything
    if ints:
        top = max(abs(v) for v in ranged if isinstance(v, int))
        bits = bits_from_probes(ranged, b0, base_word, lambda v: v,
                                min(top, 1 << 24), partial=True)
        if bits:
            n = nbits(bits)
            model = scatter_model(bits, base_word, b0)
            lo, hi = grow(ranged_all, model, b0, 1, 0 if reg else None,
                          (1 << n) - 1, most=1 << n, printed=printed, skip=claimed)
            if kind in ("cond", "pat", "prf"):
                # Only a name reaches the field, so its width is the range.
                lo, hi = 0, (1 << n) - 1
            if hi > lo and mostly_printed(lo, hi, 1):
                places = [w for _, w in bits]
                if [v for v, _ in bits] == list(range(len(bits))) and \
                        places == list(range(places[0], places[0] + len(places))):
                    enc = ("field", places[0], len(places), lo, hi)
                else:
                    enc = ("scatter", bits, lo, hi)
                options.append((hi - lo + 1, 0, enc, None, ()))
        for step in _steps(ranged, b0):
            raws = []
            if b0 + step in ranged:
                raws.append((ranged[b0 + step] - base_word) & 0xffffffff)
            if b0 - step in ranged:
                raws.append((base_word - ranged[b0 - step]) & 0xffffffff)
            for sign, d in [(1, r) for r in raws] + [(-1, (-r) & 0xffffffff) for r in raws]:
                if d == 0 or d & (d - 1):
                    continue
                lsb = d.bit_length() - 1
                for width in range(1, 27 - lsb):
                    if forbidden & (((1 << width) - 1) << lsb):
                        break
                    model = affine_model(base_word, b0, lsb, width, sign, step)
                    lo, hi = grow(ranged_all, model, b0, step, 0 if reg else None,
                                  None, most=1 << width, printed=printed, skip=claimed)
                    count = (hi - lo) // step + 1
                    if count >= 3 and (step == 1 or count >= 8) and \
                            mostly_printed(lo, hi, step):
                        options.append((count, 1 + width / 100.0,
                                        ("affine", lsb, width, sign, step, lo, hi), None, ()))

    if kind in ("imm", "shift") and ints:
        for esize in ([esize_hint] if esize_hint else []) + [64, 32, 16, 8]:
            for name, fn in (("logimm", _logfield), ("notlogimm", _notlogfield)):
                xf = lambda v, e=esize, fn=fn: fn(v, e)
                for place in _placements(accepted, b0, base_word, xf, 13):
                    model = xform_model(xf, place, base_word, b0)
                    score = model and _score(accepted, model)
                    if score:
                        options.append((score[0], 2, (name, place, esize), score[1],
                                        score[2]))
        for place in _placements(accepted, b0, base_word, _bytemask, 8):
            model = xform_model(_bytemask, place, base_word, b0)
            score = model and _score(accepted, model)
            if score:
                options.append((score[0], 2, ("bytemask", place), score[1], score[2]))
    if kind == "fimm":
        for place in _placements(accepted, b0, base_word, fp_imm8, 8):
            model = xform_model(fp_imm8, place, base_word, b0)
            score = model and _score(accepted, model)
            if score:
                options.append((score[0], 2, ("fpimm", place), score[1], score[2]))
    choice = _choice(accepted, b0, base_word)
    if choice and canon is not None and \
            sum(1 for v in choice[1] if v in canon) * 2 <= len(choice[1]):
        choice = None
    if choice:
        options.append((len(choice[1]), 3, choice[0], choice[1], ()))
    if not options:
        return None
    best = max(options, key=lambda o: (o[0], -o[1]))
    return best[2], best[3], best[4]


def _choice(accepted, b0, base_word):
    """A handful of values, each with a code of its own: `#90`/`#270`, or
    `#0.5`/`#1.0`. Only values that move one run of bits are kept."""
    by_word = {}
    for v, w in sorted(accepted.items(), key=lambda kv: (abs(kv[0]), kv[0])):
        by_word.setdefault(w, v)
    usable = {v: w for w, v in by_word.items()}
    usable[b0] = base_word
    diffs = 0
    for w in usable.values():
        diffs |= w ^ base_word
    if not diffs or len(usable) > 16:
        return None
    lsb = (diffs & -diffs).bit_length() - 1
    width = diffs.bit_length() - lsb
    if width > 4:
        return None
    mask = (1 << width) - 1
    rest = base_word & ~(mask << lsb) & 0xffffffff
    codes = {}
    for v, w in sorted(usable.items()):
        if w & ~(mask << lsb) & 0xffffffff != rest:
            return None
        code = (w >> lsb) & mask
        if code in codes.values():
            continue
        codes[v] = code
    if len(codes) < 2:
        return None
    return ("choice", lsb, width, sorted(codes.items())), sorted(codes)


def _steps(accepted, b0):
    """Distances to neighbouring accepted values, to measure one step of an
    immediate with: the usual powers, and whatever is nearest."""
    if not isinstance(b0, int):
        return []
    out = set()
    for d in (1, 2, 4, 8, 16, 32, 64, 128, 256):
        if b0 + d in accepted or b0 - d in accepted:
            out.add(d)
    return sorted(out)


def _placements(accepted, b0, base_word, xf, width):
    """Where a transformed value might sit: one run of bits, or scattered."""
    out = []
    xs = {}
    for v, w in accepted.items():
        x = xf(v)
        if x is not None and x not in xs:
            xs[x] = (v, w)
    if len(xs) < 3 or xf(b0) is None:
        return out
    for lsb in range(0, 25):
        rest = base_word & ~(((1 << width) - 1) << lsb) & 0xffffffff
        good = sum(1 for x, (v, w) in xs.items() if (rest | (x << lsb)) == w)
        if good * 3 >= len(xs) * 2:
            out.append(("field", lsb, width))
            break
    bits = bits_from_probes(accepted, b0, base_word, xf, (1 << width) - 1)
    if bits:
        out.append(("scatter", bits))
    return out


def _score(accepted, model):
    """(explained, values, rejects) if the model gives llvm-mc's word for at
    least two in three of the values it can hold, and for three at least.
    The rest have to be explained by other forms, which `fit_job` sees to. A value it
    cannot hold at all is another form's: `fmov s0, #0.0` is `fmov s0, wzr`
    to llvm-mc."""
    good, bad = [], []
    for v, w in accepted.items():
        m = model(v)
        if m is None:
            continue
        if m == w:
            good.append(v)
        else:
            bad.append(v)
    if len(good) < 3 or len(bad) * 2 > len(good):
        return None
    return len(good), sorted(good), sorted(bad, key=abs)


# ---------------------------------------------------------------------------
# Fitting one form
# ---------------------------------------------------------------------------

class Fail(Exception):
    pass


def baseline_of(atoms):
    out = []
    for a in atoms:
        vals = list(a.vals)
        if a.kind[0] in REG_KINDS:
            vals = [0] * len(vals)
        out.append(a.with_vals(vals, sp=False))
    return out


def _set_bits(word, bits, x):
    for vb, wb in bits:
        word = (word & ~(1 << wb)) | (((x >> vb) & 1) << wb)
    return word & 0xffffffff


def _set_field(word, lsb, width, x):
    mask = (1 << width) - 1
    return ((word & ~(mask << lsb)) | ((x & mask) << lsb)) & 0xffffffff


def _place(word, place, x):
    if place[0] == "field":
        return _set_field(word, place[1], place[2], x)
    return _set_bits(word, place[1], x)


XFORMS = {"fpimm": lambda v, enc: fp_imm8(v),
          "logimm": lambda v, enc: _logfield(v, enc[2]),
          "notlogimm": lambda v, enc: _notlogfield(v, enc[2]),
          "bytemask": lambda v, enc: _bytemask(v)}


def contribution(enc, v, word):
    """One number's part of the word, as the Rust encoder computes it: its
    bits are cleared and set, never added to."""
    t = enc[0]
    if t in ("fixed", "tied"):
        return word
    if t == "field":
        return _set_field(word, enc[1], enc[2], int(v))
    if t == "scatter":
        return _set_bits(word, enc[1], int(v))
    if t == "affine":
        _, lsb, width, sign, step, vmin, _vmax = enc
        mask = (1 << width) - 1
        f = (((word >> lsb) & mask) + sign * ((v - vmin) // step)) & mask
        return _set_field(word, lsb, width, f)
    if t in XFORMS:
        return _place(word, enc[1], XFORMS[t](v, enc))
    if t == "choice":
        return _set_field(word, enc[1], enc[2], dict(enc[3])[v])
    raise ValueError(enc)


def encode_form(slots, base, vals, wrap=0):
    w = base
    for s, v in zip(slots, vals):
        w = contribution(s["enc"], wrapped(s["enc"], v, wrap), w)
    return w & 0xffffffff


def measure_wrap(mn, atoms, slots, base, sp, rng):
    """The element width a form's signed immediates wrap at, if llvm-mc
    agrees they do: each is tried as the element's bits, and the form wraps
    only if every such line encodes as the negative number does."""
    e = esize_of(atoms[0].kind) if atoms else None
    signed = [i for i, sl in enumerate(slots)
              if sl.get("kind") == "imm" and enc_range(sl["enc"]) and enc_range(sl["enc"])[0] < 0]
    if e not in (8, 16, 32) or not signed:
        return 0, []
    dirs, kept = 0, []
    # Each way is measured on its own: llvm-mc may take a number above the
    # range as the element's bits and refuse one below it.
    for bit, sign in ((1, 1), (2, -1)):
        cases = []
        for i in signed:
            lo, hi, step = enc_range(slots[i]["enc"])
            if sign > 0:
                pool = list(range(lo, min(hi, -1) + 1, step))
            else:
                # Not zero: `#0` less the element's size is the size itself
                # negated, which is one past what a form takes either way.
                pool = [v for v in range(lo + ((-lo + step - 1) // step) * step, hi + 1, step)
                        if v > 0]
            if not pool:
                continue
            for v in {pool[0], pool[-1], rng.choice(pool)}:
                vals = pick_values(slots, rng)
                vals[i] = v
                alias = list(vals)
                alias[i] = v + sign * (1 << e)
                cases.append((vals, alias))
        if not cases:
            continue
        lines = [render(mn, apply_values(atoms, slots, alias, sp)) for _, alias in cases]
        got = assemble(lines)
        if all(w == encode_form(slots, base, vals) for (vals, _), w in zip(cases, got)):
            dirs |= bit
            kept.extend(zip(lines, got))
    return ((e, dirs) if dirs else 0), kept


def enc_bits(enc):
    """The word bits an encoding writes."""
    t = enc[0]
    if t == "field" or t == "choice":
        return ((1 << enc[2]) - 1) << enc[1]
    if t == "scatter":
        return sum(1 << wb for _, wb in set(enc[1]))
    if t == "affine":
        return ((1 << enc[2]) - 1) << enc[1]
    if t in XFORMS:
        place = enc[1]
        if place[0] == "field":
            return ((1 << place[2]) - 1) << place[1]
        return sum(1 << wb for _, wb in set(place[1]))
    return 0


def enc_range(enc):
    """(lo, hi, step) for an encoding whose values are a range."""
    t = enc[0]
    if t == "field":
        return enc[3], enc[4], 1
    if t == "scatter":
        return enc[2], enc[3], 1
    if t == "affine":
        return enc[5], enc[6], enc[4]
    return None


def fit_form(mn, samples, rng, earlier=()):
    """Measures one form, or raises Fail with the reason.

    Returns (atoms, slots, base, sp) where `slots` has one entry per number in
    the operands, in order, and `sp` says which register operands spell 31 as
    the stack pointer. `earlier` holds the slots of forms already fitted for
    the same printed shape.
    """
    atoms0 = samples[0][1]
    slot_ids = [(ai, vi) for ai, a in enumerate(atoms0) for vi in range(len(a.vals))]
    real = [smp for smp in samples if smp[0] is not None] or samples
    svals = {s: sorted({smp[1][s[0]].vals[s[1]] for smp in real}) for s in slot_ids}

    # A baseline the assembler accepts: registers zeroed, everything else as
    # one of the samples had it.
    tries = [baseline_of(a) for _, a in samples] + [list(a) for _, a in samples]
    words = assemble([render(mn, t) for t in tries])
    base_atoms = base_word = None
    for t, w in zip(tries, words):
        if w is not None:
            base_atoms, base_word = t, w
            break
    if base_atoms is None:
        raise Fail("no baseline llvm-mc accepts")

    tried = {s: {base_atoms[s[0]].vals[s[1]]} for s in slot_ids}
    accepted = {s: {base_atoms[s[0]].vals[s[1]]: base_word} for s in slot_ids}
    sp = set()

    def probe(values_for, atoms_for=None):
        lines, index = [], []
        for s, vs in values_for.items():
            ai, vi = s
            for v in vs:
                if isinstance(v, tuple):  # (31, "sp")
                    lines.append(render(mn, with_val(base_atoms, ai, vi, v[0], sp=True)))
                else:
                    tried[s].add(v)
                    lines.append(render(mn, atoms_for(s, v) if atoms_for
                                        else with_val(base_atoms, ai, vi, v)))
                index.append((s, v))
        return zip(index, assemble(lines))

    first = {}
    for s in slot_ids:
        ai, vi = s
        b0 = base_atoms[ai].vals[vi]
        vs = [v for v in candidates(base_atoms[ai], vi, b0, svals[s]) if v != b0]
        if base_atoms[ai].kind[0] == "g" and vi == 0:
            vs.append((31, "sp"))
        first[s] = vs
    sp_words = {}
    for (s, v), w in probe(first):
        if w is None:
            continue
        if isinstance(v, tuple):
            sp_words[s] = w
        else:
            accepted[s][v] = w
    for s, w in sp_words.items():
        # A field that takes the stack pointer; where llvm-mc also takes the
        # zero register's name for the same word, both spellings are kept.
        if 31 not in accepted[s]:
            accepted[s][31] = w
            sp.add(s)
        elif accepted[s][31] == w:
            sp.add((s, "both"))

    esize_hint = esize_of(atoms0[0].kind) if atoms0 else None

    def claimed_for(s):
        """The values of one number an earlier form encodes as llvm-mc does,
        with every other operand as the probes had it."""
        out = set()
        if is_reg_slot(base_atoms[s[0]], s[1]):
            return out
        ai, vi = s
        for prior in earlier:
            for v, w in accepted[s].items():
                probe = with_val(base_atoms, ai, vi, v)
                if covers(prior["slots"], probe, prior["wrap"]):
                    vals = [probe[sl["slot"][0]].vals[sl["slot"][1]] for sl in prior["slots"]]
                    if encode_form(prior["slots"], prior["base"], vals, prior["wrap"]) == w:
                        out.add(v)
        return out

    # Which of the values llvm-mc took are the ones its disassembler prints
    # back: `scvtf s0, w0, #0` is taken, but encodes `#32`. A window of values
    # is chosen to hold as many of these as it can.
    canonical = {s: set() for s in slot_ids}
    checked = {s: set() for s in slot_ids}

    def canon(s):
        ai, vi = s
        todo = [v for v in accepted[s] if v not in checked[s] and isinstance(v, int)]
        checked[s].update(todo)
        words = sorted({accepted[s][v] for v in todo})
        if not words:
            return canonical[s]
        printed_back = {}
        try:
            for w, text in disassemble(words):
                try:
                    pmn, patoms = parse_line(text)
                except ValueError:
                    continue
                if pmn == mn and [a.kind for a in patoms] == [a.kind for a in atoms0]:
                    printed_back[w] = patoms[ai].vals[vi]
        except RuntimeError:
            return canonical[s]
        canonical[s].update(v for v in todo if printed_back.get(accepted[s][v]) == v)
        return canonical[s]

    fits = {}
    for s in slot_ids:
        ai, vi = s
        b0 = base_atoms[ai].vals[vi]
        claimed = claimed_for(s)
        reg_like = is_reg_slot(base_atoms[ai], vi)
        printed = () if reg_like or base_atoms[ai].kind[0] == "fimm" else canon(s)
        r = fit_number(base_atoms[ai], vi, b0, base_word, accepted[s], esize_hint, claimed,
                       printed)
        # A range that stops where the probing stopped may go further.
        for _ in range(8):
            rng_ = r and enc_range(r[0])
            if not rng_ or is_reg_slot(base_atoms[ai], vi):
                break
            lo, hi, step = rng_
            more = []
            if hi + step not in tried[s]:
                more.extend(hi + step * k for k in range(1, 1025))
            if lo - step not in tried[s]:
                more.extend(lo - step * k for k in range(1, 1025))
            more = [v for v in more if v not in tried[s] and abs(v) < (1 << 40)]
            if not more:
                break
            for (s2, v), w in probe({s: more}):
                if w is not None:
                    accepted[s2][v] = w
            printed = canon(s)
            r2 = fit_number(base_atoms[ai], vi, b0, base_word, accepted[s], esize_hint,
                            claimed, printed)
            if r2 == r:
                break
            r = r2
        fits[s] = r

    # An affine field is measured by how far it counts, which says nothing
    # about its top bits: `asr z0.d, p0/m, z0.d, #33` moves bits 5-9 only, and
    # bit 10 is the governing predicate's. Fit them again kept out of every
    # other operand's bits.
    for s in slot_ids:
        r = fits[s]
        if not r or r[0][0] != "affine":
            continue
        forbidden = 0
        for o in slot_ids:
            if o != s and fits[o]:
                forbidden |= enc_bits(fits[o][0])
        if forbidden & enc_bits(r[0]):
            ai, vi = s
            b0 = base_atoms[ai].vals[vi]
            fits[s] = fit_number(base_atoms[ai], vi, b0, base_word, accepted[s], esize_hint,
                                 claimed_for(s), canonical[s], forbidden)

    slots = []
    for s in slot_ids:
        ai, vi = s
        r = fits[s]
        slots.append({"slot": s, "enc": r[0] if r else ("unfit",),
                      "kind": base_atoms[ai].kind[0],
                      "base": base_atoms[ai].vals[vi],
                      "values": r[1] if r and r[1] is not None else sorted(accepted[s]),
                      "rejects": list(r[2]) if r else [],
                      "reject_words": [(v, accepted[s][v]) for v in (r[2] if r else [])]})

    # Two registers that will not move on their own may have to move
    # together: the destructive forms write their first operand and read it
    # again, so `add z0.b, p0/m, z0.b, z1.b` needs the same register twice.
    for i, sl in enumerate(slots):
        if sl["enc"][0] != "fixed" or not is_reg_slot(atoms0[sl["slot"][0]], sl["slot"][1]):
            continue
        for j in range(i + 1, len(slots)):
            other = slots[j]
            si, sj = sl["slot"], other["slot"]
            if other["enc"][0] != "fixed" or atoms0[si[0]].kind[0] != atoms0[sj[0]].kind[0] \
                    or si[1] != sj[1] or sl["base"] != other["base"]:
                continue
            top = 16 if atoms0[si[0]].kind[0] in ("p", "pm", "pz", "plist") else 32

            def both(_s, v, si=si, sj=sj):
                return with_val(with_val(base_atoms, si[0], si[1], v), sj[0], sj[1], v)
            together = {sl["base"]: base_word}
            for (_, v), w in probe({si: [v for v in range(top) if v != sl["base"]]}, both):
                if w is not None:
                    together[v] = w
            r = fit_number(base_atoms[si[0]], si[1], sl["base"], base_word, together,
                           esize_hint)
            if r and r[0][0] in ("field", "scatter"):
                sl["enc"], sl["values"] = r[0], sorted(together)
                other["enc"] = ("tied", i)
                break

    bad = [sl for sl in slots if sl["enc"][0] == "unfit"]
    if bad:
        vals = bad[0]["values"]
        raise Fail("operand %d does not fit: %d values, %r..%r" %
                   (bad[0]["slot"][0], len(vals), vals[:4], vals[-2:]))
    for sl in slots:
        s = sl["slot"]
        # A register that will not move is one this fit failed to tie; an
        # immediate that will not move is a narrow form of its own, like
        # `fmov d0, #0.0`, which is `fmov d0, xzr`.
        if sl["enc"][0] == "fixed" and len(svals[s]) > 1 and \
                is_reg_slot(base_atoms[s[0]], s[1]):
            raise Fail("operand %d varies in the samples but not on its own" % s[0])

    # The opcode with every operand at its origin: zero, or the bottom of an
    # affine range.
    base = base_word
    for sl in slots:
        enc = sl["enc"]
        t = enc[0]
        if t in ("fixed", "tied"):
            continue
        if t in ("field", "scatter"):
            base = contribution(enc, 0, base)
        elif t == "affine":
            _, lsb, width, sign, step, vmin, _vmax = enc
            mask = (1 << width) - 1
            f0 = (base_word >> lsb) & mask
            fmin = (f0 + sign * ((vmin - sl["base"]) // step)) & mask
            base = _set_field(base, lsb, width, fmin)
        elif t == "choice":
            base = _set_field(base, enc[1], enc[2], 0)
        else:
            base = _place(base, enc[1], 0)
    return base_atoms, slots, base, sp


def _logfield(v, esize):
    """The 13-bit `N:immr:imms` of a bitmask immediate for an element of
    `esize` bits. A negative value that fits the element signed is taken as
    its low `esize` bits, as both references take `and z0.b, z0.b, #-2`.
    llvm-mc goes further, and takes any number modulo the element size, but a
    value only that reading makes encodable is refused rather than modelled."""
    if not isinstance(v, int):
        return None
    if esize < 64:
        if not -(1 << (esize - 1)) <= v < (1 << esize):
            return None
        v &= (1 << esize) - 1
    elif not -(1 << 63) <= v < (1 << 64):
        return None
    r = logical_imm(v, esize)
    if r is None:
        return None
    n, immr, imms = r
    return (n << 12) | (immr << 6) | imms


def _notlogfield(v, esize):
    """The bitmask immediate of the complement of `v` in an element of
    `esize` bits: `bic z0.b, z0.b, #0xf0` is `and z0.b, z0.b, #0x0f`."""
    if not isinstance(v, int):
        return None
    if esize < 64:
        if not -(1 << (esize - 1)) <= v < (1 << esize):
            return None
    elif not -(1 << 63) <= v < (1 << 64):
        return None
    return _logfield(~v & ((1 << esize) - 1), esize)


def _bytemask(v):
    if v is None:
        return None
    out = 0
    for k in range(8):
        b = (v >> (8 * k)) & 0xff
        if b == 0xff:
            out |= 1 << k
        elif b != 0:
            return None
    return out


# ---------------------------------------------------------------------------
# Checking a fitted form, and writing it out
# ---------------------------------------------------------------------------

def pick_values(slots, rng, extreme=None):
    """One value per number, inside what llvm-mc accepted."""
    vals = []
    for sl in slots:
        enc = sl["enc"]
        t = enc[0]
        if t == "fixed":
            v = sl["base"]
        elif t == "tied":
            v = vals[enc[1]]
        elif sl.get("kind") in ("cond", "pat", "prf"):
            vs = sl["values"]
            v = vs[-1] if extreme == "max" else vs[0] if extreme == "min" else rng.choice(vs)
        elif t in ("field", "scatter"):
            lo, hi, _ = enc_range(enc)
            v = hi if extreme == "max" else lo if extreme == "min" else rng.randrange(lo, hi + 1)
        elif t == "affine":
            _, _lsb, _w, _sign, step, vmin, vmax = enc
            n = (vmax - vmin) // step
            k = n if extreme == "max" else 0 if extreme == "min" else rng.randrange(n + 1)
            v = vmin + k * step
        elif t == "choice":
            vs = [x for x, _ in enc[3]]
            v = vs[-1] if extreme == "max" else vs[0] if extreme == "min" else rng.choice(vs)
        else:
            vs = [x for x in sl["values"] if XFORMS[t](x, enc) is not None]
            v = vs[-1] if extreme == "max" else vs[0] if extreme == "min" else rng.choice(vs)
        vals.append(v)
    return vals


def apply_values(atoms, slots, vals, sp_at_31):
    out = list(atoms)
    for sl, v in zip(slots, vals):
        ai, vi = sl["slot"]
        sp = sl["slot"] in sp_at_31 and v == 31
        nv = list(out[ai].vals)
        nv[vi] = v
        out[ai] = out[ai].with_vals(nv, sp=sp)
    return out


def check_form(mn, atoms, slots, base, sp_at_31, rng, rounds=10, earlier=()):
    """Assembles random operand values and compares with the fitted form.
    Returns the lines checked, or raises Fail. Values a form fitted before
    this one encodes are the earlier form's, and are not tried."""
    cases = [pick_values(slots, rng, "min"), pick_values(slots, rng, "max")]
    for _ in range(rounds):
        cases.append(pick_values(slots, rng))

    def claimed(vals):
        probe = apply_values(atoms, slots, vals, sp_at_31)
        return any(covers(prior["slots"], probe, prior["wrap"]) for prior in earlier)
    cases = [v for v in cases if not claimed(v)]
    lines = [render(mn, apply_values(atoms, slots, v, sp_at_31)) for v in cases]
    got = assemble(lines)
    for vals, line, w in zip(cases, lines, got):
        want = encode_form(slots, base, vals)
        if w != want:
            raise Fail("%s: llvm-mc says %s, the fit says %08x" %
                       (line.replace("\t", " "), "no" if w is None else "%08x" % w, want))
    return list(zip(lines, got))


# ---------------------------------------------------------------------------
# Emitting the Rust table
# ---------------------------------------------------------------------------

ARR_CONST = {"8b": "A_8B", "16b": "A_16B", "4h": "A_4H", "8h": "A_8H",
             "2s": "A_2S", "4s": "A_4S", "1d": "A_1D", "2d": "A_2D",
             "1q": "A_1Q", "2q": "A_2Q", "4b": "A_4B", "2h": "A_2H",
             "2b": "A_2B"}
ELEM_CONST = {"b": "E_B", "h": "E_H", "s": "E_S", "d": "E_D", "q": "E_Q",
              "": "E_NONE"}
EXT_CONST = {"uxtw": "X_UXTW", "sxtw": "X_SXTW", "uxtx": "X_UXTX",
             "sxtx": "X_SXTX", "lsl": "X_LSL"}
SHIFT_CONST = {"lsl": "SH_LSL", "msl": "SH_MSL", "mul": "SH_MUL"}


def kind_rust(kind, spelling):
    t = kind[0]
    if t == "v":
        return "K::Vec(%s)" % ARR_CONST[kind[1]]
    if t == "vidx":
        return "K::VecIdx(%s)" % ELEM_CONST[kind[1]]
    if t == "vidxa":
        return "K::VecIdxArr(%s)" % ARR_CONST[kind[1]]
    if t == "s":
        return "K::Scalar(%s)" % ELEM_CONST[kind[1]]
    if t == "g":
        return "K::Gpr(%s, %s)" % ("G_X" if kind[1] == "x" else "G_W", spelling)
    if t == "z":
        return "K::Z(%s)" % ELEM_CONST[kind[1]]
    if t == "zidx":
        return "K::ZIdx(%s)" % ELEM_CONST[kind[1]]
    if t in ("p", "pm", "pz"):
        mode = {"p": "P_PLAIN", "pm": "P_MERGE", "pz": "P_ZERO"}[t]
        return "K::P(%s, %s)" % (ELEM_CONST[kind[1]], mode)
    if t == "vlist":
        return "K::VecList(%d, %s)" % (kind[1], ARR_CONST[kind[2]])
    if t == "vlistidx":
        return "K::VecListIdx(%d, %s)" % (kind[1], ELEM_CONST[kind[2]])
    if t == "zlist":
        return "K::ZList(%d, %s)" % (kind[1], ELEM_CONST[kind[2]])
    if t == "plist":
        return "K::PList(%d, %s)" % (kind[1], ELEM_CONST[kind[2]])
    if t == "imm":
        return "K::Imm"
    if t == "fimm":
        return "K::FImm"
    if t == "cond":
        return "K::Cond"
    if t == "pat":
        return "K::Pat"
    if t == "prf":
        return "K::Prf"
    if t == "shift":
        return "K::Shift(%s)" % SHIFT_CONST[kind[1]]
    if t == "mulvl":
        return "K::MulVl"
    if t == "open":
        return "K::Open"
    if t == "close":
        return "K::Close"
    if t == "close_wb":
        return "K::CloseWb"
    if t == "ext":
        return "K::Ext(%s)" % EXT_CONST[kind[1]]
    if t == "exta":
        return "K::ExtAmt(%s)" % EXT_CONST[kind[1]]
    raise ValueError(kind)


def bits_rust(bits):
    return "&[%s]" % ", ".join("(%d, %d)" % b for b in bits)


def xf_rust(enc):
    t = enc[0]
    if t == "fpimm":
        return "X::FpImm"
    if t == "logimm":
        return "X::LogImm(%d)" % enc[2]
    if t == "notlogimm":
        return "X::NotLogImm(%d)" % enc[2]
    return "X::ByteMask"


def enc_rust(enc):
    t = enc[0]
    if t == "fixed":
        return "E::Fixed(%d)" % enc[1]
    if t == "field":
        return "E::Field { lsb: %d, width: %d, min: %d, max: %d }" % (enc[1], enc[2], enc[3],
                                                                      enc[4])
    if t == "scatter":
        return "E::Scatter { min: %d, max: %d, bits: %s }" % (enc[2], enc[3], bits_rust(enc[1]))
    if t == "affine":
        _, lsb, width, sign, step, vmin, vmax = enc
        return ("E::Affine { lsb: %d, width: %d, sign: %d, step: %d, min: %d, max: %d }"
                % (lsb, width, sign, step, vmin, vmax))
    if t in XFORMS:
        place = enc[1]
        if place[0] == "field":
            return "E::Xform(%s, %d, %d)" % (xf_rust(enc), place[1], place[2])
        return "E::XformBits(%s, %s)" % (xf_rust(enc), bits_rust(place[1]))
    if t == "tied":
        return "E::Tied(%d)" % enc[1]
    if t == "choice":
        if any(isinstance(v, float) for v, _ in enc[3]):
            return "E::FChoice { lsb: %d, width: %d, map: &[%s] }" % (
                enc[1], enc[2], ", ".join("(%r, %d)" % (float(v), c) for v, c in enc[3]))
        return "E::Choice { lsb: %d, width: %d, map: &[%s] }" % (
            enc[1], enc[2], ", ".join("(%d, %d)" % (v, c) for v, c in enc[3]))
    raise ValueError(enc)


HEADER = """\
//! The AArch64 SIMD, floating-point and SVE encoding table.
//!
//! Generated by `tools/tables/aarch64.py` from llvm-mc; do not edit. Each
//! row is one instruction form: a mnemonic, the operands it takes, and the
//! opcode left when every operand is zero. The generator measured each field
//! by assembling the form with one operand changed at a time, and checked
//! every row against llvm-mc before writing it here.
//!
//! %d forms, %d operand shapes, %d distinct operands.
#![allow(clippy::unreadable_literal)]

use super::table::{Enc as E, Form, Kind as K, Slot, Xf as X};
%s
"""


def emit_rust(forms, path):
    slot_pool, shape_pool = {}, {}
    mnems = sorted({f["mn"] for f in forms})
    mnem_index = {m: i for i, m in enumerate(mnems)}
    rows = []
    for f in forms:
        sids = []
        for ai, kind in enumerate(f["kinds"]):
            encs = [sl for sl in f["slots"] if sl["slot"][0] == ai]
            a = enc_rust(encs[0]["enc"]) if encs else "E::None"
            b = enc_rust(encs[1]["enc"]) if len(encs) > 1 else "E::None"
            spelling = "S_ANY" if ((ai, 0), "both") in f["sp"] else \
                "S_SP" if (ai, 0) in f["sp"] else "S_ZR"
            text = "Slot { kind: %s, a: %s, b: %s }" % (kind_rust(kind, spelling), a, b)
            sids.append(slot_pool.setdefault(text, len(slot_pool)))
        shape = shape_pool.setdefault(tuple(sids), len(shape_pool))
        wrap = f.get("wrap") or (0, 0)
        rows.append((mnem_index[f["mn"]], shape, f["base"], wrap[0], wrap[1]))
    rows.sort(key=lambda r: r[0])  # stable: the forms of a mnemonic keep their order

    slots = sorted(slot_pool, key=slot_pool.get)
    shapes = sorted(shape_pool, key=shape_pool.get)
    used = sorted(set(re.findall(r"\b(?:A|E|G|P|S|SH|X)_[A-Z0-9]+\b", "".join(slots))))
    imports = []
    line = "use super::table::{"
    for name in used:
        if len(line) + len(name) + 3 > 96:
            imports.append(line.rstrip(", ") + "};")
            line = "use super::table::{"
        line += name + ", "
    imports.append(line.rstrip(", ") + "};")
    out = [HEADER % (len(rows), len(shapes), len(slots), "\n".join(imports))]
    out.append("\npub static MNEMONICS: &[&str] = &[\n")
    line = "   "
    for m in mnems:
        if len(line) + len(m) + 4 > 96:
            out.append(line + "\n")
            line = "   "
        line += ' "%s",' % m
    out.append(line + "\n];\n")
    out.append("\npub static SLOTS: &[Slot] = &[\n")
    for s in slots:
        out.append("    %s,\n" % s)
    out.append("];\n")
    out.append("\npub static SHAPES: &[&[u16]] = &[\n")
    for sh in shapes:
        out.append("    &[%s],\n" % ", ".join(str(x) for x in sh))
    out.append("];\n")
    out.append("\n/// `(mnemonic, shape, opcode, wrap, directions)`, sorted by mnemonic.\n")
    out.append("pub static FORMS: &[Form] = &[\n")
    for mn, sh, base, wrap, dirs in rows:
        out.append("    Form(%d, %d, 0x%08x, %d, %d),\n" % (mn, sh, base, wrap, dirs))
    out.append("];\n")
    with open(path, "w") as fh:
        fh.write("".join(out))
    return len(rows), len(shapes), len(slots)


def emit_codes(path, pat_codes, prf_codes):
    """The name tables the generator measured: SVE patterns and prefetch ops."""
    out = ["""\
//! Names whose encoding the generator measured: the SVE element-count
//! patterns and the SVE prefetch operations.
//!
//! Generated by `tools/tables/aarch64.py`; do not edit.

/// `pow2`, `vl1` … `all`, as `cntb x0, <pattern>` encodes them.
pub static PATTERNS: &[(&str, u8)] = &[
"""]
    for name, code in sorted(pat_codes.items(), key=lambda kv: kv[1]):
        out.append('    ("%s", %d),\n' % (name, code))
    out.append("""];

/// `pldl1keep` … `pstl3strm`, as `prfb <op>, p0, [x0]` encodes them.
pub static PREFETCHES: &[(&str, u8)] = &[
""")
    for name, code in sorted(prf_codes.items(), key=lambda kv: kv[1]):
        out.append('    ("%s", %d),\n' % (name, code))
    out.append("];\n")
    with open(path, "w") as fh:
        fh.write("".join(out))


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------

def alias_groups():
    """The spellings in ALIASES, as groups the fitter can work on."""
    out = {}
    lines = expand_aliases(ALIASES)
    words = assemble(lines)
    for line, w in zip(lines, words):
        if w is None:
            continue
        try:
            mn, atoms = parse_line(line)
        except ValueError as e:
            print("alias not parsed: %s (%s)" % (line, e), file=sys.stderr)
            continue
        out.setdefault(shape_key(mn, atoms), []).append((w, atoms))
    return out


def prefix_job(args):
    """Every value of the top `bits` bits of a word, each with a few fills of
    the rest: an instance of every class whose opcode bits live above them."""
    lo, hi, seed, bits = args
    rng = random.Random(seed)
    res = Reservoir(seed)
    low = 32 - bits
    mask = (1 << low) - 1
    for start in range(lo, hi, 4096):
        words = []
        for p in range(start, min(start + 4096, hi)):
            if bits == 22:
                # Low bits of zero make every register operand `0`, which
                # nearly every form takes; bits 5-9 set are the SVE `all`
                # pattern, which `sqincw z0.s` needs; random ones reach the
                # rest.
                fills = (0, 0x3e0, 0x3ff, rng.getrandbits(low), rng.getrandbits(low))
            else:
                fills = [rng.getrandbits(low) for _ in range(64)]
            words.extend((p << low) | (f & mask) for f in fills)
        try:
            res.add_all(sweep(words))
        except RuntimeError as e:
            print("sweep: %s" % e, file=sys.stderr)
    return res.found, res.seen


def merge(into, part, rng=random.Random(0)):
    """Adds one job's reservoir to the whole sweep's, keeping the choice of
    samples uniform over everything either saw."""
    found, seen = part
    for k, v in found.items():
        cur = into.setdefault(k, ([], 0))
        pool, count = cur
        total = count + seen[k]
        merged = pool + v
        if len(merged) > SAMPLES:
            # Each side's samples stand for as many words as it saw.
            weights = [count / max(len(pool), 1)] * len(pool) + \
                [seen[k] / max(len(v), 1)] * len(v)
            chosen = set()
            while len(chosen) < SAMPLES:
                chosen.add(rng.choices(range(len(merged)), weights)[0])
            merged = [merged[i] for i in sorted(chosen)]
        into[k] = (merged, total)


def wrapped(enc, v, wrap):
    """`v` as a signed range reads it. Both references take a number of an
    SVE element's width as that element's bits: `mov z0.h, #0xfff0` is
    `#-16`, and `mov z0.b, #-241` is `#15`. A form `wrap` is set for does the
    same."""
    if isinstance(v, int) and (1 << 63) <= v < (1 << 64):
        # The backend reads a number as a 64-bit integer.
        v -= 1 << 64
    r = enc_range(enc)
    bits, dirs = wrap if wrap else (0, 0)
    if bits and r and r[0] < 0 and isinstance(v, int):
        if dirs & 1 and (1 << (bits - 1)) <= v < (1 << bits) and v > r[1]:
            return v - (1 << bits)
        if dirs & 2 and -(1 << bits) < v < -(1 << (bits - 1)) and v < r[0]:
            return v + (1 << bits)
    return v


def covers_value(enc, v, fixed, wrap=0):
    """True if one number's encoding holds the value `v`."""
    v = wrapped(enc, v, wrap)
    t = enc[0]
    if t == "fixed":
        return fixed is None or v == fixed
    if t == "tied":
        return True
    if not isinstance(v, (int, float)):
        return False
    if t in ("field", "scatter"):
        lo, hi, _ = enc_range(enc)
        return isinstance(v, int) and lo <= v <= hi
    if t == "affine":
        return isinstance(v, int) and enc[5] <= v <= enc[6] and (v - enc[5]) % enc[4] == 0
    if t == "choice":
        return v in dict(enc[3])
    return XFORMS[t](v, enc) is not None


def covers(slots, atoms, wrap=0):
    """True if a fitted form encodes these operand values: they are in its
    ranges, and none is a value its transform was seen to get wrong."""
    for sl in slots:
        ai, vi = sl["slot"]
        v = atoms[ai].vals[vi]
        if not covers_value(sl["enc"], v, sl["base"], wrap) or v in sl.get("rejects", ()):
            return False
    return True


def fit_job(args):
    """Fits every form behind one printed shape.

    The disassembler can print two encodings alike: `add z0.d, z0.d, #256` is
    the shifted form of the immediate and `#255` the plain one, and
    `mov z0.s, #imm` is `dup` or `dupm` by value. So a shape is fitted from its
    sample with the smallest numbers, then again from the smallest one that
    fit does not encode, and so on; each fit leaves out the values the ones
    before it claimed. A transform fitted over values another form encodes
    hands those on to be fitted in turn, and the shape fails if any of them is
    left over, since the transform would encode it wrongly."""
    key, samples, seed = args
    mn, kinds = key
    rng = random.Random(seed)
    a64.ensure_codes()

    def size(smp):
        return sum(abs(v) for a in smp[1] if a.kind[0] not in REG_KINDS for v in a.vals)

    # Zero and one in every immediate, as well as what was printed, so that a
    # shape whose narrow form the sweep happened not to print still has it
    # fitted first. These need not be encodable, or covered.
    queue = []
    for fill in (0, 1):
        atoms = [a.with_vals([fill if a.kind[0] == "imm" else v for v in a.vals])
                 for a in samples[0][1]]
        queue.append((None, atoms))
    real = sorted(samples, key=size)
    queue += real
    pending = []  # (|value|, atoms) a tolerated transform got wrong
    from_rejects = []
    earlier, out, why = [], [], []
    retried = set()

    def covered(atoms):
        return any(covers(prior["slots"], atoms, prior["wrap"]) for prior in earlier)

    for _ in range(32):
        queue = [smp for smp in queue if not covered(smp[1])]
        if not queue:
            pending = [p for p in pending if not covered(p[1])]
            if not pending:
                break
            pending.sort(key=lambda p: p[0])
            queue.append((None, pending.pop(0)[1]))
            from_rejects.append(queue[-1])
        anchor = queue[0]
        order = [anchor] + [smp for smp in queue[1:] + real
                            if smp[0] is not None and smp is not anchor]
        try:
            atoms, slots, base, sp = fit_form(mn, order, rng, earlier)
            lines = check_form(mn, atoms, slots, base, sp, rng, earlier=earlier)
            # A form has to encode something the disassembler printed, or a
            # value an earlier transform was seen to get wrong. A fit from the
            # zero or one filled in, alone, can be of values llvm-mc only takes
            # because it truncates them: `ucvtf d0, x0, #67`.
            explains = any(covers(slots, smp[1]) and not covered(smp[1]) for smp in real)
            if not explains and not (anchor in from_rejects and covers(slots, anchor[1])):
                raise Fail("the fit encodes no sample of its own")
        except Exception as e:  # noqa: BLE001 - a generator, not a library
            why.append(str(e) if isinstance(e, Fail) else "%s: %s" % (type(e).__name__, e))
            queue = queue[1:]
            # A transform can fail to fit while the narrower forms that share
            # its values are unfitted, so a sample gets a second turn after
            # the others.
            if anchor[0] is not None and id(anchor) not in retried:
                retried.add(id(anchor))
                queue.append(anchor)
            continue
        wrap, wrap_lines = measure_wrap(mn, atoms, slots, base, sp, rng)
        out.append(("ok", {
            "mn": mn, "kinds": kinds, "slots": slots, "base": base, "sp": sp,
            "lines": lines[:3] + wrap_lines[:1] + lines[3:], "order": len(out),
            "atoms": atoms, "wrap": wrap,
        }))
        earlier.append({"slots": slots, "base": base, "wrap": wrap})
        for sl in slots:
            ai, vi = sl["slot"]
            for v in sl["rejects"]:
                pending.append((abs(v), with_val(atoms, ai, vi, v)))
    left = [smp for smp in real if not covered(smp[1])]
    wrong = [p for p in pending if not covered(p[1])]
    if left or wrong or not out:
        if wrong:
            reason = "a transform encodes %s wrongly" % render(mn, wrong[0][1]).replace("\t", " ")
        else:
            reason = (why or ["samples left over"])[0]
        out.append(("fail", mn, kinds, reason))
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("command", nargs="?", default="table", choices=("table", "check", "fit"),
                    help="write the table and corpora, check they are up to date, "
                         "or only fit and report")
    ap.add_argument("--words", type=int, default=8000000,
                    help="random words to disassemble, on top of the prefix sweep")
    ap.add_argument("--jobs", type=int, default=min(32, os.cpu_count() or 8))
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--no-prefix-sweep", action="store_true")
    ap.add_argument("--only", default=None, help="only mnemonics matching this regex")
    ap.add_argument("--out", default=os.path.join(ROOT, "src", "arch", "aarch64"))
    ap.add_argument("--corpus", default=os.path.join(ROOT, "tools", "mc-diff"),
                    help="directory for the aarch64-*-words.txt corpora")
    ap.add_argument("--groups", default=None,
                    help="keep the swept shapes in this file, and read them from it next time")
    ap.add_argument("--dump", default=None, help="write the fitted forms, readably, here")
    ap.add_argument("--failures", default=None, help="write the forms that did not fit here")
    args = ap.parse_args()
    written = None
    if args.command == "check":
        written = tempfile.mkdtemp(prefix="rsasm-aarch64-table")
        args.out = args.corpus = written

    a64.ensure_codes()
    pool = multiprocessing.Pool(args.jobs)

    groups = {}
    loaded = False
    if args.groups and os.path.exists(args.groups):
        loaded = True
        with open(args.groups, "rb") as fh:
            groups = pickle.load(fh)
        args.no_prefix_sweep, args.words = True, 0
        print("%d forms from %s" % (len(groups), args.groups))
    if not args.no_prefix_sweep:
        for bits in (22, 16):
            span = 1 << bits
            chunk = span // CHUNKS + 1
            jobs = [(i, min(i + chunk, span), args.seed + i, bits)
                    for i in range(0, span, chunk)]
            for part in pool.imap(prefix_job, jobs):
                merge(groups, part)
            print("sweep of the top %d bits: %d forms" % (bits, len(groups)))
    if args.words:
        per = args.words // CHUNKS + 1
        for part in pool.imap(sweep_job,
                              [(args.seed * 1000 + i, per) for i in range(CHUNKS)]):
            merge(groups, part)
        print("with random words: %d forms" % len(groups))
    if not loaded:
        # A form whose opcode bits the sweeps rarely hit is usually one bit
        # away from one they did: `uqincw z0.s, #25` from `uqincw z0.s, #25,
        # mul #2`. So every bit of a few words of each shape is flipped, until
        # that finds nothing new.
        for round_ in range(4):
            words = []
            for k, (samples, _) in sorted(groups.items()):
                for w, _atoms in samples[:4]:
                    words.extend(w ^ (1 << b) for b in range(32))
            before = len(groups)
            chunk = len(words) // CHUNKS + 1
            jobs = [(words[i:i + chunk], args.seed + round_) for i in range(0, len(words), chunk)]
            for part in pool.imap(words_job, jobs):
                merge(groups, part)
            print("neighbours, round %d: %d forms" % (round_ + 1, len(groups)))
            if len(groups) == before:
                break
        simd = {k[0] for k, (samples, _) in groups.items()
                if {t[0] for t in k[1]} & SIMD_KINDS or any(sve_space(w) for w, _ in samples)}
        groups = {k: v for k, v in groups.items() if k[0] in simd}
        print("of mnemonics with a SIMD form: %d forms" % len(groups))
        groups = {k: v[0] for k, v in groups.items()}
    if args.groups and not os.path.exists(args.groups):
        with open(args.groups, "wb") as fh:
            pickle.dump(groups, fh)
    for k, v in alias_groups().items():
        cur = groups.setdefault(k, [])
        seen = {smp[0] for smp in cur}
        cur.extend(smp for smp in v if smp[0] not in seen)
    if args.only:
        pat = re.compile(args.only)
        groups = {k: v for k, v in groups.items() if pat.search(k[0])}
    print("%d candidate forms" % len(groups))

    jobs = [(k, v, args.seed + i) for i, (k, v) in enumerate(sorted(groups.items()))]
    forms, failures = [], []
    for rs in pool.imap(fit_job, jobs, chunksize=4):
        for r in rs:
            if r[0] == "ok":
                forms.append(r[1])
            else:
                failures.append(r[1:])
    pool.close()
    print("fitted %d, failed %d" % (len(forms), len(failures)))
    counts = collections.Counter(f[2].split(":")[0] for f in failures)
    for reason, n in counts.most_common():
        print("  %4d  %s" % (n, reason))
    if args.failures:
        with open(args.failures, "w") as fh:
            for mn, kinds, why in sorted(failures):
                fh.write("%-12s %-60s %s\n" % (mn, " ".join(str(k) for k in kinds), why))

    forms = resolve(forms)
    if args.command == "fit":
        if args.dump:
            dump_forms(forms, args.dump)
        return 1 if failures else 0
    if args.dump:
        dump_forms(forms, args.dump)
    n = emit_rust(forms, os.path.join(args.out, "table_data.rs"))
    emit_codes(os.path.join(args.out, "table_names.rs"), a64.PAT_CODES, a64.PRF_CODES)
    # As `cargo fmt` would leave them, so that a regenerated table is a diff
    # of what changed.
    subprocess.run(["rustfmt", "--edition", "2024",
                    os.path.join(args.out, "table_data.rs"),
                    os.path.join(args.out, "table_names.rs")], check=True)
    print("wrote %d forms, %d shapes, %d operands" % n)
    write_corpus(forms, args.corpus)
    if args.command == "check":
        return compare(written)
    return 1 if failures else 0


def dump_forms(forms, path):
    """The fitted forms, readably, for looking into what was measured."""
    with open(path, "w") as fh:
        for f in forms:
            fh.write("%s %s  base=%08x\n" %
                     (f["mn"], " ".join(":".join(map(str, k)) for k in f["kinds"]), f["base"]))
            for sl in f["slots"]:
                fh.write("    %s %s\n" % (sl["slot"], sl["enc"]))
            fh.write("    e.g. %s\n" % f["lines"][0][0].replace("\t", " "))


def compare(written):
    """0 if what was just written matches what is committed, 1 otherwise."""
    stale = []
    for name, committed in (("table_data.rs", os.path.join(ROOT, "src", "arch", "aarch64")),
                            ("table_names.rs", os.path.join(ROOT, "src", "arch", "aarch64")),
                            ("aarch64-simd-words.txt", os.path.join(ROOT, "tools", "mc-diff")),
                            ("aarch64-sve-words.txt", os.path.join(ROOT, "tools", "mc-diff"))):
        want = os.path.join(written, name)
        have = os.path.join(committed, name)
        if not os.path.exists(have) or open(have).read() != open(want).read():
            stale.append(os.path.relpath(have, ROOT))
    for name in stale:
        print("out of date: %s" % name)
    if not stale:
        print("up to date")
    return 1 if stale else 0


def domain(slots):
    """How many values a form's immediates take, to try narrow forms first."""
    n = 1
    for sl in slots:
        enc = sl["enc"]
        t = enc[0]
        if t in ("field", "scatter", "affine"):
            lo, hi, step = enc_range(enc)
            if sl["slot"][1] == 0 and t != "affine" and lo == 0 and hi in (15, 30, 31):
                continue  # a register
            n *= (hi - lo) // step + 1
        elif t == "choice":
            n *= len(enc[3])
        elif t in ("logimm", "notlogimm"):
            n *= 1 << 16
        elif t in ("fpimm", "bytemask"):
            n *= 256
    return n


def fits_i64(f):
    for sl in f["slots"]:
        r = enc_range(sl["enc"])
        nums = list(r[:2]) if r else []
        if sl["enc"][0] == "fixed" and isinstance(sl["enc"][1], int):
            nums.append(sl["enc"][1])
        if sl["enc"][0] == "choice":
            nums.extend(v for v, _ in sl["enc"][3])
        if any(not -(1 << 63) <= v < (1 << 63) for v in nums):
            print("dropped, a number beyond 64 bits: %s" % f["lines"][0][0].replace("\t", " "))
            return False
    return True


def resolve(forms):
    """Drops repeats, orders the forms of a shape narrow first, and checks
    that trying them in that order gives llvm-mc's word for every line the
    fitter assembled, as the backend will try them."""
    seen = {}
    out = []
    forms = [f for f in forms if fits_i64(f)]
    for f in sorted(forms, key=lambda f: (f["mn"], str(f["kinds"]))):
        sig = (f["mn"], f["kinds"], tuple(str(sl["enc"]) for sl in f["slots"]), f["base"])
        if sig in seen:
            continue
        seen[sig] = True
        out.append(f)
    out.sort(key=lambda f: (f["mn"], str(f["kinds"]), domain(f["slots"]), f["order"]))
    by = collections.defaultdict(list)
    for f in out:
        by[(f["mn"], f["kinds"])].append(f)
    wrong = 0

    def matched(fs, atoms, word, text):
        for g in fs:
            if covers(g["slots"], atoms, g.get("wrap", 0)):
                vals = [atoms[sl["slot"][0]].vals[sl["slot"][1]] for sl in g["slots"]]
                got = encode_form(g["slots"], g["base"], vals, g.get("wrap", 0))
                if got != word:
                    print("order: %s gives %08x, llvm-mc %08x" % (text, got, word))
                    return 1
                return 0
        print("order: %s is encoded by no form, llvm-mc %08x" % (text, word))
        return 1

    for key, fs in by.items():
        for f in fs:
            if len(fs) > 1 or f.get("wrap"):
                for line, word in f["lines"]:
                    mn, atoms = parse_line(line)
                    wrong += matched(fs, atoms, word, line.replace("\t", " "))
            # The values a tolerated transform gets wrong must reach another
            # form first.
            for sl in f["slots"]:
                ai, vi = sl["slot"]
                for v, word in sl.get("reject_words", ()):
                    atoms = with_val(f["atoms"], ai, vi, v)
                    wrong += matched(fs, atoms, word, render(f["mn"], atoms).replace("\t", " "))
    print("%d shapes with more than one form, %d lines in the wrong one" %
          (sum(1 for fs in by.values() if len(fs) > 1), wrong))
    return out


def write_corpus(forms, directory):
    """Three lines of each form, split into the SVE forms and the rest."""
    files = {"sve": set(), "simd": set()}
    for f in forms:
        # A `movprfx` must be followed by the instruction it prefixes, which
        # a corpus of lone lines cannot give it; aarch64-programs.txt has one.
        if f["mn"] == "movprfx":
            continue
        for line, _ in f["lines"][:3]:
            line = line.replace("\t", " ").strip()
            sve = re.search(r"\b[zp]\d+\b|\b[zp]\d+[./]", line)
            files["sve" if sve else "simd"].add(line)
    about = {
        "simd": "AdvSIMD (NEON), floating point and the cryptographic extensions",
        "sve": "SVE and SVE2",
    }
    for name, lines in files.items():
        path = os.path.join(directory, "aarch64-%s-words.txt" % name)
        with open(path, "w") as fh:
            fh.write("""\
# AArch64 %s: every form of every instruction, at the ends of each operand's
# range and somewhere inside it.
#
# Generated by tools/tables/aarch64.py, which took each line from a run of
# llvm-mc while measuring the encoding table; regenerate rather than edit. One
# instruction per line, compared a batch at a time by run.sh.
""" % about[name])
            for l in sorted(lines):
                fh.write(l + "\n")
        print("wrote %d lines to %s" % (len(lines), path))


if __name__ == "__main__":
    sys.exit(main() or 0)
