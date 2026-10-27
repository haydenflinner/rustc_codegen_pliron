#!/usr/bin/env -S python3 -I
"""Measure LLVM parity on every axis and write one summary.

Usage: harness/parity.py [--skip native,wasm,ctime,stack] [--from-logs]

Runs, one after another so they don't perturb each other (~20 min):
  native  bench/run.sh        criterion kernels, pliron vs saved LLVM baseline
  wasm    bench/wasm/run.sh   same kernels on wasm32 in node
  ctime   harness/ctime.py    compile time, peak RSS, machine-code bytes
  stack   harness/frames.py   frame bytes on functions both backends define

The summary opens with the parity card: one rating per category and an
overall (see CARD below). Correctness comes from the latest saved runs of
the slow suites, which this script doesn't rerun: selfhost/verify.sh (UI,
stage 2 vs stage 1), harness/crates.py and the fuzz suite.

Logs go to target/parity/<leg>.log; the parsed table (ratio = pliron/LLVM,
< 1 means pliron is faster) and geomeans go to target/parity/summary.md
and summary.json. --from-logs re-parses existing logs without rerunning.
Paste summary.md's tables into docs/PARITY.md when they move.

Noise: kernels within ±3% are parity on a quiet machine; run nothing
else heavy meanwhile (the gate, other agents' builds) or the numbers lie.
"""
import argparse, json, math, os, re, shutil, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target/parity")


def _git_bash():
    """On Windows, subprocess finds the WSL stub (System32\\bash.exe) before
    Git Bash. Prefer the Git install; fall back to PATH minus the stubs."""
    cands = [r"C:\Program Files\Git\bin\bash.exe",
             r"C:\Program Files (x86)\Git\bin\bash.exe",
             shutil.which("bash")]
    for c in cands:
        if c and os.path.isfile(c) and "system32" not in c.lower() \
                and "windowsapps" not in c.lower():
            return c
    return "bash"


SH = [_git_bash()] if os.name == "nt" else []
PY = [sys.executable, "-I"] if os.name == "nt" else []
LEGS = {
    "native": SH + ["bench/run.sh"],
    "wasm": SH + ["bench/wasm/run.sh"],
    "ctime": PY + ["harness/ctime.py"],
    "stack": PY + ["harness/frames.py"],
}

# The parity card. Each category reduces to one ratio r = pliron / LLVM
# (lower is better) and rates 100 + 50 * log2(1/r), clamped to [0, 150]:
# parity is 100, twice as good 150, half as good 50. The log scale makes
# the weighted overall a weighted geomean of the ratios, so 2x better on
# one axis exactly offsets 2x worse on another of equal weight. Weights
# are a judgement call; change them here.
CARD = [
    # code, name, weight
    ("SPD", "runtime speed", 30),
    ("CMP", "compile time", 15),
    ("MEM", "compile memory", 10),
    ("SIZ", "code size", 10),
    ("STK", "stack frames", 15),
    ("COR", "correctness", 20),
]
VERIFY = os.environ.get("PLIRON_VERIFY_OUT") or os.path.join(ROOT, "../rust-sh/verify-out")
UNIT = {"ns": 1e-6, "µs": 1e-3, "us": 1e-3, "ms": 1.0, "s": 1e3}


def parse_native(text):
    """bench/run.sh prints criterion output twice: the LLVM run against its
    previous self, then the pliron run against the fresh LLVM baseline.
    The last block's `change` is pliron vs LLVM."""
    rows = {}
    pat = re.compile(r"^(\w+)\s+time:\s+\[\S+ \S+ ([\d.]+) (\S+) .*?\n\s+change: \[\S+ (\S+)%",
                     re.M)
    for m in pat.finditer(text):
        name, t, u, ch = m.group(1), float(m.group(2)), m.group(3), m.group(4)
        ch = float(ch.replace("−", "-").replace("+", ""))
        ms = t * UNIT.get(u, 1.0)
        r = 1 + ch / 100
        rows[name] = {"pliron_ms": ms, "llvm_ms": ms / r, "ratio": r}
    return rows


def parse_wasm(text):
    rows = {}
    for line in text.splitlines():
        m = re.match(r"^(\w+)\s+([\d.]+)ms\s+([\d.]+)ms\s+(-?[\d.]+)%(.*)$", line)
        if m:
            rows[m.group(1)] = {"llvm_ms": float(m.group(2)), "pliron_ms": float(m.group(3)),
                                "ratio": 1 + float(m.group(4)) / 100,
                                "note": m.group(5).strip()}
    sizes = re.findall(r"^(\w+)\s+\S+\.wasm \((\d+) B\)", text, re.M)
    return rows, {k: int(v) for k, v in sizes}


