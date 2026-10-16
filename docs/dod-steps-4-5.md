# Data-oriented refactor: steps 4–5 (handoff brief)

Status: queued after steps 1–3 of the DoD plan. Read `docs/performance.md`
("Wall decomposition") first for where the time actually goes.

## Landing order (prerequisites first — each compiles and ships alone)

1. **R1: field→accessor facades.** `Operation`'s SmallVec/attr fields are
   `pub(crate)`-accessed directly at hundreds of sites across
   vendor/pliron, pliron-llvm, pliron-ir-opt, clif-lower, src. Route every
   `d.results`/`d.operands`/`d.successors`/`d.attributes` read through
   accessor methods (`fn operands(&self) -> &[Operand]`). This is the big
   mechanical diff — it turns step 5 into "change ~5 accessor impls".
   Expect ~zero perf delta; verify with the harness.
2. **R2: ordinal accessors.** One documented `Ptr::ord() -> u32`
   (ArenaIndex slot index = low 32 of `as_ffi()`) + confirm `Value::val_uid`
   density. All column indexing goes through these.
3. **R3: `State` accessor layer.** `st.calls(op) -> Option<&CallInfo>`
   style — rename probe sites once; the storage swap is then free.
4. **R4: consolidate the existing ad-hoc dense tables**
   (`op_links`/`block_links`/`op_kind`, PLIRON-marked) into one
   `OpTable`/`BlockTable` column struct — step 5's span columns land there.
5. Step 4 below, then step 5a (flat pools), then 5b (attr flattening on
   the AttrInstId arena from `3d06c3d`).

## Step 4 detail

Value-keyed maps → `Vec<Option<T>>` by `val_uid` (grow-on-write).
Op-keyed maps → `Vec<Option<T>>` by op ordinal, sized once at the CGU
scan that populates `State` anyway; deleted-op tombstones are fine, never
compact. String-keyed: check whether pliron `Identifier` is already an
interned u32 — if so `Vec`-indexed columns almost for free, plus an
insertion-order `Vec<Identifier>` where emission order matters
(`funcs`/`globals`).

## Step 5 detail

- `Operation` ~100B → ~48B row `{self_ptr, kind: u16, operand_span,
  result_span, succ_span, region_span, attr_span, block_links, loc}` in
  the `OpTable` (R4); `concrete_op` dispatch via the kind column.
- Global flat pools `operands_flat`/`results_flat`/`succs_flat`/
  `regions_flat` + per-op `(u32 off, u16 len)` spans; reads become slices.
- Mutation: same-len operand writes go straight into the pool (most
  peepholes); length changes append a new span, orphaning the old —
  CGU-scoped lifetime bounds the garbage; compaction only if it matters.
- Attrs → flat `Vec<(kind_id, AttrInstId)>` span, insertion-ordered
  (print order must not change — dumps/tests compare text), linear
  scan ≤4; capability bitmask per kind replaces `attr_impls` dyn casts.
- Sequence: dual-write pools at op construction → flip readers inside the
  accessor impls (R1 makes this localized) → writers last → delete
  SmallVec fields.

## Expected payoff (honest)

- Step 4: ~3-6% CPU (state probes → array index).
- Step 5a: kills per-op alloc traffic (~2.7% malloc/free) + ~2× walk
  cache density (48B rows vs ~100B) — est. 8-15% of pass CPU.
- Step 5b: ~1-2% residual (AttrInstId interning + callee memoization
  already took most attr-read cost).
- Combined ~1.3-1.6× compile-time. **The real prize is the unlock:**
  columnar `&Context` reads become Sync-shareable → per-fn parallelism in
  phaseA/iropt without `-Zthreads` — the only remaining 4-8× lever on
  the default serial-CGU path.

Baseline for whoever picks this up (x86-64 box, 8 cores, pinned
nightly-2026-10-06):

- `regex-syntax -O`: LLVM ~2.9-3.0s vs pliron ~15.5s default,
  ~6.0s with `-Zthreads=8 -Ccodegen-units=4`
- `syn -O`: LLVM ~3.8s vs pliron ~102-122s default,
  ~24s with `-Zthreads=8 -Ccodegen-units=2`
