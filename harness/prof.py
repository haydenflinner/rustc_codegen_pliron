#!/usr/bin/env python3
"""Profile a compile with macOS `sample` — no instrumentation needed.

    harness/prof.py -- rustc -O -Zcodegen-backend=target/release/librustc_codegen_pliron.dylib ...
    harness/prof.py -p PID [-d SECS]          # attach to a running rustc
    harness/prof.py -a sample.txt             # analyze a saved report

Runs `sample` on the rustc process, saves the raw report next to
/tmp/prof-<n>.txt, and prints:
  - self-time by leaf symbol ("top of stack" flat table),
  - bucketed categories (idle, allocator, DFG accessors, analyses, ...),
  - per-thread busy/idle so pool starvation is visible.

samply does not attach to our backend dylib reliably on macOS
(zero samples); /usr/bin/sample does, and symbolizes through .dSYM.
Debug dylib has line-tables-only debug info and already symbolizes;
for the release dylib run `dsymutil` on it first.
"""
import re, subprocess, sys, time, collections, os, signal

WAIT = ('__psynch_cvwait', 'semaphore_wait_trap', '__ulock_wait', 'kevent',
        '__workq_kernreturn', '__semwait_signal', 'mach_msg_trap', '__wait4',
        'swtch_pri', '__psynch_mutexwait', 'sigsuspend', 'kernel_event')


def category(sym, lib):
    if 'kernel' in lib:
        return 'WAIT/idle'
    if ('malloc' in lib or 'rjem' in sym or 'zone_' in sym or 'memmove' in sym
            or 'memset' in sym or 'bzero' in sym or 'memcpy' in sym):
        return 'allocator/memmove'
    if 'pthread' in lib:
        return 'threading misc'
    if 'RawVec' in sym or 'drop_glue' in sym or 'hashbrown' in sym:
        return 'container grow/hash/drop'
    if any(k in sym for k in (
            'resolve_aliases', 'resolve_to_data', 'resolve_all_aliases',
            'value_def', 'inst_values', 'branch_destination', 'BlockData',
            'block_insts', 'block_successors', 'non_tail_call')):
        return 'CLIF DFG/inst accessors'
    if any(k in sym for k in (
            'ControlFlowGraph', 'DominatorTree', 'LoopAnalysis', 'Uses')):
        return 'analysis rebuild (CFG/dom/loops/uses)'
    if any(k in sym for k in (
            'SSABuilder', 'Verifier', 'Elaborator', 'regalloc', 'isle',
            'machinst', 'emit', 'vcode')):
        return 'cranelift emit/verify/lower'
    if 'pliron_clif_opt' in sym:
        return 'pliron CLIF passes'
    if 'pliron' in sym:
        return 'pliron IR-opt + lowering'
    if 'rustc_driver' in lib:
        return 'rustc frontend/misc'
    return 'other'


def analyze(path):
    lines = open(path).read().splitlines()
    end = next((i for i, l in enumerate(lines)
                if l.startswith('Total number in stack')), len(lines))
    start = next((i for i, l in enumerate(lines)
                  if l.startswith('Sort by top of stack')), None)

    # per-thread busy/idle over the call-graph section
    hdr = re.compile(r'^\s{0,4}(\d+)\s+(Thread_\d+(?:: .*)?|Main Thread.*)$')
    num = re.compile(r'^\s*[!+:| ]*?\s*(\d+)\s+')
    rows, cur, tot, idle = [], None, 0, 0
    for ln in lines[:end]:
        m = hdr.match(ln)
        if m:
            if cur:
                rows.append((cur, tot, idle))
            cur, tot, idle = m.group(2), int(m.group(1)), 0
            continue
        if cur and any(w in ln for w in WAIT):
            mm = num.match(ln)
            if mm:
                idle += int(mm.group(1))
    if cur:
        rows.append((cur, tot, idle))

    print(f"{'thread':<46}{'tot':>7}{'idle':>7}{'busy':>7}")
    for t, n, i in sorted(rows, key=lambda r: -(r[1] - r[2])):
        print(f"{t:<46}{n:>7}{i:>7}{n - i:>7}")
    gt = sum(r[1] for r in rows)
    gi = sum(r[2] for r in rows)
    print(f"\nthreads {len(rows)}  total {gt}  idle {gi} "
          f"({100 * gi / max(gt, 1):.0f}%)  busy {gt - gi}")

    if start is None:
        return
    fr = re.compile(r'^\s+(.+?)\s+\(in (.+?)\)\s+(\d+)\s*$')
    entries = []
    for ln in lines[start + 1:]:
        m = fr.match(ln)
        if not m:
            break
        entries.append((m.group(1), m.group(2), int(m.group(3))))
    total = sum(n for _, _, n in entries)
    cats = collections.Counter()
    for sym, lib, n in entries:
        cats[category(sym, lib)] += n
    print(f"\nself-time buckets (total {total} samples, ~{total / 1000:.0f} thread-s):")
    for c, n in cats.most_common():
        print(f"{n:7d} {100 * n / total:5.1f}%  {c}")
    print("\ntop leaf symbols:")
    for sym, lib, n in entries[:40]:
        short = sym
        for pat, to in (('_RNv', ''), ('NtNt', ''), ('Nt', ''), ('NtB', '')):
            pass
        # crude demangle: last path-ish segment before hash
        seg = re.sub(r'^_[R$]N\w*', '', sym)
        print(f"{n:6d}  {sym[:110]}  [{lib}]")


def record(args):
    dur = 10
    if args and args[0] == '-d':
        dur = int(args[1]); args = args[2:]
    if args and args[0] == '-p':
        pid = int(args[1])
        out = f'/tmp/prof-{pid}.txt'
        subprocess.run(['sample', str(pid), str(dur), '-file', out], check=True)
        return out
    if not args or args[0] != '--':
        sys.exit('usage: prof.py -- <cmd...> | -p PID [-d SECS] | -a FILE')
    proc = subprocess.Popen(args[1:])
    time.sleep(0.3)
    pid = proc.pid
    # rustup shims spawn the real rustc as a child and wait on it —
    # sampling the shim catches only its __wait4. Descend to the
    # deepest child that is still alive.
    for _ in range(50):
        kids = subprocess.run(['pgrep', '-P', str(pid)],
                              capture_output=True, text=True).stdout.split()
        if not kids:
            break
        pid = int(kids[0])
    out = f'/tmp/prof-{pid}.txt'
    samp = subprocess.Popen(['sample', str(pid), str(dur), '-file', out])
    proc.wait()
    samp.wait()
    print(f'saved {out} (pid {pid})')
    return out


if __name__ == '__main__':
    args = sys.argv[1:]
    if args and args[0] == '-a':
        analyze(args[1])
    else:
        analyze(record(args))
