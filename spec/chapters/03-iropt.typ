#import "../pliron-spec.typ": *

#chapter("iropt", 1)
= IR optimization

The #term("iropt") stage is step 2 of #refr("model.pipeline"): after
`rustc_codegen_ssa` has built a #term("cgu") in the #term("llvm-dialect") and
before #term("lowering") turns each function into #term("clif"). It is the
only place in the backend that sees the whole CGU at once as IR, so it owns
everything interprocedural: inlining (Cranelift does none), specialization,
devirtualization and the function #term("iropt-summary", [summaries]) that
the CLIF passes and lowering rely on. It also does the memory work that is
easier before SSA construction: SROA, call-slot forwarding and constant
loads. The code lives in `pliron-ir-opt/` and is driven, one CGU at a time,
by `iropt_head` and `iropt_tail` in `src/cgupool.rs`. Facts shared between
CGUs at the #term("iropt-facts-point") and cross-CGU body copies belong to
the parallel chapter; this chapter only names the point where they plug in.

#chapter-glance()

== Where the stage runs

#r("iropt.order", title: "One fixed pass order per CGU", level: "must",
   refines: ("model.pipeline", "model.det"),
   checked: ("test.sh invokeblk", "test.sh resumehoist"))[
  With optimizations on, every CGU MUST run the same sequence of IR passes,
  in the same order, on every target: an inlining head, the memory and
  scalar passes, devirtualization with one more inlining round, the
  #term("iropt-summary", [summaries]), the #term("iropt-facts-point"), and a
  call-cleanup tail. A target or opt level MAY change a pass's budgets, but
  MUST NOT reorder, add or drop passes. Every pass that mutates the IR is
  followed by a dominance check, which reports the first def that fails to
  dominate a use when `PLIRON_CHECK_DOM=1` and does nothing otherwise.
]

#drift[
  `AGENTS.md` places the pass order in `finish_module` in `src/lib.rs`.
  `finish_module` only calls `cgupool::finish_pre`, `iropt_head` and
  `iropt_tail`; the order is written in `src/cgupool.rs`, which the
  parallel CGU pool shares with the serial path.
]

#r("iropt.order.steps", title: "The step list", tier: 2, level: "must",
   refines: ("iropt.order", "model.off-switch"))[
  Before the IR passes, `finish_pre` drops debug-only spill slots
  (#refr("iropt.dbg")) and, when asked, instruments or loads a PGO profile.
  `iropt_head` then runs these steps, each under its #term("knob"); a knob
  shown `=1` is default-off:
  + `PLIRON_OPT_FNS` selection (#refr("iropt.optfns")); early dead-copy marking (`PLIRON_EARLYDEAD`);
  + `clean` (`PLIRON_INLINE_CLEAN=1`, #refr("iropt.cleanup"));
  + parameter fact collection: `frozen` (`PLIRON_FROZEN`), `noalias` (`PLIRON_NOALIAS_FWD`), `nnargs` (`PLIRON_NNARGS`);
  + `inline`, then `inline2` on the callers it touched (`PLIRON_INLINE`, `PLIRON_INLINE_ITER`);
  + dead-copy marking again; `spec` (`PLIRON_SPEC`);
  + `nounwind` (`PLIRON_NOUNWIND`); `sret2reg` (`PLIRON_SRET2REG`); `deadarg` (`PLIRON_DEADARG=1`);
  + `instcombine` (`PLIRON_INSTCOMBINE`); `memcpyopt` (`PLIRON_MEMCPYOPT`);
  + `sroa-fwd` (`PLIRON_SROA` and `PLIRON_SROA_FWD`); `phisimp` (`PLIRON_PHISIMP`);
  + `nowrite` (`PLIRON_NOWRITE`); `nocap` (`PLIRON_NOCAP`); `byvalhome` (`PLIRON_BYVALHOME=1`);
  + `sroa` (`PLIRON_SROA`); `constload` (`PLIRON_CONSTLOAD`);
  + `devirt` (`PLIRON_DEVIRT`, only with `constload` on) and, when it rewrote a call, `devirt-inline` (`PLIRON_DEVIRT_INLINE`), `sroa-fwd2`, `phisimp2` (`PLIRON_DEVIRT_PHI`), `sroa2` (`PLIRON_DEVIRT_SROA`), `constload2` (`PLIRON_DEVIRT_CL`);
  + dead-copy marking again; `nocap2`, only when the call graph grew or SROA made #term("iropt-escape-copy", [escape copies]);
  + `nnret` (`PLIRON_NNRET`); `retarg` (`PLIRON_RETARG`); `guardelim` (`PLIRON_GUARDELIM`);
  + `nounwind2` when the call graph grew; `nowrite2` when it grew or escape copies exist; `willreturn` (`PLIRON_WILLRETURN`);
  + the #term("iropt-facts-point"): `xfacts` publish or share (`PLIRON_XFACTS`).
  `iropt_tail` runs `xfacts-adopt`, `callgvn` (`PLIRON_CALLGVN`),
  `xoridem` (`PLIRON_XORIDEM`) with `callgvn2` only when it folded a loop,
  `lutcorr` (`PLIRON_LUTCORR=1`), the lowering flag scan, and `dead_fns`
  (`PLIRON_DEADFN`). `PLIRON_INLINE=0` turns the whole inliner off.
]

#note[
  One knob does not fully switch its pass off: on COFF targets with
  `panic=unwind`, `nounwind` and `nounwind2` run even under
  `PLIRON_NOUNWIND=0`, because emulated EH lowering reads the inferred
  facts to skip the flag check after calls that cannot throw.
]

#r("iropt.o0", title: "-O0 runs only the always-inline round", level: "must",
   refines: "model.pipeline")[
  At `-Copt-level=0` the backend MUST NOT run any IR pass of
  #refr("iropt.order") except one round that inlines `#[inline(always)]`
  callees (`PLIRON_ALWAYS_INLINE=0` disables it), followed by `dead_fns`.
  Opt levels 1, 2 and 3 MUST run the identical IR pipeline with identical
  budgets; `s` and `z` differ from them only in selecting
  #term("iropt-small") (#refr("iropt.inline.small")). PGO instrumentation
  and profile loading run after the always-inline round at every opt level,
  so a profile's CFG fingerprints match between the generating and the
  using build.
]

#llvm(kind: "choice", summary: [`-Copt-level=1/2/3` run the same IR pipeline and inliner thresholds; only `s`/`z` change budgets.])[
  LLVM's new pass manager builds a different pipeline per level: `-O1`
  uses a smaller simplification pipeline without vectorization, and `-O3`
  raises the inline threshold (250 against 225) and adds argument
  promotion. Here the IR stage keys only on "optimize or not" and
  on size mode; level-specific differences, where any exist, live in the
  CLIF pipeline.
]

