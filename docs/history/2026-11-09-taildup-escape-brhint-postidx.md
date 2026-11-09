# taildup escape-repair, brhint mask fix, postidx dominator relaxation

Continuation of the `chars_*` instruction-bloat work from
`2026-11-09-esplit.md` and `2026-11-09-hqzopfli-insn-bloat.md`.

## Measurements (bench/run.sh chars, 2s × 6)

| kernel | LLVM | pliron before | pliron now | +TAILDUP4+BRIF+ESCAPE+EDGE |
|---|---|---|---|---|
| chars_alpha | 1.517 ms | 3.393 ms | 3.258 ms (2.15x) | 2.43 ms (1.60x) |
| chars_ascii | 1.567 ms | 2.552 ms | 1.658 ms (1.06x) | 1.66 ms (flat) |
| chars_mixed | 11.150 ms | 11.992 ms | 11.640 ms (1.04x) | flat |

`esplit` (default-on) fixed `chars_ascii`: the egraph2 merge block's
`icmp ult c, 128` folds to `iadd` on the ASCII edge — 7 insns/char,
beat LLVM's 9. `chars_alpha` is the residual 2.15x: its
`is_alphabetic` merge tail ends in `brif` (`orr;sub;iadd;icmp;b.hs`),
which `esplit` doesn't cover (jump-terminated blocks only). Enabling
the taildup clones recovers a third of the gap; left opt-in because
the same re-duplication measured −2x on the standalone ASCII kernel.

## What landed (all opt-in unless noted)

- **`brhint` masked-test fix (default-on):** the "x==0 arm is unlikely"
  heuristic no longer applies when `x` is a `band`/`bor`/`bxor` mask.
  `band(c,0x80)==0` is UTF-8's ASCII test — the *common* case — and
  hinting it scattered the specialised clone, worth ~2x on the decode
  loop alone.
- **`taildup` escape repair (`PLIRON_TAILDUP_ESCAPE`):** candidates
  whose params/results are used in successor bodies can now be cloned —
  the use-blocks are param'd and every in-edge threads the binding
  (`repair_plan`/`apply_repair`, `MAX_ESCAPE_VALS=4`,
  `MAX_ESCAPE_BLOCKS=8`, `MAX_REPAIRS=16`). Cloning then composes with
  the existing arg mapping so each copy supplies its own analogs.
- **`PLIRON_TAILDUP4` step:** fixpoint taildup (≤4 rounds) after
  `egraph2`, whose GVN creates the shared check tails the fix undoes.
- **Correctness fixes under mutation:**
  - `repair_plan` rebuilds the CFG (earlier clones retargeted/removed
    the terminators the plan records — stale `Inst` derefs panicked).
  - `use_sites` scans the whole layout, not the stale domtree (clone
    blocks minted mid-run were invisible → un-param'd uses).
  - `uses`/`sites` are updated for every value a clone imports as a
    free value, so a later candidate's `local_values` sees them; a
    `local_values` failure on an already-repaired candidate now keeps
    the pred (the new site has no binding on the clone's path).
  - `clone_imports`: a clone copying edges that earlier clones
    retargeted onto clone blocks must keep the new edge under those
    clones' free-value defs.
  - `pass_enabled` → `env_is` for the three suboptions (they were
    accidentally default-on).
  - The latch-edge compare flip clones the `icmp` before complementing
    it — a cloned brif can share the compare, and the in-place flip
    inverted its polarity (rustlantis seed1 loop-forever).
  - `iconst` no longer counts against `MAX_INSTS` (freely
    rematerialisable; late hoisted constants made small check tails
    look "too big").
- **post-index dominator relaxation (vendored Cranelift, PATCH
  entry 115):** `try_postindex` no longer rejects a candidate when
  `base`'s only other-block use is in a dominator that binds `base`
  (header param consumed by the header's own exit test — the canonical
  `ldrb [ptr]; ptr' = ptr+1` decode loop). `ldrb w1,[x23],#1` now fires
  on chars_alpha.

## Validation

- `cargo test -p pliron-clif-opt` — 141 + filetests green, incl. the
  new `taildup_escape.clif` (fails without the repair) and
  `flip_leaves_cloned_brif_polarity`.
- Corpus `target/corpus/std` with all four knobs: 4870 fns, 0 panics /
  verifier failures (4198 ok + 672 interpreter-inconclusive — baseline
  identical).
- `harness/quick.py` 15/15, `harness/gate.py` 94/94.

## Residual on chars_alpha (1.60x with knobs)

- `tbz` sends the ASCII arm *away* to the clone (LLVM's `tbnz` keeps
  ASCII fall-through) — a placement/branch-direction call, not a
  `brhint` one now.
- A `cbz` null check LLVM elides (NonNull knowledge).
- `mov` count++ on the alpha edge (post-alloc copy residue).
- The `tbz`-taken hop + `cbz` + `mov` ≈ the remaining ~60%.
