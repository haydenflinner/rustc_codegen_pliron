#import "../../pliron-spec.typ": *

== Words from Native objects

#gl("native-objmerge", "Splice", qual: "objmerge", see: ("native.asm.inproc", "native.asm.splice"), src: "pliron-clif-lower/src/objmerge.rs")[
  Copying the sections, symbols and relocations of an object that #term("rsasm")
  assembled into the CGU's own object, in memory. It replaces the
  assemble-then-`ld -r` step a C toolchain would use.
]

#gl("native-eh-frame", "Frame description", qual: "FDE, .eh_frame", see: ("native.eh.frames",), src: "pliron-clif-lower/src/eh/mod.rs")[
  The DWARF call-frame record that tells an unwinder how to restore the
  caller's registers at each address of one function. All FDEs of an object
  share one CIE and live in its `.eh_frame` (`__TEXT,__eh_frame` on Mach-O).
]

#gl("native-lsda", "LSDA", qual: "language-specific data area", see: ("native.eh.lsda",), src: "pliron-clif-lower/src/eh/gcc_except_table.rs")[
  The per-function table, named by its FDE, that maps each call site to a
  landing pad and an action. The personality routine reads it during a
  panic to decide whether a frame catches, cleans up, or is skipped.
]

#gl("native-personality", "Personality routine", see: ("native.eh.personality",), src: "pliron-clif-lower/src/eh/mod.rs")[
  The function the unwinder calls for every frame with an LSDA. For Rust it
  is the `eh_personality` lang item, `rust_eh_personality` in `std`, which
  rustc mangles into its internal namespace; the CIE names it.
]

#gl("native-unwind-info", "Windows unwind records", qual: ".pdata/.xdata", see: ("native.win.pdata",), src: "pliron-clif-lower/src/eh/mod.rs")[
  The x64 Windows equivalent of FDEs: one `RUNTIME_FUNCTION` entry in
  `.pdata` per function, pointing at an `UNWIND_INFO` record in `.xdata`
  that describes its prologue.
]

#gl("native-emulated-seh", "Emulated SEH", qual: "COFF emulated EH", see: ("native.win.emulated",), src: "pliron-clif-lower/src/lower.rs")[
  Unwinding on `windows-msvc` without the system exception dispatcher: a
  throw sets a thread-local flag and returns early, and every caller checks
  the flag after the call. The same scheme the wasm backend uses.
]

#gl("native-eh-word", "EH word", qual: "__pliron_eh", see: ("native.win.emulated", "native.win.throw"), src: "src/lib.rs")[
  The 48-byte thread-local `__pliron_eh` that drives emulated SEH: a flag at
  offset 0, the exception pointer at offset 8, and a 32-byte buffer at
  offset 16 that holds a copy of the thrown exception object.
]

#gl("native-crtfree", "CRT-free link", qual: "PLIRON_CRTFREE", see: ("native.win.crtfree",), src: "src/lib.rs, tools/mkwinlibs")[
  A `windows-msvc` link that needs no file from Visual Studio or the
  Windows SDK: the backend emits the static-CRT symbols itself and the
  import libraries are synthesized from the system DLLs.
]

#gl("native-loop-header", "Loop header", see: ("native.obj.loopalign",), src: "vendor/cranelift-codegen/src/machinst/vcode.rs")[
  A block that some branch reaches backwards in the final emission order,
  that is, the target of a back edge.
]

#gl("native-loclist", "Location list", see: ("native.dbg.vars",), src: "pliron-clif-lower/src/dwarf.rs")[
  A DWARF variable location that changes with the program counter: a list
  of code ranges, each with the register, frame slot or constant that holds
  the variable there.
]

#gl("native-partial-link", "Relocatable link", qual: "-r", see: ("native.lto.fat",), src: "src/lto.rs")[
  A linker run with `-r` that merges objects into one object instead of an
  executable. Relocations and undefined symbols stay for the final link.
]

#gl("native-lto-payload", "LTO payload", see: ("native.lto.payload",), src: "src/lto.rs")[
  This backend's stand-in for LLVM bitcode: a versioned, magic-prefixed
  record that rustc stores and hands back for `-Clto`. It holds no IR yet.
]

#gl("native-hot-thunk", "Hot thunk", see: ("native.hot.slot",), src: "pliron-clif-lower/src/hot.rs")[
  Under `PLIRON_HOT`, the code at a function's own symbol: one indirect jump
  through the function's hot slot. Every caller, vtable and function pointer
  reaches the body through it.
]

#gl("native-hot-slot", "Hot slot", see: ("native.hot.slot", "native.hot.loader"), src: "pliron-clif-lower/src/hot.rs")[
  The writable 8-byte word `__hot_slot.<sym>` that holds the address of a
  function's current body. A hot-reload loader repoints it at new code.
]
