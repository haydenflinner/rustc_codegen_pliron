# 2026-11-17: demanded bits (`PLIRON_DEMANDED=1`) and `PLIRON_LUCARRY=1`

Both opt-in. `pliron-clif-opt/src/demanded.rs`, step `demanded` right
after `egraph2`; `vendor/cranelift-codegen/src/context.rs`
`lastuse_schedule`.

## What the residual extends were

rust-html-rewriter's benchmark binary (aarch64-linux-musl, the
`irdiff.py` build) had 637 standalone `uxtb`/`uxth` and 217
`and w, w, #0xff`/`#0xffff`; LLVM's has 2 and 519. So LLVM does extend:
it spells it `and`. The zero-extension gap is 854 against 521, not 637
against 0. Classifying ours by the instruction that reads the extended
register: compares against an immediate 172, `madd` 94, `add` 100,
`lsr` 36, `ucvtf` 21 — consumers that read every bit, where LLVM has its
`and`. What demanded bits can remove is the rest: the `uxtb` the
lowering materializes for every `ireduce.i8`/`i16` (patch 36) when the
narrow value only feeds narrow arithmetic that is extended again or
stored narrow, and root extends whose high bits nobody reads.

## The step

A backward least fixpoint per function (masks only grow, worklist,
budget `32n + 4096`, bail = no change), block parameters across edges.
Rewrites: a constant `band`/`bor`/`bxor` on undemanded bits drops;
`uextend`/`sextend (ireduce y)` with undemanded high bits reads `y`; and
narrow webs are rebuilt wide. A web is a set of `i8`/`i16` values
(results of `iadd`/`isub`/`imul`/`ineg`/`band`/`bor`/`bxor`/`bnot`,
`ishl` by a constant, `select`, and block parameters) whose every use is
another member or a root (`uextend`/`sextend` to 32 bits or more, the
latter only with undemanded sign bits; a narrow `store`), found as a
greatest fixpoint. Each web is rebuilt at one type when a per-target
count says it pays: on aarch64 the materialized `ireduce` `uxtb`s and
root extends it removes against the `uextend`s of dirty leaves and the
`band` a demanded root keeps (a root `uextend` read once by an
`add`/`sub`/`icmp` is free there, it rides the extended-register
operand); on wasm (`Target::narrow_masked`) also the `& 255` after
every narrow add/sub/mul/shift, and the web is rebuilt at the word.

It stays at the CLIF level. The ISLE alternative — `ireduce` as a
passthrough when no consumer reads its high bits — needs every
cleanliness oracle (`val_already_extended`, `narrow_uext_clean`,
`wide_uext_clean`, the blockparam sweep) to agree on which `ireduce`s
are dirty; the CLIF rewrite keeps the lowering's invariants as they are.

## `PLIRON_LUCARRY`

regex got 0.26% worse with the step alone: its varint writer
(`data.push((n as u8) | 0x80); n >>= 7`) computes the byte as
`bor.i32 n, 128` after widening, and `lastuse_schedule` (an x64
two-address heuristic that also runs on aarch64) moved `n >> 7` and its
compare above the `orr` to make the `orr` the last use of the constant
128. `n` and `n >> 7` then overlapped and the loop paid `mov` + `b` per
byte. With `PLIRON_LUCARRY=1` the pass refuses a move whose set computes
an edge argument into a parameter the anchoring op reads.

## Numbers

Exact executed instructions (callgrind, `irdiff.py`), one build of the
backend, knobs per column. blind-sig reads `getrandom`: its count moves
by about ±30k between runs of one binary (base: 34,753,336 /
34,721,444 / 34,742,928; DEMANDED+LUCARRY: 34,482,414 / 34,483,968 /
34,449,633); the others repeat to within ~50.

| program | base | `DEMANDED=1` | `LUCARRY=1` | both | both vs base |
|---|---:|---:|---:|---:|---:|
| blake3-scalar | 49,040,376 | 49,031,230 | 49,040,378 | 49,031,230 | -9,146 (-0.02%) |
| blind-sig | 34,753,336 | 34,692,255 | 34,466,603 | 34,482,414 | -270,922 (-0.78%) |
| pulldown-cmark | 51,914,146 | 51,797,926 | 51,914,153 | 51,797,919 | -116,227 (-0.22%) |
| regex | 50,528,401 | 50,661,076 | 50,437,877 | 50,354,422 | -173,979 (-0.34%) |
| rust-compression | 44,095,655 | 44,081,270 | 44,075,523 | 44,061,132 | -34,523 (-0.08%) |
| rust-html-rewriter | 101,452,399 | 101,391,044 | 101,392,823 | 101,331,440 | -120,959 (-0.12%) |
| rust-json | 51,478,471 | 51,465,563 | 51,478,495 | 51,465,839 | -12,632 (-0.02%) |
| rust-protobuf | 65,264,097 | 65,240,573 | 65,245,599 | 65,225,036 | -39,061 (-0.06%) |

Standalone `uxtb`/`uxth` in the binaries (base -> `DEMANDED=1`; the
`and #0xff`/`#0xffff` count in brackets where it moved; LLVM in the
last column):

| program | uxt base | uxt demanded | LLVM uxt / and |
|---|---:|---:|---:|
| blake3-scalar | 9 | 4 | 1 / 207 |
| blind-sig | 103 | 99 | 1 / 271 |
| pulldown-cmark | 160 | 154 (and 232 -> 236) | 1 / 313 |
| regex | 507 | 482 | 1 / 293 |
| rust-compression | 1,805 | 1,497 (and 250 -> 243) | 1 / 353 |
| rust-html-rewriter | 637 | 542 | 2 / 519 |
| rust-json | 70 | 57 | 1 / 230 |
| rust-protobuf | 28 | 8 | 1 / 245 |

html CLIF corpus (`clif-check codegen`, which now also counts `ext`):
2,214 -> 2,033 standalone narrow extensions, 517,423 -> 516,908
instructions.

wasm kernel module: 159,493 -> 159,049 bytes (-444); 20 functions
change, every one smaller (-238 instructions, -147 `i32.and`: grisu/
dragon formatting, `pad_formatted_parts`, dlmalloc, hashbrown
`reserve_rehash`, `k_iterw`, `k_contains_rng`, `k_hashmap`, `k_hmget`).
With `PLIRON_WASM_MASKELIM=1`: 159,199 -> 158,749 (-450), so the two
do not overlap: MASKELIM drops a mask whose input already fits (known
bits), this drops work whose output bits nobody reads.

Corpus replays, `clif-check report --check` with `PLIRON_VERIFY=full`,
knob off against on, per-function verdicts identical: html 15,352, std
4,925, clcg every 8th 4,756, wasm-bench 2,662, wasm-dsa-eval every 8th
8,685.
