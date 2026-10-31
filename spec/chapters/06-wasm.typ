#import "../pliron-spec.typ": *

#chapter("wasm", 1)
= WebAssembly

For `wasm32-unknown-unknown` and `wasm32-wasip1` the backend runs the same
#term("lowering") and #term("clif-pipeline") as for native targets, then
leaves Cranelift behind: each function's optimized CLIF is translated to
#term("waffle") IR, a second inliner and waffle's structuring turn it into
wasm, and #term("wpeep") rewrites the encoded bytes. Each #term("cgu")
becomes a #term("wasm-object") in a format of our own, and
#term("pliron-wasm-ld") links the objects into one module. Panics unwind by
#term("wasm-eh"), and on `wasm32-wasip1` a pure-Rust libc,
#term("pliron-wasi-libc"), stands in for wasi-libc. Nothing on this path is
LLVM. This chapter states what each of those pieces promises, where the
copy-forward rewrite is checked by a translation validator, and where the
result differs from what stock rustc + LLVM produce for the same targets.

#chapter-glance()

== From CLIF to a wasm body

#r("wasm.pipeline", title: "The wasm end of the pipeline", level: "must",
   refines: "model.pipeline",
   checked: ("test.sh wasm_smoke", "tests/wasm/run.sh"))[
  On a wasm target each defined function MUST pass through these steps,
  after the shared stages of #refr("model.pipeline"):
  + lowering and the CLIF pipeline, with a 32-bit pointer type, so every
    address in the CLIF is an `i32`;
  + translation of the optimized CLIF to one waffle `FunctionBody`,
    followed by waffle's own SSA optimizer;
  + the #term("waffle-inliner") over all bodies of the object
    (#refr("wasm.inline.rules"));
  + waffle's backend, which makes the CFG reducible (block duplication),
    structures it into `block`/`loop`/`if` (stackify) and assigns wasm
    locals (localify), one body at a time;
  + #term("wpeep") over the encoded code section (#refr("wasm.peep.scope"));
  + the module tail: the #term("pliron-link"), `target_features` and the
    `#[link_section]` custom sections.
]

#llvm(kind: "choice", summary: [wasm code comes from Cranelift IR through waffle's reducify/stackify/localify, not from LLVM's WebAssembly backend.])[
  Stock rustc selects wasm instructions in LLVM's WebAssembly backend,
  which also structures control flow (`WebAssemblyCFGStackify`), keeps
  values on the operand stack (`WebAssemblyRegStackify`) and colors the rest
  into locals. Here the CLIF pipeline does the optimization, and waffle,
  with patches marked `PLIRON:` in `vendor/waffle` (edge-transfer local
  coalescing, select joins, single-use load sinking, a loop-nest guard on
  treeification), does the structuring. Waffle materializes more SSA values
  in locals than LLVM does; #term("wpeep") exists mostly to remove that
  traffic again.
]

#r("wasm.emit.repr", title: "Value representation", level: "must", tier: 2,
   refines: ("model.abi", "model.equiv"))[
  The translation MUST represent CLIF values in wasm as follows:
  - `i8`, `i16` and `i32` as `i32`; `i64` as `i64`; `f32` and `f64` as
    themselves;
  - an integer narrower than its wasm container (`i1`…`i31` in `i32`,
    `i33`…`i63` in `i64`) zero-extended;
  - `i128` and `f128` as a pair of `i64` values, low half first;
  - `f16` as its raw bits in an `i32`;
  - a 64-bit or 128-bit vector as one `v128` (the upper half of a 64-bit
    vector is unspecified).
  In a signature, the arguments and results flatten to these leaves in
  order, and the variable arguments of a C variadic function are passed as
  one `i32` pointer to a buffer.
]

#r("wasm.emit.frame", title: "Frames live on the shadow stack", level: "must",
   refines: "model.abi")[
  A function with stack slots MUST allocate its frame on the
  #term("wasm-shadow-stack"): on entry it reads `__stack_pointer`, computes
  the frame base as `(sp − frame) & ~(align − 1)` (alignment at least 16)
  and writes it back; every `return` MUST restore `__stack_pointer` to the
  value read on entry, including the early returns of #term("wasm-eh"). A
  function with no stack slots MUST NOT read or write `__stack_pointer`.
  When a callee is spliced by the #term("waffle-inliner"), the restores
  that derive from the callee's own frame are folded with it.
]

#r("wasm.emit.stub", title: "A body that cannot be emitted becomes a trap", level: "must",
   refines: ("model.bail", "model.intrinsics"))[
  When translating a function's CLIF to waffle, or compiling its waffle
  body to wasm, panics or fails, the backend MUST emit a #term("wasm-stub")
  with the same signature and export instead, and go on with the rest of
  the object. It MUST NOT emit a partial body. Each object with stubs MUST
  print `pliron-wasm: <cgu>: <n>/<m> functions stubbed` on stderr. With
  `PLIRON_WASM_STUBLOG=<file>` each stub MUST append one line to the file,
  `stub <symbol>: <reason>` for a translation failure and
  `stub <symbol> (backend): <reason>` for a waffle failure.
]

#llvm(kind: "gap", summary: [A function the wasm end cannot translate becomes an `unreachable` body with a warning, not a compile error.])[
  LLVM compiles every function it accepts or fails the build. Here a
  construct the CLIF-to-waffle translation does not handle (for example
  `try_call`, which #term("wasm-eh") avoids, or an unsupported vector lane
  type) turns that one function into a trap. The program still links, and
  behaves like stock until the stub is called. `PLIRON_WASM_STUBLOG` and
  the stderr count are how a run finds out.
]

