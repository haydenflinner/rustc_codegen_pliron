#import "../pliron-spec.typ": *

#chapter("par", 1)
= Codegen units, caching and determinism

rustc splits a crate into #term("cgu", [codegen units]) and expects them to be
independent. This backend lets CGUs of one crate see each other: a CGU gets
private copies of small callees that live in other CGUs, and CGUs exchange
callee contract facts. It also finishes CGUs on rustc's worker threads,
lowers functions in parallel, and caches CLIF pipeline results by content.
Each of these adds a way for scheduling to leak into the output. This
chapter says how CGUs are compiled, what crosses between them, how the
caches are keyed, and which rules keep every byte a function of the inputs
(#refr("model.det")). It ends with the `PLIRON_*` configuration layer that
every off switch and diagnostic goes through.

#chapter-glance()

== How a crate's CGUs are compiled

#r("par.pool.stages", title: "Define on the main thread, finish on workers", level: "must",
   refines: "model.pipeline")[
  `compile_codegen_unit` MUST run only the mono-item `define`s on rustc's
  main thread, because they need `TyCtxt`. With the #term("cgu-pool", [CGU pool]) on,
  it then submits the CGU's pliron `Context` and `State` as one job. The
  job runs on the worker thread that holds a jobserver token in
  `WriteBackendMethods::optimize`. A job for a CGU that takes part in the
  facts merge (#refr("par.pool.ordinal")) runs in two steps:
  - #term("stage-a", [stage A]): the pre-optimization work (the `--emit llvm-ir` print,
    the `-O0` always-inline round, PGO instrumentation or profile load), then
    the IR passes through the publish of its facts;
  - #term("stage-b", [stage B]): fact adoption, the rest of the IR passes, lowering to
    CLIF and object emission.
  Any other CGU runs the same work as one `Whole` step that never waits.
  Both forms MUST run the same passes in the same order as the serial path
  (`finish_module`).
]

#r("par.pool.serial", title: "When the pool is off", level: "must",
   refines: ("model.det", "model.off-switch"))[
  The backend MUST NOT use the CGU pool when any of these is true:
  `PLIRON_CGUPAR=0`; `PLIRON_OPT_BISECT` is set; rustc's session asks for
  a parallel front end (`opts.jobs.frontend`), whose pre-compiled CGUs
  would scramble submission order.
  `compile_codegen_unit` then runs `finish_module` inline on the main
  thread, one CGU after another, and the facts merge publishes and adopts
  at once (`share_facts`).
]

#r("par.pool.ordinal", title: "Participant ordinals", level: "must",
   refines: "model.det")[
  A CGU MUST take part in the facts merge exactly when the crate is
  optimized and `PLIRON_XFACTS` is not `0`. Each participant MUST get the
  next #term("participant-ordinal") at submission, counting from 0 per crate
  on rustc's main thread. The allocator shim is created before every CGU,
  so it holds ordinal 0 in the pool, as it holds the first `share_facts`
  position on the serial path. So the ordinal order is the serial order.
]

#r("par.pool.release", title: "Stage B waits for its wait set", level: "must",
   refines: "model.det", depends: "par.pool.ordinal")[
  When stage A of the CGU with ordinal $o$ has published, the backend MUST
  compute its stage-B wait set, a set of ordinals below $o$. The job MUST
  stay #term("parked") until every ordinal in the set has published, and
  MUST then go to the ready queue. A job whose set is already covered goes
  to the ready queue at once. With `PLIRON_XNEED=0` the wait set is every
  ordinal below $o$.
]

#r("par.pool.xneed", title: "Wait only on owners", level: "must",
   refines: "model.det", depends: "par.pool.release")[
  By default (`PLIRON_XNEED` not `0`) the wait set of ordinal $o$ MUST be the
  ordinals below $o$ that own a symbol the CGU declares without a body. An
  ordinal owns a symbol when its CGU submitted a non-`Import` function with a
  body under that name; owners are recorded at submission, in ordinal
  order. Because adoption reads only the entries of bodiless declarations,
  the adopted facts MUST be identical to those under the full prefix. Owner
  sets can only shrink between submission and publish, so a stale owner
  costs a wait, never a missed dependency.
]

