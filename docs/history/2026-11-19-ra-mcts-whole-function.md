# MCTS register allocator, whole-function homes (2026-11-19)

`PLIRON_RA_ALGO=mcts` (opt-in) colours each vreg with one home (a preg or
a stack slot) for its whole life, by a Briggs/Chaitin simplify order, an
MCTS/greedy rollout search (`mcts_ra.rs`) and a local-search `refine`;
`greedy/mod.rs` then emits operand allocs, temps and edge moves. A function
whose colouring spills more than `PLIRON_RA_MCTS_FALLBACK` (50) weighted
cost, or that runs out of temps after the reserve retries, is delegated to
ion (`run_mcts` returns `None`).

## What changed this round

- `build_interfere`: exact backward walk (per-def edges, def/use sharing
  rule; `PLIRON_RA_SHARE_DEF_USE=0` off). The earlier forward walk made a
  near-clique (tokenize 5.4x).
- Reserve-and-retry for emit temps (`PLIRON_RA_MCTS_RESERVE`, default 1;
  retries 1, 3, 6 more) instead of a whole-function all-spill.
- `renumber_slots`: only used slots take frame space.
- Reuse/Early/Late sharing rules, `reuse_def_home` for tied uses live after.
- `build_liveness`: word-wise reverse-order dataflow (10.3 s -> <1 ms on an
  11k-inst function). `Problem::best`: the greedy rollout step without
  building and sorting an action list.
- `PLIRON_RA_TIME=1` prints per-phase times for slow functions (`ra2time:`).

## Numbers

Correctness: `clif-check codegen` with `PLIRON_RA_CHECK=1
PLIRON_RELOADPRE_CHECK=1` clean on smoke (every 1), std (every 1; the 8
failures are the same unsupported-CLIF rows under ion), clcg (every 16);
rafuzz 1500 seeds greedy and mcts, 0 failures; regalloc2 lib tests 23/23.

Kernels (`/tmp/kall.sh`, geomean vs stock): ion 0.930, mcts 0.932. Only
tokenize (1.03 vs 0.99) is worse.

Held-out (`bench/heldout/ab.py PROG pliron mcts:PLIRON_RA_ALGO=mcts`),
retired instructions mcts/default: pulldown 1.001, blake3 1.000, blind-sig
1.000, regex 1.001, compression 1.001, html-rewriter 1.000, json 0.999,
protobuf 0.999. Times within noise (0.93..1.007).

Compile time (`clif-check codegen` over the std corpus, user): ion 23.3 s,
mcts 28.8 s (was 41 s before the liveness and rollout fixes).

Smoke weighted code: instructions 13.99M vs ion 13.97M; raw movs 34.7k vs
25k, stack ops about equal. `PLIRON_RA_MCTS_SIMS=1` gives nearly the same
quality as 256: the tree search does not pay, the graph/emit work dominates.

## Follow-up: read-only operands at clobbers

`build_forbid` barred every vreg present at a call from every clobbered
register, including an argument that is only read there. The call reads it
before it clobbers, so an argument can be homed in the register it is
passed in (`PLIRON_RA_FORBID_USEONLY=0` restores the old rule). Smoke movs
34.6k -> 29.9k, insts 288.2k -> 280.4k; std weighted insts 1.0048x -> 1.0012x
of ion; checker clean (smoke, std, clcg), rafuzz 3000 seeds. Held-out
instruction counts do not move (+-0.2%).

Tried and dropped: letting a plain copy `d = a` share `a`'s register while
`a` stays live. Sound, but the vendored checker has no `is_move`, so it
reports the shared register as holding only `d`; not worth patching it.

## Search budget

`PLIRON_RA_MCTS_SIMS` default 256 -> 64. std corpus, user time / weighted
insts: ion 23.3 s / 6.4831M; sims 1 23.8 s / 6.5198M; 4 23.9 s / 6.4984M;
16 24.2 s / 6.4966M; 64 25.3 s / 6.4921M; 256 29.3 s / 6.4906M.

## Verdict

Parity, not a win. Whole-function homes cannot split live ranges, which is
where ion's region split gains; ~+25% allocator time. Stays opt-in.
Next if continued: live-range splitting inside the colourer, or cut the
default search budget.

## Knobs (all opt-in/debug)

`PLIRON_RA_ALGO_FNS=lo..hi` (function-hash mille; `harness/ra_colorbisect.py`),
`PLIRON_RA_MCTS_WORK`, `_RESERVE`, `_FALLBACK`, `PLIRON_RA_SIMPLIFY=0`,
`PLIRON_RA_SHARE_DEF_USE=0`, `PLIRON_RA_FALLBACK_LOG`, `PLIRON_RA_TIME`,
`PLIRON_RA_GREEDY_DUMP`, `PLIRON_RA_BT`.
