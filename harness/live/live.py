#!/usr/bin/env -S python3 -I
"""Live loop: rerun the CLIF pipeline over the recorded corpora on every save.

Usage: harness/live/live.py [--port 8765] [--no-check] [--corpus smoke,std]

Watches the pass sources (pliron-clif-opt, pliron-cfg, pliron-clif-check).
On a change it rebuilds `clif-check` (dev profile, a few seconds), runs
`clif-check report [--check]` over each corpus in target/corpus/ (record
them with harness/corpus.sh), and serves a dashboard at
http://localhost:PORT/ that shows, against a saved baseline:

  - verifier errors, panics and interpreter mismatches (new ones first),
    each with the one-line `clif-check pipeline` repro,
  - code size (instruction count after the pipeline), total and the
    functions that moved most,
  - per-step pipeline time.

"Save baseline" on the page (or POST /baseline) snapshots the current
reports; the default baseline is the first run. Files: target/live/.
No rustc in the loop: an edit to a pass is judged on ~17k real functions
in about ten seconds.
"""
import argparse, http.server, json, os, shutil, subprocess, threading, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(ROOT, "target/live")
WATCH = ["pliron-clif-opt/src", "pliron-cfg/src", "pliron-clif-check/src"]
CLIF_CHECK = os.path.join(ROOT, "target/debug/clif-check")

status = {"state": "starting", "runs": 0, "corpora": [], "log": "", "changed": [],
          "build_s": None, "run_s": None, "finished": None}
lock = threading.Lock()


def write_status(**kw):
    with lock:
        status.update(kw)
        tmp = os.path.join(OUT, "status.json.tmp")
        json.dump(status, open(tmp, "w"))
        os.replace(tmp, os.path.join(OUT, "status.json"))


def snapshot():
    """mtime per watched .rs file."""
    m = {}
    for d in WATCH:
        for dp, _, fs in os.walk(os.path.join(ROOT, d)):
            for f in fs:
                if f.endswith(".rs"):
                    p = os.path.join(dp, f)
                    try:
                        m[p] = os.stat(p).st_mtime_ns
                    except OSError:
                        pass
    return m


def corpora(sel):
    d = os.path.join(ROOT, "target/corpus")
    have = sorted(os.listdir(d)) if os.path.isdir(d) else []
    return [c for c in have if not sel or c in sel]


def cycle(a, changed):
    write_status(state="building", changed=changed)
    t = time.time()
    b = subprocess.run(["cargo", "build", "-q", "-p", "pliron-clif-check"], cwd=ROOT,
                       capture_output=True, text=True)
    build_s = round(time.time() - t, 1)
    if b.returncode != 0:
        write_status(state="build-failed", log=b.stderr[-6000:], build_s=build_s, finished=time.time())
        return
    write_status(state="running", build_s=build_s, log="")
    t = time.time()
    names = corpora(a.corpus)
    for c in names:
        out = os.path.join(OUT, f"{c}.json")
        cmd = [CLIF_CHECK, "report", os.path.join(ROOT, "target/corpus", c), out]
        if not a.no_check:
            cmd.append("--check")
        r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
        if not os.path.exists(out) or r.returncode not in (0, 1):
            write_status(state="run-failed", log=(r.stdout + r.stderr)[-6000:])
            return
        base = os.path.join(OUT, "baseline", f"{c}.json")
        if not os.path.exists(base):
            os.makedirs(os.path.dirname(base), exist_ok=True)
            shutil.copy(out, base)
    with lock:
        runs = status["runs"] + 1
    write_status(state="idle", corpora=names, run_s=round(time.time() - t, 1), runs=runs,
                 finished=time.time())


def watcher(a):
    prev = snapshot()
    cycle(a, [])
    while True:
        time.sleep(0.5)
        cur = snapshot()
        if cur != prev:
            time.sleep(0.3)  # let the editor finish writing
            cur = snapshot()
            changed = sorted(os.path.relpath(p, ROOT) for p in set(cur) | set(prev)
                             if cur.get(p) != prev.get(p))
            prev = cur
            cycle(a, changed[:20])


class Handler(http.server.SimpleHTTPRequestHandler):
    def translate_path(self, path):
        path = path.split("?", 1)[0]
        if path in ("/", "/index.html"):
            return os.path.join(HERE, "index.html")
        if path.startswith("/data/"):
            return os.path.join(OUT, os.path.normpath(path[len("/data/"):]).lstrip("./"))
        if path.startswith("/gate/"):
            return os.path.join(ROOT, "target/gate", os.path.normpath(path[len("/gate/"):]).lstrip("./"))
        return os.path.join(HERE, "__none__")

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def do_POST(self):
        if self.path == "/baseline":
            os.makedirs(os.path.join(OUT, "baseline"), exist_ok=True)
            for c in status.get("corpora", []):
                src = os.path.join(OUT, f"{c}.json")
                if os.path.exists(src):
                    shutil.copy(src, os.path.join(OUT, "baseline", f"{c}.json"))
            self.send_response(204)
            self.end_headers()
        else:
            self.send_error(404)

    def log_message(self, *args):
        pass


def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("--port", type=int, default=8765)
    ap.add_argument("--no-check", action="store_true", help="skip the interpreter diff (faster)")
    ap.add_argument("--corpus", type=lambda s: set(filter(None, s.split(","))), default=set())
    a = ap.parse_args()
    os.makedirs(OUT, exist_ok=True)
    if not corpora(a.corpus):
        print("live: no corpus under target/corpus — run harness/corpus.sh smoke (and std) first")
    threading.Thread(target=watcher, args=(a,), daemon=True).start()
    print(f"live: http://localhost:{a.port}/  (watching {', '.join(WATCH)})", flush=True)
    http.server.ThreadingHTTPServer(("127.0.0.1", a.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
