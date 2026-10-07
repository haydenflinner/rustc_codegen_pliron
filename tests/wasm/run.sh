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
