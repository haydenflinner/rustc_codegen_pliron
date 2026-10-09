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
| 4 | `-O` pass matrix, `PLIRON_OPT_BISECT=N`, `PLIRON_VERIFY=1` in CI | mostly done: widened `-O` ui baseline accepted (`ui.*.O.json`, 16,518 pass / 21,482 files, no real failures; harness runs set `PLIRON_VERIFY=1`); `PLIRON_OPT_BISECT=N` gates every optimization-pass application globally (`PLIRON_OPT_BISECT_DEBUG=1` logs `bisect <n> <pass> run|skip`); fuzz suite `--matrix` reruns each seed with every `PLIRON_<PASS>=0` and requires identical output (status `matrix-<PASS>` on divergence) |
| 5 | abi-cafe cross-backend ABI tests | todo |
| 6 | mixed-backend (per-CGU / per-function) bisection tool | todo |
| 7 | self-host fixpoint (stage2 vs stage3 output identical) | todo; determinism of single compiles is done in step 1 |
| 8 | target matrix (x86_64-linux, aarch64-darwin, wasm32 via node, qemu/docker) | partial: expectations keyed by host; `ui-wasm` suite runs the UI corpus under wasm32-wasip1 (node WASI) differentially vs stock |
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
- `optimize_and_codegen_fat_lto`/`run_thin_lto` panicked with `unimplemented!`
  instead of reporting a clean error (`tests/ui/lto` corpus). Now a
  `dcx().fatal`.
- `mir/issue-109004-drop-large-array.rs`: a function exceeding cranelift's
  entity-capacity limits (`ImplLimitExceeded`) produced an ICE that also
  dumped the multi-MB function into the panic message. `define_function`
  failures for `ImplLimitExceeded`/`CodeTooLarge` now report
  `function too large` via `dcx().fatal` — recorded in the baseline as a
  known `compile-error` limitation (stock rustc accepts it).
- `src/bcheck.rs` produced invalid IR on
  `ui/allocator/alloc-shrink-oob-read.rs` (`try_call ... uses value arg from
  non-dominating block528` verifier ICE): the clone region kept a block
  shared whenever it had any pred outside the region, but the cloned edge
  into that shared block adds a path that bypasses the region entirely — so
  any use of a region value in the shared block's downstream cone (here
  `try_call` args threaded from a cloned preheader's params) loses its
  dominator. The region closure now also absorbs a boundary target when a
  region value is used anywhere reachable from it without re-entering the
  region; cones larger than `MAX_REGION_EXTRA` decline to version.
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
- `src/lower.rs` f16/f128 (`scalar_size` has no f16/f128 rules on aarch64 —
  raw `fcmp`/`fadd`/`fsub`/`fmul`/`fdiv`/convs ICE'd; found by
  `ui/consts/const_in_pattern/f16-f128-const-reassign.rs` plus a reproducer):
  - `fcmp.f16` promotes both operands via `__extendhfsf2` and compares as
    f32; f128 already used the `__*tf2` ordered/unordered helpers.
  - f16 arithmetic (`fadd`/`fsub`/`fmul`/`fdiv`) promotes to f32 and
    truncates back — compiler-builtins' hf set is incomplete on this
    toolchain (`__divhf3`, `__fix*hf*`, `__float*hf` all missing); f128 uses
    `__addtf3`/`__subtf3`/`__multf3`/`__divtf3` (all present).
  - `frem`: f16 promotes to f32 -> `fmodf` -> `__truncsfhf2`; f128 calls
    `fmodf128` (compiler-builtins' libm port exports it).
  - `fcvt_sat`: f16 sources extend to f32 and reuse that path; f128 and
    i128 targets use the `__fix{,uns}{sf,df,tf}{si,di,ti}` matrix (the old
    code called `__fixdfti` for *any* non-f32 float -> i128 — a silent ABI
    bug for f128).
  - int -> float: f16 goes int -> f32 -> `__truncsfhf2` (`__float*hf` don't
    exist); f128 uses `__float{un,}{s,d,t}itf` selected by the *resized*
    operand type.
  - `llvm.fabs`/`llvm.copysign` on f16/f128 are bitwise (sign-bit
    `band`/`bxor`), like the existing `fneg` path.
  - `llvm.{sqrt,floor,ceil,trunc,roundeven,fma,fmuladd,minimum,maximum}` on
    f16/f128 call compiler-builtins' libm port (`sqrtf16`, `fmaf128`,
    `fminimumf128`, ...). `llvm.round` gets `roundf16`/`roundf128`.
