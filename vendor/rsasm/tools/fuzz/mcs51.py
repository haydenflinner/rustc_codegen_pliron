#!/usr/bin/env python3
"""Differential fuzzer for rsasm's 8051 (MCS-51) backend.

Random whole programs are generated from a table of instruction forms written
from Intel's MCS-51 instruction set (not from rsasm's own tables), rendered in
two spellings, and assembled by rsasm and by both references:

    AS        the Macro Assembler AS (`asl -cpu 8051` + `p2bin`), reading the
              Intel spelling rsasm's 8-bit dialect reads: `12H`, `$`, register
              and bit names, `P1.3`, `ORG`, `DB`, the generic `JMP`/`CALL`.
    sdas      SDCC's sdas8051 + sdld, reading asxxxx's: `0x12`, `.`, `.org`,
              `.db`, and numbers for every address.

rsasm assembles both spellings, as the references do, and each program is
compared four ways. A program is generated once and rendered twice, so the
two spellings mean the same thing; forms only AS reads (the generic jumps,
named bits, `DW`, whose byte order the two assemblers disagree on) go into
AS-only programs.

    tools/fuzz/mcs51.py fuzz --count 20000
    tools/fuzz/mcs51.py fuzz --count 5000 --seed 7 --as-only
    tools/fuzz/mcs51.py corpus as   > tools/xas-diff/i8051.txt
    tools/fuzz/mcs51.py corpus sdas > tools/xas-diff/i8051-sdas.txt

`corpus` prints every form with operands at their boundaries, one instruction
per line, for tools/xas-diff.

Each case is classified:

    agree       every assembler that read it produced the same image, or
                every one refused it.
    rsasm       the references agree and rsasm does not, in either spelling.
                These are the findings; the exit status is 1 if there are any.
    lenient     AS refuses and sdas assembles, which sdas does for an operand
                that does not fit its field (it truncates without a word).
                rsasm has to refuse, as AS does.
    strict      sdas refuses and AS assembles, and rsasm assembles what AS
                does: an `LJMP` or `LCALL` to below 0, which AS reads as a
                16-bit value and sdld refuses.
    first-pass  AS refuses a jump from its first pass, where it guesses every
                forward generic JMP and CALL short, and does not start the
                pass that would have laid the program out; rsasm assembles it.
                AS says so ("additional necessary passes not started").
    split       the references disagree otherwise. Listed.
    boundary    an explicit AJMP or ACALL starting in the last two bytes of a 2 KiB
                block, where both references test the block of the
                instruction rather than of the address after it, as the CPU
                does and rsasm does; see `Enc::addr11` in the backend.

Some programs are made invalid on purpose (`--mutations`): an operand out of
range, or a relative branch pushed out of reach by reserved space. Programs
also start just short of a 2 KiB block boundary, where `AJMP` and `ACALL`
targets change block.

Environment: RSASM (default target/debug/rsasm under the repository root) and
RSASM_ORACLES (default target/oracles), which must hold asl, p2bin, sdas8051,
sdld and share/asl/stddef51.inc from tools/oracles/build.sh; llvm-objcopy on
the PATH turns sdld's Intel HEX into an image.
"""

import argparse
import collections
import concurrent.futures
import os
import random
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
RSASM = os.environ.get("RSASM", os.path.join(ROOT, "target", "debug", "rsasm"))
ORACLES = os.environ.get("RSASM_ORACLES", os.path.join(ROOT, "target", "oracles"))
BIN = os.path.join(ORACLES, "bin")
ASL_INCLUDE = os.path.join(ORACLES, "share", "asl")

# --- the instruction set -----------------------------------------------------
#
# Operand kinds:
#   A C AB DPTR @DPTR @A+DPTR @A+PC   themselves
#   Rn @Ri                            a register, or R0/R1 indirect
#   #8 #16                            an immediate
#   dir                               a direct address, 00H-FFH
#   bit /bit                          a bit address, or its complement
#   rel                               a branch target within -128..127
#   a11 a16                           an AJMP/ACALL or LJMP/LCALL target
#   jmp call                          a generic target (AS only)


