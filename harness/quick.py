#!/usr/bin/env -S python3 -I
# r[verify verify.quick.licence]
# r[verify verify.quick.tiers]
"""Fast inner loop: validate a change in seconds to ~3 minutes, chosen from `git diff`.

Usage: harness/quick.py [--tier 0|1] [--base REF] [--jobs N] [--seeds N]
                        [--files a,b] [--dry] [--no-build] [--keep-going]

  tier 0 (<30 s warm)  build clif-check (+ the dylib when rustc glue changed),
                       unit tests of only the touched crates (for a CLIF pass:
                       its lib tests + the integration tests named after it),
                       and, when a CLIF pass, pliron-cfg or vendor/ changed,
                       corpus replay (`clif-check report --check`: whole CLIF
                       pipeline + interpreter diff over every recorded function).
  tier 1 (<3 min)      tier 0 + the test.sh stanzas the changed paths select
                       (+ their SM_POISON twins), wasm_smoke, the fuzz regression
                       seeds, and N fresh fuzz seeds from a rolling window
                       (stock results are cached per seed: target/harness/fuzz-stock).

Every run ends by printing what it SKIPPED and the exact slow command that
completes the proof (harness/gate.py, crates tier, stage-2 ...). A green quick run
is ~90% confidence, never a merge licence: run the gate before merging to main.

What it cannot catch (see docs/history/2026-10-29-fast-loop.md): bugs that need a
specific program shape not in the corpora or the selected stanzas — the seed-13475
slot-merge, NARROWEXT seed 12309 and similar are found by *fresh* fuzz seeds or
crates, not by replay; quick's fresh-seed window gives them a chance, nothing more.

Files-to-stanza selection is derived from names (pass stem == stanza name, tests/<dir>,
PLIRON_<STEM> mentions in test.sh) plus the ALIAS table below. Unknown paths fall
back to CORE stanzas.
"""
import argparse, concurrent.futures as cf, importlib.util, json, os, re, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target/quick")
WIN = os.name == "nt"

# Always-on cheap smoke stanzas when a path matches nothing more specific.
CORE = ["nostd", "std", "mirshapes", "unroll", "licm", "sroa"]
# path stem -> extra stanzas / unit-test prefixes beyond the name-derived ones.
ALIAS = {
    "slotmerge": ["slotmerge", "stackmove", "resumehoist", "nocapfields", "spills", "std"],
    "slotprom": ["slotpromlt", "spills"],
    "loadfwd": ["loadpre", "std"],
    "dse": ["dseunwind", "std"],
    "indvars": ["loopvec", "unroll", "narrowlsr"],
    "looprot": ["loopvec", "unroll"],
    "loopvec": ["loopvec"],
    "slp": ["slpsplat", "xpack", "pairfuse"],
    "jumpthread": ["std", "mirshapes", "unwind"],
    "edgespec": ["edgespec", "edgemerge"],
    "tailmerge": ["edgemerge", "std"],
    "taildup": ["edgemerge", "std"],
    "switchmap": ["polsw", "mirshapes"],
    "cmp3": ["cmp3"],
    "niche": ["mirshapes", "std"],
    "ifconv": ["selmul", "std"],
    "bunroll": ["unroll", "punroll"],
    "unroll": ["unroll", "punroll"],
    "paramsink": ["cparm", "coldargs"],
    "constdedup": ["std"],
    "pipeline": CORE + ["slotmerge", "loopvec"],
    "lib": CORE,
    "inline": ["inlinecm", "inlinerec", "std", "sroa"],
    "sroa": ["sroa"],
    "memcpyopt": ["sroa", "std"],
    "xcgu": ["xcgurot", "xcgufnptr", "std"],
    "wasm": ["wasm_smoke"],
    "lto": ["lto"],
    "pgo": ["pgo"],
    "lower": CORE + ["slotmerge"],
    "builder": CORE + ["mirshapes", "unwind"],
    "intrinsic": CORE + ["cmp3", "cvariadic"],
    "simd": ["loopvec", "slpsplat", "xpack", "std"],
    "abi": ["nostd", "std", "cvariadic", "rusttailcc", "applestackargs", "retattr"],
    "debuginfo": ["debuginfo", "dbgnested"],
    "dbg": ["debuginfo", "dbgnested"],
}
# Fuzz seeds by historical bug yield (newest / subtlest first).
FUZZ_YIELD = [13475, 12309, 1939, 3441, 8631, 2598, 4940, 4895, 5042, 5542, 3938, 2542, 2187, 1776, 1276, 810, 730, 683, 538]
# Slow commands that finish the proof, by area touched.
SLOW = {
    "gate": "harness/gate.py",
    "wasm": "harness/gate.py --full   # adds the -Zbuild-std wasm sysroot legs (~4 min)",
    "fuzz": "harness/run.py --suite fuzz --no-build --fuzz-seeds A:B   # 400 seeds; stock refs cached after the first pass",
    "crates": "harness/crates.py --tier 1 --deps pliron   # real crate suites, deps through pliron too (~10 min); fast form: quick.py --crates NAME",
    "stage2": "selfhost/verify.sh   # stage-2 rustc UI suite (66-96 min); only for flag flips / big lowering changes",
    "parity": "harness/parity.py   # perf card (~12 min); only when moving a PARITY number",
}


