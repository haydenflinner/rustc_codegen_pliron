# 2026-11-06: `PLIRON_CGU_INFLIGHT=4` default-on (compile memory)

The syn MEM gap was queue depth, not live-work inflation: the serial
`define` phase submits CGU jobs faster than the workers drain them, so
the `ready` queue held most of the 16 CGUs' unoptimized `Context`s at
once (~25 MB each pre-inline, ~55 MB post-inline at stage B) — peak
1.07 GB vs LLVM's 614 MB
(`2026-11-06-siz-mem-attribution.md`).

`PLIRON_CGU_INFLIGHT` (pre-existing, default off) blocks `submit` while
N CGUs are in-flight (ready + running + parked): the main thread waits
holding only the context it is currently defining. It cannot deadlock —
a parked stage-B's fact needs are strictly lower ordinals, already
in-flight or published, so the lowest in-flight ord always progresses.

Measured on syn (`ctime.py --only syn`, interleaved runs):

| cap | peak RSS | vs LLVM 0.61 GB |
|---|---|---|
| 0 (off) | 1.05-1.08 GB | ~1.7x |
| 12 | 1.00 GB | ~1.6x |
| 8 | 0.95 GB | ~1.55x |
| 6 | 0.87-0.90 GB | ~1.45x |
| 4 | 0.75-0.78 GB | ~1.25x |

regex-syntax: 0.55 -> 0.47-0.48 GB at cap 4. Wall time: no measurable
cost under a noisy box (regex 2.21 s either way; syn walls jittered
+/-20% regardless of the cap — workers were already the bottleneck, and
the shared fn pool keeps them busy while the main thread parks).

Now `DEFAULT_VALUES`: `PLIRON_CGU_INFLIGHT=4`, `=0` restores unbounded.
Held mode (thin-local LTO two-phase facts) already bypassed the cap.
Gate + quick green on the flipped build.
