# The inner loop: edit a pass, see the result

How to go from "saved a change in `pliron-clif-opt/src/x.rs`" to a result
in seconds, what was measured to get there, and what did not work.
Measured 2026-11-13 on the shared M-series box (14 cores, 36 GB) while
three other agents were building: load average 10-80, swap full. Wall
times are noisy; CPU seconds are the steadier column.

## Use this

```sh
harness/dev.py clif target/corpus/std/<fn>.clif   # one saved function through the pipeline
harness/dev.py corpus std                          # all 4870: verdicts, size movers
harness/dev.py rs tests/parity/closing.rs          # machine instructions per function
harness/dev.py rs t.rs --env PLIRON_LICM=0 --fn hot_loop --run
harness/dev.py test licm                           # unit tests on save (relax JIT)
harness/dev.py check                               # quick.py --tier 0 on the fast build
harness/dev.py check --gate                        # the gate on the fast build
harness/dev.py bench regex pliron llvm             # held-out runtime A/B, fast dylib
harness/dev.py watch clif <file.clif>              # any of clif/corpus/rs/check, on every save
```

Every command builds what it needs first with the `fast` cargo profile
(`target/fast`) and prints its result next to the previous run and a
baseline (the first run of that command line; `--base` resets it), so an
edit shows as `vs prev` / `vs base` deltas. `watch` polls the pass
sources and prints `save -> result` seconds.

`PLIRON_PROFILE=fast` makes `harness/gate.py`, `harness/quick.py`,
`test.sh`, `harness/run.py`, `harness/live/live.py` and
`harness/corpus.sh` build and load `target/fast` instead of
`target/debug` (default unchanged). `bench/heldout/ab.py --backend
target/fast/librustc_codegen_pliron.dylib` does the same for the
held-out A/B: the runtime of the *generated* program does not depend on
how the backend was built, so only compile-time measurements still need
`cargo build --release`.

## Numbers

One real edit (a statement added or removed inside a function of the
named file, so the lines below it shift), then the build. "dev" is
`cargo build`, "fast" is `cargo build --profile fast`, both incremental,
`--jobs 4`, built one after the other for the same edit in alternating
order. Release is `CARGO_INCREMENTAL=0 cargo build --release --jobs 4`.

| edit -> | release | dev (before) | fast | 
|---|---|---|---|
| backend dylib, wall | 51-82 s | 22-25 s | 10-13 s |
| backend dylib, CPU | 150-255 s | 62-70 s | 15-25 s |
| `clif-check`, wall | - | 19-41 s | 7-14 s |
| `clif-check`, CPU | - | 63-72 s | 11-22 s |

Where the time went (`-Ztime-passes` on `pliron-clif-opt`, 130k lines,
after the edit): `LLVM_thinlto` 12.7-13.7 s of 17-19 s. At opt-level >= 1
rustc runs a crate-local ThinLTO over the incremental CGUs; `lto = "off"`
(the only difference between `fast` and `dev`) removes it. The release
build is not incremental: `pliron-clif-opt` 50 s, then `pliron-clif-lower`
17 s and the top crate 19 s.

End to end, save to printed result (`harness/dev.py`, same edit for
both; `PLIRON_PROFILE=debug` is the old loop driven by the same script):

