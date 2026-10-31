#import "../pliron-spec.typ": *

#chapter("native", 1)
= Native objects

On native targets the last stage of #refr("model.pipeline") turns each
function's optimized #term("clif") into machine code with #term("cranelift")
and writes one object file per #term("cgu"). This chapter covers everything
from there to a running program: how the object is laid out, how symbols
are bound, how panics unwind through the code (#term("native-eh-frame", [frame
descriptions]) and #term("native-lsda", [LSDAs]) on ELF and Mach-O,
#term("native-emulated-seh") on Windows), how `asm!` gets into the object
without an external assembler, how rustc's linker step sees the result,
debug information, LTO, hot reload and the native side of PGO. Everything
here happens after the #term("clif-pipeline"); nothing here may change what
a function computes.

#chapter-glance()

== Writing the object

#r("native.obj.writer", title: "One cranelift-object module per CGU", level: "must",
   refines: ("model.pipeline", "model.no-llvm"),
   checked: "test.sh std")[
  For a native target, each #term("cgu") MUST become exactly one object
  file, written by the vendored `cranelift-object` from one `ObjectModule`.
  The object format MUST be the target's own: ELF, Mach-O or COFF. Code and
  data go into the module through `cranelift-module` declarations and
  definitions; everything the module API cannot express (unwind tables,
  debug sections, aliases, assembly, the LTO payload) MUST be added to the
  finished `object::write::Object` before its bytes are produced, so no
  file is written twice and no external tool rewrites it.
]

#r("native.obj.order", title: "Post-finish steps run in a fixed order", level: "must", tier: 2,
   refines: ("native.obj.writer", "model.det"))[
  After `ObjectModule::finish`, `lower_to_object` MUST apply these steps in
  this order:
  + strip the platform prefix from symbols declared with LLVM's `\x01`
    no-mangle marker;
  + emit the merged `#[link_section]` sections (Mach-O);
  + on COFF under `PLIRON_CRTFREE=1`, add the `.drectve` section
    (#refr("native.win.crtfree"));
  + emit the unwind tables (#refr("native.eh.frames"), #refr("native.win.pdata"));
  + emit DWARF when `-g` is on and `PLIRON_DWARF` is not `0`;
  + define symbol aliases;
  + assemble and splice all pending assembly (#refr("native.asm.inproc"));
  + add the LTO payload section when rustc asked for one
    (#refr("native.lto.payload"));
  + serialize, then (Mach-O with DWARF) fix the debug sections' text
    addresses.
  Aliases come after DWARF and before the splice, so an alias can name a
  Cranelift-defined symbol but not one defined only in assembly.
]

#r("native.obj.sections", title: "Dead-strippable layout", level: "must",
   refines: "model.abi")[
  On ELF and COFF every function and every data object MUST get its own
  section, so the linker's `--gc-sections` (or `/OPT:REF`) can drop each one
  alone. On Mach-O all functions MUST share one text section and the object
  MUST set `MH_SUBSECTIONS_VIA_SYMBOLS`, because a Mach-O symbol names its
  section with an 8-bit index and per-symbol sections overflow it; ld64
  then strips dead code by symbol.
]

#r("native.obj.macho-ordinals", title: "Mach-O shares sections", level: "must", tier: 2,
   refines: "native.obj.sections")[
  On Mach-O the backend MUST NOT create one section per item where items can
  share one. All statics that name the same `#[link_section]` MUST be
  defined in one section, whose kind is the union of its members' needs
  (writable, or read-only with relocations, or read-only). Every LSDA MUST
  go into the shared read-only data section, not one `__gcc_except_tab`
  section per function.
]

#r("native.obj.linkage", title: "Symbol binding follows rustc's linkage", level: "must",
   refines: "model.abi")[
  Each symbol MUST be bound from the linkage and visibility rustc gives its
  item:
  - `Internal` becomes a local symbol;
  - `WeakAny`, `WeakODR`, `LinkOnceAny` and `LinkOnceODR` become weak
    definitions with default visibility;
  - `ExternalWeak` becomes an undefined reference;
  - every other linkage becomes a global definition, hidden when the
    visibility is hidden.
  A declaration that gains a body in this CGU MUST be emitted as a
  definition. A symbol that rustc spells with the `\x01` marker MUST appear
  in the object exactly as spelled, without the platform's global prefix
  (the `_` of Mach-O).
]

#llvm(kind: "choice", summary: [Weak and link-once items are plain weak symbols in per-symbol sections, never COMDAT groups.])[
  rustc's LLVM backend puts weak and link-once definitions into COMDAT
  groups on targets that support them. Here they are weak symbols, each in
  its own section, and the linker keeps the first definition. On COFF,
  where weak definitions need COMDAT machinery, the compiler-generated
  `llvm.*` stubs get names unique per crate and CGU instead
  (#refr("model.intrinsics")).
]

#r("native.obj.alias", title: "Aliases share their target's address", level: "must",
   refines: "model.abi")[
  A symbol alias that rustc requests (`foreign_item_symbol_aliases`) MUST
  be defined at the same section and offset as its target, with the
  target's size and kind. A weak linkage (`WeakAny`, `WeakODR`) on the
  alias MUST make it weak. A target that is local to the object MUST give
  an alias visible to the link (hidden), not a local one. An alias whose
  target this object does not define MUST NOT be emitted.
]

