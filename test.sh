#!/usr/bin/env bash
# Usage: ./test.sh [--sysroot]
# Builds the backend, then compiles and runs tests/{nostd,std,asm} with it.
# --sysroot additionally rebuilds core/alloc/std with the backend (-Zbuild-std).
set -euo pipefail
cd "$(dirname "$0")"
cargo build
BE="-Zcodegen-backend=$PWD/target/debug/librustc_codegen_pliron.so"
out=target/tests; mkdir -p $out
rustc $BE --edition 2024 -Cpanic=abort -Clink-arg=-lc tests/nostd/main.rs -o $out/nostd && $out/nostd
for t in std asm; do rustc $BE --edition 2024 tests/$t/main.rs -o $out/$t && $out/$t; done
if [[ "${1:-}" == --sysroot ]]; then
  (cd tests/sysroot && RUSTFLAGS="$BE" CARGO_TARGET_DIR=../../target/sysroot \
    cargo run -Zbuild-std=std,panic_abort --target "$(rustc -vV | sed -n 's/host: //p')")
fi
