# Wasm parity — open / undecided items

Working ledger for "pliron must not lose to LLVM on any wasmbench
kernel" (geomean first, then per-kernel). Update as items resolve;
`docs/PARITY.md` holds the measured numbers.

## Status snapshot (bench/wasm/run.sh, post `c8a744d`)

geomean pliron/llvm = **0.967–0.971** (best band yet — `walias` +
`sfwd` cross-block merges). Wins: `chars_alpha` −35%, `iterw` −41%,
`parse_only` −14%, `matmul` −12%, `eqbug` −9%, `contains_rng` −3%.
Parity: `sieve` +0.3%, `sortw` +0.6%.

Landed this round (all pushed):
- `a0beea7` — `walias` cross-block dataflow: `copymap`/`last_write`
  merge at `end`/`else`/`br`/`br_table` with reachability tracking,
  `ins_span` same-straight-line eager-dead-write guard, `usize::MAX`
  merge sentinels for divergent writes, `loop_writes` backedge kills,
  `Nop`/`canceled`-restore soundness fixes. PHI-token verifier
  (`PLIRON_WASM_WALIAS_VERIFY=1`): 0 divergences on 22630 dumped bodies.
- `ec69897` — `sfwd` `fmap` cross-block merge (same machinery):
  slot facts survive `end` iff every path agrees on (fwd,sop,si,ai),
  `exposed` ORs, loop-entry kills body-written base/fwd locals.
  **`hashmap::run` halved**: 2711→1277 ins, mem 203→81, locs 1745→873.
  Also `PLIRON_INLINE_MAXDUP` knob (default off — measured: kills hot
  paths, diagnostic only) + tee-insert sort fix (`fb74c00`).
- `cbd1b20` — `cvt` float↔int reinterprets + `cond` F32/F64 `v!=0.0`:
  unstubs `pow`, `asin`×2, `exp2` (were `unreachable` bodies).
- `c8a744d` — `bittab` single-run accepts emit `(x-a)<=u span`
  instead of guard+`ushr`-bitmap (sparse sets keep the bitmap —
  multi-run union measured worse, `tokenize` +37%→+15% on revert).

Knobs swept and rejected this round: `PLIRON_INLINE` 200/350/768
(200/350 catastrophic geomean 1.07+, 768 worse 0.977 — default 512 is
the sweet spot), `PLIRON_INLINE_MAXDUP=2` (chars_ascii +534%),
`PLIRON_INLINE_LOOP`/`_HOT`/`_BU`/`_MODEL` variants.

Module size: 204KB vs LLVM 97KB — the residual is codegen density
(~3-5x local ops in the big bodies) not inline duplication:
`quicksort` 300KB, `format_exact` 260KB, `rem_pio2` 139KB WAT.

## Lessons / pitfalls discovered

