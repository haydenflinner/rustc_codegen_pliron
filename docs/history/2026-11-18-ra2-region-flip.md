# 2026-11-18: flip `PLIRON_RA2_REGION=1` and `PLIRON_RA2_SPILLHOIST`

Default-on flip of the 2026-11-17 regalloc2 region-split / spill-hoist
pair (`vendor/regalloc2/PLIRON_PATCH.md`). Held-out exact counts of the
flip (previous default → both): compression −3.14%, pulldown-cmark
−1.04%, regex −0.87%, html-rewriter −0.67%, protobuf −0.54%, rust-json
−0.20%, blake3 +0.012%, blind-sig noise.

## Blockers fixed

The AGENTS.md note that smoke's `o0std` `seqs` failed the checker under
the flip was stale: that failure was only under the still-opt-in
`PLIRON_RA2_REGION_TIE`, and no longer reproduces. The real blockers
were in spillhoist under the default-on `PLIRON_RA2_SRCVREG`:

1. **Block-parameter edge stores.** With SRCVREG, a `reg → param-home`
   edge move is labelled with the branch argument. Spillhoist treated
   those as spill stores of the argument and deleted them. Smoke's
   `neonpair` `main` failed the checker (`IncorrectValuesInAllocation`
   at Inst 128: `p1i` held `v569` instead of `v332`). Fix: mark
   block-parameter edits (`InsertedMove::blockparam` /
   `Edits::blockparam`) and skip them in spillhoist. Stanza `neonpair`
   now builds with `PLIRON_RA_CHECK=1`.

2. **Stack pregs.** A definition allocated to a "stack preg" (the
   fuzzer's memory placeholder) must not become the `from` of a hoisted
   store: that is a stack-to-stack move. Spillhoist now uses the same
   `is_reg` test as `moves.rs`. `rafuzz` seeds 1000000..1001500: 19
   `StackToStackMove` failures under hoist alone → 0.

`slotpromlt`'s XBLOCK / SpillCSE size comparison pins
`PLIRON_RA2_REGION=0 PLIRON_RA2_SPILLHOIST=0` (with the existing
POSTOPT=0 set): with them on, XBLOCK no longer moved the instruction
count (1500 → 1500).

## Checks

`harness/quick.py` green (rafuzz, smoke/std corpora, regionsplit,
o0std, wasm_smoke, …). `harness/gate.py` after the slotpromlt pin.
`clif-check codegen` of smoke under the new defaults: 0 checker
failures.
