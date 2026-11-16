# 2026-11-16 overnight wasm parity session

Running log. Newest decisions that want a second look are under
"Review these".

## Review these

1. **sccache made rebuilds stale (found 02:05).** `~/.cargo/config.toml`
   got `build.rustc-wrapper = "sccache"` on 2026-11-14 22:30. sccache
   keys on rustc's arguments and inputs; the backend dylib behind
   `-Zcodegen-backend` and every `PLIRON_*` variable are not in the key.
   A rebuild into a target dir path that was used before returned the
   objects of the earlier build. Seen directly: `variant.sh uear` gave
   the same module before and after a backend change, `variant.sh uear2`
   (new path) gave the new one. `bench/wasm/run.sh` always builds into
   `target/wasm-bench`, so every run.sh after 11-14 22:30 could have
   timed an old backend. Variant names that were reused have the same
   problem (`half` built with `PLIRON_HALFUNROLL=1` was byte-for-byte
   the default body of `lookup_slow`). Numbers recorded between
   11-14 22:30 and this note should be re-measured before they are
   trusted. Fix: `.cargo/config.toml` in the repo sets
   `rustc-wrapper = ""` (verified: no `sccache` in `cargo build -v`).
   Builds outside the repo (`~/code/dsa`) need `RUSTC_WRAPPER=` in the
   environment.
2. **I wiped `target/` by accident (02:07).** A scratch `cargo init`
   under `target/` joined the workspace, and `cargo clean` in it removed
   the workspace target dir. Build artifacts only: backend builds,
   recorded corpora (`target/corpus/*`), held-out build caches, earlier
   agents' `target/v8prof` variants and `target/wasm-ab-*` dirs. The
   `Cargo.toml`/`Cargo.lock` edit `cargo init` made was reverted.
   Corpora were re-recorded (see below).
3. **`corpus:clcg` has one mismatch under `gate.py --full` (found
   02:58, not from tonight).** The default gate replays every 8th clcg
   function and is green; the full replay reports
   `..cranelift_codegen3isa7aarch644inst4emit.._8f1cba34.2629.clif`.
   It reproduces with `clif-check report <dir> out.json --check` on
   that one file, at `b903d33` too, and a `PLIRON_PIPELINE` prefix
   bisect puts it at `slotmerge` (prefix of 81 steps). Calls are
   identical; "before" writes 11 zero bytes at `arg3+0x44..0x6c` that
   "after" does not. The known list has the same step as "undef-byte
   reshuffle" noise, but those entries say the writes match, and here
   they do not. Not triaged further. The clcg corpus was re-recorded
   tonight, so the file name's crate hash is new.
4. **`willret` was red on main before tonight** (`b903d33`): fixed in
   `06626f0` (see below). The gate runs that passed it were presumably
   stale objects from sccache.
5. **tokenize gives back ~3 points** (-17.5% -> about -14.5%, still
   ahead of LLVM) with the new defaults. `PLIRON_UNREACH_PHI` and the
   assume folding each cost about 2.5 points on their own (`kphi`,
   `kar` variants), though both only remove work from the multi-byte
   decode arm. Not explained; the wasm for `tokenize::run` is in the
   log below. Left on because chars_mixed (+19.9% -> -1.4%) and the
   module size depend on the same two knobs.

