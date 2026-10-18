#!/usr/bin/env -S python3 -I
"""Merge gate: everything a change must pass before it lands, in parallel.

Usage: harness/gate.py [--jobs N] [--skip unit,wasm,...] [--only a,b] [--record]
       harness/gate.py --install-hook      run the gate from git's pre-push hook

Runs, after one `cargo build`:
  unit      cargo test for the stable crates (pliron-cfg, -clif-opt, -ir-opt)
  <stanza>  every `./test.sh` stanza (nostd, std, licm, ..., wasm) as its own
            job — TEST_NOBUILD=1 ./test.sh --only=<stanza>
  corpus:*  `clif-check report --check` over each recorded corpus under
            target/corpus/ (harness/corpus.sh); fails on any verifier error,
            panic, or interpreter mismatch not listed in
            harness/expectations/corpus-known.txt
--record (re)records the smoke and std corpora first (~40 s).

Logs: target/gate/<job>.log; summary: target/gate/summary.json (read by the
live dashboard). Exit status is nonzero when any job fails. Bypass the hook
with `git push --no-verify` — and say so in the commit/ledger.
"""
import argparse, concurrent.futures as cf, json, os, re, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target/gate")
KNOWN = os.path.join(ROOT, "harness/expectations/corpus-known.txt")
CLIF_CHECK = os.path.join(ROOT, "target/debug/clif-check")


def stanzas():
    src = open(os.path.join(ROOT, "test.sh")).read()
    names = re.findall(r"want (\w+) && t_\w+", src)
    return [n for n in names if n not in ("sysroot", "mold")] + ["wasm"]


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
                "exec harness/gate.py\n")
    os.chmod(path, 0o755)
    print(f"gate: installed {path}")


def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("--jobs", type=int, default=max(2, (os.cpu_count() or 4) // 2))
    ap.add_argument("--skip", default="")
    ap.add_argument("--only", default="")
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
            summary["jobs"].append(run(f"record:{s}", ["harness/corpus.sh", s]))
    skip = set(filter(None, a.skip.split(",")))
    only = set(filter(None, a.only.split(",")))
    want = lambda n: (not only or n in only or n.split(":")[0] in only) and n not in skip and n.split(":")[0] not in skip
    jobs = []
    if want("unit"):
        jobs.append(("unit", lambda: run("unit", ["cargo", "test", "-q", "-p", "pliron-cfg", "-p",
                                                  "pliron-clif-opt", "-p", "pliron-ir-opt"])))
    # Slowest first so the pool's tail is short.
    order = sorted(stanzas(), key=lambda n: n not in ("wasm", "pgo", "std", "unwind"))
    for n in order:
        if want(n):
            jobs.append((n, lambda n=n: run(n, ["./test.sh", f"--only={n}"], {"TEST_NOBUILD": "1"})))
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
