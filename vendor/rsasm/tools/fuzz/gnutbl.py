"""Reads GNU binutils' expanded x86 opcode table.

binutils generates opcodes/i386-tbl.h from i386-opc.tbl with every template
expanded, but as positional numbers. The field order comes from i386-opc.h,
so the decode is mechanical, and it gives every row GNU as assembles: its
mnemonic, opcode, opcode space, the modifier bits (VEX/EVEX length, W, `vvvv`
use, masking, broadcast, rounding, disp8 shift) and each operand's type.

The rsasm tables for the SIMD extensions were written from this, and the
fuzzer's SIMD forms are read from it (see simd.py), so it is also a tool for
looking a row up:

    tools/fuzz/gnutbl.py 'vpdpbusd|vaddph'    # rows by mnemonic regex
    tools/fuzz/gnutbl.py --cpu AVX512_FP16    # rows by CPU flag regex

The table is found under $RSASM_ORACLES/src/binutils-*/opcodes, which
tools/oracles/build.sh unpacks.
"""

import glob
import os
import re
import sys

MODIFIERS = [
    ("d", 1), ("w", 1), ("load", 1), ("modrm", 1), ("jump", 3), ("floatmf", 1),
    ("size", 2), ("checkoperandsize", 1), ("operandconstraint", 4),
    ("mnemonicsize", 2), ("no_bsuf", 1), ("no_wsuf", 1), ("no_lsuf", 1),
    ("no_ssuf", 1), ("no_qsuf", 1), ("fwait", 1), ("isstring", 2),
    ("regmem", 1), ("bndprefixok", 1), ("prefixok", 3), ("isprefix", 1),
    ("immext", 1), ("norex64", 1), ("vex", 2), ("vexvvvv", 2), ("vexw", 2),
    ("opcodeprefix", 2), ("sib", 3), ("sse2avx", 1), ("evex", 3),
    ("masking", 1), ("broadcast", 3), ("staticrounding", 1), ("sae", 1),
    ("disp8memshift", 3), ("optimize", 1), ("dialect", 2), ("intelsuffix", 1),
    ("isa64", 2), ("noegpr", 1), ("nf", 1), ("rex2", 1),
]

OPERAND_BITS = [
    "class", "instance", "imm1", "imm8", "imm8s", "imm16", "imm32", "imm32s",
    "imm64", "disp8", "disp16", "disp32", "disp64", "baseindex", "byte",
    "word", "dword", "fword", "qword", "tbyte", "xmmword", "ymmword",
    "zmmword", "tmmword", "unspecified", "unused",
]

CLASSES = ["", "Reg", "SReg", "RegFP", "RegCR", "RegDR", "RegTR", "RegMMX",
           "RegSIMD", "RegMask", "RegBND"]
INSTANCES = ["", "Accum", "RegC", "RegD", "RegB"]
CPU_COMMON = ["287", "387", "3dnow", "3dnowa", "64", "AVX", "HLE", "AVX512F",
              "AVX512VL", "APX_F", "AVX10_2", "AMX_TRANSPOSE", "no64"]


def source_dir():
    root = os.environ.get("RSASM_ORACLES")
    if not root:
        here = os.path.dirname(os.path.abspath(__file__))
        root = os.path.join(os.path.dirname(os.path.dirname(here)), "target", "oracles")
    hits = sorted(glob.glob(os.path.join(root, "src", "binutils-*", "opcodes")))
    return hits[-1] if hits else None


class Row:
    __slots__ = ("mnem", "opcode", "space", "ext", "mod", "cpu", "cpu_any", "ops")

    def __init__(self, **kw):
        for k, v in kw.items():
            setattr(self, k, v)

    def cpus(self):
        return set(self.cpu) | set(self.cpu_any)

    def __repr__(self):
        mods = ",".join(f"{k}={v}" for k, v in self.mod.items() if v)
        ops = " ; ".join("|".join(sorted(o)) for o in self.ops)
        return (f"{self.mnem:20} {self.opcode:#x} space={self.space} ext={self.ext} "
                f"cpu={'&'.join(self.cpu) or '-'} any={'&'.join(self.cpu_any) or '-'} "
                f"[{mods}] {{ {ops} }}")


