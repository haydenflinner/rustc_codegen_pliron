#import "../pliron-spec.typ": *

#chapter("model", 0)
= Model

rustc_codegen_pliron is a #term("backend") for rustc: rustc hands it a crate's
#term("mir") one #term("cgu") at a time, and it hands back object files that a
system linker can link. It does that without LLVM. This chapter fixes the
contract — what goes in, what comes out — and the properties that hold under
every configuration. It is the root of the trace: every later chapter refines
requirements stated here, and nothing here refines anything above it.

#chapter-glance()

== What the backend is

#r("model.backend", title: "A rustc codegen backend", level: "must")[
  The backend MUST be a dynamic library that rustc loads with
  `-Zcodegen-backend=<path>` and drives through the `rustc_codegen_ssa`
  traits, on the nightly toolchain pinned in `rust-toolchain.toml`. It
  MUST NOT require a patched rustc: the same library loads into a stock
  nightly, and into a #term("stage")-1 or stage-2 compiler built from the
  same commit.
]

#r("model.no-llvm", title: "No LLVM anywhere", level: "must",
   checked: "Cargo.lock has no llvm-sys")[
  Neither the backend nor any tool it runs MUST link or invoke LLVM. The
  vendored #term("pliron") is built with its `llvm-sys` feature off;
  assembly is assembled in-process by #term("rsasm"); wasm objects are linked
  by #term("pliron-wasm-ld"), not `wasm-ld`. The only external program in the
  native path is the system linker (or #term("wild")) that rustc itself
  invokes.
]

#llvm(kind: "choice", summary: [No LLVM in the dylib or the toolchain path: no `llvm-sys`, no GNU `as`, no `ld -r`, no `wasm-ld`.])[
  Stock rustc links `librustc_codegen_llvm`, which carries all of LLVM. Here
  the instruction selector and register allocator are #term("cranelift")'s,
  `asm!` goes through rsasm, and wasm structuring goes through
  #term("waffle"). `llvm.*` intrinsics that crates name with `link_name` are
  emulated or stubbed (#refr("model.intrinsics")).
]

#r("model.pipeline", title: "One pipeline, two ends", level: "must")[
  Every function MUST pass through the same stages in this order:
  + `rustc_codegen_ssa` builds it in the #term("llvm-dialect") of pliron;
  + the #term("iropt") passes optimize it as pliron IR;
  + #term("lowering") turns it into #term("clif");
  + the #term("clif-pipeline") optimizes the CLIF, one #term("step") at a time;
  + for native targets, Cranelift compiles it to machine code and
    `cranelift-object` writes the object; for wasm targets,
    the CLIF is translated to waffle IR and emitted as a wasm object.
  No stage may be skipped for one target and not the other, except that the
  last stage differs.
]

