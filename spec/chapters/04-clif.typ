#import "../pliron-spec.typ": *

#chapter("clif", 1)
= Lowering and the CLIF pipeline

This chapter covers the middle of the pipeline (#refr("model.pipeline")):
#term("lowering") turns each function from the #term("llvm-dialect") into
#term("clif"), the #term("clif-pipeline") optimizes that CLIF one
#term("step") at a time, and the vendored #term("cranelift") compiles it.
Lowering lives in `pliron-clif-lower/`, the passes and the differential
checker in `pliron-clif-opt/`, the replay tool in `pliron-clif-check/`, and
our changes to Cranelift and regalloc2 under `vendor/`. The pipeline is data:
an ordered table of steps, each with a gate, so it can be inspected,
reordered and bisected at run time. The same pipeline runs for wasm targets;
only the consumer of the optimized CLIF differs.

#chapter-glance()

== Lowering

#r("clif.lower.leaves", title: "Aggregates lower to leaf lists", level: "must",
   refines: "model.pipeline")[
  Lowering MUST translate each function on its own, from its pliron body
  and the IR passes' side tables, into one CLIF function. An aggregate value
  MUST be lowered as a #term("leaf-list"), so that `extractvalue` and
  `insertvalue` emit no CLIF instructions.
]

#r("clif.lower.phases", title: "Parallel compile, ordered output", level: "must",
   refines: ("model.det", "model.pipeline"), checked: "harness/detcheck.py")[
  A #term("cgu") MUST be compiled in three phases. Phase A lowers every
  function to a #term("fn-job"), serially, because the pliron context is not
  shareable. Phase B runs the CLIF pipeline and the Cranelift compile of each
  job, on any thread, in any order. Phase C defines the compiled functions in
  the object in declaration order. The object MUST be byte-identical for
  every thread count and every order in which phase B finishes jobs.
]

#r("clif.lower.serial", title: "Thread count and serial mode", level: "must",
   tier: 2, refines: ("clif.lower.phases", "model.det"))[
  `PLIRON_PAR=0` or `PLIRON_PAR=1` MUST run phase B serially on the CGU's
  own thread, and `PLIRON_PAR=N` MUST size the shared worker pool to `N`. When
  `PLIRON_OPT_BISECT` is set, phase B MUST run serially regardless of
  `PLIRON_PAR`, so that bisect units are numbered in a fixed order.
]

#r("clif.lower.tailcc", title: "Internal functions use the tail convention", level: "must",
   refines: "model.abi", checked: "test.sh rusttailcc")[
  A function is #dfn[internal] when it has local linkage, a body, no
  C-variadic parameters, its address never escapes, and no `invoke` calls
  it. Unless #refr("clif.lower.tailcc.except") says otherwise, a function
  MUST use the #term("tail-cc") when it is internal and `PLIRON_TAILCC` is
  not `0`, or when its Rust ABI is `extern "tail"`. Every other function MUST
  use the platform calling convention, so every symbol reachable from
  outside the CGU keeps the convention stock rustc gives it.
]

#llvm(kind: "choice", summary: [Functions only called directly inside a CGU use Cranelift's `tail` convention (8+8 return registers, tail calls), not the C convention.])[
  Stock rustc gives Rust-ABI functions the platform C convention, and LLVM
  may switch an internal function to `fastcc` on some targets. Here every
  internal function uses Cranelift's `tail` convention: aggregates of up to
  eight integer and eight float leaves come back in registers instead of
  through a hidden return pointer, and `return_call` is legal. The choice is
  invisible outside the CGU by construction (#refr("model.abi")).
]

#r("clif.lower.tailcc.except", title: "Where the tail convention is refused", level: "must",
   refines: "model.abi", depends: "clif.lower.tailcc", checked: ("test.sh rusttailcc", "test.sh invokeblk"))[
  Internal status MUST exclude every callee of an `invoke`, because a
  `try_call` into a tail-convention callee clobbers every register; an
  `extern "tail"` callee keeps the tail convention anyway, since its source
  ABI fixes it. On aarch64, a function with a struct-return (`sret`)
  parameter MUST keep the platform convention even when it is internal or
  `extern "tail"`, because Cranelift's aarch64 tail convention has no
  struct-return support.
]

#r("clif.lower.ext", title: "Narrow-value extension", level: "may",
   tier: 2, refines: "model.abi", depends: "clif.lower.tailcc")[
  On a #term("tail-cc") signature, lowering MAY omit the zero- or
  sign-extension of narrow integer parameters and returns
  (`PLIRON_TAILCC_NOEXT`, default on), because both ends are this backend's
  code and Cranelift never relies on the upper bits. On x86-64, lowering MAY
  omit the extension of narrow return values on every signature
  (`PLIRON_RET_NOEXT`, default on), because the SysV ABI leaves those bits
  unspecified.
]

