#!/usr/bin/env python3
"""Regenerate src/arch/m68k/table.rs from GNU binutils.

The 680x0 family has about a thousand instruction forms once the FPU, the
MMU and ColdFire are counted, and the only complete, checkable statement of
them is the table GNU as itself assembles from. So rsasm's table is derived
from it rather than retyped: this script reads

    opcodes/m68k-opc.c     the opcode forms and the mnemonic aliases
    gas/config/m68k-parse.h    the register numbering the tables refer to
    gas/config/tc-m68k.c   the CPU models and their control registers

out of a binutils source tree and writes the Rust table.  The encoder that
reads it (src/arch/m68k/generic.rs) follows tc-m68k.c's own reading of the
`args` strings, which include/opcode/m68k.h documents.

    tools/tables/m68k.py table     # rewrite src/arch/m68k/table.rs
    tools/tables/m68k.py check     # exit 1 if it is out of date

The tree is the one tools/oracles/build.sh unpacks (RSASM_ORACLES, default
target/oracles).  Forms for the ColdFire MAC and EMAC units are left out;
rsasm does not implement them.
"""

import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
ORACLES = os.environ.get("RSASM_ORACLES", os.path.join(ROOT, "target", "oracles"))
BINUTILS = os.path.join(ORACLES, "src", "binutils-2.47")
TABLE = os.path.join(ROOT, "src", "arch", "m68k", "table.rs")

# include/opcode/m68k.h.
ARCH_BITS = {
    "_m68k_undef": 0,
    "m68000": 0x001,
    "m68010": 0x002,
    "m68020": 0x004,
    "m68030": 0x008,
    "m68040": 0x010,
    "m68060": 0x020,
    "m68881": 0x040,
    "m68851": 0x080,
    "cpu32": 0x100,
    "fido_a": 0x200,
    "mcfmac": 0x400,
    "mcfemac": 0x800,
    "cfloat": 0x1000,
    "mcfhwdiv": 0x2000,
    "mcfisa_a": 0x4000,
    "mcfisa_aa": 0x8000,
    "mcfisa_b": 0x10000,
    "mcfisa_c": 0x20000,
    "mcfusp": 0x40000,
}
ARCH_BITS["m68040up"] = ARCH_BITS["m68040"] | ARCH_BITS["m68060"]
ARCH_BITS["m68030up"] = ARCH_BITS["m68030"] | ARCH_BITS["m68040up"]
ARCH_BITS["m68020up"] = ARCH_BITS["m68020"] | ARCH_BITS["m68030up"]
ARCH_BITS["m68010up"] = (
    ARCH_BITS["m68010"] | ARCH_BITS["cpu32"] | ARCH_BITS["fido_a"] | ARCH_BITS["m68020up"]
)
ARCH_BITS["m68000up"] = ARCH_BITS["m68000"] | ARCH_BITS["m68010up"]
ARCH_BITS["mfloat"] = ARCH_BITS["m68881"] | ARCH_BITS["m68040"] | ARCH_BITS["m68060"]
ARCH_BITS["mmmu"] = (
    ARCH_BITS["m68851"] | ARCH_BITS["m68030"] | ARCH_BITS["m68040"] | ARCH_BITS["m68060"]
)

MAC = ARCH_BITS["mcfmac"] | ARCH_BITS["mcfemac"]

# The `#define`s m68k-opc.c makes and unmakes around the cache instructions.
SCOPE = {"SCOPE_LINE": 0x1 << 3, "SCOPE_PAGE": 0x2 << 3, "SCOPE_ALL": 0x3 << 3}

