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
| + non-null facts for TLS/symbol/function/stack addresses (`PLIRON_NN_ADDR`) + inliner diagnostics | 1.18s | 2.35s | 5.37B (±0) | 12.06B (±0) | kept; no regex-syntax change (the facts matter in rustc code, not this crate) |
| + xcgu copies for `InstanceKind::Shim` (drop glue, closure-once, reify/fnptr shims) | 1.17s | 2.27s | 5.37B (±0) | 12.06B (±0) | kept; no regex-syntax change either — shims are rare in this crate — but it's the right thing (LLVM can inline those bodies) |
| + SROA copies untyped gaps as the widest unaligned chunk instead of offset-aligned 1/2/4/1 pieces (`PLIRON_SROA_GAPWIDE`) | 1.18s | 2.33s | 5.34B (−0.6%) | 12.02B (−0.3%) | kept; query accessors (`TyCtxt::features`, `as_lang_item`) loaded an `Erased<[u8;8]>` as 4 pieces + shift/or/mask reassembly and kept 4 callee-saved regs live; now one 8-byte `mov` like LLVM |
| + dead block-param elimination (`PLIRON_DEADPARAM`) + range facts without a dominating test and through `uextend`/`clz`/`k-x`/`ushr` (`PLIRON_RANGE2`) | 1.20s | 2.35s | 5.28B (−1.1%) | 11.89B (−1.1%) | kept; measured together. regex-syntax `.text` 559,422 → 547,301 (deadparam, 9,063 params removed) → 546,814 B (range2, 3,204 → 3,732 conditions folded). `expect_hir_owner_nodes` 0x700 → 0x4c8 bytes (LLVM 0x26c): the `uextend u32 > u32::MAX` tests and the VecCache bucket bounds checks are gone |
| + tail merge rematerializes constant operands in the kept block (fixes a non-dominating use, `80f6be3`) + xcgu accepts copies as a greatest fixpoint, admitting `#[inline]` callees of copies (`PLIRON_XCGU_FIXPOINT`, `2ddc80a`) | 2.04s* | 3.90s* | 5.08B (−3.8%) | 11.52B (−3.1%) | kept; measured together. `thread_local!` `.with()` is now a direct `%fs:` access as in LLVM; regex-syntax `.text` −2.0% (451 vs 148 copies), `cranelift-codegen` peak RSS +0.25%. *wall inflated: the box was running other builds during all 3 runs |
| + noalias/stack-slot load forwarding across calls and other-root stores (`PLIRON_NOALIAS_FWD`, `PLIRON_FWD_SLOTS`, `52f715a`) + no ext of narrow args/returns on internal tail-CC calls (`PLIRON_TAILCC_NOEXT`, `648a3d9`) + loads kept across calls to inferred write-free callees, incl. panic-only-writing ones on the normal edge of `try_call` (`PLIRON_NOWRITE`, `PLIRON_NOWRITE_EH`, `bb9ac15`/`62439f7`) | 1.10s | 2.13s | 5.08B (±0) | 11.50B (−0.2%) | kept; measured together (b34). regex-syntax 0.8.5 `-O`: 2160 → 2281 loads forwarded, `.text` 578785 → 578412 B. Small instruction-count win: rustc's hot paths mostly reload through `&mut self`/shared refs passed to callees, which none of these can keep |
| + dead-store elimination for stack slots nothing reads: no loads, address never escapes (`PLIRON_SLOT_DSE`, `e869dc0`) | 1.19s | 2.27s | 5.05B (−0.6%) | 11.46B (−0.3%) | kept. regex-syntax `.text` 578,412 → 577,852 B (119 stores removed); mostly dead closure-environment stores (e.g. hashbrown lookup). |
| + x86-64 callees no longer zero/sign-extend narrow (`bool`/`u8`/`i16`) return values: SysV leaves bits above `%al`/`%ax` unspecified and neither LLVM- nor gcc-compiled callers rely on them; LLVM-built stage1 has 0 `movzx; ret` pairs, b34 had 4,218 (`PLIRON_RET_NOEXT`, `3cccf12`) | 1.21s | 2.23s | 5.05B (±0.0%) | 11.44B (−0.2%) | kept. regex-syntax `.text` 577,852 → 576,736 B (16 → 0 `movzx; ret`). A C caller reading Rust `extern "C"` `bool`/`u8`/`i8`/`i16` returns, and Rust callers through fn ptrs and `dyn`, print the same with the toggle on and off. |

