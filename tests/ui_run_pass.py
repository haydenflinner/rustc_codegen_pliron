#!/usr/bin/env python3
"""Run rustc's UI tests through the backend, differentially against stock rustc.

Usage: tests/ui_run_pass.py [RUST_CHECKOUT] [FILTER]
UI_FLAGS adds rustc flags to both runs (e.g. UI_FLAGS=-O).

`run-pass`/`run-fail` tests are built and executed under both compilers; the
results must produce the same exit code, stdout and stderr (the invoked exe
path is normalized to `EXE`, panic thread ids are stripped).

`check-pass`/`build-pass`/`check-fail`/`compile-fail` tests — and every other
file without a run directive — are compile-only: built with `--emit=obj` (or
fully linked for `build-pass`). Both compilers must agree on whether the
compile succeeds; a backend ICE is a bug even when stock rustc also fails,
and `accepted` (pliron compiles what stock rejects) is a real divergence.

The stock compile+run result is cached in target/ui/stock/<hash>.json keyed
by the source plus everything that affects it. Tests whose directives we
can't honor (aux-build, revisions, only/ignore/needs, incremental, ...) are
skipped with a reason; edition defaults to 2015 like compiletest.

Writes target/ui/results.json and prints a failure summary.
"""
import collections, concurrent.futures as cf, hashlib, json, os, re, shlex, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
_sibling = os.path.join(ROOT, "..", "rust")
_default = _sibling if os.path.isdir(os.path.join(_sibling, "tests/ui")) else os.path.expanduser("~/work/rust")
RUST = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("RUST_CHECKOUT", _default)
FILTER = sys.argv[2] if len(sys.argv) > 2 else ""
OUT = os.path.join(ROOT, "target/ui")
# Directives that change what a test needs in ways we don't emulate.
SKIP = re.compile(r"^//@\s*(aux-|revisions|ignore-|only-|needs-|known-bug|proc-macro|build-aux|incremental|min-|should-fail|run-rustfix|ferrocene-|check-cfg)", re.M)
# Tests run with cwd = their own dir, so pin the toolchain explicitly.
os.environ["RUSTUP_TOOLCHAIN"] = re.search(r'channel = "([^"]+)"', open(os.path.join(ROOT, "rust-toolchain.toml")).read()).group(1)
os.environ.setdefault("RUSTC_ICE", os.path.join(ROOT, "target/harness/ices"))
os.makedirs(os.environ["RUSTC_ICE"], exist_ok=True)
SO = "dylib" if sys.platform == "darwin" else "so"
BE = f"-Zcodegen-backend={ROOT}/target/debug/librustc_codegen_pliron.{SO}"
EXTRA = os.environ.get("UI_FLAGS", "").split()  # e.g. UI_FLAGS=-O
WILD = (["-Clinker-features=-lld", "-Clink-self-contained=-linker", "-Zunstable-options",
         f"-Clink-arg=-B{ROOT}/target/wild-ld"]
        if os.path.exists(f"{ROOT}/target/wild-ld/ld") else [])
# compile-flags we can't honor: anything that changes what/where rustc emits
# or pulls in aux artifacts.
BAD_FLAG = re.compile(r"^(--emit|-o|--out-dir|--target|--extern|-L|-l|--sysroot|--print|-Zunpretty|--pretty|--crate-type|error-format|--error-format|--json|aux|-Zdump|-Zemit|-Zno-codegen|-Zno-analysis)")
DIRECTIVE = re.compile(r"^//@\s*([a-zA-Z0-9_-]+)\s*(?::\s*(.*?))?\s*$", re.M)
TID = re.compile(r"thread '([^']+)' \(\d+\) panicked")
# Raw pointers printed with {:p} or as bare usize: macOS addresses are 9-12
# hex digits; longer hex runs are more likely real data than addresses.
ADDR = re.compile(r"0x[0-9a-fA-F]+|(?<![0-9a-zA-Z_.])[0-9a-f]{9,12}(?![0-9a-zA-Z_.])")
LIBTEST = re.compile(r"^running \d+ tests$|^test result:", re.M)


