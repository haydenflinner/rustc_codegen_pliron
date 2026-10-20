#!/usr/bin/env -S python3 -I
"""sprof.py PROFILE.json.gz [--self N] [--incl N] [--under SUBSTR] [--thread SUBSTR]

Text report for a samply profile — no browser, no `sample`:

    samply record --save-only --unstable-presymbolicate -r 1000 -o p.json.gz -- \
        rustc -O ... -Zcodegen-backend=target/release/librustc_codegen_pliron.dylib
    harness/sprof.py p.json.gz --under pliron_ir_opt::sroa::run

Symbolizes through the .syms.json sidecar `--unstable-presymbolicate` writes.
Prints self time, inclusive time (each symbol once per stack), and with
--under, the inclusive time of callees directly below frames matching SUBSTR.
`/usr/bin/sample` (harness/prof.py) can wedge system-wide after a disk-full
episode; samply follows the rustup shim into the real rustc."""
import bisect, collections, gzip, json, sys


def main():
    a = sys.argv[1:]
    path = a[0]
    opt = {"--self": "30", "--incl": "40", "--under": None, "--thread": None}
    i = 1
    while i < len(a):
        opt[a[i]] = a[i + 1]
        i += 2
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
        if not t:
            return f"{libs[lib]['name'] if lib is not None else '?'}+{addr:#x}"
        k = bisect.bisect_right(t[0], addr) - 1
        if k >= 0:
            rva, size, sym = t[1][k]
            if addr < rva + max(size, 1):
                return st[sym]
        return f"{libs[lib]['name']}+{addr:#x}"

    selfc, incl = collections.Counter(), collections.Counter()
    under = collections.Counter()
    total = 0
    for t in p["threads"]:
        if opt["--thread"] and opt["--thread"] not in (t.get("name") or ""):
            continue
        ft, fu, rt, sk = t["frameTable"], t["funcTable"], t["resourceTable"], t["stackTable"]
        fname = {}

        def frame_name(f):
            if f not in fname:
                fn = ft["func"][f]
                res = fu["resource"][fn]
                lib = rt["lib"][res] if res is not None and res >= 0 else None
                fname[f] = name(lib, ft["address"][f])
            return fname[f]

        cache = {}

        def chain(s):
            if s in cache:
                return cache[s]
            out = []
            x = s
            while x is not None:
                out.append(frame_name(sk["frame"][x]))
                x = sk["prefix"][x]
            cache[s] = out  # leaf first
            return out

        for s in t["samples"]["stack"]:
            if s is None:
                continue
            c = chain(s)
            total += 1
            selfc[c[0]] += 1
            for n in set(c):
                incl[n] += 1
            if opt["--under"]:
                for j in range(1, len(c)):
                    if opt["--under"] in c[j]:
                        under[c[j - 1]] += 1
                        break
    print(f"total samples {total}")
    print("--- self")
    for k, v in selfc.most_common(int(opt["--self"])):
        print(f"{v:6d} {100*v/total:5.1f}%  {k[:150]}")
    print("--- inclusive")
    for k, v in incl.most_common(int(opt["--incl"])):
        print(f"{v:6d} {100*v/total:5.1f}%  {k[:150]}")
    if opt["--under"]:
        print(f"--- directly under *{opt['--under']}*")
        for k, v in under.most_common(30):
            print(f"{v:6d} {100*v/total:5.1f}%  {k[:150]}")


main()
