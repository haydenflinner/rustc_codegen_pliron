# Why eqbug and parse_only trail LLVM (2026-10-22)

Native aarch64, `bench/run.sh`, change vs stock LLVM (+ = slower).

| kernel | default | `PLIRON_SELCHAIN=1 PLIRON_MULCARRY=1` |
|---|---|---|
| eqbug | +22% | +10% |
| parse_only | +15.7% | +9.8% (+3.7% with `PLIRON_STEPCMP=1` too) |

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

Two more instructions per digit, fixed after this run:

- **`stepcmp` (`PLIRON_STEPCMP=1`, last step).** The exit test compared the
  old pointer with `end - 1`, and the post-indexed `ldrb` had already
  advanced the register, so isel rebuilt `p` with `sub x, p', #1`. The step
  rewrites `icmp ne p, e-K` to `icmp ne p+K, e`, which is exact mod 2^w.
- **`PLIRON_REMATLOOP=0` (vendored Cranelift `elaborate.rs`).** Remat
  copied `byte - '0'` into the next block of the same loop, so it ran twice
  per digit. The switch skips remat between blocks of one innermost loop.
  **It regresses sieve by 12 points** (−4% → +8%), even though sieve then
  compiles to fewer instructions (931 vs 1027), so the cost isn't the count.
  Not yet understood. Keep it off.

Standalone `parse_only::run` (best of 7, 3 rounds):

| build | ms | vs stock |
|---|---|---|
| stock | 364 | — |
| pliron default | 422 | +16% |
| `MULCARRY` | 401 | +10% |
| + `STEPCMP` | 378 | +3.8% |
| + `STEPCMP` + `REMATLOOP=0` | 377 | +3.4% |

The loop is now `ldrb, sub, cmp, b.hs, madd, cmp, b.ne`, 7 instructions
like each of LLVM's unrolled copies. The remaining gap is the unroll itself.
Our `unroll` needs constant-decided trips, and `punroll` needs a body with
no mid-loop exits. LLVM's version unrolls up to the `len < 9` guard's
maximum trip count and keeps each copy's exit test.

**Measurement trap:** zsh doesn't split unquoted `$e`, so
`env $e` with `e="A=1 B=1"` sets `A` to `"1 B=1"`. An earlier round of
these numbers showed "fixes making it slower" because no knob was actually
on.

Both steps are opt-in. `test.sh selmul` runs them against stock rustc's
output, and fails when either rewrite is broken on purpose.