#llvm(kind: "choice", summary: [Narrow integer arguments are not extended on internal calls, and narrow returns are not extended on x86-64.])[
  rustc marks narrow integers `zeroext`/`signext` and LLVM extends them at
  every call boundary the ABI allows. Here the extension is dropped where no
  caller can observe the upper bits: internal tail-convention calls, and
  x86-64 returns (where the SysV ABI leaves them undefined).
]

#r("clif.lower.shift128k", title: "Constant i128 shifts split into halves", level: "should",
   refines: ("model.equiv", "model.parity"),
   checked: "test.sh t_shift128k")[
  An `shl`/`lshr`/`ashr` of a scalar `i128` whose amount is a constant in
  `0..128` (after folding casts of constants) SHOULD lower to 64-bit
  operations on the two halves (`isplit`/`iconcat`): a shift by 64 is a
  register move. Out-of-range amounts keep the generic shift.
  `PLIRON_SHIFT128K=0` disables.
]

#llvm(kind: "choice", summary: [i128 constant shifts are split in lowering because Cranelift only folds a plain `iconst` amount.])[
  rustc gives an i128 shift an i128 amount, which reaches Cranelift as
  `uextend.i128 (iconst k)`; Cranelift has no i128 `iconst` and its aarch64
  rules only recognize a plain one, so `(x as u128 * y as u128) >> 64` became
  a nine-instruction variable shift (num-bigint-dig's Montgomery loop,
  docs/history/2026-10-31-tune-heldout.md). LLVM's legalizer splits a
  constant i128 shift into its halves.
]

#r("clif.lower.byval", title: "By-value aggregates", level: "must",
   refines: "model.abi", checked: "test.sh rusttailcc")[
  A by-value (`byval`) argument MUST be lowered as a Cranelift
  `StructArgument` only on x86-64 targets other than Windows. On every other
  target it MUST be passed as a plain pointer to the caller's copy, like any
  other indirect argument.
]

#r("clif.lower.libcall", title: "Libcalls follow the C ABI of the target", level: "must",
   refines: "model.abi")[
  A runtime library call that lowering introduces (128-bit arithmetic, float
  conversions, `fmod` and the like) MUST use the signature
  the target's C ABI gives that function. On COFF targets, 128-bit integer
  and float arguments MUST be passed through pointers to stack copies and a
  128-bit result read back as a vector register value. On wasm32, a 128-bit
  result MUST be returned through a hidden struct-return pointer.
]

#r("clif.lower.blackbox", title: "black_box is a zero-cost barrier", level: "must",
   refines: "model.equiv", depends: "clif.vendor.blackbox")[
  On native targets, a call to a `__pliron_bb*` symbol MUST be lowered as one
  #term("bb-op") per argument leaf, and the symbol MUST NOT be declared in
  the object. The barrier MUST NOT be weakened to a plain copy by any CLIF
  pass.
]

#llvm(kind: "choice", summary: [`core::hint::black_box` is a dedicated CLIF op that emits no instructions, not an empty inline-asm statement.])[
  LLVM implements `black_box` as `asm("" : : "r"(v) : "memory")`. Here it is
  a call to a never-defined `__pliron_bb*` symbol through the IR passes
  (opaque to all of them), then a #term("bb-op") in CLIF. The effect on
  optimization is the same; the emitted code is empty in both. wasm keeps
  an alloca and volatile-load round trip, as LLVM's wasm output does.
]

#r("clif.lower.invoke", title: "invoke lowers to try_call", level: "must",
   refines: "model.unwind", checked: ("test.sh unwind", "test.sh invokeblk"))[
  With native unwinding, an `invoke` MUST be lowered to a `try_call` (or
  `try_call_indirect`) whose exception table has one tagged entry: catch or
  cleanup. The exceptional edge MUST go through a cold block that receives
  the exception pointer as a block parameter and jumps to the landing pad.
  With emulated unwinding (wasm), an `invoke` MUST be lowered to a plain
  call followed by a check of the emulation flag that branches to the
  landing pad.
]

#r("clif.lower.switch", title: "Dense switches become one table", level: "should",
   refines: "model.parity")[
  A switch with at least four cases over an integer of at most 64 bits whose
  cases cover at least a tenth of a span of at most 4096 values SHOULD be lowered to one
  `br_table` over the span, holes going to the default
  (`PLIRON_SWITCH_DENSE`). When the default is unreachable, the table SHOULD
  be padded to a power of two and the index masked with `band`, so that no
  bounds check is emitted (`PLIRON_SWITCH_MASK`).
]

