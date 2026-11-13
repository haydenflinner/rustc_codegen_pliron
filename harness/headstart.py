#!/usr/bin/env -S python3 -I
"""Cargo build schedules, and what "headstart" would do to them.

headstart (github.com/PowderworksCode/headstart, no licence: nothing of it
is copied here) is a rustc + cargo patch series: rustc writes an early
`.rmeta` once item interfaces are checked, cargo starts dependents on it,
and a dependent waits for the full metadata only before code generation.
It needs a patched rustc and cargo; `docs/headstart.md` says why that
toolchain cannot be built for this backend yet. This tool does what can
be done on the pinned nightly:

  record   run a cargo build under a RUSTC_WRAPPER that timestamps every
           rustc: start, interface checks done, full metadata, link, end
           (`-Ztime-passes` lines, read as they arrive and not passed on).
  report   the recorded schedule (how busy the build kept the machine, its
           critical path), a replay of today's cargo rules as a check of
           the model, and the same build replayed under headstart's rules:
           an upper bound on what the patches would save.
  port-check  fetch headstart at the commit reviewed and dry-run its rustc
           patches on the pinned nightly's commit (index only, in a
           shared clone; the rust checkout is not touched).
  selftest    the simulator on a three-crate chain with known answers.

  harness/headstart.py record --heldout regex --pliron      # or: record [-o DIR] [--pliron] -- cargo build ...
  harness/headstart.py report regex-pliron [--table] [-j N] [--early 0.2]

`-j N` on `report` replays under another job limit; the "model" column
is then no longer comparable with the observed time.
"""
import argparse, heapq, json, math, os, re, resource, shutil, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target", "headstart")
TOOLCHAIN = "nightly-2026-10-22"
# The headstart revision this was written against (2026-10-20), and the
# rust-lang/rust commit its patches are made for.
HS_URL = "https://github.com/PowderworksCode/headstart"
HS_REV = "02ec0bf314ca9628844f6dbfdd6cc8a9e7ea1fe7"
HS_BASE = "6006fd03947bd7b04ae5cb5f7f9bb66b44dc8c6e"
LOG_ENV = "PLIRON_SCHED_LOG"
KEEP = ("coherence_checking", "type_check_crate", "generate_crate_metadata", "codegen_crate", "link", "total")
LINKING = {"bin", "proc-macro", "dylib", "cdylib", "staticlib"}
TIME_RE = re.compile(r"^time:\s+([0-9.]+);.*\t(\S+)\s*$")


# ------------------------------------------------------------ wrapper

def wrapper(argv):
    """`RUSTC_WRAPPER` mode: argv = [rustc, args...]."""
    log = os.environ[LOG_ENV]
    args = argv[1:]
    u = {"name": None, "types": [], "externs": [], "extra": "", "test": False, "emit": ""}
    prev = None
    for a in args:
        if prev == "--crate-name":
            u["name"] = a
        elif prev == "--crate-type":
            u["types"].append(a)
        elif prev == "--extern":
            u["externs"].append(a.rpartition("=")[2])
        elif prev == "-C" and a.startswith("extra-filename="):
            u["extra"] = a.partition("=")[2]
        elif a.startswith("--emit="):
            u["emit"] = a[7:]
        elif a == "--test":
            u["test"] = True
        prev = a
    if not u["name"] or u["name"] == "___" or "--print" in args or any(a.startswith("--print=") for a in args):
        os.execvp(argv[0], argv)  # cargo's probes
    for k in ("OUT_DIR", "CARGO_MANIFEST_DIR"):
        u[k] = os.environ.get(k)
    u["start"] = time.time()
    # close_fds=False: rustc must inherit cargo's jobserver descriptors.
    p = subprocess.Popen([argv[0], "-Ztime-passes", *args], stderr=subprocess.PIPE, close_fds=False)
    passes, arts = [], []
    err = sys.stderr.buffer
    for raw in p.stderr:
        now = time.time()
        if raw.startswith(b"time:"):
            m = TIME_RE.match(raw.decode("utf-8", "replace"))
            if m:
                if m.group(2) in KEEP:
                    passes.append((m.group(2), float(m.group(1)), now))
                continue
        elif raw.startswith(b'{"$message_type":"artifact"'):
            try:
                j = json.loads(raw)
                arts.append((j.get("emit"), now))
            except ValueError:
                pass
        err.write(raw)
        err.flush()
    u["status"] = p.wait()
    u["end"] = time.time()
    ru = resource.getrusage(resource.RUSAGE_CHILDREN)
    u["cpu"] = ru.ru_utime + ru.ru_stime
    u["passes"], u["artifacts"] = passes, arts
    with open(os.path.join(log, f"{os.getpid()}-{int(u['start'] * 1e6)}.json"), "w") as f:
        json.dump(u, f)
    sys.exit(128 - u["status"] if u["status"] < 0 else u["status"])  # killed by a signal: as a shell reports it


