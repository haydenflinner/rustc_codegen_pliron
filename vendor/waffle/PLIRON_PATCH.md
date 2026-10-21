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

All changes are marked `// PLIRON:` in the source.
