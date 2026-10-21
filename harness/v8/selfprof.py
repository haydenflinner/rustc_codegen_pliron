# selfprof.py file.cpuprofile [top] -> self-sample share per function
import json,sys,collections
d=json.load(open(sys.argv[1])); top=int(sys.argv[2]) if len(sys.argv)>2 else 15
nodes={n['id']:n for n in d['nodes']}
cnt=collections.Counter(d['samples'])
self_=collections.Counter()
for nid,c in cnt.items():
    fn=nodes[nid]['callFrame']['functionName'] or '(anon)'
    self_[fn]+=c
tot=sum(self_.values())
import re
for fn,c in self_.most_common(top):
    short=re.sub(r'^_R[A-Za-z]*?(Cs[A-Za-z0-9_]+_)?','',fn)[-70:]
    print(f"{100*c/tot:5.1f}% {c:6d} {short}")