#r("wasm.emit.par", title: "Parallel bodies, serial module", level: "must",
   refines: "model.det", checked: "harness/detcheck.py")[
  Per-body work on the wasm end (waffle compilation and #term("wpeep")) MAY
  run on several threads. The results MUST be collected in body order and
  the module assembled serially, so the object is the same byte for byte
  for every thread count (`PLIRON_PAR`, `PLIRON_WPEEP_PAR=0`).
]

== The wasm object

#r("wasm.obj.module", title: "An object is a complete module", level: "must",
   refines: ("model.pipeline", "model.abi"), checked: "test.sh wasm_smoke")[
  Each #term("cgu") MUST become one #term("wasm-object"): a module that
  validates on its own. Its code section MUST need no relocation: every
  function it calls but does not define is a function import, every symbol
  address it uses is read from a #term("wasm-got"), and the data it defines
  is not in a data section but in the #term("pliron-link"), where
  #term("pliron-wasm-ld") lays it out.
]

#llvm(kind: "choice", summary: [Our own wasm object format (complete module + `pliron.link` + GOT imports), not LLVM's relocatable wasm objects with `linking`/`reloc.*` sections.])[
  LLVM writes relocatable wasm objects as the WebAssembly tool conventions
  define them: a `linking` section with a symbol table and segments, and
  `reloc.CODE`/`reloc.DATA` sections that patch padded LEB immediates in
  the code. Ours resolve symbols through imports instead, so the code bytes
  never change at link time and the linker only renumbers indices. The cost
  is a `global.get` per address read until the linker folds it
  (#refr("wasm.ld.got")).
]

#llvm(kind: "gap", summary: [On wasm, stock-built objects and rlibs cannot be linked with ours: the whole program, `std` included, is built by this backend (`-Zbuild-std`).])[
  #refr("model.abi") asks that objects from this backend link with stock
  rustc's. On native targets they do. On wasm the formats differ
  (#refr("wasm.obj.module")) and #term("pliron-wasm-ld") reads only its own,
  so `core`, `alloc` and `std` are rebuilt with this backend
  (`tests/wasm/run.sh`, `harness/run.py --suite ui-wasm` provision such a
  #term("sysroot")).
]

#r("wasm.obj.imports", title: "What an object imports", level: "must", tier: 2,
   refines: "model.abi", checked: "test.sh wasm_smoke")[
  A #term("wasm-object") MUST import exactly these, and nothing else:
  - memory `env.memory`;
  - table `env.__indirect_function_table` (`funcref`);
  - mutable `i32` global `env.__stack_pointer`;
  - for each data symbol whose address it takes, immutable `i32` global
    `GOT.mem.<sym>`; for each function whose address it takes, immutable
    `i32` global `GOT.func.<sym>` (a table slot). Whether a symbol is data
    or a function comes from how it is referenced, also for symbols the
    object does not declare;
  - for each function it calls and does not define, a function import
    `env.<sym>`, or `<module>.<name>` for a function declared in a
    `#[link(wasm_import_module = "<module>")]` block. A function only
    referenced by address is not imported.
  In every name, a leading `\x01` (verbatim-symbol) byte is removed.
]

#r("wasm.obj.exports", title: "Defined functions are exported", level: "must", tier: 2,
   refines: "model.abi")[
  Every function body in a #term("wasm-object"), local or global, MUST be
  exported under its symbol name: the export section is the object's
  function symbol table. Function indices MUST number the imported
  functions first, then the defined ones in declaration order.
]

#r("wasm.obj.link", title: "The pliron.link section", level: "must", tier: 2,
   refines: "model.abi", checked: "test.sh wasm_smoke")[
  Every #term("wasm-object") MUST carry one custom section named
  `pliron.link`. All integers in it are `u32` little-endian (the addend is
  `i32`), and a string is a `u32` byte length followed by the bytes. The
  payload is:
  + `nfuncs`, then per defined function: name (string), flags (`u8`);
  + `ndata`, then per defined data object: name (string), flags (`u8`),
    alignment, byte count, the bytes, `nrelocs`, and per relocation: offset
    into the bytes, `is_func` (`u8`), target symbol (string), addend.
  Flag bits: 1 = global (visible to other objects), 2 = preemptible (weak),
  4 = `.init_array` entry, 8 = `#[used(linker)]` (a link-time GC root).
  `Local` linkage is 0, `Preemptible` is 3, every other linkage is 1. A
  relocation asks the linker to store, at the offset, the 4-byte value of
  the target (its address, or its table slot when `is_func` is 1) plus the
  addend. Data objects placed in a custom section by `#[link_section]` are
  not listed (#refr("wasm.obj.custom")).
]

#r("wasm.obj.features", title: "The target_features section", level: "must", tier: 2,
   refines: "model.abi")[
  Every #term("wasm-object") MUST carry a `target_features` custom section
  that lists each enabled feature as `+<name>`. The enabled set MUST be
  LLVM's `generic` CPU set (`bulk-memory`, `multivalue`, `mutable-globals`,
  `nontrapping-fptoint`, `reference-types`, `sign-ext`), or the empty set
  under `-Ctarget-cpu=mvp`, adjusted by the target's feature string and
  `-Ctarget-feature` (`+f` adds, `-f` removes). The same set is what
  `cfg(target_feature)` sees, and `simd128` in it turns on `v128` code.
]

