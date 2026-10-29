# Building and testing Burn and CubeCL with the pliron backend (host side)

What this covers: compiling **Burn's and CubeCL's own Rust code** (the host
side: tensors, autodiff, the ndarray backend, the CubeCL launch code, the
runtime/JIT driver) with `-Zcodegen-backend=librustc_codegen_pliron.dylib`, and
checking that their test suites behave exactly as under stock rustc+LLVM.

What it does *not* cover: the **kernels**. CubeCL's CPU runtime JIT-compiles
the kernels it generates at run time (pliron dialect -> LLVM OrcJit); that is
the other half, see [`cubecl-burn.md`](cubecl-burn.md) (kernel JIT through our
Cranelift lowering, `CUBECL_PLIRON_CLIF=1`). In every run below the kernels are
still compiled by LLVM; only the Rust that launches them is ours.

Investigation log (the depthwise weight-gradient bug, two compiler bugs found
here): [`history/2026-10-29-burn-host.md`](history/2026-10-29-burn-host.md).

## Recipe

Machine this was done on: macOS arm64, 36 GB shared, `nightly-2026-10-22`.

### Toolchain

- Backend and Burn both build on `nightly-2026-10-22` (`RUSTUP_TOOLCHAIN`);
  Burn has no `rust-toolchain` file, `edition = 2024`, nothing needs another
  channel. Nothing global is changed.
