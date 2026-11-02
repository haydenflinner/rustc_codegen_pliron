# Loop-only unswitching, select branch facts, pressure-aware INNERPRESS

Three CLIF-pipeline changes motivated by regex's `Fsm::next_state`
(`exec_byte` inlined), which runs 1.5x LLVM's samples (warm loop,
2026-11-02; analysis in `codegen-plan.md`, "regex, warm loop by
function"). Each lands default-off behind its own knob, with a
regression test, and is flipped only after a held-out A/B.

## 1. Branch facts through a select (`PLIRON_SELFACT`)

**Problem.** `Iterator::next` on a slice iterator, once inlined, returns
`Option<&T>` as a nullable pointer: `v = select(ptr == end, 0, ptr)`, and
the loop branches `brif v`. Code after the branch re-tests `ptr == end`
(the `enumerate` index, `nread`), and since the fact is never derived
those compares survive as `cmp`/`csel` pairs on the path where the
branch already decided them (regex's varint decoder grew three per
byte).

**Rule.** Where a block's only predecessor ends in `brif v` and
`v = select(c, a, b)` (looking through `uextend`/`ireduce`), with `c` an
`icmp` (as `cond_icmp` finds it):

| select shape | edge | derived fact |
|---|---|---|
| `select(c, 0, x)` | taken (`v != 0`) | `!c` |
| `select(c, 0, x)` | not taken (`v == 0`) | `c`, only if `x` is a nonzero constant |
| `select(c, x, 0)` | taken | `c` |
| `select(c, x, 0)` | not taken | `!c`, only if `x` is a nonzero constant |

The same applies when the brif tests `icmp eq/ne v, 0`. The derived
fact replaces the block's `v != 0` fact in `jumpthread::edge_facts` (one
fact per block; the select's condition decides strictly more compares
in practice). Consumers (`dominated_conds`, jump threading, `unroll`'s
`facts_at`) are unchanged.

**Soundness.** On the taken edge of `select(c, 0, x)` the select's value
is nonzero, so it did not pick the `0` arm, so `c` was false. The
not-taken direction needs `x != 0` to exclude "picked `x`, which was
zero"; only a nonzero constant is accepted.

**Test.** `pliron-clif-opt/tests/selfact.rs`: a loop shaped like the
inlined slice iterator; after `fold_dominated_conds` the re-test folds,
and a `select(c, x, 0)` with non-constant `x` must not fold on the
not-taken edge. Differentially checked.

## 2. Pressure-aware INNERPRESS (`PLIRON_LICM_INNERPRESS=2`)

**Problem.** INNERPRESS (`=1`, on) never hoists, out of a loop with
inner loops, a load that no inner loop uses: hoisted, it would occupy a
register across the inner loops for nothing. That fixed montgomery
(about 20 values already live across its inner multiply loops, plus 8-9
used inside them). But it also blocks regex's state-decode loop, whose
only inner loop is a varint decoder with a handful of live values: its
`SparseSet` fields reload on every element.

**Rule.** For a loop `P` with inner loops, a load used only outside
them may still hoist while every inner loop `I` of `P` has room:

- `pressure(I)` = the most integer-class values live at any point in
  any block of `I` (a backward scan from each block's live-out set;
  `i128` counts two; constants, symbol addresses and constant offsets
  count zero, since regalloc rematerializes them or lowering folds them
  into addressing modes). Entry counts alone were tried first and missed
  montgomery's in-block products and carries: they left room for three
  hoists that cost blind-sig 39%. Liveness is one sparse backward
  dataflow per `licm::run`,
  computed when first needed; it is not updated as hoists happen (an
  estimate, refreshed by the next LICM round).
- `budget(I)` = the target's allocatable integer registers minus 2
  (aarch64 24, x86-64 12), or the callee-saved count (aarch64 10,
  x86-64 5) if `I` contains a call, since values live across a call must
  sit in callee-saved registers.
- Loads are visited in program order; each hoist counts one against
  every inner loop, and hoisting stops once any `I` would reach
  `pressure(I) + hoisted >= budget(I)`.

Functions over 4,000 blocks skip the dataflow and keep the `=1`
behaviour (no such hoists).

**Soundness.** Only which hoists happen changes; each hoist still passes
`hoists()`.

**Test.** `pliron-clif-opt/tests/licm_innerpress.rs` gains two cases: a
loop whose small inner loop leaves room (the load hoists under `=2`),
and the existing montgomery-shaped case with enough values live across
the inner loop that `=2` still refuses.

## 3. Loop-only unswitching (`PLIRON_UNSWITCH_LOOP`)

