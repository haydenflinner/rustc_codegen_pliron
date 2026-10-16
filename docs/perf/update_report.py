#!/usr/bin/env python3
"""update_report.py — refresh the measured tables in wasm-and-native-performance.md
from the raw result files (run charts.py and build_html.py afterwards)."""
import math
import os
import re

H = os.path.dirname(os.path.abspath(__file__))
md_path = os.path.join(H, "wasm-and-native-performance.md")
md = open(md_path, encoding="utf-8").read()


def rows(name, prefix=None):
    out = []
    for ln in open(os.path.join(H, name), encoding="utf-8"):
        p = ln.split()
        if len(p) >= 4 and re.match(r"^[\d.]+$", p[1]) and re.match(r"^[\d.]+$", p[3]) and p[0] not in ("kernel", "program", "size"):
            if prefix and not p[0].startswith(prefix):
                continue
            out.append((p[0].removeprefix("k_"), p[1], p[2], p[3]))
    return out


def geo(vs):
    return math.exp(sum(math.log(float(v)) for v in vs) / len(vs))


def sizes(name):
    s = {}
    for ln in open(os.path.join(H, name), encoding="utf-8"):
        m = re.match(r"^(\d+) .*(pliron|stock)-", ln) or re.match(r"^size (\d+) .*/(p|s)/", ln)
        if m:
            s["p" if m.group(2) in ("pliron", "p") else "s"] = int(m.group(1))
    return s


def times(name):
    t = {}
    for ln in open(os.path.join(H, name), encoding="utf-8"):
        m = re.match(r"wasm build .* (pliron|llvm):\s+([\d.]+) s", ln)
        if m:
            t[m.group(1)] = float(m.group(2))
    return t


def replace_table(md, header_start, new_rows_md):
    """replace the table whose header row starts with `header_start`"""
    i = md.index(header_start)
    j = i
    lines = md[i:].split("\n")
    n = 0
    for k, ln in enumerate(lines):
        if not ln.startswith("|"):
            break
        n = k + 1
    head = lines[:2]
    new = head + new_rows_md
    return md[:i] + "\n".join(new + lines[n:])


def fmt_rows(rs, bold_below=None):
    out = []
    for n, a, b, r in rs:
        rr = f"{float(r):.2f}"
        if bold_below and float(r) < bold_below:
            rr = f"**{rr}**"
        out.append(f"| {n} | {a} | {b} | {rr} |")
    return out


ms, mv = rows("wasm-micro-scalar.txt", "k_"), rows("wasm-micro-simd.txt", "k_")
rs, rv = rows("wasm-real-scalar.txt"), rows("wasm-real-simd.txt")
nm = [r for r in rows("native-micro.txt") if r[0] != "eqbug"]
nr = rows("native-real.txt")

md = replace_table(md, "| Kernel | pliron ms | LLVM ms | Ratio |\n|---|---|---|---|\n| adler | 12",
                   fmt_rows(ms)) if False else md
# micro tables: two tables with the same header; replace in order
parts = md.split("| Kernel | pliron ms | LLVM ms | Ratio |")
assert len(parts) == 3, len(parts)


def body(prefix_rows, bold):
    return "\n|---|---|---|---|\n" + "\n".join(fmt_rows(prefix_rows, 0.9 if bold else None)) + "\n"


def cut(t):
    # drop the old table body, keep text after the first blank line
    k = t.index("\n\n")
    return t[k:]


md = parts[0] + "| Kernel | pliron ms | LLVM ms | Ratio |" + body(ms, False).rstrip("\n") + cut(parts[1]) \
    + "| Kernel | pliron ms | LLVM ms | Ratio |" + body(mv, True).rstrip("\n") + cut(parts[2])

# real programs (wasm)
real_head = "| Program | pliron ms | LLVM ms | Ratio, no SIMD | Ratio, +simd128 |"
i = md.index(real_head)
j = md.index("\n\n", i)
tbl = [real_head, "|---|---|---|---|---|"]
for a, b in zip(rs, rv):
    tbl.append(f"| {a[0]} | {a[1]} | {a[2]} | {float(a[3]):.2f} | {float(b[3]):.2f} |")
tbl.append(f"| **geometric mean** | | | **{geo([r[3] for r in rs]):.2f}** | **{geo([r[3] for r in rv]):.2f}** |")
md = md[:i] + "\n".join(tbl) + md[j:]

# native micro
nh = "| Micro-kernel | LLVM ms | pliron ms | Ratio |"
i = md.index(nh)
j = md.index("\n\n", i)
t = [nh, "|---|---|---|---|"] + [f"| {n} | {a} | {b} | {float(r):.2f} |" for n, a, b, r in nm]
t.append(f"| **geometric mean** | | | **{geo([r[3] for r in nm]):.2f}** |")
md = md[:i] + "\n".join(t) + md[j:]
nh = "| Real program | LLVM ms | pliron ms | Ratio |"
i = md.index(nh)
j = md.index("\n\n", i)
t = [nh, "|---|---|---|---|"] + [f"| {n} | {a} | {b} | {float(r):.2f} |" for n, a, b, r in nr]
t.append(f"| **geometric mean** | | | **{geo([r[3] for r in nr]):.2f}** |")
md = md[:i] + "\n".join(t) + md[j:]

# report the headline numbers used in the summary text
print("micro scalar", round(geo([r[3] for r in ms]), 2), "simd", round(geo([r[3] for r in mv]), 2))
print("real scalar", round(geo([r[3] for r in rs]), 2), "simd", round(geo([r[3] for r in rv]), 2))
print("native micro", round(geo([r[3] for r in nm]), 2), "real", round(geo([r[3] for r in nr]), 2))
open(md_path, "w", encoding="utf-8").write(md)