def _ints(text):
    return [int(x) for x in re.findall(r"-?\d+", text)]


def _groups(text, start):
    i = start
    while i < len(text):
        while i < len(text) and text[i] in " \t\n,":
            i += 1
        if i >= len(text) or text[i] != "{":
            return
        depth, j = 0, i
        while j < len(text):
            if text[j] == "{":
                depth += 1
            elif text[j] == "}":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        yield text[i + 1:j]
        i = j + 1


def _cpu_names(opc_h):
    i = opc_h.index("Cpu186 = 0,")
    j = opc_h.index("CpuAttrEnums")
    return [n[3:] for n in re.findall(r"^\s*(Cpu\w+)(?: = 0)?,", opc_h[i:j], re.M)]


def _cpu(arr, names):
    out = []
    if arr[0]:
        out.append(names[arr[0] - 1] if arr[0] - 1 < len(names) else f"isa{arr[0]}")
    out.extend(n for i, n in enumerate(CPU_COMMON) if i + 1 < len(arr) and arr[i + 1])
    return out


def _operand(arr):
    d = dict(zip(OPERAND_BITS, arr))
    out = set()
    if d["class"]:
        out.add(CLASSES[d["class"]])
    if d["instance"]:
        out.add(INSTANCES[d["instance"]])
    out.update(k for k in OPERAND_BITS[2:-1] if d[k])
    return out


def load(src=None):
    """All rows of the table, in table order."""
    src = src or source_dir()
    if not src:
        raise FileNotFoundError("binutils source not found; run tools/oracles/build.sh")
    names = _cpu_names(open(os.path.join(src, "i386-opc.h")).read())
    text = open(os.path.join(src, "i386-tbl.h")).read()
    body = text[text.index("const insn_template i386_optab[]"):]
    head = re.compile(r"\{\s*MN_(\w+),\s*([^,]+),\s*(\d+),\s*SPACE_(\w+),\s*(-?\w+),\s*")
    starts = [m.start() for m in re.finditer(r"\n  \{ MN_", body)] + [len(body)]
    rows = []
    for a, b in zip(starts, starts[1:]):
        chunk = body[a:b]
        m = head.search(chunk)
        if not m:
            continue
        g = list(_groups(chunk, m.end()))
        if len(g) < 4:
            continue
        mod = dict(zip((n for n, _ in MODIFIERS), _ints(g[0])))
        try:
            opcode = eval(m.group(2), {"__builtins__": {}})
        except Exception:
            continue
        ext = m.group(5)
        if not re.fullmatch(r"None|-?(0x[0-9a-fA-F]+|\d+)", ext):
            # The pseudo-prefixes (`{disp8}` ...) name their extension.
            continue
        rows.append(Row(
            mnem=m.group(1),
            opcode=opcode,
            space={"BASE": 0, "0F": 1, "0F38": 2, "0F3A": 3, "MAP4": 4, "MAP5": 5,
                   "MAP6": 6, "MAP7": 7, "XOP08": 8, "XOP09": 9, "XOP0A": 10}[m.group(4)],
            ext=None if ext == "None" else int(ext, 0),
            mod=mod,
            cpu=_cpu(_ints(g[1]), names),
            cpu_any=_cpu(_ints(g[2]), names),
            ops=[_operand(_ints(x)) for x in _groups(g[3], 0)][: int(m.group(3))],
        ))
    return rows


def main():
    args = sys.argv[1:]
    by_cpu = args[:1] == ["--cpu"]
    pat = re.compile(args[1] if by_cpu else (args[0] if args else "."))
    for r in load():
        hit = any(pat.search(c) for c in r.cpus()) if by_cpu else pat.fullmatch(r.mnem)
        if hit:
            print(r)


if __name__ == "__main__":
    main()
