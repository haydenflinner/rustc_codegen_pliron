"""SIMD and newer-extension forms for the x86 fuzzer, read from GNU's table.

The hand-written forms in x86.py cover the general-purpose instruction set
and a slice of SSE/AVX. The extensions after that — AVX-512 and its subsets,
FP16, the VEX-encoded additions, XOP, BMI, AMX, CET and the rest — are far
too many to transcribe again, so their forms are taken from GNU binutils'
expanded opcode table (gnutbl.py): every row of every extension below
becomes a form. That keeps the forms independent of rsasm's tables, which is
what makes the comparison worth running.

Each case picks a vector length the row allows, registers (the EVEX-only
`xmm16`-`xmm31` included where EVEX can reach them), and at most one memory
operand with a random base, index and displacement, so disp8*N compression
is exercised at every scale. Where the row allows them it adds a writemask,
`{z}`, a `{1toN}` broadcast and embedded rounding or `{sae}`, and a few cases
get a deliberately wrong decorator, which all three assemblers should refuse.
"""

import re

import gnutbl

# CPU flags (without GNU's `Cpu` prefix) whose rows are fuzzed. APX is not in
# rsasm; the Xeon Phi's 4FMAPS/4VNNIW register groups and the families
# llvm-mc 22 does not have are left out as well.
IN_SCOPE = {
    "AVX", "AVX2", "AES", "PCLMULQDQ", "F16C", "FMA", "FMA4", "XOP", "GFNI",
    "VAES", "VPCLMULQDQ", "SHA", "SHA512", "SM3", "SM4", "AVX_VNNI", "AVX_IFMA",
    "AVX_NE_CONVERT", "AVX_VNNI_INT8", "AVX_VNNI_INT16", "BMI", "BMI2", "TBM",
    "LWP", "ADX", "LZCNT", "POPCNT", "AVX512F", "AVX512VL", "AVX512BW",
    "AVX512DQ", "AVX512CD", "AVX512ER", "AVX512PF", "AVX512IFMA", "AVX512VBMI",
    "AVX512_VBMI2", "AVX512_VNNI", "AVX512_BITALG", "AVX512_VPOPCNTDQ",
    "AVX512_VP2INTERSECT", "AVX512_BF16", "AVX512_FP16", "AMX_TILE", "AMX_INT8",
    "AMX_BF16", "AMX_FP16", "AMX_COMPLEX", "IBT", "SHSTK", "PTWRITE", "SERIALIZE",
    "HRESET", "UINTR", "WAITPKG", "ENQCMD", "MOVDIRI", "MOVDIR64B", "RDPID",
    "ClflushOpt", "CLWB", "CLDEMOTE", "Xsave", "XSAVES", "XSAVEC", "Xsaveopt",
    "RdRnd", "RDSEED", "KL", "WideKL", "OSPKE", "RDPRU", "CLZERO", "MWAITX",
    "FSGSBase", "INVPCID", "TSXLDTRK", "RTM", "PREFETCHI", "PREFETCHWT1",
    "WBNOINVD", "PCONFIG", "CMPCCXADD", "RAO_INT", "MSRLIST", "WRMSRNS", "AVX10_2",
    "MOVRS",
}
NEUTRAL = {"64", "no64", "AVX", "AVX512F", "AVX512VL"}

VEC = {"xmmword": ("xmm", 128), "ymmword": ("ymm", 256), "zmmword": ("zmm", 512)}
ELEM = {"byte": 8, "word": 16, "dword": 32, "qword": 64}
GPR = {
    16: ["ax", "cx", "dx", "bx", "sp", "bp", "si", "di"],
    32: ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi"],
    64: ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi"],
}
PTR = {8: "byte", 16: "word", 32: "dword", 64: "qword", 128: "xmmword", 256: "ymmword",
       512: "zmmword"}
ROUND = ["rn-sae", "rd-sae", "ru-sae", "rz-sae"]


class Unusable(Exception):
    pass


