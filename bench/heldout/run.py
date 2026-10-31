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
"""
import argparse, json, math, os, re, shutil, statistics, subprocess, sys

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


def run_once(name, exe):
    """(ns, stdout ok) for one run, cwd = the benchmark dir (its inputs)."""
    src = os.path.join(OUT, "src", name)
    r = sh([exe], cwd=src)
    m = re.search(r"^heldout-ns (\d+)$", r.stderr, re.M)
    exp = os.path.join(src, "benchmark.stdout.expected")
    ok = r.returncode == 0 and m is not None
    if ok and os.path.exists(exp):
        ok = r.stdout.replace("\r\n", "\n") == open(exp).read().replace("\r\n", "\n")
    return (int(m.group(1)) if m else None), ok, r


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=7)
    ap.add_argument("--only")
    ap.add_argument("--skip-build", action="store_true")
    ap.add_argument("--backend", default=os.path.join(ROOT, "target", "debug", DYLIB))
    ap.add_argument("--json", default=os.path.join(OUT, "results.json"))
    ap.add_argument("--variant", action="append", default=[],
                    help="NAME:K=V,K=V -- another pliron build with extra backend env, "
                         "measured interleaved with the others (ratio vs LLVM each)")
    a = ap.parse_args()
    benches = [b for b in BENCHES if not a.only or b[0] in a.only.split(",")]
    os.makedirs(os.path.join(OUT, "src"), exist_ok=True)

    base = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, CARGO_TERM_COLOR="never")
    base.pop("RUSTFLAGS", None)
    envs = {"llvm": base,
            "pliron": dict(base, RUSTFLAGS=f"-Zcodegen-backend={a.backend}")}
    for v in a.variant:
        vname, _, kvs = v.partition(":")
        envs[f"pliron-{vname}"] = dict(envs["pliron"], **dict(kv.split("=", 1) for kv in kvs.split(",") if kv))
    allsides = list(envs)
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
            if a.skip_build:
                exe = os.path.join(OUT, side, name, "release", binary)
                exes[(name, side)] = exe if os.path.exists(exe) else None
            else:
                print(f"build {name} ({side})", flush=True)
                exes[(name, side)] = build(name, binary, side, envs[side])

    res = {}
    for name, _, _ in benches:
        sides = [s for s in allsides if exes.get((name, s))]
        if len(sides) < len(allsides):
            res[name] = {"status": "build failed: " + ",".join(
                s for s in allsides if not exes.get((name, s)))}
            continue
        t = {s: [] for s in sides}
        bad = None
        for r in range(a.rounds):
            for s in (sides if r % 2 == 0 else sides[::-1]):
                ns, ok, p = run_once(name, exes[(name, s)])
                if not ok:
                    bad = f"{s} run failed (rc {p.returncode}): {p.stderr[-300:]!r}"
                    break
                t[s].append(ns)
            if bad:
                break
        if bad:
            res[name] = {"status": bad}
            print(f"  {name}: {bad}")
            continue
        med = {s: statistics.median(t[s]) for s in sides}
        for s in sides[2:]:
            res.setdefault("variants", {}).setdefault(s, {})[name] = med[s] / med["llvm"]
        res[name] = {"status": "ok", "llvm_ms": med["llvm"] / 1e6, "pliron_ms": med["pliron"] / 1e6,
                     "ratio": med["pliron"] / med["llvm"],
                     "llvm_runs_ms": [x / 1e6 for x in t["llvm"]],
                     "pliron_runs_ms": [x / 1e6 for x in t["pliron"]]}
        print(f"  {name:20} llvm {med['llvm']/1e6:9.2f} ms  pliron {med['pliron']/1e6:9.2f} ms"
              f"  {med['pliron']/med['llvm']:.3f}"
              + "".join(f"  {s[7:]} {med[s]/med['llvm']:.3f}" for s in sides[2:]), flush=True)

    rs = [v["ratio"] for v in res.values() if v.get("status") == "ok"]
    gm = math.exp(sum(map(math.log, rs)) / len(rs)) if rs else None
    print(f"geomean pliron/llvm over {len(rs)} of {len(benches)}: "
          + (f"{gm:.3f}" if gm else "n/a"))
    for s, by in res.get("variants", {}).items():
        vs = list(by.values())
        print(f"geomean {s}/llvm over {len(vs)}: {math.exp(sum(map(math.log, vs)) / len(vs)):.3f}")
    with open(a.json, "w") as f:
        json.dump({"rounds": a.rounds, "geomean": gm, "benches": res}, f, indent=1)


if __name__ == "__main__":
    main()
