#!/usr/bin/env python3
"""Find the first optimization unit that makes a test fail.

Usage: harness/bisect.py [--max N] -- <command...>

<command> must rebuild the program with the pliron backend and run it,
exiting 0 when the output is right and non-zero when it is wrong. It is run
with PLIRON_OPT_BISECT=N in the environment, so only the first N pass
applications (one unit = one CLIF step on one function, numbered in a
deterministic order) happen. Cargo does not track PLIRON_* variables, so a
cargo-based command must force the rebuild itself (e.g. `cargo clean -p
<crate>` first, or call rustc directly).

Binary-searches the smallest N whose run fails, then reruns N with
PLIRON_OPT_BISECT_DEBUG=1 and reports which step ran on which function as
unit N, plus the commands that dump and differentially check that function.

Example:
  harness/bisect.py -- sh -c 'rustc -Zcodegen-backend=$BACKEND -O t.rs -o t && ./t'
"""
import argparse, os, re, subprocess, sys

UNIT = re.compile(r"^bisect (\d+) (\S+) (run|skip) ?(.*)$")


def run(cmd, n, debug=False, timeout=None):
    env = dict(os.environ, PLIRON_OPT_BISECT=str(n))
    if debug:
        env["PLIRON_OPT_BISECT_DEBUG"] = "1"
    p = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=timeout)
    return p.returncode == 0, p.stdout + p.stderr


def units(log):
    out = {}
    for line in log.splitlines():
        m = UNIT.match(line.strip())
        if m:
            out.setdefault(int(m.group(1)), (m.group(2), m.group(4)))
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--max", type=int, help="upper bound on units (default: count them with an unlimited run)")
    ap.add_argument("--timeout", type=int, default=1800, help="seconds per run")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd[:1] == ["--"] else a.cmd
    if not cmd:
        ap.error("missing command")

    hi = a.max
    if hi is None:
        ok, log = run(cmd, 10**12, debug=True, timeout=a.timeout)
        if ok:
            sys.exit("the command passes with every optimization on: nothing to bisect")
        hi = max(units(log), default=0)
        print(f"{hi} optimization units in a full run")
    ok, log = run(cmd, 0, timeout=a.timeout)
    if not ok:
        print(log[-4000:])
        sys.exit("the command fails with PLIRON_OPT_BISECT=0 too: not an optimization-pass bug "
                 "(or the build itself is broken)")

    lo = 0  # passes with lo units
    while hi - lo > 1:
        mid = (lo + hi) // 2
        ok, _ = run(cmd, mid, timeout=a.timeout)
        print(f"PLIRON_OPT_BISECT={mid}: {'pass' if ok else 'FAIL'}", flush=True)
        if ok:
            lo = mid
        else:
            hi = mid
    _, log = run(cmd, hi, debug=True, timeout=a.timeout)
    step, fn = units(log).get(hi, ("?", ""))
    print(f"\nfirst bad unit: {hi} = step `{step}` on `{fn or '?'}`")
    print(f"  passes with PLIRON_OPT_BISECT={lo}, fails with {hi}")
    if fn:
        print("next:")
        print(f"  dump its CLIF:   PLIRON_OPT_BISECT={hi} PLIRON_CLIF='{fn}' <command>")
        print(f"  check the step:  PLIRON_DIFFCHECK=step PLIRON_DIFFCHECK_FN='{fn}' <command>")
        print("                   then `clif-check step <step> pliron-diffcheck/*/before.clif`")


if __name__ == "__main__":
    main()
