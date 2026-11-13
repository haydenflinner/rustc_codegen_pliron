#!/usr/bin/env -S python3 -I
"""Profile-guided knob search for one program: the slow, thorough PGO loop.

  bench/heldout/tune.py PROG [--rounds N] [--json OUT]

The profile bundle (`PLIRON_PGO_USE`, docs/pgo.md) is one setting for
every program. The programs disagree about several knobs — the hot-site
multiplier wants 13 on rust-json and 0-3 on rust-protobuf, the
profile-guided inliner costs regex instructions — and we own the whole
pipeline: build, run, measure, rebuild. So this searches, for the one
program, greedily over the knobs below, starting from the bundle:

  for each candidate setting:
      build it on top of the best so far, time both interleaved;
      keep it if the timed region's median improves by more than 1%
      and its fastest run by more than 0.5% -- twice, in two independent
      measurements.

then times the winner against plain pliron, LLVM and LLVM with its own
profile. One build per candidate (cached, like `ab.py`), so a full
search costs ~15 builds of the program: opt-in, for the binary you are
going to ship. Needs the program's profile (`run.py --pgo --only PROG`).

The result is a line of environment for the profiled build, which
`run.py --pgo --pgo-env ...` or any `PLIRON_PGO_USE` build takes as is.
"""
import argparse, json, os, statistics, sys, importlib.util

HERE = os.path.dirname(os.path.abspath(__file__))


def load(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, name + ".py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


ab = load("ab")
run = ab.run

# (knob, values to try). Order: the ones that moved programs most first.
SPACE = [
    ("PLIRON_PGO_HOT", ["0", "3", "32"]),
    ("PLIRON_PGO_INLINE", ["0"]),
    ("PLIRON_PGO_COLDSITE", ["0"]),
    ("PLIRON_MEMFAST_SMALL", ["1", "0"]),
    ("PLIRON_PGO_XCGU", ["0", "1000"]),
    ("PLIRON_INLINE", ["640"]),
    ("PLIRON_RA2_SRCVREG", ["1"]),
    ("PLIRON_PLACE_ROT", ["1"]),
    ("PLIRON_PGO_FREQ", ["0", "3"]),
    ("PLIRON_ELABLOOPS", ["2"]),
    ("PLIRON_RA2_POSTOPT", ["15"]),
    ("PLIRON_PGO_EXPECT", ["0"]),
    ("PLIRON_PGO_LOOP", ["0"]),
    # Deeper / larger cross-CGU copies: iterator `fold` chains (73 MIR
    # statements, nine mono items deep) only inline with these; regex
    # and rust-html-rewriter run fewer instructions, rust-protobuf more.
    ("PLIRON_XCGU_DEPTH", ["16"]),
    ("PLIRON_XCGU_SIZE", ["120"]),
    ("PLIRON_EDGEKNOWN", ["1"]),
    # One codegen unit (a RUSTFLAGS entry, not a knob): every body
    # visible to the inliner. rust-html-rewriter and rust-protobuf run
    # ~3.5% fewer instructions under the profile, rust-json 7% more.
    ("@", ["-Ccodegen-units=1"]),
    ("PLIRON_INLINE_COLDONCE", ["1"]),
]


def side(env):
    if not env:
        return "pgo"
    knobs = ",".join(f"{k}={v}" for k, v in sorted(env.items()) if k != "@")
    flags = "@" + env["@"] if "@" in env else ""
    return "pgo-t:" + knobs + flags


def measure(prog, binary, sides, rounds, backend):
    # A search builds dozens of sides at ~240 MB each: keep few.
    ab.prune({ab.side_env(prog, s, backend)[0] for s in sides}, limit=6)
    exes = {s: ab.ensure(prog, binary, s, backend) for s in sides}
    data = {s: [] for s in sides}
    for r in range(rounds):
        for s in (sides if r % 2 == 0 else sides[::-1]):
            data[s].append(ab.once(prog, exes[s]))
    return {s: (statistics.median(x[0] for x in data[s]), min(x[0] for x in data[s]),
                statistics.median(x[1] for x in data[s])) for s in sides}


def main():
    ap = argparse.ArgumentParser(usage=__doc__.replace("%", "%%"))
    ap.add_argument("prog")
    ap.add_argument("--rounds", type=int, default=11)
    ap.add_argument("--json")
    ap.add_argument("--backend", default=os.path.join(run.ROOT, "target", "release", run.DYLIB))
    a = ap.parse_args()
    binary = next((b for n, b, _ in run.BENCHES if n == a.prog), None)
    if binary is None:
        sys.exit(f"tune: unknown program {a.prog}")
    best = {}
    log = []
    for knob, values in SPACE:
        for v in values:
            cand = dict(best, **{knob: v})
            m = measure(a.prog, binary, [side(best), side(cand)], a.rounds, a.backend)
            (bm, bmin, bi), (cm, cmin, ci) = m[side(best)], m[side(cand)]
            keep = cm < bm * 0.99 and cmin < bmin * 0.995
            if keep:
                # Two runs of this search picked different winners: a 1%
                # median over 11 rounds is inside this machine's noise.
                # A candidate has to win a second, independent
                # measurement (no rebuild) before it is kept.
                m2 = measure(a.prog, binary, [side(cand), side(best)], a.rounds, a.backend)
                keep = m2[side(cand)][0] < m2[side(best)][0] * 0.99 and m2[side(cand)][1] < m2[side(best)][1] * 0.995
                if not keep:
                    print(f"{knob}={v}: won once, lost the confirmation run", flush=True)
            print(f"{knob}={v}: median {cm / bm:.3f} min {cmin / bmin:.3f} instr {ci / bi:.3f}"
                  f"{'  <- kept' if keep else ''}", flush=True)
            log.append({"knob": knob, "value": v, "median": cm / bm, "min": cmin / bmin, "instr": ci / bi,
                        "kept": keep})
            if keep:
                best = cand
    sides = ["llvm", "llvm-pgo", "pliron", "pgo"] + ([side(best)] if best else [])
    m = measure(a.prog, binary, sides, max(a.rounds, 15), a.backend)
    b = m["llvm"]
    # The search accepts on 1%; the longer final run decides. A winner
    # that does not beat the plain bundle here was noise.
    if best and m[side(best)][0] >= m["pgo"][0]:
        print("the tuned setting does not beat the bundle in the final run: keeping the bundle")
        sides.remove(side(best))
        best = {}
    shown = " ".join(f"RUSTFLAGS+={v}" if k == "@" else f"{k}={v}" for k, v in sorted(best.items()))
    print(f"\n{a.prog}: tuned environment: {shown or '(the bundle as is)'}")
    for s in sides:
        name = "tuned" if s.startswith("pgo-t:") else s
        print(f"  {name:<9} {m[s][0] / 1e6:7.3f} ms  x{m[s][0] / b[0]:.3f} of llvm  (min x{m[s][1] / b[1]:.3f}, instr x{m[s][2] / b[2]:.3f})")
    if a.json:
        with open(a.json, "w") as f:
            json.dump({"prog": a.prog, "env": best, "log": log,
                       "final": {s: {"median_ns": m[s][0], "min_ns": m[s][1], "instr": m[s][2]} for s in sides}}, f,
                      indent=1)


if __name__ == "__main__":
    main()
