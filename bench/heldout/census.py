#!/usr/bin/env -S python3 -I
"""Gap census: where the held-out programs lose time to LLVM, per function.

  bench/heldout/census.py [--only a,b] [--runs N] [--top K] [--json OUT]

Uses the binaries `bench/heldout/run.py` built (target/heldout/{llvm,pliron}/
<name>/release/benchmark). Each side runs N times under samply (Homebrew
python as the parent: samply cannot attach to system binaries like
/bin/sh, but follows a non-system parent's children), self samples are
summed per demangled function over the benchmark processes, and the gap
(pliron samples - LLVM samples, scaled to each side's total runtime) is
attributed to functions. Inlining differs between the backends, so a
function present on one side only is reported as such; read the table
together with the inclusive view (`--incl`).

For each top function, static counts from the disassembly on both sides:
instructions, stack loads/stores (`[sp`/`[x29`), conditional branches,
calls. Writes target/heldout/census.json and prints a table per program.
"""
import argparse, bisect, collections, gzip, json, os, re, shutil, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
OUT = os.path.join(ROOT, "target", "heldout")
PROGS = ["blake3-scalar", "blind-sig", "pulldown-cmark", "regex", "rust-compression",
         "rust-html-rewriter", "rust-json", "rust-protobuf"]
# The untouched check set (codegen-plan.md): reported, never tuned on.
CHECK = {"rust-html-rewriter", "rust-compression", "rust-protobuf"}


def objdump():
    od = shutil.which("llvm-objdump")
    if od:
        return od
    rl = os.path.expanduser("~/.rustup/toolchains")
    for d in sorted(os.listdir(rl)) if os.path.isdir(rl) else []:
        for arch in ("aarch64-apple-darwin", "x86_64-unknown-linux-gnu", "x86_64-apple-darwin"):
            p = os.path.join(rl, d, "lib", "rustlib", arch, "bin", "llvm-objdump")
            if os.path.exists(p):
                return p
    sys.exit("census: no llvm-objdump (rustup component add llvm-tools)")


def profile(exe, cwd, runs, out):
    """samply-record `runs` executions of `exe`; returns the profile path."""
    py = shutil.which("python3")
    code = ("import subprocess,sys\n"
            f"for _ in range({runs}):\n"
            f"    subprocess.run([{exe!r}], cwd={cwd!r}, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n")
    r = subprocess.run(["samply", "record", "--save-only", "--unstable-presymbolicate", "-r", "4000",
                        "-o", out, "--", py, "-c", code], capture_output=True, text=True)
    if r.returncode != 0 or not os.path.exists(out):
        sys.exit(f"census: samply failed for {exe}:\n{r.stderr[-800:]}")
    return out


def demangle_key(s):
    """Normalize a demangled symbol: drop `::h<hash>` and crate disambiguators."""
    s = re.sub(r"::h[0-9a-f]{16}$", "", s)
    s = re.sub(r"\[[0-9a-f]{8,}\]", "", s)
    return s


def self_samples(path, proc_substr="benchmark"):
    p = json.load(gzip.open(path))
    sy = json.load(open(path.replace(".json.gz", ".json.syms.json")))
    st = sy["string_table"]
    tabs = {}
    for e in sy["data"]:
        rows = sorted((r["rva"], r["size"], r["symbol"]) for r in e["symbol_table"])
        tabs[e["debug_name"]] = ([r[0] for r in rows], rows)
    libs = p["libs"]

    def name(lib, addr):
        t = tabs.get(libs[lib]["debugName"]) if lib is not None else None
        if t:
            k = bisect.bisect_right(t[0], addr) - 1
            if k >= 0:
                rva, size, sym = t[1][k]
                if addr < rva + max(size, 1):
                    return st[sym]
        return f"{libs[lib]['name'] if lib is not None else '?'}+?"

    selfc = collections.Counter()
    total = 0
    for t in p["threads"]:
        if proc_substr not in (t.get("processName") or t.get("name") or ""):
            continue
        ft, fu, rt, sk = t["frameTable"], t["funcTable"], t["resourceTable"], t["stackTable"]
        for s in t["samples"]["stack"]:
            if s is None:
                continue
            f = sk["frame"][s]
            fn = ft["func"][f]
            res = fu["resource"][fn]
            lib = rt["lib"][res] if res is not None and res >= 0 else None
            selfc[demangle_key(name(lib, ft["address"][f]))] += 1
            total += 1
    return selfc, total


