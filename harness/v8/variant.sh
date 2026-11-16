#!/usr/bin/env bash
# variant.sh NAME [PLIRON_X=v ...] — build bench/wasm with the debug
# backend dylib (PLIRON_BACKEND, PLIRON_WASM_LD override it and the linker)
# and the given env into target/v8prof/NAME.wasm, std at one codegen unit
# like bench/wasm/run.sh (fresh target dir, removed after).
# VARIANT_KEEP=1 keeps the target dir target/v8prof/wb-NAME and reuses it: the
# backend flag is the keyed path of harness/backend_path.py (dylib contents
# + the PLIRON_* of this call), so cargo recompiles when either changed and
# does nothing when neither did; units of earlier backends are removed.
# Compare: node bench/wasm/run.mjs <llvm.wasm> a.wasm b.wasm --layouts=3
set -eu
H=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$H/../.." && pwd); OUT=$ROOT/target/v8prof; mkdir -p "$OUT"
name=$1; shift
keep=${VARIANT_KEEP:-0}
T=$OUT/wb-$name; [ "$keep" = 1 ] || rm -rf "$T"
# The helper runs under the same PLIRON_* as the build below.
BE=$(env "$@" "$ROOT/harness/backend_path.py" \
  --dylib "${PLIRON_BACKEND:-$ROOT/target/debug/librustc_codegen_pliron.dylib}" --gc "$T")
cd "$ROOT/bench/wasm"
env "$@" CARGO_TARGET_DIR="$T" RUSTC_ICE="${RUSTC_ICE:-$ROOT/target/ices}" \
  RUSTFLAGS="-Zcodegen-backend=$BE -Clinker=${PLIRON_WASM_LD:-$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld}" \
  cargo build -q --release --target wasm32-unknown-unknown -Zbuild-std=std,panic_abort \
  --config 'profile.release.package."*".codegen-units=1'
cp "$T/wasm32-unknown-unknown/release/wasmbench.wasm" "$OUT/$name.wasm"; [ "$keep" = 1 ] || rm -rf "$T"
echo "$OUT/$name.wasm"
