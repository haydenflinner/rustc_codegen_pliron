#!/usr/bin/env -S python3 -I
"""Score a build's block layout against its profile.

  harness/layout_score.py REMARKS_PREFIX [--exe BINARY] [--top N]

A build with `PLIRON_PGO_USE` and `PLIRON_REMARKS=<prefix>` records, for
every function that has profile counts, its emission order scored under
the measured edge frequencies (`layout` remarks): expected taken
unconditional jumps, taken conditional branches and block executions
over the profiled run, and the floor (the mass that leaves each block by
other than its likeliest edge, which no order avoids).

Prints the totals and the functions with the most transfers above their
floor. `--exe` keeps only functions the linked binary still defines
(a body inlined everywhere was still compiled and scored). Two builds
of the same program compare without a timing run: fewer transfers per
block is a better layout.
"""
import argparse, glob, os, subprocess, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("prefix")
    ap.add_argument("--exe")
    ap.add_argument("--top", type=int, default=8)
    a = ap.parse_args()
    live = None
    if a.exe:
        out = subprocess.run(["nm", a.exe], capture_output=True, text=True).stdout
        live = {l.split()[-1].lstrip("_") for l in out.splitlines() if l.split()}
    rows = {}
    for path in sorted(glob.glob(a.prefix + ".*")):
        for line in open(path, errors="replace"):
            p = line.rstrip("\n").split("\t", 3)
            if len(p) == 4 and p[0] == "layout":
                d = dict(kv.split("=") for kv in p[3].split())
                rows[p[1]] = {k: float(v) for k, v in d.items()}
    if live is not None:
        rows = {f: d for f, d in rows.items() if f.lstrip("_") in live}
    if not rows:
        sys.exit("layout_score: no layout remarks")
    t = {k: sum(d[k] for d in rows.values()) for k in next(iter(rows.values()))}
    n = t["blocks"]
    print(f"{len(rows)} functions, {n / 1e6:.2f}M block executions: transfers per block "
          f"{(t['jumps'] + t['branches']) / n:.3f} (floor {t['floor'] / n:.3f})")
    print(f"  taken jumps {t['jumps'] / 1e6:.2f}M ({t['backjumps'] / 1e6:.2f}M backward: unrotated loop latches), "
          f"taken branches {t['branches'] / 1e6:.2f}M ({t['backbranches'] / 1e6:.2f}M backward: rotated loops)")
    import remarks
    names = remarks.demangle(sorted(rows))
    print(f"{'excess':>9} {'jumps':>8} {'branches':>8} {'blocks':>9} {'floor':>8}  function")
    top = sorted(rows.items(), key=lambda kv: -(kv[1]["jumps"] + kv[1]["branches"] - kv[1]["floor"]))[:a.top]
    for f, d in top:
        print(f"{d['jumps'] + d['branches'] - d['floor']:9.0f} {d['jumps']:8.0f} {d['branches']:8.0f} "
              f"{d['blocks']:9.0f} {d['floor']:8.0f}  {names.get(f, f)[:100]}")


if __name__ == "__main__":
    main()