#r("iropt.state", title: "Facts live in side tables, not in the IR", level: "must",
   refines: ("model.pipeline", "model.linear"),
   checked: "pliron-ir-opt/tests/colmap.rs")[
  Every fact a pass derives or reads about the IR — constants, alloca
  sizes, #term("iropt-promoted", [promoted allocas]), invoke edges, call
  info, volatility, #term("iropt-summary", [summaries]), inline history —
  MUST be kept in the CGU's #term("iropt-state"), keyed by pliron `Value`,
  `Operation`, block or symbol, and MUST be the same table lowering reads.
  Keys use the dense ordinal columns of the vendored #term("pliron")
  (`ColMap`, `ColSet`): a key whose entity was deleted and whose slot was
  reused compares unequal, so a stale fact can never attach to a new
  entity. The vendored pliron is built with its default features (and so
  `llvm-sys`) off, at the revision in `vendor/PLIRON_REV`.
]

#llvm(kind: "choice", summary: [IR facts (attributes, metadata, summaries) are side tables beside pliron IR, not attributes on it.])[
  LLVM carries `noalias`, `nonnull`, `!range`, `memory(read)`,
  `captures(none)` and the like as attributes and metadata on the IR, and
  each pass must preserve or drop them. Here the builder records rustc's
  attributes into #term("iropt-state") once, passes add their inferred
  facts to the same tables, and nothing is printed back into the IR. The
  cost is that a pass that clones or rewrites an op must copy its rows
  (the inliner replays a callee's rows into every clone).
]

#drift[
  `AGENTS.md` and this specification's README say every vendored change is
  marked `PLIRON:` and listed in the crate's `PLIRON_PATCH.md`.
  `vendor/pliron` has the `PLIRON:` markers (the columnar side tables in
  `src/cols.rs`, pooled operand and successor spans, intrusive use lists,
  interned attributes) but no `PLIRON_PATCH.md`.
]

#r("iropt.optfns", title: "Unselected bodies are not optimized in place", level: "must",
   refines: "model.off-switch")[
  When `PLIRON_OPT_FNS` is set, each #term("iropt-unselected", [unselected
  body]) MUST be skipped by every per-function transform (sroa, memcpyopt,
  phisimp, instcombine, constload, devirt, guardelim, callgvn, xoridem,
  lutcorr), and the inliner MUST NOT inline into it. It still contributes
  to the #term("iropt-summary", [summaries]), and it may still be inlined
  into a selected caller. Selection is by symbol name, so it does not depend
  on CGU partitioning.
]

#drift[
  `AGENTS.md` describes `PLIRON_OPT_FNS` as "optimize only selected
  symbols". Three interprocedural rewrites still touch unselected bodies:
  `spec` rewrites a callee's parameter uses, `retarg` folds call results in
  every body, and `nounwind` turns invokes into plain calls everywhere. A
  function bisection can therefore blame a selected function for a change
  made inside an unselected one.
]

#r("iropt.bisect", title: "Bisect units counted in this stage", tier: 2, level: "must",
   refines: "model.off-switch")[
  Under `PLIRON_OPT_BISECT`, these IR transforms MUST each consume exactly
  one #term("iropt-bisect-unit") and MUST be skipped once the budget is
  spent: one SROA split attempt on one alloca (`sroa`, logged with the
  function's symbol), one single-store forward (`sroa-fwd`), one
  constant-load fold (`constload`), and one function rewritten by `lutcorr`
  (`lutcorr`). No other IR pass consumes units; in particular every inlining
  decision runs regardless of the budget. The counter is process-global, so
  numbering is deterministic only under `-C codegen-units=1` or
  `PLIRON_PAR=0`.
]

== Inlining

#r("iropt.inline.scope", title: "Which callees may be inlined", level: "must",
   refines: ("model.equiv", "model.abi"))[
  The inliner MUST only copy a callee whose body is in the same CGU
  (including cross-CGU body copies placed there by the parallel stage), and
  only at a direct call whose function type matches the callee's. It MUST
  NOT inline a callee that is `#[inline(never)]`, has preemptible linkage,
  has no body, calls itself directly, or uses `va_*` intrinsics, nor a
  function into itself. A callee with landing pads is eligible only while
  `PLIRON_INLINE_EH` is on.
]

