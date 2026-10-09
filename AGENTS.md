# Agent instructions

## Layout

`rustc_codegen_pliron` is a rustc codegen backend:
rustc MIR → rustc_codegen_ssa → pliron LLVM dialect → Cranelift →
cranelift-object (+ rsasm for `asm!`/`global_asm!`) → system linker.

- `src/lower.rs` — object lowering; `internal_fns` selects `CallConv::Tail`
  for local functions
- `src/eh/mod.rs` — `.eh_frame` / LSDA emission; Mach-O needs
  `__TEXT,__gcc_except_tab`, `ARM64_RELOC_POINTER_TO_GOT` personality relocs,
  and extern symbol refs (ld64 reads CFI relocs by symbol index)
- `src/objmerge.rs` — splices rsasm output into the dest object; rsasm emits
  the destination's binary format, so flags/relocs pass through verbatim
- `src/asm.rs` — emits ELF- or Mach-O-flavored assembly per target
- `src/hot.rs` — PLIRON_HOT thunks, x86-64 ELF asm only

## Build / test

Pinned toolchain: `rust-toolchain.toml` (`nightly-2026-10-06`, needs
`rustc-dev`, `rust-src`, `llvm-tools-preview`).

- `cargo build` → `target/debug/librustc_codegen_pliron.{so,dylib}`
- `./test.sh` — smoke tests (nostd, std, unwind, asm, proc-macro, wasm if
  node is installed); `--sysroot` also rebuilds std via `-Zbuild-std`;
  `--mold` cross-links the std test to `*-linux-musl` with mold (PATH or a
  sibling `mold` checkout built with `cargo build --release`) and runs it
  under docker/qemu when available — the pure-Rust link path, since mold is
  ELF-only (no Mach-O)
- `harness/run.py [--tier 0|1] [--accept]` — runs suites (smoke with
  stock-rustc output diff, object determinism, ui) and fails on regressions
  against `harness/expectations/`; plan and status in `test-harness.md`
- `tests/ui_run_pass.py [RUST_CHECKOUT] [FILTER]` — runs directive-free
  `//@ run-pass` tests from a rustc checkout (defaults: `$RUST_CHECKOUT`,
  then a `rust` dir next to this repo, else `~/work/rust`)

## Platform notes

- aarch64: `CallConv::Tail` can't take sret params; lower.rs keeps sret
  functions on the platform ABI. `ArgumentPurpose::StructArgument` (byval) is
  unsupported by cranelift-aarch64, but rustc never emits on-stack indirect
  args for AAPCS64.
- The asm test covers x86-64 and aarch64; PLIRON_HOT is x86-64/ELF only.
- Self-hosting: `selfhost/setup.sh [RUST_CHECKOUT] [WORKTREE]` creates a
  worktree at the pinned nightly commit, applies `bootstrap.patch`, and
  rsyncs this repo into `compiler/rustc_codegen_pliron`. Defaults: sibling
  `rust`/`rust-sh` dirs (or `~/work/rust`/`~/work/rust-sh` on the Linux box).
  On Linux it appends a `[target.*] linker = cc-wild` stanza; on macOS the
  system `ld` is used. `selfhost/bench.sh` is Linux-only.