#llvm(kind: "gap", summary: [A CGU's post-facts half waits for the lower-ordinal CGUs that own its callees; LLVM's CGUs never wait on each other outside the LTO step.])[
  Under stock rustc each CGU's LLVM pipeline is independent until
  thin-local LTO. Here stage B of a CGU cannot start until its wait set has
  published, so a large CGU with a low ordinal delays the tails of the CGUs
  that call into it. Within one CGU the IR passes run on one thread (as
  LLVM's do); only the CLIF half is parallel per function
  (#refr("par.pool.lower-threads")). `PLIRON_XNEED` narrows the wait; it
  cannot remove it.
]

#r("par.pool.live", title: "Workers never block on runnable work", level: "must",
   refines: "model.backend", depends: "par.pool.ordinal")[
  A worker waiting in `optimize` for its own module MUST run any runnable
  CGU job first. Outside two-phase mode it MUST then run any queued
  per-function lowering job (`PLIRON_CGUSTEAL=0` turns this off), and it
  MUST sleep only when neither exists; a lowering drain waiting on its own
  queue MAY run a CGU job in turn. A parked job's wait set
  holds only ordinals that are already submitted, so the wait always ends.
  When a job panics, its ordinal MUST count as published, so higher
  ordinals do not stall; it then contributes no facts.
]

#r("par.pool.throttle", title: "In-flight caps", level: "must",
   refines: "model.det", see: "par.det.throttle",
   checked: ("harness/detcheck.py inflight1", "harness/detcheck.py acap2"))[
  `PLIRON_CGU_INFLIGHT=N` MUST block submission on the main thread while
  $N$ submitted CGUs are unfinished (unset or `0`: no cap).
  `PLIRON_CGU_ACAP=N` MUST keep at most $N$ CGUs between the start of their
  first step and done, while the main thread keeps defining; stage A starts
  in ordinal order, so every wait set stays satisfiable. `ACAP` has a
  default in `pliron_cfg::DEFAULT_VALUES`; `=0` turns it off. Both caps
  MUST be off while jobs are held for two-phase facts
  (#refr("par.facts.two-phase")), which needs every CGU published first.
]

#r("par.pool.lower-threads", title: "Per-function parallelism inside a CGU", level: "must",
   refines: ("model.det", "model.pipeline"), checked: "harness/detcheck.py par1")[
  Lowering a CGU MUST run in three phases. Phase (A) lowers each function
  from pliron IR to CLIF on one thread, because the pliron `Context` is not
  shareable. Phase (B) runs the CLIF pipeline and Cranelift compilation of
  each function as a self-contained job on one process-wide pool of
  `PLIRON_PAR` threads (default: the available parallelism), in any order.
  Phase (C) applies the results in declaration order. `PLIRON_PAR=0` and
  `PLIRON_OPT_BISECT` MUST make phases (A) and (B) serial.
]

== Cross-CGU body copies

#r("par.xcgu.copy", title: "Small callees are copied into each CGU", level: "must",
   refines: ("model.parity", "model.off-switch"))[
  When the crate is optimized, has more than one CGU, `PLIRON_XCGU` is not
  `0` and `PLIRON_HOT` is unset, each CGU MUST get an `Internal`
  #term("xcgu-copy", [cross-CGU copy]) of every eligible callee (#refr("par.xcgu.eligible"))
  that its own functions use and that lives in another CGU of the crate.
  The search follows callees of copies for a bounded number of rounds
  (`PLIRON_XCGU_DEPTH`). Copies are predefined before and defined after the
  CGU's own items. On wasm, `spec` MUST leave copies generic
  (`PLIRON_SPEC_COPIES=1` allows it): `pliron-wasm-ld` folds same-named
  bodies across objects, so every copy of an instance must keep the same
  semantics.
]

#llvm(kind: "choice", summary: [Cross-CGU inlining comes from private body copies made before optimization, not from ThinLTO function import.])[
  Stock rustc gets cross-CGU inlining from thin-local LTO: a summary-based
  thin link decides imports, and each module imports bodies after its own
  pre-link pipeline. Here the copy set is decided per CGU from rustc's
  mono-item use lists before any pass runs, and the copies go through the
  full IR pipeline in every CGU that holds one. The price is duplicated
  optimization work, which #refr("par.cache.mem") and
  #refr("par.xcgu.keep") claw back in part.
]