#r("wasm.obj.custom", title: "link_section statics are custom sections", level: "must",
   refines: "model.abi")[
  A static with `#[link_section = "<name>"]`, where the name does not start
  with `.`, MUST become a custom section `<name>` holding the static's
  bytes, and #term("pliron-wasm-ld") concatenates same-named sections in
  link order (#refr("wasm.ld.sections")). Such a static MUST NOT contain a
  relocation (a pointer); the backend stops with an error rather than write
  a section the linker cannot patch.
]

#r("wasm.obj.gc", title: "Object-level dead-function removal", level: "must",
   refines: ("model.off-switch", "model.equiv"))[
  After the #term("waffle-inliner"), the backend MUST drop every `Local`
  function that no root reaches. Roots are the non-`Local` functions, the
  functions whose address escapes in the IR (relocations in data,
  `llvm.addressof`) and every function the object imports a `GOT.func` for;
  edges are `call` and `ref.func`. Dropped functions are left out of the
  module and the #term("pliron-link"). This removes only what the link-time
  GC of #refr("wasm.ld.gc") would remove anyway. `PLIRON_WASM_GC=0` turns
  it off.
]

#r("wasm.obj.nolto", title: "No LTO on wasm", level: "must",
   refines: "model.targets")[
  `-Clto` on a wasm target MUST stop with the error "fat LTO (-Clto) is not
  supported by the pliron backend on wasm targets". It MUST NOT fall back
  to a non-LTO link silently.
]

#llvm(kind: "gap", summary: [No LTO of any kind on wasm targets.])[
  Stock rustc runs fat or thin LTO through LLVM bitcode on wasm as on
  native targets. Native builds here accept `-Clto` and link without
  cross-crate optimization; wasm builds refuse it.
]

== The waffle inliner

#r("wasm.inline.rules", title: "What the waffle inliner may splice", level: "must",
   refines: ("model.bail", "model.off-switch"))[
  The #term("waffle-inliner") works inside one #term("wasm-object") and
  MUST NOT splice:
  - a function into itself;
  - a `#[inline(never)]` callee;
  - a callee whose entry block has predecessors, that has locals beyond
    its parameters, or whose parameter count differs from the call's
    argument count;
  - a callee with irreducible control flow, or into a caller that has it,
    unless `PLIRON_WASM_NOIRRED=0` (waffle's reducify duplicates blocks,
    and splicing compounds the duplication);
  - under the loop rule, a callee in the caller's call-graph strongly
    connected component, unless `PLIRON_WASM_NOREC=0`.
  It MUST stop after a fixed number of rounds or when a round splices
  nothing, re-run waffle's optimizer on each changed caller, and stop
  splicing into a caller once its size or its cumulative growth passes the
  caller cap. `PLIRON_WASM_INLINE=0` turns it off.
]

#llvm(kind: "choice", summary: [A second inliner runs on wasm-level IR after the CLIF pipeline, with its own size rules.])[
  LLVM inlines once, in its IR pipeline. Here the IR-level inliner runs
  first, and the waffle inliner then splices small and single-call
  callees, and bodies called inside loops, at the wasm level, where a call
  costs a frame round trip through linear memory. Its rules are sized in
  waffle instructions and tuned for module size against runtime: V8 has a
  module-size budget for its own wasm inlining, so a few KiB of growth can
  undo a win elsewhere.
]

#r("wasm.inline.budgets", title: "Inliner budgets", level: "must", tier: 2,
   refines: ("model.bail", "model.linear"))[
  Sizes are counted in waffle instructions. A callsite MUST be spliced
  only if one rule admits it:
  - `#[inline(always)]` callee of at most 300 instructions (`ALWAYS_CAP`);
  - callee of at most `PLIRON_WASM_TINY` instructions (default 6);
  - single callsite, no `ref.func`, at most `PLIRON_WASM_SOLO` (default 15);
  - `#[inline]` callee of at most `PLIRON_WASM_HINT` (default 0, so off),
    in the first round only;
  - GC completion: no `ref.func`, $k >= 2$ callsites and
    $n (k - 1) <= 5 + 2k$ for a callee of $n$ instructions;
  - loop rule: the callsite is in a loop of the caller, the callee has at
    most `PLIRON_WASM_HOT` instructions (default 400) and at most
    `PLIRON_WASM_HOTLEAF` calls of its own (default `1`, meaning 2;
    `0` lifts the limit), and the caller has absorbed at most
    `PLIRON_WASM_HOTCAP` instructions through this rule (default
    unlimited).
  A caller larger than `PLIRON_WASM_INLINE_CAP` (default 2048), or that has
  grown by more than that across rounds, MUST take no further splices. The
  inliner MUST run at most 8 rounds.
]

#drift[
  The comment in `winline` calls `PLIRON_WASM_HOTLEAF` "default off", and
  the comments on `PLIRON_WASM_GC` describe it as `=1` opt-in. Both are
  on by default: `pliron_cfg::DEFAULT_VALUES` sets `PLIRON_WASM_HOTLEAF=1`
  and `DEFAULT_ON` lists `PLIRON_WASM_GC`, `PLIRON_WASM_NOREC` and
  `PLIRON_WASM_NOIRRED`. This chapter follows the configuration.
]

