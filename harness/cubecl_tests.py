#!/usr/bin/env -S python3 -I
"""Run a CubeCL / Burn test suite under LLVM and under CUBECL_PLIRON_CLIF=1
and record pass/fail per test.

Usage:
  harness/cubecl_tests.py --dir /path/to/workspace --suite NAME \
      [--threads 8] [--only llvm|clif] [--timeout 1800] -- <cargo test args>

  e.g. harness/cubecl_tests.py --dir ~/code/cubecl --suite cubecl-cpu -- --release -p cubecl-cpu
       harness/cubecl_tests.py --dir ~/code/burn --suite burn-cpu -- --release -p burn-backend-tests \
           --no-default-features --features cpu,std,fusion

It builds the test binaries once (`cargo test --no-run`), then runs every
binary once per path with its own threads. A binary that dies (segfault,
abort, timeout) without a summary has its unfinished tests rerun one process
each, so one crash costs one test, not the binary. The LLVM-path run also
checks that no kernel went through Cranelift and the CLIF-path run that none
went through LLVM (CUBECL_PLIRON_LOG=1 prints one `[cubecl-pliron] path=...`
line per compiled kernel).

Results merge into target/parity/cubecl-tests.json under --suite:
  {suite: {"llvm": {"passed": n, "failed": [...], "ignored": n, "kernels": n},
           "clif": {...}}}
which harness/cubecl_card.py reads for the COR category.
"""
import argparse, json, os, re, subprocess, sys, time
from concurrent.futures import ThreadPoolExecutor

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target/parity")
LINE = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)", re.M)


def build(dirp, cargo_args):
    p = subprocess.run(["cargo", "test", "--no-run", "--message-format=json"] + cargo_args,
                       cwd=dirp, capture_output=True, text=True,
                       env={**os.environ, "CARGO_BUILD_JOBS": os.environ.get("CARGO_BUILD_JOBS", "3")})
    bins = []
    for line in p.stdout.splitlines():
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if m.get("reason") == "compiler-artifact" and m.get("executable") and m["profile"]["test"]:
            bins.append((m["executable"], os.path.dirname(m["manifest_path"]), m["target"]["name"]))
    if p.returncode != 0 or not bins:
        sys.exit("build failed:\n" + p.stderr[-3000:])
    return bins


def run(cmd, cwd, env, timeout):
    try:
        r = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout,
                           errors="replace")
        return r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired as e:
        return -999, (e.stdout or b"").decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or ""), \
            (e.stderr or b"").decode(errors="replace") if isinstance(e.stderr, bytes) else (e.stderr or "")


def one_path(bins, clif, threads, timeout):
    import tempfile
    logf = tempfile.mktemp(prefix="cubecl-pliron-", suffix=".log")
    env = {**os.environ, "CUBECL_PLIRON_LOG": logf, "RUST_BACKTRACE": "0"}
    env.pop("CUBECL_PLIRON_CLIF", None)
    if clif:
        env["CUBECL_PLIRON_CLIF"] = "1"
    passed, failed, ignored, kernels, wrong = 0, [], 0, {"clif": 0, "llvm": 0}, 0
    for exe, cwd, name in bins:
        rc, so, _ = run([exe, "--list"], cwd, env, 300)
        listed = [l[:-6] for l in so.splitlines() if l.endswith(": test")]
        if not listed:
            continue
        rc, so, se = run([exe, "--test-threads", str(threads)], cwd, env, timeout)
        res = {m.group(1): m.group(2) for m in LINE.finditer(so)}
        todo = [t for t in listed if t not in res]
        if todo:
            print(f"  {name}: exit {rc}, {len(todo)} unfinished tests rerun singly", flush=True)

            def single(t):
                rc, so, se = run([exe, "--exact", t, "--test-threads", "1"], cwd, env, 300)
                ok = rc == 0 and re.search(r"test \S+ \.\.\. ok", so) is not None
                ig = rc == 0 and "ignored" in so
                return t, "ok" if ok else ("ignored" if ig else "FAILED")

            with ThreadPoolExecutor(max_workers=threads) as ex:
                for t, st in ex.map(single, todo):
                    res[t] = st
        for t, st in res.items():
            full = f"{name}::{t}"
            if st == "ok":
                passed += 1
            elif st == "ignored":
                ignored += 1
            else:
                failed.append(full)
    if os.path.exists(logf):
        txt = open(logf).read()
        for k in ("clif", "llvm"):
            kernels[k] = len(re.findall(rf"\[cubecl-pliron\] path={k} ", txt))
        os.unlink(logf)
    other = "llvm" if clif else "clif"
    return {"passed": passed, "failed": sorted(failed), "ignored": ignored,
            "kernels": kernels["clif" if clif else "llvm"], "kernels_on_other_path": kernels[other]}


def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("--dir", required=True)
    ap.add_argument("--suite", required=True)
    ap.add_argument("--threads", type=int, default=8)
    ap.add_argument("--only", choices=["llvm", "clif"])
    ap.add_argument("--timeout", type=int, default=1800)
    ap.add_argument("cargo_args", nargs="*")
    a = ap.parse_args()
    bins = build(os.path.expanduser(a.dir), a.cargo_args)
    print(f"{a.suite}: {len(bins)} test binaries", flush=True)
    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, "cubecl-tests.json")
    for clif in (False, True):
        key = "clif" if clif else "llvm"
        if a.only and a.only != key:
            continue
        t0 = time.time()
        r = one_path(bins, clif, a.threads, a.timeout)
        r["seconds"] = round(time.time() - t0)
        all_ = json.load(open(path)) if os.path.exists(path) else {}
        all_.setdefault(a.suite, {})[key] = r
        json.dump(all_, open(path, "w"), indent=1)
        print(f"{a.suite} [{key}]: {r['passed']} passed, {len(r['failed'])} failed, {r['ignored']} ignored, "
              f"{r['kernels']} kernels compiled ({r['kernels_on_other_path']} on the other path), {r['seconds']} s",
              flush=True)


if __name__ == "__main__":
    main()
