# Codegen improvements (-O)

Passes run in `finish_module` (src/lib.rs) before lowering to Cranelift, only at `-O`.
Each one can be switched off with its env var set to `0` (read when the backend compiles a crate).

Benchmark: stage2 rustc (built by this backend) compiling `regex-syntax-0.8.10/src/lib.rs`,
`--edition 2021 --crate-type lib`, best of 3 wall time, `perf stat -e instructions:u`.
Script: `selfhost/bench.sh` (`RC=<rustc>` to point it at another compiler).
Stage1 (LLVM-built) rustc: 0.79s metadata.

| pass (env toggle) | metadata | link | instr. metadata | instr. link | kept? |
|---|---|---|---|---|---|
| baseline (ade7be2) | 3.25s | 6.06s | 14.84B | 32.11B | |
| SROA + promotion to Cranelift vars (`PLIRON_SROA`) | 3.02s (−7%) | 5.44s (−10%) | 13.23B (−11%) | 28.41B (−12%) | kept |
| + notrap flags on non-volatile loads/stores (`PLIRON_NOTRAP`) | 2.96s | 6.49s (noise) | 13.23B (±0) | 28.42B (±0) | left on (no instruction change, harmless) |
| + bottom-up transitive inlining, invoke sites (`PLIRON_INLINE_BU`) | 2.51s (−17%) | 4.66s (−14%) | 11.88B (−10%) | 25.63B (−10%) | kept |
| + native 128-bit SIMD (`PLIRON_SIMD`) | 2.47s | 4.04s (−13%) | 9.12B (−23%) | 20.16B (−21%) | kept |
| + switch via Cranelift `Switch` jump tables (`PLIRON_SWITCH`) | 2.21s | 4.57s (noise) | 9.09B (−0.3%) | 19.76B (−2%) | kept |
| + cold-block layout (`PLIRON_COLD`) | 2.12s | 3.95s | 9.08B (±0) | 19.82B (+0.3%) | kept (layout only; instruction count unchanged, wall time within noise) |
| + nounwind inference, invoke→call (`PLIRON_NOUNWIND=1`) | 3.13s (noise) | 4.07s | 9.08B (±0) | 19.80B (−0.1%) | opt-in only: just 62 of ~1000 invokes in regex-syntax have provably non-unwinding callees |
| + inline memcpy/memmove/memset ≤128 B in 16-byte vector chunks (was ≤64 B, 8-byte) | 2.10s | 4.08s | 8.95B (−1.4%) | 19.49B (−1.7%) | kept |
| omit frame pointers (`PLIRON_OMIT_FP`) | – | – | – | – | no effect: regex-syntax object is byte-identical; Cranelift x64 always sets up rbp frames |
| + internal ABI: tail CC for non-escaping, non-invoked local fns (`PLIRON_TAILCC`) + sret→register return (`PLIRON_SRET2REG`) | 2.15s | 3.94s | 8.92B (−0.3%) | 19.40B (−0.5%) | kept (first version, tail CC on invoked callees too, was +1%: try_call into tail CC clobbers all registers) |
| + egg instcombine (`PLIRON_INSTCOMBINE`) | 2.13s | 3.82s | 8.88B (−0.4%) | 19.30B (−0.5%) | kept |
| + dead-argument elimination for internal fns (`PLIRON_DEADARG=1`) | 2.17s (noise) | 3.99s (noise) | 8.88B (±0) | 19.30B (±0) | opt-in only: only 107 of 2655 internal-fn params in regex-syntax are dead; `.text` −0.03% |
| inline size limit 40 → 80 ops (`PLIRON_INLINE=80`) | 2.03s | 3.70s | 8.29B (−6.6%) | 18.09B (−6.3%) | kept; stage2 build time unchanged (5:55 vs 6:26) |
| inline size limit 80 → 160 ops (`PLIRON_INLINE=160`) | 1.88s | 3.42s | 7.99B (−3.6%) | 17.37B (−4.0%) | kept; librustc_driver.so +1.8%, stage2 build 6:12 |
| inline size limit 160 → 320 ops (`PLIRON_INLINE=320`, new default) | 1.89s (noise) | 3.50s (noise) | 7.83B (−2.0%) | 17.04B (−1.9%) | kept as default; librustc_driver.so +3% vs 160 (260 MB), stage2 build 6:39; returns diminishing (−6.5% → −3.8% → −2% per doubling) |
| + single-caller local fns inlined at 10× the size limit (`PLIRON_INLINE_ONCE`) + unreferenced local fns not lowered (`PLIRON_DEADFN`) | 1.84s | 3.42s | 7.70B (−1.7%) | 16.80B (−1.4%) | kept. Ablation (stage2 rebuilt with `PLIRON_INLINE_ONCE=0`): 7.83B / 17.03B, so all of the runtime gain comes from single-caller inlining; `PLIRON_DEADFN` only cuts backend work (regex-syntax at -O: 1280 of 1956 fns not lowered, compile 3.70s → 3.42s, `.text` −21%; with both: 2.71s, −23%). librustc_driver.so 260.4 → 256.7 MB |
| + runtime-size memcpy/memmove: sizes 16–32 B as two overlapping 16-byte moves, others call libc (`PLIRON_MEMFAST`) | 1.92s (noise) | 3.44s | 7.57B (−1.7%) | 16.62B (−1.1%) | kept; librustc_driver.so +0.3% |
| + inline callees that have their own landing pads, at plain call sites (`PLIRON_INLINE_EH`) | 1.71s | 3.32s | 7.40B (−2.2%) | 16.33B (−1.7%) | kept; librustc_driver.so +2.3%. Calls to `core::hint::select_unpredictable` went 7546 → 0 |
| + drop block args whose incoming values are all one value, so inlined alloca addresses reach SROA (`PLIRON_PHISIMP`) | 1.60s | 2.97s | 6.94B (−6.2%) | 15.33B (−6.1%) | kept; regex-syntax -O: 39.8k block args removed, `.text` −9%. Found via the stage1/stage2 per-function diff: `rustc_lexer::Cursor::eat_while` was 26× stage1, its `Chars` clone stayed on the stack |
| + fold loads from immutable globals with known initializers (`PLIRON_CONSTLOAD`) | 1.66s | 3.11s | 6.88B (−0.9%) | 15.18B (−1.0%) | kept; regex-syntax -O: 4019 loads folded |
| + batch: cross-CGU internal copies of small callees (`PLIRON_XCGU`), EH callees inlined into cleanup invoke sites (`PLIRON_INLINE_EH_INVOKE`), caller growth capped at 20k ops (`PLIRON_INLINE_CALLER_MAX`), dead allocas dropped after SROA | 1.47s | 2.69s | 6.42B (−6.7%) | 14.25B (−6.1%) | kept (ablations below); without the cap one rustc hit ~20 GB on cranelift-codegen and the stage2 build was OOM-killed (2.08 GB with it). regex-syntax -O: 136 cross-CGU copies, EH invoke sites left 1193 → 167 |
| ablation of the batch: `PLIRON_INLINE_EH_INVOKE=0` | 1.49s | 2.78s | 6.58B (+2.5%) | 14.54B (+2.0%) | EH-invoke inlining kept (default off for wasm and -Copt-level=s/z) |
| ablation of the batch: `PLIRON_XCGU=0` | 1.58s | 2.84s | 6.70B (+4.4%) | 14.78B (+3.7%) | cross-CGU copies kept |
| + jump threading on Cranelift IR (`PLIRON_JUMPTHREAD`): constant/`!nonnull` edges into small pure merge blocks retargeted to the taken successor, with SSA repair; rustc `!nonnull` pointer-load facts now recorded | 1.33s | 2.61s | 6.17B (−3.9%) | 13.78B (−3.3%) | kept. rustc_lexer -O: 551 edges threaded, `.text` −3.6%, `eat_while::<is_id_continue>` 0x46f → 0x3f9 B. `PLIRON_VERIFY=1` checks each threaded function with the Cranelift verifier |
| + jump threading non-null fixpoint (`inbounds` GEPs, block params; null tests folded) + cross-block load forwarding on Cranelift IR (`PLIRON_LOADFWD`: a store only kills locations it may overlap) | 1.30s | 2.55s | 6.10B (−1.1%) | 13.64B (−1.0%) | kept. rustc_lexer -O: 2150 edges/tests threaded (was 551), 533 loads forwarded, `.text` −2.4%; `eat_while` no longer reloads the end pointer per char |
| + jump threading through jump-only forwarding blocks (params must not escape), dominating-condition folding (`PLIRON_DOMCOND`), constant-i128 bit tests as 64-bit half select (`PLIRON_PEEP`) | 1.34s | 2.57s | 6.05B (−0.8%) | 13.53B (−0.8%) | kept. rustc_lexer -O: 3316 edges/tests threaded, `eat_while::<is_id_continue>` 0x3dc → 0x358 B |
| + range threading: unsigned ranges from dominating `brif`s decide merge-block compares and dominated icmps; forwarders with dead pure bodies bypassed | 1.30s | 2.53s | 6.04B (−0.2%) | 13.49B (−0.3%) | kept. rustc_lexer -O: 4074 edges (was 3316), `.text` −2.5%, `eat_while::<is_id_continue>` 0x358 → 0x2e8 B |
| + memcpyopt sret call-slot forwarding (`PLIRON_MEMCPYOPT=1`) | 2.59s | 5.23s (noise) | 11.87B (−0.1%) | 25.63B (±0) | dropped (opt-in only) |
| one CGU per crate (`rust.codegen-units = 1`), so the inliner sees the whole crate | – | – | – | – | not measurable here: stage2 build OOM-killed (28 GB RSS) on `rustc_query_impl` and `cranelift-codegen`; pliron IR for a whole large crate doesn't fit in 31 GB |
| Cranelift `opt_level=speed` instead of `speed_and_size` | – | – | – | – | not tried: Cranelift 0.135 only checks `opt_level != none` (context.rs), so the two are identical |
| + tail merging of identical exit blocks (`PLIRON_TAILMERGE`), dropping edges into `unreachable`/`assume(false)` blocks (`PLIRON_UNREACH`), one dense `br_table` per switch at LLVM's 10% density (`PLIRON_SWITCH_DENSE`); vs range threading 6.04B / 13.49B | 1.31s | 2.44s | 5.98B (−1.0%) | 13.36B (−1.0%) | kept |
| + batch: tail duplication of ≤4-inst return blocks into jump preds (`PLIRON_TAILDUP`), narrow and/or/xor widened before `uextend` (`PLIRON_WIDEN`), `uload8/16` for extended narrow loads (`PLIRON_ULOAD`); plus `[0,1]` load ranges skip the i1 `band` (`PLIRON_BOOLRANGE`) | 1.29s | 2.45s | 5.91B (−1.2%) | 13.21B (−1.1%) | all kept, now default on; this stage2 build found and fixed a taildup alias/dominance bug (bfc5941) and a uload i128 bug (ad2f84f) |
| ablation of the batch: `PLIRON_TAILDUP=0` | 1.26s | 2.44s | 5.95B (+0.7%) | 13.28B (+0.5%) | tail duplication kept (rustc_lexer `.text` +1%, but fewer instructions executed) |
| + regalloc2 spill priority weight/(4·√len) instead of weight/len (vendored regalloc2, see `vendor/regalloc2/PLIRON_PATCH.md`) | 1.33s | 2.54s | 5.90B (−0.2%) | 13.17B (−0.3%) | kept: rustc_lexer `advance_token` stack refs 292→91; to be upstreamed once proven |
| + devirtualization of calls through folded vtable/fn-pointer slots + targeted inline round (`PLIRON_DEVIRT`), `vhigh_bits(icmp slt x, 0)` → `vhigh_bits(x)` (one `pcmpgtb` less per hashbrown probe), brif-block tail duplication (`PLIRON_TAILDUP_BRIF`), unreachable blocks lowered as `trap` | 1.25s | 2.41s | 5.77B (−2.2%) | 12.92B (−1.9%) | kept. synthetic hashbrown (`/tmp/rs/hm.rs`): 151.0M → 114.9M instructions. Unreachable-block lowering order fixed a stage2 panic in `AdtDef::eval_explicit_discr` that devirt exposed |
| + `br_table` arm index facts and `x == y` dominating facts in condition folding (`PLIRON_DOMCOND`), dead blocks dropped before taildup | 1.25s | 2.40s | 5.77B (±0) | 12.91B (−0.1%) | kept: no stage2 change, but synthetic hashbrown 114.9M → 106.8M and `hm::eq` 0x15c → 0x134 B (derive(PartialEq) enum compares re-test the discriminant) |
| + `brif` on a normalized `!bool` branches on the original (`PLIRON_BRIFNOT`), loads within frozen `&T` params marked `readonly can_move` (`PLIRON_FROZEN`) | 1.26s | 2.39s | 5.75B (−0.3%) | 12.87B (−0.3%) | kept; `hm::eq` 0x134 → 0xfe B |
| + single-store alloca forwarding before and after SROA splitting (`PLIRON_SROA_FWD`); in-loop frozen `&T` load hoisting written but opt-in (`PLIRON_FROZEN_HOIST=1`, +7% on `hm.rs`) | 1.24s | 2.44s | 5.73B (−0.3%) | 12.85B (−0.2%) | kept; `hm::get` 0x258 → 0x230 B, `hm.rs` 109.7M → 103.5M |
| + inline callees up to 4× the limit at sites passing a constant address into their indirect-call target (`PLIRON_INLINE_DEVIRT`; hashbrown's `find_or_find_insert_index_inner` taking `&mut dyn FnMut`) | 1.25s | 2.40s | 5.71B (−0.3%) | 12.78B (−0.5%) | kept; 3-monomorph `HashMap::insert` test 361.9M → 224.2M (LLVM 176.7M) |
| + loop rotation on CLIF: header exit test copied into the latch (`PLIRON_LOOPROT`) | 1.22s | 2.40s | 5.70B (−0.2%) | 12.87B (+0.7%) | opt-in only (`=1`): link got worse; `derive(Hash)` enum test 931.5M → 906.7M |
| + vendored Cranelift x64: frameless leaf functions (no `push rbp`/`mov rbp,rsp` without calls, clobbers or stack), and switches with an `unreachable` default as a power-of-two table indexed by a masked value, which skips the `cmp`/`cmov` clamp (`PLIRON_SWITCH_MASK`) | 1.30s | 2.42s | 5.59B (−2.1%) | 12.50B (−2.2%) | kept (both on); `derive(Hash)` enum test 906.7M → 855.5M (frameless) → 808.3M (mask) |
| + switch to arithmetic: a `br_table` whose cases all pass constants linear in the index to one block or `return` becomes `idx*s+b` plus one bounds test, none when the default is `unreachable` (`PLIRON_SWITCHMAP`) | 1.22s | 2.39s | 5.57B (−0.4%) | 12.48B (−0.2%) | kept; rewrites 10 tables in regex-syntax, `.text` 663.8 → 662.5 KB |
| + SROA splits allocas whose address only escapes into a call right before `unreachable` (a fresh copy is passed instead; `PLIRON_SROA_ESCAPE`), and integer accesses inside a wider integer slice become shift/mask (enum tag byte of a register-returned `Result`; `PLIRON_SROA_SUBINT`) | 1.17s | 2.30s | 5.55B (−0.4%) | 12.42B (−0.5%) | kept; regex-syntax 526 more allocas promoted, `.text` −1.7%; `unwrap()` test `get` frame 0x40 → 0x20 B |
| + x64 initial-exec TLS: under `-Ztls-model=initial-exec` (bootstrap passes it for rustc), thread-locals are `mov %fs:0, r; add sym@gottpoff(%rip), r` instead of a `__tls_get_addr` call; new `elf_ie` model in vendored Cranelift + `R_X86_64_GOTTPOFF` in vendored cranelift-object (`PLIRON_TLS_IE`) | 1.21s | 2.42s | 5.46B (−1.6%) | 12.32B (−0.8%) | kept; `__tls_get_addr` calls in `librustc_driver.so` 1514 → 38. Wall times noisy (objdump ran during the bench) |
| + fold the bounds tests switch-to-arithmetic adds against dominating range facts; ranges now pass through `ireduce` (`PLIRON_SWITCHMAP_FOLD`) | 1.18s | 2.29s | 5.44B (−0.4%) | 12.30B (−0.2%) | kept; `expect_hir_owner_nodes` checked one VecCache bucket index three times |
| + known bits of non-constant merge inputs decide tag tests after the merge; shifts/`stack_addr` count as pure merge-block ops (`PLIRON_KBITS`) | 1.18s | 2.30s | 5.43B (−0.2%) | 12.26B (−0.3%) | kept; packed `Result<u32, E>` tag tests (`(x<<32)&!0xff` vs `0x201`) now thread |
| + direct PLT32 `call` to functions in other CGUs instead of GOT load + `call *reg` (`PLIRON_PLTCALL`, vendored Cranelift emit); alias block params fed one value by every predecessor before condition folding (`PLIRON_TRIVPARAM`) | 1.15s | 2.25s | 5.39B (−0.7%) | 12.11B (−1.2%) | kept; regex-syntax `.text` −1.5% / −0.6% |
| + cross-CGU copies also for small callees of copied callees, 3 levels (`PLIRON_XCGU_DEPTH`) | 1.12s | 2.18s | 5.37B (−0.4%) | 12.06B (−0.4%) | kept; regex-syntax (16 CGUs) 136 → 147 copies |

Net at the current defaults: 5.59B / 12.50B instructions (−62% / −61% vs baseline),
1.30s / 2.42s wall (stage1 LLVM-built: 3.10B, 0.79s metadata; we're at 1.85×).
Correctness at the current defaults: `./test.sh`, `./test.sh --sysroot`, -O std/asm/unwind,
and `UI_FLAGS=-O tests/ui_run_pass.py`: 2537 pass, the remaining 57 fail identically on stock LLVM.

Re-run after merging into the backend feature branch (87db5c8, with 6c68ff9's wasm/size inline
limits): stage2 rebuilt with full-bootstrap, 6.86B / 15.10B instructions, 1.61s / 2.95s wall;
`tests/ui_run_pass.py` (debug) 2537 pass / 57 environment failures, same as before.

## Loop deletion (`PLIRON_LOOPDEL`)

`src/loopdel.rs` (LLVM `loop-deletion`, on CLIF): a single-level loop with no calls, loads,
stores or traps, whose values are not used after it, and with one exit block taking
loop-invariant arguments, is skipped by sending its preheader edges straight to that exit.
It must also be provably finite: a test that runs every iteration exits once a ±1 induction
variable reaches an invariant bound (`!=`, or `<`/`>` in the step direction). Rust allows
`loop {}`, so loops that might not terminate are kept.

Before deleting, it folds debug-assertion overflow checks on `i + 1` when a dominating guard
(`i < n`, `i != MAX`) already rules overflow out. The coretests build with debug assertions,
and without this fold their loops keep a panic exit and are not deleted.

Correctness fix, not a speedup: the coretests `split_off*_max_range*` compare `[(); usize::MAX]`
slices, a `usize::MAX`-iteration loop of `() == ()` that LLVM deletes and we used to run.
With the pass, `./x test --stage 1 library/coretests -- split_off max_range` runs its 26 tests,
including the six `*_max_range*` ones that used to hang, in 0.7 ms.
regex-syntax at -O: 0 loops deleted, `.text` 585355 B → 585363 B (+8 B) with the pass on, so
the stage2 bench is unchanged (not rebuilt). The pass kept `i += 2` / `<=` / `loop {}` /
used-result / storing loops in a negative test, and that test's output matches LLVM.

## Linker: wild vs mold

Relinking the bevy game (`examples/bevy-game`, `--host -Zbuild-std`, 323 MB debug binary) with the
captured `cc` line, best of 5, idle 8-core VM. Both outputs run (300 autoplay frames).

| linker | wall | notes |
|---|---:|---|
| Wild 0.10.0 | 0.22s | pure Rust |
| mold 3.0.0 (Rust rewrite, release) | 0.30s | links C `zstd-sys` (required) and `mold-mimalloc-sys` (off with `system-allocator`) |
