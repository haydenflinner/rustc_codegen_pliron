#import "../../pliron-spec.typ": *

== Words from IR optimization

#gl("iropt-state", "State", qual: "side tables", see: "iropt.state", src: "pliron-ir-opt/src/state.rs")[
  The per-#term("cgu") struct that every IR pass and the lowering share:
  facts about values, operations, blocks and symbols kept beside the IR
  instead of in it (constants, alloca sizes, promoted slots, invokes,
  summaries, inline history).
]

#gl("iropt-summary", "Summary", qual: "IPO fact", see: ("iropt.ipo.datalog", "iropt.ipo.nowrite", "iropt.ipo.nocapture"), src: "pliron-ir-opt/src/nowrite.rs")[
  A fact about a whole function that callers may rely on without seeing
  its body: write-free, non-capturing parameters, non-null return,
  returned argument, no unwinding, comes back. Inferred bottom-up over the
  CGU's call graph as a Datalog least fixpoint.
]

#gl("iropt-small", "Small mode", see: "iropt.inline.small", src: "src/cgupool.rs CrateCfg::new")[
  The inliner's size-sensitive configuration, chosen for wasm targets and
  for `-Copt-level=s|z`: flat thresholds, no cost model and no bonuses.
]

#gl("iropt-free", "Free inline", see: "iropt.inline.free", src: "pliron-ir-opt/src/inline.rs")[
  A callsite inlined under ten times the base limit because inlining it
  does not grow the program (the callee's only caller) or because the
  source demands it (`#[inline(always)]`).
]

#gl("iropt-clone-chain", "Clone chain", qual: "inline history", see: "iropt.inline.recursion", src: "pliron-ir-opt/src/state.rs inline_hist")[
  For a call that was copied out of an inlined body, the list of callees
  whose inlining produced it, innermost first. LLVM calls the same record
  `InlineHistory`.
]

#gl("iropt-promoted", "Promoted alloca", see: "iropt.sroa", src: "pliron-ir-opt/src/sroa.rs")[
  A stack slot SROA has proven to be accessed only whole, at one type, by
  loads and stores. Lowering turns it into a Cranelift `Variable`, so it
  never reaches memory.
]

#gl("iropt-escape-copy", "Escape copy", see: "iropt.sroa", src: "pliron-ir-opt/src/sroa.rs Kind::Escape")[
  A fresh stack copy of a local that SROA hands to a call in place of the
  original, when the callee cannot tell the difference. The original can
  then still be split.
]

#gl("iropt-facts-point", "Facts point", see: ("iropt.order", "iropt.order.steps"), src: "src/cgupool.rs iropt_head")[
  The end of the IR pipeline's head, after `willreturn`: the point where a
  CGU publishes its summaries to sibling CGUs and, in the tail, adopts
  theirs. Everything after it reads merged facts.
]

#gl("iropt-unselected", "Unselected body", see: "iropt.optfns", src: "pliron-ir-opt/src/helpers.rs opt_skip_init")[
  A function body that `PLIRON_OPT_FNS` leaves out. It is kept in
  `State::opt_skip`.
]

#gl("iropt-bisect-unit", "Bisect unit", see: "iropt.bisect", src: "pliron-cfg/src/lib.rs bisect_in")[
  One application of a transform that counts against the
  `PLIRON_OPT_BISECT` budget. Once the budget is spent, each later unit is
  skipped.
]
