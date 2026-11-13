# 2026-11-13: six features

## 5. Cold-region splitting in the allocator

`PLIRON_RA2_COLDSPLIT=1` (opt-in, default off) in `vendor/regalloc2/src/ion/process.rs`
(`try_cold_split`); `=2`/`=3` run only the first/second trigger below. Knobs:
`PLIRON_RA2_COLDSPLIT_RATIO` (16), `_COLDDIV` (64), `_BIAS` (1.0).

The rule: a block is cold for a bundle when its frequency is at most 1/RATIO of the hottest block
with a use or def of the bundle; the frequency is `CFGInfo::hot_bonus`, divided by COLDDIV in
blocks the frontend marked cold (new `Function::block_is_cold`, from Cranelift's cold marks).
The cold blocks in which the bundle is live across a clobbering instruction are cut out at their
block boundaries. The rest stays one bundle with holes (the lowered block order regalloc2 sees is
the chained RPO with cold blocks interleaved; they are only sunk at emission). Use-less cold
ranges go straight to the spill slot; cold ranges with uses become minimal bundles around the uses.
Triggers: (1) the bundle was just given a callee-saved register nothing else is in: split if the
split's cost (store edges in, reload edges out, uses inside, x BIAS) is below 2 x entry frequency
and the rest fits a caller-saved register right now, which it is then given; (2) no register at
all and a cold clobber among the obstacles: split without a cost test.

What had to be fixed to make it work on its own pattern (`tests/coldsplit/main.rs`):
- the static frequencies give a cold-marked arm 1/33 of its predecessor, so a `#[cold]` call in a
  loop counted ~2x the entry block and never beat a callee-saved register: hence COLDDIV;
- a merely cool block next to the call block (the exit block with the value's last uses) was
  swallowed into the region and its uses priced the split out: regions are now only the blocks
  with a live-across clobber;
- use-less cold ranges put in the spill bundle kept its other ranges out of a second-chance
  register (reload inside the loop): they now bypass the spill bundle.
Result there: `cs_cold_call` 6 -> 0 callee-saved registers saved, `cs_err` 2 -> 0, `cs_flush`
8 -> 4 (the other four are live across the entry block's `memset`), loops without stack traffic,
stores/reloads in the cold block; LLVM saves 6, 4 and 10. `cs_push`/`cs_two` do not change:
`Vec::push`'s grow call is not marked cold and its block is 1/4 of the loop by the static model
(LLVM keeps the callee-saved registers there too). Bounds-check and `expect` panics are noreturn:
nothing is live across them.

Held-out programs (whole binaries, std included): prologue callee-saved saves -0.1..-5.1%
(regex -2.5%, html-rewriter -5.1%, json -3.3%), stack loads/stores up to +5%, code +0.2..1.1%;
retired instructions 0.993..1.007 at 11 rounds, i.e. no measurable change. Why, from the explain
logs (regex, trigger 1): of 10768 callee-saved allocations, 2143 were of an unused register by a
bundle with cold call regions; 575 were split, 126 failed the cost test and 1442 had no
caller-saved register for the rest, on average 14.9 of 16 of them closed by a fixed reservation:
the value is also live across a call that is not cold. 575 splits removed 107 saved registers:
a freed register is usually taken by a later bundle that needs it. The hot functions have
little to give: `Fsm::next_state` uses no callee-saved register, `Fsm::exec_at` 4 (1 split).
Trigger 2 removes no saves and only adds stack traffic statically.
Checked: `PLIRON_RA_CHECK=1` on all eight programs and the test; fuzz seeds 50000..50600.
Open: real (PGO) frequencies should widen the cold set (`Vec::push` growth); shrink-wrapping
lowers the callee-saved cost the test compares against and is ignored; the test is not a
`test.sh` stanza yet.

## 3. Guarded devirtualization

Two modes, one transform: a `dyn` call becomes `fnptr == &target` guards with direct
calls, the indirect call always kept as the last arm.

**Profile builds: value-profiled indirect-call promotion, on by default**
(`PLIRON_DEVIRT=0` disables; it must then be off in both builds). The front end marks
every virtual call — drop glue included, and with `PLIRON_DEVIRT_FNPTR=1` every call
through a fn pointer — in `State::icall_sites` and emits the plain indirect call in
both builds, so CFG fingerprints do not depend on the feature. `pgo::instrument` puts
`__pliron_prof_icall(site, target)` before each site; the runtime keeps the first 4
distinct target addresses per site with counts plus an "other" count. Each
instrumented CGU registers (header magic `PLPROF02`) its site table, a
`{symbol hash, &fn, flags}` table of its bodies and its symbol names;
`pliron-prof merge` resolves every recorded address through the tables of all CGUs
in the dump and writes the sites — target symbol name, defining crate hash (SVH),
defining CGU, "external" flag, count — in a trailer of the `.prof` (`PLICALL1`; old
readers stop before it, old profiles simply have no sites). After `pgo::load`,
`pliron_ir_opt::devirt::promote` rewrites each site: at most `PLIRON_DEVIRT_MAX` (2)
targets with at least `PLIRON_DEVIRT_MIN_SHARE` (10) percent of the site's calls,
hottest first. A target is used when `&symbol` is the profiled address and links:
its body is in this CGU and the address came from this CGU; or it is a symbol of this
crate another CGU can link (`xcgu::home_linkable` or promoted); or it is external in
a dependency whose crate hash still equals the one profiled (a profile of an older
dependency names nothing). Targets in crates that depend on this one (a closure the
binary passes into a library) need `PLIRON_DEVIRT_DOWNSTREAM=1`: the library object
then only links into programs defining them. So that targets can be named, both
builds make vtable slot instances that are `Internal` in exactly one CGU
hidden-external (`devirt::promotable` -> `xcgu::promoted`): closures, private and
`#[inline]` impl methods, shims; per-user copies only when their symbol is unique to
this crate (local definition or generic instance).

**Without a profile: blind guards, opt-in** (`PLIRON_DEVIRT=1`). Candidates are the
slot instances of the vtables this crate instantiates (`src/devirt.rs` `index`: every
`Unsize` coercion in the MIR of every mono item, slots resolved with
`Instance::expect_resolve_for_vtable`), filtered by fn ABI / lowered signature and
linkability; a site with at most `PLIRON_DEVIRT_MAX` usable candidates gets guards
(`Builder::devirt_call`), others are left alone.

Measured (release dylib, 2026-11-13). rust-html-rewriter, default loop: 8196 functions
matched, 0 stale; 69 sites ran, 45 get a guard; 484k profiled indirect calls, 349k
(72%) now go to a guarded target: all 215k `Fn::call` calls into vtables built in
the upstream crate, 27.5k of 34k `FnOnce::call_once`, 107k of 235k `FnMut::call_mut`.
Not guarded: 126k calls of the `&mut dyn FnMut(&[u8])` forwarder whose target is in
the downstream crate (guarded with `PLIRON_DEVIRT_DOWNSTREAM=1`), 7k where the CGU
holds its own copy of the target. pgo vs pgo with `PLIRON_DEVIRT=0`, 15 rounds:
instructions 0.994, cycles 0.970 (noisy machine); with the downstream knob 1.003 —
a direct call into another crate cannot inline, so it only adds the compare.
LLVM's ICP on the same program: instructions 0.995. Only 66k of the guarded calls
have the target's body in the calling CGU (inlinable); 283k are direct calls into
another crate or CGU.
`tests/devirt/main.rs` (one crate) and `tests/devirt_xc/` (library + binary: upstream
vtables, downstream targets) print the same through the loop as without it.

Not done: inlining a promoted target whose body is in another crate or CGU (needs a
copy with its own name, so the guard can still compare the home address);
`tests/devirt*` are not wired into `test.sh`; wasm has no value profile.

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
Cache benchmark (`tests/field_order/cache.sh`, M3 Max, 128-byte lines, 160/192-byte structs with four hot fields on two lines; plain -> reordered ns/element; stock LLVM equals plain): 20k elements (in L2) gather 2.58 -> 1.07, walk 11.2 -> 9.5, sweep 1.42 -> 1.05; 1M elements gather 14.3 -> 9.4, walk 149 -> 136 (noise-level), sweep 2.59 -> 7.12. So random access gains 35-60%, a linked walk 10-15% in cache, and an out-of-cache sequential sweep gets 2.5x slower — reproduced with stock LLVM and a hand-ordered `repr(C)` struct, so it is the layout (mechanism below). 
Held-out, profile-weighted heat, cycles reordered/plain: pulldown-cmark 1.014 (5 rounds), rust-html-rewriter 1.006 (15), rust-protobuf 0.989 (15), load average 12-17, noise about 2%: no signal at this data size. Not done: co-access pairs, `PLIRON_FIELD_ORDER_PAD`, wasm, `crates.py --deps pliron`.
Why the sweep loses, verified (`tests/field_order/sweep.rs`, stock rustc, layout only; ns/element scattered / packed / packed + prefetch 32 ahead / packed + one cold read per 64 bytes): stride 64: 1.2 / 1.2 / 1.5 / 1.2; 128: 2.1 / 4.6 / 4.4 / 2.1; 160: 2.4 / 6.7 / 4.5 / 2.7; 192: 2.7 / 5.3 / 4.0 / 3.0; 256: 3.3 / 5.6 / 4.2 / 3.9; 320: 3.8 / 6.4 / 3.9 / 4.7. The loss starts exactly when the stride skips a 64-byte line and goes away when every line is read again: consecutive lines stream at about 1.1 ns each, skipped lines turn the sweep into one demand miss per element. Software prefetch recovers only part (about 4.2 ns, flat from 32 elements ahead). No miss counters headless.
Sweep rule (default): heat carries `@seq` / `@rand` per struct (address on an arithmetic recurrence in MIR vs loaded from memory; flow-insensitive, SCC over local dependencies); a struct wider than 64 bytes with more than half its heat sequential keeps rustc's order (`PLIRON_FIELD_ORDER_SEQ=1` overrides, the log says `skip-seq`). Cache benchmark, 1M elements: sweep 2.6-3.6 plain / 2.6-3.2 rule / 7.7-8.3 forced; gather 13.5-17 -> 9.8-12.5. Limits: only shapes MIR inlining exposes; one layout per type. pulldown-cmark and rust-html-rewriter: no struct skipped, output correct.

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

Second item, `scanners.rs` (x1.65 LLVM's instructions; `scan_while`,
`scan_ch`: `data.iter().take_while(f).count()`): ten instructions an
iteration against six. Three causes, each fixed behind a knob:
- the exit test stayed `(base + idx + 1) != (base + len)` after the
  pointer was re-expressed as `base + idx`: `PLIRON_CMPCANCEL=1` (new
  pass `cmpcancel`) cancels common addends of `icmp eq/ne`, leaving
  `idx + 1 != len` on the loop's own counter;
- the egraph's remat of `x + k` cloned the counter's step onto the exit
  edge, keeping the old counter alive past its step, so the header
  param could not share its register and every trip paid `mov` + `b`:
  `PLIRON_REMAT_CARRIED=0` skips remat of a value its block passes on
  as a branch argument;
- `PLIRON_LATEGVN=1`: one more dominator GVN after the last sinks.
Repro loop: 10 -> 6 instructions per iteration (LLVM's shape).
pulldown-cmark, exact, cumulative with the first item:
56.57M -> 53.78M (x1.248 -> x1.187 of LLVM).

Third item, the scanners' byte predicates: `selset` (default on) turns
a select-chain membership test (`c == 9 || c == 11 || c == 12 || c ==
32` after if-conversion) into a range check and a bit test, absorbing
the single-constant branch left in front. `scan_while::<is_ascii_
whitespace_no_nl>`: 14 -> 8 instructions a trip, the same code as
LLVM's, mask included. pulldown-cmark x1.187 -> x1.179. `edgeknown`
(branch on a block parameter every edge decided) is default on.

Open, with the evidence: `PLIRON_SELKNOWN=1` (opt-in) folds selects
whose condition a dominating branch or the loop's edges decided — the
slice iterator's `p' = select (p == end), p, p + 1` inside a loop that
runs only while `p != end`. It removes the select and its compare, but
the loop then carries a pointer and a counter that step together, and
the pipeline retires the *counter*: the count becomes `p - base`,
computed inside the loop for the exits (`ldrb; add; sub; cmp; b.ne;
cmp; b.eq; mov; b`, nine instructions; the select form reaches six via
`base + idx` and `cmpcancel`). Exact counts, pulldown-cmark: fold off
53.44M (53.30M with `PUNROLL=0`); fold on 53.49M (54.13M with
`PUNROLL=0`; the cleaned loop also trips the partial unroller). Needed:
retire the pointer in favour of the counter when both step in lockstep
(or sink `p - base` to the exits), and keep `punroll` off multi-exit
byte scans. Repro: scratch `cf.rs` (`closing`), copied to
`tests/parity/closing.rs`.