def norm_out(s):
    """Applied to both sides at compare time so the stock cache stays valid."""
    s = ADDR.sub("PTR", s)
    if LIBTEST.search(s):
        # libtest runs tests in arbitrary order.
        s = "\n".join(sorted(s.splitlines()))
    return s


KINDS = re.compile(rb"^//@\s*(run-pass|run-fail|check-pass|build-pass|check-fail|compile-fail)", re.M)


def spec(path):
    """Parses a test's directives into (kind, edition, cflags, rflags, env, unset, skip_reason).
    Files without a recognized directive default to `compile` (compile-agreement)."""
    src = open(path, "rb").read()
    m = KINDS.search(src)
    kind = m.group(1).decode() if m else "compile"
    text = src.decode(errors="replace")
    m = SKIP.search(text)
    if m:
        return (kind, "2015", [], [], {}, [], {}, [], m.group(0).strip())
    edition, cflags, rflags, env, unset, cenv, cunset = "2015", [], [], {}, [], {}, []
    for m in DIRECTIVE.finditer(text):
        d, v = m.group(1), (m.group(2) or "")
        if "{{" in v or "}}" in v:  # compiletest substitutions we don't do
            return (kind, edition, cflags, rflags, env, unset, cenv, cunset, f"{d}: {v}")
        try:
            if d == "edition":
                edition = v
            elif d == "compile-flags":
                cflags += shlex.split(v)
            elif d == "run-flags":
                rflags += shlex.split(v)
            elif d == "exec-env":
                k, _, val = v.partition("=")
                env[k] = val
            elif d == "unset-exec-env":
                unset.append(v.split("=")[0])
            elif d == "rustc-env":
                k, _, val = v.partition("=")
                cenv[k] = val
            elif d == "unset-rustc-env":
                cunset.append(v.split("=")[0])
        except ValueError:
            return (kind, edition, cflags, rflags, env, unset, cenv, cunset, f"bad {d}: {v}")
    bad = next((f for f in cflags if BAD_FLAG.match(f)), None)
    if bad or any(f == "-o" for f in cflags):
        return (kind, edition, cflags, rflags, env, unset, cenv, cunset, f"compile-flags {bad or '-o'}")
    return (kind, edition, cflags, rflags, env, unset, cenv, cunset, None)


def tests():
    for d, _, fs in os.walk(os.path.join(RUST, "tests/ui")):
        for f in fs:
            p = os.path.join(d, f)
            if f.endswith(".rs") and FILTER in p:
                yield p


def classify_compile(err):
    m = re.search(r"panicked at ([^\n]+)\n([^\n]*)", err)
    if m:
        return "ice", f"{m.group(1)} {m.group(2)}"[:300]
    m = re.search(r"error: linking with .*?\n|undefined reference to `([^']+)'|undefined symbol: (\S+)", err)
    if m and "link" in err:
        return "link", (m.group(1) or m.group(2) or "linker error")[:300]
    m = re.search(r"error(\[E\d+\])?: [^\n]+", err)
    return "compile-error", (m.group(0) if m else err[-300:])[:300]


def run_env(sp):
    e = dict(os.environ)
    e.update(sp["env"])
    for k in sp["unset"]:
        e.pop(k, None)
    return e


