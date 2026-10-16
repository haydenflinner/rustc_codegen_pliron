#!/usr/bin/env python3
"""charts.py — build the SVG charts used by wasm-and-native-performance.md from the
raw result files in this directory. No dependencies."""
import math
import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))


def table(path, first):
    """rows of (name, pliron, llvm, ratio) from a `kernel ... ratio` table"""
    rows = []
    for ln in open(os.path.join(HERE, path), encoding="utf-8"):
        p = ln.split()
        if len(p) >= 4 and p[0] not in ("kernel", "program", "geomean") and re.match(r"^[\d.]+$", p[1]):
            if first and not p[0].startswith(first):
                continue
            try:
                rows.append((p[0].removeprefix("k_"), float(p[1]), float(p[2]), float(p[3])))
            except ValueError:
                pass
    return rows


def geomean(vals):
    return math.exp(sum(math.log(v) for v in vals) / len(vals))


COL = {"pliron": "#2b6cb0", "pliron2": "#c05621", "llvm": "#718096"}


def hbar(title, rows, fname, series_names=("ratio",), colors=("#2b6cb0",), note=""):
    """rows: [(label, v1, v2..)] ratios, log-2 axis from 0.5 to 16, line at 1.0"""
    lo, hi = math.log2(0.5), math.log2(16)
    W, left, right = 760, 170, 40
    bh = 14
    gap = 6
    n = len(series_names)
    rowh = n * bh + gap
    top = 56
    H = top + rowh * len(rows) + 50
    x = lambda v: left + (math.log2(max(min(v, 16), 0.5)) - lo) / (hi - lo) * (W - left - right)
    s = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" font-family="Segoe UI,Arial,sans-serif" font-size="12">']
    s.append(f'<rect width="{W}" height="{H}" fill="white"/>')
    s.append(f'<text x="{W/2}" y="22" text-anchor="middle" font-size="15" font-weight="bold">{title}</text>')
    if note:
        s.append(f'<text x="{W/2}" y="40" text-anchor="middle" fill="#555">{note}</text>')
    for t in (0.5, 1, 2, 4, 8, 16):
        xx = x(t)
        s.append(f'<line x1="{xx}" y1="{top-6}" x2="{xx}" y2="{H-34}" stroke="{"#222" if t == 1 else "#ddd"}" stroke-width="{1.5 if t == 1 else 1}"/>')
        s.append(f'<text x="{xx}" y="{H-18}" text-anchor="middle" fill="#444">{t}x</text>')
    s.append(f'<text x="{(left+W-right)/2}" y="{H-3}" text-anchor="middle" fill="#444">run time relative to LLVM (lower is better; log scale)</text>')
    for i, row in enumerate(rows):
        y0 = top + i * rowh
        s.append(f'<text x="{left-8}" y="{y0 + n*bh/2 + 4}" text-anchor="end">{row[0]}</text>')
        for j in range(n):
            v = row[1 + j]
            xx = x(v)
            x1 = x(1.0)
            x0, w = (min(x1, xx), abs(xx - x1))
            s.append(f'<rect x="{x0}" y="{y0 + j*bh}" width="{max(w,1.5)}" height="{bh-2}" fill="{colors[j]}"/>')
            tx = xx + 4 if v >= 1 else xx - 4
            anchor = "start" if v >= 1 else "end"
            s.append(f'<text x="{tx}" y="{y0 + j*bh + bh-3}" text-anchor="{anchor}" font-size="10" fill="#333">{v:.2f}</text>')
    if n > 1:
        lx = W - right - 150
        for j, nm in enumerate(series_names):
            s.append(f'<rect x="{lx}" y="{top-4 + j*16 - 10}" width="12" height="10" fill="{colors[j]}"/>')
            s.append(f'<text x="{lx+16}" y="{top-4 + j*16 - 1}">{nm}</text>')
    s.append("</svg>")
    open(os.path.join(HERE, fname), "w", encoding="utf-8").write("\n".join(s))


