import sys,re
lines=open(sys.argv[1]).read().split('\n')
ind=lambda s: len(s)-len(s.lstrip())
out=[]
for i,l in enumerate(lines):
    if l.strip().startswith('loop'):
        d=ind(l); j=i+1
        while j<len(lines) and not (ind(lines[j])==d and lines[j].strip()=='end'): j+=1
        body=[x.strip() for x in lines[i+1:j]]
        if any(x.startswith('loop') for x in body): continue
        ops=[x.split()[0] for x in body if x]
        nl=[o for o in ops if not o.startswith('local.') and o not in('end','block','else','if','br','br_if','loop')]
        out.append((i+1,len(ops),len(nl),' '.join(o for o in nl)))
for o in out: print(o[0],'ops=%d nonlocal=%d |'%(o[1],o[2]),o[3][:400])
