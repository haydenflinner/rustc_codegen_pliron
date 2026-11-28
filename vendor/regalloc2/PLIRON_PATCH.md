# Why regalloc2 is vendored

Vendored from crates.io regalloc2 0.15.2. Changes, each marked `PLIRON:`: the
spill-priority change below (default on), and the runtime knobs
`PLIRON_RA2_XBLOCK` and `PLIRON_RA2_MERGEDEPTH` (default off; sections
further down). The spill priority: in `src/ion/process.rs` it divides the spill weight by
`4 * isqrt(len)` instead of `len`, so long-lived values with many uses (e.g.
`self` across a large inlined lexer function) keep a register. Build with
`RA2_LINEAR=1` in the environment to get upstream behavior back.

Measured (codegen-improvements.md): rustc_lexer `Cursor::advance_token` stack
references 292 -> 91; stage2 regex-syntax instructions -0.2% / -0.3%.

TODO: once proven out, submit upstream (bytecodealliance/regalloc2) and drop
this copy plus the `[patch.crates-io]` entry in the root Cargo.toml.

## `PLIRON_RA2_XBLOCK=1` (runtime, default off): redundant moves across a call's return

`src/ion/moves.rs`: the redundant-move eliminator clears its state at
every block boundary. Under the flag it keeps the state when the next
block's only predecessor is the block right before it (Cranelift's
`try_call` return blocks); the instructions in between still clear what
they define or clobber. That elides a reload's store back into its own
spill slot after the call, and other moves whose source and destination
already hold the same value. `src/lib.rs` links `std` under the default
`std` feature for the env read. regex-syntax: `__text` -2,272 B,
spill stores -115 (docs/history/2026-10-27-spills2.md). Checked with
`PLIRON_RA_CHECK=1` (the regalloc2 checker).

## `PLIRON_RA2_MERGEDEPTH=1|2|3` (runtime, default off): loop-depth merge order

`src/ion/merge.rs`: blockparam merging normally walks the edges in order.
Under the flag it merges the edges of the deepest loops first (by
`approx_loop_depth`; ties: `1` keeps edge order, `2` shortest live range
first, `3` longest first), so a loop-carried value
shares the parameter's register and the remaining move lands on the
preheader edge instead of the back edge.

## `Output::edit_vregs` (always on): the vreg each edit's destination holds

`src/ion/data_structures.rs` (`Edits::add_for`, `take_vregs`, paired sort),
`src/ion/moves.rs`, `src/ion/mod.rs`, `src/lib.rs`: the parallel-move
resolver already knows `to_vreg` for each resolved move; it is now kept
in `Output::edit_vregs` alongside `edits` (empty from the fast
allocator). Cranelift uses it to rematerialize constant and address
reloads (cranelift-codegen PLIRON_PATCH 76).

## Five workstreams (2026-11-18): stats, USESPLIT, greedy

- `Output::stats`: `splits_minimal`, `splits_use_cluster`, `splits_region`
  (`data_structures.rs`); `PLIRON_RA_STATS=1` / explain prints
  `ra2stats:` from Cranelift `compile.rs`.
- `PLIRON_RA2_USESPLIT` (default on since 2026-11-18, `=0` off): before
  `MAX_SPLITS` → minimal, cut at the largest use-free gap
  (`try_use_cluster_split` in `process.rs`; `_GAP` default 128;
  single-block unless `_MULTIBLOCK=1`). Spillset `use_cluster_done`
  prevents loops. blake3 −154k, blind-sig −41k non-nop.
- `Algorithm::Greedy` + `PLIRON_RA_ALGO=greedy` (`src/greedy/`): all-spill
  homes + reload/spill/edge edits (checker-clean, 20k rafuzz seeds).
  `PLIRON_RA_GREEDY_COLOR=1`: per-block linear scan promoting single-block
  non-param vregs (branch args OK; params stay spilled; clobber /
  foreign-FixedReg aware). Checker-clean on 10k rafuzz seeds; unmeasured
  on held-out. `tools/rafuzz` mixes greedy and `GREEDY_COLOR=1`.
- `Algorithm::Mcts` + `PLIRON_RA_ALGO=mcts` (`src/mcts_ra.rs`,
  `greedy::run_mcts`): Gumbel-root / prior-shaped PUCT over homes for
  single-block non-param vregs. Prior prefers already-used pregs (lower
  footprint), deprioritizes new pregs and spill; FixedReg pin boost +
  clobber/foreign-fixed forbid lists from greedy. Branching capped to
  top-M free pregs + spill (`PLIRON_RA_MCTS_TOPM`, default 8). Reward =
  `-(preg_count + 8*spills)`. Sims: `PLIRON_RA_MCTS_SIMS` (256);
  `PLIRON_RA_MCTS_GUMBEL=0` disables root Gumbel. `tools/raproof
  --algo mcts` reports `preg_pressure`. Default-off. Log:
  `docs/history/2026-11-18-ra-mcts.md`.
- `src/prove.rs` + `tools/raproof`: interference-graph certificates —
  max-clique lower bound and (≤48 verts/class) chromatic upper bound —
  compared to the heuristic's distinct-preg footprint; `check_bound`
  re-verifies every cert; `try_fewer` / `--prove-fewer` searches for a
  colouring with K < heuristic footprint (trade-compute dial:
  `--budget`). See `docs/regalloc.md` "Proof engine" and
  `docs/history/2026-11-18-ra-raproof.md`.
- `PLIRON_RA_PROOFCOLOR=1` (opt-in, needs `GREEDY_COLOR=1`): greedy
  applies a checked chromatic colouring when the class has ≤
  `MAX_EXACT` verts — rematches single-block non-param homes if their
  footprint exceeds `upper_k`, and steers emit temps toward the
  colouring's K pregs (`greedy/mod.rs` `proof_recolor`). Fitness
  `match/(match+above_opt+1)` on the fixed 40-seed window: greedy
  COLOR 0.20 → COLOR+PROOFCOLOR 0.30; rafuzz 2k seeds green. Remaining
  `above_opt` rows are edit temps that need a preg while all K proof
  homes are live (abstraction gap). See
  `docs/history/2026-11-18-ra-raproof-learn.md`.

## `Function::is_remat` (`PLIRON_RA2_REMATW`, default 7 since 2026-11-18)

