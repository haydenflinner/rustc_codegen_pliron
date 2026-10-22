# Why eqbug and parse_only trail LLVM (2026-10-22)

Native aarch64, `bench/run.sh`, change vs stock LLVM (+ = slower).

| kernel | default | `PLIRON_SELCHAIN=1 PLIRON_MULCARRY=1` |
|---|---|---|
| eqbug | +22% | +10% |
| parse_only | +15.7% | +9.8% |

The other 14 kernels didn't change beyond noise (two alternating rounds).

## eqbug: ALU op count

The loop is throughput-bound: LLVM issues 36 ALU ops per trip, we issued 44
(44/36 ≈ 1.22).
- `jumpthread` turns each `if .. {a += k}` into selects, but their arms stay
  behind block params until `mergechain`. That is after the only `clifpeep`
  run, so three existing folds never saw the shape:
  - `eq_decided`: `x+3==k` and `x-5==k` can't both hold.
  - `seladd`: `iadd (select c,1,0), 2` becomes `csinc`.
  - LLVM's `select c, x+a, x+b` → `x + select c, a, b`, which we didn't have
    at all.
- `selchain`, a late step right after `mergechain`, runs them: 47 → 42
  instructions per trip.
- Most of what remains is `black_box`. LLVM round-trips it through the
  stack, so `ldrb`/`ldrh` do the zero-extends on the load ports. We keep the
  values in registers and pay five `uxtb`/`uxth` on the ALUs.

## parse_only: the carried multiply

`str::parse::<u32>` on words shorter than 9 bytes takes core's
no-overflow digit loop. LLVM fully unrolls it with one `madd` per digit, a
3-cycle carried chain. Ours carries `acc * 10` across the backedge
(`(acc + d) * 10`), so each digit pays add then mul: 4 cycles, and
Cranelift can't fuse them. `mulcarry` carries `acc` instead and recomputes
`imul p, K` right before the add, in the add's block, so isel emits `madd`.

Still open:
- the full unroll;
- Cranelift's remat duplicating `byte - '0'` into the next block;
- the post-indexed `ldrb` forcing a `ptr - 1` for the exit compare.

Both steps are opt-in. `test.sh selmul` runs them against stock rustc's
output, and fails when either rewrite is broken on purpose.
