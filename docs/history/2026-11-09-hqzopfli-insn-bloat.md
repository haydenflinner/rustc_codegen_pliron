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

## Follow-up: the walk loop, precisely (same day)

Loop at fn+0x17b0..0x180c in profbench (`nodes[i']` walk):

```
ldr x2,[sp,#0x2330]          ; fill base
madd x14, x11, x27, x2       ; addr = i*20 + base
ldur w15,[x14,#8]            ; len
ldur w14,[x14,#0x10]         ; dist
and, and, add, mov           ; combine
sub x14, x11, w2, uxtw       ; i'
cmp; b.hs                    ; bounds check
mul x3, x14, x27             ; i'*20        (dup of madd below)
ldr x4,[sp,#0x2330]          ; fill base
madd x4, x14, x27, x4        ; addr' = i'*20 + base
ldr x5,[sp,#0x2330]          ; fill base AGAIN
str w1,[x3,x5]               ; == str w1,[x4]
str w2,[x4,#4]
add; cmp; b.eq; ldr base; mov; b   ; ~21 insns/iter vs LLVM ~12
```

CLIF (`block647`): the address is shared — `v1493 = iadd(imul(v1486,20), v448)`
feeds both `store.i32 v8463, v1493` and `store.i32 v1483, v1493+4`. The
dup is **isel**: per-inst amode folding picked RegReg `[x3,x5]` for the
first store (materializing the `imul` again) and `madd`+imm for the
second. A `madd`+imm0 for the first would let one `madd` serve both
stores and survive as a register — 2 insns + 1 fill per iter.

The base `v448` is a block param (loaded once, live across the whole
walk including `try_call fn24/fn25` insert calls) → RA fills it 3-4x
per iteration at `[sp,#0x2330]`; LLVM keeps it callee-saved. The fills
aren't reload-CSE-able because the `madd` overwrites the just-filled
register. This is pressure, not a missed hoist: `escapes` for `V(v4)`
spans ~200 blocks — `self` escapes through real calls, so the LLVM
`!alias.scope`-style proof that would let the *field* loads hoist is
unavailable; LLVM agrees they escape (it hoists via scoped metadata,
not capture analysis).

`PLIRON_PUNROLL_FNMAX` (opt-in): 2368→1966 insns here; held-out A/B
neutral (1.006 [0.930, 1.042]) — committed as a size lever.

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

## Fixed: the `mul`/`madd` address duplication (same day)

Root cause: `amode_shareable` (patch 56) gated sharing on a
register-pressure proxy — `value_max_pressure <= 5`. The walk loop is
register-starved by construction (that's why the base spills), so the
gate refused to keep `iadd(imul(i,20), base)` whole and every access
decomposed it: `mul`+reg-reg for the first store, `madd`+imm for the
rest. But decomposition of an `iadd` containing an `imul` is never
cheaper — `mul`+regreg and `madd`+imm0 cost the same for a single
access, and whole-value sharing wins at ≥2 uses.

Change (`vendor/cranelift-codegen/src/machinst/lower.rs`,
`Lower::amode_shareable`): a multi-use `iadd` whose operand is an
`imul` result is always shareable, bypassing the pressure cap (plain
`iadd reg,reg` still respects it — decomposing those is genuinely
free). Regression test: `pliron-clif-opt/tests/aarch64_scaled_amode.rs`
`imul_address_is_shared_across_stores` (fails without the fix).

Result on the walk loop (fn+0x17a0..): the two stores now share one
`madd x15, x12, x27, x2` (`stur [x15]` / `stur [x15,#4]`); loop is ~19
insns vs ~21. profbench gzip+brotli 40 iters: ~125 → ~120 ms vs LLVM
~96 ms (ratio ~1.31 → ~1.22). Remaining per-iter overhead is the 3×
`[sp,#0x2328]` base fills — RA pressure, not isel.
