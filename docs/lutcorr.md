# `lutcorr`: switch → `x*s + b + corr[x]` (corsix LUT correction)

pliron-IR pass (`pliron-ir-opt/src/lutcorr.rs`), default-off behind
`PLIRON_LUTCORR=1`, running at the end of `iropt_tail` in `src/cgupool.rs`.

Implements [the corsix lookup-table-correction scheme](https://www.corsix.org/content/using-a-lookup-table-to-correct-an-approximation):
when every arm of an `llvm.switch` funnels constants into the same join
block args, the same `ret`, or the same alloca `store` slots (rustc's
`match` shape), and the values per position are approximately linear in
the case index `x`, emit `out = x*s + b + corr[x]` where `corr` is a
`u8`/`u16` immutable `__pliron_lut.<fn>.<n>` global instead of a jump
table.

## Shapes folded

- `match x { v => C }` where every arm is `store *alloca <- const; br
  join` — the dominant rustc shape (each alloca dst is a result slot).
- `br join(consts)` and trivial `ret const` arms.
- Default arm: `unreachable` → no bounds check; consts → `select` on
  `idx <= n-1`; real code → `cond_br inrange` fallback (dense domain
  required).
- Full-domain tables: when the default is a const set and the index
  type is ≤ i8, tables cover all `2^width` inputs — no range check, no
  selects (same shape LLVM's switch→LUT emits).

## Position kinds

| pos | when | code |
|---|---|---|
| `Same` | identical Value on every edge | reuse/const |
| `IntLin` | residuals fit `x*s+b` exactly | mul+add |
| `IntTab` | residuals fit u8/u16 | mul+add+byte load |
| `SymLin`/`SymTab` | `ConstVal::Sym` offsets almost linear | gep+add (±corr) |
| `RawTab` | distinct syms / unfittable residuals | `n x ty` LUT (LLVM switch→LUT) |

Bails (leaves the switch, never wrong): non-const non-uniform arms,
arms that do real work, sparse domain + real-code default, mixed
Sym/Bits that don't fit `RawTab`, residual > 65535.

## Measurements

`probe.rs`: `name_of(u8)->&'static str` (5 arms+default), `score(u8)->u32`
(7 arms+default). `-O` x86-64, code bytes per function:

| fn | stock rustc+LLVM | pliron off | pliron `LUTCORR=1` |
|---|---|---|---|
| `name_of` | 64 B | 160 B (jump table) | 32 B + 2 KB ptr tab + 256 B len tab |
| `score` | 16 B | 160 B (jump table) | 32 B + 256 B u8 tab |

Emitted code (`score`): `movzbq (%rsi,%rdi,1),%rax; ret` — identical
shape to stock LLVM's LUT. `name_of`: `mov (ptrtab,idx,8)` +
`movzbq (lentab,idx,1)+1` — 7 insns, no branches.

Coverage is intentionally narrow: only const-sink switches fold. On
`pliron-clif-opt` (1182 `match` statements, mostly real-work arms) the
pass fires zero times. Sparse domains with real-code defaults stay
jump tables — rustc flattens `a..=b` match ranges into icmp+cond_br
chains before `llvm.switch` sees them, so range-clustering LUTs are
not reachable at this layer.

Gate: `harness/gate.py` — `lto` and `slotmerge` legs fail identically
on clean `main` (preexisting); all other legs pass.
