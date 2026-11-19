# 2026-11-18 — flip `PLIRON_RA2_REGION_CAP=2`

On top of `REMATW=7`, raise the region-split use charge cap from 1 to 2
(more willingness to keep a value in-register through an S block).

Callgrind non-nop, copies=2, against CAP=1:

| program | non-nop | verdict |
|---|---|---|
| blind-sig | −40,469 | REAL win |
| pulldown-cmark | −63,804 | REAL win |
| rust-html-rewriter | −33,894 | REAL win |
| rust-protobuf | −5,320 | REAL win |
| rust-json | −885 | REAL win |
| blake3-scalar | 0 | noise |
| regex | +1,008 | REAL loss |
| rust-compression | +7,122 (+0.017%) | REAL loss |

`HistogramCombine` itself is nearly flat under CAP=2 (+156 on
copies=1); the compression loss is elsewhere
(`StoreAndFindMatchesH10`). Net strongly positive.

Source: `vendor/regalloc2` `region_use_cap` default 2.0.
