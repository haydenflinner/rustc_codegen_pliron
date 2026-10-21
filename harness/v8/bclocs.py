# bclocs.py mod.wasm mod.wat func-substr [callee-substr]
# For each call to callee inside func, decode its trailing &Location arg.
import sys,re,struct
wasm=open(sys.argv[1],'rb').read(); wat=open(sys.argv[2]).read().split('\n')
fsub=sys.argv[3]; csub=sys.argv[4] if len(sys.argv)>4 else 'panic_bounds_check'
def leb(b,i,signed=False):
    r=s=0
    while True:
        x=b[i]; i+=1; r|=(x&0x7f)<<s; s+=7
        if x<0x80: break
    if signed and x&0x40: r-=1<<s
    return r,i
i=8; globs=[]; data=[]
while i<len(wasm):
    sid=wasm[i]; i+=1; n,i=leb(wasm,i); body=wasm[i:i+n]; i+=n
    if sid==6:
        j=0; cnt,j=leb(body,j)
        for _ in range(cnt):
            j+=2  # type, mut
            op=body[j]; j+=1
            if op==0x41: v,j=leb(body,j,True); globs.append(v&0xffffffff)
            elif op==0x42: v,j=leb(body,j,True); globs.append(v)
            else: globs.append(None); 
            while body[j]!=0x0b: j+=1
            j+=1
    if sid==11:
        j=0; cnt,j=leb(body,j)
        for _ in range(cnt):
            fl,j=leb(body,j)
            if fl==0:
                assert body[j]==0x41; j+=1; off,j=leb(body,j,True); assert body[j]==0x0b; j+=1
            elif fl==1: off=None
            else: _,j=leb(body,j); j+=1; off,j=leb(body,j,True); j+=1
            ln,j=leb(body,j); data.append((off,body[j:j+ln])); j+=ln
def mem(a,n):
    for off,b in data:
        if off is not None and off<=a and a+n<=off+len(b): return b[a-off:a-off+n]
    return None
inf=False; out=[]
for k,l in enumerate(wat):
    if l.startswith('  (func '): inf = fsub in l
    if inf and 'call $' in l and csub in l:
        m=re.match(r'(global\.get|i32\.const) (-?\d+)',wat[k-1].strip())
        if not m or m.group(1)=='i32.const' and int(m.group(2))<4096:
            # loc may come via a local; take the nearest GOT read
            m=None
            for b in range(1,10):
                m=re.match(r'(global\.get) (\d+)',wat[k-b].strip())
                if m: break
        if not m: out.append((k+1,'?')); continue
        a=globs[int(m.group(2))] if m.group(1)=='global.get' else int(m.group(2))&0xffffffff
        rec=mem(a,16)
        if not rec: out.append((k+1,'addr %x?'%a)); continue
        fp,fl,line,col=struct.unpack('<IIII',rec)
        f=mem(fp,fl); out.append((k+1,'%s:%d:%d'%(f.decode(errors='replace').split('/')[-1] if f else '?',line,col)))
import collections
c=collections.Counter(x for _,x in out)
for loc,n in sorted(c.items()): print(f"{n:3d} {loc}")
