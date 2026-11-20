#!/usr/bin/env -S python3 -I
"""Register-allocation fuzzing under every combination of the regalloc knobs.

  harness/rafuzz.py loop  [--jobs N] [--hours H] [--seeds N] [--clif-share K]
  harness/rafuzz.py quick [--clif-check BIN] [--if-changed]
  harness/rafuzz.py knobs                 the knob table, and knobs the source
                                          reads that the table lacks

Two legs (docs/regalloc.md):

  ra2   tools/rafuzz: regalloc2's random functions (its own generator, plus
        answers for the hooks Cranelift implements: block frequencies, cold
        blocks, remat vregs, unmerged block params; one seed in four with
        6..16 registers per class) through `ion`, the symbolic checker and
        its `edit_vregs` label check, and a fresh-vs-reused `Ctx`
        comparison. Pure regalloc2, ~500 seeds/s per process.
  clif  `clif-check codegen` over the recorded corpora with
        PLIRON_RA_CHECK=1 (the checker on regalloc2's output) and
        PLIRON_RELOADPRE_CHECK=1 (again on the output of Cranelift's
        post-allocation rewrites, reload PRE and CSE): real functions, the
        real aarch64 machine environment, and the Cranelift-side knobs.

The knobs are read once per process (OnceLock), so every run is one process
under one combination. `loop` cycles: the defaults, everything on, each knob
alone at each of its values, then random combinations forever; seeds and the
CLIF sample offsets advance across restarts (target/rafuzz/state.json).

Failures: one line in target/rafuzz/failures.tsv (time, leg, signature,
knobs, replay command) per new signature, the details under
target/rafuzz/fail/. A ra2 failure replays as
`env K=V.. tools/rafuzz/target/release/rafuzz --replay SEED --print`, a clif
one as `env K=V.. PLIRON_RA_CHECK=1 PLIRON_RELOADPRE_CHECK=1 clif-check vcode
FILE`. `loop` logs to target/rafuzz/loop.log; run it under nohup.

A new knob: add a row to KNOBS (name, default, values to try, the legs that
read it, the value `allon` uses). `knobs` lists PLIRON_RA* names the
regalloc2 and machinst sources read that are missing from the table.
"""
import argparse, concurrent.futures as cf, hashlib, json, os, random, re, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target", "rafuzz")
RAFUZZ_DIR = os.path.join(ROOT, "tools", "rafuzz")
RAFUZZ = os.path.join(RAFUZZ_DIR, "target", "release", "rafuzz")

