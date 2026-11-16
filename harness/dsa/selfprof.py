# selfprof.py file.cpuprofile [top] -> self-time share per function of a
# `node --cpu-prof` run of bench.mjs. Samples are grouped by function name
# with the `[crate-hash]` disambiguators of rustc's v0 demangling removed,
# so one function reads the same on the LLVM and the pliron side.
# Shares are of all samples that are not `(idle)`; wasm frames are the
# ones with a `wasm://` url.
import collections
import json
import re
import sys

d = json.load(open(sys.argv[1]))
top = int(sys.argv[2]) if len(sys.argv) > 2 else 10
nodes = {n["id"]: n for n in d["nodes"]}
deltas = d.get("timeDeltas") or [1] * len(d["samples"])
self_us = collections.Counter()
wasm_us = 0
total = 0
for nid, dt in zip(d["samples"], deltas):
    cf = nodes[nid]["callFrame"]
    name = cf["functionName"] or "(anonymous)"
    if name == "(idle)":
        continue
    is_wasm = cf.get("url", "").startswith("wasm://")
    name = re.sub(r"\[[0-9a-f]{6,}\]", "", name)
    if not is_wasm and cf.get("url"):
        name = f"js: {name}"
    self_us[name] += dt
    total += dt
    if is_wasm:
        wasm_us += dt
print(f"  {total / 1000:.0f} ms sampled, {100 * wasm_us / max(total, 1):.1f}% in wasm")
for name, us in self_us.most_common(top):
    short = name if len(name) <= 110 else name[:52] + " ... " + name[-53:]
    print(f"  {100 * us / total:5.1f}%  {short}")
