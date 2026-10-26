# Stack frames: two-phase facts, largest-first coloring, flow liveness, stale lifetime markers, and inlined-resume ends

Stage-2 rustc (pliron-built) overflowed the stack on six deep-recursion UI
tests. Its frames were 2.5–12× LLVM's. This session measured where the
stack goes, then changed how slotmerge decides that two stack slots may
share storage. Everything new is default-off; the configuration measured
below is every slotmerge/nocapture flag on (`PLIRON_XFACTS_ALL`,
`SM_COALESCE`, `NOCAP_FIELDS`, `NOCAP_CALLEE`, `SM_PRECISE`, `SM_DROPNC`,
`SLOTMERGE_MARK`, `SM_MARKFLOW`, plus the new `SM_BYSIZE` and `SM_FLOWIV`).

## 1. Where the stack goes

Smallest stack each test needs (binary search on `RUST_MIN_STACK`,
`--emit=metadata`; rustc's default is 17 MiB):

| test | LLVM stage 1 | pliron stage 2, start | after this session |
|---|---|---|---|
| `associated-consts/issue-93775` | 11 MiB | 19 MiB | 16 MiB (passes) |
| `parser/survive-peano-lesson-queue` | 9 MiB | 23 MiB | 28 MiB |
| `try-trait/deep-try-chain-issue-153583` | 5 MiB | 22 MiB | 28 MiB |
| `expr/if/expr-stack-overflow` | 9 MiB | 48 MiB | 49 MiB |

The last column is after the 5c fix. Before it, the same flags measured
16 / 22 / 22 / 46: some of that gain came from the miscompile.

Overflow backtraces (lldb with `${frame.sp}` in the frame format, stack
bytes summed per function):

- `expr-stack-overflow`: 89% `lower_expr_mut`. One nesting level is
  4,208 B for us vs 1,504 B for LLVM.
- peano: LLVM's rustc runs out in the parser; ours gets through parsing
  and dies in typeck, ~11.6 KB per level (`check_expr_kind` 4,864 B).
  LLVM inlines `check_expr_kind`, `confirm_builtin_call` and
  `check_argument_types_maybe_method_like` and still has smaller frames,
  so the gap is slot sharing inside frames, not inlining.

## 2. Two-phase cross-CGU facts (`PLIRON_XFACTS_ALL`, 456857c)

Each crate's first CGU saw no other CGU's nocapture facts; the backend now
uses rustc's thin-local LTO barrier to adopt from every CGU.
`lower_item_kind` 13,360 → 9,392 B. Cost: every CGU's post-stage-A IR is
held until the barrier (`rustc_middle` peak 6.4 → 8.2 GB,
`rustc_ast_lowering` 2.9 → 4.7 GB). A stage-2 build at `-j 8` ran the
machine out of memory; at `-j 3` earlyoom had to stop it; `-j 2` fits.

## 3. Largest-first coloring (`PLIRON_SM_BYSIZE`)

Slots were colored in start order, and a region only took members no
bigger than its first. A small temporary of a `match`'s first arm opened a
region a later, larger disjoint one could never join. Now: largest first
(LLVM StackColoring's order), segment-overlap tests, and a member's
stricter alignment raises the representative's. regex-syntax total frame
bytes −2.0%.

## 4. Flow liveness for non-escaped slots (`PLIRON_SM_FLOWIV`)

A slot's live range was the convex span of its uses in layout order, so it
covered every block laid out between two uses, and inside a loop every
arm's temporaries for the whole loop. `use_flow` computes byte liveness
over the CFG for every slot that doesn't escape:

- reads gen (a store of the slot's own address counts: its bytes become
  reachable through the container);
- a straight-line run of constant-offset stores (or a `mem*` call)
  covering every byte before any read kills; addresses are resolved
  through `iadd` of constants and through block params every incoming
  edge passes the same slot address in (rustc hands a `match` arm's
  temporaries to a shared tail that way);
- liveness is clipped to points reachable from some use (before its first
  access a slot is uninitialized; the convex span assumed the same);
- under `SM_MARKFLOW`, the end-marker kills `mark_flow` uses, and exactly
  placed start markers (anchor still in its block, or heading a block
  still in the layout). An `sret` call is not a kill — the callee may
  write only part of the slot — so without start markers a result slot
  stayed live round every loop it sat in.

regex-syntax `NestLimiter::check` (an inlined visitor loop around a
`match`): 19 slots each live 16..863 of 877 → 4 regions, frame 2,288 →
1,008 B (LLVM 416). Crate total frame bytes 88,384 → 78,432 (−11%); on
the 410 functions both backends emit, the gap to LLVM went from +47% to
+32%. Stage 2: `parse_path_segment` 4,048 → 2,736 B (LLVM 1,488),
`parse_local` 4,112 → 2,768, `lower_expr_mut` 3,920 → 3,184.

## 5. Three miscompiles the new packing exposed

With BYSIZE on, stage-2 rustc panicked in ICU's list formatter on any
warning that formats a list: `struct Z; fn main() {}` built as a binary
(as a lib it never formats the dead-code list, which hid the bug from one
check). Chasing it, and then the UI suite, turned up three separate bugs.

### 5a. Stale end markers

With BYSIZE off and `PLIRON_SM_POISON` on, the same build aborted with a
bad `free` in `rustc_mir_transform`: the liveness was already wrong.
Bisection, all with `PLIRON_SM_POISON` and `PLIRON_OPT_FNS`:

- crate: `~19rustc_mir_transform` bad; `harness/fnbisect.py` over its
  27,589 functions (62 runs, ~45 s each, 6 crates rebuilt per run):
  three culprits in `promote_consts` — `Vec<Candidate>::retain_mut`,
  `PromoteTemps::run_pass`, `Collector::super_basic_block_data`;
- `PLIRON_SM_POISON_SLOTS` → `ss26`; `PLIRON_SM_POISON_SITES` → site 0,
  "dead after `inst444`" in `run_pass`;
- the CLIF: `ss26` is written in one block, then far later read by loads
  and passed to two calls, with no write in between. An end-marker kill
  sits between them — passes forward or sink accesses without moving the
  markers.

Fix: `stale_markers` runs a forward "dead per markers" dataflow over
exactly the kills `mark_flow`/`use_flow` apply. A load reached while dead,
or a call reached while dead after an end marker, contradicts the markers;
the slot's markers are dropped (a marked slot whose address escaped then
counts as escaped). After a start marker a call presumably writes (an
out-param or `sret`), so it clears instead. `PLIRON_SM_STALECHECK=0`
restores the old trust. Cost on regex-syntax: +0.1% frame bytes (14 slots
in 11 of 283 functions distrusted). Tests: `slotmerge_stale_markers.rs`
(fails with the check off); `slotmerge_markflow.rs`'s
`second_life_leaves_a_hole` now gives the second life its StorageLive.

This did not fix the ICU panic. Poison passed in the full configuration,
so the liveness was now consistent, and the panic still needed BYSIZE and
`SM_COALESCE` together (either one off passed).

### 5b. Copy windows left unoccupied

`fnbisect.py` over the 8,511 ICU/fluent functions (18 runs, ~3 min each,
72 crates rebuilt per run) gave one culprit:
`icu_locale_core::LocalePreferences::to_data_locale_language_priority`.
A standalone cargo project on `icu_locale_core 2.3.0` reproduced it
(`en-US` came back as `US-US`).

In its CLIF, `ss3` (the 3-byte language, 4-byte slot) is copied to `ss18`
by `v = load ss3 … store v, ss18`, and the 8-byte `ss1` is written and
read between that load and store. Largest-first, `ss1` opened the region,
`ss3` joined it (disjoint), and `ss18` then coalesced with `ss3`: the
join tested only `ss18`'s own segments against the other members. With
the copy pair deleted, the language bytes stay in memory across the whole
copy window, and `ss1`'s stores overwrote them. Largest-first exposed it;
in `lo` order `ss1` could not join a region opened by the smaller `ss3`.

Fix: the coalesce join also requires the window to be free of other
members, and adds it to the region's occupancy (`PLIRON_SM_COALWIN=0`
restores the old test). Test: `slotmerge_coalesce_window.rs` (fails with
the fix off).

