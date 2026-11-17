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
