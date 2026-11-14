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
duplicate constants. State: default on, see "Features 1, 2 and 6 on real programs" below.

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
State: default on, with changes to what is recorded, who is eligible and how the CGU compiles:
see "Features 1, 2 and 6 on real programs" below.

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

`PLIRON_SCOPED_NOALIAS` (opt-in as written; still opt-in; see "Scoped noalias: the pulldown-cmark loss explained and fixed"). What existed: the inliner already records each
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
State: opt-in; the representation changed (the param is not a `noalias` root any more) and the
fact is used by a step of its own: see "Features 1, 2 and 6 on real programs" below.

## Features 1, 2 and 6 on real programs

Run the same day, after the three sections above were written. All
counts are callgrind instruction totals on `aarch64-unknown-linux-musl`
(the `irdiff.py` setup; the pliron side only, one backend build per
table, the knob off against on; a rerun of one binary moves the total by
under 10 instructions, except regex, about 300). The baseline moved
during the day with other work, so compare within a table.

### FASTCC: default on

`PLIRON_FASTCC` is in `DEFAULT_ON` (aarch64 ELF and Mach-O; `=0` off).

| program | off | on | |
|---|---|---|---|
| pulldown-cmark | 52,816,871 | 52,528,747 | -0.55% |
| regex | 51,585,397 | 51,582,068 | -0.006% |
| rust-html-rewriter | 104,539,329 | 104,539,299 | 0 |
| rust-json | 51,852,558 | 51,852,402 | 0 |
| rust-protobuf | 65,691,065 | 65,689,927 | -0.002% |
| blake3-scalar | 49,075,170 | 49,075,163 | 0 |

As first written the feature did almost nothing (pulldown-cmark -0.06%,
the rest zero): 285 of regex's 2082 functions got a set and most sets
were the whole ABI set. Three changes:

- **Paths that never return do not count** (`VCode::pliron_written_regs`,
  `PLIRON_FASTCC_NORET=0` off). A bounds check gave a leaf the whole ABI
  set through its `panic_bounds_check` call. The set is now taken over
  the blocks from which a `ret` is reachable: what a path that ends in a
  trap or a noreturn call writes is not seen by a caller that resumes
  after a plain `call`, and a caller that resumes at a landing pad used
  a `try_call`, which is never narrowed.
- **Hidden definitions are eligible** (`PLIRON_FASTCC_HIDDEN=0` off): a
  function shared between the crate's CGUs is a strong hidden
  definition, bound at static link time to the body in this object.
  `Export` stays out (an ELF dylib may reach it through the PLT
  resolver, which preserves only argument registers). The
  address-not-taken condition is gone: only direct calls consume a set.
- **Compile order is dependency-driven and parallel**: a call-graph
  component starts when the components it calls are done, on the fn
  pool, instead of the whole CGU compiling serially on its thread. Clean
  builds, fast-profile backend, three alternating rounds: regex 24.2
  against 24.0 CPU-seconds and the same peak RSS (490-516 MB);
  rust-html-rewriter 34.2 against 34.7 CPU-seconds, peak RSS 518-528
  against 535-548 MB (the optimized CLIF of a CGU waits for its
  callees). Wall time was noise on the loaded machine (11-20 s for the
  same build).

With the first two, pulldown-cmark -0.36% (NORET off: -0.34%; HIDDEN
off: -0.08%). The mechanism on `tests/fastcc/main.rs` (sixteen values
live across each call): the caller of a leaf goes from 68 instructions,
5 callee-saved stores and 24 stack references to 50, 1 and 6; the
caller of a callee with a panic path from 72 to 56.

What happens to the direct call sites (`PLIRON_TRACE=pliron::fastcc`,
one `fastcc-sites` line per CGU; static counts over the program's own
crates):

| program | plain calls | callee outside the CGU | set is the whole ABI set | narrowed | not eligible / in a cycle | `try_call` to a useful set |
|---|---|---|---|---|---|---|
| pulldown-cmark | 1431 | 1098 | 251 | 63 | 19 / 0 | 4 |
| regex | 5419 | 4471 | 644 | 99 | 76 / 129 | 120 |
| rust-html-rewriter | 4495 | 3271 | 1088 | 49 | 43 / 44 | 11 |
| rust-json | 1806 | 1421 | 329 | 7 | 17 / 32 | 2 |
| rust-protobuf | 2936 | 2387 | 476 | 7 | 32 / 34 | 2 |

So 73-83% of the sites call out of the CGU (the prebuilt std, another
crate, another CGU of the same crate), and most local callees call out
themselves, which makes their set the whole ABI set. Weighted by
callgrind's call counts (`=debug` prints one `fastcc-site` line per
site; callgrind's call graph is approximate on our code, it also
reports returns and some jumps as calls): pulldown-cmark makes about
66k narrowed calls a run, 70k to a full set, 196k out of the CGU and
99k `try_call`s; regex 0.5k narrowed of about 90k; rust-html-rewriter
and rust-json practically none. That is the whole result: pulldown-cmark
calls small scanners from its parse loop, the others do not have that
shape.

The next blocker would be sets across CGUs. Measured before building
it: with every crate compiled as one CGU (`-Ccodegen-units=1`) every
same-crate call is local, which is all that cross-CGU sets could give.
Off against on: pulldown-cmark 55,459,625 -> 55,418,730 (-0.07%), regex
-0.005%, rust-html-rewriter, rust-json and rust-protobuf 0. The calls
that remain go to the prebuilt std and to other crates or are
`try_call`s; cross-CGU sets would cost the CGUs' parallel compile for
nothing on these programs.

Checked:
- `harness/fastcc_check.py`: disassembles the objects and reports any
  instruction that writes a caller-saved register missing from its
  function's recorded set (build with `PLIRON_FASTCC_NORET=0` and
  `PLIRON_TRACE=pliron::fastcc=debug`). The rlibs and `-Csave-temps`
  objects of all eight held-out programs (macOS): every recorded set
  found in the code (455 to 12,371 functions a program), 49k-597k
  instructions checked, no violation; the linked linux-musl binaries of
  five as well. 4-9% of the words are skipped: what follows a transfer
  that does not fall through, up to the next direct branch target, is
  jump-table data, a block only a jump table reaches, or a landing pad.
  On a linked binary the prebuilt std's own copy of a function we also
  compiled a local copy of (`decode_error_kind`) is checked against our
  set and reported: check objects. A set zeroed by hand is reported.
- `tests/fastcc/main.rs`, stanza `fastcc` (new): leaf, chain,
  recursion, mutual recursion, fn pointers and `dyn`, calls into std,
  floats, `u128`, ten-value and array returns, a callee whose cold path
  panics, a panic caught above a narrowed call; output equals stock
  rustc's with 1 and 4 CGUs and with each sub-knob off, the leaf's
  caller must get shorter, and the checker must pass.
- All eight held-out programs print the expected output natively, the
  six callgrind ones also under valgrind on linux-musl.