#llvm(kind: "choice", summary: [Dense switches use LLVM's 10% density rule, but an unreachable default gives a masked power-of-two table with no bounds check.])[
  Cranelift's own `Switch` splits at every hole and binary-searches between
  runs; lowering bypasses it with LLVM's jump-table density threshold. For
  an unreachable default (enum discriminants) LLVM keeps a range check or
  relies on `unreachable` metadata; here the index is masked and the table
  padded, and the vendored x64 lowering skips its bounds clamp for that form
  (#refr("clif.vendor.jt")).
]

#r("clif.lower.facts", title: "Facts handed to the pipeline", level: "must",
   tier: 2, refines: "model.equiv")[
  Each #term("fn-job") MUST carry these #term("clif-fact", [lowering
  facts]): `nonnull`, `derived` (pointer to base), `frozen`, `noalias`,
  `scoped_na`, `deref` (bytes, writable), `nowrite`, `call_frozen`,
  `nounwind`, `scoped`, `invariant`, `operand_frozen`, `lifetimes`, `nocap`,
  `block_region`, and the PGO block counts. Every fact MUST hold on every
  execution of the function; a fact that holds only inside a region MUST
  name that region (`scoped_na`, `scoped`, `operand_frozen`).
]

#r("clif.lower.vecretry", title: "Narrow vectors are scalarized on rejection", level: "may",
   refines: "model.bail")[
  On x86-64, when Cranelift rejects a function because it names a vector
  type narrower than 128 bits, lowering MAY scalarize every instruction that
  touches that type and compile again, a bounded number of times. Past the
  bound the original error MUST be reported.
]

== The pipeline as data

#r("clif.step", title: "A step is a table entry", level: "must",
   tier: 2, refines: "model.off-switch")[
  The #term("clif-pipeline") MUST be the ordered table `DEFAULT` in
  `pliron-clif-opt/src/pipeline.rs`. Each entry is a `Step` with: a unique
  `name`; an optional `bisect` unit name; a `PLIRON_STATS` label; `verify`
  (run the verifier after it under `PLIRON_VERIFY=full`); `mutates`
  (advance the analysis version after it); `cached` (worth a step-cache
  entry); `shape` and `any` (required #term("shape-bits")); `creates`
  (shape bits it may introduce); a #term("step-gate"); and the function
  that runs it.
]

#r("clif.step.gate", title: "Gate kinds", level: "must",
   tier: 2, refines: "model.off-switch", depends: "clif.step")[
  A #term("step-gate") MUST be one of: `Always` (selected
  unconditionally); `Env("PLIRON_X")` (selected unless `PLIRON_X=0`);
  `OptIn("PLIRON_X")` (selected when `PLIRON_X` reads `1`, where an unset
  variable listed in `pliron_cfg::DEFAULT_ON` reads `1`); `Flag(id)` (a
  per-CGU category flag, set from the `PLIRON_*` environment at CGU start
  and forced off at `-O0`); or `All(gates)` (every gate holds).
]

#r("clif.step.always", title: "Legality steps always run", level: "must",
   refines: ("model.equiv", "model.bail"), depends: "clif.step.gate")[
  A #term("legality-step") (gate `Always`) MUST run on every function the
  pipeline runs on: under a custom `PLIRON_PIPELINE`, on a
  #term("giant-fn"), and on a callerless copy marked for minimal
  optimization. It MUST NOT draw a bisect unit. The legality steps are
  `frozen_loads`, `wide_minmax` (split `umin`/`umax`/`smin`/`smax` wider than a
  register) and `iconst128` (rewrite `iconst.i128`, which no ISA lowers).
]

#r("clif.step.order", title: "The default order", level: "must",
   tier: 2, refines: "model.pipeline", depends: "clif.step")[
  The steps of `DEFAULT` MUST run in table order. Grouped here by purpose
  (the code has no phase markers; † marks legality steps):
  + *Early control flow*: `jumpthread`, `constbr`, `unroll`, `bunroll`,
    `unreach`, `clifpeep`, `tailmerge`, `taildup`, `tailrec`.
  + *Memory*: `loadfwd`, `slotprom`, `dse`, `egraph`.
  + *Loops*: `indvars`, `ldrot`, `licm`, `revnorm`, `loopidiom`, `lowbit`,
    `ifconv2`, `unswitch`, `bcheck`, `divconst`, `loopvec`, `slp`, `licm2`,
    `switchmap`, `constbr2`, `loopdel`, `looprot`, `zipiv`, `ivalign`,
    `punroll`, `licm3`, `constdedup`.
  + *Legality and local folds*: `frozen_loads`†, `sublea`, `commute`,
    `wide_minmax`†, `coldarg`, `fuse`, `edgefwd`, `foldf`, `pushin`,
    `celim`, `ivrefold`, `deadrec`, `lcse`, `dgvn`, `sameargs`, `loadsel`,
    `loadfwd2`, `mask01`.
  + *Edge specialization and dispatch*: `edgespec`, `bittab`, `coldedge`,
    `dse2`, `vecslot`, `lcse2`, `uload32`, `postinc`, `boundcmp`, `selfarg`,
    `brchain`, `splink`, `taildup2`, `cmp3`, `mergechain`, `selchain`,
    `jtround`, `slotmerge`, `memcall`, `csweep2`, `lcse3`.
  + *Late parameter webs and cleanup*: `sameargs2`, `paramsink`,
    `postinc2`, `ivalign2`, `ivretro`, `ivcmp`, `nfold`, `selargs`, `seltest`,
    `rgmerge`, `sameargs3`, `deadp`, `webmerge`, `rangefold`, `ovkill2`,
    `sgntest`, `memspec`, `lcse4`, `dse3`, `taildup3`, `webmerge2`, `cicm`,
    `armmerge`, `constdedup2`, `edgesink`, `reassoc`, `mulcarry`, `dem64`,
    `copypair`, `coldret`.
  + *Final*: `iconst128`†, `stepcmp`.
  `clif-check steps` prints the list the binary was built with.
]

