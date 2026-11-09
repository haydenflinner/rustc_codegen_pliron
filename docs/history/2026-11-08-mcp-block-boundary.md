# 2026-11-08: MCP block-boundary carry + the EDGEMERGE canon-pred miscompile

Post-alloc move residue on html-rewriter was ~23K reg-reg movs; buckets:
~121 `mov a,a`, ~377 near-swap pairs, ~362 forwardable chains. The
`PLIRON_MCP` value-class dedup only cleared ~204 because `vclass`
cleared at every block boundary.

## MCPBB

`PLIRON_MCPBB` carries the preg->value-class map across block
boundaries. Each emitted block's exit map is recorded
(`out_class[order_pos]`); a block's entry map seeds from the
intersection of its already-emitted CFG preds' exit states. A pred not
yet emitted (backedge, out-of-order layout) or emitted-but-merged
(`None` out-state) contributes nothing and empties the seed —
conservative. try_call handler edges are covered because `block_preds`
is derived from `block_succs`, which includes exception targets.

Also deleted unconditionally-dead moves on both emit paths:
regalloc2's same-preg `Edit::Move` metadata moves (they emitted real
`mov x,x` bytes) and inst-level `Mov{Size64}` self-moves. (`is_move`
only matches Size64, so `mov w,w` still freshens — 32-bit self-moves
write the upper bits and are not no-ops in the class sense.)

## The miscompile

`PLIRON_MCPBB=1` (first version) SIGSEGV'd building cssparser: syn's
`FixupContext::precedence` passed x0=6 — the jump-table index — to a
callee that dereferenced it.

Final CLIF for `precedence` was correct; regalloc2's `ra.edits`
contained the needed `p19i -> p2i` restore moves; they never reached
the object. `PLIRON_REMAT=0` identical; `PLIRON_EDGEMERGE=0` fixed it.

Root cause: `PLIRON_EDGEMERGE` (and `PLIRON_BJALIAS`) alias a duplicate
edge block's label to a canonical block, so the dup's CFG preds'
control-flow edges *enter the canon*. The MCPBB seed only intersected
the canon's own `block_preds` — a subset of its real entry edges. In
`precedence` the canon's own pred exited with `x2 == x19` (dedup looks
legal) while the merged jt edge block's pred — the `br x3` dispatch —
arrived with `x2` = the table index. The seeded dedup deleted the
canonical `mov x2,x19` once, dropping the restore on every aliased edge
at once.

Fix: `canon_dups` records, per block, the merged blocks whose labels
alias to it; the seed intersects their preds' exit states too. Same fix
covers BJALIAS jump-through targets. A focused reproducer
(`tests/edgemerge::restore` — dense `br_table` + bounds-check edge +
shared continuation) segfaults with the seed restricted to own preds
and is green with the fix, across `PLIRON_EDGEMERGE=0..3`.

## Numbers

syn rlib (release): 56636 vs 57107 emitted `mov`s (-471, all deleted
insns are movs; -470 total insns). cssparser builds and passes with the
carry on.
