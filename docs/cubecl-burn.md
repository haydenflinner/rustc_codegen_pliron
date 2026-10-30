# CubeCL / Burn CPU kernels through pliron -> Cranelift

CubeCL's CPU runtime (`cubecl-cpu`, used by Burn's `cpu` feature) lowers every
kernel to a pliron `llvm`-dialect `ModuleOp`. Normally `pliron-llvm` turns that
into LLVM IR and LLVM's ORC JIT compiles it. With `CUBECL_PLIRON_CLIF=1` the
same `ModuleOp` instead goes through this repo's pipeline
(`pliron-clif-lower`: scan, ir-opt, Cranelift, our CLIF passes), comes out as a
native object, and loads through the same ORC JIT wrapper. **Both paths live in
one binary; the env var picks one per process.** This page is the recipe, the
test table and the numbers. (A different integration, building Burn/CubeCL's
*host* code with `-Zcodegen-backend`, is in [`cubecl-burn-host.md`](cubecl-burn-host.md).)

Status, 2026-10-29, Apple M3 Max (aarch64, macOS):

- Builds against current main, runs, and passes everything the LLVM path
  passes: **cubecl-cpu 1068/1068, Burn `burn-backend-tests` (cpu, fusion)
  2366/2366** under both paths (0 failures on either).
- Kernel JIT is about **2x faster** than LLVM (per-kernel geomean 0.51, first
  kernel of a process 0.44). Peak RSS is parity.
- Kernel throughput is **behind**: geomean 1.35x slower over 45 rows
  (f32 compute and memory parity; max-pool forward ~2.7-3x, f16 GEMM ~2x slower).
  Two lowering flags (`PLIRON_VSELECT`, `PLIRON_FMINMAX`, below; measured as opt-in, **default-on on branch `burst/cubecl-speed` pending the gate**, `=0` kills) bring it
  to 1.23x. Objects are 1.7x larger.
