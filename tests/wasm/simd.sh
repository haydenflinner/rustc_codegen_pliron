#!/usr/bin/env bash
# wasm simd128 test: build tests/wasm/simd with the pliron backend and with stock LLVM
# (both -Ctarget-feature=+simd128), then run every export in node and require the same
# results. `simd.sh --keep` reuses the previous pliron sysroot build artifacts.
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$PWD
exe=
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) ROOT=$(cygpath -m "$PWD"); exe=.exe ;; esac
case "$(uname -s)" in
  Darwin)               lib=librustc_codegen_pliron.dylib ;;
  MINGW*|MSYS*|CYGWIN*) lib=rustc_codegen_pliron.dll ;;
  *)                    lib=librustc_codegen_pliron.so ;;
esac
cargo build -q --release --manifest-path tools/pliron-wasm-ld/Cargo.toml
LD="$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld$exe"
BE="${BACKEND:-$ROOT/target/debug/$lib}"
# The dialect->waffle path has no vector lowering; the CLIF path
# (full clif-opt pipeline -> waffle) does. Default to it; override with
# PLIRON_WASM_CLIF=0 to exercise the dialect path.
export PLIRON_WASM_CLIF="${PLIRON_WASM_CLIF:-1}"
P=$ROOT/target/wasm-simd/pliron
S=$ROOT/target/wasm-simd/stock
[ "${1:-}" = "--keep" ] || rm -rf "$P"
FEAT="-Ctarget-feature=+simd128"
(cd tests/wasm/simd && \
  RUSTFLAGS="-Zcodegen-backend=$BE -Clinker=$LD $FEAT ${PLIRON_RUSTFLAGS:-}" CARGO_TARGET_DIR=$P \
  cargo build -q --release --target wasm32-unknown-unknown \
    -Zbuild-std=core,compiler_builtins -Zbuild-std-features=compiler-builtins-mem)
(cd tests/wasm/simd && \
  RUSTFLAGS="$FEAT" CARGO_TARGET_DIR=$S \
  cargo build -q --release --target wasm32-unknown-unknown \
    -Zbuild-std=core,compiler_builtins -Zbuild-std-features=compiler-builtins-mem)
node tests/wasm/run_simd.mjs \
  "$P/wasm32-unknown-unknown/release/wasmsimd.wasm" \
  "$S/wasm32-unknown-unknown/release/wasmsimd.wasm"
ls -l "$P/wasm32-unknown-unknown/release/wasmsimd.wasm" "$S/wasm32-unknown-unknown/release/wasmsimd.wasm" | awk '{print $5, $NF}'
