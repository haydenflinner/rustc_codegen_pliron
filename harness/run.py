#!/usr/bin/env python3
"""Test harness: runs suites and compares the results with checked-in expectations.

Usage: harness/run.py [--tier N | --suite NAME ...] [--accept] [--opt] [--no-build]
                      [--rust RUST_CHECKOUT] [--filter SUBSTR]

Suites:
  smoke        tests/{nostd,std,unwind,asm,unroll,proc_macro} at -O0 and -O; each
               program is also built with stock rustc and exit code + stdout must match
  determinism  the smoke programs compiled to objects twice must be byte-identical
  ui           tests/ui_run_pass.py (rustc run-pass/run-fail UI tests, built by
               stock rustc too and diffed on exit code + stdout + stderr)
  ui-wasm      same UI corpus for wasm32-wasip1: pliron objects link via
               pliron-wasm-ld + a pliron-built wasip1 sysroot; runs under
               node+wasi, diffed against stock rustc's wasm target
  fuzz         rustlantis-generated programs (--fuzz-seeds START:END, default 0:32),
               each built+run under stock rustc and pliron at -O0 and -O and diffed

Tier 0 = smoke + determinism, tier 1 = tier 0 + ui.

Results go to target/harness/<suite>/results.json, ICE dumps to target/harness/ices.
Expectations live in harness/expectations/<suite>.<host>[.O].json; the run fails on a
test that was expected to pass and no longer does, or on a new test that fails.
--accept rewrites the expectations from this run. See test-harness.md.
"""
import argparse, collections, concurrent.futures as cf, hashlib, json, os, platform, re, shutil, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target/harness")
EXPECT = os.path.join(ROOT, "harness/expectations")
ICES = os.path.join(OUT, "ices")
TOOLCHAIN = re.search(r'channel = "([^"]+)"', open(os.path.join(ROOT, "rust-toolchain.toml")).read()).group(1)
SO = "dylib" if sys.platform == "darwin" else "so"
BACKEND = os.path.join(ROOT, f"target/debug/librustc_codegen_pliron.{SO}")
ARCH = platform.machine()

os.environ["RUSTUP_TOOLCHAIN"] = TOOLCHAIN
os.environ["RUSTC_ICE"] = ICES
# Verify CLIF after every pass; stock rustc ignores PLIRON_* anyway.
os.environ.setdefault("PLIRON_VERIFY", "1")


def backend_flags():
    flags = [f"-Zcodegen-backend={BACKEND}"]
    if sys.platform.startswith("linux") and shutil.which("wild"):
        # Same as test.sh: point gcc's -B search dir at an `ld` that is wild.
        d = os.path.join(ROOT, "target/wild-ld")
        os.makedirs(d, exist_ok=True)
        if not os.path.exists(os.path.join(d, "ld")):
            os.symlink(shutil.which("wild"), os.path.join(d, "ld"))
        flags += ["-Clinker-features=-lld", "-Clink-self-contained=-linker", "-Zunstable-options", f"-Clink-arg=-B{d}"]
    return flags


def sh(cmd, timeout, cwd=None, env=None):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, cwd=cwd,
                              env=dict(os.environ, **env) if env else None)
    except subprocess.TimeoutExpired:
        return None


def compile_failure(err):
    """Classify a failed rustc invocation as (status, message)."""
    m = re.search(r"panicked at ([^\n]+)\n([^\n]*)", err)
    if m:
        return "ice", f"{m.group(1)} {m.group(2)}"[:300]
    m = re.search(r"error: linking with .*?\n|undefined reference to `([^']+)'|undefined symbol: (\S+)", err)
    if m and "link" in err:
        return "link", (m.group(1) or m.group(2) or "linker error")[:300]
    m = re.search(r"error(\[E\d+\])?: [^\n]+", err)
    return "compile-error", (m.group(0) if m else err[-300:])[:300]


# ---------------------------------------------------------------- smoke

# (name, source, edition, extra flags, archs or None for all)
SMOKE = [
    ("nostd", "tests/nostd/main.rs", "2024", ["-Cpanic=abort", "-Clink-arg=-lc"], None),
    ("std", "tests/std/main.rs", "2024", [], None),
    ("unwind", "tests/unwind/main.rs", "2024", [], None),
    ("asm", "tests/asm/main.rs", "2024", [], {"x86_64", "aarch64", "arm64"}),
    ("unroll", "tests/unroll/main.rs", "2021", [], None),
]
OPTS = {"O0": [], "O": ["-O"]}


