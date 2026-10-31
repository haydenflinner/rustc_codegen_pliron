# Test harness

Goal: one runner that turns every correctness signal we have into a pass/fail
check against a checked-in baseline, then grow the set of signals with
generated programs, so miscompiles are found by tools rather than by users.

Entry points: `harness/quick.py` (the inner loop), `harness/gate.py`
(before merging to main; see AGENTS.md rule 1) and `harness/run.py`
(suites diffed against stock rustc; see `harness/run.py --help`).
There is no CI. `harness/gate.py --install-hook` installs a git
`pre-push` hook that runs the gate (bypass: `git push --no-verify`); it
is per clone and not installed by default. The
dated findings from building this harness, and its early plan, are in
`history/2026-10-18-test-harness-findings.md`.

## Status

| # | step | status |
|---|---|---|
| 1 | Unified runner, tiers, checked-in expectations, ICE collection | **done** (smoke, determinism, ui, fuzz suites) |
| 2 | Differential output vs stock LLVM rustc | **done** for smoke + ui: both compare exit code, stdout and stderr; the ui suite honors `run-fail`, `compile-flags` (allowlist), `edition`, `exec-env`, `unset-exec-env`, `rustc-env`, `unset-rustc-env`, `run-flags`, caches stock results in `target/ui/stock`, normalizes exe paths / panic thread ids / raw pointers / libtest ordering, records directive-unsupported tests as `skipped`, and covers `check-pass`/`build-pass`/`check-fail`/`compile-fail`/directive-free files as compile-agreement tests (both must agree on compile outcome; `accepted` = pliron compiles what stock rejects) |
| 3 | rustlantis fuzzing (differential, -O0 and -O) | **done**: suite `fuzz` (`--fuzz-seeds A:B`, generator from sibling `rustlantis` checkout built with `nightly-2025-08-01` since `box_patterns` was removed upstream; generated programs compile under the pinned nightly); seeds 0–512 all pass, baseline accepted (`fuzz.*.json`) |
| 4 | `-O` pass matrix, `PLIRON_OPT_BISECT=N`, `PLIRON_VERIFY=1` in the harness | mostly done: widened `-O` ui baseline accepted (`ui.*.O.json`, 16,518 pass / 21,482 files, no real failures; harness runs set `PLIRON_VERIFY=1`); `PLIRON_OPT_BISECT=N` gates every optimization-pass application globally (`PLIRON_OPT_BISECT_DEBUG=1` logs `bisect <n> <pass> run|skip`); fuzz suite `--matrix` reruns each seed with every `PLIRON_<PASS>=0` and requires identical output (status `matrix-<PASS>` on divergence) |
| 5 | abi-cafe cross-backend ABI tests | todo |
| 6 | mixed-backend (per-CGU / per-function) bisection tool | todo |
| 7 | self-host fixpoint (stage2 vs stage3 output identical) | todo; determinism of single compiles is done in step 1 |
| 8 | target matrix (x86_64-linux, aarch64-darwin, wasm32 via node, qemu/docker) | partial: expectations keyed by host; `ui-wasm` suite runs the UI corpus under wasm32-wasip1 (node WASI) differentially vs stock |
| 9 | crate corpus (`compatibility.md` rows) as a harness suite | todo |

## Fast inner loop: `harness/quick.py`

`harness/quick.py [--tier 0|1] [--base REF] [--files a,b] [--seeds N] [--dry]`.
Default base is `HEAD` (uncommitted + untracked; a clean tree uses
`HEAD~1..HEAD`; `--base main` for the whole branch). `--dry` prints the plan.

| tier | does | warm cost (loaded machine) |
|---|---|---|
| 0 | build clif-check (dylib too if rustc glue changed), unit tests of touched crates only (CLIF pass `x.rs`: lib tests + `tests/x*.rs`), corpus replay (smoke + std; falls back to the main checkout's `target/corpus` read-only, marked `*`) | 5-20 s |
| 1 | tier 0 + dylib + stanzas chosen from changed paths (+ SM_POISON twins for slotmerge/slotprom) + `wasm_smoke` + all `FUZZ_REGRESS` seeds + 24 fresh seeds (rolling window in `target/quick/state.json`) | ~2 min |

Selection: pass stem == stanza name, `tests/<dir>`, `PLIRON_<STEM>` mentions
in `test.sh`, plus the `ALIAS` table in `quick.py`; unmapped paths fall back
to CORE stanzas. The first run after a change compiles test binaries (60-100 s
for pliron-clif-opt cold); later runs are seconds. It refuses to start below
15 GB free.

Supporting changes: the fuzz suite caches the stock half of every seed
(`target/harness/fuzz-stock/<seed>.json`, keyed on generator + rustc; 40%
less per seed), prunes passing seed dirs (`PLIRON_FUZZ_KEEP=1` keeps them,
`PLIRON_FUZZ_NOCACHE=1` recomputes stock, `PLIRON_FUZZ_JOBS=N`), and
`harness/crates.py --deps stock` builds dependencies with stock rustc
(cargo `RUSTC_WORKSPACE_WRAPPER`, deps cached across backend changes) and
sends only workspace members through the backend (no doc-tests).

## Design

### Tiers

`harness/run.py --tier N` selects suites (`TIERS` in `harness/run.py`);
other suites (`fuzz`, `ui-wasm`) run with `--suite NAME`.

| tier | suites | budget |
|---|---|---|
| 0 | `smoke`, `determinism` | < 1 min |
| 1 | + `ui` | ~10–15 min |

Plan, not implemented: a tier 2 (library tests, the crate corpus, wasm)
and a tier 3 (self-host fixpoint), and a cadence of tier 0 per commit,
tier 1 before push, tier 2 nightly and tier 3 weekly. Nothing enforces
that cadence. What exists is AGENTS.md rule 1 (green `harness/gate.py`
before merging to main) and the optional pre-push hook above; the gate
runs only the `fuzz` suite (`fuzzreg`) from `run.py`.

### Expectations (ratchet)

Each suite writes `target/harness/<suite>/results.json` and is compared with
`harness/expectations/<suite>.<host-triple>[.<variant>].json`
(`{test: status}`; status `pass` or a failure kind).

- expected `pass`, now not `pass` → **regression** (non-zero exit), unless the
  new status is `env-*` (stock rustc fails too), which is reported as a warning.
- expected failure, now `pass` → improvement; re-run with `--accept` to record.
- test missing from the expectations and failing → **new failure** (non-zero exit).
- tests absent from this run (filters) are ignored.
- `noref` (smoke): stock rustc could not build or run the program, so only
  pliron's exit code was checked. It counts as passing.

`--accept` rewrites the expectations file from the current run. Review its git
diff like any other change.

### ICEs

All rustc invocations get `RUSTC_ICE=target/harness/ices`, so ICE dumps land
there instead of in the cwd.
