# Overnight 2026-10-27 → 2026-10-28 09:00 ET

Goal: move the LLVM-vs-pliron parity card (overall 88; MEM 36, SIZ 37 weakest).
Stop at 09:00 America/New_York. Land default-off first; flip only after
fast legs. Full gate / HOTLEAF flip deferred to measurement runs.

## Card baseline (2026-10-27)

| | SPD | CMP | MEM | SIZ | STK | COR | overall |
|---|---|---|---|---|---|---|---|
| defaults | 106 | 107 | 36 | 37 | 87 | 100 | **88** |
| + candidates | 107 | 105 | 59 | 40 | 88 | 100 | **91** |

## Workstreams

1. **MEM flip prep** — LOWERFREE, JEARENA, CGU_ACAP (+ CompactLoc already on)
2. **SIZ** — HOTLEAF/NOIRRED etc. measure on tip; more size wins
3. **SPD** — SELCHAIN/MULCARRY/STEPCMP (+ ELABLOOPS carefully)
4. **COR** — fuzz regress, crates smoke with candidate sets
5. **SIZ/MEM more** — whatever the profiles show next

## Log

Tue Oct 27 22:54:29 EDT 2026
started overnight loop

## Local ctime spot-check (bfef76a, release, runs=2, load noisy)

regex-wasm / syn peak RSS ratios (pliron/LLVM):
- defaults: see ctime-mem-off.json
- LOWERFREE+JEARENA+ACAP=8: see ctime-mem-on.json
(Wall times not comparable across the two runs — LLVM wall swung 3s→6s.)

MEM agent worktree `overnight/mem-defaults` already drafts DEFAULT_ON for
LOWERFREE/JEARENA/LTUNDEF + DEFAULT_VALUES CGU_ACAP=8.

## 23:02 ET — local measurements on bfef76a

### MEM (ctime runs=2, release)
| input | RSS defaults | RSS +LOWERFREE/JEARENA/ACAP=8 | ratio off→on |
|---|---|---|---|
| regex-wasm | 778 MiB (2.93x) | 552 MiB (2.08x) | big win |
| syn | 1836 MiB (2.97x) | 1088 MiB (1.79x) | big win |

### SIZ regex-wasm CODE (cgu=1, release dylib) — confirms prior write-up
| config | CODE | vs LLVM |
|---|---|---|
| default | 1,191,906 | 5.73x |
| GC+NOREC+NOIRRED+REDUCIBLE | 699,200 | 3.36x |
| + HOTLEAF=1 | 451,110 | 2.17x |
| + INLINE_SMALLSCC | 447,128 | 2.15x |
| LLVM | 207,914 | 1.00x |

Agents in flight: MEM flip, SPD selchain, SIZ hotleaf, COR fuzz.

## 23:08 ET — landing MEM defaults
LOWERFREE/JEARENA/LTUNDEF + CGU_ACAP=8 via DEFAULT_ON/DEFAULT_VALUES; lowerfree(+wasm)/wasm_smoke/nostd green.
Landed as `e63e878` on main (parallel MEM agent; same patch shape as
worktree `overnight/mem-defaults`).

## 23:14 ET — SPD defaults
SELCHAIN+MULCARRY+STEPCMP → DEFAULT_ON (parse_only/eqbug digit loop). Fast legs green. ELABLOOPS stays opt-in.

## 23:15 ET — MEM agent independent validation (`overnight/mem-defaults`)

Same flip as `e63e878`, validated before noticing main already had it:

- `cargo build --release` + debug; `cargo test -p pliron-cfg -p pliron-ir-opt
  -p pliron-clif-opt` green; `TEST_NOBUILD=1 ./test.sh --only=wasm_smoke,nostd,std,lowerfree,lowerfree-wasm,spills` green
  (default-on fires `lowerfree … Context released` with no `PLIRON_*` set;
  kill-switch `=0` keeps byte-identical objects).
- `harness/gate.py` (default, not `--full`): **83 jobs passed** in 232 s
  on the dirty worktree tip.
- A/B peak RSS via `ctime.py --only regex-syntax,syn --runs 3` (release
  dylib; ON = defaults including LTUNDEF; OFF =
  `LOWERFREE=0 JEARENA=0 CGU_ACAP=0 LTUNDEF=0`):

| input | OFF | ON | RSS ratio |
|---|---|---|---|
| regex-syntax | 789 MiB (2.77x LLVM) | 462 MiB (1.63x) | ×0.59 |
| syn | 1,723 MiB (2.93x) | 1,067 MiB (1.85x) | ×0.62 |

Two-input RSS geomean 2.85 → 1.74 (≈ MEM rating 25 → 60; full five-input
card B was 59). Crates tiers 1–3 / stage-2 UI still deferred.
Branch `overnight/mem-defaults` tracks main + this log; no second code
commit (already on main).