def build_forms():
    f = []
    for m in ("nop", "ret", "reti"):
        f.append((m, ()))
    for m in ("rr", "rrc", "rl", "rlc", "swap", "da"):
        f.append((m, ("A",)))
    f += [("mul", ("AB",)), ("div", ("AB",))]
    for m in ("add", "addc", "subb", "orl", "anl", "xrl"):
        for src in ("#8", "dir", "@Ri", "Rn"):
            f.append((m, ("A", src)))
    for m in ("orl", "anl", "xrl"):
        f += [(m, ("dir", "A")), (m, ("dir", "#8"))]
    for m in ("orl", "anl"):
        f += [(m, ("C", "bit")), (m, ("C", "/bit"))]
    for m in ("inc", "dec"):
        for o in ("A", "dir", "@Ri", "Rn"):
            f.append((m, (o,)))
    f.append(("inc", ("DPTR",)))
    for pair in (
        ("A", "Rn"), ("A", "dir"), ("A", "@Ri"), ("A", "#8"),
        ("Rn", "A"), ("Rn", "dir"), ("Rn", "#8"),
        ("dir", "A"), ("dir", "Rn"), ("dir", "dir"), ("dir", "@Ri"), ("dir", "#8"),
        ("@Ri", "A"), ("@Ri", "dir"), ("@Ri", "#8"),
        ("DPTR", "#16"), ("C", "bit"), ("bit", "C"),
    ):
        f.append(("mov", pair))
    f += [("movc", ("A", "@A+DPTR")), ("movc", ("A", "@A+PC"))]
    f += [("movx", ("A", "@Ri")), ("movx", ("A", "@DPTR")),
          ("movx", ("@Ri", "A")), ("movx", ("@DPTR", "A"))]
    f += [("push", ("dir",)), ("pop", ("dir",))]
    f += [("xch", ("A", "Rn")), ("xch", ("A", "dir")), ("xch", ("A", "@Ri")),
          ("xchd", ("A", "@Ri"))]
    for m in ("clr", "cpl"):
        f += [(m, ("A",)), (m, ("C",)), (m, ("bit",))]
    f += [("setb", ("C",)), ("setb", ("bit",))]
    for m in ("jc", "jnc", "jz", "jnz", "sjmp"):
        f.append((m, ("rel",)))
    for m in ("jb", "jnb", "jbc"):
        f.append((m, ("bit", "rel")))
    f += [("cjne", ("A", "#8", "rel")), ("cjne", ("A", "dir", "rel")),
          ("cjne", ("Rn", "#8", "rel")), ("cjne", ("@Ri", "#8", "rel"))]
    f += [("djnz", ("Rn", "rel")), ("djnz", ("dir", "rel"))]
    f += [("ajmp", ("a11",)), ("acall", ("a11",)),
          ("ljmp", ("a16",)), ("lcall", ("a16",)),
          ("jmp", ("@A+DPTR",))]
    return f


FORMS = build_forms()
GENERIC = [("jmp", ("jmp",)), ("call", ("call",))]