`src/lib.rs` (trait hook, default `false`; `rematw_mode`), `src/ion/process.rs`
`recompute_bundle_properties`: a bundle whose every range's vreg is
rematerializable takes a reduced spill weight instead of the long-range
sqrt normalization, as LLVM halves the weight of rematerializable
intervals. Default mode `7`: `/2` only when the bundle has at least
`PLIRON_RA2_REMATW_MINUSSES` uses (default 8). `=0` off. Other modes:
`1` always `/2`; `2` no-call; `3` call-cross; `4` `/4`; `5` `prio>64`;
`6` `uses≤MAXUSES`. Cranelift fills `remat_vregs` whenever the knob is
not `0`. Held-out (copies=2): pulldown −0.21%, protobuf +0.017%, html
+0.006%; see `docs/history/2026-11-18-ra-night.md`.

## `Function::no_param_merge` (Cranelift shrink-wrapping, opt-in)

`src/lib.rs` hook (default `false`), `src/ion/merge.rs`: blockparams of
the named block are not merged with their inputs, so the live-range
split Cranelift's `shrink_split` makes at the save block survives.

## `set_explain` / `explain!` (runtime, debug): why a value was spilled

`src/lib.rs` (thread-local flag, `explain!` macro), `src/ion/process.rs`
(`explain_bundle`, events in `process_bundle`, `split_and_requeue_bundle`,
`split_into_minimal_bundles`): with the flag set, every bundle processed,
register granted, eviction, split decision (with the cheapest conflict
set and its weights), requirement conflict, split point and minimal-bundle
fallback prints a `ra2x:` line naming the bundle's vregs, its code range,
use count, spill weight and loop depth. Cranelift re-exports
`set_explain`; pliron sets it from `PLIRON_RA_EXPLAIN=<symbol substring>`
and also dumps that function's pre-regalloc VCode, so a vreg in the log
can be matched to its instructions.

## `PLIRON_RA2_LOOPDEPTH=1` (runtime, opt-in): dominance-checked back edges

`src/cfg.rs`: `approx_loop_depth` counted every edge to an earlier block
as a loop back edge. Cranelift lays cold blocks out last, and cold paths
that rejoin hot code look like back edges: regex's `next_state` reached
"depth" 16, use weights (`1000 * 4^depth`) hit the bundle weight cap
(12 bundles at exactly 26843545), and ties decided which hot value kept a
register. Under the flag only an edge to a block that dominates its
source counts (max depth 6, no capped bundles). The back edges are also
recorded (`CFGInfo::back_edges`).

## `PLIRON_RA2_LOOPSPLIT=1` (runtime, opt-in): split at the hottest use's cycle

`src/ion/process.rs` `process_bundle`: when a bundle must split, find the
innermost cycle around its hottest use (the shortest recorded back edge
`u -> h` with `h <= use block <= u`) and split at that cycle's entry
(bundle starts before it) or exit (bundle starts inside and runs past
it), without trimming the loop piece to its uses. regalloc2 otherwise
splits at the conflict point, re-splits the remainder at its own start,
peels one use at a time and falls back to per-use reloads: pulldown's
byte-scan loop reloaded its LUT pointer (and with the depth fix alone,
its length) from the stack every byte, because they are live across the
special-byte handler's calls. With both flags the loop is
`ldrb; ldrb; cbnz; add; cmp; b.lo` with everything in caller-saved
registers, LLVM's shape. Loops sharing a header (the scan loop and the
handler both jump back to it) have one depth; the shortest back edge
still separates them.

## `PLIRON_RA2_FREQSPLIT=1` (runtime, opt-in): split at the cheapest block boundary

`process_bundle`: after the depth hoist, the split point moves to the
cheapest block boundary between the bundle's start and the conflict.
A split's move runs at the frequency of the point it lands on: inside a
block, that block's `hot_bonus`; at a block entry, the forward edges
into it (a loop header's back edge joins the second piece to itself and
carries no move), each estimated at `min(pred, block)`. Ties keep the
later point, so straight-line code splits at the conflict as before and
a loop entry wins only when its preheader edge is colder than the
conflict's block. With the frequency weights this is LLVM's region split
in spirit. Measured (11-03): tuning set 0.999, rust-compression 1.010
(100 of its functions change, mostly `FindLongestMatch` variants, a
few spills traded for register moves); held-out 0.999 [0.995, 1.003]
of base with the harness's intervals. Opt-in: the
depth hoist already lands on loop entries, and the cases where a
colder boundary exists further back did not pay.

## `PLIRON_RA2_FREQ=0..5` (runtime; 3 is the default, `=0` off): block-frequency spill weights

`Function::block_hotness(block) -> Option<f32>` (default `None`) lets the
frontend hand in each block's execution frequency relative to the entry
block. `CFGInfo::hot_bonus` holds the per-block hot bonus of the spill
weight: `1000 * 4^depth` as before, or under the flag derived from the
frequency. `spill_weight_from_hot` replaces the depth-based
`spill_weight_from_constraint` at the three call sites (use weights in
`liveranges.rs`, the two move costs in `process.rs`); the loop-depth
hoist of a split point still uses `approx_loop_depth`. Cranelift fills it
from `placement::frequencies` (`VCode::compute_block_freq`, patch 99).

Modes 1 and 2 scale the raw frequency (`1000 * freq`, `1000 * freq^0.4`)
floored at 1/64 and keep the additive def/constraint bonuses. That fails
where the hot code sits behind a cold edge: regex's `Fsm::next_state` is
a fast-path return and a cold slow path with the loops, every block is
below the entry, the floor flattened them all, and the constant bonuses
decided (bundle weights of 45..271 against millions before; the state
index went to the stack). Modes 3 and 4 normalize by the function's
hottest block, map `freq / max` (mode 4: its square root) onto
`[1, 1000 * 4^6]`, and multiply the bonuses in (`x1..x5`) instead of
adding them, so a cold region keeps its internal structure. Measured
(11-03): tuning set 0.996 (mode 3), 0.986 with `LOOPSPLIT`+`LOOPDEPTH`;
held-out 1.126 -> 1.123 either way (pulldown, compression, html-rewriter
-2%; regex, json, protobuf +1%). Opt-in.

