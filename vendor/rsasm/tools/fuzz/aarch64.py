#!/usr/bin/env python3
"""Differential fuzzer for rsasm's AArch64 backend.

Instructions come from llvm-mc's own decoder: random instruction words, most
of them in the SIMD, floating-point and SVE encoding space, are disassembled
by llvm-mc, and each line it prints is assembled again by llvm-mc, by GNU as
and by rsasm. That reaches every form llvm-mc can print, operands of every
value included, with no table of forms written for the fuzzer: a table
written from the same understanding as the backend's would share its
mistakes. Some cases are then mutated into likely-invalid ones — a lane
index or shift one past its range, a register of the wrong width, an
arrangement swapped for another — so that what rsasm refuses is checked too.

    tools/fuzz/aarch64.py fuzz --count 100000
    tools/fuzz/aarch64.py fuzz --only '^(ld|st)[1-4]' --seed 3
    tools/fuzz/aarch64.py check lines.s          # one instruction per line

A case's result is its instruction word, or ERROR. Classes:

    agree       all three agree.
    rsasm       llvm-mc and GNU as agree and rsasm does not. These are the
                findings; the exit status is 1 when there are any.
    mc-only     GNU as refuses or encodes differently, and rsasm matches
                llvm-mc, which is the reference the AArch64 corpora follow.
                Listed for a person to read.
    gas-only    llvm-mc refuses or differs, rsasm matches GNU as.
    neither     the references disagree and rsasm matches neither.

Environment: RSASM (default target/debug/rsasm under the repository root),
LLVM_MC (default `llvm-mc`), GAS (default `aarch64-elf-as` from
RSASM_ORACLES/bin, else on PATH).
"""

import argparse
import collections
import multiprocessing
import os
import random
import re
import struct
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(ROOT, "tools", "tables"))

import a64  # noqa: E402

RSASM = os.environ.get("RSASM", os.path.join(ROOT, "target", "debug", "rsasm"))
ORACLES = os.environ.get("RSASM_ORACLES", os.path.join(ROOT, "target", "oracles"))
GAS = os.environ.get("GAS") or (
    os.path.join(ORACLES, "bin", "aarch64-elf-as")
    if os.path.exists(os.path.join(ORACLES, "bin", "aarch64-elf-as")) else "aarch64-elf-as")
OBJCOPY = GAS.replace("-as", "-objcopy")
OBJDUMP = GAS.replace("-as", "-objdump")
GAS_MARCH = "-march=armv9.5-a+sve2+sve2-aes+sve2-sha3+sve2-sm4+sve2-bitperm+crypto+sm4+sha3" \
    "+dotprod+i8mm+fp16+fp16fml+bf16+rcpc+rcpc3+sme2+sve2p1+f64mm+f32mm+cssc+the+lut" \
    "+faminmax+fp8+fp8fma+fp8dot2+fp8dot4+sve-b16b16+sme2p1"


# ---------------------------------------------------------------------------
# Running the assemblers, a batch of lines at a time
# ---------------------------------------------------------------------------

def run_mc(lines):
    return a64.assemble(lines)


def _text_words(obj_path, objcopy):
    binp = obj_path + ".bin"
    subprocess.run([objcopy, "-O", "binary", "--only-section=.text", obj_path, binp],
                   capture_output=True)
    try:
        data = open(binp, "rb").read()
    except OSError:
        return []
    return [struct.unpack_from("<I", data, i)[0] for i in range(0, len(data) - 3, 4)]


def _batch(lines, run_once):
    """Per-line results from a tool that refuses a whole file for one bad line:
    run, drop the lines it names, run again."""
    rejected = {}
    for _ in range(8):
        keep = [i for i in range(len(lines)) if i not in rejected]
        words, errors, crashed = run_once([lines[i] for i in keep])
        if crashed:
            return [("crash", crashed)] * len(lines)
        if words is not None:
            if len(words) != len(keep):
                # A line that assembled to nothing or to more than a word: fall
                # back to one at a time.
                if len(lines) == 1:
                    return [("err", "not one word")]
                return sum((_batch([l], run_once) for l in lines), [])
            out = [None] * len(lines)
            for i, w in zip(keep, words):
                out[i] = ("ok", w)
            for i, msg in rejected.items():
                out[i] = ("err", msg)
            return out
        new = False
        for ln, msg in errors.items():
            if 1 <= ln <= len(keep) and keep[ln - 1] not in rejected:
                rejected[keep[ln - 1]] = msg
                new = True
        if not new:
            if len(lines) == 1:
                return [("err", "; ".join(errors.values()) or "failed")]
            mid = len(lines) // 2
            return _batch(lines[:mid], run_once) + _batch(lines[mid:], run_once)
    return [("err", "gave up")] * len(lines)