# (name, default, values to try besides the default, legs, value under `allon`).
# Legs: "ra2" = read by regalloc2 (both legs run it), "clif" = read by
# Cranelift (the CLIF leg only).
KNOBS = [
    ("PLIRON_RA2_FREQ", "3", ["0", "1", "2", "4", "5"], "ra2", "3"),
    ("PLIRON_RA2_LOOPSPLIT", "16", ["0", "1", "40"], "ra2", "16"),
    ("PLIRON_RA2_LOOPDEPTH", "0", ["1"], "ra2", "1"),
    ("PLIRON_RA2_FREQSPLIT", "0", ["1"], "ra2", "1"),
    ("PLIRON_RA2_COLDSPLIT", "0", ["1", "2", "3"], "ra2", "1"),
    ("PLIRON_RA2_COLDSPLIT_RATIO", "16", ["1", "256"], "ra2", "16"),
    ("PLIRON_RA2_COLDSPLIT_COLDDIV", "64", ["1", "4096"], "ra2", "64"),
    ("PLIRON_RA2_COLDSPLIT_BIAS", "1.0", ["0.25", "4"], "ra2", "1.0"),
    ("PLIRON_RA2_NOPINMERGE", "1", ["0"], "ra2", "1"),
    ("PLIRON_RA2_SLOTPAIR", "1", ["0"], "ra2", "1"),
    ("PLIRON_RA2_POSTOPT", "15", ["0", "1", "2", "4", "8", "3", "12"], "ra2", "15"),
    ("PLIRON_RA2_REGION", "1", ["0", "2"], "ra2", "1"),
    ("PLIRON_RA2_SPILLHOIST", "1", ["0"], "ra2", "1"),
    ("PLIRON_RA2_REGION_TIE", "0", ["1"], "ra2", "1"),
    ("PLIRON_RA2_SRCVREG", "1", ["0"], "ra2", "1"),
    ("PLIRON_RA2_REHOME", "2", ["0", "1"], "ra2", "2"),
    ("PLIRON_RA2_POTTS", "1", ["0", "2"], "ra2,clif", "1"),
    ("PLIRON_RA2_POTTS_COPY", "1", ["0"], "ra2,clif", "1"),
    ("PLIRON_RA2_POTTS_PIECES", "1", ["0"], "ra2,clif", "1"),
    ("PLIRON_RA2_XBLOCK", "0", ["1"], "ra2", "1"),
    ("PLIRON_RA2_MERGEDEPTH", "0", ["1", "2", "3"], "ra2", "2"),
    # Cranelift (vendor/cranelift-codegen/src/machinst, isa/aarch64).
    ("PLIRON_RA2_REMATW", "7", ["0", "1", "2", "3", "4", "5", "6"], "clif", "7"),
    ("PLIRON_RA2_REMATW_MAXUSES", "4", ["2", "8", "16"], "clif", "4"),
    ("PLIRON_RA2_REMATW_MINUSSES", "8", ["4", "16", "32"], "clif", "8"),
    ("PLIRON_RA2_REMATW_CHEAP", "1", ["0"], "clif", "1"),
    ("PLIRON_RA2_USESPLIT", "1", ["0"], "ra2", "1"),
    ("PLIRON_RA2_USESPLIT_GAP", "128", ["8", "32", "64"], "ra2", "128"),
    ("PLIRON_RA2_USESPLIT_MULTIBLOCK", "0", ["1"], "ra2", "0"),
    # greedy default is all-spill; COLOR=1 single-block non-edge (both clean).
    # PROOFCOLOR=1 needs COLOR; mixes chromatic rematch + temp prefs.
    # mcts = UCT homes on greedy emit (SIMS/TOPM dials; light-rafuzz clean).
    ("PLIRON_RA_ALGO", "ion", ["greedy", "mcts"], "ra2", "ion"),
    ("PLIRON_RA_GREEDY_COLOR", "0", ["1"], "ra2", "0"),
    ("PLIRON_RA_PROOFCOLOR", "0", ["1"], "ra2", "0"),
    ("PLIRON_RA_MCTS_SIMS", "256", ["64", "128", "512"], "ra2", "256"),
    ("PLIRON_RA_MCTS_TOPM", "8", ["4", "16"], "ra2", "8"),
    ("PLIRON_X18", "0", ["1"], "clif", "0"),
    ("PLIRON_REMAT", "1", ["0"], "clif", "1"),
    ("PLIRON_REMAT_STORE", "1", ["0"], "clif", "1"),
    ("PLIRON_RELOADPRE", "1", ["0"], "clif", "1"),
    ("PLIRON_RELOADPRE_ROUNDS", "3", ["1", "8"], "clif", "3"),
    ("PLIRON_RELOADCSE", "1", ["0"], "clif", "1"),
    ("PLIRON_SPILLCSE", "1", ["0"], "clif", "1"),
    ("PLIRON_EDITPAIR", "1", ["0"], "clif", "1"),
    ("PLIRON_SHRINKWRAP", "1", ["0"], "clif", "1"),
    ("PLIRON_X17", "1", ["0"], "clif", "1"),
    ("PLIRON_X30", "0", ["1"], "clif", "1"),
]
# Names the sources read that are reports or debugging aids, not behaviour.
IGNORED = {"PLIRON_RA2_STATS", "PLIRON_RA2_VERBOSE", "PLIRON_RA2_EDITS", "PLIRON_RA2_ALLOC",
           "PLIRON_RA2_POSTOPT_STATS", "PLIRON_RA_CHECK", "PLIRON_RA_CHECK_LABELS", "PLIRON_RA_EXPLAIN",
           "PLIRON_RA_STATS", "PLIRON_RA2_",
           # REGION / spill tuners and probes (behaviour only via REGION=1)
           "PLIRON_RA2_COPYHINT", "PLIRON_RA2_MOVESTATS", "PLIRON_RA2_REGION_CAP",
           "PLIRON_RA2_REGION_K", "PLIRON_RA2_REGION_MAX", "PLIRON_RA2_REGION_MAXB",
           "PLIRON_RA2_REGION_RATIO", "PLIRON_RA2_SLOTLIVE", "PLIRON_RA2_SLOTLIVE_PROBES"}
SOURCES = ["vendor/regalloc2/src", "vendor/cranelift-codegen/src/machinst"]
# Seeds that once failed (ra2 leg), replayed by `quick` under every quick combination.
REGRESS = []
CLIF_CHECK_ENV = {"PLIRON_RA_CHECK": "1", "PLIRON_RELOADPRE_CHECK": "1"}
# (corpus, every) for the loop's CLIF leg; `quick` uses smoke every 4.
CLIF_SETS = [("std", 4), ("smoke", 2), ("clcg", 16), ("html", 8)]