Why modes 3/4 lose on regex (11-03, `RA2STAT` per function, base vs
mode 3): stores 6518 -> 7974, reloads 16953 -> 14601, but the two hot
DFA functions got more reloads (`cached_state` 4 -> 9, `next_state`
19 -> 29) while their loops stayed instruction-for-instruction the same.
Placement rates a loop body at ~32x its surroundings (back edge 124
against 4); regalloc2's depth scale says 4x. Those loops walk a DFA
state's short instruction list, so the straight-line code around them
runs about as often as the loop, and 32x made the allocator spill the
state pointer out of the fast path (twelve reloads in `next_state`
where base kept it in a callee-saved register). The trip-count guess
is the untrustworthy half of the static model; the cold marks and
unlikely hints are facts.

Mode 5 ("demote only") keeps the depth scale and the additive bonuses
and lets the frequency only lower a block: `by_depth * fr / max_at_depth`
(the hottest block at the same loop depth), floored at 1/64. A loop
never rises above 4x its surroundings; a cold arm inside a loop or a
cold function tail still drops below its peers. regex: reloads 16953 ->
14867 at stores 6518 -> 6785, `cached_state` identical to base,
`next_state` 19 -> 21 reloads, 225 functions changed (418 under mode
3). Tuning set (same run, K=6 layouts x 3 rounds): mode 5 0.9928
(regex 0.982, pulldown 0.992, blake3 0.993, json 0.996, blind 1.000)
against mode 3 0.9943 (pulldown 0.982, json 1.006). Held-out on AC
power with the harness's new bootstrap intervals and A/A floor (10
rounds x 3 layouts x 2 pads): mode 5 / base 0.995 [0.990, 0.999], mode 3
0.992 [0.987, 0.998], A/A 1.000 [0.992, 1.005]; no single program is
outside its own A/A interval. Head to head from the same samples, mode
3 / mode 5 is 0.997 [0.994, 1.002] (regex 0.988, html-rewriter 0.985,
blind-sig 1.011), and against the A/A side mode 3 is a significant win
(0.993 [0.989, 0.999]) while mode 5 is not (0.996 [0.990, 1.002]). The
per-function reload counts in regex's `next_state` did not predict the
runtime: regex runs 1.2% faster under mode 3. **Mode 3 is the default**
(`=0` restores the depth-only weights); mode 5 stays as the
"facts only" variant. Compile time: no visible cost (regex's benchmark
crate 3.2 s either way). Against mode 3 as the base (same harness, 10
rounds): mode 3 + `LOOPSPLIT`/`LOOPDEPTH` 0.998 [0.994, 1.003], mode 4
1.003 [0.997, 1.007], mode 5 1.004 [0.999, 1.009], A/A 0.996 [0.985,
1.000]; nothing beats mode 3 outside the floor, the split knobs stay
opt-in.

## `PLIRON_RA2_NOPINMERGE` (runtime, default on, `=0` off): don't spread a fixed pin through merges

`Requirement::merge` promotes `Register + FixedReg(p)` to
`FixedReg(p)`, so a single fixed-use operand (a try_call-ret vreg, an
arg copy) pins whatever it merges with — and blockparam coalescing
merges whole chains. The merged bundle then requires `p` for its full
extent and splits+fills at every `p`-clobber it crosses
(`BrotliCreateHqZopfliBackwardReferences`: a try_call result pinned to
x0 dragged its 11-vreg param chain through every call in the walk
loop). `pin_spread_hurts` (`ion/merge.rs`) refuses the merge only when
the merged requirement is `FixedReg`/`FixedStack` *and* the
unconstrained side's ranges overlap a fixed reservation on that preg —
pinning would provably force a split where the edge move is cheaper.
Measured on that function: 2258 -> 2205 insns, `[sp]` accesses
527 -> 454, ~2.8 -> ~2.6 ms.

## `PLIRON_RA2_SLOTPAIR` (runtime, default on, `=0` off): pair-aware spillslot repacking

`allocate_spillslot` assigns word indices in allocation order, which has
no relation to emission order — two fills emitted consecutively almost
never land on adjacent slots, so AArch64 `ldp`/`stp` fusion of
spill/reload edits could not fire (LLVM emits ~100 pairs in
`BrotliCreateHqZopfliBackwardReferences`; we emitted none).
`slot_pair_layout` (`ion/spill.rs`, called at the end of `ion::run`)
collects weighted pairing edges between slot words — same-direction
`Edit::Move` pairs at the same emission gap, weighted by the block's
`hot_bonus`, requiring equal size and class — links each slot to its
two heaviest partners into chains (union-find, no cycles), then repacks
`[0, num_spillslots)`: chains first at the *highest* aligned bases
(the top of the spillslot area sits just below fp+clobber, inside
simm7 reach fp-relative even when deep from sp), leftovers bottom-up.
Every `Allocation::stack` in `output.edits`, `output.allocs`,
`output.debug_locations`, `ctx.spillslots`, and
`ctx.extra_spillslots_by_class` is rewritten consistently; the
permutation is a pure relabeling so semantics are unchanged. Bails
(leaving the original layout) on `multi_spillslot_named_by_last_slot`
or any stack allocation whose extent is unknown. Pairs with the
emit-side `PLIRON_EDITPAIR` peek in `vcode.rs` and the fp-relative
`pair_amode` fallback in aarch64 `emit.rs` (`PLIRON_PATCH.md` there).

## `PLIRON_RA2_POSTOPT=<bits>` (runtime, default 15 since 2026-11-17, `=0` off): spill-traffic cleanup after allocation

`src/ion/postopt.rs`, called from `ion::run` before the slot-pair
repack; knobs in `src/lib.rs` (`postopt_bits`, `postopt_stats`). Two
whole-function dataflows over the finished `Output` (edits + operand
allocations); operands, slots and registers are never changed, only
edits deleted or a load turned into a register copy:

- forward "register `r` mirrors slot `s`", intersection over all
  predecessors, optimistic around loops (8 sweeps, then one pessimistic
  sweep): bit 1 drops a reload into a register that already mirrors the
  slot, bit 2 turns a reload into a copy from another register that
  mirrors it, bit 4 drops a spill store whose slot is already clean;
- backward slot liveness: bit 8 drops stores to slots no path reads
  before the next write (can leave `debug_locations` naming a stale
  slot).

