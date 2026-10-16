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

4. Wasm inlining now uses `WASM_LIMIT` = 512 with the `small` no-bonus
   rules — geomean went 1.674 → 1.069, module 178KB → 187KB on the
   bench suite. 512 (not 320) because hashbrown's `hash_one` sits just
   over 320 — inlining it took hashmap +81% → +16%, hmget +163% → +43%.
   768 was neutral-to-worse (tokenize slipped back to parity) so the
   curve has a knee around 512. Question: should -Os/-Oz
   (`OptLevel::Size`) get the same treatment? I left them at 40 —
   they're size-first, wasm was only sharing `small` by accident of
   the `||`. If rustc.wasm size becomes a problem the knob is
   `PLIRON_INLINE`.

5. Resolved: fib's +120% was the `__stack_pointer` dance — the dialect
   `FL::finish` elided it for frame==0 functions but the CLIF-emit
   `finish` (the default wasm path) never had the rewrite. Factored
   into `elide_sp_frame`; fib is now −5% and its standalone wasm beats
   LLVM ~2x.

6. Resolved: `CX::norm` emitted `and 255` on already-0/1 icmp results
   (chars_mixed filter loop paid it per char). `normed()` now proves
   small ranges on icmp/eqz/and/select/load producers.
   chars_mixed +49% → +38%, geomean −1.2pts.

7. Remaining wasm gaps at 512+normed (vs LLVM): hmget +43%,
   chars_mixed +38%, strwork +37%, parse_only +33%, sortw +24%,
   sieve +22%, hashmap +16%, contains_rng +12%, nbody +9%.
   All return-checked clean; all fully inlined now — pure codegen.
   Two structural differences seen in `contains_rng`:
   (a) LLVM strength-reduces `ptr + i*4` to a bumped pointer AND
       rewrites the exit compare to pointer form; our indvars leaves
       the index live so each element pays `shl`+`add` for the
       address. This is probably the single biggest remaining lever —
       sieve/hashmap/parse loops all index like this.
   (b) waffle emits `if { set; br } else { return }` where LLVM uses
       flat `block { br_if }` chains — probably benign, but the
       structured form forces extra locals.
   Also: `PLIRON_WASM_CLIF=1` is now a no-op (CLIF is the default
   path; `=0` is the dialect fallback). run.sh's third column is
   therefore always identical to the second — worth removing the
   duplicate build to halve bench time?
