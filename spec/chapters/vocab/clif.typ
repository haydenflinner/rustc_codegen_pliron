#import "../../pliron-spec.typ": *

== Words from Lowering and the CLIF pipeline

#gl("leaf-list", "Leaf list", see: "clif.lower.leaves", src: "pliron-clif-lower/src/lower.rs")[
  The lowered form of an aggregate value: one CLIF value per scalar leaf, in
  layout order. `extractvalue` and `insertvalue` select or replace leaves and
  emit no instructions; a load or store of an aggregate is one access per
  leaf.
]

#gl("fn-job", "Function job", qual: "FnJob", see: "clif.lower.phases", src: "pliron-clif-lower/src/lower.rs")[
  One function's lowered CLIF together with every fact table the CLIF
  pipeline and the Cranelift compile need. A job borrows nothing from the
  pliron context, so any worker thread can run it.
]

#gl("tail-cc", "Internal tail convention", qual: "CallConv::Tail", see: "clif.lower.tailcc", src: "pliron-clif-lower/src/lower.rs")[
  Cranelift's `tail` calling convention, used for calls whose two ends are
  both code from this backend. It returns up to eight integer and eight float
  values in registers and allows `return_call`.
]

#gl("clif-fact", "Lowering fact", see: "clif.lower.facts", src: "pliron-clif-lower/src/lower.rs")[
  A property of a CLIF value, instruction or callee that lowering derives
  from the IR passes' results and hands to the CLIF pipeline (for example
  `nonnull`, `noalias`, `deref`, `frozen`). CLIF passes may assume a fact;
  they never have to prove it.
]

#gl("bb-op", "black_box op", see: ("clif.lower.blackbox", "clif.vendor.blackbox"), src: "vendor/cranelift-codegen/PLIRON_PATCH.md (22)")[
  A CLIF instruction added by our Cranelift patch: a unary identity with
  memory and other side effects, so no pass can fold, remove or move memory
  operations across it. Every ISA lowers it to zero machine instructions.
]

#gl("step-gate", "Step gate", qual: "Gate", see: "clif.step.gate", src: "pliron-clif-opt/src/pipeline.rs")[
  The condition that decides whether a step of the CLIF pipeline is
  selected: always, a default-on knob, an opt-in knob, a per-CGU category
  flag, or a conjunction of these.
]

#gl("legality-step", "Legality step", qual: "Gate::Always", see: "clif.step.always", src: "pliron-clif-opt/src/pipeline.rs")[
  A step that rewrites CLIF Cranelift cannot compile (for example
  `iconst.i128`) into CLIF it can. It is a correctness lowering, not an
  optimization, so no knob, bail or bisect budget skips it.
]

#gl("shape-bits", "Shape bits", see: "clif.step.shape", src: "pliron-clif-opt/src/pipeline.rs")[
  A small bit set of cheap function properties (has a loop, has a
  `br_table`, has a store, ...). Each bit is a necessary condition for some
  steps to apply. The cached set is a superset of the truth: a clear bit
  proves absence, a set bit proves nothing.
]

#gl("analysis-cache", "Analysis cache", qual: "Analysis", see: "clif.analysis", src: "pliron-clif-opt/src/pipeline.rs")[
  The per-function cache of the analyses CLIF passes share: control-flow
  graph, dominator tree, loop analysis, use index, parameter-alias facts.
  Keyed by an IR version that the pipeline advances after each mutating
  step.
]

#gl("uses-index", "Use index", qual: "uses::Uses", see: "clif.analysis.uses", src: "pliron-clif-opt/src/uses.rs")[
  A map from each value to the instructions that use it, built in one scan
  of the function. A snapshot: a mutation makes it stale until it is
  rebuilt.
]

#gl("alias-chain", "Alias chain", see: "clif.alias", src: "vendor/cranelift-codegen/src/ir/dfg.rs")[
  A sequence of CLIF value aliases (`v3 -> v2 -> v1`) left when passes
  replace values. A chain can end at a tombstone, a value that was
  deleted.
]

#gl("giant-fn", "Giant function", see: "clif.step.giant", src: "pliron-clif-opt/src/pipeline.rs")[
  A function whose CLIF has more instructions than the `PLIRON_GIANT_MAX`
  limit. Only legality steps run on it.
]

#gl("param-web", "Parameter web", see: "clif.phiweb", src: "pliron-clif-opt/src/proots.rs, docs/phi-web.md")[
  The block parameters connected through branch arguments, and the shared
  table of "which value arrives in this slot on each edge" facts that
  several passes query through Ascent Datalog programs.
]

#gl("inconclusive", "Inconclusive", qual: "diffcheck verdict", see: "clif.diffcheck.sound", src: "pliron-clif-opt/src/diffcheck.rs")[
  The verdict of a differential-check trial that could not compare the two
  functions: an unsupported operation, a wild pointer, exhausted fuel, a
  trap in the original, or output that depends on uninitialized bytes. It
  says nothing either way.
]

#gl("clif-check", "clif-check", see: "clif.check", src: "pliron-clif-check/src/main.rs")[
  The command-line tool that runs CLIF steps, the whole pipeline or the
  differential checker on saved `.clif` files and corpora, without rustc.
]
