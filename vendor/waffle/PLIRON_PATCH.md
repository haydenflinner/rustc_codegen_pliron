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
- `PLIRON_WBRIF=1`: post-pass If→block+br_if; also grows CODE ~16%.

## `src/backend/mod.rs` — BlockParams identity skip + SelectJoin

Skip `get L; set L` identity blockparam transfers at emit. Lower
`SelectJoin` as true/false/`select`/`set` per param.

## `src/backend/treeify.rs` — edge-arg remat (`PLIRON_WTREEREMAT=0` off)

Pure single-use values used only as a terminator/edge arg rematerialize
at the use instead of `local.set`+`local.get`. Regex-wasm CODE
404→385 KiB (−19 KiB, 1.93→1.84×).

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
