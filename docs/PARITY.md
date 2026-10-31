# LLVM parity: status and work list

The goal is parity with stock rustc + LLVM, native and wasm, on the speed
of the generated code, compile time, compile memory, code size and stack
use, without giving up correctness. This page has the parity card
definition, the current scoreboard, the known gaps by axis, the ranked
work list, and how to measure. Session logs and older measurements are in
[`history/`](history/README.md).

Update the scoreboard when a number moves. Put the narrative of how it
moved in a dated `history/` file, not here.

## The parity card

`harness/parity.py` opens its summary (`target/parity/summary.md`) with
one rating per category and an overall, FIFA-style. Each category reduces
to a ratio r = pliron / LLVM (lower is better) and rates
**100 + 50·log2(1/r)**, clamped to 0–150: 100 is parity, 150 twice as
good, 50 half as good. The overall is the weighted mean of the ratings of
the categories that have data. Inside the clamp that is a weighted
geomean of the ratios, so 2x better on one axis offsets 2x worse on
another of equal weight.

| code | category | ratio from | weight |
|---|---|---|---|
| SPD | runtime speed | geomean of the native (`bench/run.sh`) and wasm (`bench/wasm/run.sh`) kernel geomeans | 30 |
| CMP | compile time | `harness/ctime.py` wall-time geomean | 15 |
| MEM | compile memory | `ctime.py` peak-RSS geomean | 10 |
| SIZ | code size | `ctime.py` machine-code bytes (text / wasm `CODE` sections), plus the wasm bench module | 10 |
| STK | stack frames | `harness/frames.py`: frame bytes summed over the functions both backends define in regex-syntax (one CGU) | 15 |
| COR | correctness | geomean over suites of (pliron failures + 1) / (LLVM failures + 1): UI stage-2-only vs stage-1-only (capped at parity), `harness/crates.py` pliron-only failures vs 0 (only runs of 10+ crates count), fuzz seeds whose status is not `pass` (`env-*` included) vs 0 (only runs of 100+ seeds count) | 20 |

The weights are `CARD` in `harness/parity.py`. Correctness is an
attribute, not a gate: it can be traded for a while, at a visible price
(one pliron-only crate failure rates COR 50). `parity.py` does not rerun
the slow correctness suites (`selfhost/verify.sh`, `harness/crates.py`,
the fuzz suite); it reads their saved results and shows each one's age.