# The registers and bits both references name alike: AS through its
# stddef51.inc, sdas8051 through its own table. Used in either spelling.
SHARED_REGISTERS = {
    "P0": 0x80, "SP": 0x81, "DPL": 0x82, "DPH": 0x83, "PCON": 0x87,
    "TCON": 0x88, "TMOD": 0x89, "TL0": 0x8A, "TL1": 0x8B, "TH0": 0x8C,
    "TH1": 0x8D, "P1": 0x90, "SCON": 0x98, "SBUF": 0x99, "P2": 0xA0,
    "IE": 0xA8, "P3": 0xB0, "IP": 0xB8, "PSW": 0xD0, "ACC": 0xE0, "B": 0xF0,
}
SHARED_BITS = {
    "IT0": 0x88, "IE0": 0x89, "IT1": 0x8A, "IE1": 0x8B, "TR0": 0x8C,
    "TF0": 0x8D, "TR1": 0x8E, "TF1": 0x8F, "RI": 0x98, "TI": 0x99,
    "RB8": 0x9A, "TB8": 0x9B, "REN": 0x9C, "SM2": 0x9D, "SM1": 0x9E,
    "SM0": 0x9F, "EX0": 0xA8, "ET0": 0xA9, "EX1": 0xAA, "ET1": 0xAB,
    "ES": 0xAC, "EA": 0xAF, "INT0": 0xB2, "INT1": 0xB3, "RXD": 0xB0,
    "TXD": 0xB1, "PX0": 0xB8, "PT0": 0xB9, "PX1": 0xBA, "PT1": 0xBB,
    "PS": 0xBC, "P": 0xD0, "OV": 0xD2, "RS0": 0xD3, "RS1": 0xD4,
    "F0": 0xD5, "AC": 0xD6, "CY": 0xD7,
}
# Only AS's header has these.
AS_ONLY_BITS = {"T0": 0xB4, "T1": 0xB5, "WR": 0xB6, "RD": 0xB7}
BIT_ADDRESSABLE = {n: v for n, v in SHARED_REGISTERS.items() if v % 8 == 0}

# --- operands ----------------------------------------------------------------
#
# An operand is a small tree rendered per spelling: ("num", v), ("sym", name),
# ("bitof", base-operand, n), ("here", offset), ("label", name, offset),
# ("reg", text).


def hex_as(v):
    s = "%XH" % v
    return "0" + s if s[0] in "ABCDEF" else s


def render_num(v, spelling, rng):
    neg = v < 0
    a = -v if neg else v
    if spelling == "sdas":
        text = "0x%x" % a if rng.random() < 0.7 else "%d" % a
    else:
        r = rng.random()
        if r < 0.55:
            text = hex_as(a)
        elif r < 0.8:
            text = "%d" % a
        else:
            text = "%sB" % bin(a)[2:]
    return "-" + text if neg else text


def render(op, spelling, rng):
    kind = op[0]
    if kind == "reg":
        return op[1]
    if kind == "num":
        return render_num(op[1], spelling, rng)
    if kind == "sym":
        return op[1]
    if kind == "imm":
        return "#" + render(op[1], spelling, rng)
    if kind == "not":
        return "/" + render(op[1], spelling, rng)
    if kind == "bitof":
        return "%s.%d" % (render(op[1], spelling, rng), op[2])
    if kind == "here":
        here = "." if spelling == "sdas" else "$"
        off = op[1]
        return here if off == 0 else "%s%+d" % (here, off)
    if kind == "label":
        return op[1] if op[2] == 0 else "%s%+d" % (op[1], op[2])
    raise ValueError(op)


class Gen:
    """Draws operands. `common` programs use only what both references read."""

    def __init__(self, rng, common, labels):
        self.rng = rng
        self.common = common
        self.labels = labels

    def value(self, lo, hi, edges):
        r = self.rng.random()
        if r < 0.3:
            return self.rng.choice(edges)
        return self.rng.randint(lo, hi)

    def direct(self):
        r = self.rng.random()
        if r < 0.3:
            return ("sym", self.rng.choice(sorted(SHARED_REGISTERS)))
        return ("num", self.value(0, 255, [0, 0x7F, 0x80, 0xFF, 0x20, 0x30]))

    def bit(self):
        r = self.rng.random()
        if r < 0.25:
            return ("num", self.value(0, 255, [0, 0x7F, 0x80, 0xFF]))
        if r < 0.45:
            # `CY` is the carry flag to AS where `C` could stand, and the bit
            # D7H to sdas8051, so only AS-only programs use it.
            names = [n for n in sorted(SHARED_BITS) if n != "CY" or not self.common]
            names += [] if self.common else sorted(AS_ONLY_BITS)
            return ("sym", self.rng.choice(names))
        if self.common:
            # sdas reads `ACC.3` only for the names it lists that way, so a
            # common program spells a bit as a number or a shared bit name.
            return ("num", self.rng.randint(0, 255))
        if r < 0.7:
            return ("bitof", ("num", self.rng.randint(0x20, 0x2F)), self.rng.randint(0, 7))
        return ("bitof", ("sym", self.rng.choice(sorted(BIT_ADDRESSABLE))), self.rng.randint(0, 7))

    def imm8(self):
        r = self.rng.random()
        if r < 0.1 and not self.common:
            return ("num", -self.rng.randint(1, 128))
        return ("num", self.value(0, 255, [0, 1, 0x7F, 0x80, 0xFF]))

    def target(self):
        if self.labels and self.rng.random() < 0.8:
            return ("label", self.rng.choice(self.labels), 0)
        return ("here", self.rng.randint(-20, 20))

    def operand(self, kind):
        rng = self.rng
        if kind in ("A", "C", "AB", "DPTR", "@DPTR", "@A+DPTR", "@A+PC"):
            return ("reg", kind)
        if kind == "Rn":
            return ("reg", "R%d" % rng.randint(0, 7))
        if kind == "@Ri":
            return ("reg", "@R%d" % rng.randint(0, 1))
        if kind == "#8":
            return ("imm", self.imm8())
        if kind == "#16":
            if self.labels and rng.random() < 0.3:
                return ("imm", ("label", rng.choice(self.labels), 0))
            return ("imm", ("num", self.value(0, 0xFFFF, [0, 0xFF, 0x100, 0xFFFF])))
        if kind == "dir":
            return self.direct()
        if kind == "bit":
            return self.bit()
        if kind == "/bit":
            return ("not", self.bit())
        return self.target()