#r("par.xcgu.eligible", title: "What may be copied", level: "must",
   refines: ("model.equiv", "model.abi"), checked: "test.sh xcgufnptr")[
  Only `Item` and `Shim` instances MAY be copied, and only when their MIR
  size estimate is at most `PLIRON_XCGU_SIZE` (the default depends on the
  target). An instance MUST NOT be copied when it is naked, `no_mangle`,
  `rustc_std_internal_symbol`, `#[used]`, cold, thread-local, foreign,
  externally implementable, part of the allocator family, or has an
  explicit symbol name, linkage or link section. A function with a
  non-Rust ABI MUST NOT be copied: a copy has its own address, and foreign
  code compares such functions by address. An `#[inline(never)]` function
  MAY be copied, but the copy MUST be marked `no_inline`.
]

#r("par.xcgu.closed", title: "The copy set is reference-closed", level: "must",
   refines: "model.abi", checked: ("test.sh xcgurot", "test.sh xcgufnptr"))[
  A copy MUST be emitted only together with a definition for every item
  its body needs: an item of this CGU, an item some CGU defines with
  `External` linkage, or another emitted copy. Rotate and funnel-shift
  intrinsics, which are lowered at each call, need none. The number of
  copies is capped (`PLIRON_XCGU_CAP`, default the CGU's own item count),
  and the cap MUST admit whole dependency closures, in breadth-first order,
  so a cut never leaves a dangling reference.
]

#r("par.xcgu.keep", title: "Callerless copies are emitted, not optimized", level: "must",
   refines: ("model.abi", "model.parity"))[
  rustc's use lists do not show calls from one copy to another, so the
  dead-function sweep MUST NOT remove a copy, nor an `inlined` or
  `Internal` item of the CGU, which other copies may call the same way
  (`PLIRON_XCGUFN_KILL=1` allows it). A copy with no caller and no taken address that the IR
  passes can see becomes an #term("emit-only-copy") (`PLIRON_DEADUNOPT`,
  default on): it is lowered through the `Gate::Always` steps only. With
  `PLIRON_EARLYDEAD` (default on, off when `PLIRON_OPT_FNS` is set) these
  copies MUST be marked at the start of the IR passes, after inlining and
  after devirtualizing inlining, so later passes skip them. Lowering emits
  them last and only when something lowered before them, or a data
  initializer, references them; the rest get a one-`trap` body
  (`PLIRON_DEFERDEAD`).
]

#r("par.xcgu.deadfree", title: "Dead local bodies are freed", level: "must",
   refines: ("model.equiv", "model.parity"), checked: "test.sh deadfree")[
  With `PLIRON_DEADFREE` (default on), the three marking points of
  #refr("par.xcgu.keep") MUST free, not mark, every `Local` function that
  no remaining body calls and whose address is not taken, copies included,
  and repeat until nothing changes. The inliner MUST also free a local
  callee as soon as its last call is inlined. A later reference to a freed
  function MUST stop the compile with an error that names the symbol; it
  MUST NOT reach the linker. This flag changes the
  output (the freed bodies are no longer emitted); it is an optimization,
  not a scheduling knob.
]

== Cross-CGU facts

#r("par.facts.publish", title: "What a CGU publishes", level: "must",
   refines: ("model.equiv", "model.off-switch"))[
  A participating CGU MUST publish its #term("xfacts", [cross-CGU facts]) after the IR passes
  that compute them (`willreturn` is the last) and before the first pass
  that reads adopted facts (`callgvn`). It publishes one record per emitted
  body: each non-`Import` function with a body, keyed by the symbol and its
  printed function type. The record counts the copies and the copies that
  prove each of `nowrite` (strict and relaxed), `nnret`, `willreturn` and
  `nounwind`, and intersects the `nocapture` argument sets.
  `PLIRON_XFACTS=0` turns the merge off.
]

#r("par.facts.agree", title: "A fact crosses only when every copy agrees", level: "must",
   refines: "model.equiv", depends: "par.facts.publish",
   checked: "cargo test -p pliron-ir-opt xfacts_ignore_type_divergent_copies")[
  A CGU MUST adopt facts only for symbols it declares without a body; its
  own results always win. A fact MUST be adopted only when every emitted
  copy of the symbol with the same printed function type, in the merged
  records, proves it; `nocapture` adopts the intersection. Copies with
  another function type MUST NOT count: an ABI rewrite (`sret2reg`,
  `deadarg`) can leave one symbol with copies whose contracts differ.
]

