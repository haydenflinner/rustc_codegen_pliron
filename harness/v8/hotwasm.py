# hotwasm.py v8.log code.txt objdump.txt funcidx [top]
# Ticks in the function's TurboFan code -> nearest preceding source position
# (function-relative wasm byte offset) -> wasm instruction, aggregated.
import sys,re,collections,bisect
log,code,od,fidx=sys.argv[1:5]; top=int(sys.argv[5]) if len(sys.argv)>5 else 40
sec=[];tf=False;cur=None
for l in open(code,errors='replace'):
    if l.startswith('compiler:'): tf='TurboFan' in l; cur={'ins':[],'pos':[]} if tf else None; tf and sec.append(cur); mode=None; continue
    if cur is None: continue
    m=re.match(r'^(0x[0-9a-f]+)\s+([0-9a-f]+)\s+[0-9a-f]+\s+',l)
    if m: cur['ins'].append((int(m.group(1),16),int(m.group(2),16))); continue
    if l.startswith('Source positions'): mode='pos'; continue
    if mode=='pos':
        m=re.match(r'^\s+([0-9a-f]+)\s+(\d+)',l)
        if m: cur['pos'].append((int(m.group(1),16),int(m.group(2))))
        elif l.strip()=='' : mode=None
for s in sec:
    s['base']=s['ins'][0][0]-s['ins'][0][1]; s['end']=s['ins'][-1][0]+4
    s['pos']=sorted(s['pos']); s['pcs']=[p for p,_ in s['pos']]
ticks=collections.Counter(); n=0; tot=0
for l in open(log,errors='replace'):
    if l.startswith('tick,'):
        tot+=1; pc=int(l.split(',')[1],16)
        for s in sec:
            if s['base']<=pc<s['end']:
                off=pc-s['base']; n+=1
                i=bisect.bisect_right(s['pcs'],off)-1
                ticks[s['pos'][i][1] if i>=0 else -1]+=1
                break
pos=[x for s in sec for x in s['pos']]
# objdump: function body lines
lines=open(od).read().split('\n'); fstart=None; ins=[]
for l in lines:
    m=re.match(r'^([0-9a-f]+) func\[(\d+)\]',l)
    if m: 
        if fstart is not None and ins: break
        fstart=int(m.group(1),16) if m.group(2)==fidx else None; continue
    if fstart is not None:
        m=re.match(r'^ ([0-9a-f]+): [0-9a-f ]+\| (.*)$',l)
        if m: ins.append((int(m.group(1),16)-fstart,m.group(2).strip()))
offs=[o for o,_ in ins]
print(f"ticks {n}/{tot}; positions {len(pos)}; wasm insts {len(ins)}")
def near(p):
    i=bisect.bisect_right(offs,p)-1
    return ins[i] if i>=0 else (p,'?')
agg=collections.Counter()
for p,c in ticks.items(): agg[p]+=c
for p,c in agg.most_common(top):
    o,t=near(p)
    print(f"{100*c/max(n,1):5.1f}% pos {p:5d} -> {o:5d} {t}")
