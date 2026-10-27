#!/usr/bin/env -S python3 -I
"""Stack-frame bytes per function, LLVM vs pliron, for the parity card.

  harness/frames.py [--json OUT] [--input regex-syntax]

Builds the input (harness/ctime.py's command) to one object per side at -O
with one CGU, reads each function's frame from its prologue (`sub sp`
amounts, including the stack-probe form `mov x16, #n; sub sp, sp, x16`
and probe loops that add the pages back, plus callee-save pushes
(`stp ..., [sp, #-n]!`; `sub $n, %rsp` and `push` on x86-64)), and
compares the totals over the functions both sides define. The ratio,
total pliron / total LLVM, is the parity card's STK input. Writes
target/parity/stack.json.
"""
import argparse, importlib.util, json, os, re, shutil, subprocess, sys, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OD = shutil.which("llvm-objdump") or "/opt/homebrew/opt/llvm/bin/llvm-objdump"


def frames(obj):
    """Symbol -> frame bytes from its `sub sp` prologue."""
    txt = subprocess.run([OD, "-d", "--no-show-raw-insn", obj], capture_output=True, text=True).stdout
    out, cur, tot, x16, seen = {}, None, 0, 0, 0
    for line in txt.splitlines():
        m = re.match(r"^[0-9a-f]+ <(.+)>:$", line)
        if m:
            if cur is not None:
                out[cur] = tot
            cur, tot, x16, seen = m.group(1), 0, 0, 0
            continue
        if cur is None or seen > 24:  # the prologue only
            continue
        seen += 1
        if m := re.search(r"mov\s+x16, #0x([0-9a-f]+)", line):
            x16 = int(m.group(1), 16)
        elif m := re.search(r"sub\s+sp, sp, #0x([0-9a-f]+)(, lsl #12)?", line):
            tot += int(m.group(1), 16) << (12 if m.group(2) else 0)
        elif re.search(r"sub\s+sp, sp, x16", line):
            tot += x16
        elif (m := re.search(r"add\s+sp, sp, #0x([0-9a-f]+)(, lsl #12)?", line)) and tot:
            # A stack-probe loop steps sp down a page at a time and adds
            # it back before the real allocation.
            tot -= int(m.group(1), 16) << (12 if m.group(2) else 0)
        elif m := re.search(r"stp\s+[xdq]\d+, [xdq]\d+, \[sp, #-0x([0-9a-f]+)\]!", line):
            tot += int(m.group(1), 16)  # callee-save / frame-record push
        elif m := re.search(r"sub\s+\$0x([0-9a-f]+), %rsp", line):
            tot += int(m.group(1), 16)
        elif re.search(r"push[q]?\s+%r", line):
            tot += 8
    if cur is not None:
        out[cur] = tot
    return out


def build(args, backend, d):
    cmd = ["rustc", "-O", "-Ccodegen-units=1", "--emit=obj", "--out-dir", d] + args
    if backend:
        cmd.append(f"-Zcodegen-backend={backend}")
    subprocess.run(cmd, capture_output=True, check=True)
    return [os.path.join(d, f) for f in os.listdir(d) if f.endswith(".o")][0]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--input", default="regex-syntax")
    ap.add_argument("--json", default=os.path.join(ROOT, "target/parity/stack.json"))
    ap.add_argument("--backend", help="default: the release dylib harness/ctime.py builds")
    a = ap.parse_args()
    spec = importlib.util.spec_from_file_location("ctime", os.path.join(ROOT, "harness/ctime.py"))
    ct = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(ct)
    a.backend = a.backend or os.path.join(ROOT, f"target/release/{ct.LIB}rustc_codegen_pliron.{ct.SO}")
    args = ct.inputs()[a.input]
    with tempfile.TemporaryDirectory() as dl, tempfile.TemporaryDirectory() as dp:
        fl, fp = frames(build(args, None, dl)), frames(build(args, a.backend, dp))
    shared = sorted(set(fl) & set(fp))
    tl, tp = sum(fl[s] for s in shared), sum(fp[s] for s in shared)
    worst = sorted(shared, key=lambda s: fl[s] - fp[s])[:10]
    res = {"input": a.input, "shared_fns": len(shared), "llvm_bytes": tl, "pliron_bytes": tp,
           "ratio": tp / tl if tl else None,
           "worst": [{"fn": s, "llvm": fl[s], "pliron": fp[s]} for s in worst]}
    os.makedirs(os.path.dirname(a.json), exist_ok=True)
    json.dump(res, open(a.json, "w"), indent=1)
    print(f"stack {a.input}: {len(shared)} shared fns, frames {tp} B vs LLVM {tl} B = {res['ratio']:.3f}x")
    for w in res["worst"][:5]:
        print(f"  {w['pliron']:6d} vs {w['llvm']:6d}  {w['fn'][:100]}")


if __name__ == "__main__":
    main()
