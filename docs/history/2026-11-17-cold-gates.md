# Size/cold gates on the duplicating passes (2026-11-17)

PARITY item 3: gate `punroll`, peeling, `jumpthread`, `looprot` on cold
loops or huge functions. Measured; nothing pays for a default.

## What the duplicators cost

`clif-check codegen` (pipeline + Cranelift aarch64) on clcg, every 2nd
function, 940,452 machine instructions by default:

| off | insts | change |
|---|---:|---:|
| `PLIRON_PUNROLL=0` | 904,616 | -3.8% |
| `PLIRON_PEEL=0` | 910,491 | -3.2% |
| `PLIRON_LOOPROT=0` | 923,668 | -1.8% |
| `PLIRON_JUMPTHREAD=0` | 904,200 | -3.9% |
| `PLIRON_TAILDUP=0` / `UNROLL=0` / `BUNROLL=0` | | under +0.5% |

## Three gates

- `PLIRON_COLDMARK=1` (`pliron-clif-opt/src/coldmark.rs`, steps
  `coldmark` before `jumpthread` and `coldmark2` before `looprot`): every
  block from which no path reaches a `return` is marked cold, so the
  passes that skip cold code (`punroll`, `taildup`, `looprot`, which
  checks marks under this knob) leave it alone and block placement moves
  it out. std -0.06% instructions, clcg -0.01% (its loop-weighted
  spills +1.5%: the marks move register-allocation splits). Panic paths
  here are mostly one `call panic_*; trap` block the lowering already
  marks; a loop on a doomed path is rare.
- `PLIRON_PUNROLL_GROWTH=<pct>` (`PLIRON_PUNROLL_GROWTH_MIN`, default
  256): `punroll` stops once it has created more than `pct`% of the
  function's size. clcg: 10% -0.32%, 25% -0.24%, 50% -0.14%. The copies
  are spread over many small functions, not a few huge ones.
- `PLIRON_PUNROLL_FNMAX=2000` (existed): -1.25%, every loop of a big
  function stays rolled, hot ones too.

## Runtime

`bench/heldout/ab.py`, 5 rounds, retired instructions against the
default:

| side | rust-compression | pulldown-cmark |
|---|---:|---:|
| `PLIRON_COLDMARK=1` | +0.1% | +0.2% |
| `PLIRON_PUNROLL_GROWTH=10` | +0.7% | +0.3% |
| `PLIRON_PUNROLL_FNMAX=2000` | 0.0% | +0.3% |

Cycles moved within the machine's noise either way. Some of what the
gates drop runs: none is a size win for free, and the size they save is
small. The unexecuted copies of brotli's HqZopfli are live code a static
rule cannot tell from hot code; a profile can (`punroll` already skips a
header measured at zero executions).

Tests: `pliron-clif-opt/tests/coldmark.rs`. Corpus std under
`PLIRON_COLDMARK=1` with `PLIRON_VERIFY=full`: 4,244 ok, 681
inconclusive, as by default.