def generate(rng, common, mutate):
    """A program: a list of lines, each ("insn", m, ops) or a directive."""
    labels = ["L%d" % i for i in range(rng.randint(1, 6))]
    gen = Gen(rng, common, labels)
    forms = FORMS if common else FORMS + GENERIC * 6
    body = []
    n = rng.randint(3, 40)
    for _ in range(n):
        m, kinds = rng.choice(forms)
        body.append(("insn", m, [gen.operand(k) for k in kinds]))
        r = rng.random()
        if r < 0.05:
            body.append(("db", [("num", rng.randint(0, 255)) for _ in range(rng.randint(1, 4))]))
        elif r < 0.07 and not common:
            body.append(("dw", [("num", rng.randint(0, 0xFFFF))]))
        elif r < 0.09:
            body.append(("ds", rng.randint(1, 8)))
    # Labels go anywhere, the first line included.
    for lab in labels:
        body.insert(rng.randint(0, len(body)), ("label", lab))
    # Code either starts at 0 or just short of a 2 KiB block boundary, which
    # is where AJMP and ACALL targets change block.
    start = rng.choice([0, 0, 0x100, 0x7F0 - rng.randint(0, 40), 0x17F8 - rng.randint(0, 40)])
    lines = [("org", start)]
    if mutate and rng.random() < 0.5:
        # Reserved space pushes relative branches out of reach.
        at = rng.randint(1, len(body))
        body.insert(at, ("ds", rng.choice([100, 120, 127, 128, 130, 200])))
    # The references' images start at the first byte emitted, so reserved
    # space must not come first.
    m, kinds = rng.choice(forms)
    lines.append(("insn", m, [gen.operand(k) for k in kinds]))
    lines += body
    lines.append(("insn", "nop", []))
    if mutate:
        # An out-of-range operand somewhere.
        i = rng.randrange(1, len(lines))
        if lines[i][0] == "insn":
            m, ops = lines[i][1], lines[i][2]
            for j, op in enumerate(ops):
                if op[0] == "imm" and op[1][0] == "num":
                    ops[j] = ("imm", ("num", rng.choice([256, 300, -129, 0x10000])))
                    break
                if op[0] == "num":
                    ops[j] = ("num", rng.choice([256, 0x100, -1, 0x1000]))
                    break
    return lines


