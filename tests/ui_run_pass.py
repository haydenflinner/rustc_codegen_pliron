#!/usr/bin/env python3
"""Run rustc's directive-free `//@ run-pass` UI tests through the backend.

Usage: tests/ui_run_pass.py [RUST_CHECKOUT] [FILTER]
Writes target/ui/results.json and prints a failure summary. Tests with
directives (aux-build, compile-flags, revisions, ignore/only/needs, ...) are
skipped; edition defaults to 2015 like compiletest.
"""
import collections, concurrent.futures as cf, json, os, re, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RUST = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/work/rust")
FILTER = sys.argv[2] if len(sys.argv) > 2 else ""
OUT = os.path.join(ROOT, "target/ui")
SKIP = re.compile(r"^//@\s*(aux-|compile-flags|revisions|ignore-|only-|needs-|edition|check-run-results|exec-env|run-flags|run-fail|should-fail|force-host|no-prefer-dynamic|unset-exec-env|known-bug|proc-macro|build-aux|incremental)", re.M)
# Tests run with cwd = their own dir, so pin the toolchain explicitly.
os.environ["RUSTUP_TOOLCHAIN"] = re.search(r'channel = "([^"]+)"', open(os.path.join(ROOT, "rust-toolchain.toml")).read()).group(1)
BE = f"-Zcodegen-backend={ROOT}/target/debug/librustc_codegen_pliron.so"
WILD = ["-Clinker-features=-lld", "-Clink-self-contained=-linker", "-Zunstable-options", f"-Clink-arg=-B{ROOT}/target/wild-ld"]

def tests():
    for d, _, fs in os.walk(os.path.join(RUST, "tests/ui")):
        for f in fs:
            p = os.path.join(d, f)
            if not f.endswith(".rs") or FILTER not in p:
                continue
            src = open(p, errors="replace").read()
            if re.search(r"^//@\s*run-pass", src, re.M) and not SKIP.search(src):
                yield p

def run(path, backend=True):
    rel = os.path.relpath(path, os.path.join(RUST, "tests/ui"))
    exe = os.path.join(OUT, "bin", rel.replace("/", "__")[:-3] + ("" if backend else ".llvm"))
    cmd = ["rustc", path, "-o", exe, "-Awarnings", "-Ccodegen-units=1"] + ([BE] + WILD if backend else [])
    try:
        c = subprocess.run(cmd, capture_output=True, text=True, timeout=180, cwd=os.path.dirname(path))
    except subprocess.TimeoutExpired:
        return rel, "compile-timeout", ""
    if c.returncode != 0:
        err = c.stderr
        m = re.search(r"panicked at ([^\n]+)\n([^\n]*)", err)
        if m:
            return rel, "ice", f"{m.group(1)} {m.group(2)}"[:300]
        m = re.search(r"error: linking with .*?\n|undefined reference to `([^']+)'|undefined symbol: (\S+)", err)
        if m and "link" in err:
            return rel, "link", (m.group(1) or m.group(2) or "linker error")[:300]
        m = re.search(r"error(\[E\d+\])?: [^\n]+", err)
        return rel, "compile-error", (m.group(0) if m else err[-300:])[:300]
    try:
        r = subprocess.run([exe], capture_output=True, text=True, timeout=60, cwd=os.path.dirname(path))
    except subprocess.TimeoutExpired:
        return rel, "run-timeout", ""
    finally:
        pass
    if r.returncode != 0:
        tail = (r.stderr.strip().splitlines() or [""])[-1][:200]
        return rel, "run-fail", f"rc={r.returncode} {tail}"
    os.remove(exe)
    return rel, "pass", ""

def main():
    os.makedirs(os.path.join(OUT, "bin"), exist_ok=True)
    ts = sorted(tests())
    print(f"{len(ts)} tests", flush=True)
    res = {}
    with cf.ThreadPoolExecutor(os.cpu_count()) as ex:
        for i, (rel, st, msg) in enumerate(ex.map(run, ts)):
            res[rel] = [st, msg]
            if i % 200 == 0:
                print(i, collections.Counter(v[0] for v in res.values()), flush=True)
    # Failures that also fail with the stock LLVM backend aren't ours.
    bad = [p for p in ts if res[os.path.relpath(p, os.path.join(RUST, "tests/ui"))][0] != "pass"]
    with cf.ThreadPoolExecutor(os.cpu_count()) as ex:
        for rel, st, _ in ex.map(lambda p: run(p, backend=False), bad):
            if st != "pass":
                res[rel][0] = "env-" + res[rel][0]
    json.dump(res, open(os.path.join(OUT, "results.json"), "w"), indent=1, sort_keys=True)
    c = collections.Counter(v[0] for v in res.values())
    print("SUMMARY", dict(c))
    groups = collections.Counter((v[0], re.sub(r"\d+", "N", v[1])[:160]) for v in res.values() if v[0] in ("ice", "link", "compile-error", "run-fail"))
    for (st, msg), n in groups.most_common(40):
        print(f"{n:5} {st:13} {msg}")

main()