#llvm(kind: "choice", summary: [All inlining happens in pliron IR, CGU-local; Cranelift itself inlines nothing.])[
  Stock rustc relies on LLVM's inliner, which also sees only the CGU's
  bodies plus rustc's local copies of `#[inline]` and generic items. The
  scope here is the same, widened by the parallel stage's cross-CGU copies.
  Cranelift has no inliner, so a call that survives this stage stays a call
  on native targets; on wasm, the waffle inliner gets a second chance after
  CLIF (wasm chapter).
]

#r("iropt.inline.order", title: "Bottom-up, then incremental rounds", level: "must",
   refines: ("model.bail", "model.linear"))[
  The inliner MUST visit callers in a post-order of the CGU's direct call
  graph, so a callee has finished its own inlining before it is copied.
  Within one caller it MUST scan each block once and then only blocks that
  inlining spliced in, for at most 32 rounds. After the first pass,
  `inline2` MUST rescore only the callers that received code and the callers
  of callees that lost a callsite; after `devirt`, one more round MAY inline
  only the sites devirt rewrote plus any calls that inlining them exposes.
  Callers whose scan finds nothing to inline are left unchanged.
]

#llvm(kind: "choice", summary: [Inlining rounds are fixed (two plus one after devirt), not LLVM's CGSCC devirtualization iteration.])[
  LLVM's CGSCC pass manager reruns the inliner and function simplification
  on an SCC whenever devirtualization turns an indirect call into a direct
  one, up to a cap (`MaxDevirtIterations`, 4). Here there is exactly one
  devirt pass and one devirt-inline round per CGU; a call that becomes
  direct only after that round stays a call.
]

#r("iropt.inline.model", title: "Cost against threshold", level: "should",
   refines: "model.parity",
   checked: "test.sh inlinecm")[
  Outside #term("iropt-small"), a callsite SHOULD be inlined when its cost
  does not exceed its threshold, where the cost is the callee's op count
  plus a fixed weight per call inside the callee (inlining moves those
  calls, it does not remove them), doubled for a `#[cold]` callee; and the
  threshold is the base limit, raised for `#[inline]` callees, for a
  constant function address reaching an indirect-call parameter, for a
  scalar constant reaching a branch condition, by a fixed amount per use
  that a constant or local-slot argument resolves, and for a site inside a
  loop when the callee makes few calls of its own; capped at a fixed
  multiple of the base. `PLIRON_INLINE_MODEL=0` falls back to fixed
  per-callee thresholds.
]

#llvm(kind: "choice", summary: [The inline cost is a static op count with per-argument bonuses, not LLVM's simulated simplification (`InlineCost`).])[
  LLVM's `CallAnalyzer` walks the callee as if inlined at this site,
  folding constants and SROA-able pointers and counting only what survives
  (`InstrCost` 5, `CallPenalty` 25 against a threshold of 225 at `-O2`,
  325 for `inlinehint`). Here the cost is the callee's whole op count, and
  constant or local-slot arguments only raise the threshold by a fixed
  amount per use they reach. A callee whose body mostly folds away at one
  site is therefore under-valued, and one whose uses do not fold is
  over-valued.
]

#r("iropt.inline.knobs", title: "Inliner limits and knobs", tier: 2, level: "must",
   refines: ("iropt.inline.model", "iropt.inline.budget", "iropt.inline.small"))[
  Sizes count pliron ops. The defaults MUST be:
  - base limit `DEFAULT_LIMIT` 320 (`PLIRON_INLINE`; `0` disables
    inlining); in #term("iropt-small"), `SMALL_LIMIT` 40 on native and
    `WASM_LIMIT` 512 on wasm;
  - caller cap 20000 (`PLIRON_INLINE_CALLER_MAX`); per-caller growth
    budget 25 times the base (`GROWTH_MULT`, `PLIRON_INLINE_GROWTH`);
    threshold cap 32 times the base (`MAX_T`); 32 rescans per caller;
  - cost weights: 20 per nested call (`CALL_OP_W`, `PLIRON_INLINE_CALLW`),
    times 2 for `#[cold]` (`COLD_W`); bonuses of 25 per foldable use of a
    constant argument (`FOLD_USE`) and 20 per memory op a constant or
    local-slot argument reaches (`MEM_USE`);
  - threshold multipliers: 10 for a #term("iropt-free"); 4 for `#[inline]`
    (`PLIRON_INLINE_HINT`); 4 for a devirtualizable argument
    (`PLIRON_INLINE_DEVIRT`); 2 for a branch-folding argument
    (`PLIRON_INLINE_FOLD`); all three 0 in small mode;
  - loop bonus `PLIRON_INLINE_LOOP` 8 quarters of the base, for callees
    with at most `PLIRON_INLINE_LOOPCALLS` 4 calls; wasm loop multiplier
    `WASM_HOT` 3 (`PLIRON_INLINE_HOT`); fan-in factor
    `PLIRON_INLINE_FANIN` 8;
  - default-on gates: `PLIRON_INLINE_MODEL`, `_BU`, `_ITER`, `_HIST`,
    `_SCC`, `_EH`, `_ONCE1`, `_RECSTACK`, `_SCCLIMIT`, `_SMALLSCC`, and,
    outside small mode, `_ONCE` and `_EH_INVOKE`; default-off:
    `PLIRON_INLINE_COLD`, `_DUPCOST`, `_MAXDUP`, `_LOOPY`, `_WASM_COLD`,
    `_CLEAN`.
]