- perf top leaves (syn CU2): `Uses::new` 2.8%, `free`+`malloc` ~2.7%,
  `inline_call` 1.5%, then a flat tail — nothing >1.5%. The cost is
  per-node overhead across ~95 passes, not a hotspot.

## Step 4: `State` side tables → dense columns

`pliron-ir-opt/src/state.rs` `struct State` is ~18 hash maps populated in a
per-CGU scan and probed by every iropt pass and by lowering.

Current key shapes and the target representation:

- **Value-keyed** (`consts`, `allocas`, `promoted`, `escape_copies`,
  `frozen`, `noalias`): `Value` is `{val_uid: u64, defining_entity}` —
  uids are dense per Context. Replace with `Vec<Option<T>>` indexed by
  `val_uid as usize` (grow-on-insert), or `SparseSet`. Lookup: one bounds
  check + load vs today's hashbrown probe. Kill the `Eq`/`Hash` impls on
  the lookup path.
- **Op-keyed** (`calls`, `intrinsics`, `rmw`, `invokes`, `volatile`,
  `nonnull`): `Ptr<Operation>` wraps `ArenaIndex` (slotmap key).
  `key.data().index()` (or the `ffi()` low 32 bits — see
  `vendor/pliron/src/context.rs:51`, already used for `op_links`/
  `block_links`/`op_kind` dense tables) gives a dense op ordinal.
  `Vec<Option<T>>` indexed by ordinal, allocated once at scan time when
  op count is known. Do NOT shrink on op deletion — tombstones are fine;
  passes already tolerate stale entries or check liveness separately.
- **String-keyed** (`funcs`, `globals`, `nowrite`, `willreturn`,
  `nocapture`, `nnret`, `strs`, `llvm_stubs`): intern to `SymId(u32)` via
  the existing `ident_to_sym`/`sym_to_ident` interner, then dense
  `Vec<T>` by SymId. `funcs`/`globals` are `FxIndexMap` because emission
  order matters — keep an insertion-ordered `Vec<SymId>` alongside for
  iteration.
- Keep the public field names as accessor methods
  (`fn calls(&self, op) -> Option<&CallInfo>`) to limit call-site churn;
  there are ~100+ `st.calls.get`/`st.funcs[` sites across ir-opt,
  clif-lower, and src.

Expected win: every fact probe in every pass + lowering drops from a
hash probe to an array index. Modest per-site, broad coverage — this is
"sum to several %" territory, and it *unblocks* step 5 and read-shared
parallelism (a `Vec<T>` read under `&State` is `Sync` where
`FxHashMap`... is too, but the Value/Ptr ordinal columns are what
phase-parallelism wants).

## Step 5: flat op pools + per-kind payload arenas

`vendor/pliron/src/operation.rs` `struct Operation` today (~100B before
spill):

    self_ptr: Ptr<Operation>
    concrete_op: ConcreteOpInfo        // interned per op-kind metadata
    results:     SmallVec<[OpResult; 2]>
    operands:    SmallVec<[Operand<Value>; 4]>
    successors:  SmallVec<[Operand<Ptr<BasicBlock>>; 2]>
    block_links: BlockLinks
    attributes:  AttributeDict          // SmallMap<Identifier, AttrObj, 1>
    regions:     SmallVec<[Ptr<Region>; 1]>
    loc:         Location

Target layout (columns indexed by op ordinal, same indexing as step 4):

- `op_kind: Vec<u16>` — discriminant dispatch (replaces ConcreteOpInfo
  hashing on the hot path)
- flat pools: `operands_flat: Vec<Value>`, `results_flat: Vec<Value>`,
  `succs_flat: Vec<Ptr<BasicBlock>>`, `regions_flat: Vec<Ptr<Region>>`
  with per-op `Vec<(u32 off, u16 len)>` span table — kills every
  SmallVec header inline + every spill alloc. `op.operands(ctx)` becomes
  `&operands_flat[span]` — a slice, not a SmallVec deref.
- `attrs` → flat `Vec<AttrId>` spans (needs step 3's `AttrId` interning;
  v0 = intern without dedup). `attr_impls`/interface checks become a
  per-`AttrId` capability bitmask built at intern time — kills the
  `Box<dyn Attribute>` vtable + dynamic_cast per query.
