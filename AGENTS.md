# Agent instructions

`rustc_codegen_pliron` is a rustc codegen backend with no LLVM:
rustc MIR → rustc_codegen_ssa → pliron LLVM dialect (+ pliron-IR passes)
→ Cranelift IR (+ CLIF passes) → cranelift-object (native) or waffle
(wasm) → linker.

**Current goal: performance parity with stock rustc+LLVM, native and
wasm, for both the speed of generated code and compile time.** Where we
stand and what to work on next: [`docs/PARITY.md`](docs/PARITY.md).
Read it before picking a task.

## Rules

1. **Run `harness/gate.py` before merging to main** (~40 s — the wasm
   sysroot leg is off by default; `--full` or `--only wasm` adds it for
   wasm/wpeep/linker changes. `wasm_smoke`, always on, covers
   emit->link->run in ~1 s). main stays green: a red gate is fixed
   before new work lands.
2. **New passes and new transforms land default-off**
   (`env_is("PLIRON_X", "1")`), then flip on in a separate commit once
   the gate and the corpus are clean. Every pass keeps a
   `PLIRON_<PASS>=0` off switch.
3. **A miscompile fix ships with a regression test**, and the test must
   fail without the fix (check it). Pass unit tests live next to the
   pass or in `pliron-clif-opt/tests/`; end-to-end ones are `test.sh`
   stanzas.
4. **Keep passes linear** — see "Optimizer complexity rules" below.
   stage2 rustc functions are big enough that one O(n²) loop is minutes.
5. Diagnostics go through `tracing` (`PLIRON_TRACE`), not `eprintln!`.
6. Commit and push completed work freely (personal fork). No
   `Co-Authored-By` trailers. Never leave `rustc-ice-*.txt` in the repo
   (scripts set `RUSTC_ICE=target/ices`).
7. Dated logs and superseded notes go in `docs/history/`, not the repo
   root. Update `docs/PARITY.md` when you move a number.

## Layout

| path | what |
|---|---|
| `src/` | rustc glue (needs `rustc_private`): `lib.rs` (driver, pass order in `finish_module`), `builder.rs`/`intrinsic.rs`/`simd.rs`/`abi.rs`, `wasm.rs` (CLIF→waffle emit, waffle inliner, `wpeep` byte peepholes), `xcgu.rs` (cross-CGU body copies) |
| `pliron-ir-opt/` | pliron-IR passes: inline, sroa, memcpyopt, spec, phisimp, instcombine, constload, guardelim, callgvn, IPO summaries (nowrite/nocapture/nnret/retarg/nounwind/willreturn as `ascent!` Datalog), `State` side tables. Stable Rust. |
| `pliron-clif-lower/` | pliron IR → CLIF (`lower.rs`), parallel per-function lowering |
| `pliron-clif-opt/` | all CLIF passes (~80 steps; order in `pipeline.rs::DEFAULT`), the differential checker (`diffcheck.rs`), step cache. Stable Rust, `cargo test -p pliron-clif-opt` needs no rustc. |
| `pliron-clif-check/` | `clif-check`: run/check/time CLIF passes on saved functions, no rustc |
| `pliron-cfg/` | the one-shot `PLIRON_*` env parse (`pass_enabled`, `env_*`, bisect counters, `PLIRON_OPT_FNS`) |
| `vendor/` | patched Cranelift, regalloc2, pliron, rsasm; every change marked `PLIRON:` and listed in each `PLIRON_PATCH.md` |
| `harness/` | gate, corpus, live dashboard, bisectors, compile-time and disk tools |
| `bench/` | runtime kernels (criterion native, `bench/wasm/` node) |
| `tests/` | `test.sh` programs (one dir per stanza) |
| `docs/` | `PARITY.md` (start here), `audit.md` (Datalog rewrite audits), `performance.md` (narrative history of the perf work), `test-harness.md`, `phi-web.md`, `pgo.md`, `rustc-thorns.md`, `platforms.md` (Windows/CRT-free, cross targets, self-host), `history/` |