def run_rsasm(lines):
    def once(ls):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "in.s")
            open(src, "w").write("\n".join(ls) + "\n")
            p = subprocess.run([RSASM, "-a", "aarch64", "-f", "bin", "--hex", src],
                               capture_output=True, text=True)
            if "panicked" in p.stderr:
                return None, {}, "PANIC: " + p.stderr.strip().splitlines()[0][:200]
            if p.returncode == 0:
                data = bytes.fromhex(p.stdout.replace(" ", "").replace("\n", ""))
                return [struct.unpack_from("<I", data, i)[0]
                        for i in range(0, len(data) - 3, 4)], {}, None
            errors, last = {}, None
            for ln in p.stderr.splitlines():
                if ln.startswith("error: "):
                    last = ln[7:]
                m = re.match(r"^\s*--> [^:]*:(\d+):\d+", ln)
                if m and last:
                    errors.setdefault(int(m.group(1)), last)
                    last = None
            return None, errors, None
    return _batch(lines, once)


def run_gas(lines):
    def once(ls):
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "in.s")
            obj = os.path.join(d, "in.o")
            open(src, "w").write("\n".join(ls) + "\n")
            p = subprocess.run([GAS, GAS_MARCH, "-o", obj, src], capture_output=True, text=True)
            errors = {}
            for ln in p.stderr.splitlines():
                m = re.match(r"^[^:]*:(\d+): Error: (.*)$", ln)
                if m:
                    errors.setdefault(int(m.group(1)), m.group(2))
            if errors or p.returncode != 0:
                return None, errors or {0: p.stderr.strip()[:200]}, None
            return _text_words(obj, OBJCOPY), {}, None
    return _batch(lines, once)


def run_mc_results(lines):
    return [("ok", w) if w is not None else ("err", "refused") for w in run_mc(lines)]


# ---------------------------------------------------------------------------
# Cases
# ---------------------------------------------------------------------------

def interesting(text, word=None):
    """A line worth fuzzing: SIMD, floating point or SVE, and not a branch."""
    try:
        mn, atoms = a64.parse_line(text)
    except ValueError:
        return False
    if mn.startswith("b.") or mn in ("b", "bl", "adrp", "cbz", "cbnz", "tbz", "tbnz") or \
            (mn == "adr" and "z" not in text):
        return False
    if re.search(r"\bza|zt0|vgx|\bpn\d", text):
        return False
    # SME2's multi-vector operands, which the backend leaves out: two or more
    # register lists in one instruction.
    if sum(1 for a in atoms if a.kind[0] in ("zlist", "vlist")) > 1:
        return False
    kinds = {a.kind[0] for a in atoms}
    # `ldr x0, #0x10` and friends are PC-relative: the offset printed is not
    # something to assemble back.
    if mn in ("ldr", "ldrsw", "prfm") and "open" not in kinds:
        return False
    return (word is not None and (word >> 25) & 0xf == 0b0010) or \
        bool(kinds & {"v", "vidx", "vidxa", "s", "z", "zidx", "p", "pm", "pz", "vlist",
                         "vlistidx", "zlist", "plist", "fimm"})


def random_words(rng, n):
    """Words weighted towards the SIMD, FP and SVE encoding space: bits 28-25
    of 0x0111 and 0x1111 are AdvSIMD and floating point, 0x0010 is SVE."""
    out = []
    for _ in range(n):
        w = rng.getrandbits(32)
        r = rng.random()
        if r < 0.35:
            w = (w & ~(0xf << 25)) | (0b0111 << 25)
        elif r < 0.55:
            w = (w & ~(0xf << 25)) | (0b1111 << 25)
        elif r < 0.9:
            w = (w & ~(0x7 << 29 | 0xf << 25)) | (0b0010 << 25)
        out.append(w)
    return out