#r("native.obj.align", title: "Function alignment", level: "must",
   refines: ("model.abi", "model.equiv"),
   checked: "test.sh fnalign")[
  A function's start MUST be aligned to the largest of: the alignment its
  machine code needs, the minimum rustc asks for (`#[rustc_align(N)]`,
  which rustc also applies to the `dyn` shims of such a method), and the
  ISA's preferred function alignment. The last term is a vendored
  `cranelift-object` patch; without it a function lands at whatever offset
  its predecessor ended, and alignment inside the function means nothing in
  the final image.
]

#r("native.obj.loopalign", title: "Loop headers are 16-byte aligned", level: "should", tier: 2,
   refines: ("model.parity", "model.off-switch"),
   depends: "native.obj.align",
   checked: "test.sh loopalign")[
  Every #term("native-loop-header") SHOULD start on a 16-byte boundary
  within its function, padded with NOPs that run only on the fall-through
  entry, as LLVM's `.p2align 4` loop tops do. `PLIRON_ALIGN=0` MUST turn
  the padding off. The padding is a vendored `cranelift-codegen` patch in
  `VCode::emit`.
]

#drift[
  `vendor/cranelift-codegen/PLIRON_PATCH.md` (item 16) calls this an x64
  change. The code in `machinst/vcode.rs` is ISA-independent, so aarch64
  loop headers are padded too. The `loopalign` stanza checks only x86_64.
]

#r("native.obj.pic-tls", title: "Code model and TLS model per format", level: "must", tier: 2,
   refines: ("model.abi", "model.targets"))[
  The Cranelift ISA flags MUST be set per object format:
  - `is_pic` is `true`, except on COFF, which has no GOT;
  - `tls_model` is `elf_gd` on ELF, `macho` on Mach-O and `coff` on COFF;
  - on x86_64 ELF, when rustc's TLS model is `initial-exec` or
    `local-exec` (`-Ztls-model`, which bootstrap passes for rustc itself),
    `tls_model` is `elf_ie` instead: a `%fs`-relative load through a
    `R_X86_64_GOTTPOFF` relocation, with no `__tls_get_addr` call.
    `PLIRON_TLS_IE=0` keeps general-dynamic. `elf_ie` and its relocation
    are vendored patches to `cranelift-codegen-meta` and `cranelift-object`.
]

#llvm(kind: "gap", summary: [ELF TLS accesses are general-dynamic by default; LLVM picks local-exec or initial-exec in executables on its own.])[
  LLVM chooses the TLS model from the relocation model and symbol
  locality: in a position-independent executable a locally defined
  thread-local gets local-exec. Here every access is general-dynamic unless
  `-Ztls-model` asks for less. The linker can relax the canonical
  general-dynamic sequence in an executable, but the object still pays for
  it at the call site.
]

#r("native.obj.fp", title: "Frame records follow the ABI, not the target default", level: "must",
   refines: ("model.abi", "model.unwind"))[
  Unwinding MUST NOT depend on frame pointers: it reads unwind tables only
  (#refr("native.eh.frames"), #refr("native.win.pdata")). Cranelift's
  `preserve_frame_pointers` MUST be set when `-Cforce-frame-pointers` asks
  for them, when explicit tail calls are enabled (x64 `return_call` needs a
  frame pointer), or when `PLIRON_OMIT_FP=0`; otherwise leaf functions with
  no calls, clobbers, stack slots or stack arguments MAY omit the frame
  record. The target's default frame-pointer policy is not consulted.
]

#drift[
  The comment above `preserve_frame_pointers` in `build_isa` (`src/lib.rs`)
  says frame pointers "follow the target default". The code computes the
  target default but uses it only for tracing; only `-Cforce-frame-pointers`,
  tail calls and `PLIRON_OMIT_FP` decide.
]

== Unwinding on ELF and Mach-O