# ------------------------------------------------------------- record

def heldout_cmd(prog, tdir):
    src = os.path.join(ROOT, "target", "heldout", "src", prog, "rust-benchmark")
    if not os.path.isdir(src):
        sys.exit(f"headstart: {src} missing; run bench/heldout/run.py --only {prog} once")
    shim = os.path.join(ROOT, "bench", "heldout", "sightglass-api")
    return src, ["cargo", "build", "--release", "--target-dir", tdir,
                 "--config", f'patch.crates-io.sightglass-api.path="{shim}"']


def record(a):
    name = a.out or (f"{a.heldout}-{'pliron' if a.pliron else 'llvm'}" if a.heldout else time.strftime("%Y%m%d-%H%M%S"))
    out = name if os.path.isabs(name) else os.path.join(OUT, name)
    units = os.path.join(out, "units")
    shutil.rmtree(units, ignore_errors=True)
    os.makedirs(units)
    env = dict(os.environ, RUSTC_WRAPPER=os.path.abspath(__file__), RUSTUP_TOOLCHAIN=TOOLCHAIN)
    env[LOG_ENV] = units
    if a.pliron:
        ext = {"darwin": "dylib", "win32": "dll"}.get(sys.platform, "so")
        pre = "" if sys.platform == "win32" else "lib"
        dylib = os.path.join(ROOT, "target", "release", f"{pre}rustc_codegen_pliron.{ext}")
        if not os.path.exists(dylib):
            sys.exit(f"headstart: {dylib} missing (cargo build --release)")
        env["RUSTFLAGS"] = f"-Zcodegen-backend={dylib} {env.get('RUSTFLAGS', '')}".strip()
    cwd = None
    if a.heldout:
        tdir = os.path.join(out, "target")
        shutil.rmtree(tdir, ignore_errors=True)  # a clean build is the point
        cwd, cmd = heldout_cmd(a.heldout, tdir)
        cmd += a.cmd
    else:
        cmd = a.cmd
        if not cmd:
            sys.exit("headstart: record needs --heldout PROG or -- CMD...")
    if a.jobs:
        cmd = cmd + ["-j", str(a.jobs)]
    jobs = a.jobs
    if not jobs:  # `build.jobs` from the cargo configuration that applies
        q = subprocess.run(["cargo", "-Zunstable-options", "config", "get", "--format", "json-value", "build.jobs"],
                           cwd=cwd, env=env, capture_output=True, text=True)
        jobs = int(q.stdout) if q.returncode == 0 and q.stdout.strip().isdigit() else os.cpu_count()
    t0 = time.time()
    r = subprocess.run(cmd, cwd=cwd, env=env)
    t1 = time.time()
    meta = {"cmd": cmd, "start": t0, "end": t1, "status": r.returncode, "jobs": jobs,
            "pliron": bool(a.pliron), "loadavg": os.getloadavg()[0]}
    # Build-script runs are not rustc invocations: cargo's own files in the
    # run directory give their start and end.
    runs = {}
    for f in os.listdir(units):
        od = json.load(open(os.path.join(units, f))).get("OUT_DIR")
        d = os.path.dirname(od) if od else None
        # Old layout: <pkg-hash>/{invoked.timestamp,output}; new:
        # <hash>/run/invoked.timestamp, and <hash>/fingerprint/* written
        # when the run is done.
        for sub, done in (("", ["output"]), ("run", ["fingerprint", "run"])):
            try:
                if d and d not in runs:
                    s = os.stat(os.path.join(d, sub, "invoked.timestamp")).st_mtime
                    ends = []
                    for x in done:
                        x = os.path.join(d, x)
                        ends += [os.stat(os.path.join(x, f)).st_mtime for f in os.listdir(x)] if os.path.isdir(x) \
                            else [os.stat(x).st_mtime]
                    runs[d] = (s, max(ends))
            except OSError:
                pass
    meta["runs"] = runs
    with open(os.path.join(out, "meta.json"), "w") as f:
        json.dump(meta, f)
    if a.heldout and not a.keep:
        shutil.rmtree(os.path.join(out, "target"), ignore_errors=True)  # ~30 MB each
    print(f"headstart: {len(os.listdir(units))} rustc runs, {t1 - t0:.2f} s, exit {r.returncode} -> {out}")
    if r.returncode == 0 and not a.quiet:
        report_dir(out, meta["jobs"], a.early)
    return r.returncode