# tc-m68k.c's `case 'J'` switch: the 12-bit MOVEC code of each control
# register.  Registers that share a code are different names for one thing on
# different CPUs, and the CPU's own list decides which of them it takes.
MOVEC_CODES = {
    "SFC": 0x000, "DFC": 0x001, "CACR": 0x002, "TC": 0x003, "ASID": 0x003,
    "ACR0": 0x004, "ITT0": 0x004, "ACR1": 0x005, "ITT1": 0x005,
    "ACR2": 0x006, "DTT0": 0x006, "ACR3": 0x007, "DTT1": 0x007,
    "BUSCR": 0x008, "MMUBAR": 0x008, "RGPIOBAR": 0x009,
    "ACR4": 0x00C, "ACR5": 0x00D, "ACR6": 0x00E, "ACR7": 0x00F,
    "USP": 0x800, "VBR": 0x801, "CAAR": 0x802, "CPUCR": 0x802, "MSP": 0x803,
    "ISP": 0x804, "MMUSR": 0x805, "URP": 0x806, "SRP": 0x807, "PCR": 0x808,
    "ROMBAR": 0xC00, "ROMBAR0": 0xC00, "ROMBAR1": 0xC01,
    "FLASHBAR": 0xC04, "RAMBAR0": 0xC04, "RAMBAR_ALT": 0xC04,
    "RAMBAR": 0xC05, "RAMBAR1": 0xC05,
    "MPCR": 0xC0C, "EDRAMBAR": 0xC0D,
    "MBAR0": 0xC0E, "MBAR2": 0xC0E, "SECMBAR": 0xC0E,
    "MBAR1": 0xC0F, "MBAR": 0xC0F,
    "PCR1U0": 0xD02, "PCR1L0": 0xD03, "PCR2U0": 0xD04, "PCR2L0": 0xD05,
    "PCR3U0": 0xD06, "PCR3L0": 0xD07, "PCR1L1": 0xD0A, "PCR1U1": 0xD0B,
    "PCR2L1": 0xD0C, "PCR2U1": 0xD0D, "PCR3L1": 0xD0E, "PCR3U1": 0xD0F,
    "CAC": 0xFFE, "MBO": 0xFFF,
}


def read(path):
    with open(path) as f:
        return f.read()


def number(text):
    text = text.strip()
    if "|" in text:
        v = 0
        for part in text.split("|"):
            v |= number(part)
        return v
    if text in SCOPE:
        return SCOPE[text]
    if text.lower().startswith("0x"):
        return int(text, 16)
    if text.startswith("0") and len(text) > 1:
        return int(text, 8)
    return int(text, 10)


def arch_mask(text):
    v = 0
    for part in text.replace("(", " ").replace(")", " ").split("|"):
        part = part.strip()
        if part:
            v |= ARCH_BITS[part]
    return v


ENTRY_RE = re.compile(
    r'\{\s*"([^"]*)"\s*,\s*(\d+)\s*,\s*'
    r"(one|two)\(([^)]*)\)\s*,\s*"
    r"(one|two)\(([^)]*)\)\s*,\s*"
    r'"([^"]*)"\s*,\s*([^}]*)\}'
)
ALIAS_RE = re.compile(r'\{\s*"([^"]*)"\s*,\s*"([^"]*)"\s*,?\s*\}')


def opcodes(src, with_match=False):
    """The forms and aliases of opcodes/m68k-opc.c, in table order: each form
    as (name, opcode, words, args, arch), with its match mask after the
    opcode if `with_match`."""
    text = read(os.path.join(src, "opcodes/m68k-opc.c"))
    split = text.index("m68k_opcode_alias m68k_opcode_aliases")
    forms = []
    for m in ENTRY_RE.finditer(text[:split]):
        name, _size, k1, a1, k2, a2, args, arch = m.groups()

        def pair(kind, text):
            parts = [p.strip() for p in text.split(",")]
            if kind == "one":
                return number(parts[0]) << 16
            return (number(parts[0]) << 16) + number(parts[1])

        opcode = pair(k1, a1)
        match = pair(k2, a2)
        # tc-m68k.c md_begin(): a leading dot, or any fixed bit in the second
        # word, means the instruction is two opcode words wide.
        if args.startswith("."):
            args, words = args[1:], 2
        else:
            words = 2 if match & 0xFFFF else 1
        if with_match:
            forms.append((name, opcode, match, words, args, arch_mask(arch)))
        else:
            forms.append((name, opcode, words, args, arch_mask(arch)))
    aliases = [(m.group(1), m.group(2)) for m in ALIAS_RE.finditer(text[split:])]
    return forms, aliases


REGENUM_RE = re.compile(r"enum m68k_register\s*\{(.*?)\n\};", re.S)


