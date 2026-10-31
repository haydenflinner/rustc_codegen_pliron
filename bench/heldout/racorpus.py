#!/usr/bin/env python3
"""Hot-function CLIF corpus from the held-out census, for code-quality work
that needs a fast, deterministic loop (register allocation, facts).

  bench/heldout/racorpus.py build [--backend DYLIB]   rebuild the tuning-set
        programs with PLIRON_CORPUS and keep the census' hot functions in
        target/racorpus/hot/ (weights.json: pliron samples per file)
  bench/heldout/racorpus.py score [--clif-check BIN] [--tag T]   run
        `clif-check codegen` on that corpus and print a sample-weighted
        stack-access score (lower is better), per function and in total

The check set (rust-compression, rust-html-rewriter, rust-protobuf) is left
out: tuning stays off it. Needs target/heldout/census.json (census.py).
"""
import argparse, json, os, re, shutil, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import census  # noqa: E402

OUT = os.path.join(ROOT, "target", "racorpus")
HELD = os.path.join(ROOT, "target", "heldout")
TUNING = ["blake3-scalar", "blind-sig", "pulldown-cmark", "regex", "rust-json"]
TOOLCHAIN = "nightly-2026-10-22"
SHIM = os.path.join(ROOT, "bench", "heldout", "sightglass-api")
CRATE = "rust-benchmark"
# A function is hot if it has this share of the program's pliron samples.
MIN_SHARE = 0.01


def safe_name(n):
    """The corpus file stem (`pipeline::corpus_dump`): `diffcheck::safe_name`
    plus, for names over 80 chars, the full name's FNV-1a hash."""
    s = re.sub(r"[^A-Za-z0-9_]", "_", n)[:80]
    if len(n) > 80:
        h = 0x811c9dc5
        for b in n.encode():
            h = ((h ^ b) * 0x01000193) & 0xffffffff
        s += f"_{h:08x}"
    return s


def nm_map(exe):
    """Corpus file stem -> demangled keys, from the binary's symbol table
    (stems are 80-char prefixes, so one stem can name several symbols)."""
    nm = os.path.join(os.path.dirname(census.objdump()), "llvm-nm")
    raw = subprocess.run([nm, "--defined-only", exe], capture_output=True, text=True).stdout.splitlines()
    dem = subprocess.run([nm, "--defined-only", "-C", exe], capture_output=True, text=True).stdout.splitlines()
    out = {}
    for a, b in zip(raw, dem):
        pa, pb = a.split(" ", 2), b.split(" ", 2)
        if len(pa) == 3 and len(pb) == 3 and pa[0] == pb[0]:
            out.setdefault(safe_name(pa[2][1:] if pa[2].startswith("_") else pa[2]), set()).add(
                census.demangle_key(pb[2]))
    return out


def build(a):
    rep = json.load(open(os.path.join(HELD, "census.json")))
    hot_dir = os.path.join(OUT, "hot")
    shutil.rmtree(hot_dir, ignore_errors=True)
    os.makedirs(hot_dir)
    weights = {}
    for prog in TUNING:
        rows = rep.get(prog, {}).get("rows", [])
        total = rep.get(prog, {}).get("pliron_samples", 0) or 1
        want = {r["fn"]: r["pliron"] for r in rows if r["pliron"] >= MIN_SHARE * total}
        all_dir = os.path.join(OUT, prog, "all")
        tdir = os.path.join(OUT, prog, "target")
        shutil.rmtree(all_dir, ignore_errors=True)
        shutil.rmtree(tdir, ignore_errors=True)
        os.makedirs(all_dir)
        env = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, PLIRON_CORPUS=all_dir,
                   RUSTFLAGS=f"-Zcodegen-backend={a.backend}")
        print(f"build {prog}", flush=True)
        r = subprocess.run(["cargo", "build", "--release", "--target-dir", tdir,
                            "--config", f'patch.crates-io.sightglass-api.path="{SHIM}"'],
                           cwd=os.path.join(HELD, "src", prog, CRATE), env=env,
                           capture_output=True, text=True)
        if r.returncode != 0:
            print(r.stderr[-2000:])
            sys.exit(f"{prog}: build failed")
        names = nm_map(os.path.join(HELD, "pliron", prog, "release", "benchmark"))
        kept = 0
        # One file per symbol: a function can be dumped more than once (a
        # pipeline retry, or copies in several CGUs); keep the last.
        last = {}
        for f in os.listdir(all_dir):
            m = re.match(r"(.+)\.(\d+)\.clif$", f)
            if m and int(m.group(2)) >= last.get(m.group(1), (-1, ""))[0]:
                last[m.group(1)] = (int(m.group(2)), f)
        for stem, (_, f) in sorted(last.items()):
            m = re.match(r"(.+)\.\d+\.clif$", f)
            keys = names.get(m.group(1), set())
            key = next(iter(keys)) if len(keys) == 1 and keys <= set(want) else None
            if key:
                dst = f"{prog}__{f}"
                shutil.copy(os.path.join(all_dir, f), os.path.join(hot_dir, dst))
                weights[dst] = {"samples": want[key], "fn": key, "prog": prog}
                kept += 1
        print(f"  {prog}: {kept} files for {len(want)} hot functions "
              f"({len(os.listdir(all_dir))} dumped)")
        # The full dump is large; the hot subset is what the loop uses.
        shutil.rmtree(all_dir, ignore_errors=True)
        shutil.rmtree(tdir, ignore_errors=True)
    json.dump(weights, open(os.path.join(hot_dir, "weights.json"), "w"), indent=1)


def score(a):
    hot_dir = os.path.join(OUT, "hot")
    weights = json.load(open(os.path.join(hot_dir, "weights.json")))
    out = os.path.join(OUT, f"codegen-{a.tag}.json")
    r = subprocess.run([a.clif_check, "codegen", hot_dir, out, "--jobs", str(a.jobs)],
                       capture_output=True, text=True)
    print(r.stdout.strip())
    rep = json.load(open(out))
    total = spill = 0.0
    rows = []
    for f in rep["functions"]:
        w = weights.get(f["file"])
        if not w or f["status"] != "ok" or not f["winsts"]:
            continue
        # Samples spent on stack accesses, estimated: the function's samples
        # times its loop-weighted share of stack accesses.
        s = w["samples"] * f["wstack"] / f["winsts"]
        sp = w["samples"] * f.get("wspill", 0) / f["winsts"]
        total += s
        spill += sp
        rows.append((s, w["prog"], f["insts"], f["stack"], sp / max(s, 1e-9),
                     f.get("slots", 0), f.get("spills", 0), w["fn"]))
    rows.sort(reverse=True)
    for s, prog, n, st, frac, sl, sp, fn in rows[: a.top]:
        print(f"  {s:7.1f} {prog:14} insts {n:5} stack {st:4} spill% {100*frac:3.0f} slots {sl:4}B spills {sp:4}B  {fn[:66]}")
    print(f"score {a.tag}: {total:.1f} estimated stack-access samples "
          f"({spill:.1f} spill-slot, {total - spill:.1f} stack-slot)")


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build")
    b.add_argument("--backend", default=os.path.join(ROOT, "target", "release", "librustc_codegen_pliron.dylib"))
    s = sub.add_parser("score")
    s.add_argument("--clif-check", default=os.path.join(ROOT, "target", "debug", "clif-check"))
    s.add_argument("--tag", default="cur")
    s.add_argument("--jobs", type=int, default=4)
    s.add_argument("--top", type=int, default=15)
    a = ap.parse_args()
    {"build": build, "score": score}[a.cmd](a)


if __name__ == "__main__":
    main()