#r("iropt.inline.budget", title: "Inlining growth is bounded", level: "must",
   refines: "model.bail")[
  The inliner MUST NOT let any caller grow past an absolute op cap
  (`PLIRON_INLINE_CALLER_MAX`), and outside #term("iropt-small") MUST NOT
  let one caller absorb more non-free ops than a per-caller growth budget
  (`PLIRON_INLINE_GROWTH`). Both checks apply to every site, free ones
  included for the cap; a site that would exceed either stays a call.
  `#[inline(always)]` is not exempt from the cap.
]

#llvm(kind: "choice", summary: [Every caller has a hard size cap and a growth budget; LLVM has neither.])[
  LLVM bounds inlining per site (threshold) and, under the cost-benefit
  analysis, per call graph, but a caller may grow without an absolute
  limit. The cap here exists because rustc's recursive drop glue and derived
  traits otherwise unroll into one giant caller, and stage-2 rustc functions
  already sit near the size where any non-linear later pass costs minutes.
]

#drift[
  `docs/PARITY.md` lists "lazy caller-size (`op_count` computed only when a
  site reaches the cap check)" as kept. `inline::run` counts every caller's
  ops eagerly, once, before scanning it.
]

#r("iropt.inline.free", title: "Free inlines", level: "must",
   refines: ("model.parity", "model.equiv"))[
  A site whose callee is `#[inline(always)]`, or (outside
  #term("iropt-small")) a `Local`, never-address-taken callee with exactly
  one direct callsite left in the CGU, MUST be treated as a #term("iropt-free"): its
  threshold is ten times the base limit and, under the cost model, its cost
  is the bare op count. The single-callsite bonus MUST apply at most once
  per callee per inlining run (`PLIRON_INLINE_ONCE1`), since that callsite
  can be cloned into other bodies after the run starts.
  `#[inline(always)]` is honoured as a strong preference, not a guarantee:
  the caller cap, the recursion rules, the EH rule and the type check still
  apply.
]

#llvm(kind: "gap", summary: [`#[inline(always)]` can stay a call (caller cap, recursion, catch pads, invoke sites at -O0); LLVM's AlwaysInliner always inlines when legal.])[
  LLVM inlines an `alwaysinline` callee at every site where inlining is
  legal, at every opt level, invoke sites included. Here it gets a large
  threshold but still loses to the caller cap and the recursion and EH
  rules, and at `-O0` its invoke sites are skipped. For the last call to a
  local function LLVM adds `LastCallToStaticBonus` (15000), which in
  practice always inlines; the free bonus here is bounded at ten times the
  base limit.
]

#r("iropt.inline.fanin", title: "High fan-in callees stay out of line", level: "should",
   refines: ("model.parity", "model.linear"))[
  Outside #term("iropt-small"), a callee at a plain site SHOULD be inlined
  only while its op count times its remaining number of direct callsites
  stays within `PLIRON_INLINE_FANIN` times the site's threshold. Sites in
  loops, free sites and `#[inline(always)]` callees are exempt;
  `PLIRON_INLINE_FANIN=0` disables the rule.
]

#llvm(kind: "choice", summary: [A fan-in rule keeps a fat helper called from everywhere as one body; LLVM's per-site model has no such term.])[
  LLVM scores each site on its own, so a helper just under the threshold is
  copied into every caller. Rustc's per-CGU copies of drop glue make that
  the dominant source of code growth here, so total duplication is charged
  explicitly.
]

#r("iropt.inline.recursion", title: "Recursion is never unrolled", level: "must",
   refines: ("model.bail", "model.parity"),
   checked: "test.sh inlinerec")[
  The inliner MUST NOT inline a callee that is already on the site's
  #term("iropt-clone-chain") (`PLIRON_INLINE_HIST`), and MUST NOT inline,
  at a site that inlining created, a callee in the caller's own nontrivial
  call-graph SCC (`PLIRON_INLINE_SCC`). A callee in the caller's SCC MUST
  get only the base limit and no bonuses (`PLIRON_INLINE_SCCLIMIT`), and a
  caller on a recursion cycle the callee is not on MUST NOT get the
  single-callsite bonus for it (`PLIRON_INLINE_RECSTACK`). In
  #term("iropt-small"), no callee in the caller's SCC or self-recursive
  callee may be inlined at all (`PLIRON_INLINE_SMALLSCC`).
]

#llvm(kind: "extra", summary: [Two extra recursion rules protect stack depth: no free inline into a recursive caller, and base-limit-only inlining inside an SCC.])[
  The history and SCC rules are LLVM's (`InlineHistory`, the CGSCC rule for
  edges created inside an SCC). LLVM has no counterpart to `RECSTACK` and
  `SCCLIMIT`: they exist because every level of a recursion carries the
  inlined body's frame, and frames here are already larger than LLVM's
  (#refr("model.parity"), axis `STK`). A big helper inlined into a
  recursive visitor multiplied that frame by the recursion depth.
]

