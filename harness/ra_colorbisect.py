#!/usr/bin/env python3
"""Bisect a miscompile under a colouring allocator (`PLIRON_RA_ALGO=mcts`, or
greedy with `PLIRON_RA_GREEDY_COLOR=1`) down to the functions whose
`PLIRON_RA_ALGO_FNS=lo..hi` slice (per-mille of the function-content hash)
breaks a kernel.

    harness/ra_colorbisect.py contains [--env K=V ...] [--exe-args "-benchtime 50ms"]

Every probe rebuilds the bench crate through a keyed copy of the debug dylib
(~5 s) and runs the filtered kernel; a probe fails when the binary exits
nonzero or prints no `Benchmark` line. Prints the narrowest failing slice.
"""
import argparse
import json
import os
import subprocess
import sys

KNOB = "PLIRON_RA_ALGO_FNS"  # `--knob PLIRON_RA_COLOR_FNS` slices colouring only
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DYLIB = os.path.join(ROOT, "target/debug/librustc_codegen_pliron.dylib")


def sh(cmd, env, **kw):
    return subprocess.run(cmd, cwd=ROOT, env=env, text=True, capture_output=True, **kw)


def build(env, kvs):
    be = sh([os.path.join(ROOT, "harness/backend_path.py"), "--dylib", DYLIB, *kvs], env)
    path = be.stdout.strip().splitlines()[-1]
    r = sh(
        ["cargo", "rustc", "-p", "pliron-bench", "--bench", "kernels", "--profile", "bench",
         "--message-format", "json-render-diagnostics", "--", "-Z", f"codegen-backend={path}"],
        env,
    )
    exe = ""
    for line in r.stdout.splitlines():
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if m.get("reason") == "compiler-artifact" and m.get("executable"):
            exe = m["executable"]
    if not exe:
        sys.exit("build failed:\n" + r.stderr[-2000:])
    return exe


def probe(kernel, lo, hi, base_env, base_kvs, args, max_ns=None):
    kv = f"{KNOB}={lo}..{hi}"
    env = dict(base_env, **{KNOB: f"{lo}..{hi}"})
    exe = build(env, [*base_kvs, kv])
    r = sh([exe, *args, "-count", "1", "-cpu", "1", kernel], env)
    bad = r.returncode != 0 or "Benchmark" not in r.stdout
    if not bad and max_ns:
        # `--max-ns N`: a slowdown counts as a failure (ns/op of the first kernel line)
        import re
        m = re.search(r"\s(\d+) ns/op", r.stdout)
        bad = bool(m) and int(m.group(1)) > max_ns
    print(f"  {lo:4d}..{hi:4d}: {'FAIL' if bad else 'ok'} (exit {r.returncode})", flush=True)
    return bad


def main():
    global KNOB
    ap = argparse.ArgumentParser()
    ap.add_argument("kernel")
    ap.add_argument("--env", action="append", default=["PLIRON_RA_ALGO=mcts", "PLIRON_RA_MCTS_SIMS=32"])
    ap.add_argument("--knob", default=KNOB, help="PLIRON_RA_ALGO_FNS (greedy/mcts vs ion) or PLIRON_RA_COLOR_FNS")
    ap.add_argument("--max-ns", type=int, default=None, help="treat ns/op above this as a failure")
    ap.add_argument("--exe-args", default="-benchtime 50ms")
    a = ap.parse_args()
    KNOB = a.knob
    base_env = dict(os.environ)
    for kv in a.env:
        k, v = kv.split("=", 1)
        base_env[k] = v
    args = a.exe_args.split()
    lo, hi = 0, 1000
    if not probe(a.kernel, lo, hi, base_env, a.env, args, a.max_ns):
        print("full range does not fail")
        return 1
    while hi - lo > 1:
        mid = (lo + hi) // 2
        if probe(a.kernel, lo, mid, base_env, a.env, args, a.max_ns):
            hi = mid
        elif probe(a.kernel, mid, hi, base_env, a.env, args, a.max_ns):
            lo = mid
        else:
            print(f"failure needs functions from both {lo}..{mid} and {mid}..{hi}")
            return 2
    print(f"narrowest failing slice: {KNOB}={lo}..{hi}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
