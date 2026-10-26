# Stack frames: start-based interference and after-call kills

Follows `2026-10-25-slot-coloring.md`. Both changes are default-off and
work under `PLIRON_SLOTMERGE_MARK=1 PLIRON_SM_MARKFLOW=1`.

## 1. Where `lower_item_kind`'s frame went

Stage 2 with every slot flag (including `PLIRON_INLINE_RESUME_DOM`/`HOIST`)
gave `LoweringContext::lower_item_kind` a 9,040-byte frame; LLVM's is
1,264. `PLIRON_SM_INFO` showed 8,088 of it was stack slots after merging,
not spills: 42 escaped slots (2,312 B) and 67 regions (5,776 B) from 1,053
slots. Each `match` arm kept two 328-byte slots (and some 104-byte ones)
that never merged. `PLIRON_SM_INFO_SLOT=596` showed why: every such slot
was live into two exits, `trap` after an inlined resume and `call
panic_in_cleanup; trap`, so all arms' slots overlapped there.

A 15-line reproducer (`match` with three arms, each a 320-byte `Drop`
value passed to a `C-unwind` function) gave a 1,280-byte frame against
LLVM's 352. Two causes:

1. **Lost end markers.** `PLIRON_LT_PROBE=1` put the loss in jumpthread:
   each arm's end marker opened a forwarding block after the drop call
   (`call drop; jump b41; b41: jump b22; ...`). `entry_kills` hung the
   marker's kills on those jumps, and jumpthread deleted every one. With
   no kill left, the slot fell back to escaped for the whole function.
2. **Overlap is too strict.** MIR has no StorageDead on unwind paths, so
   the arms' slots are all live in the shared abort tail. LLVM's
   StackColoring merges them anyway: `llc -no-stack-coloring` on the same
   IR gives 960 B against 352. Its interference test (see the PR32488 note in
   `StackColoring.cpp`) is that two slots conflict only if one *starts*
   being in use while the other is live. Two slots that are both live in
   a block reached from different arms never are in use together.

## 2. `PLIRON_SM_AFTERKILL=1`

When the forwarding-chain walk in `entry_kills` reaches a `jump` from a
block that isn't all quiet, it also puts an `AFTER_INST` kill on the last
call before that jump. It does this only if the instructions in between
are quiet and don't touch the slot's address. The call keeps its identity
when jumpthread folds the chain. The anchor is calls only, because store
sinking can move a store past a later use. `mark_flow`, `use_flow` and
`stale_markers` treat it as a kill just after the instruction.

## 3. `PLIRON_SM_STARTS=1`

In coloring, slot S may join region r unless one of S's starts lies in
r's live set, or one of r's starts lies in S's live set. Each region keeps
its members' starts. A start is any recorded use. A slot only becomes in
use at an access: a pointer it escapes through exists only after an
earlier use, and access after its kill is already assumed not to happen.
So uses are a sound superset of starts. A dead store is a use too, which
stops it clobbering a live neighbor.

## 4. Results

Reproducer: 1,280 → 640 bytes with both flags; neither helps alone. The
remaining 320 is a temporary that `memcpy` copies into the arm's value.
Copy coalescing only matches load/store pairs.

regex-syntax frame bytes: 77,088 → 75,408 (−2.2%), no function worse.

Clean stage 2, every flag plus both:

| function | before | now | LLVM |
|---|---|---|---|
| `lower_item_kind` | 9,040 | 4,272 | 1,264 |
| `lower_to_hir` | 4,352 | 1,440 | 1,776 |
| `lower_expr_mut` | 2,944 | 2,128 | 1,232 |
| `parse_local` | 2,672 | 1,952 | 1,536 |
| `parse_item_impl` | 2,832 | 1,488 | 1,648 |
| `check_expr_call` | 1,168 | 960 | 1,232 |

| test | before | now |
|---|---|---|
| issue-93775 | 16 MiB | 14 |
| peano | 22 | 16 |
| deep-try | 21 | 14 |
| expr-stack-overflow | 42 | 20 |

Tests: `pliron-clif-opt/tests/slotmerge_starts.rs`.
`shared_abort_tail_does_not_interfere` fails with `STARTS=0`.
`after_call_kill_survives_chain_collapse` checks that only the new kill
survives the collapse. Two more tests cover a start inside a live slot
and a dead store; both must stay unmerged. `slotmerge_markflow.rs` pins
both flags off: its resume and forwarding-chain tests assert the old
behavior.

## 5. `PLIRON_SM_STARTONLY=1`

`walk_expr::<TailCallCkVisitor>` (1,936 B; LLVM 80): 487 candidates
packed into 15 regions, but 110 escaped slots (1,312 B) never merged.
They are `format_args!`-style temporaries on cold error paths. Each gets
a start marker, escapes to a call (mostly diverging: `bug!`, `span_bug`)
and has no end marker. In flow mode a slot was tracked only through an end
marker or edge kill, so these fell back to escaped for the whole function.
Now a resolved start marker also makes the slot tracked. `mark_flow` then
keeps it live from its first access to the frame's exits, with no kills
except the frame-leaving ones. LLVM treats such a slot the same way (L1,
L2 and first-use in `StackColoring.cpp`). A slot with no marker at all
stays in scope throughout and still never merges.

Reproducer (four `panic!("…{}…")` arms): 272 → 112 B (LLVM 144).
regex-syntax frame bytes 75,408 → 73,552 (−2.5%), none worse. Clean stage
2: `walk_expr` 1,936 → 640, expr-stack-overflow 20 → 16 MiB, peano 16 →
15. The stage-2 UI suite has 54 failures. The only one at stage 2 alone is
StackColoring-40883, which runs on default flags; **expr-stack-overflow
now passes**. Fuzz 7800–8199 with every flag passes. Tests:
`start_only_slots_on_diverging_paths_merge` (fails with the flag off) and
`start_only_slot_stays_live_to_exit` in `slotmerge_starts.rs`.

## Checks

- Gate passes with every flag plus both.
- Stage-2 UI suite (flags not exported, so the UI programs build with
  defaults): 55 failures. Two fail only at stage 2. `deep-try-chain` now
  passes.
  - `StackColoring-40883` checks the stack of the program it compiles,
    and that build uses default flags.
  - `expr-stack-overflow` (20 MiB) now overflows in
    `walk_expr::<TailCallCkVisitor>`: 1,936 B against LLVM's 80.
- Fuzz 7400–7799 with every flag: only seed 7738, the known O0 ICE.

## Open

- `memcpy` copies into a fresh slot: extend copy coalescing to the call
  (drop it, alias its result to the destination).
- `lower_item_kind` is still 3.4x LLVM.
- `walk_expr::<TailCallCkVisitor>` is still 640 B against 80, and
  `walk_pat` 1,520. Our inliner pastes `visit_expr`, with `check_tail_call`
  inside, at every one of `walk_expr`'s 27 sites. LLVM calls it.