def smoke_cases():
    for name, src, ed, flags, archs in SMOKE:
        if archs is None or ARCH in archs:
            for opt, oflags in OPTS.items():
                yield f"{name}@{opt}", src, ["--edition", ed] + flags + oflags


TID = re.compile(r"thread '([^']+)' \(\d+\) panicked")


def build_and_run(src, flags, exe, backend):
    """Returns (status, message, (rc, stdout, stderr)) for one build + run."""
    c = sh(["rustc", os.path.join(ROOT, src), "-o", exe] + flags + (backend_flags() if backend else []), 300)
    if c is None:
        return "compile-timeout", "", None
    if c.returncode != 0:
        return (*compile_failure(c.stderr), None)
    r = sh([exe], 60)
    if r is None:
        return "run-timeout", "", None
    norm = lambda s: TID.sub(r"thread '\1' panicked", s.replace(exe, "EXE"))
    return "ran", "", (r.returncode, norm(r.stdout), norm(r.stderr))


def smoke_one(case):
    name, src, flags = case
    d = os.path.join(OUT, "smoke/bin")
    exe = os.path.join(d, name.replace("@", "_"))
    st, msg, got = build_and_run(src, flags, exe, True)
    if got is None:
        return name, st, msg
    ref_st, ref_msg, want = build_and_run(src, flags, exe + ".llvm", False)
    if want is None:
        # No stock-rustc reference to diff against: fall back to the exit code alone.
        if got[0] != 0:
            return name, "run-fail", f"rc={got[0]} (stock rustc: {ref_st} {ref_msg})"
        return name, "noref", f"stock rustc: {ref_st} {ref_msg}"
    if got != want:
        if got[0] != want[0]:
            return name, "run-fail", f"rc={got[0]}, stock rustc rc={want[0]}"
        if got[1] != want[1]:
            return name, "output-mismatch", "stdout " + first_diff(want[1], got[1])
        return name, "stderr-mismatch", "stderr " + first_diff(want[2], got[2])
    if want[0] != 0:
        return name, "env-run-fail", f"rc={want[0]} under both backends"
    expected = os.path.join(ROOT, os.path.dirname(src), "expected.out")
    if os.path.exists(expected) and got[1] != open(expected).read():
        return name, "output-mismatch", "differs from " + os.path.relpath(expected, ROOT)
    return name, "pass", ""


def first_diff(want, got):
    for i, (a, b) in enumerate(zip(want.splitlines(), got.splitlines())):
        if a != b:
            return f"line {i + 1}: stock {a!r} vs pliron {b!r}"[:300]
    return f"stock {len(want.splitlines())} lines vs pliron {len(got.splitlines())} lines"


def proc_macro_case():
    """Proc macro built by us, loaded by stock rustc: the C ABI (byval/sret) across the bridge."""
    d = os.path.join(OUT, "smoke/bin")
    lib = os.path.join(d, f"libpm.{SO}")
    c = sh(["rustc", "--edition", "2021", "--crate-type", "proc-macro", os.path.join(ROOT, "tests/proc_macro/pm.rs"),
            "-o", lib] + backend_flags(), 300)
    if c is None:
        return "proc_macro", "compile-timeout", ""
    if c.returncode != 0:
        return ("proc_macro", *compile_failure(c.stderr))
    exe = os.path.join(d, "pm_user")
    c = sh(["rustc", "--edition", "2021", os.path.join(ROOT, "tests/proc_macro/main.rs"), "--extern", f"pm={lib}",
            "-o", exe], 300)
    if c is None:
        return "proc_macro", "compile-timeout", "stock rustc expanding the macro"
    if c.returncode != 0:
        st, msg = compile_failure(c.stderr)
        return "proc_macro", st, "stock rustc expanding the macro: " + msg
    r = sh([exe], 60)
    if r is None or r.returncode != 0:
        return "proc_macro", "run-fail", "" if r is None else f"rc={r.returncode}"
    return "proc_macro", "pass", ""


def suite_smoke(args):
    os.makedirs(os.path.join(OUT, "smoke/bin"), exist_ok=True)
    cases = [c for c in smoke_cases() if args.filter in c[0]]
    with cf.ThreadPoolExecutor(os.cpu_count()) as ex:
        res = {n: [st, msg] for n, st, msg in ex.map(smoke_one, cases)}
    if args.filter in "proc_macro":
        n, st, msg = proc_macro_case()
        res[n] = [st, msg]
    return res