6. **`~/code/dsa` (`literate-wasm`, features `eval,squish,cad`) does not
   run yet under pliron.** A background agent built it with both
   compilers (scripts in `harness/dsa/`, state in
   `/tmp/dsa-bench/findings/STATUS.md`). At the start of the night the
   pliron build hit four compiler panics and three runtime failures.
   Fixed so far, each with a regression test: regex-syntax
   (`punroll`/HALFUNROLL), jiff (second indvars round), literate-eval
   (`tailrec` overflow), rapier2d (`bunroll`), blake3 (`wpeep`
   keep-on-stack, a silent miscompile at default settings since before
   tonight). **Still open when this was written:** loro CRDT panics
   (`oplog.rs:1432 unwrap on None`, postcard "varint that didn't
   terminate") and the interpreter's endless `Ty::clone` /
   `FnHeader::clone` recursion. Both survive `PLIRON_WASM_PEEP=0`.
   yaml-rust (default features only) also fails the verifier.
   No dsa benchmark number exists until the module runs.
7. **The default-features dsa build needs 5-7 GB per rustc under
   pliron** (polars-core 6.8 GB, typst-library 5.9 GB, vortex-array
   5.7 GB when `earlyoom` killed them). At `-j 5` that pushed the
   machine into swap. Not looked at.
8. **`HALFUNROLL` was default-on for about an hour with the
   regex-syntax bug in it** (`8c83f1d` to `e843306`). The verifier
   caught it at compile time; nothing was miscompiled silently. The
   kernels and the gate have no `binary_search_by` whose result leaves
   the loop as a plain SSA use. The gate's corpora (std, smoke, clcg)
   are replayed on the native pipeline, so wasm-only paths are only
   covered by `wasm_smoke` and the wasm sysroot leg.

## What landed

| commit | what |
|---|---|
| `06626f0` | punroll: the peel's `full_unroll` deleted the residual loop on `trip_max`, a cost estimate (unknown start read as 0, `!=` bounds a stepping counter can skip). `while i != 0 { i += 2 }` lost its loop. `loopidiom::trip_max_exact` is the bound that may delete code. |
| `c29809a` | `.cargo/config.toml`: no rustc wrapper inside the repo. |
| `aa5dbf3` | `PLIRON_UNREACH_PHI`, `PLIRON_ASSUME_RANGE` as a marker op + `PLIRON_UNREACH_EARLY` (`vrp0`, `unreach0`), `PLIRON_UNROLL_SYM`; all opt-in in that commit. |

| `8c83f1d` | ASSUME_RANGE, UNREACH_EARLY, UNREACH_PHI, UNROLL_SYM, HALFUNROLL default on (wasm). |
| `4f4b3bf`, `68d4e93` | `vrp2` (late range pass) and REVCMP, then default on (wasm): strwork +10.6% -> +7.1%. `run.mjs --best`. |
| `28473ee`, `98553c9` | waffle: arm arguments stay out of the edge remat (`PLIRON_WTREEREMAT_CONDARG`), then default: parse_only -0.7% -> -11%. `armhoist` (opt-in). |
| `e843306` | punroll halving unroll: loop values named past the exit (regex-syntax). |
| `b67567f` | indvars reverse index: a sub passed on an exit edge is still used (jiff). |
| `98a6df2` | tailrec: signed min/max identities without overflow (literate-eval). |
| `b60ceb0` | wpeep keep-on-stack: keep a write a hoisted get reads (blake3 miscompile). `run_straight` differential interpreter for wpeep tests. |
| `faf61bc` | bunroll: remap a copy's clones once the whole copy exists (rapier2d). |

## Findings worth keeping

- **V8 inlines wasm functions itself, on a budget that depends on the
  whole module.** `fib` went from -7% to +97% between two builds whose
  `fib` body was byte-identical. `--trace-wasm-inlining` shows why:
  TurboFan inlines `fib` into itself once when `2 * size` fits the
  budget, and the budget is `factor * size` with a factor that rises
  with the module's share of small functions (LLVM's module 2.2, ours
  2.06, the regressed build 2.0: `budget=min(154, 600), size 77->154
  not enough inlining budget`). Two helpers that stopped being inlined
  by *our* inliner moved the share from 37.6% to 36.8%. Consequences:
  a kernel can flip by 2x on an unrelated change in function count;
  our `fib` is 77 wire bytes against LLVM's 59; and a module that
  ships more small functions gets more engine inlining. Check
  `node --trace-wasm-inlining` before believing a large swing on a
  call-heavy kernel.
- **An assume has to be invisible to the IR passes.** As a `cond_br`
  in the frontend it changed the inliner's op counts (flat 40-op limit
  on wasm) and the function set. As one `llvm.assume` marker that is
  free in `callee_info` and lowered to a branch in `lower.rs`, the
  function set is identical to a build without it.
- **`zipiv` and `cmp3` do nothing on the wasm bench** (byte-identical
  modules with the knob on). `revcmp` moves strwork by about one point.
- strwork's gap is all in `dragon::format_exact` (ours 5686 wat lines
  against LLVM's 2600 plus an out-of-line `mul_pow2`). LLVM's limb
  compare is `load a[off]; load b[off]; br_if ne; off -= 4; br_if`;
  ours walks two pointers with two end tests and builds an `Ordering`
  it then re-tests.