### 5c. Resumes that land in this frame

With 5a and 5b fixed, the stage-2 UI run dropped from 566 failures to 63,
seven of them new: option errors (`rustc --print` with no argument)
died on a bad `free`, `--cap-lints test` segfaulted. Flags: BYSIZE, FLOWIV
and MARKFLOW all needed; poison also failed, so the liveness was wrong.
`fnbisect.py` over `rustc_driver_impl` (25 runs, ~25 s each) gave
`handle_options`; slot bisect `ss15`, site bisect one segment end.

`ss15` (24 bytes, not escaped) is written by a `try_call` out-param at
the top and read in pads near the end, with no write between. Its flow
liveness had holes on paths that reach those reads. A new
`PLIRON_SM_INFO_SLOT=n` dump (accesses, kills, and the RPO block map) and
an independent recomputation found the cause: a `KILL_ALL` before
`try_call _Unwind_Resume` whose handler is another pad in the same
function. The pipeline adds `KILL_ALL` before every `_Unwind_Resume` call
on the premise that the unwind leaves the frame. But the inliner turns an
inlined callee's resume into an invoke of the call site's pad, so the
unwind comes back into this frame and that pad reads the caller's slots.

Fix: slotmerge ignores a `KILL_ALL` on a `try_call`
(`PLIRON_SM_RESUMETRY=0` restores the old behavior). Test:
`slotmerge_resume_handler.rs`, which fails with the fix off.

