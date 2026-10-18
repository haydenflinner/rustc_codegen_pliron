# Agent instructions

## Layout

`rustc_codegen_pliron` is a rustc codegen backend:
rustc MIR → rustc_codegen_ssa → pliron LLVM dialect → Cranelift →
cranelift-object (+ rsasm for `asm!`/`global_asm!`) → system linker.

- `pliron-clif-opt/` — workspace crate holding all Cranelift-IR passes
  (`&mut Function` peepholes, loops, DSE, ...). Stable Rust, no
  `rustc_private` — `cargo test -p pliron-clif-opt` runs pass unit tests
  without building the dylib. Re-exported via `pub use pliron_clif_opt::*`
  so `crate::<pass>` paths still work in `src/`.
- `pliron-ir-opt/` — pliron-LLVM-dialect passes (inline, sroa, memcpyopt,
  spec, nnret, nocapture, nowrite, phisimp, instcombine, retarg,
  constload, domcheck), `state::State` (the side tables lowering reads),
  `types.rs` layout helpers, `has_body`/`address_taken`. Stable Rust.
  The bottom-up function summaries (nowrite, nocapture, nnret, retarg,
  nounwind, willreturn) are Datalog rules in an `ascent!` block: `run`
  walks the bodies once to fill input relations, the rules derive the
  "bad" side (writes, captured, may be null, may unwind, may diverge)
  as a least fixpoint, and the table is everything not derived. Add a
  rule rather than another loop.
- `pliron-clif-check/` — `clif-check` binary: replays and reduces
  differential-checker reports without rustc (see "Miscompile hunting").
- `pliron-cfg/` — one-shot `PLIRON_*` env parse shared by both opt
  crates (`PassConfig`, `env_*`, `pass_enabled`, `bisect`).
- `src/lower.rs` — object lowering; `internal_fns` selects `CallConv::Tail`
  for local functions
- `src/eh/mod.rs` — `.eh_frame` / LSDA emission; Mach-O needs
  `__TEXT,__gcc_except_tab`, `ARM64_RELOC_POINTER_TO_GOT` personality relocs,
  and extern symbol refs (ld64 reads CFI relocs by symbol index)
- `src/objmerge.rs` — splices rsasm output into the dest object; rsasm emits
  the destination's binary format, so flags/relocs pass through verbatim
- `src/asm.rs` — emits ELF- or Mach-O-flavored assembly per target
- `src/hot.rs` — PLIRON_HOT thunks, x86-64 ELF asm only
- `src/intrinsic.rs` — `black_box` is a real barrier: it emits a call to
  a never-defined `__pliron_bbv_<ty>` (values) / `__pliron_bbm` (Refs and
  ZSTs) via `call_bb`, which flags the `CallInfo`; `lower.rs` rewrites
  those calls into the CLIF `black_box` op (vendored Cranelift patch 22 —
  `can_store` + `other_side_effects`, i.e. LLVM's `r,~{memory}` asm, and
  each ISA lowers it to zero instructions). The bodiless callee keeps
  pliron's own IR passes opaque (nowrite/nocapture/retarg are all
  conservative), so the IR-level call is never folded away before it can
  become the CLIF op. wasm has no CLIF path,
  so it keeps the alloca + volatile-load fallback — do not "simplify"
  `black_box` back to a pass-through; cg_clif's FIXME is not a model.
- `vendor/cranelift-codegen` — patched upstream tree; every local change
  is marked `PLIRON:` in source and numbered in `PLIRON_PATCH.md`.

## Build / test

Pinned toolchain: `rust-toolchain.toml` (`nightly-2026-10-06`, needs
`rustc-dev`, `rust-src`, `llvm-tools-preview`).

- `cargo build` → `target/debug/librustc_codegen_pliron.{so,dylib}`;
  incremental after a `pliron-clif-opt`/`src` edit is ~2s
- `cargo check|test -p pliron-clif-opt` (or `-p pliron-ir-opt`) — the opt
  crates build on stable Rust: sub-second typecheck, pass unit tests with
  no dylib link and no rustc