def load_gate():
    spec = importlib.util.spec_from_file_location("gate", os.path.join(ROOT, "harness/gate.py"))
    g = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(g)
    return g


def git(*a):
    r = subprocess.run(["git", *a], cwd=ROOT, capture_output=True, text=True)
    return r.stdout.strip() if r.returncode == 0 else ""


def changed_files(base):
    mb = git("merge-base", "HEAD", base) if base != "HEAD" and git("rev-parse", "--verify", "-q", base) else ""
    files = set()
    if mb:
        files |= set(git("diff", "--name-only", mb, "HEAD").splitlines())
    files |= set(git("diff", "--name-only", "HEAD").splitlines())
    files |= set(git("ls-files", "--others", "--exclude-standard").splitlines())
    if not files and base == "HEAD":
        files = set(git("diff", "--name-only", "HEAD~1", "HEAD").splitlines())
    return sorted(f for f in files if f)


def corpus_root(name):
    """This worktree's corpus, else the main checkout's (read-only; it is the same
    pre-pipeline CLIF, so replaying it against this worktree's passes is valid)."""
    d = os.path.join(ROOT, "target/corpus", name)
    if os.path.isdir(d):
        return d, False
    common = git("rev-parse", "--path-format=absolute", "--git-common-dir")
    if common:
        d2 = os.path.join(os.path.dirname(common), "target/corpus", name)
        if os.path.isdir(d2):
            return d2, True
    return None, False