def geomean(rs):
    rs = [r for r in rs if r and r > 0]
    return math.exp(sum(map(math.log, rs)) / len(rs)) if rs else None


def rating(r):
    return None if not r else max(0.0, min(150.0, 100 + 50 * math.log2(1 / r)))


def lines(path):
    return [l.strip() for l in open(path) if l.strip()] if os.path.exists(path) else None


def age(path):
    if not os.path.exists(path):
        return "missing"
    h = (time.time() - os.path.getmtime(path)) / 3600
    return f"{h:.0f} h old" if h < 48 else f"{h / 24:.0f} d old"


def correctness():
    """{suite: (ratio, detail)} from the latest saved runs. Each suite's ratio
    is (pliron failures + 1) / (LLVM failures + 1): the +1 keeps a clean run
    finite, and one failure against none rates 50. Never below parity for
    the UI suite, and small reruns don't count."""
    out = {}
    s1, s2 = lines(os.path.join(VERIFY, "stage1.failures")), lines(os.path.join(VERIFY, "stage2.failures"))
    if s1 is not None and s2 is not None and not os.path.exists(os.path.join(VERIFY, "stage2.incomplete")):
        only2, only1 = set(s2) - set(s1), set(s1) - set(s2)
        # Capped at parity: stage-1-only failures on this box are
        # environment issues (dsymutil, backtraces), not LLVM codegen.
        out["ui"] = (max(1.0, (len(only2) + 1) / (len(only1) + 1)),
                     f"{len(only2)} stage-2-only vs {len(only1)} stage-1-only "
                     f"({age(os.path.join(VERIFY, 'stage2.failures'))})")
    cp = os.path.join(ROOT, "target/crates/report.json")
    # A `--only` rerun or a one-seed bisect overwrites the same result
    # files; only a run of real size counts.
    if os.path.exists(cp) and len(rows := json.load(open(cp))) >= 10:
        bad = sum(max(len(r.get("pliron_only", [])), r["status"] in ("ICE", "BUILD", "FAIL", "TIMEOUT"))
                  for r in rows)
        out["crates"] = (bad + 1, f"{bad} pliron-only across {len(rows)} crates ({age(cp)})")
    fp = os.path.join(ROOT, "target/harness/fuzz/results.json")
    if os.path.exists(fp) and len(res := json.load(open(fp))) >= 100:
        bad = sum(v[0] != "pass" for v in res.values())
        out["fuzz"] = (bad + 1, f"{bad} of {len(res)} seeds ({age(fp)})")
    return out


def card(native, wasm, sizes, ctime, stack):
    """[(code, name, weight, ratio, detail)] for the categories with data."""
    rows = (ctime or {}).get("rows", [])
    col = lambda k: [r[k] for r in rows if r.get(k)]
    gn, gw = geomean([r["ratio"] for r in native.values()]), geomean([r["ratio"] for r in wasm.values()])
    spd = geomean([g for g in (gn, gw) if g])
    wsz = sizes["pliron"] / sizes["llvm"] if sizes.get("llvm") and sizes.get("pliron") else None
    cor = correctness()
    have = {
        "SPD": (spd, " · ".join(f"{k} {g:.3f}" for k, g in (("native", gn), ("wasm", gw)) if g)),
        "CMP": (geomean(col("ratio")), f"{len(col('ratio'))} inputs"),
        "MEM": (geomean(col("rss_ratio")), f"peak RSS, {len(col('rss_ratio'))} inputs"),
        "SIZ": (geomean(col("size_ratio") + ([wsz] if wsz else [])),
                f"machine code, {len(col('size_ratio'))} inputs" + (f" + wasm module {wsz:.2f}" if wsz else "")),
        "STK": ((stack or {}).get("ratio"),
                f"{stack['input']}, {stack['shared_fns']} shared fns" if stack else ""),
        "COR": (geomean([r for r, _ in cor.values()]), " · ".join(f"{k}: {d}" for k, (_, d) in cor.items())),
    }
    return [(c, n, w) + have[c] for c, n, w in CARD if have[c][0]]


