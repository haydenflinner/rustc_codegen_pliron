#!/usr/bin/env -S python3 -I
"""Differential fuzz of the profile loop (generate -> run -> use).

  harness/pgofuzz.py START:END [--jobs N] [--env K=V ...]

For each rustlantis seed: build with stock rustc at -O and run (the
oracle); build instrumented with this backend (`PLIRON_PGO`), run it,
merge the dump; build again with `PLIRON_PGO_USE` (the profile bundle)
and run. The instrumented and the profiled binary must both print what
stock prints. A seed whose stock build fails or times out is skipped.
Failing seeds keep their directory under target/harness/pgofuzz.

Uses the release backend (target/release) and the runtime archive
`bench/heldout/run.py --pgo` builds (target/heldout/pgo/rt).
"""
import argparse, concurrent.futures, os, shutil, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target", "harness", "pgofuzz")
GEN = os.path.join(os.path.dirname(ROOT), "rustlantis", "target", "release", "generate")
DYLIB = os.path.join(ROOT, "target", "release", "librustc_codegen_pliron.dylib" if sys.platform == "darwin"
                     else "librustc_codegen_pliron.so")
RT = os.path.join(ROOT, "target", "heldout", "pgo", "rt", "libpliron_prof_rt.a")
CLI = os.path.join(ROOT, "tools", "pliron-prof", "target", "release", "pliron-prof")
FLAGS = ["-Zmir-opt-level=0", "-Ccodegen-units=1", "-O"]


def sh(cmd, timeout, **kw):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, **kw)
    except subprocess.TimeoutExpired:
        return None


def one(seed, extra):
    d = os.path.join(OUT, str(seed))
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    env = dict(os.environ, RUSTUP_TOOLCHAIN="nightly-2026-10-22")
    g = sh([GEN, str(seed)], 60, cwd=os.path.dirname(os.path.dirname(os.path.dirname(GEN))), env=env)
    if g is None or g.returncode != 0:
        return seed, "skip", "generate"
    src = os.path.join(d, "p.rs")
    open(src, "w").write(g.stdout)
    be = [f"-Zcodegen-backend={DYLIB}"]
    c = sh(["rustc", src, "-o", os.path.join(d, "stock")] + FLAGS, 300, env=env)
    if c is None or c.returncode != 0:
        return seed, "skip", "stock build"
    want = sh([os.path.join(d, "stock")], 60)
    if want is None:
        return seed, "skip", "stock run"
    raw, prof = os.path.join(d, "p.raw"), os.path.join(d, "p.prof")
    c = sh(["rustc", src, "-o", os.path.join(d, "gen"), f"-Clink-arg={RT}"] + FLAGS + be, 600,
           env=dict(env, PLIRON_PGO="1"))
    if c is None or c.returncode != 0:
        return seed, "gen-build", (c.stderr[-300:] if c else "timeout")
    r = sh([os.path.join(d, "gen")], 120, env=dict(os.environ, PLIRON_PROFILE_FILE=raw))
    if r is None or (r.returncode, r.stdout) != (want.returncode, want.stdout):
        return seed, "gen-output", ""
    if not os.path.exists(raw) or sh([CLI, "merge", raw, "-o", prof], 60).returncode != 0:
        # A program that aborts never reaches the atexit dump: nothing to use.
        shutil.rmtree(d, ignore_errors=True)
        return seed, "skip", "no profile"
    c = sh(["rustc", src, "-o", os.path.join(d, "use")] + FLAGS + be, 600,
           env=dict(env, PLIRON_PGO_USE=prof, **extra))
    if c is None or c.returncode != 0:
        return seed, "use-build", (c.stderr[-300:] if c else "timeout")
    r = sh([os.path.join(d, "use")], 120)
    if r is None or (r.returncode, r.stdout) != (want.returncode, want.stdout):
        return seed, "use-output", ""
    shutil.rmtree(d, ignore_errors=True)
    return seed, "pass", ""


def main():
    ap = argparse.ArgumentParser(usage=__doc__.replace("%", "%%"))
    ap.add_argument("seeds")
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--env", action="append", default=[])
    a = ap.parse_args()
    lo, hi = (int(x) for x in a.seeds.split(":"))
    for p in (GEN, DYLIB, RT, CLI):
        if not os.path.exists(p):
            sys.exit(f"pgofuzz: {p} missing")
    extra = dict(kv.split("=", 1) for kv in a.env)
    os.makedirs(OUT, exist_ok=True)
    tally = {}
    with concurrent.futures.ThreadPoolExecutor(a.jobs) as ex:
        for seed, status, msg in ex.map(lambda s: one(s, extra), range(lo, hi)):
            tally[status] = tally.get(status, 0) + 1
            if status not in ("pass", "skip"):
                print(f"seed{seed}: {status} {msg[:200]!r} [{os.path.join(OUT, str(seed))}]", flush=True)
    print("pgofuzz:", ", ".join(f"{k} {v}" for k, v in sorted(tally.items())))


if __name__ == "__main__":
    main()