# -------------------------------------------------------------- model

class Unit:
    """One rustc run (or build-script run) as three stretches of work:
    `a` up to the interface checks, `b` on to full metadata, `c` code
    generation, `l` linking."""

    def __init__(self, key, name, start, a, b, c, l=0.0, linking=False, opaque=False, w=1):
        self.key, self.name, self.start = key, name, start
        self.w = w  # job slots its code generation keeps busy (rustc's threads take cargo's tokens)
        self.a, self.b, self.c, self.l = a, b, c, l
        self.linking = linking  # waits for every transitive dependency's object code
        self.opaque = opaque  # proc macro, dylib, build-script run: dependents need all of it
        self.deps = []  # direct
        self.end = start + a + b + c + l

    def trans(self):
        """Every crate this one loads: its dependencies, transitively, but
        not what an opaque one (a proc macro, a build-script run) was
        itself built from."""
        if not hasattr(self, "_t"):
            seen = {}
            for d in self.deps:
                seen[d.key] = d
                if not d.opaque:
                    for x in d.trans():
                        seen[x.key] = x
            self._t = list(seen.values())
        return self._t


def load(out):
    meta = json.load(open(os.path.join(out, "meta.json")))
    raw = [json.load(open(os.path.join(out, "units", f))) for f in sorted(os.listdir(os.path.join(out, "units")))]
    units, by_out, scripts = [], {}, {}
    for n, r in enumerate(raw):
        if r["status"] != 0:
            continue
        p = {n: (d, t) for n, d, t in r["passes"]}
        st, en = r["start"], r["end"]
        clamp = lambda t: min(max(t, st), en)
        # Full metadata; a binary writes none, so where its codegen starts.
        if "generate_crate_metadata" in p:
            tm = clamp(p["generate_crate_metadata"][1])
        elif "codegen_crate" in p:
            tm = clamp(p["codegen_crate"][1] - p["codegen_crate"][0])
        else:
            tm = en
        if "coherence_checking" in p:
            ti = p["coherence_checking"][1]
        elif "type_check_crate" in p:
            ti = p["type_check_crate"][1] - p["type_check_crate"][0]
        else:
            ti = tm
        ti = min(clamp(ti), tm)
        l = min(p["link"][0], en - tm) if "link" in p else 0.0
        linking = r["test"] or bool(LINKING & set(r["types"])) or not r["types"]
        opaque = bool({"proc-macro", "dylib"} & set(r["types"]))
        # Build scripts and binaries have no `extra-filename`: number them.
        key = r["name"] + (r["extra"] or f"#{n}")
        c = en - tm - l
        # Threads at work during code generation, from the process's CPU
        # time: the front end is one thread.
        w = max(1, math.ceil((r.get("cpu", 0) - (tm - st) - l) / c - 0.1)) if c > 0.05 else 1
        u = Unit(key, r["name"], st, ti - st, tm - ti, c, l, linking, opaque, w)
        u.raw = r
        units.append(u)
        if r["extra"]:
            by_out[key] = u
        if r["name"].startswith("build_script_"):
            scripts.setdefault(r["CARGO_MANIFEST_DIR"], []).append(u)
    runs = {}
    for d, (s, e) in meta.get("runs", {}).items():
        if e >= s:
            runs[d] = Unit("run:" + d, "run build script", s, 0.0, 0.0, e - s, opaque=True)
    for u in list(units):
        for path in u.raw["externs"]:
            stem = os.path.basename(path).rsplit(".", 1)[0]
            d = by_out.get(stem[3:] if stem.startswith("lib") else stem) or by_out.get(stem)
            if d and d is not u and d not in u.deps:
                u.deps.append(d)
        od = u.raw.get("OUT_DIR")
        run = runs.get(os.path.dirname(od)) if od else None
        if run:
            if not run.deps:
                cands = [s for s in scripts.get(u.raw["CARGO_MANIFEST_DIR"], []) if s.end <= run.start + 0.05]
                if cands:
                    run.deps.append(max(cands, key=lambda s: s.end))
                units.append(run)
            u.deps.append(run)
    return meta, units


