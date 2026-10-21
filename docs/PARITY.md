# LLVM parity: where we stand, what to do next

The goal is parity with stock rustc + LLVM on four axes: speed of the
generated code (native, wasm) and compile time (native, wasm). This page
is the current scoreboard and the ranked work list. Update it when a
number moves (`harness/parity.py` regenerates the tables).

**Compile-time rows last measured:** 2026-10-20 (branch `perf/ctime`, see
§1 "Overnight 2026-10-20"), interleaved median of 5 against stock rustc on
a shared box (load ~8). Runtime rows are older:

**Last measured:** 2026-10-18, aarch64-apple-darwin (Apple Silicon),
backend `40d729d` + parallel-CGU jobs (`src/cgupool.rs`), toolchain
nightly-2026-10-06. `harness/parity.py`, nothing else running.
Ratio = pliron / LLVM: **below 1 means pliron is better.**

| axis | pliron / LLVM | verdict |
|---|---|---|
| native runtime (geomean, 16 kernels) | **0.900** | ahead; 4 kernels still behind |
| wasm runtime (geomean, 16 kernels) | **0.917** | ahead; real gaps: `strwork` +26%, `hmget` +17%, `parse_only` +9% |
| wasm module size | 1.93 (183.9 KiB vs 95 KiB) | behind |
| compile time: bench kernels, native | **0.59** (0.25 s vs 0.43 s) | ahead (2026-10-21) |
| compile time: bench kernels, wasm | **1.17** (0.62 s vs 0.54 s) | behind (2026-10-21; tiny input, rustc-latency-dominated) |
| compile time: regex-syntax (~40 kLOC) | **0.93** (1.37 s vs 1.48 s) | parity on wall (2026-10-21) |
| compile time: regex-syntax, wasm32 (one CGU) | **1.08** (2.85 s vs 2.64 s) | near parity (2026-10-21) |
| compile time: syn 2 + `full` (~50 kLOC) | **1.10** (3.46 s vs 3.14 s) | behind (2026-10-21; was 15-22) |

Older measurements on the Windows i7 box (native 1.028, wasm 1.157) are
in `docs/history/PERF-REPORT.md`; the narrative of how each kernel got
here is `docs/performance.md`. Windows i7 wasm (post `br_if` fold +
`NonNull`-i32 `PLIRON_NN_ANYINT` + `dem64` + `clifpeep`
`ext_load`/`icmpk`/`addnorm`/`icmpred`/`bitmerge`/`post_sweep` +
`divnarrow`/`ovfk`/`lih`/`band(uextend)` + `selord` (bool-algebra over
compare-outcome masks + `brif`/`sel` peels) + `unroll::merge_arms`
(`PLIRON_UNROLL_MERGE`, fixed-trip `binary_search` → `select` chain) +
`PLIRON_WASM_HOT=400` (loop-body `next_match`/`hash_one` inline) +
`wpeep` `sfwd` (`PLIRON_WASM_SFWD`, frame store→load forward — the
spliced-callee spill/reload, ~10 mem ops/iter on `tokenize`) + `wpeep`
`prel`-scoped `br_if` fold + else-arm structured `br`/`br_if`
retarget (~265 `if{br}else{structured}` sites → `br_if`, module
−1 KiB), `bench/wasm/run.sh`): **0.977 — ahead overall** — wins
`chars_alpha` −31%, `iterw` −41%, `matmul` −11%, `parse_only` −15%,
`eqbug` −9%, `sortw` −3%; `sieve` +1.9%, `contains_rng` +1.4% (parity).
Remaining >5%: `strwork` +20% and `hashmap` +16%/`hmget` +14%/`tokenize`
+16% — `strwork`'s `run` inlines ~4700 lines of `push_str`/`extend`/fmt
wrappers LLVM keeps as calls (494 lines) — `pliron-ir-opt`/`inline`
policy; `chars_ascii` +8.5%, `chars_mixed` +8.2%, `fib` +7.7%,
`nbody` +6% — emission-level locals churn.

## How to measure

- Everything: `harness/parity.py` (~15 min; `--skip native,wasm,ctime`,
  `--from-logs`). Writes `target/parity/summary.md`.
- One kernel's runtime: `bench/run.sh <name>` / `bench/wasm/run.sh <name>`.
- Compile time: `harness/ctime.py [--only regex-syntax]` (release
  dylib). Inside one compile: `PLIRON_TRACE_FILE=t.txt` then sum column
  4 by column 3 (`*iropt*`, `*modgen*`, `*pipeline*`, per-step names);
  `-Ztime-passes`; `samply record --save-only --unstable-presymbolicate` + `harness/sprof.py`.
- CLIF pipeline time without rustc: `clif-check bench target/corpus/std`,
  or the live dashboard's step-time panel (vs baseline, on every save).
- Noise: ±3% is parity on a quiet box. Criterion compares against an
  LLVM baseline measured minutes earlier; wasm uses interleaved rounds.
  Don't benchmark while the gate or another agent is building.
  Layout bias is real: identical bytes at a different address can swing
  a tight kernel ±10% (seen: `sieve`, +12% from a 0x740 shift — ASLR is
  page-granular so low address bits are per-binary constants). If a
  delta has no corresponding CLIF/asm diff in the hot path, check it
  with `bench/layout.sh <kernel> [N]` — relinks under shuffled
  `-order_file`s and reports the cross-layout median (~35 s for 3
  layouts, 1 kernel).