- `./test.sh` — smoke tests (nostd, std, unwind, asm, proc-macro, wasm if
  node is installed); `--sysroot` also rebuilds std via `-Zbuild-std`;
  `--mold` cross-links the std test to `*-linux-musl` with mold (PATH or a
  sibling `mold` checkout built with `cargo build --release`) and runs it
  under docker/qemu when available — the pure-Rust link path, since mold is
  ELF-only (no Mach-O)
- `harness/run.py [--tier 0|1] [--accept]` — runs suites (smoke with
  stock-rustc output diff, object determinism, ui) and fails on regressions
  against `harness/expectations/`; plan and status in `test-harness.md`
- `bench/run.sh [criterion args]` — builds the `pliron-bench` criterion
  target (`bench/benches/kernels.rs`) with stock rustc and with the pliron
  backend (`cargo rustc --bench`, flag scoped to the bench crate), saves
  the LLVM run as the `llvm` baseline, then prints per-kernel significance
  stats. Args forward to both criterion binaries (a name filter selects a
  subset). Results table: `docs/performance.md`.
- `bench/wasm/run.sh [name-filter]` — wasm leg: compiles the same kernels
  (`#[path]`-included into `bench/wasm/lib.rs`, cdylib) to
  wasm32-unknown-unknown twice — stock rustc + wasm-ld, and the pliron
  backend + `tools/pliron-wasm-ld` with `-Zbuild-std` — then times both
  modules in node (`bench/wasm/run.mjs`). Criterion can't run on wasm (it
  re-execs the bench binary per benchmark), so run.mjs does its own
  warmup/calibration/sampling and prints the same kernel/LLVM/pliron/
  change table.
- Pushing: this is a personal fork that may never be upstreamed — commit
  and push completed work freely; no confirmation or personal-diff-review
  protocol applies. Do not add `Co-Authored-By` trailers.
- Disk cleanup: worktree source dirs are small — the weight is each
  worktree's `target/`. Reclaim space with `cargo clean` (or remove
  `target/`) inside a worktree rather than `git worktree remove`, which
  permanently loses uncommitted changes.
- `tests/ui_run_pass.py [RUST_CHECKOUT] [FILTER]` — runs directive-free
  `//@ run-pass` tests from a rustc checkout (defaults: `$RUST_CHECKOUT`,
  then a `rust` dir next to this repo, else `~/work/rust`)

## Landing changes: the gate

- `harness/gate.py` is the merge gate: one `cargo build`, then in
  parallel the stable crates' unit tests, every `test.sh` stanza (wasm
  included) and `clif-check report --check` over each recorded corpus.
  ~2 min; logs in `target/gate/`. Run it before every push; it is also
  the git `pre-push` hook (`harness/gate.py --install-hook`; bypass
  with `--no-verify` only with a note in the ledger). main must stay
  green — a red gate is fixed or reverted before new work lands.
- New passes and new transforms in existing passes land **default off**
  (`env_is("PLIRON_X", "1")`) and flip to default-on in a separate
  commit after the gate, the corpus and one self-host build are clean.
- Self-host copies carry their source revision: `selfhost/setup.sh`
  writes `.pliron-src-rev`; `PLIRON_VERSION=1 rustc ...` prints the
  backend's (`harness/src-rev.sh`). Check it before trusting a stage2
  result.
- `RUSTC_ICE` points at `target/ices/` in the scripts — never leave
  `rustc-ice-*.txt` in repo roots. `harness/disk.sh [--sweep]` reports
  and reclaims build-dir space.

## The corpus and the live loop

- `harness/corpus.sh smoke|std|cargo DIR` records every function's
  pre-pipeline CLIF + facts (`PLIRON_CORPUS`) into `target/corpus/<set>`
  (std: ~15k functions in 30 s). Re-record after lowering / ir-opt changes;
  pass-only edits replay the existing corpus.
- `clif-check report target/corpus/std out.json --check` runs the whole
  CLIF pipeline + interpreter diff on every function in parallel (~7 s
  for std). It found a real std miscompile (`loadsel`, sort networks) on
  its first run.
- `harness/live/live.py` watches the pass sources, rebuilds `clif-check`
  and replays the corpora on every save; http://localhost:8765 shows
  new failures with repro commands, code-size movers and per-step time
  against a saved baseline. Use it while editing any CLIF pass.

