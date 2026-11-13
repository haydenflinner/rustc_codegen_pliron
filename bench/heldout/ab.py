#!/usr/bin/env -S python3 -I
"""Fast A/B of builds of one held-out program: time, instructions, cycles.

  bench/heldout/ab.py PROG SIDE [SIDE...] [--rounds N] [--prof]

A SIDE is one of
  llvm                 stock rustc
  llvm-pgo             stock rustc with its profile (`run.py --pgo` made it)
  llvm:FLAGS           stock rustc with extra RUSTFLAGS (`+` for spaces),
                       e.g. `llvm:-Cllvm-args=-inline-threshold=500`
  llvm-pgo:FLAGS       the same on top of `-Cprofile-use`
  pliron               this backend, defaults
  NAME:K=V,K=V         this backend with extra environment
  pgo                  this backend with the program's profile
                       (`PLIRON_PGO_USE`; `run.py --pgo` wrote it)
  pgo-NAME:K=V,K=V     the same with extra environment
  @FLAGS suffix        extra RUSTFLAGS on any pliron side: `pliron@-Ccodegen-units=1`

Builds are cached per side (a stamp of its environment and the backend
dylib), so a second call only rebuilds what changed; the 24 most
recently used side directories are kept. Every side is then
run `--rounds` times, interleaved, under `/usr/bin/time -l`, and the table
gives, per side: the timed region (median and min of `heldout-ns`), and
the whole process's retired instructions, cycles and instructions per
cycle (medians), each with its ratio to the first side.

Why the counters: the wall clock here moves +-2% between runs of one
binary, the instruction count by ~0.01%. std is the same prebuilt code on
every side, so an instruction delta is the crate code's. The two ratios
together say what kind of difference it is: fewer instructions (inlining,
folding, dead code) or the same instructions running faster (layout,
branch prediction, spills).

`--prof` profiles the first and the last side (`--prof-runs` runs each
under samply) and prints where the last side's extra cycles are: by
function inclusive of callees, for the functions both builds have (so a
phase compares as a whole whatever each compiler inlined into it), and by
function self. Samples are scaled to each side's measured cycle count.
"""
import argparse, hashlib, importlib.util, os, re, statistics, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))