#r("iropt.inline.small", title: "Small mode for wasm and size levels", level: "must",
   refines: ("model.targets", "model.parity"))[
  For wasm targets and for `-Copt-level=s|z`, the inliner MUST run in
  #term("iropt-small"): a flat threshold (the wasm base limit on wasm,
  the small base limit on native), no cost model, no `#[inline]`, devirt or
  fold bonus, no single-callsite free inline, no growth budget, no fan-in
  rule, and no inlining of an EH callee at an invoke site. On wasm at `-O`
  (not `s` or `z`) a site inside a loop whose callee makes at most two calls
  MUST get `PLIRON_INLINE_HOT` times the flat limit.
]

#llvm(kind: "choice", summary: [wasm uses a separate flat inliner configuration with a loop bonus; LLVM uses one cost model for every target.])[
  LLVM's inliner is target-independent apart from TTI cost hooks; wasm and
  native at `-O2` share the 225 threshold, and `-Os`/`-Oz` lower it (to 50
  and 5). Here wasm keeps the flat rules because size growth there is
  paid in module size and V8 compile time (a bonus-driven model once grew
  rustc.wasm past V8's module limit), while the larger flat limit and the
  loop bonus recover the speed that small helpers in hot loops cost.
]

#r("iropt.inline.hot", title: "Hot and cold sites", level: "should",
   refines: "model.parity",
   checked: "test.sh pgo")[
  When a PGO profile is loaded, a site whose block has a recorded count of
  zero SHOULD NOT be inlined unless the callee is `#[inline(always)]`, and
  a site that ran at least four times SHOULD get twice the base limit added
  to its threshold. With `PLIRON_INLINE_COLD=1`, a site in a block that
  cannot reach a return gets at most a fifth of the base limit. Inlining
  MUST carry the profile into the clones, scaling the callee's block counts
  by the callsite count over the callee's entry count.
]

#drift[
  `docs/pgo.md` says hot sites get a multiplier and zero-count sites are
  "treated like `#[cold]` callers", and that the profile is applied in
  `optimize_cgu`. The code adds a fixed amount (twice the base limit) for a
  count of four or more, rejects zero-count sites outright, and runs PGO in
  `cgupool::finish_pre`.
]

#llvm(kind: "choice", summary: [Profile-guided inlining uses fixed count cut-offs, not LLVM's profile-summary hot/cold thresholds.])[
  LLVM classifies a callsite as hot or cold against the whole program's
  profile summary (`ProfileSummaryInfo`) and switches to
  `hot-callsite-threshold` (3000) or `inline-cold-callsite-threshold` (45).
  Here a recorded zero rejects the site and four or more executions add a
  fixed bonus; the profile format itself (keyed by CFG fingerprint, so a
  stale record is ignored rather than misapplied) is described in
  `docs/pgo.md`.
]

#r("iropt.inline.eh", title: "Inlining keeps unwinding exact", level: "must",
   refines: "model.unwind",
   checked: ("test.sh invokeblk", "test.sh resumehoist"))[
  When a callee is inlined at an invoke, every plain call in the copied
  body MUST become an invoke to the same landing pad, and the inlined
  callee's `resume` MUST continue into that pad. A callee that has landing
  pads of its own MUST NOT be inlined at an invoke whose pad is a catch
  (`catch_unwind`), because the unwinder's search phase would no longer see
  that catch; it MAY be inlined at an invoke whose pad is a cleanup. Every
  invoke MUST still end its block after inlining.
]

#llvm(kind: "gap", summary: [A callee with its own landing pads is never inlined into an invoke whose pad catches.])[
  LLVM inlines through any invoke: it merges the callee's landingpad
  clauses with the caller's and redirects `resume` to the invoke's unwind
  destination. The pad model here keeps one pad kind per invoke, so the
  catch case is refused rather than modelled.
]

#r("iropt.inline.dead", title: "Fully inlined locals are not emitted", level: "must",
   refines: "model.abi",
   checked: ("test.sh deadfree", "pliron-ir-opt/tests/erase_reuse.rs"))[
  After the IR passes, a `Local` function with a body MUST NOT be lowered
  when no remaining body calls it directly and its address is not taken
  (iterated, since a dead function's own calls do not count). Under
  `PLIRON_DEADFREE` the inliner MUST free such a callee's body as soon as
  its last call is inlined, and later clones reuse its arena slots. A
  function that any emitted code can still reach MUST keep its body.
  Cross-CGU copies, whose callers rustc does not report, follow the
  parallel stage's rules instead.
]

#r("iropt.dbg", title: "Debug info does not steer optimization", level: "must",
   refines: "model.dbg-neutral",
   checked: "test.sh debuginfo")[
  Before the IR passes, a stack slot that exists only to give a debug
  variable a location (declared, and only ever stored to) MUST be erased,
  with each stored value carrying the variable piece as a debug note that
  is not a use. No inlining cost, slot count or forwarding decision may
  depend on whether `-g` was given. Inlined clones MUST extend each copied
  op's inlined-at chain with the callsite's location.
]