def registers(src):
    """The m68k_register enum of gas/config/m68k-parse.h, as name -> number."""
    body = REGENUM_RE.search(read(os.path.join(src, "gas/config/m68k-parse.h"))).group(1)
    body = re.sub(r"/\*.*?\*/", " ", body, flags=re.S)
    body = re.sub(r"#define[^\n]*", " ", body)
    out = {}
    n = 0
    for item in body.split(","):
        item = item.strip()
        if not item:
            continue
        if "=" in item:
            name, value = item.split("=")
            n = int(value.strip())
            out[name.strip()] = n
        else:
            n += 1
            out[item] = n
    return out


CTRL_RE = re.compile(
    r"static const enum m68k_register (\w+)_ctrl\[\]\s*=\s*\{(.*?)\};", re.S
)
CPU_RE = re.compile(r"static const struct m68k_cpu (m68k_archs|m68k_cpus)\[\]\s*=\s*\{(.*?)\n\s*\};", re.S)
CPU_ROW_RE = re.compile(r"\{([^,{}]+),\s*([\w*&() ]+),\s*\"([^\"]*)\"\s*,\s*(-?\d+)\s*\}")


def cpus(src):
    """The control-register sets and CPU models of gas/config/tc-m68k.c."""
    text = read(os.path.join(src, "gas/config/tc-m68k.c"))
    ctrl = {}
    for m in CTRL_RE.finditer(text):
        body = re.sub(r"/\*.*?\*/", " ", m.group(2), flags=re.S)
        regs = [r.strip() for r in body.split(",")]
        ctrl[m.group(1) + "_ctrl"] = [r for r in regs if r and r != "0"]
    ctrl["cpu32_ctrl"] = ctrl["m68010_ctrl"]  # `#define cpu32_ctrl m68010_ctrl`
    tables = {}
    for m in CPU_RE.finditer(text):
        rows = []
        for row in CPU_ROW_RE.finditer(re.sub(r"/\*.*?\*/", " ", m.group(2), flags=re.S)):
            arch, regs, name, _alias = row.groups()
            regs = regs.strip()
            rows.append((name, arch_mask(arch), None if regs == "NULL" else regs))
        tables[m.group(1)] = rows
    return ctrl, tables


# src/arch/m68k/insn.rs: the mnemonics the hand-written encoders take, and the
# size suffixes each of them allows.  Those keep their own encoders, which
# choose among forms by rules of their own; this table holds the rest.  A unit
# test in generic.rs checks that the two sets really are disjoint.
B, W, L, S = 1, 2, 4, 8
BWL = B | W | L
HAND = {
    "move": BWL, "mov": BWL, "movea": W | L, "moveq": L, "movem": W | L,
    "movec": L, "lea": L, "pea": L, "exg": L, "swap": W, "ext": W | L,
    "extb": L, "clr": BWL, "neg": BWL, "negx": BWL, "not": BWL, "tst": BWL,
    "tas": B, "nbcd": B, "add": BWL, "sub": BWL, "adda": W | L,
    "suba": W | L, "addi": BWL, "subi": BWL, "cmpi": BWL, "andi": BWL,
    "ori": BWL, "eori": BWL, "addq": BWL, "subq": BWL, "addx": BWL,
    "subx": BWL, "abcd": B, "sbcd": B, "cmp": BWL, "cmpa": W | L,
    "cmpm": BWL, "and": BWL, "or": BWL, "eor": BWL, "mulu": W | L,
    "muls": W | L, "divu": W | L, "divs": W | L, "divul": L, "divsl": L,
    "chk": W | L, "chk2": BWL, "cmp2": BWL, "asr": BWL, "asl": BWL,
    "lsr": BWL, "lsl": BWL, "roxr": BWL, "roxl": BWL, "ror": BWL,
    "rol": BWL, "btst": B | L, "bchg": B | L, "bclr": B | L, "bset": B | L,
    "bra": B | W | L | S, "jra": B | W | L | S, "jbra": B | W | L | S,
    "bsr": B | W | L | S, "jbsr": B | W | L | S, "dbra": W, "jmp": 0,
    "jsr": 0, "trap": 0, "link": W | L, "unlk": 0, "stop": 0, "rtd": 0,
    "bkpt": 0, "rts": 0, "rte": 0, "rtr": 0, "trapv": 0, "nop": 0,
    "reset": 0, "illegal": 0, "bftst": 0, "bfextu": 0, "bfchg": 0,
    "bfexts": 0, "bfclr": 0, "bfffo": 0, "bfset": 0, "bfins": 0,
}
for _c in "hi ls cc hs cs lo ne eq vc vs pl mi ge lt gt le t f".split():
    HAND["db" + _c] = W
    HAND["s" + _c] = B
    if _c not in ("t", "f"):
        for _p in ("b", "j", "jb"):
            HAND[_p + _c] = B | W | L | S


