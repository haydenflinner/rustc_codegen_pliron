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

1. **Iterate with `harness/quick.py`** (tier 1 default, ~2 min, or
   `--tier 0`, seconds when warm): it reads `git diff`, builds, runs only the
   touched crates' unit tests, replays the CLIF corpora (when
   `pliron-clif-opt`, `pliron-cfg` or `vendor/` changed), runs the
   `test.sh` stanzas those paths select, `wasm_smoke`, and the fuzz
   regression seeds plus a rolling window of fresh ones, then prints what
   it skipped and the slow command that completes the proof. Green quick
   is ~90% confidence, not a merge licence.
   **Run `harness/gate.py` before merging to main** (~40 s — the wasm
   sysroot leg is off by default; `--full` or `--only wasm` adds it for
   wasm/wpeep/linker changes. `wasm_smoke`, always on, covers
   emit->link->run in ~1 s; `--install-hook` makes the gate a git
   pre-push hook). main stays green: a red gate is fixed before new work
   lands.
2. **New passes and new transforms land default-off**
   (`env_is("PLIRON_X", "1")`), then flip on in a separate commit once
   the gate and the corpus are clean, by adding the name to
   `DEFAULT_ON` (or a value to `DEFAULT_VALUES`) in `pliron-cfg/src/lib.rs`.
   Every pass keeps a `PLIRON_<PASS>=0` off switch.
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
| `src/` | rustc glue (needs `rustc_private`): `lib.rs` (driver; `finish_module` is the serial path), `cgupool.rs` (CGU job pool and the IR pass order: `finish_pre` → `iropt_head` → `iropt_tail` → `finish_b`), `builder.rs`/`intrinsic.rs`/`simd.rs`/`abi.rs`, `asm.rs` (`asm!` wrappers), `lto.rs` (fat LTO as a relocatable link), `wasm.rs` (CLIF→waffle emit, waffle inliner, `wpeep` byte peepholes) with `wpeep_audit.rs`/`wpeep_elide.rs`, `xcgu.rs` (cross-CGU body copies) |
| `pliron-ir-opt/` | pliron-IR passes: inline, sroa, memcpyopt, spec, phisimp, instcombine, constload, guardelim, callgvn, IPO summaries (nowrite/nocapture/nnret/retarg/nounwind/willreturn as `ascent!` Datalog), `State` side tables. Stable Rust. |
| `pliron-clif-lower/` | pliron IR → CLIF (`lower.rs`), parallel per-function lowering; `scalarize.rs` (vector legalization), `eh/` (`.eh_frame`/LSDA, COFF `.pdata`/`.xdata`), `objmerge.rs` (rsasm output spliced into the object), `hot.rs` (`PLIRON_HOT`), `dwarf.rs` |
| `pliron-clif-opt/` | all CLIF passes (the step list and order are `pipeline.rs::DEFAULT`), the differential checker (`diffcheck.rs`), the step and in-process pipeline caches (`stepcache.rs`). Stable Rust, `cargo test -p pliron-clif-opt` needs no rustc. |
| `pliron-clif-check/` | `clif-check`: run/check/time CLIF passes on saved functions, no rustc |
| `pliron-cfg/` | the one-shot `PLIRON_*` env parse (`pass_enabled`, `env_*`, bisect counters, `PLIRON_OPT_FNS`); `DEFAULT_ON`/`DEFAULT_VALUES` are the single list of opt-in knobs that are on by default |
| `vendor/` | patched Cranelift, regalloc2, pliron, rsasm, waffle; changes are marked `PLIRON:` in the source and listed in the crate's `PLIRON_PATCH.md` (`vendor/PLIRON_REV` pins pliron's upstream revision) |
| `harness/` | gate, corpus, live dashboard, bisectors, compile-time and disk tools |
| `bench/` | runtime kernels (benchfmt/benchstat native, `bench/wasm/` node) |
| `tests/` | `test.sh` programs (one dir per stanza) |
| `docs/` | `PARITY.md` (start here), `miscompile-triage.md`, `test-harness.md`, `audit.md` (audited wasm rewrites), `determinism.md`, `pgo.md`, `phi-web.md`, `platforms.md` (Windows/CRT-free, cross targets, self-host), `seh-design.md` (planned real SEH catch), `lutcorr.md`, `rustc-thorns.md`, `cubecl-burn.md`/`cubecl-burn-host.md`, `performance.md` (narrative history of the perf work), `history/` (dated logs) |