# ---------------------------------------------------------- determinism

def determinism_one(case):
    name, src, flags = case
    objs = []
    for i in (1, 2):
        d = os.path.join(OUT, f"determinism/{i}")
        o = os.path.join(d, name.replace("@", "_") + ".o")
        c = sh(["rustc", os.path.join(ROOT, src), "--emit=obj", "-Ccodegen-units=1", "-o", o] + flags + backend_flags(), 300)
        if c is None:
            return name, "compile-timeout", ""
        if c.returncode != 0:
            return (name, *compile_failure(c.stderr))
        objs.append(open(o, "rb").read())
    if objs[0] != objs[1]:
        at = next((i for i, (a, b) in enumerate(zip(*objs)) if a != b), min(map(len, objs)))
        return name, "nondeterministic", f"sizes {len(objs[0])}/{len(objs[1])}, first difference at byte {at:#x}"
    return name, "pass", ""


def suite_determinism(args):
    for i in (1, 2):
        os.makedirs(os.path.join(OUT, f"determinism/{i}"), exist_ok=True)
    cases = [c for c in smoke_cases() if args.filter in c[0]]
    with cf.ThreadPoolExecutor(os.cpu_count()) as ex:
        return {n: [st, msg] for n, st, msg in ex.map(determinism_one, cases)}


# ------------------------------------------------------------------- ui

def suite_ui(args):
    cmd = [sys.executable, os.path.join(ROOT, "tests/ui_run_pass.py")]
    if args.rust or args.filter:
        cmd += [args.rust or os.environ.get("RUST_CHECKOUT") or default_rust(), args.filter]
    env = dict(os.environ, UI_FLAGS=" ".join(["-O"] if args.opt else []))
    r = subprocess.run(cmd, env=env)
    if r.returncode != 0:
        sys.exit(f"ui: tests/ui_run_pass.py exited with {r.returncode}")
    return json.load(open(os.path.join(ROOT, "target/ui/results.json")))


def default_rust():
    sib = os.path.join(ROOT, "..", "rust")
    return sib if os.path.isdir(os.path.join(sib, "tests/ui")) else os.path.expanduser("~/work/rust")


# --------------------------------------------------------------- ui-wasm
#
# wasm32-wasip1 differential: pliron objects only link with pliron-wasm-ld, so
# the suite provisions a pliron-built wasip1 sysroot under target/ui-wsys
# (rebuilt when the backend dylib changes). Runs execute under node+wasi.

