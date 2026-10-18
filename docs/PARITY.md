# LLVM parity: where we stand, what to do next

The goal is parity with stock rustc + LLVM on four axes: speed of the
generated code (native, wasm) and compile time (native, wasm). This page
is the current scoreboard and the ranked work list. Update it when a
number moves (`harness/parity.py` regenerates the tables).

**Last measured:** 2026-10-18, aarch64-apple-darwin (Apple Silicon),
backend `40d729d` + parallel-CGU jobs (`src/cgupool.rs`), toolchain
nightly-2026-10-06. `harness/parity.py`, nothing else running.
Ratio = pliron / LLVM: **below 1 means pliron is better.**

| axis | pliron / LLVM | verdict |
|---|---|---|
| native runtime (geomean, 16 kernels) | **0.900** | ahead; 4 kernels still behind |
| wasm runtime (geomean, 16 kernels) | **1.045** | behind; 7 kernels behind by >5% |
| wasm module size | 2.18 (206 KiB vs 94 KiB) | behind |
| compile time: bench kernels, native | **0.98** (0.42 s vs 0.43 s) | at parity |
| compile time: bench kernels, wasm | **2.56** (1.40 s vs 0.55 s) | behind |
| compile time: regex-syntax (~40 kLOC) | **2.26** (3.3 s vs 1.5 s) | behind; was 6.57 before CGU jobs went parallel |

Older measurements on the Windows i7 box (native 1.028, wasm 1.157) are
in `docs/history/PERF-REPORT.md`; the narrative of how each kernel got
here is `docs/performance.md`.

## How to measure

- Everything: `harness/parity.py` (~15 min; `--skip native,wasm,ctime`,
  `--from-logs`). Writes `target/parity/summary.md`.
- One kernel's runtime: `bench/run.sh <name>` / `bench/wasm/run.sh <name>`.
- Compile time: `harness/ctime.py [--only regex-syntax]` (release
  dylib). Inside one compile: `PLIRON_TRACE_FILE=t.txt` then sum column
  4 by column 3 (`*iropt*`, `*modgen*`, `*pipeline*`, per-step names);
  `-Ztime-passes`; `samply`.
- CLIF pipeline time without rustc: `clif-check bench target/corpus/std`,
  or the live dashboard's step-time panel (vs baseline, on every save).
- Noise: ±3% is parity on a quiet box. Criterion compares against an
  LLVM baseline measured minutes earlier; wasm uses interleaved rounds.
  Don't benchmark while the gate or another agent is building.

## 1. Compile time (highest priority)

regex-syntax, one `rustc -O` (release dylib), 11.6 s wall:

| part | time | note |
|---|---|---|
| rustc frontend (shared with LLVM) | < 1 s | |
| our codegen (`codegen_crate`) | 10.9 s | LLVM's *entire* compile is 1.56 s |
| ├ IR-opt (`*iropt*`) | 5.1 s | was serial; now parallel across CGU jobs |
| ├ lowering phases A/B (`*phaseA*`, `*phaseB*`) | 5.5 s | |
| └ CLIF pipeline (`*pipeline*`) | ~43 CPU-s | parallel across lowering threads; 3,485 fns × every step |
| top CLIF steps (CPU) | jumpthread 2.3 s, ifconv2 1.3 s, slp 1.0 s, loadfwd 0.8 s, mergechain 0.8 s, dse 0.6 s, edgespec 0.6 s | |
| top IR steps | inline 1.1 s (+ inline2 0.4 s), sroa 0.6 s | |

Earlier profiles (docs/performance.md "Compile time") found the floor
with every CLIF pass off is still our glue: op construction, `State`
side-table hashing, lowering — not Cranelift. syn was 32x; flags alone
(`-Zthreads=8`, fewer CGUs) took it 102 s → 24 s.