#drift[
  `AGENTS.md` describes `pliron-clif-opt/` as "~80 steps". `DEFAULT` has
  grown well past that; the code is the reference.
]

#llvm(kind: "gap", summary: [Vectorization is pattern-driven (`loopvec` for elementwise loops, `slp` for store groups) with no general cost-model loop vectorizer; some loops LLVM vectorizes stay scalar.])[
  LLVM's LoopVectorize handles arbitrary reductions, interleaving and
  runtime checks under one cost model, and SLPVectorizer works on any
  isomorphic tree. Here `loopvec` recognizes elementwise loops and widening
  reductions it was taught, and `slp` pairs adjacent stores. The parity
  notes record loops that `loopvec` analyzes but declines (a compare-count
  loop LLVM turns into SSE2 `pcmpgtd`), and narrow (`<128`-bit) vector
  throughput below LLVM's (`docs/history/2026-10-16-narrow-vec-gaps.md`).
]

#r("clif.step.select", title: "Selection is a pure function", level: "must",
   refines: "model.det", depends: "clif.step.gate")[
  The list of selected steps MUST be a pure function of the CGU's category
  flags and the `PLIRON_*` configuration snapshot, so it is the same for
  every function of the CGU and the same before, during and after the
  pipeline. No gate may read the function, the clock or the thread.
]

#r("clif.step.pipeline", title: "PLIRON_PIPELINE overrides the table", level: "must",
   tier: 2, refines: "model.off-switch", depends: "clif.step.always")[
  When `PLIRON_PIPELINE=a,b,c` is set, the pipeline MUST run exactly the
  named non-legality steps, in the listed order, regardless of their gates,
  followed by every legality step in table order. An unknown name MUST be
  reported on standard error and skipped. Bisect units still apply.
]

#drift[
  The module comment of `pipeline.rs` says `PLIRON_PIPELINE` bypasses the
  *env* gates. The code bypasses every gate, including the per-CGU `Flag`
  gates that carry the `-O0` default.
]

#llvm(kind: "extra", summary: [The CLIF step list is data: any subset and order can be run by name with `PLIRON_PIPELINE`, no rebuild.])[
  LLVM's new pass manager accepts `-passes=` for `opt`, but rustc's LLVM
  backend offers no stable way to replace its pipeline per build. Here any
  rustc invocation can run a custom step list, and `clif-check` runs the
  same list on saved functions.
]

#r("clif.step.shape", title: "Shape gating never changes results", level: "must",
   refines: ("model.linear", "model.equiv"), depends: "clif.step")[
  Before a step runs, the pipeline MUST skip it if the function's cached
  #term("shape-bits") lack a bit the step requires. Each required bit MUST
  be a necessary condition for the step to change the function, and each
  mutating step MUST add the bits it may create to the cache, so a skip
  never changes the output. `PLIRON_SHAPE_CHECK` MUST run each skipped step
  anyway, report any change it makes, and restore the function.
]

#r("clif.step.giant", title: "Giant functions get only legality steps", level: "must",
   refines: ("model.bail", "model.linear"))[
  Before each non-legality step, the pipeline MUST skip the step when the
  function has more instructions than `PLIRON_GIANT_MAX` (`0` disables the
  limit) unless the step is named in `PLIRON_GIANT_STEPS`. The check MUST be
  made per step, since a step can grow the function past the limit.
]

#llvm(kind: "choice", summary: [Functions past an instruction-count limit get only legality steps.])[
  LLVM optimizes every function and caps individual transforms with
  thresholds. Generated matchers (Cranelift's own ISLE output reaches
  hundreds of thousands of instructions) cost minutes in superlinear passes
  for little gain, so here the whole optimization pipeline bails at a size
  limit. The function is then compiled correctly but unoptimized.
]

