#import "../pliron-spec.typ": *

#chapter("front", 1)
= From MIR to the LLVM dialect

The front end is the rustc-facing glue in `src/`: the driver in `lib.rs`, the
`Builder` and `CodegenCx` that implement `rustc_codegen_ssa`'s traits, and the
type, ABI, constant, intrinsic, SIMD and `asm!` translation behind them. It is
the first stage of #refr("model.pipeline"): `rustc_codegen_ssa` walks each
function's #term("mir") and calls the builder, and the builder emits pliron's
#term("llvm-dialect") into one module per #term("cgu"). What LLVM would carry
as attributes and metadata goes into a #term("front-state"). The front end
also configures the Cranelift ISA for the session. It does no optimization of
its own beyond a few call-ABI rewrites; the CGU worker pool, cross-CGU copies
and facts belong to the chapter on codegen units, and object writing and
`asm!` splicing to the chapter on native objects.

#chapter-glance()

== The driver

#r("front.entry", title: "Entry point and trait set", level: "must",
   refines: "model.backend", checked: "test.sh std")[
  The dylib MUST export `__rustc_codegen_backend`, returning a backend whose
  `name()` is `"pliron"` and that implements `CodegenBackend`,
  `ExtraBackendMethods` and `WriteBackendMethods`. Everything rustc asks of a
  backend goes through those three traits; the backend reads no MIR itself.
]

#r("front.cgu.order", title: "One CGU, one pliron module, deterministic order", level: "must",
   refines: ("model.pipeline", "model.det"))[
  `compile_codegen_unit` MUST build each CGU into a fresh pliron `Context`
  and module. It first predeclares every mono item, in rustc's
  `items_in_deterministic_order`, then defines them in the same order, then
  adds rustc's entry wrapper. Cross-CGU body copies, when enabled, are
  predeclared before and defined after the CGU's own items, as `Internal`
  functions. The finished module then goes either to the CGU worker pool or
  straight to `finish_module`.
]

#r("front.finish", title: "Finishing a module", level: "must",
   refines: "model.pipeline")[
  `finish_module` MUST run three phases in this order: the pre-pass
  (optional IR print, and at `-O0` the always-inline pass for
  `#[inline(always)]` callees); the #term("iropt") passes, only when rustc's
  opt level is not `-O0`; and lowering plus object emission. The pool path
  runs the same phases on worker threads.
]

#r("front.emit", title: "What --emit produces", level: "must",
   refines: "model.backend")[
  For each CGU the backend MUST write the object file when rustc asks for
  one. `--emit=llvm-ir` MUST write the module's #term("llvm-dialect") text as
  pliron prints it, taken before the IR passes run. `--emit=asm` and
  `--emit=llvm-bc` produce nothing.
]

#llvm(kind: "gap", summary: [`--emit=llvm-ir` prints pliron's dialect before optimization; `--emit=asm` and `--emit=llvm-bc` produce no output.])[
  Stock rustc writes LLVM IR after the LLVM pass pipeline, and an assembly
  listing from the target machine. Here the IR text is the pliron dialect
  as the builder produced it, which no LLVM tool reads, and assembly is not
  emitted. To see the final code, disassemble `--emit=obj`; to see the
  optimized form, use the `PLIRON_DUMP_OPT` and `PLIRON_CLIF` dumps.
]

#r("front.lto", title: "LTO capabilities declared at init", level: "must",
   refines: "model.backend")[
  `init` MUST declare ThinLTO unsupported, so that `-Clto=thin` falls back
  to fat LTO and rustc never runs thin-local LTO. The one exception is
  `PLIRON_XFACTS_ALL=1` without an explicit `-Clto=thin`, which uses
  thin-local LTO only as a barrier between CGUs. Real cross-crate ThinLTO
  input MUST be a fatal error, not a silent link.
]

#llvm(kind: "gap", summary: [No ThinLTO; fat LTO links objects without cross-crate optimization.])[
  Stock rustc runs ThinLTO across CGUs at `-O` by default and across crates
  with `-Clto=thin`. Here neither happens. Fat LTO (`lto.rs`, described with
  native objects) is a relocatable link of the crate's and upstream objects,
  so an LTO build behaves like a non-LTO one.
]

#r("front.trace", title: "Diagnostic knobs install one subscriber", level: "must", tier: 2,
   refines: "model.diag-observe")[
  `init` MUST install a `tracing` subscriber only when `PLIRON_TRACE` or a
  variable in `KNOB_TARGETS` is set. Each `KNOB_TARGETS` row
  `(variable, target, level)` adds `target=level` to the `PLIRON_TRACE`
  filter (for example `PLIRON_STATS` gives `pliron::stats=info`). Output goes
  to stderr without levels, targets or timestamps. With no such variable
  set, no subscriber exists and diagnostic sites cost one atomic load.
]

== Target configuration

