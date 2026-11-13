#!/usr/bin/env -S python3 -I
"""Sum `layout:` lines (`PLIRON_LAYOUT_STATS=1` with `PLIRON_PGO_USE`).

  PLIRON_PGO_USE=p.prof PLIRON_LAYOUT_STATS=1 rustc ... 2>&1 | harness/layout_score.py

Each line is one profiled function's emission order scored against the
measured edge frequencies: expected taken unconditional jumps, taken
conditional branches and block executions over the profiled run. The
totals compare two placements of the same code without a timing run:
fewer taken transfers per block executed is a better layout.
"""
import re, sys

j = b = n = f = fl = bj = bb = 0.0
for line in sys.stdin:
    m = re.match(r"layout: entries (\d+) jumps (\d+) branches (\d+) blocks (\d+) floor (\d+) backjumps (\d+) backbranches (\d+)", line)
    if m:
        f += 1
        j += float(m.group(2)); b += float(m.group(3)); n += float(m.group(4)); fl += float(m.group(5)); bj += float(m.group(6)); bb += float(m.group(7))
if not n:
    sys.exit("layout_score: no `layout:` lines")
print(f"{int(f)} functions, {n / 1e6:.2f}M block executions: taken jumps {j / 1e6:.2f}M ({100 * j / n:.1f}%), "
      f"taken branches {b / 1e6:.2f}M ({100 * b / n:.1f}%), transfers per block {(j + b) / n:.3f} "
      f"(floor {fl / n:.3f}: the mass that leaves each block by other than its likeliest edge)")
print(f"  backward: jumps {bj / 1e6:.2f}M (unrotated loop latches), branches {bb / 1e6:.2f}M (rotated loops); "
      f"forward: jumps {(j - bj) / 1e6:.2f}M, branches {(b - bb) / 1e6:.2f}M")
