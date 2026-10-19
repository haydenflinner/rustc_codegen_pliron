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
  writes, with the value source each one stores (`owr`/`nwr`), and the
  reads (`sread` for the same local in both, `rread` for a read the
  rewrite redirected to another local).
- **The assumptions, as the rewrite commits them.** "The copy-set stores
  the producer's value" (both versions name that value by the producer's
  position) and "the copy-get reads exactly the producer"
  (`assume_copy`).

Then the rules walk both programs down the *same* path in lockstep and
flag any read where the values differ (`bad_read`, `bad_rread`). That is
exactly the path-sensitive question the pass approximated with sets.
The assumption is checked by the same machinery (`cb` → `bad_assume`),
so a wrong belief is as visible as a wrong rewrite.

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

Cost: ~7% of a full std build, and proportional to the code around
each rewrite rather than to the function. It is on in
`tests/wasm/run.sh` (and so in the gate) as `PLIRON_WASM_AUDIT=panic`.

## Making it cheap: only compute where the programs can differ

The first version tracked, at every instruction, every pair of
(old source, new source) a path could produce, from the function entry.
On a 1,063,726-instruction `proc_macro` body that held a rustc for 28+
minutes. The two programs are identical except at the rewritten
instructions, so almost all of that work proved "they agree". The rules
now store and compute only what a check needs:

- **Disagreements only.** Agreement is the implicit default for a
  tracked local. A `dis` row exists only on a path where the two
  versions hold different values. Rows start at rewritten writes and end
  at the next write both versions make identically.
- **Demand-driven agreement.** Where only one version writes a local,
  the outcome depends on whether the two agreed just before. That is
  answered by a backward walk from the write (`dem` → `agree_in`) that
  stops at the nearest identical write. Renamed reads and the
  copy-get assumption are backward queries the same way.
- **Block at a time.** The CFG is contracted to control instructions and
  instructions that touch a tracked local; everything in between
  collapses.
- **Loop iterations.** Values are named by the instruction that produced
  them, so one from an earlier iteration would look like a fresh one. A
  held value becomes `STALE` when its producer runs again.

Result on the same body: 136,186 graph nodes, 49,417 disagreement rows,
**0.68 s**; the 328,801-instruction one takes 37 ms. The budget
(`BUDGET`, on graph nodes × tracked locals) remains as a safety net and
no longer triggers anywhere in the wasm test leg.

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