#r("front.target.features", title: "The effective target feature set", level: "must",
   refines: "model.targets")[
  `target_config` MUST compute the crate's target features once. On
  x86-64 and aarch64 with an OS, a feature is on when the arch baseline has
  it (`fxsr`, `sse`, `sse2`, `x87`; `neon`) or the `-Ctarget-cpu` (default:
  the spec's cpu) implies it, except that the target spec's `features`
  string decides alone for a feature it names (its last entry wins). rustc's
  `internal_target_features` helper then applies `-Ctarget-feature` and the
  implied-feature closure. On wasm32 the set comes from the wasm target
  features and decides whether vectors are native (#refr("front.simd.native")).
  Other architectures get the empty set.
]

#r("front.target.cpu", title: "CPU names and native", level: "must", tier: 2,
   refines: "front.target.features")[
  `-Ctarget-cpu` names MUST map to features through the `cpu_features`
  table: the x86-64 psABI levels `x86-64-v2` to `x86-64-v4` and the Intel and
  AMD names in each class, and Apple `apple-a12` onward and `apple-m1` to
  `apple-m4`. A name not in the table MUST mean the arch baseline.
  `-Ctarget-cpu=native` MUST probe the host with
  `is_x86_feature_detected!` or `is_aarch64_feature_detected!`.
]

#llvm(kind: "gap", summary: [Unknown `-Ctarget-cpu` names silently mean the baseline; no CPU scheduling model.])[
  LLVM knows every CPU name it accepts, rejects unknown ones with a warning,
  and tunes instruction scheduling per CPU. Here only the names in the
  table add features, an unknown name adds none without a warning, and
  Cranelift has no per-CPU tuning. `CodegenBackend::target_cpu` reports
  `generic`.
]

#r("front.target.isa", title: "ISA flags follow the feature set", level: "must",
   refines: "model.targets", depends: "front.target.features")[
  `build_isa` MUST turn on each Cranelift ISA flag whose rustc feature is in
  the effective set: on aarch64 `lse`, `dotprod`, `i8mm`, `fp16`, `bti`; on
  x86-64 `sse3` to `sse4.2`, `popcnt`, `bmi1`, `bmi2`, `lzcnt`, `cmpxchg16b`,
  and, unless `PLIRON_X64_VEX=0`, the VEX family (`avx`, `avx2`, `fma`,
  `avx512*`, `avxvnni`). On x86-64 the wide-vector ceiling for loop
  vectorization is 512 bits under `avx512f`, 256 under `avx2`, else 128.
  Lookup failure for the target triple (an architecture the dylib was not
  built for) MUST be a fatal error.
]

#drift[
  `build_isa` first tries to enable the x86 flags (`has_sse3`, ...) on the
  shared-settings builder, under a comment saying that otherwise everything
  compiles to the SSE2 baseline. Those names are not shared settings, so
  each call fails and the result is ignored (`let _ =`). The flags that take
  effect are the ones set later on the ISA builder from
  `sess.internal_target_features`.
]

#r("front.target.fnfeat", title: "One ISA per crate", level: "must",
   refines: "model.equiv", see: "front.target.isa")[
  Every function of a crate MUST be compiled for the crate-wide flag set of
  #refr("front.target.isa"). `#[target_feature(enable = ..)]` on a function
  does not widen it; only `asm!` register classes consult a function's own
  features. Code inside such a function is still correct: `simd_*`
  intrinsics lower within the crate-wide ISA, and `llvm.*` intrinsics follow
  #refr("front.intrinsic.llvm").
]

#llvm(kind: "gap", summary: [`#[target_feature(enable)]` does not change code generation for that function.])[
  LLVM compiles each function with its own `target-features` attribute, so
  an AVX2 kernel behind `is_x86_feature_detected!` gets AVX2 instructions
  even in a baseline build. Here it gets the crate-wide ISA, and the
  `llvm.x86.*` intrinsics it calls are emulated or stubbed. Runtime
  dispatch is correct but does not speed anything up; build with
  `-Ctarget-cpu` or `-Ctarget-feature` to get the wider ISA.
]

#r("front.target.settings", title: "Fixed ISA settings", level: "must", tier: 2,
   refines: ("model.abi", "front.target.isa"))[
  `build_isa` MUST set: `is_pic` true except on COFF, which has no GOT;
  `tls_model` `elf_gd`, `macho` or `coff` by object format, with `elf_ie`
  on x86-64 ELF under `-Ztls-model=initial-exec` or `local-exec`
  (`PLIRON_TLS_IE=0` keeps general-dynamic); `enable_llvm_abi_extensions`
  and `enable_multi_ret_implicit_sret`; inline stack probes
  (`probestack_strategy=inline`) on x86-64, aarch64 and riscv64;
  `opt_level` `speed_and_size`, or `none` at `-O0` or under
  `PLIRON_COMPILE_EGRAPH=0`; the register-allocation checker under
  `PLIRON_RA_CHECK=1`; the Cranelift verifier in debug builds of the dylib.
]