MUTATIONS = []


def mutation(fn):
    MUTATIONS.append(fn)
    return fn


@mutation
def bump_number(text, rng):
    """One number in the line a little past where it was."""
    nums = list(re.finditer(r"#(-?\d+)\b|\[(\d+)\]|\b([vwxzpbhsdq])(\d+)\b", text))
    if not nums:
        return None
    m = rng.choice(nums)
    delta = rng.choice([1, -1, 8, 16, 32, 64])
    if m.group(1) is not None:
        return text[:m.start(1)] + str(int(m.group(1)) + delta) + text[m.end(1):]
    if m.group(2) is not None:
        return text[:m.start(2)] + str(int(m.group(2)) + abs(delta)) + text[m.end(2):]
    return text[:m.start(4)] + str(int(m.group(4)) + abs(delta)) + text[m.end(4):]


@mutation
def swap_arrangement(text, rng):
    arrs = list(re.finditer(r"\.(8b|16b|4h|8h|2s|4s|1d|2d|1q|b|h|s|d)\b", text))
    if not arrs:
        return None
    m = rng.choice(arrs)
    pool = ["8b", "16b", "4h", "8h", "2s", "4s", "1d", "2d"] if m.group(1)[0].isdigit() \
        else ["b", "h", "s", "d"]
    return text[:m.start(1)] + rng.choice(pool) + text[m.end(1):]


@mutation
def swap_width(text, rng):
    regs = list(re.finditer(r"\b([wxbhsdq])(\d+)\b", text))
    if not regs:
        return None
    m = rng.choice(regs)
    return text[:m.start(1)] + rng.choice("wxbhsdq") + text[m.end(1):]


@mutation
def drop_operand(text, rng):
    parts = a64.split_ops(text.split(None, 1)[1]) if " " in text or "\t" in text else []
    if len(parts) < 2:
        return None
    del parts[rng.randrange(len(parts))]
    return text.split(None, 1)[0] + " " + ", ".join(parts)


def gnu_disassemble(words):
    """[(word, text)] as GNU objdump prints the words, for the spellings GNU
    as source is written in: `{v0.16b-v3.16b}`, `#0x12`, `uxtl`."""
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "w.bin")
        with open(path, "wb") as fh:
            fh.write(b"".join(struct.pack("<I", w) for w in words))
        p = subprocess.run([OBJDUMP, "-D", "-b", "binary", "-m", "aarch64", path],
                           capture_output=True, text=True)
    out = []
    for line in p.stdout.splitlines():
        m = re.match(r"^\s*[0-9a-f]+:\t([0-9a-f]{8}) \t(.*)$", line)
        if not m or "undefined" in m.group(2) or ".inst" in m.group(2):
            continue
        text = re.split(r"\s+(?://|;)", m.group(2))[0]
        out.append((int(m.group(1), 16), text))
    return out


def gen_cases(seed, count, only, mutate, source="llvm"):
    rng = random.Random(seed)
    a64.ensure_codes()
    pat = re.compile(only) if only else None
    out = []
    decode = gnu_disassemble if source == "gnu" else a64.disassemble
    while len(out) < count:
        for w, text in decode(random_words(rng, 4096)):
            text = text.split("//")[0].strip().replace("\t", " ")
            if not interesting(text, w):
                continue
            if pat and not pat.search(text.split()[0]):
                continue
            tag = "decoded"
            if rng.random() < mutate:
                fn = rng.choice(MUTATIONS)
                mutated = fn(text, rng)
                if mutated and mutated != text:
                    text, tag = mutated, fn.__name__
            out.append((text, tag))
            if len(out) >= count:
                break
    return out


# ---------------------------------------------------------------------------
# Comparing
# ---------------------------------------------------------------------------

def fmt(r):
    if r[0] == "ok":
        return "%08x" % r[1]
    return r[0].upper() + (": " + r[1] if len(r) > 1 and r[1] else "")