The stock eliminator (`redundant_moves.rs`) forgets its state at block
boundaries (`PLIRON_RA2_XBLOCK` extends it to single-predecessor
fallthrough only). After relax's post-allocation spill windows,
`redundant_saves` and `dead_spill_stores`. `PLIRON_RA2_POSTOPT_STATS=1`
prints one `ra2post:` line per changed function. Checked with
`PLIRON_RA_CHECK=1` on the eight held-out programs (identical output);
static effect at `=15`: stack stores -1..3%, instructions -0.1..0.3%.

## `PLIRON_RA2_SRCVREG` (runtime, default on since 2026-11-17, `=0` off): block-parameter moves carry the argument's vreg

`src/ion/moves.rs` (`block_param_src_vreg`, the block-param move
insertion), `src/lib.rs` (`srcvreg_on`): the `edit_vregs` entry of a
block-parameter move names the branch argument's vreg instead of the
parameter's. After the move both hold the same value; Cranelift only
uses the entry to decide rematerialization, and the argument is the one
that can be a constant. A constant passed to a spilled parameter was a
load of the constant's own spill slot plus a store (through the scratch
register, both legs keep the label); it becomes `mov #k` + store, and a
slot read only that way loses its stores (`PLIRON_REMAT_STORE`).
pulldown's `scalar_iterate_special_bytes` initialised a dozen spilled
loop states that way on every call. Static, release held-out binaries:
stack loads -4.3% (pulldown-cmark), -3.5% (regex), -3.6%
(rust-compression); stores -0.7..-1.3%. Checked with `PLIRON_RA_CHECK=1`.

## `PLIRON_RA2_COLDSPLIT=1` (runtime, opt-in): cut cold call blocks out of a hot bundle

`src/ion/process.rs` (`try_cold_split` with the rule in its doc comment,
`cold_split_range`, `cold_remainder_fits`, two hooks in `process_bundle`,
flag inheritance in `split_and_requeue_bundle`), `src/cfg.rs`
(`CFGInfo::clobber_prefix`, `cold_freq`, filled only under the knob),
`src/ion/data_structures.rs` (`LiveBundle::cold_split`,
`SpillSet::cold_spill`), `src/ion/spill.rs` (a `cold_spill` spill bundle
does not open an unused callee-saved register at second chance),
`src/lib.rs` (`Function::block_is_cold`, `coldsplit_on`/`_mode`/`_ratio`/
`_colddiv`/`_bias`). Cranelift implements `block_is_cold` from its cold
block marks (`machinst/vcode.rs`).

When a bundle is given a callee-saved register that nothing else uses,
or gets no register at all, the cold blocks in which it is live across a
clobbering instruction are cut out of it at block boundaries; the cold
part goes to the spill slot (uses inside become minimal bundles), the
rest stays one bundle and takes a caller-saved register. The first case
is cost-tested against 2 x entry frequency and requires that the rest
fits a caller-saved register. `=2`/`=3` select one trigger;
`PLIRON_RA2_COLDSPLIT_RATIO` (16), `_COLDDIV` (64), `_BIAS` (1.0).
`tests/coldsplit/main.rs` shows the pattern (6 -> 0 saved registers
around a `#[cold]` call in a loop). Held-out programs: prologue
callee-saved saves -0.1..-5.1%, retired instructions unchanged
(0.993..1.007); `PLIRON_RA_CHECK=1` clean. See
`docs/history/2026-11-13-six-features.md` section 5 for the accounting.

## `PLIRON_RA2_REHOME` (runtime, default `2`; `0` off, `1` loop latches only): a value's register in a block that only joins

`src/ion/rehome.rs` (new), called from `Env::run` (`src/ion/mod.rs`)
after `allocate_spillslots` and before the move insertion;
`src/lib.rs` (`rehome_mode`).

A bundle that must split is cut at program points, and each piece is a
contiguous stretch of the lowered block order. A block that several
paths jump to comes after all of them in that order, so it falls into
the piece that covers the code just ahead of it, usually the one that
crosses a call and sits in a callee-saved register. When the block's
successor is back in the first piece (a loop latch jumping to its
header), the value is moved into the block's register on every path in
and moved back on the way out. pulldown-cmark's `scanners::unescape`
with one latch (`PLIRON_LOOPROT_MERGE=1`) ran `mov x19, x2; mov x22, x3;
cmp; b.hs; mov x2, x19; mov x3, x22; b` on every trip; the explain log
showed the header in `[6..51)` with `x2`, the latch at instruction 545
inside `[84..714)` with `x19`.

The pass takes each block with two or more predecessors together with
its jump-only successor blocks (the split critical edges) as a region.
For every value live through the region (its live-ins and its block
parameters) it prices the moves on the edges into and out of the region
with the block frequencies (`CFGInfo::hot_bonus`), once for the
allocation the value has there and once for each register it has on the
far side of one of those edges. If another register costs at most 15/16
of the current one and is free over the region's blocks (the per-register
allocation maps, which also hold clobbers and fixed uses), the value's
live range is cut at the region's block boundaries and the middle range
gets a bundle of its own with that register. The edge moves then come
from the normal insertion in `moves.rs`. Left alone: values whose live
ranges overlap (a spilled value with per-use register pieces), bundles
with a register limit, and values with a use in the region that is not a
plain `Reg`/`Any` use.

`tests/parity/latch.rs`: the latch above becomes `cmp; b.lo` (the moves
go to the arms that call). Exact executed instructions, off -> `2`
(2026-11-13): regex 51.58M -> 51.38M, rust-protobuf 65.69M -> 65.41M,
rust-html-rewriter -0.13M, rust-compression -0.06M, pulldown-cmark
-0.04M, rust-json and blind-sig unchanged, blake3-scalar +0.02M (eight
alignment `nop`s moved in front of a loop entered 3027 times; the moves
are the same). Mode `1` (regions with an edge to an earlier block only)
gives about half of it. Compile time: regex-syntax 80.62G -> 80.65G
retired instructions. Checked with `PLIRON_RA_CHECK=1` on the eight
held-out programs.

## Fuzzing support (`pliron-fuzz` feature, checker label check)

`Cargo.toml`, `src/lib.rs`: feature `pliron-fuzz` builds the `fuzzing`
module without `arbtest` (the upstream smoke tests stay behind
`fuzzing`); `tools/rafuzz` (driven by `harness/rafuzz.py`) uses it.
`src/fuzzing/func.rs`: `Options::pliron_hints` answers the hooks Cranelift
implements (`block_hotness`, `block_is_cold`, `is_remat`,
`no_param_merge`, `multi_spillslot_named_by_last_slot`) with arbitrary
values, so `PLIRON_RA2_FREQ`, `_COLDSPLIT`, the remat weight and the
unmerged-parameter path run under the fuzzer; `Options::alloc_regs` with
`machine_env_sized` allocates with fewer registers (more splitting and
spilling).

