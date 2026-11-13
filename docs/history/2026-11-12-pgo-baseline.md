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

## Late night: ab.py, what LLVM's profile buys, first real gain

`bench/heldout/ab.py PROG SIDE...` (new): cached builds, interleaved
runs under `/usr/bin/time -l`, and per side the timed region plus the
process's retired instructions, cycles and IPC. Instructions repeat to
~0.01%, so one run answers what wall-clock needs intervals for; LLVM
sides take extra RUSTFLAGS (`llvm-pgo:-Cllvm-args=...`), so LLVM's own
passes can be ablated the same way. `--prof` adds inclusive and self
per-function cycle deltas.

pulldown-cmark, whole process, ratios to plain LLVM:

| build | instructions | cycles |
|---|---|---|
| LLVM + profile | 0.905 | 0.86-0.91 |
| ... `-disable-block-placement` | 0.924 | 0.98-1.03 |
| ... `-disable-memop-opt` | 0.927 | 0.914 |
| ... `-disable-icp` | 0.908 | 0.884 |
| ... hot-callsite thresholds at default | 0.911 | 0.895 |
| LLVM, `-inline-threshold=3000` (no profile) | 0.943 | 0.956 |
| LLVM, `-disable-block-placement` (no profile) | 1.007 | 1.03 |
| pliron | 1.196 | 1.07-1.10 |
| pliron, `PLIRON_PLACE=0` | 1.20 | +0.4% on pliron |

So LLVM's profile gain is, in order: profile-guided block placement
(~12% of cycles; 3% without a profile), memcpy size specialization
(~6%), inlining and the rest. We execute 5-20% more instructions than
LLVM on every program (mostly register moves: +7 points of the
instruction mix on shared hot functions), at higher IPC.

Built since:

- `PLIRON_MEMFAST_SMALL=1|hot`: runtime-size copies below 16 bytes are
  inline (8/4-byte overlapping pairs, 3 byte moves for 1..3). At `=1`:
  pulldown-cmark instructions -2.5%, cycles -3.5%; rust-html-rewriter
  -0.7% / -1.6%.
- `PLIRON_PGO_XCGU=<size>`: a callee whose profiled entry count
  reaches the hot count is copyable across codegen units up to
  `<size>` MIR statements (default limit 50). 1,970 of the inliner's
  rejections on pulldown-cmark were "no body in this CGU".

`run.py --pgo --pgo-env PLIRON_PGO_HOT=13 --pgo-env
PLIRON_PGO_XCGU=4000 --pgo-env PLIRON_XCGU_CAP=100000`, 7 rounds:

| | ratio | interval |
|---|---|---|
| pliron-pgo / pliron | 0.975 | [0.965, 0.987] |
| pliron-pgo / llvm | 1.061 | [1.052, 1.083] |
| llvm-pgo / llvm | 0.930 | [0.915, 0.950] |
| pliron-pgo / llvm-pgo | 1.141 | [1.127, 1.167] |

pulldown-cmark 0.936, rust-html-rewriter 0.958, rust-json 0.960;
rust-protobuf 1.021 (worse). Loop cost 2.37x a plain LLVM build. On
pulldown-cmark alone the instruction count falls 81.1M -> 74.8M.

Still open: our block placement. `PLIRON_PLACE=0` costs ~1% and real
counts (`PLIRON_PGO_FREQ=1`: 49% of blocks, 68% of branches measured at
layout time) change the code but not the time.

## 2026-11-13 early morning: the bundle as default

Since the last table: `PLIRON_PGO_FREQ` became a bit set (placement
only; spill weights from counts measured worse), counts for
pass-created blocks are inferred from the CFG before the backend sees
them (46% -> 53% of blocks, 74% -> 81% of branches measured on
pulldown-cmark), a measured count overrides a static cold mark, and
only critical-edge blocks are looked through. The bundle is on whenever
`PLIRON_PGO_USE` is set (docs/pgo.md). `harness/pgofuzz.py`: 300 seeds
through generate/run/use, all pass. Gate green.

`bench/heldout/run.py --pgo`, defaults, 7 rounds:

| | ratio | interval |
|---|---|---|
| pliron-pgo / pliron | 0.960 | [0.946, 0.977] |
| pliron-pgo / llvm | 1.043 | [1.013, 1.066] |
| pliron / llvm | 1.086 | [1.054, 1.111] |
| llvm-pgo / llvm | 0.927 | [0.907, 0.978] |
| pliron-pgo / llvm-pgo | 1.126 | [1.048, 1.144] |

pgo/pliron per program: pulldown-cmark 0.913, rust-json 0.934,
rust-protobuf 0.936, rust-html-rewriter 0.949, blake3-scalar 0.976,
rust-compression 0.979, regex 0.994, blind-sig 1.004. Loop cost 2.52x
a plain LLVM build.

