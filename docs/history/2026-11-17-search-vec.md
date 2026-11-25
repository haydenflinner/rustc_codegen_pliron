# Early-exit search-loop vectorisation (2026-11-17)

Four knobs, each opt-in, plus one default change. Native aarch64,
measured as exact retired instructions per call, `(I(2N) - I(N)) / N`
under `/usr/bin/time -l` (scratch driver: a byte buffer whose first
"hit" is at offset `len`).

## What changed

- **Default** (`loopvec`): a short-circuit `a || b` / `a && b` that
  ifconv flattened into `select(c, 1, d)` / `select(c, d, 0)` over
  compares is a lane mask (`mask_select`, emitted as `bor`/`band`/
  `bitselect` by `emit_mask_select`), and an early-exit condition
  `band(mask, 1)` is tested as the mask itself (`mask_under_band1`)
  instead of re-comparing `{0,1}` lanes against zero.
- `PLIRON_JTVEC=1`: a second jumpthread round (`jtvec`) ahead of
  `selknown`. `take_while`'s done flag, which slotprom/loadfwd turned
  into a constant block parameter after the first jumpthread, threads
  away and leaves a plain multi-exit loop.
- `PLIRON_SELKNOWN_COND=1` (`edgeknown::selects`): a select on any
  branch condition, not only an equality, folds where the branch
  decides it. `zip(..).take_while(..)`'s `p = select(i < n, base + i,
  old)` above the header's `brif (i < n)` becomes `base + i`.
- `PLIRON_VEC_EXACT=1` (`loopvec`): on a hit, the resume block finds
  the first failing lane (`vall_true` per group, then
  `ctz(!vhigh_bits(ok))`) and re-enters the scalar loop there, instead
  of at the 64-byte window's first lane.
- `PLIRON_VTRUEBR=1` (vendored Cranelift, aarch64): a branch on
  `vall_true`/`vany_true` tests the reduced lane (`uminv; fmov; cbz`)
  instead of `cmp; cset; tst; b.eq`. The `position` vector loop goes from
  21 to 18 instructions per 64 bytes.

## Numbers

Instructions per call, hit at byte `len`. `def` is the default, `all` is
JTVEC + SELKNOWN_COND + VTRUEBR, `ex` adds VEC_EXACT.

| kernel | len | LLVM | def | all | ex |
|---|---:|---:|---:|---:|---:|
| `take_ws` | 0 | 15 | 19 | 58 | 85 |
| | 8 | 69 | 97 | 179 | 88 |
| | 32 | 244 | 338 | 427 | 96 |
| | 256 | 1,794 | 2,522 | 138 | 178 |
| | 4096 | 28,752 | 41,073 | 1,457 | 1,438 |
| `mismatch` (zip) | 0 | 18 | 33 | 50 | 71 |
| | 8 | 74 | 112 | 124 | 71 |
| | 32 | 241 | 351 | 370 | 78 |
| | 4096 | 28,719 | 41,093 | 1,509 | 1,530 |
| `position` | 0 | 15 | 49 | 44 | 72 |
| | 8 | 66 | 111 | 107 | 73 |
| | 32 | 198 | 234 | 232 | 85 |
| | 4096 | 24,636 | 1,261 | 1,012 | 1,020 |

With all four on, a hit at byte 4 or later is cheaper than the scalar
loop, and the cost is flat up to the first 64-byte window. A hit at byte
0 pays the vector entry plus the lane search: 70-85 against LLVM's 15.
That needs a scalar prologue (one or a few peeled iterations ahead of
the vector entry); running `punroll`'s peel before `loopvec` leaves
interior block parameters `loopvec` refuses.

Corpus replays (`clif-check report --check`, `PLIRON_VERIFY=full`) with
JTVEC + SELKNOWN_COND + VEC_EXACT: std 4,244 ok / 681 inconclusive, the
same as the default; clcg every 4th 8,642 / 870; wasm-bench as wasm32
2,264 / 398; no failing rows. Tests: `pliron-clif-opt/tests/search_vec.rs`.

## Lane-0 probe (`PLIRON_VECPROBE=1`, 2026-11-24)

The "scalar prologue" the last paragraph asks for, done inside
`loopvec`: a block `vb0` between the trip guard and the vector header
re-runs group 0's lane test alone (one `load.i8x16` + `icmp` +
`vall_true` + `brif`). On a hit it resumes the scalar loop at the exact
lane (`VEC_EXACT`'s mechanism works unchanged — group 0's window is a
single 16-byte load, so its "first failing lane" is the answer); on
all-ok it enters the vector header at iteration 0, where group 0's test
simply re-executes. Peeling only group 0 — not the whole 64-byte
window's scalar body — is what makes it cheap: ~10 extra instructions
on a miss or deep hit, vs ~30 for the old full-window peel.

Microbenchmark (instructions/call, `position`-shaped scan, 4 KiB
buffer): hit=0: llvm ~20 / vec ~97 / **vp ~30**; hit=8: 44/55/30;
hit=32: 202/64/44; hit=64: 423/75/60. With the probe a hit anywhere in
the first 16 bytes is ~3x cheaper than before and already beats LLVM's
scalar from byte 4; the miss/deep-hit cost is +8-12 over `vec`. Tests:
`pliron-clif-opt/tests/search_vec_probe.rs` (5/5).

ab.py retired instructions, ratio to `pliron` default:

| program | vec | vp |
|---|---:|---:|
| pulldown-cmark | +3.0% | +1.4% |
| rust-html-rewriter | +1.4% | +1.1% |
| rust-json | +0.9% | +0.9% |
| regex | -0.2% | -0.2% |

irdiff.py exact counts, pulldown-cmark (def / vec / vp):
46,702,904 / 47,564,770 / 47,198,378 — the probe recovers 42% of the
vec regression. Per-function self-Ir vec->vp: `scan_atx_heading` -311k,
`append_text` -266k (cheap early hits as designed);
`scalar_iterate_special_bytes` +125k and `scan_code_fence` +61k pay the
probe on deep hits; `scan_while<ws_no_nl>` is +172k vs def either way —
its probe's `cbz` never fires in this workload (runs are <64 B or
all-ws, so the vector path isn't reached) and the residual is the
scalar path's restructure.

A gotcha that cost a measurement cycle: `ab.py`/`irdiff.py` build
against `target/release/` by default (`--backend`), while `cargo build`
writes `target/debug/`. A `vec`-side binary built from a stale release
dylib silently lacks a new opt-in knob — diff the rlib fingerprint's
`rustflags` (or disassemble for the probe's `uminv;cbz`) when a knob
appears not to fire. Also `irdiff.py --env` takes one `K=V` per flag;
a comma-joined list parses as a single `K="1,K=V,..."` and disables
everything.

Before a flip: with `PLIRON_JTVEC=1` the `slotpromlt` stanza's
`PLIRON_RA2_XBLOCK` size check sees no drop (1,456 -> 1,456 instructions;
the threading removes the moves XBLOCK targets), so that stanza would
need its baseline pinned with `PLIRON_JTVEC=0`.

Not covered: countdown scans (`rposition`, `trim_end`): `loopidiom::count`
rejects them under `loopvec`'s `relax=false`.
