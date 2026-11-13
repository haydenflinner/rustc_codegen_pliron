# headstart: evaluation, and `harness/headstart.py`

Evaluated 2026-11-13 at `PowderworksCode/headstart` commit `02ec0bf` (2026-10-20).

## What it is

A patch series for rustc (6 commits, ~125 KB) and cargo (3 commits, ~60 KB), plus the scripts
and write-ups that measure it. It is not a library and has nothing to link against.

- rustc `-Zearly-metadata`: a new `analysis_interfaces` query splits analysis into item
  interfaces (well-formedness, coherence) and function bodies. Between the two the driver writes
  `libfoo-hash.early-rmeta`. Crate loading accepts it, and `start_codegen` swaps every such
  dependency for its full `.rmeta` before code generation, blocking on a lock file the
  producer holds until the file is written. Linking waits for rlibs the same way.
- cargo `-Zheadstart`: passes the flag, starts dependents on the `early-metadata` artifact
  notification (in `check` and `build`), gives a paused compilation's job slot away, and holds a
  unit's output until its dependencies finished cleanly.
- Their numbers (their machines, LLVM): clean builds of 13 real projects up to 54% faster for
  `cargo check` and 42% for `cargo build` on 16 cores; on 4 cores 13-24% for chain-shaped
  workspaces and even for wide ones. The gain is cores the build would otherwise leave idle.
- Maturity: a prototype in upstream shape (in-tree tests, UI suite with the flag forced on,
  rustc-perf sweeps), Linux only, not submitted upstream. Its own `docs/readiness.md` says
  "ready for the design conversation, not ready to merge".
- Licence: **none**. The repository has no licence file and no licence statement, so nothing
  in it may be copied or redistributed; the patches are derived from MIT/Apache rustc and cargo
  but their author's changes carry no grant. Nothing of it is in this repository:
  `port-check` fetches it into `target/headstart/src` when asked.

## What it has to do with this backend

Nothing in the series touches `rustc_codegen_ssa` or a backend: early metadata is written and
swapped in `rustc_interface`/`rustc_metadata`, before `codegen_crate` is called. So it is
orthogonal to the backend, and it speeds up exactly the part of a build we do not own (the
front-end chain between crates). Where it matters to us:

- clean `cargo build` wall time, which is what the profile loop pays twice
  (`bench/heldout/run.py --pgo`) and what a user of the backend sees;
- it leaves code generation as an even larger share of the critical path.

One place in the backend would be wrong under it: `src/devirt.rs` (`Dep::symbols`) reads a
dependency's **rlib** during code generation of a profile-use build. Under headstart only the
dependency's full *metadata* is guaranteed at that point; the rlib is guaranteed before
linking. The read would fail or succeed by timing and the guards emitted would differ from run
to run. The fix when the time comes: take a shared lock on `<rlib>.lock` first (the producer
holds it until the rlib is written), or have rustc wait for rlibs before code generation as it
already does for `-Clto`. `src/field_order.rs` only looks at paths and `src/lto.rs` runs under
`-Clto`, which the series already covers.

## Why there is no headstart toolchain here

1. Our pin is `nightly-2026-10-22` (`1a2c4797e`); the series is made for `6006fd0`
   (2026-10-01). In between, upstream reworked metadata encoding (deterministic encoding,
   `LocalDefId`-keyed tables). Patch 2, the encoder refactor everything later builds on,
   conflicts in 14 places in `rmeta/encoder.rs`; patches 4 and 6 cannot be judged until it is
   rebased. `harness/headstart.py port-check` reproduces this in seconds and will say when
   either side has moved enough to apply.
2. A rebased series would be a derived work of unlicensed code that we could not keep here.
3. The backend builds against `rustc-dev` of the pinned nightly, so the patched compiler has
   to be a from-source build with the backend in tree (`selfhost/setup.sh` does that part), and
   the patched cargo another one. The result needs the upstream sweeps rerun before any timing
   can be trusted.

That is days, not a day, and the output would be a private toolchain to maintain. If the
series lands upstream, we get it by moving the pin, and the only work left is the `devirt.rs`
read above.

## What was built instead: `harness/headstart.py`

The question that can be answered today is how much of *our* builds headstart could remove.
The tool records a real cargo build on the pinned nightly and replays it under headstart's
scheduling rules.

    harness/headstart.py record --heldout regex --pliron   # clean release build, then the report
    harness/headstart.py record -o NAME [--pliron] -- cargo build --release ...
    harness/headstart.py report NAME... [--table] [-j N] [--early 0.2]
    harness/headstart.py port-check                        # do the patches apply to our pin yet?
    harness/headstart.py selftest