#r("iropt.cleanup", title: "Optional cleanup around inlining", level: "may",
   refines: "model.linear",
   checked: "pliron-ir-opt/tests/cleanup.rs")[
  With `PLIRON_INLINE_CLEAN=1`, every body before inlining, and each caller
  right after the inliner finishes with it, MAY get a linear cleanup:
  side-effect-free ops with no uses are erased through a worklist, and a
  block whose only predecessor edge is an unconditional `br` is spliced onto
  it. The cleanup MUST skip the entry block, landing pads, the block after
  an invoke, and any block the frozen-region tables pin. The per-caller
  cleanup inside the inliner MUST NOT change any inlining verdict: scores
  keep counting the body as it was before the cleanup. (The cleanup before
  inlining does change op counts, and so verdicts.)
]

== Memory passes

#r("iropt.sroa", title: "SROA splits and promotes local slots", level: "must",
   refines: ("model.bail", "model.parity"),
   checked: "test.sh sroa")[
  SROA MUST split an alloca into one alloca per accessed slice, and mark a
  slice #term("iropt-promoted", [promoted]) when it is accessed only whole,
  at one type; lowering then builds its SSA form with Cranelift
  `Variable`s, across exception edges too. Constant-size `memcpy`,
  `memmove` and `memset` on the alloca become per-slice loads and stores.
  It MUST refuse an alloca larger than 512 bytes, one that would need more
  than `PLIRON_SROA_MAXSLICES` slices, one whose access walk exceeds its
  work budget, and one whose address escapes. Two escapes are tolerated: a
  diverging call, or a call that provably does not retain the pointer and
  writes no memory on a path that returns to the caller, gets an
  #term("iropt-escape-copy"); and a call with a known pointee extent pins
  only that byte range in memory. Each function gets at most three split
  rounds.
]

#llvm(kind: "choice", summary: [SROA only splits; SSA construction is cranelift-frontend's, and allocas over 512 bytes are never split.])[
  LLVM's SROA partitions allocas of any size and promotes the partitions
  itself (`PromoteMemToReg`, inserting phis). Here SROA only rewrites
  accesses and records which slices are promoted; the phi (block-parameter)
  placement is `cranelift-frontend`'s `Variable` machinery at lowering. The
  escape copy and the pinned extent go beyond LLVM, which keeps an alloca
  whole once any call can see its address.
]

#r("iropt.sroa.fwd", title: "Single-store forwarding", level: "must",
   refines: ("model.equiv", "model.parity"))[
  An alloca whose only uses are one store and loads of the stored type
  (directly or through zero-offset GEPs) MUST be replaced by the stored
  value when the store dominates every reachable load, and only then; the
  store, the loads and the dead GEPs are erased. With `PLIRON_LTUNDEF`,
  lifetime markers on the slot do not block forwarding. It runs before
  SROA, before and after the split rounds, and after devirt-inline.
]

#r("iropt.memcpyopt", title: "Call-slot forwarding", level: "must",
   refines: ("model.equiv", "model.parity"))[
  `call f(sret %tmp); memcpy(%dst, %tmp, size(tmp))` MUST become
  `call f(sret %dst)` only when `%tmp` is an alloca used by nothing but the
  two ops and `%dst` is a non-escaping alloca or the caller's own `sret`
  argument that nothing touches between the call and the copy. When the
  bounded walk that proves this runs out, the copy stays.
]

#llvm(kind: "gap", summary: [memcpyopt does only call-slot forwarding; no memcpy-of-memcpy, memset forwarding or stack-move merging.])[
  LLVM's MemCpyOpt also forwards a memcpy from the source of an earlier
  memcpy, turns memset-then-memcpy into memsets, drops copies of undef and
  merges two allocas joined by a full copy (stack-move). Of these only the
  by-value home (`PLIRON_BYVALHOME=1`, default-off) exists here, and the
  CLIF pipeline catches some of the rest after lowering.
]

#r("iropt.constload", title: "Constant loads fold", level: "must",
   refines: ("model.equiv", "model.parity"))[
  A non-volatile load from a constant offset into an immutable, non-TLS
  global that this CGU defines with a known initializer MUST be replaced by
  the constant the initializer holds at that offset when it can be read at
  the load's type; otherwise the load stays. Imported and preemptible
  globals are never folded.
]

#llvm(kind: "choice", summary: [Constant-global loads fold in pliron IR because Cranelift never sees global initializers.])[
  LLVM folds these during InstSimplify and InstCombine
  (`ConstantFoldLoadFromConstPtr`). Cranelift has no view of data, so the
  fold must happen here; it matters because `rustc_codegen_ssa` puts every
  non-scalar constant (even `None::<char>`) in a private global and loads
  from it, and the folded values are what make vtable slots constant for
  #refr("iropt.devirt").
]

== Scalar and call passes

#r("iropt.spec", title: "Constant-argument specialization", level: "must",
   refines: ("model.equiv", "model.parity"),
   checked: "cargo test -p pliron-ir-opt spec::tests")[
  When every direct call of a `Local`, non-address-taken function with a
  body passes the same constant for a parameter, of the parameter's type,
  the pass MUST replace the parameter's uses in the body with that
  constant. A function MUST NOT be specialized when any call to it sits in
  a body that is emitted as-is (a dead or minimally lowered copy). The
  signature and the calls stay unchanged.
]

