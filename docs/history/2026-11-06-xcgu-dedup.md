# 2026-11-06 — duplicate-less xcgu (opt-in)

`PLIRON_XCGU_PROMOTE=copyable PLIRON_XCGU_HOME=all` emits one body per
copied item instead of one per CGU: the copy's home definition is
promoted to hidden-external and surviving copies are turned back into
imports. Opt-in; both flags default off.

## Mechanism

- `xcgu::promoted()` gains a `=copyable` mode: for every `Instance` any
  CGU's `copies()` returns, promote its home mono item when it is
  `Internal`, not `inlined`, and held by exactly one partition. The
  home CGU's `predefine_fn` then emits it `External`+`Hidden`
  (`private external`) so other objects' undefined references bind.
- `State::xcgu_extra` records which symbols *this* CGU defines as an
  `extra` copy. `predefine_fn` skips the promotion flip for them — the
  partition's own `Internal` items are `xcgu_copies` too, so without the
  distinction a copy of a promoted item would flip to hidden-external
  as well and two objects would define the same symbol.
- `is_promoted` inside `copies()` consults a new `dep_promoted` (the
  original `cold`/`1`/`0` set), not the union. Promoting every copyable
  item first grew `kernels` text ~10 KB: promoted deps count as
  `externs`/`visible`, so more copies get accepted and more bodies get
  inlined. With `dep_promoted` the accepted copy set is identical to
  baseline — promotion only makes the copied items' *homes* linkable.
- `inline::home_copies` drops the `counts || taken` gate: a callerless
  copy converts too. References `direct_callee`/`address_taken` can't
  see (copy-to-copy calls) bind the same-named home def either way, so
  erasing is never a link risk under `home_linkable`; this is what
  actually reclaims the dead-but-emitted duplicate bodies.
- Both halves are gated to non-wasm (`is_like_wasm`): wasm's copies feed
  the waffle inliner and link through pliron-wasm-ld.

## Numbers (median of 3, release dylib)

| input | rlib `__text` | SIZ ratio | CMP | MEM |
|---|---|---|---|---|
| kernels | 60436 → 56180 | 2.065 → 1.920 | 0.98 → ~same | 1.42 → 1.40 |
| regex-syntax | 426380 → 421441 | 1.897 → 1.875 | 1.16 → 1.13 | 1.78 → 1.81 |
| syn | 1762140 → 1554968 | 1.753 → 1.547 | 1.31 → 1.21 | 1.84 → 1.76 |
| kernels-wasm | unchanged | 1.794 | 0.70 | 1.44 |
| regex-wasm | unchanged | 1.808 | 0.94 | 2.13 |

SIZ geomean ≈ 1.86 → ≈ 1.78 (all native inputs).

Linked binary (all 16 kernels, `-O` exe): 278576 → 278768 (+0.07%,
noise — ld64 already dead-strips the unused copy bodies; the rlib-level
saving is mostly what the SIZ card measures). Output identical on all
16 kernels; `tests/xcguhome` gained a promote+all leg (`work` defined
once, output unchanged, fn-pointer equality preserved — imports share
the home address).

## Runtime (`bench/run.sh`, COUNT=4 BENCHTIME=1s, pliron vs pliron)

geomean **−1.01%** — net faster, not just neutral: varint_postcard
−16.6%, hmget −8.0%, iterw −1.8%; regressions contains_rng +7.5%,
chars_mixed +2.8%. The mechanism trades both ways: the home body was
optimized with its own callees local (sometimes better than the copy's
degraded view — the documented `home_copies` rationale), but a copy
`spec` specialized for this CGU's callers is lost when it converts.
`=all` is the blunt ceiling; a selective mode that keeps provably
specialized copies is the follow-up if this is to default on.

## Dead ends this run

- `=copyable` on *all* copyable items (not just copied ones): +9.7 KB —
  promotion feeding `externs`/`visible` accepted more copies.
- "Foreign-referenced" as the promotion predicate: partition used-lists
  never see the cross-CGU demand, it comes through `copies()` itself.
- `PLIRON_XCGU_HOME=all` alone (no promotion): no-op — copied items'
  homes are `Internal`, not linkable.
- Callerless-copy conversions without promotion: same, nothing to bind.
