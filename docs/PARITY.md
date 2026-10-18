# LLVM parity: where we stand, what to do next

The goal is parity with stock rustc + LLVM on four axes: speed of the
generated code (native, wasm) and compile time (native, wasm). This page
is the current scoreboard and the ranked work list. Update it when a
number moves (`harness/parity.py` regenerates the tables).

**Last measured:** 2026-10-18, aarch64-apple-darwin (Apple Silicon),
backend `40d729d` (+ uncommitted bcheck/loopvec edits), toolchain
nightly-2026-10-06. `harness/parity.py`, nothing else running.
Ratio = pliron / LLVM: **below 1 means pliron is better.**

| axis | pliron / LLVM | verdict |
|---|---|---|
| native runtime (geomean, 16 kernels) | **0.900** | ahead; 4 kernels still behind |
| wasm runtime (geomean, 16 kernels) | **1.045** | behind; 7 kernels behind by >5% |
| wasm module size | 2.18 (206 KiB vs 94 KiB) | behind |
| compile time: bench kernels, native | **1.94** (0.85 s vs 0.44 s) | behind |
| compile time: bench kernels, wasm | **3.35** (1.82 s vs 0.54 s) | far behind |
| compile time: regex-syntax (~40 kLOC) | **6.57** (10.3 s vs 1.6 s) | far behind — the biggest gap |

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
| ├ IR-opt (`*iropt*`) | 5.1 s | **serial**: CGUs run one after another |
| ├ lowering phases A/B (`*phaseA*`, `*phaseB*`) | 5.5 s | |
| └ CLIF pipeline (`*pipeline*`) | ~43 CPU-s | parallel across lowering threads; 3,485 fns × every step |
| top CLIF steps (CPU) | jumpthread 2.3 s, ifconv2 1.3 s, slp 1.0 s, loadfwd 0.8 s, mergechain 0.8 s, dse 0.6 s, edgespec 0.6 s | |
| top IR steps | inline 1.1 s (+ inline2 0.4 s), sroa 0.6 s | |

Earlier profiles (docs/performance.md "Compile time") found the floor
with every CLIF pass off is still our glue: op construction, `State`
side-table hashing, lowering — not Cranelift. syn was 32x; flags alone
(`-Zthreads=8`, fewer CGUs) took it 102 s → 24 s.

Next steps, in expected-payoff order:
1. **Run CGUs' IR-opt in parallel.** It is serial today (half of
   codegen wall on regex-syntax). `Context` is `Send`; the blocker is
   per-CGU state shared through globals (`GLOBAL_FACTS` for xfacts,
   bisect counters). Expected: near-linear on the IR-opt half.
2. **Applicability prechecks**: most steps change nothing on most
   functions. A cheap scan that proves "this step cannot fire" (no
   loops → skip loop passes; no select/brif shapes → skip ifconv2;
   no vector-able ops → skip slp) before building analyses.
   `clif-check report` + `PLIRON_STATS=1` show which steps fire where;
   `clif-check bench` measures the win with no rustc.
3. **The heavy steps**: jumpthread, ifconv2, slp, loadfwd, mergechain.
   Each is the subject of earlier rounds (see docs/performance.md); the
   remaining cost is per-function analysis rebuilds — use the shared
   `Analysis` cache, and see the complexity rules in AGENTS.md.
4. **xcgu copies** multiply IR-opt work per CGU (`PLIRON_XCGU=0` cut syn
   78 → 55 s) but feed the inliner. Share analyzed bodies across CGUs
   instead of re-optimizing each copy.
5. **Glue**: dense columns instead of `FxHashMap<Ptr<Operation>, _>`
   side tables (DoD step 4, `docs/history/dod-steps-4-5.md`).

wasm compile time (3.35x) adds the CLIF → waffle translation, the waffle
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

Also: `black_box` on wasm still takes the alloca + volatile fallback
(`intrinsic.rs` `clif_bb = !is_like_wasm`) though `clif_emit` handles
the CLIF op — likely a free win on every kernel's harness overhead.
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
