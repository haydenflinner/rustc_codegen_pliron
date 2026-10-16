# Questions for the user (logged by Devin, unattended session)

Started 04:36. User asked: broad benchmark vs LLVM, codegen work from
findings, then wasm benchmarking if time. This file collects questions
that came up while working unsupervised.

## Open questions

1. `remat.isle` now never marks `iconst` (see PLIRON_PATCH 38). This
   assumes regalloc2 can recover under true pressure by spilling, since
   it has no remat of its own. If a workload shows a regression from
   a pinned constant vreg, the fix is a pressure-aware remat, not a
   re-enable. Worth confirming no such regression in the suite.

2. `edgesink` phase 2 pushes a shared pure def into strictly-dominated
   same-loop-depth use blocks (cap 8). It never asks whether the use
   blocks are actually cold — only that cloning is never a *dynamic*
   regression. Should it be score-gated on branch probability if/when
   we have PGO feedback wired into CLIF?

3. parse_only residual vs LLVM is now ~0% (was +9.2%): fixed via
   edgesink + iconst-remat removal. The remaining ~1% is duplicated
   `sub #0x30` (remat'd `iadd` per digit block) and a per-digit
   `sub x_, x10, #5` address recompute where LLVM keeps one advancing
   pointer + immediate offsets. Chasing further is diminishing — flag
   if you want the pointer-bump threading investigated.

4. Wasm inlining now uses `WASM_LIMIT` (= `DEFAULT_LIMIT`, 320) with the
   `small` no-bonus rules — wasm geomean went 1.674 → 1.165, module
   177.8KB → 189KB on the bench suite. This unblocked a real
   `loadfwd` miscompile (fixed, see DEBUG-LOG): the `ofrozen` stop-set
   named call args under a different root than the `noalias`-keyed
   `ncc` classification. Question: should -Os/-Oz (`OptLevel::Size`)
   get the same 320 cap? I left them at 40 — they're size-first, wasm
   was only sharing `small` by accident of the `||`.
   If rustc.wasm size becomes a problem the knob is `PLIRON_INLINE`.

5. Remaining wasm gaps at the new limit (vs LLVM): fib +120%,
   hmget +171%, hashmap +109%, parse_only +95%, tokenize +72%,
   strwork +56%, chars_mixed +49%. All return-checked clean. The call
   boundary is mostly closed; what remains looks like scalar codegen
   (fib = tight FP/arith loop → likely isel/regalloc), not inlining.
   Next lever candidates: `once`/`hint` bonuses under wasm `small`
   (single-caller local fns are size-neutral), or investigate fib's
   wasm codegen directly.