`src/checker.rs`: when `Output::edit_vregs` has one entry per edit, a
labelled move must move a value its label names (`EditVRegMismatch`).
Cranelift rematerializes a labelled reload from the label (patch 76
there), so a wrong label is a miscompile the dataflow check cannot see.
Labels naming a block parameter are skipped: a block-param move ahead of
a branch carries the successor's parameter, and Cranelift's reload PRE
moves such a reload two edges ahead of it; only instruction results are
rematerializable. `PLIRON_RA_CHECK_LABELS=0` turns the check off. It runs
wherever the checker runs (`PLIRON_RA_CHECK=1`, `PLIRON_RELOADPRE_CHECK=1`,
the fuzzer).
## `PLIRON_RA2_REGION=1|2` (runtime, default 1 since 2026-11-18): min-cut region splitting

`src/ion/regionsplit.rs` (new), the hook in `process_bundle`
(`src/ion/process.rs`, ahead of the conflict-point split),
`LiveBundle::region_blocks` and `SpillSet::region_count`
(`src/ion/data_structures.rs`), knobs in `src/lib.rs` (`region_on`,
`region_mode`, `region_k`, `region_use_cap`, `region_max_blocks`,
`region_ratio`, `region_max`); `cold_split_range` is now `pub(crate)`.

LLVM's `RAGreedy::tryRegionSplit` with `SpillPlacement`, solved exactly.
When a bundle must split, every block it is live in is a node labelled R
(in one physical register) or S (on the stack). For a candidate register,
blocks where the register is not free over the bundle's part of the block
must be S. An S block costs `hot_bonus x min(register uses, CAP)` (CAP =
`PLIRON_RA2_REGION_CAP`, default 2 since 2026-11-18 (was 1): a block-local
piece reloads once per
stretch); an edge the bundle is live across costs `min(hot(pred),
hot(succ))` when its ends differ. The cheapest labelling is a minimum s-t
cut (symmetric pairwise costs: submodular), found by Dinic's max-flow with
a budget linear in the graph. The `K` registers with the largest possible
gain (sum of S costs of their free blocks; `PLIRON_RA2_REGION_K`, default
8) are cut; the cheapest wins if it costs less than `RATIO` (1.0) x all
blocks S. The ranges are cut at block boundaries only: the R blocks become
one bundle with holes, assigned the register at once; mode 1 gives each S
block with register uses a block-local piece from its first to its last
use (requeued: it may still take a register or split locally), mode 2
requeues an S component of several blocks (S blocks joined by live edges)
whole; the rest goes to the spill bundle. Moves come from the ordinary
edge/range insertion in `moves.rs`, so the checker sees ordinary splits.

The register part keeps its parent's block count and is region-split
again (after an eviction) only while it has fewer blocks (LLVM's
`RS_Split2` guard); at most `PLIRON_RA2_REGION_MAX` (4) region splits per
spillset; bundles in more than `PLIRON_RA2_REGION_MAXB` (2048) blocks are
left to the ordinary split. Explain lines: `region ...` (each cut) and
`region split -> preg` (`PLIRON_RA_EXPLAIN`; `clif-check vcode` honours it
too). Unit tests for the max-flow in the module; stanza `regionsplit`
(stock output, modes 0/1/2 under `PLIRON_RA_CHECK=1`).

Exact executed instructions (2026-11-17, aarch64-linux-musl, callgrind),
default -> `PLIRON_RA2_REGION=1` with the first parameters (CAP 2, K 4;
built with `PLIRON_RA_CHECK=1`):
rust-compression 44,095,655 -> 42,728,743 (-3.10%: Zopfli -772k,
`BrotliEstimateBitCostsForLiterals` -298k, `compress_inner` -125k),
rust-html-rewriter 101,452,418 -> 100,774,931 (-0.67%), pulldown-cmark
51,914,147 -> 51,544,931 (-0.71%), regex 50,528,248 -> 50,067,237
(-0.91%), rust-protobuf 65,262,081 -> 64,938,104 (-0.50%), rust-json
51,478,425 -> 51,372,540 (-0.21%), blake3-scalar 49,040,376 -> 49,046,430
(+6k), blind-sig 34,687,507 -> 34,724,239 (+37k; this program is not
deterministic: two default builds differ by 10k). Mode 2: compression
+112k against mode 1, the others within +-20k (blake3 -9k, below the
default). With `PLIRON_XCGU_SIZE=100 PLIRON_XCGU_LASTCALL=20` (the copy
limit that inlines `HistogramSelfAddHistogram`): compression 44,196,946
-> 42,740,891; `BrotliHistogramCombine<HistogramLiteral>` 1,914,946 ->
1,777,062, so the copy change costs +12k with the region split instead of
+101k. Compile time: html-rewriter corpus `clif-check codegen` 17.4-17.9 s
user either way.

Parameters (exact counts against CAP 2 / K 4, four programs): CAP 1
pulldown -95k, others within +-16k; CAP 3 identical; K 8 pulldown -43k;
CAP 1 + K 8 + `SPILLHOIST`: pulldown -170k, protobuf -26k, regex +21k,
compression -18k, html -6k (now the defaults). `RATIO` 0.8 (skip cuts that
save under 20% of all-stack) gives blake3 back its 6k but costs
compression +452k: stays 1.0.

**Default on since 2026-11-18** (`PLIRON_RA2_REGION` default `1`,
`PLIRON_RA2_SPILLHOIST` on unless `=0`; CAP 1, K 8). Exact counts of the
flip (previous default -> both):

| program | previous | region + hoist | change |
|---|---|---|---|
| rust-compression | 44,095,655 | 42,710,634 | -3.14% |
| rust-html-rewriter | 101,452,418 | 100,768,438 | -0.67% |
| pulldown-cmark | 51,914,147 | 51,375,285 | -1.04% |
| regex | 50,528,248 | 50,088,239 | -0.87% |
| rust-json | 51,478,425 | 51,373,365 | -0.20% |
| rust-protobuf | 65,262,081 | 64,912,380 | -0.54% |
| blind-sig | 34,687,507 | 34,691,980 | +0.01% (noise: +-35k between identical builds, random keys) |
| blake3-scalar | 49,040,376 | 49,046,430 | +0.012% |

