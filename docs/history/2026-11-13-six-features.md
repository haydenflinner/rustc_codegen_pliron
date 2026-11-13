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
defining CGU, flags ("external", "mono item of its CGU"), count — in a trailer of the
`.prof` (`PLICALL2`; old readers stop before it, old profiles simply have no sites).
After `pgo::load`, `pliron_ir_opt::devirt::promote` rewrites each site: at most
`PLIRON_DEVIRT_MAX` (2) targets with at least `PLIRON_DEVIRT_MIN_SHARE` (10) percent
of the site's calls, hottest first.

*Guard and call are two symbols* (second pass, same day). The first version compared
against the target's own symbol and called it, so a target was usable from another
CGU only as a linked call, and a CGU holding its own copy of the target could not
guard at all (the copy has the symbol name and another address). Now:
- The guard compares against the **home alias** `<symbol>.icp.<hash of the home CGU's
  name>`, a second hidden symbol the home CGU puts on every profiled address it holds
  as a mono item (`devirt::define_aliases`; `State::aliases`, which `address_taken`
  now reads). In the home CGU itself the guard uses the symbol. Nothing is promoted
  any more: the target keeps its linkage in both builds (the first version made
  profiled targets hidden-external, which cost the private-function optimizations).
- The call goes to a body in the calling CGU when there is one — the home (`L`), a
  cross-CGU copy or this crate's own instantiation of a dependency's function (`C`) —
  and through the alias otherwise (`X`).
