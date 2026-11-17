#!/usr/bin/env python3
# harness/dsa/v8inl.py LLVM.wasm OURS.wasm [LLVM.cpuprofile OURS.cpuprofile [TOP]] -- the functions V8 can
# inline on the LLVM side (body <= 500 wire bytes, V8's --wasm-inlining-max-size) but not on ours, joined by
# name (crate hashes removed); with prof.sh profiles (globs accepted), weighted by self ms on each side.
# dims_page, 2026-11-17: 1,506 such functions, 1,143 ms of our self time against 352 of LLVM's.
import subprocess, sys, re, json, glob, collections
def sizes(p):
    out = subprocess.run(['wasm-objdump', '-x', '-j', 'Code', p], capture_output=True, text=True).stdout
    r = collections.defaultdict(list)
    for l in out.splitlines():
        m = re.match(r'\s*- func\[(\d+)\] size=(\d+) <(.*)>', l)
        if m:
            n = re.sub(r'\[[0-9a-f]{8,}\]', '', m.group(3)); n = re.sub(r'\[\d+\]$', '', n)
            r[n].append(int(m.group(2)))
    return r
def prof(p):
    d = json.load(open(glob.glob(p)[0])); t = collections.Counter(); ids = {n['id']: n for n in d['nodes']}
    for s, x in zip(d['samples'], d['timeDeltas']):
        nm = ids[s]['callFrame']['functionName'] or '(anon)'
        nm = re.sub(r'\[[0-9a-f]{8,}\]', '', nm); nm = re.sub(r'\[\d+\]$', '', nm)
        t[nm] += x / 1000
    return t
a = sizes(sys.argv[1]); b = sizes(sys.argv[2])
pa = prof(sys.argv[3]) if len(sys.argv) > 3 else {}
pb = prof(sys.argv[4]) if len(sys.argv) > 4 else {}
lim = 500
rows = []
for k in set(a) & set(b):
    la, lb = min(a[k]), min(b[k])
    if la <= lim < lb:
        rows.append((pb.get(k, 0), pa.get(k, 0), la, lb, k))
print(f"names in both: {len(set(a)&set(b))}; LLVM <=500 B but ours >500 B: {len(rows)}")
print(f"ours >500 B, LLVM <=500: self ms ours {sum(r[0] for r in rows):.1f}, llvm {sum(r[1] for r in rows):.1f}")
for r in sorted(rows, reverse=True)[:int(sys.argv[5]) if len(sys.argv) > 5 else 30]:
    print(f"{r[0]:8.1f} {r[1]:8.1f} {r[2]:6d} {r[3]:6d}  {r[4][:120]}")
