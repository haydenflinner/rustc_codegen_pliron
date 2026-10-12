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
- `tests/ui_run_pass.py [RUST_CHECKOUT] [FILTER]` — runs directive-free
  `//@ run-pass` tests from a rustc checkout (defaults: `$RUST_CHECKOUT`,
  then a `rust` dir next to this repo, else `~/work/rust`)

## Miscompile hunting

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
  prints the step and function it touched.
- `cranelift-interpreter` is vendored with small fixes; see
  `vendor/cranelift-interpreter/PLIRON_PATCH.md`.

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

## Fast iteration / debugging knobs

- `./test.sh --only a,b,c` — run a subset (names in the test.sh header:
  nostd, std, bcheck, loopvec, ...). The script always `cargo build`s
  first, but incremental rebuild is ~2s.
- `PLIRON_*` env gates flip passes **without rebuilding the dylib**:
  `PLIRON_<PASS>=0` disables, `PLIRON_OPT_BISECT=N` truncates the pass
  budget, `PLIRON_OPT_BISECT_DEBUG=1` logs gate decisions,
  `PLIRON_PIPELINE=a,b` selects/orders pipeline steps, `PLIRON_PAR=0`
  serializes codegen (also forced by OPT_BISECT).
- `PLIRON_STATS=1` — per-pass rewrite counts per function.
- `PLIRON_TIMING=1` — per-CGU per-step wall time, sorted (stderr).
- `PLIRON_STEPCACHE=1` — content-addressed pipeline cache in
  `$PLIRON_CACHE_DIR` (default `~/.cache/pliron-steps`). Two levels per
  function: a whole-pipeline entry (warm reruns are ~50x faster) and
  per-step entries for the expensive passes (`Step.cached`), so a gate
  flip (`PLIRON_X=0`) invalidates only what it actually changed —
  repeat bisect runs at the same config hit fully. Keys include the
  crate build hash, so a backend edit cold-misses everything.
  `PLIRON_CACHE_LOG=1` prints hit/miss per step. Caveats: cold runs pay
  ~1.3x for hashing+storing; hits replay results rather than
  re-executing, so in-pass `eprintln!` debug prints don't fire on hits
  (aggregated `PLIRON_STATS` counters replay exactly).
- `PLIRON_CLIF=<substr>` — dump CLIF after every step for functions whose
  name contains the substring; `PLIRON_VERIFY=full` runs the cranelift
  verifier after each step; any other truthy value verifies once after
  the pipeline, then retries per-step on failure for attribution.
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
  `cranelift-codegen` at O1 (override:
  `CARGO_PROFILE_DEV_PACKAGE_CRANELIFT_CODEGEN_OPT_LEVEL=3`),
  `debug="line-tables-only"` (revert for lldb variable inspection).
