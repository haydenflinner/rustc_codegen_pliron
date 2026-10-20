#!/usr/bin/env -S python3 -I
"""Measure LLVM parity on every axis and write one summary.

Usage: harness/parity.py [--skip native,wasm,ctime] [--from-logs]

Runs, one after another so they don't perturb each other (~15 min):
  native  bench/run.sh        criterion kernels, pliron vs saved LLVM baseline
  wasm    bench/wasm/run.sh   same kernels on wasm32 in node
  ctime   harness/ctime.py    compile time, release dylib

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
}
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
    rev = subprocess.run(SH + ["harness/src-rev.sh"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    host = subprocess.run(["rustc", "-vV"], cwd=ROOT, capture_output=True, text=True).stdout
    host = re.search(r"host: (\S+)", host).group(1) if "host:" in host else "?"
    md = [f"# Parity run — {time.strftime('%Y-%m-%d')}, {host}, backend {rev}", ""]
    gn, gw = geomean([r["ratio"] for r in native.values()]), geomean([r["ratio"] for r in wasm.values()])
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
                   "ctime": ctime, "native_geomean": gn, "wasm_geomean": gw}, jf, indent=1)
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    print("\n".join(md))


if __name__ == "__main__":
    main()
