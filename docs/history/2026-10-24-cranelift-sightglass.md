# are-we-fast-yet / sightglass: Mach-O CFI, a nocapture blowup, and a slotmerge poison checker

are-we-fast-yet is cloned to `~/code/purerust/are-we-fast-yet`, with its
`sightglass` submodule. It was used as a correctness corpus: the test
suites of both workspaces ran under stock rustc and under pliron. The
`awfy` workspace matched (114 tests). `sightglass` pulls in all of
Wasmtime and Cranelift, and its tests run wasm benchmarks through a
pliron-compiled Wasmtime. It found five problems; debugging the last
one is what debug info (section 5) was built for.

## 1. `global_asm!` with `.cfi_*` on Mach-O (a94897e)

`wasmtime-internal-fiber`'s stack-switch functions failed to assemble,
and then to link:

- rsasm wrote no Mach-O call frame information.
- An FDE's PC-relative `pc_begin` had no relocation.
- `objmerge` dropped the `SUBTRACTOR` half of each relocation pair.
- `objmerge` turned `ltmpN` labels into section relocations.
- Raw `_name` asm symbols were prefixed again, giving `__name`.

The fiber's own tests pass now (`test.sh asmcfi`).

## 2. nocapture fixpoint never finished (1ac16ec)

A store into a frame-confined slot added a flow edge from the stored
value to every load of that slot, so the relation grew as stores × loads.
After sroa, one slot in cranelift-codegen's ISLE matchers reached 1.2e9
flow tuples, and `Capture::run` ran for more than 37 minutes. Stores into
a slot with more than 8 loads now go through the slot pointer. The crate
builds in 3.5 minutes.

## 3. The miscompile: sightglass-cli `instrumented_component_validates` SIGSEGV

The crash was in `cranelift_codegen::machinst::lower::compute_use_states`.
The bisection chain was:

- `fnbisect` with pliron on `cranelift_codegen` only and everything else
  stock: one function, `constructor_simplify`.
- `bisect.py`: step `sroa` unit 5193/5194, a pinned split of the 24-byte
  `ContextIter::next` result around an invoke.
- `PLIRON_NOCAP=0` fixes it, so the bad input is a nocapture fact.
  `PLIRON_NOCAP_ONLY=@file` / `PLIRON_NOCAP_LOG=file` (new) bisected the
  facts to `compute_use_states::{closure#0}`'s `[0, 1]`. That fact is
  correct; two consumers draw more from it than it allows.
- slotmerge was one consumer (`PLIRON_SM_LIMIT=n`, new, found merge #13).
  It led to the poison checker below and two real slotmerge bugs.
- SROA's call-boundary sync across an **invoke** was the other.
  `PLIRON_SROA_SYNC_INVOKE` (now opt-in) separates it: plain calls are
  fine, invokes miscompile. Ruled out:
  - the unwind trampoline;
  - Cranelift's compile-time egraph;
  - pliron's compile-stage passes (bumphoist/rematsink/lastuse).

  The IR right after the split reads correct, so the defect is in
  lowering or later. **Open**; sync across invokes is off by default
  until it is found.

## 4. slotmerge poison checker (`PLIRON_SM_POISON=1`)

slotmerge has needed 7–8 correctness fixes since it landed. Its failures
look the same every time: a wrong live range only bites when a merge
actually reuses the bytes, far from the cause. The poison mode is a
use-after-scope check for its analysis:

- it merges nothing;
- it overwrites each slot coloring would consider with `0xA5` bytes after
  its last use;
- it does the same at the head of every block outside the slot's
  interval that is entered from inside it.

A wrong interval then fails at the first stale read, on every run.

Knobs:

- `PLIRON_SM_POISON_SLOTS=a..b` and `PLIRON_SM_POISON_SITES=a..b` bisect
  the slot and the exit path.
- `PLIRON_SM_LIMIT=n` caps the number of merges.
- `PLIRON_SM_INFO` logs every poison site with its interval and block
  positions.

Its first hour found two real bugs:

- **Partial per-trip writes counted as fresh.** An 8-byte store to an
  array iterator's counter made all 64 bytes look rewritten every trip
  (tests/loopvec, a few seconds per bisect step). Only a write covering
  the whole slot counts now. This was 7 of the 9 gate failures under
  poison.
- **Nested loop bodies past the outer latch.** A span was (header start,
  back-edge source end) in RPO, but RPO placed a nested loop's body after
  the outer latch (`compute_use_states`, `ss43`). Spans now close over any
  span whose header lies inside them.

Tests: `pliron-clif-opt/tests/slotmerge_loop_spans.rs`. Each case fails
with its fix reverted.

Under poison the gate's stanzas pass except:

- slotmerge's own unit tests, which expect merges;
- the `pgo` wasm leg's `sort_check` (core's sort built for wasm32): open,
  likely another interval bug.