`black_box` is a real optimization barrier: a call to a never-defined
`__pliron_bb*` symbol (opaque to every IR pass) that `lower.rs` turns
into the CLIF `black_box` op (zero instructions). Don't "simplify" it to
a pass-through. wasm deliberately keeps an alloca + volatile-load round
trip instead (`intrinsic.rs`, `clif_bb = !is_like_wasm`): that is what
LLVM emits on wasm too, and a bare identity would let V8 fold through it
and make our wasm benchmarks look better than they are.

## Build

Pinned toolchain `nightly-2026-10-06` (`rust-toolchain.toml`; needs
`rustc-dev`, `rust-src`, `llvm-tools-preview`).

- `cargo build` → `target/debug/librustc_codegen_pliron.{dylib,so}`,
  ~3–5 s incremental. Our crates build at O1, deps and vendored
  Cranelift at O3. Measure compile time with the **release** dylib
  (`cargo build --release`); the debug one is ~1.3x slower.
- One file: `rustc -O -Zcodegen-backend=$PWD/target/debug/librustc_codegen_pliron.dylib file.rs`
- cargo does not track the backend dylib or `PLIRON_*` vars: after
  changing either, use a fresh `CARGO_TARGET_DIR` or `cargo clean -p`.

## Pick the loop for your task

`harness/parity.py` measures all of it (native + wasm runtime, compile
time) in ~15 min and writes `target/parity/summary.md`.

### Speed of generated code (runtime parity)

- `bench/run.sh [filter]` — native criterion kernels, pliron vs a saved
  LLVM baseline, per-kernel change with significance. ~6 min for all.
- `bench/wasm/run.sh [filter]` — same kernels on wasm32 in node.
- To see why a kernel is slow: `PLIRON_CLIF=<fn substr>` dumps CLIF
  after every step; `rustc --emit obj` + `llvm-objdump -d` (or
  `wasm2wat`) for final code; compare with stock rustc's output for the
  same function. `PLIRON_STATS=1` shows which passes fired.
- `PLIRON_<PASS>=0` and `PLIRON_PIPELINE=a,b,c` change the pipeline with
  no rebuild — try them before editing code.
- The live dashboard's "size movers" panel shows which corpus functions
  your edit made bigger or smaller.
- wasm runtime under V8: `harness/v8/` (per-function and per-instruction
  tick attribution, TurboFan listings, bench variants) — read
  `docs/experiments.md` first for what has already been tried.

### Compile time

- `harness/ctime.py` — stock vs pliron (release dylib) on the bench
  kernels (native and wasm) and regex-syntax; median of 3, ratios.
  This is the number in PARITY.md.
- `clif-check bench target/corpus/std [reps]` — the CLIF pipeline alone
  over ~15k real functions, per-step time table, no rustc. The fastest
  loop for pass compile-time work; the live dashboard shows the same
  per-step times against a baseline on every save.
- `PLIRON_TIMING=1` — per-CGU per-step wall time for one real compile.
  `-Zself-profile` records `pliron_*` activities next to rustc's.
- `harness/prof.py -- rustc ...` (or `-p PID`, `-a FILE`) — wraps
  `/usr/bin/sample` and prints self-time buckets + per-thread
  busy/idle. Use this *first* for "where does the time go" before
  adding instrumentation; it needs no rebuild and symbolizes through
  dSYM. If `sample` hangs (it can wedge system-wide), use
  `samply record --save-only --unstable-presymbolicate -o p.json.gz --
  rustc ...` and `harness/sprof.py p.json.gz [--under FN]`.
- Known structure of the gap (details in PARITY.md): our glue (op
  construction, side-table hashing, lowering) and IR-opt dominate, not
  Cranelift; CGUs run their IR passes serially.

### Correctness

- **Triage order for a miscompile**: [`docs/miscompile-triage.md`](docs/miscompile-triage.md)
  — diffcheck first, corpus replay next (with its fidelity caveats),
  rebuild bisects last.