def simulate(units, jobs, headstart, early=0.0):
    """Replay `units` on `jobs` slots. Returns (wall, peak running, {key: (start, end, blocker key)}).

    Today: a library starts on its dependencies' full metadata (cargo's
    pipelining), a linking crate once every transitive dependency is
    done. headstart: every crate starts once its dependencies' interfaces
    are checked (plus `early` of their body time, for bodies the early
    metadata needs); it pauses, without a job slot, until their full
    metadata exists before code generation, and until their object code
    exists before linking; resuming does not wait for a slot.
    """
    t0 = min(u.start for u in units)
    order = sorted(units, key=lambda u: u.start)
    ms = {}  # key -> dict(early, meta, end), set when the unit starts
    busy = []  # (from, to) slot intervals of started units
    res = {}
    pending = list(order)
    wake = [t0]
    peak = 0
    now = t0

    def gate(u):
        g, why = t0, None
        deps = u.trans() if (u.linking and not headstart) else u.deps
        for d in deps:
            m = ms.get(d.key)
            if m is None:
                return None, d
            if d.opaque or u.key.startswith("run:") or (not headstart and u.linking):
                t = m["end"]
            else:
                t = m["early"] if headstart else m["meta"]
            if t >= g:
                g, why = t, d
        return g, why

    while pending:
        now = heapq.heappop(wake)
        running = sum(w for s, e, w in busy if s <= now < e)
        rest = []
        for u in pending:
            g, why = gate(u)
            if g is None or g > now + 1e-9 or running >= jobs:
                rest.append(u)
                continue
            running += 1
            if not headstart:
                meta = now + u.a + u.b
                iv = [(now, meta, 1), (meta, meta + u.c, u.w), (meta + u.c, meta + u.c + u.l, 1)]
                m = {"early": now + u.a, "meta": meta, "end": meta + u.c + u.l}
            else:
                meta = now + u.a + u.b
                tr = u.trans()
                r = max([meta] + [ms[d.key]["end" if d.opaque else "meta"] for d in tr])
                cg = r + u.c
                iv = [(now, meta, 1), (r, cg, u.w)]
                end = cg
                if u.linking:
                    ls = max([cg] + [ms[d.key]["end"] for d in tr])
                    end = ls + u.l
                    iv.append((ls, end, 1))
                m = {"early": now + u.a + early * u.b, "meta": meta, "end": end}
            ms[u.key] = m
            res[u.key] = (now, m["end"], why.key if why else None)
            for s, e, w in iv:
                if e > s:
                    busy.append((s, e, w))
                    heapq.heappush(wake, e)
            for t in m.values():
                heapq.heappush(wake, t)
        pending = rest
        peak = max(peak, running)
        if pending and not wake:
            raise SystemExit("headstart: schedule stuck (dependency never ran): " + pending[0].key)
    wall = max(e for _, e, _ in res.values()) - t0
    # Peak including resumed jobs, which may exceed `jobs`.
    pts = sorted([(s, w) for s, e, w in busy] + [(e, -w) for s, e, w in busy], key=lambda x: (x[0], x[1]))
    n = 0
    for _, d in pts:
        n += d
        peak = max(peak, n)
    return wall, peak, res


