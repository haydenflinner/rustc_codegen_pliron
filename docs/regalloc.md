# Register allocation: the push to state of the art

Shared log of the register-allocation push (started 2026-11-17). Goal:
dynamic regalloc overhead (spill stores, reloads, register moves,
callee-save saves and restores) at or below LLVM's on all eight held-out
programs, with no other loss. Everyone working on regalloc2 heuristics,
moves/coalescing/remat or the analyses feeding them records here what a
knob does and where it stands.

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

### Dynamic baseline (main at d74c3554 + the harness, 2026-11-17)

Executed instructions, callgrind, aarch64-unknown-linux-musl, both sides
built with `PLIRON_RA_CHECK=1 PLIRON_RELOADPRE_CHECK=1` (all eight
checker-clean). Class cells: ours/LLVM. "Share of gap" is our excess
overhead as a share of the whole executed-instruction gap.
`target/rastat/dyn-base.json` in the agent's worktree holds the
per-function numbers.

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

## Knobs

Status: **on** = default on; **opt-in**; **eval** = under evaluation.
Values the fuzzer tries are in `harness/rafuzz.py`. Vendored crates read
the environment directly: a flip changes `== "1"` to `!= "0"` in the
source, not `pliron-cfg`'s `DEFAULT_ON`.

| knob | read by | what it does | status |
|---|---|---|---|
| `PLIRON_RA2_FREQ=0..5` | regalloc2 `cfg.rs` | block-frequency spill weights; 3 normalizes by the hottest block | on (3) |
| `PLIRON_RA2_LOOPSPLIT=<n>` | regalloc2 `process.rs` | split a bundle at the entry/exit of the small loop (<= n insts) around its hottest use | on (16) |
| `PLIRON_RA2_LOOPDEPTH=1` | regalloc2 `cfg.rs` | only dominance-checked back edges count for loop depth | opt-in |
| `PLIRON_RA2_FREQSPLIT=1` | regalloc2 `process.rs` | move a split point to the cheapest block boundary | opt-in |
| `PLIRON_RA2_COLDSPLIT=1..3` (+`_RATIO`, `_COLDDIV`, `_BIAS`) | regalloc2 | cut cold call blocks out of a hot bundle | opt-in |
| `PLIRON_RA2_NOPINMERGE` | regalloc2 `merge.rs` | refuse merges that spread a fixed-register pin | on |
| `PLIRON_RA2_SLOTPAIR` | regalloc2 `spill.rs` | repack spill slots so consecutive spills pair (`stp`/`ldp`) | on |
| `PLIRON_RA2_POSTOPT=<bits>` | regalloc2 `postopt.rs` | post-allocation cleanup: 1 redundant reloads, 2 reload as copy, 4 redundant spill stores, 8 dead spill stores | opt-in, eval |
| `PLIRON_RA2_SRCVREG=1` | regalloc2 `moves.rs` | block-param moves carry the argument's vreg (constant args rematerialize) | opt-in, eval |
| `PLIRON_RA2_REHOME` | regalloc2 `rehome.rs` | a value's register in a join-only block follows the cheaper side | on (2) |
| `PLIRON_RA2_XBLOCK=1` | regalloc2 `moves.rs` | redundant-move state survives a single-predecessor fallthrough | opt-in, eval |
| `PLIRON_RA2_MERGEDEPTH=1..3` | regalloc2 `merge.rs` | merge blockparams of the deepest loops first | opt-in, eval |
| `PLIRON_RA2_REMATW=1` | Cranelift `vcode.rs` | rematerializable bundles get half the spill weight | opt-in, eval |
| `PLIRON_REMAT`, `_STORE` | Cranelift `vcode.rs` | reloads of constants/addresses rematerialize; stores only remat reads die | on |
| `PLIRON_RELOADPRE` (`_ROUNDS`), `PLIRON_RELOADCSE`, `PLIRON_SPILLCSE` | Cranelift `vcode.rs` | reload PRE into predecessors; per-block and cross-block reload CSE | on |
| `PLIRON_EDITPAIR` | Cranelift `vcode.rs` | fuse adjacent spill/reload edits into pairs | on |
| `PLIRON_SHRINKWRAP` | Cranelift `vcode.rs`, `context.rs` | callee saves at the nearest closed dominator (the source reads `!= "0"`: on, though AGENTS.md still calls it opt-in) | on |
| `PLIRON_X17`, `PLIRON_X30` | Cranelift aarch64 `regs.rs` | x17 / the link register allocatable | opt-in |
| `PLIRON_RA_CHECK=1` | backend, clif-check | regalloc2's checker on every function | debug |
| `PLIRON_RELOADPRE_CHECK=1` | Cranelift `compile.rs` | the checker on the post-allocation rewrites' output | debug |
| `PLIRON_RA_CHECK_LABELS=0` | regalloc2 `checker.rs` | skip the `edit_vregs` label check | debug |

## Fuzz log

- 2026-11-17: harness landed. First CLIF run (smoke) with the label check
  flagged reload PRE moving a block-parameter reload two edges ahead of
  its parameter: not a miscompile (only instruction results
  rematerialize); the check now skips block-parameter labels.
  regalloc2 leg: 0 failures in the first 6,000 seeds under defaults.
