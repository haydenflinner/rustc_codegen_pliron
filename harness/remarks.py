#!/usr/bin/env -S python3 -I
"""Read optimization remarks (`PLIRON_REMARKS=<prefix>` -> `<prefix>.<pid>`).

  PLIRON_REMARKS=/tmp/r rustc -O -Zcodegen-backend=... x.rs
  harness/remarks.py /tmp/r                      # reasons, counted, per pass
  harness/remarks.py /tmp/r --fn parse_line      # why each call in it stayed a call,
                                                 # why its loops were not vectorized
  harness/remarks.py /tmp/r --callee memchr      # every site that kept a callee

A remark is one decision not to optimize: `pass, function, subject, why`
(`pliron_cfg::remark!`). The inliner writes one per call it kept, with
the numbers the decision used (callee size `n`, `cost` against threshold
`t`, caller `size` against `cap`, growth); `loopvec` one per loop it left
scalar. Names are demangled; `--fn`/`--callee` match substrings of the
demangled name. `bench/heldout/asmvs.py --fn` prints them under its
listing.
"""
import argparse, collections, glob, re, shutil, subprocess, sys


def demangle(names):
    f = shutil.which("rustfilt") or shutil.which("llvm-cxxfilt") or "/opt/homebrew/opt/llvm/bin/llvm-cxxfilt"
    try:
        out = subprocess.run([f], input="\n".join(names), capture_output=True, text=True).stdout.split("\n")
    except OSError:
        return dict(zip(names, names))
    clean = lambda s: re.sub(r"\[[0-9a-f]{8,}\]", "", re.sub(r"::h[0-9a-f]{16}$", "", s))
    return {n: clean(d) for n, d in zip(names, out)}


def read(prefix):
    """[(pass, function, subject, why)] with mangled names, last decision per site kept."""
    rows = collections.OrderedDict()
    for path in sorted(glob.glob(prefix + ".*")):
        for line in open(path, errors="replace"):
            p = line.rstrip("\n").split("\t", 3)
            if len(p) == 4:
                # The inliner rescans a caller after other passes; the last
                # verdict for (pass, function, subject) is the one that held.
                rows[(p[0], p[1], p[2])] = rows.get((p[0], p[1], p[2]), (0, ""))[0] + 1, p[3]
    return [(k[0], k[1], k[2], why, n) for k, (n, why) in rows.items()]


def load(prefix):
    rows = read(prefix)
    names = sorted({r[1] for r in rows} | {r[2] for r in rows})
    d = demangle(names)
    return [(p, d.get(f, f), d.get(s, s), why, n) for p, f, s, why, n in rows]


def reason(why):
    m = re.match(r"^(reject \([^)]*\)|[^=]*?)(?: \w+=|$)", why)
    # Loop remarks end in the loop's header (`block12`, `loop3`) or an inst.
    return re.sub(r"\s+(?:(?:block|loop|inst|v)\d+\b.*|Loop\(.*)$", "", (m.group(1) if m else why).strip())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("prefix")
    ap.add_argument("--fn")
    ap.add_argument("--callee")
    ap.add_argument("--pass", dest="pas")
    ap.add_argument("--all", action="store_true", help="with --fn: also calls to bodies this CGU does not have")
    a = ap.parse_args()
    rows = [r for r in load(a.prefix) if not a.pas or r[0] == a.pas]
    if not rows:
        sys.exit(f"remarks: nothing under {a.prefix}.*")
    if a.fn or a.callee:
        for p, f, s, why, n in rows:
            if (a.fn and a.fn not in f) or (a.callee and a.callee not in s):
                continue
            if not a.all and not a.callee and "external" in why:
                continue  # panics, libc, other CGUs' bodies: never inlinable here
            print(f"{p}\t{f}\n\t{s}\n\t{why}" + (f"  (x{n})" if n > 1 else ""))
        return
    by = collections.Counter((r[0], reason(r[3])) for r in rows)
    for (p, why), n in by.most_common():
        print(f"{n:7d}  {p:<8} {why}")


if __name__ == "__main__":
    main()
