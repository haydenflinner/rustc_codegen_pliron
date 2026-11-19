#!/usr/bin/env python3
"""Tight feedback loop oracle for proof-guided regalloc.

Runs tools/raproof, parses the summary line, appends a JSONL record, and
prints a one-line fitness score the learning agent optimizes:

  fitness = match / (match + above_opt + 1)   # higher is better
  secondary: fewer_hits (with --prove-fewer), bad_cert must stay 0

Usage:
  harness/raproof_learn.py                  # ion, 40 seeds, prove-fewer
  harness/raproof_learn.py --algo greedy --count 30 --env PLIRON_RA_GREEDY_COLOR=1
  harness/raproof_learn.py --compare        # delta vs previous JSONL row
  harness/raproof_learn.py --rafuzz-quick   # also run rafuzz quick under same env

Does not touch the backend dylib. Rebuild is just cargo --release in
tools/raproof (and tools/rafuzz when --rafuzz-quick).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RAPROOF_DIR = ROOT / "tools" / "raproof"
RAFUZZ_DIR = ROOT / "tools" / "rafuzz"
OUT_DIR = ROOT / "target" / "raproof"
LOG = OUT_DIR / "learn.jsonl"

SUM_RE = re.compile(
    r"raproof: reported=(?P<reported>\d+) skipped=(?P<skipped>\d+) "
    r"above_opt=(?P<above>\d+) match=(?P<match>\d+) gap=(?P<gap>\d+) "
    r"fewer_hits=(?P<fewer>\d+) fewer_tight=(?P<tight>\d+) bad_cert=(?P<bad>\d+)"
)


def run(cmd, env=None, cwd=None):
    e = os.environ.copy()
    if env:
        e.update(env)
    p = subprocess.run(
        cmd, cwd=cwd, env=e, text=True, capture_output=True
    )
    return p.returncode, p.stdout, p.stderr


def build_raproof():
    rc, out, err = run(
        ["cargo", "build", "--release", "-q"],
        cwd=RAPROOF_DIR,
    )
    if rc != 0:
        sys.stderr.write(err or out)
        raise SystemExit(f"raproof build failed: {rc}")


def parse_env_kv(items: list[str]) -> dict[str, str]:
    out = {}
    for it in items:
        if "=" not in it:
            raise SystemExit(f"bad --env {it!r}, want K=V")
        k, v = it.split("=", 1)
        out[k] = v
    return out


def fitness(row: dict) -> float:
    m = row["match"]
    a = row["above_opt"]
    return m / (m + a + 1.0)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--count", type=int, default=40)
    ap.add_argument("--start", type=int, default=0)
    ap.add_argument("--algo", default="ion", choices=("ion", "greedy", "fast"))
    ap.add_argument("--budget", type=int, default=2_000_000)
    ap.add_argument("--no-prove-fewer", action="store_true")
    ap.add_argument("--env", action="append", default=[], help="K=V for child")
    ap.add_argument("--compare", action="store_true", help="print delta vs last log row")
    ap.add_argument("--rafuzz-quick", action="store_true")
    ap.add_argument("--tag", default="", help="note stored in the JSONL row")
    args = ap.parse_args()

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    build_raproof()
    env = parse_env_kv(args.env)
    bin_path = RAPROOF_DIR / "target" / "release" / "raproof"
    cmd = [
        str(bin_path),
        "--start",
        str(args.start),
        "--count",
        str(args.count),
        "--algo",
        args.algo,
        "--budget",
        str(args.budget),
    ]
    if not args.no_prove_fewer:
        cmd.append("--prove-fewer")

    t0 = time.time()
    rc, out, err = run(cmd, env=env, cwd=RAPROOF_DIR)
    wall = time.time() - t0
    sys.stdout.write(out)
    sys.stderr.write(err)
    if rc != 0:
        raise SystemExit(rc)

    m = SUM_RE.search(err)
    if not m:
        raise SystemExit("no raproof summary line in stderr")
    row = {k: int(v) for k, v in m.groupdict().items()}
    # rename keys to stable names
    row = {
        "reported": row["reported"],
        "skipped": row["skipped"],
        "above_opt": row["above"],
        "match": row["match"],
        "gap": row["gap"],
        "fewer_hits": row["fewer"],
        "fewer_tight": row["tight"],
        "bad_cert": row["bad"],
        "algo": args.algo,
        "count": args.count,
        "start": args.start,
        "env": env,
        "tag": args.tag,
        "wall_s": round(wall, 2),
        "fitness": round(fitness(
            {"match": row["match"], "above_opt": row["above"]}
        ), 4),
        "ts": time.strftime("%Y-%m-%dT%H:%M:%S"),
    }

    if args.rafuzz_quick:
        # Light correctness gate under the same PLIRON_* env.
        rc2, o2, e2 = run(
            ["cargo", "run", "--release", "-q", "--", "--count", "200", "--start", "0"],
            env={**env, "PLIRON_RA_ALGO": args.algo if args.algo != "fast" else "fast"},
            cwd=RAFUZZ_DIR,
        )
        # rafuzz prints fails on stderr; treat nonzero or "FAIL" as red
        text = o2 + e2
        fails = len(re.findall(r"\bFAIL\b|checker failed|panicked", text, re.I))
        row["rafuzz_rc"] = rc2
        row["rafuzz_fail_hits"] = fails
        if rc2 != 0 or fails:
            row["rafuzz_ok"] = False
            sys.stderr.write(e2[-4000:])
        else:
            row["rafuzz_ok"] = True

    if row["bad_cert"] != 0:
        row["fitness"] = 0.0

    with LOG.open("a") as f:
        f.write(json.dumps(row) + "\n")

    print(
        f"learn: fitness={row['fitness']:.4f} match={row['match']} "
        f"above={row['above_opt']} fewer={row['fewer_hits']} "
        f"bad={row['bad_cert']} algo={args.algo} → {LOG}",
        file=sys.stderr,
    )

    if args.compare and LOG.exists():
        lines = LOG.read_text().strip().splitlines()
        if len(lines) >= 2:
            prev = json.loads(lines[-2])
            df = row["fitness"] - prev.get("fitness", 0)
            da = row["above_opt"] - prev.get("above_opt", 0)
            dm = row["match"] - prev.get("match", 0)
            print(
                f"learnΔ: fitness {df:+.4f}  match {dm:+d}  above {da:+d} "
                f"(prev tag={prev.get('tag','')!r})",
                file=sys.stderr,
            )

    # Nonzero exit if certificates broke or rafuzz went red.
    if row["bad_cert"] != 0:
        return 2
    if row.get("rafuzz_ok") is False:
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())