- `src/intrinsic.rs`: `sqrtf128`-style intrinsics ICE'd at
  `must be overridden by codegen backend` — the `float`/`libm` name maps
  only knew f32/f64. Added f16/f128 entries; `powif128` -> `__powitf2`,
  `minimum_number_nsz_f*` -> `fminimum_numf*`, `powf16`/`powif16` promote
  through f32 (`powf`/`__powisf2` + `__truncsfhf2` via `fpext`/`fptrunc`,
  matching LLVM's soft-f16 promotion). `powf128` emits `powf128` — no such
  symbol exists in compiler-builtins or libm.dylib, so it is a link error
  on macOS (honest failure; glibc ships it).
- `umin.i128`/`smin.i128`/etc. ICE'd aarch64 lowering (`put_in_reg` unwrap in
  `isle.rs`, found self-hosting `rustc_apfloat`'s `from_decimal_string`, then
  by a `u128::min` reduction loop). Two sources: our passes can emit scalar
  min/max on i128 accumulators (loopvec's reduction epilogue), and
  cranelift's own egraph — run inside `define_function` because
  `opt_level=speed_and_size` — canonicalizes `select(icmp)` -> `umin`
  unconditionally (`vendor/cranelift-codegen/src/opts/selects.isle`), a
  rule set with no width guard while no target lowers scalar min/max >64
  bits. Fixed both ends: the vendored select->min/max rules now require
  `fits_in_64` + `ty_int`, and `clifpeep::wide_minmax` rewrites any
  `min/max.i128` left in the function into `icmp` + `select` (both lowerable
  on i128) just before `define_function`.
- Upstream divergences where pliron is *more* correct than stock on
  aarch64-apple-darwin (LLVM lowers f128 to `long double` libcalls —
  `fmodl`/`sqrtl`/`floorl`/... — but arm64 macOS `long double` is f64, so
  the ABI silently truncates and stock returns wrong values; e.g.
  `7.5f128 % 2.0` prints `0` under stock, `1.5` under pliron). Stock also
  emits `__floatuntihf` for u128 -> f16, which isn't in any library, so the
  link fails; pliron goes via f32. f16/f128 tests in the UI corpus may show
  `env-link`/`output-mismatch` only when they exercise these paths.
- `src/licm.rs` (found by fuzz-matrix seed 7 with `PLIRON_TAILDUP=0`, 8
  verifier ICEs of the form `licm broke <fn>: ireduce uses value from
  non-dominating inst`): the invariance test treated "operand defined
  outside the loop" as sufficient, but on irreducible CFGs the preheader
  can be reachable from inside the loop, so an outside def need not
  dominate the insertion point — and a def inside the body can supply a
  use without dominating the header. Now: operands must dominate the
  insertion point, the insertion block must dominate the header, and the
  header must dominate every body block (bails otherwise).
- `src/loopidiom.rs` (found via stage2 `rustc` emitting invalid MIR —
  `rustc_mir_transform::ssa::compute_copy_classes`'s masked-store loop
  `for h in copies.iter_mut() { if *h == head { *h = RETURN } }` became a
  `memset`): a loop whose store sits behind an in-loop `brif` was
  converted to an unconditional `memset`. Fixed by requiring the store to
  dominate every latch; `cfg`/`domtree`/`loop_analysis` are recomputed
  after each rewrite and converted headers are skipped on rescan.
- `src/loopvec.rs` (found by `PLIRON_VERIFY=1` while self-hosting —
  `aho_corasick` `RareBytesThree::find_in` and `regex_automata`
  `find_fwd`): multiple lane-wise early exits emitted several `brif`
  terminators into one block ("terminator before end of block").
  Each check now terminates a fresh block, chaining to the next check.
- `src/loopvec.rs` (found by `PLIRON_VERIFY=1` while self-hosting
  `rustc_trait_selection`'s
  `extend_cause_with_original_assoc_item_obligation`): splatting a
  cond-tree operand *narrower* than the lanes (an i8 flag across i32
  lanes) emitted `ireduce.i32 v_i8`, which requires the arg to be wider
  than the result. Wide sources still `ireduce`; narrow sources now
  `sextend` (a -1 mask stays all-ones, 0/1 flags stay 0/1).
- Self-host status: stage1 completes end-to-end; the pliron-built stage1
  rustc builds the whole stage2 compiler with `PLIRON_VERIFY=1` clean.
  The stage2 rustc still SIGSEGVs/SIGBUSes compiling `core` — corrupted
  `ThinVec`/`Attribute` data inside AST walks (`rustc_ast_passes`,
  `rustc_expand`, `rustc_lint`; also seen as `thin_vec::capacity_overflow`
  and unbounded recursion in `flat_map_in_place`). The bug is outside the
  pass pipeline: `PLIRON_LICM=0`, every `PLIRON_*` pass toggle, and
  `PLIRON_OPT_BISECT=0` (all gated passes off) all still crash. Suspects
  left: pliron-IR-level passes (inline/sroa/etc.), lowering itself, or
  cranelift's internal egraph/regalloc.
- Wasm: `src/wasm.rs` promoted-alloca SSA work (a8b7f93) landed after a
  transient intermediate miscompiled wasip1 std (`call_indirect`
  signature mismatches, `memchr` slice-index traps, dlmalloc asserts);
  all `tests/wasm/run.sh` legs pass at HEAD. New suite `ui-wasm`
  (`harness/run.py --suite ui-wasm`): the UI corpus compiled for
  `wasm32-wasip1` by both backends — pliron objects link via
  `pliron-wasm-ld` against a provisioned pliron std sysroot
  (`target/ui-wsys`, rebuilt when the dylib hash changes), stock rustc
  uses the installed target std; both run under `node`'s WASI and are
  diffed on exit code + stdout + stderr with wasm-module noise
  normalized. Note rustlantis programs aren't wasm-portable (64-bit
  `isize`→`i64` transmutes), so wasm fuzzing needs the UI corpus or a
  32-bit-safe generator.
