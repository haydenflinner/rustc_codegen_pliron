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
| `docs/` | `PARITY.md` (start here), `miscompile-triage.md`, `test-harness.md`, `audit.md` (audited wasm rewrites), `determinism.md`, `pgo.md`, `phi-web.md`, `platforms.md` (Windows/CRT-free, cross targets, self-host), `backend-path.md` (cargo/sccache and the keyed `-Zcodegen-backend` path), `seh-design.md` (planned real SEH catch), `lutcorr.md`, `rustc-thorns.md`, `cubecl-burn.md`/`cubecl-burn-host.md`, `performance.md` (narrative history of the perf work), `history/` (dated logs) |

**Target facts.** A pass asks `Target` (`pliron-cfg/src/target.rs`):
architecture family (`is_wasm`/`is_aarch64`/`is_x64`), pointer width
(`ptr_bits`, `ptr_bytes`; the CLIF type is
`pliron_clif_opt::target::ptr_type`), the cost-rule word
(`word_bytes`), `opt_size`, register counts (`int_regs`) and named
facts (`cheap_br_table`, `mulhi64`, `own_isa`, `has_regalloc`,
`amode_scale`). It is built once from rustc's target in `src/target.rs`
and reaches IR passes as `st.target` and CLIF steps as `p.target`
(clif-check and tests: `Target::wasm32()`, `Target::host()`,
`target::of_isa(isa)`). The rule: no literal word sizes, no literal
pointer types, no private `wasm: bool` in a pass. A gate that only
records where something was measured stays `target.is_wasm()` with a
comment saying so; a new fact becomes a field or method when a pass
needs it. Lints: `pliron-clif-opt/tests/no_hardcoded_pointer_type.rs`
(address types and defaulted pointer types) and
`pliron-ir-opt/tests/no_target_literals.rs` (no `ptr32()` or
pointer-width CLIF type in IR passes). Not in `Target`: CPU features
(`Pipe::simd`, `Pipe::cmp64`) and the lowering's ABI (`Pipe::tcfg`, its
own `wasm` flag); `types::PTR32` is the layout's mirror of
`ptr_bits`.

