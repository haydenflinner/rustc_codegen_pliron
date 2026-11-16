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
   **06:20 update: both are fixed, see "dsa, 06:20" at the end.** Each
   was one root cause, neither in the pass the bisects first pointed at.
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

9. **Two transforms and one rule change went default-on with no fuzz
   sweep** (`f13cb0d`: `PLIRON_MEMCPYOPT_PATHWIN`, `PLIRON_CONSTCOPY2`;
   `532caf8`: the SROA fragment word is the pointer width, wasm32
   only). The first two apply to native too. **A third,
   `PLIRON_WASM_SMALLCOPY`, was default-on from 07:10 to 09:30 and I
   took it out again (`d166d97`): see item 15.**
   Checked with the gate, `harness/crates.py --tier 1` (20 crates,
   knobs forced on), the wasm kernels, the 19 cross-checked dsa
   workloads and the 09:00 parity card (native 0.908 -> 0.873). The
   fuzz suite cannot run here (no rustlantis generator). Each has an
   off switch.
10. **`memcpyopt` now drops a destination's lifetime markers** when a
    forwarded write crosses one (`7ee65e8`): 19% of forwards on
    rapier2d. Sound, but it gives up slot sharing for those
    destinations; moving the start marker up would keep it. The stack
    axis of the card did not move.
11. **Static constructors run once, not on every export call**
    (`30ff21b`, written by a sub-agent, reviewed by me). That is not
    what rust-lld does, on purpose. `__wasm_call_dtors` is never
    called and the once-guard is not thread-safe.
12. **The wasm emitter now panics on a non-i32 `stack_addr`**
    (`96e7801`). A compile-time ICE instead of an invalid module; if
    another pass builds one, a build that used to "work" (and fail at
    instantiation) now stops in rustc.
13. **I deleted build trees and backend snapshots** under
    `/tmp/dsa-bench` and `target/dsa-snap*` (all made tonight) to free
    14 GB; the repro scripts there name paths that are gone.
14. **`harness/dsa/workloads/` is uncommitted** because this fork is
    public and those files are copies of dsa page sources. The harness
    needs them to run from a fresh clone. Your call.