#r("wasm.diag", title: "Diagnostic and bisection knobs", level: "must", tier: 2,
   refines: ("model.diag-observe", "model.off-switch"))[
  These knobs only observe and MUST NOT change any output byte:
  `PLIRON_WASM_DUMP=<substr>` (waffle bodies), `PLIRON_WASM_STUBLOG`,
  `PLIRON_WASM_SIZELOG`, `PLIRON_WASM_PEEP_LOG`,
  `PLIRON_WASM_TRACE_BODY=<k>` (the body after each #term("wpeep")
  sub-pass), `PLIRON_WASM_BODYDUMP=<dir>` (raw bytes before and after
  wpeep), and `PLIRON_WASM_VERBOSE` (an alias
  for `PLIRON_TRACE=pliron::wasm=debug`).
  These knobs bisect, and MUST only ever leave code less optimized:
  `PLIRON_WASM_PEEP_LIMIT=<n>` (peephole the first $n$ bodies the process
  emits), `PLIRON_WASM_PEEP_MOD=<substr>` with `PLIRON_WASM_PEEP_FI=<n>`
  (the first $n$ bodies of matching objects), `PLIRON_WASM_INLINE_LIMIT=<n>`
  (at most $n$ splices) and `PLIRON_WASM_INLINE_SKIP=<substr>` (no splices
  into matching callers). `PEEP_LIMIT` numbers bodies across objects
  finished in parallel, so its cut point may vary between runs;
  `PEEP_MOD` with `PEEP_FI` is the deterministic form.
]

== Byte peepholes: wpeep

#r("wasm.peep.scope", title: "wpeep works on encoded bodies and bails whole", level: "must",
   refines: "model.bail")[
  #term("wpeep") MUST run after the module is encoded and rewrite only the
  bodies of the code section; every other section MUST be copied unchanged.
  It reads the signatures of the type, import and function sections so
  calls take part in operand-stack accounting. A body with an opcode the
  decoder does not handle (typed references, for example) MUST be left
  exactly as waffle emitted it, and a module whose section layout does not
  parse MUST be left whole. `PLIRON_WASM_PEEP=0` turns wpeep off.
]

#llvm(kind: "choice", summary: [Peephole optimization on encoded wasm bytes after emission (`wpeep`), instead of on machine IR before it.])[
  LLVM's WebAssembly backend cleans up locals and branches on its machine
  IR (`WebAssemblyRegStackify`, `WebAssemblyExplicitLocals`,
  `WebAssemblyCFGStackify`) before it writes bytes. Waffle's output is
  fixed once compiled, so the clean-up happens on the decoded instruction
  stream of each body, between waffle and the module tail.
]

#r("wasm.peep.passes", title: "wpeep sub-passes and their switches", level: "must", tier: 2,
   refines: ("model.off-switch", "model.pipeline"))[
  #term("wpeep") MUST apply its sub-passes to each body in this order. A
  switch named here MUST disable its sub-pass alone:
  + decode, merging adjacent same-type local groups;
  + adjacent-pair coalescing: `get x; set x` → nothing, `set x; get x` →
    `tee x`, `tee x; set x` → `set x`, to a fixpoint;
  + materialization forwarding of single-def single-use locals
    (`PLIRON_WASM_FWD=0`);
  + #term("wpeep-copyfwd") with a dead-write sweep after each round;
  + keep-on-stack, at most `PLIRON_WASM_KOS` rounds (default 8; `0` off);
  + back-edge `if` fold to `br_if` (`PLIRON_WASM_BRF=0`);
  + dead-else flush (`PLIRON_WASM_DELSE=0`);
  + `if {br K} else {A}` → `br_if; A` (`PLIRON_WASM_BRI=0`);
  + dead tail-branch removal (`PLIRON_WASM_BTR=0`);
  + loop-invariant hoisting of pure `get; get; op` triples, extended to
    longer pure chains (`PLIRON_WASM_LICM_CHAIN=0` disables the chains);
  + `walias` operand-stack-aware local cancellation
    (`PLIRON_WASM_WALIAS=0`);
  + `sfwd` store-to-load forwarding through a stable base local
    (`PLIRON_WASM_SFWD=0`);
  + dead stack-value flush (`PLIRON_WASM_DDROP=0`);
  + `if {A} else {br K}` → `i32.eqz; br_if; A` (`PLIRON_WASM_BRI2=0`);
  + audited elision of untargeted `block`/`loop` pairs
    (`PLIRON_WASM_ELIDEBLK=0`, #refr("wasm.audit.elide"));
  + emit, fusing `load; extend` pairs into the extending load,
    `i32.const 0; i32.eq` into `i32.eqz` (and the `i64` pair),
    `set x; get x` into `tee x`, and dropping a final `return`.
]

#note[
  Four steps have no switch of their own and go only with
  `PLIRON_WASM_PEEP=0`: pair coalescing, copy-forward
  (`PLIRON_WASM_PEEP_PATHCHECK=0` only removes its straight-line rule, to
  demonstrate the audit), the triple form of loop-invariant hoisting, and
  the emit-time fusions. That is short of #refr("model.off-switch").
]

#r("wasm.peep.budget", title: "wpeep fixpoints are bounded", level: "must", tier: 2,
   refines: "model.bail")[
  Every #term("wpeep") fixpoint MUST have a bound: pair coalescing stops
  when a round rewrites nothing (each rewrite removes an instruction);
  copy-forward runs at most 4 rounds; keep-on-stack at most
  `PLIRON_WASM_KOS` rounds; the `bri2` fold and block elision at most 8
  rounds each; the passes that mint fresh locals (loop-invariant hoisting,
  `sfwd`) mint at most 48 per body together; the `sfwd` operand walk takes
  at most 80 steps. Running into a bound MUST leave the remaining
  candidates unrewritten.
]

#note[
  The dead-write sweep after each copy-forward round compares every write
  of a local with every read of it, so it is quadratic in the body, not
  linear as #refr("model.linear") asks.
]