Layout, measured and set aside: `PLIRON_LAYOUT_STATS` /
`harness/layout_score.py` score the emitted order under the measured
frequencies (pulldown-cmark: 0.37 transfers per block against a floor
of 0.20). Placement variants (real counts on or off, emission-time
loop rotation) change the code and the score slightly and the run time
not measurably (+-1.5% over 9 rounds on three programs). LLVM loses
5-20% of cycles with its placement disabled; whatever it gains there,
our chain placement does not reproduce it with the counts we carry.

Per-consumer notes from `ab.py`: on regex the profile-guided inliner
costs ~2.5% instructions (`PLIRON_PGO_INLINE=0` is its best setting);
on rust-json dropping the never-run-site rejection
(`PLIRON_PGO_COLDSITE=0`) saves 3% instructions but it is mixed
elsewhere; the hot multiplier wants 13 on rust-json and
rust-html-rewriter and 0-3 on rust-protobuf.

## 2026-11-13: loop cost, and the per-program tuner

- Instrumented builds are unoptimized by default (`PLIRON_PGO_OPT=1`
  restores): same profile (8,196 functions matched either way on
  rust-html-rewriter, same speedup), build 9 s -> 6 s, profiling run 2x
  slower. Loop cost 2.52x -> 2.18x a plain LLVM build.
- `bench/heldout/tune.py PROG`: greedy search over 13 profile and
  codegen knobs on top of the bundle, one cached build per candidate,
  keep a setting only if the timed region's median improves >1% and its
  fastest run >0.5%; a longer final run against LLVM decides.

Final runs (15 rounds, median, ratio to plain LLVM):

| program | pliron | bundle | tuned | LLVM+profile | tuned setting |
|---|---|---|---|---|---|
| rust-protobuf | 0.98 | 1.01 | **0.90** | 0.86 | `MEMFAST_SMALL=1 PGO_COLDSITE=0 PGO_HOT=0` |
| rust-json | 1.04 | **0.96** | (1.01) | 0.92 | bundle (the search's pick lost the final run) |
| pulldown-cmark | 1.15 | 1.05 | **1.01** | 0.86 | `PGO_COLDSITE=0 PLACE_ROT=1` |
| blind-sig | 1.02 | 1.04 | — | 1.13 | bundle |
| rust-html-rewriter | 1.14 | 1.04 | — | 0.83 | bundle |
| blake3-scalar | 1.04 | 1.05 | — | 0.97 | bundle |
| rust-compression | 1.14 | 1.11 | **1.06** | 0.98 | `ELABLOOPS=2 INLINE=640 RA2_POSTOPT=15` |
| regex | 1.20 | 1.18 | **1.11** | 1.03 | `PGO_INLINE=0 PGO_LOOP=0` |

Best of bundle/tuned per program: geomean ~1.02x plain LLVM (plain
pliron 1.09x), two programs ahead of LLVM. What the picks say about the
consumers: on regex both the profile-guided inliner and the
profile-driven loop passes are net harmful; the opt-in knobs from the
relax work (`ELABLOOPS=2`, `RA2_POSTOPT`) pay on rust-compression;
rust-protobuf wants small inline copies everywhere and no hot-site
bonus.

## 2026-11-13: layout closed out; opt-in knobs without a profile

**Layout.** `PLIRON_PLACE_OPT=1` (new, opt-in): local search on the
finished order — move a block's likeliest successor (with its
fallthrough run) behind it when the expected taken-transfer mass
drops. It moves the model score from 0.384 to 0.382 transfers per block
on rust-html-rewriter and nothing measurable in run time (11 rounds,
two programs). The chain order is already a local optimum of the
model; what separates it from the floor is joins and loop shapes, which
only a transformation (tail duplication) changes, not an order. LLVM's
"placement off" baseline is source order with arbitrary branch
polarity; ours (`PLIRON_PLACE=0`: RPO with cold blocks sunk) is within
1% of our best. Layout is not where our gap is.

**LLVM's profile on rust-html-rewriter** (0.820 of plain): placement
off 0.895, memop off 0.840, ICP off 0.844, hot-callsite bonus off
0.826.

**Opt-in knobs without a profile**, all eight programs, 9 rounds,
geomean time / instructions against defaults:

| knob | time | instructions |
|---|---|---|
| `PLIRON_MEMFAST_SMALL=1` | **0.967** | 0.992 |
| `PLIRON_RA2_SRCVREG=1` | 0.990 | 1.000 |
| `PLIRON_RA2_POSTOPT=15` | 0.991 | 1.000 |
| `PLIRON_PLACE_ROT=1` | 0.992 | 1.000 |
| `PLIRON_INLINE=640` | 0.997 | 1.013 |
| `PLIRON_ELABLOOPS=2` | 1.007 | 1.006 |
| `PLIRON_XCGU_SIZE=200` | 1.009 | 1.002 |

