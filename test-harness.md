# Test harness plan

Goal: one runner that turns every correctness signal we have into a pass/fail
check against a checked-in baseline, then grow the set of signals with
generated programs, so miscompiles are found by tools rather than by users.

Entry point: `harness/run.py` (see `harness/run.py --help`).

## Status

| # | step | status |
|---|---|---|
| 1 | Unified runner, tiers, checked-in expectations, ICE collection | **done** (smoke, determinism, ui, fuzz suites) |
| 2 | Differential output vs stock LLVM rustc | **done** for smoke + ui: both compare exit code, stdout and stderr; the ui suite honors `run-fail`, `compile-flags` (allowlist), `edition`, `exec-env`, `unset-exec-env`, `rustc-env`, `unset-rustc-env`, `run-flags`, caches stock results in `target/ui/stock`, normalizes exe paths / panic thread ids / raw pointers / libtest ordering, records directive-unsupported tests as `skipped`, and covers `check-pass`/`build-pass`/`check-fail`/`compile-fail`/directive-free files as compile-agreement tests (both must agree on compile outcome; `accepted` = pliron compiles what stock rejects) |
| 3 | rustlantis fuzzing (differential, -O0 and -O) | **done**: suite `fuzz` (`--fuzz-seeds A:B`, generator from sibling `rustlantis` checkout built with `nightly-2025-08-01` since `box_patterns` was removed upstream; generated programs compile under the pinned nightly); seeds 0–512 all pass, baseline accepted (`fuzz.*.json`) |
| 4 | `-O` pass matrix, `PLIRON_OPT_BISECT=N`, `PLIRON_VERIFY=1` in CI | partial: `-O` ui baseline accepted (`ui.*.O.json`, 2886 pass, no real failures; harness runs set `PLIRON_VERIFY=1`); `PLIRON_OPT_BISECT=N` gates every optimization-pass application globally (`PLIRON_OPT_BISECT_DEBUG=1` logs `bisect <n> <pass> run|skip`); per-pass matrix todo |
| 5 | abi-cafe cross-backend ABI tests | todo |
| 6 | mixed-backend (per-CGU / per-function) bisection tool | todo |
| 7 | self-host fixpoint (stage2 vs stage3 output identical) | todo; determinism of single compiles is done in step 1 |
| 8 | target matrix (x86_64-linux, aarch64-darwin, wasm32 via node, qemu/docker) | partial: expectations are keyed by host triple |
| 9 | crate corpus (`compatibility.md` rows) as a harness suite | todo |

## What existed before

- `test.sh` — ~6 smoke programs; exit code only (except `unroll`, which diffs
  against a checked-in file).
- `tests/ui_run_pass.py` — ~2,600 directive-free `//@ run-pass` UI tests,
  exit code only, re-runs failures with stock rustc to label `env-*`.
- `./x test` on the self-host fork: UI, coretests, alloctests, std.
- `compatibility.md` — crate test suites, run by hand, results recorded as prose.
- ~25 `-O` passes behind `PLIRON_*` env toggles; `PLIRON_VERIFY=1` runs the
  Cranelift verifier.

Gaps: no machine-checked baseline; no generated programs; no systematic check
of the optimizer passes; UI runner skips directive tests and ignores output.

## Design

### Tiers

| tier | suites | budget | when |
|---|---|---|---|
| 0 | `smoke`, `determinism` | < 1 min | every commit |
| 1 | + `ui` (and later: fixed rustlantis seeds, pass matrix subset) | ~10–15 min | before push |
| 2 | (later) library tests, crate corpus, wasm | hours | nightly |
| 3 | (later) self-host fixpoint | hours | weekly / big changes |

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

## Next steps, in order