#llvm(kind: "extra", summary: [Callee contract facts flow between the CGUs of one crate without LTO, and the result does not depend on scheduling.])[
  LLVM infers function attributes per module. Facts about a callee in
  another CGU reach a caller only through ThinLTO (summary attribute
  propagation or an imported body). Here every CGU publishes `nocapture`,
  `nowrite`, `nnret`, `willreturn` and `nounwind` for the bodies it emits,
  and CGUs adopt them by ordinal, so a call to a sibling CGU's function is
  optimized as if its contract were local.
]

#r("par.facts.prefix", title: "Prefix adoption", level: "must",
   refines: "model.det", depends: ("par.pool.ordinal", "par.pool.release"),
   checked: ("harness/detcheck.py serial", "harness/detcheck.py inflight1"))[
  Outside two-phase mode, the CGU with ordinal $o$ MUST merge the records
  of ordinals below $o$ and of no others. Records are stored per ordinal
  and the merge is order-independent, so the adopted facts MUST be those
  the serial path gives the same CGU, whichever worker finished first.
]

#r("par.facts.two-phase", title: "Two-phase facts behind the LTO barrier", level: "must",
   refines: "model.det", depends: "par.pool.ordinal",
   checked: ("test.sh xfactsall", "test.sh xfactsplugin"))[
  #term("two-phase-facts", [Two-phase facts]) MUST be on exactly when `PLIRON_XFACTS_ALL=1`,
  the pool is on, rustc chose thin-local LTO for the crate, and
  `-Clinker-plugin-lto` is off. Then `optimize` MUST stop each CGU after
  publish and hold it; rustc runs its thin step only after every module's
  `optimize`, so every participant has published when held stage B runs in
  `optimize_and_codegen_thin`. Each CGU MUST then adopt from every other
  participant. Two modules MUST finish on prefix facts instead: the
  allocator shim, which rustc compiles outside the barrier, and a module
  still held when rustc skips the thin step.
]

#note[
  *The serial path ignores `PLIRON_XFACTS_ALL`.* `two_phase` requires the
  pool (`cgupool::enabled`), and `finish_module` always uses
  `FactsMode::Share`. So under `PLIRON_XFACTS_ALL=1`, `PLIRON_CGUPAR=0`
  gives other bytes than the pool. This is the one known exception to
  #refr("par.det.strategy"); `harness/detcheck.py` reports it as `expect`,
  not as a failure. Checked against the code on 2026-10-30.
]

#r("par.facts.thin-claim", title: "Thin LTO is claimed only for the barrier", level: "must",
   refines: "model.backend", depends: "par.facts.two-phase",
   checked: "test.sh xfactsplugin")[
  The backend MUST tell rustc it supports thin LTO only under
  `PLIRON_XFACTS_ALL=1` without `-Clto=thin`, so an explicit `-Clto=thin`
  falls back to fat LTO. `run_thin_lto` MUST fail with a clear fatal error
  when handed other crates' rlibs or incrementally reused modules, rather
  than produce objects.
]

#llvm(kind: "gap", summary: [No cross-crate ThinLTO, and two-phase facts cannot reuse incremental thin-local modules.])[
  Stock rustc runs ThinLTO across crates for `-Clto=thin` and reuses
  unchanged modules in incremental thin-local LTO. Here thin LTO exists
  only as the two-phase facts barrier: cross-crate thin LTO becomes fat
  LTO, and reused (`Green`) modules under `PLIRON_XFACTS_ALL` are a fatal
  error.
]

#note[
  *Facts are invisible to rustc's dependency graph* (not verified by a
  test). Cross-CGU copies are made inside the CGU's dep-graph task, so
  rustc sees the queries they read. Adopted facts come through a
  process-global table that rustc does not track, and nothing in the
  backend turns `PLIRON_XFACTS` off under `-Cincremental`. Whether a CGU
  that rustc reuses can keep a fact about a callee that has since changed
  is open.
]

== Memory

#r("par.mem.lowerfree", title: "Freeing knobs only free", level: "must",
   refines: "model.det", see: "par.det.throttle",
   checked: ("test.sh lowerfree", "test.sh lowerfree_wasm"))[
  With `PLIRON_LOWERFREE` (default on) and no `-g`, lowering MUST drop a
  CGU's pliron `Context` as soon as phase (A) has lowered every function
  and serialized the data initializers. A compiled native function job MUST
  then shed its CLIF body and lowering facts, keeping its code, its
  external-name table and its statistics. With `PLIRON_JEARENA` (default
  on), the backend's threads MUST use their own jemalloc arenas, which by
  default purge freed pages at once; without jemalloc in the host it is a
  no-op. A read
  of a released `Context` MUST panic, not read freed memory.
]

