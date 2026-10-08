#!/usr/bin/env bash
# wasm32 smoke test: core + compiler_builtins + tests/wasm all go through the
# pliron backend, are linked by tools/pliron-wasm-ld (no wasm-ld/LLVM), and run in node.
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$PWD
cargo build -q --release --manifest-path tools/pliron-wasm-ld/Cargo.toml
export RUSTFLAGS="-Zcodegen-backend=${BACKEND:-$ROOT/target/debug/librustc_codegen_pliron.so} -Clinker=$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld"
export CARGO_TARGET_DIR=$ROOT/target/wasm
# cargo doesn't track the backend dylib; start clean so core is rebuilt by the current backend.
rm -rf "$CARGO_TARGET_DIR"
(cd tests/wasm && cargo build -q --release --target wasm32-unknown-unknown \
  -Zbuild-std=core,compiler_builtins -Zbuild-std-features=compiler-builtins-mem)
node tests/wasm/run.mjs "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/wasmtest.wasm"
# std (alloc, HashMap, fmt, dlmalloc + memory.grow) on wasm32-unknown-unknown
(cd tests/wasm/std && cargo build -q --release --target wasm32-unknown-unknown -Zbuild-std=std,panic_abort)
node tests/wasm/run_std.mjs "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/wasmstdtest.wasm"
# std on wasm32-wasip1: wasi-libc is replaced by tools/pliron-wasi-libc (pure Rust).
(cd tools/pliron-wasi-libc && CARGO_TARGET_DIR=$CARGO_TARGET_DIR/wasi-libc cargo build -q --release \
  --target wasm32-wasip1 -Zbuild-std=core,panic_abort)
WASI_LIBC=$CARGO_TARGET_DIR/wasi-libc/wasm32-wasip1/release
ln -sf libpliron_wasi_libc.a "$WASI_LIBC/libc.a"
(cd tests/wasm/wasi && RUSTFLAGS="$RUSTFLAGS -Clink-self-contained=no -Lnative=$WASI_LIBC" \
  cargo build -q --release --target wasm32-wasip1 -Zbuild-std=std,panic_abort)
node --no-warnings tests/wasm/run_wasi.mjs "$CARGO_TARGET_DIR/wasm32-wasip1/release/wasmwasitest.wasm"
