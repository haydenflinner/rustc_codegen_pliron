# Wasm parity — open / undecided items

Working ledger for "pliron must not lose to LLVM on any wasmbench
kernel" (geomean first, then per-kernel). Update as items resolve;
`docs/PARITY.md` holds the measured numbers.

## Status snapshot (bench/wasm/run.sh, post `8ed1b5c`)

geomean pliron/llvm = **0.980** (best yet — `walias` mode 15 with
sound pair-cancel). Wins: `chars_alpha` −33%, `iterw` −34%, `parse_only`
−14%, `matmul` −11%, `eqbug` −9%, `contains_rng` −4%. Parity: `sieve`
+0.7%, `nbody` +0.2%.

Landed since: `sfwd` v2 + intra-span DSE (`62dbebb`), `licm` bounded
loop-invariant `get;get;binop` hoisting (`677c37b`), `bandk` in
clifpeep, `wpeep` else-arm retarget, `PLIRON_UNROLL_ITERS` knob.

Post-snapshot landed work (uncommitted at time of writing):
`sfwd` v2 (operand-stack `need`-walk + `local.tee` capture of
*computed* store values into fresh locals — fires on `nbody`'s
`v_i`/`v_j` churn and `strwork`'s frame spills), `bandk` in clifpeep
(`band/bor/bxor` const-chain reassoc on shared inner + `band
(stack_addr, k<align)` → `iconst 0` — killed `strwork`'s ~2.3k-line
dead `if` arm: `strwork3run` 4781→~2410 lines merged into `k_strwork`,
module −5 KiB).

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

2. **`hashmap` +16 / `hmget` +13 / `tokenize` +12 — locals coalescing
   (`walias`-class).** `walias` landed (`4756155`, pair-cancel landed
   `8ed1b5c`): operand-stack-aware linear scan doing `get;drop` elision,
   dead-`set`/`tee` → `drop`/`del`, `get T → get U` copy-forwarding,
   and `set T … get T` pair-cancel (deletes both when T's operand stays
   on the operand stack for the whole span — `pend[t]` tracks the
   operand's depth `dv` and dies when `vstack.len() <= dv`). Default
   `PLIRON_WASM_WALIAS_MODE`=15. Soundness rules: `pend` requires a
   tracked operand (`popped.is_some()` — multi-value call results and
   block params never cancel); pops below the tracked region clear
   `pend`; `Nop(t)` (deleted `get t;set t` — reads+writes t) and
   `canceled`-restored `set`s kill `pend`/`copymap`/`Named` on t;
   `len > dv` strict. `PLIRON_WASM_WALIAS_VERIFY=1` replays orig vs
   edited ins streams comparing popped producer tokens — 0 divergences
   on the ~23000-body wasmbench dump. `k_strwork` matches LLVM x5.
   **strwork +16.8%** (was +21-22).

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