def allon(leg="clif"):
    return {k: on for k, d, _, legs, on in KNOBS if on != d and (leg == "clif" or legs == "ra2")}


def combo_str(c):
    return " ".join(f"{k}={v}" for k, v in sorted(c.items())) or "(defaults)"


def singles(leg):
    for k, d, vals, legs, _ in KNOBS:
        if leg == "ra2" and legs != "ra2":
            continue
        for v in vals:
            yield {k: v}


def random_combo(rng, leg):
    c = {}
    for k, d, vals, legs, _ in KNOBS:
        if leg == "ra2" and legs != "ra2":
            continue
        if rng.random() < 0.4:
            c[k] = rng.choice(vals)
    return c


def scan_knobs():
    """PLIRON_RA* names read in SOURCES that KNOBS and IGNORED lack."""
    known = {k for k, *_ in KNOBS} | IGNORED
    found = set()
    for d in SOURCES:
        for dp, _, fs in os.walk(os.path.join(ROOT, d)):
            for f in fs:
                if f.endswith(".rs"):
                    found |= set(re.findall(r'"(PLIRON_RA2?_[A-Z0-9_]+)"', open(os.path.join(dp, f)).read()))
    return sorted(found - known)


def corpus_root(name):
    d = os.path.join(ROOT, "target/corpus", name)
    if os.path.isdir(d):
        return d
    common = subprocess.run(["git", "rev-parse", "--path-format=absolute", "--git-common-dir"], cwd=ROOT,
                            capture_output=True, text=True).stdout.strip()
    if common:
        d2 = os.path.join(os.path.dirname(common), "target/corpus", name)
        if os.path.isdir(d2):
            return d2
    return None


