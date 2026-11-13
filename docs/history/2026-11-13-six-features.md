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
State: the eight held-out programs build and run with the knob, also under `PLIRON_RA_CHECK=1`;
instructions 0.987..1.004x of default, inside the run-to-run spread of these 3-round runs. No fuzz yet.

## 3. Guarded devirtualization

`PLIRON_DEVIRT=1` (opt-in). `Builder::call`/`invoke` see a virtual call as
`instance = Virtual(trait_method, idx)`; `src/devirt.rs` lists the trait's impls
(`tcx.all_impls`, at most `PLIRON_DEVIRT_SCAN`=64 examined), and the usable candidates
are the non-generic positive impls for the same trait args whose method resolves
(`Instance::try_resolve`, provided methods included) to a plain item with the virtual
call's ABI and lowered signature, linkable from this CGU (defined here,
`xcgu::home_linkable`, or an upstream reachable non-generic). Generic/blanket impls,
by-value `self` and `#[track_caller]` shims are never candidates and do not reject
the site: the indirect arm, always kept last, covers them. `Builder::devirt_call`
emits one guard block per target (`fnptr == &target`, nothing else) with a hit block
holding the direct call; results merge through a block argument and every arm of an
invoke gets the unwind destination.
Which candidates get a guard: without a profile, all of them if there are at most
`PLIRON_DEVIRT_MAX` (2), else none. In a profile build (`PLIRON_PGO` or
`PLIRON_PGO_USE`) the first `PLIRON_DEVIRT_PROF_MAX` (8) by symbol name, identically
in both builds so the CFG fingerprints agree; the hit blocks' counters are the value
profile, and `pliron_ir_opt::devirt::prune` (right after `pgo::load`) keeps at most
`PLIRON_DEVIRT_MAX` arms with at least `PLIRON_DEVIRT_MIN_SHARE` (10) percent of the
site's calls, hottest first, moves the dropped counts to the fallback block and turns
a site that never ran back into the plain indirect call.
Measured (release dylib): `tests/devirt/main.rs` prints the same with and without the
knob, at O0, with 8 CGUs and through the profile loop (shares as expected, e.g.
`kept 2 of 3 arms (shares 40% 20% 40%)`). rust-html-rewriter and rust-protobuf:
instructions 1.000-1.001 blind, 1.001 for pgo+devirt vs pgo; 8196/1804 functions
matched, 0 stale, with the knob on both builds. Neither program runs an eligible
site: their sites are `fmt::Write::write_str`, `log::Log` and regex `Strategy`, all
with zero calls in the profile; the hot `dyn` calls go to generic impls or to traits
with more than 64 impls (`Iterator`). `bench/heldout/run.py --pgo-gen-env K=V`
passes a knob to the instrumented build.
Not done: speculative targets are not xcgu copy candidates (virtual calls are absent
from `items_of_instance`), so a target in another CGU is called directly but not
inlined. `tests/devirt/main.rs` is not wired into `test.sh`.

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
Order: a `layout_of` wrapper. For plain `repr(Rust)` structs named in the file, non-zero-sized fields are handled per ABI-alignment class: a class that tiles one contiguous span in rustc's layout is re-packed hottest first (ties in rustc's order) — same alignment `a`, sizes multiples of `a`, so any order fills the span exactly and nothing outside it moves; a non-contiguous class only swaps equal-size fields. Size, alignment and padding are unchanged by construction; ZSTs keep their offsets and `in_memory_order` is re-sorted by the new offsets. Only `BackendRepr::Memory { sized: true }` / `FieldsShape::Arbitrary` / `Variants::Single` layouts; structs whose tail may be unsized, lang items, enums and unions are skipped. The largest niche moves by its field's delta when the field's own layout confirms the niche, else the struct is left alone. No padding trade (`PLIRON_FIELD_ORDER_PAD`) was added: no type ever grows.
Cross-crate rule: eligibility is by the defining crate (the crate being compiled, or a dependency whose rlib/rmeta is outside the sysroot; never `core`/`alloc`/`std`/`proc_macro`/`test`/`compiler_builtins`) and the key is crate name + def path, so every crate computes the same layout for a type.
Soundness: nothing may assume a `repr(Rust)` order, and `offset_of!`, derives, const eval and debuginfo read the same query. Limits: every non-sysroot crate in the link must be built by this backend with the same file — mixing objects built with and without the knob, with different files, or with stock-rustc dependencies (`crates.py --deps stock`) is unsound and not diagnosed; use a fresh target dir.
Status (built and run): `tests/field_order/run.sh` (integer widths, arrays, nested structs, `Option<Box<_>>`, generics, niche fields, a `Vec` element, a tuple struct, an unsize-coerced tail) prints identical sizes, alignments and values plain, with heat, and under three order files at `-O` and `-Copt-level=0`; only `offset_of!` lines differ. `PLIRON_FIELD_ORDER_LOG=<file>` lists permuted layouts.
Profile path, end to end (`PLIRON_PGO` build, run, merge, `PLIRON_PGO_USE` + `PLIRON_FIELD_HEAT`, `PLIRON_PGO_USE` + `PLIRON_FIELD_ORDER`): weights are measured counts; `PLIRON_TRACE=pliron::field_order` shows every `(block, field)` pair mapped to a backend block (pulldown-cmark: 913 counted, 607 count 0, 287 in functions absent from the profile, 0 unmapped); the order file does not stale the profile (matched with/without: pulldown-cmark 2144/2144, rust-html-rewriter 8196/8196, rust-protobuf 1804/1804, 0 stale); stdout matches the expected output.
Cache benchmark (`tests/field_order/cache.sh`, M3 Max, 128-byte lines, 160/192-byte structs with four hot fields on two lines; plain -> reordered ns/element; stock LLVM equals plain): 20k elements (in L2) gather 2.58 -> 1.07, walk 11.2 -> 9.5, sweep 1.42 -> 1.05; 1M elements gather 14.3 -> 9.4, walk 149 -> 136 (noise-level), sweep 2.59 -> 7.12. So random access gains 35-60%, a linked walk 10-15% in cache, and an out-of-cache sequential sweep gets 2.5x slower — reproduced with stock LLVM and a hand-ordered `repr(C)` struct, so it is the layout (likely, not verified: hot fields packed = one sparse prefetch stream instead of several dense ones). The profile cannot tell a sweep from a gather: the knob stays opt-in.
Held-out, profile-weighted heat, cycles reordered/plain: pulldown-cmark 1.014 (5 rounds), rust-html-rewriter 1.006 (15), rust-protobuf 0.989 (15), load average 12-17, noise about 2%: no signal at this data size. Not done: co-access pairs, `PLIRON_FIELD_ORDER_PAD`, wasm, `crates.py --deps pliron`.

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

