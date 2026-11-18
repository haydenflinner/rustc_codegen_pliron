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

Before a flip: with `PLIRON_JTVEC=1` the `slotpromlt` stanza's
`PLIRON_RA2_XBLOCK` size check sees no drop (1,456 -> 1,456 instructions;
the threading removes the moves XBLOCK targets), so that stanza would
need its baseline pinned with `PLIRON_JTVEC=0`.

Not covered: countdown scans (`rposition`, `trim_end`): `loopidiom::count`
rejects them under `loopvec`'s `relax=false`.
