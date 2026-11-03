#!/usr/bin/env -S python3 -I
"""Held-out runtime benchmarks: real Rust programs we never tuned against.

  bench/heldout/run.py [--rounds N] [--only a,b] [--skip-build] [--json OUT]

The programs are sightglass's Rust benchmarks (the Bytecode Alliance's
Wasmtime/Cranelift suite, `are-we-fast-yet/sightglass/benchmarks`): each
reads a fixed input from its directory, does its work between
`bench::start()` and `bench::end()`, and prints a checkable result. Each is
copied to target/heldout/src, built natively at `--release` twice -- stock
rustc (LLVM) and the pliron backend for the whole crate graph (std stays
the prebuilt stock one, as in bench/run.sh) -- with `sightglass-api`
patched to bench/heldout/sightglass-api, which times the region with the
host clock. A build or a stdout mismatch against
`benchmark.stdout.expected` is reported, not timed.

Runs alternate LLVM/pliron for `--rounds` rounds (order flipped every
round) and the per-side median is compared. Results go to
target/heldout/results.json.

`--layouts K` (macOS): relink every side's binary under K random function
orders (ld64 `-order_file`, same objects) and time all of them; a side's
time is the mean of its per-layout medians. Placement alone moves these
ratios by ~3% (a byte-identical hot loop ran 50% slower in one layout), so
single-layout differences below that are not evidence.

`--pads P,P,..`: run every binary once per pad, with an environment
variable of P bytes (`HELDOUT_PAD`), which moves the initial stack and so
the addresses of stack data. One binary's time moves 1-5% across pads
(LLVM's 1-2%), as much as with function order, and random function orders
never vary it. Every side gets the same pads; a side's time is the mean
over (layout, pad) cells of each cell's median.

Every ratio is printed with a bootstrap 95% interval (whole rounds
resampled, so a side's cells stay paired with the others' in the same
round; 1000 replicates) and marked `*` when the interval excludes 1. The
geomean gets the same treatment. `--aa` times the pliron binaries a second
time as the variant `aa`: its ratio to pliron is the noise floor of the run
and should be 1 inside its interval. The power source and Low Power Mode
(macOS `pmset`) are recorded in the JSON and warned about: on battery or
in Low Power Mode the clocks are capped and ratios drift by a few percent.
All per-round samples of every side are saved (`rounds_ns`).
"""
import argparse, json, math, os, random, re, shutil, statistics, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SG = os.environ.get(
    "SIGHTGLASS_BENCHMARKS",
    os.path.join(os.path.dirname(ROOT), "are-we-fast-yet", "sightglass", "benchmarks"),
)
OUT = os.path.join(ROOT, "target", "heldout")
SHIM = os.path.join(ROOT, "bench", "heldout", "sightglass-api")
TOOLCHAIN = "nightly-2026-10-22"
DYLIB = {"darwin": "librustc_codegen_pliron.dylib", "win32": "rustc_codegen_pliron.dll"}.get(
    sys.platform, "librustc_codegen_pliron.so")

# (name, binary, crates to `cargo update` in the copy). All build from
# `rust-benchmark/` with cargo's default release profile. Left out:
# tract-onnx (wasi-nn), hex-simd (wasm SIMD hot path), rust-wat and
# rust-wasmprinter (wasm components: wit-bindgen imports, no native build).
# blind-sig's lockfile pins a proc-macro2 that no longer builds on this
# nightly (E0635).
BENCHES = [
    ("blake3-scalar", "benchmark", []),
    ("blind-sig", "benchmark", ["proc-macro2"]),
    ("pulldown-cmark", "benchmark", []),
    ("regex", "benchmark", []),
    ("rust-compression", "benchmark", []),
    ("rust-html-rewriter", "benchmark", []),
    ("rust-json", "benchmark", []),
    ("rust-protobuf", "benchmark", []),
]
CRATE = "rust-benchmark"


