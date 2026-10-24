# rustc_codegen_pliron

An out-of-tree rustc codegen backend with no LLVM:

```
rustc MIR → rustc_codegen_ssa → pliron LLVM dialect → Cranelift → cranelift-object (+ rsasm for asm) → system linker / wild
                                                             └→ waffle → wasm (+ tools/pliron-wasm-ld)
```

[pliron](https://github.com/vaivaswatha/pliron) is vendored in `vendor/` with its LLVM (llvm-sys) features disabled.
`pliron-clif-lower/src/lower.rs` lowers the pliron LLVM dialect to Cranelift IR.
Optimizations: pliron-IR passes in `pliron-ir-opt/` (inlining, SROA, IPO summaries, ...) and ~80 CLIF passes in `pliron-clif-opt/`.

**Status and work list: [`docs/PARITY.md`](docs/PARITY.md)** (runtime and compile-time parity with LLVM, native and wasm).
Working on the code: [`AGENTS.md`](AGENTS.md).

## Usage
```
cargo build
rustc -Zcodegen-backend=$PWD/target/debug/librustc_codegen_pliron.{so,dylib} main.rs
./test.sh            # nostd/std/asm/unwind tests against the prebuilt sysroot
./test.sh --sysroot  # also rebuild core/alloc/std with this backend
```
Uses the nightly pinned in `rust-toolchain.toml`. It needs `rustc-dev` because the backend links against rustc_private crates.
```
PLIRON_DIFFCHECK=1 rustc ...   # differentially check every CLIF optimization (see AGENTS.md)
tests/ui_run_pass.py           # rustc UI run-pass slice (needs a rust checkout, default ~/work/rust)
bench/run.sh                   # criterion kernels: LLVM baseline vs pliron, with significance stats
examples/bevy-game/build.sh    # then run target/x86_64-unknown-linux-gnu/debug/pliron_bevy_game
```

## Status

Developed mainly on aarch64-apple-darwin; x86_64 Linux and x86_64 Windows (MSVC, including an MSVC-free link path — `docs/platforms.md`) also work.

- `no_std` and `std` programs compile and run.
- `-Zbuild-std=std,panic_unwind`: core, alloc and std build with this backend, and the resulting binaries run.
- Unwinding: `invoke` lowers to Cranelift `try_call`, and `.eh_frame` and LSDA are emitted (adapted from rustc_codegen_cranelift).
  `catch_unwind` and `Drop` during panics work.
- `asm!`/`global_asm!`: each asm block becomes an out-of-line wrapper (allocator adapted from rustc_codegen_cranelift).
  The wrappers are assembled in-process by [rsasm](https://crates.io/crates/rsasm) and spliced into the object (`src/objmerge.rs`).
  No GNU `as` or `ld -r` is involved.
- Linking: `test.sh` links with [wild](https://github.com/davidlattimore/wild) when it is on `PATH`. The `cc` driver still runs it.
- `llvm.*` `link_name` intrinsics: the ones reached via runtime feature detection are emulated lane-wise.
  These include pshufb, cmpps/cmppd and vcvtps2ph, plus xgetbv, rdtsc and pause in asm.
  The rest are weak stubs that trap only if called.
- rustc UI tests: `tests/ui_run_pass.py` runs the 2594 directive-free run-pass tests.
  All 2537 that pass with stock rustc on the test box also pass here.
- `examples/bevy-game`: a small bevy 0.17 2D game (sprites, text, input).
  Its target crates are compiled by this backend and linked with wild, and it runs on Vulkan (tested on Mesa lavapipe).
  `--frames N --autoplay` gives an unattended smoke run.
  `build.sh --host` also compiles all proc macros and build scripts with this backend (they load into stock rustc, against the prebuilt host std); `build.sh -Zbuild-std=std,panic_unwind` rebuilds the game's std with it. All modes render.
- Hot reload (subsecond-style, no linker in the loop): with `PLIRON_HOT=<crate>` (and `-Ccodegen-units=1`), each function of that crate is emitted as `<sym>.hot` behind a `jmp *__hot_slot.<sym>` thunk (`src/hot.rs`).
  A patch is the crate recompiled normally with `--emit=obj`. `examples/pliron-hot` (call `pliron_hot::start()`) loads it in the running process: it maps the object next to the executable, resolves symbols against the executable's own symbol table, keeps Rust statics bound to the live copies, registers `.eh_frame`, and repoints the slots of the functions whose code changed.
  `examples/bevy-game/hot.sh` is the edit loop: save `src/main.rs`, the patch builds in ~1.2s and applies in ~50ms, and ECS state is kept. `tests/hot/run.sh` is a smoke test.
  Limits: x86_64 ELF only, no new thread-locals in patches, changed struct layouts need a restart.
- Self-hosting: a pliron-built stage1 rustc builds a working stage2 compiler (`selfhost/`).
- LTO: `-Clto` / Cargo `lto = true` builds and links (`-Clto=thin` falls back to fat), but without cross-crate optimization yet: the fat-LTO module is a relocatable link (`cc -r`) of the crate's objects and every upstream rlib's (`src/lto.rs`), so the result matches a non-LTO build. Not on MSVC or wasm targets (a clear error there).
- Debug info (`-g`, ELF and Mach-O): line tables, inlined frames (rustc's and pliron's own inlining), lexical blocks, statics, and variables with their types — data-carrying enums, generics' type parameters and `usize` as LLVM describes them, so rust-lldb's formatters work. Optimized builds track variables through SSA values to location lists (registers, spill slots, constants). lldb and std's panic backtraces read it (gdb untested); optimized code is byte-identical with and without `-g`. `harness/dbgsuite.py` (rustc's tests/debuginfo) and `harness/dbgcorpus.py` measure lldb against stock. `PLIRON_DWARF=0` drops it.
- Not done yet: cross-crate (real) LTO.

## wasm32

wasm32 goes through the same FnLower + CLIF pipeline as native, then
`src/wasm.rs::clif_emit` translates the optimized CLIF to
[waffle](https://github.com/bytecodealliance/waffle) IR, which does the
reducify/stackify/localify into structured wasm. Each
codegen unit becomes a wasm "object" that imports memory, the stack pointer,
the function table and GOT-style globals, plus a `pliron.link` custom section
with data and relocations. `tools/pliron-wasm-ld` links those objects and
rlibs into one module (wasm-ld is LLVM, so it is not used).

`tests/wasm/run.sh` builds core, std (wasm32-unknown-unknown and
wasm32-wasip1, with a pure-Rust libc in `tools/pliron-wasi-libc`) and
panic=unwind (emulated EH) test crates this way and runs them in node /
wasmtime. Not done yet: rustc itself as wasm.