class Plan:
    def __init__(self, g, files, tier):
        self.g, self.files, self.tier = g, files, tier
        self.stanza_all = set(g.stanzas())
        self.crates = set()       # cargo packages touched
        self.stanzas = []         # ordered, deduped
        self.units = {}           # crate -> list of integration test names, or None = whole crate
        self.glue = False         # rustc-glue / vendor changed: dylib must rebuild
        self.wasm = False
        self.flags = set()        # areas for the slow-command list
        self.unknown = []
        self.test_sh = False
        self.clif_opt = False     # CLIF passes / cranelift changed: corpus replay is informative
        self._derive()

    def add_stanzas(self, names):
        for n in names:
            if n in self.stanza_all and n not in self.stanzas:
                self.stanzas.append(n)

    def _stem_stanzas(self, stem):
        s = stem.lower()
        hits = [n for n in self.stanza_all if n == s or n.replace("_", "") == s.replace("_", "")]
        if s in ALIAS:  # a curated entry replaces the body-grep heuristic (it over-selects, e.g. wasm -> pgo)
            return hits + ALIAS[s]
        src = self.g.ROOT and open(os.path.join(ROOT, "test.sh")).read()
        for n, body in re.findall(r"^t_(\w+)\(\) \{\n(.*?)^\}", src, re.M | re.S):
            if re.search(rf"PLIRON_{re.escape(s.upper())}\b", body) or f"tests/{s}/" in body:
                hits.append(n)
        return hits + ALIAS.get(s, [])

    def _derive(self):
        for f in self.files:
            parts = f.split("/")
            stem = os.path.splitext(parts[-1])[0]
            if f.startswith("docs/") or f.endswith(".md"):
                continue
            if f == "test.sh":
                self.test_sh = True
                continue
            if f.startswith("tests/") and len(parts) > 2:
                d = parts[1]
                if d == "wasm":
                    self.wasm = True
                    self.add_stanzas(["wasm_smoke"])
                self.add_stanzas([n for n in self.stanza_all if n.replace("_", "") == d.replace("_", "")])
                continue
            if f.startswith("harness/") or f.startswith("bench/") or f.startswith("selfhost/"):
                self.flags.add("harness")
                continue
            top = parts[0]
            if top in ("pliron-clif-opt", "pliron-ir-opt", "pliron-clif-lower", "pliron-cfg", "pliron-clif-check"):
                self.crates.add(top)
                self.flags.add("gate")
                self.clif_opt |= top in ("pliron-clif-opt", "pliron-cfg")
                if len(parts) > 2 and parts[1] == "tests":
                    self.units.setdefault(top, [])
                    if self.units[top] is not None:
                        self.units[top].append(stem)
                    continue
                if parts[1] == "src":
                    self.units.setdefault(top, [])
                    if self.units[top] is not None:
                        self.units[top].append(stem)
                    st = self._stem_stanzas(stem)
                    if not st:
                        self.unknown.append(f)
                    self.add_stanzas(st)
                else:
                    self.units[top] = None
                if top in ("pliron-ir-opt", "pliron-cfg", "pliron-clif-lower"):
                    self.glue = True
                continue
            if top == "src" or top == "build.rs" or top == "Cargo.toml" or top == "Cargo.lock":
                self.glue = True
                self.flags.update(["gate", "crates"])
                st = self._stem_stanzas(stem)
                if stem in ("wasm", "wpeep"):
                    self.wasm = True
                    self.flags.add("wasm")
                self.add_stanzas(st or CORE)
                if not st:
                    self.unknown.append(f)
                continue
            if top == "vendor":
                self.glue = True
                self.clif_opt = True
                self.flags.update(["gate", "crates", "stage2"])
                self.add_stanzas(CORE + ["slotmerge"])
                continue
        if "pliron-clif-opt" in self.crates or "pliron-ir-opt" in self.crates:
            self.flags.add("fuzz")
        if self.glue:
            self.flags.add("fuzz")
        if self.test_sh and not self.stanzas:
            self.add_stanzas(CORE)
        # A change that nothing mapped to still gets the core smoke stanzas.
        if (self.crates or self.glue) and not self.stanzas:
            self.add_stanzas(CORE)
        self.add_stanzas(["wasm_smoke"] if (self.crates or self.glue) else [])


