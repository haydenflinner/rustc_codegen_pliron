#!/usr/bin/env -S python3 -I
"""Merge gate: everything a change must pass before it lands, in parallel.

Usage: harness/gate.py [--jobs N] [--skip unit,wasm,...] [--only a,b]
                       [--full] [--record]
       harness/gate.py --install-hook      run the gate from git's pre-push hook

Runs, after one `cargo build`:
  unit      cargo test for the stable crates (pliron-cfg, -clif-opt, -ir-opt)
  <stanza>  every `./test.sh` stanza (nostd, std, licm, ...) as its own
            job — TEST_NOBUILD=1 ./test.sh --only=<stanza>
  corpus:*  `clif-check report --check` over each recorded corpus under
            target/corpus/ (harness/corpus.sh); fails on any verifier error,
            panic, or interpreter mismatch not listed in
            harness/expectations/corpus-known.txt
The `wasm` stanza (tests/wasm/run.sh) is OFF by default: it rebuilds core/std
through the backend for every backend change (~4 min). The default gate runs
`wasm_smoke` — emit->link->run, ~1 s — instead. `--full` or `--only wasm`
adds the sysroot legs; run it before landing wasm/wpeep/linker changes.
--record (re)records the smoke and std corpora first (~40 s).

Logs: target/gate/<job>.log; summary: target/gate/summary.json (read by the
live dashboard). Exit status is nonzero when any job fails. Bypass the hook
with `git push --no-verify` — and say so in the commit/ledger.
"""
import argparse, concurrent.futures as cf, json, os, re, shutil, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target/gate")
KNOWN = os.path.join(ROOT, "harness/expectations/corpus-known.txt")
WIN = os.name == "nt"
CLIF_CHECK = os.path.join(ROOT, "target/debug", "clif-check.exe" if WIN else "clif-check")
# Bash for .sh legs: CreateProcess doesn't honor `#!/bin/sh`, so the
# shell scripts need an explicit interpreter on Windows.
BASH = shutil.which("bash") or shutil.which("sh")


# test.sh stanzas the gate also runs under PLIRON_SM_POISON (`poison:*`).
POISON_STANZAS = ["std", "unwind", "unwindstd", "loopvec", "pgo", "mirshapes", "inlinecm", "unroll", "sroa", "licm"]

# rustlantis seeds that exposed real bugs: slp same-address splat
# (538, 683, 730), loadfwd's dead-pair poisoning (810, 1276) and loop
# analysis hanging on unreachable preds (1776, compile timeout), and
# offset+width overflow in loadfwd/dse range math (2542), slp treating an
# escaped slot as disjoint from untracked pointers (3938) and cparm chasing a
# replacement cycle forever (2187, compile timeout), 2026-10-22; slp offset
# adds and loopvec's coefficient negation overflowing at the i64 edge
# (5042, 5542, 4895), and slp moving a store past a narrow `uload32` of its
# bytes (4940), 2026-10-24; slotmerge's starts-mode coalescing ignoring an
# earlier pair's copy window (13475, -O0), 2026-10-27.
FUZZ_REGRESS = [538, 683, 730, 810, 1276, 1776, 1939, 2187, 2542, 2598, 3441, 3938, 4895, 4940, 5042, 5542, 8631, 12309,
                13475]


def shell(cmd):
    """Prefix `bash`/`sh` for `.sh` commands on Windows (no shebang exec)."""
    if WIN and cmd and cmd[0].endswith(".sh"):
        return [(BASH or "bash")] + [cmd[0].replace("./", "", 1) if cmd[0].startswith("./") else cmd[0]] + cmd[1:]
    return cmd


def stanzas():
    src = open(os.path.join(ROOT, "test.sh")).read()
    names = re.findall(r"want (\w+) && t_\w+", src)
    return [n for n in names if n not in ("sysroot", "mold")]


def run(name, cmd, env=None, timeout=1800):
    log = os.path.join(OUT, name.replace(":", "_") + ".log")
    t = time.time()
    with open(log, "w") as f:
        try:
            r = subprocess.run(cmd, cwd=ROOT, stdout=f, stderr=subprocess.STDOUT, timeout=timeout,
                               env=dict(os.environ, **(env or {})))
            rc = r.returncode
        except subprocess.TimeoutExpired:
            rc = "timeout"
    return {"job": name, "ok": rc == 0, "rc": rc, "secs": round(time.time() - t, 1), "log": log}


def corpus_job(dirname):
    name = f"corpus:{dirname}"
    out = os.path.join(OUT, f"corpus_{dirname}.json")
    res = run(name, [CLIF_CHECK, "report", os.path.join(ROOT, "target/corpus", dirname), out, "--check"])
    known = set()
    if os.path.exists(KNOWN):
        known = {l.split("#")[0].strip() for l in open(KNOWN)} - {""}
    try:
        rep = json.load(open(out))
    except (OSError, ValueError):
        res["ok"] = False
        return res
    bad = [f for f in rep["functions"] if f["status"] in ("mismatch", "verifier", "panic")]
    new = [f for f in bad if f"{dirname}/{f['file']}" not in known]
    res.update(ok=not new, counts=rep["counts"], new_failures=[f"{f['status']}: {f['file']}" for f in new][:50])
    return res