`black_box` is a real optimization barrier: a call to a never-defined
`__pliron_bb*` symbol (opaque to every IR pass) that `lower.rs` turns
into the CLIF `black_box` op. Don't "simplify" it to a pass-through,
and don't let a pass number, hoist or forward it (`cse_key` returns
`None` for it: two executions are two opaque values). Native emits zero
instructions for the op. wasm emits what stock rustc emits there: a
store to a frame slot of its own and a reload (`clif_emit`,
`P::BlackBox`), because a bare identity would let V8 fold through it
and make our wasm benchmarks look better than they are. The reload is
a *volatile* load: the emitter gives it an alignment hint below its
width (`i32.load align=1`), which engines ignore and `wpeep` reads as
"never forward to or from this access". Until 2026-11-16 wasm used an
alloca and a volatile load in the IR instead, and our own CLIF passes
folded it (`k_fib`, `k_eqbug` and hmget's loop ran with no barrier);
`PLIRON_WASM_BBOP=0` restores that form for comparison.
`tests/wasm/smoke` checks the reload is still in the module.

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
- **Unit tests on save: relax.** `../relax` (sibling checkout; override
  with `RELAX=<path>`) is a from-scratch Rust JIT that compiles and runs
  `pliron-clif-opt` / `clif-check` from source. Prefer it over
  `cargo test` when iterating on a CLIF pass or a shape/`tests/*.rs`
  oracle: after `harness/dev.py relax-build` once,
  `harness/dev.py test [FILTER]` (or `--test NAME`) runs
  `relax test --live` — tests stay loaded, a save recompiles only the
  changed functions and reruns the tests that entered them (~1–3 s vs
  ~35 s for `cargo test -p pliron-clif-opt --lib licm`). Use a filter;
  the first cold run and `target/relax` cache are large. Also
  `harness/dev.py test --test parse_digit_llvm_shape`,
  `harness/dev.py relax ARGS` (`clif-check` under the JIT). Cannot run
  the backend dylib (`rustc_private`) or `pliron-ir-opt` tests (no
  `linkme`/`inventory` sections). `dev.py` sets `RELAX_LAZY_BODIES=0`.
  Full notes: [`docs/dev-loop.md`](docs/dev-loop.md) "Unit tests on
  save: relax".
- One file: `rustc -O -Zcodegen-backend=$PWD/target/debug/librustc_codegen_pliron.dylib file.rs`
- cargo does not track the backend dylib or `PLIRON_*` vars, and neither
  does sccache (`~/.cargo/config.toml` may set it as `rustc-wrapper`): both
  see `-Zcodegen-backend=PATH` as a string. For a cargo build through the
  backend, pass the keyed path:
  `RUSTFLAGS="-Zcodegen-backend=$(harness/backend_path.py [--gc TARGET_DIR] [K=V ...])"`.
  It prints `target/backends/<key>/librustc_codegen_pliron.*`, a private
  copy whose name hashes the dylib's contents and the code-affecting
  `PLIRON_*` environment, so cargo recompiles exactly when either changed
  (no wipe, no `cargo clean -p`) and a rustc wrapper cannot return another
  backend's objects. Rules: run the helper under the same `PLIRON_*` as the
  build (per-build knobs as `K=V` arguments); every path leaves its own
  units in a target dir, `--gc` drops the others; reporting-only knobs
  (`PLIRON_TRACE`, `PLIRON_STATS`, `PLIRON_TIMING`, ..., the `IGNORED` list
  in the script) are not in the key and do not force a rebuild; crates
  compiled under two paths do not link together (E0460). With the raw
  `target/debug/...` path the old rule stands: a fresh `CARGO_TARGET_DIR`
  or `cargo clean -p` after changing the dylib or a knob, and no rustc
  wrapper (the repo's `.cargo/config.toml` turns it off for builds started
  inside the repo; outside it, `RUSTC_WRAPPER=`). Scripts on the keyed
  path, measurements and what sccache keys on:
  [`docs/backend-path.md`](docs/backend-path.md).

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
  `node bench/wasm/run.mjs LLVM A [B] [filter] --perturb=4|8` judges a
  change that only moves code (ICF, a dropped instruction): V8's wasm
  inlining budget follows the module's small functions (`fib` 6.0 vs
  12.8 ms) and Apple cores care about a loop's address mod 8
  (`parse_only` 243 vs 259 ms), and `--layouts` covers neither; the
  copies append unused functions and shift every loop by 4 bytes
  (`docs/history/2026-11-17-v8-placement.md`).
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
- **A whole wasm program: `harness/dsa/`.** `build.sh {llvm,pliron}
  NAME` builds `~/code/dsa`'s `literate-wasm` (Loro CRDT, the Mimas
  compiler and interpreter, rapier2d) into `/tmp/dsa-bench/pkg-*`
  (about 5 min; the build loads a keyed private copy of the dylib, so
  the backend can be rebuilt meanwhile, and a rerun with the same dylib
  and knobs compiles nothing: `DSA_WIPE=1` forces it; a rustc that
  earlyoom kills is rerun, `harness/dsa/test_retry.sh` checks that),
  `node harness/dsa/bench.mjs --llvm=DIR
  --pliron=DIR [--extra=DIR] [WORKLOAD..]` times 19 workloads
  interleaved and cross-checks every result (a mismatch is a
  miscompile until shown otherwise: five were, 2026-11-16).
  `prof.sh WORKLOAD` gives per-function self time on both sides,
  `cmpprof.py` joins the two profiles, and `hot.sh SIDE PKG FUNC
  WORKLOAD` attributes V8 ticks to machine instructions of one
  function and names V8 runtime calls (a `memory.copy` is one). The
  workload sources are not in the repo. Numbers and open gaps:
  `docs/PARITY.md` "Held-out wasm program"; how each bug was bisected
  (crate swap, stage knobs, `PLIRON_STEP_FNS`, per-body `wasm2wat`
  diff): `docs/history/2026-11-16-wasm-night.md`.
- To see why a kernel is slow: `PLIRON_CLIF=<fn substr>` dumps CLIF
  after every step; `rustc --emit obj` + `llvm-objdump -d` (or
  `wasm2wat`) for final code; compare with stock rustc's output for the
  same function. `PLIRON_STATS=1` shows which passes fired.
- `PLIRON_<PASS>=0` and `PLIRON_PIPELINE=a,b,c` change the pipeline with
  no rebuild — try them before editing code.
- The live dashboard's "size movers" panel shows which corpus functions
  your edit made bigger or smaller.