- **V8 does not make up for inlining we skip.** A flat 30-op inline
  limit (`PLIRON_INLINE=30`) is geomean 1.47 on the bench; non-loop
  thresholds of 128 and 64 (`PLIRON_INLINE_WASM_COLD`) are +75..150%
  on the chars kernels and also drop V8's own budget for `fib`. The
  512-op wasm limit stays.
- **A move-only arm is free in V8, an arm with one instruction is
  not.** `brif c, join(args), other` whose edge only moves values
  compiles to a branch straight to the join; with one `add` on the edge
  V8 emits the arm as a block plus a jump. waffle's edge remat was
  creating those. Loop exits and back edges are the cases where the
  edge is still the right place.
- **Timing on this machine tonight**: load average 30-60 from other
  workspaces. Median round-ratios wander by tens of percent;
  `run.mjs --best` (min/min per layout) holds to about a point. Spotlight
  was indexing `target/` after the rebuild; `target/.metadata_never_index`
  stops it.
- `harness`-level tools written tonight and left in the scratchpad, not
  the repo: per-loop V8 tick attribution (`hotloops.py`: ticks per
  machine-code loop and per instruction, with the wasm offsets inside),
  `mcrange.py` (TurboFan listing of an offset range). Worth moving into
  `harness/v8/` if they are wanted.

## `~/code/dsa` (literate-wasm, features eval,squish,cad), as of 05:20

Harness: `harness/dsa/build.sh` (both compilers, wasm-bindgen),
`harness/dsa/bench.mjs` (23 workloads, interleaved, result cross-check),
`/tmp/dsa-bench/findings/` (STATUS.md by the build agent, COORD.md by me).

**Code size.** LLVM: 19,544 functions, 14.3 MB of code. pliron
(snapshot 6): 35,562 functions, 42.0 MB — 2.95x. Matching functions by
demangled name:

- 10,818 names are in both: 9.26 MB under LLVM, 15.07 MB under pliron
  (1.63x).
- 27.0 MB of pliron's code is in functions LLVM's module does not have
  at all (core 3.8 MB, alloc 3.4, parse 3.3, literate_eval 2.4,
  hashbrown 2.1, solve 1.9, loro_internal 1.9, ...): instances LLVM
  inlined everywhere and dropped, or never needed.
- The common names have a few enormous outliers, all inlining:
  `solve::exhaustion::Matrix::defaulted` 732 B -> 721,540 B,
  `Solver::check_call_wheres` 1,974 -> 409,553,
  `mimas_compile::..::Compiler::compile` 73,738 -> 437,235,
  `ResolvedDeclKind::clone` 570 -> 56,276.

The wasm inline limit is a flat 512 ops per callee (x3 at loop
sites) with no caller growth budget (`small` mode turns the budget
off; `PLIRON_INLINE_CALLER_MAX` is 20,000 ops). That was tuned on the
bench kernels (hashbrown's `hash_one`). On this program it is the
first thing to revisit once the module runs: LLVM's threshold is
about a tenth of that per call site.

**Correctness.** See "Review these" item 6 for the list. The stage
experiments (05:15): with `PLIRON_OPT_BISECT=0` the interpreter
workload runs and matches LLVM (2.45x slower); a crate-level bisect
puts the interpreter miscompile in `literate-eval`'s own code. For
loro the agent's repro shows two independent bugs, one that needs the
IR `sroa` pass and one that needs the CLIF pipeline. The linker's
same-name fold (`PLIRON_LDEDUP=0`, a 23 s relink) and the waffle/wpeep
stages are not the cause.

**The wasm verification gap** is the structural lesson. Before
tonight the gate's corpora and the differential checker only
exercised the native pipeline; wasm-only transforms and everything
after CLIF were covered by a smoke test and a sysroot build. Tonight's
additions: `PLIRON_CLIF_WASM=1 clif-check` (wasm replay), the i32
sandbox in diffcheck, the default end-of-pipeline verifier on wasm,
and `run_straight` for wpeep. Still uncovered: the pliron-IR passes
(sroa, inline, IPO summaries) and the waffle stages have no
differential check at all, and that is where the open dsa bugs are.