def same(a, b):
    return (a[0] == "ok" and b[0] == "ok" and a[1] == b[1]) or \
        (a[0] != "ok" and b[0] != "ok")


def classify(mc, gas, rs):
    if rs[0] == "crash":
        return "rsasm"
    if same(mc, gas):
        return "agree" if same(rs, mc) else "rsasm"
    if same(rs, mc):
        return "mc-only"
    if same(rs, gas):
        return "gas-only"
    return "neither"


def run_chunk(args):
    lines, use_gas = args
    mc = run_mc_results(lines)
    rs = run_rsasm(lines)
    gas = run_gas(lines) if use_gas else mc
    return list(zip(lines, mc, gas, rs))


def compare_lines(lines, jobs, use_gas=True, chunk=400):
    chunks = [(lines[i:i + chunk], use_gas) for i in range(0, len(lines), chunk)]
    out = []
    with multiprocessing.Pool(jobs) as pool:
        for part in pool.imap(run_chunk, chunks):
            out.extend(part)
    return out


def report(results, tags, limit, out_path):
    groups = collections.defaultdict(list)
    counts = collections.Counter()
    for (line, mc, gas, rs), tag in zip(results, tags):
        cls = classify(mc, gas, rs)
        counts[cls] += 1
        if cls != "agree":
            mn = line.split()[0]
            groups[(cls, mn, tag)].append((line, mc, gas, rs))
    print("cases: %d  " % len(results) + "  ".join("%s: %d" % kv for kv in counts.most_common()))
    order = ["rsasm", "neither", "gas-only", "mc-only"]
    for cls in order:
        items = sorted(((k, v) for k, v in groups.items() if k[0] == cls),
                       key=lambda kv: -len(kv[1]))
        if not items:
            continue
        print("\n== %s: %d groups" % (cls, len(items)))
        for (c, mn, tag), cases in items[:limit]:
            line, mc, gas, rs = min(cases, key=lambda c: len(c[0]))
            print("  %4d  %-10s %-16s %s" % (len(cases), mn, tag, line))
            print("        llvm-mc %s | gas %s | rsasm %s" % (fmt(mc), fmt(gas), fmt(rs)))
    if out_path:
        with open(out_path, "w") as fh:
            for (line, mc, gas, rs), tag in zip(results, tags):
                cls = classify(mc, gas, rs)
                if cls != "agree":
                    fh.write("\t".join([cls, tag, line, fmt(mc), fmt(gas), fmt(rs)]) + "\n")
    return counts["rsasm"]


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("fuzz")
    f.add_argument("--count", type=int, default=20000)
    f.add_argument("--seed", type=int, default=1)
    f.add_argument("--only", default=None, help="mnemonics matching this regex")
    f.add_argument("--mutations", type=float, default=0.25)
    f.add_argument("--source", choices=("llvm", "gnu"), default="llvm",
                   help="whose disassembler writes the cases")
    f.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    f.add_argument("--no-gas", action="store_true")
    f.add_argument("--limit", type=int, default=40)
    f.add_argument("--out", default=None)
    c = sub.add_parser("check")
    c.add_argument("files", nargs="+")
    c.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    c.add_argument("--no-gas", action="store_true")
    c.add_argument("--limit", type=int, default=60)
    c.add_argument("--out", default=None)
    args = ap.parse_args()

    if args.cmd == "fuzz":
        per = args.count // args.jobs + 1
        with multiprocessing.Pool(args.jobs) as pool:
            parts = pool.starmap(gen_cases, [(args.seed * 7919 + i, per, args.only,
                                              args.mutations, args.source)
                                             for i in range(args.jobs)])
        cases = [c for part in parts for c in part][:args.count]
        lines = [c[0] for c in cases]
        tags = [c[1] for c in cases]
    else:
        lines = []
        for path in args.files:
            for l in open(path):
                l = l.strip()
                if l and not l.startswith("#"):
                    lines.append(l)
        tags = ["file"] * len(lines)
    results = compare_lines(lines, args.jobs, use_gas=not args.no_gas)
    bad = report(results, tags, args.limit, args.out)
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
