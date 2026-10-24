#!/usr/bin/env -S python3 -I
"""rustc's lldb debuginfo suite (tests/debuginfo) under stock rustc and pliron.

  harness/dbgsuite.py [--suite DIR] [--opt 0,2] [--side stock,pliron]
                      [--only a,b] [--jobs N] [--lldb PATH] [--be DYLIB]
                      [--save NAME] [--diff]

A light stand-in for compiletest's debuginfo mode: per test (and per
revision), the `//@ compile-flags`, `aux-build`, `only-*`/`ignore-*`
directives, `#break` lines and `lldb-command`/`lldb-check` directives are
read as compiletest reads them; the test is built by each side (plus
`-Copt-level=N` for `--opt N` > 0), then run under lldb through the
suite's own `src/etc/debugger_tester` driver with the same script
compiletest writes; `lldb-check` lines must appear in order, `[...]`
being a wildcard. A side's build failure counts as a failure.

`--opt 2` is not a rustc mode (compiletest builds debuginfo tests
unoptimized) — most checks name locals that optimization legitimately
loses, so read its numbers as stock vs pliron, not as absolute.

Outputs target/dbgsuite/O<n>/<side>/<test>/{exe,script,out,build.log},
target/dbgsuite/report.md and summary.json. `--save NAME` also writes the
per-test pass set to harness/expectations/dbgsuite-NAME.json; `--diff`
reports tests whose result changed against that file.
"""
import argparse, concurrent.futures as cf, json, os, re, shutil, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target", "dbgsuite")
TOOLCHAIN = "nightly-2026-10-22"
# Apple's lldb is what rustc's CI runs this suite with on macOS; stock passes
# it cleanly there (LLVM's lldb 20 misrenders some of stock's types).
XCODE_LLDB = "/Applications/Xcode.app/Contents/Developer/usr/bin/lldb"
DEF_SUITE = os.path.join(os.path.dirname(ROOT), "rust-sh", "tests", "debuginfo")

# Revision/target conditions as compiletest evaluates them on this host.
HOST_TRUE = {"apple", "macos", "64bit", "aarch64", "unix", "non-msvc"}
HOST_FALSE = {"msvc", "windows", "cdb", "gdb", "linux", "x86_64", "32bit", "android", "windows-gnu"}


def directives(path):
    """(line_no, rev or None, name, value) for each `//@` directive, and the
    `#break` lines."""
    out, breaks = [], []
    for i, line in enumerate(open(path, encoding="utf-8", errors="replace"), 1):
        if "#break" in line:
            breaks.append(i)
            continue
        m = re.match(r"\s*//@\s*(?:\[([\w,-]+)\])?\s*([\w-]+)\s*(?::(.*))?$", line.rstrip("\n"))
        if m:
            out.append((i, m.group(1), m.group(2), m.group(3)))
    return out, breaks


def cond_false(name, val):
    """True when an only-/ignore-/needs- directive rules the test out here."""
    if name == "ignore-lldb" or name.startswith("only-") and name[5:] in ("cdb", "gdb"):
        return True
    if name.startswith("only-"):
        c = name[5:]
        return c in HOST_FALSE or (c not in HOST_TRUE and c not in ("lldb",))
    if name.startswith("ignore-"):
        c = name[7:]
        if c in ("backends", "gdb", "cdb", "android", "windows", "msvc", "linux", "x86_64",
                 "32bit", "gdb-version", "windows-gnu", "test"):
            return False
        return c in HOST_TRUE or c == "lldb"
    if name in ("needs-rust-lldb",):
        return False
    return False


def check_line(line, pat):
    """compiletest's `check_single_line`."""
    line, pat = line.strip(), pat.strip()
    start_any, end_any = pat.startswith("[...]"), pat.endswith("[...]")
    frags = [f for f in pat.split("[...]") if f]
    if not frags:
        return True
    if start_any:
        p = line.find(frags[0])
        if p < 0:
            return False
        rest, k = line[p + len(frags[0]):], 1
    else:
        rest, k = line, 0
    for f in frags[k:]:
        p = rest.find(f)
        if p < 0:
            return False
        rest = rest[p + len(f):]
    return end_any or rest == ""


def cases(suite, only):
    """(name, path, rev) for every lldb test and revision."""
    files = []
    for e in sorted(os.listdir(suite)):
        p = os.path.join(suite, e)
        if e.endswith(".rs"):
            files.append((e[:-3], p))
        elif os.path.isfile(os.path.join(p, "main.rs")):
            files.append((e, os.path.join(p, "main.rs")))
    for name, p in files:
        ds, _ = directives(p)
        if not any(n == "lldb-command" for _, _, n, _ in ds):
            continue
        revs = [None]
        for _, r, n, v in ds:
            if n == "revisions" and r is None:
                revs = v.split()
        for rev in revs:
            full = name if rev is None else f"{name}#{rev}"
            if only and not any(o == name or o == full for o in only):
                continue
            yield full, p, rev