def critical(units, res):
    by = {u.key: u for u in units}
    k = max(res, key=lambda k: res[k][1])
    path = []
    while k:
        path.append(by[k])
        k = res[k][2]
    return path[::-1]


def report_dir(out, jobs=None, early=0.2):
    meta, units = load(out)
    jobs = jobs or meta["jobs"]
    rust = [u for u in units if not u.key.startswith("run:")]
    t0 = min(u.start for u in units)
    obs = max(u.end for u in units) - t0
    work = sum(u.end - u.start for u in units)
    # Wall time with at most one compiler alive.
    pts = sorted([(u.start, 1) for u in units] + [(u.end, -1) for u in units])
    n, last, lone = 0, t0, 0.0
    for t, d in pts:
        if n <= 1:
            lone += t - last
        n, last = n + d, t
    print(f"\n== {out}")
    print(f"{len(rust)} rustc runs, {len(units) - len(rust)} build-script runs, -j {jobs}, "
          f"{'pliron' if meta.get('pliron') else 'default backend'}, load {meta.get('loadavg', 0):.0f}")
    fa, fb, fc = (sum(getattr(u, x) for u in rust) for x in "abc")
    fl = sum(u.l for u in rust)
    print(f"observed: {obs:.2f} s wall, {work:.2f} s of compiler time (mean {work / obs:.1f} alive, "
          f"{100 * lone / obs:.0f}% of the wall with at most one)")
    print(f"  compiler time: {100 * fa / work:.0f}% up to interfaces, {100 * fb / work:.0f}% bodies to metadata, "
          f"{100 * fc / work:.0f}% codegen, {100 * fl / work:.0f}% link")
    base, _, bres = simulate(units, jobs, False)
    print(f"model of today's cargo: {base:.2f} s ({100 * (base / obs - 1):+.0f}% vs observed; "
          f"trust the prediction no further than this)")
    print("  critical path (start, end, a=to interfaces, b=bodies, c=codegen+link):")
    for u in critical(units, bres):
        s, e, _ = bres[u.key]
        print(f"    {s - t0:7.2f} {e - t0:7.2f}  a {u.a:6.2f}  b {u.b:6.2f}  c {u.c + u.l:6.2f}  {u.name}")
    print("headstart, same durations replayed:")
    for label, j, f in ((f"-j {jobs}, early metadata free", jobs, 0.0),
                        (f"-j {jobs}, early metadata costs {early:.0%} of bodies", jobs, early),
                        ("unlimited jobs, free", 10 ** 6, 0.0)):
        ref = base if j == jobs else simulate(units, j, False)[0]
        w, peak, _ = simulate(units, j, True, f)
        print(f"  {label:44} {w:7.2f} s  {100 * (1 - w / ref):5.1f}% saved  (peak {peak} slots busy)")
    return base, obs