## 1. Compile time (highest priority)

### Overnight 2026-10-20: code size was the multiplier

`syn` (now a `harness/ctime.py` input, with `regex-wasm`) was **15x** LLVM
wall / 32x CPU — regex-syntax's 1.9x hid it. The cause was not per-op
weight but how much code every pass ran over: pliron emitted **24.6 MB**
of text for syn vs LLVM's 1.0 MB (regex-syntax 2.15 MB vs 0.22 MB);
`drop_glue<Box<Expr>>` alone was 628 KB vs 684 B. Compare per-symbol sizes
(`llvm-nm --no-llvm-bc -n` + `llvm-size -A` over rlib members) before
tuning passes. What moved it (all gate-green; objects byte-identical where
noted):

- **Inline history** (`PLIRON_INLINE_HIST`, `State::inline_hist`): a
  call cloned out of an inlined body may not inline a callee already on
  its clone chain (LLVM's `InlineHistory`) — recursion through drop glue
  and derived `eq` no longer unrolls lap after lap to the 20k-op cap.
- **SCC rule** (`PLIRON_INLINE_SCC`): a site created by inlining may not
  inline a callee in its caller's own nontrivial call-graph SCC (LLVM's
  CGSCC rule). History alone still made one lap through every distinct
  type of a wide recursive type graph.
- **`CALL_OP_W` 3 → 20** (`PLIRON_INLINE_CALLW`): LLVM charges a call
  several instructions' worth; at 3 a 40-call enum drop glue fit the base
  limit. 10 then 20 each runtime-checked (kernels geomean 1.001 for
  10 → 20, regex driver flat); 20 took syn CPU another −25%.
- **Loop bonus only for leaf-ish callees** (`PLIRON_INLINE_LOOPCALLS`,
  default 4 calls): parser `while` loops gave 25-call drop glues 3x the
  threshold. syn CPU −65% on its own.
- Together: syn text 24.6 → 2.9 MB, regex-syntax 2.15 → ~0.7 MB.
  Runtime: kernels alternating A/B (pre-session inliner vs all four)
  geomean 0.998, rechecked outliers within 0.5%; wasm kernels
  byte-identical; a regex-syntax parse+translate driver flat.
- **Hidden quadratics on CGU-wide ordinal columns**: `ColMap` (dense over
  every op/value/block ordinal in the CGU) iterated or allocated per
  function — lowering's `region_frozen_op` walk, callgvn's seven block
  maps, the inliner's per-caller `done_blocks`. Fixed by probing the
  function's own values / `helpers::SparseMap` for function-local
  scratch. `ColMap` itself is now a sparse set (`u32` index + compact
  entries) — inline `Option<(K, CallInfo)>` slots were ~150 B per op.
  A hybrid hashed/dense `ColMap` index was tried and was *slower*
  (+8% syn, +40% regex-wasm): hot CGU tables want the dense index.
- **`PLIRON_DEFERDEAD`**: emit-only `xcgu` copies lower last and only if
  some lowered body or data initializer references them (CLIF
  `user_named_funcs`), else get a one-`trap` body — safe by construction
  where IR-level stripping (`PLIRON_XCGUFN_KILL`) wasn't.
- **wasm**: the CLIF pipeline runs on the phase-(B) job pool
  (`run_jobs`, pipeline-only `JobShared::wasm`), and waffle's per-body
  compile + `wpeep` per-body peepholes run via `wasm::par_map` — single-CGU
  wasm builds were fully serial. Byte-identical.
- Two latent miscompiles surfaced by the new code shapes, fixed with
  regression tests: `clifpeep::selord_sel` minted an i8 zero for a wider
  `select` condition; `armmerge` paired twin arms ignoring the
  controlling type (`load.i8` vs `load.i16`).
