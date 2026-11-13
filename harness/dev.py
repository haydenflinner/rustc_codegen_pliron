#!/usr/bin/env -S python3 -I
"""Inner loop: edit a pass, see its effect in seconds (docs/dev-loop.md).

Usage: harness/dev.py <command> [args]

  build [check|dylib|all]     build clif-check / the backend dylib with the `fast` profile
  clif PATH... [--base]       run the CLIF pipeline (+ interpreter diff) on saved .clif files
                              or directories; instruction counts before -> after per function
  corpus [NAME] [--base]      the same over target/corpus/NAME (default std): verdict counts,
                              total instructions, biggest movers against the baseline
  rs FILE.rs [--fn SUB] [--env K=V]... [--run] [--base] [-- RUSTC FLAGS]
                              compile FILE with the fast dylib (-O, one CGU) and print
                              machine instructions per function
  check [quick.py args]       functional check with the fast dylib: harness/quick.py
                              (default --tier 0); `check --gate [gate.py args]` runs the gate
  bench PROG SIDE...          bench/heldout/ab.py with the fast dylib (runtime A/B only: for
                              compile-time numbers use the release dylib)
  watch COMMAND [args]        rerun COMMAND (clif, corpus, rs, check) on every save of a
                              source file; prints save -> result time
  test [-p PKG] [--lib | --test NAME] [FILTER] [--cargo]
                              unit tests on save under the relax JIT (`relax test --live`,
                              default -p pliron-clif-opt --lib): after a save only the tests
                              that entered a changed function rerun, ~1-3 s. --cargo runs
                              `cargo test --profile fast` once instead
  relax ARGS...               clif-check ARGS under the relax JIT (no cargo build; slower
                              than native on the corpus, see docs/dev-loop.md)
  relax-build                 build the relax runner from ../relax into target/dev/relax
  hack [--check]              regenerate pliron-hack/Cargo.toml (feature unification)

Every result is compared with the previous run and with a baseline (the first run of
that command line, or the run given --base). State: target/dev/.

Environment: PLIRON_PROFILE (default `fast`; `debug` uses the shared target/debug),
CARGO_TARGET_DIR (a private target dir when several agents build at once: each then
compiles for itself instead of waiting on cargo's lock).
"""
import hashlib, json, os, re, shutil, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PROFILE = os.environ.get("PLIRON_PROFILE", "fast")
TARGET = os.path.abspath(os.environ.get("CARGO_TARGET_DIR", os.path.join(ROOT, "target")))
PDIR = os.path.join(TARGET, PROFILE)
STATE = os.path.join(ROOT, "target/dev")
DYLIB = {"darwin": "librustc_codegen_pliron.dylib", "win32": "rustc_codegen_pliron.dll"}.get(
    sys.platform, "librustc_codegen_pliron.so")
BACKEND = os.path.join(PDIR, DYLIB)
CLIF_CHECK = os.path.join(PDIR, "clif-check.exe" if os.name == "nt" else "clif-check")
# Saving a file under one of these reruns a watched command.
WATCH = ["src", "pliron-cfg/src", "pliron-clif-opt/src", "pliron-ir-opt/src", "pliron-clif-lower/src",
         "pliron-clif-check/src", "vendor/cranelift-codegen/src", "vendor/regalloc2/src"]
PKGS = {"check": ["pliron-clif-check"], "dylib": ["rustc_codegen_pliron"],
        "all": ["rustc_codegen_pliron", "pliron-clif-check"]}
# Workspace crates whose package selections must resolve to one feature set (`hack`).
MEMBERS = ["rustc_codegen_pliron", "pliron-clif-check", "pliron-clif-lower", "pliron-clif-opt", "pliron-ir-opt",
           "pliron-cfg"]


