# 2026-11-17: what moves a wasm kernel under V8 when our code does not

Two changes were blocked by kernel timings that moved with nothing in
the hot code changing: ICF (`PLIRON_WASM_ICF=1`, N8) made `fib` 2x
slower, and `PLIRON_WASM_NOREALIGN` (N9 #9) cost `parse_only` 6 points.
They are two different V8/CPU effects. node 26.12.1 (V8
14.6.202.34-node.40), arm64 (Apple M-series), kernel module of main
`0ec4db1`. Scripts in the session scratchpad `v8p/` (`tf.py`: a
function's TurboFan code start and backward-branch targets from
`--print-wasm-code`; `pad.py`: N one-instruction stores at a function's
entry; `addfn.py`: N unused functions appended; `vtime.mjs`: one kernel
across module variants, interleaved, fresh salted instances).

## How V8 places wasm code

- Every code object (Liftoff or TurboFan) is 32-byte aligned: bodies are
  padded to a multiple of 32 (`Body (size = 320 = 296 + 24 padding)`) and
  start addresses are 0 mod 32. They are allocated in compile order:
  `fib`'s TurboFan code sits right after its own Liftoff code, and a
  function's start moves when other functions compile before it
  (`--layouts` in `bench/wasm/run.mjs` uses that).
- There is no loop-alignment option for arm64 (`node --v8-options` lists
  none; `--turbo-loop-*` are peeling/rotation/variable). TurboFan does
  not align loop headers inside a function, so a loop's address mod 32
  is fixed by the instructions ahead of it in the same function.

## `fib` and ICF: TurboFan's inlining budget, not placement

`fib`'s wasm body is byte-identical with and without ICF, but its
TurboFan code is not: 70 instructions without, 45 with.
`--trace-wasm-inlining` shows why:

    base: budget=min(146, 671), size 72->144 decided to inline!
    icf:  budget=min(144, 600), size 72->144 not enough inlining budget

`fib` inlines one level of its own recursion only when the budget
exceeds 144 nodes. The budget depends on module-wide statistics that
grow with the number of small functions: appending unused 3-byte
functions to the ICF module raises it (0: 144, 1: 144, 2: 145, 3: 149,
5: 158, 10: 178, 20: 216) and `fib` inlines from two extra functions on.
Timings follow the decision exactly (5 interleaved rounds, load ~6):
base 6.0 ms, base+2/+5 functions 6.0, icf 12.8, icf+1 12.8, icf+2/+3/+5
6.0. ICF removes small duplicates, so it lowers that statistic on
purpose; `fib` happens to sit on the threshold. The function count is
not the whole input: a one-store pad at every function's entry (larger
bodies) drops the base module's budget to 160 against a 79-byte callee
and `fib` stops inlining there too. Over the 8-copy grid below base
inlines in 7 copies, ICF in 5.

## `parse_only` and NOREALIGN: loop parity, period 8 bytes

NOREALIGN drops one instruction (the frame's `& -16`) at `parse_only`'s
entry; the rest of its TurboFan code is identical (488 vs 489
instructions, same start address mod 4096, every loop head 4 bytes
earlier). Inserting 0..15 one-instruction stores at the entry (each a
4-byte shift of everything after it) gives (3 interleaved rounds,
load ~7):

| stores | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| ms | 243 | 260 | 243 | 259 | 243 | 259 | 243 | 260 | 243 | 259 | 243 | 265 | 238 | 259 | 243 | 268 |

Even shifts are fast, odd are slow; over 16 shifts (64 bytes, every
alignment mod 16/32/64) nothing else matters. NOREALIGN is an odd shift:
253 ms in the same run. The hot loop's instructions are mostly
`ldr/ldrb` + `cmp` + `b.cond` pairs; which pair splits across an
8-byte boundary changes with the parity, which points at the core's
fetch/decode or cmp+branch fusion granularity, not at the branch target
alone. The mechanism was not isolated further.

## What `--layouts` covers

`--layouts=N` changes tier-up order, so a function's start moves in
32-byte steps: that covers 32/64-byte effects (chars_alpha's ±25%)
but cannot change a loop's address mod 8, and it never changes the
module, so V8's inlining budget is the same in every layout. Both
effects above showed up unchanged at `--layouts=3` (fib +99% [97..100],
parse_only -10.8% vs -4.8%).

## What we can do on our side

- Choose TurboFan's loop alignment: no. V8 exposes no option and aligns
  nothing inside a function; padding our wasm to land on a good parity
  would depend on V8's instruction selection for every instruction
  ahead of the loop, per version and per CPU.
- Order functions: does not help. Function starts are already 32-byte
  aligned, and the parity effect is inside one function; the inlining
  budget comes from module statistics, not order.
- Avoid changing V8's inlining statistics: not a goal; it would forbid
  every size change.

So: accept both as properties of V8 on this CPU and judge code changes
over them. `bench/wasm/run.mjs --perturb=N` (N up to 8) times every
kernel also in copies of both modules (LLVM's too) with 3, 8 or 16
unused functions appended and/or one store at every function entry
(a 4-byte shift of every loop), checks each copy's result, and reports
the median ratio and the min..max range over all copies. With
`--perturb=8` (load ~10):

| kernel | base | candidate |
|---|---|---|
| fib (ICF) | -7.5% [-22.4..95.4] | -4.5% [-22.4..99.2] |
| parse_only (NOREALIGN) | -4.6% [-10.9..0.2] | -4.9% [-10.5..1.7] |

Both changes are neutral on these kernels once placement is averaged
out; ICF's residual on `fib` is the 7-vs-5 copies that inline. The
whole suite at `--perturb=8` shows which kernels are placement-bound
(range over copies): chars_ascii [-53..-6], parse_only [-11..+17],
sortw [-13..+14], sieve [-16..+3], fib [-22..+94]; iterw, matmul,
hmget, hashmap stay within 2 points. Rule: judge a change that only
moves code with `--perturb=4` or more on the kernels it touches; a
kernel loss inside the base's own range is placement, not the change.
