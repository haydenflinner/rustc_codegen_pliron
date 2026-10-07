# rustc_codegen_pliron

An out-of-tree rustc codegen backend with no LLVM:

```
rustc MIR → rustc_codegen_ssa → pliron LLVM dialect → Cranelift → cranelift-object → system linker
```

[pliron](https://github.com/vaivaswatha/pliron) is vendored in `vendor/` with its LLVM (llvm-sys) features disabled.
`src/lower.rs` lowers the pliron LLVM dialect to Cranelift IR.

## Status (x86_64 Linux)
- `no_std` and `std` programs compile and run.
- `-Zbuild-std`: core, alloc and std build with this backend, and the resulting binary runs.
- `asm!`/`global_asm!`: each asm block becomes an out-of-line wrapper (allocator adapted from rustc_codegen_cranelift).
  The system `as` assembles the wrappers and `ld -r` merges them into the CGU object.
- `llvm.*` `link_name` intrinsics: a few have asm implementations. The rest are weak stubs that trap only if they are called.
- Not done yet: optimizations, debuginfo, LTO, unwinding (use `panic=abort`), and targets other than x86_64.

## Usage
```
cargo build
rustc -Zcodegen-backend=$PWD/target/debug/librustc_codegen_pliron.so main.rs
./test.sh            # nostd/std/asm tests against the prebuilt sysroot
./test.sh --sysroot  # also rebuild core/alloc/std with this backend
```
Uses the nightly pinned in `rust-toolchain.toml`. It needs `rustc-dev` because the backend links against rustc_private crates.