== Caching

#r("par.cache.key", title: "Cache keys", level: "must", tier: 2,
   refines: ("model.det", "par.cache.read-set"))[
  The whole-pipeline key of a function MUST be the SHA-256 of:
  + the #term("build-id"), which hashes the sources of `pliron-clif-opt`
    and `pliron-cfg`, the vendored `cranelift-codegen` and
    `cranelift-codegen-meta` sources, `Cargo.lock` and the git `HEAD`;
  + the function-input hash: every fact map the steps can read, each
    hashed entry by entry and sorted, with region pointers replaced by
    canonical numbers; the function's symbol name; the call convention,
    pointer width and page alignment; and the `simd`, `x64`, `cmp64`,
    `has_eh`, `dump`, `verify_full` and `unopt` fields;
  + the state hash: the CLIF stencil, the base source location, each
    external function name (as its symbol when it has one), and the
    `derived` map, sorted;
  + the `PLIRON_PIPELINE` list.
  The same key names the function's step file. A per-step entry is keyed by
  step name and input state hash, and its output hash is the next step's
  input hash.
]

#note[
  The build id does not hash the vendored `pliron` or uncommitted edits
  outside the listed trees; the git `HEAD` term covers committed ones.
]

#r("par.cache.read-set", title: "A hit replays only matching variants", level: "must",
   refines: "model.det")[
  Each stored #term("cache-variant") MUST record the #term("read-set") of the
  run that produced it; whole-pipeline variants MUST also record the names
  of the steps that ran. A lookup MUST replay a variant only when every
  recorded read equals the current configuration and the recorded step
  list equals the current selection. Read windows nest, so a pipeline
  window holds the union of its steps' reads. `PLIRON_STEPCACHE`,
  `PLIRON_CACHE_DIR`, `PLIRON_CACHE_LOG` and `PLIRON_MCPROF` configure the
  cache, not the work, and MUST NOT be recorded. A replay MUST install the
  stored function, `derived` map, carried facts and statistics, so the
  output equals that of an uncached run.
]

#llvm(kind: "extra", summary: [CLIF pipeline results are cached per function and per step, keyed by content and by the knobs actually read.])[
  rustc's incremental compilation reuses whole CGU objects, and LLVM's
  ThinLTO cache reuses whole module objects. Neither reuses the result of
  one pass on one function. Here a gate flip misses only the variants whose
  read set or step list it changes; every other step still hits.
]

#r("par.cache.layout", title: "Disk tier", level: "must", tier: 2,
   refines: "par.cache.read-set")[
  With `PLIRON_STEPCACHE=1` the disk tier of the #term("step-cache") MUST live under
  `$PLIRON_CACHE_DIR`, or `~/.cache/pliron-steps` when that is unset, as
  `pipe/<h0h1>/<hex>.bin` (whole-pipeline variants) and
  `step/<h0h1>/<hex>.bin` (per-step variants of the steps marked `cached`),
  where `<hex>` is the key in lowercase hex and `<h0h1>` its first two
  digits. Each file MUST be a postcard-encoded list of variants, written to
  a `.tmp` file and renamed. Every IO or decode failure MUST count as a
  miss. Concurrent writers MAY lose variants; they MUST NOT corrupt one.
]

#r("par.cache.mem", title: "The in-process tier", level: "must",
   refines: ("model.det", "model.parity"), depends: "par.cache.read-set")[
  The #term("memcache") (on unless `PLIRON_MEMCACHE=0`) MUST use the same
  keys, variant matching and replay as the disk tier. A lookup that finds
  another thread computing the same key MUST wait for that result instead
  of computing it again; a producer that finishes without storing MUST
  wake its waiters, which then compute for themselves. Past
  `PLIRON_MEMCACHE_MB` (default 1024) of estimated size, new keys MUST NOT
  be stored. The in-process configuration is fixed, so this tier does not
  record read sets; with both tiers on, a memory miss falls back to disk.
]

