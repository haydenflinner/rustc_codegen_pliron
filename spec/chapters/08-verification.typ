#import "../pliron-spec.typ": *

#chapter("verify", 1)
= Verification

The model chapter states what the backend promises; this chapter states how
the project holds itself to those promises. Almost every check here is
#emph[differential]: the oracle is not a hand-written expected output but
#term("stock") rustc + LLVM on the same source and flags, or the same
function before and after a #term("step"). The checks form a ladder from
cheap to slow: the #term("quick-check") (seconds to minutes), the
#term("gate") that every merge to `main` passes, then the slow suites —
real crates, rustc's UI suite under a pliron-built stage 2, generated
programs, the debugger — and, apart from correctness, the
#term("parity-card"). Most of the machinery is Python and shell under
`harness/`, `test.sh` and `selfhost/`; the requirements below name the
script that carries each one.

#chapter-glance()

#note[
  Which machinery holds which L0 claim:
  #table(
    columns: (auto, 1fr),
    stroke: 0.4pt + luma(200),
    inset: 4pt,
    [*Claim*], [*Checked by*],
    [#refr("model.equiv")], [`test.sh` stanzas, corpus replay, diffcheck, fuzz seeds, UI suites, crates, stage-2 UI],
    [#refr("model.abi")], [stanzas against the prebuilt sysroot, `proc_macro`, `--sysroot`],
    [#refr("model.unwind")], [`unwind`, `unwindstd`, `tests/wasm/run.sh`],
    [#refr("model.det")], [`determinism` suite, `detcheck.py`, byte-compare stanzas],
    [#refr("model.dbg-neutral")], [`test.sh debuginfo`],
    [#refr("model.bail")], [compile timeouts in fuzz and the gate, corpus `panic`/`verifier` rows],
    [#refr("model.off-switch")], [fuzz `--matrix`, off-side stanzas, the landing rule],
    [#refr("model.targets")], [gate per host, `wasm_smoke`, `ui-wasm`, host-keyed expectations],
    [#refr("model.parity")], [`harness/parity.py` and its legs],
  )
]

== The merge gate

#r("verify.gate.green", title: "main stays green", level: "must",
   refines: ("model.equiv", "model.off-switch"),
   checked: "harness/gate.py --install-hook")[
  A change MUST NOT merge to `main` unless `harness/gate.py` exits 0 on it.
  A red gate on `main` MUST be fixed before new work lands. `--install-hook`
  installs a git `pre-push` hook that runs the gate; a push that bypasses
  it with `git push --no-verify` MUST say so in its commit message.
]

#r("verify.gate.legs", title: "Gate legs", level: "must", tier: 2,
   refines: "verify.gate.green", checked: "harness/gate.py")[
  The gate first runs one `cargo build -p rustc_codegen_pliron -p
  pliron-clif-check` (job `build`); if it fails, nothing else runs. It then
  runs these jobs in a thread pool (`--jobs`, default half the CPUs):
  - `unit`: `cargo test -q -p pliron-cfg -p pliron-clif-opt -p pliron-ir-opt`;
  - `<stanza>`: every discovered #term("test-stanza")
    (#refr("verify.gate.stanzas")) as `TEST_NOBUILD=1 ./test.sh
    --only=<stanza>`;
  - `poison:<stanza>`: the stanzas in `POISON_STANZAS` again, under
    `PLIRON_SM_POISON=1` with `TEST_OUT=target/tests-poison/<stanza>`
    (#refr("verify.poison"));
  - `fuzzreg`: `harness/run.py --suite fuzz --no-build --fuzz-seeds
    <FUZZ_REGRESS>` (#refr("verify.fuzz.regress"));
  - `corpus:<dir>`: one replay per directory under `target/corpus/`
    (#refr("verify.corpus.replay"));
  - `wasm`: `./test.sh --only=wasm` (`tests/wasm/run.sh`, which rebuilds
    `core`/`std` for wasm through the backend), only with `--full` or
    `--only wasm`; the default gate relies on the `wasm_smoke` stanza
    instead.
  `--record` re-records the `smoke` and `std` corpora first; `--skip` and
  `--only` filter by job name or by the prefix before `:`. Each job has a
  1800 s timeout, and a timeout fails the job. Logs go to
  `target/gate/<job>.log` and the summary to `target/gate/summary.json`.
  The gate exits nonzero when any job fails.
]

#r("verify.gate.stanzas", title: "Stanza discovery", level: "must", tier: 2,
   refines: "verify.gate.green", checked: "harness/gate.py stanzas()")[
  The gate MUST discover stanzas from `test.sh` itself, not from a list:
  every match of `want (\w+) && t_\w+`, except `sysroot` and `mold`, which
  need their own flags. A stanza added to `test.sh` in that form is gated
  with no further change. The quick check uses the same discovery.
]

#drift[
  `gate.py`'s docstring says it runs "every `./test.sh` stanza". The
  pattern's `\w+` does not match a hyphen, so the `lowerfree-wasm` stanza
  (`want lowerfree-wasm && t_lowerfree_wasm`) is never discovered: neither
  the gate nor the quick check runs it. The `wasm` leg is dispatched by an
  `if want wasm` block, which the pattern also misses; that one is
  deliberate (#refr("verify.gate.legs")).
]

#r("verify.gate.vacuous", title: "Missing tools make legs pass", level: "must",
   refines: ("model.equiv", "model.targets"), see: "verify.gate.green",
   checked: "harness/gate.py")[
  A gate run that licenses a merge MUST be made on a machine that has
  `node` with the `wasm32-unknown-unknown` target installed, the
  #term("rustlantis") generator, and recorded `smoke` and `std` corpora.
  Without them the gate still passes: `wasm_smoke` returns 0 when `node` or
  the target is missing, the fuzz suite returns no results (and so no
  regressions) when the generator is missing, and a corpus that was never
  recorded has no `corpus:` job.
]

#drift[
  `gate.py` says the `fuzzreg` job "reports a skip" without the generator.
  It reports `ok`: `run.py` prints a message to the job log and exits 0.
]

#r("verify.gate.hosts", title: "A gate run covers its host", level: "must",
   refines: "model.targets", checked: "harness/gate.py")[
  A gate run checks native code for the host it runs on, and wasm32 through
  `wasm_smoke` (and `wasm` with `--full`). Every other target in
  #refr("model.targets") is checked only by a gate run on a host of that
  target. Expectation files are keyed by host triple
  (#refr("verify.diff.ratchet")), and stanzas that cannot work against the
  stock SEH `std` on Windows are skipped there by `test.sh` itself.
]

#llvm(kind: "gap", summary: [The merge gate tests only the host it runs on (plus wasm32); rustc's merge queue builds and tests every tier-1 target before each merge.])[
  rustc's merge queue runs the test suite on every tier-1 target for every
  merge. Here a gate run covers the machine it runs on. Windows and Linux
  regressions are found when someone runs the gate on those hosts.
]

== The fast loop

#r("verify.quick.licence", title: "A quick check is not a merge licence", level: "must",
   refines: "model.equiv", see: "verify.gate.green", checked: "harness/quick.py")[
  A green #term("quick-check") MUST NOT be taken as permission to merge. Every
  quick run MUST end by printing what it skipped (`SKIPPED:`) and the slower
  commands that complete the proof (`TO COMPLETE THE PROOF:`): always the
  gate, and, by the areas touched, `gate.py --full` (wasm), a wide fuzz
  range, `crates.py` and `selfhost/verify.sh`.
]

#r("verify.quick.tiers", title: "Quick check tiers and selection", level: "must", tier: 2,
   refines: "verify.quick.licence", checked: "harness/quick.py")[
  `harness/quick.py [--tier 0|1] [--base REF] [--files a,b] [--seeds N]
  [--dry] [--no-build] [--keep-going]` derives a plan from the changed
  files (`git diff` against `--base`, default `HEAD` plus untracked files;
  a clean tree uses `HEAD~1..HEAD`):
  - #emph[tier 0] builds `clif-check` (and the backend dylib when rustc
    glue, `pliron-ir-opt`, `pliron-cfg`, `pliron-clif-lower` or `vendor/`
    changed), runs the unit tests of the touched crates only (for a CLIF
    pass `x.rs`: its lib tests plus the integration tests named after it,
    such as `tests/x.rs` and `tests/x_*.rs`), and
    replays the `smoke` and `std` corpora when `pliron-clif-opt`,
    `pliron-cfg` or `vendor/` changed. A worktree without corpora borrows
    the main checkout's, read-only.
  - #emph[tier 1] (the default) adds the dylib build, the stanzas the
    changed paths select, their poison twins when a `slotmerge` or
    `slotprom` file changed, `wasm_smoke`, and, when code changed, every regression seed
    (highest-yield first, `--fail-fast`), and `--seeds` fresh seeds
    (default 24) from a rolling window in `target/quick/state.json` that
    starts at 20000 and advances only when the fuzz job passes. Tier 1 runs
    only if tier 0 passed, unless `--keep-going`.
  A path selects a stanza when its stem equals the stanza name, it lies in
  `tests/<dir>/`, the stanza body names `PLIRON_<STEM>`, or the `ALIAS`
  table lists it. A code path that selects nothing falls back to the `CORE`
  stanzas. Changes only to docs, `harness/`, `bench/` or `selfhost/` run
  nothing. The script refuses to start with less than 15 GB free.
]

#drift[
  The `quick.py` docstring lists corpus replay as part of every tier 0 run.
  The code replays only when `pliron-clif-opt`, `pliron-cfg` or `vendor/`
  changed. That is correct for the corpus format (it is pre-pipeline CLIF,
  so lowering and IR-opt changes need a re-record, not a replay), but the
  docstring overstates it.
]

== End-to-end stanzas

#r("verify.stanza.layout", title: "One stanza per test directory", level: "must", tier: 2,
   refines: ("model.abi", "model.unwind"), checked: "test.sh")[
  Each end-to-end test MUST be a #term("test-stanza"): a function
  `t_<name>` in `test.sh` that compiles `tests/<dir>/` with
  `-Zcodegen-backend=target/debug/<dylib>` and runs the result, dispatched
  by a line `want <name> && t_<name>`. By default every stanza links
  against the prebuilt stock #term("sysroot"), which is what checks
  #refr("model.abi") on every gate run. `--sysroot` adds the `sysroot`
  stanza (`tests/sysroot` with `-Zbuild-std=std,panic_unwind`); `--mold`
  adds a cross-link to `*-linux-musl` with #term("wild") or mold.
  `TEST_NOBUILD=1` skips the `cargo build`; `TEST_OUT` moves the output
  directory so variant runs do not race the plain run.
]

#r("verify.stanza.oracle", title: "What a stanza checks", level: "must",
   refines: "model.equiv", checked: "test.sh")[
  A stanza MUST fail on a wrong program, not only on a crash. Its oracle
  MUST be one of: the program's own assertions (exit status), its output
  diffed against stock rustc's build of the same source or against a
  checked-in `expected.out` that matches stock's, a byte comparison of
  objects, or a property of the machine code (a frame size, an absent
  instruction pattern). A stanza that tests one transform SHOULD also
  check that the transform fired (for example `PLIRON_STATS=1` and a match
  on `edgespec [1-9]`), so a pass that silently stops firing does not pass
  vacuously.
]

#r("verify.stanza.off-side", title: "Flag-proving stanzas spell the off side", level: "must",
   refines: "model.off-switch", checked: ("test.sh lowerfree", "test.sh lowerfree-wasm"))[
  A stanza that compares a build with a knob on against a build with it off
  MUST set the off side explicitly to `PLIRON_<X>=0`, never leave it
  unset. Knobs land default-off and flip to default-on later
  (#refr("verify.rule.default-off")); a stanza that relies on "unset means
  off" silently compares on against on after the flip.
]

#r("verify.oracle.llvm-tools", title: "LLVM only as an oracle", level: "must",
   refines: "model.no-llvm", checked: "test.sh")[
  Verification scripts MAY use LLVM-based tools as oracles and inspectors:
  stock rustc for reference builds, and rustup's `llvm-objdump`, `llvm-ar`,
  `llvm-nm`, `llvm-objcopy`, `llvm-size` and `llvm-dwarfdump` to read
  objects. No verification script MAY put an LLVM tool into the path that
  produces the artifact under test.
]

== Process rules

#r("verify.rule.regress", title: "A miscompile fix ships a failing-first test", level: "must",
   refines: "model.equiv")[
  A change that fixes a miscompile MUST add a regression test, and the
  author MUST confirm that the test fails without the fix. Pass-level tests
  live next to the pass or in `pliron-clif-opt/tests/`; end-to-end ones are
  stanzas; a #term("fuzz-seed") that found the bug joins `FUZZ_REGRESS`
  (#refr("verify.fuzz.regress")). No script enforces the "fails without the
  fix" half; it is a review rule (`AGENTS.md` rule 3).
]

#r("verify.rule.default-off", title: "Land default-off, flip separately", level: "must",
   refines: "model.off-switch")[
  A new pass or transform MUST land default-off, enabled only by
  `PLIRON_<X>=1`. It MUST be turned on in a separate change, after the gate
  and the corpus replay are clean with it on. After the flip it MUST keep a
  `PLIRON_<X>=0` off switch. The two-change shape is what lets a bisect over
  history separate "the code landed" from "the code ran".
]

== Self-checking modes

#r("verify.poison", title: "Poison runs check slot liveness", level: "must",
   refines: "model.equiv", checked: "harness/gate.py poison:*")[
  With `PLIRON_SM_POISON=1`, slotmerge MUST merge no slots and instead
  overwrite each slot it would have considered with `0xA5` bytes (at most
  the first 256 bytes of each) right after its computed live interval ends
  and at the head of every block outside the interval entered from inside
  it. A wrong interval then fails at the first stale read in every run,
  whether or not a real merge would have reused the bytes. The gate runs
  the slot-heavy stanzas in `POISON_STANZAS` this way as #term("poison-run", [poison runs]).
]

#llvm(kind: "extra", summary: [Slot merging has a poison mode that turns it into its own use-after-scope sanitizer, run in every gate.])[
  LLVM has no mode that checks StackColoring's live ranges; a wrong range
  surfaces as a miscompile. Here the merge pass can poison every byte it
  claims dead, and the gate runs slot-heavy programs that way on every
  merge.
]

#r("verify.audit.wasm", title: "wasm peepholes are audited in the gate", level: "must",
   refines: "model.equiv", checked: ("tests/wasm/smoke.sh", "tests/wasm/run.sh"))[
  The wasm test scripts MUST run with `PLIRON_WASM_AUDIT=panic` unless the
  caller sets it. In that mode every `wpeep` copy-forward round is checked
  by the Datalog translation validator in `src/wpeep_audit.rs`, and a
  rejected round fails the build. Because `wasm_smoke` is a stanza, every
  gate run audits the rounds it executes.
]

#llvm(kind: "extra", summary: [The wasm copy-forward peephole is translation-validated per application, inside the build, on every gate run.])[
  LLVM validates no transformation inside a build; Alive2 checks LLVM IR
  transformations out of tree. Here each application of the `wpeep`
  copy-forward rewrite is checked, path-sensitively, against the
  assumptions the rewrite states, and a failed check fails the build.
]

== The corpus

#r("verify.corpus.record", title: "Recording a corpus", level: "must", tier: 2,
   refines: "verify.corpus.replay", checked: "harness/corpus.sh")[
  `harness/corpus.sh smoke|std|cargo DIR [ARGS]` MUST record, through the
  debug backend dylib with `PLIRON_CORPUS=<dir>`, the pre-pipeline CLIF of
  every optimized function at `-O`, one `<name>.<n>.clif` file each whose
  first line is the `; diffcheck-facts:` header. `smoke` builds each
  `tests/*/main.rs` with `-C codegen-units=1` into its own subdirectory;
  `std` builds `std`, `alloc` and `core` with `-Zbuild-std=std,panic_abort`
  in release; `cargo` builds any project in release. The output is
  `target/corpus/<set>` (or `OUT`), replaced on every recording, and
  recording fails if no function was written. A corpus MUST be re-recorded
  after a change to lowering or the IR-opt passes, since those change its
  input.
]

#r("verify.corpus.replay", title: "Replaying a corpus without rustc", level: "must",
   refines: ("model.equiv", "model.bail"), checked: "harness/gate.py corpus:*")[
  `clif-check report <dir> <out.json> --check` MUST run, on every corpus
  function and in parallel (`--jobs`), the default CLIF pipeline for the
  host ISA, then the Cranelift verifier on the result, then
  #term("diffcheck") of the pipeline's input against its output. Each
  function gets one status: `ok`, `inconclusive`, `mismatch`, `verifier`,
  `panic` or `parse`. A bad function becomes a row, never an abort. The
  replay fails on any `mismatch`, `verifier` or `panic` row; `parse` and
  `inconclusive` rows do not fail it. The JSON also carries per-function
  sizes and per-step times, which the live dashboard reads.
]

#llvm(kind: "extra", summary: [Every change can replay the whole optimizer over real functions (std and more) with an interpreter diff per function, in seconds and without rustc.])[
  LLVM's pre-merge checks run `lit` tests on hand-written IR; corpus-wide
  IR diffing exists only out of tree. Here the recorded pre-pipeline CLIF of
  every function in `std` (and any other recorded crate) goes through the
  full pipeline, the verifier and an interpreter comparison on every gate
  run.
]

#r("verify.corpus.known", title: "Known corpus failures", level: "must", tier: 2,
   refines: "verify.corpus.replay", checked: "harness/gate.py corpus_job()")[
  `harness/expectations/corpus-known.txt` lists #term("known-failure", [known failures]),
  one `<set>/<file>` per line, with `#` starting a comment. The gate and the
  quick check MUST fail a `corpus:<set>` job on any failing row not on the
  list and MUST ignore the rows on it. Every entry SHOULD carry a `#`
  reason that says why it is checker noise and not a miscompile.
]

#r("verify.corpus.fidelity", title: "A clean replay exonerates nothing", level: "must",
   refines: "model.equiv", depends: "verify.corpus.replay")[
  A clean corpus replay MUST NOT be taken as proof that a step is correct.
  The facts header carries `nonnull`, `noalias`, `scopedna`, `deref`,
  `frozen`, `nowrite`, `derived` and the function's own name, but not
  `operand_frozen`, `call_frozen`, `invariant`, the instruction-to-region
  map or `block_region`. A step whose decision reads those facts behaves
  differently in replay than in the real build, and a step that does
  nothing offline may still be the one that corrupts the function.
]

== Differential checking of steps

#r("verify.diffcheck.observe", title: "What diffcheck compares", level: "must",
   refines: "model.equiv", checked: "pliron-clif-opt/tests/diffcheck.rs")[
  Diffcheck MUST run a function before and after a transformation in the
  Cranelift interpreter on generated inputs and compare everything a caller
  could observe: the return values, the sequence of calls made (callee and
  arguments), and the final value of every memory byte that differs from
  its seed. Inputs MUST respect the facts the optimizer was allowed to
  assume (`nonnull`, `noalias`, dereferenceable bytes, frozen memory,
  `nowrite` callees), so a pass that relies on them is not flagged. Calls
  hit deterministic stubs that record them; `memcpy`, `memmove`, `memset`
  and `memcmp` run for real. Any two NaNs compare equal. The defaults are
  16 trials, 200 000 interpreter steps of fuel and seed `0x5eed`
  (`PLIRON_DIFFCHECK_TRIALS`, `_FUEL`, `_SEED`).
]

#r("verify.diffcheck.inconclusive", title: "Only a completed original can accuse", level: "must",
   refines: "model.equiv", depends: "verify.diffcheck.observe",
   checked: "pliron-clif-opt/tests/diffcheck.rs")[
  A trial MUST be #term("inconclusive"), never a mismatch, when the original
  traps (undefined behaviour, which a transformation may refine away), runs
  out of fuel, or meets anything the sandbox cannot model; when the
  transformed function meets something the sandbox cannot model; when the
  signature changed; and when the original's output changes under a
  different stack fill (`0xA5`), that is, it depends on uninitialized stack
  bytes. When the original returned, a transformed function that traps,
  runs out of fuel or returns something else MUST be reported as a
  mismatch. An inconclusive result MUST NOT be read as clearing the step.
]

#r("verify.diffcheck.modes", title: "Diffcheck modes", level: "must", tier: 2,
   refines: ("verify.diffcheck.observe", "model.diag-observe"), checked: "pliron-clif-opt/src/pipeline.rs")[
  `PLIRON_DIFFCHECK` selects the mode per function: `0` or unset is off;
  `step` checks every step whose output text differs from its input;
  any other value (`1`) checks the pipeline's input against its output
  once and, on a mismatch, replays the pipeline from the start with
  per-step checks to name the step. After that replay the real result,
  derived-pointer table and statistics MUST be put back, so the build
  continues exactly as without checking. `PLIRON_DIFFCHECK_FN=<substr>`
  limits checking to matching function names; `PLIRON_DIFFCHECK_PANIC=1`
  turns a report into a panic.
]

#r("verify.diffcheck.repro", title: "A mismatch writes its own reproducer", level: "must", tier: 2,
   refines: "verify.diffcheck.observe", checked: "clif-check step")[
  On a mismatch diffcheck MUST write `before.clif`, `after.clif` and
  `report.txt` to `<PLIRON_DIFFCHECK_DIR>/<fn>.<step>/` (default directory
  `pliron-diffcheck`; the function name is reduced to at most 80
  `[A-Za-z0-9_]` characters). Both CLIF files start with the facts header.
  `report.txt` names the function, the step, the trial's arguments, both
  outcomes and the command that reproduces it without rustc: `clif-check
  step <step> before.clif`, or `clif-check pipeline before.clif` when no
  single step was named.
]

#llvm(kind: "extra", summary: [Per-step differential interpretation: any real build can check each CLIF step's input against its output by execution and emit a one-second reproducer.])[
  LLVM's `-verify-each` checks that IR is well formed after each pass, not
  that it means the same thing. Alive2 proves LLVM IR transformations
  correct, but it runs out of tree on `opt` invocations, not inside a real
  build of a real crate. Here `PLIRON_DIFFCHECK=step` can run under a whole
  bootstrap and name the step that changed a function's behaviour.
]

#llvm(kind: "gap", summary: [No formal verification: diffcheck samples inputs and can find bugs, but proves no peephole correct, as Alive2 does for LLVM's InstCombine.])[
  A diffcheck mismatch proves a bug; a clean run proves nothing. LLVM's
  InstCombine peepholes are routinely proved with Alive2 before they land.
  No CLIF rewrite here has a machine-checked proof; the nearest thing is
  the per-application wpeep audit (#refr("verify.audit.wasm")). The
  interpreter also runs on the host ISA, so functions with 32-bit wasm
  pointers come back inconclusive.
]

== Triage and bisection

#r("verify.triage.order", title: "Miscompile triage order", level: "should",
   refines: "model.equiv")[
  A miscompile SHOULD be narrowed cheapest-first:
  + write an #term("oracle") that rebuilds the suspect and exits nonzero
    exactly when the symptom reproduces;
  + one build with `PLIRON_DIFFCHECK=step` (and `PLIRON_VERIFY=full`);
  + if that is inconclusive, `harness/fnbisect.py` to one function;
  + corpus replay of the suspect crate, minding #refr("verify.corpus.fidelity");
  + `harness/bisect.py` over `PLIRON_OPT_BISECT`, or a prefix bisect of
    `PLIRON_PIPELINE` confirmed by a full-minus-one run, since badness is
    not monotone in the prefix length;
  + `PLIRON_TRACE=pliron::<step>` on the real build, not the replay.
  Every bisector accepts any failure, so the minimized result MUST be
  checked against the original symptom.
]

#drift[
  `AGENTS.md` orders triage as "diffcheck first, corpus replay next, rebuild
  bisects last". `docs/miscompile-triage.md`, the more detailed of the two,
  puts the function-level rebuild bisect before corpus replay because of
  the replay fidelity gap. This specification follows the triage document.
]

#r("verify.bisect.unit", title: "Transform bisection", level: "must", tier: 2,
   refines: ("verify.triage.order", "model.off-switch"), checked: "harness/bisect.py")[
  `harness/bisect.py [--max N] [--timeout S] -- <cmd>` MUST refuse to run
  unless `<cmd>` fails with no limit and passes with `PLIRON_OPT_BISECT=0`.
  It then binary-searches the smallest failing `N`, reruns it with
  `PLIRON_OPT_BISECT_DEBUG=1`, and names unit `N` from the
  `bisect <n> <pass> run|skip <func>` lines that `pliron-cfg` logs. One unit
  is one guarded pass application. The numbering is deterministic only
  under `-C codegen-units=1` or `PLIRON_PAR=0`, and `<cmd>` must force a
  rebuild, since cargo does not track `PLIRON_*` variables.
]

#r("verify.bisect.fn", title: "Function bisection", level: "must", tier: 2,
   refines: ("verify.triage.order", "model.off-switch"), checked: "harness/fnbisect.py")[
  `harness/fnbisect.py [--workdir D] [--names F] -- <cmd>` MUST check that
  `<cmd>` fails with every function optimized (logging names through
  `PLIRON_OPT_FNS_LOG`), passes with none (`PLIRON_OPT_FNS=0..0`), and fails
  with only the logged names, before it delta-debugs the name list through
  `PLIRON_OPT_FNS=@file`. When the failure needs functions from both halves,
  it finds a minimal member of each. The selector keys on the high 32 bits
  of the symbol's 64-bit FNV-1a hash, so it agrees across threads,
  #term("cgu", [CGUs]), rustc processes and rebuilds; unselected functions keep
  their unoptimized bodies while interprocedural rewrites still see every
  function.
]

== Differential suites

#r("verify.diff.stock", title: "Stock rustc is the oracle", level: "must",
   refines: "model.equiv", checked: "harness/run.py")[
  Each case of a differential suite MUST be built and run by both stock
  rustc and this backend from the same source and flags, and MUST pass only
  when the exit status, stdout and stderr agree after normalization (the
  executable path becomes `EXE`, panic thread ids are dropped; the UI suite
  also masks raw pointers and libtest ordering). When stock fails too, or
  its own reference is unstable, the case gets an #term("env-status") and
  is never counted as a regression. A case stock rejects and this backend
  compiles is `accepted`, a real divergence.
]

#llvm(kind: "choice", summary: [Correctness is judged against stock's behaviour on the same input, not against checked-in absolute expectations as rustc's compiletest does.])[
  rustc's compiletest compares against checked-in `.stderr` and
  `.run.stdout` files. Here the reference is what stock does. A bug LLVM
  shares is invisible, and a case where stock is wrong (`f128` on
  `aarch64-apple-darwin` goes through `long double` libcalls, which are
  `f64` there) shows up as a divergence that has to be classified by hand.
]

#r("verify.diff.suites", title: "The run.py suites", level: "must", tier: 2,
   refines: ("verify.diff.stock", "model.det"), checked: "harness/run.py")[
  `harness/run.py [--tier 0|1 | --suite NAME ...] [--accept] [--opt]
  [--filter S] [--no-build]` MUST provide these suites, all with
  `PLIRON_VERIFY=1` unless the caller sets it:
  - `smoke`: `nostd`, `std`, `unwind`, `asm` and `unroll` at `-O0` and `-O`,
    plus `proc_macro` (built here, loaded by stock rustc);
  - `determinism`: each smoke program compiled to an object twice with
    `-Ccodegen-units=1`; any byte difference is `nondeterministic`;
  - `ui`: rustc's run-pass and run-fail UI tests through
    `tests/ui_run_pass.py`, compile-only tests as agreement on the compile
    outcome; `--opt` adds `-O` with its own expectations;
  - `ui-wasm`: the same corpus for `wasm32-wasip1`, this backend's objects
    linked by #term("pliron-wasm-ld") against a pliron-built sysroot, both
    sides run under node's WASI;
  - `fuzz`: #refr("verify.fuzz.seeds").
  Tier 0 is `smoke` and `determinism`; tier 1 adds `ui`. ICE dumps go to
  `target/harness/ices`.
]

#drift[
  `docs/test-harness.md` plans tier 0 for "every commit" and tier 1 "before
  push". Nothing enforces either: the pre-push hook runs the gate, and the
  gate runs only the `fuzz` suite (`fuzzreg`) from `run.py`.
]

#r("verify.diff.ratchet", title: "Expectations ratchet", level: "must", tier: 2,
   refines: "verify.diff.stock", checked: "harness/run.py compare()")[
  Each suite's results MUST be compared with
  `harness/expectations/<suite>.<host-triple>[.O].json`, a map from test to
  status. A test expected to pass that now fails, and a test missing from
  the file that fails, are failures, except for #term("env-status")
  results and `skipped`. `pass` and `noref` (stock could not provide a
  reference, the backend's build ran cleanly) both count as passing. A test
  that now passes is reported and recorded only with `--accept`, which
  rewrites the file (merging into it under `--filter`). A suite with no
  expectations file fails. Tests absent from a filtered run are ignored.
]

#r("verify.fuzz.seeds", title: "Generated programs", level: "must", tier: 2,
   refines: ("verify.diff.stock", "model.bail"), checked: "harness/run.py --suite fuzz")[
  For each #term("fuzz-seed") the fuzz suite MUST generate one
  #term("rustlantis") program and build it with `-Zmir-opt-level=0
  -Ccodegen-units=1` at `-O0` and `-O` under both compilers. If stock's two
  builds disagree the seed is `env-stock-mismatch`; otherwise each pliron
  build's exit status and stdout must equal stock's at the same level. A
  compile over 300 s (`compile-timeout`) or a run over 60 s fails the seed,
  which is how an unbounded fixpoint shows up. The stock half is cached in
  `target/harness/fuzz-stock/<seed>.json`, keyed on the generator binary
  and `rustc -vV`; seed directories are kept only for real failures.
  `--fuzz-seeds` takes `A:B`, single seeds, or a comma list of both.
]

#note[
  rustlantis programs are not wasm-portable (64-bit `isize` transmutes), so
  the wasm path gets no generated-program coverage; it relies on `ui-wasm`,
  the wasm stanzas and the bench kernels.
]

#r("verify.fuzz.matrix", title: "Each pass off, same output", level: "must",
   refines: "model.off-switch", checked: "harness/run.py --suite fuzz --matrix")[
  With `--matrix`, every seed MUST also be rebuilt at `-O` once per pass in
  `MATRIX_PASSES`, with `PLIRON_<PASS>=0`, and produce stock's `-O` exit
  status and output. A divergence is reported as `matrix-<PASS>`. This is
  the direct check that the output with a pass disabled is still correct.
]

#r("verify.fuzz.regress", title: "Seeds that found bugs run on every merge", level: "must",
   refines: "model.equiv", see: "verify.rule.regress", checked: "harness/gate.py fuzzreg")[
  A seed that exposed a real bug MUST be added to `FUZZ_REGRESS` in
  `harness/gate.py`, with the bug named in the comment above it. The gate
  runs the whole bank as `fuzzreg`. Tier 1 of the quick check runs it too,
  ordered by historical yield (`FUZZ_YIELD`), with `--fail-fast` so a
  failing seed stops the queue.
]

#llvm(kind: "extra", summary: [A differential program generator (rustlantis) runs against stock in every merge gate, with a bank of seeds that found real bugs.])[
  Neither rustc's CI nor LLVM's pre-merge checks run a differential program
  generator; LLVM's in-tree fuzzers (`llvm-opt-fuzzer`, `llvm-isel-fuzzer`)
  run outside the merge path. Here generated programs are built by both
  compilers at two optimization levels on every merge.
]

== Real crates

#r("verify.crates.pliron-only", title: "Crates' own test suites", level: "must",
   refines: "model.equiv", checked: "harness/crates.py")[
  `harness/crates.py [--tier N] [--only a,b]` MUST run each pinned crate's
  own test suite with `cargo test --no-fail-fast` (`--release` unless
  `--debug`) once with stock rustc and once with this backend, and report
  the #term("pliron-only", [pliron-only failures]): failing test names,
  and crashed test binaries as `<signal: N>`, that appear only on the
  backend's side. A crate's status is `ICE`, `BUILD`, `TIMEOUT`, `FAIL`,
  `skip` (stock cannot build it), `STOCKTO` (stock timed out) or `ok`; the
  script exits nonzero on `ICE`, `BUILD`, `FAIL` or `TIMEOUT`. Crate
  versions (or commits, for tier 3's applications) are pinned in `CRATES`
  and bumped deliberately, so a new failure means a compiler change.
  Results go to `target/crates/report.md` and `report.json`.
]

#r("verify.crates.modes", title: "Crate run modes", level: "must", tier: 2,
   refines: "verify.crates.pliron-only", checked: "harness/crates.py")[
  With `--deps pliron`, the backend side MUST set both `RUSTFLAGS` and
  `RUSTDOCFLAGS` to `-Zcodegen-backend=<dylib>`, so dependencies and
  doc-tests go through the backend. With `--deps stock` (the default),
  dependencies are built by stock rustc and cached across backend changes,
  only workspace members go through the backend (`RUSTC_WORKSPACE_WRAPPER`),
  doc-tests stay stock, and both sides build with
  `CARGO_PROFILE_RELEASE_LTO=off`. The dylib is the release one unless
  `--profile` says otherwise. Stock results are cached in
  `target/crates/stock-cache/`, keyed on crate, version, arguments, profile
  and `rustc -vV`; timed-out or unbuildable stock runs are not cached.
]

#drift[
  The `crates.py` docstring and `AGENTS.md` say doc-tests go through the
  backend. That holds only with `--deps pliron`; the default is
  `--deps stock`, under which doc-tests and dependencies are built by stock
  rustc. `quick.py` prints `crates.py --tier 1 --deps pliron` as the
  command that completes the proof.
]

== Self-hosting

#r("verify.selfhost.stage2", title: "rustc's UI suite under a pliron-built rustc", level: "must",
   refines: ("model.equiv", "model.backend"), checked: "selfhost/verify.sh")[
  `selfhost/setup.sh` MUST build a rust worktree at the pinned nightly's
  commit with `codegen-backends = ["pliron"]`: stage 1 rustc is built by
  stage 0 (LLVM), and stage 1 `std` and the stage 2 compiler are built by
  this backend. `selfhost/verify.sh [WORKTREE] [SUITE]` runs the
  compiletest suite (default `tests/ui`, with `--force-rerun`) at stage 2
  and at stage 1. Both stages compile the test programs through this
  backend, so the #term("stage2-only", [stage-2-only failures]) isolate
  miscompilations of rustc itself; they are written to
  `verify-out/regressions`. If either run's log lacks a `test result:`
  line, the comparison MUST be refused.
]

#r("verify.selfhost.watchdog", title: "Self-host run hygiene", level: "must", tier: 2,
   refines: "verify.selfhost.stage2", checked: "selfhost/verify.sh")[
  While a stage runs, a watchdog MUST, every 60 s, kill stage-1 and stage-2
  rustc processes from this worktree that have run 5 minutes or more at
  under 0.5 % CPU; a miscompiled rustc can deadlock instead of crashing,
  and the killed tests then show up as ordinary failures. The backend
  revision the stage 2 was built from (`.pliron-src-rev`, written by
  `setup.sh`) is copied to `verify-out/backend-rev`. `--stage1-only` and
  `--stage2-only` run one stage and skip the comparison. A completed
  stage's failure list is cached by the SHA-256 of its rustc binary and the
  suite tree (`VERIFY_NOCACHE=1` disables it).
]

== Debug info

#r("verify.dbg.neutral", title: "-g changes no code, checked", level: "must",
   refines: "model.dbg-neutral", checked: "test.sh debuginfo")[
  The `debuginfo` stanza MUST build one program at `-O` with and without
  `-g` and fail if the two text sections differ. It also checks backtrace
  line numbers at `-O0` and `-O2`, verifies the DWARF with `llvm-dwarfdump
  --verify` when available, and checks variables through `lldb` at `-O0`
  and `-O2` when `lldb` is on `PATH`. It is skipped on Windows.
]

#r("verify.dbg.vs-stock", title: "Debugger experience against stock", level: "should",
   refines: "model.dbg-neutral", checked: ("harness/dbgsuite.py", "harness/dbgcorpus.py"))[
  Debug-info quality SHOULD be measured against stock, not in absolute
  terms. `harness/dbgsuite.py` runs rustc's `tests/debuginfo` suite under
  lldb for both compilers, reading `compile-flags`, `aux-build`, revisions
  and `lldb-command`/`lldb-check` directives as compiletest does; `--save`
  and `--diff` ratchet the pass set in `harness/expectations/`.
  `harness/dbgcorpus.py` stops at every `// BREAK` line of the programs in
  `harness/dbgcorpus/` and compares backtraces and `frame variable` values
  with stock's, variable by variable (addresses masked).
]

#llvm(kind: "gap", summary: [Debug info is exercised only under lldb on aarch64-apple-darwin; gdb and cdb are untested.])[
  rustc's CI runs the debuginfo suite under gdb, lldb and cdb. Here
  `dbgsuite.py` treats `gdb`, `cdb`, `linux`, `windows` and `x86_64`
  conditions as false on its host, and the `debuginfo` stanza drives only
  lldb. No check here runs gdb.
]

== The parity card

#r("verify.card.rating", title: "Card categories and rating", level: "must", tier: 2,
   refines: "model.parity", checked: "harness/parity.py")[
  `harness/parity.py` MUST reduce each category to one ratio
  $r = "pliron" slash "LLVM"$ (lower is better) and rate it
  $max(0, min(150, 100 + 50 log_2 (1 slash r)))$. The categories and their
  weights are the `CARD` table: `SPD` runtime speed 30 (geomean of the
  native and wasm bench geomeans), `CMP` compile time 15, `MEM` peak
  compile memory 10, `SIZ` code size 10 (machine-code bytes from
  `ctime.py`, plus the wasm bench module), `STK` stack frames 15
  (`frames.py`), `COR` correctness 20. The overall rating is the weighted
  mean of the ratings of the categories that have data; categories without
  data are left out and listed as "no data". The legs (`native`, `wasm`,
  `ctime`, `stack`) run one after another; `--skip` drops legs and
  `--from-logs` re-parses saved logs.
]

#r("verify.card.attribute", title: "Correctness is an attribute, not a gate", level: "must",
   refines: "model.parity", checked: "harness/parity.py correctness()")[
  The `COR` category MUST be computed from the latest saved results of the
  slow suites, which the card does not rerun, and MUST show the age of each.
  Per suite the ratio is: UI, $max(1, (s_2 + 1) slash (s_1 + 1))$ over
  stage-2-only and stage-1-only failures, used only when stage 2 completed;
  crates, the pliron-only test count (a crate with a failing status and no
  named test counts one) plus 1, used only for a report of at least 10
  crates; fuzz, the non-passing seed count plus 1, used only for at least
  100 seeds. `COR` is the geomean of the available ratios. A correctness
  loss lowers the card; it does not block a merge. The gate is what blocks.
]

#llvm(kind: "choice", summary: [Correctness against shared suites is one weighted category of a score, traded visibly, while the merge gate separately blocks known miscompiles.])[
  For LLVM a miscompile is a bug, not a score. Here the gate blocks every
  known miscompile it can reproduce, while the card's `COR` category prices
  the failures the slow suites find. One pliron-only crate failure rates
  `COR` 50.
]

#drift[
  `docs/PARITY.md` describes the fuzz input as "failing seeds". The code
  counts every status other than `pass`, so #term("env-status") seeds,
  which `run.py` never treats as failures, lower `COR` too. The UI ratio's
  floor of 1 is in the code's comment but not in the document.
]

#r("verify.perf.runtime", title: "Runtime measurement with significance", level: "should",
   refines: "model.parity", checked: ("bench/run.sh", "bench/wasm/run.sh"))[
  Runtime comparisons SHOULD report significance, not a best-of-N.
  `bench/run.sh [filter]` builds the criterion kernels with stock rustc,
  saves them as baseline `llvm`, rebuilds only the bench crate through the
  backend (`cargo clean -p pliron-bench`, since cargo does not track the
  dylib), and runs it against that baseline, so criterion prints per-kernel
  change and significance. `bench/wasm/run.sh [filter]` builds the same
  kernels for `wasm32-unknown-unknown` (stock with its `std`; pliron with
  `-Zbuild-std` and #term("pliron-wasm-ld")) and times them in node with
  interleaved rounds, reporting the median per-round ratio, and with
  `--layouts=N` the median across code layouts. Changes within ±3 % are
  parity, and only on a quiet machine.
]

#r("verify.perf.ctime", title: "Compile time, memory, size and frames", level: "should",
   refines: ("model.parity", "model.linear"), checked: ("harness/ctime.py", "harness/frames.py"))[
  `harness/ctime.py` SHOULD time single rustc invocations at `-O` with
  default codegen units on fixed inputs (`kernels`, `kernels-wasm`,
  `regex-syntax`, `regex-wasm`, `syn`), stock against the release dylib,
  taking the median of `--runs` (default 3) wall times and peak RSS (from
  `wait4`) and the text or wasm `CODE` bytes of every object written.
  `harness/frames.py` builds one input (default `regex-syntax`) at `-O`
  with one CGU per side, reads each function's frame from its prologue, and
  rates total pliron bytes over total LLVM bytes on the functions both
  define.
]

#r("verify.live", title: "Live corpus dashboard", level: "may", tier: 2,
   refines: ("model.equiv", "model.linear"), checked: "harness/live/live.py")[
  `harness/live/live.py [--port 8765] [--no-check] [--corpus a,b]` MAY be
  run while editing passes. On every save under `pliron-clif-opt/src`,
  `pliron-cfg/src` or `pliron-clif-check/src`, it rebuilds `clif-check`,
  replays each corpus with `report --check`, and shows new failures with
  their repro command, size movers and per-step times against a saved
  baseline. It is an aid, not a check: nothing reads its verdict.
]