def build_and_run(path, sp, exe, backend):
    """Returns (status, msg, run) where run is (rc, stdout, stderr) or None.
    Compile-only kinds emit an object (`--emit=obj`) and don't run it."""
    cmd = ["rustc", os.path.abspath(path), "--edition", sp["edition"], "-Awarnings", "-Ccodegen-units=1"]
    if backend:
        cmd += [BE] + WILD + EXTRA
    else:
        cmd += EXTRA
    # check-fail/-pass and undirected files stop at codegen; build-pass links.
    emit = ["-o", exe] if sp["kind"] in ("run-pass", "run-fail", "build-pass") else ["--emit=obj", "-o", exe]
    cmd += sp["cflags"] + emit
    cenv = dict(os.environ)
    cenv.update(sp["cenv"])
    for k in sp["cunset"]:
        cenv.pop(k, None)
    try:
        c = subprocess.run(cmd, capture_output=True, text=True, timeout=180,
                           cwd=os.path.dirname(path), env=cenv)
    except subprocess.TimeoutExpired:
        return "compile-timeout", "", None
    if c.returncode != 0:
        return (*classify_compile(c.stderr), None)
    if not os.path.exists(exe):
        return "skipped", "no output produced", None
    if "--emit=obj" in emit:
        return "compiled", "", (0, "", "")
    return exec_bin(path, sp, exe)


def exec_bin(path, sp, exe):
    try:
        r = subprocess.run([exe] + sp["rflags"], capture_output=True, text=True,
                           timeout=60, cwd=os.path.dirname(path), env=run_env(sp))
    except subprocess.TimeoutExpired:
        return "run-timeout", "", None
    # Normalize the invoked path and panic thread ids so argv[0]/current_exe
    # prints and panic lines compare equal.
    out = TID.sub(r"thread '\1' panicked", r.stdout.replace(exe, "EXE"))
    err = TID.sub(r"thread '\1' panicked", r.stderr.replace(exe, "EXE"))
    return "ran", "", (r.returncode, out, err)


STOCK_CACHE_VERSION = 3  # bump when normalization or comparison semantics change


def stock_result(path, sp):
    """Compile+run with stock rustc, cached by source+flags hash."""
    key = hashlib.sha256(
        open(path, "rb").read()
        + json.dumps([STOCK_CACHE_VERSION, sp["kind"], sp["edition"], sp["cflags"], sp["rflags"],
                      sp["env"], sp["unset"], EXTRA]
                     + ([sp["cenv"], sp["cunset"]] if sp["cenv"] or sp["cunset"] else []),
                     sort_keys=True).encode()
    ).hexdigest()[:32]
    cache = os.path.join(OUT, "stock", key + ".json")
    if os.path.exists(cache):
        res = json.load(open(cache))
        # Tuples serialize as lists; comparisons below index but the outer
        # `got != want` check needs the same shape.
        if res[2] is not None:
            res[2] = tuple(res[2])
        return res
    exe = os.path.join(OUT, "stock", key)
    res = build_and_run(path, sp, exe, False)
    if os.path.exists(exe):
        os.remove(exe)
    tmp = cache + ".tmp"
    json.dump(res, open(tmp, "w"))
    os.replace(tmp, cache)
    return res


def first_diff(want, got):
    for i, (a, b) in enumerate(zip(want.splitlines(), got.splitlines())):
        if a != b:
            return f"line {i + 1}: stock {a!r} vs pliron {b!r}"[:300]
    return f"stock {len(want.splitlines())} lines vs pliron {len(got.splitlines())} lines"


