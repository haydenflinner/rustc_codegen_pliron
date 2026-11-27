#!/usr/bin/env python3
"""Per-instruction-class executed-instruction diff between two icallgrind
dumps and their binaries (harness/elftext.py does the parsing).

Usage: harness/classdiff.py PROG TAG_A TAG_B [--top N] [--klass K]

  TAG_A is the reference (usually `llvm`), TAG_B the candidate.
  Prints: per-class totals for each side and the delta, then the top
  functions and (address->line) locations contributing to the `jump`
  delta (or --klass K).
"""
import collections, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import elftext  # noqa: E402

OUT = os.path.join(os.path.dirname(HERE), "target", "heldout")
TARGET = "aarch64-unknown-linux-musl"


def side(prog, tag):
    cg = os.path.join(OUT, "ir-out", f"{prog}.{tag}.icallgrind")
    if not os.path.exists(cg):
        cg = os.path.join(OUT, "ir-out", f"{prog}.pliron-{tag}.icallgrind")
    exe_tag = "llvm" if tag == "llvm" else f"pliron-{tag}"
    exe = os.path.join(OUT, f"ir-{exe_tag}", prog, TARGET, "release", "benchmark")
    ir, lines = elftext.callgrind_instr(cg)
    text = elftext.Text(exe)
    per_class = collections.Counter()
    fn_class = collections.Counter()  # (fn, klass) -> executed delta side total
    addr_klass = {}  # addr -> klass
    for a, c in ir.items():
        w = text.word(a)
        k = elftext.klass(w) if w is not None else "?"
        addr_klass[a] = k
        per_class[k] += c
        fn_class[(text.sym(a), k)] += c
    return per_class, fn_class, ir, lines, addr_klass, text


def main():
    args = sys.argv[1:]
    top = 40
    want = "jump"
    rest = []
    i = 0
    while i < len(args):
        if args[i] == "--top":
            top = int(args[i + 1]); i += 2
        elif args[i] == "--klass":
            want = args[i + 1]; i += 2
        else:
            rest.append(args[i]); i += 1
    prog, ta, tb = rest

    ca, fa, ira, la, ka, txa = side(prog, ta)
    cb, fb, irb, lb, kb, txb = side(prog, tb)

    print(f"{prog}: executed instructions by class (a={ta}  b={tb}  delta=b-a)")
    klasses = sorted(set(ca) | set(cb), key=lambda k: -(cb[k] - ca[k]))
    tot_d = 0
    for k in klasses:
        d = cb[k] - ca[k]
        tot_d += d
        print(f"  {k:8s} {ca[k]:>12,} -> {cb[k]:>12,}   {d:>+12,}")
    print(f"  {'TOTAL':8s} {sum(ca.values()):>12,} -> {sum(cb.values()):>12,}   {tot_d:>+12,}")

    # Function table for the wanted class.
    fns = collections.Counter()
    for (f, k), c in fb.items():
        if k == want:
            fns[f] += c
    for (f, k), c in fa.items():
        if k == want:
            fns[f] -= c
    print(f"\ntop functions by executed `{want}` delta (b-a):")
    for f, d in fns.most_common(top):
        if d <= 0:
            break
        print(f"  {d:>+10,}  {f[:140]}")

    # Per-line for the wanted class, using each side's own line info.
    line_d = collections.Counter()
    for (a, fl, ln), c in lb.items():
        if ka.get(a) == want:
            line_d[(fl, ln)] += c
    for (a, fl, ln), c in la.items():
        if ka.get(a) == want:
            line_d[(fl, ln)] -= c
    print(f"\ntop lines by executed `{want}` delta:")
    for (fl, ln), d in line_d.most_common(top):
        if d <= 0:
            break
        print(f"  {d:>+10,}  {fl}:{ln}")


if __name__ == "__main__":
    main()