blake3's +6,054 is `compress_chunks_parallel`, two reloads at the outer
loop's exit edge per call (3,032 calls): two values the cut puts on the
stack inside the loop and back in a register after it, where the previous
default kept one in `x23` throughout. `PLIRON_RA2_REGION_TIE=1` (opt-in;
`region_tie` in `src/lib.rs`, the sort in `regionsplit.rs`) breaks
ties of equal gain toward the register free in more blocks, so a
callee-saved register free across the loop would be cut instead of the
caller-saved ones probed first. Stays opt-in (not remeasured on blake3).
Checker: `clif-check codegen` with
`PLIRON_RA_CHECK=1` over smoke (0 failures after the spillhoist fix
below), std every 4th (same pre-existing compile error as without),
modes 1 and 2.

**2026-11-18 spillhoist fixes (two).** (1) With `PLIRON_RA2_SRCVREG`
(default on), block-parameter edge moves are labelled with the branch
argument. Spillhoist treated those `reg → param-home` stores as spill
stores of the argument and deleted them, so smoke's `neonpair` `main`
failed the checker (`IncorrectValuesInAllocation` at Inst 128: `p1i`
held `v569` instead of `v332`). Fix: mark block-parameter edits
(`InsertedMove::blockparam` / `Edits::blockparam`) and skip them in
spillhoist. Stanza `neonpair` builds with `PLIRON_RA_CHECK=1`. (2) A
definition allocated to a "stack preg" (the fuzzer's memory placeholder)
must not become the `from` of a hoisted store: that is a stack-to-stack
move. Spillhoist now uses the same `is_reg` test as `moves.rs` (preg and
not `pregs[i].is_stack`). `rafuzz` seeds 1000000..1000500: 19
`StackToStackMove` failures under hoist alone -> 0.

## `PLIRON_RA2_SPILLHOIST` (runtime, default on since 2026-11-18): one spill store at the definition

`src/ion/spillhoist.rs` (new), called from `ion::run` before `postopt`;
`src/lib.rs` (`spillhoist_on`). regalloc2 stores a value to its slot at
every register-to-stack boundary: every split point, every edge from a
register region into a stack region (the region split stores on each
entry into a call-heavy loop, inside an outer loop on every trip). The
values are SSA, so per vreg the stores to its slot (register-to-stack
edits whose `edit_vregs` entry names it, excluding block-parameter edge
transfers) are summed by block frequency and, when 1.5 x the defining
block's frequency is below that (the margin: split-point stores often
pair into `stp`, a lone store at the definition does not), replaced by
one store at `After(def)`, ahead of the other edits there. Sound because
the definition dominates every point the vreg is live at, a slot is
shared only between spillsets with disjoint extents (an extent covers
every path the vreg is live along) and the vregs of one spillset never
overlap. Block parameters and definitions on a block's last instruction
are left alone. Static, region split on: html-rewriter corpus weighted
stack accesses -1.6%, compression -0.9%; in
`BrotliCreateHqZopfliBackwardReferences` 5 stores replace 39. Exact counts
alone (allocator without region + hoist): compression -29k, pulldown
-12k, the others within +-3k (blind-sig noise). With the region split:
compression -15k, pulldown -24k. `PLIRON_RA_CHECK=1` clean (same runs as
above, after the SRCVREG fix).

## `PLIRON_RA2_MOVESTATS=1` (reporting only): the allocator's moves by cause

`src/lib.rs` (`movestats_on`, `MV_NAMES`, `MOVESTATS`,
`movestats_snapshot`, `movestats_take_local`), `src/ion/moves.rs`
(`InsertedMove::kind` set at every insertion site, `tally_move` after
redundant-move elimination), `src/ion/merge.rs` (why each blockparam merge
failed, `Ctx::bp_merge_kind`). Every emitted move is counted by kind:
split moves inside a block (plain, or at a fixed-register use / def),
same-vreg moves across a block edge, blockparam moves (merged before
splitting; merge refused for overlapping ranges, with `bp-ovl-br` when
the input is defined in a branching block; refused for requirements or
the pin rule; other), multi-fixed fixups, reused inputs; reg-reg and with
a stack side apart, plain and weighted by block frequency relative to the
entry (capped at 512). `clif-check codegen` prints the totals and puts
the weighted vector in each function's `mv`. Under `PLIRON_RA_EXPLAIN`
merge attempts print `bpmerge` / `merge overlap` lines.

The `bp-overlap` failures are further sub-classified
(`src/ion/merge.rs`, after the failed `merge_bundles` call): a
two-pointer scan over the two bundles' ranges tells whether the input
conflicts only with the param's own ranges or with third-party vregs
already merged into the to-bundle, and where the input is defined —
`bp-ovl-3rd` (third-party only; `bp-ovl-mix` both), `bp-ovl-eb`
(defined in a multi-successor predecessor of an edge stub),
`bp-ovl-in` (defined in the edge block itself), `bp-ovl-br` /
`bp-ovl-ss` (defined in the branching / single predecessor of a stub),
plain `bp-overlap` (defined upstream of those). std corpus
(2026-11-23, weighted reg-reg): `bp-ovl-3rd` 44.9k, `bp-overlap`
17.2k, `bp-ovl-mix` 15.6k, `bp-ovl-eb` 13.0k — the third-party
conflicts are merge-order artifacts a later merge cannot see, not
fixable by `MERGEDEPTH` order (measured: 4/5 flat, 6 trades reg-reg
for stack movs net worse).

html corpus (2026-11-17), reg-reg count / weighted: split 12.6k / 14k,
split-fixed 16.3k / 25k, xblock 17.5k / 20.5k, bp-merged 7.4k / 15.6k,
bp-overlap 4.2k / 49.6k (42.5k of it one function,
`encoding_rs::mem::check_str_for_latin1_and_bidi`), bp-ovl-br 4.4k / 7.1k,
bp-req 0.7k / 2.2k.

## `PLIRON_RA2_MERGEDEPTH=4|5|6`: merge order by frequency / range length