- CubeCL parity card ([`harness/cubecl_card.py`](../harness/cubecl_card.py)):
  **103 overall** (SPD 78, CMP 148, COR 100, MEM 101); **107** with the two flags
  (SPD 85); see [The card](#the-card).

## Checkouts and branches

| repo | path | branch | base | what the branch adds |
|---|---|---|---|---|
| this repo (`rustc_codegen_pliron`) | any checkout, call it `$PLIRON` | main | | `pliron-clif-lower`, vendored pliron + Cranelift, harness scripts |
| CubeCL fork | `~/code/cubecl` | `pliron/kernel-jit` | fork `main` (`68fa214c` + the Oct-16 CLIF commit `902ef338`) | port to the vendored pliron API; `CUBECL_PLIRON_LOG`; `CUBECL_PLIRON_DUMP_OBJ`; clif-keyed cache fingerprint; absolute `LOCAL` paths to `$PLIRON` |
| CubeK fork | `~/code/cubek` | `pliron/kernel-jit` | fork `main` | `[patch]` paths to `$PLIRON` |
| Burn fork | a worktree of `~/code/burn` | `pliron/kernel-jit` | fork `main` (= upstream `00a9db74c`) | path deps on the local cubecl/cubek, the patch tables, `turso =0.8.2`, three compat edits for cubecl main |

The branches are local only (never pushed, no PRs). Patches of each are in
[`docs/cubecl-burn/`](cubecl-burn/) (`git apply`; cubecl.patch is relative to `902ef338`, the other two to `main`).

Burn's pinned cubecl (`5e521ef`) is ~90 upstream commits older than the fork's
cubecl, so Burn's `cpu` build uses the fork's cubecl and needed three small
fixes (all in the Burn branch): `F::new` takes `f64` (51 literals),
`cubecl_common::ue8m0` replaced the `quant::scheme` helpers, and
`tensor_map_store` returns `PendingExpand<()>`.

## Cargo wiring

Cargo `[patch]` tables only work in the workspace root, so each of cubecl,
cubek and burn carries the same block pointing the unified `pliron`,
`pliron-llvm`, `pliron-derive`, `regalloc2`, `cranelift-*` and `rsasm` at
`$PLIRON/vendor/...`; that makes the `Context`/`TargetIsa` types the engine
passes into `pliron-clif-lower` the same types. `cubecl-llvm` depends on
`pliron-clif-lower = { path = "$PLIRON/pliron-clif-lower" }`. All of these are
absolute paths marked `LOCAL (pliron/kernel-jit)`. Re-point them at your
checkout:

```sh
PLIRON=$HOME/code/rustc_codegen_pliron      # any checkout of this repo at or after the commits below
OLD=/Users/wow/code/purerust/rustc_codegen_pliron/.claude/worktrees/agent-a5b6f3bbe42f9acb7
for f in ~/code/cubecl/Cargo.toml ~/code/cubecl/crates/cubecl-llvm/Cargo.toml ~/code/cubek/Cargo.toml $BURN/Cargo.toml; do
  sed -i '' "s#$OLD#$PLIRON#g" "$f"           # GNU sed: drop the ''
done
```

Why the vendored pliron matters: it replaced the per-op operand/result/region
storage with context-global pools and interned attributes, so `Operation`
accessors take a `&Context` and attributes are immutable. `cubecl-ir`,
`-opt`, `-core`, `-cpp` and `-wgpu` were ported (mechanical: add `ctx`; the one
semantic change is `FuncInterface`'s argument/result attributes, which now go
through a read-copy-set helper in `cubecl-ir/src/attributes/entrypoint.rs`).
Upstream pliron's crate (`pliron-spirv` etc.) is **not** ported: build the
`cubecl-spirv`/Vulkan backends against stock pliron, not this tree.

LLVM itself comes from `tracel-llvm-bundler` (LLVM 23.1.0-3, downloaded once
to `~/.cache/tracel`, 58 MB); Homebrew's LLVM is not used. The LLVM path needs
it, the CLIF path only needs it for the ORC JIT object loader.

## Build

Stable Rust is enough (cubecl's `rust-version` is 1.95; tested on 1.99.1). Use
`CARGO_BUILD_JOBS=3` on a shared box. Disk: ~8.5 GB cubecl `target`, 3.2 GB
cubek, 5.5 GB burn.

```sh
# CubeCL: the throughput examples and the cpu runtime's own test binaries
cd ~/code/cubecl
cargo build --release -p throughput --no-default-features --features cpu --examples
cargo test  --release -p cubecl-cpu --no-run       # cubecl-core + cubecl-std runtime tests run inside this

# CubeK benchmarks (cubecl's cpu runtime; ids are lower-case substrings for the filters)
cd ~/code/cubek
cargo build --release -p benchmarks --bench reduce --bench gemm_cpu --bench contiguous --bench pool

# Burn's CPU backend tests (the alias `cargo test-cpu` in crates/burn-backend-tests/.cargo)
cd $BURN
cargo test --release -p burn-backend-tests --no-default-features --features cpu,std,fusion --no-run
```

## Run, and check which path ran

```sh
CUBECL_PLIRON_CLIF=1 CUBECL_PLIRON_LOG=1 target/release/examples/compute_direct
# [cubecl-pliron] path=clif kernel=compute_direct_throughput_i_f32_n_4_... obj_bytes=... codegen_us=... total_us=...
#   compute-direct f32    439.4 GOPS/s
```

| variable | effect |
|---|---|
| `CUBECL_PLIRON_CLIF=1` (any value) | JIT through pliron -> Cranelift; unset = LLVM |
| `CUBECL_PLIRON_LOG=1` | one stderr line per compiled kernel: `path=clif|llvm`, object bytes, `codegen_us` (scan + ir-opt + lowering, or LLVM passes + codegen), `total_us` (adds the ORC load). `=<file>` appends to that file instead (test harnesses) |
| `CUBECL_PLIRON_DUMP_OBJ=<dir>` | write `<dir>/<kernel>.<path>.o`; `llvm-objdump -d --no-show-raw-insn` it, compare the two paths |
| `CUBECL_DEBUG_PLIRON=<dir>` | (cubecl built with it set at *build* time, feature `pliron-dump`) kernel `.pliron`, `llvm.ll`, `kernel.o`; a `.pliron` replays through `pliron-clif-lower/examples/plirun.rs` |
| `PLIRON_CLIF=<fn substr>` | dump CLIF after every step for matching kernels (kernel names are `<name>_<hash>`; `test_function` matches the runtime tests) |
| `PLIRON_<PASS>=0` | kill switch for one of ~80 passes (`PLIRON_SIMD=0` flattens vectors to scalars, `PLIRON_VEC`, `SLP`, `LICM`, `PEEP`, `INSTCOMBINE`, `SROA`, `INLINE=0`, ...); `PLIRON_STATS=1`, `PLIRON_TIMING=1` |
| `PLIRON_DIFFCHECK=step`, `PLIRON_OPT_FNS`, `PLIRON_OPT_BISECT` | the usual bisect tooling, see [`miscompile-triage.md`](miscompile-triage.md) |

Facts about the integration: the persistent compilation cache is off by default
(`[compilation] cache = true` in `cubecl.toml` turns it on); when on, the device
fingerprint carries a `-clif` suffix under `CUBECL_PLIRON_CLIF` so objects from
the two paths never mix. The kernel throughput probes cache results per device
fingerprint; a `cubecl.toml` with `[throughput] disable_cache = true` in the
working directory makes every run measure.

Host SIMD knobs (`types::WIDE_SIMD`, `NATIVE_F16`, `F16C`, `WIDE_F32_BITS`) are
read off the Cranelift ISA flags in `engine.rs::compile_clif`, so leaf planning
cannot disagree with the backend: aarch64 fp16 gives packed f16x8, AVX2/512
gives wide f32 splits.

## Tests

[`harness/cubecl_tests.py`](../harness/cubecl_tests.py) builds the test
binaries, runs each once per path, reruns tests that were unfinished when a
binary died one process each, checks no kernel took the other path
(`CUBECL_PLIRON_LOG=<file>`), and writes `target/parity/cubecl-tests.json`.

```sh
harness/cubecl_tests.py --dir ~/code/cubecl --suite cubecl-cpu --threads 8 -- --release -p cubecl-cpu
harness/cubecl_tests.py --dir $BURN --suite burn-cpu --threads 4 -- --release -p burn-backend-tests --no-default-features --features cpu,std,fusion
```

| suite | LLVM | CLIF (final) | CLIF (first run) |
|---|---|---|---|
| `cubecl-cpu` (lib: `cubecl-core` runtime tests x f16/bf16/f32/f64, i8-i64, u8-u64, `cubecl-std`; plus `f16_*`, `persistent_cache`, `relocation`) | 1068 pass, 0 fail, 20 ignored (1453 kernels, 78 s) | 1068 pass, 0 fail, 20 ignored (1453 kernels, 52 s) | 989 pass, **79 fail** |
| `burn-backend-tests` (`cpu,std,fusion`: tensor, autodiff, cubecl, f16 variants, fusion; 8 binaries) | 2366 pass, 0 fail, 140 ignored (4288 kernels, 368 s) | 2366 pass, 0 fail, 140 ignored (4287 kernels, 1023 s) | (ran once, after the fixes) |

`cubecl-llvm` has no tests of its own. The CLIF burn run's wall time is longer
(1023 s vs 368 s) but both ran at machine load 25-110 from other jobs, and
Burn's autotune and the slower pooling/f16 kernels below weigh in; do not read it
as a compile-time number.

The 79 first-run CLIF failures were all f16 (and `bf16_conversion`), from four
bugs, all fixed in this repo with tests that fail without the fix:

| commit | bug |
|---|---|
| `78fd569` | vendored `pliron-derive`: `format_op`'s `region($n)`/`regions` directives did not pass `ctx` to the pool accessors, so no op with a region format compiled against the vendored pliron (found porting `cubecl-ir`) |
| `21efaff` | aarch64: `fvdemote_f16` to a 64-bit `f16x4` leaf (CubeCL's 4-lane f16 lines) had no ISLE rule, so every f16 x 4 kernel panicked in the JIT ("should be implemented in ISLE"). Test `pliron-clif-opt/tests/f16x4_demote_aarch64.rs` |
| `ce117a4` | aarch64: `FPUOp1::Cvt16To32`/`Cvt32To16` had opcode bits 000000/000011, i.e. `fmov h,h` and `fsqrt s,s`: **every scalar f16 widen/narrow was garbage** while the vcode text still said `fcvt`. Test `aarch64_f16_cvt_encoding.rs` checks the emitted words |
| `ec415fc` | aarch64: the packed f16 demotes (`f16x4` and the `fvdemote_f16_pair` rule) passed the *source* lane size to `fcvtn`, encoding `fcvtn v.2s,v.2d` (f64->f32) instead of `v.4h,v.4s`. Test checks both words |

Anything failing on both paths would be listed here as "not ours"; there is
none (0 failures on LLVM in either suite). cubek's own test suites were not run
(they pick wgpu unless one other runtime feature is on, and their dev-dependency
features do not forward from the command line); the cubek *benchmarks* below do
run on the CPU runtime.

## Measurements

`harness/cubecl_bench.py` (run under `lockf <heavy.lock>`; other agents share
the box, load was 6-10 during the official 3-rep runs (an earlier 5-rep run at load 13-40 agreed within 0.01 on JIT time), medians; the
per-row ratios are interleaved so they are less noisy than the absolute numbers):

```sh
K=~/code/cubek/target/release/deps
lockf $LOCK harness/cubecl_bench.py --reps 5 --cubek-samples 5 \
  --cubek-bench "reduce=$K/reduce-<hash>|CUBEK_BENCH_STRATEGIES=unit;CUBEK_BENCH_PROBLEMS=sum" \
  --cubek-bench "gemm_cpu=$K/gemm_cpu-<hash>|CUBEK_BENCH_STRATEGIES=cpu_gemm" \
  --cubek-bench "contiguous=$K/contiguous-<hash>" --cubek-bench "pool=$K/pool-<hash>"
harness/cubecl_card.py
```

### Kernel JIT time (348 kernels, both paths compile the same kernels)

| | LLVM | CLIF | CLIF/LLVM |
|---|---|---|---|
| per-kernel codegen + load, geomean | | | **0.51** (median kernel 8.0 ms vs 3.2 ms) |
| per-kernel codegen only, geomean | | | 0.50 |
| first kernel of a fresh process ("cold"), median over 9 workloads | 13.3 ms | 6.3 ms | 0.44 (63 vs 27 ms at load 13-40: LLVM's start-up cost is the load-sensitive part) |
| all 348 kernels summed | 3.02 s | 1.60 s | 0.53 |
| kernels where CLIF is slower | | 11 of 348 (3.2%) | worst 1.25x (`kernel_fp8`; bf16/f16 compute n=32, 1.13-1.2x) |

"Cold" is the first compile in a process (LLVM target init, ORC session, cold
caches); "warm" is every later one. CubeCL has no cross-process kernel cache
unless enabled in `cubecl.toml`, so each process's first kernel is a real cold
compile.

### Kernel throughput (45 rows; time ratio = LLVM rate / CLIF rate, > 1 means CLIF is slower)

Default config, 3 interleaved reps at load 6-10 (`target/parity/cubecl-measure.json`;
an earlier 5-rep run at load 13-40 gave the same picture, SPD 76). Geomean **1.35**
(card SPD 78).

| group | CLIF time / LLVM time |
|---|---|
| `compute_direct` f32 / f16 / bf16 | 1.03 / 0.90 / 1.00 (f32 ~404 GOPS, f16 ~800, bf16 ~410) |
| memory read / write / copy (0.5-1 GiB) | 0.99 / 1.01 / 0.99 (bandwidth rows are noisy: 0.4-1.2x between runs) |
| reduce `Unit` sum f32 / f16 | 0.91-1.09 / 0.98-1.01 |
| contiguous (swap) | 1.25 |
| gemm_cpu (cubek `CpuGemm`) f32 | 0.96-1.21 |
| gemm_cpu f16 | 1.21-2.04 (fast leaf ~2.0 at 512 and 1024, tile-DSL 1.2-1.55) |
| pool (`max_pool2d` fwd / bwd, `avg_pool2d`, adaptive) | 1.2-3.0: `max_pool2d_forward_deep` 2.96, `forward_resnet_init` 2.71, `backward_resnet_init` 2.29, `adaptive_avg_pool2d_bwd_global` 2.13 |

The pool rows are two lowering gaps, found by diffing the objects
(`CUBECL_PLIRON_DUMP_OBJ`, `llvm-objdump`; details in
[`history/2026-10-29-cubecl-kernels.md`](history/2026-10-29-cubecl-kernels.md)):
a `<16 x f32>` `select(fcmp ogt ..)` was split into 16 scalar `fcmp`+`csel`, and
`llvm.maxnum` became one `fmaxf` libcall per lane. Both now have an opt-in fix,
default off like any new transform:

| flag | what |
|---|---|
| `PLIRON_VSELECT` (default on on the burst branch; `=0` off) | fuse `select(fcmp/icmp(a, b), x, y)` on vectors of 32-bit lanes (all 128-bit native leaves) into the existing `pliron.vcmp.*` + `pliron.vbitselect`, per leaf (vector `fcmp` + `bitselect`) |
| `PLIRON_FMINMAX` (default on on the burst branch; `=0` off) | `llvm.minnum`/`maxnum` inline (`fmin`/`fmax` + NaN selects) instead of `fminf`/`fmaxf` calls; per native leaf on vectors, lane by lane on 64-bit ones |

With both on (3 reps, load 9-10): max-pool forward 1.46 / 1.16 (from 2.7 / 3.0),
`avg_pool2d_backward` 1.14, geomean **1.23** (card SPD 85, overall 107); f16 GEMM
is now the biggest gap (1.7-2.0x). cubecl-cpu passes 1068/1068 under CLIF with
both flags on. Their semantics are pinned by native tests (`pliron-clif-opt/tests/vecsel_native.rs`: special values, every predicate) and IR tests (`pliron-clif-lower/tests/ir_peeps.rs`), see [`history/2026-10-29-cubecl-speed.md`](history/2026-10-29-cubecl-speed.md). They are aarch64-tested only; `fcmp uno`/`bitselect` on 64-bit
vectors are not lowered by the vendored aarch64 ISLE (hence the lane-by-lane
path), x64 was not run. Enable them for CubeCL with
`CUBECL_PLIRON_CLIF=1` (the flags are on by default on the burst branch; before the flip add `PLIRON_VSELECT=1 PLIRON_FMINMAX=1`). Before merging the flip they need the rustc-corpus gate (`llvm.maxnum` is also `f32::max`). Opt-in `PLIRON_F16SPLAT=1` (f16 broadcast fold for GEMM, unmeasured) is on the same branch.

Previous PARITY line ("f16 clif ~4x faster, bf16 inconclusive") predates the
two aarch64 f16 encoding fixes and was measured on a different host; on this
host f16 and bf16 compute are at parity.

### Memory and size

Peak RSS (`/usr/bin/time -l`), nine runs: card MEM 0.99 geomean. The runs that
are compile-dominated (`compute_direct`: 72.5 MB LLVM vs 67.9 MB CLIF; cubecl-cpu
f32 test subset on one thread: 1066 vs 1095 MB; gemm_cpu 1287 vs 1225 MB) are
within +-6%; the rest are dominated by the benchmark's buffers. Object size per
kernel: CLIF is **1.68x** LLVM's (geomean; 504 KB vs 284 KB total over 348
kernels; median 1132 vs 712 B). Not rated on the card.

### The card

`harness/cubecl_card.py`, modelled on `harness/parity.py` (rating =
100 + 50*log2(LLVM/CLIF), clamped to 0-150), writes `target/parity/cubecl.json`
(`--measure target/parity/cubecl-measure-flags.json --out cubecl-flags.json` for the flags run):

| config | SPD | CMP | COR | MEM | overall |
|---|---|---|---|---|---|
| default | **78** | **148** | **100** | **101** | **103** |
| `PLIRON_VSELECT=1 PLIRON_FMINMAX=1` | 85 | 150 | 100 | 101 | 107 |

weights SPD 40, CMP 25, COR 25, MEM 10. COR = (CLIF-only failures + 1) /
(LLVM-only failures + 1) over both suites = 1 (3434 tests, no failure on either
path). SPD is the throughput geomean over the 45 rows above, CMP the 348-kernel
geomean, MEM the nine-run RSS geomean. Reproduce with the commands under
[Measurements](#measurements) (`--tag default` / `--tag flags --clif-env ...`).

## What is next

- f16 GEMM (~2x slower: per-leaf widen/narrow where LLVM keeps packed ops) is the
  largest remaining gap; flip `PLIRON_VSELECT`/`PLIRON_FMINMAX` on once the rustc
  corpus is clean ([`history/2026-10-29-cubecl-kernels.md`](history/2026-10-29-cubecl-kernels.md)).
- x86_64 has not been run at all (this was an aarch64 Mac): the f16 bugs above
  were aarch64-only, x64 has separate ISLE rules, so expect a different set.
- Burn's own pinned cubecl (`5e521ef`) is not supported by this port; moving
  Burn forward to cubecl 0.11 upstream would remove the three compat edits.
- cubek's `cargo test` suites on the CPU runtime.
