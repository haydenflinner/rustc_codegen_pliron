# 2026-11-09 — MCP default-on, sibling-flag flips, pulldown-cmark dig

Three threads: post-allocation copy propagation and the EDGEMERGE/
EPIMERGE/TBNZ_IREDUCE defaults landed; the pending-flag A/B set closed
out; pulldown-cmark's remaining gap got a function-level dig.

## PLIRON_MCP (vcode post-allocation copy prop) — default on

A `mov dst, src` whose destination register already carries the
source's value class is dead: LLVM's `MachineCopyPropagation` deletes
them inside `VCode::emit`'s instruction walk (`PLIRON_MCP=0` off,
committed `76177eb` opt-in, `4ee168f` default-on).

- Per-emitted-block `vclass: PReg → class id` map; register-register
  moves propagate `class[src]`, any other operand definition freshens
  `class[dst]`, calls and block boundaries clear the map.
- Participation is restricted to **allocatable Int-class pregs**:
  `fmov d`/`fmov v` share a `Reg` key but differ in width, and
  `sp`/`fp`/`lr`/`xzr` take writes (addressing-mode writeback, SP
  amodes) that never appear as operand definitions.
- `Edit::Move` (spill/reload) items join the same map; a fused
  `ldp`/`stp` edit pair must freshen **both** destinations — skipping
  the second edit leaves the stale class behind and the next needed
  reload gets wrongly deduplicated (fuzz seed 5042's `ldp x11,x8` →
  `stp [x11,#0x30]` crash). A source/destination binding slip in the
  Edit arm poisoned source classes the same way (seed 810).
- Result: 344 dead movs, −236 insns crate-wide, HqZopfli 2166→2163.
  Held-out A/B 0.999 [0.981,1.017] — neutral wall, real code size.
  400-seed fuzz clean, gate 93/93.

## Pending-flag A/B results (eight-program held-out)

| variant | pliron/LV… (ratio) | verdict |
|---|---|---|
| `PLIRON_EDGEMERGE=3`+`PLIRON_EPIMERGE=2` | 1.008 [0.995,1.021] | neutral → **default on** (`3bbb029`) |
| `PLIRON_TBNZ_IREDUCE` | 1.010 [0.994,1.016] | neutral, −172 insns → **default on** |
| `PLIRON_SROA_MAXSLICES=18` | 1.014 [0.998,1.023] | borderline worse (blind-sig −2.2%*) → stays opt-in |
| `PLIRON_LIBCALL_DIRECT` | 1.018 [1.000,1.026] | suspicious; rust-html-rewriter −3% → **held** |

The new defaults cut the brotli object's `__text` 0xE9E30 → 0xE94F0
(−597 insns, ~2.3 KiB). `pliron_mode(var, default)` in vendored
`vcode.rs` now takes the per-var default so it tracks
`pliron-cfg::DEFAULT_VALUES`; `=0` remains the off switch for each.
Gate 93/93 on the flip.

`PLIRON_LIBCALL_DIRECT` marks libcalls `colocated` for direct `bl`
(linker's PLT stub, as clang emits) instead of `adrp+ldr[GOT]+blr` —
worth ~2 insns/site, but the A/B's html-rewriter regression needs an
explanation before flipping.

## Held-out baseline refresh (2026-11-09, 11 rounds + aa)

geomean pliron/LLVM **1.075 [1.068,1.088]** (was 1.081); aa noise
floor 1.002. rust-protobuf moved to 0.986 — first program faster than
LLVM. pulldown-cmark 1.100, rust-compression 1.144, rust-html-rewriter
1.143 remain the top three.

## pulldown-cmark: where the remaining 1.100 lives

Fresh census (`bench/heldout/census.py`, 20 runs): pliron 399 vs LLVM
365 samples — a 34-sample gap spread across the FirstPass scan
machinery, not one hot function. Symbol attribution is distorted by
inlining differences both ways:

- `Parser::next` is 578 insns vs LLVM's 138 — our IR inliner pulled
  `item_to_tag` (one call site, ~479-insn LLVM body) in while LLVM
  kept the call. But `PLIRON_INLINE_ONCE=0` makes pulldown *worse*
  (1.154 vs 1.092): the once-inlining is a net win; the size is not
  the slowdown.
- `scalar_iterate_special_bytes` (the parse_line closure): 46 samples
  vs 0 — LLVM inlined it everywhere into `parse_line`+`Tree::append`
  callers; we kept a 3594-insn standalone (3 call sites, over every
  threshold). Aggregation: LLVM 33 (parse_line) + 23 (Tree::append)
  ≈ 56 samples vs our 46 — roughly equal total work; the "gap" is
  attribution.

Genuine mechanism findings in `scan_code_fence` (16 vs 3 samples):

1. **Search-loop vectorisation missing.** `iter().any(|&b| b == c)`
   scans (the "no backtick in suffix" check) emit scalar byte loops
   for us; LLVM emits `cmeq.16b` + `umaxp`/`shrn` + `rbit`/`clz` NEON —
   the memchr idiom with early-exit lane position. `loopvec` only
   handles elementwise and reduction loops, not early-exit searches.
   This is the largest single code-shape gap in the function (and
   recurs in the `fminnm`-style LLVM output noted in the brotli dig).

2. **Peel keeps per-copy end tests.** `PLIRON_PEEL` peeled the `.any`
   loop ×8–9; every copy re-tests `ptr+k == end` + carries a per-copy
   exit-edge chain eval (`iadd inv,1`, re-materialised by `paramsink`,
   by design). ~7 insns/element vs LLVM's ~5 in its scalar
   `scan_ch_repeat` loop, which clamps the pointer select instead of
   dual index+pointer bookkeeping.

3. `_platform_memmove` +12 samples pliron-side — we feed it more or
   bigger copies (Event/tree moves), worth a separate look.

Follow-up candidates, in order of leverage: early-exit search
vectorisation in `loopvec` (`.any`/`position`/`memchr` shapes →
`cmeq`+reduce+`clz`, recurs across pulldown/regex/json); a
`ptr+k != end` fact fold inside peeled/punrolled runs where the
guard already proves `end−ptr ≥ K`; the HqZopfli ~200-mov residue
(regalloc-level, not emit-level).