#r("native.eh.frames", title: "Every function has a frame description", level: "must",
   refines: "model.unwind",
   checked: ("test.sh unwind", "test.sh unwindstd", "test.sh debuginfo"))[
  On ELF and Mach-O, every function for which Cranelift produces System V
  unwind information MUST get a #term("native-eh-frame") in the object's
  `.eh_frame`, under one CIE per object, whatever the panic strategy. With
  `panic=abort` the CIE names no personality and no FDE names an LSDA; the
  frames still serve backtraces. With `panic=unwind` every FDE MUST name an
  LSDA (#refr("native.eh.lsda")) and the CIE MUST name the personality
  routine (#refr("native.eh.personality")). The emitter is adapted from
  rustc_codegen_cranelift's `debuginfo/unwind.rs`.
]

#r("native.eh.try-call", title: "invoke becomes try_call", level: "must",
   refines: ("model.unwind", "model.pipeline"),
   checked: ("test.sh unwind", "test.sh invokeblk", "test.sh unwindstd"))[
  Outside COFF, an `invoke` MUST lower to a Cranelift `try_call` (or
  `try_call_indirect`) with exactly one exception-table entry. The entry's
  tag MUST be `1` (catch) when the landing pad catches and `0` (cleanup)
  otherwise. Its edge MUST enter a cold block that binds the exception
  pointer from the call's exception payload and jumps to the landing pad.
  A function that is the direct callee of an `invoke` MUST keep the
  platform calling convention, because a `try_call` into Cranelift's tail
  convention clobbers every register.
]

#r("native.eh.lsda", title: "LSDA layout", level: "must", tier: 2,
   refines: "native.eh.frames",
   checked: ("test.sh unwind", "test.sh unwindstd"))[
  Each function's LSDA MUST be a `.gcc_except_table`-format table with the
  landing-pad base omitted, `uleb128` call-site fields, a 4-byte aligned
  type table, and these records:
  - one call-site record for every call site in the machine code, starting
    at the return address minus one, with length 1;
  - a call site with no exception handler gets landing pad 0 and no action,
    so the unwinder passes through the frame;
  - a cleanup handler gets its landing-pad offset and no action;
  - a catch handler gets its landing-pad offset and the one catch action,
    whose type entry is 0 (catch everything), encoded `udata4`.
  On ELF the LSDA is an anonymous data object in `.gcc_except_table`; on
  Mach-O it goes into the shared read-only data section
  (#refr("native.obj.macho-ordinals")).
]

#r("native.eh.personality", title: "How the CIE names the personality", level: "must", tier: 2,
   refines: ("native.eh.frames", "model.abi"),
   checked: ("test.sh unwind", "test.sh --sysroot"))[
  The #term("native-personality") MUST be the symbol rustc gives the
  `eh_personality` lang item (`eh_personality_symbol`, the mangled
  `__rustc::rust_eh_personality`), or plain `rust_eh_personality` when the
  CGU's state names none. The CIE MUST reference it as follows:
  - on ELF, indirectly through a local data word `DW.ref.<personality>`
    that holds its address, so the personality may live in another shared
    object; the word MUST NOT be zero-initialized (an unwinder cannot read
    it from `.bss`);
  - on Mach-O, directly, with a GOT relocation (`ARM64_RELOC_POINTER_TO_GOT`,
    or `X86_64_RELOC_GOT` without the pc-relative bit) so ld64 supplies the
    indirection; only the CIE's field gets this relocation, so an FDE for
    the personality itself (when building `std`) stays ordinary.
  When position-independent, FDE addresses MUST use `pcrel|sdata4`; the
  personality pointer MUST use `indirect|pcrel|sdata4` on Mach-O and on
  x86_64 (ld64 and ld64.lld require it) and `indirect|pcrel|sdata8`
  elsewhere.
]

#r("native.eh.macho-x64", title: "x86_64 Mach-O pc-relative fields", level: "must", tier: 2,
   refines: ("native.eh.frames", "model.abi"))[
  ld64's `__eh_frame` parser on x86_64 accepts only `UNSIGNED`,
  `SUBTRACTOR` and `GOT` relocations, each naming a symbol. Every
  pc-relative `.eh_frame` field MUST therefore be written as a
  `X86_64_RELOC_SUBTRACTOR` against a local label placed at the field,
  immediately followed by a `X86_64_RELOC_UNSIGNED` against the target,
  as an assembler writes `.long target - Lfield`. Relocations MUST be
  handed to the writer in descending offset order, so that after the
  writer reverses them each `SUBTRACTOR` still precedes its `UNSIGNED`. On
  all Mach-O targets, `.eh_frame` relocations MUST reference symbols, never
  the section-plus-offset form.
]

#note[
  The x86_64 Mach-O path is reached only on an x86_64 macOS host, or with
  Cranelift built `all-arch` (#refr("model.targets")). It links with ld64
  and runs under Rosetta; no `test.sh` stanza runs it on the aarch64 test
  machine.
]

== Unwinding on Windows

#r("native.win.pdata", title: "Windows unwind records", level: "must", tier: 2,
   refines: ("model.unwind", "model.abi"))[
  On COFF the backend MUST NOT emit `.eh_frame`. For each function with
  Windows x64 unwind information it MUST append that function's
  `UNWIND_INFO` to `.xdata` (4-byte aligned, under a compilation-local
  symbol `$unwind$<id>`) and a 12-byte `RUNTIME_FUNCTION` to `.pdata`:
  begin address, end address (begin plus code size) and unwind-info
  address, each a 32-bit image-relative (`IMAGE_REL_AMD64_ADDR32NB`)
  relocation. No record names an exception handler
  (`UNW_FLAG_NHANDLER`): the system unwinder can unwind through these
  frames but never stops in them.
]

#r("native.win.emulated", title: "Panics on Windows are emulated", level: "must",
   refines: "model.unwind",
   checked: "test.sh --sysroot")[
  On COFF with `panic=unwind`, unwinding MUST use #term("native-emulated-seh"):
  - the allocator module defines the #term("native-eh-word") `__pliron_eh`
    as an exported thread-local, `{ flag: i32, pad: i32, exn: ptr,
    buf: [u8; 32] }`, zero-initialized;
  - after every call that may unwind, the caller loads the flag and, when
    it is nonzero, branches to the landing pad (for an `invoke`) or to one
    shared early-return block (otherwise);
  - a throw sets the flag to 1 and returns early through that block;
  - a landing pad reads the exception pointer from offset 8 and clears
    the flag.
  An `invoke` on COFF MUST lower to an ordinary call followed by this check,
  not to `try_call`.
]

