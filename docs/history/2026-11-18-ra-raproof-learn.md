# 2026-11-18: raproof learning loop — `PLIRON_RA_PROOFCOLOR`

Close the loop from proof-engine `heuristic_above_optimum` feedback
into greedy register allocation. Oracle:
`harness/raproof_learn.py --count 40 --start 0` →
`fitness = match / (match + above_opt + 1)`.

## Baseline vs best (from `target/raproof/learn.jsonl`)

| tag | algo / env | match | above_opt | fitness |
|---|---|---|---|---|
| `baseline-ion` | ion | 5 | 32 | **0.1316** |
| `baseline-greedy-color` | greedy `COLOR=1` | 2 | 7 | **0.2000** |
| `proofcolor-v1` | + `PROOFCOLOR=1` (homes only) | 2 | 7 | 0.2000 |
| `proofcolor-v2-prefs` / `proofcolor-v2-restored` | + temp prefs | 3 | 6 | **0.3000** |
| `proofcolor-v4-align` | always-rematch (reverted) | 2 | 6 | 0.2222 |

Best: greedy `COLOR=1` + `PROOFCOLOR=1` at **0.3000** (+0.10 vs COLOR
alone, +0.17 vs ion) on the fixed 40-seed window. `bad_cert` stayed 0.

## What changed

Knob: **`PLIRON_RA_PROOFCOLOR=1`** (default off; needs
`PLIRON_RA_GREEDY_COLOR=1`).

In `vendor/regalloc2/src/greedy/mod.rs`:

1. After the per-block linear scan, `proof_recolor` builds
   `interfere_graph`, runs `prove_class` per class (≤ `MAX_EXACT=48`),
   and `check_colouring`s the certificate.
2. Maps abstract colours → allocatable PRegs (FixedReg pins hard;
   clobber / foreign-FixedReg refuse a home).
3. Rematches single-block non-param homes only when home footprint >
   `upper_k` and the rematch strictly drops footprint; params /
   multi-block stay spilled.
4. Hands emit a preferred temp set of those K pregs so reload/spill
   edits try to stay inside the chromatic footprint.

Also: `harness/rafuzz.py` mixes the knob; notes in
`docs/regalloc.md`, `vendor/regalloc2/PLIRON_PATCH.md`, `AGENTS.md`.

## Rafuzz

Green under `PLIRON_RA_ALGO=greedy COLOR=1 PROOFCOLOR=1`:

- 500 seeds, 0 failures
- 2000 seeds, 0 failures (checked twice across the v2 / restored
  builds)

(An earlier `--rafuzz-quick` via the learn harness hit SIGTERM while
compiling regalloc2 under memory pressure; serial
`CARGO_BUILD_JOBS=1` builds are fine.)

## What the signal taught

**Did translate.** Most of the COLOR-alone `above_opt` gap was not
"homes use too many colours" but **edit temps picking a preg outside
the chromatic set**. Preferring the proof's K pregs in `pick_temp`
closed one class on seed 18 (Vector 15 → 14) and lifted match 2 → 3.

Home-only rematch (v1) moved nothing: for the small exact classes,
linear-scan home footprint was already ≤ chromatic; the extra preg
appeared at emit.

**Did not translate (wall after ≥5 attempts).** Remaining six
`above_opt` rows (seeds 8/10/15/18-Int/22, …) need a temp while all K
proof homes are live in `forbidden`. The interference-graph
abstraction ignores FixedReg/reuse throwaway temps, so footprint >
chromatic is sometimes forced. Tried and rejected:

1. Homes-only rematch — no fitness change
2. Temp prefs (kept) — +0.10 fitness
3. Dead-home-before-fallback ordering — neutral
4. Always rematch + soft-pin homes onto colours — fitness drop (locked
   the fat set; soft-pin also made `map_colours` abort and skip prefs)
5. Align prefs to actual homes after revert — same regression

Ion was not hooked: its baseline fitness (0.13) is dominated by many
`above_opt` on large classes; a greedy-shaped emit prefs path does not
drop in cleanly.

## Next concrete step

When a prefer-set temp is impossible (all K forbidden), **evacuate**
one live proof home to stack for the edit's lifetime and reload after
— keep footprint at K. That is the emit-side analogue of ion eviction.
Gate it behind `PLIRON_RA_PROOFCOLOR` (or a sub-knob), measure the
40-seed window + rafuzz 2k before touching held-out.