def render_program(lines, spelling, rng):
    out = []
    sd = spelling == "sdas"
    for line in lines:
        kind = line[0]
        if kind == "org":
            out.append("\t%s %s" % (".org" if sd else "ORG", render_num(line[1], spelling, rng)))
        elif kind == "label":
            out.append("%s:" % line[1])
        elif kind == "db":
            out.append("\t%s %s" % (".db" if sd else "DB", ",".join(render(o, spelling, rng) for o in line[1])))
        elif kind == "dw":
            out.append("\tDW %s" % ",".join(render(o, spelling, rng) for o in line[1]))
        elif kind == "ds":
            out.append("\t%s %s" % (".ds" if sd else "DS", render_num(line[1], spelling, rng)))
        else:
            m, ops = line[1], line[2]
            text = ",".join(render(o, spelling, rng) for o in ops)
            out.append("\t%s %s" % (m if sd or rng.random() < 0.5 else m.upper(), text))
    return "\n".join(out) + "\n"


# --- running the assemblers --------------------------------------------------


def run(cmd, cwd):
    try:
        p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=30)
    except subprocess.TimeoutExpired:
        return 124, "timeout"
    return p.returncode, p.stdout + p.stderr


def image(path):
    try:
        with open(path, "rb") as fh:
            return fh.read()
    except OSError:
        return None


def rsasm(source, workdir):
    src = os.path.join(workdir, "rs.s")
    out = os.path.join(workdir, "rs.bin")
    with open(src, "w") as fh:
        fh.write(source)
    code, log = run([RSASM, "-a", "8051", "-f", "bin", "-o", out, src], workdir)
    if code != 0:
        return ("ERROR", log.strip().splitlines()[0] if log.strip() else "error")
    return ("OK", image(out) or b"")


def boundary_jumps(listing):
    """Whether AS's listing shows an explicit AJMP or ACALL starting in the
    last two bytes of a 2 KiB block, where AS and sdas test the block of the
    instruction and rsasm, like the CPU, that of the address after it."""
    for line in listing.splitlines():
        head, sep, rest = line.partition(" : ")
        if not sep or "/" not in head:
            continue
        try:
            addr = int(head.split("/")[-1].strip(), 16)
        except ValueError:
            continue
        words = rest.lower().split()
        if (addr & 0x7FF) >= 0x7FE and ("ajmp" in words or "acall" in words):
            return True
    return False


def asl(source, workdir):
    with open(os.path.join(workdir, "as.s"), "w") as fh:
        fh.write('\tinclude "stddef51.inc"\n' + source)
    code, log = run([os.path.join(BIN, "asl"), "-cpu", "8051", "-L", "-i", ASL_INCLUDE,
                     "-o", "as.p", "as.s"], workdir)
    try:
        with open(os.path.join(workdir, "as.lst"), errors="replace") as fh:
            boundary = boundary_jumps(fh.read())
    except OSError:
        boundary = False
    # Without -q, AS ends with a count of errors, so look for a message.
    if code != 0 or "error:" in log:
        first_pass = "passes not started" in log
        text = " ".join(l for l in log.splitlines() if "error:" in l)[:200]
        return ("ERROR", text, boundary, first_pass)
    code, log = run([os.path.join(BIN, "p2bin"), "-q", "-l", "0", "as.p", "as.bin"], workdir)
    if code != 0:
        return ("ERROR", log[:200], boundary, False)
    return ("OK", image(os.path.join(workdir, "as.bin")) or b"", boundary, False)


def sdas(source, workdir):
    with open(os.path.join(workdir, "sd.s"), "w") as fh:
        fh.write("\t.area CSEG (ABS)\n" + source)
    for f in ("sd.rel", "sd.ihx", "sd.bin"):
        try:
            os.unlink(os.path.join(workdir, f))
        except OSError:
            pass
    code, log = run([os.path.join(BIN, "sdas8051"), "-o", "sd.s"], workdir)
    if code == 0:
        code, more = run([os.path.join(BIN, "sdld"), "-i", "sd.ihx", "sd.rel"], workdir)
        log += more
    if code != 0 or "Error" in log or "Warning" in log:
        return ("ERROR", " ".join(l.strip() for l in log.splitlines() if "rror" in l or "arning" in l)[:200])
    code, log = run(["llvm-objcopy", "-I", "ihex", "-O", "binary", "sd.ihx", "sd.bin"], workdir)
    if code != 0:
        return ("ERROR", log[:200])
    return ("OK", image(os.path.join(workdir, "sd.bin")) or b"")


