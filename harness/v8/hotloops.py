# hotloops.py v8.log code.txt od.txt funcidx [top]
# Ticks per machine instruction of the function's TurboFan code, grouped by
# innermost backward-branch loop; each loop lists size, tick share, the
# wasm offsets of the source positions inside it and the wasm ops there.
import sys,re,collections,bisect
log,code,od,fidx=sys.argv[1:5]; top=int(sys.argv[5]) if len(sys.argv)>5 else 14
sec=[];cur=None;mode=None
for l in open(code,errors='replace'):
    if l.startswith('compiler:'):
        cur={'ins':[],'pos':[]} if 'TurboFan' in l else None
        if cur is not None: sec.append(cur)
        mode=None; continue
    if cur is None: continue
    m=re.match(r'^(0x[0-9a-f]+)\s+([0-9a-f]+)\s+[0-9a-f]+\s+(\S+)\s*(.*)',l)
    if m: cur['ins'].append((int(m.group(1),16),int(m.group(2),16),m.group(3),m.group(4))); continue
    if l.startswith('Source positions'): mode='pos'; continue
    if mode=='pos':
        m=re.match(r'^\s+([0-9a-f]+)\s+(\d+)',l)
        if m: cur['pos'].append((int(m.group(1),16),int(m.group(2))))
        elif l.strip()=='': mode=None
secs=[x for x in sec if x['ins']]
for x in secs:
    x['base']=x['ins'][0][0]-x['ins'][0][1]; x['end']=x['ins'][-1][0]+4; x['t']=collections.Counter()
n=0
for l in open(log,errors='replace'):
    if l.startswith('tick,'):
        pc=int(l.split(',')[1],16)
        for x in secs:
            if x['base']<=pc<x['end']: x['t'][pc-x['base']]+=1; n+=1; break
# identical compiles of the function (one per fresh instance): sum them
grp=collections.defaultdict(list)
for x in secs: grp[len(x['ins'])].append(x)
best=max(grp.values(),key=lambda g:sum(sum(x['t'].values()) for x in g))
s=best[0]; t=collections.Counter()
for x in best: t.update(x['t'])
# wasm insts
lines=open(od).read().split('\n'); fstart=None; wins=[]
for l in lines:
    m=re.match(r'^([0-9a-f]+) func\[(\d+)\]',l)
    if m:
        if fstart is not None and wins: break
        fstart=int(m.group(1),16) if m.group(2)==fidx else None; continue
    if fstart is not None:
        m=re.match(r'^ ([0-9a-f]+): [0-9a-f ]+\| (.*)$',l)
        if m: wins.append((int(m.group(1),16)-fstart,m.group(2).strip()))
woffs=[o for o,_ in wins]
def wop(p):
    i=bisect.bisect_right(woffs,p)-1
    return wins[i][1] if i>=0 else '?'
offs=[i[1] for i in s['ins']]
loops=[]
for a,off,op,args in s['ins']:
    m=re.search(r'#([+-])0x([0-9a-f]+) \(addr',args)
    if m and m.group(1)=='-' and off-int(m.group(2),16)>=0:
        loops.append((off-int(m.group(2),16),off))
# innermost: loops containing no other loop
inner=[L for L in loops if not any(M!=L and L[0]<=M[0] and M[1]<=L[1] for M in loops)]
pos=sorted(s['pos'])
tot=sum(t.values())
res=[]
cov=0
for lo,hi in inner:
    tk=sum(c for o,c in t.items() if lo<=o<=hi)
    ni=sum(1 for o in offs if lo<=o<=hi)
    ps=sorted({p for o,p in pos if lo<=o<=hi})
    res.append((tk,lo,hi,ni,ps)); cov+=tk
print(f"ticks in fn {tot}; code insts {len(offs)}; innermost loops {len(inner)}; ticks in innermost loops {100*cov/max(tot,1):.1f}%")
for tk,lo,hi,ni,ps in sorted(res,reverse=True)[:top]:
    ops=collections.Counter(wop(p).split()[0] for p in ps)
    print(f"{100*tk/max(tot,1):5.1f}% mc {lo:#06x}..{hi:#06x} insts={ni:3d} wasm {ps[0] if ps else -1}..{ps[-1] if ps else -1} ops: "+' '.join(f"{k}x{v}" for k,v in ops.most_common(8)))

# top single instructions and 32-inst buckets
print("-- hottest machine instructions")
byoff={i[1]:i for i in s['ins']}
for o,c in t.most_common(28):
    i=byoff.get(o)
    # nearest preceding source position in machine order
    k=bisect.bisect_right([q for q,_ in pos],o)-1
    wp=pos[k][1] if k>=0 else -1
    print(f"{100*c/max(tot,1):5.1f}% {o:#06x} {i[2] if i else '?':6s} {i[3][:38] if i else '':38s} wasm~{wp} {wop(wp)}")