#llvm(kind: "choice", summary: [Stack probes are always inline, as rustc asks of LLVM with `probe-stack=inline-asm`.])[
  Large frames probe each guard page with inline code. No
  `__rust_probestack` or `__cranelift_probestack` helper is called.
]

#r("front.target.fp", title: "Frame pointers", level: "must",
   refines: "model.abi", see: "front.target.isa")[
  The backend MUST keep frame pointers when `-Cforce-frame-pointers` asks
  for them, when the module needs tail calls, or under `PLIRON_OMIT_FP=0`.
  Otherwise it MUST let Cranelift omit them, even where the target spec's
  default is `Always`: unwinding uses `.eh_frame` or `.xdata`, not the frame
  chain.
]

#llvm(kind: "choice", summary: [The target spec's default frame-pointer policy is not followed; only `-Cforce-frame-pointers` forces them.])[
  Stock rustc passes the spec's policy (`Always` on windows-msvc, `NonLeaf`
  on Apple targets) to LLVM as a function attribute. Here Cranelift may
  omit them unless the user asks; on x86-64 `rbp` is then free for register
  allocation, and profilers that walk frame chains instead of reading
  unwind tables see shorter stacks.
]

#r("front.target.f16", title: "No reliable f16 or f128", level: "must",
   refines: "model.equiv")[
  `target_config` MUST report `has_reliable_f16`, `has_reliable_f16_math`,
  `has_reliable_f128` and `has_reliable_f128_math` as false on every target,
  so `core` and `std` take their fallback paths behind
  `cfg(target_has_reliable_f16)` and friends.
]

#llvm(kind: "gap", summary: [`target_has_reliable_f16`/`f128` are false everywhere.])[
  The LLVM backend reports them true where LLVM's lowering and the platform
  libraries agree (for example f16 on aarch64). Programs that gate f16 or
  f128 code on these cfgs take the fallback here.
]

== Types and calling conventions

#r("front.types.layout", title: "Layouts become dialect types as in cg_llvm", level: "must",
   refines: "model.abi")[
  The backend type of a rustc layout MUST follow rustc_codegen_llvm's
  `type_of`: a `Scalar` is its integer, float or pointer type; a
  `ScalarPair` a two-field struct; a `SimdVector` a fixed vector; a
  `Memory` layout with arbitrary fields a struct with explicit integer-array
  padding, packed when a field's effective alignment is below its ABI
  alignment; a primitive or union `Memory` layout a struct holding one
  padding array of the layout's size. `bool` is `i1` as an immediate and
  `i8` in memory. Scalable vectors are a fatal error.
]

#r("front.abi.fnsig", title: "Function signatures follow rustc's FnAbi", level: "must",
   refines: "model.abi")[
  `fn_decl_backend_type` MUST map each `PassMode` the way the LLVM backend
  does: an indirect return becomes a leading pointer parameter and a `void`
  result; `Pair` two parameters; `IndirectUnsized` a pointer and its
  metadata; `Cast` its `i32` padding then the cast type; `Ignore` nothing;
  variadic functions keep only their fixed parameters. Alongside, the
  front end MUST record one extension per backend parameter (`Zext`, `Sext`,
  `SRet`, or `ByVal(size)` for `IndirectMode::OnStack`) for lowering to
  apply.
]

#r("front.abi.facts", title: "ABI facts are forwarded, never strengthened", level: "must",
   refines: ("model.equiv", "model.abi"))[
  The front end MUST forward rustc's ABI attributes as side-table facts and
  MUST NOT claim more than rustc does. A pointer parameter is frozen
  (readable for its pointee size) only when rustc marks it `noalias`,
  `readonly` and `nonnull` with a nonzero pointee size; a nullable reference
  such as `Option<&T>` gets `noalias` but no dereferenceable size. Deduced
  `captures(none)` and read-only facts apply only to by-value (`Indirect`)
  arguments of Rust-ABI calls to known items. A call whose return layout is
  uninhabited is marked as not returning; `#[cold]` and `!can_unwind` are
  recorded per function.
]

#r("front.fn.attrs", title: "Per-function attributes", level: "must",
   refines: "model.abi", checked: "test.sh fnalign")[
  `predefine_fn` MUST record, per symbol: `#[inline(never)]`,
  `#[inline(always)]` (and forced inlining), `#[inline]` hints, ODR linkage,
  `#[rustc_align(N)]` as the function's object alignment, and the
  foreign-item symbol aliases rustc lists for externally implementable
  items, each emitted as an extra symbol at the aliasee's address.
]

#r("front.abi.internal", title: "Internal functions use Cranelift's tail convention", level: "must",
   refines: "model.abi", checked: "test.sh rusttailcc")[
  Every #term("front-internal-fn") MUST use Cranelift's `Tail` calling
  convention unless `PLIRON_TAILCC=0`. A function whose Rust ABI is
  `extern "tail"` MUST use `Tail` whether or not it is internal, as MUST
  every call through an `extern "tail"` function pointer. On aarch64 a
  function that returns through an sret pointer MUST keep the platform
  convention, since Cranelift's aarch64 `Tail` has no struct return.
  Functions reachable from outside the CGU keep the platform convention.
]

