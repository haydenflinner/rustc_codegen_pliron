# hotfn.py DIR FUNC [TOP] — ticks per machine instruction of one function's
# TurboFan code. DIR holds `v8.log` (node --prof) and `code.txt` (node
# --print-wasm-code); FUNC is the end of the demangled function name with
# crate hashes removed. Prints the function's tick total, the share per
# mnemonic, the share on instructions that touch the frame (fp/sp), and
# the TOP hottest instruction runs with their listing. See hot.sh.
import sys,re,collections
d=sys.argv[1]; want=sys.argv[2]; top=int(sys.argv[3]) if len(sys.argv)>3 else 6
name=''
sec=[];cur=None
for l in open(d+'/code.txt',errors='replace'):
    if l.startswith('name:'): name=re.sub(r'\[[0-9a-f]{8,}\]','',l[5:].strip()); continue
    if l.startswith('compiler:'):
        cur={'ins':[],'name':name} if ('TurboFan' in l and name.endswith(want)) else None
        if cur is not None: sec.append(cur)
        continue
    if cur is None: continue
    m=re.match(r'^(0x[0-9a-f]+)\s+([0-9a-f]+)\s+[0-9a-f]+\s+(\S+)\s*(.*)',l)
    if m: cur['ins'].append((int(m.group(1),16),int(m.group(2),16),m.group(3),m.group(4)))
secs=[x for x in sec if x['ins']]
for x in secs: x['base']=x['ins'][0][0]-x['ins'][0][1]; x['end']=x['ins'][-1][0]+4; x['t']=collections.Counter()
n=0;tot=0
for l in open(d+'/v8.log',errors='replace'):
    if l.startswith('tick,'):
        tot+=1; pc=int(l.split(',')[1],16)
        for x in secs:
            if x['base']<=pc<x['end']: x['t'][pc-x['base']]+=1; n+=1; break
if not secs: sys.exit(f'no TurboFan code for a function ending in {want!r} (not tiered up, or the name differs)')
x=max(secs,key=lambda x:sum(x['t'].values()))
t=x['t']; ft=sum(t.values()); ins=x['ins']
print(f"ticks in fn {ft} of {tot} ({100*ft/tot:.1f}%), {len(ins)} machine insts, {len(secs)} turbofan copies")
mn=collections.Counter()
for a,o,m,args in ins: mn[m]+=t.get(o,0)
print('by mnemonic:',' '.join(f"{m}:{100*c/ft:.1f}%" for m,c in mn.most_common(14)))
st=sum(c for (a,o,m,args) in ins for c in [t.get(o,0)] if re.search(r'\[(fp|sp)',args))
print(f"insts touching fp/sp frame: {100*st/ft:.1f}% of ticks")
# hot regions: merge instructions with ticks within 6 insts
idx=[i for i,(a,o,m,args) in enumerate(ins) if t.get(o,0)]
regs=[]
for i in idx:
    if regs and i-regs[-1][1]<=6: regs[-1][1]=i
    else: regs.append([i,i])
regs.sort(key=lambda r:-sum(t.get(ins[i][1],0) for i in range(r[0],r[1]+1)))
for r in regs[:top]:
    rt=sum(t.get(ins[i][1],0) for i in range(r[0],r[1]+1))
    print(f"--- region {ins[r[0]][1]:#x}..{ins[r[1]][1]:#x}: {100*rt/ft:.1f}% of fn ticks, {r[1]-r[0]+1} insts")
    for i in range(max(0,r[0]-2),min(len(ins),r[1]+2)):
        a,o,m,args=ins[i]; print(f"   {t.get(o,0):5d} {o:6x} {m} {args[:70]}")