def install_hook():
    hooks = subprocess.check_output(["git", "rev-parse", "--git-common-dir"], cwd=ROOT, text=True).strip()
    path = os.path.join(ROOT, hooks, "hooks", "pre-push")
    with open(path, "w") as f:
        f.write("#!/bin/sh\n# Installed by harness/gate.py --install-hook. Bypass: git push --no-verify\n"
                "exec python harness/gate.py\n")
    os.chmod(path, 0o755)
    print(f"gate: installed {path}")


def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("--jobs", type=int, default=max(2, (os.cpu_count() or 4) // 2))
    ap.add_argument("--skip", default="")
    ap.add_argument("--only", default="")
    ap.add_argument("--full", action="store_true",
                    help="include heavyweight legs off by default (wasm sysroot builds)")
    ap.add_argument("--record", action="store_true")
    ap.add_argument("--install-hook", action="store_true")
    a = ap.parse_args()
    if a.install_hook:
        return install_hook()
    os.makedirs(OUT, exist_ok=True)
    for f in os.listdir(OUT):
        os.remove(os.path.join(OUT, f))
    os.environ.setdefault("RUSTC_ICE", os.path.join(ROOT, "target/ices"))
    t0 = time.time()
    rev = subprocess.run(["git", "describe", "--always", "--dirty"], cwd=ROOT, capture_output=True,
                         text=True).stdout.strip()
    summary = {"rev": rev, "started": int(t0), "jobs": [], "state": "running"}

    def save():
        tmp = os.path.join(OUT, "summary.json.tmp")
        json.dump(summary, open(tmp, "w"), indent=1)
        os.replace(tmp, os.path.join(OUT, "summary.json"))

    save()
    build = run("build", ["cargo", "build", "-p", "rustc_codegen_pliron", "-p", "pliron-clif-check"])
    summary["jobs"].append(build)
    if not build["ok"]:
        summary["state"] = "failed"
        save()
        print(open(build["log"]).read()[-3000:])
        sys.exit("gate: build failed")
    if a.record:
        for s in ("smoke", "std"):
            summary["jobs"].append(run(f"record:{s}", shell(["harness/corpus.sh", s])))
    skip = set(filter(None, a.skip.split(",")))
    only = set(filter(None, a.only.split(",")))
    want = lambda n: (not only or n in only or n.split(":")[0] in only) and n not in skip and n.split(":")[0] not in skip
    jobs = []
    if want("unit"):
        jobs.append(("unit", lambda: run("unit", ["cargo", "test", "-q", "-p", "pliron-cfg", "-p",
                                                  "pliron-clif-opt", "-p", "pliron-ir-opt"])))
    # Slowest first so the pool's tail is short.
    order = sorted(stanzas(), key=lambda n: n not in ("pgo", "std", "unwind"))
    for n in order:
        if want(n):
            jobs.append((n, lambda n=n: run(n, shell(["./test.sh", f"--only={n}"]), {"TEST_NOBUILD": "1"})))
    # The same stanzas under `PLIRON_SM_POISON=1`: slotmerge merges nothing
    # and overwrites every slot it would consider with 0xA5 outside its
    # computed live interval, so a wrong interval fails at the first stale
    # read even where no merge would have reused the bytes. It found four
    # real slotmerge liveness bugs (docs/history/2026-10-24-*). Slot-heavy
    # stanzas only; each in its own TEST_OUT so it can't race its plain run.
    for n in POISON_STANZAS:
        if want(f"poison:{n}") and n in order:
            jobs.append((f"poison:{n}", lambda n=n: run(f"poison:{n}", shell(["./test.sh", f"--only={n}"]),
                         {"TEST_NOBUILD": "1", "PLIRON_SM_POISON": "1",
                          "TEST_OUT": f"target/tests-poison/{n}"})))
    # The wasm sysroot leg is opt-in: four -Zbuild-std builds per backend
    # change makes it ~10x every other job. wasm_smoke covers emit->link->run.
    if (a.full or "wasm" in only) and "wasm" not in skip:
        jobs.append(("wasm", lambda: run("wasm", shell(["./test.sh", "--only=wasm"]),
                                         {"TEST_NOBUILD": "1"})))
    # rustlantis seeds that found miscompiles (needs the sibling rustlantis
    # generator; the job reports a skip without it).
    if want("fuzzreg"):
        seeds = ",".join(str(s) for s in FUZZ_REGRESS)
        jobs.append(("fuzzreg", lambda: run("fuzzreg", [sys.executable, "-I", "harness/run.py", "--suite", "fuzz",
                                                        "--no-build", "--fuzz-seeds", seeds])))
    cdir = os.path.join(ROOT, "target/corpus")
    for d in sorted(os.listdir(cdir)) if os.path.isdir(cdir) else []:
        if want(f"corpus:{d}"):
            jobs.append((f"corpus:{d}", lambda d=d: corpus_job(d)))
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        futs = {ex.submit(fn): n for n, fn in jobs}
        for fut in cf.as_completed(futs):
            r = fut.result()
            summary["jobs"].append(r)
            save()
            print(f"  {'ok  ' if r['ok'] else 'FAIL'} {r['job']:<16} {r['secs']:>6}s"
                  + ("" if r["ok"] else f"  {r['log']}"), flush=True)
            for nf in r.get("new_failures", [])[:5]:
                print(f"       {nf}")
    bad = [j for j in summary["jobs"] if not j["ok"]]
    summary.update(state="failed" if bad else "passed", secs=round(time.time() - t0, 1))
    save()
    print(f"gate: {'FAILED' if bad else 'passed'} — {len(summary['jobs'])} jobs in {summary['secs']}s ({rev})")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
