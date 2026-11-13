#!/usr/bin/env -S python3 -I
"""How many more pipeline-cache hits would callee-renaming give?

  harness/memo_census.py target/corpus/std

`PLIRON_MEMCACHE` keys a function on its body with callees named by symbol.
relax keys its back-end memo on the body with callee ids replaced by their
index of first occurrence, so `Vec<A>::reserve` and `Vec<B>::reserve` share
one result. This counts, over a recorded corpus, the duplicate bodies under
each identity, by functions and by body lines (a proxy for pipeline time).
std corpus, 2026-11-12: by symbol 11.6% of fns / 3.6% of lines; by position
16.1% / 4.5%; data symbols by position too 20.9% / 5.8%. Too little for the
per-callee fact keying it would need.
"""
import sys, os, re, hashlib, collections
d = sys.argv[1]
SPECIAL = ("_Unwind_Resume", "memcmp", "bcmp", "memcpy", "memmove", "memset")
tot = 0; insts = 0
keys = {k: collections.Counter() for k in "ABC"}
size = {}
for fn in os.listdir(d):
    if not fn.endswith(".clif"): continue
    txt = open(os.path.join(d, fn), errors="replace").read()
    head, _, body = txt.partition("\nfunction ")
    names = {}
    m = re.search(r"^; diffcheck-names: (.*)$", head, re.M)
    if m:
        for kv in m.group(1).split(","):
            if ":" in kv:
                k, v = kv.split(":", 1); names[k] = v
    facts = "\n".join(l for l in head.split("\n") if not l.startswith("; diffcheck-names"))
    facts = re.sub(r" self=\S+", "", facts)
    n = body.count("\n")
    # A: callee identity = symbol name (what the cache keys on today)
    a = re.sub(r"^(\s*)(fn\d+) = (colocated )?u0:\d+", lambda g: f"{g.group(1)}{g.group(2)} = {names.get(g.group(2), '?')}", body, flags=re.M)
    # B: callee identity = position only, except names passes special-case
    b = re.sub(r"^(\s*)(fn\d+) = (colocated )?u0:\d+", lambda g: f"{g.group(1)}{g.group(2)} = {names.get(g.group(2), '?') if names.get(g.group(2)) in SPECIAL else '#'}", body, flags=re.M)
    # C: also data symbols by position
    c = re.sub(r"u1:\d+", "u1:#", b)
    a = re.sub(r"^u0:\d+", "", a); b = re.sub(r"^u0:\d+", "", b); c = re.sub(r"^u0:\d+", "", c)
    for k, t in (("A", a), ("B", b), ("C", c)):
        h = hashlib.sha1((facts + t).encode()).digest()
        keys[k][h] += 1
        size[h] = n
    tot += 1; insts += n
print(f"{tot} functions, {insts} body lines")
for k, label in (("A", "callee by symbol (today)"), ("B", "callee by position"), ("C", "callee + data symbols by position")):
    c = keys[k]
    dup_fns = tot - len(c)
    dup_lines = sum((cnt - 1) * size[h] for h, cnt in c.items())
    print(f"  {label:<36} hits {dup_fns:5d} ({100*dup_fns/tot:4.1f}% of fns, {100*dup_lines/insts:4.1f}% of lines)")
