# 2026-11-18 — flip `PLIRON_RA2_REMATW=7`

Default-on rematerialization spill-weight cut, gated on use count.

Plain `PLIRON_RA2_REMATW=1` (LLVM's always-`/2`) wins pulldown
`FirstPass::parse_block` (~−270k non-nop) but costs protobuf
`prost::encoding::merge_loop` +205k on every tried narrowing that still
fires there (call-crossing, `prio>64`, `/4`). Mode `6` (`uses≤4`)
avoids none of the protobuf loss and kills the pulldown win: the
helpful remats are the dense ones.

Mode `7` (`uses ≥ PLIRON_RA2_REMATW_MINUSSES`, default 8) keeps the
dense remats. Callgrind non-nop, copies=2:

| program | non-nop | verdict |
|---|---|---|
| pulldown-cmark | −107,944 (−0.21%) | REAL win |
| regex | −6,450 | REAL win |
| rust-compression | −4,713 | REAL win |
| blind-sig | +19,988 | noise (spread 83k) |
| blake3-scalar | 0 | noise |
| rust-json | +283 | noise |
| rust-html-rewriter | +6,492 (+0.006%) | REAL loss |
| rust-protobuf | +10,921 (+0.017%) | REAL loss |

`=0` off. `PLIRON_RA2_REMATW_MINUSSES=16` clears protobuf but loses
blind-sig (+50k); leave the floor at 8.

Source: `vendor/regalloc2` `rematw_mode` (unset → 7), Cranelift
`compute_remat_vregs` (`!= "0"`).