## Miscompile hunting

- `harness/fnbisect.py -- <cmd>` finds the function(s) whose optimization
  breaks a program with `PLIRON_OPT_FNS` (per-symbol selection — stable
  across threads, CGUs, processes and rebuilds, so it works on a whole
  `-Zbuild-std` or bootstrap build, unlike `PLIRON_OPT_BISECT`). Selected
  functions get the per-function pliron-IR transforms and the CLIF
  pipeline; the rest keep their unoptimized bodies (IPO rewrites still
  apply to all). `<cmd>` must recompile (fresh `CARGO_TARGET_DIR`).
  Then `PLIRON_OPT_BISECT`/`PLIRON_DIFFCHECK_FN` within that function.

- `PLIRON_DIFFCHECK=1` runs each function's CLIF before and after the
  pipeline in `cranelift-interpreter` on random inputs (pointer params get
  sandboxed memory; calls become recorded deterministic stubs) and compares
  returns, calls and memory writes. On a mismatch it replays step by step to
  name the step and saves `before.clif`/`after.clif`/`report.txt` under
  `PLIRON_DIFFCHECK_DIR` (default `pliron-diffcheck/`). `=step` checks every
  step. Also `_FN=<substr>`, `_TRIALS`, `_FUEL`, `_SEED`, `_PANIC=1`,
  `_VERBOSE=1` (one summary line per function). Code:
  `pliron-clif-opt/src/diffcheck.rs`; tests: `pliron-clif-opt/tests/`.
  Traps and anything the sandbox can't model are skipped, never reported.
- `cargo run --release -p pliron-clif-check -- step <step> before.clif`
  replays a report in about a second (`pipeline`, `pair`, `corpus <dir>`,
  `steps`; `--print` shows both functions). Reduce a repro by deleting
  instructions while it still prints `MISMATCH`, then add it as a test.
- `harness/bisect.py -- <cmd>` binary-searches `PLIRON_OPT_BISECT` for the
  first bad step application when a program misbehaves at run time, and
  prints the step and function it touched. `PLIRON_OPT_BISECT` unit
  numbers race across parallel codegen units, so the reported step/fn is
  only deterministic with `-C codegen-units=1` — always bisect under it.
- `cranelift-interpreter` is vendored with small fixes; see
  `vendor/cranelift-interpreter/PLIRON_PATCH.md`.

## MSVC-free Windows links (PLIRON_CRTFREE)

`PLIRON_CRTFREE=1` makes the backend emit the static-CRT pieces
msvcrt/vcruntime normally supply — `mainCRTStartup`/`wmainCRTStartup`/
`WinMainCRTStartup`/`wWinMainCRTStartup` (exec) or `_DllMainCRTStartup`
(dylib), `_tls_used`+`_tls_start`/`_tls_end`/`_tls_index` (the PE TLS
directory — `_tls_start` is a plain byte in unsuffixed `.tls`, the
bookkeeping object is first input so it anchors the template),
`_pliron_tls_cbs`, `_fltused`, the `type_info` vftable slot, and
`__security_cookie`/`__security_check_cookie`/`__GSHandlerCheck` (GS is a
stub: check-cookie compares against the sentinel, the SEH handler returns
ExceptionContinueSearch). Every emitted object gets a `.drectve` with
`/DEFAULTLIB:ucrtbase /DEFAULTLIB:vcruntime140`.

Combined with synthesized import libraries, this links with zero
MSVC/WinSDK files:

    uv run --no-project tools/mkwinlibs/mkwinlibs.py <OUTDIR>
    PLIRON_CRTFREE=1 rustc -O foo.rs -Zcodegen-backend=.../rustc_codegen_pliron.dll         -Clinker=rust-lld -Lnative=<OUTDIR>

`tools/mkwinlibs/mkwinlibs.py` dumps System32 DLL export tables via
`llvm-readobj --coff-exports` and builds `.lib` files with
`rust-lld -flavor link -def:` — kernel32/ntdll/userenv/ws2_32/dbghelp/
msvcrt/ucrtbase/vcruntime140/ole32/advapi32/shell32/user32/bcrypt/
bcryptprimitives plus `synchronization.lib` (api-set forwarders). Names
unspellable in .def (C++ operators, `=X.#N` ordinal forwards) are skipped —
rustc objects never import them. Verified end-to-end: the criterion bench
builds and runs with zero VS inputs.

