# 2026-11-09 — BrotliCreateHqZopfliBackwardReferences insn bloat

Follow-up to `2026-11-09-copypair-scalar-merge.md`: with the swap now
vectorized, the remaining `rust-compression` gap lives in the ~1.8x
instruction-count difference (2368 vs LLVM's 1320) — self-time still
~1.4x per iteration (721 vs 421 samples at equal iters).

## Where the extra ~1000 insns are

Census of our final CLIF (1702 insts) vs disasm (2368) — the extra
~660 are regalloc spill/fill:

1. **RA spill pressure (~640 insns, biggest bucket).** 628 `[sp,#imm]`
   accesses vs LLVM's 254; 112 distinct offsets vs 70. `PLIRON_RA2_REMATW`
   doesn't help — these are genuine long-lived values (block params
   like `v448`/`v449` = array base/len live across calls), not remat
   candidates. Circular with code size: bigger function → more live
   temporaries → more spills → more insns.

2. **punroll duplication (~400 insns, verified no perf loss).**
   `PLIRON_PUNROLL=0` cuts the function 2368→1968 with no wall-time
   change (~805ms either way) — the duplicated loops never execute
   here. Work-item #3 (size/cold gates on duplicating passes) is the
   fix; needs a bench A/B before landing.

3. **Bounds-check sites: 26 vs LLVM's 7** (~160 cold insns + live
   range for panic args). MIR emits ~36 `panic_bounds_check` calls
   into our pipeline; we prune to 26. LLVM removes most via
   constraint elimination / range reasoning we lack. Each surviving
   check also carries dead `umulhi(0,1)=0` math in its panic block
   (36 sites — a missed const-fold in the divconst tail).

4. **Per-op density**: 330 `iadd` + 203 `iconst` + 184 `icmp`/`brif`.
   In the hot node-walk loop this lands as ~21 insns/iter vs LLVM's
   ~12: 4× base reloads + a `mul`/`madd` pair computing the same
   `i*20` twice (CSE-shape miss across the reg+reg vs madd lowering
   paths) + a dead mov.

## LLVM-side facts

- LLVM's loop keeps `x3`=base, `x12`=20 (stride) in callee-saved regs,
   zero reloads — it can afford them because the function is smaller.
- Its bounds checks: 7 sites for the same source (~29 of MIR's checks
   proven redundant by range analysis).
- `log2f` self-samples 50 vs our 85, `__bzero` 123 vs 189 — the same
   library functions, more calls/zeroing on our side; consistent with
   more stack temporaries being cleared per iteration.

## Implications

- The gap is not one transform — it's LLVM's constraint elimination +
  pressure discipline versus our "more temporaries per source op".
- Cheapest confirmed lever: punroll size-gate (work item #3).
- Medium: bounds-check redundancy elimination (dominated
  `idx < len` checks sharing a still-live `len`), and folding
  `umulhi`/`symbol_value` in cold panic blocks.
- Deep: stack-slot provenance for hoisting `Vec`-header field loads
  (`S(ss)` roots into `iso_l` via ncc when escapes are nocap-only) —
  investigated; `call fn0(stack_addr ss27)` escapes early enough that
  it doesn't apply to this function without richer reasoning.