- Bodies: the profiled targets of a function's sites are cross-CGU copy candidates
  like its direct callees (`xcgu::copies_fixpoint`, hot targets up to
  `PLIRON_PGO_XCGU`). A dependency's target is instantiated here when rustc has its
  MIR (`devirt::dep_instances`: by the symbol the dependency mangled it to,
  `symbol_name_for_instance_in_crate`, over this crate's generic instances and the
  dependency's non-generic functions and closures); its symbol differs from the
  dependency's (the instantiating crate is part of it), and it takes the
  dependency's profile record (`State::prof_as`).
- rustc does not encode MIR for a closure of a plain function — exactly what
  `Box<dyn Fn>` targets are. The profile-use build of the defining crate adds its
  profiled closure targets to `reachable_set` (`devirt::provide`), closed over what
  their MIR mentions: private functions and statics become exported, inlinable ones
  and nested closures instantiable; a thread local, inline asm, a private `const`
  item or a vtable/coroutine constant leaves the target alone. The using crate
  re-checks that closure before instantiating (`closed_downstream`; a miss in rustc's
  collector is a fatal error). `tests/devirt_xc` has one closure of each kind.
- A guard whose pointer turned constant (inlining exposed the vtable) is decided by
  name after `constload` (`devirt::fold_static`): the arm with the inlined body
  stays, the indirect arm goes. Taking the indirect arm instead and letting the
  static devirtualization make it direct loses the body — a cross-CGU copy is an
  import again by then (`home_copies`) — and measured about 0.2% more instructions.
- Does the alias link: same crate, the profiled address is a mono item of that CGU
  in this build's partition; a dependency, a crate linked right here reads the rlib's
  symbol table, an rlib crate requires the profiled crate hash and the "mono item"
  flag. Targets in crates that depend on this one need `PLIRON_DEVIRT_DOWNSTREAM=1`.

**Without a profile: blind guards, opt-in** (`PLIRON_DEVIRT=1`). Candidates are the
slot instances of the vtables this crate instantiates (`src/devirt.rs` `index`: every
`Unsize` coercion in the MIR of every mono item, slots resolved with
`Instance::expect_resolve_for_vtable`), filtered by fn ABI / lowered signature and
linkability; a site with at most `PLIRON_DEVIRT_MAX` usable candidates gets guards
(`Builder::devirt_call`), others are left alone.

Measured (release dylib, 2026-11-13, second pass). The first pass's call counts
summed the remark lines of every CGU copy of a caller; the profile itself (one record
per function, copies merged) has **191.8k** indirect calls on rust-html-rewriter, in
26 sites. Per profiled target, over all emitted copies of its caller:

| | calls | share |
| --- | --- | --- |
| guarded | 128.5k | 67.0% |
| guarded, body in the calling CGU | 118.2k | 61.6% |
| inlined in every emitted copy | 115.6k | 60.3% |
| inlined in some copies, linked call in others | 2.7k | 1.4% |
| body local but not inlined | 0 | |
| guarded, linked call only (`X`) | 10.3k | 5.4% |
| not guarded: target defined downstream | 62.8k | 32.7% |

What is inlined: the `&mut dyn FnMut(&[u8])` forwarder into lol_html's serializers
(62.8k; its copy is now made in the callers' CGUs); lol_html's tag-name selector
closure (`Expr<OnTagNameExpr>::compile::{closure}`, 42.9k) into the benchmark's
instantiations of the selector VM, through `reachable_set` — a plain closure of a
non-generic function, no MIR downstream before; the binary's own closures and
`call_once` shims across its CGUs (9.9k, plus 2.7k in some copies of the caller).
The linked-only rest is three targets of 3.4k calls each, all below the profile's
hot count, so the copy rules are the default 50 statements: lol_html's attribute
selector closure (instantiable here, but it needs `AttributeMatcher::find`
instantiated with it, which is bigger), a `call_once` vtable shim (its closure body
is bigger) and one closure of `main`; and a dozen calls of drop-glue and shim
instances of the dependency (only plain functions and closures are looked up
there). Body local but not inlined: none, so no inliner reason to list.

The 62.8k downstream calls are one site: the forwarder calls the closure the binary
passed into lol_html (`DispatcherDelegate::token_produced::{closure}`). Guard and
target are in different crates in the wrong order — the site's function is compiled
before the target's crate exists, and the callers (lol_html's non-generic
serializers) have no MIR for the binary to instantiate — so there is no build in
which both ends are local; that needs cross-crate IR, not promotion. A guard alone
(`PLIRON_DEVIRT_DOWNSTREAM=1`, or a weak reference, which would link everywhere but
generate the same code) costs instructions: 0.998 against 0.995 without it. It
stays opt-in.

Instructions, `bench/heldout/ab.py rust-html-rewriter`, ratio to `pgo-off:PLIRON_DEVIRT=0`
(which also switches the static devirtualization off), 401 interleaved rounds (the
program's own run-to-run spread is about 1%, hash seeds; 21 rounds resolve 0.3%, 401
about 0.05%):

| side | instructions | cycles |
| --- | --- | --- |
| default (`pgo`) | 0.995 | 0.990 |
| no promotion, static devirtualization only (`PLIRON_DEVIRT_MAX=0`, no copies, no export) | 1.002 | 1.003 |
| `PLIRON_DEVIRT_LINKED=0` | 0.995 | 0.992 |
| `PLIRON_DEVIRT_DOWNSTREAM=1` | 0.998 | 0.999 |

At the requested 21 rounds: 0.993. LLVM's ICP on this program: 0.995. The first
pass's build, measured the same way against its own `PLIRON_DEVIRT=0` build: 0.9985
(sides must come from one backend dylib: other work landing the same morning moved
this program by 0.7%). Where the difference comes from, by ablation on the way:
bodies for same-crate targets, linked guards off, 0.998-0.999 — the forwarder is
three instructions, so inlining it is worth about what its guard costs, and it is
the binary's closures (10k calls, real bodies) that pay; folding the forwarder's
guards by name, no measurable change in the median; the dependency's closures, to
0.995. `-Zalways-encode-mir` on both builds gives the same 0.995: the
`reachable_set` addition finds what that flag would.

rust-protobuf: 2.5k profiled indirect calls, all guarded and inlined (first pass: 2 of
4 sites guarded); instructions 0.998-1.000, nothing to gain.

`bench/heldout/run.py --pgo --rounds 5`, all eight programs build and run, 0 stale
functions each (blake3-scalar 220 matched, blind-sig 6677, pulldown-cmark 2204,
regex 10451, rust-compression 4742, rust-html-rewriter 8485, rust-json 2587,
rust-protobuf 1804); time, 5 rounds on a loaded machine:
```
geomean pliron/llvm over 8 of 8: 1.061 [1.033,1.077]*
geomean pliron-pgo/llvm over 8: 1.051 [1.015,1.069]*  pgo/pliron 0.990 [0.959,1.015]
geomean llvm-pgo/llvm over 8: 0.942 [0.911,0.969]*  llvm-pgo/pliron 0.888 [0.865,0.917]*
pliron-pgo loop / plain llvm build: 1.81x
```
`tests/devirt/main.rs` (one crate, 1 and 8 CGUs) and `tests/devirt_xc/` (library +
binary: upstream vtables and closures — with a private function, a private static, a
nested closure, a thread local — and downstream targets, also with
`PLIRON_DEVIRT_DOWNSTREAM=1`) print the same through the loop as without it.
`harness/pgofuzz.py 61000:61150`: pass 150.

Knobs added: `PLIRON_DEVIRT_COPY=0` (no copies for targets), `PLIRON_DEVIRT_DEPCOPY=0`
(no instantiation of a dependency's targets), `PLIRON_DEVIRT_EXPORT=0` (no
`reachable_set` addition), `PLIRON_DEVIRT_FOLD=0` (keep guards on constants).

Not done: the downstream site above; shim and drop-glue instances of a dependency as
copies; copying the body under a `call_once` shim for a warm (not hot) target; a
profile-use rlib built into a target directory that holds rlibs of a build without
the profile is trusted by crate hash (cargo does not track `PLIRON_*`; use a fresh
directory, as for every profile build); the aliases are exercised on Mach-O only;
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

### Fourth item: the open select fold, closed (`selknown` default on)

The select fold above was correct but lost instructions because of
what happened to the loop afterwards. Five causes, fixed in the pass
that owned each; `tests/parity/closing.rs` (`closing`) went from 92
instructions (283 with the fold on, partial unroll included) to 83
(LLVM: 52), its two scans from 6 and 9 instructions a trip to 6 and 6
— LLVM's loop, `ldrb [base, idx]; cmp; b.ne; add; cmp; b.ne`.

- **The counter was retired, not the pointer.** `webmerge` (inside
  `sameargs2`) saw a pointer and a counter stepping in lockstep, each
  expressible from the other (`p = c + base`, `c = p - base`); its cycle
  check kept whichever candidate came first, which was the pointer. The
  count then had to be computed from the pointer for both exits and the
  cursor copied at entry. `sameargs` now orders the candidates so a slot
  seeded with a constant survives (`WmCtx::keep_count`,
  `PLIRON_WM_KEEPCOUNT=0` off). Only in `sameargs`: the early `cparm`
  rounds keep the pointer walk, which `bytescan` and `punroll` match —
  with the counter kept there, `escape_html_scalar`'s `position` over a
  byte table lost its 16-byte scan and pulldown-cmark ran 4% more
  instructions (55.55M).
- **The exit test of a scan that starts mid-slice kept the pointer
  alive.** `(bytes + found) + idx + 1 != bytes + len`: `cmpcancel`
  cancelled `bytes` but refused the two-value left side. It now moves
  loop-invariant addends to the other side, computing `len - found`
  once in the block that enters the loop (`PLIRON_CMPCANCEL_INV=0` off),
  and reads sums through parameters of single-edge blocks (loop
  rotation passes the guard's values that way). Second scan of
  `closing`: 8 instructions a trip -> 6.
- **`punroll` unrolled search loops.** With the select gone the scans
  matched its multi-block path (`run_region`), which made 8 copies
  behind a group guard: 283 instructions, and each run of a loop that
  makes a trip or two paid the guard and fell into the scalar tail. A
  loop that leaves through a data test into warm code (not a cold block
  or a trap stub) is now left rolled unless a measured trip count says
  otherwise (`punroll::warm_exit`; `PLIRON_PUNROLL_SEARCH=1` restores
  the old behaviour; LLVM's runtime unroller refuses multi-exit loops
  the same way). Counted loops and loops whose other exits are
  bounds-check panics unroll as before.
- **Loop-exit values computed in the loop** (`edgesink::exit_sink`,
  `PLIRON_EXITSINK=0` off). Where the counter *is* retired early (the
  pointer loops `cparm` leaves), `p - base` sat in the header for the
  exits. A pure in-loop instruction whose every use is outside the loop
  or on an exit edge is cloned to those uses; a header parameter
  operand is read through its step (`q - k`) where the step dominates,
  so the parameter still dies at its step. The vendored `bump_hoist`
  (patch 116) learned to alias a dominated duplicate of the kept bump
  instead of rematerializing it as `(q - k) + k`. With
  `PLIRON_WM_KEEPCOUNT=0` the `closing` scans become
  `ldrb [p], #1; cmp; b.ne; cmp; b.ne` — five a trip, but two or three
  more per run of the loop, which is why the counter form is the one
  kept for these scans.
- Tried and left opt-in: `PLIRON_ELABHOT=1` (vendored elaborator: a
  block that lost its loop's stack entry to the pop-on-leave rule gets
  it back when the loop has no call and the block dominates the
  latches). It fixes `scan_while`'s in-loop mask constant when exit
  blocks precede the body in the domtree walk, but hoists out of loops
  that run a trip or less: pulldown-cmark +0.5% (`ELABLOOPS=1`: +2.4%).
  Needs trip counts.

Two things `closing` showed besides the loops, both default on:

- `deadslot` (new step, `PLIRON_DEADSLOT=0` off): a stack slot whose
  last store `dse3` deleted after `slotmerge` kept its size, so the
  function kept a frame (`sub sp` / `add sp`). The closure handed to
  `take_while` captures the fence byte by reference; the byte's slot is
  dead by the end. `scan_while.rs`: 40 -> 35 instructions (`scan_ch` is
  frameless). pulldown-cmark -0.12M.
- `retest` (`edgeknown::retests`, `PLIRON_RETEST=0` off): a `brif` that
  repeats the test on its block's only way in. The rotated loop's guard
  is `p == p + len` until `cmpcancel` rewrites it at the end of the
  pipeline and the last GVN finds it equal to the caller's `is_empty`
  check (`cbz x1` twice). pulldown-cmark -0.19M.

Exact totals (callgrind, `aarch64-unknown-linux-musl`), every knob of
this item off -> defaults:

| program | LLVM | before | after | |
|---|---|---|---|---|
| pulldown-cmark | 45.33M | 53.40M (x1.178) | 52.99M (x1.169) | -0.77% |
| regex | 45.97M | 55.29M (x1.203) | 55.12M (x1.199) | -0.31% |
| rust-html-rewriter | 85.83M | 106.98M (x1.246) | 106.54M (x1.241) | -0.41% |
| rust-json | 45.55M | 51.95M (x1.140) | 52.08M (x1.143) | +0.25% |
| rust-protobuf | 60.76M | 66.87M (x1.100) | 66.77M (x1.099) | -0.15% |
| blake3-scalar | 46.63M | 49.08M | 49.08M | 0 |

pulldown-cmark by knob (each one off against 52.99M): search-loop rule
+0.21M, `retest` +0.19M, `exit_sink` +0.16M, `deadslot` +0.12M,
`keep_count` +0.08M, `selknown` +0.02M, `cmpcancel` invariants 0.

rust-json's +0.13M is `exit_sink` (51.93M with it off) and is not the
loop: `serialize_str` is called 38,850 times and runs four more
instructions a call — three alignment `nop`s in front of its scan loop
(the function is two instructions longer, so the loop head moved off a
16-byte boundary) and one more register copy in the prologue. The
macOS whole-process counts (`ab.py`) cannot arbitrate: the same pair of
builds measured 0.991 and 1.001 for rust-json in two runs an hour
apart, and blake3-scalar (identical code on both sides in the exact
count) 0.986, 1.004 and 1.012, with other agents loading the machine.

Validation: `harness/gate.py` passes; fuzz seeds 74000:74700 and
75000:75700 pass (the first range found one verifier failure in
`exit_sink`, seed 74256 — a header parameter of an *enclosing* loop was
read through its step, which was the instruction being sunk; fixed,
regression test `exit_sink_does_not_read_a_value_through_itself` in
`pliron-clif-opt/tests/scan_loops.rs`); all eight held-out programs
build and exit 0 under `ab.py`.

### irdiff on regex

`irdiff.py regex` died in callgrind with SIGILL: valgrind does not
decode `ldr q0, [sp, x16, sxtx]`, which Cranelift emits for a frame
offset no immediate form holds (regex's `ExecBuilder::build` has a
14KB frame). `mem_finalize` now uses the plain register form
`[sp, x16]` (same address for a 64-bit index, vendored patch 132);
valgrind decodes that one (checked both encodings with a three-line
`asm!` program in the container). Still not runnable: rust-compression
(our `main` touches its multi-megabyte frame without probing it page by
page; valgrind only grows the stack near SP, so the first access
faults — `--main-stacksize` does not help) and blind-sig (the LLVM side
does not link for musl: `open64` from getrandom 0.2.8).

### Leads from the tables, not yet taken

- `punroll` as a whole still costs pulldown-cmark 0.12M
  (`PLIRON_PUNROLL=0`: 52.87M) — peeling (`PLIRON_PEEL=0`) accounts for
  it: `scalar_iterate_special_bytes` +0.25M, against gains in
  `parse_block`. Across programs peel is a wash (protobuf -0.43M with
  it, html-rewriter +0.30M, pulldown +0.17M), and unrolling plain
  counted loops costs protobuf +0.7% and html-rewriter +0.3% (seen when
  a bug in the first version of the search rule also switched those
  off). Trip counts again.
- pulldown-cmark lines after this item: `iterator.rs:2503` +1.07M,
  `scanners.rs:373` +0.81M, `vec/mod.rs:1002` +0.72M, `parse.rs:2837`
  +0.63M, `parse.rs:2937` +0.62M, `parse.rs:780` +0.55M,
  `tree.rs:163` +0.33M.
- `closing` is still 83 instructions to LLVM's 52: each rotated scan
  keeps a peeled first iteration (`looprot2` rotates a loop whose latch
  already ends in a `brif`), the bounds checks of `&bytes[i..]` compare
  twice where LLVM uses one `subs`, and `scan_eol`'s
  `bytes.get(1) == Some(&b'\n')` loads the promoted constant's pointer
  from memory and null-checks it (LLVM folds the whole comparison: its
  result is unused).

### Fifth item: rust-html-rewriter and regex, first line-level pass

Exact totals, this item's knobs off -> defaults:

| program | LLVM | before | after | |
|---|---|---|---|---|
| regex | 45.97M | 55.12M (x1.199) | 53.10M (x1.155) | -3.7% |
| rust-html-rewriter | 85.83M | 106.54M (x1.241) | 104.50M (x1.218) | -1.9% |
| rust-protobuf | 60.76M | 66.77M (x1.099) | 65.72M (x1.082) | -1.6% |
| rust-json | 45.55M | 52.08M (x1.143) | 51.91M (x1.139) | -0.3% |
| pulldown-cmark | 45.33M | 52.99M (x1.169) | 52.93M (x1.168) | -0.1% |
| blake3-scalar | 46.63M | 49.08M | 49.08M | 0 |

**html-rewriter 1: the allocator shim** (`src/asm.rs`,
`alloc_shim_jump`, `PLIRON_ALLOC_JMP=0` off). `__rust_alloc` and
friends forward to `__rdl_*` with the same arguments. LLVM emits each
as one `b`; ours was a function, `stp; mov; bl; ldp; ret`, because
Cranelift tail-calls only between `tail`-convention functions and
these are C-ABI exports. The shim is now module asm, one branch
(aarch64 ELF and Mach-O; other targets keep the function — x86-64
would be `jmp`, not enabled because nothing here ran it). The program
allocates 89,351 times: rust-html-rewriter 106.54M -> 105.81M,
rust-json and rust-protobuf -0.2M and -0.6M.

**html-rewriter 2: short runs skip the vector dispatch**
(`loopvec.rs`, `PLIRON_VEC_SHORT=0` off). `[u8]::to_ascii_lowercase`
on tag and attribute names of a few bytes
(`tests/parity/lowercase.rs`; `Map<Iter<u8>, _>::fold`, called 21,406
times, 2.08M instructions). The vectorized loop's dispatch block
computed the range-overlap tests, the trip rounding and the splats
before deciding anything — about thirty instructions ahead of the
first byte. A new block in front compares the trip count with one
vector group and goes straight to the scalar loop below it, as LLVM's
`min.iters.check` does ahead of its memory checks. 105.81M -> 104.87M;
pulldown-cmark -0.08M. Still open on this path: the chain
`as_lowercase_string -> from_iter -> extend_trusted -> fold` is three
calls here and one inlined function in LLVM (about 135 instructions a
call against 44), and the scalar remainder of a vectorized loop is
group-unrolled again.

**regex 1: `follow_epsilons`** (`PLIRON_LICM_DOMCOND`, now in
`DEFAULT_ON`). `SparseSet::insert` asserts `len < cap` and then
`Vec::push` tests `len == cap` for its grow call; the call takes the
set, so the set escaped inside the loop and LICM promoted none of its
fields. The fold of the dead test existed (opt-in) but had not been
measured exactly: regex 55.09M -> 53.17M, `follow_epsilons` 17.56M ->
15.64M (LLVM: 15.80M). pulldown-cmark +0.03M (`scan_setext_heading`
changes shape and is no longer unrolled).

**regex 2: zigzag decode** (new step `selxor`, `PLIRON_SELXOR=0`
off). `let mut n = un >> 1; if un & 1 != 0 { n = !n }` if-converts to
`select (un & 1), !n, n`: `lsr; and; mvn; cmp; csel`. It is
`n ^ -(un & 1)`: `and; neg; eor ..., lsr #1` (LLVM: `sbfx; eor`).
`tests/parity/instptrs.rs`; regex 53.17M -> 53.02M.

Tried and left opt-in, with the blocker:
- `PLIRON_LOOPROT_MERGE=1` gives a loop with several back edges one
  latch so it can rotate (regex's `exec_byte` dispatch loop,
  `tests/parity/prog_loop.rs`: 16 instructions for a skipped
  instruction against LLVM's 12). html-rewriter -0.27M and json
  -0.16M, but regex +0.63M and pulldown +0.64M: the rotated loop
  recomputes its end pointer in the latch (the elaborator's
  pop-on-leave placement again) and shuffles the cursor between a
  caller-saved and a callee-saved register around the call.
- `PLIRON_LICM_INNERPRESS=2` hoists the `SparseSet` fields out of the
  state-decode loop (the repro then matches LLVM instruction for
  instruction, 28 a state) but `next_state` itself gets 1.07M worse —
  a spill somewhere in the larger function; not traced.
- `PLIRON_LOADPRE_CARRY=1` (loop-carried load PRE, for `Vec::push` in
  `cached_state`'s varint writer): regex worse, others mixed.

regex after this: `next_state` 9.86M against 6.63M, `cached_state`
12.62M against 11.11M, `hash_one::<&State>` 0.87M against 0.04M (not
inlined here).

### Trip counts

Measured (profiled builds, `ab.py PROG pgo pgo-x:...`, retired
instructions, 11 rounds):

- Group unroll needs an average of at least 4 trips
  (`PLIRON_PUNROLL_MINTRIP`, was 2): rust-protobuf -0.2%, regex -0.5%,
  pulldown-cmark 0, html-rewriter +0.2%, rust-json +0.2%.
- A floor for peeling loses, so there is none
  (`PLIRON_PEEL_MINTRIP`, default 0): with 2, html-rewriter's profiled
  build ran 2.7% more instructions; the other four programs moved
  0.1-0.5% the other way. Loops that make about one trip are where a
  straight-line first iteration pays most. The number of peeled copies
  is now capped at the measured trips plus one.

Static (no profile): a plain counted loop — no retained exit — whose
trip count is neither measured nor a constant of at least two groups
is no longer group-unrolled (`PLIRON_PUNROLL_COUNTED=1` restores).
Exact counts: rust-protobuf -0.48M (prost's `encode` loops over a
message's few fields, hashbrown's group walk), rust-html-rewriter
-0.38M, pulldown-cmark and rust-json 0, regex +0.08M. This is LLVM's
policy for generic aarch64 and x86-64 too (no runtime unrolling). The
bench kernels that were tuned with the unroll were not re-timed (the
machine was loaded all day); the knob is the way back.

### Self-compile check

Another session reported ICEs while building the backend with itself.
With this tree, `PLIRON_VERIFY=full` builds `pliron-clif-opt` and its
dependencies, then the whole backend, and that stage-2 backend builds
miette 7.6 with its dependencies, all without a verifier failure. The
two failures on record were `mask01 broke Vec::extend_trusted`
(webmerge materializing a slot of an unreachable self-loop; the fix,
skipping unreachable blocks, is in `clifpeep.rs` with its test) and
`edgesink broke HashMap::retain` (the `exit_sink` substitution fixed
above, seed 74256).

Validation for the fifth item and the trip-count rules: gate passes;
fuzz 76000:76700 found one more `exit_sink` verifier failure (seeds
76003, 76268: a header parameter read through a step that was itself
being sunk out of an inner loop — the substitution now skips steps
that are leaving a loop; regression test
`exit_sink_does_not_read_a_step_that_was_sunk`), then 77000:77700 and
both seeds pass; all eight programs build and exit 0. One `test.sh`
stanza (`slotpromlt`) measures what `PLIRON_RA2_XBLOCK` removes and
needed `PLIRON_LICM_DOMCOND=0` pinned in its comparison builds, like
the other passes that delete the same moves first.