#llvm(kind: "choice", summary: [Internal functions get Cranelift's `tail` convention (8+8 return registers), like LLVM promoting internal functions to `fastcc`.])[
  LLVM's GlobalOpt may switch an internal function to `fastcc`. Here the
  rule is fixed and module-local: no escape, no invoke, not variadic. The
  `tail` convention returns up to eight integer and eight float registers,
  which #refr("front.abi.sret2reg") relies on.
]

#r("front.abi.sret2reg", title: "Small sret returns move to registers", level: "must",
   refines: ("model.abi", "model.off-switch"), depends: "front.abi.internal")[
  For an internal function whose return goes through an sret pointer and
  whose return type flattens into at most eight integer and eight float
  scalar leaves, the front end MUST rewrite the signature to return those
  leaves as a struct. The callee writes into a local slot and loads the
  leaves at each return; each caller stores the leaves into the slot it
  used to pass. After an invoke the stores go at the head of a new
  normal-edge block, never after the invoke in its own block. The rewrite is
  skipped if any call site's function type differs from the callee's.
  `PLIRON_SRET2REG=0` turns it off.
]

#llvm(kind: "extra", summary: [Internal functions returning small aggregates through sret are rewritten to return them in registers.])[
  LLVM keeps the platform ABI's sret for any aggregate over two registers,
  and only argument promotion and inlining remove the memory round trip.
  Here the slot is gone from the signature and SROA usually promotes both
  ends.
]

#r("front.abi.variadic", title: "C-variadic definitions take a hidden buffer", level: "must",
   refines: "model.abi", checked: "test.sh cvariadic")[
  A Rust-defined C-variadic function MUST receive its variadic arguments
  through a #term("front-va-buf"): callers pack them into a stack buffer and
  pass its address as one hidden extra parameter. `va_start` MUST
  initialize the platform `va_list` with every register area marked
  exhausted and its stack cursor at the buffer, so both our `va_arg` and a
  foreign callee handed the `VaList` read from the buffer. Calls to foreign
  variadic callees (extern items, and `#[naked]` functions whose assembly
  reads the native convention) MUST use the platform C convention, and calls
  to variadic `extern "win64"` functions MUST use `WindowsFastcall`.
]

#llvm(kind: "gap", summary: [A C-variadic function defined in Rust cannot be called from C or other foreign code.])[
  LLVM gives Rust-defined variadic functions the platform's variadic ABI,
  so C can call them. Here they expect the hidden buffer parameter, which
  only callers built by this backend pass. Calling C variadic functions
  from Rust, and forwarding a `VaList` to `vprintf`-style functions, work.
]

#r("front.abi.va-layout", title: "va_list and slot layout", level: "must", tier: 2,
   refines: "front.abi.variadic")[
  `va_start` MUST write: on x86-64 SysV, `gp_offset = 48`,
  `fp_offset = 304`, and the buffer address to `overflow_arg_area` (+8) and
  `reg_save_area` (+16); on non-Apple AAPCS64, the buffer to `__stack` (+0)
  and zero to `__gr_offs` (+24) and `__vr_offs` (+28); elsewhere (Windows,
  Apple arm64, wasm, UEFI) the buffer pointer as the whole `va_list`.
  `va_arg` MUST read native arguments from 8-byte slots aligned to the
  argument's alignment clamped to 8..16, and wasm arguments from 4-byte
  slots at natural alignment, where aggregates other than single-element
  structs are passed as a pointer to a caller-side copy.
]

== Building the dialect

#r("front.ir.sidetables", title: "Attributes and metadata live in side tables", level: "must",
   refines: "model.pipeline")[
  The builder MUST emit only LLVM-dialect operations, and MUST record what
  LLVM keeps as attributes or metadata in the #term("front-state"):
  constants, symbol names and linkage, alloca size and alignment, volatile
  accesses, `!nonnull` loads, `bool` loads known to be 0 or 1
  (`PLIRON_BOOLRANGE`), `inbounds` GEPs, invariant loads, branch
  expectations, invoke edges, intrinsic names, and per-call facts.
  Lowering and every IR pass read these tables; the module is never printed
  and re-parsed between stages.
]

#llvm(kind: "choice", summary: [Metadata and attributes are side tables keyed by operation, not part of the IR text.])[
  In LLVM, `!nonnull`, `volatile`, `inbounds` and call attributes are part
  of each instruction and survive printing. Here they are maps from
  operations or values to facts. A pass that replaces an operation must
  move its entries (as #refr("front.abi.sret2reg") does), and
  `--emit=llvm-ir` does not show them.
]