#r("wasm.peep.equiv", title: "wpeep preserves stack, locals and structure", level: "must",
   refines: "model.equiv",
   checked: ("wpeep_tests (src/wasm.rs)", "tests/wasm/run.sh", "harness/run.py --suite ui-wasm"))[
  Every #term("wpeep") rewrite MUST leave, on every path, the same operand
  stack shape, the same value in every local at every read that remains,
  and the same memory effects in the same order (loads MAY be reordered
  with other loads, since an out-of-bounds trap is the same trap). The
  rewritten body MUST still be well-formed structured control flow, and a
  branch MUST still reach the same instruction after a fold renumbers its
  label.
]

#r("wasm.peep.pathcheck", title: "Copy-forward needs straight-line control", level: "must",
   refines: "model.equiv", depends: "wasm.peep.equiv",
   checked: "wpeep_tests (src/wasm.rs)")[
  #term("wpeep-copyfwd") MUST retarget a producer `set t` to the copy's
  destination `x` only when the producer and the copy's `set x` are in the
  same structured context with no `br`, `br_if`, `br_table` or `return`
  between them, so that every path from the producer passes the copy.
  Moving the write of `x` earlier otherwise changes what a read of `x`
  sees on a path that leaves before the copy, even when the same set of
  definitions still reaches it.
]

== The translation validator

#r("wasm.audit.copyfwd", title: "Audited copy-forward rounds revert on failure", level: "must",
   refines: ("model.equiv", "model.bail"), depends: "wasm.audit.model",
   checked: ("tests/wasm/run.sh", "test.sh wasm_smoke", "wpeep_tests (src/wasm.rs)"))[
  With `PLIRON_WASM_AUDIT` set to any value but `0`, every
  #term("wpeep-copyfwd") round that changes a body MUST be checked by the
  #term("wpeep-audit") before the next step runs. When the audit reports a
  violation, `PLIRON_WASM_AUDIT=panic` MUST stop the compile with the
  report; any other value MUST print the report, restore the body to its
  state before that round, and end copy-forward for the body. Earlier
  rounds stand. `tests/wasm/run.sh` and `tests/wasm/smoke.sh` set
  `PLIRON_WASM_AUDIT=panic`.
]

#llvm(kind: "extra", summary: [A Datalog translation validator (`PLIRON_WASM_AUDIT`) checks each wasm copy-forward round path-sensitively and reverts rounds it cannot confirm.])[
  LLVM ships no in-tree translation validation of its peepholes; tools
  such as Alive2 check InstCombine rewrites offline. Here the check runs
  inside the compiler on the actual function, states the rewrite's
  assumptions as facts next to the code, and acts as a guard: a round it
  rejects is undone, so an audited build only contains copy-forward rounds
  it confirmed (or that were over its budget, #refr("wasm.audit.budget")).
]

#r("wasm.audit.model", title: "What the validator models", level: "must",
   refines: "model.equiv")[
  The #term("wpeep-audit") MUST take as input only the raw body bytes, the
  instruction streams before and after the round (same positions, deleted
  entries marked), and one claim per committed rewrite (producer, copy
  pair, scratch local, destination local). From these it MUST build:
  control-flow edges shared by both versions; each version's writes with
  the value source each stores; reads of the same local in both versions
  and reads the rewrite redirected; and the claims as assumptions ("the
  copy stores the producer's value", "the copy reads exactly the
  producer"). It MUST report a violation for every read, and every
  assumption, at which the two versions can hold different values on one
  path. It MUST share no analysis code with #term("wpeep").
]

#note[
  Scope: local-variable dataflow only. The operand stack is not modelled,
  which is sound for copy-forward because it deletes only stack-neutral
  `get t; set x` pairs. Values are named by producing position, and a
  value held from an earlier loop iteration becomes `STALE`, which equals
  nothing. Facts are kept only where the versions disagree, and agreement
  is asked backward on demand, so the cost follows the code around each
  rewrite, not the function size.
]

#drift[
  `docs/audit.md` names keep-on-stack and the dead-write sweep as the
  #term("wpeep") rewrites the audit does not cover. In the code every
  sub-pass except copy-forward and block elision is unaudited: forwarding,
  keep-on-stack, the dead-write sweep, `brf`, `delse`, `bri`, `bri2`,
  `btr`, loop-invariant hoisting, `walias`, `sfwd` and `ddrop`. Block
  elision has its own, always-on fact check (#refr("wasm.audit.elide")),
  which `docs/audit.md` does not mention.
]

#r("wasm.audit.budget", title: "Audit budget", level: "must", tier: 2,
   refines: "model.bail")[
  The #term("wpeep-audit") MUST skip a round whose contracted control-flow
  graph has more than `BUDGET` = 50,000,000 node × (tracked locals + 1)
  units. A skipped round MUST stand as the pass decided, and MUST be
  counted and logged at debug level, never reported as a violation. A
  round whose control-flow graph cannot be built MUST be reported as a
  violation.
]

#r("wasm.audit.elide", title: "Block elision is always fact-checked", level: "must",
   refines: ("model.equiv", "model.bail"), checked: "wpeep_elide tests (src/wpeep_elide.rs)")[
  Each round of untargeted `block`/`loop` elision MUST, whatever
  `PLIRON_WASM_AUDIT` says, re-derive from the bytes after the round that
  each removed pair was untargeted (no branch inside names its label) and
  had the empty block type, and that the remaining control structure
  still parses. If any fact fails, the round MUST be undone
  (`PLIRON_WASM_AUDIT=panic` panics instead). The check shares no analysis
  with the candidate picker beyond the instruction bytes.
]

