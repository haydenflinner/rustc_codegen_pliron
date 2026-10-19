# Wasm parity — open / undecided items

Working ledger for "pliron must not lose to LLVM on any wasmbench
kernel" (geomean first, then per-kernel). Update as items resolve;
`docs/PARITY.md` holds the measured numbers.

## Status snapshot (bench/wasm/run.sh, post `9e75ac5` + pending)

geomean pliron/llvm = **0.977**. Wins: `chars_alpha` −31%, `iterw`
−41%, `parse_only` −15%, `matmul` −11%, `eqbug` −9%, `sortw` −3%.
Parity: `sieve` +1.9%, `contains_rng` +1.4%.

Post-snapshot landed work (uncommitted at time of writing):
`sfwd` v2 (operand-stack `need`-walk + `local.tee` capture of
*computed* store values into fresh locals — fires on `nbody`'s
`v_i`/`v_j` churn and `strwork`'s frame spills), `bandk` in clifpeep
(`band/bor/bxor` const-chain reassoc on shared inner + `band
(stack_addr, k<align)` → `iconst 0` — killed `strwork`'s ~2.3k-line
dead `if` arm: `strwork3run` 4781→~2410 lines merged into `k_strwork`,
module −5 KiB).

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

2. **`hashmap` +16 / `hmget` +14 / `tokenize` +16 — locals coalescing
   (`walias`-class).** Residual `local.set`/`local.get`/`local.tee`
   churn from waffle materialization. A straight-line `get A; set D …
   get D → get A` + dead-`set` drop is bounded, but most churn crosses
   control (phi-edge moves) — a real dataflow problem. Agent claims of
   this (`b7cd5a7`, `7ebf0f1`, `8d13f65`) were phantom commits — **no
   `walias`/`pderef`/`sinks`/`localcfg-gto`/`paramcopy`/`ir_callee_size`
   work exists on main.** If implementing: must be a `wpeep` linear
   scan with the same barrier discipline as `sfwd`, or a proper
   reaching-defs pass — decide scope first.

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

6. **`sfwd` coverage gaps — partially resolved.** v2 resolves
   operands via a `need`-counted backward walk (any producer shape,
   not just adjacent `get;get`) and captures non-`get` stored values
   with a `local.tee T` splice into a minted local. Still open:
   narrow stores (`i32.store8/16`, `i64.store8/16/32`) kill but never
   mint; **dead-store elimination** — repeated `store (B,off)` in one
   span (nbody's per-pair `v_i` stores) could collapse to `drop;drop`
   + final store, LLVM keeps `v` in regs until loop end. Deferred:
   needs per-entry "exposed" tracking (unforwarded overlapping reads
   block the kill).

7. **`prel`-anchored `sfwd` interplay.** `sfwd` preserves byte
   positions of rewritten instrs so `prel` pushes stay correct — but
   `sfwd` deletes a `get B` whose *position* a `prel` could anchor.
   Reviewed: safe (the push emits at the `Del`'d ins, then `get V`
   follows — same stack shape). Left as an invariant to keep, not a
   pass to write.

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
