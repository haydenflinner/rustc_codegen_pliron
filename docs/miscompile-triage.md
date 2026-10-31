# Miscompile triage playbook

Ordered cheapest-first. Each stage either names the culprit or produces a
smaller reproducer for the next stage. Written after the
`resolve_imports`/`free(0x8)` hunt (2026-10), which took ~20 rebuild probes
where the first two stages below would have answered it in one build — had
the corpus format been complete (see "Replay fidelity gap").

## 0. Set up the oracle

A check script that rebuilds the suspect crate and runs the crashing
command, exiting nonzero iff the symptom reproduces. Heap-corruption oracles
should grep the log for the signal/`malloc:` line, not just the exit code —
a miscompile can also surface as a *different* crash or as clean diagnostics.

Probe cost is dominated by `./x build`: it recompiles every crate downstream
of the suspect plus the driver relink. To cut per-probe time from ~5 min to
~1 min, capture the suspect crate's exact rustc invocation once (`./x -vv`,
the failing command is logged), run it standalone with the `PLIRON_*` knobs,
then relink only `librustc_driver.dylib`. Separate `--build-dir`s let
quartile probes run in parallel.

## 1. `PLIRON_DIFFCHECK=step` — try this first

One build can name the corrupting step directly. It interprets CLIF before
and after every step that mutates each selected function and compares what a
caller could observe: return values, the **sequence of calls made (callee +
args)**, and memory effects.

```sh
PLIRON_OPT_FNS=@/tmp/culprits.txt \      # optional: only these fns
PLIRON_DIFFCHECK=step \
PLIRON_VERIFY=full \
PLIRON_DIFFCHECK_DIR=/tmp/diffcheck \
./x build --stage 2 compiler             # or the suspect crate alone
```

On a mismatch it writes `<dir>/<fn>.<step>/{before,after}.clif` +
`report.txt` — a ready-made minimized reproducer, replayable in ~1 s with
`clif-check step <step> before.clif`. `PLIRON_VERIFY=full` catches
structural corruption in the same build for free.

Strengths that aren't obvious:

- Calls don't kill the check — they hit deterministic stubs that record
  `(callee, args)`. A stale-pointer-to-`deallocate` bug shows up as a
  changed call argument, which is exactly the `free(0x8)` shape.
- `PLIRON_DIFFCHECK_FN=<substr>` scopes it to one function; `=1` is the
  cheaper mode (one in/out check per function, internal replay on
  mismatch).

Limits (from `diffcheck.rs`): fuel exhaustion, traps, and coverage are
*inconclusive*, never failures — an inconclusive result does not exonerate
anything. Random inputs may never drive the diverging path (bump
`PLIRON_DIFFCHECK_TRIALS`, default 16; `PLIRON_DIFFCHECK_FUEL`,
default 200k). wasm32-pointer functions can't be interpreted on a native
host.

## 2. Function-level bisect (only if stage 1 is inconclusive)

`harness/fnbisect.py [--names F] -- <check.sh>` delta-debugs
`PLIRON_OPT_FNS`: ~16 rebuild probes → one culprit symbol. Verify the
minimized set still reproduces the *original* symptom — the oracle accepts
any failure.

## 3. Corpus replay — fast but lossy today

```sh
PLIRON_CORPUS=/tmp/corpus ./x build <suspect crate>   # one build
./target/release/clif-check report /tmp/corpus out.json --check --jobs 16
./target/release/clif-check pipeline /tmp/corpus/<fn>.<n>.clif
```

Dumps every opted function's pre-pipeline CLIF **with its fact headers**;
`report --check` runs the full pipeline + interpreter diff over all of them
in seconds, no rustc.

### Replay fidelity gap (found the hard way)

The `diffcheck-facts` header serializes `nonnull`, `noalias`, `scopedna`,
`deref`, `frozen`, `nowrite`, `derived`, plus `nocap`/`lifetimes`/`names`.
It does **not** serialize `operand_frozen`, `call_frozen`, `invariant`, the
`scoped` inst→region map, or `block_region`. Any transform whose decision
reads those facts replays differently than the real build.

Worked example: `resolve_imports`'s bad `licm` load hoist depended on
`iso` verdicts fed by region-scoped `noalias` carried through earlier steps.
The corpus replay hoisted only arithmetic — the load stayed put — so the
replay *appeared* to exonerate `licm` while the real build corrupted the
function. Consequences:

- A clean corpus replay does **not** exonerate a step.
- A step that does nothing offline may still be the corruptor.

Fix direction: extend `OwnedFacts`/`corpus_dump` to serialize the remaining
`Pipe` fact tables (done for `scoped_na`; the map-valued tables need a
value/inst-keyed encoding).

## 4. Prefix-bisect `PLIRON_PIPELINE` — rebuild-per-probe, last resort

`PLIRON_PIPELINE=a,b,c` selects steps by name in listed order; bisect the
prefix length. Two traps:

- **Number the *selected* list, not `pipeline::DEFAULT`.** Opt-in steps
  (`egraph`) aren't selected by default, so the Nth entry of DEFAULT is not
  the Nth step that runs. Dump the real list once (the `PLIRON_CLIF` /
  stepdump output or `clif-check` help) and bisect that. The
  `resolve_imports` hunt initially blamed `ldrot` (DEFAULT position 14)
  when the real step 14 was `licm`.
- **Badness is not monotone in N.** A later step can mask earlier
  corruption (DCE eats the bad value), and a truncated pipeline can produce
  IR the backend chokes on differently — still greps as BAD. So confirm
  the winner `k` with one *full-minus-one* run: `PLIRON_PIPELINE=<all
  except k>` or a pass-level/finer gate (`PLIRON_<PASS>=0`,
  `PLIRON_LICM_LOADS=0`-style transform-class gates). If it goes green,
  `k` is confirmed; prefer the diffcheck result if they disagree.

`harness/bisect.py -- <check.sh>` automates the same oracle over
`PLIRON_OPT_BISECT=N` at finer granularity (one unit = one pass application,
across IR and CLIF passes). The numbering is deterministic without extra
flags: under `PLIRON_OPT_BISECT` the backend runs CGUs and lowering
serially by itself (`cgupool::enabled`, `lower.rs`) and turns the step and
pipeline caches off.

## 5. Inside the step

Once a step is named, `PLIRON_TRACE=pliron::<step>` on the **real build**
(not the replay — see the fidelity gap) logs each candidate transform's
decision. If a step applies many transforms, gate them with a per-transform
`crate::config::bisect_in("<step>", fname)` call so `PLIRON_OPT_BISECT=N`
counts individual transforms; `PLIRON_OPT_BISECT_DEBUG=1` names each unit.

Worked example — `resolve_imports`: trace showed
`licm: hoist v16480 = load.i64 notrap v16363` after
`root V(v16363) iso=true deref=Some((16,false))`; `PLIRON_LICM_LOADS=0`
turned the build green. Root cause: `v16363`'s `noalias` was region-scoped
(clone-arg seed) but had leaked into `loadfwd::isolated`'s function-wide
isolation set, so `overlap_keeps` declared an in-loop store to the same
bytes disjoint.