1. **Output differential for UI tests**: build each test with stock rustc as
   well (cache the LLVM binaries/outputs by source hash), compare stdout/stderr,
   and stop skipping `check-run-results`, `run-fail`, `compile-flags`,
   `edition`, `exec-env`, `run-flags`. Longer term: run compiletest itself
   (the fork's `./x test tests/ui` already does) and feed its results here.
2. **rustlantis**: `harness` suite `fuzz` that generates seeds `[a, b)`,
   builds with LLVM and pliron at `-O0` and `-O`, compares the printed
   checksum. Tier 1 runs a fixed seed range; a long-running campaign mode
   reduces failures (creduce/treereduce) into `tests/regress/`.
3. **Pass matrix**: re-run a tier-1 subset at `-O` with each `PLIRON_*` pass
   off in turn; a test that fails only with a pass on blames that pass. Add
   `PLIRON_OPT_BISECT=N` (apply only the first N transformations across all
   passes, like LLVM's `-opt-bisect-limit`) for O(log n) blame. Run with
   `PLIRON_VERIFY=1` everywhere in the harness.
4. **abi-cafe**: caller/callee pairs across pliron, stock rustc and C; key for
   the aarch64 sret / tail-CC special cases.
5. **Mixed-backend bisection**: build some CGUs with LLVM and some with
   pliron, link, and bisect to the miscompiled function.
6. **Self-host fixpoint**: stage2 (pliron-built) builds stage3; compare their
   output objects on a corpus byte for byte.
7. **Crate corpus** suite from `compatibility.md` rows, results ratcheted
   like the rest.

## Findings

- `src/licm.rs` miscompiled `ui/impl-trait/example-calendar.rs` at `-O`
  (SIGBUS/SIGTRAP, `PLIRON_LICM=0` was clean): `clobbers` only ran the precise
  overlap check on `notrap` stores, so a hoisted load crossed a plain store to
  the same stack slot. Fixed by overlapping-checking every store.
- `src/licm.rs` produced invalid IR across exception edges (45 verifier ICEs
  in the `-O` ui run): `split_edge` copied `BlockArg::TryCallRet`/`TryCallExn`
  arguments — which only `try_call` terminators may carry — onto a plain
  `jump`, and promotion appended ordinary version values to `try_call`
  edges. Fixed by refusing to split edges with non-`Value` args and by
  declining to promote when a loop edge can't be threaded through a
  trampoline block.
- `src/licm.rs` promotion miscompiled `ui/mir/mir_raw_fat_ptr.rs`: groups
  keyed by `(root, offset)` recorded one type, so an i8 access hid an i64
  access covering more bytes; the overlap check then missed the wide
  neighbor and promoted bytes the wide load still read from memory. Groups
  now track the widest byte extent and reject overlapping offsets, plus any
  group without uniform type or without a store.
- `src/licm.rs` promotion + store-sinking miscompiled
  `ui/coroutine/iterator-count.rs`: the access scan matched only
  `Opcode::Load`/`Opcode::Store`, so `uload8`/`sload*`/`istore*` (same
  `InstructionData::Load`/`Store` formats, different opcodes) were invisible
  — they neither rewrote nor poisoned the location and read stale bytes
  after the plain stores were deleted. Both scans now cover the whole
  load/store opcode families (extending loads and narrowing stores record
  their true byte width and mark the location unpromotable; unknown
  `can_load`/`can_store` ops bail out entirely).
- `src/lower.rs`: LLVM `\x01` verbatim symbols were stripped at declaration but
  the post-finish fixup searched the prefixed name — broke `linkme` sections
  during self-compile. Fixed by looking up the declared (stripped) name and
  dropping the platform mangling prefix instead.
- `src/indvars.rs`: `iconst` immediates must fit the type's unsigned range;
  `K*step` now gets masked (wrapping semantics unchanged).
- `iconst.i128` (found by rustlantis seed 6, verifier ICE): `iconst`'s result
  typevar only admits ints up to 64 bits, so materializing a wide constant
  needs `iconst.i64` + `uextend`/`sextend`/`iconcat`. Fixed in `indvars`
  (`emit_add`/`emit_lin`), `unroll` (folded results on >64-bit types bail),
  `jumpthread` (`Arg::K` limited to ≤64-bit types), `switchmap` (`Lin::Map`
  zero-extends the i64 offset) and `loopidiom` (`Ins::K` sign-extends).
- `src/switchmap.rs` (found by rustlantis seeds 4, 9, 12, 14, 20, 24, 31, 49,
  54, 57, 58 — 11 verifier ICEs): the `s == 0` constant path emitted
  `ireduce.i64` on a value already `i64`; `ireduce` must produce a strictly
  narrower type. Now `uextend` for >64, passthrough for ==64, `ireduce` for <64.
- `src/lower.rs` switch lowering: MIR `switchInt` on a `u128` value (e.g.
  `zext` of a negative `i64` enum discriminant) went through
  `cranelift_frontend::Switch::emit`, which subtracts the case-cluster
  minimum with a signed 64-bit immediate — correct mod 2^64, wrong on i128
  (`zext(-2) - (-2)` gave `2^64`, not `0`). i128 switches are now split into
  a hi-half switch feeding per-hi lo-half i64 switches. Found by
  `ui/mir/enum/negative_discr_ok.rs` (SIGABRT under pliron, pass under stock).
- `ui/abi/mir/mir_codegen_calls_variadic.rs`: deliberate assert for
  non-integer C-variadic args on aarch64-apple-darwin (same restriction as
  cg_clif). Stock rustc can't link it here anyway (`rust_test_helpers`
  missing), so it is `env-ice`.
- `tests/asm` on aarch64-apple-darwin: stock rustc fails to link it, because
  the `global_asm!` label `pliron_ga_add` has no Mach-O `_` prefix while
  `extern "C"` references are `_pliron_ga_add`. pliron links it anyway: the
  object has no undefined reference, so the reference was resolved to the
  unprefixed asm label inside the object. pliron accepts a program that LLVM
  rejects. It is recorded as `noref` until either the test or the symbol
  resolution changes.