def report(a):
    dirs = [d if os.path.isdir(d) else os.path.join(OUT, d) for d in a.dirs]
    if not a.table:
        for d in dirs:
            report_dir(d, a.jobs, a.early)
        return
    print(f"| build | observed | model | alone | to metadata | codegen+link | headstart | with {a.early:.0%} early cost |")
    print("|---|--:|--:|--:|--:|--:|--:|--:|")
    for d in dirs:
        meta, units = load(d)
        jobs = a.jobs or meta["jobs"]
        t0 = min(u.start for u in units)
        obs = max(u.end for u in units) - t0
        pts = sorted([(u.start, 1) for u in units] + [(u.end, -1) for u in units])
        n, last, lone = 0, t0, 0.0
        for t, dn in pts:
            if n <= 1:
                lone += t - last
            n, last = n + dn, t
        rust = [u for u in units if not u.key.startswith("run:")]
        work = sum(u.end - u.start for u in rust)
        front = sum(u.a + u.b for u in rust)
        base = simulate(units, jobs, False)[0]
        h0 = simulate(units, jobs, True)[0]
        h1 = simulate(units, jobs, True, a.early)[0]
        print(f"| {os.path.basename(d.rstrip('/'))} | {obs:.2f} s | {100 * (base / obs - 1):+.0f}% | {100 * lone / obs:.0f}% "
              f"| {100 * front / work:.0f}% | {100 * (1 - front / work):.0f}% | -{100 * (1 - h0 / base):.0f}% | -{100 * (1 - h1 / base):.0f}% |")


# --------------------------------------------------------- port-check

def git(*args, cwd=None, env=None, check=True):
    r = subprocess.run(["git", *args], cwd=cwd, env=env, capture_output=True, text=True)
    if check and r.returncode != 0:
        sys.exit(f"headstart: git {' '.join(args)}: {r.stderr.strip()}")
    return r


def port_check(a):
    src = os.path.join(OUT, "src")
    if not os.path.isdir(os.path.join(src, ".git")):
        os.makedirs(OUT, exist_ok=True)
        git("clone", "-q", "--no-checkout", HS_URL, src)
    git("fetch", "-q", "origin", a.rev, cwd=src, check=False)
    git("checkout", "-q", "--detach", a.rev, cwd=src)
    lic = [f for f in os.listdir(src) if f.upper().startswith(("LICEN", "COPYING"))]
    print(f"headstart {a.rev[:12]}: licence file: {', '.join(lic) or 'none (all rights reserved; do not copy)'}")
    rust = a.rust or os.path.join(os.path.dirname(ROOT), "rust")
    if not os.path.isdir(os.path.join(rust, ".git")):
        sys.exit(f"headstart: no rust checkout at {rust} (--rust DIR)")
    vv = subprocess.run(["rustc", f"+{TOOLCHAIN}", "-vV"], capture_output=True, text=True).stdout
    pin = re.search(r"commit-hash: (\S+)", vv).group(1)
    idx = os.path.join(OUT, "rust-idx")
    if not os.path.isdir(idx):
        git("clone", "-q", "--shared", "--no-checkout", rust, idx)
    pdir = os.path.join(src, "patches", "rustc")
    patches = [os.path.join(pdir, p) for p in sorted(os.listdir(pdir)) if p.endswith(".patch")]
    ours = [os.path.join(ROOT, "selfhost", p) for p in ("bootstrap.patch", "rustc-session.patch")]
    worst = 0
    for label, base, series in ((f"headstart's own base {HS_BASE[:12]}", HS_BASE, patches),
                                (f"{TOOLCHAIN} {pin[:12]} + selfhost patches", pin, ours + patches)):
        if git("cat-file", "-e", base + "^{commit}", cwd=idx, check=False).returncode != 0:
            print(f"-- {label}: commit not in {rust}; fetch it first")
            continue
        print(f"-- {label}")
        git("read-tree", base, cwd=idx)
        bad = 0
        for p in series:
            r = git("apply", "--cached", "-3", p, cwd=idx, check=False)
            conf = sorted(set(re.findall(r"Applied patch to '([^']+)' with conflicts", r.stderr)))
            miss = sorted(set(re.findall(r"error: ([^:]+): does not exist in index", r.stderr)))
            if r.returncode == 0:
                print(f"   ok        {os.path.basename(p)}")
            else:
                bad += 1
                print(f"   CONFLICT  {os.path.basename(p)}: {', '.join(conf + miss) or r.stderr.strip()[:200]}")
        if base == pin:
            worst = bad
    print("portable to the pinned nightly as is" if worst == 0 else
          f"{worst} patches need a hand rebase onto the pinned nightly (see docs/headstart.md)")
    return 1 if worst else 0


