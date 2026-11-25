# Register allocation: the push to state of the art

Shared log of the register-allocation push (started 2026-11-17). Goal:
dynamic regalloc overhead (spill stores, reloads, register moves,
callee-save saves and restores) at or below LLVM's on all eight held-out
programs, with no other loss. Everyone working on regalloc2 heuristics,
moves/coalescing/remat or the analyses feeding them records here what a
knob does and where it stands.

## Five workstreams (2026-11-18)

Knob-tweaking on spill weights is largely exhausted (ratios still
1.12–1.46). Active tracks — details in
`docs/history/2026-11-18-ra-five-start.md`:

| # | track | knob / entry | status |
|---|---|---|---|
| 1 | Decision stats | `PLIRON_RA_STATS=1`, `ra2stats:` | landed |
| 2 | RA vs inlining gap | `rastat.py dyn` `both` / `inline_only` | landed |
| 3 | Register file facts | `PLIRON_X18`, `Target::int_regs_alloc` | landed (Linux default) |
| 4 | Ion use-cluster split | `PLIRON_RA2_USESPLIT` (GAP=128, single-block) | **on** since 2026-11-18 |
| 5 | Greedy + SplitKit | `PLIRON_RA_ALGO=greedy` (+ `GREEDY_COLOR=1`) | all-spill clean; COLOR=1 single-block non-param (args OK) clean (10k rafuzz) |
| 6 | Proof engine | `tools/raproof` + `regalloc2::prove` | MVP + opt-in `PLIRON_RA_PROOFCOLOR=1` (greedy) |
| 7 | Split budget | `PLIRON_RA2_MAXSPLITS` (upstream 2) | **4** since 2026-11-20 (`history/2026-11-20-ivsr.md`) |

USESPLIT already flipped. Default allocator remains ion until greedy
wins held-out non-nop callgrind and stays rafuzz/checker clean.

### Proof engine (`tools/raproof`)

Register allocation as an assembly-line certificate: the largest set of
same-class values live together is a *proof* you need that many homes
(registers or spill slots); a colouring with K colours is a proof that
K registers suffice with no spill. `vendor/regalloc2/src/prove.rs`
builds the interference graph from liveness, finds a max-clique lower
bound (exact Bron–Kerbosch up to `MAX_EXACT=48` vertices per class,
greedy beyond), and binary-searches the chromatic number when the class
is small enough. `tools/raproof` runs that against ion / greedy / fast
on fuzz seeds and prints a verdict:

```
cargo run --release -p raproof --manifest-path tools/raproof/Cargo.toml -- --count 50
cargo run --release --manifest-path tools/raproof/Cargo.toml -- --algo greedy --replay 22
```

- `heuristic_matches_optimum` — preg footprint equals chromatic number.
- `heuristic_above_optimum` — footprint > chromatic: a colouring uses
  fewer registers; the certificate names the clique and the K.
- `heuristic_spills_under_*` — footprint below the bound because the
  allocator spilled (expected; clique is simultaneous live values).
- `gap_above_clique_unresolved` — class too big for exact colouring;
  only the clique lower bound is known.

Every bound is re-checked (`check_bound`) before it is printed.
`--prove-fewer` runs `try_fewer`: search for a colouring with K <
heuristic footprint (budget via `--budget`, default 2e6). A hit prints
the assignment; `tight=true` means K−1 was also ruled out. That is the
"prove one fewer is impossible, or hand me the counterexample" loop.

Trade more compute: raise `MAX_EXACT` / `--budget`, feed profile-hot
functions, or score colourings by spill/move cost. First signal: ion
leaves register footprint on the table on many small fuzz funcs
(`docs/history/2026-11-18-ra-raproof.md`).

**Learning loop** (`harness/raproof_learn.py`): rebuilds only
`tools/raproof`, runs a fixed seed window, appends
`target/raproof/learn.jsonl` with
`fitness = match / (match + above_opt + 1)`. Optional `--rafuzz-quick`
for checker green under the same env.

**`PLIRON_RA_PROOFCOLOR=1`** (opt-in, requires `PLIRON_RA_GREEDY_COLOR=1`):
when a regclass has ≤ `MAX_EXACT` vertices and a checked chromatic
colouring exists, greedy rematches single-block non-param homes if
their footprint exceeds `upper_k`, and emit prefers that colouring's
K pregs for reload/spill temps. Fixed 40-seed window: COLOR alone
fitness 0.20 → COLOR+PROOFCOLOR 0.30; rafuzz 2k green. Do not flip
default — remaining `above_opt` is live-K + temp (proof graph ignores
edit temps). Log: `docs/history/2026-11-18-ra-raproof-learn.md`.

The allocator is regalloc2 0.15.2, vendored with PLIRON patches
(`vendor/regalloc2/PLIRON_PATCH.md`), behind Cranelift's machinst glue
(`vendor/cranelift-codegen/src/machinst`: `compile.rs` runs it,
`vcode.rs` holds the post-allocation rewrites: remat through
`Output::edit_vregs`, reload PRE, reload CSE, slot pairing at emission).

## Safety net: fuzzing (`harness/rafuzz.py`)

Every regalloc change runs under two legs, each under the knob
combinations below.

**ra2 leg** (`tools/rafuzz`, its own cargo workspace): regalloc2's own
random-function generator (`vendor/regalloc2/src/fuzzing/func.rs`) with
the options of upstream's `ion` fuzz target, plus

- answers for the hooks Cranelift implements (block frequencies, cold
  blocks, remat vregs, unmerged block params, slot naming), so
  `PLIRON_RA2_FREQ`, `_COLDSPLIT`, the remat weight and the
  `no_param_merge` path run at all (upstream's generator leaves them at
  the trait defaults; one seed in five still does, for upstream's exact
  shape);
- one seed in four with 6..16 registers per class instead of 32 (heavy
  splitting and spilling);
- the symbolic checker, now also checking `edit_vregs` labels (a labelled
  move must move the value its label names; Cranelift rematerializes from
  the label, so a wrong one is a silent miscompile);
- one seed in eight also allocated with a fresh `Ctx` and compared with
  the reused one (stale state across functions);
- regalloc2's internal `debug_assert!`s and overflow checks (the tool's
  release profile turns them on).

About 450 seeds/s per process on a quiet machine. A seed is a 64-bit
number; the function is a pure function of it.

