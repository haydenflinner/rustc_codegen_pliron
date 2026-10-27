# The four kernels behind LLVM: parse_only, eqbug, varint, strwork (2026-10-27)

Today's parity card has four native kernels behind LLVM: parse_only
1.08–1.16, eqbug 1.13–1.26, varint 1.00–1.20, strwork 1.07–1.12
(2026-10-27-parity-card.md). This note is the per-kernel cause, the fix,
and the measurement. All measurements are aarch64, worktree build of
b0e4ada plus the changes here, with the candidate flag set
`EDGEDEDUP JT_SHAPE CMP3 LTUNDEF NORETTRAP EDGEMERGE EPIMERGE NARROWEXT`
on both sides unless noted.

## Standalone first: two kernels are already at parity

`bench/run.sh` compares against an LLVM baseline measured minutes earlier
and the box was never quiet today (load 7–60), so the first step was a
standalone harness: each kernel's `run()` compiled with `-O
-Ccodegen-units=1` by stock rustc and by pliron, timed as min/median of 7
samples of ≥0.25 s each, the variants interleaved round-robin under
`heavy.lock` (scratchpad `k/timeall.sh`). Load 4–8 during this run:

| kernel | LLVM | pliron, candidates | ratio |
|---|---|---|---|
| parse_only | 363 ms | 483 ms | **1.33** |
| eqbug | 0.1115 ms | 0.1357 ms | **1.22** |
| strwork | 13.44 ms | 13.30–13.78 ms | 1.00 |
| varint | 6.38 ms | 6.17–6.30 ms | 0.97 |

So varint and strwork are not behind: their card ratios are the noise of a
loaded box (the branches note of the same day saw single kernels swing
30–40%). parse_only and eqbug are real, and larger than the card says.

## parse_only: three instructions per digit too many

`str::parse::<u32>` on words shorter than 9 bytes runs core's no-overflow
digit loop. LLVM unrolls it fully with one 7-instruction copy per digit
(`ldrb, sub, cmp, b.hi, madd, cmp, b.eq`). Ours stayed rolled at 10
instructions per digit — and it was 7 on 2026-10-22 with the opt-in late
steps (2026-10-22-eqbug-parse-only.md), so two of the three were
regressions since:

1. **`PLIRON_MULCARRY`, `PLIRON_STEPCMP`, `PLIRON_SELCHAIN` are still
   opt-in** (c547d14, d932a3c). Without them the loop carries `(acc+d)*10`
   (`mul` after `add`, no `madd`) and rebuilds the pre-increment pointer
   for the exit compare. They were never flipped and are not in the
   candidate set. Standalone: parse_only 483 → 414 ms, eqbug 0.1357 →
   0.1218 ms, nothing else changes (strwork/varint byte-identical hot
   loops).
2. **The egraph elaborator lost the loop's end pointer** (`add x, ptr,
   len` recomputed every digit). `v1641 = iadd v628, v629` sits in the
   inner loop's preheader in the final pipeline CLIF, but Cranelift's
   elaboration placed it in the loop body. Cause (vendored Cranelift
   `egraph/elaborate.rs`, `start_block`): the elaborator pops a loop's
   `LoopStackEntry` as soon as the domtree preorder visits a block
   outside that loop. The digit loop's exit block is dominated by the
   header, so it comes before the body among the header's domtree
   children (the children list is built in RPO and prepended, so the
   later block in RPO is visited first; which child that is depends on
   the `brif` arm order). Every body block visited after the exit sees no
   entry for its own loop, `loop_hoist_level == loop_stack.len()`, and
   its invariants are placed in the body. This is upstream behaviour,
   and it depends on branch-arm order, so any pass that flips a `brif`
   (`coldret`, `edgesink`, …) can turn it on or off for a given loop.
   **Fix: `PLIRON_ELABLOOPS=1`** (vendored patch 66): `start_block`
   rebuilds the loop stack from the block's loop nest
   (`innermost_loop` → `loop_parent`) using the entry recorded when each
   header was visited. Headers dominate their bodies, so the entry exists;
   the hoist block (the header's idom) dominates every body block, so the
   scoped-map depth it carries is still open. Test:
   `pliron-clif-opt/tests/elabloops.rs` (parse_only's nested shape, both
   arm orders; the body-first order fails without the knob).
3. **Remat of `byte - '0'` across the digit test** (`PLIRON_REMATLOOP=0`,
   known since 10-22, off because sieve regressed 12 points then with no
   asm diff to explain it — the layout-bias finding came later).

With all three the loop is LLVM's: `ldrb, sub, cmp, b.hs, madd, cmp, b.ne`.

## eqbug: the extends are the whole gap, and they come from black_box

Per trip, LLVM issues 37 ALU/branch ops plus 6 byte/halfword stores and
6 loads (its `black_box` is `asm("" : "+m")`: each narrow value goes
through the stack and comes back zero-extended by `ldrb`/`ldrh`). We
issued 42 ALU ops and no memory ops: 5 `uxtb`/`uxth` for the barrier
results (Cranelift i8/i16 values have undefined high bits; a compare
needs one clean operand and extends the other in `cmp w, w, uxtb`), the
rest identical op for op. The loads and stores run on ports that are
otherwise idle in this loop, so LLVM's shape is strictly cheaper.

**`PLIRON_BBMEM=1`** (`src/intrinsic.rs`): a narrow (i8/i16) scalar
`black_box` takes the alloca + volatile-load round trip on native too,
with the slot's address passed to the `__pliron_bbm` memory barrier
between the store and the reload. The address must escape: a barrier on
the value alone is not enough, `slotprom` correctly forwards an
unescaped slot's store across it (first attempt). The barrier's
`stack_addr` operands are loop-invariant and get hoisted, so the
per-trip cost is exactly LLVM's 6 `strb/strh` + 6 `ldrb/ldrh`; two
`uxtb`/`uxth` remain where patch 36 materializes an `ireduce` whose
only uses are a store and an `imul` (neither needs a clean value) — a
refinement for the `ireduce` rule, not done here.

**Result: slower.** Standalone eqbug 0.1219 ms (SELCHAIN) → 0.1254 ms
with the round trip, stable over three interleaved rounds. The 12 memory
ops cost more than the 5 extends they replace even though the per-trip
work is independent, so LLVM's `*m` shape is not the answer on this core;
the knob was removed again (the mechanism is kept in this note). What is
left of the gap, 0.1117 → 0.1219 ms (1.09), is the 5 extends: 3 for the
`k` operands of `x+c == k` (both sides dirty, one must be clean) and 2
for `x == const` compares lowered with an immediate (`cmp w7, #0x11`
needs a clean `w7`; a hoisted constant register plus `cmp wk, w7, uxtb`
would save them, at a register per constant — not done).