- ui-wasm first full run (21,482 files): 16,466 pass, 4,881 directive
  skips, 34 env-compile-error, 25 env-link, 17 env-ice — all
  environmental. Real divergences found and fixed: f16/f128 `fneg`
  emitted `f32.neg`/`f64.neg` on integer raw-bits reps (f16 = i16 bits
  in i32, f128 = i64 pair) → invalid wasm rejected at instantiate; now
  sign-bit xor. Remaining knowns: 11 simd run-fails (v128 memory ops
  unimplemented → honest `unreachable` stubs), 2 output-mismatches
  (raw pointer prints — wasm addresses are <9 hex digits, below the
  ADDR normalizer's floor), 1 stderr-mismatch (`hygiene/panic-location`
  embeds the local std build path), 1 `accepted`
  (`explicit-tail-calls` — pliron emits tail calls stock can't), ~44
  `can't find crate for test` fixed by adding `test` to the sysroot
  build-std set. f16/f128 arith/cmp/cvt now panic→stub (wasm has no
  hf/tf ops and the sysroot lacks `__*hf*`/`__*tf*` builtins) instead
  of emitting invalid wasm.
- wasm `wloop_opt` (in-flight loopopt: `mk_pre`/`loop_licm`/
  `loop_indvars`/`wdce`) briefly hung `sum_squares` — stale
  `cfg.def_block` after inst motion; a live `defb` map fixed it.
  Debug: `PLIRON_WASM_WLOOP=0` gates the pass, `PLIRON_WASM_LOOPS=<pat>`
  dumps pre/post bodies for functions whose name contains <pat>.