def static_counts(exe):
    """Demangled function -> (insts, stack mem ops, cond branches, calls)."""
    txt = subprocess.run([objdump(), "-d", "-C", "--no-show-raw-insn", exe],
                         capture_output=True, text=True).stdout
    out, cur, c = {}, None, None
    for line in txt.splitlines():
        m = re.match(r"^[0-9a-f]+ <(.+)>:$", line)
        if m:
            if cur:
                out[demangle_key(cur)] = tuple(c)
            cur, c = m.group(1), [0, 0, 0, 0]
            continue
        if cur is None or not line.strip():
            continue
        parts = line.split("\t")
        if len(parts) < 2:
            continue
        mn = parts[1].strip() if len(parts) > 1 else ""
        ops = parts[2] if len(parts) > 2 else ""
        c[0] += 1
        if "[sp" in ops or "[x29" in ops:
            c[1] += 1
        if mn.startswith(("b.", "cbz", "cbnz", "tbz", "tbnz")):
            c[2] += 1
        if mn in ("bl", "blr"):
            c[3] += 1
    if cur:
        out[demangle_key(cur)] = tuple(c)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only")
    ap.add_argument("--runs", type=int, default=150)
    ap.add_argument("--top", type=int, default=12)
    ap.add_argument("--json", default=os.path.join(OUT, "census.json"))
    a = ap.parse_args()
    progs = [p for p in PROGS if not a.only or p in a.only.split(",")]
    report = {}
    for prog in progs:
        cwd = os.path.join(OUT, "src", prog)
        sides = {}
        for side in ("llvm", "pliron"):
            exe = os.path.join(OUT, side, prog, "release", "benchmark")
            if not os.path.exists(exe):
                sys.exit(f"census: {exe} missing; run bench/heldout/run.py first")
            prof = profile(exe, cwd, a.runs, os.path.join(OUT, f"census-{side}-{prog}.json.gz"))
            sc, tot = self_samples(prof)
            sides[side] = (sc, tot, static_counts(exe))
        (lc, lt, ls), (pc, pt, ps) = sides["llvm"], sides["pliron"]
        gap = pt - lt
        rows = []
        for fn in set(lc) | set(pc):
            d = pc.get(fn, 0) - lc.get(fn, 0)
            rows.append((d, fn, pc.get(fn, 0), lc.get(fn, 0)))
        rows.sort(reverse=True)
        tag = " (check set)" if prog in CHECK else ""
        print(f"\n== {prog}{tag}: pliron {pt} / LLVM {lt} samples ({pt/max(lt,1):.2f}x), gap {gap}")
        print(f"{'gap':>6} {'share':>6} {'pliron':>7} {'llvm':>6}  {'insts p/l':>11} {'stack p/l':>9} {'br p/l':>7}  function")
        res = []
        for d, fn, p, l in rows[:a.top]:
            sp, sl = ps.get(fn), ls.get(fn)
            st = lambda i: f"{sp[i] if sp else '-'}/{sl[i] if sl else '-'}"
            share = d / gap if gap else 0
            print(f"{d:6d} {100*share:5.1f}% {p:7d} {l:6d}  {st(0):>11} {st(1):>9} {st(2):>7}  {fn[:90]}")
            res.append({"fn": fn, "gap": d, "share": share, "pliron": p, "llvm": l,
                        "static_pliron": sp, "static_llvm": sl})
        # LLVM-side functions with no pliron counterpart of similar weight:
        # usually where LLVM inlined what pliron kept separate (pair them up).
        neg = [r for r in rows if r[0] < 0][-4:]
        for d, fn, p, l in reversed(neg):
            print(f"{d:6d} {'':>6} {p:7d} {l:6d}  {'':>11} {'':>9} {'':>7}  {fn[:90]}  (LLVM-heavy)")
        report[prog] = {"pliron_samples": pt, "llvm_samples": lt, "rows": res}
    with open(a.json, "w") as f:
        json.dump(report, f, indent=1)


if __name__ == "__main__":
    main()