#llvm(kind: "gap", summary: [Specialization is all-sites-agree constants only; no IPSCCP lattice, return-value propagation or function cloning.])[
  LLVM's IPSCCP propagates constants through arguments, return values and
  global variables with a lattice over the whole module, and
  `FunctionSpecialization` can clone a function for one constant argument.
  Here a parameter is folded only when every site already agrees.
]

#r("iropt.phisimp", title: "Trivial block arguments go", level: "must",
   refines: ("model.equiv", "model.parity"))[
  A block argument whose incoming values are all one value (or the
  argument itself) MUST be removed and its uses replaced by that value.
  This keeps inlined returns of argument-derived pointers from hiding an
  alloca's address behind a block argument, which would block SROA.
]

#r("iropt.instcombine", title: "Integer instcombine", level: "must",
   refines: ("model.equiv", "model.bail"),
   checked: "cargo test -p pliron-ir-opt instcombine::tests")[
  instcombine MUST only rewrite integer ops of width 1 to 128, and MUST
  replace a result only with a constant or with a value that was
  transitively an operand of it (so it dominates the result). It covers at
  most the first 20000 ops of a function. `PLIRON_INSTCOMBINE=egg` runs an
  equality-saturation version with the same contract for comparison.
]

#llvm(kind: "gap", summary: [instcombine is integer-only (plus `(x/d)*d`); LLVM's InstCombine also covers FP, vectors, memory and calls.])[
  LLVM's InstCombine and InstSimplify rewrite thousands of patterns over
  every type. Here the IR-level pass handles integer identities and
  constant folding only; most remaining peepholes run as CLIF steps after
  lowering, where Cranelift's own ISLE rules also apply.
]

#r("iropt.devirt", title: "Devirtualization", level: "must",
   refines: ("model.equiv", "model.parity"))[
  An indirect call whose callee operand is known to be the address of a
  function symbol at offset zero MUST become a direct call to that symbol.
  The rewritten sites and their callers seed the devirt-inline round of
  #refr("iropt.inline.order").
]

#r("iropt.guardelim", title: "Dead panic guards", level: "must",
   refines: ("model.equiv", "model.bail"),
   checked: "cargo test -p pliron-ir-opt guardelim::tests")[
  A conditional branch MUST be folded to its other successor only when the
  abandoned successor ends in `unreachable` (a panic path) and dominating
  branch facts decide the condition. Every proof search, dominator walk,
  reaching-store set and fixpoint has a fixed budget; when one runs out the
  branch stays.
]

#llvm(kind: "choice", summary: [guardelim folds only branches into panic paths, to upgrade summaries; LLVM's CVP and ConstraintElimination fold any implied compare.])[
  LLVM's CorrelatedValuePropagation and ConstraintElimination remove any
  comparison implied by dominating conditions. This pass exists to delete
  provably dead panic edges so that `nounwind` and `nowrite` can upgrade;
  general implied-compare folding happens in the CLIF pipeline.
]

#r("iropt.callgvn", title: "Call dedup, hoist and dead elimination", level: "must",
   refines: ("model.equiv", "model.parity"))[
  On the final call graph, after the #term("iropt-facts-point"), the pass
  MUST only: erase an unused call to a callee that is strictly write-free,
  nounwind and `willreturn`; reuse an earlier result for a direct call with
  the same callee and arguments when the callee is write-free (on returning
  paths) and `willreturn` and no write lies on any path between the two;
  and hoist such a call out of a write-free natural loop into its unique
  preheader when its arguments are available there. Invokes may be hoisted
  but never erased or deduplicated, and `black_box`, tail and foreign calls
  are never touched.
]

#llvm(kind: "choice", summary: [Dead-call elimination, readonly-call GVN and LICM of readonly calls are one linear pass over IPO facts.])[
  LLVM spreads this over DCE (`isRemovable` needs `memory(read)`,
  `nounwind`, `willreturn`), EarlyCSE/GVN for `readonly` calls, and LICM.
  Here one pass does all three on pliron IR, because the facts it needs are
  #term("iropt-summary", [summaries]) that do not survive into CLIF as
  attributes.
]

== Interprocedural summaries

#r("iropt.ipo.datalog", title: "Summaries are Datalog least fixpoints", level: "must",
   refines: ("model.bail", "model.det"),
   checked: "test.sh facts")[
  Each #term("iropt-summary") MUST be computed by an `ascent!` program over
  the CGU's call graph as the least fixpoint of a monotone relation seeded
  from the bodies: for five summaries the "may" complement (may write, may
  capture, may unwind, may diverge, may return null), for `retarg` the map
  from values to the parameter they provably equal. Recursion and loops
  through block arguments then converge conservatively. An indirect call or a call to a function without a body
  in the CGU MUST count as "may" unless a table names it (a short list of
  read-only libc functions, known throwing EH entry points). Preemptible
  bodies MUST NOT get facts unless their linkage is ODR. A summary computed
  before a pass that only deletes calls stays sound, so a summary is
  recomputed (`nocap2`, `nounwind2`, `nowrite2`) only after the call graph
  grew (devirt) or, for capture and write facts, SROA added escape copies.
]