def applies(r, rev):
    return r is None or (rev is not None and rev in r.split(","))


def plan(path, rev):
    """Directives for one revision, or a skip reason."""
    ds, breaks = directives(path)
    flags, aux, cmds, checks, edition = [], [], [], [], None
    for i, r, n, v in ds:
        if not applies(r, rev):
            continue
        if cond_false(n, v):
            return f"{n}" + (f" [{r}]" if r else "")
        if n == "compile-flags":
            flags += v.split()
        elif n == "edition":
            edition = v.strip()
        elif n in ("aux-build", "aux-crate"):
            aux.append(v.strip().split("=")[-1])
        elif n == "lldb-command":
            cmds.append(v)
        elif n == "lldb-check":
            checks.append((i, v))
        elif n == "lldb-repr":
            cmds.append(f"repr {v.strip()}")
            checks.append((i, f"{v}: Ok"))
    if rev:
        flags += ["--cfg", rev.lower().replace("-", "_")]
    if edition:
        flags += ["--edition", edition]
    return dict(flags=flags, aux=aux, cmds=cmds, checks=checks, breaks=breaks)


def rustc(side, be):
    cmd = ["rustup", "run", TOOLCHAIN, "rustc"]
    if side == "pliron":
        cmd.append(f"-Zcodegen-backend={be}")
    return cmd


