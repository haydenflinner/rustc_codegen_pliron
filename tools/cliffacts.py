#!/usr/bin/env python3
"""cliffacts.py — extract register-allocation facts from a PLIRON_CLIF dump.

Reads a `function uN:N ...` CLIF body (the final dump section) and emits
mimas fact rows for the spill-solver datalog:

    def(v, i)      vreg v defined at instruction index i
    use(v, i)      vreg v used at instruction index i
    param(v, b)    v is a parameter of block b
    succ(b, c)     CFG edge b -> c
    cost(v, w)     estimated dynamic cost of spilling v
                   (defs+uses weighted by block loop-depth estimate)

Usage: cliffacts.py FILE [HEADER_LINE]   -- extracts the function whose
`====` header is at/after HEADER_LINE, or the last function in the file.
"""
import re
import sys

V = re.compile(r"\bv(\d+)\b")


def parse(fn_text):
    blocks = {}          # name -> list of (defs, uses, succs)
    params = {}          # name -> [v]
    order = []
    cur = None
    for line in fn_text.splitlines():
        m = re.match(r"^block(\d+)(?:\(([^)]*)\))?( cold)?:", line.strip())
        if m:
            cur = m.group(1)
            blocks[cur] = []
            params[cur] = [int(v) for v in V.findall(m.group(2) or "")]
            order.append(cur)
            continue
        if cur is None:
            continue
        line = line.strip()
        if not line or line.startswith(";"):
            continue
        defs = []
        m = re.match(r"^((?:v\d+,\s*)*v\d+)\s*=", line)
        if m:
            defs = [int(v) for v in V.findall(m.group(1))]
            rest = line[m.end():]
        else:
            rest = line
        m = re.match(r"^(v\d+)\s*->", line)   # v_dst -> v_src alias annotation
        if m:
            defs = [int(m.group(1)[1:])]
            rest = line[m.end():]
        uses = [int(v) for v in V.findall(rest)]
        succs = [int(b) for b in re.findall(r"block(\d+)", rest)]
        blocks[cur].append((defs, uses, succs))
    return order, blocks, params


def loop_weight(order, succ_edges):
    """weight = 10**depth via simple back-edge nesting estimate."""
    # natural loop detection: edge u->v where v reaches u back
    reach = {b: set() for b in order}
    changed = True
    while changed:
        changed = False
        for b in order:
            for s in succ_edges.get(b, ()):  # noqa
                for w in reach.get(s, ()) | ({s} if s in reach else set()):
                    if w not in reach[b]:
                        reach[b].add(w)
                        changed = True
    backedges = set()
    for b in order:
        for s in succ_edges.get(b, ()):
            if b in reach.get(s, ()) or s == b:
                backedges.add((b, s))
    depth = {b: 0 for b in order}
    for u, v in backedges:  # loop body = nodes reaching u (the back-edge source)
        for b in order:
            if u in reach[b] or b == u:
                depth[b] += 1
    return {b: 10 ** depth[b] for b in order}


def main():
    path = sys.argv[1]
    lines = open(path).read().splitlines()
    starts = [i for i, l in enumerate(lines) if l.startswith("==== clif")]
    if len(sys.argv) > 2:
        hdr = int(sys.argv[2])
        i = next(j for j, s in enumerate(starts) if s >= hdr)
    else:
        i = starts[-1]
    end = starts[i + 1] if i + 1 < len(starts) else len(lines)
    fn_text = "\n".join(lines[starts[i] + 1:end])
    order, blocks, params = parse(fn_text)
    succ_edges = {}
    inst = 0
    defs, uses, succs, prm, wtab = [], [], [], [], {}
    for b in order:
        sset = set()
        for d, u, s in blocks[b]:
            sset.update(s)
            inst += 1
        succ_edges[int(b)] = sset
    weights = loop_weight(order, succ_edges)
    cost = {}
    i = 0
    for b in order:
        for d, u, s in blocks[b]:
            for v in d:
                defs.append((v, i))
                cost[v] = cost.get(v, 0) + weights[b]
            for v in u:
                uses.append((v, i))
                cost[v] = cost.get(v, 0) + weights[b]
            i += 1
        for v in params[b]:
            prm.append((v, int(b)))
    for b, ss in succ_edges.items():
        for s in ss:
            succs.append((b, s))
    # block-level liveness summaries: buse = upward-exposed use (used
    # before any def in the block), bdef = defined anywhere in the block
    # (params count as defs at the top).
    buse, bdef = set(), set()
    for b in order:
        defined = set(params[b])
        for v in params[b]:
            bdef.add((v, int(b)))
        for d, u, _s in blocks[b]:
            for v in u:
                if v not in defined:
                    buse.add((v, int(b)))
            defined.update(d)
            for v in d:
                bdef.add((v, int(b)))
    print(f"// {len(order)} blocks, {i} insts, {len(cost)} vregs")
    print("let blk_rows = [")
    for b in sorted(order, key=int):
        print(f"    ({b},),")
    print("];")
    print("let buse_rows = [")
    for v, b in sorted(buse):
        print(f"    ({v}, {b}),")
    print("];")
    print("let bdef_rows = [")
    for v, b in sorted(bdef):
        print(f"    ({v}, {b}),")
    print("];")
    print("let def_rows = [")
    for v, i in sorted(set(defs)):
        print(f"    ({v}, {i}),")
    print("];")
    print("let use_rows = [")
    for v, i in sorted(set(uses)):
        print(f"    ({v}, {i}),")
    print("];")
    print("let param_rows = [")
    for v, b in sorted(set(prm)):
        print(f"    ({v}, {b}),")
    print("];")
    print("let succ_rows = [")
    for b, s in sorted(set(succs)):
        print(f"    ({b}, {s}),")
    print("];")
    print("let cost_rows = [")
    for v in sorted(cost):
        print(f"    ({v}, {cost[v]}),")
    print("];")


if __name__ == "__main__":
    main()
