#!/usr/bin/env python3
"""Half-hour coding watchdog for the regalloc push.

Tracks mtimes under vendor/regalloc2 and tools/{raproof,rafuzz}. Every
invocation (intended on a 30-minute cadence) reports what fraction of the
last 30 minutes had source edits, and exits 1 if coding time was a
minority — so a cron/loop can nudge the agent back to writing code
instead of waiting on big compiles or A/B knob sweeps.

  harness/ra_pomodoro.py              # status + pulse log
  harness/ra_pomodoro.py --require 0.5
  # cron example (every 30 min):
  # */30 * * * * cd …/rustc_codegen_pliron && harness/ra_pomodoro.py --require 0.5

Does not run cargo. Pure filesystem pulse.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "target" / "raproof"
PULSE = OUT / "pomodoro_pulse.jsonl"
STATE = OUT / "pomodoro_state.json"

WATCH = [
    ROOT / "vendor" / "regalloc2" / "src",
    ROOT / "tools" / "raproof" / "src",
    ROOT / "tools" / "rafuzz" / "src",
    ROOT / "harness" / "raproof_learn.py",
    ROOT / "harness" / "rafuzz.py",
    ROOT / "harness" / "ra_pomodoro.py",
]


def watched_files() -> list[Path]:
    out: list[Path] = []
    for p in WATCH:
        if p.is_file():
            out.append(p)
        elif p.is_dir():
            for f in p.rglob("*"):
                if f.is_file() and f.suffix in {".rs", ".py", ".md"}:
                    out.append(f)
    return out


def latest_mtime(files: list[Path]) -> float:
    best = 0.0
    for f in files:
        try:
            best = max(best, f.stat().st_mtime)
        except OSError:
            pass
    return best


def load_state() -> dict:
    if STATE.exists():
        return json.loads(STATE.read_text())
    return {"pulses": [], "window_s": 1800}


def save_state(st: dict) -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    STATE.write_text(json.dumps(st, indent=2) + "\n")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--window", type=int, default=1800, help="seconds (default 30m)")
    ap.add_argument(
        "--require",
        type=float,
        default=0.0,
        help="exit 1 if coding fraction of window is below this (e.g. 0.5)",
    )
    ap.add_argument("--pulse-every", type=int, default=60, help="granularity seconds")
    args = ap.parse_args()

    now = time.time()
    files = watched_files()
    mt = latest_mtime(files)
    age = now - mt if mt else 1e9
    coding_now = age < args.pulse_every * 2  # edited within ~2 min

    st = load_state()
    st["window_s"] = args.window
    pulses: list[dict] = st.get("pulses", [])
    pulses.append({"t": now, "coding": coding_now, "latest_age_s": round(age, 1)})
    # keep last 2 windows
    cutoff = now - args.window * 2
    pulses = [p for p in pulses if p["t"] >= cutoff]
    st["pulses"] = pulses
    save_state(st)

    window = [p for p in pulses if p["t"] >= now - args.window]
    if not window:
        frac = 0.0
    else:
        frac = sum(1 for p in window if p["coding"]) / len(window)

    OUT.mkdir(parents=True, exist_ok=True)
    with PULSE.open("a") as f:
        f.write(
            json.dumps(
                {
                    "ts": time.strftime("%Y-%m-%dT%H:%M:%S"),
                    "coding_now": coding_now,
                    "frac_30m": round(frac, 3),
                    "pulses": len(window),
                    "latest_age_s": round(age, 1),
                }
            )
            + "\n"
        )

    status = "CODING" if coding_now else "IDLE"
    print(
        f"pomodoro: {status}  last_edit={age:.0f}s ago  "
        f"coding_frac_window={frac:.0%}  pulses={len(window)}  "
        f"require={args.require:.0%}",
        file=sys.stderr,
    )
    if args.require > 0 and frac < args.require and len(window) >= 5:
        print(
            "pomodoro: FAIL — majority of the last window was not coding; "
            "stop A/B / big compiles, write allocator code.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    # Allow invoking without writing when RA_POMO_DRY=1
    if os.environ.get("RA_POMO_DRY") == "1":
        print("pomodoro dry", file=sys.stderr)
        raise SystemExit(0)
    raise SystemExit(main())