`MEMFAST_SMALL=1` wins on six programs (regex 0.92, rust-protobuf
0.93, pulldown-cmark 0.94, rust-json 0.95) and costs code size:
instructions in the binary +1.7% (pulldown-cmark, rust-html-rewriter),
+1.9% (rust-compression), +6.5% (regex). Left opt-in for that reason
(SIZ is the card's weakest axis); under a profile it is on at hot
sites only. The rest are inside the noise of a 9-round run.

Candidate bundle changes (`PGO_LOOP=0`, `PGO_WARM=0`, `PLACE_ROT=1`),
eight programs, 9 rounds: 0.977 / 1.008 / 0.993 geomean with one noisy
program driving the first — no change made.

## Validation at the end of the night (main at this commit)

- gate: passed (101 jobs).
- fuzz at defaults, seeds 34000..36000: the first 1,300 clean before
  the disk filled (cached A/B builds; `ab.py` now keeps 24 sides), the
  remaining 700 rerun: 700 pass.
- `harness/pgofuzz.py`: 400 seeds through generate/run/use, all pass.
- `harness/crates.py --tier 1`: 20 crates, 0 pliron-only problems.

## 2026-11-13 (later): the layout model's frequencies were wrong

`harness/layout_score.py` reported 268M block executions for
pulldown-cmark, 96% of them in `scan_containers` — which samples at 2%
of the time, and the raw profile's largest block count is 0.85M (40M
in total). The counts are fine; `placement::analyze` turns them into
successor *shares* and propagates, and a loop whose latch never left
in the profiled run gets the 1e-4 floor as its exit probability: ten
thousand modelled iterations. `PLIRON_PLACE_PIN=1` (opt-in) pins every
counted block's frequency to count/entry and rescales count-less
blocks by their nearest counted dominator. With it the model totals
38.5M blocks (profile: 40.5M) and the score is honest: 0.498 transfers
per block against a floor of 0.179 (pulldown-cmark), 0.420 / 0.201
(rust-html-rewriter). Use it whenever scoring a layout.

Run time with it (11 rounds): pulldown-cmark 1.009, rust-html-rewriter
1.028 (noise), with `PLACE_ROT`/`PLACE_OPT` on top the same. So the
earlier conclusion stands on a sound model now: on this CPU the order
of blocks is not where the time is; retired instructions do not move
(1.001). Also ruled out: the inliner's count scaling (new `pgoscale`
remark; only 14 sites, all tiny, have site > callee entry).

**Loop cost: sharing the front end does not help.** Phase times
(`--jobs 2`, warm): the instrumented build costs 0.8-1.0x a plain
build even unoptimized, the profiled build 1.0-1.3x. Tried: both
builds in one target directory with `-Cincremental` on every crate and
`-Zdisable-incr-comp-backend-caching`, so the profiled build reuses the
front end's query results and only regenerates code. Profiled build
6.8 s -> 6.8 s (rust-html-rewriter), 3.0 -> 2.3 (pulldown-cmark),
4.0 -> 4.5 (regex), and the instrumented build pays ~10% for writing
the cache. The backend is the cost, not the front end. (Fixed on the
way: an instrumented unit with no function bodies emitted a
constructor for a header it never defined — a link error under
incremental builds.)

**`PLIRON_MEMFAST_SMALL=loop`** (new, opt-in, no profile needed):
inline small copies only in blocks on a CFG cycle of the function.
Eight programs, 11 rounds, against defaults:

| mode | time | instructions run | instructions in binary (per program) |
|---|---|---|---|
| `=1` | 0.971 | 0.992 | +6.5 +1.7 +1.7 +0.6 +0.1 +1.9 +0.2 +1.4 % |
| `=loop` | 0.980 | 0.997 | +2.0 +1.0 +0.1 +0.1 +0.0 +0.8 +0.1 +0.1 % |

(program order: regex, pulldown-cmark, rust-html-rewriter, rust-json,
rust-protobuf, rust-compression, blake3-scalar, blind-sig.) Two thirds
of the speed for a quarter of the size; rust-protobuf 0.91 and regex
0.97 carry it. The best candidate from this work for a default flip.

