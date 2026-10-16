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

## Update — wide-guard merge + swap_unchecked inlining (second pass)

8. `punroll::cap_unroll` had `let t = trip?` — any loop without a
   profile count bailed, so the whole run_loop group-guard unroll was
   dead on non-PGO builds (all the wasm benches). Now `None` keeps the
   requested width. Also swapped the order so run_loop/run_region try
   before run_peel (peel was unconditionally consuming every eligible
   loop's `tried` slot first).

9. The remaining bail — "exit cone re-enters loop" — fired on every
   nested inner loop: the inner exit's dominance cone contains the
   outer-loop re-entry wedge back into the inner header (matmul
   block17→block18). The check was unconditional; it's now gated on
   `needs_cone`/`elvals` — the edge is only unsafe if the cone must be
   cloned because loop values escape into it. Same relaxation applied
   to `analyze_exit_cone` (side exits). New filetest
   `punroll_reentry.clif` locks the shape in. mm (i*k*j matmul, the
   nested case) now unrolls and verifies correct for N=1..64 incl.
   odd sizes; microbench ~0.81x LLVM time.

10. `swap_unchecked` stayed out of line in sortw because rustc
    assigned its mono-item body to a different CGU (heapsort's cgu.0);
    in quicksort's cgu.1 it's `linkage=Import has_body=false` → the IR
    inliner rejects it as ineligible. LLVM gets cross-CGU bodies via
    internalized copies — our equivalent exists: `xcgu::copies`, but it
    was gated `!is_like_wasm` (rustc.wasm 875MB vs V8 1GB cap). I
    removed that gate. Result: swap_unchecked fully inlined (0 calls
    remain in the module), sortw ~-4 to -6% vs LLVM (was +35%).
    WARNING: on the bench this ballooned the module 188KB → 811KB —
    quicksort alone emitted ~344K wat lines from ~300 CLIF insts. Not
    punroll (still huge with PLIRON_PUNROLL=0). It's waffle's
    `Reducifier`: the post-inline CFG is irreducible, and Reducifier
    clones blocks per loop-context — 60 blocks → ~340K lines. That's
    a wasm-emit amplification landmine any large irreducible function
    can trip. If rustc.wasm size matters, PLIRON_XCGU=0 restores the
    old gate; a real fix needs reducible-CFG hygiene in the CLIF
    pipeline or a bound on Reducifier cloning.

11. The inliner "ineligible" reject log now prints the reason
    (no_inline/linkage/has_body) under pliron::inline debug — kept it,
    it's gated and was the only way to see the cross-CGU decl problem.
    Reasonable to keep or strip; cheap either way.

## Update — wasm va_arg (committed db38575)

12. The vararg buffer used the SysV overflow-area layout (8-byte min
    slots, align clamped [8,16]) — a copy-paste of the native convention.
    On wasm the buffer IS the real C ABI (tool-conventions BasicCABI;
    clang emitVoidPtrVAArg with a 4-byte slot): each arg takes
    align_up(size,4) at natural alignment. Two consecutive i32 varargs
    sat at 0/8 where LLVM reads 0/4 — any real wasi-libc variadic call
    (snprintf, scanf) would decode garbage. Fixed in both packing sites
    (FL + clif-lower; the non-foreign CLIF branch is the only one wasm
    uses) and in builder::va_arg. Aggregates are indirect per clang's
    isAggregateTypeForABI — dead code under rustc's VaArgSafe gate but
    kept so the convention is complete. New wasi check `variadic slots`
    (i32,u32,i32,i32,i64,f64) fails on the old layout, passes now.

13. Aggregate varargs are unreachable from Rust (VaArgSafe excludes
    them, and E0617 rejects non-promoted vararg args). va_indirect is
    defensive/completeness only — flag if it should be dropped.
