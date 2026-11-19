# 2026-11-18: raproof — register allocation as certificates

Assembly-line view of regalloc: values that must be live together need
distinct homes; a colouring proves how many registers suffice. A
profile-hot function plus a proof engine that searches for a cheaper
assignment (or proves none exists under K registers) is how you trade
compile compute for runtime — and how you learn, from the certificates,
what the heuristic left on the table.

## Landed (MVP)

- `vendor/regalloc2/src/prove.rs`: build the interference graph from
  bitset liveness; max-clique lower bound (Bron–Kerbosch ≤ `MAX_EXACT=48`,
  greedy beyond); binary-search chromatic number when exact; compare to
  distinct-preg footprint of an allocation (`preg_pressure` /
  `verdict`).
- `tools/raproof`: fuzz-seed CLI (`--start` / `--count` / `--replay` /
  `--algo ion|greedy|fast`). Own cargo workspace so it stays light under
  low RAM (no backend dylib).

## First 30 seeds (ion)

`cargo run --release --manifest-path tools/raproof/Cargo.toml -- --count 30`

- reported=29, ~0.6 s
- `heuristic_above_optimum`: 23 class rows — ion's preg footprint >
  chromatic number on a certificate that names the clique and K
- `match`: 5
- `gap_above_clique_unresolved`: 40 (classes > 48 verts; clique only)
- spills-under-* rows expected when the allocator spills

Footprint ≠ simultaneous pressure (spilling drops footprint below the
clique; FixedReg/ABI pins and edits inflate it). The useful signal is
`heuristic_above_optimum` with an exact colouring: a proof-guided
search has a concrete target.

## Certificate instrumentation + try-fewer (same day)

- `check_clique` / `check_colouring` / `check_bound`: independent
  verifiers — raproof re-checks every bound before trusting it
  (`bad_cert` counter).
- `try_fewer(g, class, heuristic_pregs, budget)`: walks heuristic−1
  down to the clique looking for a colouring. A hit is the constructive
  "you can do this with fewer registers"; `tight` when K−1 is also
  proven uncolourable (or K equals the clique). `--prove-fewer` /
  `--budget N` on the CLI is the trade-compute dial.

```
cargo run --release --manifest-path tools/raproof/Cargo.toml -- \
  --prove-fewer --count 20
```

## What the signal teaches

Ion often **sprinkles** live values across more physical registers than
a chromatic colouring needs. That is footprint waste on the
interference abstraction — not a proof of extra spills, and not yet a
held-out cycle win. The constructive fix is to *apply* the colouring
(or prove K−1 is impossible). Spills-under-* means the allocator
already compressed via stack; leave those alone.

## Learning loop

`harness/raproof_learn.py` is the fitness oracle (light: raproof only;
`--rafuzz-quick` adds checker). A background agent closes the loop by
adding opt-in proof colouring and iterating on
`target/raproof/learn.jsonl` — report in
`docs/history/2026-11-18-ra-raproof-learn.md` when done.

## Next

1. Cost model: spill/reload/move weight, not just preg count — so a
   "better" colouring is one that wins held-out non-nop, not just K.
2. Raise `MAX_EXACT` / SAT encoding for the unresolved gap; profile-hot
   functions from `rastat.py` instead of only fuzz seeds.
3. ~~Feed certificates back into greedy/ion~~ — landed as opt-in
   `PLIRON_RA_PROOFCOLOR=1` on greedy+COLOR; see
   `2026-11-18-ra-raproof-learn.md`.
4. Profile-guided "heat": weight the search by block frequency so the
   engine optimizes tacos/sec on the hot path, not uniform K.