#r("wasm.audit.diffcheck", title: "Diffcheck cannot judge wasm32 functions", level: "must",
   status: "draft", refines: "model.equiv")[
  The CLIF #term("diffcheck") interprets functions on the host, so a
  wasm32 function (32-bit pointers in a 64-bit sandbox) MUST come back
  inconclusive, never as a pass. The stages after the CLIF pipeline
  (translation, the #term("waffle-inliner"), waffle's backend and
  #term("wpeep")) are outside diffcheck altogether; they are held to
  #refr("model.equiv") by #refr("wasm.audit.copyfwd"),
  #refr("wasm.audit.elide") and the end-to-end wasm tests.
]

== Linking: pliron-wasm-ld

#r("wasm.ld.args", title: "A wasm-ld compatible command line", level: "must",
   refines: "model.no-llvm", checked: ("test.sh wasm_smoke", "tests/wasm/run.sh"))[
  #term("pliron-wasm-ld") MUST accept rustc's `wasm-ld` command line when
  given as `-Clinker`: input objects and archives; `-o <file>`;
  `--export <sym>` and `--export=<sym>`; `--no-entry`; `-L <dir>` and
  `-l <name>` (finds `lib<name>.a`); `-z stack-size=<n>` (rounded up to
  16). Every other flag MUST be ignored. Archive members that are not wasm
  modules are skipped.
]

#llvm(kind: "choice", summary: [`pliron-wasm-ld` replaces `wasm-ld` (LLD) as the wasm linker.])[
  Stock rustc links wasm with `rust-lld -flavor wasm`, which is LLVM's.
  `pliron-wasm-ld` is a Rust program over `wasmparser`/`wasm-encoder`
  that understands only #term("wasm-object", [wasm objects]) from this
  backend. It is also a library (`pliron_wasm_ld::link`) so a host that
  cannot spawn processes can link in-process.
]

#r("wasm.ld.resolve", title: "Symbol resolution", level: "must",
   refines: "model.abi")[
  #term("pliron-wasm-ld") MUST always include the objects named directly,
  and include an archive member only when it defines a global symbol that
  an included object references and nothing included defines yet, or a
  root (an `--export` name, `_start`). It MUST repeat until no member
  qualifies. When several included objects define the same global symbol,
  the earliest in input order MUST win. A reference MUST resolve first to
  a definition in its own object, local or global, then to the winning
  global definition.
]

#llvm(kind: "gap", summary: [The wasm linker ignores weak linkage: the earliest definition wins even over a later strong one.])[
  `wasm-ld`, like ELF linkers, lets a strong definition override a weak
  one wherever they appear. `pliron-wasm-ld` reads the preemptible bit only
  to keep such symbols out of deduplication (#refr("wasm.ld.dedup")); for
  resolution, input order decides. The rule is relied on deliberately:
  `std`'s `rust_begin_unwind` precedes the one in
  #term("pliron-wasi-libc") on the link line.
]

#r("wasm.ld.gc", title: "Link-time GC and exports", level: "must",
   refines: "model.equiv", checked: "test.sh wasm_smoke")[
  #term("pliron-wasm-ld") MUST emit only the functions and data reachable
  from the roots: the `--export` names, or when there are none every
  global function of the directly named objects; `_start` unless
  `--no-entry`; every data object with the `.init_array` or
  `#[used(linker)]` flag; and every `__pliron_prof_*` data object. Edges are
  the `call`, `ref.func` and `return_call` targets and the `GOT.mem`/
  `GOT.func` reads of reachable code in live bodies, and the relocation
  targets of live data. Code after an unconditional transfer (`br`,
  `br_table`, `return`, `unreachable`, a tail call) up to the end of its
  block is unreachable and contributes no edges. The output MUST export
  `memory`, the roots that are defined functions, and `_start` when it is
  defined and an entry is wanted.
]

#llvm(kind: "extra", summary: [Link-time GC ignores calls in unreachable code inside a live body.])[
  `wasm-ld --gc-sections` keeps every function a live function's
  relocations name. `pliron-wasm-ld` scans each live body and skips calls
  that follow an unconditional branch, `return` or `unreachable` in the
  same block, so a call left in dead code does not pin its callee.
]

#r("wasm.ld.dedup", title: "Duplicate instances are merged", level: "must",
   refines: "model.equiv")[
  When several included objects define a live function under the same
  rustc-mangled name (`_R`, `_ZN` or `ZN` prefix) with the same link flags
  (not preemptible) and the same signature, #term("pliron-wasm-ld") MUST
  keep only the smallest body (ties broken by object order, then index)
  and redirect every call, table entry, GOT value and relocation that named
  another copy to it. Other names MUST NOT be merged. `PLIRON_LDEDUP=0`
  turns it off.
]

#llvm(kind: "choice", summary: [Duplicate generic instances across objects are merged by mangled name at link time, not through COMDAT groups.])[
  LLVM marks such functions `linkonce_odr` in a COMDAT, and `wasm-ld` keeps
  the first group. Our objects have no COMDATs, and cross-CGU copies of a
  body can differ in size after optimization, so the linker keeps the
  smallest.
]

#r("wasm.ld.layout", title: "Memory, table and linker-defined symbols", level: "must", tier: 2,
   refines: "model.abi", checked: ("test.sh wasm_smoke", "tests/wasm/run.sh"))[
  The output module MUST define (not import) one memory and one table, and
  lay out linear memory in this order:
  + bytes `[0, 1024)` unused;
  + the stack, `-z stack-size` bytes (default 1 MiB), growing down; global
    0 is the mutable `__stack_pointer`, initialized to `1024 + stack size`;
  + live data objects in link order, each at its alignment;
  + the `__pliron_prof_hdrs` table when profiling headers exist;
  + the #term("wasm-eh-word") (#refr("wasm.eh.word"));
  + the heap: `__heap_base` (16-aligned) to `__heap_end`, the end of
    initial memory, which is the pages needed to reach `__heap_base` plus
    one; no maximum.
  The linker MUST define `__heap_base`, `__heap_end`, `__data_end` (end of
  data), `__pliron_eh` and `__wasm_call_ctors`. Table slot 0 MUST be null
  and function slots start at 1; the table size is fixed. Data with only
  zero bytes MUST get no segment, and segments less than 65 bytes apart
  MUST be merged with zero fill (engines cap the segment count).
]

