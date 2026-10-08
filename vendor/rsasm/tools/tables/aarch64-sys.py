#!/usr/bin/env python3
"""Derives rsasm's AArch64 system tables from GNU as.

`src/arch/aarch64/sysreg_data.rs` — the `mrs`/`msr` register names, the
PSTATE fields, the `dc`/`ic`/`at`/`tlbi` operand names and the aliases of
`hint` — is not written by hand. The names come from binutils' own tables
(`opcodes/aarch64-sys-regs.def` and the `aarch64_sys_regs_*` arrays in
`opcodes/aarch64-opc.c`), and every encoding comes from assembling that name
with `aarch64-elf-as`: nothing here says what a word should be.

    tools/tables/aarch64-sys.py table   # rewrite the table and the corpora
    tools/tables/aarch64-sys.py check   # exit 1 if either is out of date

`table` also writes the corpora: a line per name goes to
tools/mc-diff/aarch64-sys-words.txt when llvm-mc assembles it to the same
word as GNU as, and to tools/xas-diff/aarch64.txt when it does not — the
names GNU as alone knows, which is most of the newer ones.

Environment: RSASM_ORACLES (default target/oracles), GAS (default
aarch64-elf-as from there), LLVM_MC (default llvm-mc).
"""

import argparse
import os
import re
import struct
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)

import a64  # noqa: E402

ORACLES = os.environ.get("RSASM_ORACLES", os.path.join(ROOT, "target", "oracles"))
BINUTILS = os.path.join(ORACLES, "src", "binutils-2.47")
GAS = os.environ.get("GAS") or os.path.join(ORACLES, "bin", "aarch64-elf-as")
OBJCOPY = GAS.replace("-as", "-objcopy")
TABLE = os.path.join(ROOT, "src", "arch", "aarch64", "sysreg_data.rs")
MC_CORPUS = os.path.join(ROOT, "tools", "mc-diff", "aarch64-sys-words.txt")
GAS_CORPUS = os.path.join(ROOT, "tools", "xas-diff", "aarch64.txt")

# Every optional feature GNU as has a name for, so that no name in its tables
# is turned away for the CPU the corpus was measured on. The corpora are
# assembled with the same string: tools/xas-diff/run.sh carries a copy.
FEATURES = """
crc crypto fp lse lsfe lse128 lsui simd pan lor ras rdma fp16 fp16fml fprcvt
profile sve tme fcma jscvt rcpc rcpc2 dotprod sha2 frintts sb predres
predres2 poe2 tev aes sm4 sha3 rng ssbs lscp memtag occmo cmpbr sve2 sve2-sm4
sve2-aes sve2-sha3 sve2-bitperm sme sme-f64f64 sme-i16i64 sme2 bf16 i8mm
f32mm f64mm ls64 flagm flagm2 pauth xs wfxt mops hbc cssc chk gcs the rasv2
ite d128 sve-b16b16 sve-bfscale sme2p1 sve2p1 sve-f16f32mm f8f32mm f8f16mm
sve-aes sve-aes2 ssve-aes sve-bitperm ssve-bitperm rcpc3 cpa faminmax fp8 lut
brbe sme-lutv2 fp8fma fp8dot4 fp8dot2 ssve-fp8fma ssve-fp8dot4 ssve-fp8dot2
sme-f8f32 sme-f8f16 sme-f16f16 sme-b16b16 pops sve2p2 sme2p2 gcie ssve-fexpa
sme-tmop sme-mop4 mops-go sve2p3 sme2p3 f16f32dot f16f32mm f16mm sve-b16mm
mtetc tlbid sme-fa64
"""
MARCH = "-march=armv9.5-a+" + "+".join(FEATURES.split())

# The `mrs`/`msr` and `sys` words a measured encoding is taken apart against.
MRS = 0xD5300000
MSR = 0xD5100000
SYS = 0xD5080000

# What a name may not do, as GNU as says it.
F_READ = 1  # cannot be written to
F_WRITE = 2  # cannot be read from
F_DEPRECATED = 4


# ===========================================================================
# Assembling
# ===========================================================================


