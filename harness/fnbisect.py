#!/usr/bin/env -S python3 -I
# r[verify verify.bisect.fn]
"""Find which functions' optimization breaks a program (PLIRON_OPT_FNS).

Usage: harness/fnbisect.py [--workdir DIR] [--names FILE] -- CMD...

CMD must exit 0 when the program is good and nonzero when it is bad. It
is run with PLIRON_OPT_FNS set (and inherits everything else), so it must
actually recompile with the backend each time: rustc invocations do,
cargo does not notice env changes — `rm -rf` its target dir (or pass a
fresh CARGO_TARGET_DIR) inside CMD.

Unlike PLIRON_OPT_BISECT (a per-process counter that races across
threads), PLIRON_OPT_FNS selects by symbol name, so this works across
codegen units, parallel lowering and every rustc process of a multi-crate
or bootstrap build. Steps:

  1. CMD with every function optimized (must be bad) while logging the
     selected names (PLIRON_OPT_FNS_LOG) -> the candidate list,
  2. CMD with nothing optimized (must be good),
  3. delta-debugging over the name list (PLIRON_OPT_FNS=@file): halves
     while one half alone stays bad; when the failure needs functions
     from both halves, it finds a minimal member of each.

Prints the culprit set (one name per line on stdout at the end) and the
selector to reproduce it. With one culprit this is ~log2(#fns) runs.
"""
import argparse, os, subprocess, sys, tempfile, time

ALL = "0..0x100000000"
NONE = "0..0"


def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("--workdir", default=None, help="where name lists go (default: a temp dir)")
    ap.add_argument("--names", default=None, help="skip step 1: candidate names file")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd[:1] == ["--"] else a.cmd
    if not cmd:
        ap.error("missing CMD after --")
    wd = a.workdir or tempfile.mkdtemp(prefix="fnbisect-")
    os.makedirs(wd, exist_ok=True)
    runs = [0]

    def run(sel, log=None):
        env = dict(os.environ, PLIRON_OPT_FNS=sel)
        env.pop("PLIRON_OPT_FNS_LOG", None)
        if log:
            env["PLIRON_OPT_FNS_LOG"] = log
        runs[0] += 1
        t = time.time()
        r = subprocess.run(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        bad = r.returncode != 0
        print(f"  run {runs[0]:3}: {sel[:60]:60} -> {'BAD' if bad else 'good'} ({time.time() - t:.0f}s)",
              file=sys.stderr, flush=True)
        return bad

    def bad_set(names):
        p = os.path.join(wd, f"sel{runs[0] + 1}.txt")
        with open(p, "w") as f:
            f.write("\n".join(sorted(names)) + "\n")
        return run("@" + p)

    if a.names:
        names = [l.rsplit("\t", 1)[-1].strip() for l in open(a.names) if l.strip()]
    else:
        log = os.path.join(wd, "all.log")
        if os.path.exists(log):
            os.remove(log)
        if not run(ALL, log):
            sys.exit("fnbisect: CMD is good with every function optimized — nothing to find")
        names = [l.rstrip("\n").split("\t", 1)[1] for l in open(log) if "\t" in l]
    names = sorted(set(names))
    print(f"fnbisect: {len(names)} candidate functions (workdir {wd})", file=sys.stderr)
    if run(NONE):
        sys.exit("fnbisect: CMD is bad with nothing optimized — not an optimization bug")
    if not bad_set(names):
        sys.exit("fnbisect: the logged names alone are good — the failure needs a function the "
                 "log missed (an unlogged process?); rerun with --names")

    def find(c, fixed):
        # Invariant: bad(fixed + c), good(fixed).
        if len(c) == 1:
            return c
        h = len(c) // 2
        x, y = c[:h], c[h:]
        if bad_set(fixed + x):
            return find(x, fixed)
        if bad_set(fixed + y):
            return find(y, fixed)
        cx = find(x, fixed + y)
        return cx + find(y, fixed + cx)

    culprits = find(names, [])
    out = os.path.join(wd, "culprits.txt")
    with open(out, "w") as f:
        f.write("\n".join(culprits) + "\n")
    print(f"fnbisect: {len(culprits)} culprit(s) after {runs[0]} runs; reproduce with "
          f"PLIRON_OPT_FNS=@{out}", file=sys.stderr)
    print("\n".join(culprits))


if __name__ == "__main__":
    main()
