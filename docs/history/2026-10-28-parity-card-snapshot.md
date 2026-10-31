# PARITY.md parity-card snapshot, 2026-10-27 to 2026-10-29

> Moved verbatim from `docs/PARITY.md` on 2026-10-30. Superseded; the current status page is [`../PARITY.md`](../PARITY.md).

**Latest (2026-10-27, docs/history/2026-10-27-parity-card.md): 88
overall on defaults** (SPD 106, CMP 107, MEM 36, SIZ 37, STK 87, COR 100),
**91 with the candidate flags** (MEM 59, SIZ 40). As of 2026-10-28 overnight
(`e63e878` / `5ae7b88` / wasm-size defaults / card-B leftovers):
`LOWERFREE`/`JEARENA`/`LTUNDEF`/`CGU_ACAP=8`, `SELCHAIN`/`MULCARRY`/
`STEPCMP`, the wasm size stack (`NOIRRED`/`NOREC`/`GC`/`REDUCIBLE`/
`HOTLEAF`/`SMALLSCC`), the remaining card-B set (`EDGEDEDUP`/
`JT_SHAPE`/`CMP3`/`NORETTRAP`/`EDGEMERGE=1`/`EPIMERGE=1`/`NARROWEXT`),
and `JT_NARROW` (deferred narrow jump tables) are default-on (kill with
`=0`). Spot-check MEM A/B: peak-RSS geomean 2.85 → 1.74 (~MEM 25 → ~60
on regex-syntax+syn). regex-wasm CODE **~1.78x** LLVM (373 KiB with ELIDEBLK+WIFRET typed-if
results + edge remat + SOLO=15 + TINY=6; was 1.80x/378 KiB; was 1.93x / 401 KiB;
was 5.73x). Native cgu=1: regex-syntax `__text`
−5.6 KiB / syn −8.4 KiB with `JT_NARROW`. Attribution:
docs/history/2026-10-27-wasm-size.md, 2026-10-27-jump-tables.md.

Burn/CubeCL host-build suites (per-suite pliron-only vs stock failures, for
the COR input): [`cubecl-burn-host.md`](../cubecl-burn-host.md); 2026-10-29: 0
pliron-only failures across 8.2k tests after two fixes.