#llvm(kind: "choice", summary: [Windows panics are emulated with a thread-local flag checked after each call; no funclets, no C++ EH tables, no `__CxxFrameHandler3`.])[
  rustc's LLVM backend lowers Rust panics on `windows-msvc` to MSVC C++
  exceptions: `_CxxThrowException`, funclet-based landing pads and
  `FuncInfo` tables read by `__CxxFrameHandler3`. Cranelift has no
  funclets, so this backend unwinds the way its wasm end does: a flag in
  the #term("native-eh-word") and an early return. The cost is a load and a
  branch after every call that may unwind; the benefit is that a landing pad
  is an ordinary block of its own function.
]

#r("native.win.throw", title: "Throws and resumes are intercepted", level: "must", tier: 2,
   refines: ("native.win.emulated", "model.unwind"))[
  Under emulated SEH, lowering MUST replace these calls:
  - `_CxxThrowException(obj, info)`: copy the 24 bytes of
    `panic_unwind::seh::Exception` at `obj` into the EH word's buffer at
    offset 16, store the buffer's address as the exception pointer at
    offset 8, then throw. The copy outlives the frame that built the
    exception.
  - `_Unwind_Resume(exn)`: store `exn` at offset 8, then throw.
  - `pliron.eh.rethrow`: throw with the exception pointer already stored.
]

#r("native.win.stock-std", title: "Catching needs a backend-built std on Windows", level: "must",
   refines: ("model.unwind", "model.abi"),
   checked: ("test.sh --sysroot", "test.sh std"))[
  A Rust panic raised in code built by stock rustc (the prebuilt `std`) is
  a real SEH exception. It MUST unwind correctly through frames built here
  (#refr("native.win.pdata")), and `std::rt::lang_start`'s own catch in the
  stock `std` MUST still see it. A `catch_unwind` compiled here cannot
  catch it. Programs that need to catch panics on Windows MUST be built
  with `-Zbuild-std=std,panic_unwind`; `test.sh` builds the catching
  assertions only without `--cfg pliron_stock_seh`, which it passes on
  Windows.
]

#llvm(kind: "gap", summary: [On Windows, `catch_unwind` cannot catch a panic thrown by the prebuilt stock `std`; it needs `-Zbuild-std`.])[
  A generic `catch_unwind` is compiled into the caller's object, so its
  landing pad is an emulated flag check. A panic raised inside the stock
  `std` is a real `_CxxThrowException` that never sets the flag. `test.sh`
  skips the `unwind`, `noret`, `dseunwind`, `unwindmem`, `unwindstd`,
  `stackmove` and `resumehoist` stanzas on Windows for this reason.
  `docs/seh-design.md` describes the fix: a personality that claims SEH
  exceptions in phase 1 and resumes at the landing block.
]

#r("native.win.seh-inert", title: "The SEH personality is not wired in", level: "must", tier: 2,
   refines: ("native.win.emulated", "model.unwind"), see: "native.win.pdata",
   status: "draft")[
  `__pliron_seh_personality` (the phase-1 claim handler of
  `docs/seh-design.md`) MUST NOT be referenced by any unwind record until
  the `.xdata` handler data and the `try_call` lowering for it exist. While
  inert it is built, in the allocator module, only under an explicit
  `PLIRON_SEH` setting; default builds do not contain it.
]

#drift[
  `docs/seh-design.md`, `docs/history/2026-10-23-seh-handoff.md` and the commit that added the
  stub say it is built behind `PLIRON_SEH=1`. The code in
  `codegen_allocator` tests `env_is("PLIRON_SEH", "0")`, so it is built only
  when `PLIRON_SEH=0`. Also, `docs/platforms.md` places the unwind emitter
  at `src/eh/mod.rs`; it is `pliron-clif-lower/src/eh/mod.rs`.
]

#r("native.win.crtfree", title: "MSVC-free links", level: "may", tier: 2,
   refines: ("model.no-llvm", "model.abi"),
   checked: "test.sh cvariadic")[
  With `PLIRON_CRTFREE=1` on COFF, the allocator module MAY define the
  static-CRT symbols that `msvcrt.lib` and `vcruntime` normally supply:
  - the PE TLS directory `_tls_used` and its pieces `_tls_index`,
    `_pliron_tls_cbs`, `_tls_start` (a plain byte in an unsuffixed `.tls`
    section) and `_tls_end` (in `.tls$ZZZ`), so the merged `.tls` region is
    bracketed;
  - `_fltused`, the `type_info` vftable slot `??_7type_info@@6B@`;
  - `__security_cookie` (the MSVC default sentinel),
    `__security_check_cookie` and `__GSHandlerCheck` (which returns
    `ExceptionContinueSearch` and validates nothing);
  - for executables, `mainCRTStartup`, `wmainCRTStartup`,
    `WinMainCRTStartup` and `wWinMainCRTStartup`, each calling
    `main(0, NULL, NULL)` and then `ExitProcess`; for dynamic libraries,
    `_DllMainCRTStartup` returning 1.
  Every object MUST then carry a `.drectve` section with
  `/DEFAULTLIB:ucrtbase /DEFAULTLIB:vcruntime140`. The knob MUST stay off
  by default: against a real CRT these definitions collide with
  `msvcrt.lib`'s.
]

