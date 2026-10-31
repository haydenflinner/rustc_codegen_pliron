#!/usr/bin/env python3
"""Measured share of time on stack accesses, pliron vs LLVM, per held-out
program: profile both binaries under samply (census.profile), map every
sample's address to its instruction, and sum samples on instructions that
address the stack (`[sp`/`[x29`, prologue pushes and pops excluded).
Sample skid smears a little onto neighbours; the comparison is what counts.

  bench/heldout/dynstack.py [--only a,b] [--runs N] [--top K]
"""
import argparse, bisect, collections, gzip, json, os, re, subprocess, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import census  # noqa: E402

HELD = census.OUT
PROGS = census.PROGS


def insts(exe):
    """address -> (function, instruction text) for the benchmark binary."""
    txt = subprocess.run([census.objdump(), "-d", "-C", "--no-show-raw-insn", exe],
                         capture_output=True, text=True).stdout
    out, fn = {}, None
    for line in txt.splitlines():
        m = re.match(r"^([0-9a-f]+) <(.+)>:$", line)
        if m:
            fn = census.demangle_key(m.group(2))
            continue
        m = re.match(r"^\s*([0-9a-f]+):\s+(.*)$", line)
        if m and fn:
            out[int(m.group(1), 16)] = (fn, m.group(2).strip())
    return out


def klass(t):
    """Coarse class of an instruction for the excess breakdown."""
    mn = t.split()[0] if t else "?"
    # Prologue/epilogue: frame-record and callee-save pushes/pops, sp moves.
    if ("[sp" in t and ("]!" in t or "], #" in t)) or re.match(r"(sub|add)\s+sp, sp", t) or t == "mov\tx29, sp":
        return "frame"
    if is_stack(t):
        return "stack"
    if mn.startswith(("ld", "ldr")):
        return "load"
    if mn.startswith("st"):
        return "store"
    if mn in ("b", "br", "ret", "bl", "blr") or mn.startswith(("b.", "cb", "tb")):
        return "call" if mn in ("bl", "blr") else "branch"
    if mn in ("mov", "movz", "movk", "movn", "fmov") or (mn == "orr" and "zr" in t):
        return "move"
    if mn in ("cmp", "cmn", "tst", "ccmp", "ccmn") or (mn.startswith("subs") and "zr" in t):
        return "compare"
    if mn.startswith(("csel", "csinc", "csinv", "csneg", "cset", "cinc", "cneg")):
        return "select"
    if mn.startswith(("mul", "madd", "msub", "umulh", "smulh", "udiv", "sdiv", "umaddl", "smaddl")):
        return "muldiv"
    if "." in t.split(",")[0] or mn.startswith(("cmeq", "ushr", "sqxt", "dup", "umov", "ins", "uzp", "zip", "addv", "umaxv")):
        return "simd"
    return "alu"


def is_stack(t):
    return ("[sp" in t or "[x29" in t) and "]!" not in t and "], #" not in t


def addr_samples(path, exe_name="benchmark"):
    """Samples per address (relative to the binary's load base) in `exe`."""
    p = json.load(gzip.open(path))
    libs = p["libs"]
    c = collections.Counter()
    total = 0
    for t in p["threads"]:
        if exe_name not in (t.get("processName") or t.get("name") or ""):
            continue
        ft, fu, rt, sk = t["frameTable"], t["funcTable"], t["resourceTable"], t["stackTable"]
        for s in t["samples"]["stack"]:
            if s is None:
                continue
            f = sk["frame"][s]
            res = fu["resource"][ft["func"][f]]
            lib = rt["lib"][res] if res is not None and res >= 0 else None
            total += 1
            if lib is not None and libs[lib]["name"] == exe_name:
                c[ft["address"][f]] += 1
    return c, total


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", default="")
    ap.add_argument("--runs", type=int, default=60)
    ap.add_argument("--top", type=int, default=5)
    ap.add_argument("--klass", default="", help="list functions by excess samples of this class")
    ap.add_argument("--annotate", default="", help="print FN's instructions with samples, both sides")
    ap.add_argument("--reuse", action="store_true", help="reuse the last dyn-*.json.gz profiles")
    a = ap.parse_args()
    for prog in [p for p in PROGS if not a.only or p in a.only.split(",")]:
        res = {}
        for side in ("llvm", "pliron"):
            exe = os.path.join(HELD, side, prog, "release", "benchmark")
            prof = os.path.join(HELD, f"dyn-{side}-{prog}.json.gz")
            if not (a.reuse and os.path.exists(prof)):
                prof = census.profile(exe, os.path.join(HELD, "src", prog), a.runs, prof)
            c, total = addr_samples(prof)
            ins = insts(exe)
            # samply addresses are offsets from the image base (0x100000000 on macOS).
            base = 0x100000000
            stack = 0
            byfn = collections.Counter()
            cls = collections.Counter()
            fncls = collections.defaultdict(collections.Counter)
            for addr, n in c.items():
                it = ins.get(addr + base) or ins.get(addr)
                cls[klass(it[1]) if it else "outside"] += n
                if it:
                    fncls[it[0]][klass(it[1])] += n
                if it and is_stack(it[1]):
                    stack += n
                    byfn[it[0]] += n
            res[side] = (stack, total, byfn, cls)
            if a.annotate:
                rows = sorted((ad, it) for ad, it in ins.items() if a.annotate in it[0])
                fns = sorted({it[0] for _, it in rows})
                print(f"==== {side} {prog}: {len(rows)} insts in {fns[:3]}")
                for ad, (_, t) in rows:
                    n = c.get(ad - base, 0) + c.get(ad, 0)
                    print(f"  {n:5} {ad:x}  {t}")
            res[side + "_fn"] = fncls
        (ls, lt, lf, lc), (ps, pt, pf, pc) = res["llvm"], res["pliron"]
        print(f"== {prog}: stack-access samples llvm {ls}/{lt} ({100*ls/max(lt,1):.1f}%)  "
              f"pliron {ps}/{pt} ({100*ps/max(pt,1):.1f}%)  excess {ps - ls * pt / max(lt, 1):.0f} "
              f"of pliron's {pt - lt} extra samples")
        ex = {k: pc[k] - lc[k] for k in set(pc) | set(lc)}
        print("   excess samples by class: " + "  ".join(
            f"{k} {v:+d}" for k, v in sorted(ex.items(), key=lambda kv: -kv[1])))
        if a.klass:
            pfc, lfc = res["pliron_fn"], res["llvm_fn"]
            exf = collections.Counter({fn: pfc[fn][a.klass] - lfc.get(fn, collections.Counter())[a.klass] for fn in pfc})
            print(f"   excess {a.klass} samples by function (pliron/llvm; a function LLVM inlined shows llvm 0):")
            for fn, n in exf.most_common(a.top):
                print(f"   {n:+6}  ({pfc[fn][a.klass]}/{lfc.get(fn, collections.Counter())[a.klass]})  {fn[:100]}")
        else:
            for fn, n in pf.most_common(a.top):
                print(f"   {n:6}  (llvm {lf.get(fn, 0):5})  {fn[:100]}")


if __name__ == "__main__":
    main()
