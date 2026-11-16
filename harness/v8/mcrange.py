import sys,re,collections
# mcrange.py code.txt lo hi : print machine insts of the largest TurboFan section in [lo,hi]
sec=[];cur=None
for l in open(sys.argv[1],errors='replace'):
    if l.startswith('compiler:'):
        cur=[] if 'TurboFan' in l else None
        if cur is not None: sec.append(cur)
        continue
    if cur is None: continue
    m=re.match(r'^(0x[0-9a-f]+)\s+([0-9a-f]+)\s+[0-9a-f]+\s+(\S+)\s*(.*)',l)
    if m: cur.append((int(m.group(2),16),m.group(3),m.group(4)))
grp=collections.defaultdict(list)
for x in sec: grp[len(x)].append(x)
n=int(sys.argv[4]) if len(sys.argv)>4 else max(grp)
s=grp[n][0]
lo,hi=int(sys.argv[2],16),int(sys.argv[3],16)
for o,op,a in s:
    if lo<=o<=hi: print(f"{o:#06x} {op:6s} {a}")
