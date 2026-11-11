# 2026-11-11: srccopy soundness + promotion, MEMCAP, narrowext ret sink

## `PLIRON_LOADFWD_SRCCOPY` (61f5f76, fixed a38c99a, promoted 2d2b8a2)

`loadfwd` gained a source-copy rule: a load whose location is covered
by `store (load s)` re-reads `s` directly — LLVM's memcpy-forwarding
for the `store`-of-`load` shape `clone_from_slice`/small memcpy leaves
behind. Brotli `sub_match!` copies 16B chunks into stack buffers then
u64-loads them; after `bunroll` exposes the copies with constant
offsets, srccopy turns the slot reads into direct slice loads and
`ldmerge` folds the byte chain into one `ldur` — LLVM's exact shape.

Two offset bugs found by corpus diffcheck + held-out crash:

1. The emitted load was `(q, base+d)` where `base` = source root's
   normalized offset + source inst offset; `q` already carries its
   normalized offset, so addresses with nonzero root offset were
   double-counted (`core::net::parser` write lost, byte 24->23).
2. The `kpair` call sites passed the normalized root offset but
   dropped the *instruction's own* `+imm` field — `load p+8` matched
   the covering entry for `p+0` and re-read field 0
   (`unicode_escape`, 1086 corpus mismatches).

Fixes: emit `(q, qoff+d)`; add `imm` back at both call sites.
Regression tests: `pliron-clif-opt/tests/loadfwd_srccopy.rs` (the
`unicode_escape` shape needs *no store at all* — the covering entry
is the earlier load's own availability record).

Also landed in a38c99a: `bunroll`'s exit-cone budget charged every
forward-reachable block (inner loops always blew the 64-block cap);
the walk is now pruned to blocks that can reach a direct free use of
an in-loop value. `FindMatchLengthWithLimitMin4`'s index loop
unrolls.

Numbers (held-out, 5 interleaved rounds, `sc=SRCCOPY+BUNROLL_BODY=200`
was 0.983 [0.969,1.000] on rust-compression; `SRCCOPY` alone across
the suite: geomean 1.001 [0.989,1.017], rust-compression 0.984,
regex-syntax .text +64B). Promoted: sound + near-free; `=0` stays the
off switch. `PLIRON_BUNROLL_BODY` stays a knob (default 48): 200
costs +672B .text on regex-syntax and mostly buys the srccopy
exposure.

## `PLIRON_CGU_MEMCAP=<MiB>` (3b63d8b, opt-in)

Memory-pressure throttle on `CGU_INFLIGHT`: `submit` checks current
process RSS against the cap and, when over, admits stage-A jobs only
for ords a parked stage-B job is waiting on (deadlock-safe — the
`needs` ords recorded at park). Predictive accounting: each running
job carries an `est_mib` growth estimate from `Context::approx_bytes`
so admission accounts for post-inline amplification, not just current
footprint. Deterministic output verified (object bytes identical);
no deadlock at absurd cap (10 MiB) under all facts modes. Off-macOS
`footprint_mib` returns 0 so the knob is inert there.

Full "share IR-opt results across live xcgu copies" (the MEM(a)
share of the card gap) needs either worker-side `tcx` access for
lazy copy-body materialization or a ~10-table body-serialization
format — multi-day refactor, determinism-risky; deferred. Bounded
deltas recorded: xcgu copies ~13MB/0.65s on regex-syntax, ~152MB/6.7s
on syn (43% of its user time); `*modgen*` copy define is the single
largest copy cost (1.38s serial front-half on syn's big CGU).

## narrowext ret-arg ireduce sink (29fced9)

`ret (ireduce)` on aarch64 materialized a `uxtb`/`uxth` for the ABI
narrow field even though the source register already holds clean
bits — `put_in_regs_vec` made the extension before `gen_return`'s
`skip_uext` could see the ireduce. New `put_in_regs_vec_ret` ISLE
extern substitutes a narrow ireduce's clean source at the return
boundary; the ireduce then has no remaining uses and never
materializes. `_ret_shift` now matches LLVM exactly (`lsr; ret`).
regex-syntax .text −1 insn (shape is rare there). Gated by
`PLIRON_NARROWEXT` bit 2 (the ret-arg oracle bit).

## Brotli residuals — measured, not fixed

Post-w3 profile of rust-compression: `HqZopfliBackwardReferences`
24.9%, `FindMatchLengthWithLimit` 17.8%, `EstimateBitCostsForLiterals`
12.1%. Per-function insn counts (pliron vs LLVM):

| fn | pliron | llvm | gap |
|---|---|---|---|
| `HqZopfliBackwardReferences` | 2301 | 1320 | spill traffic: 559 vs 291 stack refs, frames both ~8KB — regalloc pressure |
| `ZopfliCostModel::set_from_literal_costs` | 652 | 100 | ~50 per-iteration `b.hs` bounds checks LLVM hoists into one guard; LLVM const-folds `FastLog2(11+i)` i<245 into a table load |
| `FindMatchLengthWithLimitMin4` | 661 (952 w/ sc+body200) | 392 | rolled remainder + byte-assembly; unrolled head now matches LLVM |
| `EstimateBitCostsForLiterals` | 695 | 635 | +29 memops |
| `alloc_if<ZopfliNode>` (5 variants) | ~550 total | 0 | LLVM inlines; 2 non-loop callers → `PLIRON_INLINE_PAIR` (opt-in, ab235ae) reaches it; −2.6KB binary, runtime neutral — stays opt-in |

`PLIRON_INLINE_PAIR` went multiplicative in d73638b: the floor form
(`t.max(limit*2)`) never lifted a callee that had already earned the
single-site bonus. At k=2 it inlines pulldown-cmark's
`scalar_iterate_special_bytes` (callers=2, cost 5704 vs t=3840, LLVM
inlines it too): the symbol dies and that benchmark gains ~2-5%.
Held-out A/B `pair/pliron` (5 rounds): geomean 1.010 — NET NEGATIVE.
pulldown-cmark 0.980 and rust-json 0.975 win, but regex is 1.079
*slower* (DFA helpers the bonus inlines cost runtime, not just
.text) and rust-protobuf 1.020. Plus +3.9% regex-syntax `.text`.
Firmly opt-in; no promotion path at this formula. The residual would
need per-callsite specialization evidence (what LLVM actually pays
for), not a callers-count bonus.

Fresh 5-round held-out on HEAD: geomean **1.076 [1.060,1.095]**
(vs 1.054 on 11-10 — within machine drift; the table is updated).
`regex` reads 1.082 [1.069,1.188] vs its old 1.011 — wide CI,
interleaves cleanly on manual re-runs (~1.08), `PLIRON_BUNROLL=0`
makes no difference. Watch item, not clearly a regression.

`pulldown-cmark` (1.139): top self-time `escape_html_scalar` 17%
(already NEON-vectorized; 282 vs 149, the delta is bounds-check tails)
and `scalar_iterate_special_bytes` 11% — 5704 IR ops, LLVM inlines
it into `parse_line` (1144 total); we reject at cost 5704 > t=3840
and the chain inlines the 3716-op `parse_line` closure *into* it,
making it worse (8084). Same shape as `alloc_if`: LLVM pays for
specialization value our static cost model can't see.

Common residual theme for the remaining held-out gap: per-iteration
bounds-check tails (`b.hs` → cold panic blocks), regalloc spill
traffic in >2k-insn bodies, and specialization-value inlining.