**regex `Fsm::next_state`** (+5.8% of LLVM's cycles, self). One shape
reproduced in isolation (`InstPtrs::next` over `read_varu32`): the
rotated loop header retests `len == 0` although both edges into it
already decided that. `PLIRON_EDGEKNOWN=1` (new pass `edgeknown`,
opt-in) folds a `brif` on a block parameter when every incoming edge
settles it (constant, the predecessor's own condition, the value it
compares with zero, or `isub`/`bxor` of the values it compares): repro
171 -> 165 instructions. On the eight programs it changes nothing
measurable (instructions 1.001), nor does `PLIRON_INBOUNDSNN=1`
(non-null from in-bounds offsets, instructions 1.002) or both. The
hot test in the real function is not this one; still open.

**rust-html-rewriter: a lowercase loop LLVM vectorizes and we call.**
`<[u8]>::to_ascii_lowercase` is `iter().map().collect()`; in isolation
we vectorize it too. In the program the `Map::fold` instance lives in
another CGU and stays a call (1.6% of cycles in a scalar loop that
stores the length through a pointer every iteration). The cross-CGU
copier refuses it twice over: `slice::Iter::fold` is 73 MIR statements
(limit 50), and the chain down to `SetLenOnDrop::drop` is nine mono
items deep (`PLIRON_XCGU_DEPTH` default 8). Eight programs, 9 rounds:

| setting | time | instructions | per program instructions |
|---|---|---|---|
| `XCGU_DEPTH=16` | 1.000 | 1.001 | html-rewriter 0.991 |
| `+ XCGU_SIZE=80` | 1.010 | 1.005 | html-rewriter 0.989, protobuf 1.028 |
| `+ XCGU_SIZE=120` | 1.004 | 0.999 | regex 0.977, html-rewriter 0.986, protobuf 1.022 |

Program-dependent, so not a default: more bodies help where a
vectorizable loop is behind the call and hurt where the inliner then
spends its budget badly (rust-protobuf). Added to `tune.py`'s search
space instead.

Gate: passed on a01e55a (101 jobs). Fuzz: 600 seeds with
`MEMFAST_SMALL=loop`, 400 with `EDGEKNOWN=1`, all pass.

**The tuner's picks were not stable.** A second full run (extended
space) chose differently from the first: rust-protobuf's 0.90 setting
and pulldown-cmark's 1.01 setting were not found again (both kept the
bundle), regex kept only `PGO_INLINE=0` in common, rust-compression
only `ELABLOOPS=2`. Read the first table as an upper bound with
selection bias, not a result: what repeated is regex 1.18-1.20 -> 1.10-1.11
and rust-compression 1.09-1.11 -> 1.05-1.06 of LLVM. `tune.py` now
keeps a candidate only if it wins twice in independent measurements.

Third tuner run, with the confirmation rule (9 candidates won once and
lost the confirmation, across the eight programs):

| program | bundle | tuned | LLVM+profile | setting |
|---|---|---|---|---|
| rust-protobuf | 0.96 | **0.88** | 0.88 | `INLINE=640 PGO_FREQ=0 PGO_HOT=0 PGO_XCGU=0` |
| rust-html-rewriter | 1.09 | **1.05** | 0.84 | `XCGU_DEPTH=16` |
| regex | 1.19 | **1.12** | 0.95 | (`PGO_INLINE=0` again among them) |
| rust-compression | 1.11 | 1.10 | 0.97 | `INLINE=640` |
| others | — | bundle | | |

Across three runs the repeatable signals are: rust-protobuf without the
hot-site bonus (0.90, —, 0.88: level with LLVM's profiled build), regex
without the profile-guided inliner (1.11, 1.10, 1.12). The bundle's
`PGO_HOT=13` is right for rust-json and wrong for rust-protobuf; a
profile-derived rule for when the bonus pays is the next thing to find.

## `PLIRON_PGO_HEAVY`: why the profile made regex slower

regex with the profile-guided inliner off was the one tuner signal that
repeated (three runs). Split by rule (13 rounds, against the bundle):
`PGO_HOT=0` 1.02, `PGO_WARM=0` 1.01, both off **0.936** with 3.3% fewer
instructions — either bonus alone is enough to make the same bad
decision. Diffing the inliner's score rejections with and without the
bonuses names it: `Fsm::follow_epsilons` (regex's hottest function, a
loop nest, cost 482) inlined into `next_state` (1,383 ops) and
`exec_byte`. LLVM keeps it out of line.

The rule: a bonus buys back call overhead, so a callee that runs many
blocks per call has nothing to gain. `PLIRON_PGO_HEAVY=<k>`: a callee
whose profile shows more than `k` block executions per entry gets
neither bonus. Eight programs, 11 rounds, against the bundle:

| k | time | instructions | regex time / instr |
|---|---|---|---|
| 16 | 0.988 | 1.000 | 0.93 / 0.965 |
| 64 | 0.993 | 0.997 | 0.94 / 0.965 |
| 256 | 0.985 | 0.997 | 0.94 / 0.965 |

The other seven programs move by at most 0.5% in instructions at 256.
In the bundle at 256; `harness/pgofuzz.py` 200 seeds pass.