#r("model.targets", title: "Supported targets", level: "must")[
  The backend MUST produce working code for `aarch64-apple-darwin`,
  `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
  `wasm32-unknown-unknown` and `wasm32-wasip1`. Native code generation is
  for the host architecture: Cranelift is built with `host-arch` only, and
  asking for another architecture is a clear error, not wrong code.
]

== What it must preserve

#r("model.equiv", title: "Same behaviour as stock", level: "must",
   checked: ("harness/run.py --suite ui", "harness/crates.py", "selfhost/verify.sh", "harness/run.py --suite fuzz"))[
  For every program whose behaviour Rust defines, the executable built with
  this backend MUST behave as the one built by #term("stock") rustc + LLVM
  from the same source and flags: the same output, exit status, panics and
  panic messages. Behaviour that Rust leaves undefined carries no
  obligation.
]

#llvm(kind: "gap", summary: [Stack frames are larger than LLVM's (2–10× in rustc's parser), so very deep recursion overflows sooner.])[
  #refr("model.equiv") is about defined behaviour. Stack depth is a resource,
  not behaviour, and frames are still larger than LLVM's: under a
  pliron-built stage-2 rustc, six deep-recursion UI tests overflow the
  stack that pass under stage 1. The parity card tracks it as `STK`.
]

#r("model.abi", title: "Links with LLVM-built code", level: "must",
   checked: ("test.sh (prebuilt sysroot)", "test.sh proc_macro"))[
  Objects from this backend MUST link and interoperate with objects and
  rlibs built by stock rustc for the same target: the prebuilt stock
  `std` links into programs built here, proc macros built here load into a
  stock rustc, and every symbol that code outside the #term("cgu") can reach
  MUST use the symbol name, calling convention and data layout stock rustc
  gives it.
]

#r("model.intrinsics", title: "Foreign intrinsics never miscompile", level: "must")[
  A function that names an `llvm.*` intrinsic through `link_name` MUST either
  get a lowering with LLVM's semantics or a #term("stub") that traps when
  called. It MUST NOT silently compute something else.
]

#r("model.unwind", title: "Panics unwind", level: "must",
   checked: ("test.sh unwind", "test.sh unwindstd", "tests/wasm/run.sh"))[
  With `panic=unwind`, a panic MUST run the `Drop` impls of every frame it
  leaves and stop at the innermost `catch_unwind`, on every supported
  target, including wasm (where unwinding is emulated) and Windows.
]

== What it must guarantee about itself

#r("model.det", title: "Output is a function of the inputs", level: "must",
   checked: ("harness/detcheck.py", "harness/run.py --suite determinism"))[
  Every byte the backend writes (objects, the LTO payload, emitted IR and
  assembly) MUST be a function of three inputs only: the crate's MIR with
  rustc's session options, the backend's own source revision, and the
  #term("knob", [`PLIRON_*` configuration]). Nothing else — scheduling,
  thread count, wall-clock time, memory addresses, hash seeds — may reach the
  output.
]

#r("model.dbg-neutral", title: "Debug info changes no code", level: "must",
   checked: "test.sh debuginfo")[
  With optimizations on, building with `-g` MUST produce the same machine
  code as building without it. Debug info is an observer of the optimized
  program, never an input to it.
]

#r("model.diag-observe", title: "Diagnostics only observe", level: "must")[
  Turning on a diagnostic knob (`PLIRON_TRACE`, `PLIRON_STATS`, dumps,
  `*_INFO` probes) MUST NOT change the output. A diagnostic that has to run
  an analysis says so and is kept out of measured and cached builds.
]

#r("model.bail", title: "A bail is never wrong", level: "must")[
  Every optimization MUST be sound when it gives up. Each fixpoint and walk
  has an explicit budget counted in work (operations, rounds, visited
  items), and running out of budget leaves the code unoptimized, never
  incorrect.
]

#r("model.linear", title: "Passes stay linear", level: "should")[
  Each pass SHOULD run in time linear (or $n log n$) in the size of the
  function it transforms. Stage-2 rustc functions are large enough that one
  quadratic loop costs minutes; analyses are shared and invalidated, not
  recomputed per instruction.
]

#r("model.off-switch", title: "Every pass can be turned off", level: "must")[
  Every optimization pass MUST have a #term("knob") `PLIRON_<PASS>=0` that
  disables it without a rebuild, and the output with a pass disabled MUST
  still be correct. A new pass or transform lands default-off and is turned
  on in a separate change once the #term("gate") and the #term("corpus") are
  clean.
]

#llvm(kind: "extra", summary: [Every pass has a runtime off switch, and transforms can be bisected one application at a time (`PLIRON_OPT_BISECT`) or per function (`PLIRON_OPT_FNS`).])[
  LLVM has `-opt-bisect-limit` for its pass manager; here the same budget
  also counts individual sroa splits, constant-load folds and LICM hoists,
  and per-function selection is keyed on symbol names, so it is stable
  across codegen units, processes and rebuilds.
]

== What it aims for

#r("model.parity", title: "Parity with LLVM", level: "should",
   checked: "harness/parity.py")[
  The backend SHOULD match or beat stock rustc + LLVM on six axes, weighted
  into one #term("parity-card") rating: speed of the generated code (native
  and wasm), compile time, peak compile memory, code size, stack-frame size,
  and correctness against shared test suites. A rating of 100 is parity;
  each axis rates $100 + 50 log_2 (1 slash r)$ for the ratio
  $r = "pliron" slash "LLVM"$.
]

#note[
  The parity card is the project's scoreboard, kept in `docs/PARITY.md` and
  regenerated by `harness/parity.py`. Numbers move daily; this specification
  states what is measured and how, never the current numbers.
]