def card_md(rows):
    tot = sum(w for _, _, w, _, _ in rows)
    ovr = sum(w * rating(r) for _, _, w, r, _ in rows) / tot if tot else None
    miss = [c for c, _, _ in CARD if c not in {r[0] for r in rows}]
    md = [f"## Parity card: **{ovr:.0f}** overall" + (f" (no data: {', '.join(miss)})" if miss else ""), "",
          "| " + " | ".join(c for c, *_ in rows) + " |", "|" + "---|" * len(rows),
          "| " + " | ".join(f"**{rating(r):.0f}**" for _, _, _, r, _ in rows) + " |", "",
          "| category | weight | pliron/LLVM | rating | from |", "|---|---|---|---|---|"]
    md += [f"| {c} {n} | {w} | {r:.3f} | {rating(r):.0f} | {d} |" for c, n, w, r, d in rows]
    md += ["", "Rating = 100 + 50·log2(LLVM/pliron), clamped to 0–150: 100 is parity, "
           "150 twice as good, 50 half as good. Overall = weighted mean of ratings."]
    return md, ovr


def table(rows):
    out = ["| kernel | LLVM | pliron | pliron/LLVM |", "|---|---|---|---|"]
    for k in sorted(rows):
        r = rows[k]
        flag = " ◄ faster" if r["ratio"] < 0.97 else (" ▲ slower" if r["ratio"] > 1.03 else "")
        out.append(f"| {k} | {r['llvm_ms']:.3f} ms | {r['pliron_ms']:.3f} ms | {r['ratio']:.3f}{flag} |")
    return out


def main():
    ap = argparse.ArgumentParser(usage=__doc__)
    ap.add_argument("--skip", default="")
    ap.add_argument("--from-logs", action="store_true")
    a = ap.parse_args()
    os.makedirs(OUT, exist_ok=True)
    skip = set(filter(None, a.skip.split(",")))
    for leg, cmd in LEGS.items():
        if leg in skip or a.from_logs:
            continue
        print(f"parity: running {leg} ...", flush=True)
        with open(os.path.join(OUT, f"{leg}.log"), "w") as f:
            subprocess.run(cmd, cwd=ROOT, stdout=f, stderr=subprocess.STDOUT)
    rd = lambda leg: open(os.path.join(OUT, f"{leg}.log"), encoding="utf-8",
                          errors="replace").read() \
        if os.path.exists(os.path.join(OUT, f"{leg}.log")) else ""
    native = parse_native(rd("native"))
    wasm, sizes = parse_wasm(rd("wasm"))
    ctp = os.path.join(OUT, "ctime.json")
    ctime = json.load(open(ctp)) if os.path.exists(ctp) else None
    skp = os.path.join(OUT, "stack.json")
    stack = json.load(open(skp)) if os.path.exists(skp) else None
    crows = card(native, wasm, sizes, ctime, stack)
    rev = subprocess.run(SH + ["harness/src-rev.sh"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    host = subprocess.run(["rustc", "-vV"], cwd=ROOT, capture_output=True, text=True).stdout
    host = re.search(r"host: (\S+)", host).group(1) if "host:" in host else "?"
    md = [f"# Parity run — {time.strftime('%Y-%m-%d')}, {host}, backend {rev}", ""]
    gn, gw = geomean([r["ratio"] for r in native.values()]), geomean([r["ratio"] for r in wasm.values()])
    cmd, ovr = card_md(crows)
    md += cmd + ["", "## Axes", ""]
    md += ["| axis | pliron/LLVM |", "|---|---|"]
    if gn:
        md.append(f"| native runtime (geomean, {len(native)} kernels) | {gn:.3f} |")
    if gw:
        md.append(f"| wasm runtime (geomean, {len(wasm)} kernels) | {gw:.3f} |")
    if sizes.get("llvm") and sizes.get("pliron"):
        md.append(f"| wasm module size | {sizes['pliron'] / sizes['llvm']:.2f} ({sizes['pliron'] // 1024} KiB vs {sizes['llvm'] // 1024} KiB) |")
    for r in (ctime or {}).get("rows", []):
        if r.get("ratio"):
            md.append(f"| compile time: {r['input']} | {r['ratio']:.2f} ({r['pliron_s']:.2f} s vs {r['llvm_s']:.2f} s) |")
    if native:
        md += ["", "## Native runtime", ""] + table(native)
    if wasm:
        md += ["", "## Wasm runtime", ""] + table(wasm)
    open(os.path.join(OUT, "summary.md"), "w", encoding="utf-8").write("\n".join(md) + "\n")
    with open(os.path.join(OUT, "summary.json"), "w", encoding="utf-8") as jf:
        json.dump({"rev": rev, "host": host, "native": native, "wasm": wasm, "wasm_sizes": sizes,
                   "ctime": ctime, "native_geomean": gn, "wasm_geomean": gw, "stack": stack,
                   "card": {"overall": ovr, "categories": {c: {"weight": w, "ratio": r, "rating": rating(r),
                                                                "detail": d} for c, _, w, r, d in crows}}},
                  jf, indent=1)
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    print("\n".join(md))


if __name__ == "__main__":
    main()