#llvm(kind: "extra", summary: [`PLIRON_CRTFREE=1` links `windows-msvc` programs with `rust-lld` and synthesized import libraries, with no Visual Studio or Windows SDK file.])[
  Stock rustc on `windows-msvc` needs `link.exe` (or `rust-lld`) plus the
  MSVC CRT and Windows SDK import libraries. Here the backend supplies the
  static-CRT pieces itself and `tools/mkwinlibs/mkwinlibs.py` builds the
  import libraries from the export tables of the system DLLs, so a machine
  without Visual Studio can link: `PLIRON_CRTFREE=1 rustc ...
  -Clinker=rust-lld -Lnative=<dir>`. The GS cookie check is a stub.
]

== Assembly in the object

#r("native.asm.inproc", title: "Assembly is assembled in-process", level: "must",
   refines: ("model.no-llvm", "model.abi"),
   checked: ("test.sh asm", "test.sh asmcfi", "test.sh asmextern"))[
  All assembly a CGU needs — #term("front-asm-wrapper", [`asm!` wrappers]),
  `global_asm!` text, `llvm.*` intrinsic stubs, atomic and variadic
  trampolines, #term("native-hot-thunk", [hot thunks]) and PGO
  constructors — MUST be collected into one GAS-dialect text and assembled
  once by the vendored #term("rsasm"), for the same object format as the
  destination. The result MUST be #term("native-objmerge", [spliced]) into
  the CGU's object in memory. No external assembler and no `ld -r` run. An
  assembly error MUST stop the compilation with rsasm's diagnostics and the
  assembled source.
]

#llvm(kind: "choice", summary: [`asm!` and `global_asm!` go through rsasm in-process and are spliced into the CGU object; no LLVM MC, GNU `as` or `ld -r`.])[
  LLVM assembles inline assembly with its integrated MC assembler while it
  emits the object. rustc_codegen_cranelift runs the system assembler and
  links the result with `ld -r`. This backend assembles with rsasm and
  copies sections, symbols and relocations itself. `PLIRON_ASMOBJ_DUMP=<dir>`
  keeps each assembled object for inspection.
]

#drift[
  The README places the splice in `src/objmerge.rs` and the hot thunks in
  `src/hot.rs`; both live in `pliron-clif-lower/src/`. The module comment
  of `src/asm.rs` still says the asm text "is assembled by the system
  assembler"; it is assembled by rsasm.
]

#r("native.asm.splice", title: "Splice rules", level: "must", tier: 2,
   refines: ("native.asm.inproc", "model.abi"),
   checked: ("test.sh asmcfi", "test.sh asmextern", "test.sh asm"))[
  When #term("native-objmerge", [splicing]) an assembled object, the backend MUST:
  - copy every non-empty code, data, read-only, BSS and TLS section, and
    every ELF-typed section, with its flags; also keep COFF sections whose
    name contains `$` even when empty (`.tls$`, `.CRT$` range markers and
    COMDAT groups) and Mach-O `__mod_init_func`;
  - give an x86_64 ELF `.eh_frame` the type `SHT_X86_64_UNWIND`, so the
    linker merges it with Cranelift's;
  - let an assembly definition fill an existing undefined symbol of the
    same name, keeping the existing spelling;
  - on Mach-O, drop one leading `_` from a global assembly name unless the
    name already matches a declared symbol, and keep a raw `_name` for an
    undefined reference (`bl _malloc`);
  - on Mach-O, keep section labels as real local symbols, and keep the
    `SUBTRACTOR` of every `A - B` relocation pair;
  - copy every relocation's flags unchanged; the assembler wrote the same
    format.
]

#r("native.asm.wrapper", title: "asm! wrappers in the object", level: "must",
   refines: ("model.abi", "model.equiv"),
   checked: "test.sh asm")[
  Each #term("front-asm-wrapper") MUST be a global symbol invisible
  outside the linked image: `.hidden` on ELF, `.private_extern` on Mach-O.
  On ELF it MUST have its own `.text.<name>` section and a symbol size. It
  MUST set up a frame record (`rbp` on x86_64, `fp`/`lr` on aarch64) and
  keep the buffer pointer in a callee-saved register (`rbx`, `x19`). A
  `noreturn` block MUST end in a trap (`ud2`, `brk #0x1`) after the
  template.
]

#llvm(kind: "gap", summary: [`asm!` with `options(may_unwind)` cannot unwind: the wrapper has no CFI and is reached by a plain call.])[
  LLVM emits inline assembly in the body of its function, covered by the
  function's own unwind information, and an `asm!` marked `may_unwind` can
  be an `invoke`. Here the wrapper carries no `.cfi_*` directives and its
  call is never a `try_call`, so a panic that leaves the assembly cannot
  reach a landing pad.
]