15. **Superseded (afternoon): SMALLCOPY was not the cause, and the
    unoptimized build is fixed.** See "Afternoon" at the end: two bugs
    in stages that run with every optimization off (a wpeep forward
    and a waffle rematerialization), both fixed with tests; the module
    built with `PLIRON_OPT_BISECT=0` now returns LLVM's results on all
    19 workloads. The original entry follows.
    **`PLIRON_WASM_SMALLCOPY` was on by default for 2h20 with a bug in
    it, and the unoptimized build of dsa is wrong in three more ways.**
    At 09:15 I built the module with `PLIRON_OPT_BISECT=0` on the
    final backend to close the "does it work with optimizations off"
    question from 05:15. It does not:
    - with SMALLCOPY on, `vm_strings` prints `6\x04\x000288` for
      6570288 (itoa's 2-byte table copy); with `PLIRON_WASM_SMALLCOPY=0`
      it is right. No optimized build showed a wrong result with it on
      (all 19 workloads matched), it had no measured speed effect, and
      I did not find the cause in the time left, so it is opt-in again.
      (With the assume lowering on, the same build traps in itoa
      instead: the `assume` is right, the index it guards is garbage.)
    - independent of SMALLCOPY, and so older than tonight:
      `dims_page` and `compile_page` return different results,
      `game_tetris` fails with "no game running in slot 0", and the
      three CRDT workloads panic in std's `no_threads` mutex ("cannot
      recursively acquire mutex").
    Not investigated. It matters twice: something in the
    all-passes-off path is wrong (lowering, the emitter or a wasm stage
    that still runs), and that path is the baseline every
    `PLIRON_OPT_BISECT` hunt compares against. Packages:
    `/tmp/dsa-bench/pkg-pliron-ob0b` (defaults), `ob0c`
    (`ASSUME_RANGE=0`), `ob0d` (also `SMALLCOPY=0`).
16. Not turned on, each measured, each a lead: `PLIRON_INLINE_ONCE=1`
    on wasm (pages +2-3%, three kernels worse),
    `PLIRON_INLINE_CALLER_MAX=4000` (size -10%, speed worse). See
    "dsa, performance".

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

## dsa, 06:20: the two runtime miscompiles

Both were found by bisecting to a function and then reading the
function, and in both the pass the bisect named was innocent.

1. **Interpreter recursion (`Ty::clone` forever): IR `memcpyopt`,
   exposed by `slotmerge`.** `PLIRON_OPT_BISECT` named `slotmerge` in
   mimas-library's `install_assoc`; `PLIRON_SM_FLOWIV=0` and
   `PLIRON_SLOTMERGE_MARK=0` each hid it. slotmerge had put a 36-byte
   temporary on top of bytes 12..24 of a 188-byte struct that held a
   live `Vec` header, because the struct's `lifetime.start` marker sat
   *after* the stores that built the header. The marker was right when
   rustc emitted it: `memcpyopt::try_forward` had rebased a temp's
   writers onto the destination (deleting `memcpy(dst+12, tmp)`) and
   left `dst`'s marker between the moved writes and their read.
   slotmerge's `stale_markers` check is meant to catch that, but any
   later partial write of the slot clears its "dead" state, so a slot
   that is written in pieces is never caught. Fix (`7ee65e8`): the
   forward walks back from the copy and the temp's readers, cutting at
   writers; a `dst` marker on the way drops `dst`'s markers. Unit tests
   in `memcpyopt.rs`. *Iffy:* dropping all of `dst`'s markers costs
   slot sharing for that slot; moving the start marker up to the first
   writer would keep it. Not measured.
2. **Loro panics and the postcard varint error: the wasm emitter's
   i128 `>=`.** `src/wasm.rs::icmp128` compared the high words with
   `>=` in its `uge`/`sge` arm, so `a >= b` was true whenever the high
   words were equal — any `u128`/`i128` `>=` on wasm, since the emitter
   was written. rustc mostly emits `<`, which is why it survived:
   `looprot` inverted an exit test in serde_columnar's
   `AnyRleEncoder<i128>::flush_lit_run` (a LEB128 writer) and the loop
   stopped seeing its last byte. The stage table had said "one bug
   needs sroa, one needs the CLIF pipeline, either alone breaks it":
   both were this, reached through different inlining. Fix `716d8ff`;
   the wasm smoke module now checks all eight ordered compares and 26
   other i128 operations against BigInt (`cmp128`, `ops128`: all pass
   after the fix).

How they were found, for next time:

- `PLIRON_STEP_FNS=<step>=<lo>..<hi>` (new, `9e8c208`): one CLIF step
  only in a hash range of functions, the rest of the build untouched
  and parallel. 18 probes of 45 s named one function out of 33,000 in
  loro-internal. `fnbisect.py` could not: with nothing optimized the
  module fails differently.
- With `PLIRON_WASM_INLINE=0` the function stays whole, so `wasm2wat`
  of a good and a bad build differ in exactly one body. Reading that
  body found the compare in two minutes; the CLIF before and after
  `looprot` was correct, which is what said to look below CLIF.
- **Not a miscompile:** with `PLIRON_WASM_INLINE=0 PLIRON_WASM_BASICOPT=0
  PLIRON_WASM_PEEP=0` together the emitter cannot emit some bodies and
  stubs them (`pliron-wasm: loro_internal: 592/7416 functions stubbed`
  in the cargo log); a stub traps. The earlier "third bug with all
  wasm stages off" was that. A stubbed function is a silent runtime
  trap: the count should be an error unless asked for.
- The gate now replays `target/corpus/wasm-*` as wasm (`76b33be`).
  Before, a wasm corpus in `target/corpus/` made the gate red (every
  function panics as native code).

Both bugs sit in the two places the 05:20 note called uncovered: a
pliron-IR pass and the CLIF-to-wasm emitter. Neither has a
differential check. See "Architecture" in the summary for what I
would build.

## dsa, performance (07:30)

The module runs, so there is a benchmark. `harness/dsa/bench.mjs`,
features `eval,squish,cad`, node 26, ratio = pliron/LLVM, median of 9
interleaved rounds. "05:56" is the first table (snapshot 6 with
`PLIRON_SLOTMERGE_MARK=0` to dodge the memcpyopt bug); "now" is main at
`f13cb0d`.

| workload | 05:56 | now | |
|---|---|---|---|
| vm_loop | 1.56 | 1.03 | |
| vm_fib_rec | 1.25 | 0.97 | |
| vm_mandelbrot | 1.54 | 1.07 | |
| vm_physics | 1.49 | 0.97 | |
| vm_sieve | 1.42 | 1.02 | |
| vm_collections | 1.18 | 0.97 | |
| vm_strings | 1.21 | 1.05 | |
| vm_match | 1.22 | 1.03 | |
| vm_closures | 1.29 | 1.04 | |
| hash_text | 1.18 | 0.99..1.17 | bimodal between links of the same code |
| compile_page | failed | 1.05 | |
| checks_page | failed | 1.04 | |
| live_page | failed | 1.04 | |
| dims_page | wrong result | 1.24 | flat profile: allocator, `Ty::clone`, lexer |
| game_tetris | failed | 0.79 | |
| game_asteroids | failed | 1.44, **wrong result** | open, see below |
| crdt_edits | panic | 0.49 | not like for like, see below |
| crdt_sync | error | 0.51 | same |
| ink_strokes | panic | 0.37 | same |

The nine interpreter rows and hash_text: geomean 1.32 at 05:56, 1.02
now. The three CRDT rows flatter us: rust-lld wraps every export of the
LLVM module so that all static constructors run again on every call
(35% of `crdt_edits` there is `__wasm_call_ctors` and
`inventory::Registry::submit`); our linker now runs them once.

What moved it:

1. **`PLIRON_MEMCPYOPT_PATHWIN`** (`f9e1608`, default on in `f13cb0d`).
   The interpreter's `step_one` returns a 160-byte `RtResult<Flow>`.
   rustc emits `tmp = Flow::Next; memcpy(sret, tmp, 160)` at every
   `return`; `memcpyopt` forwards that into the `sret` slot, but its
   "nothing touches dst between the writer and the copy" test was
   dominators only, so every other return site's copy counted as
   "between" and every site was refused. Each interpreter step then
   executed a 160-byte `memory.copy`, which V8 runs as a call into C++
   (`wasm::memory_copy`: 17.5% of `step_one`'s own ticks at the call
   instruction, plus the callee). LLVM writes one word. Found with
   ticks per machine instruction of V8's TurboFan code for the one
   function, which is the only view that showed it: the function-level
   profile said "step_one is 2.2x slower" and the wasm text showed 17
   `memory.copy` in 14,000 instructions.
2. **`PLIRON_CONSTCOPY2`** (`bf4bba6`): blake3 on wasm32 kept 112
   bounds checks and table loads per block that LLVM folds
   (`compress_in_place` 5088 instructions against 2106). The pass that
   folds copies of constant tables ran before phisimp turned the
   inlined `round` argument into a constant; sroa hides that on native
   by splitting the row copy into word loads, and
   `PLIRON_SROA_FRAGCOPY` keeps 4-byte pieces whole. *Iffy:* that
   FRAGCOPY rule counts "words" as 8 bytes on every target; on wasm32
   a `[usize; 16]` is sixteen sub-word pieces to it. I reran the
   constant-copy pass instead of touching the rule. Worth a look.
3. **`PLIRON_WASM_SMALLCOPY`** (`03668fd`): 670 `memory.copy` of at
   most 16 constant bytes in the module (359 of zero bytes: BTree
   nodes of zero-sized values) become 39. No measured effect on these
   workloads. **Opt-in again since 09:30** ("Review these" item 15).
4. **Static constructors** (`30ff21b`, by a sub-agent in a worktree,
   reviewed and cherry-picked): `pliron-wasm-ld` built
   `__wasm_call_ctors` and nothing called it in a `--no-entry` link, so
   `inventory::submit!` registered nothing and every page that converts
   units failed to compile ("invalid evaluation"). Exports are now
   wrapped to run constructors once, by priority. *Deliberately not
   lld's behaviour:* lld calls them on every export call. Also not done:
   `__wasm_call_dtors` is never called; the once-guard is a plain
   load/store (two threads entering at once could both run them).

Tried and not kept:

- `PLIRON_INLINE_CALLER_MAX=4000` (default 20000): module 50.3 -> 44.9
  MB, but vm_physics 0.99 -> 1.24, vm_match 1.01 -> 1.09, hash_text
  1.00 -> 1.32. A caller size cap is the wrong lever for the size gap.
- `PLIRON_INLINE_ONCE=1` (single-caller inlining, off on wasm since
  the 40-op days): functions 35,774 -> 27,066, but code 40.6 -> 43.9 MB.
  Pages 2-3% faster (compile_page 1.03 -> 1.00, dims_page 1.26 -> 1.22,
  tetris 0.78 -> 0.71), kernels `hmget` +5.5% -> +19.9% and `tokenize`
  -14.4% -> +3.1%: with it the whole kernel (`repeat`'s closure, one
  caller) lands in the exported function next to its set-up code and
  the same loop runs slower there. That is the thing to understand
  before turning it on; LLVM inlines the same way and does not pay.
  Limiting it to small callees does not dodge it:
  `PLIRON_INLINE_ONCE=1 PLIRON_INLINE_FREEMAX=256` leaves hmget and
  tokenize alone but moves sieve from -3.9% to +2.1% (module -1.0%).
  In all three the kernel body ends up nested inside the exported
  function's `repeat` loop instead of being called from it.
- Folding every GOT `global.get` to `i32.const` in the linker (2404
  globals -> 1, +0.18% size): no change outside noise. V8 handles an
  immutable global as well as a constant here.

Still open:

- ~~`game_asteroids` returns a different result~~ **Fixed 08:00
  (`154456d`), the third silent miscompile of the night.** f32
  coordinates of the rapier2d soft bodies were off in the last bits
  from the first frame. The sub-agent bisected it in an hour: crate
  rapier2d, stage wpeep, sub-pass store-to-load forwarding
  (`PLIRON_WASM_SFWD_FW=0`), function `symmetric_eigen`; 70 std float
  operations and 50 `libm` functions had been checked bit for bit first
  (`/tmp/dsa-bench/repro-fmath`) and agreed. With the pass named, the
  bug was readable: at a loop entry the forwarding table kept every
  entry whose base local differs from the body's stores (`b3 != b2 ||`),
  where the in-line rule drops them because two locals may hold one
  address. A load at the top of a loop then returned the pre-loop value
  on every iteration: the eigen solver iterated on stale values. The
  kernels are unchanged by the fix. Notes:
  `/tmp/dsa-bench/findings/asteroids.md`.
- **Code size: 50.3 MB against LLVM's 18.4 MB.** Not improved tonight.
- `dims_page` 1.24x: diffuse.
- The default feature set (polars, typst, vortex) was never built with
  pliron: 5-7 GB per rustc.
- `PLIRON_WASM_ADDROFF=1` (opt-in, used by nothing) miscompiles a store
  in the new constructor smoke test: it folds a `+3` on the stored
  *value* into the address offset. Found by the sub-agent, not fixed.
- No fuzz sweep on any of tonight's default flips: this machine has no
  rustlantis generator (`harness/run.py --suite fuzz` runs zero
  tests). Gate and `harness/crates.py --tier 1` (20 crates) are clean.
- `harness/dsa/workloads/` is untracked on purpose: the page sources
  are copies from the dsa repository and this fork is public.
- Four `host/rustc-ice-2026-11-16T09_*.txt` files appeared in
  `~/code/dsa` (a command replayed without `RUSTC_ICE`). Not touched.

## dsa, 08:40: final state

`main` at `532caf8`, defaults, quiet machine, all 19 workloads return
LLVM's results (`/tmp/dsa-bench/pkg-pliron-final2`, 49.4 MB):

| workload | pliron/LLVM | | workload | pliron/LLVM |
|---|---|---|---|---|
| vm_loop | 1.04 | | compile_page | 1.02 |
| vm_fib_rec | 0.97 | | checks_page | 1.02 |
| vm_mandelbrot | 1.08 | | live_page | 1.02 |
| vm_physics | 0.99 | | dims_page | 1.22 |
| vm_sieve | 1.01 | | game_tetris | 0.78 |
| vm_collections | 0.97 | | game_asteroids | 1.31 |
| vm_strings | 1.05 | | crdt_edits | 0.50 * |
| vm_match | 1.01 | | crdt_sync | 0.52 * |
| vm_closures | 1.02 | | ink_strokes | 0.38 * |
| hash_text | 1.18 ** | | | |

\* constructors, see above. \*\* an artifact of the interleaved
harness: 62% of that workload is JS copying the string in, and run
alone the three modules take 81.3 (ours), 81.3 (`FRAGWORD=4`) and 82.0
ms (LLVM) with blake3 at 32-33% of each. Fifteen rows without it:
geomean 1.03.

Two more fixes after 08:00, both found through the `FRAGWORD`
experiment:

- **`slotprom` built slot addresses as i64 on every target**
  (`fbcf813`). The dsa module compiled with `PLIRON_SROA_FRAGWORD=4`
  did not validate (`local.get fp; i32.wrap_i64` in
  `Page::tape_pack`): slotprom's writeback before a synced call and
  its reload after it used `stack_addr.i64`, and on wasm32 the emitter
  resolves a slot address to the i32 frame pointer and then "narrows"
  it. Nothing in a default build reached that path on wasm, and the
  CLIF verifier accepts the type. The emitter now refuses a non-i32
  `stack_addr` with a panic naming the cause (`96e7801`) instead of
  writing a module that fails at instantiation.
- **The `PLIRON_SROA_FRAGCOPY` word is the pointer width**
  (`532caf8`): 8 bytes on native as before, 4 on wasm32, where a
  `[u32; 16]` or a 2x2 f32 block was "sixteen sub-word fragments" and
  stayed in memory. rapier2d's FEM assembly: game_asteroids 1.43 ->
  1.31. strwork +5.6% -> +3.9% in a same-run A/B. Module 50.3 -> 49.4
  MB.

`game_asteroids` at 1.31 is `SoftFemSystem::assemble_operator` and the
conjugate-gradient solve (soft FEM group: about 1220 ms of samples
against LLVM's 670 before the word change). Not looked at further.

wasm kernels on the same commit (`--layouts=3 --best`): geomean 0.928;
strwork +6.5%, hmget +6.0%, hashmap +2.8%, nbody +1.0%, sortw +0.6%,
chars_mixed +0.4% behind; the other ten ahead by 3 to 27%.

Last leads and housekeeping (08:50):

- `assemble_operator` (the game_asteroids gap): ours is 6,938 wasm
  instructions with 43 loops, LLVM's 3,497 with the small fixed-size
  matrix loops flattened (258 `f32.mul` in line). `unroll` sees only 16
  candidate loops there and calls 14 undecidable (`no: undecided blockN
  brif`: the trip walk meets a branch on a value it cannot evaluate).
  Raising its 8-loops-per-function cap to 64 changed nothing, so the cap
  is not it. `PLIRON_OPT_FNS=~assemble_operator
  PLIRON_TRACE=pliron::unroll=trace` on rapier2d shows the list.
- `gate.py` and `gate.py --only wasm` pass on `83fa711`.
- I removed tonight's superseded build trees under `/tmp/dsa-bench`
  (37 -> 23 GB) and the backend snapshots `target/dsa-snap*` except
  `snap19` (= the dylib of `532caf8`). The repro scripts under
  `/tmp/dsa-bench/repro-*` and `asteroids/` name snapshot paths that
  are gone; pass `PLIRON_BACKEND=` (and for the single-crate rustc
  lines, expect a relink of dependents: the backend path is part of the
  crate hash). The bugs they reproduce are fixed and have tests in the
  repo.

## Architecture observations

What the night's evidence says about the compiler as a whole, as
opposed to the individual fixes. Each has the cases behind it.

1. **Checking stops at CLIF, and that is where the bugs are not.** The
   differential checker, the corpora and the verifier cover the CLIF
   pipeline. The dsa module found six wrong-code or invalid-output
   bugs in one night and none was a CLIF pass: an IR pass
   (`memcpyopt` and lifetime markers), the wasm emitter (i128 `>=`),
   two wasm peepholes (keep-on-stack, loop-entry forwarding), the
   linker (constructors), and a CLIF pass whose output was valid CLIF
   but wrong for the target (`slotprom`'s i64 addresses). Every one
   was found by a whole program and a bisect. What would have caught
   them mechanically:
   - *IR passes:* lower the function before and after the IR pipeline
     and run the existing CLIF interpreter diff on the pair. No new
     interpreter; the lowering exists.
   - *Emitter and wpeep:* run each emitted body in a wasm interpreter
     on the diffcheck's inputs and compare with the CLIF result (waffle
     ships an interpreter; `run_straight` in `src/wasm.rs` is the
     straight-line start of one). Until then, validate every emitted
     module in the gate, and keep table tests like `ops128` for each
     hand-lowered family (floats are next: the sub-agent's
     `repro-fmath` compares 120 float functions against an LLVM build
     and could be a smoke leg).
   - *Whole programs on wasm:* `harness/crates.py` is native only. A
     wasm leg (the same crates' tests under node or wasmtime) would
     have hit the i128 compare on the first crate that prints a u128.
2. **Side information that passes must remember to maintain gets
   stale.** Lifetime markers are consumed by `slotmerge` as kills, and
   any pass that moves a write has to move them too; `memcpyopt` did
   not, and `stale_markers`, the safety net, is defeated by a partial
   write. The same shape: `wpeep`'s forwarding table with one aliasing
   rule in line and a weaker one at loop entries. Where two pieces of
   code must agree on a rule, make it one function; where a fact is
   derived (liveness), prefer computing it from the accesses and using
   the marker as an upper bound that is checked, not trusted.
3. **Target facts live in constants scattered through passes.** `8`
   as the word size in `sroa`'s fragment rule and `types::I64` as the
   pointer type in `slotprom` were both wrong on wasm32, one a
   performance loss and one an invalid module. A `Target` value
   (pointer type, word size, what a call costs, "memory.copy is a
   call") handed to the IR and CLIF passes, and a lint for `types::I64`
   next to `stack_addr`/address arithmetic in non-test code, would
   remove the class.
4. **Pass order hides constants.** The inliner binds a callee's
   parameters through block arguments that only `phisimp` removes, so
   every pass between them sees `gep(table, param)` where the argument
   is a constant (`constcopy` and blake3; the bounds checks that then
   survive). Either substitute constant arguments at inline time or
   run the cheap canonicalisation (phisimp, constant folding)
   immediately after inlining and before anything that pattern-matches.
5. **The cost model for wasm is V8's, and it is not instruction
   count.** Measured tonight: `memory.copy` of any size is a C++ call
   (one per interpreter step was a 50% slowdown); an immutable
   `global.get` costs nothing against `i32.const`; a function-level
   profile cannot show either. `harness/dsa/hot.sh` (ticks per machine
   instruction of one function, with V8's runtime calls named) is the
   tool that showed it and is now in the repo, with `cmpprof.py` to
   join two profiles by function. Earlier findings of the same kind
   are in "Findings worth keeping": V8's inlining budget depends on
   the module's share of small functions; a move-only branch arm is
   free and an arm with one instruction is a block and a jump.
6. **`return` through a temporary is the shape rustc emits, so its
   removal has to be complete.** `tmp = value; memcpy(sret, tmp)`
   appears at every `return` of an aggregate. One conservative test in
   `memcpyopt` (dominators instead of paths) left all of them in any
   function with more than one return site. A first census, rapier2d
   on wasm32 at `c6989d3` (`PLIRON_TRACE=pliron::memcpyopt=debug`):
   10,464 copies forwarded; refused: 3,227 "dst has no provable base"
   (the destination is a loaded pointer or a parameter that is not
   `sret`: `vec.push(value)`), 998 opaque uses of the temp, 911 "copy
   writer's src may alias dst", 656 "writer does not dominate the copy"
   (sibling-arm partial writers), 479 kept for promotion, 238 + 176
   window and route touches. The first class needs the `&mut`/`noalias`
   facts the CLIF side already has; the fourth is the next cheap one.
   And 1,991 of the forwards (19%) now drop the destination's lifetime
   markers (the 06:00 fix): moving the start marker up to the first
   writer instead would keep those slots mergeable.
7. **Inlining on wasm has no notion of a body that dies.** A flat
   512-op limit, no single-caller rule, no growth budget: 35,000
   functions and 49 MB against 19,544 and 18 MB, with the difference
   in the 1-10 KB range (three times as many functions, three times
   the bytes). The single-caller rule fixes the function count and
   speeds the pages up, and slows two kernels by putting a hot loop in
   one function with its set-up code. Splitting cold set-up out of a
   function (or not inlining *into* a function whose hot loop it does
   not feed) is the missing half; a caller-size cap is not (it cost
   speed everywhere for 10% of size).
8. **Benchmarks need their own checks.** `hash_text` swings 1.0 to 1.3
   in the interleaved table and is at parity run alone; the CRDT rows
   compare our once-run constructors with lld's rerun-on-every-call
   wrappers. A row that is mostly JS, or mostly a toolchain artifact,
   should say so in the harness output.

Final table on `d166d97` (SMALLCOPY off again), `pkg-pliron-final3`: all
19 workloads equal; the rows are those of the 08:40 table within a
point (vm 0.96..1.07, pages 1.02..1.03, dims_page 1.22, tetris 0.75,
asteroids 1.31).

## Afternoon (10:00 to 15:00): the review list, worked through

Asked for: SMALLCOPY, the unoptimized build first, how single-caller
inlining slows three kernels and whether the pages gain can be kept
without that, the eight architecture items, sccache, the size outliers.

### The unoptimized build (fixed), and SMALLCOPY (innocent)

The `PLIRON_OPT_BISECT=0` module was wrong in four workloads. Two bugs,
both in stages that run with every pass off:

- **wpeep store-to-load forwarding** named a slot by a local that was
  overwritten between its `local.get` and the `store` (the keep-on-stack
  shape). `2ba0f69`, test
  `sfwd_does_not_forward_a_local_overwritten_before_the_store`. Found by
  `PLIRON_WASM_PEEP_LIMIT` (body 2077 of `literate_wasm`,
  `debugsession_compileInfo`) and `PLIRON_WASM_SFWD_FWLIM`.
- **waffle treeify** rematerialized a tree that owned a load, so the
  load ran again after a store to the same address. `5d703d0`
  (`validate_load_sinks` now sees the `remat` set).

With both, the unoptimized module returns LLVM's results on all 19
workloads, with SMALLCOPY on or off: the `vm_strings` garbage blamed on
SMALLCOPY in item 15 was the treeify bug, whose output depended on what
the stale load happened to read (the `ob0c` and `ob0d` modules are
byte-identical where I had said they differed). `tests/wasm/smoke` has a
third object compiled with `PLIRON_OPT_BISECT=0` now.

### `black_box` was not a barrier on wasm (`90c2c6f`)

Found while reading kernel code for the inlining question: the wasm
`black_box` was an alloca and a volatile load in the IR, and our own
CLIF passes folded it (`k_fib`, `k_eqbug` and hmget's loop ran with no
barrier, so those kernels were flattered). It is the opaque call and the
CLIF `black_box` op again; the emitter expands it to a store to a frame
slot of its own and a reload whose alignment hint is below its width,
which `wpeep` reads as "do not forward". `cse_key` returns `None` for
the op (dgvn had merged the barriers of unrolled iterations). Honest
numbers after the fix: eqbug -3.3% to -1.6%, contains_rng -0.6% to
about +9% (a real gap now: its bounds are no longer constants), the rest
unchanged; geomean about 0.93.

### sccache and cargo (`e624fb9`, `9202d0f`)

`harness/backend_path.py` prints a private copy of the dylib under
`target/backends/<key>/`, the key hashing the dylib's contents and the
code-affecting `PLIRON_*` environment. Passed as `-Zcodegen-backend`,
that path makes both cargo's fingerprint and sccache's key right, with
no wipe. The dsa, variant, bench, smoke and crates scripts use it.
Measured by a sub-agent: sccache 0.19.0 hashes the arguments as text
and `CARGO_*` only; stale hits without the keyed path, none with it.

### How single-caller inlining slows the three kernels

Measured again on the fixed backend (`PLIRON_INLINE_ONCE=1`, against
LLVM): hmget +5.0% to +18.8%, tokenize -15.2% to +3.1%, sieve -2.7% to
-1.1%; hashmap and strwork gain 1.6 and 2.6 points; geomean 0.934 to
0.955.

- **hmget.** In the separate closure the map is a `noalias` parameter,
  and licm hoists the five loads of its header (items, the two hash
  keys, mask, control pointer) on that fact. Inlined into `k_hmget` next
  to the set-up code the map is `black_box`'s result, an ordinary
  pointer, and SipHasher's tail buffer is a stack slot that
  `isolated_escapes` calls escaped because it is read at a variable
  offset (`buf[i]`). A store to an escaped slot may write anything, so
  the five loads stay in the inner loop.
  `PLIRON_LICM_SLOTPROV=1` (`0ec8c12`, opt-in) is the root fix for that
  half: `slotprov` tracks slots whose address only feeds loads, stores,
  compares and `iadd`s with an integer that cannot hold an address (one
  computed from constants and loop counters; CLIF has no
  `getelementptr`, so `heap.wrapping_add(&local as usize)` has to be
  excluded this way). Such a slot and a root outside its class are
  different objects. The loads hoist again: +18.8% to +11.2%. The
  default build is byte-identical with the knob on; on the dsa corpus it
  changes two functions' sizes. With the rule the lookup loop's final
  CLIF is block for block the closure's (the same 21 blocks with the
  same instruction counts and the same eight loads), so the remaining 6
  points arise after CLIF: the loop now shares one wasm function with
  the map's set-up code. Not profiled under V8.
- **tokenize.** `run` is `for _ in 0..10 { n += s.split_whitespace().count() }`.
  By default the IR inliner leaves the iterator's `next` (one caller,
  has a loop) out of line, the CLIF pipeline optimizes it alone, and the
  *waffle* inliner puts it into `run` afterwards. With the bonus the IR
  inliner merges them first (68 blocks become 225 going into the
  pipeline) and the pipeline does worse on the merged loop nest (3 loops
  and no memory traffic instead of 4 loops with the cursor in memory,
  and 19 points slower). The -15% we hold over LLVM there is a property
  of the split function; what exactly the merged form loses was not
  profiled.
- **sieve.** `sieve::sieve` takes `Vec::from_elem` (1,586 to 1,891 wasm
  instructions); 1.6 points, not looked at further.

### Keeping the pages gain: three modes, measured on a quiet machine

All three kernel losses are a callee *with a loop* moved into its caller
before the CLIF pipeline, so two restricted modes exist (both opt-in):
`=2` gives the single-caller bonus to loop-free callees only (`fe28de4`),
`=3` also to a callee with a loop when its caller has no loop and has
taken no such callee yet (`126ec04`).

dsa, each column against the same LLVM package (`pkg-llvm-eval2`, the
same dsa HEAD as ours), 9 rounds, machine at load 3; the geomean is over
the 14 like-for-like rows (vm, pages, asteroids). `rx` is the recursion
rule of the last section, which is what the default is now:

| | default | `=2` + rx | `=1` | `=3` + rx |
|---|---|---|---|---|
| vm_loop | 1.082 | 1.068 | 1.040 | 1.051 |
| vm_collections | 0.959 | 0.946 | 0.881 | 0.886 |
| vm_strings | 1.077 | 1.025 | 0.975 | 0.969 |
| compile_page | 1.023 | 1.018 | 0.990 | panics |
| checks_page | 1.025 | 1.007 | 0.980 | panics |
| live_page | 1.020 | 1.010 | 0.988 | panics |
| dims_page | 1.240 | 1.231 | 1.200 | panics |
| game_asteroids | 1.412 | 1.384 | 1.364 | panics |
| geomean, 14 rows | 1.063 | 1.056 | 1.025 | (0.993 on the 9 vm rows) |
| module, MB | 49.81 | 44.40 | 49.57 | 43.47 |

So the full bonus is worth 3.6% on this program and `=2` keeps about a
fifth of it. The earlier reading ("pages 2-3%") came from a loaded
machine and undersold it.

Wasm kernels against LLVM, same quiet machine:

| | default | `=1` | `=3` |
|---|---|---|---|
| hmget | +5.6% | +18.8% | +19.0% |
| tokenize | -14.2% | +3.1% | -14.1% |
| sieve | +0.7% | (+1.6 pts) | -0.4% |
| hashmap | +2.5% | +0.5% | +0.3% |
| sortw | +4.6% | | +1.4% |
| geomean, 16 | 0.930 | 0.955 | 0.938 |

`=2` leaves every kernel where it is (0.920 against 0.920, measured
earlier). `=3` protects tokenize and sieve and still loses hmget. The
inliner's trace says why: the hot closure stays out of line and is
optimized alone (the waffle inliner merges it afterwards), but
`hmget::setup` now moves into `k_hmget`, which has no loop of its own.
There the map is a local whose address goes to calls
(`reserve_rehash`), where in `setup` it was the `noalias` out
parameter, and the insert loop reloads its header. With
`PLIRON_LICM_NCC=1 PLIRON_LICM_SLOTPROV=1 PLIRON_SCOPED_NOALIAS=1` on
top hmget is +11.7%, the same residue as under `=1`. The root is one
thing in both modes: **a local that escapes only to calls is treated
worse than a `noalias` parameter**, and single-caller inlining turns the
second into the first. That is the work item, not another inliner mode.

**`=3` + rx miscompiles the dsa module**: every page and game workload
dies in dlmalloc's `assertion failed: psize <= size + max_overhead`
(heap corruption); the vm rows pass. `=1`, `=2` + rx, rx alone and the
default all return LLVM's results on all 19 workloads, so the new
inlining decisions expose a bug that is latent in the others. A
sub-agent is bisecting it. Until it is found **no single-caller mode is
turned on for wasm**: `=2` is validated by the same 19 workloads that
`=3` fails, which is not enough once a neighbouring configuration is
known to corrupt the heap.

(The first dsa build under `=2` needed a fix: `webmerge` wrote a key's
i64 offset sum into an `iconst.i32` unmasked and the verifier rejected
`SpecFromIterNested::from_iter` in mimas-vm, `6d4a4d1`.)

The other opt-in knobs on the same quiet run, 14-row geomean against
the default's 1.063: `PLIRON_LICM_SLOTPROV` 1.059,
`PLIRON_MEMCPYOPT_MOVEMARK` 1.062, `PLIRON_WASM_SMALLCOPY` 1.061
(vm_mandelbrot and vm_physics two points better, the rest equal). All
19 results equal in each. None is worth a default on these numbers.

`hash_text` reads 1.18 for whichever module is loaded third and 0.99
for the second; alone each module takes 107 ms. It is a position
artifact of the harness on a row that is 63% JS.

### The benchmark's own checks (item 8, `658b519`)

`bench.mjs` loads a second, instrumented instance of each side and
reports per row how much of a call is spent outside the module and how
many times JS enters it; a no-op export is timed alone. Measured:

- a no-op export costs **11.4 us in the LLVM module and 9 ns in ours**
  (rust-lld's `command_export` wrappers rerun the static constructors
  on every entry). That is 43% of LLVM's `crdt_edits` and 63% of its
  `ink_strokes`: those two rows and `crdt_sync` time the wrappers, not
  generated code.
- `hash_text` is 63% JS.

Rows like that get a note and a second geomean leaves them out. **The
headline dsa geomean of about 0.90 is mostly those rows**; without them
the module is a few percent slower than LLVM's, which is what the vm
and page rows always said.

### The differential checker on the whole dsa corpus (`9f9fba1`)

The gate replays every 8th function of `wasm-dsa-eval`. All 69,698
showed 15 interpreter mismatches the sample never met. None was a wrong
transform; three checker rules were:

- undetermined bytes were compared by address *set* (a write list holds
  the bytes that differ from the seeded memory, so a padding byte equal
  to the seed dropped out of one list): 7 functions after slotmerge;
- NaN sign and payload in memory were compared exactly: 8 functions
  after `commute` and the egraph;
- the call stub filled to the end of whatever slot its pointer landed
  in, so an out-slot merged into a larger slot took a longer fill in the
  rewritten function only.

A mismatch also prints the determined bytes that differ now. The per-
address rule is stricter than the old one in one way (a fill-dependent
write set used to forgive the whole trial) and that exposed 3 more
functions; with the stub bound, 4 are left on the dsa corpus and 1 on a
clcg sample, one of them a trap in the rewritten function
(`robust::incircleadapt`). A sub-agent is on those five.

### wpeep differential (`19d4dd6`..`ed5771f`, sub-agent)

`harness/wasm/peepdiff.mjs` runs every body before and after the
peepholes in V8 on a `PLIRON_WASM_MODDUMP` dump and compares calls,
results, traps, memory and globals; fuel instead of a deadline, typed
stubs for indirect calls, aliasing pointer inputs. With each of the
three wpeep bugs of this session put back it names the function of the
original report; on the fixed backend it is clean on 68,555 dsa bodies
and on the unoptimized build. It is a leg of `tests/wasm/smoke.sh`.
Block coverage is 27% on dsa, so a clean run is not a proof.

### Smaller items

- `PLIRON_PHISIMP_EARLY=1` (`f80753a`, opt-in): phisimp before
  instcombine, for inlined constant arguments (item 4). Kernel module
  180,013 to 179,793 bytes; not timed.
- `PLIRON_MEMCPYOPT_MOVEMARK=1` (`92d914a`, opt-in): a forward whose
  writers land ahead of `lifetime.start(dst)` moves the marker ahead of
  the first writer when everything is in one block and nothing touches
  `dst` on the way, instead of dropping all of `dst`'s markers (items 2
  and 6).
- A source lint against literal `types::I64` addresses in CLIF passes,
  and emitter asserts for `func_addr`/`symbol_value` (item 3, `2371f52`).
- `PLIRON_INLINE_RECX`: see the next section; on by default for wasm.

### The size outliers

`Matrix::defaulted` is 732 bytes under LLVM and 721 KB under pliron.
The inliner's trace for it (`PLIRON_TRACE=pliron::inline=debug` on a
rebuild of `mimas-solve`): 317 accepted sites, 664 "recursive"
rejections, 126 "cap". Its body calls `rest.to_vec()` on a slice of AST
patterns, and the derived `Clone` of the AST is a call-graph cycle of
dozens of functions (`ExprKind::clone`, `Box<Expr>::clone`,
`to_vec::<Expr>`, `to_vec::<Stmt>`, ...), each under the 512-op limit.
The inline history stops the *same* function twice on one chain; a
caller outside the cycle still unrolls the cycle along every path
without a repeat, until the 20,000-op caller cap. LLVM never starts:
its cost counts 25 per call, so a clone with a dozen calls is over the
225 threshold. `PLIRON_INLINE_RECX=1`: a site cloned out of an inlined
body may not inline a member of a cycle its chain has already entered.
The kernel module is byte-identical with it on.

On dsa (`5564e82`, test stanza `inlinerecx`): module 49.81 to 45.15 MB,
code bytes 40.1 to 35.5 MB, 35,766 to 35,550 functions.
`Matrix::defaulted` 192,843 to 1,597 bytes, `fold_mimas_call` 136,536
to 7,485, `check_call_wheres` 122,306 to 4,341, `Diff::compose_ref`
71,028 to 2,674. All 19 workloads equal; the 14 like-for-like rows
1.063 to 1.061. **On by default for wasm** as
`PLIRON_INLINE_RECX_WASM` (`bbd0cc5`); native is unchanged and
unmeasured. What is left at the top: `Compiler::compile` 305 KB (LLVM
74 KB), `Search::go_explore` 276 KB, `MovableListState` 110 KB: single
large functions, not this pattern.

### Things that went wrong in the process

- A benchmark chain ran while a dsa build and the gate were running:
  ratios of 0.26 to 6.69 inside one row. Every dsa number above is from
  a rerun with nothing else of mine running. Benchmarks and builds do
  not share this machine.
- I edited `test.sh` while the gate was running it; bash reads a script
  as it executes, and six stanzas failed with "command not found" on
  half-read lines. The rerun was green.
- The disk filled (thirty 0.8 GB dsa trees, four agent worktrees with
  their own `target/`): a build died with "No space left on device",
  which rustc reports as "the compiler unexpectedly panicked". 41 GB
  freed by deleting finished configurations' trees and the finished
  worktrees' `target/`.
- `gate.py --full` replays the whole dsa corpus and reports two
  functions: the `memcpyopt` marker miscompile fixed at 06:18, in a
  corpus recorded at 04:45 (the sub-agent's finding,
  `docs/history/2026-11-16-diffcheck-five.md`). The corpus is being
  re-recorded.