`black_box` is a real optimization barrier: a call to a never-defined
`__pliron_bb*` symbol (opaque to every IR pass) that `lower.rs` turns
into the CLIF `black_box` op (zero instructions). Don't "simplify" it to
a pass-through. wasm deliberately keeps an alloca + volatile-load round
trip instead (`intrinsic.rs`, `clif_bb = !is_like_wasm`): that is what
LLVM emits on wasm too, and a bare identity would let V8 fold through it
and make our wasm benchmarks look better than they are.

## Build

Pinned toolchain `nightly-2026-10-22` (`rust-toolchain.toml`; needs
`rustc-dev`, `rust-src`, `llvm-tools-preview`).

- `cargo build` → `target/debug/librustc_codegen_pliron.{dylib,so}`,
  ~3–5 s incremental. Our crates build at O1, deps and vendored
  Cranelift at O3. Measure compile time with the **release** dylib
  (`cargo build --release`); the debug one is ~1.3x slower.
- Inner loop after a pass edit: `harness/dev.py` (`clif`, `corpus`, `rs`,
  `test`, `check`, `bench`, `watch ...`) builds with the `fast` profile
  (`target/fast`, 2-4x quicker to rebuild than `dev`) and prints
  before/after; `PLIRON_PROFILE=fast` points gate/quick/test.sh/run.py at
  the same build. Numbers and details: [`docs/dev-loop.md`](docs/dev-loop.md).
- One file: `rustc -O -Zcodegen-backend=$PWD/target/debug/librustc_codegen_pliron.dylib file.rs`
- cargo does not track the backend dylib or `PLIRON_*` vars: after
  changing either, use a fresh `CARGO_TARGET_DIR` or `cargo clean -p`.

## Pick the loop for your task

`harness/parity.py` measures all of it (native + wasm runtime, compile
time, memory, code size, stack frames) in ~20 min and writes
`target/parity/summary.md`.

### Speed of generated code (runtime parity)

- `bench/run.sh [filter]` — native kernels (benchfmt::Suite, vendored
  `benchfmt.rs`): `COUNT` steady-state `-benchtime` epochs per side,
  `benchstat` per-kernel change with Mann-Whitney p-values, `B/op` and
  `allocs/op` via `CountingAlloc`, `-cpuprofile`/`-memprofile` pprof.
  Medians append to `bench/history.tsv` (commit it when a number moves).
  ~8 min for all; `BENCHTIME`/`COUNT` env vars tune it.
- `bench/wasm/run.sh [filter]` — same kernels on wasm32 in node.
- `bench/heldout/ab.py PROG SIDE...` — the fast A/B for one held-out
  program. Sides: `llvm`, `llvm-pgo`, `llvm:FLAGS` (ablate LLVM's own
  passes), `pliron`, `NAME:K=V,..`, `pgo`, `pgo-NAME:K=V,..`
  (profiled), `@FLAGS` for extra RUSTFLAGS. Builds are cached per
  side; each side is timed interleaved under `/usr/bin/time -l` and
  reported with retired instructions (repeatable to ~0.3%), cycles and
  IPC — so one run says whether a change removes work or only moves
  it. `--prof` adds inclusive/self per-function cycle deltas. Start
  here for any "is X faster" question on real programs.
- `bench/heldout/run.py --pgo` — the profile loop for all eight
  programs, ours and LLVM's, with build-time totals; `tune.py PROG`
  searches the profile knobs for one program. `harness/pgofuzz.py`
  fuzzes the generate/run/use loop. `PLIRON_REMARKS` +
  `harness/layout_score.py` score a profiled build's block layout.
  State of the profile work: `docs/pgo.md`,
  `docs/history/2026-11-12-pgo-baseline.md`.
- `bench/heldout/irdiff.py PROG [--env K=V] [--tag T] [--fn S | --line
  FILE:LINE] [--backend DYLIB]` — exact executed-instruction counts
  (callgrind, aarch64-linux-musl in docker), ours against LLVM's, per
  function and source line; all eight held-out programs run. `--tag`
  gives our side its own build dir so several run at once (delete
  `target/heldout/ir-pliron-<tag>` afterwards: ~0.5 GB each). The
  function table is by innermost inlined function (DWARF), not by ELF
  symbol.