## Platform notes

- windows-msvc (x86_64): `is_pic=false`, Cranelift `tls_model="coff"`.
  `src/eh/mod.rs` emits `.pdata`/`.xdata` (serialized `UNWIND_INFO` +
  `RUNTIME_FUNCTION`, `RelocationKind::ImageOffset` → `ADDR32NB`) instead of
  `.eh_frame`. Unwinding is *emulated* like the wasm backend: TLS `__pliron_eh`
  state word, flag checks after calls, `pliron.eh.exn`/`rethrow`, and
  `_CxxThrowException`/`_Unwind_Resume` interception. Catch works when std +
  panic_unwind are also backend-built (`-Zbuild-std`, run via
  `test.sh --sysroot`); with stock std, real SEH throws unwind *through* our
  frames (correct `.pdata`) but our `catch_unwind` can't intercept them —
  `tests/std/main.rs` gates those asserts on `--cfg pliron_stock_seh`.

  MSVC i128/f128 libcall ABI: args byval through pointers, return in xmm0 —
  `libcall()` in lower.rs adapts (`__udivti3`, `__floattisf`, `__rust_i128_*`).
  rsasm COFF: `.linkonce`, `.seh_*`, `3f@IMGREL` local-label modifiers; LLVM
  `$$` escapes become `$` (AT&T) or nothing (Intel syntax). Compiler-stub
  symbols (`__pliron_llvm_*`) get per-CGU-unique names on COFF (no weak syms).
- aarch64: `CallConv::Tail` can't take sret params; lower.rs keeps sret
  functions on the platform ABI. `ArgumentPurpose::StructArgument` (byval) is
  unsupported by cranelift-aarch64, but rustc never emits on-stack indirect
  args for AAPCS64.
- The asm test covers x86-64 and aarch64; PLIRON_HOT is x86-64/ELF only.
- Cross-target codegen: cranelift-codegen is built `host-arch`-only
  (Cargo.toml), so the dylib can't emit other architectures —
  `error: cranelift: Support for this target is disabled`. Flip to
  `all-arch` to cross-compile (x86_64-apple-darwin links via ld64 and runs
  under Rosetta; x86_64-macho `__eh_frame` uses SUBTRACTOR+UNSIGNED pairs,
  see src/eh/mod.rs). Still needs the target std component and a target
  linker (mold for ELF).
- Self-hosting: `selfhost/setup.sh [RUST_CHECKOUT] [WORKTREE]` creates a
  worktree at the pinned nightly commit, applies `bootstrap.patch`, and
  rsyncs this repo into `compiler/rustc_codegen_pliron`. Defaults: sibling
  `rust`/`rust-sh` dirs (or `~/work/rust`/`~/work/rust-sh` on the Linux box).
  On Linux it appends a `[target.*] linker = cc-wild` stanza; on macOS the
  system `ld` is used. `selfhost/bench.sh` is Linux-only.
- `selfhost/verify.sh [WORKTREE] [SUITE]` — run a compiletest suite under
  the pliron-built stage2 rustc and the LLVM-built stage1 baseline, then
  diff failures: stage2-only entries are pliron regression candidates.
  Handles Darwin's `rust-objcopy` PATH requirement and watchdog-kills
  rustc stuck idle (miscompiled-rustc deadlocks). Run it after optimizer
  changes that touch stage2 codegen.

## Optimizer complexity rules

Stage2 rustc functions are large enough that any O(item × fn) pass
becomes a multi-minute blowup. Invariants that keep passes linear:

- Never call a `layout.blocks()`/`inst_values` whole-function scan (or
  CFG/domtree rebuild) inside a per-inst/per-edge/per-candidate loop.
  "Who uses `v`" is `uses::Uses` (`sites`/`by_block`/`is_used`/
  `escapes`/`in_block`, `Uses::build` shares the walk with extra
  per-use tables); mutation-aware loops use incremental count maps —
  see `memspec`, `reap_dead`, `rebind_exports`.