def classify(o):
    """The operand kind for a GNU operand type: (kind, detail)."""
    if o & {"RegC", "RegD", "RegB"} or ("Accum" in o and "RegSIMD" not in o):
        raise Unusable("implicit register")
    if o & {"RegFP", "SReg", "RegCR", "RegDR", "RegTR", "RegBND", "RegMMX", "imm1",
            "imm64", "fword", "tbyte"}:
        raise Unusable("register class")
    if "Accum" in o:
        return "xmm0", None
    mem = "baseindex" in o
    if "RegSIMD" in o:
        if "tmmword" in o:
            return "tmm", None
        sizes = [v[1] for k, v in VEC.items() if k in o]
        elems = [ELEM[k] for k in ELEM if k in o]
        return ("vm" if mem else "v"), (sizes, elems)
    if "RegMask" in o:
        return ("km" if mem else "k"), None
    if "Reg" in o:
        sizes = [ELEM[k] for k in ELEM if k in o]
        return ("gm" if mem else "g"), sizes
    if o & {"imm8", "imm8s"}:
        return "i8", None
    if o & {"imm16"}:
        return "i16", None
    if o & {"imm32", "imm32s"}:
        return "i32", None
    if mem:
        sizes = [ELEM[k] for k in ELEM if k in o] + [v[1] for k, v in VEC.items() if k in o]
        return "m", sizes
    raise Unusable(f"operand {sorted(o)}")


class SimdForm:
    """One GNU table row, ready to generate cases from."""

    def __init__(self, row):
        m = row.mod
        cpus = row.cpus()
        if not cpus & IN_SCOPE or "APX_F" in row.cpu or "AMX_MOVRS" in row.cpu:
            raise Unusable("cpu")
        if m["sse2avx"] or m["isprefix"] or m["jump"] or m["isstring"]:
            raise Unusable("kind")
        # The register-group constraint of 4FMAPS, and GNU's fake operands.
        if m["operandconstraint"] in (2, 5, 7) and not m["evex"]:
            raise Unusable("constraint")
        self.row = row
        self.mnem = row.mnem
        self.kinds = [classify(o) for o in row.ops]
        self.group = "simd:" + next((c for c in row.cpu if c not in NEUTRAL),
                                    row.cpu[0] if row.cpu else "misc").lower()
        self.syntaxes = {0: ("att", "intel"), 1: ("intel",), 2: ("att",),
                         3: ("att",)}[m["dialect"]]
        self.modes = (64,) if "64" in row.cpu else (16, 32) if "no64" in row.cpu else (16, 32, 64)
        self.evex = m["evex"]
        self.vex = m["vex"]
        self.flags = ""
        self.sfx = None
        self.ops = [k for k, _ in self.kinds]

    def __repr__(self):
        return f"{self.mnem} {','.join(self.ops)}"

    def lengths(self):
        sizes = sorted({s for k, d in self.kinds if k in ("v", "vm") for s in d[0]})
        fixed = {1: [512], 2: [128], 3: [256], 4: [128]}.get(self.evex)
        if fixed:
            return fixed
        if self.vex == 2:
            return [256]
        if self.vex == 3:
            return [128]
        if self.vex == 1 and not self.evex:
            return [s for s in sizes if s < 512] or [128]
        return sizes or [None]

    def generate(self, rng, mode, syntax, mutation_ratio):
        return SimdCase(self, rng, mode, syntax, rng.random() < mutation_ratio)


class Opnd:
    def __init__(self, **kw):
        self.__dict__.update(kw)


