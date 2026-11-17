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
**Last measured 2026-11-16**, aarch64-apple-darwin, backend `6c7fd69`,
nightly-2026-10-22.

**Overall 95** (2026-11-13: 94, 2026-11-07: 94, 2026-10-30: 95).

| SPD | CMP | MEM | SIZ | STK | COR |
|---|---|---|---|---|---|
| **108** | **105** | **73** | **67** | **83** | **100** |

| category | weight | pliron/LLVM | rating | from |
|---|---|---|---|---|
| SPD runtime speed | 30 | 0.897 | 108 | native 0.873 · wasm 0.922 |
| CMP compile time | 15 | 0.930 | 105 | 5 inputs |
| MEM compile memory | 10 | 1.454 | 73 | peak RSS, 5 inputs |
| SIZ code size | 10 | 1.580 | 67 | machine code, 5 inputs + wasm module 1.83 |
| STK stack frames | 15 | 1.259 | 83 | regex-syntax, 337 shared fns |
| COR correctness | 20 | 1.000 | 100 | crates: 0 pliron-only across 20 crates (same day) |

Correctness inputs: crates tier 1 run in the default `--deps stock` mode
(the crates' own code only); the UI input is the 2026-10-27 stage-2
self-host run. x86-64 was last measured 2026-10-22. The fuzz suite did
not run for this card (no rustlantis generator on the machine).

What moved since the 2026-11-13 card: SPD 0.962 -> 0.897, wasm 1.019
-> 0.922 (the 2026-11-16 wasm work: `history/2026-11-16-wasm-night.md`)
and native 0.908 -> 0.873 (not worked on that night; two of its
transforms, the memcpyopt path window and the second constant-copy
run, apply to native too, and nothing regressed). SIZ 1.519 -> 1.580
and MEM 1.424 -> 1.454 are worse and were not attributed: the wasm
bench module was 183.2 KB when the 11-16 session started and is 178.4
KB now, so that part grew between the two cards, not during it. CMP
0.918 -> 0.930 is inside the machine's noise. STK is where the 11-13
card left it (1.260 then: the two `driftsort_main` monomorphs have 8 KB
frames against LLVM's 4 KB because the `memcpyopt` memset/sret-temp
sink stopped firing there, suspected to be the `frag_copy_stays_whole`
SROA rule; the rest is `visit_`/`parse_` spill traffic).

**Native kernels** (benchfmt + benchstat, pliron vs LLVM measured minutes earlier):

| kernel | LLVM | pliron | pliron/LLVM |
|---|---|---|---|
| arrayvec | 3.672 ms | 3.506 ms | 0.955 ◄ faster |
| chars_alpha | 1.426 ms | 1.398 ms | 0.981 |
| chars_ascii | 1.534 ms | 0.797 ms | 0.519 ◄ faster |
| chars_mixed | 10.835 ms | 10.310 ms | 0.951 ◄ faster |
| contains_rng | 1.575 ms | 1.154 ms | 0.733 ◄ faster |
| eqbug | 0.082 ms | 0.090 ms | 1.101 ▲ slower |
| fib | 10.825 ms | 8.797 ms | 0.813 ◄ faster |
| hashmap | 38.786 ms | 39.834 ms | 1.027 |
| hmget | 8.028 ms | 7.342 ms | 0.915 ◄ faster |
| iterw | 13.440 ms | 9.403 ms | 0.700 ◄ faster |
| matmul | 5.924 ms | 5.872 ms | 0.991 |
| nbody | 78.533 ms | 62.963 ms | 0.802 ◄ faster |
| parse_only | 253.364 ms | 225.123 ms | 0.889 ◄ faster |
| sieve | 8.248 ms | 7.546 ms | 0.915 ◄ faster |
| sortw | 4.696 ms | 4.230 ms | 0.901 ◄ faster |
| strwork | 10.078 ms | 10.020 ms | 0.994 |
| tokenize | 3.849 ms | 2.889 ms | 0.750 ◄ faster |
| varint | 4.266 ms | 4.346 ms | 1.019 |
| varint_postcard | 4.088 ms | 3.532 ms | 0.864 ◄ faster |

**wasm kernels** (node, interleaved rounds):

| kernel | LLVM | pliron | pliron/LLVM |
|---|---|---|---|
| chars_alpha | 3.279 ms | 2.809 ms | 0.846 ◄ faster |
| chars_ascii | 1.673 ms | 1.259 ms | 0.743 ◄ faster |
| chars_mixed | 12.763 ms | 12.476 ms | 0.993 |
| contains_rng | 3.336 ms | 3.329 ms | 1.000 |
| eqbug | 0.093 ms | 0.090 ms | 0.968 ◄ faster |
| fib | 6.325 ms | 5.933 ms | 0.937 ◄ faster |
| hashmap | 45.278 ms | 46.349 ms | 1.023 |
| hmget | 7.847 ms | 8.279 ms | 1.054 ▲ slower |
| iterw | 45.752 ms | 35.556 ms | 0.777 ◄ faster |
| matmul | 21.116 ms | 15.426 ms | 0.731 ◄ faster |
| nbody | 57.878 ms | 58.431 ms | 1.010 |
| parse_only | 262.750 ms | 232.967 ms | 0.888 ◄ faster |
| sieve | 9.595 ms | 9.216 ms | 0.967 ◄ faster |
| sortw | 4.138 ms | 4.134 ms | 1.002 |
| strwork | 11.894 ms | 12.215 ms | 1.028 |
| tokenize | 5.033 ms | 4.407 ms | 0.875 ◄ faster |
<!-- SCOREBOARD:END -->

The wasm table above is one interleaved `parity.py` run. The
layout-averaged `node bench/wasm/run.mjs llvm.wasm pliron.wasm
--layouts=3 --best` on the same commit gives geomean 0.928 with strwork
+6.5%, hmget +6.0%, hashmap +2.8%, nbody +1.0%, sortw +0.6%, chars_mixed
+0.4% behind and the other ten ahead by 3 to 27%. Behind LLVM: strwork
(`dragon::format_exact`), hmget and hashmap (a few percent more moves
in V8's code for the same probe loop).
`docs/history/2026-11-16-wasm-night.md` has what moved and what was
tried.

### Held-out wasm program: dsa (not in the card)

`~/code/dsa`'s `literate-wasm` (wasm-bindgen cdylib: Loro CRDT page,
the Mimas compiler and bytecode interpreter, rapier2d soft bodies),
features `eval,squish,cad`, never tuned on before 2026-11-16.
`harness/dsa/build.sh {llvm,pliron} NAME` then `node
harness/dsa/bench.mjs --llvm=DIR --pliron=DIR` (results are
cross-checked; a mismatch is reported and left out of the geomean).
2026-11-16, main at `bbd0cc5`, pliron/LLVM, median of 9 rounds on a
quiet machine, both packages built from the same dsa HEAD; all 19
workloads return LLVM's results:

| workload | pliron/LLVM | | workload | pliron/LLVM |
|---|---|---|---|---|
| vm_loop | 1.08 | | compile_page | 1.02 |
| vm_fib_rec | 0.98 | | checks_page | 1.02 |
| vm_mandelbrot | 1.11 | | live_page | 1.02 |
| vm_physics | 0.99 | | dims_page | 1.25 |
| vm_sieve | 1.02 | | game_asteroids | 1.41 |
| vm_collections | 0.96 | | game_tetris | 0.97 * |
| vm_strings | 1.07 | | crdt_edits | 0.48 * |
| vm_match | 1.01 | | crdt_sync | 0.50 * |
| vm_closures | 1.00 | | ink_strokes | 0.35 * |
| hash_text | 0.99 ** | | | |

\* not like for like: a no-op export costs 11.4 us in the LLVM module
(rust-lld's export wrappers rerun every static constructor on each
call) and 9 ns in ours; that is 16% of LLVM's `game_tetris`, 30% of
`crdt_edits`, 22% of `crdt_sync` and 49% of `ink_strokes`
(`bench.mjs` measures it and marks the rows).
\*\* 63% of a call is JS; the row reads 1.18 for whichever module is
loaded third.
The fourteen like-for-like rows (vm, pages, asteroids) are geomean
**1.06**. (The morning's table had `game_tetris` at 0.78 and the
comparable rows at 1.03: its LLVM package was built from an older dsa
HEAD.)

Open on this program:
- **Single-caller inlining**, the largest known lever: with
  `PLIRON_INLINE_ONCE=1` the fourteen rows are **1.025** (pages 0.98 to
  0.99, `vm_collections` 0.88), but the wasm kernels `hmget` and
  `tokenize` lose 13 and 17 points. The restricted modes: `=2`
  (loop-free callees) keeps every kernel and gets 1.056; `=3` (one loop
  nest per function) keeps tokenize, loses hmget, and **its dsa build
  corrupts the heap** (being bisected). The root of the hmget loss is
  that a local whose address goes to calls is treated worse than the
  `noalias` out parameter it was before inlining.
- **Code size 45.2 MB against LLVM's 18.4 MB** (49.8 before the
  recursion rule `PLIRON_INLINE_RECX_WASM`, default since `bbd0cc5`;
  35,550 functions against 19,544). Largest single functions:
  `Compiler::compile` 305 KB (LLVM 74 KB), `Search::go_explore` 276 KB.