def run(path):
    rel = os.path.relpath(path, os.path.join(RUST, "tests/ui"))
    s = spec(path)
    kind, edition, cflags, rflags, env, unset, cenv, cunset, why = s
    sp = {"kind": kind, "edition": edition, "cflags": cflags, "rflags": rflags, "env": env,
          "unset": unset, "cenv": cenv, "cunset": cunset}
    if why:
        return rel, "skipped", why
    ref_st, ref_msg, want = stock_result(path, sp)
    exe = os.path.join(OUT, "bin", rel.replace("/", "__")[:-3])
    st, msg, got = build_and_run(path, sp, exe, True)
    if kind not in ("run-pass", "run-fail"):
        # Compile-only kinds: agreement on compile outcome is the whole
        # contract. A backend ICE is a bug even where stock also fails.
        for p in (exe, exe + ".o"):
            if os.path.exists(p):
                os.remove(p)
        if st == "ice":
            return rel, "ice", msg
        if st == "compile-timeout" and ref_st != "compile-timeout":
            # A pliron hang where stock finished is a divergence even if
            # stock's finish was an error (possibly pre-codegen).
            return rel, "compile-timeout", f"(stock: {ref_st} {ref_msg})"
        if (st == "compiled") == (ref_st == "compiled"):
            return rel, "pass", ""
        if ref_st == "compiled":
            return rel, "no-output" if st == "skipped" else st, msg
        if st == "skipped":
            return rel, "skipped", f"{msg} (stock: {ref_st})"
        return rel, "accepted", f"compiles under pliron, stock {ref_st}: {ref_msg}"
    if got is None:
        if os.path.exists(exe):
            os.remove(exe)
        if st == "skipped":
            return rel, "skipped", msg
        if want is None and ref_st != "run-timeout":
            return rel, "env-" + st, f"{msg} (stock: {ref_st} {ref_msg})"
        return rel, st, msg
    if want is None:
        if os.path.exists(exe):
            os.remove(exe)
        # No stock run to diff against; exit code against the directive alone.
        if (got[0] == 0) == (kind == "run-pass"):
            return rel, "noref", f"stock: {ref_st} {ref_msg}"
        return rel, "run-fail", f"rc={got[0]} on a {kind} test (stock: {ref_st} {ref_msg})"
    res = None
    if got != want:
        if got[0] != want[0]:
            tail = (got[2].strip().splitlines() or [""])[-1][:200]
            res = ("run-fail", f"rc={got[0]}, stock rc={want[0]} {tail}")
        else:
            gw, gg = norm_out(want[1]), norm_out(got[1])
            if gw != gg:
                res = ("output-mismatch", "stdout " + first_diff(gw, gg))
            else:
                ew, eg = norm_out(want[2]), norm_out(got[2])
                if ew != eg:
                    res = ("stderr-mismatch", "stderr " + first_diff(ew, eg))
        if res:
            # Run the pliron binary once more; a test whose own output varies
            # between runs (e.g. HashMap RandomState order) is flaky, not a
            # backend mismatch.
            _, _, got2 = exec_bin(path, sp, exe)
            if got2 is not None and got2 != got:
                res = None
                return rel, "skipped", "nondeterministic output"
    if os.path.exists(exe):
        os.remove(exe)
    if res:
        return rel, res[0], res[1]
    # Identical behavior; flag it only if the directive itself wasn't met.
    if (want[0] == 0) == (kind == "run-pass"):
        return rel, "pass", ""
    return rel, "env-run-fail", f"rc={want[0]} under both backends on a {kind} test"


def main():
    os.makedirs(os.path.join(OUT, "bin"), exist_ok=True)
    os.makedirs(os.path.join(OUT, "stock"), exist_ok=True)
    ts = sorted(tests())
    print(f"{len(ts)} files", flush=True)
    res = {}
    with cf.ThreadPoolExecutor(os.cpu_count()) as ex:
        for i, (rel, st, msg) in enumerate(ex.map(run, ts)):
            if st != "filtered":
                res[rel] = [st, msg]
            if i % 200 == 0:
                print(i, collections.Counter(v[0] for v in res.values()), flush=True)
    json.dump(res, open(os.path.join(OUT, "results.json"), "w"), indent=1, sort_keys=True)
    c = collections.Counter(v[0] for v in res.values())
    print("SUMMARY", dict(c))
    groups = collections.Counter((v[0], re.sub(r"\d+", "N", v[1])[:160]) for v in res.values()
                                 if v[0] not in ("pass", "noref", "skipped"))
    for (st, msg), n in groups.most_common(60):
        print(f"{n:5} {st:16} {msg}")


main()