The CubeCL CPU-kernel path has its own card (`harness/cubecl_card.py`,
[`cubecl-burn.md`](cubecl-burn.md#the-card)).

## Scoreboard

<!-- SCOREBOARD:BEGIN -->
**Last measured 2026-10-30**, aarch64-apple-darwin, backend `3c43d21`,
nightly-2026-10-22, load 7–9 (SPD and CMP ±3–5%). Full run, per-input
tables and the comparison with the previous card:
[`history/2026-10-30-parity-card.md`](history/2026-10-30-parity-card.md).

**Overall 95** (2026-10-27: 88).

| SPD | CMP | MEM | SIZ | STK | COR |
|---|---|---|---|---|---|
| **106** | **112** | **68** | **64** | **92** | **100** |

| category | weight | pliron/LLVM | rating | from |
|---|---|---|---|---|
| SPD runtime speed | 30 | 0.918 | 106 | native 0.917 · wasm 0.919 |
| CMP compile time | 15 | 0.853 | 112 | 5 inputs |
| MEM compile memory | 10 | 1.556 | 68 | peak RSS, 5 inputs |
| SIZ code size | 10 | 1.655 | 64 | machine code, 5 inputs + wasm module 1.71 |
| STK stack frames | 15 | 1.124 | 92 | regex-syntax, 327 shared fns |
| COR correctness | 20 | 1.000 | 100 | ui: 0 stage-2-only vs 3 stage-1-only (3 d old) · crates: 0 pliron-only across 20 crates (0 h old) · fuzz: 0 of 200 seeds (0 h old) |

Correctness inputs: crates tier 1 run in the default `--deps stock` mode
(the crates' own code only); the UI input is the 2026-10-27 stage-2
self-host run. x86-64 was last measured 2026-10-22.

**Native kernels** (criterion, pliron vs an LLVM baseline taken minutes earlier):

| kernel | LLVM | pliron | pliron/LLVM |
|---|---|---|---|
| arrayvec | 5.576 ms | 5.150 ms | 0.924 ◄ faster |
| chars_alpha | 2.094 ms | 2.014 ms | 0.962 ◄ faster |
| chars_ascii | 2.185 ms | 1.301 ms | 0.595 ◄ faster |
| chars_mixed | 15.527 ms | 15.371 ms | 0.990 |
| contains_rng | 2.189 ms | 1.720 ms | 0.786 ◄ faster |
| eqbug | 0.112 ms | 0.122 ms | 1.089 ▲ slower |
| fib | 14.395 ms | 13.072 ms | 0.908 ◄ faster |
| hashmap | 71.040 ms | 64.149 ms | 0.903 ◄ faster |
| hmget | 14.369 ms | 12.822 ms | 0.892 ◄ faster |
| iterw | 19.584 ms | 13.856 ms | 0.708 ◄ faster |
| matmul | 8.495 ms | 8.529 ms | 1.004 |
| nbody | 113.523 ms | 90.536 ms | 0.798 ◄ faster |
| parse_only | 367.568 ms | 391.640 ms | 1.065 ▲ slower |
| sieve | 12.320 ms | 11.887 ms | 0.965 ◄ faster |
| sortw | 6.841 ms | 6.368 ms | 0.931 ◄ faster |
| strwork | 13.876 ms | 13.951 ms | 1.005 |
| tokenize | 5.913 ms | 7.264 ms | 1.229 ▲ slower |
| varint | 6.372 ms | 6.448 ms | 1.012 |
| varint_postcard | 5.920 ms | 5.187 ms | 0.876 ◄ faster |

**wasm kernels** (node, interleaved rounds):

| kernel | LLVM | pliron | pliron/LLVM |
|---|---|---|---|
| chars_alpha | 4.635 ms | 4.008 ms | 0.866 ◄ faster |
| chars_ascii | 2.319 ms | 2.316 ms | 0.999 |
| chars_mixed | 18.381 ms | 18.227 ms | 0.993 |
| contains_rng | 4.924 ms | 3.939 ms | 0.804 ◄ faster |
| eqbug | 0.136 ms | 0.119 ms | 0.877 ◄ faster |
| fib | 9.352 ms | 8.862 ms | 0.940 ◄ faster |
| hashmap | 66.195 ms | 70.189 ms | 1.061 ▲ slower |
| hmget | 11.473 ms | 11.509 ms | 1.003 |
| iterw | 66.800 ms | 53.491 ms | 0.802 ◄ faster |
| matmul | 30.773 ms | 21.701 ms | 0.705 ◄ faster |
| nbody | 84.100 ms | 87.151 ms | 1.036 ▲ slower |
| parse_only | 380.972 ms | 389.002 ms | 1.021 |
| sieve | 13.941 ms | 14.024 ms | 1.006 |
| sortw | 6.042 ms | 5.630 ms | 0.930 ◄ faster |
| strwork | 17.729 ms | 19.087 ms | 1.078 ▲ slower |
| tokenize | 7.674 ms | 5.495 ms | 0.714 ◄ faster |
<!-- SCOREBOARD:END -->

## Known gaps by axis

Each entry names the mechanism and where to look. Numbers for these live
in the scoreboard and in the linked history files.

### Runtime speed

- **wasm local traffic.** CLIF SSA values become wasm locals; waffle's
  relooper passes block params through scratch locals at `if`/`else`
  boundaries, and we emit structured `if` diamonds where LLVM emits
  `block` + `br_if` and keeps values on the operand stack. TurboFan
  removes much of it, but not all (`hashmap`, `hmget`, `tokenize`,
  `chars_*`). Code: `src/wasm.rs` (`clif_emit`, `wpeep`).
  History: `history/2026-10-21-wasm-runtime.md`,
  `history/2026-10-20-wasm-open.md`.
- **`strwork` on wasm.** Native is at parity; wasm loses in
  `format_exact` and the bignum helpers: more surviving bounds checks
  than LLVM (`bignum.rs` `i < 40` checks) and more inlined `push_str` /
  `extend` / fmt wrappers that LLVM keeps as calls (inline policy in
  `pliron-ir-opt/src/inline.rs`).
- **`parse_only` on wasm.** TurboFan code matches LLVM per instruction;
  the cold `Err` arm carries block-arg moves, so V8 places an edge block
  inline on the hot path. Levers: no moves on cold edges, or wasm branch
  hints.
- **`eqbug` native.** `uxtb`/`uxth` on narrow `black_box` results before
  compares. Lever: compare lowering (hoisted constant + `cmp wk, w, uxtb`).
  History: `history/2026-10-27-slow-kernels.md`.
- **Register pressure from loop-invariant hoists.** `PLIRON_ELABLOOPS=1`
  (vendored Cranelift egraph elaboration) hoists correctly and helps
  `parse_only`, but costs `strwork` spills. It stays opt-in until the
  hoist is pressure-aware.
- **`nonnull` lost through stack slots.** Iterator pointers spilled and
  reloaded across calls lose `!nonnull`, which blocks `Option`-niche
  `eqz` folds after the reload (`tokenize`'s `Split`). Needs nonnull
  facts carried through our own slot demotion.
- **Layout noise.** Identical code at a different address swings tight
  kernels (`sieve`, `nbody`) by up to ±10% native and ±25% under V8.
  Check deltas with `bench/layout.sh` and `run.mjs --layouts=N` before
  acting on them.
- **x86-64** has not been measured since 2026-10-22 (Windows i7,
  `history/2026-10-22-parity-scoreboard.md`). Re-measure before working
  on x64-specific gaps.
- **CubeCL kernels**: f16 GEMM is the largest gap (per-leaf widen and
  narrow where LLVM keeps packed ops); x86-64 not run.
  [`cubecl-burn.md`](cubecl-burn.md#what-is-next).

### Compile time

- **Per-instruction pass cost on giant functions.** Functions over ~5k
  instructions carry most of the CLIF pipeline time, and they are hot, so
  a size cap costs runtime. The top steps (jumpthread, ifconv2, slp,
  loadfwd, the mergechain/webmerge family, dse) are already linear; the
  remaining cost is per-access weight (`RefCell` arena access, analysis
  rebuilds across mutation boundaries). LLVM also starts from MIR that
  rustc already cleaned; we redo some of that work at CLIF.
- **IR-opt**: about a quarter of samples (inliner cloning, sroa,
  nocapture). Live `xcgu` copies rerun the pliron-IR passes in every CGU
  that holds them; the CLIF side shares results through the in-process
  pipeline cache (`PLIRON_MEMCACHE`, default on), the pliron-IR side does
  not. Skipping copy optimization was measured worse: the result must be
  shared, not skipped.
- **Serial parts.** The front half (MIR → pliron `define`s) runs on the
  main thread. A CGU's stage B waits until the CGUs that own its bodiless
  declarations have published facts, so the biggest CGU's IR-opt gates
  the wall on regex-syntax; reordering CGUs changes output, so this is
  not reachable byte-identically (`src/cgupool.rs`, `docs/determinism.md`).
- **wasm** adds CLIF → waffle translation, the waffle inliner and `wpeep`
  on top of the same pipeline. The small `kernels-wasm` input is
  dominated by fixed rustc latency.
- History: `history/2026-10-20-compile-time-worklist.md`,
  `history/2026-10-27-ctime.md`.

### Compile memory

- The default-on memory set (`PLIRON_LOWERFREE`, `PLIRON_JEARENA`,
  `PLIRON_CGU_ACAP=8`, `PLIRON_DEADFREE`, `PLIRON_LTUNDEF`; see
  `pliron-cfg/src/lib.rs` `DEFAULT_ON`/`DEFAULT_VALUES`) closed most of
  the first gap. Remaining levers: the wasm phase-(A) hump (the whole
  CGU's optimized CLIF is held until waffle translation; translating each
  function inside its pipeline job would release it earlier), and fewer
  ops after inlining. History: `history/2026-10-26-memory.md`,
  `history/2026-10-27-memory2.md`, `history/2026-10-27-memory3.md`.

### Code size

- **Native.** Not per-instruction quality: call-structure-matched
  functions are no larger than LLVM's. Most of the gap is speed-for-size
  transforms with no size or cold gate: bounded partial unrolling and
  peeling (`punroll.rs`), jump threading, loop rotation, inlining volume,
  and the inline memcpy/memmove fast path (`MEMFAST`,
  `lower.rs::small_copy_or_call`). Then aggregates scalarized and moved
  slice by slice where LLVM keeps a memcpy (`sroa.rs`, `slotprom.rs`),
  `uxtb` after bool-returning calls, and loop-header alignment `nop`s
  (`PLIRON_ALIGN`). Kill-switch attribution:
  `history/2026-10-29-ir-excess.md`, `history/2026-10-29-code-size.md`.
- **wasm.** Op density, not dead code: `local.get`/`local.set`, `br` and
  `if`/`else` counts are several times LLVM's (see the runtime entry on
  local traffic). The default-on wasm size set (`PLIRON_WASM_NOIRRED`,
  `PLIRON_WASM_NOREC`, `PLIRON_WASM_GC`, `PLIRON_REDUCIBLE`,
  `PLIRON_WASM_HOTLEAF`, `PLIRON_INLINE_SMALLSCC`, `PLIRON_WASM_SOLO`,
  `PLIRON_WASM_TINY`, `PLIRON_WASM_ELIDEBLK`, `PLIRON_WIFRET`) removed
  the inlining blow-up. `tools/pliron-wasm-ld` folds GOT `global.get`s
  to `i32.const` where the immediate is no larger than the global index.
  History: `history/2026-10-27-wasm-size.md`.

### Stack frames

- Copy-only aggregates (regex-syntax `pop_group`/`pop_class`, 160–224 B
  states taken apart field by field) get one spill slot per scalarized
  field. Lever: memcpy-chain forwarding so a copy-only alloca can stay in
  memory. History: `history/2026-10-27-spills3.md`.
- regalloc2 reloads once per `br_table` edge instead of once at the join.
- The stage-2 self-host fails 6 deep-recursion UI tests that stock
  passes: rustc's parser and AST lowering frames are larger than LLVM's
  (README, `selfhost/verify.sh`).

### Correctness

- **windows-msvc catch.** With stock std, `catch_unwind` in pliron-built
  code cannot catch a real SEH throw; EH is emulated and works only when
  std is backend-built. Design for real SEH catch:
  [`seh-design.md`](seh-design.md) (not landed).
- **IPO on `xcgu` copies vs the wasm linker's name fold.**
  `pliron-wasm-ld` folds same-named bodies across objects and keeps the
  smallest, which is sound only if every copy keeps the generic
  semantics. `spec` now skips copies on wasm; any other callee-side IPO
  that rewrites a copy's body per CGU has the same hazard
  (`history/2026-10-30-wasm-spec-xcgu-copy-fold.md`).
- **Corpus replay is lossy.** The corpus fact header does not carry
  every `Pipe` fact table, so a clean replay does not clear a step
  ([`miscompile-triage.md`](miscompile-triage.md#replay-fidelity-gap-found-the-hard-way)).
- **Crate coverage.** `harness/crates.py` defaults to `--deps stock`:
  only the crates' own code goes through the backend, not their
  dependencies or doc-tests. `--deps pliron` is the full run.

## Work list

Ranked by expected card gain per effort. Each item links its evidence.

1. **Validate and flip the pending size flags.** `PLIRON_COPYPAIR=1`,
   `PLIRON_TBNZ_IREDUCE=1`, `PLIRON_SROA_MAXSLICES=18`
   (`history/2026-10-29-ir-excess.md` §5) and `PLIRON_LIBCALL_DIRECT=1`,
   `PLIRON_EDGEMERGE=3`/`PLIRON_EPIMERGE=2`
   (`history/2026-10-29-code-size.md`) still need a bench A/B,
   `harness/gate.py --full`, crates and stage-2 UI.
2. **Size/cold gates on duplicating passes**: `punroll`, peeling,
   `jumpthread`, `looprot` and the `MEMFAST` fast path, gated on cold
   loops (bodies that lead to a panic) or huge functions. Bench every
   kernel.
3. **Aggregate copies as memcpy**: a principled "do not split an alloca
   whose only accesses are whole-object copies" rule in place of the
   `SROA_MAXSLICES` threshold; memcpy-chain forwarding for copy-only
   allocas (also a stack-frame lever).
4. **wasm local traffic**: wpeep-friendly `CondBr` → `br_if` emission,
   block-typed merges for branch arms, make multi-value `WIFRET`
   (`PLIRON_WIFRET_MULTI`) shrink code instead of growing it.
5. **wasm memory hump**: translate each function to waffle inside its
   pipeline job.
6. **Pressure-aware loop-invariant hoisting**, so `PLIRON_ELABLOOPS` can
   be default-on.
7. **`eqbug` compare lowering**, and `uxtb` after `zeroext` bool call
   results (trust the callee's extension on Apple arm64).
8. **Compile time on giant functions**: per-instruction cost of the top
   CLIF steps; sharing pliron-IR results for live `xcgu` copies.
9. **Correctness debt**: real SEH catch on windows-msvc; audit
   callee-side IPO on `xcgu` copies for wasm; serialize the remaining
   fact tables in the corpus header.
10. **Re-measure x86-64** (Windows and Linux), native and CubeCL.

## How to measure

- Everything: `harness/parity.py` (~20 min; `--skip
  native,wasm,ctime,stack`, `--from-logs`). Writes
  `target/parity/summary.md`, card first. Run nothing else heavy
  meanwhile.
- One kernel's runtime: `bench/run.sh <name>` / `bench/wasm/run.sh
  <name>`. Placement check: `bench/layout.sh <kernel> [N]` (relinks under
  shuffled symbol orders, reports the cross-layout median) and
  `bench/wasm/run.mjs --layouts=N`. V8 attribution tools: `harness/v8/`.
- Compile time: `harness/ctime.py [--only regex-syntax]` (release
  dylib; also records peak RSS and machine-code bytes). Inside one
  compile: `PLIRON_TIMING=1`, `PLIRON_TRACE_FILE=t.txt`,
  `-Zself-profile`, `-Ztime-passes`, or `samply record --save-only
  --unstable-presymbolicate` + `harness/sprof.py`.
- CLIF pipeline alone, no rustc: `clif-check bench target/corpus/std`, or
  the live dashboard's step-time panel (`harness/live/live.py`).
- Stack: `harness/frames.py [--input regex-syntax]`.
- Code size per function: `llvm-nm --no-llvm-bc -n` + `llvm-size -A`
  over the rlib members, joined on symbol; per-pass attribution with each
  `PLIRON_<PASS>=0`.
- Noise: ±3% is parity on a quiet machine. Criterion compares against an
  LLVM baseline measured minutes earlier; wasm uses interleaved rounds.
  Per-step span times under the CGU pool read high (descheduled time);
  trust A/B `user` time for pass-level costs.

## History

- Scoreboards and card snapshots: `history/2026-10-30-parity-card.md`
  (the current scoreboard's full run), `history/2026-10-22-parity-scoreboard.md`
  (2026-10-18 to 10-22, incl. Windows i7), `history/2026-10-27-parity-card.md`
  (first full cards), `history/2026-10-28-parity-card-snapshot.md`.
- Former PARITY sections: `history/2026-10-20-compile-time-worklist.md`,
  `history/2026-10-21-wasm-runtime.md`, `history/2026-10-27-native-runtime.md`.
- wasm experiment ledger: `history/2026-10-21-wasm-experiments.md`.
- Narrative of the perf work, oldest first: [`performance.md`](performance.md).
- LLVM's -O2 pass list vs ours (2026-10-10, partly stale):
  `history/llvm-passes.md`.