Next steps, in expected-payoff order:
1. ~~Run CGUs' IR-opt in parallel~~ **done**: `compile_codegen_unit`
   stops after mono-item `define`s and queues the pliron
   `Context`+`State` as a job in `src/cgupool.rs`; `optimize` (rustc's
   jobserver-token workers) runs whatever job is runnable until its own
   module exists — IR-opt *and* CLIF lowering both moved off the main
   thread. The one cross-CGU edge (`xfacts` publish→adopt) splits into
   `facts_publish`/`facts_adopt` on per-CGU ordinals; a CGU's tail parks
   until every earlier ordinal has published, reproducing serial adopt
   sets exactly (parallel objects are byte-identical to
   `PLIRON_CGUPAR=0`'s). `--jobs-backend` (default: all cores) sizes the
   pool; `PLIRON_CGUPAR=0`, `PLIRON_OPT_BISECT` and `--jobs-frontend`
   keep the serial path. regex-syntax 6.57 → 2.27; kernels 1.94 → 0.98.
   Remaining in that gap: the front half (MIR→pliron `define`s) is still
   main-thread-serial, and each job's inner lowering spawns its own
   per-function threads — a shared pool or `PLIRON_PAR` capping would
   cut the oversubscription.
2. ~~Applicability prechecks~~ **done**: `shape_of` computes a bitmask
   of function-shape facts (loops, brifs, params, inst kinds) in one
   pass; `Step::needs`/`needs_any` gate heavy steps before their
   analysis builds, and the cached bits widen by each mutating step's
   `creates` (superset — a clear bit certifies absence; under-admission
   is a missed opt, never a miscompile; `PLIRON_SHAPE_CHECK` catches it).
   `clif-check bench target/corpus/std`: 35.5 s → 28.7 s (~19%).
3. **The heavy steps**: jumpthread, ifconv2, slp, loadfwd, mergechain.
   Each is the subject of earlier rounds (see docs/performance.md); the
   remaining cost is per-function analysis rebuilds — use the shared
   `Analysis` cache, and see the complexity rules in AGENTS.md.
4. **xcgu copies** multiply IR-opt work per CGU (`PLIRON_XCGU=0` cut syn
   78 → 55 s) but feed the inliner. Share analyzed bodies across CGUs
   instead of re-optimizing each copy.
5. ~~Glue~~ **partly done, premise revised**: the inter-step
   bookkeeping is now ~3% of phaseB CPU. `carry_aliases`/`compact_aliases`
   walk `dfg.alias_writes()` (the per-write list behind `alias_epoch`)
   instead of rescanning `values()` — step-glue −41% on regex-syntax.
   `sample` profiling says the remaining mass is the ~80 per-fn scan
   loops themselves (`inst_values`/`Uses::new`/`resolve_aliases` ≈ 26%
   of leaf samples, spread across every step); `FxHashMap` side-table
   hashing is only ~6%, so dense columns are no longer the obvious win
   (DoD step 4, `docs/history/dod-steps-4-5.md` still applies to the
   pliron-IR side).

wasm compile time (2.56x) adds the CLIF → waffle translation, the waffle
inliner and `wpeep` on top of the same pipeline; profile
`--target wasm32-unknown-unknown --emit obj` the same way.

## 2. wasm runtime

| kernel | pliron/LLVM | lead |
|---|---|---|
| fib | **2.01** | Recursion turned into a loop by both, but our loop body is an `if/else` + `br` with 5 locals vs LLVM's single `br_if` with 2 (`wasm2wat` the `fib3fib` function in both modules). Check what V8 makes of each before assuming the shape is the cause. |
| chars_mixed | 1.35 | non-ASCII path; native is at parity, so look at what `clif_emit`/`wpeep` make of the rangefold output |
| parse_only | 1.31 | digit loops; native is +5% too |
| hmget | 1.22 | hashbrown probe + SipHash; earlier inlining work closed most of it (`WASM_HOT`) |
| strwork | 1.21 | string building / memcpy-heavy |
| nbody | 1.08 | f64 math |
| contains_rng, hashmap | 1.05 | |
| matmul 0.74, iterw 0.79, tokenize 0.79, chars_alpha 0.82, eqbug 0.88 | | ahead |

`black_box` on wasm is a stack store + reload under both backends (LLVM
does the same), so it is not a source of the gap — don't "optimize" it
to an identity; that would only make the benchmarks lie.
Module size (2.2x) is mostly pliron-wasm-ld's weaker dead-code GC and
inlining growth; `winline`'s caller-growth cap already took it from 8x.

## 3. Native runtime

| kernel | pliron/LLVM | lead |
|---|---|---|
| eqbug | 1.10 | narrow `black_box` barriers: LLVM's `*m` asm form folds the `uxtb`/`uxth` into the store/load; we pay an ALU op per barrier, plus constant remat under pressure |
| parse_only | 1.05 | was at parity after `edgesink`; regressed — bisect with `harness/bisect.py` |
| nbody | 1.04 | flip-flops between runs; earlier: an invariant's spill pair per iteration (regalloc pressure) |
| sieve | 1.03 | marking loop is instruction-tight; residual is elsewhere |
| chars_ascii 0.57, tokenize 0.66, iterw 0.70, contains_rng 0.78, hashmap 0.88, sortw 0.90, fib 0.95 | | ahead |

x86-64 numbers are older (Windows box, geomean 1.028); re-measure there
before working on x64-specific gaps.

## Where the old checklists live

- LLVM's -O2 pass list vs ours (Oct 10; partly stale):
  `docs/history/llvm-passes.md`.
- Per-session optimization logs: `docs/history/{native,x64}-notes.md`,
  `docs/history/coordination-ledger.md`.