#drift[
  `AGENTS.md` lists `PLIRON_MEMCACHE=1` as the way to turn the in-process
  tier on, and the doc comment on `mem_on` and `docs/PARITY.md` call it
  opt-in. The code reads `config().enabled("PLIRON_MEMCACHE")`, which is
  true when the variable is unset: the tier is on by default (commit
  `08ccc04`), and `=0` turns it off.
]

#llvm(kind: "extra", summary: [A function body duplicated across CGUs (copies, shared generics) runs the CLIF pipeline once per compile.])[
  LLVM optimizes every module's copy of a `linkonce_odr` or imported body
  separately. Here identical bodies key alike across CGUs, and parallel
  CGUs share one computation.
]

#r("par.cache.remap", title: "Keys and replay are independent of FuncId numbering", level: "must",
   refines: "model.det", depends: "par.cache.key")[
  Keys MUST hash each callee reference that has a symbol by that symbol,
  not by the module-local `FuncId` index. A variant MUST record the symbol
  of each such reference, and a replay MUST remap them into the consumer's
  numbering. A variant with a reference that does not resolve in the
  consumer MUST NOT be applied. A variant stored without symbols
  (verbatim, as when the producer could not name a reference) MUST be
  applied only where raw indices mean the same thing: in a consumer with
  no symbol table, or to a function that names no callee.
]

#r("par.cache.bypass", title: "When the caches stand aside", level: "must",
   refines: ("model.det", "model.dbg-neutral", "model.off-switch"))[
  Both tiers MUST be off while `PLIRON_OPT_BISECT` is set: a hit replays
  the producer's step list and cannot honour the consumer's remaining
  budget. A function whose CLIF carries `-g` value labels, and every
  function under per-step differential checking, MUST bypass the cache,
  because keys do not cover labels.
]

== Determinism rules

These are rules R1–R9 of `docs/determinism.md`, stated as requirements.
Together they make #refr("model.det") hold under parallelism.

#r("par.det.schedule", title: "R1: scheduling never decides results", level: "must",
   refines: "model.det", depends: ("par.facts.prefix", "par.pool.xneed"),
   checked: ("harness/detcheck.py inflight1", "harness/detcheck.py serial"))[
  Workers MAY finish CGUs in any order. Anything a CGU learns from other
  CGUs MUST be a function of the participant ordinals it depends on, never
  of which CGUs happened to finish first.
]

#r("par.det.strategy", title: "R2: execution strategy is invisible", level: "must",
   refines: "model.det",
   checked: ("harness/detcheck.py serial", "harness/detcheck.py par1"))[
  The same inputs MUST give the same bytes whether CGUs run in the pool or
  serially (`PLIRON_CGUPAR=0`), and whether functions in a CGU are lowered
  in parallel or not (`PLIRON_PAR=1`), apart from the documented
  `PLIRON_XFACTS_ALL` exception (see the note after
  #refr("par.facts.two-phase")).
]

#llvm(kind: "choice", summary: [Byte-identical output under any schedule despite cross-CGU dependencies; LLVM gets this from CGU independence.])[
  Stock rustc's CGUs share nothing before LTO, so thread count cannot
  matter. Here CGUs share copies and facts, and functions within a CGU are
  compiled in any order. Ordinals, declaration-order assembly and
  content-keyed caches restore the same guarantee, and
  `harness/detcheck.py` holds it to account with each
  #term("det-variant").
]

#r("par.det.throttle", title: "R3: throttles change timing, never bytes", level: "must",
   refines: "model.det", depends: ("par.pool.throttle", "par.mem.lowerfree"),
   checked: ("harness/detcheck.py inflight1", "harness/detcheck.py acap2", "test.sh lowerfree", "test.sh lowerfree_wasm"))[
  Thread counts, `PLIRON_PAR=N`, `PLIRON_CGU_INFLIGHT`, `PLIRON_CGU_ACAP`,
  `PLIRON_CGUSTEAL`, `PLIRON_LOWERFREE` and `PLIRON_JEARENA` MUST NOT change
  any output byte.
]

#drift[
  `docs/determinism.md` says `harness/detcheck.py`'s `memfree` variant
  checks the memory knobs. The variant sets `PLIRON_LOWERFREE=1` and
  `PLIRON_JEARENA=1`, which are now the defaults (`DEFAULT_ON`), so it
  compares the default build with itself. Only `test.sh lowerfree` and
  `lowerfree_wasm` build the `=0` leg. Likewise `--self-test` relies on
  `PLIRON_DEADFREE=1` changing the output, but that is the default too, so
  the self-test can no longer see a difference.
]