- **Corpus**: `harness/corpus.sh smoke|std|cargo DIR` records every
  function's pre-pipeline CLIF + facts into `target/corpus/<set>`.
  `clif-check report target/corpus/std out.json --check` runs the whole
  pipeline + an interpreter diff on every function in parallel (~7 s
  for std). Re-record after lowering or IR-opt changes.
- **Real crates**: `harness/crates.py [--tier 1] [--only a,b]` runs pinned
  crates' own test suites (+ doc-tests, `--release`) under stock and
  pliron and reports pliron-only failures (~10 min for tier 1; logs and
  `report.md` in `target/crates/`). It found four miscompiles on its first
  run. Narrow one with a probe script (touch the crate's `src/lib.rs`,
  rerun the failing test) → `harness/fnbisect.py` → `PLIRON_OPT_FNS=~fn
  PLIRON_OPT_BISECT=N` (licm load hoists are separate `licm-load` units).
- **Live loop**: `harness/live/live.py` → http://localhost:8765. On
  every save to a pass it rebuilds `clif-check`, replays the corpora and
  shows new failures (with repro commands), size movers and step times.
- **Which function?** `harness/fnbisect.py [--names F] -- <cmd>`
  delta-debugs `PLIRON_OPT_FNS` (optimize only selected symbols; stable
  across CGUs, processes, rebuilds). `<cmd>` exits nonzero when broken
  and must recompile. Beware: it accepts *any* failure, so check the
  minimized set reproduces the original symptom.
- **Which transform?** `PLIRON_OPT_BISECT=N` allows only the first N
  pass applications (CLIF steps, sroa splits/forwards, constload folds);
  `PLIRON_OPT_BISECT_DEBUG=1 PLIRON_TRACE=pliron::cfg=debug` names each
  unit. Deterministic under `-C codegen-units=1` or `PLIRON_PAR=0`.
  `harness/bisect.py -- <cmd>` automates it when `<cmd>`'s stderr is
  passed through.
- **Is that step wrong?** `PLIRON_DIFFCHECK=1` (`=step` for every step)
  interprets CLIF before/after on random inputs and saves
  `before.clif`/`after.clif` on a mismatch; `clif-check step <step>
  before.clif` replays in a second. Limits: the interpreter runs on the
  host ISA, so wasm32 (i32-pointer) functions come back inconclusive.
- **wasm-only stages** (after CLIF): the waffle inliner
  (`PLIRON_WASM_INLINE=0`), byte peepholes (`PLIRON_WASM_PEEP=0`,
  `PLIRON_WASM_PEEP_LIMIT=N` peepholes only the first N bodies — bisect
  it; `PLIRON_WASM_PEEP_MOD=<substr>` + `PLIRON_WASM_PEEP_FI=N` target a
  deterministic module-local body instead — the global counter jitters
  under parallel CGUs — and `PLIRON_WASM_TRACE_BODY=N` dumps the body
  after each `wpeep` sub-pass, `PLIRON_WASM_BODYDUMP=<dir>` saves raw
  pre/post bytes), `PLIRON_WASM_DUMP=<substr>` dumps waffle bodies,
  `PLIRON_WASM_STUBLOG=<file>` lists functions that fell back to stubs.