- wasm runtime under V8 — **profile, don't guess from insn counts**
  (static `i32.mul` / size lied on parse_only: 17-mul shared form was
  *slower* under TurboFan than 45-mul rematted arms; `hotw` shows both
  sides `load8_u`-bound with tick ratio ≈ wall time). Loop:
  `harness/v8/variant.sh NAME [PLIRON_X=…]` → `target/v8prof/NAME.wasm`;
  `cpuprof.sh mod k_parse_only iters reps` (per-fn self time);
  `hotw.sh mod k_parse_only parse_only iters reps` (ticks → wasm ops;
  paths may be relative — scripts absolutize);
  `mcode.sh mod k_parse_only parse_only` (TurboFan listing; diff before
  trusting a delta). Score with `node bench/wasm/run.mjs llvm.wasm
  a.wasm [b.wasm] --layouts=3`. Wasm defaults for parse_only ≤1×:
  `WBRIF` on (`PLIRON_WBRIF=0` off) and `SWITCH_GCC` on for wasm
  only (`PLIRON_SWITCH_GCC=0` off, `=1` also on native). Together
  parse_only is **0.99×** LLVM and tokenize **−17%** (geomean
  0.921). The parse win is a 2-case `byte < '+'` split.
  `PLIRON_SWITCH_GCC_MAX` (default 2, `=0` no cap) keeps that
  and leaves the 4-case `c >> 8` bucket switch in
  `IsWhitespace::next_match` alone — clustering it stores the
  searcher cursor on every byte (tokenize ~+3%). Native
  `SWITCH_GCC` stays opt-in.
  `divconst` before the egraph on wasm is what narrowed `i % 5`.
  `PLIRON_WASM_BRI_NEST=1` and that plus `PLIRON_EGRAPH_IMULCAP=1`
  were slower (parse ~1.23× and ~1.28×).
  Details: `harness/v8/README.md`; what was already tried:
  `docs/history/2026-10-21-wasm-experiments.md`.

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
- **Off switches**: `harness/offswitch.py [--corpus DIR] [--every N]
  [--only X,Y] [--resume --out DIR]` runs `clif-check report` once per
  CLIF off switch (`PLIRON_<X>=0`, read from the source: `Gate::Env`
  steps, `pass_enabled` reads, the `FnFlags` categories, `DEFAULT_ON`
  knobs pliron-clif-opt reads; 245 on 2026-11-16) under
  `PLIRON_VERIFY=full` and fails on any row that is not `ok`. A pass
  turned off must not break a later one: `PLIRON_JUMPTHREAD=0` crashed
  loopidiom, `PLIRON_TAILDUP=0` broke slotprom. std every 4th function:
  about 9 min for all switches; `wasm-*` corpora replay as wasm32. IR-pass
  switches are not covered (corpora are recorded after the IR passes).
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
  every save to a pass it rebuilds `clif-check` (`PLIRON_PROFILE=fast`
  by default, ~3 s warm), replays corpora (defaults to `smoke` when
  present; larger sets sampled with `--every 8`), and shows new
  failures (with repro commands), size movers and step times. Full
  std: `--corpus std --every 1`. For pass **unit tests** on save, use
  relax (`harness/dev.py test`, under Build) — faster than cargo, and
  better than this dashboard for a single shape/`#[test]` oracle.
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
  before.clif` replays in a second. wasm32 functions are checked too
  (i32 pointers in the sandbox since 2026-11-16): in the backend it
  follows the target, and `PLIRON_CLIF_WASM=1 clif-check ...` replays a
  corpus recorded from a wasm build (`PLIRON_CORPUS=<dir>` on
  `harness/v8/variant.sh` or `harness/dsa/build.sh`) the way the wasm
  backend runs the pipeline. The stages after CLIF (waffle, `wpeep`)
  are not covered by it. Reading a report (which step of a corpus
  function, `PLIRON_DIFFCHECK_WATCH=<address>` / `PLIRON_DIFFCHECK_PATH=1`
  to follow a differing byte back, never-written bytes, lifetime markers
  an IR pass left stale, a corpus older than a fix):
  `docs/miscompile-triage.md` "Reading a corpus mismatch".
- **Did an IR pass change behaviour?** `harness/xcorpus.py --off
  PLIRON_<PASS>=0 [--on K=V] [wasm|std|rs FILE|cargo DIR]` records two
  corpora of one build, the pass off and on, and `clif-check xcorpus`
  compares every function by symbol: A against B and A against the CLIF
  pipeline's output for B (stale lifetime markers show only there).
  Functions whose call sets differ (inlining, devirt) are skipped.
  Names `mcrdwrap::build` with `PLIRON_MEMCPYOPT_RDWRAP=0`; no false
  positive on the wasm kernels + std for `PLIRON_MEMCPYOPT=0`; on the
  full dsa corpus 77 not yet explained (0.3% of pairs compared, mostly
  serde/bincode `Deserialize` instances: compare a memcpyopt change's
  list with the tree's before it). How it works,
  what it cannot see:
  `docs/miscompile-triage.md` "IR passes".
- **wasm-only stages** (after CLIF): the waffle inliner
  (`PLIRON_WASM_INLINE=0`), byte peepholes (`PLIRON_WASM_PEEP=0`,
  `PLIRON_WASM_PEEP_LIMIT=N` peepholes only the first N bodies — bisect
  it; `PLIRON_WASM_PEEP_MOD=<substr>` + `PLIRON_WASM_PEEP_FI=N` target a
  deterministic module-local body instead — the global counter jitters
  under parallel CGUs — and `PLIRON_WASM_TRACE_BODY=N` dumps the body
  after each `wpeep` sub-pass, `PLIRON_WASM_BODYDUMP=<dir>` saves raw
  pre/post bytes), `PLIRON_WASM_DUMP=<substr>` dumps waffle bodies,
  `PLIRON_WASM_STUBLOG=<file>` lists functions that fell back to stubs.
  `PLIRON_WASM_MODDUMP=<dir>` writes each module before and after the
  peepholes (`<name>.pre.wasm` / `.post.wasm`): `wasm-objdump -d` both
  and diff one function (`PLIRON_WASM_SFWD_FWLIM=N` / `_FWSKIP=N` pin a
  single forwarded load inside it).
- **Did a wasm peephole change behaviour?** `node
  harness/wasm/peepdiff.mjs <dir>` on a `PLIRON_WASM_MODDUMP=<dir>` dump
  (a new directory per build) calls every function body before and after
  `wpeep` in V8 with the same seeded arguments on the same seeded
  memory and lists the bodies that differ: import calls (callee,
  arguments, visible memory), results, traps, memory at return. A wrong
  body is told from its callers (`ISOLATED`: differs with every other
  body unchanged) and comes with a `--fn N --seed S --trace` replay
  line. Use it before bisecting a wasm-only miscompile, on the dump of
  the failing build: 0.8 ms per function (wasm bench + std 2,643
  functions 1.3 s, dsa 68,555 functions 52 s), no false positive on
  either, optimized or not. With the three wpeep bugs of 2026-11-16 put
  back one at a time it names the function of each report:
  `blake3::portable::compress_in_place`, rapier2d's `symmetric_eigen`
  and dsa's unoptimized `debugsession_compileInfo` shim.
  `tests/wasm/smoke.sh` runs it on the smoke objects plus the three
  shapes (`tests/wasm/smoke/peep.rs`, `peep_raw.rs`), so it is in the
  gate. A clean run is not a proof: `--coverage` says how much code the
  trials ran (40% of blocks on the bench, 27% on dsa) and `--mutate`
  how many one-instruction mutants they detect (67%, 62%); waffle and
  the CLIF pipeline are upstream of both dumps and not checked. Blind
  spots and how to read a report: `harness/wasm/README.md`.
- **wasm static constructors** (`.init_array[.N]` statics: `inventory`,
  `ctor`): `pliron-wasm-ld` calls them from `__wasm_call_ctors` by
  priority. A module that neither calls nor exports that function (a
  cdylib) gets every export wrapped (`<name>.command_export`) so they
  run once before the first export's body; `PLIRON_WASM_CTORWRAP=0`
  (read by the linker) leaves the exports alone. Tests: the `ctors`
  rows of `tests/wasm/smoke.sh`, `tools/pliron-wasm-ld/tests/ctors.rs`.
- **Identical-code folding** (`PLIRON_WASM_ICF=1`, opt-in, read by
  the linker): `pliron-wasm-ld` keeps one body per class of identical
  functions (same type, same bytes, call and `ref.func` targets equal
  by class: partition refinement to a fixpoint, so mutually recursive
  copies fold) and points calls, table entries, exports, `start` and the
  name section at the lowest-index member
  (`tools/pliron-wasm-ld/src/icf.rs`, tests `tests/icf.rs`).
  `PLIRON_WASM_LD_VERBOSE=1` prints the count, bytes and time.
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
- `PLIRON_MEMCPYOPT_PATHWIN=0`, `PLIRON_CONSTCOPY2=0` — off switches
  for the 2026-11-16 dsa round
  (`docs/history/2026-11-16-wasm-night.md`, "dsa, performance"), each
  default on: memcpyopt's writer-to-copy window follows paths instead
  of dominators alone (a function's several `return` sites each
  forward their `store tmp; memcpy(sret, tmp)`), and
  `constload::forward_copies` runs again after phisimp.
  `PLIRON_WASM_SMALLCOPY=0`: off switch, default on again since the
  afternoon: the wasm emitter expands constant copies of at most 16
  bytes instead of emitting `memory.copy`. (The corruption once blamed
  on it was a waffle rematerialization bug, fixed in `5d703d0`.)
- `PLIRON_MEMCPYOPT_RDWRAP=0` — memcpyopt without the loop-wrap scan
  of post-copy readers (a miscompile, for bisecting only: a temporary
  read again inside a loop that changes the copy's destination;
  `docs/history/2026-11-16-memcpyopt-reader-wrap.md`, stanza
  `mcrdwrap`). `PLIRON_TRACE=pliron::memcpyopt=debug` names the
  function (`fn <symbol>`) ahead of its `fwd:` lines.
- 2026-11-16 afternoon, each opt-in
  (`docs/history/2026-11-16-wasm-night.md`, "Afternoon"):
  `PLIRON_INLINE_ONCE_WASM` (wasm's single-caller inlining mode,
  **default `3` since 2026-11-17**: a callee with a loop goes only into a
  loop-free caller that has taken no such callee, loop-free callees
  always; `=0` the old wasm behaviour, `=2` loop-free callees only, `=1`
  every single-caller callee, which costs tokenize and sieve;
  `PLIRON_INLINE_ONCE` overrides it on every target). It is on together
  with `PLIRON_LICM_GTE_WASM` and `PLIRON_LICM_SLOTPROV_WASM` (default
  on, wasm only; `PLIRON_LICM_GTE=0` / `PLIRON_LICM_SLOTPROV=0` off
  everywhere, `=1` on native too): licm hoists a load that runs on every
  iteration without a dereferenceability proof, and a slot indexed by a
  loop counter stays its own object; without them single-caller
  inlining costs hmget. Kernels geomean 0.931 -> 0.929, hmget +5.7% ->
  +1.9%; dsa 14 like-for-like rows 1.030 -> 0.999, module -1.7 MB
  (`docs/history/2026-11-16-licm-gte.md`),
  `PLIRON_INLINE_RECX=1` (a site cloned out of an inlined body does not
  inline a member of a call-graph cycle its clone chain already
  entered: derived `Clone` of a recursive AST; **on by default for
  wasm** as `PLIRON_INLINE_RECX_WASM`, `PLIRON_INLINE_RECX=0` turns it
  off, test stanza `inlinerecx`),
  `PLIRON_LICM_SLOTPROV=1` (a stack slot indexed by a loop counter is
  still an object of its own for licm's clobber test,
  `pliron-clif-opt/src/slotprov.rs`), `PLIRON_PHISIMP_EARLY=1` (phisimp
  once before instcombine), `PLIRON_MEMCPYOPT_MOVEMARK=1` (a forward
  moves `dst`'s `lifetime.start` ahead of its writers instead of
  dropping `dst`'s markers).
- `PLIRON_WASM_SMALLFN=<insts>` (opt-in, wasm, 2026-11-17, queue item
  4): a function of at most that many CLIF instructions whose loops are
  all search loops (two or more exit edges) skips `looprot`, `looprot2`
  and `punroll` (with its peeling). V8 inlines a wasm callee only up to
  500 body bytes (`--wasm-inlining-max-size`); our `memchr_aligned` was
  1,329 bytes (LLVM 294) and `memcmp` 602 (LLVM 74), with `=120` they are
  340 and 116. A scratch string-search driver goes from 1.33x to 1.14x
  LLVM's time; the kernel module changes in one cold function.
- `PLIRON_PUNROLL_COUNTED_WASM_FNMIN=<insts>` (opt-in, wasm): the
  `COUNTED_WASM` 4-wide unroll skips functions under that many
  instructions, so small counted-loop callees stay under V8's 500-byte
  inline limit (bignum `add`/`sub` 596/586 -> 439/444 bytes; a
  `Big32x40` subtraction loop 1.61x -> 0.94x LLVM). Loses `sieve` at 100
  (its `vec![x; n]` fill is a small function with a long loop): not for
  default (PARITY queue item 10).
- `PLIRON_INLINE_V8CAP=<lean ops>` (opt-in, wasm, 2026-11-17): the
  inliner refuses an inline that would take a caller from at most that
  many `lean_cost` ops past it, when the caller has at least
  `PLIRON_INLINE_V8CAP_SITES` (2) call sites and at most
  `PLIRON_INLINE_V8CAP_CALLS` (3) calls of its own, and the callee is
  over `PLIRON_INLINE_V8CAP_TINY` (8) lean ops: V8 inlines a wasm callee
  only up to 500 body bytes, about 115 lean ops (calibration on
  rapier2d's 2,401 functions: median 4.4 bytes per lean op, `lean > 100`
  predicts `> 500 B` for 93%; `PLIRON_INLINE_SIZELOG=<prefix>` writes
  every function's raw and lean counts after inlining). The calls bound
  matters: without it dragon's `mul_pow10` stayed small, was inlined
  into `format_shortest`/`format_exact` with its twenty calls, and the
  kernel module grew 25%. dims driver with `PLIRON_WASM_SMALLFN=120`:
  1.114 -> 1.043; alone 1.326 -> 1.340; rapier2d driver unchanged.
- `PLIRON_INLINE_LEANCOST=<w>` and `PLIRON_UNROLL2_BUNROLL=<insts>`
  (opt-in, 2026-11-17, queue item 3). The first scores a callee with at
  most `w` warm calls by its `inline_cost::lean_cost` (jumps, slots,
  markers, slot loads/stores and dead compares free) when that is below
  the threshold and at most `PLIRON_INLINE_LEANCOST_PCT` (default 30) of
  its raw count: nalgebra's `Matrix<2,3> * Matrix<3,2>` is 2,294 raw ops
  and 478 lean, and is inlined into rapier2d's `assemble_operator`. The
  second (with `PLIRON_UNROLL2`) runs `bunroll` in the late round on
  loops that contain loops, up to three rounds, with exact trip counts
  and the copies' total capped at `<insts>` (256): `for q in 0..3 { for
  p in 0..=q { if blocks[..] == MAX { continue } .. } }`, which `unroll`'s
  trip walk cannot take. `PLIRON_UNROLL2_LOOPS` (default 32) is the late
  `unroll`'s loop budget. Rapier2d driver: 1.353 (item 2's knobs) ->
  1.281 (+LEANCOST) / 1.240 (+BUNROLL) / 1.197 (both); the wasm kernel
  module is byte-identical with `UNROLL2_BUNROLL`, LEANCOST changes
  `quicksort` and dlmalloc's `free`.
- `PLIRON_FCMPFOLD=1`, `PLIRON_FMUL1=1` (opt-in) and `PLIRON_UNROLL2`
  (default on since the evening of 2026-11-17, PARITY item 9; `=0` off),
  2026-11-17, queue item 2: `fcmp` of two float constants folds (egraph
  rule and `jumpthread::eval`, so `unroll`'s trip walk decides it),
  `x * 1.0` is `x` (vendored egraph rule), and `unroll2` runs `unroll`
  again after the first `loadfwd`/`egraph` (then `loadfwd`, `slotprom`,
  `dse`, `egraph` once more when it unrolled anything; loops with a call
  or `black_box` stay; emission cap `PLIRON_UNROLL2_INSTS`, default 256).
  Together they turn nalgebra's small matrix products (`gemm` testing a
  constant `beta` against zero) into straight-line code: rapier2d's
  `Matrix<2,3> * Matrix<3,2>` 192 -> 33 CLIF instructions, and a
  rapier2d soft-FEM driver on wasm from 1.57x to 1.37x LLVM's time. The
  wasm kernel module is byte-identical with them on. Not yet timed on
  dsa (its tree did not build) or on native.
- `PLIRON_SROA_FRAGWORD=<bytes>` — the word size `PLIRON_SROA_FRAGCOPY`
  counts in; default the pointer width (4 on wasm32 since 2026-11-16,
  `=8` restores the old rule there; native is 8).
- `PLIRON_SROA_SRETNOSYNC` (default on since 2026-11-17, `=0` off) — sroa does not write a split
  temporary back before a call whose only pin on it is its whole
  `sret` argument (the destination is uninitialized when the call
  starts; invokes keep the writeback for their cleanup). An
  interpreter loop refilling one sret `Flow` lost 18 stores per step
  (PARITY item 7); stanza `sretnosync`,
  `PLIRON_TRACE=pliron::sroa=debug` prints `sret writeback skipped`.
- `PLIRON_WASM_CONSTLOCAL=1`, `PLIRON_WASM_BLKHOIST=1` (opt-in,
  vendored waffle, N9 #3/#4): a constant used often enough lives in a
  local set at function entry instead of an immediate per use; the
  straight-line code before a `block` opener moves into the block, so a
  value set for an `if`'s condition (the `block`+`br_if` rewrite) is not
  parked across the openers. Kernel module -1.5% each, -3.1% together,
  timing unchanged (PARITY item 6; tests `vendor/waffle/tests/
  const_local.rs`, `blk_hoist.rs`).
- `PLIRON_PUNROLL_DWLATCH` (default on since 2026-11-17, `=0` off) —
  `punroll` does not keep a do-while's counting latch test in every
  copy (PARITY items 5 and 9).
- `PLIRON_LDNARROW=1` (opt-in, 2026-11-17) — step `ldnarrow` after
  `dem64`: a `notrap` integer load whose every use is an `ireduce` loads
  the widest reduced type (enum tag reads forwarded out of a copy word:
  wasm `i32.load8_u` instead of `i32.load` + `&255`, aarch64 `ldrb`).
  Native exact counts: pulldown-cmark -0.28%, html-rewriter -0.14%,
  protobuf -0.09%, none worse (PARITY item 8).
- `PLIRON_SHRINKWRAP=1` (opt-in, aarch64; vendored Cranelift) — callee
  saves and frame allocation move to the nearest closed dominator of the
  blocks that need them (`VCode::pliron_shrink_wrap`), after a CLIF
  live-range split at that block (`Context::shrink_split`) that runs only
  with a non-tail call, a warm early exit and at most
  `PLIRON_SHRINKWRAP_MAXLIVEIN` (default 3) live-ins
  (`PLIRON_SHRINKWRAP_LEAF=1` / `_COLD=1` drop the first two rules).
  `PLIRON_SHRINKWRAP_FNS=lo..hi` limits it to functions whose content
  hash is in range (bisecting; `PLIRON_SHRINKWRAP_DEBUG` prints hashes).
  Stanza `shrinkwrap`; PARITY item 10.
- `PLIRON_UNDEFPHI=1` (opt-in, not ready; PARITY item 10) — a block
  parameter whose inputs are one dominating value or the lowering's
  undefined zeros becomes that value (`pliron_clif_opt::undefphi`, run
  on the lowered function). Mixed: html-rewriter -0.22%, regex +0.6%,
  blind-sig +26%.
- `PLIRON_STEP_FNS=<step>=<lo>..<hi>` — run one CLIF step only in the
  functions whose `fn_hash` is in `[lo, hi)` (everything else as usual),
  `PLIRON_STEP_FNS_LOG=<file>` names them. Halve the range per probe to
  find the function one step breaks in a crate too big for the serial
  `PLIRON_OPT_BISECT` paths; set `PLIRON_MEMCACHE=0` with it.
- `PLIRON_PAR=0|N` — lowering threads.
- `PLIRON_VERIFY=1|full` — Cranelift verifier after the pipeline / each step.
  On wasm the verifier runs once after the pipeline by default (nothing
  after the pipeline would reject invalid IR there); `PLIRON_VERIFY=0` off.
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
- 2026-11-13 afternoon round (`docs/history/2026-11-13-six-features.md`,
  "Handoff: parity"): `PLIRON_RA2_REHOME` (vendored regalloc2, default
  `2`, `0` off: a value's register in a join-only block follows the
  cheaper side of its edges), `PLIRON_MEMCMPK_EQ=0` /
  `PLIRON_MEMCMPK_CONST=0`, `PLIRON_NEON_PAIRWISE=0` (aarch64 NEON pairwise ops inline), `PLIRON_XCGU_GLUEDEPTH=0` (drop glue free of copy descent
  depth), `PLIRON_SELXOR_SIGN=0`, `PLIRON_PHISIMP_DEADPRED=0`,
  `PLIRON_SROA_FRAGCOPY=<k>` (default 8, `0` off: whole-copied
  aggregates cut into sub-word pieces stay whole) — off switches for
  what is default on. Opt-in: `PLIRON_INLINE_LEAN=<k>` (wrapper
  callees pass the inline score; 32 measured, pulldown-cmark +0.21M
  open), `PLIRON_VTRUNC_UZP=1` (aarch64 lane narrowing as `uzp1`; fails
  the `simdshift` stanza when on), `PLIRON_PADJUMP=<n>` (vendored Cranelift: jump over alignment
  pads of `n`+ instructions; unmeasured).
- `PLIRON_LICM_GTE` (default on everywhere since 2026-11-17, PARITY
  item 9; `=0` off) — licm hoists a `notrap` load that runs
  on every loop entry (the header's straight line, before any branch,
  call or trapping instruction) without a dereferenceability fact;
  with `PLIRON_INLINE_ONCE=1|3` and `PLIRON_LICM_SLOTPROV=1` (+`NCC` for
  `=1`) hmget goes from +11% to +1..2% against LLVM.
  `PLIRON_LICM_NOPRESS_WASM=1` (opt-in): no `INNERPRESS` cap on wasm32
  (`docs/history/2026-11-16-licm-gte.md`).
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
- 2026-11-16 wasm round (`docs/history/2026-11-16-wasm-night.md`),
  each default on and wasm only (native was not timed):
  `PLIRON_ASSUME_RANGE=0` (`assume(i < CONST)` as an `llvm.assume`
  marker op that `lower.rs` turns into a branch to the unreachable
  trap), `PLIRON_UNREACH_EARLY=0` (`vrp0` + `unreach0` first in the
  pipeline: fold what an assume edge implies, then drop it),
  `PLIRON_UNREACH_PHI=0` (edges that decide a test toward the
  unreachable trap go with their guard: `unwrap_unchecked` on a
  branchy `Option<&T>`), `PLIRON_UNROLL_SYM=0` (full unroll of
  fixed-array pointer loops), `PLIRON_HALFUNROLL=0` (constant-length
  `binary_search` as a select chain), `PLIRON_REVCMP=0` (equal-length
  reverse compares drop the second end test) and `PLIRON_VRP2=0` (a
  second range pass ahead of `egraph2`). `PLIRON_WTREEREMAT_CONDARG=1`
  (vendored waffle) rematerializes arm arguments on their edge again
  (off: parse_only -11% against LLVM); `PLIRON_ARMHOIST=1` is opt-in.
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
  function become one block (.text -0.1..-0.2% on regex and
  rust-html-rewriter, less elsewhere; executed instructions unchanged). `PLIRON_PANICSTUB_STRIP=0` keeps
  each site's `coldargs` wrapper, `PLIRON_PANICSTUB_FWD=0` keeps
  forwarder blocks. Sites on different source lines stay apart when the
  build has line tables.
- `PLIRON_SCOPED_NOALIAS=1` (opt-in; measured ready for default-on but
  the 2000-seed fuzz sweep it needs first did not complete, see the
  handoff section of `docs/history/2026-11-13-six-features.md`): an
  inlined callee's `&mut` param keeps its `noalias` fact for
  the span of the inlined body: a first pipeline step (`scopedfwd`)
  runs `loadfwd` on the function as lowered, where locs reached through
  the param survive stores and calls proven not based on it. The step
  runs only in functions where a probe finds such a loc
  (`PLIRON_SCOPEDFWD_PROBE=0`: wherever there is a candidate), and
  `phisimp` keeps a clone-entry arg only where the fact can be used
  (`PLIRON_SCOPED_PHI_ALL=1`: every `noalias` arg, `PLIRON_SCOPED_PHI_RO=1`:
  `readonly` ones too, `PLIRON_SCOPED_PHI_KEEP=<bits>`: one class).
  Exact counts: rust-protobuf -1.44%, rust-json -0.77%,
  rust-html-rewriter -0.04%, the other five held-out programs within
  0.01% or their noise (`docs/history/2026-11-13-six-features.md`).
  `PLIRON_TRACE=pliron::scopedna` says why candidates drop out;
  `PLIRON_SCOPEDFWD=0` skips the step, `PLIRON_SCOPEDFWD_PRE=0` its load
  PRE, `PLIRON_SCOPED_NCC=0` the `nocapture` relaxation. Corpora
  recorded with the knob on carry the candidates
  (`; diffcheck-facts: scopedna=`), so `clif-check report --check`
  replays the step.
- `PLIRON_LT_PINANCHOR=0` — a lifetime marker is anchored on the last
  instruction lowered before it again, pure ones included (default: the
  anchor steps back over instructions the egraph may move; a
  `stack_addr` anchor that sank below a slot's first writes made
  slotmerge reuse the slot at opt-level 0, stanza `o0std`).
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
- `target/backends/` holds the keyed dylib copies of
  `harness/backend_path.py` (`--list` shows them). Each call keeps the 8
  most recently used and anything used in the last 2 hours;
  `harness/backend_path.py --prune --keep N [--min-age 0]` or
  `disk.sh --sweep` (keeps 2) removes more.
- Worktrees: reclaim space with `cargo clean` inside one rather than
  `git worktree remove`, which loses uncommitted work.
