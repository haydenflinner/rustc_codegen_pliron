# 2026-11-17: a native placement control (`harness/placement.py`)

Two opt-in knobs were stuck on small native losses that might have been
code placement rather than worse code: `PLIRON_WIDE128=1` (pulldown-cmark
+23.3k exact, `parse_block` 43 instructions shorter, 25 fewer alignment
`nop`s) and `PLIRON_UNDEFPHI=1` (a `varint` kernel +0.2% instructions,
about +3% cycles, a loop that lost its pad). This is the native
counterpart of `bench/wasm/run.mjs --perturb=N`
(`2026-11-17-v8-placement.md`).

## The perturbation: `PLIRON_PLACE_PHASE=<bytes>`

Vendored Cranelift, a measurement aid (default 0 = off; a multiple of 4
below 64). Every function starts that many bytes past a 64-byte boundary:
`cranelift-object` puts a pad ahead of the symbol (never executed; the
symbol, its relocations, `.eh_frame` and DWARF are symbol-relative), and
the emitter aligns the loop-head pad (16 bytes), the 64-byte loop rule and
the constants (`MachBuffer::align_to`) on the real address. So the code is
the default's, laid out as if `bytes` of code preceded it. What changes:
how many `nop`s each loop head gets (executed on its fall-through entry)
and every cache-line crossing. What does not: every other instruction.
Checked on Mach-O (ld64) and ELF (`wild`): symbols land at the phase mod
64, loop heads stay 16-byte aligned in memory, and a scratch program's
loop pads go 11 / 25 / 21 `nop`s at phases 0 / 36 / 12.

Copy i of a build uses phase P_i (0, 36, 24, 12, 48, 20, 56, 44: every
residue mod 16 by copy 4); for timing, copy i > 0 is also relinked under
random function order i (ld64 `-order_file`), which moves page and set
bits.

## Exact counts: what moves

callgrind with `--dump-instr=yes`; every executed address is classified
from its encoding (`nop`, jump, conditional branch, call, ret, load,
store, other) and charged to its ELF symbol. Callgrind's own `fn=`
context is not the containing symbol on our code (an address of
pulldown-cmark's `HtmlWriter::run` showed up under `escape_html_scalar`,
its `'2` recursion context and `Parser::next'2`), which gave a first
function table of ±1.2M swaps between unrelated functions. `irdiff.py`'s
function table reads the same `fn=` lines; its source-line table is not
affected.

Base range over 8 copies (exact instructions against the copies' median;
executed `nop`s min..max; spread of the non-`nop` count):

| program | range | nops | non-nop spread |
|---|---|---|---|
| blake3-scalar | -0.03..+0.04% | 22.7k..54.5k | 7 |
| blind-sig | -0.11..+1.40% | 296k..790k | 31.3k |
| pulldown-cmark | -0.45..+0.48% | 385k..873k | 2.4k |
| regex | -0.37..+1.59% | 491k..1.48M | 1.6k |
| rust-compression | -0.03..+0.03% | 12k..47k | 7.7k |
| rust-html-rewriter | -0.25..+0.39% | 402k..1.06M | 83 |
| rust-json | -0.59..+0.62% | 327k..949k | 2.8k |
| rust-protobuf | -0.38..+0.28% | 211k..643k | 2.5k |

So placement alone moves an exact count by up to 1.9% (regex), and all
of it is `nop`s: a 0.05% delta in a program is far inside its placement
range and says nothing until split. The non-`nop` count moves only
through data addresses (a longer `.text` shifts `.rodata`, and musl's
`memcpy`/`memchr` paths and `RandomState` hashing follow); blind-sig's
31k is that. Beyond the base's own spread, a non-`nop` delta is the
candidate's code.

Kernels (macOS instructions per op, `(I(2N) - I(N))/N`): `varint` moves
33.95M..37.54M per op across 4 phases (+10.6%): its inner loop's padded
head is entered by fall-through once per decoded value.
`varint_postcard` moves ±0.02%.

## Cycles

macOS `/usr/bin/time -l`, 7 interleaved rounds per copy. Base cycle
ranges over 8 copies on this run (load 10-30, other agents building):
blake3 -1.3..+0.7%, blind-sig -3.5..+2.8%, pulldown-cmark -3.4..+1.0%,
regex -1.1..+0.8%, compression -0.5..+2.1%, html-rewriter -1.2..+0.4%,
rust-json -17..+2%, protobuf -6.1..+5.6%. Most of that width is load,
not placement; on a quiet machine the earlier `run.py --layouts` figure
(~3%) is the better guide.

## Verdicts

**`PLIRON_WIDE128=1`: stays opt-in; pulldown-cmark's loss is real
code, not placement.** Exact, against the base over 8 copies:

| program | copy-0 delta | of which nop | non-nop (median over copies) |
|---|---|---|---|
| html-rewriter | -371.0k | +6.5k | -377.5k (-0.37%) real win |
| pulldown-cmark | +23.3k | -8.6k | +31.8k (+0.061%) real loss |
| the other six | within ±0.6k | | noise |

pulldown-cmark's +31.9k is all in `FirstPass::parse_block`, by class
load +52.9k, store -7.0k, other -14.1k, nop -8.6k: the 25 fewer pads do
save `nop`s, and the function executes 53k more loads (the allocation
shift the PARITY note suspected; register pressure after the 128-bit
rewrite elsewhere in the function). The same +31.8k in every copy, so
no placement explains it. Cycles: inside the placement range on every
program (pulldown-cmark median -0.45%). Under the no-trading rule it
stays opt-in until `parse_block`'s extra loads are gone; that is a
regalloc question (the regalloc work can check it with
`harness/placement.py heldout pulldown-cmark --cand w:PLIRON_WIDE128=1`).

**`PLIRON_UNDEFPHI=1`: stays opt-in; `varint_postcard`'s loss is real.**
Exact: pulldown-cmark -95.7k (-0.18%, real), html-rewriter -69.2k
(-0.07%, real), the other six noise (regex's earlier +0.8k: -0.2k
median, noise). Kernels: `varint` identical in every phase (the
reported +0.2% on `varint` was placement or a mislabel); `varint_postcard`
+0.53% instructions per op in all four copies (base range ±0.02%), real.
Cycles: KERNELS-TBD.
