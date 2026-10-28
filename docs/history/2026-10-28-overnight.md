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

## 23:18 ET — wasm size defaults

DEFAULT_ON: WASM_NOIRRED/NOREC/GC, REDUCIBLE, INLINE_SMALLSCC;
DEFAULT_VALUES: WASM_HOTLEAF=1.
regex-wasm CODE under plain defaults: **444,300** (2.14x LLVM 207,914).
Was 1,191,906 (5.73x). reducible_dup_off now uses `=0`. Fast legs green.
Fuzzreg + unit tests from agents: pass.
INLINE_CLEAN/FANIN landed default-off as `b2de35e` (iropt cleanup).

## 23:22 ET — INLINE_CLEAN spot-check

`PLIRON_INLINE_CLEAN=1` on syn (1 run): text 1,951,488 → 1,929,224 (−1.1%),
RSS 1,113 → 1,053 MiB; wall 7.1s → 15.6s (noisy/expensive — **leave opt-in**).
FANIN not flipped. Agent smoke with CLEAN+FANIN: wasm_smoke+nostd green.
Fuzz 14200:14500 agent reported success.

## 23:23 ET — gates
MEM-defaults gate: 83/83 pass. Tip gate once failed `debuginfo` (-g text); re-ran twice green (likely mid-rebuild flake). CLEAN+FANIN gate reported green by agent; CLEAN stays opt-in (wall).

## 23:25 ET — SIZ agent independent check (tip after `378026e`)

Re-measured on release dylib; confirms the flip and adds HOTLEAF runtime.

### regex-wasm CODE (cgu=1)
| config | CODE | vs LLVM |
|---|---|---|
| kill-switch all six `=0` | 1,184,130 | 5.70x |
| plain defaults (post-flip) | **444,300** | **2.14x** |
| explicit GC+NOREC+NOIRRED+REDUCIBLE | 699,200 | 3.36x |
| + HOTLEAF=1 | 451,110 | 2.17x |
| + INLINE_SMALLSCC | 447,128 | 2.15x |
| LLVM | 207,914 | 1.00x |

(Plain defaults slightly under the stacked env run: other DEFAULT_ON
passes already in the tip interact.)

### HOTLEAF=1 vs HOTLEAF=0 runtime (`bench/wasm/run.mjs --layouts=3`)
Modules: hotleaf0 183,475 B / hotleaf1 175,159 B. Ratio = on/off
(negative change = HOTLEAF faster). Load ~47 — noisy absolutes, but
median-of-layout ratios consistent:

| kernel | HOTLEAF=1 / HOTLEAF=0 | range |
|---|---|---|
| hashmap | **0.944 (−5.6%)** | −12.7% .. −4.5% |
| tokenize | **0.945 (−5.5%)** | −8.6% .. +1.6% |
| hmget | **0.976 (−2.4%)** | −15.5% .. −0.4% |

All ≤+3% threshold (actually wins). Flipping HOTLEAF with the rest was
correct. `reducible_dup{,_off}` + `wasm_smoke` green on tip.

## 23:24 ET — HOTLEAF wasm runtime A/B (agent)

bench/wasm full suite (noisy box; LLVM baselines swung hard between runs):
- HOTLEAF on:  module 176,580 B, geomean vs LLVM 0.915
- HOTLEAF off: module 185,383 B, geomean vs LLVM 0.812
pliron-only on/off geomean across kernels ~see log; hashmap leaf path OK under ON.
Size win on bench module −8.8 KiB. Keep HOTLEAF default-on.
CLEAN vs FANIN debuginfo bisect: agent reported success (CLEAN stays opt-in).

## 23:28 ET — COR validation (overnight/cor-fixes)

No miscompiles found; no code fix commit. Tip under test moved during the
session as MEM/SPD/SIZ flips landed (`e63e878` → `5ae7b88` → `378026e` →
`dcf8093` / later `a82cdb2`). Candidate set exercised:

