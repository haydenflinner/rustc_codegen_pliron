# Native runtime status, 2026-10-27

> Moved verbatim from `docs/PARITY.md` on 2026-10-30. Superseded; the current status page is [`../PARITY.md`](../PARITY.md).

## 3. Native runtime

| kernel | pliron/LLVM | lead |
|---|---|---|
| eqbug | 1.22 → **1.09** with `SELCHAIN` (2026-10-27-slow-kernels.md) | the rest is five `uxtb`/`uxth` on narrow `black_box` results; LLVM's stack round trip (`*m` asm) was built and measured slower here, so the fix would be compare lowering (hoisted constant + `cmp wk, w, uxtb`) |
| parse_only | **~1.05** standalone with defaults (SELCHAIN/MULCARRY/STEPCMP + REMATLOOP off) (2026-10-27-slow-kernels.md) | digit loop is LLVM's 7 instructions; `ELABLOOPS` stays opt-in (strwork +4–6% pressure) |
| nbody | 1.04 | flip-flops between runs; earlier: an invariant's spill pair per iteration (regalloc pressure) |
| sieve | 1.02 | marking loop is instruction-tight; residual is elsewhere |
| chars_ascii 0.57, tokenize 0.66, iterw 0.70, contains_rng 0.78, hashmap 0.88, sortw 0.90, fib 0.95 | | ahead |

**`PLIRON_AMODESHARE` — default on** (aarch64 amode rules keep a
multi-use `iadd` base whole instead of reg-reg decomposition;
`vendor/cranelift-codegen` patches #55–56; `PLIRON_AMODESHARE=0`
disables). Strictly fewer insts where it fires, but unbounded it was
a wash: sieve −6%, sortw +5%, hashmap +2.6%. Use-shape
discriminators (non-amode use, fanout ≤3) each kept or lost
everything together — wins and losses are in the same class. What
finally discriminated was a **block live-in pressure proxy**
(cross-block arg uses per block, rolled up per value): gate
`amode_shareable` = non-amode use OR (fanout ≤3 AND
`PLIRON_AMODESHARE_PRESS` ≤5). Flipped on at PRESS=5 (`5ad49ed`):
paired medians give sieve −5.5% / parse_only −3%, and the sortw /
hashmap hot functions stay byte-identical to the off state.

`PLIRON_REASSOC` (opt-in CLIF pass) reassociates `((r+a)+b)+c`
accumulator chains into `r+((a+b)+c)` when `r` is a block param — one
add on the loop-carried edge instead of the whole chain (LLVM's isel
does this free). On eqbug it produced the intended depth-1 asm but
only ~0.8% — the serial chain was already hidden in the loop's ILP
shadow; eqbug's ~10% residual vs LLVM is elsewhere (likely
per-comparison `uxtb` before `cmp…,uxtb` folds and call-boundary
extends). Wash on matmul/nbody/fib. Kept opt-in.

x86-64 numbers are older (Windows box, geomean 1.028); re-measure there
before working on x64-specific gaps.
