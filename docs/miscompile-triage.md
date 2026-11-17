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

### Reading a corpus mismatch

`report --check` compares the recorded function with the pipeline's
output. To go from "mismatch" to a cause (the five reports of
2026-11-16 took these steps; three were the checker, two a stale corpus):

1. **Which step.** Copy the file into a directory of its own and run
   `PLIRON_DIFFCHECK=step PLIRON_DIFFCHECK_DIR=<out> clif-check report
   <dir> out.json --check` (`PLIRON_CLIF_WASM=1` for a wasm corpus): the
   pipeline checks every step inside its one `Pipe` and saves
   `<out>/<fn>.<step>/{before,after}.clif`. `clif-check pair before.clif
   after.clif` replays that pair. (`clif-check pipeline <file>` builds a
   `Pipe` per step; it carries the lifetime markers from step to step
   since 2026-11-16, before that slotmerge replayed with markers at the
   wrong instructions and passed.)
2. **Which bytes.** The message lists the differing bytes (`-`: left at
   the seeded value) and, when the original did not return under the
   other stack fill, what it did instead.
3. **Where they come from.** `PLIRON_DIFFCHECK_WATCH=<hex address>
   PLIRON_TRACE=pliron::diffcheck=info` logs every load, store, copy and
   stub fill covering that byte in each run, with the instruction; a copy
   names its source byte, so watch that next. `PLIRON_DIFFCHECK_PATH=1`
   logs the blocks each run enters (diff the two for a step that keeps
   block numbers). A stack address is `stack_base + slot offset`: slots
   sit one per 4 KiB page in index order, larger ones on as many pages as
   they need.
4. **Is the original's value one the program determines?** A byte the
   original never wrote (an enum's payload behind a stored tag, copied
   whole) may differ. A byte written *ahead of the slot's
   `lifetime.start`* and read after it may not, but slotmerge is right
   to reuse it: the marker is wrong, and the pass that put the write
   there is upstream (`memcpyopt` until `7ee65e8`). Check the markers
   against the code before blaming a CLIF step.
5. **How old is the corpus?** A recorded function is the IR passes'
   output at recording time. `ls -lT target/corpus/<set> | head` against
   `git log` of `pliron-ir-opt/` and `pliron-clif-lower/`: a mismatch can
   be a bug fixed since. Re-record before spending time on one.

Fix direction: extend `OwnedFacts`/`corpus_dump` to serialize the remaining
`Pipe` fact tables (done for `scoped_na`, since 2026-11-13 including the
clone-entry block params that are `PLIRON_SCOPED_NOALIAS` candidates, and
for `block_region`/`region_parent`; the other map-valued tables need a
value/inst-keyed encoding).

## 3b. IR passes: ablation across two corpora (`clif-check xcorpus`)

Everything above checks the CLIF pipeline. A corpus file is a function
as lowered, after the pliron-IR passes, so two corpora of one build, one
with an IR pass switched off, differ by what that pass did, and the CLIF
interpreter can compare them:

```sh
harness/xcorpus.py --off PLIRON_MEMCPYOPT=0              # wasm kernels + std, ~2 min
harness/xcorpus.py --off PLIRON_SROA=0 std               # natively
harness/xcorpus.py --off PLIRON_MEMCPYOPT=0 rs tests/mcrdwrap/main.rs
harness/xcorpus.py --off PLIRON_MEMCPYOPT=0 --on PLIRON_MEMCPYOPT_RDWRAP=0 rs tests/mcrdwrap/main.rs
# by hand, any two corpora (PLIRON_CLIF_WASM=1 for wasm ones):
clif-check xcorpus <dirA> <dirB> out.json --pipeline [--fn SUBSTR] [--trials N] [--every N]
clif-check xpair a.clif b.clif --pipeline [--print]       # one pair, the replay line of a report
```

A is the reference (pass off), B the build under test. Functions pair by
the symbol in `self=`; one recorded in several CGUs pairs when the
copies match byte for byte but one. Each pair is checked A against B and
(`--pipeline`) A against the CLIF pipeline's output for B: a fact the IR
pass left stale (a lifetime marker) does harm only once `slotmerge`
trusts it. A mismatch says which, and `B against pipeline(B)` says
whether `report --check` on B alone would have shown it (then it is a
CLIF step, not the IR pass).

Statuses: `identical` (the pass did nothing), `ok`, `inconclusive`,
`mismatch`, `skipped: calls differ` (the two call different callees, or
a different number of indirect call sites: an inliner or devirt decision,
not comparable), `skipped: signature differs`, `only in A|B`,
`ambiguous`.

What the cross-build mode adds to the checker (`diffcheck::check_cross`):

- **Names by symbol.** Callees (`u0:N`) and data (`u1:N`) are a build's
  own ids. The corpus records `; diffcheck-syms:` (every external name's
  symbol) and stubs, function addresses and data regions are keyed by
  symbol.
- **Constant data.** `; diffcheck-data:` holds the bytes (and pointer
  relocations) of the immutable data a function names, at most 4 KiB
  each: `constload` folds a load in one build that the other performs.
- **`undef`.** `; diffcheck-undef:` lists the values the lowering wrote
  as 0 for `undef` (an `undef` constant, a promoted variable read before
  a write). Under the other stack fill they take that fill, so a byte
  made of one is not pinned.
- **What a callee reads.** A stub hashes, with its arguments, the bytes
  written so far behind each pointer argument (to the end of its slot or
  region, its `nocap` extent, 64 bytes; never an `sret` area). The second
  function's stubs read the first one's window and do what the first
  one's did (same fill lengths). Without it a value that only a callee
  reads is invisible (`tests/mcrdwrap`).
- **Calls before a sandbox stop.** When the original stops because the
  sandbox cannot follow (a copy out of the sandbox, fuel), the calls it
  made up to there must be made by B too. Most loop iterations end that
  way; without it a bug in a loop's second iteration is seen only by a
  trial that also returns.
- **Return area.** Bytes of the `sret` area the original left untouched
  are not compared.
- Byte-sized stub results and arguments are 0 or 1 (a `bool` of 46 is not
  one two functions must treat alike), and the stub's fills are words a
  program can go on with (pointers into the heap regions, small integers).

Default 256 trials (`report` uses 16; on a dsa-sized corpus pass
`--trials 64 --every 4`, 256 takes hours). Acceptance, 2026-11-16: with
`PLIRON_MEMCPYOPT_RDWRAP=0` it names `mcrdwrap::build` (call 5,
`core::fmt::write`, reads `{cap 0, ptr 1, len 0}` in A and `body`'s
header in B: the second iteration's `String` starts as a copy of
another) and is clean on the fixed tree; `docs/history/2026-11-16-xcorpus.md`
has the numbers.

What it cannot see: functions the inliner treated differently (skipped,
by design: on the wasm kernels + std that is 15 of ~800 changed functions
for memcpyopt, ~130 for sroa, ~250 for constload, which turns devirt off
with it); interprocedural facts (a pass that changes a *callee's*
behaviour shows only in the callee); stub behaviour (a callee is a hash,
not the real function); what the sandbox stops on. Known false positives,
all in ablations other than memcpyopt: values the program never makes
(a `bool` or an enum tag of 0x80 in seeded memory that sroa's or
constload's code treats differently from the original).

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

`harness/optbisect.py -- <check.sh>` automates the same oracle over
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
