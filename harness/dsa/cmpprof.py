# cmpprof.py LLVM.cpuprofile PLIRON.cpuprofile [TOP] — the two prof.sh profiles of
# one workload joined by function name (crate hashes removed): self ms on each
# side, sorted by how much more the second spends. Globs are accepted:
#   python3 harness/dsa/cmpprof.py '/tmp/dsa-bench/prof/W-llvm/*.cpuprofile' '/tmp/dsa-bench/prof/W-pliron/*.cpuprofile'
# Inlining differs between the builds, so read families (caller + callee), not single rows.
import json,sys,re,collections,glob
def load(p):
    d=json.load(open(glob.glob(p)[0])); t=collections.Counter(); dt=d['timeDeltas']; ids={n['id']:n for n in d['nodes']}
    for s,x in zip(d['samples'],dt):
        n=ids[s]; f=n['callFrame']; nm=f['functionName'] or '(anon)'
        nm=re.sub(r'\[[0-9a-f]{8,}\]','',nm); nm=re.sub(r'\[\d+\]$','',nm)
        t[nm]+=x/1000
    return t
a=load(sys.argv[1]); b=load(sys.argv[2]); top=int(sys.argv[3]) if len(sys.argv)>3 else 25
print(f"total llvm {sum(a.values()):.0f} ms pliron {sum(b.values()):.0f} ms")
ks=sorted(set(a)|set(b), key=lambda k: -(b.get(k,0)-a.get(k,0)))
for k in ks[:top]: print(f"{a.get(k,0):8.1f} {b.get(k,0):8.1f} {b.get(k,0)-a.get(k,0):+8.1f}  {k[:110]}")
print('  ... most negative:')
for k in ks[-8:]: print(f"{a.get(k,0):8.1f} {b.get(k,0):8.1f} {b.get(k,0)-a.get(k,0):+8.1f}  {k[:110]}")