== Shared analyses and complexity

#r("clif.analysis", title: "One version-keyed analysis cache", level: "must",
   refines: "model.linear")[
  The control-flow graph, dominator tree, loop analysis, use index and
  parameter-alias facts that steps share MUST come from one
  #term("analysis-cache") per function. After every step marked `mutates`,
  the pipeline MUST advance the cache's version; a pass that mutates the
  function in the middle of a step MUST call `invalidate`. The structural
  analyses MAY survive a version change when an edge fingerprint (blocks,
  terminators, successors) is unchanged; the value-keyed entries MUST NOT.
  In builds with debug assertions, a structural read after an
  un-invalidated CFG change MUST panic.
]

#r("clif.analysis.uses", title: "Who uses v is the use index", level: "must",
   refines: "model.linear", depends: "clif.analysis")[
  A pass that asks which instructions use a value MUST answer from a
  #term("uses-index") built once per IR version (or once per pass phase),
  never by scanning the function per query. A pass that mutates while it
  iterates MUST keep incremental counts or rebuild the index at the points
  where the function changed.
]

#r("clif.alias", title: "Alias chains stay flat", level: "must",
   refines: "model.linear")[
  After a mutating step that reports a change, the pipeline MUST flatten
  every #term("alias-chain") to one hop (`compact_aliases`) when an alias
  was written since the last flattening. Code that iterates all values or
  holds value pairs across mutation MUST use the fallible accessors
  (`try_resolve_aliases`, `try_value_def`), which return nothing for a chain
  that ends at a tombstone.
]

#r("clif.linear.walks", title: "No whole-function work per item", level: "must",
   refines: "model.linear")[
  A pass MUST NOT run a whole-function scan (`layout.blocks()`,
  `inst_values` over all instructions) or rebuild the CFG or dominator tree
  inside a loop over instructions, edges or candidates. A fixpoint MUST
  converge through a worklist, not by rescanning the function each round.
  N overlapping per-item walks MUST be merged into one walk (one backward
  cone, or one memoized walk with a shared budget). A pass MUST read its
  `PLIRON_*` configuration once, not per instruction.
]

#r("clif.budget", title: "Every fixpoint has a budget", level: "must",
   refines: "model.bail")[
  Every fixpoint, recursive walk and search in a CLIF pass MUST carry an
  explicit budget counted in work (named `MAX_*` constants, `seen` sets,
  round limits). When a budget runs out, the pass MUST leave the code it
  has not yet transformed as it was; it MUST NOT emit a partial rewrite
  that is wrong.
]

#r("clif.phiweb", title: "One table for block-parameter questions", level: "should",
   refines: ("model.linear", "model.equiv"))[
  Passes that ask which value reaches a block parameter on each edge SHOULD
  read one shared fact table, built by one CFG walk that resolves aliases
  and drops self-carry edges, and derive their answers through the Ascent
  Datalog programs in `proots.rs` (uniform source, affine offset,
  accumulator). A fix to the table's extraction then fixes every consumer.
]

== Checking and bisecting

#r("clif.verify", title: "PLIRON_VERIFY", level: "must",
   tier: 2, refines: "model.diag-observe", checked: "harness/run.py --suite ui")[
  `PLIRON_VERIFY=full` MUST run Cranelift's verifier after every step whose
  `verify` field is set, and panic naming the step on failure. Any other
  value MUST verify once after the pipeline; on failure the function MUST be
  restored from its pre-pipeline snapshot and the pipeline re-run with
  per-step verification, so the panic names the first failing step. A build that passes
  verification MUST produce the same output as one without `PLIRON_VERIFY`.
]

#drift[
  `harness/run.py` sets `PLIRON_VERIFY=1` with the comment "Verify CLIF after
  every pass". `=1` verifies once after the pipeline and per step only on a
  failure; per-step verification is `=full`.
]

#r("clif.bisect", title: "Bisect units in the CLIF pipeline", level: "must",
   tier: 2, refines: "model.off-switch", depends: "clif.lower.serial",
   checked: "harness/bisect.py")[
  Under `PLIRON_OPT_BISECT=N`, each selected non-legality step MUST draw one
  unit per function before it runs (before any shape skip), and run only if
  fewer than `N` units were drawn before it. Inside `licm`, each accepted
  load hoist MUST draw an extra `licm-load` unit. Legality steps draw none.
  `PLIRON_OPT_BISECT_DEBUG=1` MUST log each unit with its step and function.
]

#r("clif.optfns", title: "Per-function selection", level: "must",
   tier: 2, refines: "model.off-switch")[
  A function that `PLIRON_OPT_FNS` does not select MUST skip the CLIF
  pipeline entirely and go to Cranelift as lowered. Selection is by symbol
  name, so it is the same in every CGU, process and rebuild.
]

