#!/usr/bin/env python3
"""Sum a PLIRON_FIELD_HEAT file and print each type's field ranking.

    harness/field_order.py HEAT_FILE [--min-weight N]

The file has one "<crate>::<type path>\\t<field>\\t<weight>" line per
(crate, type, field); crates append one after another, so the same pair
can repeat. The backend reads the raw file itself (PLIRON_FIELD_ORDER);
this is for looking at it. Types are listed hottest first; within a type
the fields are in the order the backend prefers (it only swaps fields of
identical size and alignment, so the final layout may differ).
"""
import argparse
import collections
import sys


def load(path):
    heat = collections.defaultdict(lambda: collections.defaultdict(int))
    bad = 0
    with open(path, errors="replace") as f:
        for line in f:
            parts = line.rstrip("\n").split("\t")
            if len(parts) != 3:
                bad += bool(line.strip())
                continue
            ty, field, w = (p.strip() for p in parts)
            if not ty or ty.startswith("#") or not field:
                continue
            try:
                w = int(w)
            except ValueError:
                try:
                    w = int(float(w))
                except (ValueError, OverflowError):
                    bad += 1
                    continue
            if w < 0:
                bad += 1
                continue
            heat[ty][field] += w
    return heat, bad


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("heat_file")
    ap.add_argument("--min-weight", type=int, default=1, metavar="N",
                    help="skip types whose total weight is below N (default 1)")
    a = ap.parse_args()
    try:
        heat, bad = load(a.heat_file)
    except OSError as e:
        sys.exit(f"{a.heat_file}: {e}")
    types = sorted(heat.items(), key=lambda kv: (-sum(kv[1].values()), kv[0]))
    shown = 0
    for ty, fields in types:
        total = sum(fields.values())
        if total < a.min_weight:
            continue
        shown += 1
        print(f"{ty}  total={total}")
        width = max(len(f) for f in fields)
        for f, w in sorted(fields.items(), key=lambda kv: (-kv[1], kv[0])):
            share = 100.0 * w / total if total else 0.0
            print(f"  {f:<{width}}  {w:>14}  {share:5.1f}%")
    print(f"{shown} of {len(types)} types shown"
          + (f", {bad} unparsable lines skipped" if bad else ""), file=sys.stderr)


if __name__ == "__main__":
    main()