class SimdCase:
    def __init__(self, form, rng, mode, syntax, mutate):
        self.form = form
        self.rng = rng
        self.mode = mode
        self.syntax = syntax
        self.mutation = None
        self.decor = []
        self._build(mutate)

    # ---- operand choice ----------------------------------------------------

    def _reg_num(self, limit):
        rng, f = self.rng, self.form
        if self.mode != 64:
            return rng.randrange(8)
        if f.evex and limit > 16 and rng.random() < 0.35:
            return rng.randrange(16, 32)
        return rng.randrange(16 if limit > 8 else 8)

    def _mem(self, bits, vsib=None):
        rng = self.rng
        addr = 64 if self.mode == 64 else 32
        if self.mode == 64 and rng.random() < 0.1:
            addr = 32
        regs = GPR[addr] + ([f"r{i}{'' if addr == 64 else 'd'}" for i in range(8, 16)]
                            if self.mode == 64 else [])
        base = None if rng.random() < 0.1 else rng.choice(regs)
        index, scale = None, 1
        if vsib:
            index = f"{vsib}{self._reg_num(32)}"
            scale = rng.choice([1, 2, 4, 8])
        elif rng.random() < 0.4:
            index = rng.choice([r for r in regs if r not in ("esp", "rsp")])
            scale = rng.choice([1, 2, 4, 8])
        if base is None and index is None:
            base = regs[0]
        # Displacements that land on and off every disp8*N scale.
        disp = rng.choice([None, 0, 1, 2, 4, 8, 16, 32, 64, 0x40, 0x80, 0x100, 0x200,
                           0x3f, -0x40, -0x80, -0x100, 0x7f0, 0x1000, 0x12345, -0x2000])
        return Opnd(kind="mem", bits=bits, base=base, index=index, scale=scale, disp=disp)

    def _build(self, mutate):
        rng, f = self.rng, self.form
        lens = f.lengths()
        L = rng.choice(lens)
        rank = lens.index(L)
        mem_slots = [i for i, (k, _) in enumerate(f.kinds) if k in ("vm", "km", "gm", "m")]
        must_mem = [i for i, (k, _) in enumerate(f.kinds) if k == "m"]
        mem_at = must_mem[0] if must_mem else (
            rng.choice(mem_slots) if mem_slots and rng.random() < 0.5 else None)
        vsib = {1: "xmm", 2: "ymm", 3: "zmm"}.get(f.row.mod["sib"])
        gpr_bits = None
        ops = []
        for i, (kind, detail) in enumerate(f.kinds):
            if kind in ("i8", "i16", "i32"):
                hi = {"i8": 0xff, "i16": 0xffff, "i32": 0x7fffffff}[kind]
                v = rng.choice([0, 1, 3, 0x11, 0x7f, hi])
                if f.mnem.startswith("vpermil2"):
                    v = rng.choice([0, 1, 2, 3, 15, 16])
                ops.append(Opnd(kind="imm", value=v))
                continue
            if kind == "xmm0":
                ops.append(Opnd(kind="reg", name="xmm0"))
                continue
            if kind == "tmm":
                ops.append(Opnd(kind="reg", name=f"tmm{rng.randrange(8)}"))
                continue
            if kind in ("v", "vm"):
                sizes, elems = detail
                if not sizes:
                    raise Unusable("vector with no size")
                size = L if L in sizes else sizes[min(rank, len(sizes) - 1)]
                if i == mem_at:
                    scalar = f.evex == 4 or f.vex == 3
                    bits = elems[-1] if scalar and elems else size
                    o = self._mem(bits, vsib)
                    o.vec = size
                    o.elems = elems
                    ops.append(o)
                else:
                    cls = VEC[{128: "xmmword", 256: "ymmword", 512: "zmmword"}[size]][0]
                    ops.append(Opnd(kind="reg", name=f"{cls}{self._reg_num(32)}"))
                continue
            if kind in ("k", "km"):
                if i == mem_at:
                    ops.append(self._mem(None))
                else:
                    ops.append(Opnd(kind="reg", name=f"k{rng.randrange(8)}"))
                continue
            if kind in ("g", "gm"):
                sizes = [s for s in detail if s >= 16 and (s != 64 or self.mode == 64)]
                if not sizes:
                    raise Unusable("gpr size")
                if gpr_bits not in sizes:
                    gpr_bits = rng.choice(sizes)
                if i == mem_at:
                    ops.append(self._mem(gpr_bits))
                else:
                    n = self._reg_num(16)
                    name = GPR[gpr_bits][n] if n < 8 else (
                        f"r{n}" + {16: "w", 32: "d", 64: ""}[gpr_bits])
                    ops.append(Opnd(kind="reg", name=name, bits=gpr_bits))
                continue
            if kind == "m":
                bits = max(detail) if detail and len(detail) == 1 else None
                ops.append(self._mem(bits, vsib))
                continue
            raise Unusable(kind)
        self.ops = ops
        self.gpr_bits = gpr_bits

        m = f.row.mod
        mem = next((o for o in ops if o.kind == "mem"), None)
        if m["broadcast"] and mem is not None and getattr(mem, "vec", None) and rng.random() < 0.35:
            elem = 1 << (m["broadcast"] - 1)
            n = mem.vec // 8 // elem
            if mutate and rng.random() < 0.3:
                n *= 2
                self.mutation = "bcst-count"
            mem.bcst = n
            mem.bits = elem * 8
        if m["masking"] and rng.random() < 0.4:
            k = rng.randrange(1, 8)
            if mutate and rng.random() < 0.3:
                k = 0
                self.mutation = "k0"
            self.decor.append(f"k{k}")
            if rng.random() < 0.4:
                self.decor.append("z")
        elif mutate and rng.random() < 0.2:
            self.decor.append("z")
            self.mutation = "z-alone"
        self.round = None
        if (m["staticrounding"] or m["sae"]) and mem is None and rng.random() < 0.35:
            self.round = rng.choice(ROUND) if m["staticrounding"] else "sae"
            if mutate and rng.random() < 0.3:
                self.round = "sae" if m["staticrounding"] else rng.choice(ROUND)
                self.mutation = "rounding"

    # ---- rendering -----------------------------------------------------------

    def _text(self, o):
        att = self.syntax == "att"
        if o.kind == "reg":
            return ("%" if att else "") + o.name
        if o.kind == "imm":
            return (f"${o.value:#x}" if att else f"{o.value:#x}")
        bcst = f"{{1to{o.bcst}}}" if getattr(o, "bcst", None) else ""
        d = o.disp
        if att:
            disp = "" if d is None else (f"-{-d:#x}" if d < 0 else f"{d:#x}")
            inner = f"%{o.base}" if o.base else ""
            if o.index:
                inner += f",%{o.index},{o.scale}"
            return f"{disp}({inner}){bcst}"
        terms = [t for t in [o.base, f"{o.index}*{o.scale}" if o.index else None] if t]
        body = "+".join(terms)
        if d is not None:
            body += f"-{-d:#x}" if d < 0 else f"+{d:#x}"
        ptr = ""
        if o.bits and self.rng.random() < 0.6:
            ptr = f"{PTR[o.bits]} ptr "
        return f"{ptr}[{body}]{bcst}"

    def lines(self):
        att = self.syntax == "att"
        texts = [self._text(o) for o in self.ops]
        if self.decor and texts:
            deco = "".join("{%" + d + "}" if d.startswith("k") and att else "{" + d + "}"
                           for d in self.decor)
            # GNU lists operands in AT&T order: the destination is the last.
            texts[-1] += deco
        if self.round:
            # After an immediate or a general register the source starts
            # with, as GNU as wants it; Intel syntax mirrors the AT&T place.
            first = self.ops[0] if self.ops else None
            lead = first is not None and (first.kind == "imm" or getattr(first, "bits", None)
                                          and first.kind == "reg")
            texts.insert(1 if lead else 0, "{" + self.round + "}")
        if not att:
            texts.reverse()
        mnem = self.form.mnem
        row = self.form.row.mod
        if att and self.gpr_bits and any(o.kind == "mem" for o in self.ops) and \
                not any(o.kind == "reg" and getattr(o, "bits", None) for o in self.ops):
            sfx = {32: "l", 64: "q"}.get(self.gpr_bits)
            if sfx and not row[f"no_{sfx}suf"]:
                mnem += sfx
        return [mnem + (" " + ", ".join(texts) if texts else "")]

    def signature(self):
        shapes = []
        for o in self.ops:
            if o.kind == "reg":
                shapes.append(re.sub(r"\d+$", "", o.name) + ("H" if re.search(r"(1[6-9]|2\d|3[01])$", o.name) else ""))
            elif o.kind == "mem":
                shapes.append(f"m{o.bits or ''}" + (f"b{o.bcst}" if getattr(o, "bcst", None) else ""))
            else:
                shapes.append("imm")
        extra = ("{" + ",".join(self.decor) + "}" if self.decor else "") + \
            (f"{{{self.round}}}" if self.round else "")
        return f"{self.form.mnem} {','.join(shapes)}{extra}" + \
            (f" <{self.mutation}>" if self.mutation else "")

    def form_key(self):
        return self.signature()


def forms():
    """Every usable row as a form, or an empty list without the binutils source."""
    try:
        rows = gnutbl.load()
    except FileNotFoundError:
        return []
    out = []
    for row in rows:
        try:
            out.append(SimdForm(row))
        except Unusable:
            pass
    return out
