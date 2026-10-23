# MIR move elimination: what it does to us (2026-10-22)

This assesses the upstream project goal "MIR move elimination"
([goals#396](https://github.com/rust-lang/goals/issues/396), RFC
[#3943](https://github.com/rust-lang/rfcs/pull/3943), Amanieu) against this
backend. The goal adds a MIR pass that removes copies between locals even
when their addresses are observed. It also changes MIR semantics so that the
pass is legal. The test programs are in
[`2026-10-22-mir-move-elimination/`](2026-10-22-mir-move-elimination/).

## Summary

- **None of this is on by default.** Everything is behind `-Zmir-move-elimination`
  (default `no`). Our pinned `nightly-2026-10-06` (rustc `ea137335b`,
  2026-10-05) has the flag and `TailCopyToMove` [2/6], but not
  `MoveElimination` [4/6]. The gate never runs either pass.
- **We never see `Operand::Move`/`Copy`, `StorageLive`/`StorageDead` or any
  other MIR directly.** All MIR reaches us through `rustc_codegen_ssa`, the
  same code LLVM uses. `src/` has no "moved-from place is dead" assumption.
  The only fact we take from MIR is `deduced_param_attrs`
  (`src/builder.rs:1180-1229`).
- **The risk is new IR shapes, not new semantics.** Call destinations land
  inside other aggregates, including inside the incoming sret and inside an
  enum payload before `SetDiscriminant`. The sret pointer escapes in the
  middle of a function. Temps get merged into argument memory. Storage
  markers are first deleted, then rebuilt in new places. I hand-wrote all of
  these shapes as `custom_mir` and ran them through pliron on the pinned
  toolchain. The output matches stock under the default settings,
  `PLIRON_SRET2REG=0`, `PLIRON_SLOTMERGE_MARK=1` and `PLIRON_SLOTMERGE=0`.
- **The runtime upside is large and real.** Measured with stock rustc on
  nightly-2026-10-22, pass on vs off: 1.4x to 3.1x faster on address-observed
  move chains. Pliron on the pinned nightly does *not* get these wins today:
  our copy-removal passes are blocked by the same address escapes that block
  LLVM.
- **Recommendation:** bump the toolchain. The bump is small, and another agent
  is already doing one in `../rcp-bump`. There is nothing to disable, because
  the pass is off unless asked for. Before claiming parity numbers under the
  flag, wait for [5/6] (storage reconstruction). Land the two `custom_mir`
  tests below as test.sh stanzas now.

## 1. The pieces

Status as of 2026-10-22. The PR numbers are rust-lang/rust.

| PR | piece | state | what it does to MIR |
|---|---|---|---|
| #163335 | [1/6] preparation | open | Adds the `StorageAlloc(local)` statement (allocates a live-but-unallocated local; a no-op in codegen_ssa and cg_clif). The inliner always emits `StorageAlloc` for a non-indirect call destination, because the destination must be allocated even if the inlined callee unwinds. Documents the new semantics. Fixes an async-drop use-after-move. |
| #163359 | [1.5/6] ZSTs always initialized | open | `RemoveZsts` only removes ZST assignments through indirect places, because a local only gets an allocation when it is first written. This adds ZST inits in shims and intrinsic lowering. |
| #163336 | [2/6] TailCopyToMove | **merged 09-30**, in our nightly | Rewrites `copy` to `move` in `Use`/`Aggregate` rvalues on the `Goto` path leading to `Return`, when nothing later on that path touches the local. It stops at indirect places and at writes to borrowed locals. It also **adds the `-Zmir-move-elimination` flag.** |
| #163337 | [3/6] PreciseLiveness | merged 10-09 | Analysis only (`rustc_mir_dataflow`). For each local it computes the points where the local needs its own allocation. It splits each statement into an early point (operand reads) and a late point (destination writes). A range starts at a direct write. It ends at `StorageLive`, `StorageDead`, a whole-local `move`, or (for never-borrowed locals) the last use. Call move-args stay live through the late point, so they conflict with each other and with the destination. |
| #163338 | [4/6] MoveElimination | **merged 10-21** | Unifies the two sides of `dest = src` and `dest = Aggregate(..ops)` when their PreciseLiveness rows don't overlap. A local can map onto a projection of another local (`_3` becomes `_1.0.1`, or `(_0 as Some).0`) through stable offsets only: no `Deref` or `Index`, no SIMD, no packed fields, no `DynMetadata`, no rust-call tuples. Temps can map onto args or onto `_0`, but two special locals are never merged. It then **turns every `StorageLive`/`StorageDead` in the body into `nop`**, not only those of merged locals. Finally an alias fixup pass deletes self-assignments, routes overlapping `Use` assignments through a temp, and splits aliasing aggregates into per-field writes (enum: fields first, then `SetDiscriminant`). The flag also turns off `DestinationPropagation`, which this pass replaces. |
| #163339 | [5/6] storage reconstruction | open | Rebuilds `StorageLive`/`StorageDead` for all locals from the post-unification liveness matrix. `StorageLive` sinks to the first write, possibly inside a branch. `StorageDead` is placed after the last use for unborrowed locals, at the head of the successor block when a terminator ends the range, and at function end for borrowed locals. It splits critical edges, and it can emit duplicated or unpaired `StorageDead` (see the `storage_live_elided_on_join_fork` test). |
| #163340 | [6/6] Miri | open | Implements the semantics: `LiveUnallocated` locals; a whole-local `move` frees the allocation during operand evaluation; call destinations are evaluated last. |

**Semantics under the flag** (taken from the `syntax.rs` docs in [1/6]):
- `StorageLive` makes a local live but *unallocated*. The local is allocated
  when a destination place based on it is evaluated, or by `StorageAlloc`.
- `move _x` of a whole local reads the value, frees `_x`'s allocation, and
  leaves `_x` live but unallocated. This happens during operand evaluation,
  before later operands are evaluated. Accessing memory after a move is UB
  (RFC).
- `Assign` evaluates rvalue operands before the destination place. `Call`
  evaluates the destination last. A direct call destination must not be
  rooted in a local that is also a whole-local move argument.
- MIR assignments still require that source and destination do not overlap
  for non-scalar types. The alias fixup restores this after unification.

**Pipeline position and gating.** In `run_optimization_passes` the order is
`DeadStoreElimination::Final`, then `DestinationPropagation` (off under the
flag), then `TailCopyToMove`, then `MoveElimination`, then
`SimplifyLocals::Final`. Both new passes use
`PassPolicy::optional(mir_opt_level >= 2 && mir_move_elimination)`, so they
run at `-O` (mir-opt-level 2) and not at `-Copt-level=0` (level 1). The flag
is `[TRACKED]` and is not a target modifier, so prebuilt std/core MIR,
including the generic bodies we instantiate, does not get the pass.

**Flags** (all checked on nightly-2026-10-22):
- Off (the default): leave out `-Zmir-move-elimination`, or pass
  `-Zmir-move-elimination=no`. This is the only complete off switch.
- `-Zmir-move-elimination -Zmir-enable-passes=-MoveElimination,-TailCopyToMove`
  skips the two passes. It still keeps the new semantics and still has
  DestProp off, so its MIR is not the same as the default's.
- Avoid `-Zmir-enable-passes=+MoveElimination` *without* the flag. It
  force-runs the pass under the old semantics, with DestProp still on.
  rustc's help warns that this route enables passes "without further
  warning".
- A codegen backend cannot opt out of a single MIR pass. `CodegenBackend`
  has `init(&EarlySession)` (read-only options) and `provide(&mut
  Providers)` (we don't override any query). In theory we could override
  `optimized_mir`, but that is not a sane way to drop one pass. If we ever
  need to, `init` can check `sess.opts.unstable_opts.mir_move_elimination`
  and warn.

## 2. Is any of it active on our pinned nightly?

`TailCopyToMove` and the flag are present (`rustc +nightly-2026-10-06
-Zhelp`), but the pass is gated by the flag, which is off by default. **No
gate, corpus or crates run exercises it.**

Even when it is on, it is codegen-neutral. I compiled
`2026-10-22-mir-move-elimination/moves.rs` with
`-Zmir-enable-passes=+TailCopyToMove` (stock, pinned nightly). It changed 10
MIR lines (`copy` to `move` before return), and the LLVM IR was
**byte-identical**. codegen_ssa lowers `Use(Move)` and `Aggregate(move ..)`
the same way as their `copy` forms. `Move` only matters for indirect call
arguments, and TailCopyToMove never touches those.

I also ran pliron on the pinned nightly with `-Zmir-move-elimination`, which
here means TailCopyToMove plus DestProp off. `moves.rs` ran correctly, with
the same timings as without the flag.

## 3. Audit: where we could break

How the code divides up: everything below `rustc_codegen_ssa` is ours.
codegen_ssa decides how to lower `Operand::Move` (it passes the place's
pointer directly for `PassMode::Indirect` arguments) and how to lower
storage markers (`lifetime_start`/`lifetime_end` on our `Builder`,
`src/builder.rs:1129-1138`). So MIR semantics reach us only as IR shapes,
plus one MIR-derived fact. Ranked by risk:

1. **Medium: interior and escaping sret / indirect-arg pointers.** MoveElimination
   makes `(_0.1: Big) = f(..)`, `((_0 as Some).0: T) = f(..)` and `_3 =
   &(_0.0)` common. Before, MIR almost never took `&_0`, and call
   destinations were fresh locals. The code that depends on how sret and
   indirect pointers behave:
   - `pliron-clif-opt/src/slotmerge.rs:18-22, 769-781`: an sret argument
     counts as a *point use*, not an escape ("the ABI forbids the callee
     retaining it"). This still holds: rustc marks indirect args and sret
     `noalias captures(address)` (`rustc_target/src/callconv/mod.rs`,
     `indirect_pass_mode`). If upstream ever relaxes that attribute to let
     the address of `_0` live past return, this rule becomes unsound.
   - `src/abi.rs:282-326` (sret2reg, callee side) replaces the incoming sret
     with a local slot and loads the leaves at `return`. With move
     elimination, that slot's address now escapes to callees in the middle
     of the function, and callees write through it. This is correct only
     while every pass treats the new slot as escaped, which they do: escape
     detection works on IR uses. `src/abi.rs:328-370` (caller side) stores
     the leaves through `ptr_plus(cargs[0], off)`. That already handles
     interior pointers, which are now common.
   - `pliron-ir-opt/src/memcpyopt.rs:91-100` accepts the caller's own sret
     as a forwarding destination only when the sret is `non_escaping`.
     Escaping sret pointers just make this pass bail.
   - `pliron-clif-opt/src/loadfwd.rs:505-640` (`isolated_escapes`) and
     `dse.rs:1-12` treat `noalias` params, sret included, as isolated roots
     only while they don't escape, and a call use is never benign. That is
     sound by construction, but expect less isolation, and so less
     forwarding, in callees.
   - `pliron-ir-opt/src/sroa.rs:200-225` (`pin_extent`) uses the sret extent
     `(ret size, align)` from `src/builder.rs:1207-1216` for each operand.
     For an interior sret this pins `[p, p+size)` at the right offset.

   **Checked today:** `mirshapes.rs` (`make4`, `wrap`, `wrap_opt`) builds
   each of these shapes by hand, and sret2reg fires on 3 of them. Output
   matches stock.

2. **Medium: `deduced_param_attrs` (`src/builder.rs:1180-1229`).** This is
   the one fact we take from MIR. It is rustc's analysis of the callee's
   *optimized MIR*, and we turn its `captures_none` into `nocap`. That
   `nocap` makes an Indirect argument a bounded use in slotmerge (`:769-781`)
   and a bounded escape in loadfwd (`:630`). MoveElimination changes the MIR
   this analysis reads, because temps (borrowed ones included) get merged
   into argument locals. Today that only makes the deduction more
   conservative. `deduce_param_attrs.rs` changed between our two nightlies,
   but only an early-bail heuristic, and it has no flag-specific logic. If
   upstream later teaches it the new semantics (for example "a moved-out
   argument is dead"), our use is only as sound as LLVM's, and that is the
   same trust we already extend.

3. **Low (High only if we enable `PLIRON_SLOTMERGE_MARK`): storage-marker
   placement.**
   - Markers are recorded in `pliron-clif-lower/src/lower.rs:6195-6210`.
   - They are trusted only in `slotmerge.rs:380-430, 921-934` under
     `PLIRON_SLOTMERGE_MARK`, which is off by default. In that mode, marked
     slots also skip the backedge rule (`slotmerge.rs:995`).
   - [4/6] deletes every marker. MARK mode then quietly falls back to the
     default behaviour, which is fine.
   - [5/6] brings markers back in new places: `StorageLive` only on some
     paths, `StorageDead` with no matching `StorageLive` on other paths,
     duplicated `StorageDead`, and `StorageDead` at a successor's head. For
     borrowed locals in loops, the `StorageDead` goes at function end.
   - `markers.rs` builds these shapes (fork/join with a duplicated
     `StorageDead`, and a borrowed local live across a loop). Pliron matches
     stock in both modes.
   - Re-check MARK mode against real [5/6] output before ever turning it on.

4. **Low: everything else that sees markers treats them as no-ops.** That
   is `sroa.rs:300` (`inherit_markers`) and `:636`, `nocapture.rs:162,339`,
   and `xoridem.rs:190`. Unpaired or duplicated markers are harmless to
   them.

5. **Low, performance only: bigger merged allocas.** A temp that used to be
   its own promotable alloca can become a field of an aggregate that
   escapes, for example one passed by `&` or used as an interior sret.
   `sroa` (pins plus syncs) and `slotmerge` (whole-slot granularity) then do
   less with it. LLVM has the same problem, which is why upstream excludes
   rust-call tuples. Measure this after the bump. It does not affect
   correctness.

6. **No risk: Move vs Copy.** `src/` never branches on it, and the IR for
   TailCopyToMove's rewrites is byte-identical (section 2). Enum payloads
   written before `SetDiscriminant` reach us as ordinary stores followed by
   a tag store, and codegen_ssa could already emit that shape.

## 4. Measurements (stock rustc, nightly-2026-10-22 `1a2c4797e`, `-O`)

`moves.rs` has five kernels. Each one observes addresses with
`black_box(&x)`, which is exactly the case LLVM cannot optimize. All runs
were interleaved, 3 rounds, best of 5; spread was under 1%.

| kernel | flag off | flag on | change | pliron, pinned nightly | q-reg ld/st pairs, off to on | frame bytes, off to on |
|---|---|---|---|---|---|---|
| `wrap2` (`Some(wrap(make_big()))`) | 17.85 ns | 5.77 ns | **3.1x** | 15.7 ns | 26 to 0 | 272 to 48 |
| `nest` (tuple of 2 Big + array) | 18.5 ns | 7.45 ns | **2.5x** | 19.0 ns | 20 to 0 | 352 to 64 |
| `parse2` (`Result` + `?` chain) | 38.4 ns | 23.1 ns | **1.66x** | 55.7 ns | 39 to 34 | 464 to 480 |
| `push_many` (`Vec<Big>::push`) x64 | 741 ns | 515 ns | **1.44x** | 596 ns | 28 to 16 | 384 to 368 |
| `build_chain` (`self -> Self` builder) | 29.6 ns | 31.7 ns | **-7%** | 46.5 ns | 0 to 0 | 448 to 1008 |

- **No `memcpy` libcalls in either variant.** These copies are 136 to 192
  bytes, so LLVM inlines them as q-register load/store pairs, and those
  pairs are the copy count in the table.
- **`build_chain` gets worse.** Each `set(move x)` call's argument and
  destination conflict, so nothing can be unified. Meanwhile the deleted
  storage markers cost LLVM its stack coloring: the frame grows from 448 to
  1008 bytes. [5/6] exists to fix this. Our slotmerge does not use markers
  by default, so we should not see this regression.
- **On regex-syntax 0.8.9** (`--emit mir`, flag off to on):
  - locals: 19,414 to 16,082 (-17%)
  - plain `x = copy|move y` assignments: 6,420 to 5,348 (-17%)
  - non-marker statements: 30,195 to 28,940 (-4%)
  - `StorageLive`/`StorageDead`: 32,739 to 116
  - stock compile user time was 2-5% lower with the flag (noisy; load 6-7)

  For us, the first two lines mean less IR to build and optimize. The marker
  line means about 33k fewer `llvm.lifetime.*` intrinsic ops, until [5/6]
  brings them back.
- **Side finding:** by default we pay to build every lifetime marker and
  never use it. Lowering records markers and then emits nothing
  (`lower.rs:6195`), and slotmerge ignores them unless
  `PLIRON_SLOTMERGE_MARK` is set. Making `lifetime_start`/`lifetime_end` a
  no-op is a possible compile-time lever. I have not measured it. Check the
  `sroa.rs:300` comment before trying it.
- **Unrelated gap:** pliron is 1.57x slower than stock on `build_chain`
  (46.5 vs 29.6 ns), a `self -> Self` builder chain of 200-byte structs.
  It's worth a bench kernel.

**Bump size.** Between the pinned nightly and 10-22, codegen_ssa's
`traits/` diff touches five places:
- a new required `IntrinsicCallBuilderMethods::ctpop`
- a new required `type_ppcf128`
- `br_with_attrs` now takes `&[UnrollAttr]`
- `print_pass_timings`, `print_statistics` and `print_statistics_json` were
  removed from `CodegenBackend`
- a new `incr_comp_session` parameter in `traits/write.rs`

The whole compiler diff is around 500 lines in codegen_ssa and 1,000 in
rustc_middle. There is no earlier bump in git history to compare with, since
the repo was imported at 9169c1d on 2026-10-07. A toolchain-bump worktree is
in progress (`../rcp-bump`, branch `toolchain-bump`, by another agent).

## 5. Recommendation

- **Bump: yes, on the normal schedule.** The pass is opt-in, so a bump changes
  nothing for default builds. After the bump, add the flag as an
  *experiment*, not a default: build the corpus, gate and `crates.py --tier
  1` with `RUSTFLAGS=-Zmir-move-elimination` once, and compare runtime parity
  against stock *with* the flag.
- **Wait for [5/6] before quoting parity under the flag.** Without it, stock
  LLVM loses stack coloring and we don't, so we would look better than we
  are.
- **Disable: nothing to disable today.** If it later becomes the default and
  causes trouble, use `-Zmir-move-elimination=no` for the whole thing, or
  `-Zmir-enable-passes=-MoveElimination` to skip only the pass. A backend
  cannot veto a MIR pass.
- **Regression tests to land now.** Each runs on the pinned toolchain and
  passes today.
  1. `tests/moveshapes`, from `mirshapes.rs` (custom MIR):
     - sret escaping while partly written, with a callee writing through it
     - a call destination at a non-zero offset in `_0`
     - an enum payload written in place, then `SetDiscriminant`
     - two address-taken locals in a loop with no markers
     - a callee scribbling on its by-value argument

     Run it as a stdout diff against stock, plus `PLIRON_SRET2REG=0` and
     `PLIRON_SLOTMERGE_MARK=1` variants. It also documents the shapes for
     whoever does the bump.
  2. `tests/markers`, from `markers.rs` (custom MIR): [5/6]-style unpaired
     and duplicated `StorageDead`, and a `StorageLive` on only one branch,
     with escaped slots. Run with and without `PLIRON_SLOTMERGE_MARK=1`.
  3. After the bump: a `moves.rs`-style stanza compiled with
     `-Zmir-move-elimination` and diffed against stock. Also re-record the
     std corpus with the flag and run `clif-check report --check`.
  4. CLIF unit tests in `pliron-clif-opt/tests/` (no rustc needed):
     slotmerge, dse and loadfwd on `call f(sret slot+16)` followed by reads
     of the slot's other fields, and on an sret param that escapes to a call
     and is then stored to.
- **Which passes become partly redundant under the flag:**
  - `memcpyopt` (IR call-slot forwarding) does what MoveElimination does for
    call results, and the MIR pass does more: interior destinations, enum
    payloads, address-observed sources. Expect it to fire rarely.
  - `slotmerge` will see about 17% fewer locals. It is still needed for
    disjoint scopes that are not related by an assignment, and that is most
    of its frame-size work.
  - `loadfwd`/`dse` lose some copy chains (store-to-temp then reload) but
    keep their main job.
  - `memcall` fires less often but is still needed for user
    `ptr::copy_nonoverlapping`.
  - `sroa` and `sret2reg` are orthogonal (scalarization and ABI). They stay.

  None of these can be deleted: the flag is opt-in, and std is built without
  it.

## Toolchains

`nightly-2026-10-22` (`1a2c4797e`, with `rustc-dev` + `rust-src` +
`llvm-tools-preview`) was installed during this session by the concurrent
`toolchain-bump` agent, not by this assessment. I used it read-only. If the
bump is abandoned, `rustup toolchain uninstall nightly-2026-10-22` removes
it.