def run_case(a, name, path, rev, side, opt):
    p = plan(path, rev)
    if isinstance(p, str):
        return dict(test=name, side=side, opt=opt, result="skip", why=p)
    d = os.path.join(OUT, f"O{opt}", side, name.replace("#", "."))
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    env = dict(os.environ, RUSTC_ICE=os.path.join(d, "ices"))
    extra = [f"-Copt-level={opt}"] if opt else []
    log = open(os.path.join(d, "build.log"), "w")
    base = rustc(side, a.be)
    for ax in p["aux"]:
        src = os.path.join(os.path.dirname(path), "auxiliary", ax)
        r = subprocess.run(base + ["-g", "--crate-type=rlib", "--out-dir", d, src] + extra,
                           stdout=log, stderr=subprocess.STDOUT, env=env, cwd=d)
        if r.returncode:
            return dict(test=name, side=side, opt=opt, result="fail", why="aux build failed")
    exe = os.path.join(d, "exe")
    cmd = base + [path, "-o", exe, "-L", d] + p["flags"] + extra
    try:
        r = subprocess.run(cmd, stdout=log, stderr=subprocess.STDOUT, env=env, cwd=d, timeout=300)
    except subprocess.TimeoutExpired:
        return dict(test=name, side=side, opt=opt, result="fail", why="build timeout")
    if r.returncode:
        return dict(test=name, side=side, opt=opt, result="fail", why="build failed")
    etc = os.path.join(os.path.dirname(os.path.dirname(a.suite)), "src", "etc")
    src_name = os.path.basename(path)
    script = ["settings set auto-confirm true", "settings set target.inherit-tcc true", "version",
              f"command script import {etc}/lldb_lookup.py", "script print(lldb_lookup.FEATURE_FLAGS)"]
    script += [f"breakpoint set --file '{src_name}' --line {l}" for l in p["breaks"]]
    script += p["cmds"] + ["", "quit"]
    sp = os.path.join(d, "script")
    open(sp, "w").write("\n".join(script) + "\n")
    lenv = dict(os.environ, DEBUGGER_TESTER_TARGET_PATH=exe, DEBUGGER_TESTER_SCRIPT_PATH=sp,
                DEBUGGER_TESTER_INPUT_DATA_PATH=os.path.join(os.path.dirname(path), "lldb_input",
                                                             "non_windows.json"),
                DEBUGGER_TESTER_BLESS_TEST_DATA="0", DEBUGGER_TESTER_TARGET_TRIPLE="aarch64-apple-darwin",
                DEBUGGER_TESTER_DEBUGGER="lldb", PYTHONUNBUFFERED="1", PYTHONPATH=etc)
    try:
        r = subprocess.run([a.lldb, "--batch", "--one-line",
                            "script --language python -- import debugger_tester; debugger_tester.main()"],
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=lenv, cwd=d, timeout=90,
                           text=True, errors="replace")
        out, rc = r.stdout, r.returncode
    except subprocess.TimeoutExpired as e:
        out, rc = (e.stdout or b"").decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or ""), -1
    open(os.path.join(d, "out"), "w").write(out)
    lines = out.splitlines()
    missing, last = [], 0
    for ln, pat in p["checks"]:
        for k in range(last, len(lines)):
            if check_line(lines[k], pat):
                last = k
                break
        else:
            missing.append((ln, pat))
    res = "pass" if rc == 0 and not missing else "fail"
    why = "" if res == "pass" else (f"lldb exit {rc}; " if rc else "") + \
        f"{len(missing)}/{len(p['checks'])} checks missing" + (f", first L{missing[0][0]}: {missing[0][1].strip()}" if missing else "")
    return dict(test=name, side=side, opt=opt, result=res, why=why,
                checks=len(p["checks"]), missing=len(missing))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--suite", default=DEF_SUITE)
    ap.add_argument("--opt", default="0,2")
    ap.add_argument("--side", default="stock,pliron")
    ap.add_argument("--only", default="")
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--lldb", default=XCODE_LLDB if os.path.exists(XCODE_LLDB) else (shutil.which("lldb") or "lldb"))
    ap.add_argument("--be", default=os.path.join(ROOT, "target", "debug", "librustc_codegen_pliron.dylib"))
    ap.add_argument("--save", default="")
    ap.add_argument("--diff", default="")
    a = ap.parse_args()
    a.suite = os.path.abspath(a.suite)
    global OUT
    OUT = os.path.join(OUT, "apple" if "Xcode" in a.lldb or a.lldb == "/usr/bin/lldb" else "llvm")
    only = [o for o in a.only.split(",") if o]
    cs = list(cases(a.suite, only))
    opts = [int(o) for o in a.opt.split(",")]
    sides = a.side.split(",")
    jobs = [(n, p, r, s, o) for o in opts for s in sides for n, p, r in cs]
    os.makedirs(OUT, exist_ok=True)
    res = []
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        futs = [ex.submit(run_case, a, *j) for j in jobs]
        for f in cf.as_completed(futs):
            r = f.result()
            res.append(r)
            if r["result"] != "skip":
                print(f"{r['result']:4} O{r['opt']} {r['side']:6} {r['test']}  {r.get('why', '')}", flush=True)
    # --only: merge into the previous full run's results.
    sp = os.path.join(OUT, "summary.json")
    if only and os.path.exists(sp):
        new = {(r["opt"], r["side"], r["test"]) for r in res}
        res += [r for r in json.load(open(sp))["results"] if (r["opt"], r["side"], r["test"]) not in new]
        opts = sorted({r["opt"] for r in res})
        sides = [s for s in ("stock", "pliron") if any(r["side"] == s for r in res)]
    res.sort(key=lambda r: (r["opt"], r["test"], r["side"]))
    by = {(r["opt"], r["side"], r["test"]): r for r in res}
    tests = sorted({r["test"] for r in res if r["result"] != "skip"})
    md = ["# lldb debuginfo suite: stock vs pliron", "",
          f"suite `{a.suite}`, lldb `{a.lldb}`", ""]
    summ = {}
    for o in opts:
        md += [f"## O{o}", "", "| side | pass | of | checks found |", "|---|---|---|---|"]
        for s in sides:
            rs = [by[(o, s, t)] for t in tests if (o, s, t) in by]
            n = sum(r["result"] == "pass" for r in rs)
            ck = sum(r.get("checks", 0) for r in rs)
            found = ck - sum(r.get("missing", r.get("checks", 0)) for r in rs)
            summ[f"O{o}/{s}"] = dict(pass_=n, of=len(rs), checks=ck, found=found,
                                     passed=sorted(r["test"] for r in rs if r["result"] == "pass"))
            md.append(f"| {s} | {n} | {len(rs)} | {found}/{ck} |")
        md += ["", "| test | " + " | ".join(sides) + " |", "|---|" + "---|" * len(sides)]
        for t in tests:
            cells = []
            for s in sides:
                r = by.get((o, s, t))
                cells.append("" if r is None else ("ok" if r["result"] == "pass" else "FAIL " + r.get("why", "")))
            if len(set(c[:2] for c in cells)) > 1 or "FAIL" in cells[0][:4]:
                md.append(f"| {t} | " + " | ".join(cells) + " |")
        md.append("")
    skipped = sorted({(r["test"], r["why"]) for r in res if r["result"] == "skip"})
    md += ["## skipped", ""] + [f"- {t}: {w}" for t, w in skipped]
    open(os.path.join(OUT, "report.md"), "w").write("\n".join(md) + "\n")
    json.dump(dict(summary=summ, results=res), open(os.path.join(OUT, "summary.json"), "w"), indent=1)
    for k, v in summ.items():
        print(f"{k}: {v['pass_']}/{v['of']} tests, {v['found']}/{v['checks']} checks")
    exp = lambda n: os.path.join(ROOT, "harness", "expectations", f"dbgsuite-{n}.json")
    if a.diff:
        old = json.load(open(exp(a.diff)))
        for k, v in summ.items():
            if k not in old:
                continue
            was, now = set(old[k]), set(v["passed"])
            for t in sorted(was - now):
                print(f"REGRESSED {k} {t}")
            for t in sorted(now - was):
                print(f"fixed     {k} {t}")
    if a.save:
        json.dump({k: v["passed"] for k, v in summ.items()}, open(exp(a.save), "w"), indent=1)
    print(f"report: {os.path.join(OUT, 'report.md')}")


if __name__ == "__main__":
    main()
