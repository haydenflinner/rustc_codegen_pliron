# LLVM parity: status and work list

The goal is parity with stock rustc + LLVM, native and wasm, on the speed
of the generated code, compile time, compile memory, code size and stack
use, without giving up correctness. This page has the parity card
definition, the current scoreboard, the known gaps by axis, the ranked
work list, and how to measure. Session logs and older measurements are in
[`history/`](history/README.md).

Update the scoreboard when a number moves. Put the narrative of how it
moved in a dated `history/` file, not here.

## The parity card

`harness/parity.py` opens its summary (`target/parity/summary.md`) with
one rating per category and an overall, FIFA-style. Each category reduces
to a ratio r = pliron / LLVM (lower is better) and rates
**100 + 50·log2(1/r)**, clamped to 0–150: 100 is parity, 150 twice as
good, 50 half as good. The overall is the weighted mean of the ratings of
the categories that have data. Inside the clamp that is a weighted
geomean of the ratios, so 2x better on one axis offsets 2x worse on
another of equal weight.

| code | category | ratio from | weight |
|---|---|---|---|
| SPD | runtime speed | geomean of the native (`bench/run.sh`) and wasm (`bench/wasm/run.sh`) kernel geomeans | 30 |
| CMP | compile time | `harness/ctime.py` wall-time geomean | 15 |
| MEM | compile memory | `ctime.py` peak-RSS geomean | 10 |
| SIZ | code size | `ctime.py` machine-code bytes (text / wasm `CODE` sections), plus the wasm bench module | 10 |
| STK | stack frames | `harness/frames.py`: frame bytes summed over the functions both backends define in regex-syntax (one CGU) | 15 |
| COR | correctness | geomean over suites of (pliron failures + 1) / (LLVM failures + 1): UI stage-2-only vs stage-1-only (capped at parity), `harness/crates.py` pliron-only failures vs 0 (only runs of 10+ crates count), fuzz seeds whose status is not `pass` (`env-*` included) vs 0 (only runs of 100+ seeds count) | 20 |

The weights are `CARD` in `harness/parity.py`. Correctness is an
attribute, not a gate: it can be traded for a while, at a visible price
(one pliron-only crate failure rates COR 50). `parity.py` does not rerun
the slow correctness suites (`selfhost/verify.sh`, `harness/crates.py`,
the fuzz suite); it reads their saved results and shows each one's age.