## Parity by exact counts (`bench/heldout/irdiff.py`)

Both compilers build a program for `aarch64-unknown-linux-musl`; each
binary runs once under callgrind in a container, which charges every
executed instruction to its function and source line. pulldown-cmark:
LLVM 45.33M, pliron 56.57M (x1.248). The per-line table is independent
of inlining decisions and names the code to fix.

First item taken from it, `parse.rs:3051-3052` (+1.46M, 13% of the gap):
`let event = item_to_event(..); if let Event::Start(..) = event {..};
Some(event)`. IR-level SROA split the 64-byte `Event` temporary although
its callee pin kept every slice in memory, so the whole-copy became
eleven byte-offset load/store pairs; in the isolated repro (callee
visible) the slices were promoted and nine values rode callee-saved
registers across `Vec::push`'s grow path. `PLIRON_SROA_PINWHOLE=1`
(opt-in): an alloca whose slices are all pinned under a call operand and
that is copied whole stays whole when nothing can leave memory, or when
direct accesses touch at most a third of the slices. With the existing
`PLIRON_SLOTPROM_SYNCCOST=1` (CLIF slot promotion's sync cost test) the
repro goes 94 -> 55 instructions (LLVM: 57), and pulldown-cmark
56.57M -> 55.17M (x1.248 -> x1.217). Eight programs (macOS, 5 rounds):
all correct; instructions 0.9957 with both (pulldown-cmark 0.981,
blake3-scalar 0.990, rust-html-rewriter 1.005).