- `bench/heldout/asmvs.py [--only prog] [--fn F | --line FILE:LINE]` —
  our code next to LLVM's for a held-out program, keyed by source line
  (so inlining differences don't break the pairing) with every
  instruction classified (stack load/store, register copy, extend,
  ...) and weighted by self samples: a class table of what the gap is
  made of, the lines that lose most, and side-by-side listings. `--fn`
  ends with the function's optimization remarks.
- `PLIRON_REMARKS=<prefix>` + `harness/remarks.py <prefix> [--fn F]
  [--callee C]` — one fixed-shape record per decision *not* to
  optimize: each call the inliner kept (reason, cost vs threshold, cap,
  growth) and each loop `loopvec` left scalar. New passes report
  refusals with `pliron_cfg::remark!`.
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
  `docs/history/2026-10-21-wasm-experiments.md` first for what has
  already been tried.

### Compile time

- `harness/ctime.py` — stock vs pliron (release dylib) on the bench
  kernels (native and wasm), regex-syntax (native and wasm) and syn;
  median of 3, ratios, plus peak RSS and machine-code bytes. These are
  the card's CMP, MEM and SIZ inputs.
- `clif-check bench target/corpus/std [reps]` — the CLIF pipeline alone
  over ~15k real functions, per-step time table, no rustc. The fastest
  loop for pass compile-time work; the live dashboard shows the same
  per-step times against a baseline on every save.
- `harness/headstart.py record --heldout PROG [--pliron]` (or `-- cargo
  build ...`) — a cargo build's schedule: per rustc, when interfaces
  were checked, metadata written, code generated; the critical path;
  and the same build replayed under early-metadata pipelining
  (`docs/headstart.md`, a prediction).
- `PLIRON_TIMING=1` — per-CGU per-step wall time for one real compile.
  `-Zself-profile` records `pliron_*` activities next to rustc's.
- `harness/prof.py -- rustc ...` (or `-p PID`, `-a FILE`) — wraps
  `/usr/bin/sample` and prints self-time buckets + per-thread
  busy/idle. Use this *first* for "where does the time go" before
  adding instrumentation; it needs no rebuild and symbolizes through
  dSYM. If `sample` hangs (it can wedge system-wide), use
  `samply record --save-only --unstable-presymbolicate -o p.json.gz --
  rustc ...` and `harness/sprof.py p.json.gz [--under FN]`.
- Known structure of the gap (details in PARITY.md): per-instruction
  cost of our passes on giant functions and IR-opt dominate, not
  Cranelift's own codegen. CGUs run IR-opt and lowering in parallel on
  the `src/cgupool.rs` job pool; the MIR → pliron front half is still
  serial on the main thread.

### Correctness

- **Triage order for a miscompile**: [`docs/miscompile-triage.md`](docs/miscompile-triage.md)
  — set up the oracle, then `PLIRON_DIFFCHECK=step` first, then
  `harness/fnbisect.py` (which function), then corpus replay (fast but
  lossy: a clean replay clears nothing), then `PLIRON_PIPELINE` prefix /
  `PLIRON_OPT_BISECT` bisects (rebuild per probe), then
  `PLIRON_TRACE=pliron::<step>` inside the named step.
- **Corpus**: `harness/corpus.sh smoke|std|cargo DIR` records every
  function's pre-pipeline CLIF + facts into `target/corpus/<set>`.
  `clif-check report target/corpus/std out.json --check` runs the whole
  pipeline + an interpreter diff on every function in parallel (~7 s
  for std). Re-record after lowering or IR-opt changes.
  `PLIRON_VERIFY=full` adds the verifier after every step (a failing
  step is a `panic` row naming it; the gate and quick set it), `--every
  N` replays a sample. `harness/corpus.sh clcg` records our vendored
  cranelift-codegen and its dependencies (~38k functions, ~1.5 min):
  compiling our own crates found bugs std does not show. The gate
  replays every 8th function of it (~20 s), `gate.py --full` all of it.
- **Real crates**: `harness/crates.py [--tier 1|2|3] [--only a,b]
  [--deps stock|pliron]` runs pinned crates' own test suites
  (`--release`) under stock and pliron and reports pliron-only failures,
  crashed test binaries included (tier 2 adds atomics/threads/async
  crates, tier 3 large applications; logs and `report.md` in
  `target/crates/`). The default `--deps stock` builds dependencies with
  stock rustc and sends only the crates' own code through the backend
  (no doc-tests); `--deps pliron` compiles dependencies and doc-tests
  with pliron too. Narrow a failure with a probe script (touch the
  crate's `src/lib.rs`, rerun the failing test) → `harness/fnbisect.py`
  → `PLIRON_OPT_FNS=~fn PLIRON_OPT_BISECT=N` (licm load hoists are
  separate `licm-load` units).
- **Live loop**: `harness/live/live.py` → http://localhost:8765. On
  every save to a pass it rebuilds `clif-check`, replays the corpora and
  shows new failures (with repro commands), size movers and step times.
- **Which function?** `harness/fnbisect.py [--names F] -- <cmd>`
  delta-debugs `PLIRON_OPT_FNS` (optimize only selected symbols; stable
  across CGUs, processes, rebuilds). `<cmd>` exits nonzero when broken
  and must recompile. Beware: it accepts *any* failure, so check the
  minimized set reproduces the original symptom. Also, a few
  interprocedural rewrites still touch unselected bodies (`spec` rewrites
  a callee's parameter uses, `retarg` folds call results everywhere,
  `nounwind` turns invokes into calls everywhere), so a selected
  function can be blamed for a change made in an unselected one.
- **Which transform?** `PLIRON_OPT_BISECT=N` allows only the first N
  pass applications (CLIF steps, sroa splits/forwards, constload folds);
  `PLIRON_OPT_BISECT_DEBUG=1 PLIRON_TRACE=pliron::cfg=debug` names each
  unit. The numbering is deterministic: under `PLIRON_OPT_BISECT` the
  backend itself takes the serial paths (no CGU job pool, serial
  lowering; `cgupool::enabled`, `lower.rs`). The step and pipeline caches
  also turn off. `harness/optbisect.py -- <cmd>` automates it when
  `<cmd>`'s stderr is passed through.
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
- **Audits** (`docs/audit.md`): `PLIRON_WASM_AUDIT=1|panic` checks
  every wpeep copy-forward round with a Datalog translation validator
  (the code and the rewrite's assumptions as one fact stream) and
  reverts rounds that fail; `tests/wasm/run.sh` and `tests/wasm/smoke.sh`
  set `=panic` (so it is in the gate). Unused-`block`/`loop` elision
  (`PLIRON_WASM_ELIDEBLK`, default on) checks its own assumptions on
  every round, audit flag or not, and reverts on failure (`=panic`
  panics instead). The other wpeep rewrites are not audited.
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
  Opt-in knobs that are on by default are listed in `pliron-cfg/src/lib.rs`
  (`DEFAULT_ON`, `DEFAULT_VALUES`); the vendored Cranelift and waffle
  read a few of their own the same way (named in the `DEFAULT_ON` doc
  comment and the `PLIRON_PATCH.md` files).
- `PLIRON_PIPELINE=a,b,c` — explicit CLIF step list (`clif-check steps`).
- `PLIRON_OPT_FNS=lo..hi|~substr|@file` (`!` inverts),
  `PLIRON_OPT_FNS_LOG=<file>` — per-function optimization selection.
- `PLIRON_OPT_BISECT=N`, `PLIRON_OPT_BISECT_DEBUG=1` — transform budget.
- `PLIRON_PAR=0|N` — lowering threads.
- `PLIRON_VERIFY=1|full` — Cranelift verifier after the pipeline / each step.
- `PLIRON_RA_EXPLAIN=<symbol substring>` — regalloc2's decisions for
  matching functions (`ra2x:` lines: splits, evictions, weights, loop
  depth, minimal-bundle fallbacks) plus their pre-regalloc VCode, on
  stderr. Use it before guessing why a value is reloaded in a loop.
- `PLIRON_STATS=1`, `PLIRON_TIMING=1`, `PLIRON_FACTS=<substr>`,
  `PLIRON_CLIF=<substr>`, `PLIRON_DUMP=1` / `PLIRON_DUMP_OPT=1` (pliron
  IR before/after IR passes).
- `PLIRON_TRACE=<env-filter>` — all logging (`pliron` = everything;
  targets `pliron::<pass>`, `pliron::stats`, `pliron::timing`,
  `pliron::dump`). Old `PLIRON_*_DEBUG`/`_PROF` knobs map onto it
  (`KNOB_TARGETS` in `src/lib.rs`).
- `PLIRON_STEPCACHE=1` (opt-in) — content-addressed on-disk per-step
  cache (`~/.cache/pliron-steps`); keyed on pass sources, vendored
  Cranelift and Cargo.lock, so edits cold-miss.
- `PLIRON_MEMCACHE` (default on; `=0` off) — in-process whole-pipeline
  cache sharing optimization results across CGU-duplicate functions
  (xcgu copies, shared generics). Symbol-normalized keys, `FuncId`
  remapping on replay. `PLIRON_MEMCACHE_MB` caps it (default 1024).
  Both tiers turn off under `PLIRON_OPT_BISECT` — a hit can't honor the
  consumer's remaining budget. `PLIRON_MCPROF=1` prints
  lookup/store/record counters per CGU.
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
- `PLIRON_CGU_MEMCAP=<MiB>` (opt-in) — memory-pressure throttle on the
  CGU pool: a stage-A job is admitted only if current footprint plus the
  committed growth of running jobs plus its own estimate stays under the
  cap. Deadlock floors: one job always runs, and an ordinal a parked
  stage-B job waits on is exempt. macOS only (`footprint_mib`).
- `PLIRON_BUNROLL_BODY=<insts>` — `bunroll` per-body inst cap
  (default 48). ~200 exposes memcpy'd chunk loops to `loadfwd`'s
  source-copy rule; costs .text on big crates.
- `PLIRON_PUNROLL_SEARCH=1` (opt-in) — `punroll` also group-unrolls
  search loops (a data exit into warm code: `position`, `any`,
  `take_while(..).count()`). Off by default without a measured trip
  count: pulldown-cmark's scanners make a trip or two and paid the
  guard (exact counts: pulldown -0.4%, html-rewriter -0.2% with the
  rule). Counted loops and loops whose other exits are cold unroll
  either way.
- `PLIRON_ELABHOT=1` (opt-in, vendored elaborator) — restore a popped
  loop-stack entry for blocks that dominate the latches of a call-free
  loop; hoists out of one-trip loops (pulldown-cmark +0.5%).
- `PLIRON_WM_KEEPCOUNT=0`, `PLIRON_EXITSINK=0`, `PLIRON_CMPCANCEL_INV=0`,
  `PLIRON_SELKNOWN=0`, `PLIRON_RETEST=0`, `PLIRON_DEADSLOT=0` — the
  byte-scan loop work of 2026-11-13
  (`docs/history/2026-11-13-six-features.md`, "Fourth item"), each
  default on.
- `PLIRON_PUNROLL_COUNTED=1` (opt-in) — `punroll` also group-unrolls
  plain counted loops whose trip count is neither measured nor a
  constant (the default until 2026-11-13; off: rust-protobuf -0.7%,
  html-rewriter -0.4% exact). Under a profile the floor is
  `PLIRON_PUNROLL_MINTRIP` (default 4 average trips);
  `PLIRON_PEEL_MINTRIP` (default 0) is the same for peeling.
- `PLIRON_LOOPROT_MERGE=1` (opt-in) — one latch for a loop with several
  back edges so `looprot` can rotate it. The repro
  (`tests/parity/prog_loop.rs`) is at 11 instructions a skipped state
  (LLVM 12) since `PLIRON_ELABKEEP`, `PLIRON_BUMPEDGE` and
  `PLIRON_WM_WIDE`; exact counts are still mixed (html-rewriter -0.39M,
  rust-json -0.17M, pulldown-cmark +0.78M, regex +0.25M, blind-sig
  +0.29M): regalloc2 rotates registers at the merged latch (see
  `looprot::merge_latches`).
- `PLIRON_LICM_INNERPRESS` (default `2`; `=1` the old rule, `=0` off),
  `PLIRON_UNSWITCH_LOOP=0`, `PLIRON_LF_TRIVPHI=0`, `PLIRON_ELABKEEP=0`
  (vendored elaborator), `PLIRON_BUMPEDGE=0`, `PLIRON_WM_WIDE=0`,
  `PLIRON_PROBE_SP=0` (vendored aarch64 emitter) — the 2026-11-13
  parity round (`docs/history/2026-11-13-six-features.md`, "Sixth
  item"): pressure-aware hoisting of outer-loop loads, loop-only
  unswitching, no single-value block params from `loadfwd`'s PRE,
  elaboration keeps the pipeline's hoists, a cursor stepped on the
  edges into the block that only steps it, `webmerge` keeps the wide
  slot of an extended pair, and a stack-probe loop that moves `sp`
  (valgrind).
- `PLIRON_ALLOC_JMP=0`, `PLIRON_VEC_SHORT=0`, `PLIRON_SELXOR=0`,
  `PLIRON_LICM_DOMCOND=0` — off switches for the allocator shim as one
  jump (aarch64), `loopvec`'s short-trip dispatch, the zigzag select
  fold, and the dominated-branch fold ahead of LICM
  (`docs/history/2026-11-13-six-features.md`, "Fifth item").
- `PLIRON_FASTCC=0` — off switch for per-callee clobber sets (aarch64
  ELF/Mach-O, default on): a direct call to a function compiled in the
  same object destroys only the registers that function's code writes
  on a path that returns, so the caller keeps values in caller-saved
  registers across it. `PLIRON_FASTCC_NORET=0` counts paths that never
  return too, `PLIRON_FASTCC_HIDDEN=0` limits callees to internal
  linkage. `PLIRON_TRACE=pliron::fastcc` prints one `fastcc-sites` line
  per CGU (what happened to every direct call site); with `=debug` and
  `PLIRON_FASTCC_NORET=0`, `harness/fastcc_check.py LOG OBJECTS...`
  checks every recorded set against the disassembled code. Exact
  counts: pulldown-cmark -0.55%, the other held-out programs unchanged
  (`docs/history/2026-11-13-six-features.md`).
- `PLIRON_PANICSTUB=0` — off switch for shared panic tails (default on,
  native only): the cold `call f(..); trap` tails of one callee in a
  function become one block (.text -0.15% on three held-out programs,
  executed instructions unchanged). `PLIRON_PANICSTUB_STRIP=0` keeps
  each site's `coldargs` wrapper, `PLIRON_PANICSTUB_FWD=0` keeps
  forwarder blocks. Sites on different source lines stay apart when the
  build has line tables.
- `PLIRON_INLINE_PAIR=<k>` (opt-in) — callees called from at most `k`
  functions get 2x their callsite limit (the body dies when all sites
  inline, LLVM's last-callsite bonus generalised). Reaches
  pulldown-cmark's `scalar_iterate_special_bytes` at k=2, but the
  held-out A/B is net-negative (regex +8% slower) — `fanin` still
  applies; no promotion path.
- `PLIRON_SRETNC=1` (opt-in) — the sret out-pointer counts as a bounded
  call operand (`CallInfo::sret_nocap`), so slotprom syncs the
  out-slot's covered fields instead of pinning it to memory. Costed
  +1.7–4.7KB .text on regex-syntax for no measured runtime — dormant
  by default.
- `PLIRON_SLOTPROM_EVICT=1` (opt-in) — over `MAX_FIELDS`, evict
  least-accessed slots to fit instead of the all-or-nothing bail. The
  bail doubles as a profitability bound; partial promotion measured
  +5.9KB .text on regex-syntax for no runtime gain.
- `PLIRON_SLOTPROM_EXT=0` — slotprom whole-slot call coverage, i.e.
  revert extent-limited syncs (`nocap` carries per-operand byte
  extents from `arg_extent`).
- `PLIRON_RA2_POSTOPT=<bits>` (opt-in) — regalloc2 post-allocation
  cleanup over the whole CFG: 1 redundant reloads, 2 reloads served
  from a register that already mirrors the slot, 4 redundant spill
  stores, 8 dead spill stores (`vendor/regalloc2/src/ion/postopt.rs`;
  `PLIRON_RA2_POSTOPT_STATS=1` counts). Check with `PLIRON_RA_CHECK=1`.
- `PLIRON_RA2_SRCVREG=1` (opt-in) — a block-parameter move is labelled
  with the branch argument's vreg, so a constant passed to a spilled
  parameter is rematerialized instead of loaded from its own spill
  slot (stack loads -3.5..-4.3% on three held-out binaries).
- `PLIRON_ELABLOOPS=2` (opt-in) — `=1`'s egraph loop hoisting, but
  cheap invariants stay inside loops that contain a call.
- `PLIRON_HOT_LAYOUT=1` — type-layout table in each object for the
  hot-reload loader's layout guard; `PLIRON_HOT_REACH=1` — thunks mark
  their function entered (tests on save, `examples/hot-test.sh`);
  runtime `PLIRON_HOT_DIRECT=1` (direct-jump thunks, arm64 macOS) and
  `PLIRON_HOT_FREE=protect` (`docs/hot-reload.md`).
- `clif-check bench <corpus>` (with `PLIRON_MEMCACHE=0`) also prints
  each step's time on functions it left unchanged and the repeat rounds
  that ran with nothing changed since the family's last no-effect run.
- `PLIRON_VERSION=1` — print the source revision the dylib was built
  from (check it before trusting a self-host or bisect result).

## Housekeeping

- `harness/disk.sh [--sweep|--prune]` — build-dir sizes; drop stale
  build units / scratch `target-*` dirs.
- Worktrees: reclaim space with `cargo clean` inside one rather than
  `git worktree remove`, which loses uncommitted work.