#r("par.det.order", title: "R4: no output order from unordered iteration", level: "must",
   refines: "model.det",
   checked: ("harness/detcheck.py again", "harness/run.py --suite determinism"))[
  Code MUST NOT iterate a randomly seeded `std` `HashMap` or `HashSet` into
  output. Output order MUST NOT rely on `Fx` map order either, which
  depends on allocation history: emission follows the insertion order of
  `SymMap` (which follows rustc's deterministic item order), the copy list
  is built in CGU item order, and phase (C) applies results in declaration
  order. A map hashed into a cache key MUST be hashed entry by entry and
  sorted, and Cranelift's `params` MUST be walked, not serialized, because
  it serializes in per-process order.
]

#r("par.det.work-budget", title: "R5: no wall-clock decisions", level: "must",
   refines: ("model.det", "model.bail"))[
  Every budget MUST be counted in work (operations, rounds, visited items),
  never in time. Clock reads MAY feed timing output and test assertions
  only.
]

#r("par.det.no-addr", title: "R6: no addresses", level: "must",
   refines: "model.det")[
  Output, keys and orderings MUST NOT depend on real memory addresses
  (`ptr as usize`, `Rc::as_ptr`) or on which allocator arena holds an
  object. Arena indices such as pliron `Ptr`s are allocation-history
  dependent and MUST be canonicalized before they reach a cache key (the
  step cache numbers region labels in first-seen order).
]

#r("par.det.fresh", title: "R7: per-crate state starts fresh", level: "must",
   refines: "model.det")[
  At the start of each crate's codegen, `cgupool::begin` MUST reset the job
  store and the crate configuration, and MUST clear the facts table and its
  ordinals (`state::reset_facts`). Any new process-global state MUST be
  reset there too, or keyed by content (as the memcache is).
]

#r("par.det.diag", title: "R8: diagnostics only observe", level: "must",
   refines: "model.diag-observe", see: "par.cfg.trace")[
  `PLIRON_TRACE`, the legacy print knobs, `PLIRON_*_INFO`, `PLIRON_MEM_MARK`,
  `PLIRON_FRAME_INFO`, `PLIRON_LT_PROBE` and the dumps MAY print; they MUST
  NOT change the IR. A diagnostic that has to run an analysis
  (`PLIRON_MEM_DEADPROBE` runs the dead-function sweep) MUST say so and
  MUST NOT be used in a measured or cached build.
]

#r("par.det.numbering", title: "R9: global numbering forces serial paths", level: "must",
   refines: "model.det", see: "par.cfg.bisect")[
  A process-global counter whose value reaches the output MUST force the
  serial paths. `PLIRON_OPT_BISECT` numbers #term("bisect-unit", [bisect units])
  globally, so it MUST turn off the CGU pool (#refr("par.pool.serial")),
  make lowering serial (#refr("par.pool.lower-threads")) and turn off both
  cache tiers (#refr("par.cache.bypass")).
]

#drift[
  `PLIRON_WASM_PEEP_LIMIT=N` (wasm byte peepholes on the first $N$ bodies)
  draws from a process-global counter but does not force the serial
  paths, so under parallel CGUs it peepholes a schedule-dependent set of
  bodies. `AGENTS.md` and the code comment admit the jitter and offer
  `PLIRON_WASM_PEEP_MOD` + `PLIRON_WASM_PEEP_FI` instead; R9 as written
  forbids it. Conversely `AGENTS.md` says bisection is deterministic "under
  `-C codegen-units=1` or `PLIRON_PAR=0`": the code already forces both
  serial paths whenever `PLIRON_OPT_BISECT` is set.
]

#llvm(kind: "extra", summary: [Transform bisection forces a deterministic serial order by itself.])[
  LLVM's `-opt-bisect-limit` counter is shared by the threads of a parallel
  build, so with several codegen units the numbering can change from run
  to run unless the user pins `-Ccodegen-units=1`. Here setting
  `PLIRON_OPT_BISECT` turns off every source of reordering.
]

== Configuration

#r("par.cfg.once", title: "The environment is parsed once", level: "must",
   refines: ("model.off-switch", "model.det"))[
  `pliron_cfg::config()` MUST read every `PLIRON_*` variable once per
  process, on first use, and every later read MUST be a map lookup.
  Changing the environment after that first read MUST have no effect. The
  tracing set-up (`PLIRON_TRACE` and the legacy print knobs) and
  `PLIRON_MCPROF` read the environment directly; they print only.
]

