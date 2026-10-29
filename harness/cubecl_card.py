#!/usr/bin/env -S python3 -I
"""The CubeCL parity card: CubeCL's CPU runtime compiling its kernels
through pliron -> Cranelift (CUBECL_PLIRON_CLIF=1) vs through LLVM.

Usage: harness/cubecl_card.py [--measure FILE] [--tests FILE]

Inputs (both written by harness/cubecl_bench.py; see docs/cubecl-burn.md):
  target/parity/cubecl-measure.json
    {"host": str, "load": {"start": f, "end": f},
     "spd": {name: {"llvm": thr, "clif": thr, "unit": str}},   # higher is better
     "cmp": {kernel: {"llvm_us": t, "clif_us": t}},            # per-kernel JIT time
     "mem": {name: {"llvm_kb": n, "clif_kb": n}},              # peak RSS of a compile-heavy run
     "size": {kernel: {"llvm_bytes": n, "clif_bytes": n}}}     # object size (reported, not rated)
  target/parity/cubecl-tests.json
    {suite: {"llvm": {"passed": n, "failed": [test, ...]},
             "clif": {"passed": n, "failed": [test, ...]}}}

Same rating as harness/parity.py: each category reduces to one ratio
r = clif / LLVM (lower is better) and rates 100 + 50 * log2(1/r), clamped
to [0, 150]: 100 is parity, 150 twice as good, 50 half as good. The overall
is the weighted mean of the ratings.

  SPD  kernel throughput, geomean over the measured kernels (LLVM/CLIF rate)
  CMP  kernel JIT time, geomean over kernels both paths compiled
  MEM  peak RSS of the compile-heavy runs, geomean
  COR  (tests failing only under CLIF + 1) / (failing only under LLVM + 1),
       over all suites; a test failing on both paths is not ours and is
       counted for neither side

Writes target/parity/cubecl.json and prints the card.
"""
import argparse, json, math, os, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target/parity")

CARD = [
    # code, name, weight
    ("SPD", "kernel throughput", 40),
    ("CMP", "kernel JIT time", 25),
    ("COR", "correctness", 25),
    ("MEM", "compile memory", 10),
]


def geomean(rs):
    rs = [r for r in rs if r and r > 0]
    return math.exp(sum(map(math.log, rs)) / len(rs)) if rs else None


def rating(r):
    return None if not r else max(0.0, min(150.0, 100 + 50 * math.log2(1 / r)))


def load(path):
    return json.load(open(path)) if os.path.exists(path) else None


def categories(meas, tests):
    """{code: (ratio, detail)} for the categories with data."""
    out = {}
    if meas:
        spd = {k: v["llvm"] / v["clif"] for k, v in meas.get("spd", {}).items()
               if v.get("llvm") and v.get("clif")}
        if spd:
            out["SPD"] = (geomean(spd.values()), f"{len(spd)} rows, geomean of LLVM rate / CLIF rate")
        cmp_ = {k: v["clif_us"] / v["llvm_us"] for k, v in meas.get("cmp", {}).items()
                if v.get("llvm_us") and v.get("clif_us")}
        if cmp_:
            out["CMP"] = (geomean(cmp_.values()), f"{len(cmp_)} kernels, codegen+load wall time")
        mem = {k: v["clif_kb"] / v["llvm_kb"] for k, v in meas.get("mem", {}).items()
               if v.get("llvm_kb") and v.get("clif_kb")}
        if mem:
            out["MEM"] = (geomean(mem.values()), f"peak RSS, {len(mem)} runs")
    if tests:
        only_c = only_l = both = total = 0
        parts = []
        for suite, t in sorted(tests.items()):
            lf, cf = set(t["llvm"]["failed"]), set(t["clif"]["failed"])
            only_c += len(cf - lf)
            only_l += len(lf - cf)
            both += len(cf & lf)
            total += t["llvm"]["passed"] + len(lf)
            parts.append(f"{suite}: {len(cf - lf)} clif-only / {len(lf - cf)} llvm-only / {len(cf & lf)} both")
        out["COR"] = ((only_c + 1) / (only_l + 1), f"{only_c} CLIF-only vs {only_l} LLVM-only failures "
                      f"({both} on both, {total} tests): " + "; ".join(parts))
    return out


def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("--measure", default=os.path.join(OUT, "cubecl-measure.json"))
    ap.add_argument("--tests", default=os.path.join(OUT, "cubecl-tests.json"))
    a = ap.parse_args()
    meas, tests = load(a.measure), load(a.tests)
    cats = categories(meas, tests)
    rows = [(c, n, w) + cats[c] for c, n, w in CARD if c in cats]
    if not rows:
        sys.exit("no data: run harness/cubecl_bench.py first")
    tot = sum(w for _, _, w, _, _ in rows)
    ovr = sum(w * rating(r) for _, _, w, r, _ in rows) / tot
    miss = [c for c, _, _ in CARD if c not in cats]
    print(f"CubeCL parity card: {ovr:.0f} overall" + (f" (no data: {', '.join(miss)})" if miss else ""))
    print()
    print("  " + "  ".join(f"{c:>5}" for c, *_ in rows))
    print("  " + "  ".join(f"{rating(r):5.0f}" for _, _, _, r, _ in rows))
    print()
    for c, n, w, r, d in rows:
        print(f"  {c} {n:<18} weight {w:>3}  clif/LLVM {r:6.3f}  rating {rating(r):4.0f}  {d}")
    if meas:
        ld = meas.get("load", {})
        print(f"\n  host {meas.get('host', '?')}, load average {ld.get('start')} -> {ld.get('end')}")
        sizes = [v["clif_bytes"] / v["llvm_bytes"] for v in meas.get("size", {}).values()
                 if v.get("llvm_bytes") and v.get("clif_bytes")]
        if sizes:
            print(f"  object size clif/LLVM geomean {geomean(sizes):.2f} over {len(sizes)} kernels (not rated)")
    os.makedirs(OUT, exist_ok=True)
    with open(os.path.join(OUT, "cubecl.json"), "w") as f:
        json.dump({"host": (meas or {}).get("host"), "load": (meas or {}).get("load"),
                   "card": {"overall": ovr,
                            "categories": {c: {"weight": w, "ratio": r, "rating": rating(r), "detail": d}
                                           for c, _, w, r, d in rows}}}, f, indent=1)


if __name__ == "__main__":
    main()