def vbar(title, groups, fname, ylabel):
    """groups: [(label, [(series, value)])] absolute values"""
    W, H = 760, 330
    left, bottom, top = 70, 60, 50
    vmax = max(v for _, ss in groups for _, v in ss) * 1.15
    s = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" font-family="Segoe UI,Arial,sans-serif" font-size="12">']
    s.append(f'<rect width="{W}" height="{H}" fill="white"/>')
    s.append(f'<text x="{W/2}" y="24" text-anchor="middle" font-size="15" font-weight="bold">{title}</text>')
    ph = H - top - bottom
    for k in range(5):
        v = vmax * k / 4
        yy = H - bottom - ph * k / 4
        s.append(f'<line x1="{left}" y1="{yy}" x2="{W-20}" y2="{yy}" stroke="#ddd"/>')
        s.append(f'<text x="{left-6}" y="{yy+4}" text-anchor="end" fill="#444">{v:.0f}</text>')
    s.append(f'<text x="14" y="{H/2}" transform="rotate(-90 14 {H/2})" text-anchor="middle" fill="#444">{ylabel}</text>')
    gw = (W - left - 20) / len(groups)
    cols = ["#2b6cb0", "#718096", "#c05621", "#2f855a"]
    for gi, (lab, ss) in enumerate(groups):
        bw = gw * 0.7 / len(ss)
        for si, (nm, v) in enumerate(ss):
            h = ph * v / vmax
            xx = left + gi * gw + gw * 0.15 + si * bw
            s.append(f'<rect x="{xx}" y="{H-bottom-h}" width="{bw-3}" height="{h}" fill="{cols[si]}"/>')
            s.append(f'<text x="{xx+bw/2-1}" y="{H-bottom-h-4}" text-anchor="middle" font-size="10">{v:g}</text>')
        s.append(f'<text x="{left+gi*gw+gw/2}" y="{H-bottom+16}" text-anchor="middle">{lab}</text>')
    names = [nm for nm, _ in groups[0][1]]
    for si, nm in enumerate(names):
        s.append(f'<rect x="{left+si*150}" y="{H-22}" width="12" height="10" fill="{cols[si]}"/>')
        s.append(f'<text x="{left+si*150+16}" y="{H-13}">{nm}</text>')
    s.append("</svg>")
    open(os.path.join(HERE, fname), "w", encoding="utf-8").write("\n".join(s))


ms = table("wasm-micro-scalar.txt", "k_")
mv = table("wasm-micro-simd.txt", "k_")
rs = table("wasm-real-scalar.txt", None)
rv = table("wasm-real-simd.txt", None)
nm = [r for r in table("native-micro.txt", None) if r[0] != "eqbug"]
nr = table("native-real.txt", None)

hbar("WebAssembly micro-kernels, no SIMD", [(r[0], r[3]) for r in ms], "chart-wasm-micro-scalar.svg",
     note=f"geometric mean {geomean([r[3] for r in ms]):.2f}x")
hbar("WebAssembly micro-kernels, +simd128", [(r[0], r[3]) for r in mv], "chart-wasm-micro-simd.svg",
     note=f"geometric mean {geomean([r[3] for r in mv]):.2f}x")
hbar("WebAssembly real programs", [(a[0], a[3], b[3]) for a, b in zip(rs, rv)], "chart-wasm-real.svg",
     series_names=("no SIMD", "+simd128"), colors=("#2b6cb0", "#c05621"),
     note=f"geometric mean {geomean([r[3] for r in rs]):.2f}x / {geomean([r[3] for r in rv]):.2f}x")
hbar("Native x86-64 (Windows): micro-kernels and real programs",
     [("micro " + r[0], r[3]) for r in nm] + [("real " + r[0], r[3]) for r in nr], "chart-native.svg",
     note="Cranelift-based backend relative to LLVM -O")


def size(path):
    out = {}
    for ln in open(os.path.join(HERE, path), encoding="utf-8"):
        m = re.match(r"size (\d+) .*/(p|s|pliron-\w+|stock-\w+)/", ln) or re.match(r"(\d+) .*/(pliron-\w+|stock-\w+)/", ln)
        if m:
            out[m.group(2)[0] if m.group(2) in ("p", "s") else m.group(2)] = int(m.group(1))
    return out


sizes = [
    ("micro, no SIMD", [("pliron", 38124 // 1024), ("LLVM", 10077 // 1024)]),
    ("micro, +simd128", [("pliron", 70491 // 1024), ("LLVM", 10861 // 1024)]),
    ("real, no SIMD", [("pliron", 460457 // 1024), ("LLVM", 138286 // 1024)]),
    ("real, +simd128", [("pliron", 485543 // 1024), ("LLVM", 142139 // 1024)]),
]
vbar("WebAssembly module size", sizes, "chart-wasm-size.svg", "KiB")
ct = [
    ("wasm micro crate", [("pliron", 0.55), ("LLVM", 0.42)]),
    ("wasm real, with std", [("pliron", 24.6), ("LLVM", 18.5)]),
    ("native real", [("pliron", 3.0), ("LLVM", 0.8)]),
]
vbar("Compile time", ct, "chart-compile-time.svg", "seconds")
print("charts written")
