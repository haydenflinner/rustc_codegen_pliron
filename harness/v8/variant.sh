#!/usr/bin/env bash
# variant.sh NAME [PLIRON_X=v ...] — build bench/wasm with the debug
# backend dylib and the given env into target/v8prof/NAME.wasm, std at
# one codegen unit like bench/wasm/run.sh (fresh target dir, removed after).
# Compare: node bench/wasm/run.mjs <llvm.wasm> a.wasm b.wasm --layouts=3
set -eu
H=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$H/../.." && pwd); OUT=$ROOT/target/v8prof; mkdir -p "$OUT"
name=$1; shift
T=$OUT/wb-$name; rm -rf "$T"
cd "$ROOT/bench/wasm"
env "$@" CARGO_TARGET_DIR="$T" RUSTC_ICE="$ROOT/target/ices" \
  RUSTFLAGS="-Zcodegen-backend=$ROOT/target/debug/librustc_codegen_pliron.dylib -Clinker=$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld" \
  cargo build -q --release --target wasm32-unknown-unknown -Zbuild-std=std,panic_abort \
  --config 'profile.release.package."*".codegen-units=1'
cp "$T/wasm32-unknown-unknown/release/wasmbench.wasm" "$OUT/$name.wasm"; rm -rf "$T"
echo "$OUT/$name.wasm"
