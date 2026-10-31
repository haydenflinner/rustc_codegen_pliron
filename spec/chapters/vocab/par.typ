#import "../../pliron-spec.typ": *

== Words from Codegen units, caching and determinism

#gl("cgu-pool", "CGU pool", see: "par.pool.stages", src: "src/cgupool.rs")[
  The process-wide job store that finishes CGUs off rustc's main thread.
  rustc's main thread submits each defined CGU as a job; rustc's
  jobserver-token worker threads, inside `optimize`, run whatever job is
  runnable until their own module's output exists.
]

#gl("participant-ordinal", "Participant ordinal", see: "par.pool.ordinal", src: "src/cgupool.rs Store::next_ord")[
  The number a CGU that takes part in the cross-CGU facts merge gets when it
  is submitted: 0, 1, 2, … in submission order on rustc's main thread,
  which is also the order the serial path compiles CGUs in. Facts are
  stored and adopted by ordinal, never by completion order.
]

#gl("stage-a", "Stage A", see: "par.pool.stages", src: "src/cgupool.rs Step::A")[
  The first half of a participating CGU's job: the pre-optimization work
  and the IR passes up to and including the publish of its facts.
]

#gl("stage-b", "Stage B", see: "par.pool.stages", src: "src/cgupool.rs Step::B")[
  The second half of a participating CGU's job: fact adoption, the
  remaining IR passes, lowering and object emission. It runs only once
  the CGU's wait set has published.
]

#gl("parked", "Parked", qual: "stage-B job", see: "par.pool.release", src: "src/cgupool.rs Store::parked")[
  A stage-B job whose wait set is not yet #dfn[covered], that is, not every
  ordinal in it has published. A parked job is released to the ready queue
  the moment its wait set is covered.
]

#gl("xcgu-copy", "Cross-CGU copy", qual: "xcgu copy", see: "par.xcgu.copy", src: "src/xcgu.rs")[
  An `Internal` definition, in one CGU, of a small function whose home is
  another CGU of the same crate. It exists so the inliner can see the body.
]

#gl("emit-only-copy", "Emit-only copy", qual: "dead_unopt", see: "par.xcgu.keep", src: "pliron-ir-opt/src/inline.rs mark_dead_unopt")[
  A cross-CGU copy that no call the IR passes can see reaches. It is kept
  and emitted for link safety but gets only the minimal pipeline.
]

#gl("xfacts", "Cross-CGU facts", qual: "xfacts", see: "par.facts.publish", src: "pliron-ir-opt/src/state.rs GlobalFacts")[
  Callee contract facts (`nocapture`, `nowrite`, `nnret`, `willreturn`,
  `nounwind`) that one CGU computed on the bodies it emits and that other
  CGUs of the crate adopt for symbols they only declare.
]

#gl("two-phase-facts", "Two-phase facts", qual: "PLIRON_XFACTS_ALL", see: "par.facts.two-phase", src: "src/cgupool.rs two_phase")[
  The mode in which stage B runs behind rustc's thin-local LTO barrier, so
  each CGU adopts the facts of every other participant instead of only the
  lower ordinals.
]

#gl("step-cache", "Step cache", qual: "PLIRON_STEPCACHE", see: "par.cache.key", src: "pliron-clif-opt/src/stepcache.rs")[
  The on-disk, content-addressed store of CLIF pipeline results: one file
  of whole-pipeline variants and one file of per-step variants per input
  function.
]

#gl("memcache", "Memcache", qual: "PLIRON_MEMCACHE", see: "par.cache.mem", src: "pliron-clif-opt/src/stepcache.rs MEM")[
  The in-process tier of the step cache: whole-pipeline results held in
  memory for the life of one rustc process, shared by all its CGUs.
]

#gl("cache-variant", "Cache variant", see: "par.cache.read-set", src: "pliron-clif-opt/src/stepcache.rs PipeVariant, Variant")[
  One stored result for one cache key, together with the conditions under
  which it was produced: its read set and, for whole-pipeline variants, the
  list of steps that ran.
]

#gl("read-set", "Read set", see: "par.cache.read-set", src: "pliron-cfg/src/lib.rs reads_begin, reads_take")[
  The `(variable, value)` pairs of the `PLIRON_*` knobs a computation read
  through `pliron-cfg`, recorded in a tracking window. An unset variable is
  recorded with no value.
]

#gl("build-id", "Build id", see: "par.cache.key", src: "pliron-clif-opt/build.rs")[
  `PLIRON_BUILD_ID`: a fingerprint of the sources a CLIF step can run,
  baked into `pliron-clif-opt` at build time and folded into every cache
  key.
]

#gl("fn-hash", "Function hash", qual: "fn_hash", see: "par.cfg.opt-fns", src: "pliron-cfg/src/lib.rs fn_hash")[
  The high 32 bits of the 64-bit FNV-1a hash of a symbol name. It depends
  on the name alone, so it is the same in every thread, CGU, process and
  rebuild.
]

#gl("bisect-unit", "Bisect unit", see: "par.cfg.bisect", src: "pliron-cfg/src/lib.rs bisect_in")[
  One application of an optimization that asks the `PLIRON_OPT_BISECT`
  budget for permission: a CLIF step on a function, one SROA split or
  forward, one constant-load fold, one LICM hoist, and so on.
]

#gl("det-variant", "Detcheck variant", see: "par.det.strategy", src: "harness/detcheck.py VARIANTS")[
  A set of `PLIRON_*` settings that must change only how work is
  scheduled. `harness/detcheck.py` builds each input once as a reference
  and once per variant, and requires byte-identical objects.
]