## 5. Debug info (`-g`): line tables, inlined frames, variables

Until now the backend wrote no DWARF, so every pliron frame in lldb or a
panic backtrace was `fn + offset`. It now writes it for ELF and Mach-O
objects. The pipeline:

- **Builder** (`src/debuginfo.rs`): rustc's scopes and spans become a per-CGU
  table (`pliron-ir-opt/src/dbg.rs`) of interned locations
  `(file, line, col, scope, inlined_at)`. Every op the Builder emits carries
  the current one as `Location::Dbg(id)`, a copy-sized variant added to
  pliron's `Location`.
- **pliron inliner**: the clones' locations get their inlined-at chain
  extended with the call site, so pliron's inlining shows up as inlined
  frames too, not only rustc's MIR inlining.
- **Lowering**: the id becomes the CLIF srcloc. Cranelift reports each
  function's machine-code ranges per srcloc, and phase C collects them.
- **Writer** (`pliron-clif-lower/src/dwarf.rs`, gimli):
  - a CU per object;
  - a line table, with an op that has no location continuing the previous
    row;
  - a `DW_TAG_subprogram` per function, with `DW_TAG_inlined_subroutine`
    trees rebuilt from the inlined-at chains.
- **Relocations** follow clang:
  - ELF: symbol relocations;
  - Mach-O: section-relative addresses, patched after layout, which ld64,
    dsymutil and lldb all accept.
- **Variables**:
  - `dbg_var_addr` on an alloca tags its stack slot with a `StackSlotKey`;
  - after compilation the frame layout gives each slot's FP offset, which
    becomes `DW_OP_breg29`/`breg6`;
  - types come from the layout: base types, thin and fat pointers, structs,
    tuples, closures, unions, arrays, C-like enums; other enums are opaque
    for now.
- **At `-g -O0`**, slotprom and slotmerge leave variable slots alone, so
  lldb shows every local, as with LLVM.
- **At `-O`**, the alloca spills rustc adds only for debug info are erased
  before the passes (`dbg::drop_spills`). Otherwise they skewed inlining and
  load forwarding, and `-g` changed the generated code.

`PLIRON_DWARF=0` drops the sections. Checked by test.sh `debuginfo`:

- std's own panic backtrace at O0 and O2 shows the inlined chain
  `leaf → mid → top` with the right lines;
- lldb shows the variables at a line breakpoint;
- `llvm-dwarfdump --verify` is clean;
- the optimized text is byte-identical with and without `-g`.

## 6. Miscompile: loadfwd PRE speculated a load onto a critical edge

The new line tables found this one. sightglass-cli's
`pca_metrics_succeeds_on_all_benchmarks` segfaulted on `ocaml-raytrace`,
and the backtrace now read:

> `BTreeMap::range` ← `FreeList::dealloc` (free_list.rs:275) ←
> `add_capacity` ← `DrcHeap::replace_memory` (Wasmtime's GC heap growth)

The bisection:

- `fnbisect` with pliron on `wasmtime` only (26 runs): one function,
  btree's `search_tree_for_bifurcation::<u32, (Bound<u32>, Bound<u32>)>`.
- A 20-line `BTreeMap<u32, u32>` range program reproduces it standalone.
- `bisect.py` on that program: CLIF step `loadfwd2`.

`edge_pre` (partial-availability PRE) made a load available on every
incoming edge by copying it to the end of each predecessor that lacked
it. Its soundness note claimed `notrap` loads are speculatable. They are
not: `notrap` only holds where the load was. Here the load read the
`Bound` key through a pointer that is null for `Unbounded`. The copy sat
before a `br_table` whose `Unbounded` arm never loaded.

Now a predecessor that can also branch elsewhere (a critical edge) needs
a `can_move` load. Splitting the edge, as LLVM does, would keep these
redundancies; not done. Test: `pliron-clif-opt/tests/loadfwd_critical_edge.rs`
fails without the fix.

## Performance notes from the same session

These are instruction counts (`/usr/bin/time -l`, load-independent) from
the real `bench/benches/kernels.rs` built against a criterion stand-in;
not landed.

- **selchain + mulcarry + stepcmp:** parse_only goes from +27% to +11%
  instructions (cycles +15% to +6%). No kernel regresses. Candidate for
  default-on after a crates run.
- **`PLIRON_XCGU_SIZE=300`:** hashmap goes from +45% to +25% instructions,
  binaries grow 3%. SipHash's `hash_one` sits in another CGU, beyond the
  native copy limit of 50. Compile time not yet measured.

## 7. Stage-2 self-host and fuzz: five more miscompiles

The stage-2 compiler (pliron-built rustc) ICEd compiling `core`.
Probing method:

- Rebuild only one stage-2 crate per step: invalidate
  `build/<host>/stage2-rustc/<host>/release/build/<crate>/*/fingerprint`
  and `./x build --stage 2 compiler`. That takes 40 s–2 min per step, not
  8.5 minutes for a full rebuild.
- `PLIRON_OPT_FNS='!~<len><crate>'` shows which crate's symbols are
  needed. `fnbisect --names` over those symbols, then `bisect.py`.
- Scripts: `rust-sh/selfhost-logs/s2tp/cmd.sh` and `s2op/cmd.sh`.

The bugs, in order of appearance:

1. **webroot `other` (5a3ca02):** as_place.rs:258 "could not resolve local".
   - A function argument, or a `try_call` result, had an empty pointer web
     in the block-refined query, so `select(arg, &slot)` looked like an
     exact slot address.
   - slotprom then promoted `resolved.as_ref().unwrap_or(self)` and read
     the stale `resolved` when it was `None`.
2. **slotmerge, partial loop writes read after the loop (3071348):**
   - Found by `PLIRON_SM_POISON` (wasm32 `sort_check`): core's
     smallsort scratch.
   - The gate now runs ten stanzas under poison (`poison:*`, 08e9343).
3. **selord (903eed6):** 28 bogus "non-defining use of `impl Trait`"
   errors.
   - `bool_mask` re-based `icmp eq (uextend z), 1` through `z`'s pair as
     if `z` were 0/1, but `z = select c, 2, -1` (an `Option<Ordering>`
     niche).
   - `collect_and_apply` then kept NLL region vars.
   - `bisect.py` blamed `sameargs2`, because `post_sweep` runs uncounted
     inside the `Gate::Always` `iconst128` step. `PLIRON_POSTSWEEP=0` and
     then `PLIRON_SELORD=0` found it.
4. **slp (a9fb1f8):** fuzz seed 4940.
   - `plan_store` re-issues the earlier store at the later one's
     position, and let a `uload32` (not a `mem_range` access) sit in
     between.
5. **slp/loopvec overflow ICEs (778a921):** seeds 4895, 5042, 5542.

Stage 2 now gets through typeck and borrowck of `core` and panics in
`rustc_monomorphize/src/partitioning.rs:349` ("the len is 110 but the
index is 15": the bounds check itself is wrong). That is the next one.

Fuzz: seeds 4096–6143 (2048 programs), 2043 matched, plus the
four above.

Tooling note: an lldb session on an x86_64 binary under Rosetta left
processes in uninterruptible state and wedged every later lldb launch
(`task_for_pid`). It needs a reboot; until then the gate's `debuginfo`
stanza fails (it has a 120 s limit and no longer hangs).