class Runner:
    def __init__(self, a, g, plan):
        self.a, self.g, self.plan = a, g, plan
        os.makedirs(OUT, exist_ok=True)
        self.rows = []
        self.cc = os.path.join(ROOT, "target/debug", "clif-check.exe" if WIN else "clif-check")
        self.env = dict(os.environ, RUSTC_ICE=os.path.join(ROOT, "target/ices"))

    def sh(self, name, cmd, env=None, timeout=900, quiet=False):
        t = time.time()
        log = os.path.join(OUT, name.replace(":", "_") + ".log")
        with open(log, "w") as f:
            try:
                rc = subprocess.run(cmd, cwd=ROOT, stdout=f, stderr=subprocess.STDOUT, timeout=timeout,
                                    env=dict(self.env, **(env or {}))).returncode
            except subprocess.TimeoutExpired:
                rc = "timeout"
        r = {"job": name, "ok": rc == 0, "secs": round(time.time() - t, 1), "log": log}
        if not quiet:
            self.report(r)
        return r

    def report(self, r):
        self.rows.append(r)
        print(f"  {'ok  ' if r['ok'] else 'FAIL'} {r['job']:<22} {r['secs']:>6}s" + ("" if r["ok"] else f"  {r['log']}"),
              flush=True)
        for nf in r.get("new_failures", [])[:5]:
            print(f"       {nf}")

    def corpus(self, name):
        d, borrowed = corpus_root(name)
        job = f"corpus:{name}" + ("*" if borrowed else "")
        if not d:
            self.report({"job": job, "ok": True, "secs": 0.0, "log": "", "skipped": "no corpus (harness/corpus.sh)"})
            return
        out = os.path.join(OUT, f"corpus_{name}.json")
        r = self.sh(job, [self.cc, "report", d, out, "--check"], quiet=True)
        t = r["secs"]
        known = set()
        kp = os.path.join(ROOT, "harness/expectations/corpus-known.txt")
        if os.path.exists(kp):
            known = {l.split("#")[0].strip() for l in open(kp)} - {""}
        try:
            rep = json.load(open(out))
            bad = [f for f in rep["functions"] if f["status"] in ("mismatch", "verifier", "panic")]
            new = [f for f in bad if f"{name}/{f['file']}" not in known]
            r.update(ok=not new, counts=rep["counts"], new_failures=[f"{f['status']}: {f['file']}" for f in new][:50])
        except (OSError, ValueError):
            r["ok"] = False
        r["secs"] = t
        self.report(r)

    def unit(self, crate):
        sel = self.plan.units.get(crate)
        base = ["cargo", "test", "-q", "-p", crate]
        if crate == "pliron-clif-check":
            return
        if sel is None:
            self.sh(f"unit:{crate}", base)
            return
        # Lib tests + the integration tests named after the touched passes
        # (slotmerge.rs -> tests/slotmerge_*.rs); filetests/diffcheck always.
        tdir = os.path.join(ROOT, crate, "tests")
        names = []
        if os.path.isdir(tdir):
            have = sorted(os.path.splitext(x)[0] for x in os.listdir(tdir) if x.endswith(".rs"))
            for stem in set(sel):
                names += [h for h in have if h == stem or h.startswith(stem + "_") or stem.startswith(h)]
        cmd = base + ["--lib"] + [x for n in sorted(set(names)) for x in ("--test", n)]
        self.sh(f"unit:{crate}", cmd)

    def fuzz(self, nfresh):
        g = self.g
        sys.path.insert(0, os.path.join(ROOT, "harness"))
        gen = os.path.join(ROOT, "..", "rustlantis")
        sp = os.path.join(OUT, "state.json")
        try:
            st = json.load(open(sp))
        except (OSError, ValueError):
            st = {}
        nxt = st.get("fuzz_next", 20000)
        # Highest-yield seeds first (recent, subtle bugs: slotmerge, NARROWEXT, jumpthread
        # rounds), then the rest of the regress bank, then fresh seeds; --fail-fast stops
        # at the first failure so a real bug costs seconds, not the whole bank.
        first = [s for s in FUZZ_YIELD if s in g.FUZZ_REGRESS]
        seeds = first + [s for s in g.FUZZ_REGRESS if s not in first] + list(range(nxt, nxt + nfresh))
        cmd = [sys.executable, "-I", "harness/run.py", "--suite", "fuzz", "--no-build", "--fail-fast",
               "--fuzz-seeds", ",".join(map(str, seeds))]
        r = self.sh("fuzz", cmd, env={"PLIRON_FUZZ_JOBS": str(self.a.jobs)})
        if r["ok"]:
            st["fuzz_next"] = nxt + nfresh
            json.dump(st, open(sp, "w"))
        return seeds