Net at the current defaults: 5.08B / 11.52B instructions (−66% / −64% vs baseline)
(stage1 LLVM-built: 3.10B, 0.79s metadata; we're at 1.64×).
Correctness at the current defaults: `./test.sh`, `./test.sh --sysroot`, -O std/asm/unwind,
and `UI_FLAGS=-O tests/ui_run_pass.py`: 2537 pass, the remaining 57 fail identically on stock LLVM.

## CLIF loop optimization pipeline (aarch64/macOS session)

New passes at `-O` in `lower.rs`, all off-switchable (`PLIRON_*=0`), verified
with `PLIRON_VERIFY=1`:

| pass | file | notes |
|---|---|---|
| LICM | `src/licm.rs` | hoists speculatable insts + unclobberable `notrap` loads to the preheader, sinks invariant stores to exit edges, and promotes isolated loop memory to registers (scalar promotion) using `noalias`/`dereferenceable` attrs plumbed from rustc param attributes |
| indvars | `src/indvars.rs` | `iv*K` strength reduction to a recurrence block param. Narrow-IV widening deliberately not done: wrapping `i += 1` is observable and CLIF carries no nowrap flags, so it is unsound in general |
| loop idiom | `src/loopidiom.rs` | zeroing/copy loops → `memset`/`memcpy` under runtime fast-path guards (order, bounds-check implication, overlap versioning); the scalar loop stays as fallback |
| loopvec | `src/loopvec.rs` | elementwise loops → 128-bit vector loop + scalar epilogue. Contiguous affine streams, lane-wise op whitelist (vector shifts take a scalar amount — no per-lane shifts), `icmp` masks + `bitselect` for selects, transitive invariance re-materialization, runtime disjointness predicates, SIMD targets only |

Also: load PRE (`PLIRON_LOADPRE`) is composed before normal load forwarding and
now defaults on; inliner callsites get a 4× size bonus for const args that
devirtualize a call and 2× for const args that fold a callee branch
(`PLIRON_INLINE_DEVIRT`/`PLIRON_INLINE_FOLD`); `PLIRON_MEMCPYOPT` and
`PLIRON_LOOPROT` flipped to default-on.

Notable traps found:

- `PLIRON_NOUNWIND` default-on **miscompiles wasm emulated EH** — the
  `llvm.wasm.throw`/`__pliron_eh` unwinding path goes through ordinary calls
  the body scan can't see, so `invoke → call` conversion is unsound there.
  Reverted to opt-in (`PLIRON_NOUNWIND=1`).
- Vectorizer correctness bugs caught by the Cranelift verifier + the bevy
  build: vector shift amount must be scalar; invariant insts defined outside
  the loop must be used directly, not cloned; same-stream `store; load`
  program order is observable and must be preserved (mems emitted in layout
  order).
- `switchmap::resolve` could jump through a forwarder block whose inst
  results were still used downstream, orphaning their defs (pre-existing;
  fixed with a `confined` check).

Bevy demo (`examples/bevy-game`, 600 frames, Apple M3 Max, Metal, release):
stock rustc 18.07 ms/frame, this backend 18.08 ms/frame — identical; the
workload is presentation/driver-bound so the codegen wins don't show here.
(A single earlier "3 ms" reading was a macOS throttling artifact.)

## Microbenchmarks (vs stock LLVM rustc -O)

`perf stat -e instructions:u`, best of 3 (runs agree to <0.001%). md5: RustCrypto `md-5` hashing a buffer in a loop,
digest checked against the LLVM build.

| Change | md5 (LLVM: 2.667B) | Notes |
|---|---|---|
| baseline (b36 defaults) | 4.783B (1.79×) | the `[u32; 16]` input copy in `compress` stays a loop over a stack array, with the `try_into().unwrap()` `Result` round-tripping through two stack slots |
| + full unroll of constant-trip loops on the final CLIF (`PLIRON_UNROLL`, src/unroll.rs): from the loop's single entry edge, constants (incl. folded pre-loop values like `(p+64-p)/4`) and stack-slot bytes stored during the trip are propagated; when every branch on the path is decided within 32 header visits / 400 cloned insts, the trip becomes one straight-line block | 3.306B (1.24×, −30.9%) | kept. `compress` stores now hit constant offsets, so `loadfwd` keeps the words in registers. regex-syntax -O: 6 loops unrolled, `.text` 557,076 → 557,434 B (+0.06%). Not yet in a stage2 benchmark |
| + constant evaluator folds imul/udiv/urem/shifts/rotates (`PLIRON_EVAL_ARITH`, used by jump threading and unroll), so unrolled addresses like `v30 + 0*4` become `iconst` offsets loadfwd can see | 3.277B (1.23×, −0.9%) | kept. `compress` round block: 20 → 4 `load.i32` after loadfwd (the 4 left are the hash state), but stack refs 116 → 123 as the 16 words now compete for registers. regex-syntax -O `.text` 557,434 → 557,338 B |
| + remove unused `notrap` loads (Cranelift keeps them), alternating with slot DSE so a load whose only user was a dead store stops keeping its slot alive (`PLIRON_DEAD_LOADS`) | 3.038B (1.14×, −7.3%) | kept. `compress` 804 → 748 instructions, stack refs 123 → 83: the 16 unrolled `try_into` temporaries (`movb $0,(%rsp)` + stores into 8-byte slots) are gone. regex-syntax -O `.text` 557,338 → 555,055 B |
| + the same dead-code sweep also drops unused side-effect-free, non-trapping ops (and `udiv`/`urem` by a nonzero constant) (`PLIRON_DEAD_PURE`) | 3.038B (flat) | kept for size: regex-syntax -O `.text` 555,055 → 554,093 B |
| + after a successful unroll: drop the skipped loop's now-unreachable blocks, fold `brif c, B(a), B(a)` to `jump`, then dead params / dead code to a fixpoint (`PLIRON_UNROLL_CLEANUP`), so the leftover trip-count `isub end, base` no longer escapes the `[u32; 16]` slot and slot DSE deletes its 16 stores + zeroing | 2.925B (1.10×, −3.7%) | kept. `compress` 748 → 716 instructions, stack refs 83 → 74. regex-syntax `.text` 554,093 → 554,085 B |

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

Branches inside the loop are decided with the values `jumpthread` can prove without rewriting
the function (constants and conditions implied by a dominating test, `known_values`), and uses of
loop values only count in blocks still reachable once the loop is bypassed. That deletes the
`<`/`<=`/`cmp`/`partial_cmp` loops on two `[(); usize::MAX]` slices at -O and at
`-Copt-level=3 -Cdebug-assertions=y` (6 loops, same output as LLVM; with `PLIRON_LOOPDEL=0` it
still hangs). -O0 runs no CLIF passes and LLVM -O0 hangs on it too. Bounds checks the loop test
does not imply (`v[i]` with `i < n`, `v[i + 1]`, ZST `v[i]` past the length) still panic like
LLVM. regex-syntax at -O: 0 loops deleted, `.text` 558312 B → 558296 B (−16 B).
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

## aarch64 codegen push (later session, same box)

More passes at `-O`, all `PLIRON_*`-switchable:

| pass | file | notes |
|---|---|---|
| if-conversion | `src/ifconv.rs` | select-diamond and partial-diamond `brif` CFGs → straight-line `select`/`bitselect`. Arm blocks may carry cloneable pure insts. An `edge_ok` check rejects conversions whose new edge would bypass a def used in the merge subtree (the `objc2-encode` verifier bug: a collapsed block-param alias left hanging when a same-target `brif` became a `jump` past its defining arm) |
| SLP | `src/slp.rs` | adjacent scalar stores `base + i*esz` (8/16-byte groups) with isomorphic lane trees → one vector tree + `str q`. Packs adjacent loads, splats, equal consts, isomorphic ops; refuses mixed opcodes and any writer in the span. 22 hits in the bevy dep tree (mostly `Default` struct zeroing) |
| spec | `src/spec.rs` | IPSCCP-lite: a `Local` fn whose address never escapes and whose every callsite passes the same `ConstVal` for a param adopts it — param uses are replaced by a fresh orphan `UndefOp` const, so downstream constprop does the rest |
| bcheck | `src/bcheck.rs` | Loop-versioning bounds-check elimination (LLVM `LoopPredication`): for a `+1` counting loop, compute `m*last_i + b < len` once in a merged guard block `g`, clone the loop with all panic edges already taken, keep the original as the exact-panic slow path. Handles latch-tested *and* header-tested (while-form) exits, affine index terms `m*i + Σ b`, widened (i128) guard math, multi-entry loops (all entry edges retarget to `g`, whose params are the canonical entry values), and pulls cold trap blocks + private exits into the clone region (they're not `LoopAnalysis` members but use body values freely) |
| indvars `Mul::V` | `src/indvars.rs` | `iv * invariant` (and `base + iv * invariant`) → stepped param (`b_idx += n` in matmul's `b[k*n+j]`). `Mul::K(1)` deliberately skipped: `base + iv` is already one cheap add, and a stepped param buys only regalloc parallel-copy movs |
| minmax | `src/clifpeep.rs` | `select|bitselect (icmp cc x, y)` → `{u,s}{min,max}`, incl. the nested clamp idiom `x < lo ? lo : min(x, hi)` (needs a `lo <= hi` constant proof the vendored egraph can't do). Running before `loopvec` makes `u8::clamp`-style loops emit `umin`/`umax` lanes — `clamp_u8_4M` 0.063 → 0.055 ms/iter (stock 0.056) |
| indvars post-vec | `src/lower.rs` | a second `indvars` run right after `loopvec` strength-reduces the fresh `base + iv*esize` addressing into stepped pointer params (`PLIRON_INDUCT2`); axpy_f32_4M reaches stock parity |

Note: an `iconcat(hi, lo)` operand-order bug in `bcheck`'s widened constants
made every guard fail, silently pinning execution to the slow path — caught
by a gather/hist microbenchmark (0.76/0.73 → 0.47/0.28 ms/iter after the
fix, beating stock's 0.60/0.41).

aarch64 lowering additions in vendored cranelift (`vendor/cranelift-codegen/PLIRON_PATCH.md`):
`udot`/`usdot`/`smlal`/`umlal`/`mls` instructions + ISLE folds for the
`iadd_pairwise`/`iadd(imul(swiden…))` trees `loopvec`'s widening dot products
emit, scalar `smaddl`/`umaddl` from `iadd(c, [su]extend(a) * [su]extend(b))`,
and `mla` from `iadd(acc, imul a, b)`.

`target_config` now uses `internal_target_features` so `-Ctarget-cpu` /
`-Ctarget-feature` are parsed, validated, and expanded (spec features + a
cpu table + native detection); `build_isa` maps them onto Cranelift ISA flags
(`dotprod`→`has_dotprod`, `i8mm`→`has_i8mm`, `lse`, `fp16`, `bti`, x86
sse/avx/fma/bmi/lzcnt/popc/lse-equiv).

The upstream Cranelift **egraph pass already runs inside `compile`** at
`opt_level != none` (after our passes, before machinst lowering): GVN +
ISLE cprop/remat + redundant-load elimination. `PLIRON_EGRAPH=1` adds an
extra run before `switchmap`/`loopdel`/`looprot` (aliases resolved first —
the egraph indexes `Value`s directly and doesn't follow them; `try_call`
fns are skipped since upstream egraph predates EH). No measured gain so
far — opt-in.

CPU kernels vs stock `-O -Ctarget-cpu=apple-m1` (`/tmp/cpubench`, M3 Max):
axpy 0.313/0.315, vadd 0.465/0.462, clamp8 0.063/0.075 (faster),
dot_i32 0.075/0.075, sum_u8 **0.105/0.196 (1.9× faster — `uaddlp` chain
beats LLVM's `tbl` widening)**, dot_i8 0.075/0.074 (`sdot` ×4 unrolled,
same as LLVM), matmul 11.9/11.2 (residual: LLVM propagates `n=256` *and*
`len=65536` into the internal fn; `black_box` keeps `len` opaque to us —
with runtime `n` it's ~1%). The bevy release build compiles clean with
everything on and the game runs on Metal.

`PLIRON_JT_CHECK=1` verifies CLIF after each `jumpthread` fixpoint step —
that's how the ifconv dominance bug was isolated.

Also fixed this session: C-variadic calls on `aarch64-apple-darwin` now
support `f32`/`f64` varargs (fill the v0-v7 bank with dummy `F64`s so real
varargs land on the stack where Darwin wants them — previously ICE'd), and
sub-64-bit varargs are promoted to their 8-byte slot form (`uextend` to
i64, `fpromote` f32→f64) since Darwin stack varargs stride at 8 bytes —
an `i32` in a 4-byte slot shifted every later `va_arg` read.
