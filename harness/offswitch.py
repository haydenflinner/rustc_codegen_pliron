#!/usr/bin/env python3
"""Every CLIF off switch must be safe: run each `PLIRON_<X>=0` over a corpus.

Usage: harness/offswitch.py [--corpus DIR] [--every N] [--only A,B] [--list]
                            [--jobs N] [--out DIR] [--no-verify]

AGENTS.md rule 2: every pass keeps a `PLIRON_<PASS>=0` off switch. That
is only worth something if turning a pass off never crashes or breaks a
later one: `PLIRON_JUMPTHREAD=0` crashed loopidiom (an exit path full of
the forwarders jumpthread would have threaded; 2026-11-16, N4 in
docs/PARITY.md). This runs `clif-check report` once per switch, with
`PLIRON_VERIFY=full` (the verifier after every step), and fails on any
row that is not `ok` (a `panic` row names the step).

The switches are read from the source, so a new one is covered without
editing this script:
  - `Gate::Env("PLIRON_X")` steps in pliron-clif-opt/src/pipeline.rs,
  - `pass_enabled("PLIRON_X")` anywhere in pliron-clif-opt/src,
  - the step categories of `FnFlags::from_env` (`PLIRON_JUMPTHREAD`, ...),
  - `DEFAULT_ON` knobs (pliron-cfg/src/lib.rs) that pliron-clif-opt reads.
IR-pass switches (src/cgupool.rs) are not covered: corpora are recorded
after the IR passes.

A corpus under a `wasm-*` name is replayed as wasm32 (`PLIRON_CLIF_WASM=1`),
as the gate does. Defaults: the std corpus of the main checkout, every 4th
function. Uses target/debug/clif-check (`PLIRON_PROFILE=fast`: target/fast).
"""
import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main_checkout():
    # Corpora live in the main checkout's target/ (worktrees read them there).
    try:
        common = subprocess.run(["git", "rev-parse", "--git-common-dir"], cwd=ROOT,
                                capture_output=True, text=True, check=True).stdout.strip()
        return (ROOT / common).resolve().parent
    except Exception:
        return ROOT


def switches():
    src = ROOT / "pliron-clif-opt" / "src"
    names = set()
    pipe = (src / "pipeline.rs").read_text()
    names |= set(re.findall(r'Gate::Env\("(PLIRON_[A-Z0-9_]+)"\)', pipe))
    m = re.search(r"pub fn from_env\(\) -> FnFlags \{(.*?)\n    \}", pipe, re.S)
    if m:
        names |= set(re.findall(r'on\("(PLIRON_[A-Z0-9_]+)"\)', m.group(1)))
    text = "".join(p.read_text() for p in sorted(src.glob("*.rs")))
    names |= set(re.findall(r'pass_enabled\("(PLIRON_[A-Z0-9_]+)"\)', text))
    cfg = (ROOT / "pliron-cfg" / "src" / "lib.rs").read_text()
    d = re.search(r"pub const DEFAULT_ON: &\[&str\] = &\[(.*?)\];", cfg, re.S)
    if d:
        body = "\n".join(l.split("//")[0] for l in d.group(1).splitlines())
        for n in re.findall(r'"(PLIRON_[A-Z0-9_]+)"', body):
            if f'"{n}"' in text:
                names.add(n)
    return sorted(names)


def run(clif_check, corpus, every, jobs, env_extra, out):
    env = dict(os.environ)
    env.update(env_extra)
    cmd = [str(clif_check), "report", str(corpus), str(out), "--every", str(every)]
    if jobs:
        cmd += ["--jobs", str(jobs)]
    t0 = time.time()
    p = subprocess.run(cmd, env=env, capture_output=True, text=True)
    dt = time.time() - t0
    # A report with failing rows exits nonzero too: read the JSON first.
    if not out.exists():
        return None, dt, (p.stderr or p.stdout)[-400:]
    r = json.loads(out.read_text())
    bad = [(f["status"], f["file"], f.get("msg", "")[:160]) for f in r["functions"] if f["status"] != "ok"]
    return (len(r["functions"]), bad), dt, ""


def main():
    # One line per switch as it finishes, also when piped to a log.
    sys.stdout.reconfigure(line_buffering=True)
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--corpus", default=None)
    ap.add_argument("--every", type=int, default=4)
    ap.add_argument("--only", default=None, help="comma-separated switch names (PLIRON_ prefix optional)")
    ap.add_argument("--list", action="store_true", help="print the switches and exit")
    ap.add_argument("--jobs", type=int, default=0)
    ap.add_argument("--out", default=None, help="keep the per-switch JSON reports here")
    ap.add_argument("--no-verify", action="store_true", help="skip PLIRON_VERIFY=full")
    a = ap.parse_args()

    names = switches()
    if a.only:
        want = {x if x.startswith("PLIRON_") else "PLIRON_" + x for x in a.only.split(",")}
        unknown = want - set(names)
        if unknown:
            print(f"offswitch: not a CLIF off switch: {', '.join(sorted(unknown))}", file=sys.stderr)
            return 2
        names = [n for n in names if n in want]
    if a.list:
        print("\n".join(names))
        return 0

    corpus = Path(a.corpus) if a.corpus else main_checkout() / "target" / "corpus" / "std"
    if not corpus.is_dir():
        print(f"offswitch: no corpus at {corpus} (harness/corpus.sh std records one)", file=sys.stderr)
        return 2
    prof = os.environ.get("PLIRON_PROFILE", "debug")
    clif_check = ROOT / "target" / ("fast" if prof == "fast" else "debug") / "clif-check"
    if not clif_check.exists():
        print(f"offswitch: build clif-check first ({clif_check})", file=sys.stderr)
        return 2
    base_env = {}
    if not a.no_verify:
        base_env["PLIRON_VERIFY"] = "full"
    if corpus.name.startswith("wasm-"):
        base_env["PLIRON_CLIF_WASM"] = "1"
    out = Path(a.out) if a.out else Path(tempfile.mkdtemp(prefix="offswitch-"))
    out.mkdir(parents=True, exist_ok=True)

    print(f"offswitch: {len(names)} switches over {corpus} (every {a.every}"
          f"{', wasm32' if 'PLIRON_CLIF_WASM' in base_env else ''}"
          f"{', PLIRON_VERIFY=full' if 'PLIRON_VERIFY' in base_env else ''})")
    failed = []
    t_all = time.time()
    for n in ["(none)"] + names:
        env = dict(base_env)
        if n != "(none)":
            env[n] = "0"
        res, dt, err = run(clif_check, corpus, a.every, a.jobs, env, out / f"{n.strip('()')}.json")
        if res is None:
            print(f"  FAIL {n:<32} clif-check exited: {err.strip()}")
            failed.append(n)
            continue
        nfn, bad = res
        if bad:
            failed.append(n)
            print(f"  FAIL {n:<32} {len(bad)} of {nfn} rows not ok ({dt:.1f}s)")
            for st, f, msg in bad[:5]:
                print(f"         {st}: {f}: {msg}")
        else:
            print(f"  ok   {n:<32} {nfn} fns ({dt:.1f}s)")
    print(f"offswitch: {len(names) - len([f for f in failed if f != '(none)'])}/{len(names)} switches clean"
          f" in {time.time() - t_all:.0f}s; reports in {out}")
    if failed:
        print(f"offswitch: FAILED: {', '.join(failed)}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
