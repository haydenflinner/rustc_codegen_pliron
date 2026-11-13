# 2026-11-13: six features

## 5. Cold-region splitting in the allocator

`PLIRON_RA2_COLDSPLIT=1` (opt-in, default off; `PLIRON_RA2_COLDSPLIT_RATIO=<K>`, default 16) in
`vendor/regalloc2/src/ion/process.rs` (`try_cold_split`). When a bundle finds no register and a
call clobber in a cold block is what kept it out of a caller-saved one, the cold blocks it is live
across a clobber in are cut out of it at their block boundaries. Cold = block frequency
(`CFGInfo::hot_bonus`) at most `1/K` of the hottest block the bundle has a use or def in; cold
blocks without a clobber stay. The hot remainder stays one bundle with holes and is requeued; a
cold piece without uses goes straight to the spill bundle, one with uses is requeued with its low
weight. The store and reload then come from the normal edge moves, on the cold edges.
Finding: regalloc2 does not see cold code as one contiguous tail. Cranelift sinks cold blocks only
at emission (`VCode::emit`'s `final_order`); the lowered order regalloc2 allocates on is the
chained RPO with cold blocks interleaved. Hence one bundle with holes rather than a
before/middle/after three-way split, which would cut the hot code into pieces at every panic block.
Runs at most once per original bundle (`LiveBundle::cold_split`, inherited by later splits).
State: compiles (`cargo check`); not yet run on real code, no checker/fuzz/benchmark run.

## 3. Guarded devirtualization

`PLIRON_DEVIRT=1` (opt-in; `PLIRON_DEVIRT_MAX`, default 2, caps the trait's impl count).
`Builder::call`/`invoke` see a virtual call as `instance = Virtual(trait_method, idx)`;
`src/devirt.rs` lists the trait's impls (`tcx.all_impls`), keeps non-generic positive
ones for the same trait args, resolves the method (`Instance::try_resolve`, provided
methods included) and rejects by-value `self`, `#[track_caller]`/shim vtable entries,
any ABI or lowered-signature difference, and targets this CGU cannot link (not defined
here, not `xcgu::home_linkable`, not an upstream reachable non-generic).
`Builder::devirt_call` then emits `fnptr == &target` guards with direct calls and
always keeps the indirect call as the last arm; results merge through a block
argument, and for invokes every arm's call gets the unwind destination.
Each site writes a `devirt` remark (targets or reject reason). Not done: speculative
targets are not xcgu copy candidates (virtual calls are absent from `items_of_instance`),
so a target in another CGU is called directly but not inlined. Only `cargo check`ed;
`tests/devirt/main.rs` is the (unwired) run test.

## 6. Shared panic tails

`PLIRON_PANICSTUB=1` (opt-in; `pliron-clif-opt/src/panicstub.rs`, step `panicstub` after `commonret`, before `brhint`).
What existed: lowering's `cold_blocks` marks blocks that end in `unreachable`, call a `#[cold]` fn, or sit on an unlikely/zero-count edge;
`coldargs` rebinds each hot int arg of a cold call (`iadd a, (umulhi ..)`) so the ABI register pin sits in the cold block; `coldedge` gives
hot->cold param edges a cold adapter block so the edge copies run only when taken; `edgesink` sinks pure insts used by one `brif` edge onto it;
`stubhoist` folds single-pred pure stubs back into the pred; `coldrets` is return-block layout only; `tailmerge` merges identical blocks and
`tailmergep` (default on) merges same-*shape* tails into one param'd block — which already covers `panic_bounds_check` sites of one shape.
So (c) argument-free cold edges was done; (a) was done per block shape. New: all cold `call f(args); trap code` suffixes with the same callee,
trap code and arg types share one cold block per function regardless of what precedes the call (constant vs hot index, with/without `coldargs`
wrapper, `tailmergep`'s per-shape blocks, which become forwarders and are bypassed). Per position: equal constants materialize once in the
shared block, one common value is used directly, the rest are block params; stripped `coldargs` wrappers are redone once in the shared block.
Plain `call` only (`try_call` keeps its edge); sound without a noreturn fact. The shared call keeps the first member's srcloc (other sites lose
the call's debug line; the `&Location` argument is unaffected). (b) location tables not done: CLIF passes cannot declare data (only lowering's
`write_data_addr`), and an 8-byte relocated entry per site costs more than the `adrp+add` it replaces.
Expected gain is small: `bl`->`b` is size-neutral on aarch64 and `NORETURN_CALL` traps were already dropped; it saves per-site wrappers and
duplicate constants. State: unit tests + `cargo check`; not run on real code, no corpus/fuzz/benchmark.

## 2. Internal calling convention (FASTCC)

`PLIRON_FASTCC=1` (opt-in, aarch64 only; off under `PLIRON_HOT` and instrumented `PLIRON_PGO`): per-callee
clobber sets. Phase (B) of `lower_module` runs only the CLIF pipeline; `fastcc_compile`
(`pliron-clif-lower/src/lower.rs`) then compiles the CGU's functions serially, callee-first over the SCCs
of the direct call graph taken from the final CLIF (other CGUs still compile in parallel on their threads).
After each compile `VCode::pliron_written_regs` gives the registers the machine code may write: regalloc
move destinations, def allocations and every instruction's clobber set, calls included, so the set is
transitive; a body with a tail call records nothing. It is recorded for functions with internal linkage,
address not taken, not variadic, not in a cycle. The aarch64 `gen_call_info` (`lower/isle.rs`) intersects
a direct, non-`try_call`, non-patchable call's ABI clobbers with the callee's set plus x16/x17/x30 (and x9
when a return value comes back on the stack). Callees without a set keep the full ABI set, so compile
order affects only coverage, not correctness. Cost: the Cranelift compile of a CGU is no longer spread
over the fn pool. Not done: x64 consumer, level-parallel compile, cross-CGU sets.
State: compiles (`cargo check`); not yet run on real code, no gate/fuzz/benchmark run.

## 4. Profile-guided field order

`PLIRON_FIELD_HEAT=<file>` records, `PLIRON_FIELD_ORDER=<file>` applies (`src/field_order.rs`, both opt-in; unset, no provider is installed and output is unchanged).
Heat: the builder only sees byte offsets (`project_field` emits nothing at offset 0), so it is a MIR walk per codegen'd instance: each `Field` projection on a struct, times the profile count of its MIR block (`append_block` is handed the `bbN` name, `resolve` reads `pgo::block_count` after `pgo::load`; a block merged into its only predecessor takes that block's count); without `PLIRON_PGO_USE`, `8^loop depth`. Lines are `<crate>::<path>\t<field>\t<weight>`, appended per crate; `harness/field_order.py` prints the ranking.
Order: a `layout_of` wrapper. For plain `repr(Rust)` structs named in the file, fields of identical size and alignment swap slots (offset and memory-order position) hottest first, ties in rustc's order, so size, alignment and padding are unchanged by construction. Only `BackendRepr::Memory { sized: true }` / `FieldsShape::Arbitrary` / `Variants::Single` layouts; structs whose tail may be unsized, lang items, enums and unions are skipped. The largest niche moves by its field's delta when the field's own layout confirms the niche, else the struct is left alone.
Cross-crate rule: eligibility is by the defining crate (the crate being compiled, or a dependency whose rlib/rmeta is outside the sysroot; never `core`/`alloc`/`std`/`proc_macro`/`test`/`compiler_builtins`) and the key is crate name + def path, so every crate computes the same layout for a type.
Soundness: nothing may assume a `repr(Rust)` order, and `offset_of!`, derives, const eval and debuginfo read the same query. Limits: every non-sysroot crate in the link must be built by this backend with the same file — mixing objects built with and without the knob, with different files, or with stock-rustc dependencies (`crates.py --deps stock`) is unsound and not diagnosed; use a fresh target dir.
Status: `cargo check` only — not built, run or measured. Equal-size swaps cannot change padding, so the expected win is small (offset 0, first cache line); reordering across size classes is the follow-up.

## 1. Scoped noalias

`PLIRON_SCOPED_NOALIAS=1` (opt-in, default off). What existed: the inliner already records each
clone as a region (`State::region_blocks`/`region_parent`), pins the clone-entry params bound to
rustc-`noalias` callee params (`region_noalias`, `region_frozen`) and lowers them into
`noalias`/`scoped_na`; the readonly half already worked (`region_frozen_op` -> loadfwd's
region-frozen locs, `frozen_loads` -> licm). Missing was the `&mut` half: a scoped root only got a
distinct name, no immunity. New (`loadfwd::ScopedNa`, rule in the comment above it): a loc rooted
at such a param survives a store or call when the param is still the clone-entry block param,
every use of it stays tracked (never stored, passed or variably offset: `isolated_escapes`), the
store/call is in a block whose live region is inside the clone, and the store address / every call
operand provably is not based on the param (backward cone ending in loads, call results, entry
params, constants; never the param or its caller-side root). Stores through the param still kill
foreign locs (the caller may read them after the region). The candidates are snapshotted at
pipeline start (`loadfwd::scoped_seed`), so facts carried onto merge survivors never qualify.
Under the knob the inliner records param facts only when the clone entry still has the declared
leaf layout, and `cleanup::merge_blocks` does not merge blocks across a region boundary (a caller
continuation would otherwise run "inside" the callee). licm/dse do not use the fact yet.
State: `cargo check` + `pliron-clif-opt/tests/loadfwd_scoped_noalias.rs` (4 cases) pass; no fuzz/gate.