#r("front.ir.consts", title: "Constants are placeholders with a side-table value", level: "must",
   refines: "model.equiv", depends: "front.ir.sidetables")[
  Every constant MUST be the result of an `undef` placeholder op whose
  #term("front-constval") is recorded in the side table. A constant
  allocation MUST become a packed struct of byte runs and pointer-sized
  symbol references, one per relocation. Poison constants are recorded as
  undef. String literals are deduplicated per CGU; other constant
  allocations become local globals named `__rcg_alloc.<n>`. Zero-sized
  allocations get a dangling address equal to their alignment, as in
  rustc_codegen_llvm.
]

#r("front.ir.noflags", title: "No poison-generating flags, no fast-math", level: "must",
   refines: "model.equiv")[
  The builder MUST emit integer arithmetic without `nsw`, `nuw` or `exact`,
  `zext` without `nneg`, and every floating-point operation as a plain IEEE
  operation, including rustc's `*_fast` and `*_algebraic` variants. Unsigned
  checked add and sub MUST be an operation plus an unsigned compare; the
  others use `llvm.{s,u}{add,sub,mul}.with.overflow`.
]

#llvm(kind: "choice", summary: [No `nsw`/`nuw`/`exact`/`nneg` and no fast-math flags are emitted.])[
  Stock rustc tags `unchecked_*` arithmetic with `nsw`/`nuw`, exact
  divisions with `exact`, and `*_fast`/`*_algebraic` float operations with
  fast-math flags, and LLVM exploits the resulting poison and
  reassociation. Here those operations are fully defined. This never
  changes defined behaviour; it gives up some folding, and float
  reductions written with `fadd_algebraic` are not reassociated or
  vectorized.
]

#r("front.ir.align", title: "Memory accesses carry no alignment", level: "must",
   refines: "model.equiv")[
  Loads and stores MUST be emitted without an alignment; rustc's alignment
  argument is dropped. Allocas MUST record their size and alignment in the
  side table. Every alloca is an `i8` array inserted at the front of the
  function's entry block, wherever rustc asked for it.
]

#r("front.ir.lifetime", title: "Lifetime markers at every opt level", level: "must",
   refines: "model.equiv")[
  `StorageLive` and `StorageDead` MUST reach the dialect as
  `llvm.lifetime.start` and `llvm.lifetime.end` calls at every opt level,
  `-O0` included. Passes MUST treat them as liveness hints, not as points
  past which access is undefined.
]

#llvm(kind: "choice", summary: [Lifetime markers are emitted at `-O0` too, and are hints rather than hard bounds.])[
  rustc gives LLVM lifetime markers only at `-O` or under some sanitizers,
  and LLVM treats `lifetime.end` as making later access undefined. rustc's
  MIR does not always uphold that (`docs/rustc-thorns.md` §2–3), so the
  stack-slot passes here use the markers only where the slot does not
  escape and the marker resolves to it unambiguously.
]

#r("front.ir.switch", title: "Switches", level: "must", tier: 2,
   refines: "model.off-switch")[
  A MIR switch with cases MUST become one `llvm.switch` op with case values
  masked to the operand width, unless `PLIRON_SWITCH=0`, or
  `PLIRON_SWITCH_WASM=0` on wasm, in which case it becomes a chain of
  equality compares. A switch with no cases is a branch to the default.
]

#drift[
  The comment in `Builder::switch` says wasm has no `SwitchOp` lowering and
  gets the compare chain. `PLIRON_SWITCH_WASM` defaults on, so wasm gets the
  switch op like native; wasm now lowers through the same CLIF path.
]

#r("front.ir.atomics", title: "Atomics", level: "must",
   refines: "model.equiv")[
  Atomic loads, stores, read-modify-writes, compare-exchanges and fences
  MUST map one to one onto the dialect's atomic ops with the same ordering
  (`Relaxed` becomes `monotonic`) and the system sync scope, except that
  single-thread fences keep the single-thread scope. A weak
  compare-exchange MUST be emitted as a strong one.
]

#llvm(kind: "choice", summary: [Weak `compare_exchange_weak` is emitted strong.])[
  LLVM may lower a weak cmpxchg to a single LL/SC attempt that fails
  spuriously. Here it is always strong, which is a valid refinement:
  callers loop on failure either way.
]

#r("front.ir.symbols", title: "Symbol names and linkage", level: "must", tier: 2,
   refines: ("model.abi", "front.ir.sidetables"))[
  Each symbol MUST get a pliron identifier `s<n>_<name>`, with every
  character outside `[A-Za-z0-9_]` replaced by `_` and a per-CGU counter
  `n`; the side table maps it back, and objects carry the original rustc
  symbol. Linkage MUST map as: `Internal` to local; `WeakAny`, `WeakODR`,
  `LinkOnceAny`, `LinkOnceODR` to a weak preemptible definition;
  `ExternalWeak` to import; anything else to hidden or exported by
  visibility. A foreign static with `#[linkage]` MUST become a local pointer
  global `_rust_extern_with_linkage_<crate id as 16 hex digits>_<sym>`
  holding the static's address, as in rustc_codegen_llvm.
]

