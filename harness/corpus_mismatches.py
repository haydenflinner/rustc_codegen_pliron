# corpus_mismatches.py report.json: one block per mismatch/panic of a
# `clif-check report` — what differs (return, calls, bytes written, a
# trap) and the first differing writes. Two-byte write diffs inside an
# aggregate and 0x7f/0xff top bytes (a NaN sign) are the usual noise.
import json,re,sys
d=json.load(open(sys.argv[1]))
for f in d['functions']:
    if f['status'] not in ('mismatch','panic','verifier'): continue
    m=f['msg']
    print(f['status'], f['file'][:120])
    if f['status']!='mismatch' or '|   before:' not in m:
        print('   ',m[:300]); continue
    head,rest=m.split('|   before:',1)
    b,a=rest.split('|   after:',1)
    def parts(s):
        calls=re.findall(r'\("u0:\d+", \[[^\]]*\]\)',s)
        wr=dict(re.findall(r'\(([0-9a-f]+), ([0-9a-f]+)\)',s.split('bytes written')[1])) if 'bytes written' in s else {}
        return s.strip().split(', calls')[0].split(', ')[0] if False else s.strip()[:s.strip().find(']')+1], calls, wr
    rb,cb,wb=parts(b); ra,ca,wa=parts(a)
    ks=sorted(set(wb)|set(wa)); diff=[(k,wb.get(k),wa.get(k)) for k in ks if wb.get(k)!=wa.get(k)]
    kind=[]
    if rb!=ra: kind.append('RET')
    if cb!=ca: kind.append('CALLS')
    if diff: kind.append(f'WRITES({len(diff)})')
    if 'trapped' in a[:60] or 'trapped' in b[:60]: kind.append('TRAP')
    print('   ',head.strip()[:100],'=>',' '.join(kind))
    print('      before',b.strip()[:150]); print('      after ',a.strip()[:150])
    if diff: print('      writes',diff[:8])
