# Inlining into recursion, memcpy coalescing, frame breakdown

Follows `2026-10-26-slot-interference.md`. Every new flag is off by default.

## 1. Why `walk_expr` was 24x LLVM

With `STARTONLY`, stage-2 `walk_expr::<TailCallCkVisitor>` was 640 B
(it had been 1,936) against LLVM's 80. The two builds differ in code, not
slots:

| | LLVM | pliron |
|---|---|---|
| code | 2,384 B | 67,840 B |
| `visit_expr` | 15 calls | inlined at all 27 sites |
| also pasted in | — | `fn_sig`, `report_nonfn_callee`, `bug_impl`, … (27–54 copies each) |

`visit_expr` itself is three lines: check for `become`, call
`check_tail_call`, call `walk_expr`. Inlining it is fine. What went wrong
is `check_tail_call` (big, one caller) riding along into every copy.
The inliner's single-site bonus (`PLIRON_INLINE_ONCE`, 10x the limit
for a local with one calling function) is computed once per run, from
calling *functions* rather than sites. Once `visit_expr` had been cloned
27 times into `walk_expr`, each clone's `check_tail_call` call still
counted as the only one. Every level of the expression recursion then
carried the frame of all of them.

## 2. Flags

All live in `inline.rs`, in the per-site decision just before
accept/reject:

- `PLIRON_INLINE_ONCE1=1`: the single-site bonus applies at most once per
  callee per run.
- `PLIRON_INLINE_RECSTACK=1`: no single-site bonus into a caller on a
  recursion cycle (multi-function SCC, or self-recursive) the callee
  isn't on. The out-of-line copy still dies, but every recursion level
  pays the callee's frame.
- `PLIRON_INLINE_SCCLIMIT=1`: a callee on the caller's own cycle gets the
  base limit and no bonuses.
- `PLIRON_INLINE_DUPCOST=1`: the k-th copy of one callee in one caller
  costs k times its size (callees under `limit/8` exempt).
