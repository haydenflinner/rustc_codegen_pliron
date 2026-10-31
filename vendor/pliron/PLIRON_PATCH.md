# Why pliron is vendored

Vendored from upstream pliron at the revision in `vendor/PLIRON_REV`, built
with its `llvm-sys` feature off. The changes are a memory-layout rewrite for
compile time and peak memory. Each is marked `PLIRON:` in the source
(`grep -rn PLIRON: vendor/pliron` lists them all); by theme:

- **Context-global pools** (`src/operation.rs`, `src/basic_block.rs`,
  `src/context.rs`): an operation's results, operands, successors and
  regions, and a block's arguments, are `(offset, length)` spans into flat
  pools on the `Context` instead of per-op `Vec`s. `PoolIter` re-borrows a
  span without allocating; whole-list rewrites (`replace operands`, rebuild
  results/successors) append a fresh tail span.
- **Intrusive use lists** (`src/value.rs`, `src/context.rs`): uses live in a
  Context-wide arena of `UseSlot`s with a free list; a definition's uses are
  a linked list through that arena.
- **Dense links and dispatch** (`src/linked_list.rs`, `src/context.rs`):
  next/prev pointers of the op and block lists move to dense side tables
  (`NodeLinks`); `op_kind` is a dense table giving each op's concrete-type
  dispatch pair and `TypeId` without a hash lookup.
- **Interned attributes** (`src/attribute.rs`, `src/builtin/attributes.rs`,
  `src/builtin/given_names.rs`, `src/builtin/ops.rs`,
  `pliron-llvm/src/ops.rs`): attribute dictionaries hold `AttrInstId` handles
  into `Context::attr_store`, deduplicated on hash and equality (closer to
  MLIR's uniqued attributes). Interned attributes are immutable; an edit
  clones, mutates and re-interns.
- **Compact locations** (`src/location.rs`): `Location::Dbg(u32)` indexes
  the embedder's debug-location table (`State::dbg` here), so every op is
  stamped without allocating; ops and blocks store a 16-byte `CompactLoc`.
- **Dense side tables** (`src/cols.rs`, new module): `Vec`-indexed maps and
  sets keyed by arena ordinals, with `retain`. `rustc_codegen_pliron`'s
  `State` tables (`ColMap`) are built on them.
- **Cloning** (`src/irbuild/cloning.rs`): value and block maps use dense
  ordinal tables up to a span limit, hash maps beyond it.
- **Interface casts** (`src/utils/trait_cast.rs`): the registered
  `(from, to)` pairs can be enumerated, so interface `TypeId`s are
  precomputed once.