#r("native.asm.cfi", title: "CFI in global_asm! reaches the unwinder", level: "must",
   refines: ("model.unwind", "model.abi"),
   checked: "test.sh asmcfi")[
  `.cfi_*` directives in `global_asm!` MUST produce unwind records that the
  final image's unwinder finds, on ELF and Mach-O: rsasm writes
  `.eh_frame` (`__TEXT,__eh_frame` on Mach-O, with `SUBTRACTOR`+`UNSIGNED`
  pairs for pc-relative fields) and the splice keeps it intact
  (#refr("native.asm.splice")).
]

== Linking

#r("native.link.rustc", title: "rustc links", level: "must",
   refines: ("model.no-llvm", "model.abi"),
   checked: ("test.sh std", "test.sh proc_macro"))[
  The backend MUST NOT replace rustc's link step. rustc picks the linker
  and its flavor, builds the command line and runs it, exactly as for
  stock. Because assembly is already in each CGU's object, the backend
  MUST report no separate `global_asm` object. The only linker run the
  backend makes itself is the #term("native-partial-link") of
  #refr("native.lto.fat").
]

#r("native.link.wild", title: "Linking with wild", level: "may", tier: 2,
   refines: "native.link.rustc",
   checked: ("test.sh std", "test.sh --mold", "tests/hot/run.sh"))[
  On Linux, objects from this backend MAY be linked by #term("wild") through
  the `cc` driver. Stock nightly adds `-fuse-ld=lld`, under which the
  bundled `rust-lld` silently wins over any `-Clinker`, so a wild link
  MUST pass `-Clinker-features=-lld -Zunstable-options` (`test.sh` also
  passes `-Clink-self-contained=-linker` and points `cc -B` at a directory
  whose `ld` is wild). `selfhost/cc-wild` is the same driver as a
  `-Clinker`; it drops `-z text`, which wild does not know and which this
  backend's code never needs. `test.sh --mold` cross-links the `std` test
  for `*-linux-musl` with wild (lld flavor) or, without wild, mold (GNU ld
  flavor).
]

== Debug information

#r("native.dbg.format", title: "DWARF 4 per object", level: "must",
   refines: "model.abi",
   checked: ("test.sh debuginfo", "test.sh dbgnested"))[
  With `-g`, each ELF or Mach-O object MUST carry DWARF version 4, 32-bit
  format, with one compile unit named `<crate root>/@/<cgu>` (as
  rustc_codegen_llvm names it; ld64 drops the debug info of objects that
  share a `DW_AT_name`), the working directory as `DW_AT_comp_dir`,
  language Rust, and producer `rustc version <v> (rustc_codegen_pliron)`.
  It MUST hold a line table and one `DW_TAG_subprogram` per defined
  function, inside its `DW_TAG_namespace` path, with its mangled
  `DW_AT_linkage_name`; statics this object defines MUST appear as
  variables. Optimized builds MUST mark subprograms `DW_AT_APPLE_optimized`.
  On Mach-O the DWARF stays in the objects for `dsymutil` and lldb, and its
  section-relative addresses MUST be corrected after layout. COFF objects
  carry no debug information.
]

#llvm(kind: "gap", summary: [No debug information on Windows (no CodeView or PDB); on ELF and Mach-O, gdb is untested.])[
  LLVM writes CodeView for `windows-msvc` and DWARF elsewhere. This
  backend writes DWARF for ELF and Mach-O only, and with
  `is_like_msvc` columns are zero. lldb (with rust-lldb's formatters) and
  `std`'s own backtrace symbolizer are the tested readers; gdb has not been
  tried.
]

#r("native.dbg.inline", title: "Inlined frames", level: "must",
   refines: "model.abi",
   checked: "test.sh debuginfo")[
  Every machine-code range MUST map to the source location of the
  operation it came from, through the CLIF source location that lowering
  copies from the op's debug location. Code inlined by rustc's MIR inliner
  and code inlined by pliron's own inliner MUST both appear as
  `DW_TAG_inlined_subroutine` trees under the caller's subprogram, with the
  call site's location, so a backtrace through `std`'s symbolizer shows
  every inlined frame with its line at `-O0` and when optimized. Lexical
  blocks that own code MUST appear as `DW_TAG_lexical_block` with their
  ranges.
]

#r("native.dbg.vars", title: "Variables and location lists", level: "should",
   refines: "model.abi",
   checked: ("test.sh debuginfo", "harness/dbgcorpus.py"))[
  Each variable SHOULD have a location wherever stock rustc's has one:
  - at `-O0` with full debug info, variable slots MUST stay in memory
    (slot promotion skips them) and are described relative to the frame
    pointer, as LLVM's allocas are;
  - when optimizing, variables are tracked through SSA values to
    #term("native-loclist", [location lists]): a DWARF register, a spill
    slot at an offset from the CFA, or a constant (`DW_OP_constu`,
    `DW_OP_stack_value`), composed with `DW_OP_piece` when the variable is
    split. A code range where any byte of the variable has no location is
    left out, so the debugger reports the variable as unavailable rather
    than showing a wrong value.
  `PLIRON_DBGVALS=0` turns off SSA tracking; variables in promoted slots
  then read as unavailable.
]