## Numbers

Standalone harness (min of 7 samples, three interleaved rounds, load
6–10, every round agrees to <1%):

| kernel | LLVM | candidates | +SELCHAIN MULCARRY STEPCMP | +ELABLOOPS | +REMATLOOP=0 |
|---|---|---|---|---|---|
| parse_only | 364 ms | 484 (1.33) | 415 (1.14) | 401 (1.10) | **381 (1.05)** |
| eqbug | 0.1117 ms | 0.1358 (1.22) | **0.1219 (1.09)** | 0.1220 | 0.1220 |
| strwork | 13.3–13.8 ms | 13.2–13.6 | 13.3–13.6 | 13.75–13.9 | 13.8–13.9 |
| varint | 6.38–6.43 ms | 6.17–6.31 | 6.20–6.27 | 6.18–6.23 | 6.10–6.15 |

`ELABLOOPS` costs strwork 2–3% standalone: `main` (format + parse) grows
by 66 instructions, 32 of them stack stores and 12 stack loads — the
invariants hoisted out of its short inner loops (digit parses, bignum
limbs) raise pressure in the outer loop body. varint's `main` loses 37
instructions (13 fewer stack loads). parse_only's digit loop is 7
instructions with all three, LLVM's count.

### Criterion, all 19 kernels

`bench/run.sh` twice under `heavy.lock`, candidates (B) then B +
`SELCHAIN MULCARRY STEPCMP ELABLOOPS REMATLOOP=0`; load 9–12 with spikes
(LLVM's own arrayvec went 5.5 → 16.2 ms between the runs, so single rows
are not trustworthy):

| | B | B + new |
|---|---|---|
| parse_only | 1.172 | **1.015** |
| eqbug | 1.211 | **1.091** |
| varint_postcard | 1.480 (spike) | 0.930 |
| matmul | 1.107 | 1.001 |
| sieve | 0.968 | 1.189 (spike, see layout) |
| strwork | 0.988 | 1.042 |
| tokenize | 1.071 | 1.098 |
| varint | 1.024 | 1.071 |
| geomean, 19 | 0.949 | 0.860 |

### Layout sweeps (`bench/layout.sh`, 3 layouts, medians)

| kernel | flags | median | runs |
|---|---|---|---|
| sieve | B + SMS | 11.98 ms | 11.92 11.98 12.04 |
| sieve | + ELABLOOPS | 11.99 | 11.84 11.99 12.03 |
| sieve | + ELABLOOPS + REMATLOOP=0 | 11.75 | 11.70 11.75 13.04 (load) |
| strwork | B + SMS | 13.89 | 13.82 13.89 14.34 |
| strwork | + ELABLOOPS | 14.76 | 14.06 14.76 14.85 |
| parse_only | both | — | load 14+, spreads 10–78%, unusable |

(SMS = SELCHAIN MULCARRY STEPCMP.) So `REMATLOOP=0` does **not** regress
sieve: the 2026-10-22 "+12 points" and today's criterion 1.189 are both
load/layout, and the fewer-instruction build is the faster median. The
sieve function loses 75 instructions with it (the rematerialized `i + 1`
and `c + 1` per unrolled copy). `ELABLOOPS` does cost strwork 4–6% on
top of the standalone 3%: real, from pressure.

A variant that kept constants (no-argument values) from being hoisted
out of a restored loop entry produced byte-identical strwork code, so the
extra 32 spills are hoisted *computations*, not constants; that variant
was dropped.

## Status of the knobs

- `PLIRON_SELCHAIN`, `PLIRON_MULCARRY`, `PLIRON_STEPCMP`,
  `PLIRON_REMATLOOP=0`: no downside found (sieve layout-checked); add
  them to the candidate set. Together: parse_only 1.33 → 1.10 standalone
  (criterion ~1.05), eqbug 1.22 → 1.09.
- `PLIRON_ELABLOOPS=1`: new, default off. Correct LICM (parse_only −4%,
  varint −1–2%, sieve 0) but strwork +4–6% from spills in its `main`;
  needs a pressure-aware hoist decision (or better live-range splitting in
  regalloc) before it can be a candidate.
