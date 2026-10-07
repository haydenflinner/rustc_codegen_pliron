# rustc_codegen_pliron

An out-of-tree rustc codegen backend with no LLVM:

```
rustc MIR → rustc_codegen_ssa → pliron LLVM dialect → Cranelift → cranelift-object (+ rsasm for asm) → wild
```

[pliron](https://github.com/vaivaswatha/pliron) is vendored in `vendor/` with its LLVM (llvm-sys) features disabled.
`src/lower.rs` lowers the pliron LLVM dialect to Cranelift IR.

## Status (x86_64 Linux)
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
- Mid-level passes: a small same-CGU inliner (`src/inline.rs`, at -O; `PLIRON_INLINE=0` disables it).
- Not done yet: debuginfo, LTO, and targets other than x86_64.

## wasm32 (in progress)

`--target wasm32-unknown-unknown` lowers the pliron LLVM dialect to
[waffle](https://github.com/bytecodealliance/waffle) IR, which does the
reducify/stackify/localify into structured wasm (`src/wasm.rs`). Each
codegen unit becomes a wasm "object" that imports memory, the stack pointer,
the function table and GOT-style globals, plus a `pliron.link` custom section
with data and relocations. `tools/pliron-wasm-ld` links those objects and
rlibs into one module (wasm-ld is LLVM, so it is not used).

`tests/wasm/run.sh` builds core, compiler_builtins and a test crate this way
and runs it in node. Not done yet: std, unwinding, rustc itself as wasm.

## Usage
```
cargo build
rustc -Zcodegen-backend=$PWD/target/debug/librustc_codegen_pliron.so main.rs
./test.sh            # nostd/std/asm/unwind tests against the prebuilt sysroot
./test.sh --sysroot  # also rebuild core/alloc/std with this backend
```
Uses the nightly pinned in `rust-toolchain.toml`. It needs `rustc-dev` because the backend links against rustc_private crates.
```
tests/ui_run_pass.py           # rustc UI run-pass slice (needs a rust checkout, default ~/work/rust)
examples/bevy-game/build.sh    # then run target/x86_64-unknown-linux-gnu/debug/pliron_bevy_game
```