#r("front.det.names", title: "Invented names are deterministic", level: "must",
   refines: "model.det")[
  Every name the front end invents (pliron identifiers, `__rcg_alloc.<n>`,
  `asm!` wrappers, COFF intrinsic stubs) MUST be built only from the
  rustc symbol, the stable crate id, the CGU name and a per-CGU counter
  advanced in the deterministic item order of #refr("front.cgu.order").
  No address, hash seed or thread id may reach a name.
]

#r("front.alloc-shim", title: "Allocator shim", level: "must",
   refines: "model.abi")[
  `codegen_allocator` MUST emit, for each allocator method, an exported
  function under rustc's internal-symbol mangling of `__rust_<method>` that
  forwards its arguments to the mangled `__rdl_<method>`, and an empty exported
  function for `NO_ALLOC_SHIM_IS_UNSTABLE`. A `Layout` argument is passed as
  two `usize` values.
]

#r("front.wasm.split", title: "Where wasm differs in the front end", level: "must",
   refines: "model.pipeline")[
  The front end MUST emit the same dialect for native and wasm targets
  except at these points, all keyed on the target spec: `black_box`
  (#refr("front.intrinsic.blackbox")); the `va_arg` slot layout
  (#refr("front.abi.va-layout")); whether vectors are native
  (#refr("front.simd.native")); the switch knob of #refr("front.ir.switch");
  recording `#[link(wasm_import_module)]`
  imports for the wasm object; and skipping speculative cross-CGU copies
  unless `PLIRON_SPEC_COPIES=1`.
]

== Exceptions

#r("front.eh.invoke", title: "Invokes and landing pads", level: "must",
   refines: "model.unwind", checked: ("test.sh unwind", "test.sh unwindstd"))[
  An invoke MUST be emitted as an ordinary call followed by a branch to the
  normal successor, with the call recorded in the side table against its
  landing block. A landing pad MUST read the in-flight exception through
  the `pliron.eh.exn` intrinsic, and `resume` MUST call `_Unwind_Resume`,
  marked as not returning. MSVC and wasm funclet pads MUST map onto the same
  scheme: cleanup and catch pads read `pliron.eh.exn`, a `cleanupret`
  without a target calls `pliron.eh.rethrow`, and a `catchswitch` branches
  to its first handler, which is a catch-all.
]

#llvm(kind: "gap", summary: [No funclet-based EH on windows-msvc; unwinding there is emulated.])[
  LLVM lowers MSVC `catchswitch`/`cleanuppad` funclets to SEH tables that
  any C++ or Rust frame can participate in. Here the funclet calls map onto
  the emulated scheme also used for wasm (described with native objects
  and wasm). Catching works when `std` and `panic_unwind` are built by this
  backend; a stock-`std` panic unwinds through our frames but our
  `catch_unwind` cannot stop it.
]

#r("front.eh.catch-unwind", title: "catch_unwind", level: "must",
   refines: "model.unwind", checked: "test.sh unwind")[
  With `panic=unwind`, the `catch_unwind` intrinsic MUST call `try_fn(data)`
  as an invoke whose landing block calls `catch_fn(data, exn)` with the
  exception from `pliron.eh.exn`, and MUST return 0 on the normal path and 1
  on the caught path. With `panic=abort` it MUST be a plain call that
  returns 0.
]

== Intrinsics

#r("front.intrinsic.rust", title: "Rust intrinsics", level: "must",
   refines: "model.equiv")[
  The front end MUST lower rustc's intrinsics as follows. `sqrt`, `fabs`,
  `floor`, `ceil`, `trunc`, `round_ties_even`, `copysign`, `fma`,
  `fmuladd`, `minimum` and `maximum` become `llvm.*` intrinsic ops; `round`,
  the trigonometric, exponential, logarithm and `pow` functions, `minnum`
  and `maxnum` become calls to the C math library by its name (`sinf`,
  `pow`, `fmin`, ...); `powi` calls compiler-rt's `__powi*f2`; f16 `pow` and
  `powi` compute in f32. `compare_bytes` with a constant length up to 32
  compares inline and calls `memcmp` only when the bytes differ; `raw_eq`
  up to 32 bytes compares inline. An intrinsic not handled here MUST fall
  back to its MIR body.
]

#r("front.intrinsic.hints", title: "Hint intrinsics", level: "must",
   refines: "model.equiv")[
  `prefetch_*` MUST emit nothing. `breakpoint` and `abort` MUST emit
  `llvm.trap`. `assume(false)` MUST end the block with `unreachable`
  (`PLIRON_UNREACH=0` drops it); other `assume`s, `expect`, and retags are
  dropped. `is_val_statically_known` MUST be `false`.
]

#llvm(kind: "gap", summary: [`breakpoint` aborts instead of trapping to a debugger; `prefetch` is a no-op.])[
  LLVM lowers `breakpoint` to `llvm.debugtrap` (`int3`, `brk #0xf000`),
  which a debugger can step over. Here it is the same trap as `abort`. LLVM
  emits real prefetch instructions; here prefetches vanish, which is
  correct but loses the hint.
]