- The `cpu` feature (CubeCL's LLVM CPU runtime) additionally needs one
  workaround: `branches 0.4.6` (a dependency of `turso_core`, via
  `cubecl-environment`) calls `core::intrinsics::abort`, which this nightly
  removed, and its `build.rs` picks that path on any nightly. Stock rustc fails
  the same way, so it is not a pliron issue. Force its stable path with a patch
  copy (a one-line `build.rs` change: never emit `branches_nightly`):

  ```toml
  # branches-fix.toml, passed as `cargo --config branches-fix.toml ...`
  [patch.crates-io]
  branches = { path = "/path/to/branches-0.4.6-with-build.rs-always-stable" }
  ```

  Do **not** put this into Burn's `.cargo/config.toml`: that file is tracked
  (it holds the `xtask` aliases).
- LLVM for the JIT: `cubecl-llvm` finds it itself (here: Homebrew LLVM on
  `PATH`); nothing extra is needed.

### Build the backend

```sh
cd <rustc_codegen_pliron checkout>
CARGO_BUILD_JOBS=3 cargo build --release
DYLIB=$PWD/target/release/librustc_codegen_pliron.dylib
```

### Build and test Burn

```sh
export RUSTUP_TOOLCHAIN=nightly-2026-10-22
cd <burn checkout>                      # tested: main 00a9db74c (cubecl 5e521ef, cubek a8207f8)

# stock
CARGO_TARGET_DIR=/scratch/tgt-stock \
  cargo test --release -j 2 --no-fail-fast -p burn-backend-tests --features ndarray,std

# pliron: separate target dir, RUSTFLAGS *and* RUSTDOCFLAGS (doc-tests)
CARGO_TARGET_DIR=/scratch/tgt-pliron RUSTFLAGS="-Zcodegen-backend=$DYLIB" RUSTDOCFLAGS="-Zcodegen-backend=$DYLIB" \
  cargo test --release -j 2 --no-fail-fast -p burn-backend-tests --features ndarray,std
```

The suites run (each on both sides, a fresh target dir per side; cargo does not
track the backend dylib, so wipe the pliron target dir whenever the dylib
changes):

| invocation | what |
|---|---|
| `-p burn-ndarray -p burn-autodiff -p burn-tensor -p burn-backend -p burn-std -p burn-ir -p burn-fusion -p burn-nn -p burn-optim -p burn-core` | host crates' own unit/integration/doc tests |
| `-p burn-backend-tests --features ndarray,std` | the backend conformance suite on the pure-Rust ndarray backend (`autodiff`, `tensor`) |
| `-p burn-backend-tests --features cpu,std` | the same suite on the CubeCL CPU runtime (autotune on; kernels JIT through LLVM) |
| `-p burn-cubecl -p burn-cubecl-fusion --features burn-cubecl/cpu` | CubeCL launch code + fusion |

and in a CubeCL checkout (tested: `5e521ef`):

```sh
cargo --config branches-fix.toml test --release -j 2 --no-fail-fast \
  -p cubecl-common -p cubecl-ir -p cubecl-opt -p cubecl-macros -p cubecl-runtime \
  -p cubecl-core -p cubecl-std -p cubecl-cpu -p cubecl-llvm
```

### Practical notes

- **earlyoom**: a cold pliron build of the cpu stack peaks above 5 GB per
  rustc; earlyoom SIGTERMs rustc under pressure (`signal: 15`). A retry
  wrapper that reruns cargo while the log contains `signal: 15` finishes the
  build (cargo keeps the finished units).
- **Disk**: one Burn workspace target dir is 5-6 GB (cpu + ndarray + tests).
  `cp -cR` (APFS clone) makes a free copy to bisect in without disturbing a
  running test.
- **Per-test results**: libtest lines `test <name> ... ok|FAILED|ignored`,
  keyed by `Running <binary>`; diff stock vs pliron per test. A test missing on
  one side means that test binary died (see the SIGSEGV note below).
- **Bisecting a pliron-only failure** (what found both bugs below): `touch`
  the failing crate's `src/lib.rs`, run its test with the pliron target dir
  `--lib`, and drive `harness/fnbisect.py` with a command that exits non-zero
  when the test fails; then `harness/bisect.py` with `PLIRON_OPT_FNS=@culprit`
  (the command must print the cargo output for the unit log), then
  `PLIRON_CORPUS=<dir>` + `clif-check pipeline`/`step` to reduce.

## Results

Date 2026-10-29, backend `cb7f1bf` (after both fixes below), Burn main
`00a9db74c` + cubecl `5e521ef` + cubek `a8207f8`, macOS arm64, release,
`nightly-2026-10-22`. Every row is "stock rustc" vs "pliron host build"; the
JIT kernels are LLVM on both sides. A test counts as a pliron-only failure if
it fails (or is missing because its binary died) under pliron and does not fail
under stock.

**Bottom line: 0 pliron-only failures in ~7.8k tests (+188 doc-tests compiled
and run by pliron), after two backend bugs found by this work were fixed**
(`bfa5499` rsasm Mach-O `.pushsection` ICE; `cb7f1bf` `jumpthread::round`
miscompile, 5 `burn-fusion` tests). Both are written up in
[`history/2026-10-29-burn-host.md`](history/2026-10-29-burn-host.md).

### Burn host-build card input

One row per crate suite; "pliron-only" is what counts against COR, "stock" is
the baseline the card subtracts (the +1 smoothing is the card's). Detail tables
follow.

| group | tests | pliron-only failures | stock failures |
|---|---|---|---|
| host crates (10 packages, 23 test binaries) | 1370 | **0** (5 before `cb7f1bf`) | 0 |
| `burn-backend-tests` ndarray: autodiff, tensor | 2494 | **0** | 0 |
| `burn-backend-tests` cpu: autodiff | 694 | **0** | 12 (copy_into bug, below) |
| `burn-backend-tests` cpu: tensor | 1542 | **0** | 2 (copy_into bug) |
| `burn-backend-tests` cpu: cubecl kernels | 192 | **0** | 1 (+2 of the 3 repro tests added here fail on both until the fix) |
| `burn-cubecl`, `burn-cubecl-fusion` (cpu) | 72 | **0** | 1 |
| CubeCL host crates (9 packages) | 1425 | **0** | 2 (trybuild, environment) |

### Tables

Read the cpu table with two caveats: stock's `tests/cubecl.rs` row is partial (that
binary died with SIGSEGV in the stock run, see Known failures), and the 2
"pliron-only" there are two of the three new repro tests, which fail on both
sides until the cubecl fix (stock evidence in the history file), so they are
not counted in the card rows. Rows with 0 pliron-only and 0 stock failures are
kept so each test binary is accounted for.

#### Burn host crates (`-p burn-ndarray -p burn-autodiff -p burn-tensor -p burn-backend -p burn-std -p burn-ir -p burn-fusion -p burn-nn -p burn-optim -p burn-core`, default features, unit + integration tests)

| suite | stock ok/fail/ign | pliron ok/fail/ign | pliron-only failures | stock failures |
|---|---|---|---|---|
| lib:burn_backend | 19/0/0 | 19/0/0 | 0 | 0 |
| lib:burn_core | 96/0/0 | 96/0/0 | 0 | 0 |
| lib:burn_fusion | 138/0/1 | 138/0/1 | 0 | 0 |
| lib:burn_ir | 27/0/0 | 27/0/0 | 0 | 0 |
| lib:burn_ndarray | 41/0/0 | 41/0/0 | 0 | 0 |
| lib:burn_nn | 569/0/0 | 569/0/0 | 0 | 0 |
| lib:burn_optim | 131/0/0 | 131/0/0 | 0 | 0 |
| lib:burn_std | 189/0/0 | 189/0/0 | 0 | 0 |
| lib:burn_tensor | 52/0/0 | 52/0/0 | 0 | 0 |
| tests/config.rs | 3/0/0 | 3/0/0 | 0 | 0 |
| tests/device_settings.rs | 1/0/0 | 1/0/0 | 0 | 0 |
| tests/device_settings_default_creation.rs | 1/0/0 | 1/0/0 | 0 | 0 |
| tests/device_settings_explicit_creation.rs | 1/0/0 | 1/0/0 | 0 | 0 |
| tests/einsum_diagonal_dtype.rs | 1/0/0 | 1/0/0 | 0 | 0 |
| tests/einsum_macros.rs | 25/0/0 | 25/0/0 | 0 | 0 |
| tests/lstm.rs | 2/0/0 | 2/0/0 | 0 | 0 |
| tests/module_state.rs | 3/0/0 | 3/0/0 | 0 | 0 |
| tests/quantize.rs | 6/0/0 | 6/0/0 | 0 | 0 |
| tests/reparameterization.rs | 6/0/0 | 6/0/0 | 0 | 0 |
| tests/shape_macros.rs | 27/0/0 | 27/0/0 | 0 | 0 |
| tests/test_derive_config.rs | 9/0/0 | 9/0/0 | 0 | 0 |
| tests/test_derive_module.rs | 20/0/0 | 20/0/0 | 0 | 0 |
| tests/test_record.rs | 3/0/0 | 3/0/0 | 0 | 0 |

#### `burn-backend-tests --features ndarray,std` (pure-Rust ndarray backend)

| suite | stock ok/fail/ign | pliron ok/fail/ign | pliron-only failures | stock failures |
|---|---|---|---|---|
| tests/autodiff.rs | 694/0/2 | 694/0/2 | 0 | 0 |
| tests/tensor.rs | 1800/0/18 | 1800/0/18 | 0 | 0 |

#### `burn-backend-tests --features cpu,std` (CubeCL CPU runtime, LLVM-JIT kernels)

| suite | stock ok/fail/ign | pliron ok/fail/ign | pliron-only failures | stock failures |
|---|---|---|---|---|
| tests/autodiff.rs | 680/12/2 | 680/12/2 | 0 | 12 |
| tests/cubecl.rs | 118/1/0 | 188/3/1 | 2 | 1 |
| tests/graph_capture.rs | 2/0/0 | 2/0/0 | 0 | 0 |
| tests/tensor.rs | 1536/2/4 | 1536/2/4 | 0 | 2 |
PLIRON-ONLY FAIL: tests/cubecl.rs::cube::kernel::conv_depthwise_wgrad::folded_image_permute_reshape_matches_reference
PLIRON-ONLY FAIL: tests/cubecl.rs::cube::kernel::conv_depthwise_wgrad::depthwise_weight_grad_matches_reference_repeated

#### `burn-cubecl`, `burn-cubecl-fusion` with `--features burn-cubecl/cpu`

| suite | stock ok/fail/ign | pliron ok/fail/ign | pliron-only failures | stock failures |
|---|---|---|---|---|
| lib:burn_cubecl | 27/1/0 | 27/1/0 | 0 | 1 |
| lib:burn_cubecl_fusion | 44/0/0 | 44/0/0 | 0 | 0 |

#### CubeCL host crates (`cubecl-common/ir/opt/macros/runtime/core/std/cpu/llvm`, rev 5e521ef + the copy_into fix)

| suite | stock ok/fail/ign | pliron ok/fail/ign | pliron-only failures | stock failures |
|---|---|---|---|---|
| lib:cubecl_common | 129/0/0 | 129/0/0 | 0 | 0 |
| lib:cubecl_core | 11/0/0 | 11/0/0 | 0 | 0 |
| lib:cubecl_cpu | 810/0/16 | 810/0/16 | 0 | 0 |
| lib:cubecl_ir | 36/0/0 | 36/0/0 | 0 | 0 |
| lib:cubecl_llvm | 7/0/0 | 7/0/0 | 0 | 0 |
| lib:cubecl_macros | 11/0/0 | 11/0/0 | 0 | 0 |
| lib:cubecl_runtime | 133/0/0 | 133/0/0 | 0 | 0 |
| lib:cubecl_std | 47/0/0 | 47/0/0 | 0 | 0 |
| tests/f16_accumulators.rs | 6/0/0 | 6/0/0 | 0 | 0 |
| tests/f16_chain.rs | 5/0/0 | 5/0/0 | 0 | 0 |
| tests/f16_evaluation.rs | 2/0/0 | 2/0/0 | 0 | 0 |
| tests/f16_per_operation.rs | 1/0/0 | 1/0/0 | 0 | 0 |
| tests/memory_ssa.rs | 33/0/0 | 33/0/0 | 0 | 0 |
| tests/mod.rs | 0/2/0 | 0/2/0 | 0 | 2 |
| tests/pre_analyses.rs | 12/0/0 | 12/0/0 | 0 | 0 |
| tests/relocation.rs | 1/0/0 | 1/0/0 | 0 | 0 |
| tests/sccp.rs | 21/0/0 | 21/0/0 | 0 | 0 |
| tests/uniformity.rs | 17/0/0 | 17/0/0 | 0 | 0 |
| tests/value_numbering.rs | 14/0/0 | 14/0/0 | 0 | 0 |
| tests/visibility.rs | 5/0/0 | 5/0/0 | 0 | 0 |


### The same cpu suites with the cubecl `copy_into` fix (branch `pliron/copy-into-same-shape`)

Built with a cargo `[patch."https://github.com/tracel-ai/cubecl"]` pointing at
the fixed copy (all cubecl crates patched together). With the fix both host
builds pass everything except one unrelated test, and agree test for test:

| suite | stock + fix ok/fail/ign | pliron + fix ok/fail/ign | pliron-only failures |
|---|---|---|---|
| `tests/autodiff.rs` (cpu) | 692/0/2 | 692/0/2 | 0 |
| `tests/tensor.rs` (cpu) | 1538/0/4 | 1538/0/4 | 0 |
| `tests/cubecl.rs` (cpu, incl. the 3 repro tests) | 190/1/1 | 190/1/1 | 0 |

The one failure is `should_panic_when_shape_cannot_store_quants` on both sides.


### Known failures (all also fail under stock rustc)

- **The `copy_into` bug** (12 autodiff conv tests, 2 `einsum` tests, plus
  intermittent SIGSEGV of the `tests/cubecl.rs` binary and, once, the
  checkpointing `attention_grads_match_composed_ops` test going NaN under
  pliron): a CubeCL CPU bug, not ours; investigation and fix in
  [`history/2026-10-29-burn-host.md`](history/2026-10-29-burn-host.md). With
  the one-condition fix both stock and pliron pass all of them (tables
  above). The SIGSEGV is heap corruption by the mis-shaped copy: stock's
  `tests/cubecl.rs` died the same way in the first run, and no run with the fix
  crashed. The one-off `attention` failure showed up only in the first pliron
  run (before the `jtround` fix), never again in four later runs; it is
  probably the same uninitialised-read bug but was not attributed.
- `cube::kernel::quantization::quantize_dequantize::should_panic_when_shape_cannot_store_quants`
  (expected panic does not happen): unrelated, fails identically on both,
  remains with the fix.
- `burn_cubecl` lib `kernel::contiguous::storage_tiled::a_tiled_weight_computes_the_same_product`:
  fails identically on both; not investigated.
- cubecl `tests/mod.rs` (`compile_fail_tests`, `compile_pass_tests`, trybuild):
  the nested cargo it spawns does not see our `--config` patch for `branches`
  and dies on `core::intrinsics::abort`; fails on both sides for that reason.
- `burn-backend-tests` has a `remote` and a `fusion` feature variant that were
  not run.

## What using the backend for the Burn host build costs

Measured under `lockf heavy.lock`, with the load average noted (the box is
shared and its load swings between 10 and 100, so wall times are indicative;
CPU seconds and RSS are the stable columns).

**Cold build of `burn-ndarray`** (`cargo build --release -p burn-ndarray -j 2`,
fresh target dir, the whole dependency tree included; "max RSS" is the largest
rustc, from `/usr/bin/time -l`):

| | wall | CPU (user+sys) | max RSS | load before -> after |
|---|---|---|---|---|
| stock rustc + LLVM | 201 s | 382 s | 2.2 GB | 52 -> 10 (other jobs finishing during the run) |
| pliron | 202 s | 580 s (1.5x) | 6.3 GB (2.8x) | 8.7 -> 18 |

pliron needed three attempts: earlyoom SIGTERMed `rustc` for `burn_ndarray` at
~5.8 GB twice (swap at 80%). So, at `-j 2`: same wall, 1.5x the CPU, and the
big crate needs a >6 GB process; on a memory-tight box that is the real cost.
A larger, unlocked and noisier sample points the same way: the full
`burn-backend-tests --features ndarray,std` cold build (all deps + tests):
stock 310 s wall / 552 s CPU / 1.3 GB vs pliron 226 s / 909 s / 5.6 GB (load
25-55); the cpu stack (`--features cpu,std`, cubecl + cubek + LLVM glue)
under pliron: 674 s wall / 2958 s CPU / 4.5 GB peak. Parallel codegen units
are why the wall time holds at low `-j` while CPU time rises (many small CGUs,
each optimized by our pipeline).

**Test-suite run time** (binaries built by each backend, `-O` release, 5 runs,
load 9-10, `burn-backend-tests --features ndarray,std`):

| suite (tests) | stock | pliron |
|---|---|---|
| autodiff (694) | 2.27 s wall (min of 5; 3.5 s CPU) | 3.05 s (4.6 s CPU): **1.35x slower** |
| tensor (1800) | 1.08 s | 1.08 s (a 1 s sleep dominates both) |

The autodiff gap is two tests: single-threaded per-test times
(`--report-time`) put `autodiff::{base,checkpointing}::cummax::should_diff_cummax`
at 1.0 s stock vs 1.5-1.7 s pliron (every other test is under 0.15 s on both).
That is the one runtime-performance gap this exercise found in Burn's host
code (a `burn-ndarray` `cummax` loop); not investigated further.

The `cpu` suites are dominated by LLVM compiling kernels at run time
(autodiff, 694 tests: 389 s stock vs 144 s pliron, run at different box
loads between 20 and 100), so host-side speed does not show in them; they are
not a useful timing signal.

