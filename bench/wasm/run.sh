#!/usr/bin/env bash
# wasm leg of bench/: the kernels in bench/benches compiled to
# wasm32-unknown-unknown twice — stock rustc (LLVM + wasm-ld) and the
# pliron backend (+ pliron-wasm-ld) — then timed in node by run.mjs.
#   bench/wasm/run.sh            # all kernels
#   bench/wasm/run.sh tokenize   # subset by name filter
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$PWD

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) lib=rustc_codegen_pliron.dll ;;
  Darwin)               lib=librustc_codegen_pliron.dylib ;;
  *)                    lib=librustc_codegen_pliron.so ;;
esac

cargo build  # the backend dylib
cargo build -q --release --manifest-path tools/pliron-wasm-ld/Cargo.toml
WLD=$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) WLD=$(cygpath -m "$(cygpath -u "$WLD.exe")") ;; esac

# Cargo mangles RUSTFLAGS containing POSIX-spelled paths; spell ROOT
# Windows-style for the -Zcodegen-backend/-Clinker flags below.
WROOT=$ROOT
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) WROOT=$(cygpath -m "$ROOT") ;; esac

# LLVM: prebuilt wasm32 std, rustc's own wasm-ld invocation.
(cd bench/wasm &&
  CARGO_TARGET_DIR=$ROOT/target/wasm-bench-llvm \
  cargo build -q --release --target wasm32-unknown-unknown)

# pliron: std and the kernels all go through the backend — prebuilt std
# rlibs hold wasm-ld objects pliron-wasm-ld cannot read, so -Zbuild-std
# rebuilds them (same reason as tests/wasm/run.sh). Cargo does not track
# the backend dylib, so the target dir starts fresh.
rm -rf "$ROOT/target/wasm-bench"
(cd bench/wasm &&
  CARGO_TARGET_DIR=$ROOT/target/wasm-bench \
  RUSTFLAGS="-Zcodegen-backend=$WROOT/target/debug/$lib -Clinker=$WLD" \
  cargo build -q --release --target wasm32-unknown-unknown \
  -Zbuild-std=std,panic_abort)

exec node "$ROOT/bench/wasm/run.mjs" \
  "$ROOT/target/wasm-bench-llvm/wasm32-unknown-unknown/release/wasmbench.wasm" \
  "$ROOT/target/wasm-bench/wasm32-unknown-unknown/release/wasmbench.wasm" "$@"