- `resolve_aliases`/`value_def` walk alias chains linearly; passes that
  re-alias (merges, `change_to_alias`) extend them. `compact_aliases`
  runs after every mutating step — new passes should still prefer
  resolved lookups batched over per-use resolves.
- Alias chains can dead-end at reserved/tombstoned targets;
  `resolve_aliases`/`value_def` panic on them (unchecked index).
  Iterating `dfg.values()` or consuming stale pairs must use
  `try_resolve_aliases`/`try_value_def`.
- Fixpoint loops need an explicit progress bound or budget
  (`MAX_ACC_WALK`, `MAX_OPS`, `MAX_REGION_EXTRA`, `seen` sets) — a
  conservative bail leaves the function unoptimized, never wrong.
  They must also converge incrementally: seed a worklist and update
  counts/reachability per insertion (`closed_region`'s `pending`
  pred-counts) — never rescan the whole function per round.
- Overlapping per-item walks are one walk: N forward cones through the
  same blocks = one backward cone from the interesting nodes
  (`closed_region`'s leak check); K nested subgraph queries = one
  memoized walk with a shared budget (`AccMemo`).

## Fast iteration / debugging knobs

- `./test.sh --only a,b,c` — run a subset (names in the test.sh header:
  nostd, std, bcheck, loopvec, ...). The script always `cargo build`s
  first, but incremental rebuild is ~2s.
- `PLIRON_*` env gates flip passes **without rebuilding the dylib**:
  `PLIRON_<PASS>=0` disables, `PLIRON_OPT_BISECT=N` truncates the pass
  budget, `PLIRON_OPT_BISECT_DEBUG=1` logs gate decisions,
  `PLIRON_PIPELINE=a,b` selects/orders pipeline steps, `PLIRON_PAR=0`
  serializes codegen (also forced by OPT_BISECT).
  `PLIRON_INSTCOMBINE=egg` runs the legacy e-graph instcombine for A/B
  (default is the worklist peephole, ~50x faster with identical folds).
- `PLIRON_STATS=1` — per-pass rewrite counts per function.
- `PLIRON_FACTS=<substr>` — print what nowrite/nocapture/nnret/retarg/
  nounwind inferred for functions whose symbol contains the substring
  (`./test.sh --only facts` checks a few against `tests/facts/main.rs`).
- `PLIRON_TIMING=1` — per-CGU per-step wall time, sorted (stderr).
- `PLIRON_DSE_PROF=1` — per-function `dse` phase timing on stderr:
  param-src, isolated-scan, op decode, fixpoint pops/time, kill sweep.
- `-Zself-profile` records the backend's work as `pliron_*` generic
  activities (`pliron_iropt`, `pliron_ir_<pass>`, `pliron_pipeline`/
  `pliron_compile` per fn, one event per pipeline step name) — the same
  clock and viewer as rustc's own `LLVM_*` activities.
- `PLIRON_TRACE=<env-filter>` — the one logging switchboard. All
  diagnostics are `tracing` events/spans on `pliron::<module>` targets
  (bare messages on stderr — no level/target/time prefix, so stats,
  facts, and timing lines stay verbatim-greppable). Span targets:
  `pliron::module`/`pliron::phase` (per-CGU phases), `pliron::iropt`,
  `pliron::fn`, `pliron::pipeline`, `pliron::step` (every pipeline step,
  span name = step name); event targets follow the pass file stem
  (`pliron::jumpthread`, `pliron::slp`, `pliron::guardelim`, ...) plus
  report channels `pliron::stats`/`pliron::timing`/`pliron::facts`/
  `pliron::dump`. `pliron` enables all of them. (EnvFilter's
  `[{field=x}]` syntax only gates events *inside* a matching span — it
  cannot suppress a span's own close line.) Unset = no subscriber
  installed; each site costs one atomic load.
  **The legacy `PLIRON_*` diagnostic knobs still work**: `tracing_init`
  (src/lib.rs) maps each set knob to a target+level directive in
  `KNOB_TARGETS` — e.g. `PLIRON_JT_PROF=1` → `pliron::jumpthread=info`,
  `PLIRON_XPACK_DEBUG=1` → `pliron::slp=debug`, `PLIRON_UNROLL_DEBUG=3`
  → `pliron::unroll=trace`. Knob-gated prints became
  `tracing::debug!`/`info!` and `if env_set(...)`/`var_os(...)` print
  gates became `tracing::enabled!(target: ..., Level::..)`. Keep it
  that way: no `eprintln!` for diagnostics — `eprintln!` only for
  output that must appear with no subscriber (unknown-step errors,
  panic paths, diffcheck reports). Behavioral knobs
  (`PLIRON_XPACK_NOCACHE`, `PLIRON_WASM_NOSHL`, `PLIRON_PIPEFN`,
  substring filters like `PLIRON_CLIF=<substr>`) stay `env_*` reads.
  For sub-phase timing inside a pass, prefer an entered
  `tracing::info_span!(target: "pliron::<pass>", "name", field = ...)`
  over a new `PLIRON_*_PROF` env var — close lines render nested
  and parse with the same scraper. Example: `guardelim.rs`
  `gefn`/`gefix`/`geprove`.
- `samply record --save-only -o prof.json -- cargo rustc ...` — sampling
  profile of the whole build (rustc + the dylib frames symbolize);
  `samply load prof.json` opens Firefox Profiler. Use this first for
  "where is time going" before adding any instrumentation.
- `PLIRON_STEPCACHE=1` — content-addressed pipeline cache in
  `$PLIRON_CACHE_DIR` (default `~/.cache/pliron-steps`). Two levels per
  function: a whole-pipeline entry (warm reruns are ~50x faster) and
  per-step entries for the expensive passes (`Step.cached`), so a gate
  flip (`PLIRON_X=0`) invalidates only what it actually changed —
  repeat bisect runs at the same config hit fully. Keys include the
  crate build hash, so a backend edit cold-misses everything.
  `PLIRON_CACHE_LOG=1` prints hit/miss per step. Caveats: cold runs pay
  ~1.3x for hashing+storing; hits replay results rather than
  re-executing, so in-pass `tracing::debug!` prints don't fire on hits
  (aggregated `PLIRON_STATS` counters replay exactly).
- `PLIRON_CLIF=<substr>` — dump CLIF after every step for functions whose
  name contains the substring; `PLIRON_VERIFY=full` runs the cranelift
  verifier after each step; any other truthy value verifies once after
  the pipeline, then retries per-step on failure for attribution.
- Wasm always goes via FnLower + the clif-opt pipeline then
  CLIF→waffle (`src/wasm.rs::clif_emit`); the older dialect→waffle
  path was removed (`PLIRON_WASM_CLIF=0` now just warns it is ignored).
  `PLIRON_WASM_STUBLOG=<file>` writes stub reasons per function;
  `PLIRON_WASM_VERBOSE=1` / `PLIRON_WASM_PANIC=1` list stubs / unmute
  panic locations; `PLIRON_WASM_DUMP=<substr>` dumps waffle bodies.
- Quick single-file check (no test.sh):
  `rustc +nightly-2026-10-06 -Zcodegen-backend=$PWD/target/debug/librustc_codegen_pliron.dylib file.rs -o out`
- Gotcha: build-level caching does **not** track `PLIRON_*` env or the
  backend dylib. Empirically `-Cincremental` re-runs our backend every
  invocation (work-product `.o`s are stored but codegen queries rerun),
  so rustc-incremental stale reuse is not the risk — **cargo's own
  fingerprinting is**: unchanged `RUSTFLAGS` means a `PLIRON_*` flip
  never re-invokes rustc. When bisecting under `cargo build`/`-Zbuild-std`,
  `cargo clean -p <pkg>` or use a fresh target dir after gate changes.
- Dev profile: our crates at `opt-level=1`, deps at O3, vendored
  `cranelift-codegen` at O3 (flip to 1 for cheap rebuilds while patching
  ISLE/-meta — package-profile overrides can't come from the env, use
  `--config 'profile.dev.package.cranelift-codegen.opt-level=1'`),
  `debug="line-tables-only"` (revert for lldb variable inspection).