#r("wasm.ld.got", title: "GOT values are folded when it pays", level: "must",
   refines: "model.abi", depends: "wasm.ld.layout")[
  Each #term("wasm-got") import MUST resolve to the target's address
  (`GOT.mem`) or table slot (`GOT.func`). For each GOT import of an object,
  with $u$ reads in live bodies and value $v$, #term("pliron-wasm-ld") MUST
  rewrite each `global.get` of it to `i32.const v` when
  $u (1 + "sleb"(v)) <= 8 + 3u$, where $"sleb"(v)$ is the signed-LEB length
  of $v$; otherwise it MUST keep an immutable global initialized to $v$.
]

#drift[
  `docs/PARITY.md` (module size) says the GOT `global.get`s are kept,
  that LLVM writes resolved `i32.const` immediates instead, and lists GOT
  `global.get` → `i32.const` as measured and rejected. The linker now folds
  GOT reads to `i32.const` wherever the immediate is no larger
  (`Mapper::instruction`, `gconsts`).
]

#r("wasm.ld.imports", title: "Imports that stay", level: "must",
   refines: "model.abi")[
  An `env` function import MUST be linked to the definition it resolves
  to. An `env` function import that no input defines, and every function
  import from any other module, MUST become a #term("wasm-host-import") of
  the output, once per (module, name), and only if live code calls it, or
  an included object's `GOT.func` global or a live data object's function
  relocation names it. A data symbol that neither an input nor the linker
  (#refr("wasm.ld.layout")) defines, a `GOT.func` for a function that is neither
  defined nor imported, a function name used as data, and any global
  import other than `env.__stack_pointer`, `GOT.mem.*` and `GOT.func.*`
  MUST fail the link with an error naming the symbol.
]

#r("wasm.ld.main", title: "No-argument main gets a C main wrapper", level: "must", tier: 2,
   refines: "model.abi")[
  When the program defines a live `main` of type `() -> i32`,
  #term("pliron-wasm-ld") MUST add a function of type `(i32, i32) -> i32`
  that calls the original and returns its result, export it as `main` in
  place of the original, and name the original `__original_main` in the
  name section. This is the C `main(argc, argv)` entry point wasm-bindgen
  and other hosts look for.
]

#r("wasm.ld.ctors", title: "Constructors run from __wasm_call_ctors", level: "must", tier: 2,
   refines: "model.abi")[
  #term("pliron-wasm-ld") MUST synthesize `__wasm_call_ctors` as a function
  that calls, in link order, every function pointer stored in a live data
  object flagged `.init_array`, and MUST resolve references to
  `__wasm_call_ctors` (calls and table slots) to it. A relocation in such
  an object that does not name a live function MUST fail the link.
]

#r("wasm.ld.sections", title: "Custom sections, names and features", level: "must",
   refines: "model.abi")[
  The output MUST contain every custom section of the included objects
  other than `pliron.link`, `name`, `producers`, `target_features` and
  `reloc.*`, with same-named sections concatenated in link order; a
  `name` section naming each function by its symbol; and, when any object
  has one, a `target_features` section listing the union of the objects'
  `+` features. wasm-bindgen reads its `__wasm_bindgen_unstable` metadata
  and the feature list from these.
]

== Unwinding: emulated EH

#r("wasm.eh.word", title: "The EH word", level: "must", tier: 2,
   refines: "model.unwind")[
  `__pliron_eh` MUST be an 8-byte, 8-aligned, zero-initialized data symbol
  that #term("pliron-wasm-ld") synthesizes after the data: an `i32` flag at
  offset 0, nonzero exactly while an unwind is in flight, and the `i32`
  exception pointer at offset 4. Code reaches it through
  `GOT.mem.__pliron_eh`.
]

#r("wasm.eh.check", title: "Every call that may unwind is checked", level: "must",
   refines: "model.unwind",
   checked: ("tests/wasm/run.sh", "harness/run.py --suite ui-wasm"))[
  With `panic=unwind` on wasm, lowering MUST follow every call that may
  unwind with a load of the #term("wasm-eh-word") flag and a branch on it:
  for an `invoke`, to the landing pad; for a plain call, to a shared cold
  block that returns zero for each result (restoring the shadow stack,
  #refr("wasm.emit.frame")). A call is assumed unable to unwind, and is not
  checked, only when the callee is known `nounwind` or is a backend helper
  (`__pliron_asm_*`, `__pliron_llvm_*`, `__pliron_a64_*`). Lowering MUST
  NOT emit `try_call` on wasm.
]

#r("wasm.eh.intrinsics", title: "Throw, catch and rethrow", level: "must",
   refines: "model.unwind", depends: "wasm.eh.check",
   checked: "tests/wasm/run.sh")[
  With `panic=unwind` on wasm:
  - `llvm.wasm.throw` (also by the literal symbol std's `unwind::wasm`
    uses, on `wasm32-unknown-unknown`) MUST store its exception pointer at
    offset 4, set the flag to 1 and return through the cold block;
  - `llvm.wasm.rethrow` MUST set the flag and return;
  - `llvm.wasm.get_exception` MUST load offset 4;
    `llvm.wasm.get_ehselector` and `llvm.wasm.landingpad_index` MUST give 0;
  - a cleanup or catch pad MUST read the exception from offset 4 and clear
    the flag; a cleanup return with no unwind destination MUST set the flag
    again and return; a catch switch MUST take its first handler, since
    every handler rustc emits catches everything.
]