def _gas(lines):
    """One run: (errors by line number, warnings by line number, words)."""
    with tempfile.TemporaryDirectory() as d:
        src, obj, binp = (os.path.join(d, n) for n in ("in.s", "in.o", "in.bin"))
        open(src, "w").write("".join("\t" + line + "\n" for line in lines))
        p = subprocess.run([GAS] + MARCH.split() + ["-o", obj, src],
                           capture_output=True, text=True)
        errors, warnings = {}, {}
        for line in p.stderr.splitlines():
            m = re.match(r"^[^:]*:(\d+): (Error|Warning): (.*)$", line)
            if m:
                where = errors if m.group(2) == "Error" else warnings
                where.setdefault(int(m.group(1)), m.group(3))
        if errors or p.returncode != 0:
            return errors or {1: p.stderr.strip()[:200]}, warnings, None
        subprocess.run([OBJCOPY, "-O", "binary", "--only-section=.text", obj, binp],
                       capture_output=True, check=True)
        data = open(binp, "rb").read()
        return {}, warnings, [w[0] for w in struct.iter_unpack("<I", data)]


def gas(lines):
    """Assembles each line on its own terms: (word, warning) for each, with a
    word of None for a line GNU as refuses.

    GNU as refuses a whole file for one bad line, so the lines it names are
    dropped and the rest run again."""
    out = [None] * len(lines)
    keep = list(range(len(lines)))
    for _ in range(16):
        errors, warnings, words = _gas([lines[i] for i in keep])
        if words is not None:
            if len(words) != len(keep):
                raise SystemExit("a line assembled to other than one word")
            for n, (i, word) in enumerate(zip(keep, words), start=1):
                out[i] = (word, warnings.get(n))
            return out
        dropped = {keep[n - 1] for n in errors if 1 <= n <= len(keep)}
        if not dropped:
            raise SystemExit("GNU as: " + "; ".join(errors.values())[:200])
        for i in dropped:
            out[i] = (None, None)
        keep = [i for i in keep if i not in dropped]
    raise SystemExit("GNU as kept failing")


def llvm(lines):
    """The word llvm-mc makes of each line, or None where it refuses it."""
    out = []
    for i in range(0, len(lines), 500):
        out += a64.assemble(lines[i:i + 500])
    return out


# ===========================================================================
# The names, from binutils' tables
# ===========================================================================


def _body(text, decl):
    """The braces of a table declaration."""
    start = text.index(decl)
    start = text.index("{", start)
    return text[start:text.index("\n};", start)]


