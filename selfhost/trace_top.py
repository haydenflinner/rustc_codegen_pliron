#!/usr/bin/env python3
"""Live PLIRON_TRACE aggregator. Usage: trace_top.py <trace-file> [--once]
Tail-free: re-reads the file every 2s (lines are append-only), prints:
  - top steps by total ms (whole trace)
  - per-pid most recent step (what each rustc is doing right now)
  - recent function pipeline/compile totals
"""
import sys, time, collections

def scan(path):
    tot = collections.Counter()
    last = {}            # pid -> (step, fname)
    hot = []             # (ms, step, fname) pseudo-steps
    try:
        with open(path) as f:
            for line in f:
                p = line.split()
                if len(p) < 5 or p[0] != 'trace':
                    continue
                _, pid, step, ms, name = p[0], p[1], p[2], float(p[3]), p[4]
                tot[step] += ms
                last[pid] = (step, name)
                if step.startswith('*'):
                    hot.append((ms, step, name))
    except FileNotFoundError:
        pass
    return tot, last, hot

def report(tot, last, hot):
    print(f'--- {time.strftime("%H:%M:%S")} ---')
    for s, ms in tot.most_common(8):
        print(f'  {ms/1000:9.1f}s  {s}')
    print('  live:')
    for pid, (step, name) in sorted(last.items()):
        print(f'    pid{pid:>7} {step:<16} {name[:80]}')
    if hot:
        print('  slowest fns:')
        for ms, step, name in sorted(hot, reverse=True)[:5]:
            print(f'    {ms/1000:8.1f}s {step:<11} {name[:70]}')

def main():
    path = sys.argv[1]
    once = '--once' in sys.argv
    while True:
        tot, last, hot = scan(path)
        report(tot, last, hot)
        if once:
            return
        time.sleep(2)

if __name__ == '__main__':
    main()
