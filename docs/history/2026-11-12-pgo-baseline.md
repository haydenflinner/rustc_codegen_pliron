# 2026-11-12: PGO baseline on the held-out programs

`bench/heldout/run.py --pgo` (new): per program, each side profiles one
run of the benchmark and rebuilds with the profile — ours with
`PLIRON_PGO` / `pliron-prof merge` / `PLIRON_PGO_USE`, LLVM with
`-Cprofile-generate` / `llvm-profdata merge` / `-Cprofile-use`. 7
interleaved rounds, quiet machine (load ~3), backend 705dc5e + runner.

| | ratio | 95% interval |
|---|---|---|
| pliron / llvm | 1.089 | [1.081, 1.118] |
| pliron-aa / pliron (noise floor) | 0.995 | [0.974, 1.006] |
| **pliron-pgo / pliron** | **1.005** | [0.980, 1.032] |
| llvm-pgo / llvm | 0.922 | [0.904, 0.943] |
| pliron-pgo / llvm | 1.095 | [1.078, 1.130] |
| pliron-pgo / llvm-pgo | 1.188 | [1.168, 1.215] |

Over 6 of 8 programs (two fail under our profile, below). Our whole
profile loop (instrumented build + one run + profiled build) costs
2.59x a plain LLVM build.

Per program, pgo/pliron: blake3-scalar 0.991, pulldown-cmark 0.999,
regex 1.039*, rust-html-rewriter 0.944*, rust-json 1.049,
rust-protobuf 1.013. LLVM's own profile, llvm-pgo/llvm: about 0.98
(blake3), 0.85 (pulldown-cmark), 0.96 (regex), 0.83 (html-rewriter),
0.94 (json), 0.99 (protobuf).

So today our profile is matched (2,098 functions on pulldown-cmark,
9,633 on regex, none stale) and buys nothing, while LLVM gets 8% from
the same single run. The consumers that exist (cold layout, the
inliner's cold-site reject and warm bonus, branch expectations, loop
trip estimates, switch ordering) are not where the time is.

Failures under `PLIRON_PGO_USE`:

- rust-compression: build panics, `cranelift rejected ...
  brotli::enc::block_splitter::FindBlocks: should be implemented in
  ISLE: ineg.i32x4` — the profile sends a loop down a `loopvec` path
  that emits a vector `ineg` the aarch64 backend has no rule for.
- blind-sig: the profiled binary dies with SIGBUS (the instrumented one
  runs).

## Follow-up (same evening)

- rust-compression: fixed — aarch64 had no rule for `ineg` on 64-bit
  integer vectors (`slp` builds `ineg.i32x2`); added next to the other
  `ty_vec64_int` rules. With it, pgo/pliron 1.021 [0.969, 1.056],
  llvm-pgo/llvm 0.965 on that program.
- blind-sig: still open. `fnbisect` names
  `rsa::internals::blind::<ThreadRng, RsaPublicKey>` (crash in
  `SmallVec<[u64; 4]>::extend` called from it). `PLIRON_SROA=0` hides
  it; `PLIRON_PGO_LOOP=0`, `MEMCPYOPT=0`, `SLOTPROM=0`, `EGRAPH=0` do
  not. `optbisect` lands on `egraph2` (unit 421), and
  `PLIRON_DIFFCHECK=step` reports nothing for the function, so the
  wrong step is probably on the pliron-IR side (`sroa`) with the CLIF
  step only exposing it. Repro: `PLIRON_PGO_USE=target/heldout/pgo/
  blind-sig/pliron.prof` on the blind-sig benchmark crate.

## Night: both failures fixed, two consumers built, neither pays yet

- blind-sig crash: not a profile bug. `skip_jmp` (drop the jump into an
  aligned loop header that is emitted next, fall through the pad) did
  not check that the header still had code; `PLIRON_BJALIAS` removes
  bare-jump headers, so execution fell into the following block. The
  profile only changed block order enough to hit it (4a2221b).
  Ablation knobs added on the way: `PLIRON_PGO_{EXPECT,INLINE,COLD}=0`.
- All eight programs now run under our profile. With
  `PLIRON_PGO_HOT=13`: pliron-pgo/pliron 1.000 [0.973, 1.030],
  llvm-pgo/llvm 0.908 [0.904, 0.970], pliron-pgo/llvm-pgo 1.194, loop
  cost 2.52x a plain LLVM build.

What LLVM's profile does on pulldown-cmark (per-function sample diff of
its two builds): hot callees disappear into their callers
(`parse_line`, `parse_block`, `escape_html_scalar`, `Tree::append`,
`item_to_event`, `memchr`), text +8%, no `.cold` symbols.

Consumers built, both opt-in:

- `PLIRON_PGO_HOT=<k>`: a call site at or above the profile's hot
  count (blocks covering 99% of all counts) gets `k` x the inline limit
  and skips `loopy`/`fanin`/growth. Scaled counts no longer round a
  block that ran down to 0.
- `PLIRON_PGO_FREQ=1`: block counts ride `Layout::prof_count` into the
  lowering order, and `placement::analyze` takes measured edge
  probabilities where every successor has a count — so block placement
  and regalloc2's spill weights see real frequencies instead of the
  8:1 back-edge / hint / cold guesses.

Measured (15-21 interleaved rounds, ratios to plain LLVM):

| | pulldown-cmark | rust-html-rewriter |
|---|---|---|
| pliron | 1.12-1.18 | 1.14 |
| + profile | 1.13-1.20 | 1.09 |
| + profile + FREQ | 1.12 | 1.10 |
| + profile + FREQ + HOT=13 | 1.11 | 1.12 |
| LLVM + profile | 0.84-0.87 | 0.84-0.85 |
| LLVM + profile, hot-callsite bonus off | 0.86 | 0.87 (binary identical) |

Both knobs change the code and neither moves the time beyond noise.
LLVM keeps nearly all of its gain with the hot-call-site threshold set
back to the default, so the threshold bonus is not what its profile
buys either. Open: what is. Leads not yet followed: 1,970 of our
rejected call sites on pulldown-cmark are "no body in this CGU"
(LLVM's release builds have thin-local LTO across the crate's units;
our cross-unit copies skip large callees), and one unit for everything
is not the answer (pulldown-cmark at `-Ccodegen-units=1` is 1.29x even
without a profile — itself worth a look).
