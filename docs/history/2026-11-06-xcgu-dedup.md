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

## 2026-11-06 later: `HOME=mono` — `inlined` mono items

The remaining `syn` duplication was not `extra` copies: rustc gives
every partition that reaches an `inlined` mono item its own
`Internal` "CGU-private copy" (`rustc_monomorphize::partitioning`),
each emitting a `non-external` body ld64 can't merge. On `syn` that
was ~205 symbols / ~430 KB — `drop_glue<BareFnArg>` defined `t` in
13 of 16 objects with ~97 live call sites each. LLVM keeps 2 defs
because it *inlines* the glue chain; pliron's inliner rejects it
(`drop_glue<BareFnArg>`: 414 ops, cost 1314 vs limit 320) — forcing
`always_inline` on `DropGlue` shims exploded syn 2.2 MB → 14 MB
(recursive glue trees duplicated whole, no per-site collapse).

`PLIRON_XCGU_HOME=mono` instead applies the design-(b) bind to
`inlined` items: `xcgu::inlined_home` elects one owner partition per
shared item (a real `inlined=false` root holder wins, else the first
inlined holder), `promoted()` flips the owner's `Internal` def to
hidden-external, and non-owner copies — mono items in `items()` and
`extra` copies alike — go into `xcgu_home` for `home_copies` to
convert post-inline. `xcgu_extra` suppresses the promoted flip on
non-owner decls, the same mechanism as for `extra` copies.

Numbers (`PLIRON_XCGU_PROMOTE=copyable PLIRON_XCGU_HOME=mono`):
syn rlib text 2208965 → 1515853 (−31%, **zero** multi-defined
symbols, was 205+), kernels −1.4%, regex −4.2%; card deltas syn SIZ
1.753→1.202, CMP 1.31→1.13, MEM 1.84→1.69 (converted bodies skip
lowering), wasm untouched. Bench runtime −0.04% geomean (neutral).
`tests/xcguhome` gained a `mono` leg: `work` defined once, output
identical.

Dead ends inside `mono`:

- Electing *every* inlined item: +165 KB on syn — a promoted `Hidden`
  def can never `dead_fns`-strip, so items inlined away in all 16
  CGUs emitted for the first time (kernels: 97 → 265 defs).
- `data.size_estimate` floor at `size_limit` (50): killed the win —
  drop glue scores ~dozens of MIR stmts but expands 10x at codegen.
  Resolved as `PLIRON_XCGU_MONOMIN` (default 200) for plain `Item`s
  plus an unconditional elect for `ShimKind::DropGlue`.
- `n>=2 holders` alone missed `inlined`-in-one-partition +
  `extra`-copied-elsewhere items (`PrivateIter::next` x6); the
  `copied` set covers them.

## Default-on flip (same day, later)

`harness/crates.py --tier 2` under `PLIRON_XCGU_PROMOTE=copyable
PLIRON_XCGU_HOME=mono`: 31 crates (ryu…toml + crossbeam, rayon, tokio,
serde_json), **0 pliron-only failures** — so `DEFAULT_VALUES` now ships
`PLIRON_XCGU_HOME=mono` + `PLIRON_XCGU_PROMOTE=copyable`. `=1` keeps the
old degraded-only mode, `=0` disables home conversion entirely. Gate +
quick green on the flipped build.
