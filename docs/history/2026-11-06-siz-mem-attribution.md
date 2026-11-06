# 2026-11-06 — SIZ/MEM gap attribution (kernels, regex-syntax, syn)

Card cells at the time: SIZ 1.655x, MEM 1.556x. Fresh `harness/ctime.py`
run gave geomeans ~1.86 SIZ / ~1.71 MEM (jitter); per input:

| input | CMP | MEM | SIZ |
|---|---|---|---|
| kernels | 0.98 | 1.42 | 2.07 |
| kernels-wasm | 0.70 | 1.47 | 1.79 |
| regex-syntax | 1.16 | 1.78 | 1.90 |
| regex-wasm | 0.92 | 2.14 | 1.81 |
| syn | 1.31 | 1.84 | 1.75 |

## SIZ decomposition (kernels, ~31 KB rlib __text gap)

Mach-O `nm` has no size field; function sizes were derived from sorted
`__text` symbol addresses, LLVM's `.llvm.<hash>` suffixes normalised.

- **~14 KB — xcgu specialised clones.** Per-CGU IPO copies whose bodies
  differ from the canonical (e.g. a `reserve_rehash` clone of 107 insns
  vs the canonical 739). These buy runtime; they are not
  byte-duplicates, so linker dedup cannot remove them.
- **~5.5 KB — byte-identical xcgu copies.** Same local symbol, same
  bytes, emitted in two CGU objects (`sieve` 2880 B, `matmul` 1248 B,
  `fib` 128 B, ...). `non-external`, so ld64 keeps every copy in the
  final binary. Candidates for a weak/coalesced or hidden-external
  single-owner scheme (xcgu already promotes uncopyable deps that way).
- **~7.7 KB — punroll** (`PLIRON_PUNROLL=0`: 60,436 -> 52,660). Already
  budgeted (`MAX_LOOPS=8`, `MAX_CHAIN=4`); the bytes are the runtime
  parity price.
- **~8.1 KB — jump-thread transform** (`PLIRON_JT=0`: 52,372).
- **~0.3-0.6 KB — loop-head pads on large loops.** New knob
  `PLIRON_ALIGN_MAXB=<bytes>` skips the 16-byte loop-head pad when the
  emitted body exceeds the cap. Measured: kernels 60,436 -> 60,148 at
  cap=64/128/256 (-0.5%); regex-syntax -640 B (-0.15%). Most of the
  641-vs-220 nop excess is on *small* loops, where the pad earns — the
  Oct-29 note's "align only small bodies" idea turns out to be mostly
  what already happens; large-loop pads are rare. Kept opt-in
  (default u32::MAX = no gate); not worth a default flip.
- **~0.6 KB — pending env flags** (LIBCALL_DIRECT, EDGEMERGE=3,
  EPIMERGE=2).
- Remainder: per-function bloat in a few kernels (`sieve` 18x,
  `ipnsort` 9x, `matmul` 5x vs LLVM) — unroll/vectorise transforms.

**Linked-binary check**: a binary linking all kernels came out 269,936
(pliron) vs 255,920 (LLVM) `__text` — a 14 KB / 5.5% gap, about half
the rlib-level number; ld dead-strips roughly half the excess.

## MEM decomposition (syn native, peak RSS 1.04 GB vs LLVM 614 MB)

- ~16 CGU `Context`s (~55 MB each) live simultaneously through the
  parallel stage-A IR-opt; LLVM's modules are leaner per CGU.
- Post-inline op amplification: an example CGU grew 65,933 -> 130,389
  ops; peak lands during `callgvn`/`xoridem`/`devirt`/`xfacts-adopt`.
- pliron arenas recycle slots but never return to the OS — every
  Context lives to its high-water mark for the job's lifetime.
- `jearena` already purges freed pages promptly; the remaining peak is
  live data, not dirty-page retention.

## Next levers (not done)

- xcgu: promote byte-identical copies to a single hidden-external
  owner, or emit weak defs so ld coalesces (final-binary only).
- MEM: drop `job.parts` pieces parked stage-B jobs no longer need, or
  throttle `PLIRON_CGU_INFLIGHT` under memory pressure — both trade
  either plumbing depth or wall time.
- punroll/jumpthread size-vs-speed gates need a deliberate tradeoff
  decision, not a blind cap.
