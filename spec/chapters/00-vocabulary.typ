#import "../pliron-spec.typ": *

#chapter("vocab", 0)
= Vocabulary

One word, one idea. Each entry is addressable as `term.<id>` and every
#term("cgu", [underlined word]) in the text links here. Entries list the
requirements that define the word ("defined by"), the requirements that use
it (collected automatically) and the code that confirms it ("checked in").
Glossary entries are not requirements and do not count toward tracey
coverage.

== The toolchain around us

#gl("stock", "Stock", qual: "rustc + LLVM", src: "rust-toolchain.toml")[
  The upstream rustc of the pinned nightly with its default LLVM backend. The
  reference every comparison in this document is made against.
]

#gl("backend", "Backend", qual: "codegen backend", see: "model.backend", src: "src/lib.rs")[
  A dynamic library implementing rustc's `rustc_codegen_ssa` traits, loaded
  with `-Zcodegen-backend`. "The backend" without qualification means this
  one, `librustc_codegen_pliron`.
]

#gl("mir", "MIR", src: "rustc_middle::mir")[
  rustc's mid-level IR. The backend never reads it directly:
  `rustc_codegen_ssa` walks it and calls the backend's builder methods.
]

#gl("cgu", "Codegen unit", qual: "CGU", src: "src/cgupool.rs")[
  The slice of a crate rustc hands a backend at once; it becomes one object
  file. Its count comes from `-Ccodegen-units`.
]

#gl("stage", "Stage", qual: "stage 1, stage 2", src: "selfhost/")[
  Bootstrap stages of rustc itself. Stage 1 is built by a stock compiler;
  stage 2 is stage 1 rebuilding itself. A #dfn[pliron-built stage 2] is rustc
  compiled by this backend.
]

#gl("sysroot", "Sysroot", src: "test.sh --sysroot")[
  The `core`, `alloc` and `std` rlibs a program links against. The
  #dfn[prebuilt sysroot] is stock's; `-Zbuild-std` rebuilds it with this
  backend.
]

== Our intermediate forms

#gl("pliron", "pliron", src: "vendor/pliron, vendor/PLIRON_REV")[
  An MLIR-like extensible IR framework in Rust, vendored with its LLVM
  bindings disabled.
]

#gl("llvm-dialect", "LLVM dialect", src: "vendor/pliron/pliron-llvm")[
  pliron's dialect that mirrors LLVM IR's operations and types. The builder
  emits it; nothing here ever turns it into real LLVM IR.
]

#gl("iropt", "IR optimization", qual: "iropt", src: "pliron-ir-opt/")[
  The passes that run on the LLVM dialect before lowering: inlining, SROA,
  interprocedural summaries and the like.
]

#gl("lowering", "Lowering", src: "pliron-clif-lower/src/lower.rs")[
  The translation of one function from the LLVM dialect into CLIF.
]

#gl("cranelift", "Cranelift", src: "vendor/cranelift-codegen/PLIRON_PATCH.md")[
  The code generator from the Wasmtime project: CLIF in, machine code out.
  Vendored with patches, each marked `PLIRON:`.
]

#gl("clif", "CLIF", qual: "Cranelift IR", src: "cranelift-codegen::ir")[
  Cranelift's SSA IR with block parameters. The form most optimization runs
  on.
]

#gl("clif-pipeline", "CLIF pipeline", src: "pliron-clif-opt/src/pipeline.rs")[
  The ordered list of steps run on each function's CLIF, `DEFAULT` in
  `pipeline.rs`.
]

#gl("step", "Step", src: "pliron-clif-opt/src/pipeline.rs")[
  One named entry of the CLIF pipeline: a pass, or a pass with particular
  settings, guarded by a gate that decides when it runs.
]

#gl("waffle", "waffle", src: "vendor/waffle, src/wasm.rs")[
  A wasm SSA IR and its structuring algorithms (reducify, stackify,
  localify). The wasm end of the pipeline.
]

== Our tools

#gl("rsasm", "rsasm", src: "vendor/rsasm, src/asm.rs")[
  A pure-Rust assembler. Assembles `asm!` and `global_asm!` in-process.
]

#gl("wild", "wild", src: "test.sh")[
  A fast pure-Rust linker for ELF. Used when it is on `PATH`; the `cc`
  driver still runs it.
]

#gl("pliron-wasm-ld", "pliron-wasm-ld", src: "tools/pliron-wasm-ld")[
  Our wasm linker, replacing LLVM's `wasm-ld`. Links wasm objects and rlibs
  produced by this backend into one module.
]

#gl("stub", "Stub", qual: "intrinsic stub", see: "model.intrinsics", src: "src/intrinsic.rs")[
  A weak definition emitted for an `llvm.*` intrinsic the backend cannot
  lower. It traps if called.
]

#gl("knob", "Knob", qual: "PLIRON_* variable", see: "model.off-switch", src: "pliron-cfg/src/lib.rs")[
  An environment variable named `PLIRON_*`, parsed once per process by
  `pliron-cfg`. A knob turns a pass on or off, sets a budget, or turns on a
  diagnostic.
]

== How we check ourselves

#gl("gate", "Gate", src: "harness/gate.py")[
  The set of checks that must pass before a change merges to main: build,
  unit tests, `test.sh` stanzas, corpus replay, wasm smoke, fuzz
  regression seeds.
]

#gl("corpus", "Corpus", src: "harness/corpus.sh")[
  Recorded pre-pipeline CLIF and facts for every function of a set of
  crates (`smoke`, `std`, `cargo`), replayed through the CLIF pipeline
  without rustc.
]

#gl("diffcheck", "Diffcheck", src: "pliron-clif-opt/src/diffcheck.rs")[
  Differential checking of a step: interpret the function's CLIF before and
  after, on random inputs, and compare.
]

#gl("parity-card", "Parity card", see: "model.parity", src: "harness/parity.py")[
  One rating per axis (speed, compile time, memory, size, stack,
  correctness) and a weighted overall; 100 is parity with stock.
]

#include "vocab/front.typ"

#include "vocab/iropt.typ"

#include "vocab/clif.typ"

#include "vocab/native.typ"

#include "vocab/wasm.typ"

#include "vocab/par.typ"

#include "vocab/verify.typ"
