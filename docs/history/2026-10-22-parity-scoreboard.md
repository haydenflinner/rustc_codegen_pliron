# PARITY.md scoreboard, 2026-10-18 to 2026-10-22 (with the Windows i7 re-measure)

> Moved verbatim from `docs/PARITY.md` on 2026-10-30. Superseded; the current status page is [`../PARITY.md`](../PARITY.md).

# LLVM parity: where we stand, what to do next

The goal is parity with stock rustc + LLVM on four axes: speed of the
generated code (native, wasm) and compile time (native, wasm). This page
is the current scoreboard and the ranked work list. Update it when a
number moves (`harness/parity.py` regenerates the tables).

**Compile-time rows last measured:** 2026-10-20 (branch `perf/ctime`, see
§1 "Overnight 2026-10-20"), interleaved median of 5 against stock rustc on
a shared box (load ~8). Runtime rows are older:

**Last measured:** 2026-10-18, aarch64-apple-darwin (Apple Silicon),
backend `40d729d` + parallel-CGU jobs (`src/cgupool.rs`), toolchain
nightly-2026-10-06. `harness/parity.py`, nothing else running.
Ratio = pliron / LLVM: **below 1 means pliron is better.**

| axis | pliron / LLVM | verdict |
|---|---|---|
| native runtime (geomean, 16 kernels) | **0.900** | ahead; 4 kernels still behind |
| wasm runtime (geomean, 16 kernels) | **0.898** | ahead (2026-10-21, `--layouts=3`); gaps: `strwork` +14%, `parse_only` +10% (placement-sensitive, 9-19%), `nbody` +4%, `hmget` +2% (2026-10-21) |
| wasm module size | ~1.78 regex-wasm CODE (ELIDEBLK+WIFRET+SOLO=15+TINY=6; was 1.79/1.93) | behind |
| compile time: bench kernels, native | **0.60** (0.26 s vs 0.43 s) | ahead (2026-10-21) |
| compile time: bench kernels, wasm | **1.18** (0.62 s vs 0.52 s) | behind (2026-10-21; tiny input, rustc-latency-dominated) |
| compile time: regex-syntax (~40 kLOC) | **0.93** (1.37 s vs 1.48 s) | parity on wall (2026-10-21) |
| compile time: regex-syntax, wasm32 (one CGU) | **1.02** (2.65 s vs 2.60 s) | ~parity (2026-10-21) |
| compile time: syn 2 + `full` (~50 kLOC) | **1.09** (3.49 s vs 3.21 s) | behind (2026-10-21; was 15-22) |

**Windows i7 re-measure 2026-10-22** (`dd96cb3`, box ~60% loaded —
noisy): native geomean ~1.09 pre-fix; wasm 0.961→0.955 after `ovkill2`
(chars_ascii +34.6→+21%, still behind; tokenize +14%, strwork +15%,
hashmap +12%, fib +13%; parse_only/contains_rng/iterw/matmul ahead).
ctime (release dylib, first run): kernels 0.69, kernels-wasm 1.38,
regex-syntax 1.05, regex-wasm 0.95, syn 1.71. **After this session's
compile-time fixes** (slp incremental uses 7.5→1.6s, `PLIRON_MEMCACHE`
default-on, looprot pre-check reorder): kernels 0.55, kernels-wasm
1.14, regex-syntax 0.98, regex-wasm 0.96, syn 1.37 (still worst;
remaining = jumpthread/ifconv2/etc on giant parse fns — trailer_helper
alone is ~1.8s of a 30 CPU-s corpus).
**CubeCL kernels (CPU runtime through our Cranelift path, aarch64, 2026-10-29): see [`cubecl-burn.md`](../cubecl-burn.md) for the recipe and the CubeCL parity card (`harness/cubecl_card.py`): overall 103 = SPD 78 / CMP 148 / COR 100 / MEM 101 (107 with `PLIRON_VSELECT`/`PLIRON_FMINMAX`, which are **default-on on branch `burst/cubecl-speed`, pending the gate/corpus**; numbers here were measured when they were opt-in; `=0` turns each off); kernel JIT 0.51x LLVM's time, throughput geomean 1.35x slower (max-pool and f16 GEMM worst), cubecl-cpu 1068/1068 and Burn cpu 2366/2366 tests pass.** The line below is the older, x64-era note.
cubecl `throughput` (warm cache, extreme variance): f32 ~parity
(422 vs 440 GOPS), f16 clif ~4x faster (LLJIT f16 broken on Windows),
bf16 inconclusive (clif 47-620 GOPS across runs vs llvm ~280-580),
copy 23 vs 34 GB/s, read 40 vs 14 (clif ahead), write ~parity.
Fixed this session: `bench/run.sh`'s `cargo clean -p` needed
`--profile bench` — Oct-16 bench binaries had survived every run
since; and `icmp eq v, -1` decode-tree checks surviving to codegen
(`vmax_d` `ishl`/`ireduce` arms + `ovkill2` late step: chars_ascii
native +34% → ~parity). Open: contains_rng is a missed vectorization —
LLVM emits SSE2 `pcmpgtd` count loops; `loopvec` analyzes the loop
(`mems=[(Load,0)]`) but declines; sortw/nbody/parse_only +20-29% also
unexplained (partly layout noise on a loaded box).

Older measurements on the Windows i7 box (native 1.028, wasm 1.157) are
in `docs/history/PERF-REPORT.md`; the narrative of how each kernel got
here is `docs/performance.md`. Windows i7 wasm (post `br_if` fold +
`NonNull`-i32 `PLIRON_NN_ANYINT` + `dem64` + `clifpeep`
`ext_load`/`icmpk`/`addnorm`/`icmpred`/`bitmerge`/`post_sweep` +
`divnarrow`/`ovfk`/`lih`/`band(uextend)` + `selord` (bool-algebra over
compare-outcome masks + `brif`/`sel` peels) + `unroll::merge_arms`
(`PLIRON_UNROLL_MERGE`, fixed-trip `binary_search` → `select` chain) +
`PLIRON_WASM_HOT=400` (loop-body `next_match`/`hash_one` inline) +
`wpeep` `sfwd` (`PLIRON_WASM_SFWD`, frame store→load forward — the
spliced-callee spill/reload, ~10 mem ops/iter on `tokenize`) + `wpeep`
`prel`-scoped `br_if` fold + else-arm structured `br`/`br_if`
retarget (~265 `if{br}else{structured}` sites → `br_if`, module
−1 KiB), `bench/wasm/run.sh`): **0.977 — ahead overall** — wins
`chars_alpha` −31%, `iterw` −41%, `matmul` −11%, `parse_only` −15%,
`eqbug` −9%, `sortw` −3%; `sieve` +1.9%, `contains_rng` +1.4% (parity).
Remaining >5%: `strwork` +20% and `hashmap` +16%/`hmget` +14%/`tokenize`
+16% — `strwork`'s `run` inlines ~4700 lines of `push_str`/`extend`/fmt
wrappers LLVM keeps as calls (494 lines) — `pliron-ir-opt`/`inline`
policy; `chars_ascii` +8.5%, `chars_mixed` +8.2%, `fib` +7.7%,
`nbody` +6% — emission-level locals churn.