`LOWERFREE JEARENA CGU_ACAP=8 LTUNDEF SELCHAIN MULCARRY STEPCMP
WASM_NOIRRED WASM_HOTLEAF` (env-on before flips; DEFAULT_ON after).

| leg | result |
|---|---|
| gate default @ bfef76a (pre-flips) | **86/86 pass** (316 s) |
| gate default @ dcf8093 (all flips) | **86/86 pass** (130 s) |
| fuzzreg @ defaults (gate) | **19/19** |
| fuzzreg + candidate env flags | **19/19** |
| fuzzreg @ tip defaults post-flip | **19/19** |
| fuzz 14000:14200 + flags | **200/200** |
| fuzz 14200:14500 + flags | **300/300** |
| fuzz 14500:14700 tip defaults | **200/200** |
| debuginfo isolated (×2) | **pass** |
| miscompile fixes | **none** |

Spurious `debuginfo: -g changed the code` once under overlapping gates
sharing `target/tests` — not reproduced in isolation (`TEST_OUT` separate).
Residual: no crates.py / stage-2 UI; concurrent gates on one checkout race
`TEST_OUT`; keep watching fuzzreg when more DEFAULT_ON lands.

## 23:28 ET — card-B leftovers default-on

JT_SHAPE, CMP3, NORETTRAP → DEFAULT_ON; EDGEMERGE/EPIMERGE → DEFAULT_VALUES 1;
vendored EDGEDEDUP/NARROWEXT/EDGEMERGE/EPIMERGE default on unless =0.
HOTLEAF wasm A/B: bench module −8.8 KiB; pliron on/off geomean ~0.80 (noisy).
Unit narrowext/noret/jt_shape/cmp3 + stanzas + wasm_smoke green.

### Gating (for the flip)

| flag | was | flip |
|---|---|---|
| JT_SHAPE | `config().is(..., "1")` in `s_brchain` | DEFAULT_ON |
| CMP3 | `Gate::OptIn` | DEFAULT_ON |
| NORETTRAP | `env_is(..., "1")` in lower | DEFAULT_ON |
| EDGEDEDUP | vendored `== Ok("1")` | `!= Ok("0")` |
| NARROWEXT | vendored `== Ok("1")` | `!= Ok("0")` |
| EDGEMERGE / EPIMERGE | vendored `pliron_mode` unset→0 | unset→1 + DEFAULT_VALUES `"1"` |

INLINE_CLEAN not flipped (syn wall). Crates/UI not run (overnight brief).

### Fast validation (`3104d38`)

- `cargo test -p pliron-cfg --lib` green
- `cargo test -p pliron-clif-opt` green (incl. cmp3, jt_shape, noret_trap, narrowext_lower)
- `TEST_NOBUILD=1 ./test.sh --only=wasm_smoke,nostd,std,narrowext,edgemerge,noret,cmp3` green
  (edgemerge now also exercises `=0`; noret kill-switch vs force-on unchanged)

## 23:30 ET — INLINE_FANIN=8 default

lockf ctime matrix (regex-syntax + syn): FANIN=8 alone size 2.04/1.94 → 1.76/1.46,
RSS 1.62/1.81 → 1.60/1.52; wall mixed. CLEAN alone hurts wall — stays opt-in.
Agents: regex-wasm CODE still 444,300 under plain defaults; clif-opt suite green;
fuzzreg + 14000–14700 windows clean; debuginfo isolated green.

## 23:34 ET — REMATLOOP default off (SPD)

Cranelift `maybe_remat_arg` no longer remats into the def's own loop unless
`PLIRON_REMATLOOP=1`. Completes the parse_only digit-loop set with
SELCHAIN/MULCARRY/STEPCMP (slow-kernels: 1.14 → 1.05 standalone). ELABLOOPS
stays opt-in. Worktree overnight/spd-selchain.