That alone cost stack: peano 22 → 29 MiB, deep-try 22 → 29,
expr-stack-overflow 46 → 52 (regex-syntax frame bytes +0.5%). The bogus
kill had been ending every inlined callee's escaped slots at its resume.
The true statement is narrower: unwinding out of the callee ends the
callee's locals. Under MARKFLOW the inliner now places an
`llvm.lifetime.end` before each such resume for every local of the
clone's entry block that has an end marker of its own
(`PLIRON_INLINE_RESUME_ENDS=0` skips it). Locals outside the entry
block, such as SROA's escape copies next to their calls, need not
dominate the resume; including them broke `erase_unreachable`. Result:
peano and deep-try 28, expr-stack-overflow 49, issue-93775 16 (passes).

### 5d. Ending the inlined callee's other locals (2026-10-26)

Rebuilding only `rustc_hir_typeck` with `PLIRON_SM_RESUMETRY=0` and `=1`
put the whole peano/deep-try loss in one frame: `check_expr_kind`, 4,848
→ 7,744 bytes, times about 2,000 levels of the check_expr recursion. Its
`PLIRON_SM_INFO` dump showed 95 marker-tracked slots (6.2 KB) live into
block200, the function's shared cleanup tail. The `try_call
_Unwind_Resume` of an inlined callee's pad leads there. The slots are all
`esc_seen` (their address reached an opaque call), so only the marker
window is sound for them, and with the end marker missing on the unwind
path the window ran on to the function's exits.

The entry-block rule was the gap. The new `PLIRON_INLINE_RESUME_INFO=
<caller substr>` counts, per inline, which callee locals got an end at
the resume. On regex-syntax 843 locals are in the callee's entry block
and 2,927 elsewhere. In check_expr_kind's inlines most are elsewhere:
`check_expr_assign_op` 91 with an end marker, `check_expr_break` 42.

Two default-off steps, both in `resume_ends`:

- `PLIRON_INLINE_RESUME_DOM=1`: a non-entry local also ends at each resume
  its block dominates. The dominator tree (Cooper–Harvey–Kennedy) is built
  once per inline over the clone's blocks, counting invoke pad edges. A
  first version used a reachability search per block. It counted resumes
  in dead blocks an earlier inline left in the callee as dominated, so it
  put ends there without a dominating def. Resumes the clone entry can't
  reach now get no end. Alone this gave check_expr_kind 7,488 and peano 27
  MiB: branch locals rarely dominate the one resume.
- `PLIRON_INLINE_RESUME_HOIST=1` (with DOM): a local that misses a live
  resume moves to the front of the clone entry, with its size constant if
  the clone defines it, and then ends at every live resume. Every alloca
  lowers to a static stack slot wherever it sits (`scan.rs` panics on a
  dynamic size), so the move matters only to SSA.

An end marker whose other operands don't dominate the resume is skipped
(a size operand next to the original end).

Clean stage 2, every flag plus both:

| test | before 5c | 5c | + DOM | + HOIST |
|---|---|---|---|---|
| issue-93775 | 16 MiB | 16 | 16 | 16 |
| peano | 22 | 28 | 27 | 22 |
| deep-try | 22 | 28 | 27 | 21 |
| expr-stack-overflow | 46 | 49 | 42 | 42 |

check_expr_kind 4,832, `lower_to_hir` 8,352 → 4,352, `parse_local`
3,088 → 2,672. regex-syntax frame bytes don't move (77,072 → 77,088);
5c had cost it only 0.4%. Test: `tests/resumehoist` (test.sh
`resumehoist`). It builds with both modes on under `PLIRON_CHECK_DOM`,
needs at least one hoist, and matches stock rustc's output.

## 6. Tooling used

- `fsz.py`/`frames.py`/`frametot.py` (session scratch,
  `~/.claude/jobs/f6a87689/tmp/`): frame bytes from `sub sp` sequences per
  symbol, per pair of builds, and totals.
- `btagg.py`: stack bytes per function from an lldb overflow backtrace.
- `memsample.py`: per-crate peak `phys_footprint` during a build.
- `clifflow.py`: every use of a value through block params in a CLIF dump.

## Checks at the end

After 5d (every flag, DOM and HOIST): the stage-2 UI suite has 53
failures. Six of them are cross-crate ThinLTO tests that `XFACTS_ALL`
refuses by design; the flags were exported for the whole run, so the
UI programs themselves also built with every flag. The only real
stage-2-only failures are `expr-stack-overflow` and `deep-try-chain`;
StackColoring-40883 now passes. The gate passes with defaults and with
every flag.

Before 5d:

- Stage 2, clean build, every flag: 56 UI failures (the stage-1 baseline
  is 57). Three fail only at stage 2, all of them older:
  `StackColoring-not-blowup-stack-issue-40883`, `expr-stack-overflow`,
  `deep-try-chain`. The ICU repro and `rustc --print` pass.
- Gate passes with defaults and with every flag. One defaults run out of
  four failed while a stage-2 build ran beside it; that failure was not
  captured.
- Fuzz 7000–7767 with every flag: all pass except seed 7738, an O0
  verifier ICE (`inst2059 has no results`) that also fails at 456857c
  with no flags set. It is unrelated to this work and still open.

## Open

- Remaining frame gap: `lower_expr_mut` 3,184 vs 1,232, `check_expr_kind`
  4,864, `lower_item_kind` 9,056 vs 1,264. `NestLimiter::check` shows the
  next lever: with slots at 280 B the rest of its 1,008 B frame is spills
  and callee saves.
- Callee locals with no end marker at all get no end at the resume
  (regex-syntax: 126 in entry blocks, 455 elsewhere). Synthesizing one
  needs a template for the intrinsic and a check that the slot is marked.
- `XFACTS_ALL` memory: compute facts in a separate first pass instead of
  holding every CGU's IR.
- Deciding defaults for the whole slotmerge flag set needs a crates
  tier 1–3 run and perf numbers.