- `game_asteroids` (rapier2d's soft-FEM assembly and solve): the gap is
  `SoftFemSystem::assemble_operator` (3.2x LLVM's self time). nalgebra's
  small products (`gemm` -> `gemv` -> `axcpy`) stayed six loops each:
  `!beta.is_zero()` on a `beta` held in a stack slot keeps the early
  `unroll` from deciding the trip, and nothing folded `fcmp` of
  constants. `PLIRON_FCMPFOLD=1 PLIRON_FMUL1=1 PLIRON_UNROLL2=1`
  (opt-in, 2026-11-17) make them straight-line; on a rapier2d driver
  (`$TMPDIR/astdrv`, four squish-style Neo-Hookean FEM bodies, 60
  steps) pliron/LLVM goes 1.57 -> 1.37 (load 7, checksums equal), `assemble_operator` self time
  761 -> 328 ms against LLVM's 173. What is left there: the products are
  calls (the IR inliner sees nalgebra's unsimplified 2,294-op bodies;
  LLVM inlines the 33-instruction result), so the loops around them
  stay. The dsa row is pending: the dsa tree did not build on 11-17.
- `dims_page` (1.18 on 11-17, `pkg-pliron-flip` vs `pkg-llvm-n9`): the
  gap is mostly small functions V8 inlines on LLVM's side and cannot on
  ours. V8 inlines a wasm callee only up to 500 body bytes
  (`--wasm-inlining-max-size`). 1,506 functions are at most 500 bytes in
  LLVM's module and over 500 in ours; on `dims_page` they carry 1,143 ms
  of our self time against 352 of LLVM's (+791 of the row's +1,253 ms;
  LLVM's share sits in the callers it was inlined into). Top rows:
  `memchr_aligned` 395 vs 4 ms (1,329 vs 294 bytes), `memcmp` 163 vs 16
  (602 vs 74), `drop_glue::<DecKind>` 108 vs 0 (523 vs 239: we inline
  `drop_glue::<Literal>` into it, LLVM calls it), `Parser::at` 53 vs 23
  (1,122 vs 208), `char::is_contained_in` 39 vs 0 (1,010 vs 193). Two
  causes: search loops that `looprot` and `punroll`'s peel copy eight
  times (memchr, memcmp, `CharSearcher`: `PLIRON_WASM_SMALLFN=120`,
  opt-in, keeps them rolled: 340 and 116 bytes, a string-search driver
  1.33x -> 1.14x), and inlining into small callers (the drop glue,
  `Parser::at`): open. `harness/dsa/v8inl.py LLVM.wasm OURS.wasm
  [profiles]` lists the class. The dsa row is
  pending (the dsa tree did not build on 11-17).
- Compile memory: the default feature set with polars and typst needs
  5-7 GB per rustc and was not built.

The module built with `PLIRON_OPT_BISECT=0` returns LLVM's results too
since `5d703d0` (two bugs in stages that run with every pass off).
`docs/history/2026-11-16-wasm-night.md` has the five bugs this program
found and what moved the numbers.

### Held-out runtime (not in the card)

SPD is measured on the kernels above, which the backend has been tuned
against for weeks. On real programs it was never tuned on, native code is
**1.054x slower** than LLVM (geomean of 8; 2026-11-13, 5 interleaved
rounds, `bench/heldout/run.py`, AC power; was 1.076x on 11-11 and
1.054x on 11-10 — within day-to-day machine drift):

| program (sightglass) | LLVM | pliron | pliron/LLVM |
|---|---|---|---|
| blake3-scalar | 4.32 ms | 4.05 ms | 0.939 |
| blind-sig | 1.36 ms | 1.50 ms | 1.096 |
| pulldown-cmark | 4.81 ms | 5.51 ms | 1.147 |
| rust-compression | 3.65 ms | 3.32 ms | 0.910 |
| rust-html-rewriter | 5.69 ms | 7.36 ms | 1.293 |
| regex | 3.42 ms | 4.13 ms | 1.209 |
| rust-json | 3.60 ms | 3.85 ms | 1.068 |
| rust-protobuf | 8.23 ms | 7.00 ms | 0.851 |

Since the 11-09 measurement `PLIRON_BJALIAS` (default on, afcc31b) removed ~600 dead
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
- **`parse_only` on wasm.** ~1.45× LLVM (2026-11-14). Cold edges /
  `ERRCOLD` / `COLDBR` / `WBRIF` are neutral. Residual is **digit
  `from_str` shape**: hot TF 517 vs 229 insns (`madd` 50 vs 7); wasm
  `i32.mul` 45 vs 7. Cause: `ifconv` speculates several length/sign
  arms' digit unrolls into selects (all paths' muls run); `IFCONV=0`
  drops to 3 muls / loop form and is *slower* (~1.54×). LLVM keeps one
  unroll behind `br_table`. Landed toward that: SwitchOp on wasm,
  no `small_switch` on wasm, no `br_table`→select ifconv on wasm
  (module `br_table` 0→19) — parse's hot fn has 0 tables / 45 muls.
  `PLIRON_SWITCH_GCC=1` (opt-in)
  adds GCC-style clustering (xoranth.net/gcc-switch): bit-test packs,
  density jump tables with outlier peel, balanced decision tree of
  leftover ranges (`pliron-clif-lower/src/switch_gcc.rs`). Wasm A/B:
  parse_only ~1.46× → ~1.23× (win is bit tests elsewhere). Digit-shape
  bisect (CLIF dumps of `k_parse_only`): **not** `ifconv2`
  (stays at 2 imuls). Peel/punroll → ~10; late **`egraph2` remats
  the shared Horner `*10` into every `len==k` early-exit** →
  15→45 static muls. Skipping `egraph2` leaves 17 muls but is
  *slower* on V8 (~1.52× vs ~1.46×): specialized straight-line
  length arms beat the shared branched unroll. `SWITCH_GCC=1` is
  the measured win (~1.23×) via bit tests, not the digit shape.
  LLVM `k_parse_only`: **7 mul / 2 br_table / 335 ins** (br_tables
  are sign/mode dispatch, not length; digit path is a 7-mul
  shared unroll with `len==k` br_ifs). Ours: 45 mul / 0 br_table /
  ~826 ins. Clean ≤8 digit peel pinned in
  `pliron-clif-opt/tests/parse_digit_llvm_shape.rs`. Still open:
  emit LLVM's compact shared unroll (not the egraph2-rematted
  arms). `SWITCH_GCC` is the wasm default; see the card below.
  V8 profile loop (2026-11-15, `hotw`/`mcode`/`run.mjs --layouts=3`):
  both sides are `load8_u`-bound; tick ratio ≈ wall time. TF `madd`
  50 vs LLVM 7 / `ldrb` 26 vs 14 — rematted Horner, not mul latency.
  LLVM `k_parse_only`: flat `br_if`/`br_table` (0 `if`, 2 `br_table`,
  7 mul, 335 ins); ours: ~34 nested `if`/`else`, 0 `br_table`, 45 mul.
  Before the `divconst` order fix, `SWITCH_GCC`+`WBRIF` was parse
  ~1.21× and geomean 0.940 (the win was `if`→`br_if`, not fewer
  muls). Progressive
  peel (`EGRAPH2=0` / `PLIRON_EGRAPH_IMULCAP=1`) stays slower on parse
  under V8 even with WBRIF. `PLIRON_PEEL_DIGIT=1` (opt-in) peels a
  `* 10` loop 8 copies instead of 4 when the body exceeds the 12-inst
  cap; with GCC+WBRIF, parse_only went ~1.21× → ~1.23×, so it stays
  off. Flattening a nest of `if { br } else` (`PLIRON_WASM_BRI_NEST=1`,
  opt-in; one pass only folds the outermost if, and a `Nop` from a
  dead `get t; set t` used to hide the inner `br`) on the rematted
  arms: parse ~1.23×, geomean 0.963 vs WBRIF’s 0.944, and it drops
  chars_ascii’s −25%. The same flatten on the 17-mul progressive
  chain (`IMULCAP` + `BRI_NEST`): parse ~1.28×, geomean 0.951.
  LLVM’s short path is one `cmp remaining, #9; b.lo` into a single
  w32 `madd` chain (no `0x19999999` overflow check — len≤8 cannot
  overflow); the i64 `mul` loop is only len≥9.
  The remaining gap was not that chain. TurboFan’s hot loop spent
  ~20 instructions on `i % 5` because the egraph rewrote `urem.i64`
  to a 64-bit magic multiply before `divconst` could narrow a
  dividend proved `< 2^32` (the `0..1e8` counter) to `umulhi.i32`.
  `s_egraph` now runs `divconst` first on wasm. That alone is
  parse_only **1.09×** (294 vs 270 ms), wasm geomean 0.943.
  `SWITCH_GCC` (wasm default; `PLIRON_SWITCH_GCC=0` off, `=1`
  forces it on native too) plus `WBRIF` (default on, `=0` off):
  parse_only **0.986×** (265 vs 270 ms), geomean **0.932**, every
  kernel result matches LLVM. Module ~171 KB vs LLVM 98 KB.
  WBRIF alone was the best card (parse 1.057×, geomean 0.922,
  tokenize −18%) but does not get parse to 1×. GCC alone does
  not move parse. Native stays off `SWITCH_GCC`: the tree stores
  a searcher cursor on every byte of `split_whitespace`.
  Knob split after that fix (`--layouts=3`, same llvm.wasm):
  WBRIF alone is the best card — parse **1.057×**, geomean
  **0.922**, tokenize −18%, `chars_ascii` −25%. GCC alone does
  not move parse (~1.086×) and gives tokenize back (+1.8%,
  geomean 0.956). GCC+WBRIF is the only ≤1× parse config and
  costs tokenize (+3%, geomean 0.932). The parse win is one
  `byte < '+'+1` split so the digit path skips the sign
  `select`s; the len==1 bit-test is cold for this bench.
  `PLIRON_SWITCH_JT_MIN=1000000` (bit tests, no jump tables) is
  byte-identical to full GCC+WBRIF. Raising the wide-span bail
  so Cranelift keeps huge switches kept the parse win (−1.4%)
  and did not save tokenize (still ~+2%, geomean 0.936) — the
  ASCII whitespace switch is narrow, so it still clusters.
  `hotw` on `tokenize::run`: WBRIF's ticks are `load8`/`load`;
  GCC's are a per-character `i32.store` of the searcher cursor
  in front of the whitespace test (~60% of ticks). Dropping the
  bit-test (`PLIRON_SWITCH_NOBIT` plus `PLIRON_BITTAB=0`, which
  rebuilds the same `mask >> c` from the compare tree) removes
  the shift and leaves the store. Not landed.
  Both knobs are the wasm default (native `SWITCH_GCC` stays
  opt-in). The store is not the ASCII bit-test: it is the
  4-case `c >> 8` bucket switch in
  `CharPredicateSearcher<IsWhitespace>::next_match` (cases
  0/22/32/48). `PLIRON_SWITCH_GCC_MAX` (default 2, `=0` no cap)
  clusters only the 2-case `'+'`/`'-'` split. `--layouts=3`:
  parse_only **0.994×** (274 vs 276 ms), tokenize **−17%**
  (the no-cap tree was +3%), geomean **0.921**. The other
  kernels match clustering-off; `strwork` improves 10.0% → 8.6%.
  Profile: `harness/v8/{cpuprof,hotw,mcode}.sh`.
  **`sortw`:** the hot path is `ipnsort`'s descending-run scan
  (`v[i]` vs `v[i-1]` over the reversed 200k array). `ldrot`
  already knew that pair. It skips the pair on native because
  rotating it hides the loads from `loopvec`: the pass comment
  records ~17% on native sortw's run scan. That native number
  was not re-run here. The same pass also bailed when the index
  was still a stack store. `PLIRON_LDROT_PAIR` (wasm default,
  `=0` off) rotates i64 pairs and ignores an isolated stack
  store. Native stays off so that loopvec result is left in
  place; the stack-store exception was timed on wasm only, and
  there is no native number saying it would be slower.
  Wasm `--layouts=3`: sortw **1.007×** (6.147 vs 6.091 ms; was
  1.29×, then 1.11× after i64 `ldrot`). Only i64 pairs: the
  dumped ipnsort body had no vector compare, and that is the
  pair the wall time moved. A wasm build that rotated every
  width also had `PLIRON_ASSUME_RANGE=1` and lost chars_ascii's
  −25%; those two were not split, so narrower pairs are left
  alone. With i64-only and assume-range off, chars_ascii stayed
  −25%, tokenize −17%, strwork flat, parse_only ≤1×.
  The scan is now a carried pointer (`local.get p; i64.load;
  p += 8`). Two wasm-only indvars changes, neither re-timed on
  native: `ivrefold` keeps a ratio-1 pointer whose base is not
  a constant (a constant base still folds into the load
  offset; native `amode_scale` still refolds every ratio), and
  a bare `load (base + iv)` becomes a stepped pointer even
  when `base` stays live, following the block param the latch
  passes into the load. The same pass steps
  `base + ((A - iv) << k)` by `-k` per iv step, so the reverse
  or swap loop stops recomputing `index << 3` on the other
  address. Native keeps that index form for scaled addressing;
  it was not timed. Same-run `--layouts=3` against the
  forward-pointer binary: sortw 0.7% [0.5..1.1] vs 6.4%
  [5.5..6.5]. chars_ascii −24.9%, chars_alpha −11%, strwork
  still ~1.10×. `chars_mixed` is **1.10×** with
  `PLIRON_HALFUNROLL=1` (opt-in, wasm only, not timed on
  native): the 54-entry probe chain unrolls; the byte scan
  does not. Folding the probe table into the memarg
  (`PLIRON_WASM_ADDROFF=1`, opt-in) made chars_mixed about 1
  point slower and left ascii at −25%, so it stays off.
  `PLIRON_ASSUME_RANGE` (`=1`, and ignored on native, which
  was not timed) turns `assume(i < n)` into a branch. It
  stays off; it was only timed in that confounded wasm binary.
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
- **x86-64** was re-measured 2026-11 on Windows (i7, msvc): native
  geomean ~1.09 (`tokenize` ~1.77, `fib` 1.35, `iterw` 1.37 worst),
  wasm ~1.01, STK **1.218** after the Tail-conv Windows ABI fixes
  (`PLIRON_X64_TAILXMM` xmm callee-saves + `PLIRON_X64_TAILARGS`
  fastcall-lane args; was 1.556 after honest `__chkstk`/probe
  accounting). Session log: `history/x64-notes.md` "Windows x64
  session".
  2026-12 follow-ups on the same box: `tokenize` ~**0.91** (faster
  than LLVM — `ASSUMENN` seeds the cursor's nonnull through
  `assume(!ptr.is_null())`'s `xor`-peeled form, killing the
  `Option<&u8>` niche `testq/je` per byte; plus `bt` lowering for the
  whitespace table probe through `ireduce`/`band-63` index masks);
  `tokenize` re-measured ~1.4x before `EDGESEL` (the earlier ~0.91
  build predates later churn — measured on this box's exact kernel:
  `split_whitespace().count()` ×10 over a 400 KB string), then
  ~**0.93** (faster than LLVM — `EDGESEL` folds
  `select c`/`brif c` under the edge condition that pins it: chars'
  `next().unwrap_or(0)` clamps emitted a dup `cmp` + 2 `cmov` per
  ASCII byte, ~3.8 → ~2.5 ms vs LLVM ~2.7);
  `fib` ~**1.05** with the exact LLVM loop shape (`ARGDIRECT`: a
  call-arg copy died once `arg_detach` stopped firing on transitive
  `Multiple`); `iterw` ~**1.0** (`CHAINJOIN` places a diamond's arm
  before its join — `filter`'s keep-block was out-of-line at 2 taken
  branches/kept element, ~1.40x); `contains_rng` ~**0.65** (faster
  than LLVM — `VRANGE` fuses the `(lo..=hi).contains` keep mask into
  `~((x<lo)|(x>hi))` with one shared sign flip and invariant bound
  flips, ~6 insts/lane group vs ~12 with per-bound remats; ~3.0 →
  ~1.6 ms vs LLVM ~2.5); `parse_only` ~283 → ~270 ms vs LLVM ~280
  (`IMULEA`: `x·d`/`x·d·2^k` → `lea` + consumer-amode `ishl` fold —
  `acc·10+d` is two `leal`s — plus opt-in `SUAMODE`: `to_amode_add`
  only folds `iadd x,K` into the displacement when the sum has ≤1
  direct use, so `digit = char-0x30` materializes once for its
  range-check + accumulator uses and `char`'s live range dies —
  LLVM's exact digit-loop shape; now default-on). Later re-survey
  on the same box (standalone min-of-N mains): `strwork` ~1.04,
  `chars_mixed` ~1.05, `hashmap` ~1.05–1.1, `hmget` ~1.09,
  `nbody` ~1.0, `sieve` ~1.03, `matmul` ~0.97, `sortw` ~0.87,
  `chars_ascii` ~0.66 — all at parity or faster. `fib` resolved:
  same-process A/B (both backends' fib linked into one binary)
  proved the ~1.4x was block *layout*, not instructions — our
  placement inlined the leaf arm and forced a taken `jae` per
  non-leaf call, a front-end redirect bubble; LLVM puts the loop
  arm on the fall-through path. `LOOPFT` (default-on,
  `PLIRON_LOOPFT=0` off) prefers a successor that can reach a
  back-edge latch on placement ties — reverse BFS from latch blocks
  in both `BCHAIN`'s chain pick and `PLACE`'s edge-weight key —
  and fib now measures ~1.0x standalone (5.9 ms both). `stubhoist`
  (default-on, `PLIRON_STUBHOIST=0` off) then closed the residual
  bench gap: single-pred stubs that compute their `jump` args in-block
  (`uext n; jump ret(v)`) fold into the pred's edge, so fib's leaf
  arm `jb`s straight into the shared return — fib(28) ~0.49 ms vs
  LLVM ~0.55 ms. `tailrec`
  relaxed (e076991): the accumulate may now sit anywhere between
  the self-call and the return with only speculatable insts
  between, so `sum(n-1)+(n&7)` rotates — 50 M-deep recursion runs
  in O(1) stack and `loopvec` vectorizes it. The vector loop now
  carries affine lane values (`coeff*iv + off`, |coeff| = 1) as
  vector-header params stepped once per group (`PLIRON_VECIV`,
  default-on) — LLVM's vector-IV form; `sum(50M)` measures
  ~5.4 ms vs LLVM ~6.8 ms (was ~11.7 ms with per-group
  splat+iota+psubq).
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
- **Emulated-EH tax (measured 2026-11-12, x86-64 COFF).** On a
  call-heavy probe (5254 B vs 2437 B LLVM = 2.16x, 1226 vs 694
  insts): every maythrow call site pays `load __pliron_eh.flag;
  brif` plus register shuffling (~10-15 B), and every function
  containing one pays the ~25 B `_tls_index`/`gs:[0x58]` chain —
  zero inline cost under LLVM's SEH `.xdata`. Forwarding shims
  (`__rust_alloc` et al.) show it worst: 59 B vs LLVM's 5-byte
  `jmp`. Secondary: no push/pop prologue (movq spill pairs at
  ~4-5 B vs 1 B), movq traffic 379 vs 157 (call arg shuffling +
  EH spills), no tail calls, dead `jmp` stubs after `ret`.
  Landed: strict-`nowrite` callees now skip `eh_check` (an
  emulated raise is a TLS store; a write-free callee can contain
  none) — nounwind inference already covered `nounwind` callees;
  `stubhoist` sweeps orphaned forwarder blocks unconditionally.
  `commonret` (default-on, `PLIRON_COMMONRET=0` off) folds
  `brif c, A, B` whose arms are the same `return` — the
  `brif flag, return, return` `eh_check` shape on drop-free/void
  exits — and the flag load is `MemFlagsData::trusted` so the
  dead check DCEs: `__rust_dealloc` 49 B → 13 B, probe text
  5254 → 5023 B (-4.4%). Import/indirect calls keep the check —
  a foreign callee can call back into pliron code that raises
  (the `sort_by` callback case), so they legitimately maythrow.
  The remaining big lever is real SEH (`docs/seh-design.md`,
  unlanded).
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

### Codegen queue (2026-11-17, overnight)

Ranked work for the agents that keep improving generated code; take the
top item nobody holds, mark it with the date when you start, move it to
the history log when it lands. Every item: default-off knob, unit tests,
corpus replays, `harness/xcorpus.py` for IR-pass changes, the wasm
kernels (no kernel may lose; time only at load under ~10, else report
bytes and leave it opt-in), one dsa build with the 19-workload
cross-check (the LLVM side must be built from the same dsa HEAD:
`bench.mjs` refuses otherwise).

1. **N1, the wasm loop-site inline bonus** (in progress 2026-11-17).
2. **`game_asteroids` 1.39x against LLVM** (2026-11-17: `PLIRON_FCMPFOLD`, `PLIRON_FMUL1`, `PLIRON_UNROLL2` landed opt-in, see "Held-out wasm program"; flip once the dsa row is timed; next lever: inlining nalgebra's products), the worst like-for-like dsa
   row (rapier2d's soft-FEM assembly and solve). Profile both sides
   (`harness/dsa/prof.sh game_asteroids`, `cmpprof.py`, `hot.sh`), find
   the top functions by self time and what our code does worse there
   (V8 machine code via `harness/v8/mcode.sh`-style listings), fix the
   largest cause.
3. **Inline nalgebra's small products by their simplified size.** After
   item 2's knobs, rapier2d's `Matrix<2,3> * Matrix<3,2>` (`Mul::mul`)
   compiles to 33 CLIF instructions (12 loads, 8 `fmul`, 4 `fadd`), but
   the IR inliner scores its unsimplified body: `reject (score) n=2294
   t=512` (`PLIRON_REMARKS`; the IR is 765 `br`, 431 lifetime markers,
   225 allocas, 163 loads, 160 stores, 12 `fmul`). LLVM inlines the
   simplified body, then unrolls the `for p`/`for q` loops of
   `SoftFemSystem::assemble_operator` around the four products and keeps
   everything in registers. Ours stays four calls in loops (waffle
   inlines them afterwards, too late for the caller's passes):
   `assemble_operator` self time 328 ms against LLVM's 173 on the
   rapier2d driver (`$TMPDIR/astdrv`, 1.37x with item 2's knobs).
   `PLIRON_INLINE=2400` globally is slower (assemble 430 ms). Ideas: a
   cost that does not count free ops (`inline_cost::spec_cost` is
   native-only), allocas whose loads/stores SROA removes, or constant-trip
   loops at their unrolled size; check hmget/tokenize (no kernel may lose).
4. **`dims_page` 1.2x** (in progress 2026-11-17; diffuse in the profile): same method; the
   self-time join first.
5. **`contains_rng` +9.5% on wasm** (an honest gap since the `black_box`
   fix: its bounds are no longer constants). Compare with LLVM's wasm and
   TurboFan code for the loop.
6. **N7, the default hmget +5% / 102 vs 96 TurboFan instructions per
   lookup**: `invparam` splits the lookup loop into two headers (the
   memory base reloads at both), the hash expression has two extra
   xor/rotate pairs, `k + (round & 1)` is recomputed per lookup.
7. **N9 #3 and #4**, constant rematerialization (up to 824 KB on dsa)
   and values parked in locals across block openers (0.3-0.6 MB):
   waffle `treeify.rs::is_remat`, `stackify.rs`.
8. **vm_loop / vm_mandelbrot +5-10%** (the Mimas interpreter's dispatch
   loop): profile `step_one`. Looked at 2026-11-17 on the fad51e94
   packages (dsa HEAD does not build, so there is no dsa cross-check
   yet). Retired instructions per call (`--loop=N` minus `--loop=2`):
   vm_loop 2783M against LLVM's 2717M (+2.4%), vm_mandelbrot 3081M
   against 3006M (+2.5%); cycles about even. The profile shares match
   (dispatch loop ~51%, `step_one` 37-40%) and the TurboFan code of
   both hot functions is nearly the same instruction for instruction:
   `step_one` has a larger frame (240 bytes against 144) and loads
   opcode tags as `ldr`+`and` where LLVM uses `ldrb`; the gap is
   spread out, with no single cause. A standalone reduction of the
   dispatch shape (`step` returning a 136-byte `Flow` through sret)
   found one real cause that the dsa package does not have at its
   `step_one` call: sroa wrote every slice of the split sret temporary
   back before each call (18 `i64.store` per iteration; 33.1 ms
   against LLVM's 20.2). `PLIRON_SROA_SRETNOSYNC=1` (opt-in, stanza
   `sretnosync`) skips that writeback (21.5 ms). On the wasm kernels
   it changes only `k_sieve` (two dead stores; timing unchanged under
   `--perturb=4`), and on std 60 of 4,925 functions (`xcorpus`: no
   mismatch on wasm or std). Open: what makes `step_one`'s frame
   larger, and the byte tag loads.
9. **Native**: the held-out programs (`bench/heldout/run.py`, 1.08x
   on 2026-11-09) with tonight's wasm-only defaults tried on native one
   at a time (`PLIRON_LICM_GTE=1`, `PLIRON_LICM_SLOTPROV=1`,
   `PLIRON_INLINE_RECX=1`): exact instruction counts with
   `bench/heldout/irdiff.py` decide.

Opt-ins waiting only on a quiet-machine kernel timing (a detached batch
times them; flip each that keeps every kernel): `PLIRON_WASM_PEEP_SIMD`,
`PLIRON_WASM_BRTAB_SHARE`, `PLIRON_WCOAL_EDGE`,
`PLIRON_MEMCPYOPT_SIBARMS`, `PLIRON_WASM_ONEEXIT`, `PLIRON_WASM_MASKELIM`.
Blocked on V8 code placement (being investigated): `PLIRON_WASM_ICF`,
`PLIRON_WASM_NOREALIGN`.

### Next: wasm code size and robustness (queued 2026-11-16 evening)

From the `Compiler::compile` investigation
(`history/2026-11-16-wasm-night.md`, "Evening"). Each item is sized
for one agent; take them as agents free up. The single-crate loop is
the fast way to measure all of them: build only `mimas-compile` from
`~/code/dsa/host` through the backend (it reproduces the whole-program
function sizes, and rerunning the one crate's rustc command takes 9 s;
recipe in `$TMPDIR/mcomp/NOTES.md`, script `bin/run.sh TAG K=V..`),
then a whole dsa build and `bench.mjs` for the result cross-check.

- **N1. The wasm loop-site inline bonus.** In small (wasm) mode a call
  in a loop gets 3x the 512-op limit (`t=1536`, `PLIRON_INLINE_HOT`).
  It pastes SipHash `write_str` (930 ops) and `IndexMap::insert_full`
  (472) four times each into `Compiler::compile::export`, where LLVM
  calls them: 24 KB against LLVM's 7 KB; `PLIRON_INLINE_HOT=1` gives
  18 KB, and it carried 58% of the ops inlined into `compile`. The
  bonus exists for hashbrown's `hash_one` in probe loops (the hmget and
  hashmap kernels). Find a rule that keeps those and stops the rest
  (callee in a loop of the *callee's own* hot path, a cap per callee
  per caller, or the bonus only for callees below some size), measure
  the kernels (`bench/wasm/run.mjs`, no kernel may lose: user rule) and
  the dsa module (size and the 14 like-for-like rows).
  **2026-11-17, on main with single-caller inlining:** the `export`
  case is gone: no `write_str`/`insert_full` copies, one bonus
  `hash_one`; `export` is 18,376 B with the bonus and 18,301 B without
  (`mimas_compile` alone, crate 1,482,364 B; `PLIRON_INLINE_HOT=1`
  1,466,697 B, `=2` 1,488,188 B). Accepts that need the bonus (cost
  above 512): 18 on kernels+std (`hash_one` 2x in `hashmap::run`, 1x in
  `hmget::run`/`setup`; `Big32x40::cmp` 4x in `format_exact`, which
  `strwork` runs; quicksort `partition`), 1,398 on dsa (1.1 M ops, mostly
  `hash_one` again, up to 9 copies in one caller). Opt-in
  `PLIRON_INLINE_HOTCAP=k` caps the bonus copies of one callee per
  caller, `PLIRON_INLINE_HOTCAP_MIN=c` only for callees costing more
  than `c`. Without the floor the cap loses kernels (cap 2: `strwork`
  +1.4 points; cap 1: `hashmap` +1.5, quiet machine). `HOTCAP=2
  HOTCAP_MIN=600` leaves the kernel module byte-identical, takes 0.46%
  off `mimas_compile` and 9.1% of the dsa bonus ops by the trace; the
  dsa module size and rows are not measured yet (dsa HEAD 8e943d64 did
  not build: its `mimas` submodule was behind).
- **N2. waffle's handling of an irreducible CFG.** One irreducible edge
  pair turned 4,752 waffle instructions into 123,047 wasm instructions:
  the `Reducifier`'s max-SSA conversion keeps every cut block's
  parameter even when all predecessors pass the same value
  (`vendor/waffle/src/passes/maxssa.rs`, the `is_cut_block` exception),
  and `stackify.rs::do_branch_select` gives every `br_table` entry its
  own edge block (520 of 915 were byte-identical repeats; a 35-entry
  table had 35 distinct depths where LLVM's 93-entry table has 4).
  Two fixes, each a `PLIRON:` patch with a `PLIRON_PATCH.md` entry:
  share one edge block per distinct (target, arguments) and branch
  straight to the target when there are no arguments; apply the
  all-same alias rule at cut blocks where it is sound (work out when
  that is: the cut exists to give the loop one entry). Measure on a
  function that still reaches waffle irreducible (force one with
  `PLIRON_REDUCIBLE=0` on `mimas-compile`) and module-wide: 98 dsa
  functions had edge-copy runs of 32+ copies (878 KB) before
  `f4cf460`; count again after it. `peepdiff` and the gate's wasm leg
  check the output.
  **Done 2026-11-16 (three `vendor/waffle` patches, `PLIRON_PATCH.md`).**
  dsa at `fad51e94`, against main's defaults (34,293,484 B of code),
  each alone, all 19 workloads equal on each:
  (1) shared `br_table` edge blocks (`PLIRON_WASM_BRTAB_SHARE=1`,
  opt-in): -528,900 B (`Compiler::compile` 48,775 -> 40,651); kernels
  -462 B (strwork, tokenize, `fmt::pad_formatted_parts`), so it waits
  for a quiet-machine timing. (2) cut-block aliases in the reducifier's
  max-SSA plus removal of strongly connected sets of parameters that
  carry one value (`PLIRON_WASM_MAXSSA_CUTALIAS`, default on, `=0` off):
  -56,915 B, kernel module identical. The rule: a cut block's parameter
  can be an alias when every edge passes one value defined outside the
  blocks the reducifier may copy (blocks whose header set meets an
  irreducible header); with the exact set of copied blocks instead
  (more aliases) `compile` under `PLIRON_REDUCIBLE=0` grew 299,624 ->
  350,760 B, because longer outer live ranges force more edge copies, so
  the superset stays. (3) the same parameter-set removal on every body
  in the emitter (`PLIRON_WASM_PARAMSCC`, default on): -329,027 B
  (loro's `MovableListState::apply_diff` 109,331 -> 67,117), kernel
  module identical. `mimas-compile` alone with `PLIRON_REDUCIBLE=0`
  (forced irreducible): `compile` 306,050 B; (2) 299,624; (1) 168,372;
  both 160,639 (49,474 reducible). The rest of N9 row 1 is not
  parameters that carry one value: in the kernels there are none.
  **Edge-copy coalescing** (`PLIRON_WCOAL_EDGE=1`, opt-in, waffle
  `localify.rs` `coalesce_edge_copies`: merge a parameter's local with
  an argument's local on the precise interference graph when they do not
  interfere): small payoff. dsa (main's defaults 33,911,719 B):
  603,375 edge transfers between different locals, 14,842 pairs did not
  interfere, -86,693 B (-0.26%), all 19 workloads equal; kernels -115 B.
  Of the interfering pairs (`mimas-compile`, 19,437), 63% have neither
  value live past the edge (the old parameter is still read in the
  predecessor after the argument is computed: a scheduling question,
  moving the argument's computation past the parameter's last use), 34%
  pass an argument that stays live after the edge (value-based
  interference could merge some), 2% other. waffle's tests now run in
  the gate (`unit:waffle`).
- **N3. Byte peepholes on bodies with SIMD.** `wpeep`'s decoder gives up
  on any `0xfd`-prefixed opcode, so a body containing one `v128.load`
  gets no peepholes at all. The emitter lowers a 24-byte constant copy
  as `i8x16` + `i64` (independently of `PLIRON_WASM_SMALLCOPY`), which
  put `compile`, `export` and 45 other bodies of `mimas-compile` out of
  reach; similar bodies shrink 5-10% under the peepholes. Teach the
  decoder the `0xfd` immediates (memarg for loads/stores, lane index,
  16-byte `v128.const`, `i8x16.shuffle`) and treat SIMD values as
  opaque to the forwarding rules. Count the bodies skipped before and
  after (`PLIRON_WASM_SIZELOG=1`), `peepdiff` the result (it skips
  v128 signatures as entry points: check it still runs the bodies),
  `PLIRON_WASM_AUDIT=panic` in the gate.
  **Implemented 2026-11-16, opt-in `PLIRON_WASM_PEEP_SIMD=1`.** The
  decoder reads every `0xfd` immediate; SIMD instructions stay opaque to
  the forwarding rules (unknown to their effect tables, a clobber for
  `sfwd`), v128 locals are never a candidate of a local rewrite, and a
  body whose signature has a v128 param is still skipped; `btr` and `bob`
  size SIMD instructions (`simd_eff`, depth only). dsa: all 2,456 SIMD
  bodies are now peepholed (none before; 6.44 -> 5.81 MB in the
  objects), code 34,293,484 -> 33,810,313 B (-483 KB), untargeted
  blocks in SIMD bodies 45k -> 1.9k, branch-over-branch 15k -> 0,
  fallthrough `br` 15k -> 651; all 19 workloads equal; peepdiff clean
  on 68,923 bodies (a sample of 9 SIMD bodies: 8 ran to a `return` in
  some trial), built and run under `PLIRON_WASM_AUDIT=panic`. Kernels:
  145,300 -> 145,003 B (three SIMD bodies: hashbrown `reserve_rehash`,
  two `type_id`). Not flipped: the kernel module is not byte-identical
  and the machine never got quiet enough to time it (load 20-140).
- **N4. Done 2026-11-16.** `PLIRON_JUMPTHREAD=0` crashed loopidiom's
  dead-loop deletion: its walk over the exit's forwarder blocks stopped
  at its 8-block budget one step past the block its args described
  (index out of bounds, or a fast edge with the wrong values), and a
  forwarder's param became an escape bound on edges it did not dominate
  (verifier errors). `PLIRON_TAILDUP=0` left dead blocks whose branches
  slotprom did not extend. clif-check also ignored the fifteen
  step-category switches (`PLIRON_JUMPTHREAD`, `PLIRON_LICM`, ...), so a
  corpus could not reproduce the crash. All fixed with tests;
  `harness/offswitch.py` sweeps every CLIF off switch (std and
  wasm-dsa-eval clean after the fixes).
- **N5. The 19 MB in functions LLVM's module does not have: analysed**
  (2026-11-16, both modules from dsa `0caf4bb8`; scripts and every
  number in the session scratchpad `n5/NOTES.md`). Ours 36,046
  functions / 36.10 MB of code, LLVM's 19,845 / 14.48 MB. By normalized
  name 15,010 of ours have a counterpart (16.37 MB); 21,036 do not
  (19.73 MB). First-match classes of those 21,036:

  | class | functions | MB |
  |---|---|---|
  | one direct caller, not exported or in a table | 9,468 | 14.70 |
  | identical code to another of our functions | 9,271 | 2.57 |
  | same path in LLVM with other generic arguments | 1,540 | 1.48 |
  | the rest | 757 | 0.98 |

  The bytes sit in 256 B-4 KB and larger functions, not in tiny ones
  (5,354 functions under 64 B hold 90 KB). The linker's dead-function
  GC is already complete (2 unreachable functions). **Inlining the
  single-caller ones does not shrink the module today**:
  `PLIRON_INLINE_ONCE=1` removed 8,492 functions and added 7.65 MB
  (locals +4.25 MB, control +1.70 MB, functions of 50 KB+ from 10 to
  56), because our code inlined into a large function grows local
  traffic: the single-caller clusters are 2.64x LLVM's bytes for the
  same work while their roots alone are 1.39x. So the order is density
  first (N9), then single-caller inlining. What is left of this item:
- **N8. Identical-code folding in `pliron-wasm-ld`**: 9,271 bodies are
  byte-identical to another of ours (signature and code equal, call
  targets compared by class), 2.99 MB module-wide. Examples: 19
  `quicksort<Hap>` copies (41 KB), 67 `Vec<String>::extend_trusted`
  (40 KB), 13 local copies of SipHash `write_str` at 2,687 B each (the
  same-name fold `PLIRON_LDEDUP` only folds exported mangled symbols).
  Fold to one body and redirect calls and table entries; keep functions
  whose address is compared if any (Rust does not promise distinct
  function pointers; check exports and `ref.func` uses). Iterate to a
  fixpoint (folding callees makes callers identical). Measure module
  bytes and that all 19 dsa workloads still return LLVM's results.
  **Done behind `PLIRON_WASM_ICF=1` (opt-in, read by the linker,
  2026-11-17):** partition refinement on the linked module
  (`tools/pliron-wasm-ld/src/icf.rs`). dsa at fad51e94 (the build with
  tape/pl/tensor/typeset, 133,912 functions): 34,789 folded into 9,744
  kept in 12 rounds, raw module 189.6 MB -> 167.2 MB (-11.8%), package
  203.7 -> 178.8 MB; ICF itself 1.5 s of a 3-7 s relink; all 19
  workloads return LLVM's results. Wasm kernels: 6 of 164 folded,
  -1,536 B. `fib` runs 2x slower with ICF: not placement but TurboFan's
  wasm inlining budget, which follows the module's small functions
  (144 instead of 146 nodes, so `fib` stops inlining its own recursive
  call; two unused functions appended restore it). Over
  `run.mjs --perturb=8` fib is -7.5% (base) vs -4.5% (ICF), and
  NOREALIGN's `parse_only` loss (an odd 4-byte loop shift; even shifts
  are fast) is -4.6% vs -4.9%: both neutral once placement is averaged
  (`history/2026-11-17-v8-placement.md`).
- **N9. Code density: the same function is 1.42x LLVM's bytes, and the
  excess is in small functions** (13,089 functions present in both:
  under 256 B 2.45x, 256 B-1 KB 2.00x, 1-4 KB 1.42x, 4-16 KB 1.29x,
  16 KB+ 0.81x; 65% of the excess local traffic, 28% control). Decoded
  module-wide, ranked by bytes (analysis and scripts: session
  scratchpad `n9/NOTES.md`):

  | # | change | where | saves |
  |---|---|---|---|
  | 1 | drop block params every edge passes unchanged; coalesce a param's local with its edge argument | waffle `passes/maxssa.rs`, `backend/localify.rs`, `BlockParams` moves in `backend/mod.rs` | est. 1.0-1.6 MB (copies 2.24 MB vs LLVM 0.15) |
  | 2 | `block; ..; br_if 0; br K; end` -> inverted test; `br_if K-1` | `wpeep`, or stackify's `If` rewrite (`PLIRON_WBRIF`) | 557 KB (114,763 sites; LLVM 676) |
  | 3 | keep a repeated long constant in a local instead of rematerializing | waffle `treeify.rs::is_remat` | up to 824 KB (481 KB of it gap) |
  | 4 | `local.set X; block+; local.get X` (224,868; LLVM 0): open blocks before the value is computed so it stays on the stack or tees | waffle `stackify.rs` | 0.3-0.6 MB |
  | 5 | blocks nothing branches to (185k in 20,462 functions) although `wpeep_elide` exists and is on: find why it misses them first | `src/wpeep_elide.rs` | 370 KB |
  | 6 | `i32.const 255; i32.and` on values already zero-extended (41,399 vs 4,926; 11,639 double masks) | `clif_emit` narrow values | ~180 KB |
  | 7 | a `br` whose fallthrough reaches the same place | `wpeep` | 104 KB |
  | 8 | restore the stack pointer once per function, not before every `return` (2.45 per function vs ~1) | `src/wasm.rs` `CX::ret` | 75-100 KB |
  | 9 | no `& -16` realignment for frames that need at most 16 (all 17,050 frames emit it; none needs more) | `src/wasm.rs` `CX::finish` | 51 KB, certain |
  | 10 | renumber locals by use count in the 61 functions with 128+ locals | `localify.rs` | 56 KB |

  Measured items: 3.2-4.4 MB of the 21.6 MB gap. The rest in a 20-function
  sample is locals LLVM's stackifier sinks or tees (we tee half as
  often), other structural `br`s (748K `br` vs 81K module-wide), and
  more unrolling by our CLIF pipeline (`Memchr2/3` 500 to 2,900 B).

  **Landed 2026-11-16 (#5, #7, #2 default on; #9, #8, #6 opt-in).** dsa
  at HEAD `fad51e94`, main's behaviour against the branch with every
  item on: code 35,337,904 -> 34,218,327 B (-1.12 MB, -3.2%; LLVM
  14,484,892), all 19 workloads return LLVM's results, peepdiff clean
  on 68,923 bodies. By item (decoded counts, non-SIMD bodies):
  #9 `strip_realign` (`PLIRON_WASM_NOREALIGN`), 17k realigns, ~51 KB;
  #5 cause: `wpeep`'s elision deletes an untargeted block whose body
  always branches away and leaves the `br` after its old `end` dead;
  that dead `br` kept an outer block "targeted", and wasm-bindgen's
  re-encoding then dropped the dead code and left the block untargeted
  in the shipped module (the raw link had 11k such blocks, all with a
  `br_table` inside; the package 78k). `wpeep_elide::dead_exact`
  (`PLIRON_WASM_DEADTAIL`) deletes unreachable code ahead of elision:
  78k -> 14k, ~190 KB; #7 `btr` again after elision
  (`PLIRON_WASM_BTR2`): fallthrough `br`s 37k -> 7.8k, ~58 KB; #2
  `wpeep_elide::bob_round` (`PLIRON_WASM_BOB`), done in `wpeep` with a
  stack-depth check rather than in stackify: 99,871 -> 0 sites, ~0.5 MB;
  #8 `one_exit` (`PLIRON_WASM_ONEEXIT=1`): restores 54.7k -> 32.0k,
  ~36 KB; #6 `mask_elim` (`PLIRON_WASM_MASKELIM=1`): masks of a block
  param whose incoming values fit, masks after `local.get` 26.5k ->
  20.3k. SIMD bodies (1,715 functions, 45k untargeted blocks, 15k
  branch-over-branch sites) are untouched: `wpeep` skips them (N3).
  Kernels: 152,336 -> 145,300 B with the three defaults, every kernel
  within noise of main (`--layouts=3`, parse_only -10.8%/-10.7%,
  tokenize -14.4%/-14.2% with `--layouts=5`). #9 stays opt-in:
  `parse_only` moves from -10.3% to -4.4% against LLVM with it (tight
  intervals, and tokenize turns bimodal), while TurboFan's code for
  parse_only is identical but for the dropped `and`
  (`harness/v8/mcode.sh`): a 4-byte shift of the hot loop, code
  alignment rather than work. #8 and #6 are small and touch hot
  kernel bodies; not timed on a quiet machine. dsa with the three
  defaults alone: 35,337,904 -> 34,293,484 B (-1.04 MB), all 19
  workloads equal. Open: `hash_text` reads 0.996 against 1.224 of (Rechecked at merge: alone and alternating the two builds trade places, 77.6/79.7, 83.9/78.8, 83.3/111 ms per call at load 63, and with the harness's load order swapped the slower column moves with it: the row's known position artefact, not this change.)
  LLVM's time (7 rounds, load ~60) although its hot body
  `blake3::compress_in_place` is byte-identical and each side alone
  costs 80.4 against 82.6 ms per call (63% of it in JS
  `passStringToWasm0`); not explained.
- **N6, done 2026-11-17: single-caller inlining is on for wasm**
  (`PLIRON_INLINE_ONCE_WASM=3` with licm's GTE and SLOTPROV rules,
  `28e56c7`). Quiet-machine kernels geomean 0.931 -> 0.929, no kernel
  outside its range (tokenize and parse_only rechecked with 5 layouts
  twice), hmget +5.7% -> +1.9%. dsa: the 14 like-for-like rows 1.030 ->
  0.999 against LLVM, all 19 workloads equal; current main builds the
  module at 41.92 MB (44.09 at the start of the round, LLVM 18.71). The
  analysis that led there:
- **N6 (analysis). hmget's loss under single-caller inlining is two licm refusals**
  (the V8 comparison, notes in the session scratchpad `hmget_notes.md`,
  summarised in `history/2026-11-16-wasm-night.md`). Not alignment: the
  loop head sits at the same offset mod 64 in the fast and the slow
  build. (a) Under `PLIRON_INLINE_ONCE=3` the closure's loop keeps the
  five header loads of the map and the key-only SipHash prefix (26 more
  machine instructions per lookup, +19% against LLVM's +5%): licm
  refuses each with `deref=None -> safe=false`, because `&HashMap` is
  read out of the closure environment and carries no dereferenceable
  fact, and licm (`licm.rs` ~476-510) needs an unconditional
  dereferenceability proof for any load hoist. Either give a pointer
  loaded from a reference-typed place a dereferenceable fact (rustc
  knows the pointee type; the IR has `noalias`/`frozen` facts for
  parameters only) or let licm hoist a load that executes on every
  iteration (header to preheader is not speculation when the header
  dominates every exit and the loop is entered). (b) Under `=1` with
  NCC and SLOTPROV one load (k0) stays because
  `PLIRON_LICM_INNERPRESS=2` caps hoists by a native register-pressure
  estimate; on wasm TurboFan allocates registers. A build with
  `PLIRON_LICM_INNERPRESS=0` has 96 machine instructions per lookup
  against the default's 102 (timing pending). Each fix must keep every
  other kernel (`bench/wasm/run.mjs`) and the default dsa rows.
  **Fixed behind `PLIRON_LICM_GTE=1` (opt-in, 2026-11-16):** licm hoists
  a `notrap` load on the header's straight line (before any branch,
  call or trapping instruction) without a dereferenceability fact: the
  loop's first iteration would make the access anyway. hmget against
  LLVM (load ~50, `--layouts=3`): ONCE=1+NCC+SLOTPROV 10.7% -> 1.1%,
  ONCE=3+SLOTPROV 11.5% -> 2.2%, default 5.5% (unchanged: the default
  module is byte-identical with the knob). In the kernel module only
  `k_hmget` changes under single-caller inlining. The pressure cap off
  on wasm (`PLIRON_LICM_NOPRESS_WASM=1`) gives hmget the same code but
  also changes `reserve_rehash` and `dragon::mul_pow10`; not needed with
  GTE. Without SLOTPROV the SipHash tail-buffer store still clobbers
  (ONCE=1 or 3 alone stay at +19%). Details:
  `history/2026-11-16-licm-gte.md`.
- **N7. The default hmget, +5% against LLVM: analysed.** Our lookup
  miss path is 102 TurboFan instructions against LLVM's 96. `invparam`
  splits the lookup loop into two headers, and TurboFan reloads the
  memory base at both; `PLIRON_INVPARAM=0` gives one header and keeps
  the base in a register (1,872 B of TurboFan code against 2,008) but
  the count stays 102 as another value spills. The other 6: our hash
  expression has 27 xor/27 rotate against 25/25, and `k + (round & 1)`
  is recomputed per lookup where LLVM hoists the parity into one
  `madd`. Not timed (load 60-110 throughout).

Landed 2026-11-16 evening: the `Target` facts struct, the off-switch
sweep (`harness/offswitch.py`, N4), and the IR-pass differential
checker (`clif-check xcorpus`, `harness/xcorpus.py`). The checker
catches the reintroduced `PLIRON_MEMCPYOPT_RDWRAP=0` bug and is quiet
on the kernels+std corpus, but on dsa it still reports 24 mismatches in
a 1-in-4 sample of the `memcpyopt` ablation (12 are a stub artefact in
`<[(FieldKey, Expr)]>::to_vec`, 12 undiagnosed) and one dsa function,
the closure in `cadcore_union::arrange::universal::fuse_filaments_with`,
mismatches against its own pipeline output in the default build and
goes away with `PLIRON_SLOTMERGE=0`: a possible live miscompile.
Until those are cleared, gate further `memcpyopt` work (`noalias`
destinations, sibling-arm writers whose arms together cover every
path into the copy) by comparing mismatch lists before and after.

### Standing list

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