| save -> result | dev (before) | fast |
|---|---|---|
| `dev.py clif f.clif` (one function replayed) | 15-21 s | 7-11 s |
| `dev.py rs tests/parity/closing.rs` (instruction counts) | 21-27 s | 11-14 s |
| `dev.py corpus std` (4870 functions, interpreter diff) | 21-24 s | 12-17 s |
| `harness/quick.py --tier 0` (build, the pass's unit tests, corpus replay) | 30-50 s | 15-25 s |


What the fast build costs: the backend it produces is slower. `clif-check
report --check` over the std corpus takes 33-35 CPU-s with the `dev`
binary and 39-40 with `fast` (+15%; 14 jobs, so ~0.5 s of wall).
opt-level 2 for our crates gets 4% of that back at the same rebuild time
(not adopted). Results are the same: verdict counts and instruction
totals match.

Until 2026-11-13 the last sentence was false for `fast`: `webmerge`
memoized structural hashes by node address (`proots::khash_m`) and
reused the hash of whatever had been at a recycled stack or heap
address, so which merges it found depended on the build's stack layout.
A `fast` backend compiled pulldown-cmark to 54.56M executed
instructions where `dev` and `release` gave 52.93M (the scanners kept a
pointer walk instead of the counter). The memo now pins what it
remembers (`proots::KHash`); `fast`, `dev` and `release` agree again.

Three more fixes that apply to every profile:

- **One build of each crate instead of one per command.** Cargo resolves
  features for the packages a command selects. `cargo build`, `cargo
  build -p pliron-clif-check` (quick tier 0, live.py), `-p
  rustc_codegen_pliron`, the gate's `-p rustc_codegen_pliron -p
  pliron-clif-check` and `cargo test -p pliron-clif-opt` each resolved
  cranelift-codegen differently (`host-arch`, `timing`, target-lexicon
  `std`, ...), so each kept its own cranelift-codegen and its own
  `pliron-clif-opt`, and after one edit `pliron-clif-opt` was recompiled
  once per command in use (measured: a no-op `-p rustc_codegen_pliron`
  build after a `-p ... -p ...` build took 6.7 s in dev, and the first
  switch costs a 55 s cranelift-codegen rebuild in release).
  `pliron-hack/Cargo.toml` (generated: `harness/dev.py hack`, verified:
  `harness/dev.py hack --check`) lists the union of the features for the
  21 third-party crates that differed; `pliron-cfg` depends on it. Now
  every selection, tests included, uses the same units: after a
  two-package build, `-p pliron-clif-check` is fresh in 0.4 s and `cargo
  test -p pliron-clif-opt` compiles only the test targets. Rerun `dev.py
  hack` after changing dependencies or their features.
- **The dylib was rebuilt on every `cargo build`.** `build.rs` printed
  `rerun-if-changed=.pliron-src-rev` for a file that only exists in
  self-host copies; cargo treats a missing path as changed (`Dirty
  rustc_codegen_pliron: the file .pliron-src-rev is missing`), reran the
  script and recompiled and relinked the top crate, 2-12 s with nothing
  edited. It is now printed only when the file exists.
- `harness/quick.py` and the gate run their unit tests with the selected
  profile too (`cargo test --profile fast`).

## Unit tests on save: relax

`../relax` is a from-scratch Rust compiler and JIT. With two fixes made
there (below) it compiles and runs `pliron-clif-opt`, the vendored
Cranelift, the interpreter and `clif-check` from source.

`harness/dev.py test [FILTER]` runs `relax test --live pliron-clif-opt
--lib -- FILTER`: the tests stay loaded, and a save recompiles the changed
functions and reruns the tests that entered them.

| `licm` lib tests (9), after an edit in `licm.rs` | save -> verdict |
|---|---|
| `cargo test -p pliron-clif-opt --lib licm` (dev profile) | 34-35 s |
| `relax test --live`, edit that changes nothing observable | 0.8-2.4 s |
| same, edit that breaks the pass (6 of 9 fail, as under cargo) | 2.7 s |
| same, reverting it | 5.0 s |

The first run is slow (the whole lib suite, cold: 3 min 45 s; 9 tests
with a warm cache: 15 s), and the cache in `target/relax` is 3-4 GB. Use
a filter. Build the runner once with `harness/dev.py relax-build` (needs
the patched `../relax`; `RELAX=<path>` overrides).

For the corpus replay relax loses to a native build, because the program
being run is itself a compiler:

| std corpus, `clif-check report --check` | wall |
|---|---|
| native, dev or fast build (after a 7-14 s rebuild) | 3-8 s |
| relax, cold cache | 79 s |
| relax, warm cache | 27-48 s |
| one function (`pipeline f.clif`), relax warm / after an edit | 11 s / 7.5 s |
| `bench` single thread, per rep: native 14.4 s, relax first rep 87 s, later reps ~40 s |

`harness/dev.py relax ARGS` runs `clif-check ARGS` under relax if you
want it; verdict counts are identical to native (4198 ok, 672
inconclusive).

What relax cannot run yet:

- `pliron-ir-opt` tests: 28 of 31 fail with "Unregistered dialect
  builtin". pliron registers dialects through `linkme::distributed_slice`
  and `inventory`, i.e. linker sections; the JIT has no equivalent.
- The backend dylib (`rustc_private`).
- `RELAX_LAZY_BODIES=0` is required (`dev.py` sets it): relax's lazy
  function-body parser rejects a `macro_rules!` defined inside a function
  body in `combine` (a dependency of `pliron-derive`).

Changes made in `../relax` (uncommitted there), each with a test in
`libs/relax/compiler/tests/` that fails before and passes after, and
passes under rustc:

- `front/src/parser.rs` `pos_before`: a local bound from a macro fragment
  was invisible to a `macro_rules!` defined later in the same expansion
  (`combine`'s tuple parsers; E0425 for `A`). Test:
  `nested_macro_fragment_local.rs`.
- `types/src/typeck.rs`: `<T>::Assoc::item` resolved `item` on `T`
  instead of on `<T>::Assoc` (cranelift-codegen `<M>::I::rc_for_type`;
  E0599). Test: `qself_assoc_item_path.rs`.

## What did not work

- **Building the backend with itself** (`RUSTFLAGS=-Zcodegen-backend=...
  cargo build -p pliron-clif-check`). Update, same day: the failures
  below were one bug (`webmerge` materializing a param's value on a dead
  self-loop, where it mentions the param itself; `mask01`, the egraph and
  the verifier then met the orphaned value), fixed with a test in
  `clifpeep.rs`. `clif-check` now builds with the backend under
  `PLIRON_VERIFY=full` in 3 min 18 s and the self-built binary replays
  the std corpus with identical results; the eight interpreter mismatches
  were checker artifacts (`tests/diffcheck.rs`). What follows is the
  state before that. It does not get through
  `pliron-clif-opt`: a release backend panics (`index out of bounds ...
  4294967295` in `dfg.rs`), a debug one fails Cranelift's verifier. With
  `PLIRON_VERIFY=full` the committed HEAD already fails in
  cranelift-codegen: `mask01` leaves a use of a value whose instruction
  it removed. Repro, one second:
  `clif-check pipeline target/devloop/repro/mask01-vec-extend-trusted.clif`
  (`mask01: VERIFIER ERROR ... v807 is defined by inst927 which has no
  block`). In the working tree `edgesink` and `lcse2` fail the same way
  on other functions. Even fixed, it would not help this loop: our
  backend has no incremental reuse, so every edit would recompile all of
  `pliron-clif-opt`, against 4-6 s for incremental LLVM in `fast`. The
  dependencies took as long as with LLVM (2 min 53 s). It is a good test
  corpus, though: `target/devloop/corpus-clcg` (cranelift-codegen through
  the HEAD backend, 18973 functions) replays with 8 interpreter
  mismatches (`DebugMap::entries`, `HashMap` `Debug`) that the std corpus
  does not have.
- **Hot-patching the backend inside rustc or `clif-check`** with
  `PLIRON_HOT`. It needs the patched crate compiled by our backend as one
  CGU, which is the item above, and a patch is a whole-crate recompile.
  relax's live tests are the working version of this idea.
- **A cargo config instead of `pliron-hack`** (`[resolver]
  feature-unification = "workspace"`, nightly). It works, but a
  `.cargo/config.toml` at the repo root also applies to every crate built
  under `target/` (held-out benchmarks, `crates.py`, fuzz), where it would
  change their features.
- **A target dir per agent.** The agents share one working tree, so they
  compile the same sources; separate dirs make each compile them again.
  The shared dir costs waiting on cargo's lock, not invalidation. Use
  `CARGO_TARGET_DIR` only for experiments that must not disturb a running
  fuzz or benchmark.
- Faster linker: the link is 0.7 s of the top crate's ~4 s; `wild` does
  not link Mach-O.

## Left

- Make `lto = "off"` the `dev` default (one line) once nobody has a long
  job on `target/debug`: it forces one full rebuild there, and the debug
  backend gets ~15% slower.
- The top crate costs ~4 s after any dependency edit (type check,
  monomorphization walk, metadata, link) even though nothing in it
  changed.
- relax: linker-section registries (for `pliron-ir-opt`) and the
  lazy-body parser bug. Seen once and not investigated: a full
  `pliron-clif-opt --lib` run under `relax test` stopped after the one
  test that also fails under cargo (147 ok, 1 failed of 149) without a
  summary line.