#r("front.intrinsic.blackbox", title: "black_box is a real barrier", level: "must",
   refines: ("model.equiv", "model.parity"))[
  On native targets `black_box` MUST become a #term("front-bb"): every IR
  pass treats the call as unknown (it may write memory, capture its
  arguments and return anything), and lowering turns it into the CLIF
  `black_box` op, which emits no instructions. On wasm it MUST be a store
  to a fresh stack slot and a volatile reload, with a second volatile copy
  for aggregates. Neither form may be simplified to the identity.
]

#llvm(kind: "choice", summary: [`black_box` matches LLVM's barrier strength: an opaque call on native, a stack round trip on wasm.])[
  LLVM uses an empty inline-asm with a memory clobber, which on wasm
  becomes a store and reload. The wasm path copies that on purpose: an
  identity would let V8 fold through it and make wasm benchmarks look
  better than they are. The native path is as opaque as LLVM's asm but
  costs no instructions.
]

#r("front.intrinsic.blackbox.names", title: "Barrier symbols", level: "must", tier: 2,
   refines: "front.intrinsic.blackbox")[
  A barrier on an immediate MUST call `__pliron_bbv_<tag>`, declared
  `fn(T) -> T`, where the tag is `i<bits>`, `f16`, `f32`, `f64`, `f128`,
  `p` for pointers, `v<n>x<tag>` for vectors, else `o`. A pair makes one
  call per half. A barrier on a place or a zero-sized value MUST call
  `__pliron_bbm(ptr)` with the place's address (null for zero-sized). These
  symbols are never defined.
]

#r("front.intrinsic.llvm", title: "Foreign llvm.* intrinsics in three tiers", level: "must",
   refines: "model.intrinsics", checked: "test.sh asm")[
  A call to a function whose `link_name` is `llvm.*` MUST get, in order of
  preference: an #term("front-emulated") expansion (`pshufb` 128 and 256,
  `pclmulqdq` 128/256/512, SSE/AVX/AVX-512 `cmp`, `min` and `max` on ps and
  pd, the `sha256` message and round ops, `vcvtps2ph.128`); else an
  out-of-line assembly body with the intrinsic's semantics (x86 `xgetbv`,
  `rdtsc`, `pause`, `vzeroupper`, `vzeroall`, fences; aarch64 barriers,
  crypto, CRC32, `frint*` rounding and a table of NEON ops; any
  `llvm.prefetch*` as a plain return); else a #term("stub") that traps.
]

#r("front.intrinsic.stub", title: "Stub symbols and bodies", level: "must", tier: 2,
   refines: ("model.intrinsics", "front.intrinsic.llvm"))[
  The out-of-line body for intrinsic `name` MUST be named
  `__pliron_<name>` with every non-alphanumeric character replaced by `_`,
  emitted once per CGU into the module's assembly: on ELF weak and hidden in
  its own `.text.<sym>` section, on Mach-O `.weak_definition` and
  `.private_extern`. On COFF, which has no weak symbols without COMDAT, it
  MUST be a global named
  `__pliron_<name>_<crate id hex>_<cgu>_<n>`. A trap body MUST be `ud2` on
  x86-64 and `brk #0x1` on aarch64. On wasm, calls to `__pliron_llvm_*`
  are matched by the wasm emitter instead.
]

#drift[
  README.md calls the fallback bodies "weak stubs"; on COFF they are
  ordinary globals with per-CGU unique names. `docs/platforms.md` states
  this correctly.
]

#r("front.simd.native", title: "SIMD: native vectors or lanes", level: "must",
   refines: ("model.equiv", "model.off-switch"))[
  A `simd_*` intrinsic on #term("front-native-vec", [native vectors]) MUST
  become vector ops: integer and float arithmetic, splats
  (`pliron.vsplat`), compares (`pliron.vcmp.<op>.<s|u|f>`), selects
  (`pliron.vbitselect`) and bitmasks (`pliron.vhigh_bits`). Every other
  `simd_*` intrinsic, and every one on a non-native vector, MUST be
  expanded lane by lane with `extractelement` and `insertelement`.
  `PLIRON_SIMD=0` makes no vector native, and neither is any vector on wasm
  without `simd128`. Ill-typed arguments MUST produce rustc's
  `InvalidMonomorphization` diagnostics, as the LLVM backend does.
]

#llvm(kind: "gap", summary: [Vectors wider than the native shapes, and many `simd_*` ops, are scalarized in the front end.])[
  LLVM keeps every vector operation whole and legalizes it for the target.
  Here only the listed operations on native shapes stay whole; the rest
  become per-lane scalar code, which the CLIF passes may or may not
  re-vectorize.
]

== Inline assembly