def build_rafuzz():
    r = subprocess.run(["cargo", "build", "--release", "--offline", "-q", "--manifest-path",
                        os.path.join(RAFUZZ_DIR, "Cargo.toml")], cwd=ROOT, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit("rafuzz: build of tools/rafuzz failed:\n" + r.stderr[-3000:])


def signature(kind, msg):
    """Failures that differ only in numbers (vregs, blocks, offsets) are one bug."""
    m = re.sub(r"0x[0-9a-f]+|\d+", "N", msg.split("\n")[0])[:160]
    return kind + ": " + m


class Log:
    def __init__(self, path=None):
        os.makedirs(OUT, exist_ok=True)
        self.f = open(path, "a") if path else None
        self.tsv = os.path.join(OUT, "failures.tsv")
        self.seen = set()
        if os.path.exists(self.tsv):
            for l in open(self.tsv):
                p = l.rstrip("\n").split("\t")
                if len(p) > 2:
                    self.seen.add(p[2])

    def say(self, s):
        line = time.strftime("%H:%M:%S ") + s
        print(line, flush=True)
        if self.f:
            self.f.write(line + "\n")
            self.f.flush()

    def failure(self, leg, kind, msg, combo, replay, detail):
        sig = signature(kind, msg)
        new = sig not in self.seen
        self.say(f"FAIL {leg} {'NEW ' if new else ''}{sig}\n    knobs: {combo_str(combo)}\n    replay: {replay}")
        if new:
            self.seen.add(sig)
            h = hashlib.sha1(sig.encode()).hexdigest()[:10]
            d = os.path.join(OUT, "fail", h)
            os.makedirs(d, exist_ok=True)
            open(os.path.join(d, "README.txt"), "w").write(
                f"{sig}\nleg: {leg}\nknobs: {combo_str(combo)}\nreplay: {replay}\n\n{detail}\n")
            with open(self.tsv, "a") as f:
                f.write("\t".join([time.strftime("%Y-%m-%d %H:%M"), leg, sig, combo_str(combo), replay, d]) + "\n")
        return new


def snapshot(path, tag):
    """A private copy of a binary for a long loop, so rebuilding the tree
    meanwhile changes neither the loop nor its replay commands."""
    import shutil
    d = os.path.join(OUT, "bin")
    os.makedirs(d, exist_ok=True)
    h = hashlib.sha1(open(path, "rb").read()).hexdigest()[:10]
    dst = os.path.join(d, f"{tag}-{h}")
    if not os.path.exists(dst):
        shutil.copy2(path, dst + ".tmp")
        os.replace(dst + ".tmp", dst)
    return dst


def run_ra2(combo, start, count, log):
    """One rafuzz process. Returns (ok, summary line)."""
    env = dict(os.environ, **combo)
    faildir = os.path.join(OUT, "ra2-out")
    r = subprocess.run([RAFUZZ, "--start", str(start), "--count", str(count), "--out", faildir],
                       env=env, capture_output=True, text=True)
    summ = next((l for l in r.stdout.splitlines() if l.startswith("rafuzz:")), r.stderr[-300:])
    ok = r.returncode == 0
    for l in r.stdout.splitlines():
        m = re.match(r"FAIL seed=(\d+) mode=(\S+) regs=(\d+) kind=(\S+): (.*)", l)
        if not m:
            continue
        seed, kind, msg = m.group(1), m.group(4), m.group(5)
        replay = f"env {combo_str(combo) if combo else ''} {RAFUZZ} --replay {seed} --print".replace("  ", " ")
        detail_p = os.path.join(faildir, f"seed-{seed}.txt")
        detail = open(detail_p).read()[:200000] if os.path.exists(detail_p) else ""
        log.failure("ra2", kind, msg, combo, replay, detail)
    if not ok and "FAIL" not in r.stdout:
        log.failure("ra2", "crash", r.stderr[-500:], combo, f"env {combo_str(combo)} {RAFUZZ} --start {start} --count {count}",
                    r.stderr[-20000:])
    return ok, summ


def run_clif(clif_check, combo, corpus, every, offset, log):
    d = corpus_root(corpus)
    if not d:
        return True, f"no corpus {corpus}"
    out = os.path.join(OUT, f"clif-{os.getpid()}-{corpus}-{offset}-{random.random():.6f}.json")
    env = dict(os.environ, **CLIF_CHECK_ENV, **combo)
    jobs = os.environ.get("RAFUZZ_CLIF_JOBS", "4")
    cmd = [clif_check, "codegen", d, out, "--every", str(every), "--offset", str(offset), "--jobs", jobs]
    t = time.time()
    r = subprocess.run(cmd, env=env, capture_output=True, text=True)
    try:
        rep = json.load(open(out))
        os.remove(out)
    except (OSError, ValueError):
        log.failure("clif", "crash", r.stderr[-300:] or "no report", combo, " ".join(cmd), r.stderr[-20000:])
        return False, "crash"
    bad = 0
    for f in rep["functions"]:
        st = f["status"]
        # `compile`: Cranelift refuses the signature (many return values) on
        # this ISA; not an allocation problem.
        if st in ("ok", "compile"):
            continue
        bad += 1
        path = os.path.join(d, f["file"])
        knobs = " ".join(f"{k}={v}" for k, v in sorted({**CLIF_CHECK_ENV, **combo}.items()))
        log.failure("clif", st, f.get("msg", ""), combo, f"env {knobs} {clif_check} vcode {path}",
                    f"{path}\n\n{f.get('msg', '')}")
    n = len(rep["functions"])
    return bad == 0, f"clif {corpus} every {every}+{offset}: {n} fns, {bad} failed, {time.time() - t:.0f}s"


def default_clif_check():
    prof = os.environ.get("PLIRON_PROFILE", "release")
    return os.path.join(ROOT, "target", prof, "clif-check")


def state_load():
    try:
        return json.load(open(os.path.join(OUT, "state.json")))
    except (OSError, ValueError):
        return {}


def state_save(st):
    p = os.path.join(OUT, "state.json")
    json.dump(st, open(p + ".tmp", "w"))
    os.replace(p + ".tmp", p)


def changed_since_main():
    def git(*a):
        return subprocess.run(["git", *a], cwd=ROOT, capture_output=True, text=True).stdout
    base = git("merge-base", "HEAD", "main").strip() or "HEAD"
    files = set(git("diff", "--name-only", base).split()) | set(git("ls-files", "--others", "--exclude-standard").split())
    pre = ("vendor/regalloc2/", "vendor/cranelift-codegen/src/machinst/", "vendor/cranelift-codegen/src/isa/aarch64/abi",
           "tools/rafuzz/", "harness/rafuzz.py")
    return sorted(f for f in files if f.startswith(pre))


def cmd_quick(a):
    """Seconds: fixed seeds under four combinations, plus the smoke corpus
    under the defaults and everything on."""
    if a.if_changed:
        ch = changed_since_main()
        if not ch:
            print("rafuzz quick: no regalloc2/machinst change since main: skipped")
            return 0
    build_rafuzz()
    log = Log()
    rng = random.Random(7)
    combos = [{}, allon("ra2"), random_combo(rng, "ra2"), random_combo(rng, "ra2")]
    t = time.time()
    tasks = []
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        for c in combos:
            tasks.append((f"ra2 {combo_str(c)}", ex.submit(run_ra2, c, 1_000_000, a.seeds, log)))
            for s in REGRESS:
                tasks.append((f"ra2 regress {s}", ex.submit(run_ra2, c, s, 1, log)))
        cc = a.clif_check or default_clif_check()
        if os.path.exists(cc):
            for c in ({}, allon("clif")):
                tasks.append((f"clif {combo_str(c)}", ex.submit(run_clif, cc, c, "smoke", 4, 0, log)))
        else:
            print(f"rafuzz quick: no {cc}: CLIF leg skipped")
        ok = True
        for name, fut in tasks:
            good, summ = fut.result()
            ok &= good
            if not good or not name.startswith("ra2 regress"):
                print(f"  {'ok  ' if good else 'FAIL'} {summ}")
    print(f"rafuzz quick: {'passed' if ok else 'FAILED'} in {time.time() - t:.1f}s")
    return 0 if ok else 1


def cmd_loop(a):
    global RAFUZZ
    build_rafuzz()
    RAFUZZ = snapshot(RAFUZZ, "rafuzz")
    log = Log(os.path.join(OUT, "loop.log"))
    miss = scan_knobs()
    if miss:
        log.say(f"knobs read by the sources but missing from KNOBS: {', '.join(miss)}")
    cc = a.clif_check or default_clif_check()
    if os.path.exists(cc):
        cc = snapshot(cc, "clif-check")
    st = state_load()
    st.setdefault("next_seed", 0)
    st.setdefault("round", 0)
    st.setdefault("clif_offset", 0)
    rng = random.Random(st["round"] * 7919 + st["next_seed"])

    def ra2_combos():
        yield {}
        yield allon("ra2")
        yield from singles("ra2")
        while True:
            yield random_combo(rng, "ra2")

    def clif_combos():
        yield {}
        yield allon("clif")
        yield from singles("clif")
        while True:
            yield random_combo(rng, "clif")

    rc, kc = ra2_combos(), clif_combos()
    deadline = time.time() + a.hours * 3600 if a.hours else None
    log.say(f"loop: jobs {a.jobs}, {a.seeds} seeds per ra2 run, one clif run in {a.clif_share}, "
            f"seed {st['next_seed']}, clif-check {cc if os.path.exists(cc) else '(missing: ra2 only)'}")
    stats = {"ra2_runs": 0, "clif_runs": 0, "fail_runs": 0}
    k = 0
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        pending = set()
        while True:
            while len(pending) < a.jobs and not (deadline and time.time() > deadline):
                k += 1
                if os.path.exists(cc) and k % a.clif_share == 0:
                    corpus, every = CLIF_SETS[(k // a.clif_share) % len(CLIF_SETS)]
                    off = st["clif_offset"] % every
                    st["clif_offset"] += 1
                    c = next(kc)
                    fut = ex.submit(run_clif, cc, c, corpus, every, off, log)
                    fut.desc = f"clif {combo_str(c)}"
                    stats["clif_runs"] += 1
                else:
                    c = next(rc)
                    s = st["next_seed"]
                    st["next_seed"] += a.seeds
                    fut = ex.submit(run_ra2, c, s, a.seeds, log)
                    fut.desc = f"ra2 {combo_str(c)}"
                    stats["ra2_runs"] += 1
                state_save(st)
                pending.add(fut)
            if not pending:
                break
            done, pending = cf.wait(pending, return_when=cf.FIRST_COMPLETED)
            for f in done:
                ok, summ = f.result()
                stats["fail_runs"] += not ok
                log.say(f"{'ok  ' if ok else 'FAIL'} {f.desc[:200]}\n    {summ}")
    log.say(f"loop: done {stats}")
    return 0


def cmd_knobs(a):
    for k, d, vals, legs, on in KNOBS:
        print(f"  {k:<32} default {d:<5} tries {','.join(vals):<20} leg {legs:<5} allon {on}")
    miss = scan_knobs()
    print(f"missing from KNOBS: {', '.join(miss) if miss else 'none'}")
    return 1 if miss else 0


def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("cmd", choices=["loop", "quick", "knobs"])
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--hours", type=float, default=0)
    ap.add_argument("--seeds", type=int, default=0, help="seeds per ra2 run (loop 20000, quick 1500)")
    ap.add_argument("--clif-share", type=int, default=4, help="loop: every Kth run is a CLIF run")
    ap.add_argument("--clif-check", default="")
    ap.add_argument("--if-changed", action="store_true",
                    help="quick: skip unless vendor/regalloc2, machinst or the fuzzer changed since main")
    a = ap.parse_args()
    if not a.seeds:
        a.seeds = 20000 if a.cmd == "loop" else 1500
    return {"loop": cmd_loop, "quick": cmd_quick, "knobs": cmd_knobs}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
