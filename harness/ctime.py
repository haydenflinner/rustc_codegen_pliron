#!/usr/bin/env -S python3 -I
"""Compile-time parity: stock rustc (LLVM) vs the pliron backend on fixed inputs.

Usage: harness/ctime.py [--runs N] [--debug-dylib] [--only a,b] [--json OUT]

Inputs (single rustc invocations, no cargo, so nothing is cached):
  kernels         bench/wasm/lib.rs (the 16 bench kernels, ~4 kLOC) -> native rlib
  kernels-wasm    same, --target wasm32-unknown-unknown --emit obj
  regex-syntax    regex-syntax 0.8.x lib from ~/.cargo/registry (~40 kLOC) -> native rlib
  regex-wasm      same, --target wasm32-unknown-unknown --emit obj (one CGU)
  syn             syn 2.x (default features + full, ~50 kLOC) -> native rlib; its
                  deps (proc-macro2, quote, unicode-ident) are prebuilt once with
                  stock rustc into target/ctime-deps. Deep recursive AST types:
                  the input that exposed inliner blow-ups regex-syntax hid.
All at -O, default codegen units. Median of --runs (default 3) wall times.

The backend is the release dylib (`cargo build --release`, built first):
the debug dylib's O1 pass code inflates compile time ~1.3x and would
make every ratio look worse than it is. --debug-dylib measures it anyway.

Results print as a table and go to target/parity/ctime.json. Where the
time goes inside one compile: rerun the pliron side with PLIRON_TIMING=1
(per-step wall, sorted) or `samply record`; for the CLIF pipeline alone,
`clif-check bench target/corpus/std` (no rustc). See docs/PARITY.md.
"""
import argparse, glob, json, os, shutil, statistics, subprocess, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def git_bash():
    """On Windows, bare `bash` resolves to the WSL stub (System32\\bash.exe)
    before Git Bash. Prefer the Git install; fall back to PATH minus stubs."""
    for c in (r"C:\Program Files\Git\bin\bash.exe",
              r"C:\Program Files (x86)\Git\bin\bash.exe",
              shutil.which("bash")):
        if c and os.path.isfile(c) and "system32" not in c.lower() \
                and "windowsapps" not in c.lower():
            return c
    return "bash"


SH = [git_bash()] if os.name == "nt" else []
SO = {"darwin": "dylib", "win32": "dll"}.get(sys.platform, "so")
LIB = "" if sys.platform == "win32" else "lib"
TOOLCHAIN = open(os.path.join(ROOT, "rust-toolchain.toml")).read().split('channel = "')[1].split('"')[0]


def regex_syntax():
    c = sorted(glob.glob(os.path.expanduser("~/.cargo/registry/src/*/regex-syntax-0.8.*")))
    return os.path.join(c[-1], "src/lib.rs") if c else None


def latest(name, major="[0-9]"):
    c = glob.glob(os.path.expanduser(f"~/.cargo/registry/src/*/{name}-{major}*"))
    key = lambda p: [int(x) if x.isdigit() else x for x in p.rsplit("-", 1)[1].replace("+", ".").split(".")]
    return max(c, key=key) if c else None


def syn_args():
    """rustc args for syn, prebuilding its deps with stock rustc."""
    syn = latest("syn", "2.")
    pm, q, ui = (latest(n) for n in ("proc-macro2", "quote", "unicode-ident"))
    if not all((syn, pm, q, ui)):
        return None
    deps = os.path.join(ROOT, "target/ctime-deps")
    os.makedirs(deps, exist_ok=True)
    env = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN)
    def dep(crate, src, ed, *extra):
        if not os.path.exists(os.path.join(deps, f"lib{crate}.rlib")):
            subprocess.run(["rustc", "-O", "--crate-name", crate, "--crate-type", "rlib", "--edition", ed,
                            "--cap-lints", "allow", "--out-dir", deps, "-L", deps, *extra, src + "/src/lib.rs"],
                           env=env, check=True, capture_output=True)
    dep("unicode_ident", ui, "2018")
    dep("proc_macro2", pm, "2021", "--cfg", 'feature="proc-macro"',
        "--extern", f"unicode_ident={deps}/libunicode_ident.rlib")
    dep("quote", q, "2018", "--cfg", 'feature="proc-macro"', "--extern", f"proc_macro2={deps}/libproc_macro2.rlib")
    feats = ["derive", "parsing", "printing", "clone-impls", "proc-macro", "full"]
    return [syn + "/src/lib.rs", "--crate-name", "syn", "--crate-type", "rlib", "--edition", "2021",
            "--cap-lints", "allow", "-L", deps,
            "--extern", f"proc_macro2={deps}/libproc_macro2.rlib", "--extern", f"quote={deps}/libquote.rlib",
            "--extern", f"unicode_ident={deps}/libunicode_ident.rlib"] + sum(
        (["--cfg", f'feature="{f}"'] for f in feats), [])


