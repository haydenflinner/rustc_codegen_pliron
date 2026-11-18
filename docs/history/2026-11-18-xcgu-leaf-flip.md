# 2026-11-18 — XCGU_LEAF=100 + LASTCALL=20 default-on

After `PLIRON_RA2_REGION` + `SPILLHOIST` flipped, retimed the cross-CGU
copy candidates with callgrind (`harness/placement.py heldout`, non-nop
deltas).

## SIZE=100 + LASTCALL=20 (still not a flip)

| program | non-nop |
|---|---|
| pulldown-cmark | −0.92% |
| rust-json | −1.13% |
| rust-html-rewriter | −0.29% |
| regex | −0.06% |
| blake3-scalar | 0 |
| rust-compression | +0.03% |
| blind-sig | +0.17% |
| rust-protobuf | **+1.18%** |

Protobuf flipped from the earlier −0.73% (pre-REGION table) to a real
loss. `INLINE_XCGU_SOLO` / `LATE` still repair regex or copies and cost
pulldown.

## LEAF=100 + LASTCALL=20 (flipped)

Leaves only (cold / intrinsic / transitive small-leaf callees) up to
100 MIR stmts, with LASTCALL=20 so single-user deps over that size stay
Internal.

| program | non-nop |
|---|---|
| rust-json | −1.23% |
| rust-html-rewriter | −0.42% |
| pulldown-cmark | −0.40% |
| regex | −0.08% |
| rust-protobuf | −0.07% |
| blind-sig | −0.02% |
| blake3-scalar | 0 |
| rust-compression | +0.03% |

LASTCALL is required: LEAF=100 alone is protobuf +3.24%. Compression's
+12.6k is the same `HistogramSelfAddHistogram` RA residual REGION left
at +12k vs the old +101k.

Defaults: `pliron-cfg` `DEFAULT_VALUES`; off with `PLIRON_XCGU_LEAF=0`
and `PLIRON_XCGU_LASTCALL=0`.