def hand_written(name):
    """Whether src/arch/m68k/insn.rs's `resolve` claims this mnemonic."""
    suffix = {"b": B, "w": W, "l": L, "s": S}.get(name[-1:])
    if suffix and HAND.get(name[:-1], 0) & suffix:
        return True
    return name in HAND


INIT_RE = re.compile(r'\{\s*"([^"]+)"\s*,\s*(\w+)\s*\}')


def movec_names(src):
    """The `movec` control-register spellings of tc-m68k.c's init_table.

    Only the block the file marks off as control registers, and in its own
    order: `asid` is entered twice there, and the second entry wins.
    """
    text = read(os.path.join(src, "gas/config/tc-m68k.c"))
    block = text[text.index("/* Control registers.  */"):]
    block = block[: block.index("/* End of control registers.  */")]
    out = {}
    for m in INIT_RE.finditer(block):
        out[m.group(1)] = m.group(2)
    return out


def rust_string(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def render(src):
    forms, aliases = opcodes(src)
    regs = registers(src)
    ctrl, cpu_tables = cpus(src)
    names = movec_names(src)

    kept = [f for f in forms if not f[4] & MAC and not hand_written(f[0])]
    have = {f[0] for f in kept}
    # Sorting by name only, and stably, is what tc-m68k.c's m68k_compare_opcode
    # does: forms of one mnemonic keep the order they have in the table, which
    # is the order they are tried in.
    kept.sort(key=lambda f: f[0])
    all_aliases = aliases
    aliases = [a for a in aliases if a[1] in have and not hand_written(a[0])]
    aliases.sort()

    # `mcfv4e_ctrl` and its relatives list `PC`, which tc-m68k.c's own range
    # test for a MOVEC operand (`USP` to `MBO`) then refuses; drop it here so
    # the same names are reachable on both sides.
    movec = lambda r: regs["USP"] <= regs[r] <= regs["MBO"] and r in MOVEC_CODES
    ctrl = {k: [r for r in v if movec(r)] for k, v in ctrl.items()}
    reachable = {r for rows in cpu_tables.values() for _, _, c in rows if c for r in ctrl[c]}
    spellings = sorted(
        (n, r) for n, r in names.items() if r in MOVEC_CODES and r in reachable
    )

    chunks = []
    out = chunks.append
    out("//! The 680x0 instruction table, generated from GNU binutils 2.47.\n")
    out("//!\n")
    out("//! Do not edit: `tools/tables/m68k.py table` writes this file from\n")
    out("//! `opcodes/m68k-opc.c`, `gas/config/m68k-parse.h` and\n")
    out("//! `gas/config/tc-m68k.c`. [`super::generic`] reads it, following\n")
    out("//! `tc-m68k.c`'s own reading of the `args` strings that\n")
    out("//! `include/opcode/m68k.h` documents.\n")
    out("//!\n")
    out("//! The ColdFire MAC and EMAC forms are left out; rsasm does not\n")
    out("//! implement them.\n")
    out("\n")
    out("/// A CPU feature, as `include/opcode/m68k.h` numbers them.\n")
    out("pub mod feature {\n")
    for name in [
        "m68000", "m68010", "m68020", "m68030", "m68040", "m68060",
        "m68881", "m68851", "cpu32", "fido_a", "mcfmac", "mcfemac", "cfloat",
        "mcfhwdiv", "mcfisa_a", "mcfisa_aa", "mcfisa_b", "mcfisa_c", "mcfusp",
    ]:
        out("    pub const %s: u32 = %#x;\n" % (name.upper(), ARCH_BITS[name]))
    out("    /// Every 68k-proper bit, as against the ColdFire ones.\n")
    out("    pub const M68K_MASK: u32 = 0x3ff;\n")
    out("}\n\n")

    out("/// One way of writing one instruction.\n")
    out("pub struct Form {\n")
    out("    pub name: &'static str,\n")
    out("    /// The two opcode words, high word first.\n")
    out("    pub opcode: u32,\n")
    out("    /// How many of those words the instruction really has, 1 or 2.\n")
    out("    pub words: u8,\n")
    out("    /// Operand kinds and places, in pairs.\n")
    out("    pub args: &'static str,\n")
    out("    /// The CPUs that have it, as [`feature`] bits.\n")
    out("    pub arch: u32,\n")
    out("}\n\n")
    out("const fn f(name: &'static str, opcode: u32, words: u8, args: &'static str, arch: u32) -> Form {\n")
    out("    Form { name, opcode, words, args, arch }\n")
    out("}\n\n")
    out("/// Every form, sorted by name; forms of one mnemonic keep the order\n")
    out("/// GNU as tries them in.\n")
    out("pub const FORMS: &[Form] = &[\n")
    for name, opcode, words, args, arch in kept:
        out(
            "    f(%s, %#010x, %d, %s, %#x),\n"
            % (rust_string(name), opcode, words, rust_string(args), arch)
        )
    out("];\n\n")

    hand = {}
    for name, _, _, _, arch in forms:
        if hand_written(name):
            hand[name] = hand.get(name, 0) | arch
    for alias, primary in all_aliases:
        if primary in hand and alias not in hand:
            hand[alias] = hand[primary]
    out("/// The CPUs that have each mnemonic the hand-written encoders take, by\n")
    out("/// GNU's name for it (size letter included, alias resolved): the union\n")
    out("/// of its forms' CPUs.\n")
    out("pub const HAND_ARCH: &[(&str, u32)] = &[\n")
    for name in sorted(hand):
        out("    (%s, %#x),\n" % (rust_string(name), hand[name] & ~MAC))
    out("];\n\n")

    cf = ARCH_BITS["mcfisa_a"] | ARCH_BITS["mcfisa_aa"] | ARCH_BITS["mcfisa_b"] \
        | ARCH_BITS["mcfisa_c"] | ARCH_BITS["mcfusp"] | ARCH_BITS["mcfhwdiv"] | ARCH_BITS["cfloat"]
    cf_forms = [f for f in opcodes(src, with_match=True)[0]
                if hand_written(f[0]) and f[5] & cf and not f[5] & MAC]
    cf_forms.sort(key=lambda f: f[0])
    cf_names = {f[0] for f in cf_forms}
    out("/// The ColdFire forms of the mnemonics the hand-written encoders take,\n")
    out("/// with the mask GNU's disassembler matches them by, sorted by name.\n")
    out("/// ColdFire lacks many 68000 addressing modes, so what those encoders\n")
    out("/// write for a ColdFire is checked against these.\n")
    out("pub const CF_FORMS: &[CfForm] = &[\n")
    for name, opcode, match, words, args, arch in cf_forms:
        out(
            "    cf(%s, %#010x, %#010x, %d, %s, %#x),\n"
            % (rust_string(name), opcode, match, words, rust_string(args), arch)
        )
    out("];\n\n")
    out("/// A form of [`CF_FORMS`].\n")
    out("pub struct CfForm {\n")
    out("    pub form: Form,\n")
    out("    /// Which bits of [`Form::opcode`] are fixed.\n")
    out("    pub mask: u32,\n")
    out("}\n\n")
    out("const fn cf(name: &'static str, opcode: u32, mask: u32, words: u8, args: &'static str, arch: u32) -> CfForm {\n")
    out("    CfForm { form: f(name, opcode, words, args, arch), mask }\n")
    out("}\n\n")
    out("/// Aliases of the mnemonics in [`CF_FORMS`], sorted by the alias.\n")
    out("pub const CF_ALIASES: &[(&str, &str)] = &[\n")
    for alias, primary in sorted(all_aliases):
        if primary in cf_names:
            out("    (%s, %s),\n" % (rust_string(alias), rust_string(primary)))
    out("];\n\n")

    out("/// Mnemonics that mean another one, sorted by the alias.\n")
    out("pub const ALIASES: &[(&str, &str)] = &[\n")
    for alias, primary in aliases:
        out("    (%s, %s),\n" % (rust_string(alias), rust_string(primary)))
    out("];\n\n")

    out("/// GNU's number for each register name, from the `m68k_register`\n")
    out("/// enum of `gas/config/m68k-parse.h`. The operand parser answers with\n")
    out("/// these, and the encoders ask what a number means. The whole enum is\n")
    out("/// written out, GNU's numbering included, so a name is there when a\n")
    out("/// form starts using it; not every name has a use yet.\n")
    out("#[allow(dead_code)]\n")
    out("pub mod rid {\n")
    for name, num in sorted(regs.items(), key=lambda kv: kv[1]):
        out("    pub const %s: u16 = %d;\n" % (name, num))
    out("}\n\n")

    out("/// A `MOVEC` control register: its name, GNU's number for it, and\n")
    out("/// the 12-bit code the instruction carries.\n")
    out("pub const MOVEC_REGS: &[(&str, u16, u16)] = &[\n")
    for name, reg in spellings:
        out('    ("%s", %d, %#05x),\n' % (name, regs[reg], MOVEC_CODES[reg]))
    out("];\n\n")
    out("/// `RAMBAR` means `RAMBAR1` except on the few CPUs that list\n")
    out("/// `RAMBAR_ALT`, where it means `RAMBAR0`.\n")
    out("pub const RAMBAR: u16 = %d;\n" % regs["RAMBAR"])
    out("pub const RAMBAR_ALT: u16 = %d;\n\n" % regs["RAMBAR_ALT"])

    out("/// A CPU model: what it can assemble and which control registers\n")
    out("/// `movec` reaches on it.\n")
    out("pub struct CpuDef {\n")
    out("    pub name: &'static str,\n")
    out("    pub arch: u32,\n")
    out("    pub ctrl: &'static [u16],\n")
    out("}\n\n")
    out("const fn c(name: &'static str, arch: u32, ctrl: &'static [u16]) -> CpuDef {\n")
    out("    CpuDef { name, arch, ctrl }\n")
    out("}\n\n")
    for key in sorted(ctrl):
        if not any(c == key for rows in cpu_tables.values() for _, _, c in rows):
            continue
        out(
            "const %s: &[u16] = &[%s];\n"
            % (key.upper(), ", ".join(str(regs[r]) for r in ctrl[key]))
        )
    out("\n")
    for table, title in (
        ("m68k_archs", "The architecture names `.arch` takes."),
        ("m68k_cpus", "The processor names `.arch` also takes."),
    ):
        out("/// %s\n" % title)
        out("pub const %s: &[CpuDef] = &[\n" % table.upper())
        for name, arch, regset in cpu_tables[table]:
            out(
                "    c(%s, %#x, %s),\n"
                % (rust_string(name), arch, regset.upper() if regset else "&[]")
            )
        out("];\n\n")

    # rustfmt's layout, so that `cargo fmt` leaves the file as written.
    text = "".join(chunks)
    try:
        text = subprocess.run(
            ["rustfmt", "--edition", "2024", "--emit", "stdout"],
            input=text, capture_output=True, text=True, check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as e:
        sys.exit("rustfmt failed (%s)" % e)
    return text


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd not in ("table", "check"):
        sys.exit(__doc__)
    text = render(BINUTILS)
    stale = open(TABLE).read() != text
    if cmd == "check":
        if stale:
            print("out of date: %s" % os.path.relpath(TABLE, ROOT))
        sys.exit(1 if stale else 0)
    if stale:
        with open(TABLE, "w") as f:
            f.write(text)
    print("rewrote %d file(s)" % stale, file=sys.stderr)


if __name__ == "__main__":
    main()