- **V8 wasm-inlining is a hard module-size budget**: `fib` regressed
  +79% on identical function bytes because the module grew ~3 KiB past
  V8's per-module inlining cap (`budget=min(172, 700)` vs `min(177,
  831)` — `fib3fib`'s self-inline needed ≤172). Deferred tee-minting
  brought the module back under and restored the win. **Any future
  size-growing transform must be checked against this cliff** — diff
  per-function sizes (`wasm-tools print | func line counts`) between
  builds and watch `fib`/`chars_ascii` specifically.
- **`prel` KOS pushes are invisible to `ins`**: any operand-stack walk
  (`operands`, `esf`, `eff`) sees an unbalanced stream on KOS'd bodies.
  Fix was materializing `prel` into real `Get` instrs before the scan.
- **`esf` table had `f32/f64` unary ops (0x8e–0x91, 0x9c–0x9f) as
  (2,1)** — `f64.sqrt`/`floor`/`trunc`/`nearest` underflowed every
  walk. Table has 3 copies (lines ~802, ~1678, ~2258) — all fixed.
- `PLIRON_WASM_SFWD_DBG=1` prints per-event `sfwd` tracing; removed
  the temp `sfwd_advance_body_debug` test (read a temp-file body).

## Undecided / deferred calls

1. **`strwork` +21% — IR-inline policy (dead arm fixed).** `strwork::run` is ~4700 wat
   lines; LLVM's is 494 — we inline `push_str`/`Vec::extend`/fmt
   wrappers LLVM leaves as calls (`format_inner`, `do_reserve_and_handle`,
   `dealloc`). The lever is `pliron-ir-opt/src/inline.rs` byte-budget /
   callsite policy — but shrinking it fights `hashmap`/`hmget`/`tokenize`,
   which *benefited* from the same aggressive inlining
   (`hash_one`/`next_match` loop-body inline via `PLIRON_WASM_HOT=400`
   at wasm level, and IR-level inlining feeding it). **Undecided:**
   per-callsite or per-callee distinction (hot-loop call vs flat
   convenience inline) rather than a global budget change. Deferred to
   whoever owns `pliron-ir-opt` next.

2. **`hashmap` +16 / `hmget` +12 / `tokenize` +15 — density residual
   after cross-block merges.** `walias` cross-block dataflow landed
   (`a0beea7`) + `sfwd` `fmap` merge (`ec69897`): `hashmap::run`
   halved (2711→1277 ins) — SipHash state now stays in locals across
   the probe-loop backedge. `hmget` ~895 vs LLVM ~263 ins remains —
   the probe loop's tag-compare/probe-step codegen (hashbrown
   `eqz`+`cmov` cluster) is ~3x denser in LLVM; `tokenize`'s
   `next_match` digit/ident-scan is i64-bitmap+select chains LLVM
   emits as `x-9>23`-style range checks. **Undecided:** the residual
   is operand-stack local-threading (LLVM's wasm backend numbers
   locals to minimize get/set; ours materializes per-SSA-value) —
   `walias`/`sfwd` chip locally but can't collapse loop-carried
   phi-moves. Would need a wasm-level copy-coalescing/SSA-minimizing
   pass, or better local-numbering at emit.

3. **`br_if` fold — arm1 pure-op relaxation.** Else-retarget landed
   (`9e75ac5`), but `if c {arith/stores;br} else {R}` still can't fold
   — arm1 is restricted to `Get`/`Set`/`Tee`/`Nop`+`br`. Hoisting pure
   value ops (const/add/and/xor/cmp — no loads, no traps, no calls)
   out of arm1 would fold more sites, but each hoisted op runs on the
   false path too — needs a strict non-trapping whitelist and dead-on-
   else-path results (`bal==0` already enforces the stack part).
   **Undecided:** worth it? The remaining arm1s are mostly *stores*
   (result writes) which can never hoist — expected gain may be small.

4. **Else-arm `br` at `d == rel`** (targets the removed `if`'s own
   end). Currently rejects. For `br` *as the last else instruction*
   it's semantically "fall through" and could be dropped; mid-arm it
   means "skip rest of else" — needs a synthetic block or rejection.
   Current: always reject. Cheap refinement available if a hot site
   shows it.

5. **`br_table` inside else arm.** Decode doesn't emit `WPeek` for
   `0x0e` bodies today (bail), so the fold never sees them. If decode
   ever grows `br_table` support, retarget each operand by the same
   `rel` rule (operand == rel → reject).

6. **`sfwd` coverage gaps — narrowed.** DSE landed (`62dbebb`):
   intra-span same-slot overwrite → `drop`, with per-entry `exposed`
   tracking blocking kills across unforwarded reads; tee-capture is
   deferred to first forwarded use so dead stores mint nothing.
   Remaining: narrow stores (`i32.store8/16`, `i64.store8/16/32`)
   kill but never mint (no forwarding local exists for a partial
   write — could mint the *loaded-back* bytes, complex); 80-step
   operand-walk cap may need `PLIRON_WASM_SFWD_STEPS` if a real body
   hits it.

7. **`prel`-anchored `sfwd` interplay — resolved by materialization.**
   `prel` pushes are spliced into `ins` as real `Get` instrs before
   the `sfwd` scan (and `prel` cleared so emit doesn't double them) —
   walks see a balanced stream and `Del`/`Drop` edits compose
   normally. Invariant to keep: any new operand-stack walk must run
   after materialization.

8. **`strwork`/`hashmap`/`tokenize` shared shape — `pliron-ir-opt`
   nonnull-through-slot.** Iter-pointer spill→reload loses
   `!nonnull`, blocking `eqz`-fold chains downstream of reloads.
   Needs nonnull facts carried through our own stack-slot demotion —
   pliron-ir-opt scope, already noted in PARITY.md §residuals.

## Measurement notes

- `sortw`, `fib`, `eqbug` are sub-10 ms and swing ±5% between runs —
  judge with repeated filtered runs, not one sample.
- `bench/wasm/run.sh` rebuilds `target/wasm-bench` fresh each run;
  `PLIRON_*` flips do apply (no cargo-fingerprint staleness).
- `PLIRON_WASM_SFWD=0`, `PLIRON_WASM_BRF=0`, `PLIRON_WASM_HOT`,
  `PLIRON_WASM_AUDIT=panic` are the wpeep knobs.