- Tier 1 at `-O` (UI corpus) then found two bugs already present at
  `4f8018e`, both fixed with regression tests: `bool_mask` handed out a
  `src` that didn't compute its mask (`x ^ 1`, `icmp cc z, K` peels), so
  `selord_icmp` aliased `icmp eq 1, (c ^ 1)` to `c` (UI
  `deriving-cmp-generic-struct-enum.rs` wrong result); and `lin_flat`
  looped forever on an alias cycle in unreachable code (`v6 -> v15`,
  `v15 = iadd v6, 16` — UI `test-case-on-use-item-129765.rs` hung), now
  bounded by a visit budget. Root cause fixed too: `try_dominates`
  answered yes between any two unreachable blocks (default preorder
  numbers), so `remove_trivial_params` aliased a dead loop's header param
  to its own latch `iadd` (cranelift PLIRON_PATCH #57). Tier 1 -O: 16,517 pass;
  the only expectation miss is `hashmap-debug-format.rs`, skipped as
  nondeterministic (randomized `HashMap` order).
- rustlantis seeds 0-95 (`harness/run.py --suite fuzz`): 95 pass, 1 fail
  after `c00303c` + the iconst.i128/proots/dem64/licm fixes below — seed
  1 -O run timeout. Seed 14's debug-only
  `proots.rs:1211` offset-subtract overflow (release passes; fixed by
  wrapping key arithmetic — test `proots::tests::extreme_offsets_wrap`).
  Artifacts: `target/harness/fuzz/<seed>`.
  Seed 24 root cause: not `slotprom` — it merely created the trigger
  shape (a promoted i16 block param feeding `uextend.i128` into
  `isub.i128`). The four PLIRON aarch64 `iadd`/`isub.i128` + `uextend`
  fusion rules read the narrow operand's raw register (`put_in_reg`);
  `lsl`/`eor`-produced i16 values keep garbage in bits 16..32, so the
  "zero-extended" low half read 0x52FFFF instead of 0xFFFF. Fixed with
  `put_in_reg_zext64` (PLIRON_PATCH #39); regression `tests/zext128`.
  Seeds 65/89 shared one root cause: `lcse`'s `(x+c…)-x → Σc` fold and
  `bandk`'s `band(band x,k1),k2 → 0` fold replaced the inst with
  `iconst` at the result's own type — `iconst.i128` is invalid (verifier
  `unreachable`; rgmerge's lane-mask probe also overflowed reading it).
  Both now emit `iconcat` pairs (tests/i128const.rs); the verifier
  reports bad iconst types instead of panicking (PLIRON_PATCH #58).
  Seed 88's "tailmerge broke `load.i32` on an f64 address" was also
  misattributed — tailmerge's verify just surfaced it first (clifpeep
  runs unverified): `dem64`'s web admission matched on the
  `LoadNoOffset` *format*, which `bitcast` and `atomic_load` share,
  so `bitcast.i64 (bitcast.f64 K)` was demoted and its i32 twin
  emitted as `load.i32` of the f64 operand. `dem64_op` now admits no
  `LoadNoOffset` inst (an `atomic_load` twin would have silently
  dropped atomicity too); regression
  `clifpeep::tests::dem64_loadnoffset_is_not_a_load`.
  Seed 41's -O SIGSEGV was `licm` speculation: a `load.i128 notrap`
  at `stack_addr ss84 + 0x2518_0084_2820_8000` sat on a loop path
  the program never runs (so `notrap` held where it sat), and the
  `Root::S` dereferenceability check ignored the offset entirely —
  hoisting it to the preheader executed the wild address
  unconditionally. Stack-slot roots are now only proven
  dereferenceable within the slot's own bounds, in both the hoist
  `safe` check and `promote`'s entry-store requirement; regression
  `licm::tests::wild_offset_load_stays_in_loop`.
- Tooling: `clif-check step` now replays reap-gated steps
  (`iconst128`'s `post_sweep`) as the pipeline does — a diffcheck repro of
  a `post_sweep` miscompile used to print "unchanged".

Rejected / measured: `PLIRON_XCGU=0` (−35% CPU before the inliner
changes, but kernels alternating A/B: tokenize +70%, sieve +9.5%,
geomean +4% — copies are load-bearing; an earlier "30-180%" figure came
from the broken A/B script and is void); an EH-cold callsite rule (callsites reachable
only via landing pads; +27% syn CPU, because un-inlined cleanup callees
keep `xcgu` copies alive and fully optimized); `PLIRON_INLINE_CALLER_MAX`
8000/4000 (−5-9% CPU, runtime risk on giant hot fns).

Correction: commit `bda3f63` says "bench kernels: binaries
byte-identical" — that came from an A/B script whose `cargo clean -p
pliron-bench` missed the bench profile, so both sides were the same
binary. The object-level kernel diffs and the later alternating A/B
(clean with `--profile bench`) are the real evidence.

Where the rest is (samply on syn, `harness/sprof.py`): flat — IR-opt
~27% of samples (inliner cloning ~5%, sroa ~3.5%, nocapture ~3.6%), the
CLIF pipeline spread over ~90 steps, allocator/zone dispatch ~8%. Next:
(1) ~17% of syn's pipeline time is functions fully optimized in more than
one CGU, but live `xcgu` extras are only 4.5% of optimized ops — the rest
is rustc's own per-CGU local copies (`#[inline]`, drop glue), which LLVM
compiles per CGU too; redirecting live extras to their home symbol isn't
worth the ABI risk (`sret2reg`). (2) `slp::xpack_block` rebuilds the
whole-function use index after every applied pack (`Analysis::uses` keys
on `num_insts`): 2.3% of syn's samples; an incremental append of the new
insts' uses would remove it. (3) the regex-syntax native wall is gated by the biggest
CGU's IR-opt before its fact publish (other CGUs' stage B parks on it).

regex-syntax, one `rustc -O` (release dylib), 11.6 s wall:

| part | time | note |
|---|---|---|
| rustc frontend (shared with LLVM) | < 1 s | |
| our codegen (`codegen_crate`) | 10.9 s | LLVM's *entire* compile is 1.56 s |
| ├ IR-opt (`*iropt*`) | 5.1 s | was serial; now parallel across CGU jobs |
| ├ lowering phases A/B (`*phaseA*`, `*phaseB*`) | 5.5 s | |
| └ CLIF pipeline (`*pipeline*`) | ~43 CPU-s | parallel across lowering threads; 3,485 fns × every step |
| top CLIF steps (CPU) | jumpthread 2.3 s, ifconv2 1.3 s, slp 1.0 s, loadfwd 0.8 s, mergechain 0.8 s, dse 0.6 s, edgespec 0.6 s | |
| top IR steps | inline 1.1 s (+ inline2 0.4 s), sroa 0.6 s | |

Earlier profiles (docs/performance.md "Compile time") found the floor
with every CLIF pass off is still our glue: op construction, `State`
side-table hashing, lowering — not Cranelift. syn was 32x; flags alone
(`-Zthreads=8`, fewer CGUs) took it 102 s → 24 s.

Next steps, in expected-payoff order:
1. ~~Run CGUs' IR-opt in parallel~~ **done**: `compile_codegen_unit`
   stops after mono-item `define`s and queues the pliron
   `Context`+`State` as a job in `src/cgupool.rs`; `optimize` (rustc's
   jobserver-token workers) runs whatever job is runnable until its own
   module exists — IR-opt *and* CLIF lowering both moved off the main
   thread. The one cross-CGU edge (`xfacts` publish→adopt) splits into
   `facts_publish`/`facts_adopt` on per-CGU ordinals; a CGU's tail parks
   until every earlier ordinal has published, reproducing serial adopt
   sets exactly (parallel objects are byte-identical to
   `PLIRON_CGUPAR=0`'s). `--jobs-backend` (default: all cores) sizes the
   pool; `PLIRON_CGUPAR=0`, `PLIRON_OPT_BISECT` and `--jobs-frontend`
   keep the serial path. regex-syntax 6.57 → 2.27; kernels 1.94 → 0.98.
   Remaining in that gap: the front half (MIR→pliron `define`s) is still
   main-thread-serial, and each job's inner lowering used to spawn its
   own per-function threads — **done** (`e246be2`): one process-wide
   `VecDeque`+condvar queue sized to `available_parallelism`/`PLIRON_PAR`
   serves every CGU's phase-B jobs; `FnJob`/`JobShared` are fully owned
   (`'static`, profiler via Arc'd `SelfProfilerRef`) and a producer that
   finishes lowering drains queue items itself while waiting on its own
   results. regex-syntax wall was neutral (3.16→3.21 s, user +0.2 s —
   cgupool's ordinals already limited concurrent phase-B), but thread
   count is now bounded regardless of CGU fan-out and no core parks in
   a drain while jobs sit queued.
2. ~~Applicability prechecks~~ **done**: `shape_of` computes a bitmask
   of function-shape facts (loops, brifs, params, inst kinds) in one
   pass; `Step::needs`/`needs_any` gate heavy steps before their
   analysis builds, and the cached bits widen by each mutating step's
   `creates` (superset — a clear bit certifies absence; under-admission
   is a missed opt, never a miscompile; `PLIRON_SHAPE_CHECK` catches it).
   `clif-check bench target/corpus/std`: 35.5 s → 28.7 s (~19%).
3. ~~The heavy steps~~ **partly done**: `clif-check bench`: 28.7 →
   **27.5 s**. `remove_trivial_params`/`NonNull::new`/`remove_dead_params`
   now walk CFG predecessors of (candidate-)param'd blocks instead of
   every edge — `trivp` went from the #1 leaf (3443 samples) to ~1 ms.
   `Analysis` carries a `facts` slot (`proots::facts_cfg`) next to
   `amap`/`iso`: `dse_inputs` and `loadfwd::run` share one edge-fact
   build per version instead of 3-4 private scans. `webmerge_ctx`'s
   per-block `Instant::now()`s are gated on `wmprof`; `edgespec`
   reuses its entry cfg/dom for `splices` when nothing applied and
   skips the second sweep pair when splices is a no-op. Every
   diagnostics-only `Instant::now()` in the CLIF passes is now gated on
   its tracing target (`run_step`'s two per-step reads behind
   `pliron_cfg::step_timing_on()` — `mach_absolute_time` was ~2-3% of
   rustc-compile leaf samples); clif-check marks the flag itself so
   `bench`/`report` tables are unchanged.
   `dominated_conds` builds its domtree adjacency CSR (`head`/`kids`)
   instead of `FxHashMap<Block, Vec<Block>>` and hoists the per-block
   `fsv`/`eqs`/`alts` Vecs; `webmerge_ctx`'s `eargs`/`svh`/`okey`/
   `skc`/`pk` matrices are one flat `Mat` (stride-indexed) each;
   `slp` builds `param_src` lazily on the first block that reaches the
   plan stage. `loadfwd` carries one `NrMemo` (root_pair / norm_root-
   na0 / bounded root_min) per phase — the fixpoint and apply scan had
   re-walked each base pointer's ≤16-hop chain per round; `edgespec`'s
   DFS keeps block params as slices and clones edge args only on the
   landing `Fin` (rejections used to pay a `Vec` clone each);
   `jumpthread::eval`'s icmp arm reads `known` once per operand.
   `ControlFlowGraph` edge storage is flat CSR (sorted `edges` Vec →
   `succ_off`/`succs` + `pred_off`/`preds`; `PredIter`/`SuccIter` are
   slice walks now) and `Analysis` recycles cleared cfg/dt/la as
   compute-into shells, so the ~54k corpus CFG rebuilds reuse their
   backing storage instead of regrowing bforest node pools (the #1
   allocator ancestor, ~3.5% of pipeline samples via
   `visit_block_succs`): `clif-check bench` 28.4 → 28.0 s, .o members
   byte-identical, corpus clean. The CFG edge build itself is
   counting-sorted (O(E+B) bucket scatters keyed on block index, ~114 →
   58 ms of analysis-build time). `slp`'s per-block `SecondaryMap`s are
   stamp-reset dense maps allocated once per `run` (the reset was
   O(blocks × insts)); `jumpthread`'s nested `ifconv::run` is gated on
   an `ic_dirty` flag (only edits that can expose a new pure forwarder
   re-scan), a quiet round skips the identical follow-up, and the
   `NonNull` param-proof fixpoint persists across rounds as a bare
   `FxHashSet` with per-block eviction on retargets — dropped wholesale
   only on trivp/domcond/ifconv aliasing edits (jtround 612 → 528 ms).
   Dense-storage pass: `vmap::EMap`/`ESet` are generation-stamped dense
   entity-keyed maps/sets (`VMap`/`IMap`/`BMap` aliases) with O(1)
   clear and insertion-order iteration; the long-lived probe-heavy
   `proots` tables (`nf`/`canon`/`alias`/`affine`/`memo`) and
   `Pipe::derived` migrated, `clifpeep`'s block-keyed bounds memos are
   `BMap`/`BSet` and its `BoundsMap` chains share `Rc`s with COW on
   mutation, `webmerge_ctx`'s per-block matrices and `slp`'s per-block
   insts/stores buffers reuse caller-owned storage, and `WmCtx`
   refreshes cfg/dom/edges in place on const_sweep rounds. bench
   27.5 → 27.07 s; sparse per-edge/per-block scratch stays hashed
   (dense tables pay O(entity-space) allocation, wrong for a few dozen
   entries).
   Next: `Pipe::since_reap` gates `iconst128`'s bundled `post_sweep`
   (three whole-function walks skipped when nothing mutated;
   507 → 385 ms), and a `Cd` epoch box shares one cfg+domtree across
   each fixpoint's phases — `stale()`/`get()` recompute into the same
   storage instead of `with_function` per epoch (jumpthread, mergechain,
   edgespec, bittab, memspec, coldrets, switchmap, loopdel). bench
   27.07 → 26.94 s. cfg+domtree construction was already only ~1% of
   corpus time, so the plumbing mainly removes a repeated-build class
   rather than moving the needle.
   Left: jumpthread ~3.8 s (a ~6-phase
   fixpoint; `round`'s threading walk is the residue — `domcond`
   ≈0.7 s, `deadp` ≈0.3 s by knob A/B),
   the webmerge family ~4 s (`cparm`/`sameargs*` per-block key
   matrices + per-call `facts_cfg`/`affine_nf`/`Proots` Ascent —
   cross-round caching is unsound: `apply_param_drops` rewrites inst
   args and detaches params, so the `aff` facts themselves move), and
   mergechain ~1.8 s (`edgespec::run` inside ≈ half of it). The next
   tier is mechanical-and-risky, not algorithmic: `SymMap` u32 keys on
   the pliron-IR side, interned-sym inline maps.
4. **xcgu copies** multiply IR-opt work per CGU (`PLIRON_XCGU=0` cut syn
   78 → 55 s) but feed the inliner. Share analyzed bodies across CGUs
   instead of re-optimizing each copy. **CLIF-side mitigation landed
   (opt-in)**: `PLIRON_MEMCACHE=1` is a whole-pipeline content-addressed
   in-process cache — keys normalize `UserExternalName` indices to
   declared symbols and canonicalize pliron region `Ptr`s in the fact
   maps (`block_region`/`region_parent`/`scoped`/`operand_frozen`) so
   copies key alike; hits replay func+derived+facts+stats, remap
   `FuncId`s by symbol, and pending claims let parallel CGUs share one
   computation instead of racing misses. regex-syntax: 882/3386 lookups
   hit (26%), output byte-identical cached/uncached/repeated. CPU is
   ~neutral there — hits land on small fns and the ~0.9 s key/store
   overhead eats the replayed work — but it's the mechanism an
   iropt-side version needs, and it costs nothing when off. Bisect
   (`PLIRON_OPT_BISECT`) disables both cache tiers: a hit can't honor a
   consumer's remaining budget. Read-set recording now arms only for
   the disk tier — in-process env is fixed, so mem-tier validation is
   vacuous and arming used to de-memoize every `config()` read on
   armed threads (~1.5M recorded reads/compile). Remaining: the
   pliron-IR (iropt) half of the duplicated work is untouched.
   **Dead-copy mitigation landed (default-on)**: after inlining, ~73%
   of emitted fns are callerless `xcgu` copies kept only for link
   safety (rustc's used-lists can't see copy→copy calls, so `dead_fns`
   can't strip them). `PLIRON_DEADUNOPT` emits them through
   `Gate::Always` lowerings only — −3 CPU-s on regex-syntax, all
   symbols still defined. **`PLIRON_EARLYDEAD` (default-on)** marks
   them at iropt entry and post-devirt so `bodied`/`opt_body` passes
   skip their emit-only bodies entirely — ~8% wall on regex-syntax;
   the retained bodies emit rawer (+~6 KB object bytes), accepted
   since they're callerless (`=0` opts out). The remaining iropt duplication is live copies re-running
   pliron-IR passes per CGU. **Tried and rejected**: copies skipping
   body-rewriting passes outright (`opt_skip` on `xcgu_extra`) is
   ~+0.7 CPU-s *worse* — copy iropt is load-bearing prepay that shrinks
   the bodies before they splice into callers; unoptimized fodder just
   moves the cost downstream. Dedup must share the *result* of copy
   optimization across CGUs, not skip it.
   **Tried and rejected — inliner callsite worklist**: replacing
   `inline`'s per-round block rescan (`done_blocks`/`pending`) with an
   incrementally-maintained callsite index cut the `in.scan` span 8.2 s
   → 0.25 s but moved *zero* real CPU — the span was ~95%
   descheduled-time inflation from 16-jobs-over-14-cores, and the
   worklist flipped ~970 cap-boundary decisions toward more accepts
   (+0.15 s clone work, +0.3 s wall) — reverted. Kept: lazy caller-size
   (`op_count` computed only when a site reaches the cap check) and
   permanent `in.scan`/`in.call`/`in.info` `PLIRON_TRACE_FILE` rows.
   Lesson: per-step span times under the CGU pool read ~3-10x high;
   trust A/B `user` time for pass-level costs. The real inliner cost
   is `in.call` cloning (~1.4 s), already linear; the remaining
   pliron-IR mass is the same 80-linear-passes-over-big-functions
   shape as the CLIF pipeline (item 5).
   **Dedup ceiling measured — dead end on regex-syntax**
   (`PLIRON_MCLOGFN=1` names hit/miss fns in the trace): all 861
   memcache hits land on ~58-inst fns (~0.6 s pipeline total); every
   >1 s pipeline fn is *unique* — the 15-35k-inst `drop_glue`/derived
   `eq`/`parse`/`translate`/`visit_post` towers exist once per crate,
   so whole-pipeline dedup saves ≈0 there. An earlier "53% of pipeline
   ms on duplicate names" estimate was a name-truncation artifact.
   **Giant-cap and diet pipelines measured — rejected**: the >5k-inst
   fns carry ~65% of pipeline cost (`PLIRON_GIANT_MAX=5000` →
   20.6→11.5 user-s, wall 3.05→1.97) but they're also hot:
   GM=5000 costs ~8% runtime on a parse loop bench (2.48→2.67 s),
   GM=2000 ~15% — and pliron is *already* ~21% behind stock's 2.05 s
   on that bench, so the quality can't be traded. A 7-step giant
   allowlist (`PLIRON_GIANT_STEPS=jumpthread,ifconv2,dse,loadfwd,licm,
   indvars,clifpeep`) recovered only ~⅓ of the loss; giant-fn quality
   is diffuse across ~85 steps, no tier wins cleanly. Concentration:
   top-10 fns ≈ 50% of traced step time, top-50 ≈ 89%; <1ms fns are
   ~64% of fns but ~1% of cost — small-fn overheads are irrelevant.
   **Incremental pass wins keep landing but are ~100-300 ms each** —
   the corpus is uniformly well-gated/worklisted: `selfarg` 861→137 ms
   (batch param drops + `BPARAM` gate), `callgvn` 1283→807 ms
   (borrowed callee names, op-ptr buckets), `edgespec` SmallVec edge
   args, `shape_of` lazy per-block maps + early exit, `dispatch_dup`
   lazy `escaping` scan, pliron `PoolIter` for the op accessors.
   regex-syntax user-s ~20.7→20.4; wall is saturated (~6.7x on 14
   cores) so CPU savings don't move wall. `bound_at`'s `isub`
   unsigned-bound was unsound (`fd0fb39`): `a - b` wraps on underflow
   — fixed in `55707f3` with a regression test.
   **LLVM's own numbers** (`-Zllvm-time-trace` on the same compile):
   LLVM's *entire* function-pass manager is 2.2 s vs our ~14 CPU-s
   pipeline; per pass it's ~15-45x — JumpThreading 91 ms vs our
   `jumpthread` ~3.7 s, DSE 79 ms vs ~2.0 s, SLPVectorizer 56 ms vs
   ~2.5 s, IndVarSimplify 101 ms vs `indvars` ~1.3 s. LLVM also starts
   from MIR-opt-pre-cleaned IR (rustc runs inline/SROA-ish/GVN-ish
   passes upstream for it; we redo that work at CLIF). The gap is
   per-inst pass efficiency on ~20-40k-inst functions, not pass count
   or algorithm — the remaining lever is making the top-6 steps
   (jumpthread/ifconv2/slp/loadfwd/mergechain-family/dse, ≈13 s
   traced) cheaper per instruction: `Uses`/alias-map reuse across the
   mutation boundary, `an`-cached facts, and per-step worklists
   instead of full scans. Version-skip for repeat steps (licm×3,
   dse×3, lcse×4…) was analyzed and rejected: the big fns mutate
   constantly, so a dirty flag almost never holds on the fns that
   carry the time.
5. ~~Glue~~ **partly done, premise revised**: the inter-step
   bookkeeping is now ~3% of phaseB CPU. `carry_aliases`/`compact_aliases`
   walk `dfg.alias_writes()` (the per-write list behind `alias_epoch`)
   instead of rescanning `values()` — step-glue −41% on regex-syntax.
   `sample` profiling says the remaining mass is the ~80 per-fn scan
   loops themselves (`inst_values`/`Uses::new`/`resolve_aliases` ≈ 26%
   of leaf samples, spread across every step); `FxHashMap` side-table
   hashing is only ~6%, so dense columns are no longer the obvious win
   (DoD step 4, `docs/history/dod-steps-4-5.md` still applies to the
   pliron-IR side).
   **Def-use arena pivot landed** (`d8dfc70`, `79b22cd`, `b73892b`):
   `DefNode`'s `SmallSet<Use,1>` (indexmap heap-spill past a single
   use — nearly every multi-use def paid malloc+hashing per operand
   touch) is now a Context-wide intrusive use-slot arena
   (`Context::use_links`: `Vec<UseSlot{user_op,use_uid,prev,next}>` +
   free list; `use_uid = gen<<32 | slot` so stale `Use`s can't alias a
   reused slot; removal splices the tail into the vacated chain
   position, preserving indexmap `swap_remove` iteration order — hence
   byte-identical output). `Operation::new`/`rewrite_*`/`drop_*` push
   result/operand/successor spans straight into the columnar pools
   under one `use_links`+pool borrow each — the per-op temp `Vec`s and
   per-operand `RefCell` borrows are gone. `Value::uses_iter`/
   `Ptr<BasicBlock>::uses_iter` give alloc-free lazy walks at ~26
   read-only sites; `.is_empty()` queries became `is_used()`.
   Measured: regex-syntax objects byte-identical on all 16 CGUs, gate
   36/36, ~2% CPU (~19.9 vs ~20.3 user-s), wall flat. One follow-up
   measured negative and was reverted: storing the operand index in
   the use slot for O(1) `try_find_index` (−0.5% — the extra slot
   field costs more than the small operand scans it saves). Follow-ups:
   dense per-block scratch in `Dom::new`/`loop_blocks` (`669f623`) and
   lazy `blocks_iter`/`ops_iter` over the intrusive op/block lists in
   read-only scans (`c47cb77` — mutating or indexed callers keep
   snapshot Vecs) — byte-identical again, ~0.6% user. Conclusion:
   per-use hashing was never the bottleneck — the ~20 CPU-s is
   intrinsic `RefCell`-arena access volume spread across every pass;
   LLVM does this compile in ~4.5 CPU-s, so the remaining gap is
   per-access weight, not any single structure.

wasm compile time (2.56x) adds the CLIF → waffle translation, the waffle
inliner and `wpeep` on top of the same pipeline; profile
`--target wasm32-unknown-unknown --emit obj` the same way.

## 2. wasm runtime

| kernel | pliron/LLVM | lead |
|---|---|---|
| fib | **2.01** | Recursion turned into a loop by both, but our loop body is an `if/else` + `br` with 5 locals vs LLVM's single `br_if` with 2 (`wasm2wat` the `fib3fib` function in both modules). Shared cause below: the loop's `brif` lowers to `if c { get*;set*; br L } else { ret }` — the param copies sit in scratch locals inside the `if` arm where `wpeep`'s `pathcheck` (straight-line rule) can't retarget them, while LLVM updates `n`/`acc` unconditionally (`local.tee`) then `br_if`s. **DONE** (`PLIRON_WASM_BRF=0` off): `wpeep` now folds `if c { get*/set*/tee*; br L } [else { R }] end` → hoisted copies + `br_if` (depth−1), guarded by arm-purity, an operand-stack balance check (a `set`/`tee` popping the cond or a nonzero net depth rejects), no `Get`/`Nop` read of a copied local in the else arm or later, no depth-referencing/structured op in the else arm, `prel` empty. Unit tests `backedge_if_*`. NOTE: produces LLVM's shape + ~8B/fn smaller, but **V8-runtime-neutral on this box** (both control forms compile to the same work) — the residual gap is op *count*, below. |
| chars_mixed | ~1.4 | **partially DONE**: `NonNull`-i32 (`PLIRON_NN_ANYINT`) folds `eqz`-through-`phi`/`select`/`nonnull` → the `ptr==end`+`byte_ptr==0` `Option`-niche double-check is gone; `dem64` (`PLIRON_DEM64`) demotes `Option<u32>`/`usize`/`char` low-32 i64 webs → the `i64.extend; i64.and 128; i64.ne`→`i32` per-char overhead is gone (`chars_ascii` `i64` refs 23→0, +35→+9%). Residual = `next_code_point`/UTF-8 multi-byte decode (`load offset=1..3`, `&63`, `shl`, `or`, `ge_u` branches — LLVM's `byte<0?decode:cp<128` path is tighter) + locals churn. |
| parse_only | ~1.09 | **SwitchOp on wasm** (`PLIRON_SWITCH_WASM`, `builder.rs`): the "wasm.rs has no SwitchOp lowering" exclusion was stale — `dense_switch`→`br_table`/`ifconv` select chains work; the `from_str_radix` digit `match` became selects, +31%→+9%. |
| tokenize | ~0.67 | ahead |
| hmget | ~1.17 | hashbrown probe + SipHash; the probe loop's `local.*` traffic is ~1.4× LLVM's — emit-level density, below |
| hashmap | ~1.03 | at parity |
| strwork | ~1.26 | `dragon::format_exact` is +53% of its samples vs LLVM — emit-level density: 182 `if`/`else` pairs vs LLVM's flat `block`+`br_if` (0 `if`), 271 labels vs 124, `local.*` 1.9×. |
| fib | ~0.96 | ahead post `br_if` fold + `dem64`/`NonNull` |
| nbody | ~1.04 | f64 math |
| sieve, sortw | ~1.0 | at/near parity post `dem64`/`NonNull` (sortw 0.9%, sieve 2.5%) |
| chars_alpha, iterw, matmul, eqbug, contains_rng | <1.0 | ahead |
| tokenize 0.66, iterw 0.70, contains_rng 0.78, hashmap 0.88, sortw 0.90, fib 0.95, chars_ascii (was 0.57 → now ~0.92) | | ahead |

`Option`-niche residual: the `NonNull`-i32 widening doesn't reach iter pointers that **round-trip through stack slots across calls** (`tokenize`'s `Split` spill/reload) — the reload isn't `!nonnull`-marked and the spilled value's arithmetic isn't in `derived`, so `ptr` can't be proven there. That needs nonnull facts carried through our own slot demotion — a separate, larger change.
| matmul 0.74, iterw 0.79, tokenize 0.79, chars_alpha 0.82, eqbug 0.88 | | ahead |

`black_box` on wasm is a stack store + reload under both backends (LLVM
does the same), so it is not a source of the gap — don't "optimize" it
to an identity; that would only make the benchmarks lie.
Module size (1.95x) is **op density**, not GC: the whole module runs
~2.3× LLVM's op count (`local.get/set/tee` 3.4×/2.4×/2.9×, `br` 6.8×,
`if`/`else` 2746 pairs vs LLVM's 0 — we emit structured `if` diamonds
where LLVM emits flat `block`+`br_if`), and `compute/mem` itself is
1.9×. Reachable-but-cold bodies are all data/vtable-rooted. wasm-opt
-O3 recovers −28% (189.7→136.4 KiB); `coalesce-locals` alone −13 KiB.
Remaining systematic anomalies vs LLVM: `global.get` 7.6× (GOT entries
pliron-wasm-ld synthesizes — LLVM writes resolved `i32.const`
immediates into the code section; folding ours post-link would *grow*
the module), `return` 6.4× (early returns vs joined epilogue),
`br_table` 0 vs 17 (SwitchOp now reaches wasm via `dense_switch`; the
CLIF pipeline if-converts most of them to select chains before emit),
`i32.and`/`i32.shl` ~3.2× (incl. ~44 dead `&31`/`&63` shift masks —
wasm shifts already mask). wpeep `ddrop` (`PLIRON_WASM_DDROP`, default
on) removed ~480 dead `drop`s; `brf` folded ~450 `if{…;br}` sites
(−2.5 KiB); `delse` (`PLIRON_WASM_DELSE`, default on) flushes dead
else arms `if{A}else{brK}`→`if{A}` (~56 sites, −315 B); `bri`
(`PLIRON_WASM_BRI`, default on) flattens `if{brK}else{A}`→`br_if;A`
(~116 sites — the arm1 `br`s it catches are `WPeek::Br` markers
synthesized by `brf`'s own retargets, which brf can't refold); `btr`
(`PLIRON_WASM_BTR`, default on) deletes `br K` tail-branches that fall
through their K+1 enclosing `end`s anyway, incl. unreachable `br`s
(~570 sites, −640 B). btr's reachability model keeps three states per
inst — dead-region-open / live / live-unknown-depth — after a
miscompile where a boundary `end` resuming with an unknown base marked
a live `loop` back-edge "dead" and folded it (wasmstdtest words trap).
Remaining `if`/`else` pairs ~2,515 vs LLVM's 0. Next levers: deeper
`if`→`br_if` restructuring in stackify, local coalescing parity with
binaryen.

## 3. Native runtime

| kernel | pliron/LLVM | lead |
|---|---|---|
| eqbug | 1.10 | narrow `black_box` barriers: LLVM's `*m` asm form folds the `uxtb`/`uxth` into the store/load; we pay an ALU op per barrier, plus constant remat under pressure |
| parse_only | 1.05 | `divnarrow` regression fixed (wasm-only narrowing now); residual vs LLVM = packed-Result `band`/`orr` juggling + `bump_hoist` remat `sub`s in the unrolled digit cascade — both measured net-positive, don't remove |
| nbody | 1.04 | flip-flops between runs; earlier: an invariant's spill pair per iteration (regalloc pressure) |
| sieve | 1.02 | marking loop is instruction-tight; residual is elsewhere |
| chars_ascii 0.57, tokenize 0.66, iterw 0.70, contains_rng 0.78, hashmap 0.88, sortw 0.90, fib 0.95 | | ahead |

**`PLIRON_AMODESHARE` — default on** (aarch64 amode rules keep a
multi-use `iadd` base whole instead of reg-reg decomposition;
`vendor/cranelift-codegen` patches #55–56; `PLIRON_AMODESHARE=0`
disables). Strictly fewer insts where it fires, but unbounded it was
a wash: sieve −6%, sortw +5%, hashmap +2.6%. Use-shape
discriminators (non-amode use, fanout ≤3) each kept or lost
everything together — wins and losses are in the same class. What
finally discriminated was a **block live-in pressure proxy**
(cross-block arg uses per block, rolled up per value): gate
`amode_shareable` = non-amode use OR (fanout ≤3 AND
`PLIRON_AMODESHARE_PRESS` ≤5). Flipped on at PRESS=5 (`5ad49ed`):
paired medians give sieve −5.5% / parse_only −3%, and the sortw /
hashmap hot functions stay byte-identical to the off state.

`PLIRON_REASSOC` (opt-in CLIF pass) reassociates `((r+a)+b)+c`
accumulator chains into `r+((a+b)+c)` when `r` is a block param — one
add on the loop-carried edge instead of the whole chain (LLVM's isel
does this free). On eqbug it produced the intended depth-1 asm but
only ~0.8% — the serial chain was already hidden in the loop's ILP
shadow; eqbug's ~10% residual vs LLVM is elsewhere (likely
per-comparison `uxtb` before `cmp…,uxtb` folds and call-boundary
extends). Wash on matmul/nbody/fib. Kept opt-in.

x86-64 numbers are older (Windows box, geomean 1.028); re-measure there
before working on x64-specific gaps.

## Where the old checklists live

- LLVM's -O2 pass list vs ours (Oct 10; partly stale):
  `docs/history/llvm-passes.md`.
- Per-session optimization logs: `docs/history/{native,x64}-notes.md`,
  `docs/history/coordination-ledger.md`.
