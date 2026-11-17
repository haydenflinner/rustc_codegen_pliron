#!/usr/bin/env python3
"""Differential check of the pliron-IR passes by ablation: record two
corpora of one build, one with an IR pass (or a set) switched off and one
without the switch, and compare them function by function with
`clif-check xcorpus` (docs/miscompile-triage.md, "IR passes").

    harness/xcorpus.py --off PLIRON_MEMCPYOPT=0 [--on K=V ...] [BUILD] [-- CHECK ARGS]

BUILD (default `wasm`):
  wasm            the wasm bench kernels + std (harness/v8/variant.sh, ~1 min a side)
  std             std+alloc+core natively (harness/corpus.sh std)
  rs FILE         one program, rustc -O -C codegen-units=1, native
  cargo DIR       a cargo project (harness/corpus.sh cargo DIR)

`--off` settings go to side A (the reference), `--on` settings to both
sides (the build under test: a bug switch such as
PLIRON_MEMCPYOPT_DSTMARK=0 shows that the check finds it). Corpora go to
target/xcorpus/<tag>/{a,b} (`--tag`, default from the settings), the
report to target/xcorpus/<tag>/report.json; `--keep` reuses corpora that
are there. CHECK ARGS reach clif-check (`--trials N`, `--fn SUBSTR`,
`--every N`, `--jobs N`). `--pipeline` is always on.

Exit status: 0 when no pair mismatches.
"""

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LIB = "librustc_codegen_pliron.dylib" if sys.platform == "darwin" else "librustc_codegen_pliron.so"


def env_with(pairs):
    env = dict(os.environ)
    for p in pairs:
        k, _, v = p.partition("=")
        env[k] = v
    env.setdefault("RUSTC_ICE", str(ROOT / "target" / "ices"))
    return env


def record(build, args, out: Path, pairs, profile):
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    env = env_with(pairs)
    env["PLIRON_CORPUS"] = str(out)
    dylib = ROOT / "target" / profile / LIB
    if build == "wasm":
        env.setdefault("PLIRON_BACKEND", str(dylib))
        name = "xcorpus-" + out.parent.name + "-" + out.name
        cmd = [str(ROOT / "harness/v8/variant.sh"), name]
        subprocess.run(cmd, env=env, check=True, stdout=subprocess.DEVNULL)
    elif build in ("std", "cargo"):
        env["OUT"] = str(out)
        env["PLIRON_PROFILE"] = profile
        cmd = [str(ROOT / "harness/corpus.sh"), build] + args
        # corpus.sh records into OUT itself; PLIRON_CORPUS must not be
        # set twice.
        del env["PLIRON_CORPUS"]
        subprocess.run(cmd, env=env, check=True, stdout=subprocess.DEVNULL)
    elif build == "rs":
        exe = out.parent / (out.name + ".bin")
        cmd = ["rustc", f"-Zcodegen-backend={dylib}", "--edition", "2021", "-O",
               "-C", "codegen-units=1", args[0], "-o", str(exe)]
        subprocess.run(cmd, env=env, check=True, cwd=ROOT,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    else:
        sys.exit(f"xcorpus.py: unknown build {build!r}")
    n = sum(1 for _ in out.rglob("*.clif"))
    print(f"xcorpus.py: {out}: {n} functions", flush=True)
    if n == 0:
        sys.exit("xcorpus.py: nothing recorded")


def main():
    argv = sys.argv[1:]
    check_args = []
    if "--" in argv:
        k = argv.index("--")
        argv, check_args = argv[:k], argv[k + 1:]
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--off", action="append", default=[], help="K=V for side A only")
    ap.add_argument("--on", action="append", default=[], help="K=V for both sides")
    ap.add_argument("--tag")
    ap.add_argument("--keep", action="store_true", help="reuse recorded corpora")
    ap.add_argument("--profile", default=os.environ.get("PLIRON_PROFILE", "debug"))
    ap.add_argument("build", nargs="?", default="wasm")
    ap.add_argument("args", nargs="*")
    o = ap.parse_args(argv)
    if not o.off:
        sys.exit("xcorpus.py: --off K=V is required (the pass to ablate)")
    tag = o.tag or "_".join(p.replace("PLIRON_", "").replace("=", "") for p in o.off + o.on).lower()
    base = ROOT / "target" / "xcorpus" / tag
    a, b = base / "a", base / "b"
    if not (o.keep and a.exists()):
        record(o.build, o.args, a, o.on + o.off, o.profile)
    if not (o.keep and b.exists()):
        record(o.build, o.args, b, o.on, o.profile)
    check = ROOT / "target" / o.profile / "clif-check"
    if not check.exists():
        subprocess.run(["cargo", "build", "-q", "-p", "pliron-clif-check"]
                       + ([] if o.profile == "debug" else ["--profile", o.profile]),
                       cwd=ROOT, check=True)
    env = dict(os.environ)
    if o.build == "wasm":
        env["PLIRON_CLIF_WASM"] = "1"
    cmd = [str(check), "xcorpus", str(a), str(b), str(base / "report.json"), "--pipeline"] + check_args
    r = subprocess.run(cmd, env=env)
    sys.exit(r.returncode)


if __name__ == "__main__":
    main()