The CubeCL CPU-kernel path has its own card (`harness/cubecl_card.py`,
[`cubecl-burn.md`](cubecl-burn.md#the-card)).

## Scoreboard

<!-- SCOREBOARD:BEGIN -->
**Last measured 2026-11-07** (STK re-measured 2026-11-11 after the
`memcpyopt` memset/sret-temp sink below), aarch64-apple-darwin, backend
`862ce3a`, nightly-2026-10-22. `PLIRON_COPYPAIR` is default-on since
`862ce3a`
(held-out A/B 0.982 [0.971, 0.998] with `AMODESHARE`); this card is the
first under that default — the 2026-10-30 card's CMP/SIZ cells came from
a stale mix of flag states.

**Overall 94** (2026-10-30: 95).

| SPD | CMP | MEM | SIZ | STK | COR |
|---|---|---|---|---|---|
| **102** | **101** | **70** | **61** | **98** | **100** |

| category | weight | pliron/LLVM | rating | from |
|---|---|---|---|---|
| SPD runtime speed | 30 | 0.979 | 102 | native 0.980 · wasm 0.977 |
| CMP compile time | 15 | 0.993 | 101 | 5 inputs |
| MEM compile memory | 10 | 1.524 | 70 | peak RSS, 5 inputs |
| SIZ code size | 10 | 1.710 | 61 | machine code, 5 inputs + wasm module 1.84 |
| STK stack frames | 15 | 1.047 | 96 | regex-syntax, 336 shared fns |
| COR correctness | 20 | 1.000 | 100 | ui: 0 stage-2-only vs 3 stage-1-only (11 d old) |

Correctness inputs: crates tier 1 run in the default `--deps stock` mode
(the crates' own code only); the UI input is the 2026-10-27 stage-2
self-host run. x86-64 was last measured 2026-10-22. The CMP rating drop
(112 → 101) is mostly the corrected measurement mix, not a slowdown —
the same 0.85–0.99 ratio band, weighted differently across the five
inputs; STK drifted up to 1.207, then dropped to ~1.03 once `memcpyopt`
learned to sink a memset/sret-written temp into the alloca a `memcpy`
copies it to (was: two 4 KiB `driftsort` scratch slots + a 4 KiB copy).
The generalisation to multi-writer temps, `memmove` and
struct-field/`sret` destinations landed with the quiescence rule for
post-copy temp reads (fuzz 24242). `memcpyopt` now also resolves copy
destinations through block parameters — every incoming edge must agree
on (alloca, offset) — and follows destination pointers into block
params when scanning the writer→copy and copy→read windows
(`1ed006c`; regex-syntax forwards 1257→1398). Fresh measurement
2026-11-11 puts STK at 1.047; the residual is `visit_post`
register-pressure spills, not retained slots — `slotmerge` already
packs 125 explicit slots down to 288 B and the rest of the ~1.5 KB
frame is spill traffic, which tracks the function's inlining shape
(204 calls vs LLVM's 142 — see `PLIRON_INLINE_GLUEWARM` in the work
list and the `eh callee at invoke` stats).

**Native kernels** (benchfmt + benchstat, pliron vs LLVM measured minutes earlier):

| kernel | LLVM | pliron | pliron/LLVM |
|---|---|---|---|
| arrayvec | 4.011 ms | 3.744 ms | 0.934 ◄ faster |
| chars_alpha | 2.082 ms | 2.098 ms | 1.008 |
| chars_ascii | 1.567 ms | 1.658 ms | 1.058 |
| chars_mixed | 11.150 ms | 11.640 ms | 1.044 ▲ slower |
| contains_rng | 1.570 ms | 1.054 ms | 0.671 ◄ faster |
| eqbug | 0.080 ms | 0.095 ms | 1.194 ▲ slower |
| fib | 10.265 ms | 9.941 ms | 0.968 ◄ faster |
| hashmap | 39.799 ms | 41.528 ms | 1.043 ▲ slower |
| hmget | 8.412 ms | 8.164 ms | 0.971 |
| iterw | 14.293 ms | 9.860 ms | 0.690 ◄ faster |
| matmul | 6.258 ms | 6.072 ms | 0.970 |
| nbody | 83.731 ms | 64.450 ms | 0.770 ◄ faster |
| parse_only | 271.165 ms | 239.094 ms | 0.882 ◄ faster |
| sieve | 8.719 ms | 8.726 ms | 1.001 |
| sortw | 4.982 ms | 4.464 ms | 0.896 ◄ faster |
| strwork | 10.530 ms | 10.279 ms | 0.976 |
| tokenize | 4.270 ms | 5.379 ms | 1.260 ▲ slower |
| varint | 5.724 ms | 4.350 ms | 0.760 ◄ faster |
| varint_postcard | 5.638 ms | 3.564 ms | 0.632 ◄ faster |

`chars_ascii` is back near parity after `esplit` (the merge tail is
cloned per-edge and the ASCII arm's `icmp ult c, 128` folds to the
bump, 7 insns/char) plus the `brhint` masked-test fix (don't mark the
zero arm of `band`/`bor`/`bxor` tests unlikely — UTF-8's
`c & 0x80 == 0` is the *hot* ASCII case). `chars_alpha` closed at
1.008 (2.098 vs 2.082 ms): `brhint` marks the single-bit `band`'s
bit-set arm unlikely so ASCII falls through (`tbnz`-away like LLVM),
`bump_hoist`+post-index emit `ldrb [x], #1`, and `PLIRON_RETNN`+
`PLIRON_NNBB` (now default-on) fold the `Option<NonNull>` niche `cbz`
through `black_box`. See
[`history/2026-11-09-hqzopfli-insn-bloat.md`](history/2026-11-09-hqzopfli-insn-bloat.md).

**wasm kernels** (node, interleaved rounds):

| kernel | LLVM | pliron | pliron/LLVM |
|---|---|---|---|
| chars_alpha | 3.269 ms | 2.851 ms | 0.878 ◄ faster |
| chars_ascii | 1.652 ms | 1.662 ms | 1.004 |
| chars_mixed | 13.052 ms | 13.927 ms | 1.067 ▲ slower |
| contains_rng | 3.513 ms | 3.675 ms | 1.051 ▲ slower |
| eqbug | 0.097 ms | 0.093 ms | 0.966 ◄ faster |
| fib | 6.721 ms | 6.389 ms | 0.955 ◄ faster |
| hashmap | 47.192 ms | 48.665 ms | 1.029 |
| hmget | 8.192 ms | 8.288 ms | 1.012 |
| iterw | 47.669 ms | 66.542 ms | 1.402 ▲ slower |
| matmul | 21.865 ms | 10.810 ms | 0.493 ◄ faster |
| nbody | 59.478 ms | 59.240 ms | 0.999 |
| parse_only | 275.676 ms | 376.178 ms | 1.374 ▲ slower |
| sieve | 10.166 ms | 9.206 ms | 0.906 ◄ faster |
| sortw | 4.462 ms | 4.148 ms | 0.926 ◄ faster |
| strwork | 12.776 ms | 14.145 ms | 1.107 ▲ slower |
| tokenize | 5.512 ms | 4.533 ms | 0.824 ◄ faster |
<!-- SCOREBOARD:END -->

### Held-out runtime (not in the card)

SPD is measured on the kernels above, which the backend has been tuned
against for weeks. On real programs it was never tuned on, native code is
**1.076x slower** than LLVM (geomean of 8; 2026-11-11, 5 interleaved
rounds, `bench/heldout/run.py`; was 1.054x on 11-10 — within
day-to-day machine drift):

| program (sightglass) | LLVM | pliron | pliron/LLVM |
|---|---|---|---|
| blake3-scalar | 2.90 ms | 3.07 ms | 1.057 |
| blind-sig | 1.35 ms | 1.40 ms | 1.043 |
| pulldown-cmark | 3.60 ms | 4.12 ms | 1.147 |
| rust-compression | 2.22 ms | 2.50 ms | 1.122 |
| rust-html-rewriter | 4.88 ms | 5.54 ms | 1.134 |
| regex | 2.68 ms | 2.90 ms | 1.082 |
| rust-json | 3.02 ms | 3.18 ms | 1.053 |
| rust-protobuf | 5.26 ms | 5.17 ms | 0.983 |

Since then `PLIRON_BJALIAS` (default on, afcc31b) removed ~600 dead
`b .+4` fallthrough branches per binary — eager aliasing of bare-jump
edge-block labels so MachBuffer's purgatory sees the real target;
held-out A/B `bja/pliron` geomean 0.985
(`history/2026-11-09-bjalias-dead-branches.md`). A `PLIRON_BFUSE`
`imms`-encoding miscompile (`band (ishl x,k), m` emitted `ubfx`
where `ubfiz` was meant — fuzz seed 6279) is fixed in 02baa35;
narrowext bit 32 (`ushr`-of-clean + narrow logical `lsr` fusion) is
default-on in c1fc7d8. `PLIRON_CCMP` aarch64 compare-chain fusion is
default-on (4144dde). `PLIRON_LOOPDEL` post-rotation dead-loop
deletion is default-on (64ca6c2 + d205f29): `st(f(iv))` countdown
loops fold to one closed-form store (`montgomery` 858→816 insns);
held-out A/B `loopdel/pliron` geomean 0.994 [0.977,1.015],
html-rewriter 0.978* — `history/2026-11-11-loopdel-countdown.md`.
`loopvec` now classes mask/data `band`/`bor`/`bxor` as lane-exact, so
`to_ascii_lowercase`'s `b | ((upper&1) << 5)` collect loops vectorize
(NEON `sub`/`cmhi`/`and`/`shl`/`orr.16b`); held-out A/B geomean
1.010 [0.993,1.042] toward faster —
`history/2026-11-11-loopvec-mixed-band.md`.
`PLIRON_INLINE_WARMCALLS=3` is default-on (b1b9413 + dc32cd2): a
warm-call-sparse call-carrier earns the loop bonus when its warm body
alone fits the base limit and its total growth passes the `fanin`
bound. That inlines brotli's `EvaluateNode` (18 calls, 17 of them cold
`panic_bounds_check` scaffolding, 3 callers — LLVM inlines it, we kept
it out-of-line at ~6% self-time) while regex-syntax's `bump`/`char`/
`peek` stay out. Held-out A/B `w3/pliron`: rust-compression 0.944
[0.935,0.958], geomean 0.998 [0.974,1.008]; binary sizes neutral
except the `regex` crate +0.8%.
`PLIRON_LOADFWD_SRCCOPY` is default-on (2d2b8a2): a load covered by
`store (load s)` re-reads `s` (LLVM's memcpy-forwarding) — brotli's
`sub_match!` buffers collapse to direct slice loads once `bunroll`
exposes the copy (`PLIRON_BUNROLL_BODY`, default 48, stays a knob).
Held-out A/B `sc/pliron`: geomean 1.001 neutral, rust-compression
0.984 — `history/2026-11-11-srccopy-memcap-narrowext-ret.md` (which
also covers the `PLIRON_CGU_MEMCAP` memory throttle and the
narrowext ret-arg ireduce sink).
`nocap` call operands now carry byte extents and `slotprom` syncs
only the covered fields (bc8e581; `PLIRON_SLOTPROM_EXT=0` reverts).
The two extra mechanisms it enabled — sret out-pointer boundedness
(`PLIRON_SRETNC`) and `MAX_FIELDS` partial eviction
(`PLIRON_SLOTPROM_EVICT`) — measured +3.1% `.text` on regex-syntax
for no runtime gain and are opt-in
(`history/2026-11-11-nocap-extents-sret-evict.md`).

Two more `narrowext` users are default-on: bit 512 skips the call-arg
`uxtb`/`uxth` when the source register is already narrow-clean
(353ec61; −172 extends on regex-syntax, −25 on lol_html — most
residual `uxtb`s were compare materialisation, not ABI work), and bit
1024 folds a narrow `icmp` of `iadd`/`isub`-by-constant against a
constant (f24bc1a): eq/ne rolls the constant (`(c∓j) mod W == k` is
`c == (k±j) mod W`, one `cmp` on `c`, the arithmetic goes dead) and
the unsigned relations compare the unmasked 32-bit `sub c, j`/`sub c,
W-j` when the wrapped and unwrapped orderings provably agree
(`j + k' ≤ W` / `k' ≤ j`); a dirty operand is still widened first, so
no cleanliness proof is needed. −886 `uxtb`/`uxth` and −1,510
instructions on rust-html-rewriter's benchmark binary (`__text`
−6,144 B); residual `uxtb→cmp` sites 1,462 → 632. The narrow-clean
oracle gained an `ishl` arm (`bool << k` — lowercase-style idioms —
is clean when the source's bits at/above `width-k` are known zero) and
the narrow-`icmp`-vs-imm path now consults it (c13494c; −297 more
extends on the same binary). Bit 1024 also folds the `br_table`
bounds check on a narrow `(c−s) mod W` index — `uextend (iadd c, -s)`
becomes `sub c, s; cmp c-s, N` when `s + N ≤ W` since wrap-around
indices land ≥N either way (84c22f2; 26 masked-bound sites folded on
the benchmark CGUs). Bit 8's `is_nonzero` rules 5/6 turn a single-use
narrow `ireduce`/`band (ireduce y), k` brif into `tst` on the
unmasked source (383eba0; −118 extends on lol_html, `cbz`/`cbnz` −158
↔ `tst`+cond-br +158, net ≈ −67 insns). Bit 2048 then extends the
cleanliness oracle itself (a70194d): blockparams whose every incoming
blockcall arg is clean — a per-function greatest-fixpoint sweep,
coinductively sound so loop-carried `phi(clean, band(y, k))` phis
qualify — and call results whose signature return declares `uext`
(−315 extends, −2,112 B `__text` on rust-html-rewriter's benchmark;
885 standalone `uxtb`/`uxth` remain vs LLVM's 0 — dirty-producer
extends LLVM's demanded-bits avoids, a structural gap).

`rust-compression` got its own deep-dive
(`history/2026-11-08-amode-crc-brotli.md`,
`history/2026-11-09-copypair-scalar-merge.md`): `compress_inner` is at
parity after aarch64 scaled-amode fixes and Apple `crc` feature tiers;
the brotli hot function's 32-byte swap now emits `ldp q`/`stp q` under
the opt-in `PLIRON_COPYPAIR`+`PLIRON_AMODESHARE`, and its self-time is
near parity. The walk-loop `[sp]` fill traffic was pinned chains, not
pressure: `PLIRON_RA2_NOPINMERGE` (now default on) stops a try_call-ret
`FixedReg` from spreading through blockparam merges — 527→454 stack
accesses in `BrotliCreateHqZopfliBackwardReferences`
(`history/2026-11-09-hqzopfli-insn-bloat.md`). Treat the kernel geomean as "parity on code shapes we have
worked on", not as generated-code speed in general. `blind-sig`'s 2.148 was a licm
miscompile livelock (`history/2026-11-06-licm-frozen-clobber.md`), and
most of `blake3-scalar`'s 3.149 fell to the intervening optimisation
work — what remains is an honest ~10% spread.
[`history/2026-10-31-heldout.md`](history/2026-10-31-heldout.md).

## Known gaps by axis

Each entry names the mechanism and where to look. Numbers for these live
in the scoreboard and in the linked history files.

### Runtime speed

- **Held-out programs: 1.08x** (was 1.57x on 2026-10-31). The livelock
  behind blind-sig's 2.1x is fixed
  (`history/2026-11-06-licm-frozen-clobber.md`); the remaining spread is
  0.99-1.14x across compressors and parsers — rust-protobuf is the first
  program faster than LLVM. pulldown's gap decomposes in
  `history/2026-11-09-mcp-sibling-pulldown.md`: the top mechanism is
  early-exit search-loop vectorisation (`iter().any`/`memchr` shapes →
  `cmeq`+reduce, which `loopvec` doesn't cover) plus per-copy end-test
  cost in peeled scans.
- **wasm local traffic.** CLIF SSA values become wasm locals; waffle's
  relooper passes block params through scratch locals at `if`/`else`
  boundaries, and we emit structured `if` diamonds where LLVM emits
  `block` + `br_if` and keeps values on the operand stack. TurboFan
  removes much of it, but not all (`hashmap`, `hmget`, `tokenize`,
  `chars_*`). Code: `src/wasm.rs` (`clif_emit`, `wpeep`).
  History: `history/2026-10-21-wasm-runtime.md`,
  `history/2026-10-20-wasm-open.md`.
- **`strwork` on wasm.** Native is at parity; wasm loses in
  `format_exact` and the bignum helpers: more surviving bounds checks
  than LLVM (`bignum.rs` `i < 40` checks) and more inlined `push_str` /
  `extend` / fmt wrappers that LLVM keeps as calls (inline policy in
  `pliron-ir-opt/src/inline.rs`).
- **`parse_only` on wasm.** TurboFan code matches LLVM per instruction;
  the cold `Err` arm carries block-arg moves, so V8 places an edge block
  inline on the hot path. Levers: no moves on cold edges, or wasm branch
  hints.
- **`varint_postcard` native.** Fixed by `bunroll`'s LCSSA
  exit-cone repair (`95cab3a`, default-on `3f6a2ff`): the
  `0..varint_max` decode loops now fully unroll to the
  ldrb + const-shift `orr` + `tbz` shape LLVM emits (the `done`
  exit's `i == max` check used to fail the free-var cone check).
  Same-state A/B −16%.
  History: `history/2026-11-05-bunroll-lcssa-varint.md`.
- **`eqbug` native.** Root-caused: the kernel is ALU-port-bound
  (~5 int ops/cycle) and our loop executes ~6 more ALU ops/iter than
  LLVM's. The difference is `black_box` materialisation: LLVM lowers it
  to a `strb`/`ldrb` stack round-trip, so every narrow operand arrives
  pre-truncated — zero standalone `uxtb`/`uxth` in the loop (the wrap
  semantics then ride the compares' flag-extension for free). Ours is a
  zero-cost op on native, so each of the six narrow values pays one
  ALU-port `uxtb`/`uxth`. Loop shape is irrelevant: a `while` rewrite
  producing the ideal single-block form (`add x,x,#1` in place, one
  `b.lt`) timed *slower* than the `for` version. Fixing this means
  forcing `black_box` through byte memory on native too — a deliberate
  divergence (the wasm path already does it), not a bug.
  History: `history/2026-10-27-slow-kernels.md`.
- **Register pressure from loop-invariant hoists.** `PLIRON_ELABLOOPS=1`
  (vendored Cranelift egraph elaboration) hoists correctly and helps
  `parse_only`, but costs `strwork` spills. It stays opt-in until the
  hoist is pressure-aware.
- **`nonnull` lost through stack slots.** Iterator pointers spilled and
  reloaded across calls lose `!nonnull`, which blocks `Option`-niche
  `eqz` folds after the reload (`tokenize`'s `Split`). Needs nonnull
  facts carried through our own slot demotion.
- **Layout noise.** Identical code at a different address swings tight
  kernels (`sieve`, `nbody`) by up to ±10% native and ±25% under V8.
  Check deltas with `bench/layout.sh` and `run.mjs --layouts=N` before
  acting on them.
- **x86-64** has not been measured since 2026-10-22 (Windows i7,
  `history/2026-10-22-parity-scoreboard.md`). Re-measure before working
  on x64-specific gaps.
- **CubeCL kernels**: f16 GEMM is the largest gap (per-leaf widen and
  narrow where LLVM keeps packed ops); x86-64 not run.
  [`cubecl-burn.md`](cubecl-burn.md#what-is-next).

### Compile time

- **Per-instruction pass cost on giant functions.** Functions over ~5k
  instructions carry most of the CLIF pipeline time, and they are hot, so
  a size cap costs runtime. The top steps (jumpthread, ifconv2, slp,
  loadfwd, the mergechain/webmerge family, dse) are already linear; the
  remaining cost is per-access weight (`RefCell` arena access, analysis
  rebuilds across mutation boundaries). LLVM also starts from MIR that
  rustc already cleaned; we redo some of that work at CLIF.
- **IR-opt**: about a quarter of samples (inliner cloning, sroa,
  nocapture). Live `xcgu` copies rerun the pliron-IR passes in every CGU
  that holds them; the CLIF side shares results through the in-process
  pipeline cache (`PLIRON_MEMCACHE`, default on), the pliron-IR side does
  not. Skipping copy optimization was measured worse: the result must be
  shared, not skipped.
- **Serial parts.** The front half (MIR → pliron `define`s) runs on the
  main thread. A CGU's stage B waits until the CGUs that own its bodiless
  declarations have published facts, so the biggest CGU's IR-opt gates
  the wall on regex-syntax; reordering CGUs changes output, so this is
  not reachable byte-identically (`src/cgupool.rs`, `docs/determinism.md`).
- **wasm** adds CLIF → waffle translation, the waffle inliner and `wpeep`
  on top of the same pipeline. The small `kernels-wasm` input is
  dominated by fixed rustc latency.
- **Incremental round trips** (edit a leaf binary over a 281-crate
  egui/wgpu/winit tree, dev profile): at parity on both targets —
  thin leaf ~0.51 s native and ~0.65 s wasm, large leaf ~0.88 s, cold
  tree ~23 s, all ≈ stock. rustc-side the thin rebuild is ~36% faster
  than stock (incremental CGU reuse skips LLVM's re-emission); the
  end-to-end floor is the system link (~0.2 s, Apple ld beats ld64.lld
  2:1 here) plus cargo bookkeeping (~0.2 s). `tools/pliron-wasm-ld`
  went 1.01 → 0.40 s on the same link (FxHash, O(n²) dedup loops,
  fused type-scan) which moved the wasm round trip from 2.1× slower
  to parity.
- `tools/pliron-inc-ld` (drop-in `-Clinker`, splices only changed
  leaf objects into the previous image) now lifts the link floor on
  native: thin leaf 0.409 s stock → **0.238 s** (0.58×), large leaf
  0.670 s → 0.627 s, both ASLR-safe via rebase-blob extension and a
  writable `__DATA` pad. `history/2026-11-06-inc-ld.md`.
- History: `history/2026-10-20-compile-time-worklist.md`,
  `history/2026-10-27-ctime.md`,
  `history/2026-11-06-round-trip-parity.md`,
  `history/2026-11-06-inc-ld.md`.

### Compile memory

- The default-on memory set (`PLIRON_LOWERFREE`, `PLIRON_JEARENA`,
  `PLIRON_CGU_ACAP=8`, `PLIRON_CGU_INFLIGHT=4`, `PLIRON_DEADFREE`,
  `PLIRON_LTUNDEF`; see `pliron-cfg/src/lib.rs`
  `DEFAULT_ON`/`DEFAULT_VALUES`) closed most of the first gap.
  `CGU_INFLIGHT` (default-on since 2026-11-06) bounds the
  submitted-and-unfinished set at 4 CGUs — the main thread parks in
  `submit` instead of queueing more unoptimized `Context`s ahead of the
  workers — and cut syn's peak 1.07→0.75 GB (1.70→1.22× LLVM) and
  regex-syntax's 0.55→0.47 GB with no measured wall cost.
  Remaining levers: the wasm phase-(A) hump (the whole
  CGU's optimized CLIF is held until waffle translation; translating each
  function inside its pipeline job would release it earlier), and fewer
  ops after inlining. History: `history/2026-10-26-memory.md`,
  `history/2026-10-27-memory2.md`, `history/2026-10-27-memory3.md`,
  `history/2026-11-06-inflight-cap.md`.
  The remaining peak is live data, not allocator retention: CGU
  `Context`s parked at stage B hold their post-inline form (~2× ops,
  ~55 MB each on syn; `history/2026-11-06-siz-mem-attribution.md`).

### Code size

- **Native.** Not per-instruction quality: call-structure-matched
  functions are no larger than LLVM's. Most of the gap is speed-for-size
  transforms with no size or cold gate: bounded partial unrolling and
  peeling (`punroll.rs`), jump threading, loop rotation, inlining volume,
  and the inline memcpy/memmove fast path (`MEMFAST`,
  `lower.rs::small_copy_or_call`). Then aggregates scalarized and moved
  slice by slice where LLVM keeps a memcpy (`sroa.rs`, `slotprom.rs`),
  `uxtb` after bool-returning calls, and loop-header alignment `nop`s
  (`PLIRON_ALIGN`). Kill-switch attribution:
  `history/2026-10-29-ir-excess.md`, `history/2026-10-29-code-size.md`.
  Inlining volume update 2026-11-10 (`d21d9da`): the inliner's
  loop-callsite bonus (`+limit*8/4` threshold) gated on warm calls
  only, so helpers carrying unexecuted panic paths got a 3x budget —
  regex-syntax's `while !is_eof()` loop inlined `bump` (9 calls) and
  `char` (6 calls) where LLVM keeps them as calls (LLVM's ~5/insn
  cost puts their ~60-insn bodies over its 225 threshold). Gating on
  all calls (`PLIRON_INLINE_WARMCALLS=0`, now default) splits the
  motivating leaf (`hash_one`, 1 call) from call-carrying helpers:
  regex-syntax .text −5.3%, matched-symbol excess vs LLVM −9.4%
  (parse fns at/below LLVM size), parse_only −9.3%, hash kernels
  flat. `PLIRON_INLINE=200` shaves another −6% but regressed
  Hmget +18%/Hashmap +3.3% — rejected; the residual is
  per-function codegen, not one transform. Drop-glue update
  2026-11-10 (`bfc18cc`+`2337769`): glue bodies lower ~1 op:1
  insn, so the ~150-op `drop_glue<Vec<T>>`/`Box<T>` bodies slip
  under the 320-op limit (+alloca-arg save) where LLVM keeps
  them as calls — each clone drags its own `dealloc` /
  `panic_in_cleanup` / `Unwind_Resume` copies (~63 K ops across
  406 sites in regex-syntax alone). `PLIRON_INLINE_DROPLIM=64`
  (default) caps drop-glue call-site thresholds: .text
  356,016→346,064 (−2.8%), `pop_class_op` 752→600 insns (LLVM
  234 — its remaining excess is `Box::new`/`push_mut` inlines
  LLVM also makes plus extra MIR drop sites), regex wasm
  objects −1.0%, kernels flat. Byte-copy update 2026-11-10
  (`0a00710`+`b984514`): scalarized 2-byte aggregate copies
  (the `{u8,u8}` swaps in `drift::sort`/`quicksort`) emitted
  `ldrb`+`ldrb`+`strb`+`strb` where LLVM emits `ldrh`/`strh`.
  `stmerge` (default) folds adjacent `load.i8`/`store.i8` copy
  pairs into `load.i16`/`store.i16`: `drift::sort<
  ClassBytesRange>` 963→867 insns (LLVM 369 — its residual is
  the usual cmp/cset/add bookkeeping, not copies), rlib
  −200 insns, wasm −573 B, Sortw −11%. Narrow-int update
  2026-11-10 (`2e23071`): `ireduce.i8/i16` lowering materialised
  `uxtb`/`uxth` unconditionally (1,633 `uxtb` in the regex-syntax
  rlib vs LLVM's 9). `PLIRON_NARROWEXT` bits 64/128/256 add a
  `wide_uext_clean` source oracle (skip the extension when the
  wide register is already zero above the destination width) and
  targeted source/consumer fusions (`ushr`+`ireduce`→`ubfm`,
  `ireduce` of a single-use load→narrow `uload`, narrow stores of
  `ireduce` store the wide reg, `ushr (ireduce x),k`→`ubfx`,
  chained `ireduce`s extend once, `select` arms read the wide
  source): regex-syntax .text −768 B, `uxtb` 1,633→1,381. A
  dirty-source passthrough variant converted one shared `uxtb`
  into per-use extensions/tests and grew .text — reverted; the
  residual `uxtb`s are multi-use producers where materialising
  once is already optimal. Tail-trap update 2026-11-10
  (`da593ef`+`b6adf36`): functions ended in `bl noret_callee;
  udf` where LLVM ends at the `bl` (the `NORETURN_CALL` trap
  elision already dropped mid-function copies; the last block's
  was kept so a return address never pointed past the function —
  unwind lookups use `ret_addr - 1`, which still lands on the
  `bl`). `PLIRON_NORETUDF` (default) drops it too: regex-syntax
  .text −1.1 KB, `udf`s 523→200.
- **xcgu copies.** On kernels, ~14 KB of the rlib gap is per-CGU IPO
  clones (genuinely specialised, not dedup-able at the link) and ~5.5 KB
  is byte-identical `non-external` copies ld64 keeps in full. A linked
  binary shrinks the gap roughly in half (kernels: 14 KB / 5.5%
  `__text`), so part of the rlib metric is dead-strip noise. The
  `PLIRON_ALIGN` pads are concentrated on small loops where they earn;
  `PLIRON_ALIGN_MAXB=<bytes>` (skip pads on loops whose body exceeds the
  cap) recovered only ~0.2-0.5% and stays opt-in. Full attribution:
  `history/2026-11-06-siz-mem-attribution.md`.
  **Dedup (default-on since 2026-11-06):** `PLIRON_XCGU_HOME=mono`
  elects one owner partition per shared `inlined` mono item (rustc's
  "CGU-private copies" — drop glue and shared generics, ~205 symbols
  / 430 KB of `syn` text defined in 6–13 objects each) as well as
  per `extra` copy, `PLIRON_XCGU_PROMOTE=copyable` promotes the
  owner's `Internal` def hidden-external, and `home_copies` turns
  every surviving duplicate into an import of the one body — zero
  multi-defined symbols on `syn`. Card deltas when flipped on:
  syn SIZ 1.753→**1.202** (also CMP 1.31→1.13, MEM 1.84→1.69 —
  converted bodies skip lowering entirely), regex-syntax SIZ
  1.897→1.811, kernels 2.065→1.975, wasm unchanged. Bench runtime
  neutral (−0.04% geomean); crates.py tier 2 clean (31 crates).
  `PLIRON_XCGU_MONOMIN` (default 200 MIR stmts) keeps ordinary
  small items per-CGU — bodies that inline away everywhere must not
  force an owner def `dead_fns` can't strip; drop glue and other
  shims always elect since they survive inlining regardless.
  `=1`/`=all`/`=0` keep the older copy modes.
  `history/2026-11-06-xcgu-dedup.md`.
- **wasm.** Op density, not dead code: `local.get`/`local.set`, `br` and
  `if`/`else` counts are several times LLVM's (see the runtime entry on
  local traffic). The default-on wasm size set (`PLIRON_WASM_NOIRRED`,
  `PLIRON_WASM_NOREC`, `PLIRON_WASM_GC`, `PLIRON_REDUCIBLE`,
  `PLIRON_WASM_HOTLEAF`, `PLIRON_INLINE_SMALLSCC`, `PLIRON_WASM_SOLO`,
  `PLIRON_WASM_TINY`, `PLIRON_WASM_ELIDEBLK`, `PLIRON_WIFRET`) removed
  the inlining blow-up. `tools/pliron-wasm-ld` folds GOT `global.get`s
  to `i32.const` where the immediate is no larger than the global index.
  History: `history/2026-10-27-wasm-size.md`.

### Stack frames

- Copy-only aggregates (regex-syntax `pop_group`/`pop_class`, 160–224 B
  states taken apart field by field) get one spill slot per scalarized
  field. Lever: memcpy-chain forwarding so a copy-only alloca can stay in
  memory. History: `history/2026-10-27-spills3.md`.
- regalloc2 reloads once per `br_table` edge instead of once at the join.
- The stage-2 self-host fails 6 deep-recursion UI tests that stock
  passes: rustc's parser and AST lowering frames are larger than LLVM's
  (README, `selfhost/verify.sh`).

### Correctness

- **windows-msvc catch.** With stock std, `catch_unwind` in pliron-built
  code cannot catch a real SEH throw; EH is emulated and works only when
  std is backend-built. Design for real SEH catch:
  [`seh-design.md`](seh-design.md) (not landed).
- **IPO on `xcgu` copies vs the wasm linker's name fold.**
  `pliron-wasm-ld` folds same-named bodies across objects and keeps the
  smallest, which is sound only if every copy keeps the generic
  semantics. `spec` now skips copies on wasm; any other callee-side IPO
  that rewrites a copy's body per CGU has the same hazard
  (`history/2026-10-30-wasm-spec-xcgu-copy-fold.md`).
- **wasm32 std miscompiles (fixed 2026-11-06).** `float_fmt` (trap)
  and `hm_u64` (missed hashbrown lookups) were one bug: wpeep's `delse`
  folded `br K` whose label K was a `loop` — a back-edge, not a forward
  exit — because `ends_clear` never checked the construct type of the
  `end`s it counted. Ancestor-opcode stack added; `run_std.mjs` is
  green (`history/2026-11-06-delse-loop-backedge.md`).
- **Corpus replay is lossy.** The corpus fact header does not carry
  every `Pipe` fact table, so a clean replay does not clear a step
  ([`miscompile-triage.md`](miscompile-triage.md#replay-fidelity-gap-found-the-hard-way)).
- **Crate coverage.** `harness/crates.py` defaults to `--deps stock`:
  only the crates' own code goes through the backend, not their
  dependencies or doc-tests. `--deps pliron` is the full run.

## Work list

Ranked by expected card gain per effort. Each item links its evidence.

1. **Runtime parity on held-out programs** (`bench/heldout/run.py`,
   1.08x on 2026-11-09): the phased plan, with the LLVM and GCC
   frameworks we lack (SCEV/LFTR, value ranges, regalloc
   rematerialization, block placement, isel idiom mining), is
   [`codegen-plan.md`](codegen-plan.md). Current top mechanisms:
   early-exit search-loop vectorisation (`.any`/`memchr` → `cmeq`,
   `history/2026-11-09-mcp-sibling-pulldown.md`), the HqZopfli ~200-mov
   residue (`history/2026-11-09-hqzopfli-insn-bloat.md`).
2. ~~Validate and flip the pending size flags~~ — done 2026-11-09:
   `EDGEMERGE=3`/`EPIMERGE=2`/`TBNZ_IREDUCE` default-on (`3bbb029`),
   `SROA_MAXSLICES=18` rejected (borderline regression),
   `LIBCALL_DIRECT` flipped on (`812b317`) after randomized-layout
   reruns showed both flagged regressions were layout noise
   (`lib/pliron` 1.005 geomean vs `aa` 1.001; blind-sig deep run 1.001).
   Full table in `history/2026-11-09-mcp-sibling-pulldown.md`.
3. **Size/cold gates on duplicating passes**: `punroll`, peeling,
   `jumpthread`, `looprot` and the `MEMFAST` fast path, gated on cold
   loops (bodies that lead to a panic) or huge functions. Bench every
   kernel.
4. **Aggregate copies as memcpy**: a principled "do not split an alloca
   whose only accesses are whole-object copies" rule in place of the
   `SROA_MAXSLICES` threshold. The forwarding side is done: `memcpyopt`
   now sinks a `memset`/`sret`-written temp into the alloca a `memcpy`
   moves it to whenever no use can observe the gap (STK 1.207→1.029 —
   the `driftsort` 4 KiB temp + copy is gone). 2026-11-11: the inliner's
   `PLIRON_INLINE_GLUEWARM` (default on, `e4830a7`) prices drop glue by
   its warm body so leaf glue reaches the doomed unwind-edge sites LLVM
   inlines it into (regex-syntax −1.8 KB .text); the remaining callsite
   divergence is bigger callees (`try_allocate_in`, `canonicalize`) that
   miss the `fanin`/score bounds.
5. **wasm local traffic**: wpeep-friendly `CondBr` → `br_if` emission,
   block-typed merges for branch arms, make multi-value `WIFRET`
   (`PLIRON_WIFRET_MULTI`) shrink code instead of growing it.
6. **wasm memory hump**: translate each function to waffle inside its
   pipeline job.
7. **Pressure-aware loop-invariant hoisting**, so `PLIRON_ELABLOOPS` can
   be default-on.
8. **`eqbug` is port-bound, not shape-bound** (see above) — the only
   remaining lever is `black_box` materialisation; `uxtb` after
   `zeroext` bool call results (trust the callee's extension on Apple
   arm64) is worth its own look elsewhere.
9. **Compile time on giant functions**: per-instruction cost of the top
   CLIF steps; sharing pliron-IR results for live `xcgu` copies. The
   existing `PLIRON_MEMCACHE` whole-pipeline cache measured 2026-11-11:
   9 hits / 806 lookups on regex-syntax with `codegen-units=8` (~1%) —
   its value is confined to crates whose generics duplicate across CGUs.
10. **Correctness debt**: real SEH catch on windows-msvc; audit
   callee-side IPO on `xcgu` copies for wasm; serialize the remaining
   fact tables in the corpus header.
11. **Re-measure x86-64** (Windows and Linux), native and CubeCL.

## How to measure

- Everything: `harness/parity.py` (~20 min; `--skip
  native,wasm,ctime,stack`, `--from-logs`). Writes
  `target/parity/summary.md`, card first. Run nothing else heavy
  meanwhile.
- Held-out runtime: `bench/heldout/run.py [--rounds N] [--only a,b]`
  (sightglass's Rust programs, native, interleaved; needs the
  `are-we-fast-yet` checkout next to this repo).
- One kernel's runtime: `bench/run.sh <name>` / `bench/wasm/run.sh
  <name>`. Placement check: `bench/layout.sh <kernel> [N]` (relinks under
  shuffled symbol orders, reports the cross-layout median) and
  `bench/wasm/run.mjs --layouts=N`. V8 attribution tools: `harness/v8/`.
- Compile time: `harness/ctime.py [--only regex-syntax]` (release
  dylib; also records peak RSS and machine-code bytes). Inside one
  compile: `PLIRON_TIMING=1`, `PLIRON_TRACE_FILE=t.txt`,
  `-Zself-profile`, `-Ztime-passes`, or `samply record --save-only
  --unstable-presymbolicate` + `harness/sprof.py`.
- CLIF pipeline alone, no rustc: `clif-check bench target/corpus/std`, or
  the live dashboard's step-time panel (`harness/live/live.py`).
- Stack: `harness/frames.py [--input regex-syntax]`.
- Code size per function: `llvm-nm --no-llvm-bc -n` + `llvm-size -A`
  over the rlib members, joined on symbol; per-pass attribution with each
  `PLIRON_<PASS>=0`.
- Noise: ±3% is parity on a quiet machine. Criterion compares against an
  LLVM baseline measured minutes earlier; wasm uses interleaved rounds.
  Per-step span times under the CGU pool read high (descheduled time);
  trust A/B `user` time for pass-level costs.

## History

- Scoreboards and card snapshots: `history/2026-10-30-parity-card.md`
  (the current scoreboard's full run), `history/2026-10-22-parity-scoreboard.md`
  (2026-10-18 to 10-22, incl. Windows i7), `history/2026-10-27-parity-card.md`
  (first full cards), `history/2026-10-28-parity-card-snapshot.md`.
- Former PARITY sections: `history/2026-10-20-compile-time-worklist.md`,
  `history/2026-10-21-wasm-runtime.md`, `history/2026-10-27-native-runtime.md`.
- wasm experiment ledger: `history/2026-10-21-wasm-experiments.md`.
- Narrative of the perf work, oldest first: [`performance.md`](performance.md).
- LLVM's -O2 pass list vs ours (2026-10-10, partly stale):
  `history/llvm-passes.md`.