def binutils_names():
    """Every name GNU as has for a system register, a PSTATE field, a
    `dc`/`ic`/`at`/`tlbi` operand and a `hint` alias."""
    opc = open(os.path.join(BINUTILS, "opcodes", "aarch64-opc.c")).read()
    tbl = open(os.path.join(BINUTILS, "opcodes", "aarch64-tbl.h")).read()
    defs = open(os.path.join(BINUTILS, "opcodes", "aarch64-sys-regs.def")).read()

    regs = re.findall(r'SYSREG\s*\(\s*"([^"]+)"', defs)
    pstate = re.findall(r'^\s*\{\s*"([a-z0-9_]+)",', _body(opc, "aarch64_pstatefields ["),
                        re.M)

    # The sys-instruction operands. A TLBI_XS_OP or PLBI_XS_OP line defines
    # the name twice, the second with `nxs` on the end.
    ins = {}
    for mnemonic, table in (("at", "aarch64_sys_regs_at"), ("dc", "aarch64_sys_regs_dc"),
                            ("ic", "aarch64_sys_regs_ic"), ("tlbi", "aarch64_sys_regs_tlbi"),
                            ("plbi", "aarch64_sys_regs_plbi"), ("cfp", "aarch64_sys_regs_sr"),
                            ("dvp", "aarch64_sys_regs_sr"), ("cpp", "aarch64_sys_regs_sr"),
                            ("cosp", "aarch64_sys_regs_sr")):
        names = []
        for line in _body(opc, table + "[] =").splitlines():
            if line.lstrip().startswith("#"):
                continue
            m = re.search(r'\(\s*"([a-z0-9_]+)"', line) or re.search(r'"([a-z0-9_]+)"', line)
            if not m or m.group(1) == "nxs":  # the macro's own suffix, not a name
                continue
            names.append(m.group(1))
            if "_XS_OP (" in line or "_XS_OP(" in line:
                names.append(m.group(1) + "nxs")
        ins[mnemonic] = names

    # The hint space: every mnemonic binutils gives a word of the `hint`
    # encoding, the barriers among them. `smstart`, `smstop` and the two that
    # take a register are encoded by hand instead.
    hints = []
    for name in re.findall(r'INSN\s*\(\s*"([a-z0-9]+)",\s*0xd503[0-9a-f]{4}', tbl):
        if name not in hints and name not in ("smstart", "smstop", "wfet", "wfit"):
            hints.append(name)
    # What such an alias may take as its one operand: the hint options and
    # the barrier names, `dsb`'s nXS variants included.
    options = [name for name in re.findall(r'^\s*\{\s*"([a-z0-9]*)",', _body(
        opc, "aarch64_hint_options[]"), re.M) if name]
    options += [name for name in re.findall(r'^\s*\{\s*"([a-z0-9#x]+)",', _body(
        opc, "aarch64_barrier_options[16]"), re.M) if not name.startswith("#")]
    options += re.findall(r'^\s*\{\s*"([a-z0-9]+)",', _body(
        opc, "aarch64_barrier_dsb_nxs_options[4]"), re.M)
    # `chkfeat` names a register rather than an option, but it names only the
    # one, so it is a name like the others here.
    options.append("x16")
    return regs, pstate, ins, hints, options


# ===========================================================================
# Measuring
# ===========================================================================


def measure_regs(names):
    """(name, encoding bits, flags) for each register GNU as knows."""
    reads = gas([f"mrs x0, {n}" for n in names])
    writes = gas([f"msr {n}, x0" for n in names])
    out = []
    for name, (rword, rwarn), (wword, wwarn) in zip(names, reads, writes):
        if rword is None and wword is None:
            continue
        flags = 0
        bits = None
        for word, warn, base, flag in ((rword, rwarn, MRS, F_WRITE),
                                       (wword, wwarn, MSR, F_READ)):
            if word is None:
                flags |= flag
                continue
            if warn and "deprecated" in warn:
                flags |= F_DEPRECATED
            elif warn:
                flags |= flag
            bits = word & ~base & ~0x1F
        out.append((name, bits, flags))
    return out


def measure_pstate(names):
    """(name, the word of `msr <field>, #0`, where the immediate goes, how
    large it may be) for each PSTATE field."""
    out = []
    zero = gas([f"msr {n}, #0" for n in names])
    for name, (word, _) in zip(names, zero):
        if word is None:
            continue
        # The immediate's bits are the ones it moves; its range is how far
        # GNU as follows it.
        ones = gas([f"msr {name}, #{v}" for v in (1, 15)])
        moved = 0
        for value, (other, _) in zip((1, 15), ones):
            if other is not None:
                moved |= other ^ word
        if moved == 0:
            continue
        lsb = (moved & -moved).bit_length() - 1
        top = max(v for v in (0, 1, 15) if gas([f"msr {name}, #{v}"])[0][0] is not None)
        out.append((name, word, lsb, top))
    return out


def measure_ins(ins):
    """(mnemonic, name, the word with no register in it, how the register is
    taken) for each `dc`/`ic`/`at`/`tlbi` operand name."""
    lines, keys = [], []
    for mnemonic, names in ins.items():
        for name in names:
            keys.append((mnemonic, name))
            lines.append(f"{mnemonic} {name}, x0")
            lines.append(f"{mnemonic} {name}")
    words = gas(lines)
    out = []
    for i, (mnemonic, name) in enumerate(keys):
        withr, without = words[2 * i][0], words[2 * i + 1][0]
        if withr is None and without is None:
            continue
        if withr is not None and without is not None and withr & ~0x1F != without & ~0x1F:
            raise SystemExit(f"{mnemonic} {name}: the register changes more than Rt")
        word = (withr if withr is not None else without) & ~0x1F
        takes = (2 if withr is not None and without is not None
                 else 1 if withr is not None else 0)
        out.append((mnemonic, name, word, takes))
    return out