#r("front.asm.wrapper", title: "asm! becomes an out-of-line wrapper", level: "must",
   refines: ("model.no-llvm", "model.equiv"), checked: "test.sh asm")[
  Each `asm!` block MUST become an #term("front-asm-wrapper") named
  `__pliron_asm_<crate id hex>_<cgu>_<n>`, with registers and stack slots
  allocated by the allocator adapted from rustc_codegen_cranelift. The call
  site MUST store the inputs into a 16-byte-aligned stack buffer, call the
  wrapper with its address, and copy the outputs from the buffer into their
  places; a `noreturn` block is followed by `unreachable`. The wrapper MUST
  save and restore clobbered registers around the template. Its text is
  appended to the module's assembly, which #term("rsasm") assembles in
  process; splicing it into the object is described with native objects.
]

#llvm(kind: "choice", summary: [`asm!` is a call to an out-of-line function with a memory round trip; asm options do not enable optimization.])[
  LLVM inlines the template at the use site and allocates its operand
  registers along with the surrounding code. Here every block costs a
  call, a store and load per operand, and saved clobbers. `pure`, `nomem`,
  `readonly` and `preserves_flags` are not used to optimize around the
  call.
]

#drift[
  The module comment of `src/asm.rs` says the asm text is assembled by the
  system assembler and merged in `lib.rs`. The code assembles it with
  rsasm in `pliron-clif-lower/src/objmerge.rs`; no external assembler runs
  (#refr("model.no-llvm")).
]

#r("front.asm.limits", title: "What asm! cannot do", level: "must",
   refines: ("model.equiv", "model.targets"))[
  `asm!` on an architecture other than x86-64 or aarch64, and `asm goto`
  (label operands), MUST be fatal errors naming the pliron backend. A block
  with `may_unwind` is emitted like any other: its catch funclet is
  ignored, so an exception from the template is not caught by the
  enclosing landing pad.
]

#llvm(kind: "gap", summary: [No `asm!` on wasm, no `asm goto`, and `may_unwind` asm cannot unwind into a landing pad.])[
  LLVM supports wasm inline assembly, `asm goto` via `callbr`, and
  unwinding out of `may_unwind` blocks into the invoke's landing pad.
]

#r("front.asm.syntax", title: "Template text", level: "must", tier: 2,
   refines: ("model.abi", "front.asm.wrapper"))[
  On x86-64, templates MUST be assembled in Intel syntax with
  `.intel_syntax noprefix` and back to `.att_syntax` around them, unless
  `att_syntax` is given. `$$` MUST become `$` in AT&T text and be removed
  in Intel text. Symbol operands and naked-function labels MUST carry the
  object format's global prefix (`_` on Mach-O). `global_asm!` text MUST
  be appended to the module's assembly with its `const` and `sym` operands
  substituted.
]

== What the front end does not do

#r("front.unsupported", title: "Unsupported constructs stop the build", level: "must",
   refines: ("model.equiv", "model.targets"))[
  A construct the front end cannot lower MUST stop compilation; it MUST NOT
  emit code. This covers scalable vectors and `vscale`, `type_checked_load`
  (virtual-function elimination), `ppc_fp128`, `simd_*` intrinsics with no
  lowering, and the `asm!` cases of #refr("front.asm.limits").
]

#note[
  Scalable vectors, unsupported `simd_*` intrinsics and the `asm!` cases
  are rustc diagnostics. `vscale`, `type_checked_load` and `ppc_fp128`
  stop with a panic, which rustc reports as an internal compiler error.
]

#r("front.ignored", title: "Instrumentation requests are ignored", level: "must",
   refines: "model.equiv")[
  `-Cinstrument-coverage` MUST produce no counters (`add_coverage` is
  empty), and `-Zsanitizer` MUST add no instrumentation: rustc still links
  the sanitizer runtime. MIR retags emit nothing. None of these produce a
  warning.
]

#llvm(kind: "gap", summary: [No coverage instrumentation and no sanitizer instrumentation, without a warning.])[
  The LLVM backend inserts coverage counters and ASan, MSan, TSan or HWASan
  checks. Here a `-Zsanitizer=address` build only gets the runtime's
  interceptors (`malloc`, `free`, `memcpy`), so heap misuse through those
  is caught but direct out-of-bounds loads are not
  (`docs/rustc-thorns.md` §4). UI tests under `sanitizer/` are skipped in
  the harness expectations.
]

#r("front.const.align", title: "Read-only data gets LLVM-like alignment", level: "must",
   refines: ("model.abi", "model.equiv"))[
  In native objects, an immutable global without an explicit
  `#[link_section]` MUST be aligned to at least 16 bytes when larger than 16
  bytes, to its size when that is a power of two up to 16, and otherwise to
  its declared alignment.
]

#llvm(kind: "choice", summary: [Read-only data is over-aligned the way LLVM's mergeable constant sections are.])[
  The minimum would be the declared alignment. LLVM places constants in
  sections aligned to the constant's size up to 16, and crates that
  reinterpret a `&[u8]` static as wider integers depend on that by
  accident; this matches it.
]