`record` runs the build under a `RUSTC_WRAPPER` (the script itself) that adds `-Ztime-passes`,
timestamps the lines as they arrive and drops them, so cargo's output is unchanged. Per rustc
it keeps: start, `coherence_checking` done (the point where early metadata would be written),
`generate_crate_metadata` done, the `link` time, end, and the process's CPU time. Build-script
runs come from cargo's own timestamp files. Recordings are `target/headstart/NAME/`.

`report` prints the observed schedule (wall, compiler time, share of the wall with at most one
compiler alive, share of compiler time per phase), then two replays of the same durations:

- today's cargo (libraries start on full metadata, linking crates when every dependency is
  done) with its critical path. Its distance from the observed wall is the model's error.
- headstart's rules: a crate starts when its dependencies' interfaces are checked, pauses
  without a job slot until their full metadata exists before code generation and until their
  object code exists before linking, and resumes without waiting for a slot.

It is an upper bound: durations are replayed unchanged (no extra contention), the early write
is free unless `--early F` charges a share of each crate's body time for the bodies early
metadata needs (`const fn`, `-> impl Trait`, `async`), and the job limit is modelled as slots,
with code generation weighted by the threads its CPU time shows.

### Measured

One clean release build each of the eight held-out programs, both backends, `build.jobs = 6`
(this machine's cargo config), machine busy (load 15-25), so absolute times are noisy; each row
is one recording. "model" is the replay of today's rules against the observed wall; the last
two columns are predicted, not measured.

| build | observed | model | wall with <=1 rustc | time to metadata | codegen+link | headstart | with 20% early cost |
|---|--:|--:|--:|--:|--:|--:|--:|
| blake3-scalar-llvm | 1.69 s | -12% | 56% | 64% | 36% | -22% | -22% |
| blake3-scalar-pliron | 1.92 s | -10% | 49% | 57% | 43% | -21% | -21% |
| blind-sig-llvm | 4.94 s | -9% | 24% | 45% | 55% | -5% | -5% |
| blind-sig-pliron | 5.66 s | -0% | 20% | 48% | 52% | -4% | -4% |
| pulldown-cmark-llvm | 1.86 s | -5% | 77% | 33% | 67% | -16% | -16% |
| pulldown-cmark-pliron | 2.09 s | -5% | 84% | 33% | 67% | -12% | -12% |
| regex-llvm | 2.28 s | +5% | 42% | 41% | 59% | -26% | -24% |
| regex-pliron | 3.82 s | -7% | 48% | 41% | 59% | -24% | -19% |
| rust-compression-llvm | 3.10 s | -2% | 49% | 41% | 59% | -24% | -22% |
| rust-compression-pliron | 3.27 s | -3% | 70% | 51% | 49% | -20% | -18% |
| rust-html-rewriter-llvm | 6.59 s | -7% | 53% | 65% | 35% | -20% | -18% |
| rust-html-rewriter-pliron | 6.44 s | -4% | 55% | 65% | 35% | -19% | -17% |
| rust-json-llvm | 4.40 s | -7% | 36% | 61% | 39% | -20% | -17% |
| rust-json-pliron | 5.61 s | -3% | 43% | 61% | 39% | -29% | -26% |
| rust-protobuf-llvm | 7.13 s | -7% | 17% | 68% | 32% | -10% | -10% |
| rust-protobuf-pliron | 8.03 s | -2% | 15% | 69% | 31% | -2% | -1% |

Reading it:

- The model reproduces today's schedule to within 12% (median 5%, usually a little fast: cargo's
  own per-unit overhead is not in it). Predictions are no better than that, and they moved by
  several points when the slot weighting changed (rust-protobuf-pliron: 8% to 2%).
- Chain-shaped builds (regex, json, compression, html-rewriter, pulldown-cmark) have 12-29% of
  their wall in waits headstart removes; the six-crate builds spend 40-80% of the wall with one compiler
  alive. Wide graphs (blind-sig, protobuf) are already saturated at six jobs: 2-10%.
- The backend makes little difference to the prediction, as expected from where the patches sit.
- What remains on the critical path afterwards is one crate's analysis plus the code generation
  of the last big crates (for regex: `regex` itself, 1.4 s of the 2.7 s left with pliron).

The recorder is useful without headstart: it is the only tool here that shows a *cargo* build's
schedule (which crate's front end, code generation or link the wall is waiting on).
`harness/ctime.py` times single rustc invocations.

## Left

- A real A/B. It needs the series rebased over the metadata-encoding rework (or the pin moved
  to a nightly that has it), a from-source rustc with the backend in tree, the patched cargo,
  and a quiet machine; then `record` on both gives predicted against measured.
- The `devirt.rs` rlib read (above), once such a toolchain exists.
- `check` builds and larger workspaces have not been recorded. The slot model treats a
  resumed job as free, as headstart's cargo does, and ignores the extra contention.
