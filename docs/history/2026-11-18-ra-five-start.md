# 2026-11-18 — five regalloc workstreams started

Plan: instrument, gap vs inline, register-file facts, ion use-cluster
split, greedy+SplitKit competitor. See `docs/regalloc.md` "Five
workstreams".

## Landed (this commit batch)

1. **Stats** — `Output::stats` adds `splits_minimal`, `splits_use_cluster`,
   `splits_region`. `PLIRON_RA_STATS=1` (or `RA_EXPLAIN`) prints
   `ra2stats:` after allocate.
2. **Gap** — `harness/rastat.py dyn` classifies excess as `both` vs
   `inline_only` and prints both-only overhead ratio.
3. **Regs** — `PLIRON_X18=1` opt-in (Linux default recorded but not
   flipped: first on-by-default attempt stack-overflowed blake3 under
   callgrind); `Target::int_regs_alloc` tracks X17/X18/X30.
4. **USESPLIT** — flipped default-on (GAP=128, single-block); see below.
5. **Greedy** — `Algorithm::Greedy` + `PLIRON_RA_ALGO=greedy`: large-first
   colouring with eviction; reload/spill edits for stack homes that need
   a register. Opt-in only.

## Next

- Greedy: all-spill + edits checker-clean (20k seeds). FixedReg-first
  operand order was the last IncorrectValues fix. Default greedy is
  all-spill; `PLIRON_RA_GREEDY_COLOR=1` for broken colouring. Back in
  `rafuzz.py` mix. Next: make colouring checker-clean, then held-out.
- A/B Linux x18 (automatic on musl held-out) vs `PLIRON_X18=0`.

## Flipped: `PLIRON_RA2_USESPLIT` default on (GAP=128, single-block)

| program | GAP=8 1-block | GAP=128 1-block |
|---|---|---|
| blake3-scalar | −154k | −154k |
| blind-sig | **+47k** | **−41k** |
| pulldown-cmark | 0 | 0 (multiblock was +89k) |
| regex / html | ~0 | — |
| protobuf / json | small wins | — |

Gates that make it safe: single-block only; gap ≥ 128. `=0` off.

Verify after release rebuild (2026-11-18 16:02, placement copies=2,
cand=`usesplit0:PLIRON_RA2_USESPLIT=0` vs new default): blake3 OFF is
+154k non-nop REAL loss; pulldown 0; blind-sig median −16k within base
spread (noise).