def ui_wasm_sysroot(args):
    """Build/assemble the pliron wasip1 sysroot; returns its lib dir or None."""
    tgt = "wasm32-wasip1"
    wsys = os.path.join(ROOT, "target/ui-wsys")
    libdir = os.path.join(wsys, "lib/rustlib", tgt, "lib")
    build = os.path.join(ROOT, "target/ui-wsys-build")
    stamp = os.path.join(wsys, "stamp")
    key = hashlib.sha256(open(BACKEND, "rb").read()).hexdigest()
    if os.path.exists(stamp) and open(stamp).read() == key:
        return libdir
    print("ui-wasm: provisioning pliron wasip1 sysroot (one-off, ~minutes)")
    # pliron-wasm-ld
    r = sh(["cargo", "build", "-q", "--release",
            "--manifest-path", os.path.join(ROOT, "tools/pliron-wasm-ld/Cargo.toml")], 600)
    if r is None or r.returncode != 0:
        print("ui-wasm: pliron-wasm-ld build failed"); return None
    be = f"-Zcodegen-backend={BACKEND}"
    ld = os.path.join(ROOT, "tools/pliron-wasm-ld/target/release/pliron-wasm-ld")
    # wasi-libc (pure Rust, pliron-compiled)
    env = dict(os.environ, RUSTFLAGS=f"{be} -Clinker={ld}",
               CARGO_TARGET_DIR=os.path.join(build, "wasi-libc"))
    r = sh(["cargo", "build", "-q", "--release", "--target", tgt,
            "-Zbuild-std=core,panic_abort"], 900,
           cwd=os.path.join(ROOT, "tools/pliron-wasi-libc"), env=env)
    if r is None or r.returncode != 0:
        print(f"ui-wasm: wasi-libc build failed\n{r.stderr[-2000:] if r else 'timeout'}"); return None
    wasi_libc = os.path.join(build, "wasi-libc", tgt, "release")
    lnc = os.path.join(wasi_libc, "libc.a")
    if os.path.lexists(lnc):
        os.remove(lnc)
    os.symlink("libpliron_wasi_libc.a", lnc)
    with open(os.path.join(wasi_libc, "libunwind.a"), "w") as f:
        f.write("!<arch>\n")
    # dummy crate → forces -Zbuild-std to emit std rlibs/rmeta we can harvest
    srcdir = os.path.join(build, "src")
    os.makedirs(srcdir, exist_ok=True)
    with open(os.path.join(build, "Cargo.toml"), "w") as f:
        f.write('[package]\nname = "wsys"\nversion = "0.0.0"\nedition = "2021"\n\n[workspace]\n')
    with open(os.path.join(srcdir, "main.rs"), "w") as f:
        f.write("fn main() {}\n")
    env = dict(os.environ,
               RUSTFLAGS=f"{be} -Clinker={ld} -Clink-self-contained=no -Lnative={wasi_libc}",
               CARGO_TARGET_DIR=os.path.join(build, "app"))
    r = sh(["cargo", "build", "-q", "--release", "--target", tgt,
            "-Zbuild-std=std,panic_abort,panic_unwind,test"], 1200, cwd=build, env=env)
    if r is None or r.returncode != 0:
        print(f"ui-wasm: build-std failed\n{r.stderr[-2000:] if r else 'timeout'}"); return None
    os.makedirs(libdir, exist_ok=True)
    for old in os.listdir(libdir):
        os.remove(os.path.join(libdir, old))
    out = os.path.join(build, "app", tgt, "release/build")
    n = 0
    for pkg in os.listdir(out):
        od = os.path.join(out, pkg)
        for h in os.listdir(od):
            d = os.path.join(od, h, "out")
            if not os.path.isdir(d):
                continue
            for f in os.listdir(d):
                if f.endswith((".rlib", ".rmeta")):
                    shutil.copy2(os.path.join(d, f), libdir)
                    n += 1
    for f in os.listdir(wasi_libc):
        if f.endswith(".a"):
            shutil.copy2(os.path.join(wasi_libc, f), libdir)
    print(f"ui-wasm: sysroot assembled ({n} rlibs/rmeta)")
    with open(stamp, "w") as f:
        f.write(key)
    return libdir


def suite_ui_wasm(args):
    tgt = "wasm32-wasip1"
    # stock reference needs the installed target std; pliron needs our sysroot.
    v = sh(["rustc", "--print", "sysroot"], 30)
    stock_lib = os.path.join(v.stdout.strip(), "lib/rustlib", tgt) if v else ""
    if not os.path.isdir(stock_lib):
        print(f"ui-wasm: stock rustc has no {tgt} std (rustup target add {tgt})")
        return {}
    if shutil.which("node") is None:
        print("ui-wasm: node not found"); return {}
    libdir = ui_wasm_sysroot(args)
    if libdir is None:
        return {}
    wsys = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(libdir))))
    cmd = [sys.executable, os.path.join(ROOT, "tests/ui_run_pass.py")]
    if args.rust or args.filter:
        cmd += [args.rust or os.environ.get("RUST_CHECKOUT") or default_rust(), args.filter]
    env = dict(os.environ, UI_FLAGS=" ".join(["-O"] if args.opt else []),
               UI_TARGET=tgt, UI_WSYS=wsys)
    r = subprocess.run(cmd, env=env)
    if r.returncode != 0:
        sys.exit(f"ui-wasm: tests/ui_run_pass.py exited with {r.returncode}")
    return json.load(open(os.path.join(ROOT, f"target/ui-{tgt}/results.json")))


# ------------------------------------------------------------------ fuzz

# rustlantis `generate` binary; built outside this repo. Programs are
# custom-MIR, terminating, UB-free and deterministic: any backend divergence
# is a real bug. -Zmir-opt-level=0 keeps MIR as generated for all backends.
GENERATE = os.environ.get(
    "PLIRON_FUZZ_GENERATOR",
    os.path.join(ROOT, "..", "rustlantis", "target", "release", "generate"),
)


def fuzz_build_run(src, exe, opt, backend, env=None):
    """Compile+run one generated program; returns (status, msg, (rc, out))."""
    flags = ["-Zmir-opt-level=0", "-Ccodegen-units=1"] + OPTS[opt]
    c = sh(["rustc", src, "-o", exe] + flags + (backend_flags() if backend else []), 300, env=env)
    if c is None:
        return "compile-timeout", "", None
    if c.returncode != 0:
        return (*compile_failure(c.stderr), None)
    r = sh([exe], 60)
    if r is None:
        return "run-timeout", "", None
    return "ran", "", (r.returncode, r.stdout.replace(exe, "EXE"))