#r("clif.diffcheck.observe", title: "What diffcheck compares", level: "must",
   refines: "model.equiv", checked: "pliron-clif-opt/tests/diffcheck.rs")[
  #term("diffcheck") MUST interpret a function before and after a
  transformation on the same random inputs, in a sandbox, and compare what a
  caller could observe: the returned values (NaNs equal regardless of
  payload), the sequence of calls made with their callees and arguments, and
  every byte written to memory the function can reach. Inputs MUST respect
  the facts the optimizer was allowed to assume (`nonnull`, `noalias`,
  `deref`, `frozen`, `nowrite`), so a pass that relies on them is not
  flagged.
]

#llvm(kind: "extra", summary: [Any CLIF step can be differentially checked in a real build (`PLIRON_DIFFCHECK=step`), with a ready-made reproducer on a mismatch.])[
  LLVM has no in-tree equivalent; Alive2 proves single passes on LLVM IR
  outside the compiler. Here the interpreter-based check runs inside
  rustc on every function of a real build, names the step that changed
  observable behaviour, and writes `before.clif`/`after.clif` that
  `clif-check step` replays in seconds.
]

#r("clif.diffcheck.sound", title: "Mismatches come only from completed runs", level: "must",
   refines: "model.equiv", depends: "clif.diffcheck.observe",
   checked: "pliron-clif-opt/tests/diffcheck.rs")[
  A trial MUST report a mismatch only when the original function returned
  normally. A trap in the original is undefined behaviour and MUST make the
  trial #term("inconclusive"); so MUST exhausted fuel or an unsupported
  operation in the original, an unsupported operation or wild pointer in the
  transformed function, a changed signature, and a difference confined to
  bytes the original leaves undetermined (found by re-running the original
  under a different stack fill). A transformed function that traps or runs
  out of fuel where the original returned MUST be reported as a mismatch. An
  inconclusive trial MUST NOT count as a pass.
]

#r("clif.diffcheck.limits", title: "Diffcheck limits", level: "must",
   refines: "model.equiv", depends: "clif.diffcheck.observe")[
  The sandbox models pointers as 64-bit values and runs with the host ISA's
  interpreter. Functions lowered for wasm32 (32-bit pointers) MUST therefore
  come back #term("inconclusive") rather than compared. Inputs are random:
  a clean result MUST NOT be read as coverage of every path.
]

#r("clif.diffcheck.modes", title: "PLIRON_DIFFCHECK", level: "must",
   tier: 2, refines: "model.diag-observe", depends: "clif.diffcheck.observe")[
  `PLIRON_DIFFCHECK=1` MUST check each function's pipeline input against its
  output and, on a mismatch, replay the pipeline step by step to name the
  step; `=step` MUST check every step that changes the function.
  `PLIRON_DIFFCHECK_FN=<substr>` restricts the check to matching symbols;
  `PLIRON_DIFFCHECK_TRIALS`, `_FUEL` and `_SEED` set the trial count, the
  per-run step limit and the seed. A mismatch MUST write
  `before.clif`, `after.clif` (with a facts header) and `report.txt` under
  `PLIRON_DIFFCHECK_DIR`, and `PLIRON_DIFFCHECK_PANIC=1` MUST turn the report
  into a panic. Checking MUST NOT change the compiled output.
]

#r("clif.check", title: "clif-check replays without rustc", level: "must",
   tier: 2, refines: "model.equiv", checked: "harness/gate.py corpus")[
  #term("clif-check") MUST provide `step <name>` (run one step and check
  it), `pipeline` (run the default pipeline and check each step), `pair`
  (check two saved functions), `corpus`, `bench` (time the pipeline over a
  #term("corpus")), `report` (the pipeline over a corpus in parallel, with
  `--check` adding a differential check per function) and `steps`. It MUST
  run for the host ISA with every per-CGU category flag on, and read facts
  from the `.clif` header.
]

#llvm(kind: "extra", summary: [The whole CLIF pipeline replays offline on recorded corpora of real functions, with an interpreter diff per function.])[
  Reproducing an LLVM pass bug needs the IR dumped from the right point and
  `opt`. Here `PLIRON_CORPUS` records every function's pre-pipeline CLIF
  with its facts, and the gate replays whole corpora through the current
  pipeline with differential checking on every change.
]

#r("clif.check.fidelity", title: "Replay fidelity", level: "must",
   tier: 2, refines: "model.equiv", depends: "clif.check")[
  The saved facts header MUST carry `nonnull`, `noalias`, `scoped_na`,
  `deref`, `frozen`, `nowrite`, `derived`, the callee's own name, and (in
  corpus dumps) `nocap`, `lifetimes` and callee names. It does not carry
  `operand_frozen`, `call_frozen`, `invariant`, `scoped` or `block_region`,
  so a step whose decision reads those MAY replay differently from the real
  build. A clean replay MUST NOT be taken to exonerate a step.
]