def measure_sys():
    """The `sys` and `sysl` words with every field zero, and a check that the
    four fields sit where the encoder puts them."""
    lines = ["sys #0, c0, c0, #0, x0", "sysl x0, #0, c0, c0, #0",
             "sys #7, c15, c15, #7, x0", "sysl x0, #7, c15, c15, #7"]
    words = [w for w, _ in gas(lines)]
    if any(w is None for w in words):
        raise SystemExit("GNU as refused `sys`")
    ones = (7 << 16) | (15 << 12) | (15 << 8) | (7 << 5)
    if words[2] != words[0] | ones or words[3] != words[1] | ones:
        raise SystemExit("`sys` does not hold its fields where they were expected")
    return words[0], words[1]


def measure_hints(names, options):
    """(mnemonic, operand name or None, word) for each alias of `hint` GNU as
    assembles on its own or with one named operand."""
    lines, keys = [], []
    for name in names:
        for option in [None] + options:
            keys.append((name, option))
            lines.append(name if option is None else f"{name} {option}")
    words = gas(lines)
    return sorted((name, option or "", word)
                  for (name, option), (word, _) in zip(keys, words) if word is not None)


# ===========================================================================
# Writing
# ===========================================================================


def rust(regs, pstate, ins, hints, sys_words):
    def rows(items):
        return "".join("    " + row + "\n" for row in items)

    return f'''//! The system-register and system-instruction names GNU as knows.
//!
//! Generated by `tools/tables/aarch64-sys.py`, which takes the names from
//! binutils' own tables and every encoding from a run of `aarch64-elf-as`;
//! do not edit. The names are sorted, and looked up by binary search.

/// The register cannot be written to; `msr` warns, as GNU as does.
pub(super) const READ_ONLY: u8 = {F_READ};
/// The register cannot be read from.
pub(super) const WRITE_ONLY: u8 = {F_WRITE};
/// The name is deprecated and may be removed from the architecture.
pub(super) const DEPRECATED: u8 = {F_DEPRECATED};

/// `mrs`/`msr` register names: the `o0:op1:CRn:CRm:op2` bits of the word,
/// already in place, and what the register allows.
pub(super) static REGS: &[(&str, u32, u8)] = &[
{rows(f'("{n}", {b:#010x}, {f}),' for n, b, f in regs)}];

/// PSTATE fields, which `msr` writes with an immediate: the word of
/// `msr <field>, #0`, the bit the immediate starts at and its largest value.
pub(super) static PSTATE: &[(&str, u32, u8, u8)] = &[
{rows(f'("{n}", {w:#010x}, {lsb}, {top}),' for n, w, lsb, top in pstate)}];

/// The register operand takes no `Xt`.
pub(super) const NO_XT: u8 = 0;
/// It must have one.
pub(super) const NEEDS_XT: u8 = 1;
/// It may have one.
pub(super) const OPTIONAL_XT: u8 = 2;

/// `sys #0, c0, c0, #0, x0` and `sysl x0, #0, c0, c0, #0`: the words the
/// four fields and the register are written into.
pub(super) const SYS: u32 = {sys_words[0]:#010x};
pub(super) const SYSL: u32 = {sys_words[1]:#010x};

/// The operand names of `at`, `dc`, `ic`, `tlbi` and friends: the mnemonic,
/// the name, the `sys` word it stands for with no register in it, and
/// whether a register follows it.
pub(super) static SYS_INS: &[(&str, &str, u32, u8)] = &[
{rows(f'("{m}", "{n}", {w:#010x}, {x}),' for m, n, w, x in ins)}];

/// The aliases of `hint` and of the barriers that take no operand or one
/// named operand: the mnemonic, the name if there is one, and the word.
pub(super) static HINTS: &[(&str, &str, u32)] = &[
{rows(f'("{m}", "{o}", {w:#010x}),' for m, o, w in hints)}];
'''