**Problem.** `bcheck::unswitch_loop` clones the loop's closed region:
the loop plus every block only it reaches. When the loop is followed by
most of the function, that region is "too big" and nothing happens
(regex's `exec_byte` qcur loop: the rest of `next_state`). It also takes
only an invariant `brif` whose block dominates every back edge, so a
test that only some iterations reach (the `b == EOF` check in the
`Bytes` arm) is never a candidate. LLVM clones just the loop and repairs
SSA at its exits.

**Rule.** With `PLIRON_UNSWITCH_LOOP=1`, when the closed region is too
big, unswitch by cloning only the loop body:

1. Candidate condition: as now, an invariant `brif` dominating every back
   edge; failing that, any `brif` in the body on an invariant condition
   (defined outside the body, or a header param passed through
   unchanged).
2. Bail if any exit edge leaves through `br_table` or an exception
   table, or if more than 16 values defined in the body are used
   outside it, or more than 64 such uses exist.
3. Clone the body (`clone_region` with the body as region), folding the
   candidate (and every brif on the same condition) to "true" in the
   clone and "false" in the original. Clone exits keep the original exit
   targets, passing the cloned values.
4. Entry edges go to a new guard block `g(header params)` that branches
   on the condition to the clone's header or the original's.
5. SSA repair for each value `v` defined in the body and used outside it
   (`v'` its clone): every outside use is rewritten to `read(v, U)` for
   its block `U`, where `read` is the on-the-fly construction of Braun
   et al. over the now-complete CFG: a block with one predecessor reads
   through it; a block with several gets a new block param (created
   before recursing, so cycles terminate) whose argument on each incoming
   edge is `v` from an original-body predecessor that `v`'s definition
   dominates, `v'` from the clone of one, `read` of the predecessor inside
   the region `v`'s definition dominates, and a zero constant otherwise
   (no path from such an edge reaches a use; fuzz seed 57201 passed `v`
   on exit edges from body blocks before its definition). The backward
   walk stays inside that dominance region; only integer values up to 64
   bits are repaired. Trivial params are left for `remove_trivial_params`.
6. Drop blocks the folds made unreachable (existing sweep).

**Soundness.** Each use outside the loop was dominated by `v`'s
definition, so every backward path from it reaches the loop through an
exit edge, now either the original's (carrying `v`) or the clone's
(carrying `v'`); it cannot reach the guard or the entry without passing
through one of the loops. The two versions are the original loop with a
branch on a loop-invariant value resolved one way each, chosen by that
same value before entry.

**Budgets.** As now: body at most 320 instructions, 8 loops and 4
versioning rounds per function; the use index is built once per
versioning.

**Test.** `pliron-clif-opt/tests/unswitch_loop.rs`: a loop whose closed
region is too big, with a value defined in the loop used after it, and
an invariant test reached only on some iterations; it unswitches, the
verifier passes, and the result is differentially checked against the
original on inputs taking both sides.

## Results (2026-11-02)

Five tuning benches, 3 function orders x 2 stack pads x 4 rounds,
interleaved against the default build:

| knob | geomean | notes |
|---|---|---|
| `PLIRON_SELFACT=1` | 1.000 | json -2%, regex +2.5% (regex layouts span 1.05-1.14) |
| `PLIRON_LICM_INNERPRESS=2` (entry pressure) | 1.076 | blind-sig +39%: three hoists into montgomery |
| `PLIRON_LICM_INNERPRESS=2` (peak pressure) | 1.005 | montgomery refuses again; regex +2.8% within its spread |
| `PLIRON_UNSWITCH_LOOP=1` | 1.004 | fires on `next_state`'s qcur loop: `b` stays in w19, the per-state EOF test is gone from one version |
| all three | 1.000 | |

All three are correct and tested but measure neutral; they stay opt-in.
regex's `next_state` loops are not where its time goes once these
compile as intended: the remaining gap is spread over loads in the
decode and `follow_epsilons` loops (codegen-plan.md).

## Tried: splitting live ranges around crowded loops (`PLIRON_LOOPSPILL`, reverted)

A last CLIF step stored values live through a crowded loop but unused in
it to a stack slot before the loop and reloaded them at their later uses
(blocks strictly dominated by the preheader; a use in the preheader runs
before the store and, around an outer loop, keeps the value live
anyway; the first version missed this and miscompiled regex and
pulldown). Correct after that, but no gain: regex 1% faster, pulldown 2%
slower (noise), and regex's qcur loop still reloads `b`, now twice per
state. The pressure numbers say why: about 37 integer values are live
across that loop's `follow_epsilons` call against 10 callee-saved
registers, most of them values the IR and CLIF passes forwarded or
hoisted across calls to the whole function. A spill (store + reload) of
those cannot catch up; re-loading them from their original address
(when memory is unchanged, the same facts that justified forwarding
them) would cost only the load. That is the next lever: bound how far
forwarded/GVN'd loads live across calls, or rematerialize them.
