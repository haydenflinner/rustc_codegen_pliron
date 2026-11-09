# aarch64 `cmp; ccmp` fusion + foldcommon (2026-11-11)

Two-part attack on the boolean-materialization gap: LLVM emitted ~450
`cset` + 421 `ccmp` on html-rewriter where we emitted ~1754 `cset` +
163 `ccmp` (div-checks only). Its ccmps are ~86% branch-consumed —
SimplifyCFG `FoldBranchToCommonDest` merges the short-circuit `&&`/`||`
block pair first, then the combined compare lowers as a flag chain.

## `PLIRON_CCMP` (vendored Cranelift lowering, default on — e5a33c2 + 4144dde)

`band`/`bor` of two `icmp`s and `select` of an `icmp` against a 0/1 arm
lower to `cmp; ccmp u,v,#nzcv,guard` (`is_nonzero_cmp` consumers incl.
`brif`/`trapnz`, plus `lower` value producers). Four modes:

| mode | bool op  | select form     | guard   | NZCV const  |
|------|----------|-----------------|---------|-------------|
| 0    | `A && B` | `sel(A, B, 0)`  | condA   | falsifies B |
| 1    | `!A && B`| `sel(A, 0, B)`  | !condA  | falsifies B |
| 2    | `!A || B`| `sel(A, B, 1)`  | condA   | satisfies B |
| 3    | `A || B` | `sel(A, 1, B)`  | !condA  | satisfies B |

- Externs are **total** (isle-0.135.5 can't tail-call partial ctors —
  `Option<&CondResult>` codegen bug): on decline they build exactly the
  generic result (`is_nonzero`/alu-`and`/`orr`/`lower_select`), so
  `=0` is byte-identical.
- Secondary compare restricted to I32/I64 (plain reg/reg `ccmp`);
  primary anything `emit_icmp` handles. Both operand orders tried.
- `nzcv_for_cond` brute-forces the 16 flag states through `eval_cond`.

Numbers: html-rewriter ccmp 163→207, cset −73, cmp −46, and −36,
b −130; net −10 of 212,879 static insns (regalloc churn eats the rest).
Held-out geomean 1.007 (wash), json 1.001 on 12 rounds. Gate 94/94,
fuzz 400/400. Tests: `pliron-clif-opt/tests/aarch64_ccmp.rs` (7,
red-verified 6/7 without the patch).

## `PLIRON_FOLDCOMMON` (CLIF pass, opt-in — c84172c)

`brif A -> S,F` / `S: brif B -> T,F` (shared edge args equal, `S`
single-pred) → `brif (band A B) -> T,F`; `||` mirror → `bor`. Clones
`S`'s ≤4 `pure_op` insts into `b`, deletes `S`. After `mergechain`,
before `jtround`.

Iterations that did **not** work:
- Folding any bool-typed `band`/`bor` conditions: +312 static —
  non-icmp conditions materialize `cset; op; cbnz`.
- `icmpish` only (any width): +294 — narrow secondaries can't `ccmp`,
  so declined fusions still cost `cset`/`uxtb` (+148/+101).
- Final gate: both conditions `icmp`/`uextend(icmp)` **and** at least
  one compare on 32/64-bit operands ⇒ fusion guaranteed, every fold is
  `cmp;ccmp;b` (3) replacing `cmp;b`+`cmp;b` (4) plus a block.

With the final gate + ccmp: html-rewriter ccmp 163→401 (≈ LLVM's 421),
cset −9, static +67 vs ccmp-only (block-layout/regalloc churn, not
duplication — `S` is deleted not copied). Suite geomean 0.996
[0.940,1.062], compression 0.966* win; kept **opt-in** — the flag-chain
lowering stands alone but the CFG fold doesn't yet pay for its churn.