# -O pass toggles (`PLIRON_<NAME>=0` disables); --matrix reruns each seed
# with each pass disabled and requires identical output.
MATRIX_PASSES = [
    "JUMPTHREAD", "UNROLL", "UNROLL_CLEANUP", "LOADFWD", "EGRAPH", "INDUCT",
    "LICM", "VEC", "SLP", "BCHECK", "LOOPDEL", "LOOPROT", "CONSTBR",
    "SWITCHMAP", "IDIOM", "PEEP", "TAILMERGE", "TAILDUP", "UNREACH",
    "SLOT_DSE", "DEAD_LOADS", "DEAD_PURE", "DOMCOND", "MEMFAST",
]


def fuzz_one(seed, matrix=False):
    name = f"seed{seed}"
    d = os.path.join(OUT, "fuzz", str(seed))
    os.makedirs(d, exist_ok=True)
    src = os.path.join(d, "p.rs")
    # `generate` reads config.toml from its cwd, i.e. the rustlantis root.
    g = sh([GENERATE, str(seed)], 120, cwd=os.path.dirname(os.path.dirname(os.path.dirname(GENERATE))))
    if g is None or g.returncode != 0:
        return name, "generate-error", (g.stderr if g else "timeout")[-300:]
    with open(src, "w") as f:
        f.write(g.stdout)
    refs = {}
    for tag, opt, backend in (("stock0", "O0", False), ("stockO", "O", False),
                              ("pliron0", "O0", True), ("plironO", "O", True)):
        refs[tag] = fuzz_build_run(src, os.path.join(d, f"p.{tag}"), opt, backend)
    s0, sO, p0, pO = refs["stock0"], refs["stockO"], refs["pliron0"], refs["plironO"]
    if s0[2] is None or sO[2] is None:
        return name, "env-compile-error", f"stock O0: {s0[0]}, stock -O: {sO[0]} | {s0[1] or sO[1]}"
    if s0[2] != sO[2]:
        # The LLVM reference itself isn't stable across opt levels; we can't
        # tell a pliron bug from an upstream one.
        return name, "env-stock-mismatch", f"stock -O vs -O0 differ"
    for tag, ref, got in (("O0", s0, p0), ("O", sO, pO)):
        if got[2] is None:
            return name, got[0], f"{tag}: {got[1]} (stock {ref[0]}) [{d}]"
        if got[2][0] != ref[2][0]:
            return name, "run-fail", f"{tag}: rc={got[2][0]}, stock rc={ref[2][0]} [{d}]"
        if got[2][1] != ref[2][1]:
            return name, "output-mismatch", f"{tag}: " + first_diff(ref[2][1], got[2][1]) + f" [{d}]"
    if matrix:
        for p in MATRIX_PASSES:
            got = fuzz_build_run(src, os.path.join(d, f"p.no_{p}"), "O", True,
                                 env={f"PLIRON_{p}": "0"})
            if got[2] is None:
                return name, got[0], f"-O PLIRON_{p}=0: {got[1]} [{d}]"
            if got[2] != sO[2]:
                return name, f"matrix-{p}", (first_diff(sO[2][1], got[2][1])
                                             if got[2][0] == sO[2][0]
                                             else f"rc={got[2][0]} vs {sO[2][0]}") + f" [{d}]"
    return name, "pass", ""


def suite_fuzz(args):
    if not os.path.exists(GENERATE):
        print(f"fuzz: no generator at {GENERATE}; build rustlantis or set PLIRON_FUZZ_GENERATOR")
        return {}
    a, _, b = args.fuzz_seeds.partition(":")
    seeds = range(int(a), int(b or int(a) + 32))
    seeds = [s for s in seeds if args.filter in f"seed{s}"]
    import functools
    one = functools.partial(fuzz_one, matrix=args.matrix)
    with cf.ThreadPoolExecutor(min(8, os.cpu_count())) as ex:
        return {n: [st, msg] for n, st, msg in ex.map(one, seeds)}


# --------------------------------------------------------- expectations

SUITES = {"smoke": suite_smoke, "determinism": suite_determinism, "ui": suite_ui,
          "ui-wasm": suite_ui_wasm, "fuzz": suite_fuzz}
