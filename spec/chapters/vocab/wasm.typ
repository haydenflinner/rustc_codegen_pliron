#import "../../pliron-spec.typ": *

== Words from WebAssembly

#gl("wasm-object", "Wasm object", qual: "one per CGU", see: ("wasm.obj.module", "wasm.obj.imports"), src: "src/wasm.rs (emit_obj)")[
  The file the backend writes for one #term("cgu") on a wasm target. It is a
  complete, valid wasm module, not an LLVM-style relocatable object: every
  symbol it does not define is reached through an import, and its link
  metadata is in the #term("pliron-link"). Only #term("pliron-wasm-ld")
  links it.
]

#gl("pliron-link", "pliron.link section", qual: "custom section", see: "wasm.obj.link", src: "src/wasm.rs (link_funcs, link_data)")[
  The custom section of a #term("wasm-object") that lists its defined
  functions with link flags, and its data objects with their bytes,
  alignment and relocations.
]

#gl("wasm-got", "GOT global", qual: "GOT.mem / GOT.func", see: ("wasm.obj.imports", "wasm.ld.got"), src: "src/wasm.rs (Obj::got_at)")[
  An immutable `i32` global that a #term("wasm-object") imports from module
  `GOT.mem` (a data address) or `GOT.func` (a function-table slot). Code
  reads a symbol's address with `global.get` of its GOT global; the linker
  supplies the value.
]

#gl("wasm-shadow-stack", "Shadow stack", qual: "__stack_pointer", see: "wasm.emit.frame", src: "src/wasm.rs (CX::new, CX::ret)")[
  The downward-growing stack in linear memory that holds every frame's
  stack slots. Its top is the mutable global `__stack_pointer`.
]

#gl("wasm-stub", "Wasm stub", qual: "unreachable body", see: "wasm.emit.stub", src: "src/wasm.rs (stub)")[
  A function body that is a single `unreachable`, emitted in place of a
  function the wasm end could not translate or compile. Not the same thing
  as an intrinsic #term("stub").
]

#gl("waffle-inliner", "Waffle inliner", see: ("wasm.inline.rules", "wasm.inline.budgets"), src: "src/wasm.rs (winline)")[
  The second inliner, which splices callee bodies in #term("waffle") IR
  after the CLIF pipeline, inside one #term("wasm-object").
]

#gl("wpeep", "wpeep", qual: "wasm byte peepholes", see: ("wasm.peep.scope", "wasm.peep.passes"), src: "src/wasm.rs (wpeep, peep_body_opts)")[
  The peephole pass that rewrites the encoded bytes of each wasm function
  body after waffle has compiled it: local-shuffle cleanup, copy
  forwarding, keep-on-stack, branch folds, store-to-load forwarding and
  dead-code flushes.
]

#gl("wpeep-copyfwd", "Copy-forward", qual: "wpeep sub-pass", see: ("wasm.peep.pathcheck", "wasm.audit.copyfwd"), src: "src/wasm.rs (peep_body_opts)")[
  The #term("wpeep") rewrite of `set t ← V … get t; set x` into
  `set x ← V …`: the producer of a scratch local writes the destination
  directly and the copy pair is deleted.
]

#gl("wpeep-audit", "Wpeep audit", qual: "translation validator", see: ("wasm.audit.copyfwd", "wasm.audit.model"), src: "src/wpeep_audit.rs")[
  The Datalog program (`ascent`) that checks one #term("wpeep-copyfwd")
  round: both instruction streams and the rewrite's assumptions go in as
  facts, and the rules report every read whose value differs on some path.
  Turned on by `PLIRON_WASM_AUDIT`.
]

#gl("wasm-eh", "Emulated EH", qual: "wasm unwinding", see: ("wasm.eh.check", "wasm.eh.intrinsics"), src: "pliron-clif-lower/src/lower.rs (eh_check, eh_raise)")[
  How a panic unwinds on wasm: a throw sets a flag in the
  #term("wasm-eh-word") and returns; every caller checks the flag after the
  call and either enters its landing pad or returns too. No wasm
  exception-handling instructions are used.
]

#gl("wasm-eh-word", "EH word", qual: "__pliron_eh", see: "wasm.eh.word", src: "tools/pliron-wasm-ld/src/lib.rs")[
  The 8-byte data symbol `__pliron_eh` that #term("pliron-wasm-ld")
  synthesizes: an `i32` flag at offset 0 (nonzero while an unwind is in
  flight) and the exception pointer at offset 4.
]

#gl("wasm-host-import", "Host import", see: "wasm.ld.imports", src: "tools/pliron-wasm-ld/src/lib.rs (host)")[
  A function import that stays in the linked module, for the embedder to
  provide: an `env` import no input defines, or any import from another
  module (`wasi_snapshot_preview1`, a `#[link(wasm_import_module)]` name).
]

#gl("pliron-wasi-libc", "pliron-wasi-libc", see: ("wasm.libc.pure", "wasm.eh.libc"), src: "tools/pliron-wasi-libc")[
  A `no_std` Rust static library that replaces the part of wasi-libc std
  calls on `wasm32-wasip1`, plus the `_Unwind_*` API over
  #term("wasm-eh").
]
