# 2026-11-11: nocap extents, sret boundedness, field eviction — kept machinery, opt-in defaults

## What landed (bc8e581)

`CallInfo::nocap` operands now carry a byte extent
(`FxHashMap<(Inst,u32),u64>`, `u64::MAX` = whole slot) sourced from
`arg_extent`, propagated through CLIF lowering (`csrc` re-indexed),
diffcheck's `fn{a}:{k}:{ext}` facts, slotmerge's per-callee widening
(max extent), and the step-cache keys. `slotprom` records bounded
calls as `(inst, off, ext)` and syncs/reloads only the promoted
fields the operand can reach (`covers`), with `webroot::ANY`
normalised to whole-slot and per-call field dedup.

The sret out-pointer's boundedness travels on a new
`CallInfo::sret_nocap` flag — deliberately NOT in `nocap`: pliron-IR
SROA reads a `nocap` mark as "a fresh copy is unobservable" and would
hand the callee a copy of the out-buffer (the `carried` repro read 0;
fixed and re-verified). ABI rewrites re-index it (`dead_args` clears
when operand 0 dies, sret lowering clears it outright).

Also landed: when promoted fields exceed `MAX_FIELDS` (48), evict the
least-accessed slots to fit instead of the old all-or-nothing bail.

## What the measurements said

Motivator was `lol_html`'s `selectors_vm::exec_without_attrs`
(1054 vs 466 insns vs LLVM, 372 vs 100 stack refs). With the
mechanisms on: 1045 insns — sync traffic and regalloc, not call
sync, dominate. Runtime neutral on regex and html-rewriter
(~4.2M / ~7.7M ns both ways, interleaved).

regex-syntax `.text` attribution (per-knob bisect on the committed
dylib):

| config | .text |
|---|---|
| evict off + sret off (= old HEAD) | 343428 |
| evict off, sret on | 345188 (+1760) |
| evict on, sret off | 349316 (+5888) |
| all on | 354052 (+10624, +3.1%) |

Extent-limiting itself is ~neutral (-96B). The `>48 fields` bound was
a de-facto profitability guard: functions over it have wide CFGs where
per-edge param moves + syncs cost more than the memops removed.
Eviction silently converted "unprofitable" into "partially promote" —
+5.9KB for nothing measured.

## Follow-up defaults change (this commit)

- `PLIRON_SLOTPROM_EVICT` is now opt-in (`=1`); default restores the
  all-or-nothing bail.
- `PLIRON_SRETNC` is now opt-in (`=1`); sret boundedness is plumbed
  end-to-end and verified, dormant by default.
- `PLIRON_SLOTPROM_EXT` (new) is default-on with a `=0` kill switch —
  extent-limited syncs are strictly less work than whole-slot syncs.

Default codegen on regex-syntax is back to +96B over pre-bc8e581
(343524 vs 343428); `PLIRON_SRETNC=1 PLIRON_SLOTPROM_EVICT=1`
reproduces 354052 exactly. Corpus 4198 ok / 0 mismatches, quick 21
steps, gate 98 jobs.

## Structural residual

`exec_without_attrs`'s 2x insn / 3.7x stack-ref gap is regalloc-class
traffic: ~65% spill/reload + moves around struct copies into
out-buffers and 58 vs 34 calls (LLVM inlines `try_allocate_in`,
`LimitedVec::push`, more drop glue — `has_body=false` xcgu copies cap
at `PLIRON_XCGU_SIZE=50`, and 400 made it *worse*: post-inline cleanup
is the missing half). Not a slot-sync problem; the knobs stay for the
next time a bounded-call-heavy function actually profits.