- **Audits**: `PLIRON_WASM_AUDIT=1|panic` checks every wpeep
  copy-forward round with a Datalog translation validator (the code and
  the rewrite's assumptions as one fact stream; `docs/audit.md`) and
  reverts rounds that fail. On in `tests/wasm/run.sh` and `smoke.sh`
  (so in the gate).
- `harness/run.py --tier 0|1` — smoke/determinism/UI suites diffed
  against stock rustc, ratcheted in `harness/expectations/`
  (`docs/test-harness.md`). `selfhost/verify.sh` runs rustc's own UI
  suite under a pliron-built stage2 (`docs/platforms.md`).

## Optimizer complexity rules

- Never call a whole-function scan (`layout.blocks()`, `inst_values`) or
  a CFG/domtree rebuild inside a per-inst/per-edge/per-candidate loop.
  "Who uses `v`" is `uses::Uses`; mutation-aware loops keep incremental
  count maps (see `memspec`, `reap_dead`). Analyses come from the
  shared, version-keyed `Analysis` cache in `pipeline.rs`; mutating
  passes call `an.invalidate()`.
- `resolve_aliases`/`value_def` walk alias chains; `compact_aliases`
  runs after mutating steps. Iterating `dfg.values()` or stale pairs
  must use `try_resolve_aliases`/`try_value_def` (chains can end at
  tombstones).
- Every fixpoint has an explicit budget (`MAX_ACC_WALK`, `MAX_OPS`,
  `MAX_REGION_EXTRA`, `seen` sets). A bail leaves code unoptimized,
  never wrong. Converge incrementally with a worklist — never rescan
  the whole function per round.
- N overlapping per-item walks are one walk (one backward cone instead
  of N forward cones; one memoized walk with a shared budget).
- Read `PLIRON_*` config once per pass, not per instruction.

## Knob reference

- `PLIRON_<PASS>=0` — disable a pass (most default on); no rebuild.
- `PLIRON_PIPELINE=a,b,c` — explicit CLIF step list (`clif-check steps`).
- `PLIRON_OPT_FNS=lo..hi|~substr|@file` (`!` inverts),
  `PLIRON_OPT_FNS_LOG=<file>` — per-function optimization selection.
- `PLIRON_OPT_BISECT=N`, `PLIRON_OPT_BISECT_DEBUG=1` — transform budget.
- `PLIRON_PAR=0|N` — lowering threads.
- `PLIRON_VERIFY=1|full` — Cranelift verifier after the pipeline / each step.
- `PLIRON_STATS=1`, `PLIRON_TIMING=1`, `PLIRON_FACTS=<substr>`,
  `PLIRON_CLIF=<substr>`, `PLIRON_DUMP=1` / `PLIRON_DUMP_OPT=1` (pliron
  IR before/after IR passes).
- `PLIRON_TRACE=<env-filter>` — all logging (`pliron` = everything;
  targets `pliron::<pass>`, `pliron::stats`, `pliron::timing`,
  `pliron::dump`). Old `PLIRON_*_DEBUG`/`_PROF` knobs map onto it
  (`KNOB_TARGETS` in `src/lib.rs`).
- `PLIRON_STEPCACHE=1` — content-addressed per-step cache
  (`~/.cache/pliron-steps`); keyed on pass sources, vendored Cranelift
  and Cargo.lock, so edits cold-miss.
- `PLIRON_MEMCACHE=1` — in-process whole-pipeline cache sharing
  optimization results across CGU-duplicate functions (xcgu copies,
  shared generics). Symbol-normalized keys, `FuncId` remapping on
  replay. `PLIRON_MEMCACHE_MB` caps it (default 1024). Both tiers turn
  off under `PLIRON_OPT_BISECT` — a hit can't honor the consumer's
  remaining budget. `PLIRON_MCPROF=1` prints lookup/store/record
  counters per CGU.
- `PLIRON_XCGU=0`, `PLIRON_XFACTS=0` — cross-CGU body copies / fact
  sharing.
- `PLIRON_XNEED=0` — stage-B release falls back to the full ordinal
  prefix instead of waiting only on the ords that own this CGU's
  bodiless decls (adopt's lookups only read those ords' facts, so
  outputs are identical; the need-based wait just unblocks earlier).
- `PLIRON_DEADUNOPT=0` — callerless retained `xcgu` copies emit through
  `Gate::Always` lowerings only. `PLIRON_EARLYDEAD=0` — also marks
  them before the iropt passes so `bodied`/`opt_body` skip them too
  (default-on; marked bodies emit rawer, so object bytes differ;
  disabled under `PLIRON_OPT_FNS`).
- `PLIRON_VERSION=1` — print the source revision the dylib was built
  from (check it before trusting a self-host or bisect result).

## Housekeeping

- `harness/disk.sh [--sweep|--prune]` — build-dir sizes; drop stale
  build units / scratch `target-*` dirs.
- Worktrees: reclaim space with `cargo clean` inside one rather than
  `git worktree remove`, which loses uncommitted work.
