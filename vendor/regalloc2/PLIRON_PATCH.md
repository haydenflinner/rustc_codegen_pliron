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

## `Function::is_remat` (opt-in through Cranelift's `PLIRON_RA2_REMATW=1`)

`src/lib.rs` (trait hook, default `false`), `src/ion/process.rs`
`recompute_bundle_properties`: a bundle whose every range's vreg is
rematerializable takes `total / prio / 2` as its spill weight (capped
like the others) instead of the long-range sqrt normalization, as LLVM
halves the weight of rematerializable intervals.

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