def same(a, b):
    if a[0] != b[0]:
        return False
    return a[0] == "ERROR" or a[1] == b[1]


def classify(as_res, sd_res, rs_as, rs_sd):
    """Returns (class, detail) for one program."""
    cls = classify_results(as_res, sd_res, rs_as, rs_sd)
    if cls[0] in ("rsasm", "split") and as_res[2]:
        return ("boundary", "")
    if cls[0] in ("rsasm", "split") and as_res[0] == "ERROR" and as_res[3] and rs_as[0] == "OK":
        return ("first-pass", "")
    return cls


def classify_results(as_res, sd_res, rs_as, rs_sd):
    if sd_res is None:
        return ("agree", "") if same(rs_as, as_res) else ("rsasm", "as")
    if same(as_res, sd_res):
        if same(rs_as, as_res) and same(rs_sd, sd_res):
            return ("agree", "")
        return ("rsasm", "as" if not same(rs_as, as_res) else "sdas")
    if as_res[0] == "ERROR" and sd_res[0] == "OK":
        if rs_as[0] == "ERROR" and rs_sd[0] == "ERROR":
            return ("lenient", "")
        return ("rsasm", "lenient")
    if as_res[0] == "OK" and sd_res[0] == "ERROR" and same(rs_as, as_res) and same(rs_sd, as_res):
        return ("strict", "")
    return ("split", "")


def one(job):
    seed, common, mutate = job
    rng = random.Random(seed)
    lines = generate(rng, common, mutate)
    as_text = render_program(lines, "as", rng)
    sd_text = render_program(lines, "sdas", rng) if common else None
    with tempfile.TemporaryDirectory() as d:
        as_res = asl(as_text, d)
        rs_as = rsasm(as_text, d)
        sd_res = rs_sd = None
        if common:
            sd_res = sdas(sd_text, d)
            rs_sd = rsasm(sd_text, d)
    cls, detail = classify(as_res, sd_res, rs_as, rs_sd)
    return {
        "seed": seed, "class": cls, "detail": detail, "as_text": as_text, "sd_text": sd_text,
        "as": as_res, "sd": sd_res, "rs_as": rs_as, "rs_sd": rs_sd,
    }


def fmt(res):
    if res is None:
        return "-"
    if res[0] == "ERROR":
        return "ERROR " + res[1]
    return res[1].hex(" ")


def fuzz(args):
    rng = random.Random(args.seed)
    jobs = []
    for i in range(args.count):
        common = not args.as_only and rng.random() < 0.5
        jobs.append((rng.getrandbits(48), common, rng.random() < args.mutations))
    counts = collections.Counter()
    findings = []
    with concurrent.futures.ProcessPoolExecutor(max_workers=args.jobs) as ex:
        for r in ex.map(one, jobs, chunksize=8):
            counts[r["class"]] += 1
            if r["class"] in ("rsasm", "split"):
                findings.append(r)
    print("cases: %d  %s" % (args.count, "  ".join("%s %d" % kv for kv in sorted(counts.items()))))
    findings.sort(key=lambda r: (r["class"] != "rsasm", len(r["as_text"])))
    for r in findings[: args.limit]:
        print("### %s (%s) seed %d" % (r["class"], r["detail"], r["seed"]))
        print("  --- AS spelling")
        print("\n".join("    |" + l for l in r["as_text"].splitlines()))
        if r["sd_text"]:
            print("  --- sdas spelling")
            print("\n".join("    |" + l for l in r["sd_text"].splitlines()))
        print("  AS:           " + fmt(r["as"]))
        print("  sdas:         " + fmt(r["sd"]))
        print("  rsasm (AS):   " + fmt(r["rs_as"]))
        print("  rsasm (sdas): " + fmt(r["rs_sd"]))
    return 1 if counts["rsasm"] else 0


# --- the one-line corpora ----------------------------------------------------