def build(what):
    """`cargo build` of the fast profile; returns seconds, exits on failure."""
    t = time.time()
    cmd = ["cargo", "build", "-q"] + ([] if PROFILE == "debug" else ["--profile", PROFILE])
    for p in PKGS[what]:
        cmd += ["-p", p]
    r = subprocess.run(cmd, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    if r.returncode != 0:
        errs = [l for l in r.stdout.splitlines() if not l.startswith("warning")]
        i = next((k for k, l in enumerate(errs) if l.startswith("error")), 0)
        print("\n".join(errs[i:i + 60]))
        print(f"dev: build failed after {time.time() - t:.1f}s")
        raise SystemExit(1)
    return time.time() - t


def state_path(kind, key):
    os.makedirs(STATE, exist_ok=True)
    return os.path.join(STATE, f"{kind}-{hashlib.sha256(key.encode()).hexdigest()[:12]}")


def compare(path, cur, rebase):
    """Rotate `cur` (name -> number) into the state files; returns (previous, baseline)."""
    def load(p):
        try:
            return json.load(open(p))
        except (OSError, ValueError):
            return None
    prev, base = load(path + ".last.json"), load(path + ".base.json")
    if base is None or rebase:
        base = cur
        json.dump(cur, open(path + ".base.json", "w"))
    json.dump(cur, open(path + ".last.json", "w"))
    return prev, base


def delta(new, old):
    if old is None:
        return "     new"
    return f"{new - old:+8d}" if new != old else "       ="


def table(rows, cur, prev, base, unit):
    """rows: names to print; cur/prev/base: name -> count."""
    w = max([len(n) for n in rows] + [8])
    print(f"  {'':<{w}} {unit:>8} {'vs prev':>8} {'vs base':>8}")
    for n in rows:
        print(f"  {n:<{w}} {cur[n]:>8} {delta(cur[n], (prev or cur).get(n))} {delta(cur[n], base.get(n))}")
    tot = lambda d: sum(d.get(n, 0) for n in cur) if d else None
    print(f"  {'total':<{w}} {sum(cur.values()):>8} {delta(sum(cur.values()), tot(prev) if prev else sum(cur.values()))} "
          f"{delta(sum(cur.values()), tot(base))}")


def flag(args, name):
    if name in args:
        args.remove(name)
        return True
    return False


def report(src, out):
    """`clif-check report --check` over a directory; returns (json, seconds)."""
    t = time.time()
    r = subprocess.run([CLIF_CHECK, "report", src, out, "--check"], cwd=ROOT, capture_output=True, text=True)
    if not os.path.exists(out) or r.returncode not in (0, 1):
        print((r.stdout + r.stderr)[-3000:])
        raise SystemExit("dev: clif-check report failed")
    return json.load(open(out)), time.time() - t


def bad(rep):
    return sorted(f"{f['status']}: {f['file']} {f['msg'][:120]}" for f in rep["functions"]
                  if f["status"] in ("mismatch", "verifier", "panic"))


def cmd_clif(args):
    rebase = flag(args, "--base")
    if not args:
        raise SystemExit("dev clif: give .clif files or directories")
    tb = build("check")
    path = state_path("clif", " ".join(os.path.abspath(a) for a in args))
    ind = path + ".in"
    shutil.rmtree(ind, ignore_errors=True)
    os.makedirs(ind)
    for a in args:
        fs = [os.path.join(a, f) for f in sorted(os.listdir(a)) if f.endswith(".clif")] if os.path.isdir(a) else [a]
        for f in fs:
            dst = os.path.join(ind, os.path.basename(f))
            if not os.path.exists(dst):
                os.symlink(os.path.abspath(f), dst)
    rep, tr = report(ind, path + ".report.json")
    cur = {f["file"]: f["insts_after"] for f in rep["functions"]}
    prev, base = compare(path, cur, rebase)
    if len(cur) <= 40:
        before = {f["file"]: f["insts_before"] for f in rep["functions"]}
        w = max(len(n) for n in cur)
        print(f"  {'':<{w}} {'before':>7} {'after':>7} {'vs prev':>8} {'vs base':>8}  verdict")
        for f in rep["functions"]:
            n = f["file"]
            print(f"  {n:<{w}} {before[n]:>7} {cur[n]:>7} {delta(cur[n], (prev or cur).get(n))} "
                  f"{delta(cur[n], base.get(n))}  {f['status']}")
    else:
        movers(cur, prev, base)
    for b in bad(rep):
        print("  FAIL", b)
    print(f"dev clif: {len(cur)} fn(s) {rep['counts']}  build {tb:.1f}s  run {tr:.1f}s")
    return not bad(rep)


def movers(cur, prev, base):
    tot = sum(cur.values())
    for name, old in (("prev", prev), ("base", base)):
        if not old:
            continue
        ch = sorted(((cur[n] - old[n], n) for n in cur if n in old and cur[n] != old[n]), key=lambda x: -abs(x[0]))
        print(f"  vs {name}: total insts {tot} ({tot - sum(old.get(n, cur[n]) for n in cur):+d}), "
              f"{len(ch)} function(s) changed")
        for d, n in ch[:8]:
            print(f"    {d:+6d}  {n}")


def cmd_corpus(args):
    rebase = flag(args, "--base")
    name = args[0] if args else "std"
    src = os.path.join(ROOT, "target/corpus", name)
    if not os.path.isdir(src):
        raise SystemExit(f"dev corpus: no {src} (record it with harness/corpus.sh {name})")
    tb = build("check")
    path = state_path("corpus", name)
    rep, tr = report(src, path + ".report.json")
    cur = {f["file"]: f["insts_after"] for f in rep["functions"]}
    prev, base = compare(path, cur, rebase)
    movers(cur, prev, base)
    known = set()
    kp = os.path.join(ROOT, "harness/expectations/corpus-known.txt")
    if os.path.exists(kp):
        known = {l.split("#")[0].strip() for l in open(kp)} - {""}
    new = [b for b in bad(rep) if f"{name}/{b.split()[1]}" not in known]
    for b in new[:20]:
        print("  FAIL", b)
    print(f"dev corpus: {name} {len(cur)} fns {rep['counts']}  build {tb:.1f}s  run {tr:.1f}s")
    return not new


def objdump():
    sysroot = subprocess.run(["rustc", "--print", "sysroot"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    host = subprocess.run(["rustc", "-vV"], cwd=ROOT, capture_output=True, text=True).stdout
    host = re.search(r"host: (\S+)", host).group(1)
    p = os.path.join(sysroot, "lib/rustlib", host, "bin/llvm-objdump")
    return p if os.path.exists(p) else shutil.which("llvm-objdump") or shutil.which("objdump")


def insts(obj):
    """symbol -> machine instruction count of a relocatable object."""
    out = subprocess.run([objdump(), "-d", "--no-show-raw-insn", obj], capture_output=True, text=True).stdout
    counts, sym = {}, None
    for l in out.splitlines():
        m = re.match(r"^[0-9a-f]+ <(.+)>:$", l)
        if m:
            sym = m.group(1)
            # Local labels inside a function (Mach-O `ltmp`, `L...`) are not functions.
            if re.match(r"^(ltmp\d+|L|l_)", sym):
                sym = None if not counts else list(counts)[-1]
            else:
                counts.setdefault(sym, 0)
        elif sym and re.match(r"^\s*[0-9a-f]+:\s+\S", l):
            counts[sym] += 1
    return counts


def demangle(s):
    s = s[1:] if sys.platform == "darwin" and s.startswith("_") else s
    m = re.match(r"^_?ZN(.*)17h[0-9a-f]{16}E$", s)
    if not m:
        return s
    parts, rest = [], m.group(1)
    while rest:
        k = re.match(r"(\d+)", rest)
        if not k:
            return s
        n = int(k.group(1))
        parts.append(rest[len(k.group(1)):len(k.group(1)) + n])
        rest = rest[len(k.group(1)) + n:]
    return "::".join(parts).replace("$LT$", "<").replace("$GT$", ">").replace("$u20$", " ").replace("..", "::")


def cmd_rs(args):
    rebase, run = flag(args, "--base"), flag(args, "--run")
    extra = []
    if "--" in args:
        i = args.index("--")
        args, extra = args[:i], args[i + 1:]
    env, sub = dict(os.environ), None
    while len(args) > 1:
        if args[1] == "--env" and len(args) > 2:
            k, _, v = args[2].partition("=")
            env[k] = v
        elif args[1] == "--fn" and len(args) > 2:
            sub = args[2]
        else:
            raise SystemExit(f"dev rs: unknown argument {args[1]}")
        del args[1:3]
    if not args:
        raise SystemExit("dev rs: give a .rs file")
    src = os.path.abspath(args[0])
    tb = build("dylib")
    key = " ".join([src, *extra, *sorted(f"{k}={v}" for k, v in env.items() if k.startswith("PLIRON_"))])
    path = state_path("rs", key)
    text = open(src).read()
    is_bin = re.search(r"^\s*(pub\s+)?fn\s+main\s*\(", text, re.M) and "crate_type" not in text
    env.setdefault("RUSTC_ICE", os.path.join(ROOT, "target/ices"))
    base_cmd = ["rustc", "-O", "-Ccodegen-units=1", f"-Zcodegen-backend={BACKEND}", src, *extra]
    kind = [] if "crate_type" in text or is_bin else ["--crate-type=lib"]
    t = time.time()
    r = subprocess.run(base_cmd + kind + ["--emit=obj", "-o", path + ".o"], cwd=ROOT, env=env,
                       capture_output=True, text=True)
    if r.returncode != 0:
        print(r.stderr[-4000:])
        print("dev rs: compile failed")
        return False
    tc = time.time() - t
    cur = {demangle(k): v for k, v in insts(path + ".o").items() if v}
    prev, base = compare(path, cur, rebase)
    rows = sorted(n for n in cur if not sub or sub in n)
    if len(rows) > 60 and not sub:
        ch = [n for n in rows if (prev or cur).get(n) != cur[n] or base.get(n) != cur[n]]
        print(f"  {len(rows)} functions; showing the {len(ch)} that changed (--fn SUB selects)")
        rows = ch
    table(rows, cur, prev, base, "insts")
    if run and is_bin:
        exe = path + ".exe"
        r = subprocess.run(base_cmd + ["-o", exe], cwd=ROOT, env=env, capture_output=True, text=True)
        if r.returncode == 0:
            r = subprocess.run([exe], capture_output=True, text=True)
        print(f"  --run: exit {r.returncode}\n" + "".join("    " + l + "\n" for l in (r.stdout + r.stderr).splitlines()[-15:]),
              end="")
    print(f"dev rs: {os.path.relpath(src, ROOT)}  build {tb:.1f}s  compile {tc:.1f}s")
    return True


def cmd_check(args):
    env = dict(os.environ, PLIRON_PROFILE=PROFILE)
    t = time.time()
    if flag(args, "--gate"):
        cmd = [sys.executable, "-I", "harness/gate.py", *args]
    else:
        cmd = [sys.executable, "-I", "harness/quick.py", *(args or ["--tier", "0"])]
    rc = subprocess.run(cmd, cwd=ROOT, env=env).returncode
    print(f"dev check: {'ok' if rc == 0 else 'FAILED'} in {time.time() - t:.1f}s ({' '.join(cmd[2:])}, profile {PROFILE})")
    return rc == 0


def cmd_bench(args):
    tb = build("dylib")
    print(f"dev bench: build {tb:.1f}s; backend {os.path.relpath(BACKEND, ROOT)}")
    return subprocess.run([sys.executable, "-I", "bench/heldout/ab.py", *args, "--backend", BACKEND], cwd=ROOT).returncode == 0


def snapshot(extra):
    m = {}
    for d in WATCH:
        for dp, _, fs in os.walk(os.path.join(ROOT, d)):
            for f in fs:
                if f.endswith((".rs", ".isle", ".toml")):
                    p = os.path.join(dp, f)
                    try:
                        m[p] = os.stat(p).st_mtime_ns
                    except OSError:
                        pass
    for p in extra:
        try:
            m[p] = os.stat(p).st_mtime_ns
        except OSError:
            pass
    return m


def cmd_watch(args):
    if not args or args[0] not in ("clif", "corpus", "rs", "check"):
        raise SystemExit("dev watch: clif | corpus | rs | check")
    extra = [os.path.abspath(a) for a in args[1:] if os.path.isfile(a)]

    def once():
        try:
            COMMANDS[args[0]](list(args[1:]))
        except SystemExit as e:
            if e.code not in (0, 1, None):
                print(e.code)
    prev = snapshot(extra)
    once()
    print("dev watch: waiting for a save (ctrl-c to stop)", flush=True)
    while True:
        time.sleep(0.25)
        cur = snapshot(extra)
        if cur == prev:
            continue
        time.sleep(0.15)  # let the editor finish writing
        cur = snapshot(extra)
        ch = sorted(os.path.relpath(p, ROOT) for p in set(cur) | set(prev) if cur.get(p) != prev.get(p))
        saved = max((cur[p] for p in cur if cur.get(p) != prev.get(p)), default=time.time_ns()) / 1e9
        prev = cur
        print(f"\n--- {', '.join(ch[:4])}{' ...' if len(ch) > 4 else ''}", flush=True)
        once()
        print(f"dev watch: save -> result {time.time() - saved:.1f}s", flush=True)


# ---- feature unification (`hack`) -------------------------------------------------
#
# Cargo resolves features for the packages a command selects, so `cargo build`,
# `cargo build -p pliron-clif-check`, `cargo build -p rustc_codegen_pliron` and
# `cargo test -p pliron-clif-opt` each used their own build of cranelift-codegen
# (with/without `host-arch`, `timing`, ...) and therefore of every crate above it:
# after one edit pliron-clif-opt was compiled once per command in use. pliron-hack
# lists, for each third-party crate whose features differ between selections, the
# union; pliron-cfg depends on it, so every selection resolves to the same units.

def unit_features(sel):
    """Target-side (not build-script, not proc-macro) third-party units of a cargo
    selection: package id -> feature set."""
    r = subprocess.run(["cargo", *sel, "--unit-graph", "-Zunstable-options"], cwd=ROOT, capture_output=True, text=True)
    if r.returncode != 0:
        raise SystemExit(f"dev hack: cargo {' '.join(sel)} --unit-graph failed:\n{r.stderr[-2000:]}")
    g = json.loads(r.stdout)
    units = g["units"]
    seen, stack, out = set(), list(g["roots"]), {}
    while stack:
        i = stack.pop()
        if i in seen:
            continue
        seen.add(i)
        u = units[i]
        kind = u["target"]["kind"]
        if "custom-build" in kind or "proc-macro" in kind or u["mode"] == "run-custom-build":
            continue
        if any(k in kind for k in ("lib", "rlib", "dylib", "cdylib")):
            out.setdefault(u["pkg_id"], set()).update(u["features"])
        stack += [d["index"] for d in u["dependencies"]]
    return out


def pkg_name_version(pid):
    src, _, frag = pid.partition("#")
    if "@" in frag:
        return tuple(frag.split("@"))
    return src.rstrip("/").split("/")[-1], frag


def hack_selections():
    return [["build", "-p", m] for m in MEMBERS] + [["test", "--no-run", "-p", m] for m in MEMBERS
                                                     if m != "rustc_codegen_pliron"]


def hack_diff():
    """Third-party packages whose target-side features differ between selections:
    package id -> union of the features."""
    per = [unit_features(s) for s in hack_selections()]
    union = {}
    for d in per:
        for k, v in d.items():
            union.setdefault(k, set()).update(v)
    return {k: v for k, v in union.items() if any(k in d and d[k] != v for d in per)}


def hack_manifest(entries):
    meta = json.loads(subprocess.run(["cargo", "metadata", "--format-version", "1"], cwd=ROOT,
                                     capture_output=True, text=True).stdout)
    declared = {(p["name"], p["version"]): set(p["features"]) for p in meta["packages"]}
    names = [pkg_name_version(k)[0] for k in entries]
    lines = []
    for pid in sorted(entries, key=pkg_name_version):
        name, ver = pkg_name_version(pid)
        feats = sorted(f for f in entries[pid] if f != "default" and f in declared.get((name, ver), ()))
        key = name if names.count(name) == 1 else f"{name}-{ver.replace('.', '_')}"
        pk = "" if key == name else f'package = "{name}", '
        df = "" if "default" in entries[pid] else "default-features = false, "
        fs = ", ".join(f'"{f}"' for f in feats)
        lines.append(f'{key} = {{ {pk}version = "{ver}", {df}features = [{fs}] }}')
    return HACK_HEAD + "\n".join(lines) + "\n"


HACK_HEAD = '''# Generated by `harness/dev.py hack` -- do not edit by hand.
#
# Feature unification for the workspace (docs/dev-loop.md): the union of the
# features that the different cargo package selections (`cargo build`, `-p
# pliron-clif-check`, `-p rustc_codegen_pliron`, `cargo test -p ...`) turn on
# in shared third-party crates. pliron-cfg depends on this crate, so every
# selection builds the same cranelift-codegen and the same pliron-clif-opt
# instead of one copy per command. Rerun the generator after changing
# dependencies or their features; `harness/dev.py hack --check` verifies it.
[package]
name = "pliron-hack"
version = "0.1.0"
edition = "2024"
license = "MIT OR Apache-2.0"
publish = false

[lib]
path = "lib.rs"

[dependencies]
'''


def cmd_hack(args):
    p = os.path.join(ROOT, "pliron-hack/Cargo.toml")
    if "--check" in args:
        d = hack_diff()
        for k in sorted(d):
            print("  differs between selections:", "@".join(pkg_name_version(k)))
        print("dev hack: " + ("stale; run harness/dev.py hack" if d else "up to date"))
        return not d
    os.makedirs(os.path.dirname(p), exist_ok=True)
    lib = os.path.join(ROOT, "pliron-hack/lib.rs")
    if not os.path.exists(lib):
        open(lib, "w").write("//! Empty: this crate only unifies dependency features (see Cargo.toml).\n")
    have = open(p).read() if os.path.exists(p) else ""
    # Start from an empty list; a listed crate joins every selection and can pull
    # further features in, so repeat until no selection differs.
    entries = {}
    write = lambda text: open(p, "w").write(text)
    write(HACK_HEAD)
    for _ in range(6):
        d = {k: v for k, v in hack_diff().items() if pkg_name_version(k)[0] not in MEMBERS + ["pliron-hack"]}
        if not d:
            break
        for k, v in d.items():
            entries.setdefault(k, set()).update(v)
        write(hack_manifest(entries))
    else:
        print("dev hack: selections still differ after 6 rounds:", sorted("@".join(pkg_name_version(k)) for k in d))
        return False
    print(f"dev hack: pliron-hack/Cargo.toml {'unchanged' if open(p).read() == have else 'written'} ({len(entries)} crates)")
    return True


# ---- relax (../relax: a Rust JIT; runs the stable crates from source) --------------

RELAX_SRC = os.path.join(os.path.dirname(ROOT), "relax")
# RELAX_LAZY_BODIES=0: relax's lazy fn-body parser rejects combine's nested
# macro_rules (pliron-derive depends on it); RELAX_OFFLINE: never download.
RELAX_ENV = {"RELAX_LAZY_BODIES": "0", "RELAX_OFFLINE": "1"}


def relax_bin():
    for p in (os.environ.get("RELAX"), os.path.join(STATE, "relax/release/relax"),
              os.path.join(RELAX_SRC, "target/release/relax")):
        if p and os.path.exists(p):
            return p
    raise SystemExit("dev: no relax binary: harness/dev.py relax-build (needs ../relax), or set RELAX=<path>")


def cmd_relax_build(args):
    if not os.path.isdir(RELAX_SRC):
        raise SystemExit(f"dev relax-build: no checkout at {RELAX_SRC}")
    t = time.time()
    rc = subprocess.run(["cargo", "build", "--release", "-p", "relax-runner", "--bin", "relax",
                         "--target-dir", os.path.join(STATE, "relax")], cwd=RELAX_SRC).returncode
    print(f"dev relax-build: {'ok' if rc == 0 else 'FAILED'} in {time.time() - t:.0f}s")
    return rc == 0


def cmd_relax(args):
    cmd = [relax_bin(), "run", "--unattended", "--allow-c", "pliron-clif-check", "--", *args]
    t = time.time()
    rc = subprocess.run(cmd, cwd=ROOT, env=dict(os.environ, **RELAX_ENV)).returncode
    print(f"dev relax: exit {rc} in {time.time() - t:.1f}s")
    return rc == 0


def cmd_test(args):
    cargo = flag(args, "--cargo")
    pkg, sel, rest = "pliron-clif-opt", ["--lib"], []
    i = 0
    while i < len(args):
        if args[i] == "-p" and i + 1 < len(args):
            pkg = args[i + 1]
            i += 2
        elif args[i] == "--test" and i + 1 < len(args):
            sel = ["--test", args[i + 1]]
            i += 2
        elif args[i] == "--lib":
            i += 1
        else:
            rest.append(args[i])
            i += 1
    if cargo:
        t = time.time()
        prof = [] if PROFILE == "debug" else ["--profile", PROFILE]
        rc = subprocess.run(["cargo", "test", "-q", *prof, "-p", pkg, *sel, *rest], cwd=ROOT).returncode
        print(f"dev test: cargo {'ok' if rc == 0 else 'FAILED'} in {time.time() - t:.1f}s")
        return rc == 0
    # Stays up: prints a `testverdict` line after the first run and after every save.
    cmd = [relax_bin(), "test", "--live", pkg, *sel] + (["--", *rest] if rest else [])
    return subprocess.run(cmd, cwd=ROOT, env=dict(os.environ, **RELAX_ENV)).returncode == 0


def cmd_build(args):
    what = args[0] if args else "all"
    if what not in PKGS:
        raise SystemExit("dev build: check | dylib | all")
    print(f"dev build: {what} in {build(what):.1f}s -> {os.path.relpath(PDIR, ROOT)}")
    return True


COMMANDS = {"build": cmd_build, "clif": cmd_clif, "corpus": cmd_corpus, "rs": cmd_rs, "check": cmd_check,
            "bench": cmd_bench, "watch": cmd_watch, "hack": cmd_hack, "test": cmd_test, "relax": cmd_relax,
            "relax-build": cmd_relax_build}


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in COMMANDS:
        print(__doc__)
        return 2
    try:
        return 0 if COMMANDS[sys.argv[1]](sys.argv[2:]) else 1
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())