def load(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, name + ".py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


run = load("run")
OUT = run.OUT


def side_env(prog, side, backend):
    """(directory name, environment, description) for one side."""
    base = dict(os.environ, RUSTUP_TOOLCHAIN=run.TOOLCHAIN, CARGO_TERM_COLOR="never")
    base.pop("RUSTFLAGS", None)
    spec, _, xrf = side.partition("@")
    name, _, arg = spec.partition(":")
    if name in ("llvm", "llvm-pgo"):
        flags = arg.replace("+", " ")
        if name == "llvm-pgo":
            data = os.path.join(OUT, "pgo", prog, "llvm.profdata")
            if not os.path.exists(data):
                sys.exit(f"ab: {data} missing; run bench/heldout/run.py --pgo --only {prog} first")
            flags = f"-Cprofile-use={data} {flags}"
        if not arg and not xrf:
            return name, None, name  # run.py's own build, as is
        env = dict(base, RUSTFLAGS=f"{flags} {xrf}".strip())
        return "ab-" + re.sub(r"[^A-Za-z0-9]+", "_", side)[:60], env, side
    env = dict(base, RUSTFLAGS=f"-Zcodegen-backend={backend} {xrf}".strip())
    if name == "pgo" or name.startswith("pgo-"):
        prof = os.path.join(OUT, "pgo", prog, "pliron.prof")
        if not os.path.exists(prof):
            sys.exit(f"ab: {prof} missing; run bench/heldout/run.py --pgo --only {prog} first")
        env["PLIRON_PGO_USE"] = prof
    for kv in arg.split(","):
        if kv:
            k, _, v = kv.partition("=")
            env[k] = v
    return "ab-" + re.sub(r"[^A-Za-z0-9]+", "_", side)[:60], env, side


def ensure(prog, binary, side, backend):
    d, env, _ = side_env(prog, side, backend)
    exe = os.path.join(OUT, d, prog, "release", binary)
    if env is None:
        if not os.path.exists(exe):
            sys.exit(f"ab: {exe} missing; run bench/heldout/run.py first")
        return exe
    be = os.path.getmtime(backend) if "codegen-backend" in env.get("RUSTFLAGS", "") else 0
    stamp = hashlib.sha1(repr((sorted((k, v) for k, v in env.items() if k.startswith(("PLIRON_", "RUSTFLAGS"))),
                               be)).encode()).hexdigest()
    sfile = os.path.join(OUT, d, prog + ".stamp")
    if os.path.exists(exe) and os.path.exists(sfile) and open(sfile).read() == stamp:
        return exe
    t0 = time.time()
    # Cargo tracks neither the backend dylib nor PLIRON_* variables: a
    # rebuild into the old directory would be a no-op.
    import shutil
    shutil.rmtree(os.path.join(OUT, d, prog), ignore_errors=True)
    # Memory-pressure kills show up as failed builds on this machine: retry.
    for attempt in range(3):
        got = run.build(prog, binary, d, env)
        if got:
            break
        time.sleep(10)
    if not got:
        sys.exit(f"ab: build of {side} failed")
    with open(sfile, "w") as f:
        f.write(stamp)
    print(f"built {side} in {time.time() - t0:.0f}s", flush=True)
    return got


def prune(keep, limit=24):
    """Drop the least recently used cached side directories beyond `limit`.

    Every side is a full build of the program (50-100 MB); a night of
    A/Bs filled the disk once."""
    import shutil
    dirs = [d for d in os.listdir(OUT) if d.startswith("ab-") and os.path.isdir(os.path.join(OUT, d))]
    dirs = sorted((d for d in dirs if d not in keep), key=lambda d: os.path.getmtime(os.path.join(OUT, d)))
    for d in dirs[:max(0, len(dirs) - limit)]:
        shutil.rmtree(os.path.join(OUT, d), ignore_errors=True)
        for f in os.listdir(OUT):
            if f.startswith(f"build-{d}-") and f.endswith(".log"):
                os.remove(os.path.join(OUT, f))


def once(prog, exe):
    """(timed ns, instructions, cycles) of one run."""
    p = subprocess.run(["/usr/bin/time", "-l", exe], cwd=run.src_dir(prog), capture_output=True, text=True)
    ns = re.search(r"^heldout-ns (\d+)$", p.stderr, re.M)
    ins = re.search(r"^\s*(\d+)\s+instructions retired", p.stderr, re.M)
    cyc = re.search(r"^\s*(\d+)\s+cycles elapsed", p.stderr, re.M)
    if p.returncode != 0 or not ns:
        sys.exit(f"ab: {exe} failed (rc {p.returncode})")
    return int(ns.group(1)), int(ins.group(1)) if ins else 0, int(cyc.group(1)) if cyc else 0


def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("prog")
    ap.add_argument("sides", nargs="+")
    ap.add_argument("--rounds", type=int, default=15)
    ap.add_argument("--prof", action="store_true")
    ap.add_argument("--prof-runs", type=int, default=400)
    ap.add_argument("--backend", default=os.path.join(run.ROOT, "target", "release", run.DYLIB))
    a = ap.parse_args()
    binary = next((b for n, b, _ in run.BENCHES if n == a.prog), None)
    if binary is None:
        sys.exit(f"ab: unknown program {a.prog}")
    prune({side_env(a.prog, s, a.backend)[0] for s in a.sides})
    exes = {s: ensure(a.prog, binary, s, a.backend) for s in a.sides}
    data = {s: [] for s in a.sides}
    for r in range(a.rounds):
        for s in (a.sides if r % 2 == 0 else a.sides[::-1]):
            data[s].append(once(a.prog, exes[s]))
    med = {s: [statistics.median(x[i] for x in data[s]) for i in range(3)] for s in a.sides}
    b = med[a.sides[0]]
    w = max(len(s) for s in a.sides)
    print(f"{a.prog}, {a.rounds} rounds; ratios to {a.sides[0]}")
    print(f"{'side':<{w}}  {'ms':>7} {'x':>6} {'min':>7}  {'instr M':>9} {'x':>6}  {'cycles M':>9} {'x':>6}  {'ipc':>5}")
    for s in a.sides:
        m = med[s]
        mn = min(x[0] for x in data[s])
        print(f"{s:<{w}}  {m[0] / 1e6:7.3f} {m[0] / b[0]:6.3f} {mn / 1e6:7.3f}  "
              f"{m[1] / 1e6:9.2f} {m[1] / b[1] if b[1] else 0:6.3f}  "
              f"{m[2] / 1e6:9.2f} {m[2] / b[2] if b[2] else 0:6.3f}  {m[1] / m[2] if m[2] else 0:5.2f}")
    if a.prof and len(a.sides) >= 2:
        census = load("census")
        res = []
        for s in (a.sides[0], a.sides[-1]):
            prof = census.profile(exes[s], run.src_dir(a.prog), a.prof_runs,
                                  os.path.join(OUT, f"ab-{hashlib.sha1(s.encode()).hexdigest()[:8]}-{a.prog}.json.gz"))
            res.append(stacks(census, prof))
        (aself, aincl, at), (bself, bincl, bt) = res
        # Samples are time: scale each side so its total is its median
        # cycle count, in units of 0.01% of the first side's cycles.
        ka = 10000.0 / at if at else 0
        kb = 10000.0 * (med[a.sides[-1]][2] / b[2] if b[2] else 1) / bt if bt else 0
        print(f"\n{a.sides[-1]} against {a.sides[0]}, in 0.01% of {a.sides[0]}'s cycles "
              f"({at} and {bt} samples)")
        print("inclusive, functions both builds have (a phase's whole cost, whatever was inlined into it):")
        rows = sorted(((bincl[f] * kb - aincl[f] * ka, f) for f in set(aincl) & set(bincl)), reverse=True)
        for d, f in rows[:14]:
            print(f"  {d:+7.0f}  {aincl[f] * ka:6.0f} -> {bincl[f] * kb:6.0f}  {f[:96]}")
        print("self:")
        rows = sorted(((bself.get(f, 0) * kb - aself.get(f, 0) * ka, f) for f in set(aself) | set(bself)), reverse=True)
        for d, f in rows[:10] + rows[-8:]:
            print(f"  {d:+7.0f}  {aself.get(f, 0) * ka:6.0f} -> {bself.get(f, 0) * kb:6.0f}  {f[:96]}")


def stacks(census, path, proc_substr="benchmark"):
    """(self samples, inclusive samples, total) per demangled function."""
    import bisect, collections, gzip, json
    p = json.load(gzip.open(path))
    sy = json.load(open(path.replace(".json.gz", ".json.syms.json")))
    st = sy["string_table"]
    tabs = {}
    for e in sy["data"]:
        rows = sorted((r["rva"], r["size"], r["symbol"]) for r in e["symbol_table"])
        tabs[e["debug_name"]] = ([r[0] for r in rows], rows)
    libs = p["libs"]
    selfc, incl, total = collections.Counter(), collections.Counter(), 0
    for t in p["threads"]:
        if proc_substr not in (t.get("processName") or t.get("name") or ""):
            continue
        ft, fu, rt, sk = t["frameTable"], t["funcTable"], t["resourceTable"], t["stackTable"]
        memo = {}

        def fname(f):
            if f in memo:
                return memo[f]
            res = fu["resource"][ft["func"][f]]
            lib = rt["lib"][res] if res is not None and res >= 0 else None
            n = "?"
            tb = tabs.get(libs[lib]["debugName"]) if lib is not None else None
            if tb:
                k = bisect.bisect_right(tb[0], ft["address"][f]) - 1
                if k >= 0 and ft["address"][f] < tb[1][k][0] + max(tb[1][k][1], 1):
                    n = census.demangle_key(st[tb[1][k][2]])
            memo[f] = n
            return n
        for s in t["samples"]["stack"]:
            if s is None:
                continue
            total += 1
            selfc[fname(sk["frame"][s])] += 1
            seen = set()
            q = s
            while q is not None:
                n = fname(sk["frame"][q])
                if n not in seen:
                    seen.add(n)
                    incl[n] += 1
                q = sk["prefix"][q]
    return selfc, incl, total

if __name__ == "__main__":
    main()
