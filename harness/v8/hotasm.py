# hotasm.py — used by hot.sh; v8.log code.txt [min_pct] -> TurboFan listing with per-instruction tick counts
import sys,re,collections
log,code=sys.argv[1],sys.argv[2]; minp=float(sys.argv[3]) if len(sys.argv)>3 else 0.5
ins=[]; tf=False
for l in open(code,errors='replace'):
    if l.startswith('compiler:'): tf = 'TurboFan' in l
    m=re.match(r'^(0x[0-9a-f]+)\s+[0-9a-f]+\s+[0-9a-f]+\s+(.*)$',l)
    if tf and m: ins.append((int(m.group(1),16),m.group(2).strip()))
if not ins: sys.exit("no TurboFan code in "+code)
lo,hi=ins[0][0],ins[-1][0]+4
ticks=collections.Counter(); total=0
for l in open(log,errors='replace'):
    if l.startswith('tick,'):
        total+=1; pc=int(l.split(',')[1],16)
        if lo<=pc<hi: ticks[pc]+=1
n=sum(ticks.values())
print(f"function ticks {n} / {total} total ({100*n/max(total,1):.1f}%)")
hot=[i for i,(a,_) in enumerate(ins) if 100*ticks[a]/max(n,1)>=minp]
show=set()
for i in hot:
    for j in range(max(0,i-3),min(len(ins),i+3)): show.add(j)
prev=-2
for i in sorted(show):
    if i!=prev+1: print('   ...')
    a,t=ins[i]; c=ticks[a]
    print(f"{100*c/max(n,1):5.1f}% {a-lo:5x}  {t}")
    prev=i