**clif leg**: `clif-check codegen <corpus>` over the recorded corpora
(`target/corpus/{std,smoke,clcg,html}`, sampled with `--every N
--offset K`) with `PLIRON_RA_CHECK=1` (the checker on regalloc2's output)
and `PLIRON_RELOADPRE_CHECK=1` (the checker again on the output of
Cranelift's post-allocation rewrites). Real functions, the backend's
aarch64 flags and machine environment, and the Cranelift-side knobs. std
(4,925 functions) takes about 20 s.

What neither leg sees: the emission-time rewrites that change
instructions rather than the allocation (`PLIRON_REMAT` turning a labelled
reload into a constant, `PLIRON_REMAT_STORE` dropping stores into slots
only remat reads, `PLIRON_EDITPAIR` fusing spill pairs). The label check
covers the input those rely on; the end-to-end check for them is the
test suite and `harness/crates.py`.

### Running it

```
harness/rafuzz.py quick                 # ~15 s: fixed seeds, 4 combos, smoke corpus
harness/rafuzz.py knobs                 # the knob table; lists knobs the source reads that it lacks
nohup harness/rafuzz.py loop --jobs 4 > /dev/null 2>&1 &
tail -f target/rafuzz/loop.log
cat target/rafuzz/failures.tsv          # one line per new failure signature, with a replay command
```

`loop` runs the defaults, everything on (`allon`), each knob alone at
each of its values, then random combinations forever; one run in four
is a CLIF run, rotating corpora and sample offsets. Seeds advance across
restarts (`target/rafuzz/state.json`), and the loop runs private copies
of `rafuzz` and `clif-check` (`target/rafuzz/bin/`), so rebuilding the
tree does not change a running loop or its replay commands.

Replays:

```
env PLIRON_RA2_X=1 target/rafuzz/bin/rafuzz-<hash> --replay SEED --print
env PLIRON_RA_CHECK=1 PLIRON_RELOADPRE_CHECK=1 PLIRON_RA2_X=1 clif-check vcode <corpus>/<file>.clif
```

`harness/quick.py` runs `rafuzz.py quick` (tier 0) when a file under
`vendor/regalloc2/`, `vendor/cranelift-codegen/src/machinst/`,
`tools/rafuzz/` or `harness/rafuzz.py` changed; `harness/gate.py` runs it
(job `rafuzz`) when any of those changed since `main`.

**A new knob**: add a row to `KNOBS` in `harness/rafuzz.py` (name,
default, values to try, which leg reads it, its value in `allon`).
`harness/rafuzz.py knobs` exits nonzero while the regalloc2 or machinst
sources read a `PLIRON_RA*` name the table lacks. A knob outside the
`PLIRON_RA*` names (like `PLIRON_RELOADPRE`) has to be added by hand.

**A fuzz bug**: minimize (the generator's byte budget is log-uniform, so a
smaller failing seed usually exists in the same run; `--print` shows the
function), fix it, add the seed to `REGRESS` in `harness/rafuzz.py` (the
quick leg replays those under every quick combination) and, for a bug
reachable from Cranelift, a unit test next to the fix. Record it in the
fuzz log below.

## Metric (`harness/rastat.py`)

One classifier (aarch64) for both compilers and both modes:

| class | what |
|---|---|
| `st` | store addressed off sp/x29 outside the prologue (spills, and locals kept in memory) |
| `ld` | load addressed off sp/x29 outside the epilogue's restores |
| `mov` | register-to-register copy within one bank (`mov xA, xB`, `fmov dA, dB`, `mov vA.16b, vB.16b`) |
| `csr` | callee-saved (x19-x28, d8-d15) saves (a store of one no earlier instruction wrote) and the epilogue's restores (the loads in the run before `ret` or a tail branch) |
| `frame` | frame record and sp moves (reported, not overhead) |
| `slot` | static only: sp/fp accesses to explicit stack slots and incoming arguments, kept apart from spill-slot `st`/`ld` |

**overhead = st + ld + mov + csr.**

- `rastat.py dyn [--only P] [--env K=V] [--tag T] [--save F]`: exact
  executed counts. Both sides build each held-out program for
  aarch64-unknown-linux-musl (`irdiff.py`'s builds), run once under
  callgrind `--dump-instr=yes` (docker); every executed address is
  classified from llvm-objdump. Per program ours/LLVM per class, then
  the functions with the most excess overhead. The dynamic `st`/`ld`
  include locals (LLVM's side cannot be split); it is the same rule on
  both sides. About 5 minutes a program on a loaded machine; delete
  `target/heldout/ir-pliron-<tag>` afterwards.
- `rastat.py static [--corpus std|html|smoke|clcg] [--env K=V] [--save F]`:
  the fast loop, no docker. `clif-check codegen --vcode` (pipeline +
  Cranelift with the backend's flags), the same classes per function,
  each instruction weighted 8^min(loop depth, 3). std 20 s, html (15,348
  functions) 12 s.
- `rastat.py cmp A.json B.json`: two saved runs, totals and the functions
  that moved most.

`target/corpus/html` is html-rewriter's corpus (15,348 functions,
recorded from the held-out build); copy it from another checkout or
record it with `PLIRON_CORPUS`.

### Move kinds

Register moves are the largest excess class in five of the eight
programs (baseline below), so `rastat.py dyn` splits every executed
`mov` by its context in the disassembly (`move_kinds`):

| kind | rule |
|---|---|
| `callarg` | into x0-x7/v0-v7, a `bl`/`blr` follows within 8 instructions of the same block |
| `callret` | out of x0-x7/v0-v7 right after a call |
| `ret` | into x0-x7/v0-v7 with only the epilogue (restores, sp moves) before `ret` or a tail branch |
| `edge` | in a run of moves that ends in a `b`, falls into a branch target, or starts at one: regalloc2's block-parameter and edge moves |
| `inblock` | the rest (fixed-register constraints, splits inside a block) |

Block starts are branch targets and the instructions after branches and
calls, so a run of moves never crosses one. The rules are heuristics on
final code: an edge move that regalloc2 put mid-block (a split) reads as
`inblock`, and LLVM's own copies are classified by the same rules.
`rastat.py selftest` checks them on a hand-written function.

First table (2026-11-19, from the 2026-11-18 night builds and callgrind
outputs already on disk: `ir-llvm` against `ir-pliron-night`, coverage
1.000 on both sides; before `PLIRON_SIBCALL`; per-function numbers in
`target/rastat/dyn-night-kinds.json`). Executed moves, LLVM / ours,
millions, and our excess edge moves as a share of the program's whole
instruction gap:

| program | gap | edge | callarg | callret | ret | inblock | edge excess / gap |
|---|---|---|---|---|---|---|---|
| rust-compression | 3.03M | 0.04 / 1.11 | 0.04 / 0.06 | 0.06 / 0.01 | 0.03 / 0.00 | 0.43 / 0.28 | 35% |
| regex | 3.88M | 0.36 / 1.07 | 0.09 / 0.15 | 0.01 / 0.01 | 0.01 / 0.01 | 0.81 / 0.55 | 18% |
| pulldown-cmark | 5.45M | 0.39 / 1.01 | 0.54 / 0.90 | 0.10 / 0.13 | 0.13 / 0.06 | 0.67 / 1.21 | 11% |
| rust-protobuf | 3.99M | 0.21 / 0.65 | 1.70 / 1.69 | 0.09 / 0.06 | 0.18 / 0.08 | 0.88 / 1.02 | 11% |
| rust-html-rewriter | 14.29M | 0.38 / 1.62 | 1.31 / 2.04 | 0.31 / 0.31 | 0.29 / 0.17 | 1.85 / 2.08 | 9% |
| blind-sig | 2.92M | 0.16 / 0.40 | 0.05 / 0.10 | 0.01 / 0.01 | 0.00 / 0.00 | 0.23 / 0.11 | 8% |
| rust-json | 5.13M | 0.33 / 0.42 | 0.28 / 0.53 | 0.01 / 0.01 | 0.03 / 0.02 | 0.39 / 0.55 | 2% |
| blake3-scalar | 2.42M | 0.00 / 0.02 | 0.12 / 0.16 | 0.00 / 0.00 | 0.01 / 0.00 | 0.99 / 0.41 | 1% |

Edge moves are the largest excess kind on six programs (compression
runs 27x LLVM's), `callarg` second (html-rewriter +0.73M, pulldown
+0.36M, json +0.25M). Bringing edge moves to LLVM's count alone would
remove 2.5% of compression's executed instructions, 1.4% of regex's,
1.2% of html-rewriter's and pulldown-cmark's. Where they come from:
a value held in different registers on the two sides of an edge, which
ion's bundle merging leaves when it splits a range (the Potts / flow
framing in `history/2026-11-19-ra-flow.md`).

### Judging a regalloc change against placement

A regalloc change shifts code, which moves loop heads past 16- and 64-byte
lines: the emitter's loop pads (`nop`s executed on fall-through entries)
change, and so do cycles. Placement alone moves held-out exact counts by
up to 1.9% (regex; pulldown-cmark ±0.5%, html-rewriter -0.25..+0.39%),
all of it `nop`s, and the `varint` kernel by 10% instructions per op.

    harness/placement.py heldout pulldown-cmark,rust-html-rewriter --cand ra:PLIRON_RA2_X=1 --copies 4 --no-cycles
    harness/placement.py kernels varint --cand ra:PLIRON_RA2_X=1 --copies 4

The held-out table splits the candidate's delta into executed `nop`s and
real instructions by class (load, store, other, branches) and charges
both to ELF symbols. The real (non-`nop`) delta is the same in every
copy, so one copy is enough for it: `--copies 1 --no-cycles` costs two
builds and two callgrind runs per program. The `ld`/`st` overhead
`rastat.py dyn` reports does not count `nop`s, so it is placement-free
already; a total-count claim is not. Timings on this machine at load
30-100 are noise; kernels use retired instructions and cycles per op at
a fixed iteration count. Details:
`history/2026-11-17-native-placement.md`.

**Function tables before 2026-11-17 evening.** Callgrind's `fn=` context
is not the function that contains an address: on pulldown-cmark it
charged 10.1% of our executed instructions and 11.6% of LLVM's to a
different function (ours: `FirstPass::parse_block` 5.06M by `fn=`, 8.06M
by symbol; `HtmlWriter::run` 0.35M vs 2.34M; `item_to_event` 3.66M vs
2.70M). `irdiff.py`'s function table read `fn=` until this change; it now
charges by ELF symbol (`harness/elftext.py`), and its source-line table
was never affected. So per-function figures from `irdiff.py` taken
before this (the PARITY notes that cite "`<function>` +N" from exact
counts, e.g. WIDE128's `parse_block` +29.8k, now +31.9k by symbol) may
have charged a share of each delta to the wrong function; recheck one
before acting on it. `rastat.py dyn` already charged by llvm-objdump's
function ranges and used `fn=` only for the load offset (now 0 when the
addresses are the binary's own); its tables stand.

### Dynamic baseline (main after POTTS / COPY / PIECES / LATCHSPEC_LIVE / SIBCALL, 2026-11-20)

`rastat.py dyn --tag now` at `deeb0f89` (per-function numbers in
`target/rastat/dyn-now.json`). Ours / LLVM, executed; against the
2026-11-18 table below every program's overhead ratio fell (compression
1.459 -> 1.339, pulldown 1.240 -> 1.158, blind-sig 1.411 -> 1.345) and so
did every total (html 1.166 -> 1.150, pulldown 1.120 -> 1.105).

| program | st | ld | mov | csr | overhead | all insts |
|---|---|---|---|---|---|---|
| blake3-scalar | 1.00 | 1.75 | 0.44 | 1.33 | 1.222 | 1.048 |
| blind-sig | 1.76 | 2.24 | 1.25 | 1.11 | 1.345 | 1.090 |
| pulldown-cmark | 1.27 | 0.82 | 1.62 | 0.99 | 1.158 | 1.105 |
| regex | 0.88 | 1.02 | 1.31 | 1.01 | 1.084 | 1.078 |
| rust-compression | 3.17 | 0.88 | 2.00 | 0.84 | 1.339 | 1.069 |
| rust-html-rewriter | 1.45 | 1.20 | 1.48 | 1.11 | 1.284 | 1.150 |
| rust-json | 1.29 | 1.06 | 1.41 | 0.90 | 1.110 | 1.108 |
| rust-protobuf | 1.73 | 1.81 | 1.07 | 1.02 | 1.144 | 1.055 |

Excess moves by kind (ours - LLVM, executed): edge moves still lead
(html +1.22M, compression +0.84M, regex +0.67M, pulldown +0.52M,
protobuf +0.49M); `callarg` fell most (html +0.73M -> +0.47M, pulldown
+0.36M -> +0.25M, protobuf now -0.29M). Compression's stores are 3.2x
LLVM's, the largest single class ratio left.

### Dynamic baseline (main after X17 + WIDE128 + REGION, 2026-11-18)

Executed instructions, callgrind, aarch64-unknown-linux-musl, both sides
under the default knobs of that night (`PLIRON_X17`, `PLIRON_WIDE128`,
`PLIRON_RA2_REGION` / `SPILLHOIST`, `XCGU_LEAF`/`LASTCALL`). Class cells:
ours/LLVM. "Share of gap" is our excess overhead as a share of the whole
executed-instruction gap. `target/rastat/dyn-night.json` holds the
per-function numbers. Against the 2026-11-17 baseline below, overhead
ratios fell on every program (compression 1.999 → 1.459, pulldown 1.434
→ 1.240, regex 1.265 → 1.118).

| program | insts LLVM | insts ours | overhead LLVM | overhead ours | ours/LLVM | share of gap | st | ld | mov | csr |
|---|---|---|---|---|---|---|---|---|---|---|
| blake3-scalar | 46.63M | 49.06M | 3.98M | 5.03M | 1.264 | 43% | 0.59/0.59M | 3.22/1.81M | 0.60/1.11M | 0.62/0.47M |
| blind-sig | 31.50M | 34.42M | 0.81M | 1.14M | 1.411 | 11% | 0.06/0.04M | 0.21/0.09M | 0.62/0.46M | 0.25/0.22M |
| pulldown-cmark | 45.33M | 50.77M | 6.84M | 8.48M | 1.240 | 30% | 1.34/0.98M | 1.46/1.64M | 3.31/1.82M | 2.37/2.40M |
| regex | 45.97M | 49.85M | 3.59M | 4.01M | 1.118 | 11% | 0.87/0.98M | 0.77/0.76M | 1.79/1.27M | 0.58/0.58M |
| rust-compression | 39.72M | 42.75M | 2.21M | 3.23M | 1.459 | 34% | 0.50/0.15M | 1.15/1.31M | 1.45/0.60M | 0.13/0.15M |
| rust-html-rewriter | 85.83M | 100.12M | 12.16M | 15.75M | 1.295 | 25% | 2.16/1.47M | 1.46/1.21M | 6.22/4.14M | 5.90/5.34M |
| rust-json | 45.55M | 50.68M | 3.20M | 3.63M | 1.135 | 8% | 0.23/0.17M | 0.51/0.48M | 1.54/1.04M | 1.36/1.51M |
| rust-protobuf | 60.76M | 64.76M | 7.28M | 8.61M | 1.182 | 33% | 1.05/0.61M | 0.72/0.40M | 3.51/3.06M | 3.33/3.21M |

### Dynamic baseline (main at d74c3554 + the harness, 2026-11-17)

Pre-REGION / pre-X17 reference. `target/rastat/dyn-base.json` in the
agent's worktree holds the per-function numbers. Checker-clean with
`PLIRON_RA_CHECK=1 PLIRON_RELOADPRE_CHECK=1`.

| program | insts LLVM | insts ours | overhead LLVM | overhead ours | ours/LLVM | share of gap | st | ld | mov | csr |
|---|---|---|---|---|---|---|---|---|---|---|
| blake3-scalar | 46.6M | 49.0M | 3.98M | 5.03M | 1.265 | 44% | 0.64/0.59M | 3.31/1.81M | 0.46/1.11M | 0.62/0.47M |
| blind-sig | 31.5M | 34.7M | 0.81M | 1.29M | 1.594 | 15% | 0.06/0.04M | 0.22/0.09M | 0.75/0.46M | 0.25/0.22M |
| pulldown-cmark | 45.3M | 51.9M | 6.84M | 9.80M | 1.434 | 45% | 1.58/0.98M | 1.86/1.64M | 3.40/1.82M | 2.96/2.40M |
| regex | 46.0M | 50.5M | 3.59M | 4.54M | 1.265 | 21% | 1.09/0.98M | 0.98/0.76M | 1.90/1.27M | 0.57/0.58M |
| rust-compression | 39.7M | 44.1M | 2.21M | 4.42M | 1.999 | 51% | 0.68/0.15M | 2.03/1.31M | 1.58/0.60M | 0.13/0.15M |
| rust-html-rewriter | 85.8M | 101.5M | 12.16M | 16.87M | 1.388 | 30% | 2.59/1.47M | 1.93/1.21M | 6.40/4.14M | 5.96/5.34M |
| rust-json | 45.6M | 51.5M | 3.20M | 4.13M | 1.291 | 16% | 0.23/0.17M | 0.58/0.48M | 1.82/1.04M | 1.50/1.51M |
| rust-protobuf | 60.8M | 65.3M | 7.28M | 8.94M | 1.228 | 37% | 1.18/0.61M | 0.87/0.40M | 3.60/3.06M | 3.29/3.21M |

Register moves are the largest excess class in five programs. The
largest excess function per program (where, not why: inlining differs,
so a 0 on LLVM's side often means LLVM inlined it):

- blake3: `portable::compress_in_place` +0.82M (ld 3.19M/1.80M; LLVM uses more moves, 0.93M/0.21M)
- blind-sig: `monty::montgomery` +0.15M (ld 0.11M/0)
- pulldown-cmark: `FirstPass::parse_block` +2.45M (inlined by LLVM)
- regex: `dfa::Fsm::follow_epsilons` +0.66M (st 0.22M/3k, mov 0.45M/3k)
- rust-compression: `BrotliCreateHqZopfliBackwardReferences` +1.04M (st 0.39M/28k, mov 0.55M/0.11M)
- html-rewriter: a `transform_stream::dispatcher` closure +0.50M (mov + csr, inlined by LLVM)
- rust-json: a serde `Deserialize` for `Post` +0.41M (inlined by LLVM)
- rust-protobuf: `prost::encoding::string::encode` +1.02M (csr 0.42M, mov 0.38M; inlined by LLVM)

### Static baseline (main at d74c3554 + the harness, 2026-11-17)

Weighted (8^depth) counts; static counts in parentheses.

| corpus | fns | st | ld | mov | csr | overhead | slot |
|---|---|---|---|---|---|---|---|
| std | 4,917 | 355,457 (5,772) | 443,806 (8,644) | 673,590 (52,858) | 12,001 (11,952) | 1,484,854 (79,226) | 482,131 |
| html | 15,348 | 115,876 (3,554) | 301,569 (5,630) | 683,317 (88,324) | 22,808 (22,724) | 1,123,570 (120,232) | 232,603 |
| smoke | 2,979 | 238,356 (3,499) | 997,677 (6,939) | 949,651 (25,462) | 5,449 (5,323) | 2,191,133 (41,223) | 1,171,475 |

### Static, the opt-in knobs one at a time (overhead change against the baseline)

| knob | std st | std ld | std mov | std overhead | html st | html ld | html mov | html overhead |
|---|---|---|---|---|---|---|---|---|
| `POSTOPT=15` | -4.95% | -0.20% | +0.19% | -1.16% | -23.88% | -1.42% | +0.34% | -2.64% |
| `POSTOPT=7` | -1.53% | -0.20% | +0.20% | -0.34% | -5.07% | -1.17% | +0.41% | -0.58% |
| `POSTOPT=8` | -2.39% | 0 | 0 | -0.57% | -7.47% | -0.04% | 0 | -0.78% |
| `SRCVREG=1` | -1.71% | -6.23% | -0.78% | -2.63% | -6.20% | -18.98% | -0.62% | -6.11% |
| `REMATW=1` | -1.05% | +0.08% | -2.14% | -1.20% | -11.68% | -16.12% | -3.51% | -7.67% |
| `XBLOCK=1` | -0.08% | 0 | -0.01% | -0.02% | -0.08% | -0.17% | +0.05% | -0.02% |
| `MERGEDEPTH=1` | -25.55% | -12.30% | -9.51% | -14.11% | +0.57% | +1.14% | -4.21% | -2.20% |
| `MERGEDEPTH=2` | -26.50% | -12.93% | -8.58% | -14.10% | -7.73% | -0.86% | -7.41% | -5.54% |
| `MERGEDEPTH=3` | -25.69% | -12.61% | -9.22% | -14.10% | +0.12% | +0.06% | -4.86% | -2.93% |
| `POSTOPT=15 SRCVREG=1 REMATW=1 MERGEDEPTH=2` | -35.88% | -24.37% | -7.77% | -19.40% | -37.36% | -38.82% | -6.52% | -18.24% |

csr moved under 0.12% in every row. Static is a ranking aid, not the
verdict: the dynamic counts and exact instruction totals below decide.

### Knob evaluation (2026-11-17, executed, against the dynamic baseline)

Each knob alone on top of the baseline, all eight programs built with the
checker on (all checker-clean). Change in executed instructions (whole
program) and in regalloc overhead (st+ld+mov+csr), pliron side:

| knob | blake3 | blind-sig | pulldown | regex | compression | html | json | protobuf | verdict |
|---|---|---|---|---|---|---|---|---|---|
| `POSTOPT=15` insts | +4 | -41,476 | -72,602 | -9,694 | -14,412 | -33,436 | -516 | -25,178 | **on** |
| overhead | 0 | -34 | -2,185 | -705 | -23,092 | -20,242 | -260 | -8,473 | |
| `SRCVREG=1` insts | 0 | -37,772 | -129,603 | -4,519 | -2,914 | -56,569 | +615 | -54,349 | **on** |
| overhead | 0 | -4,012 | -144,483 | -11,086 | -246,155 | -113,940 | +13 | -38,793 | |
| `REMATW=1` insts | +1 | +79,719 | -212,819 | +7,809 | +1,394 | +53,332 | -935 | +298,190 | opt-in |
| overhead | +1 | -15,663 | -359,667 | -2,390 | +50,160 | -44,065 | -8,369 | +252,340 | |
| `XBLOCK=1` insts | 0 | -13,310 | +5,097 | +225 | -28 | -38 | +644 | -30,749 | opt-in |
| overhead | 0 | -1,822 | +44 | -23 | -18 | -10,724 | +20 | +39 | |
| `MERGEDEPTH=1` insts | -6 | -10,369 | +632,706 | +50,155 | -86,616 | +15,726 | -110,179 | +50,399 | opt-in |
| overhead | -5 | +6,592 | +594,435 | +17,318 | -82,939 | -18,554 | +8,147 | -1,350 | |
| `MERGEDEPTH=2` insts | -2 | -23,195 | +1,194,178 | +48,031 | -95,819 | +22,196 | -117,576 | +28,831 | opt-in |
| overhead | -5 | +4,402 | +1,182,460 | +18,395 | -89,967 | -12,074 | +4,385 | -13,100 | |

- `POSTOPT=15` and `SRCVREG` are default on (commits "regalloc2:
  PLIRON_RA2_POSTOPT default 15", "... SRCVREG default on"). SRCVREG's
  rust-json +615 (0.001%) is outside the overhead classes (layout). With
  SRCVREG on, block-parameter labels name instruction results, so the
  checker's label check covers them.
- `REMATW` halves the spill weight of rematerializable bundles. Plain
  `=1` cut pulldown overhead hard but cost protobuf `merge_loop` +205k
  non-nop. Default since 2026-11-18 is mode `7` (only bundles with ≥8
  uses): pulldown −0.21% non-nop (copies=2), protobuf +0.017%, html
  +0.006% (`docs/history/2026-11-18-ra-night.md`). The remat set for
  weights is *cheap* by default (`rematerialize_cheap`: immediates,
  not GOT/near symbol loads — LLVM/Go/GCC), `PLIRON_RA2_REMATW_CHEAP=0`
  restores the old set.
- `XBLOCK` is too small to matter (largest win html -10.7k moves, which
  layout eats) and loses 5k on pulldown.
- `MERGEDEPTH` breaks pulldown-cmark (+0.63M / +1.19M; `mov` +0.17M /
  +0.76M, `st` +0.29M) though it helps compression and json. The static
  metric ranked it first (std -14%): std's number is dominated by
  miniz's `decompress_with_limit`. Read static as a ranking aid only.
- Both together (the new defaults, measured after the flips, against the
  baseline): instructions blake3 +2, blind-sig -74,297, pulldown
  -202,278, regex -11,018, compression -16,977, html -90,049, json -250,
  protobuf -81,218; overhead lower in seven, blake3 unchanged. Overhead
  ratios against LLVM now: blake3 1.265, blind-sig 1.585, pulldown
  1.413, regex 1.262, compression 1.905, html 1.379, json 1.291,
  protobuf 1.221 (`target/rastat/dyn-newdef.json`).
- `test.sh` stanza `slotpromlt` measured XBLOCK's and SPILLCSE's size
  effect against a baseline that now includes POSTOPT's store-back
  removal; it pins `PLIRON_RA2_POSTOPT=0` there now.
- Native kernel timings (`bench/run.sh`) were not taken: the machine ran
  at load 30-45 throughout (other agents), so timings would be noise; the
  exact counts above are the evidence.

## Investigations

### bp-move census after EVICT (2026-11-25)

`PLIRON_RA2_MOVESTATS=1 clif-check codegen target/corpus/std` now also
prints the `ra2 evict refusals` table (`EVICTWHY`) and a remat-arg share
per move kind (kind bit `0x20` on a refused merge whose arg
`is_remat`). Headline: **about a third of the bp-* census weight is
phantom cost** — a refused merge for a remat-able arg emits a move that
Cranelift rematerializes into the same instruction the def would have
cost anyway (`movz w22, #K` on the edge vs. the merged `movz` in the
arm). Per-kind weighted (non-remat / remat):

| kind | real | remat |
|---|---|---|
| xblock | 21,201 | 0 |
| bp-ovl-3rd | 19,543 | 26,612 |
| bp-overlap | 16,881 | 251 |
| bp-ovl-eb | 10,374 | 2,525 |
| bp-merged | 8,711 | 0 |
| bp-ovl-mix | 7,651 | 13,753 |

- `StdoutLock::write_all_vectored` (formerly 21.6k, the #2 function):
  a ~45-arm `icmp` cascade, each arm `brif .., join(state, const_k)`.
  `v1209` (`iconst 0`) is passed to **both** param slots on one edge and
  one slot on ~40; its merged range spans every arm block, so all 41
  distinct const args conflict → 41 `cold` refusals (evicting it would
  restore ~40×512). 99.7% remat args → the bucket was ~phantom.
- `rustc_demangle legacy Demangle::fmt` (~1100 blocks, 10–12-wide
  params) is the real whale: ~26.7k real bp-* (self 21.3k, param 10.8k
  of refusals tried) + ~10.3k stack-side xblock/split — spilled values
  crossing edges, not reg-reg moves.
- `bp-overlap` (16.9k) is ~99% non-remat self-overlap, defs upstream —
  demangle 5.7k + `small_sort_*` instantiations ~7k + btree `navigate`.
  At the ra2 level the fork (one reg → two live values past the arg's
  def) is fundamental: any bundle/range split relocates the same move —
  measured zero-sum. Wins here are upstream (argtail-style def sinking)
  or LLVM-style value-structure changes.
- EVICT measured-dead extensions: bit1 param eviction (`=3`: html
  +0.6%), bit2 exact cost (`=5`: +0.2%), `=7` +0.2..0.5%; `_CAP=8/16`
  recovers ~1k (cap refusals mostly re-classify to `self`).
- `PLIRON_RA2_MERGEDEPTH=4/6` global merge reordering shuffles the
  buckets (overlap −4..5k, mix −3k) but total weighted insts regress
  +6..52k — bigger early bundles cost spill pressure. Rejected.
- ARGTAIL×EVICT (`ab.py`, exact instr): `PLIRON_ARGTAIL=1` is flat on
  html (+0.005%) under EVICT-on but +0.6% under `EVICT=0` — EVICT does
  rescue the stranded edge-tail clones as predicted. Still +0.1%
  pulldown, +0.3% blake3 — not gate-clean enough to flip on this box
  (cycle numbers are noise on a loaded machine).
- armmerge's uniform params + tail sameargs: `armmerge`'s
  `change_to_alias` and the late edge/thread steps leave uniform-input
  params (every in-edge arg resolving to one root — StdoutLock block95:
  4 of 5 uniform across 45 arms; demangle block137: 8 edges each
  passing a distinct `iconst 1`). Lowering already resolves aliases
  (`lower.rs`), so ra2 merges the uniform arg for free — but a refused
  merge pays an edge move per arm, and the spilled-param machinery is
  real. `PLIRON_SAMEARG4=1` runs sameargs once at the pipeline tail
  (after argtail, before brhint): corpus-wide 234 fns changed,
  −4.1k winsts (−17.9k wspill, +18.0k wmovs — collapse trades the
  param's spill/copy machinery for materialized copies that cost more
  in hot loops, e.g. gimli skip_attributes +7.3k winsts vs
  InlinedFunction −12.2k). Held-out instr: html +0.3%, pulldown
  −0.06%, protobuf +0.1%, blake3 −0.02% — net-neutral, opt-in only.
- demangle `Demangle::fmt` EVICTWHY decomposition (91 overlap-failed
  merges): `self` 50 @ 21.3k (arg's bundle overlaps the param's own
  range — the fork pattern, unevictable at ra2), `param` 23 @ 10.8k
  (under EVICT=3 they re-classify to `cold` 9.8k: the param's in-edge
  weight exceeds the merge gain — only 2 evictions fire, −450
  weighted), `cap` 12 @ 4.7k, `tied` 4 @ 2k, `cold` 2 @ 1k.

### blake3 `portable::compress_in_place` reloads (2026-11-17)

Executed per call (51,465 calls): ours 11 spill stores, 62 reloads, 4
register moves; LLVM 11 store instructions (pairs: `stp w15, w13`),
35 reloads, 18 moves. The function is one straight-line block of 832
instructions.

- **Register pressure**: 32-35 values live at 640 of 832 points (16
  state words, 16 message words and the `cv` pointer live across the
  whole round, plus temporaries). We allocate 26 integer registers
  (x0-x15, x19-x28); LLVM uses 30 here: also x16, x17, x18 (free on
  Linux) and x30 (a leaf). So we must keep at least 9 long-lived values in
  memory at every point (best case about 8 x 6 = 48 reloads), LLVM 5.
- **How we lose more than that**: the message words are the longest
  bundles, so they are allocated first; the round's short, dense values
  then evict them, each is split at the conflict twice
  (`MAX_SPLITS_PER_SPILLSET` = 2) and then goes to minimal bundles: one
  reload per use, for 11 of the 16 words. The `cv` pointer (uses at
  instructions 0-8 and 785-831) keeps x0 throughout: 17 uses give it a
  higher weight (236) than any message word (120-135), so evictions
  always pick a word.
- Tried, no gain (not committed): `RA2_LINEAR=1` (upstream spill
  priority) 62 -> 68 reloads; raising the split cap to 3..64, 62 -> 64..68;
  a gap split (a bundle cut at a use-free gap over 6x its average use
  spacing, the gap to the spill bundle) frees x0 but saves 1 reload, and
  is +0.03..0.06% static overhead on std/html.
- **What moves it**: registers. `PLIRON_X17=1 PLIRON_X30=1` (both
  existing opt-in knobs) take this function to 6 stores, 51 reloads,
  4 moves (static html overhead -6.1%, std -0.9%). `PLIRON_X16=1`
  alone: 72 -> 65 ldr here, program -257k (-0.53%) exact — the largest
  single-knob blake3 delta; pulldown -28.6k, html/compression noise.
  x18 (Linux only; reserved on macOS) is default-on since 685a9a65,
  x16 default-on since c35a3d7f (both residuals closed: the
  scratch-borrow imm12 overflow 4b720edc, and the far-`b` veneer
  scratch — a veneer can't restore a reg carrying its target, so
  functions big enough to out-range `b` keep both IP regs reserved,
  265b8254).
- **Reload-per-use can't be cured here** (2026-11-26): post-x18 the
  function is ours 7 st / 48 ld / 2 mov vs LLVM 11 st / 35 ld / 18 mov
  per call; the 48 `ldur`s come from per-use minimal bundles (LLVM keeps
  one register across several nearby uses). `PLIRON_RA2_MINCLUS=1`
  clusters such uses into one reload bundle, but every cluster span is
  squatted by already-allocated long bundles the cluster weight can
  never evict, so all 8 clusters fall back to per-use minimal — the
  emitted binary is identical. Giving the cluster the always-evict
  grade evicts the squatters but measured strictly worse (88 ldr/23 str
  vs 72/16 baseline): their reloads just move. The residual gap is
  register count, not the fallback policy.

### Callee-save traffic (csr), pulldown and html-rewriter (2026-11-17)

- Most of the excess is in functions LLVM inlined into their callers (0 on
  its side: `FirstPass::parse_block`, `append_text`, html-rewriter's
  dispatcher closure, prost's `string::encode`): an inlining difference,
  not allocation.
- Where both sides keep the function, we save more registers per call:
  pulldown `Parser::next` 10 csr instructions per call against 4. Our body
  is 557 instructions with more inlined (LLVM's: 147, calling
  `handle_inline` and others), so more values live across calls.
- `PLIRON_SHRINKWRAP` is on by default (the source reads `!= "0"`;
  `=1` is identical to unset; AGENTS.md calls it opt-in, which is stale).
  Static csr counts rise with it (saves duplicated into colder blocks),
  so judge it dynamically only.
- Tried, no gain (not committed): LLVM's "CSR first-time cost" (a bundle
  that would be the first in a callee-saved register goes to minimal
  bundles when its uses cost less than a save and restore). On
  `Parser::next` csr stayed at 10 (later bundles take the register anyway)
  and moves rose 102 -> 154. A rule that works must decide per register
  (all the bundles that would use it), as `PLIRON_RA2_COLDSPLIT` does for
  cold call regions.

## Knobs

Status: **on** = default on; **opt-in**; **eval** = under evaluation.
Values the fuzzer tries are in `harness/rafuzz.py`. Vendored crates read
the environment directly: a flip changes `== "1"` to `!= "0"` in the
source, not `pliron-cfg`'s `DEFAULT_ON`.

| knob | read by | what it does | status |
|---|---|---|---|
| `PLIRON_RA2_FREQ=0..5` | regalloc2 `cfg.rs` | block-frequency spill weights; 3 normalizes by the hottest block | on (3) |
| `PLIRON_RA2_LOOPSPLIT=<n>` | regalloc2 `process.rs` | split a bundle at the entry/exit of the small loop (<= n insts) around its hottest use | on (16) |
| `PLIRON_RA2_LOOPDEPTH=1` | regalloc2 `cfg.rs` | only dominance-checked back edges count for loop depth | opt-in (2026-11-18: blind-sig +1.6%; reject) |
| `PLIRON_RA2_FREQSPLIT=1` | regalloc2 `process.rs` | move a split point to the cheapest block boundary | opt-in (2026-11-18: json +0.07%, protobuf +0.02% non-nop; not for default) |
| `PLIRON_RA2_COLDSPLIT=1..3` (+`_RATIO`, `_COLDDIV`, `_BIAS`) | regalloc2 | cut cold call blocks out of a hot bundle | opt-in |
| `PLIRON_RA2_NOPINMERGE` | regalloc2 `merge.rs` | refuse merges that spread a fixed-register pin | on |
| `PLIRON_RA2_SLOTPAIR` | regalloc2 `spill.rs` | repack spill slots so consecutive spills pair (`stp`/`ldp`) | on |
| `PLIRON_RA2_POSTOPT=<bits>` | regalloc2 `postopt.rs` | post-allocation cleanup: 1 redundant reloads, 2 reload as copy, 4 redundant spill stores, 8 dead spill stores | on (15) since 2026-11-17 |
| `PLIRON_RA2_REGION=1\|2` | regalloc2 `regionsplit.rs` | min-cut region splitting (LLVM `tryRegionSplit`); `_CAP` default 2 since 2026-11-18 | on (1) since 2026-11-18 |
| `PLIRON_RA2_SPILLHOIST` | regalloc2 `spillhoist.rs` | one spill store at the definition | on since 2026-11-18 |
| `PLIRON_RA2_SRCVREG` | regalloc2 `moves.rs` | block-param moves carry the argument's vreg (constant args rematerialize) | on since 2026-11-17 |
| `PLIRON_RA2_REHOME` | regalloc2 `rehome.rs` | a value's register in a join-only block follows the cheaper side | on (2) |
| `PLIRON_RA2_POTTS=1\|2` | regalloc2 `potts.rs` | per value, α-expansion by min-cut over its blocks to remove edge moves; `1` Pareto steps only, `2` trusts frequencies | on (1) since 2026-11-20 (`=1` no program worse: html -0.10%, blind-sig -0.18%, protobuf -0.08%; `=2` regex +0.41%) |
| `PLIRON_RA2_POTTS_COPY=1` | regalloc2 `potts.rs` | after allocation, a copy whose source dies there (or destination starts there) and whose side is one plain range takes the other side's register: the copy is dropped at emission | on since 2026-11-20 (pulldown -0.41%, protobuf -0.20%, html -0.12%, regex -0.07%, nothing worse) |
| `PLIRON_RA2_POTTS_PIECES=1` | regalloc2 `potts.rs` | `POTTS` also relabels partial block pieces (live in and dying; defined and leaving or dying) and counts the copies inside them as energy pairs | on since 2026-11-20 (protobuf -0.45%, html -0.43%, regex -0.23%, json -0.22%, pulldown -0.19%, none worse) |
| `PLIRON_A64_HALFCLOB=1` | Cranelift `vcode.rs` | a plain call with no wide (>64-bit float-class) value live across it does not clobber `v8`-`v15` for regalloc | on since 2026-11-20 (compression -0.12%, rest noise) |
| `PLIRON_RA2_XBLOCK=1` | regalloc2 `moves.rs` | redundant-move state survives a single-predecessor fallthrough | opt-in (evaluated: pulldown +5k) |
| `PLIRON_RA2_EVICT` (`_RATIO`, `_CAP`) | regalloc2 `merge.rs` | a merge refused only by third-party range overlap evicts the squatting vregs and retries when the edge is RATIO× hotter; bit1 (`=2`) also evicts conflicting block parameters, bit2 (`=4`) falls back to an exact boundary-move cost when the per-vreg edge sum fails | on since 2026-11-25 (pulldown -0.21%, protobuf -0.10%, blind-sig -0.07%, html -0.05%; compression +0.10% is an ivsr cost flip, not the mechanism). Bits 1+2 measured dead (html +0.2..0.6%, census table: `self` 31k and `cap` 28.6k weighted dominate) |
| `PLIRON_RA2_MERGEDEPTH=1..6` | regalloc2 `merge.rs` | merge-order experiments: 1..3 deepest loop first, 4/5 hottest edge first (4 ties shortest-span), 6 shortest span first | opt-in (evaluated: pulldown +0.6M..1.2M; 4/6 shift census buckets but regress total weighted insts +6..52k — rejected 2026-11-25) |
| `PLIRON_RA2_MINCLUS` (`_SPAN`, default 192) | regalloc2 `process.rs` | in the minimal-bundle fallback, consecutive `Reg` uses of one vreg in one block within SPAN instructions form one bundle (one reload serves the cluster); the `minclus` flag makes a failed cluster fall back to per-use minimal bundles | opt-in (2026-11-26: pulldown -19.5k, html -6.2k exact, blake3 identical — clusters can't evict the state bundles that squat their spans; an always-evict variant measured strictly worse) |
| `PLIRON_RA2_REMATW=0..7` (+`_MAXUSES`, `_MINUSSES`, `_CHEAP`) | Cranelift `vcode.rs` + regalloc2 `process.rs` | rematerializable spill weight; default `7` (`uses≥MINUSSES`, default 8); `=0` off; `_CHEAP` default on (immediates only for weight set) | on (7) since 2026-11-18 |
| `PLIRON_REMAT`, `_STORE` | Cranelift `vcode.rs` | reloads of constants/addresses rematerialize; stores only remat reads die | on |
| `PLIRON_RELOADPRE` (`_ROUNDS`), `PLIRON_RELOADCSE`, `PLIRON_SPILLCSE` | Cranelift `vcode.rs` | reload PRE into predecessors; per-block and cross-block reload CSE | on |
| `PLIRON_EDITPAIR` | Cranelift `vcode.rs` | fuse adjacent spill/reload edits into pairs | on |
| `PLIRON_SHRINKWRAP` | Cranelift `vcode.rs`, `context.rs` | callee saves at the nearest closed dominator (the source reads `!= "0"`: on, though AGENTS.md still calls it opt-in) | on |
| `PLIRON_X16` | Cranelift aarch64 `regs.rs`, `emit.rs` | x16 allocatable (LLVM uses both IP regs); emit-time address scratch borrows IP regs with a below-`sp` save/restore; both IP regs stay reserved in a function big enough to out-range `b` (veneer scratch) | on since 2026-11-26 (`=0` off); blake3 -0.53% |
| `PLIRON_X17` | Cranelift aarch64 `regs.rs` | x17 allocatable (LLVM uses both IP regs) | on since 2026-11-18 (`=0` off) |
| `PLIRON_X30` | Cranelift aarch64 `regs.rs` | link register allocatable (`=1` everywhere, `=2` framed only) | opt-in |
| `PLIRON_RA_CHECK=1` | backend, clif-check | regalloc2's checker on every function | debug |
| `PLIRON_RELOADPRE_CHECK=1` | Cranelift `compile.rs` | the checker on the post-allocation rewrites' output | debug |
| `PLIRON_RA_CHECK_LABELS=0` | regalloc2 `checker.rs` | skip the `edit_vregs` label check | debug |

## Fuzz log

- 2026-11-17: harness landed. First CLIF run (smoke) with the label check
  flagged reload PRE moving a block-parameter reload two edges ahead of
  its parameter: not a miscompile (only instruction results
  rematerialize); the check now skips block-parameter labels.
  regalloc2 leg: 0 failures in the first 6,000 seeds under defaults.
- 2026-11-17: the first held-out builds with `PLIRON_RA_CHECK=1` failed
  on blake3's `derive_key` with `EditVRegMismatch`: the release dylib
  predated the block-parameter skip above (only clif-check had been
  rebuilt). Rebuilt, all eight programs are checker-clean. `clif-check`
  gained `CLIF_CHECK_TRIPLE` (aarch64-unknown-linux-musl) for replaying
  the held-out corpora on their own target.
- 2026-11-17, first loop (12:07-13:44): 455 runs, 0 failures: 6.88M
  regalloc2 seeds and 114 CLIF corpus runs (std, smoke, clcg, html
  samples) under defaults, everything on, each knob alone at each value,
  and random combinations. Restarted on the new defaults (POSTOPT=15,
  SRCVREG on).