- Fuzz: seeds 91000:91600 (`PLIRON_FASTCC=1`, first version),
  95000:95600 (with the two rule changes), and 95000:95600 again on
  the final tree with the knob at its default: 600 pass each. (A run
  on fresh seeds 98000:98600 died in the harness near its end when
  another session's fuzz removed a seed directory under it.)
- Two builds of regex and rust-json are byte-identical. `PLIRON_PAR=0`
  gives a different binary than the parallel build, with the knob on
  and off alike (rust-json: the same two hashes either way), so that
  difference is not this feature's.
- `harness/crates.py --tier 1 --deps pliron` with FASTCC and the panic
  tails on: 19 of 20 crates as stock. hashbrown fails two doc-tests
  (`symmetric_difference`, `bitxor`: `{4}` where `{1, 4}` is expected).
  The same wrong set comes out of a ten-line program on std's `HashSet`
  at opt-level 0 with both knobs off, and with a backend built before
  this work. Not this work's and not fixed here.

Not done: an x64 consumer; cross-CGU and cross-crate sets; `try_call`
(regalloc2 has one clobber set per instruction, the unwind edge needs
the full one); Windows (the knob is off there); a self-hosted rustc was
not built with it.

### Shared panic tails: default on, a size change

`PLIRON_PANICSTUB` is in `DEFAULT_ON` (native targets; `=0` off).

| program | cold `call; trap` tails | sharing a tail | shared tails | alone in their function | .text, off -> on | instructions |
|---|---|---|---|---|---|---|
| pulldown-cmark | 398 | 118 | 48 | 280 | 500,676 -> 499,940 (-0.15%) | +0.05% |
| regex | 1575 | 542 | 173 | 1033 | 835,616 -> 834,176 (-0.17%) | +0.03% |
| rust-html-rewriter | 5260 | 488 | 169 | 4772 | 851,380 -> 850,228 (-0.14%) | -0.01% |
| rust-json | 3415 | 213 | 67 | 3202 | 442,580 -> 442,548 | 0 |
| rust-protobuf | 9160 | 354 | 123 | 8806 | 366,628 -> 366,500 (-0.03%) | 0 |

(Tail counts: `PLIRON_TRACE=pliron::panicstub`, the whole crate graph
of a native build; .text: the linux-musl binary with line tables, built
before the source-location key below, prebuilt std included. The
native binaries without debug info, final tree, `__text` off -> on:
pulldown-cmark 416,176 both, regex 751,152 -> 749,744, rust-html-rewriter
766,640 -> 765,616, rust-json 359,856 -> 359,792, rust-protobuf
283,440 -> 283,184.) Executed instructions do not move beyond what one
instruction in one hot function is worth, and that moves with the tree:
across four backend builds of the day the same knob measured
pulldown-cmark -0.06%, -0.06%, 0 and +0.05%, regex +0.30%
(`Fsm::next_state`, +153k), 0, -0.01% and +0.03%, rust-json 0, 0,
+0.07% (`parse_str`, one instruction a call) and 0. Of the two
sub-knobs, `PLIRON_PANICSTUB_STRIP=0` changed nothing and
`PLIRON_PANICSTUB_FWD=0` gave up pulldown-cmark's -0.06% in the build
that had it; both stay on.

What blocks the rest: 66-96% of the tails are the only one of their
(callee, trap code, argument types) in their function, usually because
`tailmergep` already merged the same-shaped ones. Sharing across
functions would need an outlined stub per location, which is the
`adrp+add` it would save.

One change: sites share a tail only if their calls have the same source
location. With line tables the panicking frame's line comes from the
call's return address, and one shared call reported the first site's
line for all of them; without debug info every location is the default
and nothing is kept apart (unit test `source_lines_stay_apart`). Not run
on wasm (the step returns early there).

Checked: all eight programs' output; the std corpus replayed with the
interpreter check, off against on: 4198 ok and 672 inconclusive either
way, 392,274 -> 391,601 CLIF instructions (-0.17%), 196 of 4870
functions changed; fuzz seeds 96000:96600 with the knob set (before
the source-location key) and 95000:95600 on the final tree with it on
by default, 600 pass each; the gate with the knob on by default.

### Scoped noalias: sound now, still opt-in

As written the `&mut` half never fired. `PLIRON_TRACE=pliron::scopedna`
(new: one line per `loadfwd` run with a candidate, why candidates drop
out) on five programs: every candidate failed "no longer a clone-entry
param" (regex 672 of 672 over 194 runs, pulldown-cmark 248 of 248).
The pipeline's first block merge folds the clone's entry param into its
source before `loadfwd` runs, and on a small example `phisimp` had
already folded it in the IR.

Keeping the param alive long enough (first by holding it back in
`phisimp` and `jumpthread`) made the fact reach the passes and four of
six programs miscompiled. Four bugs, each with a test that fails
without its fix:

- `pipeline::carry_one` inserted `noalias` on the surviving value and
  then asked whether that value was function-wide `noalias`, so a
  region-scoped fact always became function-wide: licm hoisted a load
  out of a loop that writes it (aho-corasick `Compiler::compile`).
  Test `carry_keeps_noalias_scope`.
- The same carry copied a scoped param's read-only `deref` onto the
  caller's own `&mut` param, replacing its writable one: an inlined
  `&self` method made `self.stack` "never written" in
  regex-syntax's `HeapVisitor::visit`. Test
  `carry_keeps_scoped_deref_off_wide_values`.
- A `&mut` param was a `noalias` root, i.e. a distinct object to every
  pass, while it is the caller's memory under a second name. Lowering
  now gives it the scoped mark only (under the knob); `loadfwd` alone
  treats it as a root stop with two names, on the machinery the
  operand-frozen stops already use (kill under both names,
  `kill_stops`), plus: a load through the stop also finds what is known
  under the caller's name, a store through it is recorded under both,
  and a store keeps locs of the same object that it does not overlap.
  Tests `store_through_the_param_kills_the_caller_name`,
  `caller_name_of_the_same_object_kills`, stanza `scopedna`
  (`tests/scopedna/main.rs`: wrong output on the first live version).
- Publishing a load's result under the caller's name ended in a value
  aliased to itself (rust-compression, verifier error): the load side
  is lookup only.

Holding the params back was also the wrong mechanism: with every
candidate kept through the first `loadfwd`, regex ran 7.1% more
instructions (`follow_epsilons` +15%, `next_state` +17%: the callee's
param hid the caller's function-wide `noalias` root), and after
dropping those candidates pulldown-cmark, rust-json and rust-protobuf
still ran 0.6-0.9% more (not traced to a pass). The final form holds
nothing back: a first pipeline step
`scopedfwd` runs `loadfwd` once on the function as lowered, only in
functions with a candidate, and candidates whose caller-side root is a
stack slot or the caller's own function-wide `noalias` param are not
seeded.

Final form, the knob on against the same backend's defaults:

| program | off | on | |
|---|---|---|---|
| pulldown-cmark | 52,528,195 | 52,864,224 | +0.64% |
| regex | 51,582,040 | 51,580,776 | 0 |
| rust-html-rewriter | 104,539,286 | 104,093,682 | -0.43% |
| rust-json | 51,853,297 | 51,488,114 | -0.70% |
| rust-protobuf | 65,690,420 | 65,066,483 | -0.95% |
| blake3-scalar | 49,075,163 | 49,066,078 | -0.02% |

What the `scopedfwd` run sees (`pliron::scopedna` trace, the whole
crate graph of a native build). "Kept": a store or call inside the
clone met locs under the param and was proven not based on it; "killed":
it was not proven.

| program | functions with a candidate | candidates | qualify | escape (rule 2) | no single source | stores kept / killed | calls kept / killed |
|---|---|---|---|---|---|---|---|
| regex | 465 | 3362 | 483 | 2559 | 320 | 120 / 117 | 143 / 66 |
| pulldown-cmark | 145 | 1127 | 241 | 839 | 47 | 11 / 52 | 5 / 13 |
| rust-html-rewriter | 358 | 1713 | 253 | 1233 | 225 | 166 / 66 | 88 / 42 |
| rust-json | 129 | 2069 | 590 | 1453 | 26 | 0 / 17 | 196 / 37 |
| rust-protobuf | 217 | 1796 | 529 | 1230 | 37 | 100 / 36 | 201 / 59 |

It stays opt-in. Three programs gain, pulldown-cmark loses 0.64%, and
that loss is not understood: +0.10% is there with the step off
(`PLIRON_SCOPEDFWD=0`; only the IR side then differs — `phisimp` keeps
the clone-entry args — and per-function counts move by hundreds of
thousands in both directions), the rest comes with the step. Without
load PRE in that run (`PLIRON_SCOPEDFWD_PRE=0`) pulldown-cmark is at
+0.43% but rust-protobuf at +0.55% instead of -0.95%, so PRE stays in.
Without the `phisimp` hold nothing reaches lowering on pulldown-cmark
(the count is the baseline's to the instruction).

Open: the pulldown-cmark loss; rule 2 (68-76% of the candidates are
handed to a callee or stored; counting `nocapture` operands as tracked,
`PLIRON_SCOPED_NCC=0` off, moved regex from 421 to 483 qualifying);
"no single source" at the start of the pipeline; licm and dse do not
use the fact; the corpus header records `noalias`/`scopedna` only for
entry params, so a corpus replay never has a candidate and `--check`
says nothing about this feature.

Checked: the five unit tests in `loadfwd_scoped_noalias.rs`, the two
carry tests, stanza `scopedna`; all eight held-out programs print the
expected output with the knob on (native), six under valgrind; fuzz
seeds 94000:94600 with the knob on, final tree: 600 pass (the first
live version failed in that range: 18 compile errors, 34
`env-compile-error`s and a run timeout, seed 94297). A memory-pass change wants a longer sweep than that before it
becomes a default.

### Scoped noalias: the pulldown-cmark loss explained and fixed (still opt-in)

Same day, later. On the tree of that hour the knob cost pulldown-cmark
+0.67% (51,928k -> 52,280k), and all but 15k of it was there with the
step off (`PLIRON_SCOPEDFWD=0`): the loss was the IR side, not
`loadfwd`. Per ELF symbol (callgrind `--dump-instr`, our two builds):
`Tree::append_text` +188k, `parse_block` +113k,
`scalar_iterate_special_bytes` +45k, a `Rev<Iter<u8>>::try_fold` +44k;
with the step on `item_to_event` +98k and `parse_block` -93k on top.
Two mechanisms.

**The `phisimp` hold hid the argument from the IR passes.** To let the
fact reach lowering, `phisimp` kept every clone-entry arg bound to a
`noalias` param as a block param, `readonly` ones included. In
`append_text` the inlined callee is the derived `<ItemBody as
PartialEq>::eq(&ItemBody::Text, &self[ix].item.body)`: the first
operand is a constant, and through the held param `constload` no longer
saw one. The discriminant test `ldrb; cmp #1` became a load from the
constant's memory and the whole derived `eq` switch (90 -> 173
instructions in the repro, `tests/scopedna/hold.rs`). The arg is now
held only where `loadfwd::ScopedNa` can use it (`phisimp::scoped_hold`):
the param is written through, and its source is not an address inside
an alloca (rule 1 refuses stack slots), not a constant, and not the
function's own `noalias` param at a constant offset (`scoped_seed` drops
those). That alone brought pulldown-cmark to +0.35%.

What the wider hold did elsewhere was not the fact either. On
rust-html-rewriter, by class (`PLIRON_SCOPED_PHI_KEEP=<bits>` holds one
class again; against the final rule): alloca fields -222k
(`exec_without_attrs` -326k, `pop_up_to` -97k, `exec_for_start_tag`
-86k against `memcpy` +201k, `as_lowercase_string` +104k: an alloca
kept from SROA), constants -166k (the benchmark's two closures), the
own `noalias` param -96k (`TagScanner`; no candidate is seeded for it).
Holding `readonly` args with the same source rule
(`PLIRON_SCOPED_PHI_RO=1`) gives pulldown-cmark -0.12% and takes 0.92%
from rust-protobuf. These are leads for SROA and `constload`, not
reasons to hold an arg: the 0.43% rust-html-rewriter had in the table
above was this, and is gone (-0.04% now).

**`scopedfwd` ran `loadfwd` where the fact had nothing to add.** The
step ran in every function with a candidate: 145 functions in
pulldown-cmark's crate graph, where 16 stores or calls were sheltered.
`loadfwd` with load PRE on the function as lowered, before the first
jump threading, is not free: the four `Rev<Iter<u8>>::try_fold`
scanners (`scan_rev_while`) went from 38 to 48 instructions in that
step, jump threading then left 29 where it leaves 20, and the final
code was 31 instructions against 22, with an unrotated loop behind 15
alignment `nop`s executed on every call (186k -> 318k for one of them).
The step now asks first (`loadfwd::scoped_probe`: the availability
fixpoint without PRE, nothing rewritten) whether a store or call
evicts a loc that rule 4 puts back, and runs only then
(`PLIRON_SCOPEDFWD_PROBE=0`: as before).

Result, knob off against on, one backend build (measured on a build
with the knob in `DEFAULT_ON`, `=0` for the off side):

| program | off | on | |
|---|---|---|---|
| rust-protobuf | 65.408M | 64.468M | -939,611 (-1.44%) |
| rust-json | 51.575M | 51.185M | -389,873 (-0.76%) |
| pulldown-cmark | 51.923M | 51.866M | -56,667 (-0.11%) |
| rust-html-rewriter | 102.529M | 102.492M | -37,354 (-0.04%) |
| regex | 50.657M | 50.655M | -2,043 |
| blake3-scalar | 49.040M | 49.040M | -5 |
| rust-compression | 44.236M | 44.236M | +16 |
| blind-sig | 34.695M | 34.696M | +608 (noise, below) |

blind-sig draws random numbers: one binary gives 34.68M to 34.71M from
run to run, and the function that carries the difference between two
builds (`montgomery`) has identical code in both. Its static code
differs in three functions by 10 instructions.

`PLIRON_SCOPED_NOALIAS` stays opt-in: the 2000-seed fuzz sweep a
memory-pass default needs died at about seed 457 of 2000 when the disk
filled (no verdicts were written). See the handoff section at the end.

The corpus header now names the candidates: `scopedna=` lists the
clone-entry block params that are scoped only, not just entry params
(`diffcheck::facts_header`). A std corpus recorded with the knob on has
candidates in 1216 `loadfwd` runs, 624 with one that qualifies, 732
sheltered stores and calls; `clif-check report --check` under
`PLIRON_VERIFY=full`: 4241 ok, 684 inconclusive, as with the knob off.
A corpus recorded before this has none (re-record: `gate.py --record`).

Checked: `probe_counts_only_what_the_fact_shelters`
(`loadfwd_scoped_noalias.rs`), stanza `scopedna` (now also: `append_text`
no larger with the knob on; 87 against 170 instructions with
`PLIRON_SCOPED_PHI_ALL=1`), all eight
held-out programs exit 0 with the expected stdout (linux-musl, the
binaries of the table; the expected file is empty for seven of them);
the gate with the knob default-on: 106 of 107 jobs, `simdshift` fails on
another session's uncommitted NEON narrowing (`PLIRON_VTRUNC_UZP=0`
passes), not on this. Not done: the fuzz sweep.

Open: rule 2 as before; the leads above (SROA on an alloca whose field
address is passed to an inlined callee, rust-html-rewriter -0.2%); the
readonly hold's rust-protobuf loss is not traced.

### Opt-level 0: a lifetime marker anchored on an instruction that moves

`{1,2,3}.symmetric_difference({2,3,4})` on std's `HashSet` gave `{4}`
at opt-level 0 (two hashbrown doc-tests under `crates.py --deps
pliron`). The function is instantiated in the user's crate from std's
optimized MIR: `difference` and `iter` are inlined into it and the
storage markers are kept, which the local crate's own MIR does not have
at opt-level 0 (rustc's `RemoveStorageMarkers`).

Lowering recorded a marker's position as "after the last instruction
emitted". For the markers at the top of the entry block that was the
last alloca's `stack_addr`, which the egraph takes out of the layout
and puts back at its first use, here the `chain` call 28 instructions
down. The start marker of the first `Difference` moved with it, below
the six stores that fill the slot. `slotmerge::use_flow` treats an
exactly placed start marker as a kill, so the slot was dead from its
last store to the call (`segs=[(8,13),(29,29)]` for a slot written at
8..13 and read by the call at 29), and the second `iter()` result was
given the same bytes. `stale_markers` did not object: a call after a
start marker counts as a write there.

Fix (`lower.rs`, `PLIRON_LT_PINANCHOR=0` off): the anchor steps back
over instructions the egraph may move (side-effect-free non-loads and
read-only loads, `marker_can_step_over`). They touch no memory, so the
marker keeps its place among the slot's accesses. In a 400-function
std-collections program at opt-level 0 four functions had such a hole;
none at -O in the two programs compared.

Age: reachable since 6dd307d (2026-11-13 06:58, `SLOTPROM_SYNCCOST`
default on; with `=0` slotprom still promotes these slots and nothing
is left to merge), found by `git bisect` over 645 commits in a scratch
worktree. The anchor rule itself dates from the start-marker kills
(2026-10-26).

Why nothing caught it: it was eight hours old, and it needs optimized
MIR from another crate lowered by the opt-level-0 pipeline with a
by-value aggregate handed to a callee that does not capture it. The
gate's opt-level-0 legs compile local code (no markers) and rustlantis
programs; every `poison:*` leg ran at -O. New: stanza `o0std`
(`tests/o0std/main.rs`: std's set, map, deque, heap and iterator
adaptors at opt-level 0 and 1, output against stock rustc's; fails
without the fix with 10 wrong lines) and `poison:o0std` in the gate
(`PLIRON_SM_POISON` crashes the unfixed build whether or not a merge
reuses the bytes); `quick.py` runs `o0std` with its core set.
`crates.py --deps pliron` doc-tests stay out of the gate (minutes).

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


### Sixth item: what HEAD measures, regex's `next_state`, the allocator-independent parts of the one-latch rotation

**The 54.56M pulldown-cmark figure.** HEAD (cbc0840) built `--release`
gives the fifth item's numbers exactly (pulldown-cmark 52.93M). The same
tree built with `--profile fast` gave 54.56M, and regex 52.80M instead
of 53.11M: the backend's output depended on how the backend was
compiled. `webmerge` memoized structural key hashes by node address
(`proots::khash_m`), including stack temporaries and keys that
`key_sub_h` builds and drops, and returned the hash of whatever had
been at that address before. Every hit is confirmed with `==`, so a
stale hash only lost a merge, but which ones depended on stack layout:
without crate-local ThinLTO the scanners' pointer/counter pair was not
recognized and `WM_KEEPCOUNT` did nothing. The memo now hashes the node
it is given from its children and remembers only nodes held by an `Rc`,
keeping a clone so the address cannot be reused (`proots::KHash`, test
`khash_m_ignores_recycled_addresses`). `fast`, `dev` and `release`
backends now produce the same code; measurements below are from `fast`
builds and reproduce the release numbers where both were run.

Exact totals, HEAD -> this item's defaults:

| program | LLVM | before | after | |
|---|---|---|---|---|
| regex | 45.97M | 53.11M (x1.155) | 51.58M (x1.122) | -2.9% |
| rust-compression | 39.72M | 44.51M (x1.120) | 44.28M (x1.115) | -0.5% |
| pulldown-cmark | 45.33M | 52.93M (x1.168) | 52.82M (x1.165) | -0.2% |
| rust-json | 45.55M | 51.91M (x1.139) | 51.85M (x1.138) | -0.1% |
| blind-sig | 31.51M | 34.71M (x1.101) | 34.68M (x1.100) | -0.1% |
| rust-protobuf | 60.76M | 65.72M (x1.082) | 65.69M (x1.081) | 0 |
| rust-html-rewriter | 85.83M | 104.49M (x1.218) | 104.54M (x1.218) | 0 |
| blake3-scalar | 46.63M | 49.08M | 49.08M | 0 |

**regex: why `PLIRON_LICM_INNERPRESS=2` lost, and the fix**
(`PLIRON_LF_TRIVPHI`, default on). With the `SparseSet` fields hoisted
out of the state-decode loop, `loadfwd2` forwarded the hoisted loads to
their re-reads in the outer body by giving every header and join of the
nest a block param, each fed by the one load or by another such param.
`sameargs2` dissolved the web thirty steps later; in between the varint
loop carried four extra params, `taildup2`/`mergechain` refused it, the
late threading never saw the iterator's `Option` test decided, and the
loop kept its `select`s (found by replaying the recorded function,
`clif-check pipeline`, and comparing which steps changed it). The PRE
now drops a new param whose arguments, followed through other new
params, reach exactly one value. `INNERPRESS=2` is the default: regex
53.09M -> 51.88M (`next_state` 9.70M -> 8.18M; the decode loop no
longer loads the fields), pulldown-cmark -0.18M, others within 0.04M.
Without `LF_TRIVPHI` regex is 53.27M. `PLIRON_UNSWITCH_LOOP` (the
per-state `b == EOF` test, written and tested on 2026-11-02, neutral by
wall time then) is default on too: regex -0.29M, nothing else moves.
`test.sh`'s `slotpromlt` stanza pins `LF_TRIVPHI=0` in the builds that
measure `RA2_XBLOCK`: the edge moves that knob removed were these
params'.

`hash_one::<&State>` (0.95M against LLVM's 0.04M) is not an inlining
miss: LLVM keeps `hash_one` out of line as well and calls
`Hasher::write` from it (0.66M). The pair costs LLVM 0.70M and us
0.95M; the difference is our inlined SipHash, not a call.

`next_state` after this: 8.18M against 6.63M. What is left in its two
loops: `b` and the `SparseSet` pointer are reloaded from the stack in
the dispatch loop (about 37 values live across `follow_epsilons`, see
`docs/loop-unswitch-pressure.md`), the dispatch is two range checks and
a compare where LLVM has two compares, and the loop is not rotated
(below).

**The one-latch rotation** (`PLIRON_LOOPROT_MERGE`, still opt-in). The
two effects named in the fifth item are fixed, each default on:

- *End pointer recomputed in the latch*: `PLIRON_ELABKEEP` (vendored
  elaborator, patch 134). A pure instruction the pipeline placed outside
  a loop is not elaborated inside it when one of its operands has no
  other use in that loop. The first version kept every hoist and cost
  pulldown-cmark 1.3M (`parse_block`: hoists whose operands the loop
  needs anyway add a live value each); with the operand test:
  rust-compression -0.23M, pulldown-cmark +0.08M.
- *Cursor copied around the loop*: the rotated loop's top block only
  stepped the cursor (`q = p + 8`, `p` otherwise unused), and the
  elaborator recomputed `p + 8` in each of three blocks with `p` kept
  live. New step `bumpedge` (last in the pipeline): such a param is
  stepped on the edges into the block, next to the load of the same
  pointer, which becomes `ldr x, [p], #8`.
- Found on the way: with one latch, regex's `follow_epsilons` header
  received its `u32` instruction pointer and the extended copy from the
  same block, `webmerge` kept the `u32` and re-extended it every trip
  (+0.44M). It now keeps the wide slot and makes the narrow one an
  `ireduce` (`PLIRON_WM_WIDE`).

`tests/parity/prog_loop.rs` with the knob: 20 instructions for a
skipped state -> 11 (LLVM 12, default 16). The programs, knob on
against off: rust-html-rewriter -0.39M, rust-json -0.17M,
rust-protobuf -0.03M, regex +0.25M (was +0.63M), pulldown-cmark
+0.78M, blind-sig +0.29M, rust-compression +0.04M. By ELF symbol
(the irdiff function table is by inlined function and shifts with
block merging) pulldown-cmark loses in `handle_inline_pass1` +0.32M,
`parse_block` +0.18M, `handle_inline` +0.15M, `unescape` +0.11M. In
`unescape` the body is unchanged and the back edge is `mov x19, x1;
mov x21, x23; cmp; b.hs; mov x23, x28; mov x25, x22; mov x1, x19; mov
x22, x25; mov x28, x23; b`: values live through the merged latch are
given a different register there than in the body and moved back.
`PLIRON_RA2_XBLOCK=1` does not remove the pairs. That is the blocker:
regalloc2's choice for a bundle piece in a block that only joins.

**irdiff runs all eight programs.**
- rust-compression: we do emit stack probes (inline, every guard page);
  Cranelift's probe loop stored below an unmoved `sp`, which Linux
  serves and valgrind does not. The loop now moves `sp` (patch 133,
  `PLIRON_PROBE_SP=0` off, `pliron-clif-opt/tests/stack_probe.rs`). Any
  program of ours with a frame over three pages crashed under valgrind
  before this. rust-compression: LLVM 39.72M, ours 44.51M.
- blind-sig: `open64` is aliased to `open` at link (musl 1.2.4 dropped
  the LFS64 names). LLVM 31.51M, ours 34.71M; 26.4M of ours is
  `montgomery` inlined where LLVM calls `SmallVec::extend`.
- `--tag T` gives our side its own build directory (parallel runs);
  LLVM's counts are reused while its binary is unchanged.

**Tried, not adopted.**
- `PLIRON_XCGU_SIZE=100/200` (the `to_ascii_lowercase` chain:
  `slice::Iter::fold` is 73 MIR statements, over the 50 cap, so
  `Map::fold` has no body in `extend_trusted`'s CGU and the chain stays
  three calls): rust-html-rewriter -0.83M / -0.93M and pulldown-cmark
  -0.8M, but rust-json +8.9M / +1.2M (`serialize_str` 20.9M -> 28.7M)
  and rust-protobuf +2.2M / +1.3M: the extra bodies change what the
  inliner spends its growth on elsewhere. Needs a rule for the chain,
  not a larger cap.
- `PLIRON_RA2_POSTOPT=3`: no change in any of six programs.
- `PLIRON_WM_KEEPCOUNT=0` on regex (suspected from the stale-hash
  build's 52.80M): no gain.

Not started: the html-rewriter line table beyond the chain above, the
three `closing` leftovers, the pulldown-cmark line list.

Validation: `harness/gate.py` (fast profile) passes after the
`slotpromlt` pin; fuzz seeds 93000:93700 pass (700); all eight programs
build and exit 0 under callgrind with the final defaults. Measured
again on the working tree at the end, which by then also held another
session's edits: pulldown-cmark 52.53M and blind-sig 34.65M, the other
six as in the table.

## Handoff: O0 HashSet miscompile and scoped noalias (2026-11-13 afternoon)

Written for someone with no memory of the session. Details and numbers
are in the two subsections "Scoped noalias: the pulldown-cmark loss
explained and fixed" and "Opt-level 0: a lifetime marker anchored on an
instruction that moves" above.

### The opt-level-0 miscompile: fixed, test in the gate

- Reduced repro: `tests/o0std/repro.rs` (6 lines). Commands:
  `rustc +nightly-2026-10-22 --edition 2021 -Zcodegen-backend=$PWD/target/fast/librustc_codegen_pliron.dylib tests/o0std/repro.rs -o /tmp/r && /tmp/r`
  prints `Some(1) Some(4) None` (order may vary; two `Some`). The bug
  prints `Some(4) None None`; `PLIRON_LT_PINANCHOR=0` on the compile
  brings it back. Edition 2021 is needed (array `into_iter`).
- Established: the wrong code is in the user crate, in its copy of
  `HashSet::symmetric_difference` built from std's optimized MIR; the
  pass is `slotmerge` (`PLIRON_SLOTMERGE=0` hides it), fed a start
  marker whose anchor (`stack_addr`) the egraph moved. See
  `PLIRON_SM_INFO=all PLIRON_SM_INFO_FN=symmetric_difference
  PLIRON_SM_INFO_SLOT=9 PLIRON_PAR=0`: before the fix `ss9 start pos=28`
  and `segs=[(8,13),(29,29)]`.
- Fix: `pliron-clif-lower/src/lower.rs`, `marker_can_step_over` and the
  anchor loop at the lifetime-marker lowering.
- Age: `git bisect` (645 commits, scratch worktree, each built into its
  own target dir) names 6dd307d, 2026-11-13 06:58; of its default flips
  it is `PLIRON_SLOTPROM_SYNCCOST` (`=0` hides the bug). ca07e7c
  (10-26) and everything up to 6dd307d's parent print the right answer.
- Ruled out: prebuilt std's own code; FASTCC, PANICSTUB, REMAT_CARRIED,
  LATEGVN, CMPCANCEL; -O (opt-levels 1, 2, 3, s are right).
- Gate: stanza `o0std` and `poison:o0std` (see above). Both fail
  without the fix (checked with `PLIRON_LT_PINANCHOR=0`).
- Not done, optional hardening: `slotmerge::stale_markers` excuses a
  call that reads a slot after a start marker ("presumably writes").
  A write of the slot that reaches a start marker with no end marker
  between, followed by such a call, contradicts the marker and could
  drop the slot's markers. It would have caught this bug class on its
  own. Check frame sizes (stanza `slotmerge`, `harness/frames.py`)
  if you add it.

### Scoped noalias: ready except for the fuzz sweep; left opt-in

- State of the tree: `PLIRON_SCOPED_NOALIAS` is opt-in. The two fixes
  (`phisimp::scoped_hold`, `loadfwd::scoped_probe` in `s_scopedfwd`)
  and the corpus header change are in and only act under the knob.
- Measured with the knob on (table above): no program worse beyond
  noise, rust-protobuf -1.44%, rust-json -0.76%.
- The one missing precondition for the default: 2000 fuzz seeds with
  the knob on. The run died on a full disk near seed 457 with no
  verdicts. Redo, with at least 10 GB free:
  `mkdir -p target/snfuzz && cp target/fast/librustc_codegen_pliron.dylib target/snfuzz/ &&
  PLIRON_PROFILE=snfuzz PLIRON_FUZZ_JOBS=4 PLIRON_SCOPED_NOALIAS=1 harness/run.py --suite fuzz --no-build --fuzz-seeds 120000:122000`
  (`PLIRON_PROFILE` only names the directory under `target/`, so the
  copy pins the backend while others rebuild `target/fast`). It took
  about 9 minutes per 450 seeds on the loaded machine.
- Then flip: add `"PLIRON_SCOPED_NOALIAS"` to `DEFAULT_ON` in
  `pliron-cfg/src/lib.rs`, change the AGENTS.md entry to "default on,
  `=0` off", rerun `PLIRON_PROFILE=fast harness/gate.py --jobs 4`. The
  gate passed that way this afternoon except `simdshift`, which fails
  on another session's uncommitted `PLIRON_VTRUNC_UZP` lowering.
- `target/corpus/std` was re-recorded with the knob on (4925
  functions), so the gate's replay has scoped candidates when the knob
  is on. `target/corpus/clcg` and `smoke` were not re-recorded.
- Commands used: `bench/heldout/irdiff.py PROG --backend DYLIB [--env
  PLIRON_SCOPED_NOALIAS=1] --tag T`, then per ELF symbol
  `symc.py PROG ir-pliron-A ir-pliron-B` and per instruction
  `asmc.py PROG ir-pliron-T FN` (scratch tools from the earlier
  session, not in the repo; they read callgrind `--dump-instr` output
  against `llvm-nm`/`llvm-objdump`).

### Traps

- blind-sig is not deterministic (random numbers): one binary gives
  34.68M to 34.71M. Do not read a 25k difference there as a result.
- Copy the backend dylib before a long measurement; `target/fast` is
  rebuilt by other sessions with their uncommitted edits in it.
- `test.sh --only=NAME` with an unknown name passes vacuously.
- `irdiff.py` lets cargo use every core; set `CARGO_BUILD_JOBS=4`.
- The disk went from 25 GB to 0.5 GB free in five minutes while swap
  was full (18 GB). Check `df -h` before a fuzz sweep; `run.py` loses
  every verdict when one write fails.
- Callgrind in docker fails now and then under load: rerun.
- At opt-level 0 the local crate has no storage markers
  (`RemoveStorageMarkers`); only MIR from other crates does. A
  self-contained opt-level-0 test needs
  `-Zmir-enable-passes=-RemoveStorageMarkers` to have any.

## Handoff: parity (2026-11-13 afternoon)

Written for whoever continues with no memory of the session. Counts are
callgrind executed instructions (`bench/heldout/irdiff.py`), in
millions. Run-to-run noise: blind-sig +-0.03M, regex +-0.0003M, the
rest under 10 instructions.

### Table

"start" is the tree at the session's start (ddb1668 plus the FASTCC
work). "now" is the working tree's defaults **as last measured**: the
run named `t13 defaults` = the `per=8` run below; the tree changed
after it only by a refactor of the SROA rule into
`frag_copy_stays_whole` (same condition) and an opt-in knob
(`PLIRON_PADJUMP`, default off). **Re-measure before quoting.**
"lean" is the same tree with `PLIRON_INLINE_LEAN=32` (opt-in).

| program | LLVM | start | now | now + `INLINE_LEAN=32` |
|---|---|---|---|---|
| rust-html-rewriter | 85.83 | 104.54 | 102.30 | 101.25 |
| pulldown-cmark | 45.33 | 52.53 | 51.66 (51.60 in a second build) | 51.87 |
| regex | 45.97 | 51.58 | 50.66 | 50.42 |
| rust-json | 45.55 | 51.85 | 51.58 | 51.11 |
| rust-protobuf | 60.76 | 65.69 | 65.41 | 63.99 |
| rust-compression | 39.72 | 44.28 | 44.24 | 44.16 |
| blind-sig | 31.51 | 34.67 | 34.67-34.70 | 34.67 |
| blake3-scalar | 46.63 | 49.08 | 49.04 | 49.04 |

### How to measure (copy these)

```sh
# backend: fast profile, snapshot the dylib so later edits do not move it
cargo build --profile fast --jobs 4 -p rustc_codegen_pliron -p pliron-clif-check
cp target/fast/librustc_codegen_pliron.dylib /tmp/be.dylib      # any scratch path
# one program, exact counts, own build dir (delete it afterwards: 0.5 GB)
bench/heldout/irdiff.py regex --backend /tmp/be.dylib --tag x1 --top 40 [--env PLIRON_INLINE_LEAN=32]
rm -rf target/heldout/ir-pliron-x1
```

A loop over the eight names with three in parallel takes 2-3 minutes
when the machine is idle and 15+ when its load is over 100. The first
line of the output is the total. To see *which ELF symbol* moved
between two builds, keep both build dirs and compare callgrind
`--dump-instr=yes` runs per symbol; the session's scripts did this
(`symc.py A B`, `asmc.py PROG DIR FN` for per-instruction counts with
disassembly) and are small enough to rewrite: run valgrind in the
`pliron-callgrind` image with `--dump-instr=yes`, sum the `0x...` cost
lines per `llvm-nm -n` symbol range. The irdiff function table is by
*inlined* function and shifts with inlining; use the symbol comparison
to decide what changed.

Traps:
- `cargo` does not track the backend dylib or `PLIRON_*`: irdiff
  deletes our side's dir per run, but a hand-rolled `cargo build` into
  an existing dir silently reuses old objects.
- A function whose name filter needs `bisect_name` (`PLIRON_SROA_DEBUG=<fn>`)
  prints nothing unless `PLIRON_OPT_BISECT=1000000000` is also set.
- `PLIRON_SROA_WHYFN=<fn> PLIRON_STATS=1` prints why allocas stayed
  whole; `llvm.br #0` means the address is a branch operand.
- Totals move by alignment `nop`s: a loop head padded to 64 bytes
  executes up to 15 `nop`s per entry on the fallthrough path. Before
  explaining a +0.1M by a transform, diff the per-instruction counts
  and look for `nop` runs (blake3 +0.02M and 99k of pulldown's
  `parse_block` cost under `INLINE_LEAN` were only that).
- A compile error in a repro leaves the previous object file in place;
  delete the `.o` before disassembling.
- Commit ae04d1b (the FASTCC commit) swept in this session's files as
  they were at 13:29: `vendor/regalloc2/src/ion/rehome.rs` (then opt-in),
  the `memcmpk` changes with tests, `inline_cost::lean_cost` and the
  first `INLINE_LEAN` code, `tests/parity/{bom,latch}.rs`.

### What changed, default on unless said (knob, effect)

1. `PLIRON_RA2_REHOME` (default 2, `0` off; `vendor/regalloc2/src/ion/rehome.rs`,
   described in `vendor/regalloc2/PLIRON_PATCH.md`): after allocation a
   value live through a join block (plus its jump-only edge blocks)
   takes the register it has on the cheaper side of the region's edges.
   regex -0.20M, rust-protobuf -0.28M, html -0.13M, compression -0.06M,
   pulldown -0.04M, blake3 +0.02M (alignment nops). Repro
   `tests/parity/latch.rs` with `PLIRON_LOOPROT_MERGE=1`: the latch goes
   from `mov; mov; cmp; b.hs; mov; mov; b` to `cmp; b.lo`. Checked with
   `PLIRON_RA_CHECK=1` on all eight. Compile time +0.03%.
2. `memcmpk` (`PLIRON_MEMCMPK_EQ=0`, `PLIRON_MEMCMPK_CONST=0` off):
   power-of-two sizes under zero tests are one compare, a constant side
   is read at compile time (`const_tables` now holds objects from 2
   bytes), compare chains fuse to `cmp; ccmp`. `tests/parity/bom.rs` 51
   -> 28 instructions (LLVM 24). html -0.48M, pulldown -0.15M, json
   -0.06M. Unit tests in `pliron-clif-opt/tests/memcmpk.rs`.
3. NEON pairwise ops and lane narrowing inline (`PLIRON_NEON_PAIRWISE=0`,
   `PLIRON_VTRUNC_UZP=0` off; `pliron-clif-lower/src/lower.rs`
   `neon_pairwise` and the `pliron.vtrunc` arm): `vpmaxq_u8` was a call
   to a one-instruction asm stub that clobbered every caller-saved
   register inside memchr's loop. `tests/parity/umaxp.rs`, test.sh
   stanza `neonpair` (`tests/neonpair/main.rs`, self-checking). html
   -0.78M, pulldown -0.41M, json -0.31M.
4. `PLIRON_XCGU_GLUEDEPTH` (`=0` off; `src/xcgu.rs`): drop glue does not
   spend cross-CGU descent depth. This, not the 50-statement cap, was
   what kept `Map::fold` out of `as_lowercase_string`'s CGU: the copy
   closure needs five levels of closure drop glue under `Iter::fold`
   (found with `PLIRON_STATS_XCGU=to_ascii_lowercase`, lines `xcgu
   reject`). html -0.24M, nothing else moves. `PLIRON_XCGU_DEPTH=16`
   gets html -0.51M but pulldown +0.33M and protobuf +0.26M: not taken.
5. `selxor` sign form (`PLIRON_SELXOR_SIGN=0` off): zigzag *encode*,
   `select (n < 0), !y, y` -> `y ^ (n >> 31)`. regex -0.44M
   (`cached_state`). `tests/parity/statekey.rs` (`zigzag`), unit test in
   `selxor.rs`.
6. `phisimp` (`PLIRON_PHISIMP_DEADPRED=0` off): in code the entry cannot
   reach, a block with one incoming edge and no path back takes its
   params' values. No effect alone; needed by `INLINE_LEAN` (below).
   Test `pliron-ir-opt/tests/phisimp.rs` (fails with the knob off).
7. `PLIRON_SROA_FRAGCOPY=<k>` (default 8, `0` off; `sroa.rs`
   `frag_copy_stays_whole`): a whole-copied aggregate cut into more
   pieces than words, two of them off word boundaries, with at most `k`
   direct accesses a piece, stays whole. regex -0.28M, html -0.61M,
   pulldown -0.27M, blake3 -0.06M; rust-json +0.10M (`parse_str` and
   `skip_to_escape` one instruction more per call, 38,850 calls each,
   memcpy +17k: not looked at further), compression +0.02M
   (`StoreAndFindMatchesH10` +75k against `HqZopfli` -72k). With no
   access bound blake3 goes 49.1M -> 73.4M: keep the bound. k = 2, 4,
   8, 16 were measured; 8 and 16 are equal.

Opt-in, with the blocker:

8. `PLIRON_INLINE_LEAN=<k>` (default 0): a callee of at most `k` live
   ops and one call (`inline_cost::lean_cost`: constant branches folded,
   slot loads/stores and dead pure ops free) passes the inliner's
   score; `State::lean_discount` keeps the caller's apparent size, call
   count and warm-call count as if the call were still there (without
   it rust-json +0.99M: `has_next_key` grew past its callers' limit).
   At 32: protobuf -1.42M, html -1.05M, json -0.47M, regex -0.24M,
   compression -0.08M, **pulldown +0.21M**. That is `parse_block` +303k
   against -113k of glue no longer called; of it 99k is one 15-`nop`
   alignment pad on a 6636-times path, the rest (+80k register moves,
   +41k stack loads by class counting) is not explained. Next step:
   measure `PLIRON_PADJUMP=4` (item 9) with and without `INLINE_LEAN=32`
   on pulldown; if pulldown is then no worse than without `LEAN`, add
   `("PLIRON_INLINE_LEAN", "32")` to `DEFAULT_VALUES` in
   `pliron-cfg/src/lib.rs` (the inliner reads it with `env_get`; check
   that `env_get` consults `DEFAULT_VALUES`, else change the
   `unwrap_or(0)` in `inline.rs`), then gate + fuzz. 24, 32 and 48
   differ only on html (101.29 / 101.25 / 101.20).
   Repro `tests/parity/dropv.rs`: `PLIRON_TRACE=pliron::inline=debug`
   prints `lean=<n>` per rejected site; remarks carry `lean=` too.
9. `PLIRON_PADJUMP=<n>` (default 0 = off, **never measured, never
   run**; `vendor/cranelift-codegen/src/machinst/vcode.rs`, just above
   the `gen_nop` pad loop in `emit`): a pad of `n` or more instructions
   starts with a jump to the padded block. pulldown executes 510k
   alignment nops (LLVM's build 3.6k). Next step: build, compile
   `tests/parity/*.rs` with `PLIRON_PADJUMP=4` and check a padded loop
   head disassembles as `b <head>; nop...`, run the gate with the knob
   exported, then irdiff all eight with `--env PLIRON_PADJUMP=4` (and
   `=2`). If it holds, default 4.
10. `PLIRON_LOOPROT_MERGE=1` (still opt-in). With `REHOME` the
    `unescape` rotation is gone (-51k there), but on vs off is still
    html -0.45M, json -0.17M, protobuf -0.14M against pulldown +0.69M,
    regex +0.39M, blind-sig +0.29M. Two blockers found:
    (a) derived header params are recomputed in the merged latch:
    `handle_inline_pass1` runs a new 12-instruction block per trip with
    two `mul #48` and a stack-slot rotation (`ldp/stp`) where the
    unmerged loop passed already-computed values on each edge;
    (b) blind-sig `montgomery`: a loop-carried value is split inside the
    body (`mov x10, x28` twice, `mov x28, x10` on the back edge, which
    also turns `b.lo head` into `b.hs exit; mov; b head`), +3
    instructions per outer trip. `REHOME` cannot fix (b): the register
    is busy inside the latch block. Next step for (a): in
    `looprot::merge_latches`, give the latch a param for a header param
    whose incoming values differ only by being derived from another
    param, instead of letting `webmerge`/`sameargs` re-derive it; find
    the pass with `PLIRON_CLIF=handle_inline_pass1` on pulldown and the
    first step whose dump shows the `imul` in the latch.
    Important side finding: the reported "raising `PLIRON_XCGU_SIZE`
    costs rust-json +8.9M" is this same missing rotation. At 100,
    `format_escaped_str_contents` becomes a copy, `serialize_str`'s
    byte loop keeps four back edges, is not rotated (11 instructions a
    byte against 7, table address rematerialized in the loop);
    `XCGU_SIZE=100` + `LOOPROT_MERGE=1` is 53.66M, not 60.74M.
    rust-protobuf's +2.2M at 100 is `grow_amortized` copied into
    `do_reserve_and_handle`'s CGU without `finish_grow` (over the cap),
    so the inlined copy calls what the home body had inlined (94
    instructions a grow against 58). A size raise must not admit a copy
    whose callee is left behind (`xcgu::degraded` computes this).

### Not started

- regex `next_state` (8.05M against 6.64M) and `cached_state` (12.19M
  against 11.11M). In `cached_state` the push arm's back edge re-runs
  two loop-invariant instructions and two alignment nops (`add x2,
  x28, x22, lsl #3; mov x1, #0x100000000; nop; nop`, 217k times): the
  invariants sit in the inner loop's preheader, which is inside the
  outer loop. `tests/parity/statekey.rs` (`state_key`, `cached_key`)
  reproduces the function; it also shows a double load (`ldr x1,
  [x24], #8` then `sub x2, x24, #8; ldr w26, [x2]`) for `ip as u32`.
- html-rewriter symbols with the largest gap to LLVM after this work:
  `TagScanner::finish_tag_name` (+1.6M), `try_allocate_in` (LLVM
  inlines it; ours is "external (no body in this CGU)" at 216 sites:
  check `PLIRON_STATS_XCGU=try_allocate_in`), `data_state` (LLVM tail
  calls the next state and uses `shrn`/`umaxp`; ours `uzp1` and
  `uzp1; uzp2; umax`: an ISLE rule `umax(uzp1 a b, uzp2 a b) -> umaxp`
  would finish it), `as_lowercase_string` (scalar loop 10 instructions
  a byte against 9: `add` of the counter ahead of its last use forces
  `mov x15, x4`).
- `closing` leftovers and the pulldown line list: untouched.

### Late change: `PLIRON_VTRUNC_UZP` is opt-in

The gate on the final tree failed one stanza, `simdshift` ("movemask not
vectorized"), with the `uzp1` narrowing on; `PLIRON_VTRUNC_UZP=0`
passed. It is now opt-in (`=1`), so item 3 above is the pairwise ops
only and the "now" column overstates html, pulldown and json by the
narrowing's share (not measured separately; `memchr`'s mask is `ushr;
movi; and; sqxtun` again). Next step: read `t_simdshift` in `test.sh`,
find the pass that recognizes the movemask (it matches `band` +
`unarrow`), teach it the `shuffle` form or do the `uzp1` rewrite after
it, then flip the knob and re-measure.

### Validation state

On the final tree: `PLIRON_PROFILE=fast harness/gate.py --jobs 4`
passed (107 jobs, includes the unit tests, corpus replays and the fuzz
regression seeds). The eight programs were last built and run under
callgrind one step before the final tree (before `VTRUNC_UZP` went
opt-in and the `frag_copy_stays_whole` refactor). Not run at any
point in this session: fresh fuzz seeds (700 were required), `harness/crates.py`, wasm, x86-64.
`PLIRON_RA_CHECK=1` builds of all eight ran for `REHOME` only, before
the later changes.

## Evening: handoff items run

The four items the handoff listed as not run, in the evening of the
same day. The tree moved on meanwhile (`283ad02`, `6091329`,
`35eba67`: the wasm-ld tombstone and the SelectJoin fix below), but
none of it touches the native pipeline these measure.

**Fresh fuzz.** Seeds 130000:130700 on default flags: 700/700 pass.
A parallel 120000:122000 sweep under scoped noalias: 1998/2000; the two
failures are compile crashes the verifier catches (a `loadfwd`
overflow panic and an `esplit` dominance break), not silent
miscompiles.

**Crates.** `harness/crates.py` tier 1, 20 crates: 0 pliron-only
problems. hashbrown 388/0, so both doc-tests that failed earlier now
pass. Report at `target/crates/report.md`.

**Re-measure of the eight programs.** `bench/heldout/run.py --rounds
5`, wall-clock A/B against LLVM (`/tmp/heldout.log`):

| program | LLVM ms | pliron ms | ratio | CI |
|---|---|---|---|---|
| blake3-scalar | 4.32 | 4.05 | 0.939 | [0.752, 1.482] |
| blind-sig | 1.36 | 1.50 | 1.096 | [1.030, 1.908] |
| pulldown-cmark | 4.81 | 5.51 | 1.147 | [0.621, 1.669] |
| regex | 3.42 | 4.13 | 1.209 | [0.601, 1.905] |
| rust-compression | 3.65 | 3.32 | 0.910 | [0.790, 1.611] |
| rust-html-rewriter | 5.69 | 7.36 | 1.293 | [0.896, 2.574] |
| rust-json | 3.60 | 3.85 | 1.068 | [0.591, 1.441] |
| rust-protobuf | 8.23 | 7.00 | 0.851 | [0.383, 1.700] |

Geomean pliron/LLVM 1.054 [0.917, 1.278], 8 of 8. Only blind-sig's
interval excludes 1.0 (slower); the rest are inside the noise on this
machine. Callgrind counts, which are stable, were not re-run.

**Wasm.** The full DSA `literate_wasm` module now instantiates and
runs under the pliron toolchain. Two bugs fixed:

- `pliron-wasm-ld` emitted `els.declared([0])` so remapped dead-region
  `ref.func`s validated. That pinned import 0 — here
  `__wbindgen_describe` — alive through wasm-bindgen's GC, and the
  generated module then demanded a `__wbindgen_placeholder__` import
  that nothing provides. Dead refs now map to a synthesized
  unreachable tombstone function (`283ad02`).
- `WasmBlock::SelectJoin` lowering evaluated each param's `select`
  and immediately wrote its destination local. A param select result
  could take a local that a later param's rematerialized condition or
  arm still read — in `Option<usize>::branch` the payload select
  consumed the out-discriminant just written, so `x?` on `Some(v)`
  yielded `Continue(0)` (found by `PLIRON_OPT_FNS` bisection of the
  DSA crash, a `ptr::sub` precondition panic inside hashbrown probing
  fed by a `?` on `Option<usize>`). The lowering now evaluates every
  select before writing any local, the same pattern `BlockParams`
  already used (`6091329`, test `vendor/waffle/tests/select_join.rs`).

Verified: `harness/quick.py` 13 steps; a full-opt
(`PLIRON_OPT_FNS=0..0x100000000`) DSA build passes `PageHandle.bare()`;
the web tests that load the pliron-built `pkg-lite` pass (caps, carry,
shelfmerge, warc, wireimport). Two web tests failed (tablekit,
syntax_peers) but run against the stock `pkg/` module, not pliron —
the tree was mid-refactor at the time. Still not run: x86-64.

A fresh `harness/parity.py` card ran at the end of the day: overall
94 (was 94 on 11-07). CMP 0.993 → 0.918, MEM 1.524 → 1.424, SIZ
1.710 → 1.519 all improved. **STK regressed 1.047 → 1.260**: the two
`driftsort_main` monomorphs are back to 8 KB frames against LLVM's
4 KB — the `memcpyopt` memset/sret-temp sink no longer fires there,
suspect the `frag_copy_stays_whole` SROA refactor. That is the first
thing to check next session.
