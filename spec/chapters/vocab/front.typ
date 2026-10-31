#import "../../pliron-spec.typ": *

== Words from From MIR to the LLVM dialect

#gl("front-state", "Side table", qual: "State", see: ("front.ir.sidetables",), src: "pliron-ir-opt/src/state.rs")[
  The per-CGU `State` that travels with a pliron module. It holds what LLVM
  would keep as attributes and metadata: constant values, symbol names and
  linkage, call-site facts, volatile marks, alloca sizes, invoke targets.
  Keyed by operation or value, never by address order.
]

#gl("front-constval", "ConstVal", see: ("front.ir.consts",), src: "pliron-ir-opt/src/state.rs")[
  The side-table record of a constant: raw bits, zero, undef, bytes, an
  aggregate of other constants, or a symbol address plus offset. The value
  in the IR is the result of an `undef` placeholder op.
]

#gl("front-internal-fn", "Internal function", see: ("front.abi.internal",), src: "pliron-clif-lower/src/lower.rs")[
  A local, bodied, non-variadic function whose address never escapes its
  module and that is never the target of an invoke. Only this CGU can call
  it, so the backend may pick its calling convention.
]

#gl("front-bb", "Barrier call", qual: "black_box", see: ("front.intrinsic.blackbox",), src: "src/intrinsic.rs")[
  A call to a never-defined `__pliron_bb*` symbol, flagged in the call's
  side-table entry. Every IR pass treats it as an unknown call; lowering
  replaces it with the CLIF `black_box` op.
]

#gl("front-emulated", "Emulated intrinsic", see: ("front.intrinsic.llvm",), src: "src/simd.rs")[
  An `llvm.*` intrinsic, named by a crate through `link_name`, that the
  front end expands into ordinary LLVM-dialect ops with the intrinsic's
  semantics, lane by lane where needed.
]

#gl("front-asm-wrapper", "Asm wrapper", see: ("front.asm.wrapper",), src: "src/asm.rs")[
  The out-of-line assembly function an `asm!` block becomes. It takes one
  pointer to a stack buffer, moves inputs from the buffer into the chosen
  registers, runs the template and writes outputs back.
]

#gl("front-native-vec", "Native vector", see: ("front.simd.native",), src: "pliron-ir-opt/src/types.rs")[
  A vector type that lowers to Cranelift SIMD values (a 64- or 128-bit
  integer or float vector, or a whole multiple of one) instead of one value
  per lane.
]

#gl("front-va-buf", "Variadic buffer", see: ("front.abi.variadic",), src: "src/intrinsic.rs")[
  The stack buffer into which a caller packs the variadic arguments of a
  Rust-defined C-variadic function. Its address is the hidden last
  parameter, read in the body through `pliron.va.buf`.
]