- `PLIRON_INLINE_COLD=1`: a site in a block that can't reach a `return`
  or a loop gets `limit/5` (LLVM's 45 of 225 cold-callsite ratio).

Stage 2 with every slot flag, rebuilding `rustc_mir_build`,
`rustc_ast_lowering`, `rustc_hir_typeck` and `rustc_parse` per set.
Frames are in bytes, stack needs in MiB, code is `librustc_driver`
`__text` against the baseline's 121.2 MB:

| set | walk_expr | walk_pat | lower_expr_mut | parse_path_segment | expr-stack | peano | deep-try | code |
|---|---|---|---|---|---|---|---|---|
| none | 640 | 1,520 | 2,048 | 1,760 | 16 | 15 | 14 | 0 |
| ONCE1 | 112 | 1,520 | 2,048 | 1,728 | 16 | 15 | 14 | −0.3% |
| RECSTACK | 112 | 144 | 1,712 | 1,296 | 17 | 14 | 13 | −0.3% |
| SCCLIMIT | 640 | 1,552 | 1,696 | 1,072 | 13 | 14 | 12 | +3.2% |
| DUPCOST | 2,496 | 1,520 | 2,256 | 1,744 | 26 | 15 | 14 | −0.7% |
| COLD | 640 | 1,552 | 2,496 | 1,952 | 18 | 14 | 13 | +0.4% |
| all five | 112 | 32 | 912 | 1,152 | 12 | 13 | 11 | −0.1% |
| **ONCE1+RECSTACK+SCCLIMIT** | **112** | **32** | **912** | **912** | **12** | **12** | **11** | +0.1% |
| … + COLD | 112 | 32 | 976 | 1,168 | 12 | 13 | 11 | +0.4% |
| LLVM | 80 | 80 | 1,232 | 1,488 | | | | |

The three-flag set is best. On its own `DUPCOST` makes `walk_expr` worse:
fewer `visit_expr` copies leave budget that pulls fatter bodies in. `COLD`
grows code: once a callee's cold paths stay out of line, the callee is
small enough to inline into more callers (a `Display::fmt` wrapper went
16 → 640 B). Both stay available but aren't in the recommended set.
`lower_item_kind` doesn't move (4,208; LLVM 1,264) and needs its own look.

Runtime: the native bench (`bench/run.sh`, 19 kernels) has a pliron/LLVM
geomean of 0.913 without the set and 0.917 with it (+0.4%, noise).
`strwork` +3.8% in that run was noise: four reruns gave 0.992/1.015
without and 0.989/0.999 with.

Test: `tests/inlinerec` (test.sh `inlinerec`). A recursive visitor
calls a big single-caller helper. Under `RECSTACK` the inline log must
show no `check` accept, and the output must match stock rustc's. With
the flag off, `check` is inlined.

## 3. `PLIRON_SM_COALCALL=1`

Copy coalescing only matched copies made of load/store pairs. A `Drop`
value is built in a temporary and moved into its binding with
`memcpy(D, S, size)`. A whole-slot copy whose source dies and whose
destination is born at the call now coalesces as the pair (call, call).
The call is removed and its result, which is D, is aliased to its first
argument. Under `STARTS` the coalescing test itself had to become
start-based. Region members' live sets may overlap there, so each region
keeps a member list, and the other members' live sets and starts are
rebuilt without the source.

Reproducer (three arms, each a 320-byte `Drop` value): 640 → 320 B
(LLVM 352). regex-syntax frame bytes 73,552 → 69,424 (−5.6%), one function
worse. Tests: `slotmerge_coalcall.rs`. `whole_slot_memcpy_coalesces`
fails with the flag off; in `source_read_after_copy_stays` the source is
read after the copy, so the call stays.

## 4. `PLIRON_FRAME_INFO=<symbol substring>`

This prints each compiled function's frame as `slots spills clobber outgoing
fixed`. It comes from a vendored-Cranelift hook, `pliron_last_frame()`
(`PLIRON_PATCH.md`). On regex-syntax with every slot flag:

| | bytes |
|---|---|
| stack slots | 42,672 |
| spills | 10,208 |
| callee-saved saves | 12,896 |
| outgoing args | 320 |

On the 400 functions the LLVM and pliron objects share, ours have 20.7%
more frame bytes. We emit 1,234 callee-saved pair stores against 656 and
3,267 stores to the stack against 834. With slots under control, register
pressure across calls (callee-saves and spills) is the next gap.

## 5. Checks

Clean stage 2, every slot flag plus ONCE1, RECSTACK, SCCLIMIT and
COALCALL. Frames (LLVM in parentheses):

- under LLVM: `walk_pat` 32 (80), `parse_path_segment` 912 (1,488),
  `lower_expr_mut` 912 (1,232), `check_argument_types` 880 (1,072),
  `parse_item_impl` 1,120 (1,648), `lower_to_hir` 1,360 (1,776);
- above LLVM: `walk_expr` 112 (80), `parse_local` 1,872 (1,536),
  `lower_qpath` 944 (448), `lower_item_kind` 4,208 (1,264).

Stack needs: issue-93775 13 MiB, expr-stack-overflow 12, peano 12,
deep-try 11. The stage-2 UI suite has 54 failures. The only stage-2-only
one is StackColoring-40883, which builds its program with default flags.
Gate passes with every flag and with defaults (71 jobs). Fuzz 8200–8599
with every flag passes.

## 6. `PLIRON_SM_ESCFLOW=1`

`PLIRON_FRAME_INFO` on stage-2 `lower_item_kind` showed 3,280 B of slots,
784 of spills and 80 of callee saves. Its 12 merged regions were only
1,004 B. The other 2,240 B was 38 escaped slots, among them six 328-byte
slots that had lost both start and end markers. `PLIRON_LT_PROBE` put the
loss in jumpthread, which dropped 895 of the function's 2,324 markers and
2,064 of its 3,602 edge kills. Later passes lost a few hundred more.

Rather than teach every pass to carry markers, `ESCFLOW` treats every
escaped slot, markers or not, as live from its first access to the
frame's exits (`mark_flow` with only the frame-leaving kills). Under
`STARTS`, two such slots share storage only if no path touches both, so
no run can see both addresses. A slot passed to a tail call or stack
switch outlives the frame, so it is still escaped (`hard_esc`).

Clean stage 2, every flag plus this: `lower_item_kind` 4,208 → 1,968 B
(LLVM 1,264), `lower_expr_mut` 912 → 816, expr-stack-overflow 12 → 11 MiB.
regex-syntax −0.3% (few such slots there). Fuzz 8600–8999 with every flag
found seed 8631, a slotprom overflow that also fails with default flags
(fixed in `50dce72`). Tests (`slotmerge_starts.rs`):
`markerless_escaped_slots_on_disjoint_paths_merge` (fails with the flag
off), `markerless_escaped_slot_stays_live`, and
`tail_called_slot_never_merges` (fails without `hard_esc`). The stage-2
UI suite has 54 failures. The only stage-2-only one is
StackColoring-40883 (default flags).

## 7. `PLIRON_REMAT_SADDR=1`

In regex-syntax's worst spiller (`NestLimiter::check`: 632 B of spills,
1,552 disassembly lines against LLVM's 416) the same few spill slots are
reloaded before nearly every call. Most are long-lived values: `self`,
and the function's own indirect-result pointer in `x8`. All ten
callee-saved registers are already in use, so that part comes from the
larger inlined body, not from the allocator. The vendored regalloc2
spill-weight patch already favors such values. Frame addresses were a
smaller cheap part: GVN merges a function's `stack_addr`s into one value
that then lives across calls. LLVM's register allocator rematerializes
frame indices instead. A gated e-graph remat rule (`remat.isle`, extern
`pliron_remat_saddr`) now recomputes `stack_addr` in each block that
uses it.

regex-syntax: stack loads 7,322 → 6,942 (−5%), spill bytes −1.5%, text
−0.3%. Native bench geomean pliron/LLVM 0.917 → 0.910; no kernel more
than 1% worse (chars_alpha −7.2%, tokenize −3.4%, strwork −3.3%, all near
noise). Gate passes with every flag and with defaults; fuzz 9000–9299
with every flag passes.

## 8. Crates tiers 1–3 with every flag

`harness/crates.py` runs each crate's own tests built by stock rustc and by
pliron with every flag on: all slot flags, ONCE1/RECSTACK/SCCLIMIT,
COALCALL, ESCFLOW and REMAT_SADDR.

- Tier 1 (20 crates, about 4,500 tests: ryu, memchr, regex, hashbrown,
  bytes, num-bigint, blake3, png, …): no pliron-only failures.
- Tiers 2–3 (17 crates: crossbeam, rayon, tokio 2,310 tests, serde_json,
  uiua, rapier, rune, makepad, …): 15 clean. prql and nushell failed to
  build on the pliron side, in a second each. `XFACTS_ALL` bug, fixed in
  `be81acb`: both workspaces set `lto`, cargo then builds their
  dependencies with `-Clinker-plugin-lto`, and rustc skips thin-local
  LTO. Modules held for the two-phase barrier reached `codegen`
  unfinished and every object came out empty. `two_phase` now requires
  linker-plugin-lto off, and `codegen` finishes any module still held.
  Test: `tests/xfactsplugin`.