#r("native.dbg.types", title: "Types as LLVM describes them", level: "should", tier: 2,
   refines: ("native.dbg.vars", "model.parity"),
   checked: ("test.sh debuginfo", "harness/dbgsuite.py", "harness/dbgcorpus.py"))[
  Type descriptions SHOULD match the shapes LLVM gives rustc's
  non-MSVC debug info, because rust-lldb's formatters match on them:
  - a data-carrying enum is a structure holding a `DW_TAG_variant_part`
    keyed by the tag's offset and size, with one variant per enum variant;
    a niche layout's untagged variant is the default variant;
  - a fieldless enum is a `DW_TAG_enumeration_type`;
  - `usize`/`isize` are `DW_TAG_typedef`s of the fixed-width integers;
  - a generic type lists its type parameters as
    `DW_TAG_template_type_parameter`;
  - structs, tuples, closures and fat pointers are structures; unions put
    every field at offset 0.
  A type not yet described MUST still get its name and size.
  `harness/dbgsuite.py` runs rustc's `tests/debuginfo` under lldb for both
  compilers, and `harness/dbgcorpus.py` compares every variable at every
  breakpoint against stock.
]

#r("native.dbg.neutral", title: "-g changes no optimized code", level: "must",
   refines: "model.dbg-neutral",
   checked: "test.sh debuginfo")[
  With optimizations on, the text section of an object built with `-g`
  MUST be byte-identical to the one built without it. Debug locations and
  value labels ride along as side tables and CLIF source locations; no
  pass may read them to make a decision. At `-O0` with full debug info,
  keeping variable slots in memory (#refr("native.dbg.vars")) is the one
  permitted code change.
]

#r("native.dbg.off", title: "PLIRON_DWARF=0 drops the sections", level: "must", tier: 2,
   refines: ("model.dbg-neutral", "model.off-switch"))[
  `PLIRON_DWARF=0` MUST drop every debug section from the object while the
  `-g` tables are still built, so the code is the same as with DWARF on and
  only the debug sections differ.
]

== Link-time optimization

#r("native.lto.fat", title: "Fat LTO is a relocatable link", level: "must",
   refines: ("model.abi", "model.equiv"),
   checked: "test.sh lto")[
  Under `-Clto` (fat), rustc leaves upstream Rust objects off the link line
  and expects the backend's one LTO module to carry the whole crate graph,
  except `compiler_builtins` and `#![no_builtins]` crates. The backend MUST
  produce that module as one object: a #term("native-partial-link") of
  this crate's CGU objects (in memory, or from a serialized payload when
  incremental compilation saved them), the allocator shim, and every Rust
  object member of each LTO-participating rlib, as its `lib.rmeta-link`
  member names them. The relocatable link MUST use the session's own
  linker with its flavor's `-r` spelling (on Darwin also `-arch`, the
  deployment target and `-keep_private_externs`). A single input MUST be
  renamed into place instead. On Darwin with debug info the inputs MUST
  outlive the final link, because the debug map names them.
]

#llvm(kind: "gap", summary: [`-Clto` builds and links, but performs no cross-crate optimization; the output matches a non-LTO build.])[
  LLVM's fat LTO merges the crate graph's IR and optimizes it as one
  module. Here the LTO module is native code merged by the linker, so no
  function is inlined across a crate boundary that was not already
  inlinable (generics and `#[inline]`). The final link's `--gc-sections` or
  `-dead_strip` still drops what nothing references. The payload's version
  field (#refr("native.lto.payload")) is where serialized pliron IR for
  real LTO would go.
]

#drift[
  #refr("model.no-llvm") and the README say no `ld -r` runs and that the
  only external program is the linker rustc invokes. Under `-Clto` the
  backend itself runs the session's linker (`cc -r`, `ld -r`, or `lld -r`)
  for the relocatable link. Assembly never needs it.
]

#r("native.lto.thin", title: "ThinLTO falls back to fat", level: "must",
   refines: ("model.backend", "model.abi"))[
  The backend MUST NOT claim ThinLTO support for a session with
  `-Clto=thin`, so rustc falls back to fat LTO (and says so) instead of
  handing it other crates' modules. ThinLTO is claimed only under
  `PLIRON_XFACTS_ALL=1`, and then only for thin-local LTO; a ThinLTO run
  with upstream rlibs, or with reused incremental modules, MUST be a fatal
  error.
]

#r("native.lto.refuse", title: "Unsupported LTO is a clear error", level: "must",
   refines: ("model.targets", "model.abi"))[
  Fat LTO MUST be a fatal error, never a wrong link, when:
  - the target is wasm;
  - the linker flavor has no relocatable link (MSVC `link.exe`); the error
    names the target;
  - an LTO rlib member is LLVM bitcode (an upstream crate built by the LLVM
    backend with `-Clinker-plugin-lto`);
  - the relocatable link fails; the error shows the command and its output.
  A rustc built in-tree with this backend (the self-host fork) MUST NOT
  claim fat LTO at all, so it drops LTO requests as the fork's knob says.
]

#r("native.lto.payload", title: "The LTO payload", level: "must", tier: 2,
   refines: ("native.lto.fat", "model.det"),
   checked: "test.sh lto")[
  The #term("native-lto-payload") MUST be the 8 bytes `PLIRLTO\0` followed
  by a postcard encoding of `{ version: u32, objects: [{ name, bytes }] }`;
  the version MUST be bumped on any layout change. When rustc asks a CGU's
  object to carry bitcode (`-Cembed-bitcode`, `-Clinker-plugin-lto`,
  `--emit=llvm-bc`), the object MUST carry a payload with no objects (the
  object itself is the code) in a section every linker discards:
  `.pliron.lto` with `SHF_EXCLUDE` on ELF, `.pliron.lto` with
  `IMAGE_SCN_LNK_REMOVE | IMAGE_SCN_LNK_INFO` on COFF, and
  `__LLVM,__pliron_lto` on Mach-O. No linked image may contain it. A
  serialized LTO input with a bad magic or another version MUST be a fatal
  error naming the file; an unreadable embedded payload MUST be ignored,
  because the object's native code is always a valid input.
]