# ----------------------------------------------------------- selftest

def selftest(_a):
    # a -> b -> c, each: 1 s to interfaces, 4 s bodies, 2 s codegen; c is a
    # binary that links for 1 s.
    A = Unit("a", "a", 0, 1, 4, 2)
    B = Unit("b", "b", 5, 1, 4, 2)
    C = Unit("c", "c", 10, 1, 4, 2, 1, linking=True)
    B.deps, C.deps = [A], [B]
    us = [A, B, C]
    today = simulate(us, 8, False)[0]
    # a 0..7 (meta 5), b 5..12, c starts when both are done: 12, +8.
    assert abs(today - 20) < 1e-6, today
    hs = simulate(us, 8, True)[0]
    # a meta 5; b starts 1, meta 6, codegen 6..8; c starts 2, meta 7,
    # codegen 7..9, link 9..10.
    assert abs(hs - 10) < 1e-6, hs
    hs1 = simulate(us, 8, True, 0.5)[0]
    # early = +3 per crate: b starts 3, c starts 6, meta 11, codegen ..13, link ..14.
    assert abs(hs1 - 14) < 1e-6, hs1
    one = simulate(us, 1, True)
    # One slot: starts are serialised behind whole slot intervals; a paused
    # job gives its slot up and resumes without one.
    assert one[0] <= 22 + 1e-6 and one[1] >= 1, one
    assert abs(simulate(us, 1, False)[0] - 22) < 1e-6
    print("headstart selftest: ok")
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="sub", required=True)
    r = sub.add_parser("record")
    r.add_argument("-o", "--out")
    r.add_argument("--heldout", metavar="PROG", help="clean release build of a bench/heldout program")
    r.add_argument("--pliron", action="store_true", help="build with target/release's backend dylib")
    r.add_argument("-j", "--jobs", type=int)
    r.add_argument("--early", type=float, default=0.2)
    r.add_argument("--quiet", action="store_true")
    r.add_argument("--keep", action="store_true", help="keep a --heldout build's target directory")
    r.add_argument("cmd", nargs=argparse.REMAINDER)
    p = sub.add_parser("report")
    p.add_argument("dirs", nargs="+")
    p.add_argument("--table", action="store_true", help="one markdown row per recording")
    p.add_argument("-j", "--jobs", type=int)
    p.add_argument("--early", type=float, default=0.2,
                   help="share of a crate's body time spent before its early metadata (default 0.2)")
    c = sub.add_parser("port-check")
    c.add_argument("--rev", default=HS_REV)
    c.add_argument("--rust", help="rust-lang/rust checkout (default ../rust)")
    sub.add_parser("selftest")
    a = ap.parse_args()
    if a.sub == "record" and a.cmd and a.cmd[0] == "--":
        a.cmd = a.cmd[1:]
    sys.exit({"record": record, "report": report, "port-check": port_check, "selftest": selftest}[a.sub](a) or 0)


if __name__ == "__main__":
    if os.environ.get(LOG_ENV) and len(sys.argv) > 1 and sys.argv[1] not in ("record", "report", "port-check", "selftest", "-h", "--help"):
        wrapper(sys.argv[1:])
    main()