== Vendored Cranelift and regalloc2

#r("clif.vendor", title: "Every vendored change is marked and listed", level: "must",
   refines: "model.pipeline")[
  The backend MUST build against vendored copies of `cranelift-codegen`,
  `cranelift-codegen-meta`, the x64 assembler crates, `cranelift-interpreter`,
  `cranelift-object`, `cranelift-reader`, `cranelift-jit` and `regalloc2`.
  Each change to them MUST be marked `PLIRON:` in the source and described,
  with its reason, in that crate's `PLIRON_PATCH.md`.
]

#drift[
  `vendor/cranelift-object` has no `PLIRON_PATCH.md`; its two changes (the
  `R_X86_64_GOTTPOFF` relocation and the preferred-alignment fix) are
  described in `vendor/cranelift-codegen/PLIRON_PATCH.md` (items 3 and 16),
  and the relocation arm is not marked `PLIRON:`. Item 3 also says both
  crates are vendored "only for this". `vendor/regalloc2/PLIRON_PATCH.md`
  says it is vendored for one change and then lists a second
  (`PLIRON_RA2_XBLOCK`).
]

#r("clif.vendor.frame", title: "Frameless x64 functions", level: "must",
   tier: 2, refines: ("model.parity", "model.unwind"))[
  On x64, when `preserve_frame_pointers` is off, a function MUST get no
  `push rbp; mov rbp, rsp` frame unless its outgoing tail-call argument area
  differs from its incoming one, and `rbp` MUST then be allocatable as a
  callee-saved register. The backend MUST turn `preserve_frame_pointers` on
  when the crate uses tail calls, under `-Cforce-frame-pointers`, or with
  `PLIRON_OMIT_FP=0`; otherwise it MUST leave it off, even where the target
  spec defaults to keeping frame pointers, because unwinding uses
  `.eh_frame`/`.xdata` metadata.
]

#drift[
  `vendor/cranelift-codegen/PLIRON_PATCH.md` item 1 says only *leaf*
  functions (no calls, clobbers, stack slots or stack arguments) lose the
  frame. The vendored `isa/x64/abi.rs` omits it for every function under
  `!preserve_frame_pointers` and frees `rbp` for the register allocator
  (`PLIRON_NO_FREE_RBP` turns that off); the code is the reference.
]

#llvm(kind: "choice", summary: [x64 frame pointers are omitted by default even where the target spec keeps them (windows-msvc).])[
  Stock rustc follows the target spec's frame-pointer default. Here frame
  pointers are kept only when `-Cforce-frame-pointers` asks, when tail calls
  need them, or with `PLIRON_OMIT_FP=0`; unwinding is metadata-driven on
  every native target, so the frame chain is not needed for panics.
]

#r("clif.vendor.jt", title: "Jump-table forms", level: "must",
   tier: 2, refines: "model.parity", depends: "clif.lower.switch",
   checked: ("pliron-clif-opt/tests/jt_narrow.rs", "pliron-clif-opt/tests/jt_narrow_w4.rs", "test.sh narrowext"))[
  The vendored Cranelift MUST: skip the x64 `br_table` bounds clamp when the
  index is `band x, k` with `k` below the table size; share one split-edge
  block among `br_table` entries with the same target and arguments
  (`PLIRON_EDGEDEDUP`); and, where a sound size bound proves both
  placeholders stay in pc-relative range, write each jump table after the
  function body with the narrowest unsigned entry width (1, 2 or 4 bytes)
  that fits (`PLIRON_JT_NARROW`), falling back to the stock inline table
  otherwise.
]

#llvm(kind: "choice", summary: [Jump tables are deferred to the end of the function with 1/2/4-byte entries, on both aarch64 and x64.])[
  LLVM compresses aarch64 jump tables to narrow entries and keeps x64
  tables as 4-byte entries in a read-only section. Here both ISAs place the
  table after the function with the narrowest width that fits, at the cost
  of one more instruction in the aarch64 dispatch sequence.
]

#r("clif.vendor.tls", title: "Initial-exec TLS on x86-64 ELF", level: "must",
   tier: 2, refines: "model.abi")[
  On x86-64 ELF, when rustc asks for the initial-exec or local-exec TLS
  model and `PLIRON_TLS_IE` is not `0`, a thread-local access MUST be
  lowered as `mov %fs:0, r; add sym@gottpoff(%rip), r` with an
  `R_X86_64_GOTTPOFF` relocation (`tls_model = "elf_ie"`), not as a
  `__tls_get_addr` call. Otherwise ELF uses the general-dynamic model.
]