# r[impl verify.quick.tiers]
# r[impl verify.quick.licence]
def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("--tier", type=int, default=1, choices=(0, 1))
    ap.add_argument("--base", default="HEAD",
                    help="diff against this ref (default HEAD = uncommitted work; a clean tree uses HEAD~1; "
                         "--base main = the whole branch)")
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--seeds", type=int, default=24, help="fresh fuzz seeds (tier 1)")
    ap.add_argument("--files", default="", help="comma list instead of git diff")
    ap.add_argument("--dry", action="store_true", help="print the plan only")
    ap.add_argument("--crates", default="", help="comma list of crates.py crates: run just those (stock results cached, "
                    "stock-built deps) instead of the diff-driven plan")
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--keep-going", action="store_true")
    a = ap.parse_args()
    g = load_gate()
    if a.crates:
        if not a.no_build:
            subprocess.run(["cargo", "build", "-p", "rustc_codegen_pliron"], cwd=ROOT, check=True)
        t = time.time()
        rc = subprocess.run([sys.executable, "-I", "harness/crates.py", "--tier", "3", "--only", a.crates,
                             "--profile", "debug", "--deps", "stock"], cwd=ROOT).returncode
        print(f"quick: crates {a.crates} {'ok' if rc == 0 else 'FAILED'} in {time.time() - t:.0f}s "
              f"(only the crate's own code went through pliron; completes with: harness/crates.py --tier 3 "
              f"--only {a.crates} --deps pliron --profile release)")
        return rc
    import shutil
    free = shutil.disk_usage(ROOT).free / 2**30
    if free < 15:
        print(f"quick: only {free:.1f} GB free (< 15): run harness/disk.sh --sweep first")
        return 2
    files = [f for f in a.files.split(",") if f] or changed_files(a.base)
    plan = Plan(g, files, a.tier)
    load = os.getloadavg()[0]
    a.jobs = max(2, min(a.jobs, int((os.cpu_count() or 4) - load)))
    print(f"quick: tier {a.tier}, {len(files)} changed file(s), load {load:.1f} -> jobs {a.jobs}")
    for f in files[:12]:
        print(f"  changed {f}")
    print(f"  crates   : {sorted(plan.crates) or '-'}   glue/dylib: {plan.glue}")
    print(f"  stanzas  : {','.join(plan.stanzas) or '-'}")
    if plan.unknown:
        print(f"  unmapped (CORE fallback): {', '.join(plan.unknown)}")
    if not (plan.crates or plan.glue or plan.stanzas or plan.test_sh):
        print("quick: only docs/harness changes; nothing to build. (harness changes: run the script you changed.)")
        return 0
    if a.dry:
        return 0
    t0 = time.time()
    R = Runner(a, g, plan)
    # --- build -------------------------------------------------------------
    if not a.no_build:
        pk = ["-p", "pliron-clif-check"] + (["-p", "rustc_codegen_pliron"] if (plan.glue or a.tier >= 1) else [])
        b = R.sh("build", ["cargo", "build", *pk])
        if not b["ok"]:
            print(open(b["log"]).read()[-3000:])
            print("quick: build failed")
            return 1
    # --- tier 0 --------------------------------------------------------------
    jobs = []
    for c in sorted(plan.crates):
        if c != "pliron-clif-check":
            jobs.append(("unit:" + c, lambda c=c: R.unit(c)))
    if plan.clif_opt:
        for name in ("smoke", "std"):
            jobs.append((f"corpus:{name}", lambda n=name: R.corpus(n)))
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        list(ex.map(lambda j: j[1](), jobs))
    skipped = []
    # --- tier 1 --------------------------------------------------------------
    seeds = []
    if a.tier >= 1 and (a.keep_going or all(r["ok"] for r in R.rows)):
        jb = []
        for n in plan.stanzas:
            jb.append((n, lambda n=n: R.sh(n, g.shell(["./test.sh", f"--only={n}"]), {"TEST_NOBUILD": "1"})))
            if n in g.POISON_STANZAS and any("slotmerge" in f or "slotprom" in f for f in plan.files):
                jb.append((f"poison:{n}", lambda n=n: R.sh(
                    f"poison:{n}", g.shell(["./test.sh", f"--only={n}"]),
                    {"TEST_NOBUILD": "1", "PLIRON_SM_POISON": "1", "TEST_OUT": f"target/tests-poison/{n}"})))
        heavy_first = sorted(jb, key=lambda j: j[0] not in ("std", "pgo", "unwind"))
        with cf.ThreadPoolExecutor(a.jobs) as ex:
            futs = [ex.submit(fn) for _, fn in heavy_first]
            if "fuzz" in plan.flags or plan.glue or plan.crates:
                futs.append(ex.submit(lambda: R.fuzz(a.seeds)))
            for f in futs:
                f.result()
    elif a.tier >= 1:
        skipped.append("tier-1 steps (tier 0 failed; --keep-going to run anyway)")
    # --- summary -------------------------------------------------------------
    bad = [r for r in R.rows if not r["ok"]]
    secs = round(time.time() - t0, 1)
    print(f"\nquick: {'FAILED' if bad else 'passed'} - {len(R.rows)} steps in {secs}s")
    if a.tier == 0:
        skipped.append("stanzas, wasm_smoke, fuzz seeds (tier 1: harness/quick.py)")
    skipped += [f"stanza {n}" for n in sorted(plan.stanza_all - set(plan.stanzas))[:0]]
    not_run = sorted(plan.stanza_all - set(plan.stanzas))
    print(f"SKIPPED: {len(not_run)} of {len(plan.stanza_all)} test.sh stanzas, fresh fuzz beyond the window, crates, "
          f"stage-2 UI, parity card" + (f"; {skipped[0]}" if skipped else ""))
    print("TO COMPLETE THE PROOF:")
    print(f"  {SLOW['gate']}   # everything above + all stanzas + poison + all corpora (~2-3 min quiet)")
    for k in ("wasm", "fuzz", "crates", "stage2"):
        if k in plan.flags:
            print(f"  {SLOW[k]}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