def corpus_lines(regs, pstate, ins, hints):
    """A line per name, in the spelling both assemblers read."""
    lines = []
    for name, _, flags in regs:
        if not flags & F_WRITE:
            lines.append(f"mrs x0, {name}")
        if not flags & F_READ:
            lines.append(f"msr {name}, x1")
    for name, _, _, top in pstate:
        lines += [f"msr {name}, #0", f"msr {name}, #{top}"]
    for mnemonic, name, _, takes in ins:
        if takes != 1:
            lines.append(f"{mnemonic} {name}")
        if takes != 0:
            lines += [f"{mnemonic} {name}, x0", f"{mnemonic} {name}, x30"]
    for mnemonic, option, _ in hints:
        lines.append(mnemonic if not option else f"{mnemonic} {option}")
    # The raw `sys` and `sysl`, each field at both ends of its range.
    lines += ["sys #0, c0, c0, #0, x0", "sys #7, c15, c15, #7, x30",
              "sys #3, c7, c4, #1", "sys #0, c7, c8, #0, x5",
              "sysl x0, #0, c0, c0, #0", "sysl x30, #7, c15, c15, #7",
              "sysl x7, #3, c7, c4, #1"]
    return lines


def split_corpus(lines):
    """The lines llvm-mc assembles as GNU as does, and the rest."""
    theirs, ours = gas(lines), llvm(lines)
    shared, gas_only = [], []
    for line, (word, _), other in zip(lines, theirs, ours):
        if word is None:
            continue
        (shared if word == other else gas_only).append(line)
    return shared, gas_only


MC_HEADER = """\
# AArch64 system registers, PSTATE fields, `dc`/`ic`/`at`/`tlbi` operands and
# the aliases of `hint`: every name in GNU as's tables that llvm-mc assembles
# to the same word, with and without a register where both are allowed.
#
# Generated by tools/tables/aarch64-sys.py; regenerate rather than edit. One
# instruction per line, compared a batch at a time by run.sh.
"""

GAS_HEADER = """\
# AArch64 system registers and system-instruction operands GNU as knows and
# llvm-mc does not, or encodes differently: most of the newer ones, and the
# nXS TLB maintenance names.
#
# Generated by tools/tables/aarch64-sys.py; regenerate rather than edit.
"""


def write(path, text, check):
    old = open(path).read() if os.path.exists(path) else None
    if old == text:
        return True
    if check:
        print(f"out of date: {os.path.relpath(path, ROOT)}")
        return False
    open(path, "w").write(text)
    print(f"wrote {os.path.relpath(path, ROOT)}")
    return True


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("what", choices=["table", "check"])
    args = ap.parse_args()
    check = args.what == "check"

    names, pstate_names, ins_names, hint_names, options = binutils_names()
    regs = measure_regs(sorted(set(names)))
    pstate = measure_pstate(pstate_names)
    ins = sorted(measure_ins(ins_names))
    hints = measure_hints(hint_names, options)
    sys_words = measure_sys()
    print(f"{len(regs)} registers, {len(pstate)} PSTATE fields, "
          f"{len(ins)} system-instruction operands, {len(hints)} hint aliases")

    text = rust(regs, pstate, ins, hints, sys_words)
    with tempfile.TemporaryDirectory() as d:
        src = os.path.join(d, "sysreg_data.rs")
        open(src, "w").write(text)
        subprocess.run(["rustfmt", "--edition", "2024", src], check=True)
        text = open(src).read()

    shared, gas_only = split_corpus(corpus_lines(regs, pstate, ins, hints))
    print(f"{len(shared)} corpus lines both assemblers agree on, "
          f"{len(gas_only)} GNU as alone")
    ok = write(TABLE, text, check)
    ok &= write(MC_CORPUS, MC_HEADER + "".join(l + "\n" for l in shared), check)
    ok &= write(GAS_CORPUS, GAS_HEADER + "".join(l + "\n" for l in gas_only), check)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
