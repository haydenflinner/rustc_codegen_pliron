# Platform notes

Target-specific details moved out of AGENTS.md. Nothing here changes day to day.

## MSVC-free Windows links (PLIRON_CRTFREE)

`PLIRON_CRTFREE=1` makes the backend emit the static-CRT pieces
msvcrt/vcruntime normally supply — `mainCRTStartup`/`wmainCRTStartup`/
`WinMainCRTStartup`/`wWinMainCRTStartup` (exec) or `_DllMainCRTStartup`
(dylib), `_tls_used`+`_tls_start`/`_tls_end`/`_tls_index` (the PE TLS
directory — `_tls_start` is a plain byte in unsuffixed `.tls`, the
bookkeeping object is first input so it anchors the template),
`_pliron_tls_cbs`, `_fltused`, the `type_info` vftable slot, and
`__security_cookie`/`__security_check_cookie`/`__GSHandlerCheck` (GS is a
stub: check-cookie compares against the sentinel, the SEH handler returns
ExceptionContinueSearch). Every emitted object gets a `.drectve` with
`/DEFAULTLIB:ucrtbase /DEFAULTLIB:vcruntime140`.

Combined with synthesized import libraries, this links with zero
MSVC/WinSDK files:

    uv run --no-project tools/mkwinlibs/mkwinlibs.py <OUTDIR>
    PLIRON_CRTFREE=1 rustc -O foo.rs -Zcodegen-backend=.../rustc_codegen_pliron.dll         -Clinker=rust-lld -Lnative=<OUTDIR>

`tools/mkwinlibs/mkwinlibs.py` dumps System32 DLL export tables via
`llvm-readobj --coff-exports` and builds `.lib` files with
`rust-lld -flavor link -def:` — kernel32/ntdll/userenv/ws2_32/dbghelp/
msvcrt/ucrtbase/vcruntime140/ole32/advapi32/shell32/user32/bcrypt/
bcryptprimitives plus `synchronization.lib` (api-set forwarders). Names
unspellable in .def (C++ operators, `=X.#N` ordinal forwards) are skipped —
rustc objects never import them. Verified end-to-end: the criterion bench
builds and runs with zero VS inputs.

## Targets, cross-compiling, self-hosting

- windows-msvc (x86_64): `is_pic=false`, Cranelift `tls_model="coff"`.
  `src/eh/mod.rs` emits `.pdata`/`.xdata` (serialized `UNWIND_INFO` +
  `RUNTIME_FUNCTION`, `RelocationKind::ImageOffset` → `ADDR32NB`) instead of
  `.eh_frame`. Unwinding is *emulated* like the wasm backend: TLS `__pliron_eh`
  state word, flag checks after calls, `pliron.eh.exn`/`rethrow`, and
  `_CxxThrowException`/`_Unwind_Resume` interception. Catch works when std +
  panic_unwind are also backend-built (`-Zbuild-std`, run via
  `test.sh --sysroot`); with stock std, real SEH throws unwind *through* our
  frames (correct `.pdata`) but our `catch_unwind` can't intercept them —
  `tests/std/main.rs` gates those asserts on `--cfg pliron_stock_seh`.

  MSVC i128/f128 libcall ABI: args byval through pointers, return in xmm0 —
  `libcall()` in lower.rs adapts (`__udivti3`, `__floattisf`, `__rust_i128_*`).
  rsasm COFF: `.linkonce`, `.seh_*`, `3f@IMGREL` local-label modifiers; LLVM
  `$$` escapes become `$` (AT&T) or nothing (Intel syntax). Compiler-stub
  symbols (`__pliron_llvm_*`) get per-CGU-unique names on COFF (no weak syms).
- aarch64: `CallConv::Tail` can't take sret params; lower.rs keeps sret
  functions on the platform ABI. `ArgumentPurpose::StructArgument` (byval) is
  unsupported by cranelift-aarch64, but rustc never emits on-stack indirect
  args for AAPCS64.
- The asm test covers x86-64 and aarch64; PLIRON_HOT is x86-64/ELF only.
- Cross-target codegen: cranelift-codegen is built `host-arch`-only
  (Cargo.toml), so the dylib can't emit other architectures —
  `error: cranelift: Support for this target is disabled`. Flip to
  `all-arch` to cross-compile (x86_64-apple-darwin links via ld64 and runs
  under Rosetta; x86_64-macho `__eh_frame` uses SUBTRACTOR+UNSIGNED pairs,
  see src/eh/mod.rs). Still needs the target std component and a target
  linker (mold for ELF).
- Self-hosting: `selfhost/setup.sh [RUST_CHECKOUT] [WORKTREE]` creates a
  worktree at the pinned nightly commit, applies `bootstrap.patch`, and
  rsyncs this repo into `compiler/rustc_codegen_pliron`. Defaults: sibling
  `rust`/`rust-sh` dirs (or `~/work/rust`/`~/work/rust-sh` on the Linux box).
  On Linux it appends a `[target.*] linker = cc-wild` stanza; on macOS the
  system `ld` is used. `selfhost/bench.sh` is Linux-only.
- `selfhost/verify.sh [WORKTREE] [SUITE]` — run a compiletest suite under
  the pliron-built stage2 rustc and the LLVM-built stage1 baseline, then
  diff failures: stage2-only entries are pliron regression candidates.
  Handles Darwin's `rust-objcopy` PATH requirement and watchdog-kills
  rustc stuck idle (miscompiled-rustc deadlocks). Run it after optimizer
  changes that touch stage2 codegen.

