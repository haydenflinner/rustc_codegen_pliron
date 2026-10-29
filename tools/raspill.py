#!/usr/bin/env python3
# raspill.py — reconstruct which vregs regalloc2 spilled, from a
# PLIRON_RA2_EDITS dump: simulates vreg->location transitions across
# progpoints (args seeded from inst0, operand allocs pin state).
# Usage: raspill.py VREGMAP.txt RA2EDITS.txt  (but defaults below).
import sys
if len(sys.argv)>2: import io; sys.exit("hardcoded paths")
import re, collections

# x64 gpr enc: pNi index == modrm enc
GPR = {'rax':0,'rcx':1,'rdx':2,'rbx':3,'rsp':4,'rbp':5,'rsi':6,'rdi':7,
       'r8':8,'r9':9,'r10':10,'r11':11,'r12':12,'r13':13,'r14':14,'r15':15}

vcode2clif = {}
for line in open('/tmp/vregmap.txt'):
    line = line.strip()
    if not line: continue
    cl, regs = line.split(':')
    for r in regs.split():
        vcode2clif[r.strip()] = cl.strip()

# seed arg locs (from prevcode inst0)
loc = {'v192':'p7i','v193':'p6i','v194':'p2i','v195':'p1i','v196':'p8i','v197':'p9i'}
loc_of_preg = {l:v for v,l in loc.items()}
loc_of_stack = {}
spill_to_stack = collections.defaultdict(set)
moves = 0; unattributed = 0

for line in open('/tmp/ra2edits.txt'):
    m = re.match(r"\s*progpoint(\d+)-(pre|post) \[blkSome\((\d+)\) inst(\d+) <(.*)>\]: Move \{ from: (\w+), to: (\w+) \}", line)
    if not m:
        continue
    pp, pos, blk, inst, ctx, frm, to = m.groups()
    moves += 1
    # operand observations pin locs for THIS inst (post-move state)
    obs = re.findall(r"v(\d+)=(p\d+i|stack\d+)", ctx)
    for v, a in obs:
        v = 'v'+v
        loc[v] = a
        if a.startswith('p'): loc_of_preg[a] = v
        else: loc_of_stack[a] = v
    owner = loc_of_preg.get(frm) if frm.startswith('p') else loc_of_stack.get(frm)
    if owner is None:
        cands = [v for v,l in loc.items() if l == frm]
        owner = cands[0] if len(cands) == 1 else None
    if owner is None:
        unattributed += 1
        continue
    if frm.startswith('p'): loc_of_preg.pop(frm, None)
    else: loc_of_stack.pop(frm, None)
    loc[owner] = to
    if to.startswith('stack'):
        loc_of_stack[to] = owner
        spill_to_stack[owner].add(to)
    else:
        loc_of_preg[to] = owner

print(f"moves={moves} unattributed={unattributed}")
print("vregs ever moved to stack:", len(spill_to_stack))
for v in sorted(spill_to_stack, key=lambda x:int(x[1:])):
    print(f"  {v} (clif {vcode2clif.get(v,'?')}): {sorted(spill_to_stack[v], key=lambda s:int(s[5:]))}")
final_stack = {v:l for v,l in loc.items() if l.startswith('stack')}
print("ending in stack:", len(final_stack), sorted(final_stack.items(), key=lambda x:int(x[0][1:])))
print("stack slots:", sorted({s for v in spill_to_stack.values() for s in v}, key=lambda x:int(x[5:])))