#r("par.cfg.accessors", title: "Accessor semantics", level: "must", tier: 2,
   refines: ("model.off-switch", "par.cfg.once"))[
  - `pass_enabled(X)`: true unless `X` is set to `0`.
  - `env_set(X)`: true when `X` is present, whatever its value.
  - `env_is(X, v)`: true when `X` equals `v`.
  - `env_get(X)`: the raw value, or none.
  `PLIRON_PIPELINE` MUST be split on commas, trimmed, with empty entries
  dropped. Every read through these accessors, except of the cache's own
  knobs (#refr("par.cache.read-set")), MUST land in each open read-set
  window of the calling thread.
]

#r("par.cfg.defaults", title: "Defaults live in one table", level: "must",
   refines: "model.off-switch", depends: "par.cfg.accessors")[
  A knob that is on by default MUST be listed in `DEFAULT_ON`, and a valued
  default in `DEFAULT_VALUES`; the parse fills each unset entry once, so
  every read site sees the same default and `=0` reaches all of them. Such
  knobs MUST be read with `env_is` or `env_get`, never `env_set`, which
  would count `=0` as on. A new pass lands as an `env_is(X, "1")` opt-in
  and is turned on by adding it to `DEFAULT_ON`.
]

#r("par.cfg.opt-fns", title: "PLIRON_OPT_FNS selectors", level: "must", tier: 2,
   refines: ("model.off-switch", "par.cfg.once"),
   checked: "cargo test -p pliron-cfg")[
  `PLIRON_OPT_FNS=<sel>` MUST select functions by symbol name, with `<sel>`
  one of:
  - `lo..hi`: #term("fn-hash") at least `lo` and below `hi`; each bound decimal or `0x`
    hex; `0..0x100000000` selects everything;
  - `~substr`: names containing `substr`;
  - `@file`: names listed in `file`, one per line; blank lines and lines
    starting with `#` are skipped, and a leading `<hash>` column ending in
    a tab is dropped.
  A leading `!` MUST invert the selection. An unparsable selector or an
  unreadable file MUST panic. An unselected function keeps its
  unoptimized body through the per-function IR transforms and skips the
  CLIF pipeline; interprocedural rewrites still see every function.
  `PLIRON_OPT_FNS_LOG=<path>` MUST append `<hash>\t<name>` once per
  selected name per process.
]

#r("par.cfg.bisect", title: "PLIRON_OPT_BISECT budget", level: "must", tier: 2,
   refines: ("model.off-switch", "par.det.numbering"))[
  `PLIRON_OPT_BISECT=N` MUST allow only the first $N$ bisect units of the
  process: each gate call takes the next number from one global counter and
  runs when that number is below $N$. Unset or unparsable, the budget is
  unlimited. With `PLIRON_OPT_BISECT_DEBUG=1`, each gate MUST log
  `bisect <n> <pass> run|skip <func> rss=<r>` on the `pliron::cfg` target
  at debug level, numbering units from 1.
]

#r("par.cfg.trace", title: "One tracing switch", level: "must",
   refines: "model.diag-observe")[
  `PLIRON_TRACE` MUST be an `EnvFilter` over the `pliron::*` targets. Each
  legacy print knob in `KNOB_TARGETS` (`src/lib.rs`) MUST, when present,
  add its `(target, level)` pair to the filter; `PLIRON_STATS` maps to
  `pliron::stats`, `PLIRON_TIMING` to `pliron::timing`,
  `PLIRON_OPT_BISECT_DEBUG` to `pliron::cfg` at debug, and so on. The table
  MUST hold print-gating knobs only. With neither set, no subscriber is
  installed.
]

#r("par.cfg.version", title: "The dylib names its source", level: "must",
   refines: "model.det")[
  With `PLIRON_VERSION` present, the backend's `init` MUST print
  `pliron backend <rev>` to standard error, where `<rev>` is the source
  revision baked in at build time (`harness/src-rev.sh`, or
  `.pliron-src-rev` in a self-host copy). This names the second input of
  #refr("model.det").
]