def corpus(spelling):
    """Every form, with operands at their boundaries, one per line. Branches
    are to the location counter plus an offset, so each line stands alone."""
    sd = spelling == "sdas"
    here = "." if sd else "$"
    num = (lambda v: "0x%X" % v) if sd else hex_as
    out = []
    if sd:
        out.append("# Every 8051 instruction as sdas8051 reads it: `0x` numbers, `.` for the")
        out.append("# location counter, and plain numbers for direct and bit addresses.")
        out.append("# Generated by tools/fuzz/mcs51.py corpus sdas.")
    else:
        out.append("# Every 8051 instruction as the Macro Assembler AS reads it, with `12H`")
        out.append("# numbers and `$` for the location counter. Generated by")
        out.append("# tools/fuzz/mcs51.py corpus as; the register and bit names and the")
        out.append("# `A.B` bit notation are in i8051-programs.txt.")

    def choices(kind):
        if kind in ("A", "C", "AB", "DPTR", "@DPTR", "@A+DPTR", "@A+PC"):
            return [kind]
        if kind == "Rn":
            return ["R%d" % n for n in range(8)]
        if kind == "@Ri":
            return ["@R0", "@R1"]
        if kind == "#8":
            return ["#" + num(v) for v in (0, 0x7F, 0x80, 0xFF)]
        if kind == "#16":
            return ["#" + num(v) for v in (0, 0x1234, 0xFFFF)]
        if kind == "dir":
            return [num(v) for v in (0, 0x7F, 0x80, 0xFF)]
        if kind == "bit":
            return [num(v) for v in (0, 0x7F, 0x80, 0xFF)]
        if kind == "/bit":
            return ["/" + num(v) for v in (0, 0x7F, 0x80, 0xFF)]
        if kind == "rel":
            # Forward only: a line stands alone at address 0, and AS refuses
            # a target below it. The backward edges are in the programs.
            return [here, "%s+5" % here]
        if kind == "a11":
            # One target on each of the eight pages the opcode encodes.
            return [num(v) for v in (0, 0x1FF, 0x2AA, 0x355, 0x400, 0x5FF, 0x6C3, 0x7FF)]
        if kind == "a16":
            return [num(v) for v in (0, 0x1234, 0xFFFF)]
        raise ValueError(kind)

    for m, kinds in FORMS:
        # Each operand is walked through its choices with the others at their
        # first, so every register and every edge appears and the corpus
        # stays linear in the number of choices.
        opts = [choices(k) for k in kinds]
        base = [o[0] for o in opts]
        lines = ["\t%s %s" % (m, ",".join(base)) if base else "\t%s" % m]
        for i, o in enumerate(opts):
            for alt in o[1:]:
                ops = base[:i] + [alt] + base[i + 1:]
                lines.append("\t%s %s" % (m, ",".join(ops)))
        out.extend(lines)
    # The edges of relative reach, per instruction length: a displacement is
    # measured from the next instruction, so `$+129` is +127 from a two-byte
    # branch and `$+130` from a three-byte one.
    for line in (
        "\tsjmp %s+129" % here, "\tjc %s+129" % here, "\tdjnz R3,%s+129" % here,
        "\tjb %s,%s+130" % (num(0x20), here), "\tjbc %s,%s+130" % (num(0x20), here),
        "\tcjne A,#%s,%s+130" % (num(1), here), "\tcjne @R1,#%s,%s+130" % (num(1), here),
        "\tdjnz %s,%s+130" % (num(0x30), here),
    ):
        out.append(line)
    return "\n".join(out) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    fz = sub.add_parser("fuzz")
    fz.add_argument("--count", type=int, default=2000)
    fz.add_argument("--seed", type=int, default=1)
    fz.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    fz.add_argument("--mutations", type=float, default=0.25)
    fz.add_argument("--as-only", action="store_true", help="only AS-spelling programs")
    fz.add_argument("--limit", type=int, default=20)
    co = sub.add_parser("corpus")
    co.add_argument("spelling", choices=["as", "sdas"])
    args = ap.parse_args()
    if args.cmd == "corpus":
        sys.stdout.write(corpus(args.spelling))
        return 0
    return fuzz(args)


if __name__ == "__main__":
    sys.exit(main())