def inputs():
    k = os.path.join(ROOT, "bench/wasm/lib.rs")
    out = {
        "kernels": [k, "--crate-type", "rlib", "--edition", "2021"],
        "kernels-wasm": [k, "--crate-type", "rlib", "--edition", "2021",
                         "--target", "wasm32-unknown-unknown", "--emit", "obj"],
    }
    rs = regex_syntax()
    if rs:
        out["regex-syntax"] = [rs, "--crate-type", "rlib", "--edition", "2021",
                               "--cfg", 'feature="std"', "--cap-lints", "allow"]
        out["regex-wasm"] = out["regex-syntax"] + ["--target", "wasm32-unknown-unknown", "--emit", "obj"]
    sa = syn_args()
    if sa:
        out["syn"] = sa
    return out


def time_one(args, backend, tmp):
    cmd = ["rustc", "-O", "--out-dir", tmp] + args
    if backend:
        cmd.append(f"-Zcodegen-backend={backend}")
    env = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, RUSTC_ICE=os.path.join(ROOT, "target/ices"))
    t = time.time()
    r = subprocess.run(cmd, env=env, capture_output=True, text=True)
    dt = time.time() - t
    if r.returncode != 0:
        return None, r.stderr[-800:]
    return dt, ""


def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--debug-dylib", action="store_true")
    ap.add_argument("--only", default="")
    ap.add_argument("--json", default=os.path.join(ROOT, "target/parity/ctime.json"))
    a = ap.parse_args()
    prof = "debug" if a.debug_dylib else "release"
    subprocess.run(["cargo", "build", "-q"] + ([] if a.debug_dylib else ["--release"]), cwd=ROOT, check=True)
    backend = os.path.join(ROOT, f"target/{prof}/{LIB}rustc_codegen_pliron.{SO}")
    only = set(filter(None, a.only.split(",")))
    rows = []
    print(f"{'input':<14} {'LLVM':>9} {'pliron':>9} {'ratio':>7}   ({prof} dylib, median of {a.runs})")
    for name, args in inputs().items():
        if only and name not in only:
            continue
        res = {}
        for side, be in (("llvm", None), ("pliron", backend)):
            ts = []
            for _ in range(a.runs):
                with tempfile.TemporaryDirectory() as tmp:
                    dt, err = time_one(args, be, tmp)
                if dt is None:
                    print(f"{name}: {side} failed:\n{err}", file=sys.stderr)
                    break
                ts.append(dt)
            res[side] = statistics.median(ts) if len(ts) == a.runs else None
        ratio = res["pliron"] / res["llvm"] if res["llvm"] and res["pliron"] else None
        rows.append({"input": name, "llvm_s": res["llvm"], "pliron_s": res["pliron"], "ratio": ratio})
        f = lambda s: f"{s * 1000:8.0f}ms" if s else "   failed"
        print(f"{name:<14} {f(res['llvm'])} {f(res['pliron'])} {ratio:6.2f}x" if ratio else
              f"{name:<14} {f(res['llvm'])} {f(res['pliron'])}       -", flush=True)
    os.makedirs(os.path.dirname(a.json), exist_ok=True)
    rev = subprocess.run(SH + ["harness/src-rev.sh"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    json.dump({"rev": rev, "profile": prof, "runs": a.runs, "rows": rows, "when": int(time.time())},
              open(a.json, "w"), indent=1)


if __name__ == "__main__":
    main()