`src/ion/merge.rs`: 4 orders blockparam edges by `hot_bonus` (powers of
two, hottest first) then shortest input range, 5 by `hot_bonus` only, 6
by shortest input range only. Static html corpus loop-weighted
instructions: base 6.970M, `=2` 6.882M, `=4` 6.910M, `=6` 6.887M. Exact
counts are mixed (pulldown-cmark +2.30% / -0.17% / +1.37% for 2/4/6,
regex +1.87% for 4, rust-compression -0.22% / +0.11% / -0.58%): the
greedy merge order is not a lever, none is a flip candidate.

## `PLIRON_RA2_COPYHINT=1|2` (opt-in): copy hints

`src/ion/hints.rs` (new; `copy_hint`, `build_copy_hint_index`),
`src/ion/process.rs` (`process_bundle` takes the hint), `src/lib.rs`
(`copyhint_mode`). A bundle about to be allocated weighs the registers of
its move neighbours already allocated (abutting ranges of its vregs,
blockparam edges in both directions) by block frequency and tries the
heaviest first, as LLVM's greedy allocator orders candidates by copy
hints. `1`: only for a bundle without a spillset hint; `2`: in place of
it. Static html corpus, `=2`: loop-weighted moves 716k -> 675k,
instructions 6.970M -> 6.919M; `=1` almost nothing.
  2026-11-19: exact interference, reserve-and-retry temps, ion delegation over
  `PLIRON_RA_MCTS_FALLBACK`, `PLIRON_RA_TIME`: `docs/history/2026-11-19-ra-mcts-whole-function.md`.

## `PLIRON_RA2_POTTS` (2026-11-19; default `1` since 2026-11-20, `0` off): relabel registers block by block to remove edge moves

`src/ion/potts.rs`, called from `Env::run` after `rehome_joins`. Per value,
an α-expansion over the blocks it lives through (Potts energy: edge
frequency where the two ends of an edge disagree), each step a minimum
cut (`regionsplit::Flow`, now `pub(crate)` with `to_cap`); a relabelled
block's piece gets its own bundle, as in `rehome.rs` (whose
`rehome_lr_at` / `rehome_split` are now `pub(super)`). `=1` takes only
Pareto steps (no edge of the value gains a move); `=2` trusts the block
frequencies. `PLIRON_RA2_POTTS_MAXLIVE` (live-in budget per function),
`PLIRON_RA2_POTTS_LIMIT=N` (first N relabelled values, for bisecting).
Unit test `expansion_matches_brute_force`.

`PLIRON_RA2_POTTS_COPY=1` (2026-11-20): `potts_copies` makes register
copies identities. New `Function::pliron_move(inst) -> Option<(src, dst)>`
(default `None`; Cranelift answers it from `MachInst::is_move`, the fuzz
`Func` for every one-use one-def plain instruction under `Hints::copies`).
For a copy whose two sides differ, one side that is a single plain live
range defined by an instruction (no block parameter, no branch argument)
takes the other side's register when free over the range; kept only when
no copy of that range stops being an identity. `pliron_potts_counts()`
reports how many values / copy ranges were relabelled (rafuzz prints it).

## `PLIRON_RA2_MAXSPLITS=<n>` (default 4 since 2026-11-20, upstream 2)

`src/lib.rs` (`max_splits`), `src/ion/process.rs`: upstream's
`MAX_SPLITS_PER_SPILLSET` (2) as a knob. Past it, a bundle that still
conflicts goes to minimal bundles (a reload at every use). aho-corasick's
`leftmost_find_at_no_state` takes its end bound in `x3`; two loop splits
used the budget, the third (a call at the entry clobbers `x3`) fell back to
minimal bundles, and whether the hot latch reloaded the bound depended on
which register a temporary took. At 4 the loop keeps the bound and the
automaton pointer in registers (loop-weighted stack accesses 181 -> 133).
rafuzz: 400k seeds at 4 and 8, no failure.

## `PLIRON_RA2_RELOADCOPY` (default on since 2026-11-23, `=0` off)

`src/ion/postopt.rs` (`reload_into_copy`), `src/lib.rs`
(`reloadcopy_on`): after the other post-allocation cleanups, an edit that
fills `r` just before a register copy `mov d, r` (`Function::pliron_move`)
fills `d` instead, and the copy reads `d`, when nothing reads `r` before it
is written again in the block, no later edit at that point touches `r` or
`d`, neither copy operand is pinned and neither register is a fixed stack
slot (`PRegData::is_stack`; the first version broke two rafuzz seeds,
82467 and 93016, on such a slot). Cranelift drops the resulting `mov d, d`.
A spilled call argument was reloaded into a scratch register and copied
into the argument register (`ldr x11, [sp, #k]; mov x1, x11`):
rust-compression 776 such pairs -> 156 (LLVM 142); std corpus -801
instructions, -2.2% frequency-weighted copies; checker clean.

## `PLIRON_RA2_EVICT` (2026-11-25, default on, `=0` off): evict merge-order squatters from a blockparam bundle

`src/ion/merge.rs` (`bp_merge_evict`), `src/ion/requirement.rs`
(`compute_requirement_skip`), `src/ion/data_structures.rs`
(`LiveBundle::set_cached_flags`), `src/lib.rs` (`evict_on`,
`evict_ratio`, `evict_cap`, `EVICTSTATS`). A blockparam merge can be
refused only because the input's ranges overlap *other* vregs that
earlier merges pulled into the param's bundle (the `bp-ovl-3rd`
census kind — the dominant refusal, ~45k weighted on the std corpus).
Since those vregs are mutually non-overlapping, they can go back out
together: move every conflicting vreg's ranges into one fresh bundle
with a fresh spillset, then retry the merge. This undoes a merge-order
artifact instead of reordering attempts (MERGEDEPTH measured dead).

An eviction is refused when the conflicts include the param itself, a
blockparam (param-in merges would cascade), or a reuse-tied vreg, and
when the incoming edge is not `PLIRON_RA2_EVICT_RATIO` (default 2)
times hotter than the arg edges whose moves the eviction restores
(`arg_cost[v]` sums each conflicting vreg's `blockparam_outs` edge
frequencies; it over-counts by including edges whose merges failed, so
the test is conservative). `PLIRON_RA2_EVICT_CAP` (default 4) bounds
the conflict set. The merged `Requirement` is pre-checked with the
conflicts skipped so a requirement refusal doesn't strand evictions;
both bundles' cached fixed/def/stack flags and spillset extents are
recomputed (merge_bundles skips its overlap scan on flagged bundles).
`EVICTSTATS` = [merges recovered, vregs evicted, retries still failed],
printed by clif-check under the knob; the `ra2x: bp-evict:` explain
line names them.