TIERS = {0: ["smoke", "determinism"], 1: ["smoke", "determinism", "ui"]}
# Suites whose test set depends on --opt (the others cover both opt levels themselves).
OPT_VARIANT = {"ui", "ui-wasm"}


def host():
    return re.search(r"host: (\S+)", subprocess.check_output(["rustc", "-vV"], text=True)).group(1)


def expect_path(suite, args):
    variant = ".O" if args.opt and suite in OPT_VARIANT else ""
    return os.path.join(EXPECT, f"{suite}.{host()}{variant}.json")


def ok(st):
    """pass, or noref (ran fine but there was no stock-rustc output to compare with)."""
    return st in ("pass", "noref")


def compare(suite, res, args):
    """Prints the comparison; returns the number of failing differences."""
    path = expect_path(suite, args)
    exp = json.load(open(path)) if os.path.exists(path) else None
    counts = collections.Counter(v[0] for v in res.values())
    print(f"\n== {suite}: {len(res)} tests, {dict(counts)}")
    if args.accept:
        if args.filter:
            # Partial run: merge into the existing expectations.
            exp = dict(exp or {}, **{t: v[0] for t, v in res.items()})
        else:
            exp = {t: v[0] for t, v in res.items()}
        os.makedirs(EXPECT, exist_ok=True)
        with open(path, "w") as f:
            json.dump(exp, f, indent=0, sort_keys=True)
            f.write("\n")
        print(f"   wrote {os.path.relpath(path, ROOT)}")
        return 0
    if exp is None:
        print(f"   no expectations at {os.path.relpath(path, ROOT)}; run with --accept to create them")
        return 1
    regress, new_fail, improved, env = [], [], [], []
    for t, (st, msg) in sorted(res.items()):
        want = exp.get(t)
        if want is None:
            if not ok(st) and not st.startswith("env-") and st != "skipped":
                new_fail.append((t, st, msg))
        elif ok(want) and not ok(st):
            (env if st.startswith("env-") else regress).append((t, st, msg))
        elif st != want and ok(st):
            improved.append(t)
    for title, rows in (("REGRESSIONS", regress), ("NEW FAILURES", new_fail), ("env (stock rustc fails too)", env)):
        if rows:
            print(f"   {title} ({len(rows)}):")
            for t, st, msg in rows[:50]:
                print(f"     {t}: {st} {msg}")
            if len(rows) > 50:
                print(f"     ... and {len(rows) - 50} more, see target/harness/{suite}/results.json")
    if improved:
        print(f"   now passing ({len(improved)}), record with --accept:")
        for t in improved[:50]:
            print(f"     {t}")
    if not (regress or new_fail):
        print("   OK, matches expectations")
    return len(regress) + len(new_fail)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tier", type=int, choices=sorted(TIERS), default=0)
    ap.add_argument("--suite", action="append", choices=sorted(SUITES), help="run these suites instead of a tier")
    ap.add_argument("--accept", action="store_true", help="write this run's results as the expectations")
    ap.add_argument("--opt", action="store_true", help="run the ui suite at -O (separate expectations)")
    ap.add_argument("--filter", default="", help="only tests whose name contains this")
    ap.add_argument("--matrix", action="store_true",
                    help="fuzz: also rerun each seed with every -O pass disabled")
    ap.add_argument("--fuzz-seeds", default="0:32", metavar="START:END",
                    help="seed range for the fuzz suite (default 0:32)")
    ap.add_argument("--rust", help="rust checkout for the ui suite (default: ../rust or ~/work/rust)")
    ap.add_argument("--no-build", action="store_true", help="skip `cargo build` of the backend")
    args = ap.parse_args()
    if not args.no_build:
        subprocess.run(["cargo", "build"], cwd=ROOT, check=True)
    os.makedirs(ICES, exist_ok=True)
    ices_before = set(os.listdir(ICES))
    bad = 0
    for suite in args.suite or TIERS[args.tier]:
        res = SUITES[suite](args)
        os.makedirs(os.path.join(OUT, suite), exist_ok=True)
        with open(os.path.join(OUT, suite, "results.json"), "w") as f:
            json.dump(res, f, indent=1, sort_keys=True)
        bad += compare(suite, res, args)
    ices = sorted(set(os.listdir(ICES)) - ices_before)
    if ices:
        print(f"\n{len(ices)} new ICE dump(s) in {os.path.relpath(ICES, ROOT)}")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