- `loc` → a `Vec<Location>` column (or drop for non-debug builds if we
  don't need it in release codegen — check `PLIRON_TRACE`/diffcheck
  users first).
- `block_links`/`self_ptr` already effectively columns via the
  `op_links` dense table — unify with it rather than duplicating.

Hard part: `Operation`'s SmallVec/attr fields are `pub(crate)`-accessed
from every dialect crate — this is a vendor/pliron-wide edit (~hundreds
of sites). Order it: (a) add the column tables alongside the existing
fields, dual-write; (b) flip readers to columns behind the same accessor
method names; (c) delete the inline fields. Each sub-step compiles.

Do NOT try to also make `Context`/`Arena` sync in this step — that's the
phase-parallelism question, keep it separate.

## "Smart amount of work" (no user tuning)

The `-Zthreads=8`/`-Ccodegen-units=N` win is real but is a user flag;
the backend should adapt internally:

- `tcx.sess.opts.cg.codegen_units` and `unstable_opts.threads` are
  *readable* at `codegen_crate` entry — the backend can see both and
  choose its own internal strategy. It cannot *set* them (session opts
  are frozen by then, and `-Zthreads` drives rustc's own CGU loop in
  rustc_codegen_ssa, before/around our calls).
- What the backend CAN do without rustc flags:
  - scale `PLIRON_XCGU_DEPTH`/`PLIRON_XCGU_SIZE` copy budgets on observed
    CGU count (fewer CGUs ⇒ already-shared generic bodies ⇒ skip copies)
  - skip second-round fact analyses on tiny CGUs (facts mostly matter
    post-inline; a 3-fn CGU gains nothing measurable)
  - adapt `unroll`/`punroll` budgets to fn size — already partially done
  - phaseB already parallelizes internally regardless of `-Zthreads`;
    phaseA (fnlower) is serial only because `pliron::Context` is
    `RefCell`/`Ref` throughout — post-step-4/5 the read paths could be
    `Sync` shards, making phaseA parallel *inside* one CGU without any
    rustc flag.
- Selfhost candidate (untested): `RUSTFLAGS_NOT_BOOTSTRAP=-Zthreads=8`
  or bootstrap.toml `[rust] codegen-units = 4..8` — CU1 OOM'd at 28GB
  on this box; needs a full stage build to validate memory.

## Measuring

- Fast harness: `/tmp/ctbench` workspace (`syn`, `regex-syntax` members);
  `/tmp/rt.sh` = regex-syntax release build against the .so
  (~15s default / ~6s tuned). MUST delete
  `target/release/{build,deps}/regex-syntax*`, `.fingerprint/regex-syntax-*`,
  `libsyn.*` etc. — cargo fingerprints don't track `PLIRON_*` env.
- `PLIRON_TRACE_FILE=<path>` gives per-CGU phase + per-pass lines:
  `*modgen*`, `*iropt*`, `*phaseA|B|C*`, plus each `pt!` step name and
  `nocap-fixpoint`.
- `perf record -F 499 -g --call-graph fp` on the cargo rustc invocation.
- Guard: `cargo test -p pliron-ir-opt --lib` and `-p pliron-clif-opt`
  after each step; `./test.sh` smoke before calling a step done.

## Traps already hit

- `RefCell` `Arena<T>`: `&Context`-era access patterns hold borrows;
  `panic_already_mutably_borrowed` in `finish_module` was fixed by
  copying `cgu_name` out at construction — watch for the same pattern
  when moving tables under `State`.
- `ArenaIndex` version bits: `ffi()` low-32 is the slot index, high is
  generation — index columns by the low bits only.
- Iteration order: `FxIndexMap` consumers may depend on insertion order
  (`funcs`/`globals` emission) — preserve with an explicit order vec.

## Status update (c908b7a): steps 4 + R1 COMPLETE

- Step 4a (b5a15df): ColMap/ColSet/ColKey in vendor/pliron/cols.rs;
  ordinal-keyed State tables (26 fields) → dense Vec slots.
- Step 4b (987dc85): SymMap/SymSet in state.rs — names intern once
  (ids map + names vec + parallel vals), insertion-ordered iter for
  funcs/globals emission. ident_to_sym/sym_to_ident stay FxHashMap.
- R1a (d0f1d4e): Operation/BasicBlock `attributes` sealed behind
  attributes()/attributes_mut() (~120 sites incl. derive templates).
