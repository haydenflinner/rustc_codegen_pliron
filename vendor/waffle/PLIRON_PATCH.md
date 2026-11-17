# PLIRON patches to waffle 0.3.2

## `src/backend/localify.rs` — edge-transfer local coalescing

`Localifier::compute` gains a `compute_preferred()` step between
`find_ranges` and `allocate`. For every edge transfer (`BlockTarget`
arg `v` feeding target-block param `p`), each endpoint is recorded as
a coalescing candidate for the other (`preferred` map).

In `allocate`, when a value's live range starts and it has candidates,
the allocator first tries to take a candidate's already-allocated
local out of the type-matched freelist instead of popping an
arbitrary entry. Safety: a local only sits in the freelist when its
previous owner's range has ended, so preferring it can never merge
overlapping live ranges; the pairing only biases which *free* local
is chosen.

Effect: when an edge arg dies at the transfer (or the param died
before the arg's def, e.g. a backedge arg computed after the loop
param's last use), the arg and the param land in the same local and
the emitted `local.get L; local.set L` copy degenerates to a no-op
(removed by wpeep's existing get;set-same rule). This recovers a
small part of LLVM-style phi/copy coalescing that the type-freelist
linear scan previously missed.

`PLIRON_WCOAL=0` disables it (stock freelist order).

## `src/backend/localify.rs` — same-block param steal

`compute_steal()` runs after `compute_preferred`. For an edge
transfer arg `a` → param `p`, it records `a -> p` in `steal` when:

- `a`'s def is an instruction in the same block as the edge,
- `p`'s live range outlives `a`'s (`a.end <= p.end`), so `p`'s local
  cannot be recycled while `a` still needs it, and
- no use of `p` follows `a`'s def in that block — instruction args
  and all terminator args (every edge, including `cond`), and
- `p` is not live-out of the block: `a`'s write lands before the
  branch and dominates every outgoing edge, but only the edge to
  `p`'s own block rewrites the local — a sibling edge reading the
  old `p` would observe the clobber (br_table to a shared
  loop-header block vs a `)`-handler edge that still reads `i`).

In `allocate`, a single-typed value with a steal candidate whose
param's local is still owned by `p`'s range (`owner` map) takes that
local outright; nothing is pushed to `expiring` for the slot since
`p`'s range continues to own it.

Soundness: `p`'s uses outside this block can only be reached through
edges into `p`'s block, which rewrite the local — and the live-out
check rejects any `p` another successor edge could still read —
so writing it early inside the block is unobservable; within the
block the no-late-use scan rules out a stale read. This is the loop-carried update case:
`i+1` computed in the latch takes `i`'s local before the backedge,
turning `get i; iadd; tee tmp; set i` into `get i; iadd; set i` —
LLVM's destructive-update shape.

`PLIRON_WCOAL=0` disables it (stock freelist order).

## `src/backend/localify.rs` — lifetime-hole recolor (`PLIRON_WCOAL_HOLES=1`)

`coalesce_holes()` runs after `allocate`. The freelist colors
linearized reverse-RPO intervals, so a value live in B_early and
B_late looks live through every block between them in that order
even when `block_end_live` says it is dead there — a short-lived
value whose true range sits only in such a hole then gets a fresh
local. The pass rebuilds a local-interference graph from precise
per-block SSA liveness and greedily recolors same-type locals
(entry-param indices stay fixed). On kernels-wasm: 764→636 locals
(−17%), CODE −345 B. Default on; `PLIRON_WCOAL_HOLES=0` or
`PLIRON_WCOAL=0` kills.

## `src/backend/stackify.rs` — SelectJoin / brif-fork / WBRIF

- `PLIRON_WSEL` (default on, `=0` off): same-join CondBr →
  `SelectJoin` (per-param `select` + `br`) instead of `if`/`else`.
- `PLIRON_WBFORK=1`: different-join CondBr → `block`+`br_if` fork.
  Removes if/else but **grows** CODE (~+7 KiB regex); opt-in only.
- `PLIRON_WBRIF` (default on, `=0` off, `=exit` both-exit Ifs only):
  post-pass If→block+br_if. Grows CODE. With wasm `SWITCH_GCC`
  limited to 2-case switches, parse_only is ≤1× LLVM and
  tokenize keeps its −17% (geomean 0.921).
- `PLIRON_WASM_COLDBR=1`: force `FunctionBody.cold` (CLIF cold marks)
  and short CondBr→join error tails into `merge_nodes`, so stackify
  emits a labeled `br` instead of inlining them into an `if` arm.
  Block-arg moves for the join then live inside the cold region; the
  hot edge stays bare (LLVM's shape for `Result::Err` / `len == 0`).
  Default off: parse_only A/B 2026-11-14 was neutral (±0.5%).

## `src/backend/mod.rs` — BlockParams identity skip + SelectJoin

Skip `get L; set L` identity blockparam transfers at emit. Lower
`SelectJoin` as true/false/`select`/`set` per param.

## `src/backend/treeify.rs` — edge-arg remat (`PLIRON_WTREEREMAT=0` off)

Pure single-use edge-only values rematerialize at the use instead of
`local.set`+`local.get`. Cheap unary ops (eqz/wrap/extend/…) with ≤2
uses also remat (`PLIRON_WTREEREMAT_UNARY=0` off). Regex-wasm CODE
404→385 KiB (−19 KiB); with SOLO=15+TINY=6 → 376 KiB; +`WIFRET` typed
single-param if results → 375 KiB (1.79×). `PLIRON_WIFRET=0` off.
Multi-param typed if (`PLIRON_WIFRET_MULTI=1` + `BlockTypeIntern`) is
wired but default-off — grew regex-wasm CODE in 2026-10-28 trials.
`PLIRON_WBRIF=exit` rewrites only both-exit Ifs to block+br_if (still
grows CODE vs off; full `=1` grows more). Label bump for the wrapper
block is depth-aware: only escaping `br`/`br_if`/`br_table` depths move
(+1); a nested `br 0` stays (blind bump miscompiled hmget/sortw/
parse_only under `=exit`).

`PLIRON_WTREEREMAT_CONDARG` (2026-11-16): a single-use value passed
to a target of a `CondBr`/`Select` is not rematerialized on that edge
when the edge stays inside the branch block's loops and is not a back
edge. The remat made a move-only arm (compiled by V8 as a branch
straight to the join) into an arm with code and its own jump. Loop
exits and back edges keep the remat (matmul's outer-loop steps on the
inner loop's exit; `i + 1` on the latch's back edge measured 2% better
than ahead of the test). parse_only -0.7% -> -11% against LLVM,
module +200 B. `=1` restores the remat on every edge.

All changes are marked `// PLIRON:` in the source.

## `src/backend/treeify.rs` — loop-nest guard (`PLIRON_WTREE_LOOP=0` off)

`Trees::compute` takes the `CFGInfo` and computes natural
loops (`LoopNest`: backward walk from each back-edge). A single-use
value is not treeified into a consumer that sits in a loop the def is
outside of: emission would re-run the def every iteration, undoing a
CLIF licm hoist (hmget's SipHash key-only half-round). Such a use marks
the value `multi_use`, so no later use can own it either.

## `src/backend/treeify.rs` — single-use load sinking

`is_movable` gains a `loads_ok` parameter so that a single-use memory
load can be recorded as owned by its consumer's operand tree, the
same as a pure op. Emission then places the load inline at the
ultimate consuming root's position — LLVM's wasm backend emits this
operand-stack shape — instead of a `local.set`/`local.get` round
trip per load.

Sinking reorders the load in program order, so a post-pass
(`validate_load_sinks`) reverts ownership unless the consuming root
is in the load's own block and every inst strictly between def and
root is pure or another load:

- Stores, calls, fences, divides, and any other effectful or
  non-load trapping op between def and root block the sink — they
  could clobber the loaded memory, diverge, or change which trap
  fires first.
- Other loads between are fine: loads don't write memory, and wasm
  OOB traps are indistinguishable, so swap order is unobservable.
- The same-block requirement prevents executing the load on CFG
  edges that never reached its def.

On by default; `PLIRON_WTREELOAD=0` disables it.

A load is never owned by a rematerialized tree (2026-11-16). A value in
`remat` (a constant, an edge-only pure value, a cheap unary with at most
two uses) is emitted again at each use, so a load inside its operand
tree would run at every use position, wherever those are; the
def-to-root check above only describes a tree emitted once. Unoptimized
code hit it: `x = wrap(load slot); slot = f(x); y = x - ..` re-read the
slot after the store (itoa printed 99 as "00"; every unoptimized dsa
page failed). `validate_load_sinks` now drops such loads from the tree
(they get a local), and also loads whose owner chain is longer than
its 64-step walk. Regression: `tests/wasm/smoke/raw.rs`, compiled with
`PLIRON_OPT_BISECT=0`.

## Shared `br_table` edge blocks (`PLIRON_WASM_BRTAB_SHARE`, default on since 2026-11-17, `=0` off)

`backend/stackify.rs` `do_branch_select` wraps the `br_table` in one
`block` + parameter transfer + `br` per table entry and per default, so
a table whose entries repeat a few targets pays one wrapper per entry
(dsa's `Compiler::compile`: 35 entries, 35 distinct depths; LLVM's
93-entry table for the same match has 4). `do_branch_select_shared`
gives entries with the same target and the same argument values one
wrapper, and an entry that passes no arguments names its target's label
directly (plus the wrappers around the table). Read once per function
(`Context::brtab_share`). Test: `tests/brtab_share.rs` (compiles a
7-entry table both ways, re-parses, interprets every selector).

## Cut-block aliases in the reducifier's max-SSA (`PLIRON_WASM_MAXSSA_CUTALIAS`, default on, `=0` off)

`passes/maxssa.rs` gives a cut block (an irreducible loop's side entry
or exit, `backend/reducify.rs`) a parameter for every value live across
it and never turns one into an alias, because the predecessors may be
copies of a duplicated region in which a value defined inside the
region has another name. A value defined in a block that is never
copied has one name in all copies, so a parameter every edge passes it
for carries nothing. `run_with_clones` takes the blocks that may be
copied (a superset: blocks whose header set meets an irreducible
header; a copy is made only in a non-empty context, and contexts hold
only irreducible headers of the block's header set) and applies the
all-same alias rule at cut blocks for values defined outside them.
`remove_trivial_params` then removes parameters that carry one value
around a cycle (Braun et al.'s redundant-phi sets: a strongly connected
set of parameters whose incoming values from outside the set are one
value is that value; Tarjan, linear), which the per-use walk cannot see
because it decides each parameter before its cycle is closed. Tests:
`reducify.rs` `cut_alias_drops_outer_params` (parameter count) and
`tests/maxssa_cut.rs` (semantics through compile, re-parse and the
interpreter; a version without the "defined outside the copies" check
fails it with invalid IR).

## Tests run in place (`[workspace]`, `Cargo.lock`)

`Cargo.toml` ends with an empty `[workspace]` table, so the crate is its
own workspace root (the repo's workspace excludes `vendor/`; without the
table cargo walked up to it and refused). The lockfile, ignored by the
upstream `.gitignore`, is committed (`git add -f`) and the tests run
`--locked`: `cargo test --manifest-path vendor/waffle/Cargo.toml
--target-dir target/waffle-test`, job `unit:waffle` in `harness/gate.py`
and in `harness/quick.py` for `vendor/waffle/` changes. The backend still
builds the crate as a path dependency from the repo's `Cargo.lock`.

## `src/backend/localify.rs` — edge-copy coalescing (`PLIRON_WCOAL_EDGE (default on since 2026-11-17, `=0` off)`, opt-in)

`coalesce_edge_copies`, called by `coalesce_holes` on its precise local
interference graph before the greedy recolor: for each edge transfer
`arg -> param` (one local each, same type, the argument not a tree or a
rematerialized value) whose two local classes do not interfere, merge
the classes (union-find; the representative carries the union of the
members' interference; entry-param locals keep their index and never
merge with each other). The recolor then colors classes. The emitter
already skips a transfer between equal locals. Sound under the same
model as the recolor: transfers are emitted on their own edge, reading
every argument before writing any parameter. Counters
(`waffle::EDGE_COPIES`, `EDGE_NONINTERF`, `EDGE_MERGED`) feed the
backend's `PLIRON_WASM_COALSTATS=<file>` report. Payoff measured small:
of 24,273 edge copies in the kernel module's objects 560 pairs were
non-interfering when checked (2,801 transfers ended up between equal
locals); `mimas-compile` 406 of 19,843. Test: `tests/wcoal_edge.rs`
(rotating loop, a parameter read after the next value is computed, a
three-edge join; knob on and off through the interpreter; merging
without the interference check fails it).

## `src/backend/mod.rs` — constants in locals (`PLIRON_WASM_CONSTLOCAL=1`, opt-in)

`treeify::is_remat` rematerializes every constant at each use, and the
CLIF emitter makes one IR value per use, so a long constant is spelled
out again every time: the hashbrown group mask `0x8080808080808080` is
an 11-byte `i64.const` (542 KB of them in the 2026-11-16 dsa module,
138 KB in LLVM's). `const_locals`, run after the localifier, groups the
function's rematerialized constants by (type, bits), counts their
emitted uses (operator arguments and terminator uses), and gives a
group a new local when `n * c > c + set + n * get + 2` (the immediate
form against one `const; local.set` at function entry, a `local.get`
per use and the local's `(1, type)` declaration), best saving first so
the most profitable groups get the shortest indices. The prologue goes
before the body's first instruction: the entry dominates every use and
nothing else writes the new locals. `lower_inst` reads the local for a
mapped value. Kernel module 165,438 -> 162,902 bytes (-1.5%), timing
unchanged under `--perturb=4`. Test: `tests/const_local.rs` (long
`i64`/`i32`/`f64` constants in the entry, a loop, both arms of a branch
and an edge argument; knob on and off through the interpreter; the body
must shrink; dropping one prologue `local.set` fails it).

## `src/backend/stackify.rs` — straight-line code into a following `block` (`PLIRON_WASM_BLKHOIST (default on since 2026-11-17, `=0` off)`, opt-in)

`hoist_into_blocks`, the last step of `Context::compute`: the run of
leaves and local-only parameter transfers right before a `block` moves
to the front of the block's body, recursively, never into a `loop`.
Entering an empty-typed `block` does nothing and the moved items hold no
branch, so meaning and label depths are unchanged. Cause, confirmed on
the kernel module: `rewrite_ifs_to_brif` (`PLIRON_WBRIF`, default on)
replaces `Leaf; If` with `Leaf; block; block; br_if ..`, so a value the
leaf sets for the condition or an arm is read across the two openers
(`local.set X; block; block; local.get X`, which `wpeep` cannot fuse).
Counts on the kernel module: 1,010 such sites by default, 96 with
`PLIRON_WBRIF=0` (the merge-node openers account for few), 91 with the
knob (90 of them cross a `loop` opener, which stays). Module 165,438 ->
162,895 bytes (-1.5%). Transfers with an empty side (typed-`if` results
on the operand stack) stay put. Test: `tests/blk_hoist.rs` (a diamond
whose condition and arms read the entry leaf's value, and the same
inside a loop; knob on and off through the interpreter; with the knob no
value is parked across openers; moving the run to the end of the block
instead fails it).