#llvm(kind: "choice", summary: [wasm panics unwind by a flag word and early returns (emulated EH), not with the wasm exception-handling proposal.])[
  Stock rustc with `panic=unwind` on wasm emits the exception-handling
  proposal's instructions (`try`/`catch`, or `try_table` and `throw` with
  a tag), which the engine must support and which cost nothing until a
  throw. Emulated EH runs on any engine and any proposal level, but every
  call that may unwind pays a load and a branch, and a function's results
  are zero on the unwinding path. It does not interoperate with exceptions
  thrown by the host or by C++ code compiled for the proposal.
]

#r("wasm.eh.libc", title: "_Unwind_RaiseException over the EH word", level: "must",
   refines: "model.unwind", checked: "tests/wasm/run.sh")[
  #term("pliron-wasi-libc") MUST define `_Unwind_RaiseException` to store
  the exception pointer at offset 4 of `__pliron_eh`, then set the flag to
  1, both with volatile stores, and return; the caller's check
  (#refr("wasm.eh.check")) carries the unwind up the stack.
  `_Unwind_DeleteException` MUST call the exception's cleanup function.
  The context accessors (`_Unwind_GetIP`, `_Unwind_GetGR`,
  `_Unwind_GetLanguageSpecificData`, …) MUST return 0 or do nothing.
]

#note[
  `_Unwind_Resume` sets the word the same way and then spins, since it is
  declared not to return. rustc's wasm code never calls it: on wasm,
  landing pads are funclet pads, and a cleanup that keeps unwinding sets
  the flag again itself (#refr("wasm.eh.intrinsics")).
]

== A libc for wasm32-wasip1

#r("wasm.libc.pure", title: "A pure-Rust libc replaces wasi-libc", level: "must",
   refines: ("model.no-llvm", "model.targets"),
   checked: ("tests/wasm/run.sh", "harness/run.py --suite ui-wasm"))[
  On `wasm32-wasip1`, the C library MUST be #term("pliron-wasi-libc"): a
  `no_std`, single-threaded Rust `staticlib` over the `wasip1` syscall
  bindings, with `dlmalloc` for the heap and `libm` for math. It MUST
  provide the functions std's wasip1 backend calls (memory, files,
  directories, environment, clocks, `errno`), and is linked as `libc.a`
  with `-Clink-self-contained=no`; with `panic=unwind` an empty
  `libunwind.a` satisfies std's `-lunwind`, because the `unwind` crate and
  this library provide the symbols.
]

#llvm(kind: "choice", summary: [On wasm32-wasip1, a pure-Rust libc replaces wasi-libc (C, built with clang).])[
  Stock rustc ships wasi-libc's `crt1-command.o` and `libc.a`, compiled
  from C by clang, as its self-contained linking files. A C library would
  need a C compiler for wasm, which means LLVM. `pliron-wasi-libc` covers
  only what std uses; functions outside that slice are missing at link
  time, and `pthread_create` cannot create threads.
]

#r("wasm.libc.start", title: "The command entry point", level: "must", tier: 2,
   refines: "model.abi")[
  `_start` in #term("pliron-wasi-libc") MUST call `__wasm_call_ctors`, then
  `__main_void`, and call `proc_exit` with the result when it is nonzero.
  Its `#[panic_handler]` (which also defines `rust_begin_unwind`) MUST
  trap, and loses to std's by #refr("wasm.ld.resolve") because std precedes
  `libc.a` on the link line.
]

== Measurement integrity and size

#r("wasm.bb", title: "black_box on wasm is a stack round trip", level: "must",
   refines: ("model.parity", "model.equiv"))[
  On wasm targets `core::hint::black_box` MUST store its argument to a
  fresh stack slot and read it back with a volatile load (a scalar, a
  scalar pair or a vector), or copy it into a second slot with a volatile
  `memcpy` and return that slot (any other layout). It MUST NOT use the
  zero-instruction CLIF `black_box` op that native targets use: in wasm
  that op would be a bare identity, which V8 optimizes through, making the
  barrier weaker than LLVM's (a stack store and reload on wasm) and wasm
  benchmarks look better than they are.
]

#r("wasm.size", title: "Module size", level: "should",
   refines: "model.parity", checked: "harness/parity.py")[
  A linked wasm module SHOULD be no larger than the one stock rustc + LLVM
  links from the same source. The parity card's code-size axis counts the
  wasm `CODE` section bytes of the compile-time inputs and the size of the
  wasm benchmark module.
]

#llvm(kind: "gap", summary: [wasm modules are larger than LLVM's, from operation density: more local traffic and structured `if` diamonds.])[
  The gap is not dead code: the link-time GC leaves only reachable bodies.
  It is density. Waffle puts block parameters through scratch locals at
  `if`/`else` boundaries where LLVM keeps values on the operand stack, and
  it emits `if`/`else` diamonds where LLVM emits flat `block` + `br_if`;
  GOT reads that do not fold stay `global.get`. #term("wpeep") and the
  waffle patches recover part of it. Size matters for speed too: V8's own
  wasm inlining has a per-module budget, so growth anywhere in a module can
  turn off inlining in a hot kernel (`docs/history/2026-10-20-wasm-open.md`).
]