`PLIRON_RA2_EVICT_SPAN` (default 4, `=0` off) vetoes one eviction case
the edge-cost test cannot see: a squatter whose to-bundle ranges cover
a block more than EVICT_SPAN times hotter than the merge edge is never
evicted. The ratio test compares the edge's weight against the moves
eviction restores, but an evicted vreg must also hold a register alone
through every block its ranges cover — when one of those blocks is a
hot loop and the fresh singleton bundle goes minimal, the spill and
per-use reloads land inside it. `bp_merge_evict` computes each
candidate's hottest covered block weight (`hot_bonus` max over
`insn_block` of its to-bundle ranges — those are the ranges the
eviction moves) and refuses when it exceeds `EVICT_SPAN * incoming`;
refusals count as `hotspan` in `EVICTWHY`.
`PLIRON_RA2_EVICTDBG=1` (reporting only) prints each candidate's
arg-edge cost, the edge weight, and its span/use heat maxima. The
ratio was tuned on the held-out set: 4 beats both 2 and 8 on
rust-compression and pulldown-cmark.

Measured (EVICT on with SPAN=4 vs SPAN=0, exact instructions):
rust-compression recovers the whole eviction regression — 41,092,802
vs `PLIRON_RA2_EVICT=0`'s 41,093,028 on callgrind (the hot-loop
evictions in `BrotliCreateHqZopfliBackwardReferences` all refused) —
and beats or matches SPAN=0 on pulldown-cmark, blind-sig,
rust-html-rewriter and rust-protobuf, so the veto is a net win, not a
trade. Std
corpus: 365 evictions vs 402 without it (-9%), weighted codegen score
a hair better. The `evictspan` test.sh stanza pins core's
`Big64x20::mul_pow2` (five evictions under `SPAN=0`, none under the
default) via clif-check's evict table.

Held-out exact-instruction deltas of EVICT itself (non-nop):
pulldown-cmark -0.21%, rust-protobuf -0.10%, blind-sig -0.07%,
rust-html-rewriter -0.05%, blake3/regex flat, rust-json +0.003%
(noise); rust-compression was +0.10% until EVICT_SPAN (above).
rafuzz: 35k seeds mixed combos, clean.

## `PLIRON_RA2_MINCLUS` (2026-11-26, opt-in `=1`, `PLIRON_RA2_MINCLUS_SPAN` default 192): cluster nearby uses into one minimal-bundle reload range

`src/ion/process.rs` (`split_into_minimal_bundles`, `minclus_flush`,
`push_minimal_bundle`), `src/ion/data_structures.rs`
(`LiveBundle::minclus`, `Stats::splits_minclus`, shown as `minclus=`
in `ra2stats:`), `src/lib.rs` (`minclus_mode`, `minclus_span`).

Motivation: on blake3's `compress_in_place` we spill *less* than LLVM
but reload ~1.43x more — the minimal-bundle fallback cuts one tiny
bundle per use (a reload per use) where LLVM keeps a value in a
register across several nearby uses of the same vreg. With the knob
on, consecutive plain `Reg`-constrained uses of one vreg in one block
within `MINCLUS_SPAN` instructions of the cluster's first use form ONE
bundle spanning first use to last use; a single reload serves the
cluster when a register stays free across it (LLVM's wider
spill-reload ranges). Defs, `Any`-constrained and non-`Reg` uses break
the cluster and keep the per-use minimal bundle. A cluster bundle gets
its ordinary computed spill weight and stays non-minimal; if no
register is free over its span it splits at the first conflict and the
piece lands back in `split_into_minimal_bundles`, where its `minclus`
flag makes the uses plain minimal bundles — the same result as if
clustering had never run, so the retry terminates.

Why opt-in and why it cannot help `compress_in_place`: every candidate
cluster span there is already squatted by long state bundles whose
weight a cluster can never evict, so all 8 clusters split straight back
to per-use minimal (the produced binary is identical). A variant that
gave cluster bundles the minimal-bundle always-evict grade evicted the
squatters — and measured strictly worse on the function (88 ldr/23 str
vs 72/16 baseline): the evicted bundles' reloads just land elsewhere.
Register pressure there is real (~19 long live state vregs vs ~29
allocatable regs), so the gap is not reachable by this mechanism.

Measured: blake3-scalar exact instructions unchanged (all clusters
failed as designed); pulldown-cmark 46,398,014 -> 46,378,507 (-19.5k,
-0.04%); rust-html-rewriter 92,924,616 -> 92,918,460 (-6.2k, -0.007%);
std corpus (4,979 fns) -745 weighted stack accesses, -448 weighted
instructions, one addr2line regression (+306 weighted movs — a cluster
register picked up copies).

Fix (2026-11-27, rafuzz seeds 23300 and 11344, `assertion failed:
res.is_none()` in `try_to_allocate_bundle_to_reg`): the spill coverage
a minimal split adds to the spill bundle covers the whole split range
— including the span of every cluster just created, since the cluster
bundle is a live range of the same vreg inside it. Unlike a per-use
minimal bundle, a cluster bundle is *non-minimal* and re-enters the
splitting machinery when no register is free; its pieces then migrate
into the same spill bundle a second time (the re-split's own
whole-range coverage, a trimmed end) and overlapped the coverage
already there — two overlapping ranges in one bundle, which the
allocation btree forbids. The coverage is now cut around the cluster
spans (one spill range per piece instead of one for the whole range):
the span is owned by the cluster bundle alone, and every piece of it
that later lands in the spill bundle fills only its own part of the
hole, so the pieces stay disjoint. The invariant this restores is
upstream's: ranges migrate into a spill bundle only as freshly carved,
disjoint pieces. One move is new when a cluster keeps its register:
the slot no longer carries the value across the span, so the boundary
at the cluster's end inserts a store back into it (the reload at its
first use is unchanged) — needed because spill-slot sharing
(`PLIRON_RA2_SLOTLIVE`) may give the slot to another value across the
hole. Because `any`-constrained uses migrate into the coverage's use
list (and a use must attach to the one piece covering it), an entry
that has one is no longer clustered at all; such uses break clusters
anyway, so this only widens the break distance.