#llvm(kind: "gap", summary: [`-Ztls-model=local-exec` is lowered as initial-exec on x86-64 ELF (one GOT load per access).])[
  LLVM emits `%fs:sym@tpoff` for local-exec, with no GOT access. The
  vendored Cranelift has only the initial-exec form, so a local-exec request
  gets it: correct, one load slower.
]

#r("clif.vendor.blackbox", title: "The black_box instruction", level: "must",
   tier: 2, refines: "model.equiv")[
  The vendored Cranelift MUST define the #term("bb-op") as a unary
  instruction over any scalar or vector type, marked as storing memory and
  having other side effects, and every ISA MUST lower it to its operand with
  no machine instruction. The vendored interpreter MUST evaluate it as the
  identity.
]

#r("clif.vendor.noret", title: "No trap after a noreturn call", level: "must",
   tier: 2, refines: "model.parity",
   checked: ("test.sh noret", "pliron-clif-opt/tests/noret_trap.rs"))[
  After a call to a noreturn callee, lowering MUST emit a trap with
  `TrapCode::NORETURN_CALL` (`PLIRON_NORETTRAP`, default on). The vendored
  emitter MUST drop that trap when it sits exactly at the call's return
  address and another block follows in emission order; the last block of a
  function MUST keep its trap, so a return address never points past the
  function.
]

#r("clif.vendor.align", title: "Function and loop alignment", level: "must",
   tier: 2, refines: "model.parity", checked: "test.sh loopalign")[
  `cranelift-object` MUST align each function symbol to at least the ISA's
  preferred function alignment. On x64, each block that is the target of a
  back edge in the final emission order MUST be padded to a 16-byte
  boundary (`PLIRON_ALIGN=0` disables).
]

#r("clif.vendor.ra2", title: "regalloc2 spill weights", level: "must",
   tier: 2, refines: "model.parity")[
  The vendored regalloc2 MUST divide a live range's spill weight by
  `4 * isqrt(len)` instead of `len`, so long-lived values with many uses
  keep a register; `RA2_LINEAR=1` restores upstream behaviour.
  `PLIRON_RA2_XBLOCK=1` (default off) MAY keep the redundant-move
  eliminator's state across a block boundary whose successor has the
  previous block as its only predecessor.
]

#llvm(kind: "choice", summary: [Register allocation is Cranelift's regalloc2 (with a patched spill weight), not LLVM's greedy allocator; frames are larger.])[
  LLVM's greedy allocator splits live ranges around loops and calls with a
  global cost model, and its stack coloring shares slots aggressively.
  regalloc2's backtracking allocator is faster but spills more; the patched
  spill weight keeps hot long-lived values in registers. The stack-frame
  gap is tracked as `STK` on the parity card.
]

#r("clif.vendor.isel", title: "Instruction-selection additions", level: "should",
   tier: 2, refines: "model.parity",
   checked: ("pliron-clif-opt/tests/widevec_gate.rs", "pliron-clif-opt/tests/aarch64_f16_cvt_encoding.rs", "pliron-clif-opt/tests/rorfold_multi_use.rs", "pliron-clif-opt/tests/carry_cinc.rs", "test.sh pairfuse"))[
  The vendored Cranelift SHOULD lower the shapes our passes emit to the
  instructions LLVM uses for them: on aarch64, `udot`, `mla`/`mls`,
  `smlal`/`umlal`, widening adds, shifted-operand logic and adds (a
  rotate only when it has one use), `adds`/`cinc` for an unsigned carry,
  `ext`/`rev64` lane permutes and `ldp`/`stp` pair fusion at emission; on
  x64, `psadbw`, wide (256/512-bit) vector operations, VSIB gathers and
  scatters, and memory-operand compares. A vector type wider than the
  enabled register class MUST fail compilation with an error, never compile
  to code that keeps only the low lanes.
]

#llvm(kind: "gap", summary: [Instruction selection is Cranelift's ISLE plus rules added one shape at a time; LLVM's DAG combiner catches more patterns generically.])[
  Each fused form above was added after a benchmark showed the stock ISLE
  lowering emitting the unfused sequence. Shapes no rule covers still lower
  instruction by instruction, where LLVM's selection DAG and combiner would
  find the fused form without a dedicated rule.
]

#r("clif.vendor.interp", title: "The interpreter matches codegen", level: "must",
   tier: 2, refines: "model.equiv", depends: "clif.diffcheck.observe",
   checked: "pliron-clif-opt/tests/diffcheck.rs")[
  The vendored `cranelift-interpreter` MUST evaluate every opcode the
  vendored Cranelift adds, MUST reinterpret bytes on a `bitcast` that
  changes lane count, and MUST sign-extend a load or store offset to the
  address width, as real code generation does, so that #term("diffcheck")
  does not report correct rewrites as mismatches.
]