#llvm(kind: "gap", summary: [Inferred function attributes cover six facts; no `norecurse`, `nofree`, `nosync`, argument-memory effects, `noalias` returns or `dereferenceable`.])[
  LLVM's FunctionAttrs (and Attributor, where enabled) infer, per SCC,
  memory effects including `argmemonly`, `captures(none)`, `nonnull` and
  `noalias` returns, `returned`, `nounwind`, `willreturn`, `norecurse`,
  `nofree` and `nosync`. Here the six summaries of this section exist; the
  rest have no consumer yet. Writing them as Datalog makes each rule one
  line that can be audited, and the facts can be dumped per function
  (`PLIRON_FACTS`), which is how `test.sh facts` checks them.
]

#r("iropt.ipo.nowrite", title: "nowrite: write-free functions", level: "must",
   refines: "model.equiv",
   checked: "test.sh facts")[
  A function MUST be summarized write-free (`true`) only when no store
  outside its own frame, atomic, fence, inline asm, `va_arg`,
  memory-writing intrinsic, or call that may write appears anywhere in it.
  It MAY be summarized write-free on return (`false`) only when every such
  write sits in a block that cannot reach a normal return or inside a plain
  call directly followed by `unreachable`, and no invoke in a returning
  block calls a callee that may write at all. Every other function gets no
  entry. `memcmp`, `bcmp` and `strlen` are write-free by table.
]

#llvm(kind: "extra", summary: [`nowrite` has a second level, write-free on every path that returns, which LLVM's memory effects cannot express.])[
  LLVM's `memory(read)` is all-or-nothing, so a function whose only write
  is on its panic path (a bounds-check failure that formats a message) is
  not `readonly`. Here such a function is write-free for every caller that
  sees it return, which is enough for load forwarding across the call and
  for call deduplication.
]

#r("iropt.ipo.nocapture", title: "nocapture: parameters not retained", level: "must",
   refines: "model.equiv",
   checked: ("test.sh facts", "test.sh nocapfields", "cargo test -p pliron-ir-opt nocapture::tests"))[
  A parameter MUST be summarized non-capturing only when no value derived
  from it is stored to memory other than a frame-confined temporary,
  returned, handed to inline asm, an unknown intrinsic or an indirect call,
  or passed in a position the callee may retain. Comparisons derive
  nothing; `ptrtoint` and `inttoptr` are the only bridges between integers
  and pointers. Parameters past the 64th are always treated as captured.
  The inference covers every parameter of every bodied function in the CGU,
  not only those rustc passes indirectly.
]

#r("iropt.ipo.nounwind", title: "nounwind: invokes become calls", level: "must",
   refines: "model.unwind",
   checked: "test.sh facts")[
  A function MUST be summarized nounwind only when no plain call in it may
  unwind and it rethrows nothing; invoked calls are caught by its own pads.
  The known throwing entry points (`_Unwind_Resume`,
  `_Unwind_RaiseException`, `_CxxThrowException` and the wasm throw and
  rethrow intrinsics and their emulations) MUST always count as unwinding.
  Every invoke of a nounwind callee MUST become a plain call, and the fact
  MUST be recorded on the function so that lowering can rely on it.
]

#r("iropt.ipo.willreturn", title: "willreturn: comes back deterministically", level: "must",
   refines: "model.equiv",
   checked: "test.sh facts")[
  A function MUST NOT be summarized `willreturn` when it contains a
  volatile access, an atomic op or fence, inline asm, a trap, an indirect
  call or a call to a function with no table entry that is not directly
  followed by `unreachable`, or a call to a function that is not
  `willreturn`. Loops and recursion do not, by themselves, deny the fact:
  the inference treats an infinite loop with no observable interaction as
  undefined.
]

#note[
  The code justifies the loop rule by saying rustc marks functions
  `mustprogress`. Stock rustc's LLVM backend emits no `mustprogress`, and
  Rust defines `loop {}` as running forever. A side-effect-free
  non-terminating function is therefore summarized `willreturn`, write-free
  and nounwind, and #refr("iropt.callgvn") may erase an unused call to it
  or hoist it into a loop preheader the loop never reaches. This is an open
  correctness question for #refr("model.equiv"), not a guarantee.
]

#llvm(kind: "choice", summary: [`willreturn` ignores loops (assumes forward progress); LLVM proves loops finite with SCEV before inferring it.])[
  LLVM's FunctionAttrs infers `willreturn` only when every instruction
  will return and the function has no cycle that might be unbounded, which
  needs a SCEV-computed maximum trip count unless the function is
  `mustprogress`. The rule here is weaker in what it demands and so infers
  the fact more often; see the note above for what that costs.
]

#r("iropt.ipo.ret", title: "Return-value summaries", level: "must",
   refines: "model.equiv",
   checked: ("test.sh retattr", "test.sh facts"))[
  A pointer-returning function MUST be summarized non-null-returning
  (`nnret`) only when every `ret` produces a value proven non-zero: a
  `!nonnull` load, a `nonnull` parameter, an alloca or defined symbol's
  address, an inbounds offset or select of such values, a block argument
  all of whose incoming values qualify, or the result of an `nnret` call.
  A function MUST be summarized as returning argument `k` (`retarg`) only
  when every `ret` returns parameter `k`, directly or through calls and
  selects that preserve it; each call to it then has its result replaced by
  its `k`-th argument. Lowering reads `nnret` to seed null-test folding.
]
