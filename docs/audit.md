# Auditing rewrites with Datalog

`src/wpeep_audit.rs` checks every copy-forward round of the wasm
peephole pass (`wpeep`) with a small `ascent` program. It is the
prototype of a general tool: **write the program and the rewrite's
assumptions into one stream of facts, and let Datalog decide whether the
rewrite preserved meaning.**

## Why

Every peephole is a claim ("this code means the same as that code") plus
a side condition the pass believes makes the claim true. Miscompiles
live in the gap between the condition the pass *checked* and the one the
rewrite *needed*.

The bug fixed in dab149b is the clean example. Copy-forward rewrites

    set t ← V  …  get t; set x        into        set x ← V  …

Moving the write of `x` earlier makes it happen on more paths. The pass
checked that the same *set* of definitions reaches every read of `x`
before and after. That held: the old value and `V` both still reached
the read after the block. But on the path that branched out of an `if`
arm before the copy, the read used to see the old value (an inlined
frame-slot address) and now saw `V`. A set comparison can't tell which
source arrives on which path, so the rewrite passed its own check and
broke wasm std.

Tightening the side condition fixed that instance. The audit fixes the
*method*: it doesn't trust the pass's reasoning at all.

## How

One database, two kinds of rows:

- **The code, both versions.** Control-flow edges between instruction
  positions (`succ`; the rewrite only renames locals and deletes copy
  pairs, so both versions share positions), and for each version the
  writes, with the value source each one stores (`odef`/`ndef`), and the
  reads (`oread`/`nread`).
- **The assumptions, as the rewrite commits them.** "The copy-set stores
  the producer's value" (both versions name that value by the producer's
  position) and "the copy-get reads exactly the producer"
  (`assume_copy`).

Then four rules walk both programs down the *same* path in lockstep:

    flow(pos, old_local, new_local, old_src, new_src)

and one rule flags a read where the two sources differ (`bad_read`).
That product construction is exactly the path-sensitive question the
pass approximated with sets. The assumption gets checked by the same
reaching-definition machinery (`oflow` → `bad_assume`), so a wrong
belief is as visible as a wrong rewrite.

## Why it's a good shape

- **Code and assumptions are the same kind of thing.** A pass's private
  belief becomes a row: printable, countable, checkable, and named in
  the failure report. Usually it lives only in a comment.
- **One checker, many rewrites.** Nothing about copy-forward is in the
  rules. Any rewrite that renames locals, deletes copies or moves writes
  is audited by emitting its facts. A new peephole adds facts, not a
  checker.
- **Independent and small.** Reaching definitions over a product state
  is textbook Datalog, a dozen lines, and shares no code with the pass,
  so a bug in the pass's own analysis can't hide itself.
- **A guard, not just an alarm.** On a violation the round is reverted,
  so an audited build is a "verified peephole" build.
- **It complements the interpreter checker.** `PLIRON_DIFFCHECK` runs
  code on random inputs: it needs inputs that reach the bad path, and it
  can't run wasm32 functions on a 64-bit host at all. The audit covers
  every path symbolically, but only for the dataflow it models.

## Evidence (2026-10-18, wasm std suite)

| configuration | result | rounds rejected |
|---|---|---|
| straight-line check off, no audit | 6 tests fail (the original bug) | — |
| straight-line check off, audit on | all pass: the audit caught and reverted the bad rounds | 32 |
| straight-line check on, audit on | all pass, no false alarms over every std body | 0 |

Cost: ~7% of a full std build. The product state grows as positions ×
(old sources × new sources), so like every other fixpoint in this repo
it has a budget (`BUDGET` in `wpeep_audit.rs`): a round over it is left
unaudited and counted (`PLIRON_TRACE=pliron::wasm=debug` shows them),
never hung on. Before the budget, one `proc_macro` body for wasip1 held a
rustc for 28+ minutes. With it, the whole wasm test leg runs in the same
~4.5 min as without the audit; 21 of its rounds were over budget. It is
on in `tests/wasm/run.sh` (and so in the gate) as
`PLIRON_WASM_AUDIT=panic`. Unit tests:
`wasm::wpeep_tests::{without_pathcheck_branch_past_copy_miscompiles,
audit_rejects_branch_past_copy, audit_accepts_plain_copy}`.

## Limits

- It checks the rewrite against the *model*: local-variable dataflow.
  The operand stack isn't modelled (copy-forward only deletes
  stack-neutral `get; set` pairs). A rewrite that reorders stack values
  needs stack facts first.
- It audits what a rewrite emits. Uninstrumented rewrites (the
  keep-on-stack and dead-write passes in `wpeep`) aren't covered yet.

## Where to apply it next

1. **The rest of `wpeep`**: keep-on-stack (`KOS`) and the dead-write
   sweep, with a few stack facts.
2. **CLIF code motion**: `loadfwd`, `licm`, `jumpthread`, `edgespec`.
   Same pattern over CLIF: blocks and edges as `succ`, `def`/`use` of
   SSA values, memory as named locations. Each rewrite emits its claim:
   "no store to `p` between these loads on any path", or "this block
   param is the same value on every incoming edge".
3. **Facts handed between passes**: `nonnull`, `noalias`, `nocapture`,
   `CallInfo` argument tables. Re-derive or sanity-check them against
   the IR they describe after each pass. That is the class behind stage2
   miscompile #3 (an argument array that went stale after an ABI
   rewrite).