def sh(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def build(name, binary, side, env):
    """Build one side; returns the binary path or None (with the log saved)."""
    src = os.path.join(OUT, "src", name)
    tdir = os.path.join(OUT, side, name)
    if side.startswith("pliron"):
        # Cargo's fingerprint ignores the backend dylib: start clean.
        shutil.rmtree(tdir, ignore_errors=True)
    cmd = ["cargo", "build", "--release", "--target-dir", tdir,
           "--config", f'patch.crates-io.sightglass-api.path="{SHIM}"']
    r = sh(cmd, cwd=os.path.join(src, CRATE), env=env)
    log = os.path.join(OUT, f"build-{side}-{name}.log")
    with open(log, "w") as f:
        f.write(r.stdout + r.stderr)
    if r.returncode != 0:
        print(f"  {name}: {side} build failed (see {log})")
        return None
    exe = os.path.join(tdir, "release", binary + (".exe" if sys.platform == "win32" else ""))
    if not os.path.exists(exe):
        print(f"  {name}: {side}: no {exe}")
        return None
    return exe


def layouts(name, binary, side, env, k):
    """`k` relinks of one built side, each under a random function order."""
    src = os.path.join(src_dir(name), CRATE)
    tdir = os.path.join(OUT, side, name)
    base = os.path.join(tdir, "release", binary)
    # The fresh crate would print nothing: relink it, keeping temporaries.
    sh(["cargo", "clean", "-p", "benchmark", "--release", "--target-dir", tdir], cwd=src, env=env)
    r = sh(["cargo", "rustc", "--release", "--bin", binary, "--target-dir", tdir,
            "--config", f'patch.crates-io.sightglass-api.path="{SHIM}"',
            "--", "--print", "link-args", "-C", "save-temps"], cwd=src, env=env)
    cmd = [l for l in r.stdout.splitlines() if l.strip()]
    if r.returncode != 0 or not cmd:
        print(f"  {name}: {side}: no link command for layouts")
        return [base]
    import random, shlex
    args = shlex.split(cmd[-1])
    nm = sh(["nm", "-n", "--defined-only", base]).stdout.split("\n")
    syms = [l.split()[2] for l in nm if len(l.split()) == 3 and l.split()[1] in ("t", "T")]
    out = []
    for i in range(1, k + 1):
        d = os.path.join(tdir, f"layout{i}")
        os.makedirs(d, exist_ok=True)
        order = list(syms)
        random.Random(i).shuffle(order)
        with open(os.path.join(d, "order.txt"), "w") as f:
            f.write("\n".join(order) + "\n")
        a = list(args)
        a[a.index("-o") + 1] = os.path.join(d, binary)
        a.append(f"-Wl,-order_file,{os.path.join(d, 'order.txt')}")
        if sh(a).returncode == 0:
            out.append(os.path.join(d, binary))
    return out or [base]


def src_dir(name):
    return os.path.join(OUT, "src", name)


def run_once(name, exe, pad=0):
    """(ns, stdout ok) for one run, cwd = the benchmark dir (its inputs)."""
    src = os.path.join(OUT, "src", name)
    r = sh([exe], cwd=src, env=dict(os.environ, HELDOUT_PAD="x" * pad) if pad else None)
    m = re.search(r"^heldout-ns (\d+)$", r.stderr, re.M)
    exp = os.path.join(src, "benchmark.stdout.expected")
    ok = r.returncode == 0 and m is not None
    if ok and os.path.exists(exp):
        ok = r.stdout.replace("\r\n", "\n") == open(exp).read().replace("\r\n", "\n")
    return (int(m.group(1)) if m else None), ok, r


def power_state():
    """{"source": "AC"|"Battery"|"?", "lowpower": bool|None} (macOS `pmset`)."""
    st = {"source": "?", "lowpower": None}
    if sys.platform != "darwin":
        return st
    b = sh(["pmset", "-g", "batt"]).stdout
    st["source"] = "AC" if "AC Power" in b else "Battery" if "Battery Power" in b else "?"
    m = re.search(r"^\s*(?:low)?powermode\s+(\d)", sh(["pmset", "-g"]).stdout, re.M)
    if m:
        st["lowpower"] = m.group(1) == "1"
    return st


def side_stat(rounds, idx):
    """Mean over cells of the per-cell median, over the rounds `idx`."""
    cells = len(rounds[0])
    return statistics.mean(statistics.median(rounds[i][c] for i in idx) for c in range(cells))


def boot_ratio(num, den, reps=1000, seed=0):
    """(ratio, lo, hi): stat(num)/stat(den) with a 95% interval from resampling
    whole rounds (the same rounds for both sides)."""
    n = min(len(num), len(den))
    idx = list(range(n))
    r = side_stat(num, idx) / side_stat(den, idx)
    rng = random.Random(seed)
    bs = []
    for _ in range(reps):
        k = [rng.randrange(n) for _ in range(n)]
        bs.append(side_stat(num, k) / side_stat(den, k))
    bs.sort()
    return r, bs[int(0.025 * reps)], bs[int(0.975 * reps) - 1]


def boot_geomean(pairs, reps=1000, seed=0):
    """(geomean, lo, hi) of per-program ratios, resampling each program's rounds."""
    def gm(xs):
        return math.exp(sum(map(math.log, xs)) / len(xs))
    g = gm([side_stat(a, range(min(len(a), len(b)))) / side_stat(b, range(min(len(a), len(b))))
            for a, b in pairs])
    rng = random.Random(seed)
    bs = []
    for _ in range(reps):
        rs = []
        for a, b in pairs:
            n = min(len(a), len(b))
            k = [rng.randrange(n) for _ in range(n)]
            rs.append(side_stat(a, k) / side_stat(b, k))
        bs.append(gm(rs))
    bs.sort()
    return g, bs[int(0.025 * reps)], bs[int(0.975 * reps) - 1]


def fmt(r, lo, hi):
    return f"{r:.3f} [{lo:.3f},{hi:.3f}]" + ("*" if lo > 1 or hi < 1 else " ")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=7)
    ap.add_argument("--only")
    ap.add_argument("--skip-build", action="store_true")
    ap.add_argument("--backend", default=os.path.join(ROOT, "target", "debug", DYLIB))
    ap.add_argument("--json", default=os.path.join(OUT, "results.json"))
    ap.add_argument("--layouts", type=int, default=0,
                    help="relink each side under K random function orders (macOS)")
    ap.add_argument("--pads", default="0",
                    help="comma-separated environment pad sizes in bytes (stack placement)")
    ap.add_argument("--variant", action="append", default=[],
                    help="NAME:K=V,K=V -- another pliron build with extra backend env, "
                         "measured interleaved with the others (ratio vs LLVM each)")
    ap.add_argument("--aa", action="store_true",
                    help="time the pliron binaries again as the variant `aa` (noise floor)")
    a = ap.parse_args()
    pads = [int(x) for x in a.pads.split(",")]
    power = power_state()
    if power["source"] == "Battery" or power["lowpower"]:
        print(f"WARNING: timing on {power['source']} power, Low Power Mode "
              f"{'on' if power['lowpower'] else 'off'}: clocks may be capped", flush=True)
    benches = [b for b in BENCHES if not a.only or b[0] in a.only.split(",")]
    os.makedirs(os.path.join(OUT, "src"), exist_ok=True)

    base = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, CARGO_TERM_COLOR="never")
    base.pop("RUSTFLAGS", None)
    envs = {"llvm": base,
            "pliron": dict(base, RUSTFLAGS=f"-Zcodegen-backend={a.backend}")}
    for v in a.variant:
        vname, _, kvs = v.partition(":")
        envs[f"pliron-{vname}"] = dict(envs["pliron"], **dict(kv.split("=", 1) for kv in kvs.split(",") if kv))
    allsides = list(envs) + (["pliron-aa"] if a.aa else [])
    exes = {}
    for name, binary, update in benches:
        dst = os.path.join(OUT, "src", name)
        if not a.skip_build:
            shutil.rmtree(dst, ignore_errors=True)
            shutil.copytree(os.path.join(SG, name), dst,
                            ignore=shutil.ignore_patterns("target", "*.wasm"))
            for c in update:
                sh(["cargo", "update", "-q", "-p", c], cwd=os.path.join(dst, CRATE), env=base)
        for side in allsides:
            if side == "pliron-aa":
                exes[(name, side)] = exes.get((name, "pliron"))
                continue
            if a.skip_build:
                exe = os.path.join(OUT, side, name, "release", binary)
                exes[(name, side)] = exe if os.path.exists(exe) else None
            else:
                print(f"build {name} ({side})", flush=True)
                exes[(name, side)] = build(name, binary, side, envs[side])
            if exes[(name, side)] and a.layouts > 0 and sys.platform == "darwin":
                exes[(name, side)] = layouts(name, binary, side, envs[side], a.layouts)
            elif exes[(name, side)]:
                exes[(name, side)] = [exes[(name, side)]]

    res = {}
    for name, _, _ in benches:
        sides = [s for s in allsides if exes.get((name, s))]
        if len(sides) < len(allsides):
            res[name] = {"status": "build failed: " + ",".join(
                s for s in allsides if not exes.get((name, s)))}
            continue
        cells = {s: [(i, pad) for i in range(len(exes[(name, s)])) for pad in pads] for s in sides}
        # rounds[s][r] = the round's samples, one per cell (cell order fixed).
        rounds = {s: [] for s in sides}
        bad = None
        for r in range(a.rounds):
            for s in (sides if r % 2 == 0 else sides[::-1]):
                row = []
                for c in cells[s]:
                    ns, ok, p = run_once(name, exes[(name, s)][c[0]], c[1])
                    if not ok:
                        bad = f"{s} run failed (rc {p.returncode}): {p.stderr[-300:]!r}"
                        break
                    row.append(ns)
                if bad:
                    break
                rounds[s].append(row)
            if bad:
                break
        if bad:
            res[name] = {"status": bad}
            print(f"  {name}: {bad}")
            continue
        # Mean over (layout, pad) cells of each cell's median (one cell: the median).
        idx = range(a.rounds)
        med = {s: side_stat(rounds[s], idx) for s in sides}
        pl = boot_ratio(rounds["pliron"], rounds["llvm"])
        vs = {s: boot_ratio(rounds[s], rounds["pliron"]) for s in sides[2:]}
        for s in sides[2:]:
            res.setdefault("variants", {}).setdefault(s, {})[name] = med[s] / med["llvm"]
        res[name] = {"status": "ok", "llvm_ms": med["llvm"] / 1e6, "pliron_ms": med["pliron"] / 1e6,
                     "ratio": pl[0], "ratio_ci": pl[1:],
                     "variant_vs_pliron": {s: {"ratio": v[0], "ci": v[1:]} for s, v in vs.items()},
                     "llvm_runs_ms": [x / 1e6 for row in rounds["llvm"] for x in row],
                     "pliron_runs_ms": [x / 1e6 for row in rounds["pliron"] for x in row],
                     "rounds_ns": rounds}
        print(f"  {name:20} llvm {med['llvm']/1e6:8.2f} ms  pliron {med['pliron']/1e6:8.2f} ms "
              f" {fmt(*pl)}"
              + "".join(f"  {s[7:]}/pliron {fmt(*v)}" for s, v in vs.items()), flush=True)

    ok = [n for n, _, _ in benches if res.get(n, {}).get("status") == "ok"]
    gm = None
    if ok:
        g = boot_geomean([(res[n]["rounds_ns"]["pliron"], res[n]["rounds_ns"]["llvm"]) for n in ok])
        gm = g[0]
        print(f"geomean pliron/llvm over {len(ok)} of {len(benches)}: {fmt(*g)}")
        for s in res.get("variants", {}):
            vl = boot_geomean([(res[n]["rounds_ns"][s], res[n]["rounds_ns"]["llvm"]) for n in ok])
            vp = boot_geomean([(res[n]["rounds_ns"][s], res[n]["rounds_ns"]["pliron"]) for n in ok])
            print(f"geomean {s}/llvm over {len(ok)}: {fmt(*vl)}  {s[7:]}/pliron {fmt(*vp)}")
    with open(a.json, "w") as f:
        json.dump({"rounds": a.rounds, "pads": pads, "power": power, "geomean": gm,
                   "benches": res}, f, indent=1)


if __name__ == "__main__":
    main()