== Hot reload

#r("native.hot.slot", title: "Hot functions go through slots", level: "must", tier: 2,
   refines: ("model.abi", "model.equiv"),
   checked: "tests/hot/run.sh")[
  With `PLIRON_HOT=<crate>`, while compiling that crate, every function
  with a body whose symbol is patchable MUST be emitted as follows:
  - the body is defined as the hidden symbol `<sym>.hot`;
  - `<sym>` is a #term("native-hot-thunk") in its own section
    `.text.<sym>`: one `jmp *__hot_slot.<sym>(%rip)`. It keeps the
    function's binding: weak for weak linkage, global for exported, global
    and hidden otherwise;
  - the #term("native-hot-slot") `__hot_slot.<sym>` is a hidden, 8-byte
    aligned word in `.data.__hot_slot.<sym>`, initialized to the address of
    `<sym>.hot`.
  Every reference — direct calls, vtables, function pointers — MUST name
  `<sym>`, so repointing the slot redirects all of them. A symbol is
  patchable when it is not `main` and contains only ASCII letters, digits
  and `_`; other functions are compiled normally. Symbol names must match
  between the base build and the patch, so both MUST use
  `-Ccodegen-units=1`. While `PLIRON_HOT` is set, cross-CGU body copies are
  off.
]

#r("native.hot.loader", title: "Patches load without a linker", level: "may",
   refines: ("model.backend", "model.unwind"),
   checked: "tests/hot/run.sh")[
  A hot patch is the same crate recompiled normally (without
  `PLIRON_HOT`) with `--emit=obj`. The loader `examples/pliron-hot`
  (`pliron_hot::start()`, watching `PLIRON_HOT_DIR`) MAY apply it in the
  running process: it maps the object near the executable, resolves
  undefined symbols against the executable's own symbol table and then
  `dlsym`, binds the patch's writable statics to the live copies so state
  survives, registers the patch's `.eh_frame` with `__register_frame`, and
  repoints the slots of exactly the functions whose code changed since the
  previous object (`base.ref` for the first patch). Unchanged functions keep
  running the old code; the patch reaches them through their thunks.
]

#llvm(kind: "extra", summary: [Hot reload in a running process, with no linker in the loop: `PLIRON_HOT` thunks plus an in-process object loader.])[
  LLVM-based Rust hot reloading (subsecond and similar) relinks a patch
  dynamic library each time. Here the base build routes every function
  through a slot, and a patch is one relocatable object the running
  program maps, relocates and splices in itself, with Rust statics kept and
  panics able to unwind through the new code.
]

#r("native.hot.limits", title: "Hot reload limits", level: "must",
   refines: "model.targets",
   checked: "tests/hot/run.sh")[
  `PLIRON_HOT` MUST be used only for x86_64 ELF targets: the thunks are
  x86_64 GAS text with ELF section directives. A patch MUST NOT add
  thread-locals, and a change to a struct's layout needs a restart; the
  loader cannot migrate live state.
]

#llvm(kind: "gap", summary: [Hot reload is x86_64 ELF only, and nothing rejects `PLIRON_HOT` on another target up front.])[
  No target check guards `PLIRON_HOT`. On Mach-O or aarch64 the thunk text
  fails to assemble, which stops the compilation with an assembler panic
  (#refr("native.asm.inproc")) instead of a diagnostic about the knob.
]

== Profile-guided optimization, object side

#r("native.pgo.ctor", title: "Profile counters register through constructors", level: "must", tier: 2,
   refines: ("model.abi", "model.no-llvm"),
   checked: "test.sh pgo")[
  Under `PLIRON_PGO`, each CGU's counter header MUST be registered at
  program start by a constructor written as assembly and spliced
  (#refr("native.asm.inproc")): a weak hidden stub that loads the header's
  address and tail-calls `__pliron_prof_register`, plus a pointer to the
  stub in `.init_array` (ELF), `__DATA,__mod_init_func` (Mach-O) or
  `.CRT$XCU` (COFF). Profile names go in `pliron_prof_names`, or
  `pliron_pnames` on Mach-O, whose section names are capped at 16 bytes.
  `__pliron_prof_register` MUST be an ordinary undefined import, resolved
  from `libpliron_prof_rt.a`; linking without the runtime MUST fail with an
  undefined-symbol error that names it.
]

#llvm(kind: "choice", summary: [PGO uses its own `no_std` runtime and one self-merging `.prof` file instead of `profiler_builtins` and `llvm-profdata`.])[
  LLVM's instrumentation PGO needs compiler-rt's `profiler_builtins` at run
  time and `llvm-profdata merge` between runs. Here the runtime
  (`tools/pliron-prof-rt`) is a small `no_std` archive that links into
  programs that already carry a `std`, and `tools/pliron-prof` merges and
  dumps the raw output. The profile format and its consumers are described
  in `docs/pgo.md`.
]