- R1b (c908b7a): results/operands/successors_slice(+_mut), regions_slice/
  regions_mut on Operation; args_slice/args_mut on BasicBlock. External
  surface was only ~13 sites (value.rs DefNode/UseNode projections,
  region.rs reparent). All remaining field access is inside impl
  Operation/BasicBlock = the store internals step 5 rewrites.

## Step 5 refined design (post-R1)

Fields are pub(crate); accessor seam is in place. Signature at step 5
becomes op.xxx_slice(ctx) — pool lives on Context.
Recommended layout: keep Operation's row; move payload to a SECOND
arena slot keyed by the same ArenaIndex (`payloads: SlotMap<Idx,
RefCell<OpPayload>>`) — deref(row) and deref(payload) are two separate
RefCell borrows, no split-borrow problem, index stays ordinal-dense.
OpPayload = {results, operands, successors, regions, attributes}.
Then flatten each payload Vec into Context-global pools field-by-field
(spans (u32,u16); same-len writes in place; len changes append +
orphan). Freelist per pool only if orphans ever matter (CGU-scoped).

## Step 5 status (as of 2707715)

DONE: regions, successors, operands, results all → `OpSpan{off,len}`
into `Context::op_{region,successor,operand,result}_pool`
(RefCell<Vec<_>>). Mutation protocols:
- operands/successors: drop all uses in span → fresh nodes → new span
  (Use invalidation was already documented); same-len replace writes
  the pool slot + drop_use on swapped-out.
- results: rewrite_results seeds a fresh tail span with new OpResults
  then swaps old nodes into mapped slots — preserves val_uid/DefNode
  identity without Clone; unmapped/skipped nodes orphan (same as
  SmallVec::remove).
- get_region/get_result/get_operand/get_successor/regions()/results()/
  operands()/uses()/has_use()/num_uses()/result_types() gained a ctx
  arg; *_slice(ctx) accessors return Ref/RefMut slices from the pools.
- value.rs DefNode/UseNode projections re-root onto pools via op span.

REMAINING: BasicBlock.args (Vec<BlockArgument>, same seed+swap
protocol — DefNode inside), Operation.attributes (needs AttrId
interning — deferred; measure first), Operation.loc (POD, no gain).
Pools are append-only; orphaned spans are bounded garbage under
CGU-scoped Context lifetimes.

## Step 6 design: Sync-shareable reads -> per-fn phaseA parallelism

Goal: parallelize phaseA (iropt) across fns inside a CGU without
-Zthreads. Blockers as of 62eafb7:

- Read path: op payload reads are pool `RefCell` borrows (Ref<[T]>
  projections). RefCell is !Sync — fine on one thread, hard-blocked
  under rayon/scope threads.
- Options:
  a) **Snapshot views**: before phaseA, snapshot each op's spans +
     needed columns into immutable Vecs/Arcs; workers read snapshots,
     main thread applies mutations. iropt is already scan-then-edit —
     facts tables (ColMap/ColSet) are Vec<Option<T>> reads under & —
     Sync if the container is. But ops pools sit inside RefCell.
  b) **UnsafeCell + unsafe sync** — ruled out (no unsafe).
  c) **Phase-split Context**: split mutation points so that during a
     "read phase" all pools are taken out (&mut Context borrow gives
     us `&Vec<T>` re-borrowed as `&[T]` — provably immutable) and
     worker fns get a read-only `IrView<'a>` struct holding `&'a`
     slices + `&'a State` columns. Writes queue to a per-worker
     edit list, applied single-threaded between phases. iropt's
     structure (scan bodies -> fact tables -> inline decisions ->
     splice) maps onto this naturally: body-scan+fact-derive is the
     50%-of-modgen part and is read-only per fn.
- Prerequisite for (c): State columns are already &T-readable
  (ColMap = Vec-backed). Need: Operation/BasicBlock read API on
  IrView (spans + pools as slices), printable/verifier deps checked,
  then a scoped-threads map over CGU fn list for the scan+derive leg.
- Writes that stay serial: op construction (alloc spans appends),
  inline splicing, symbol registration. Estimated split:
  ~60-70% of iropt CPU is the read-only scan+derive per fn.
