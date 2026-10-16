#!/usr/bin/env bash
# bench.sh [--simd] [--keep] [reps] [filter] — build tests/wasm/bench with the pliron
# backend and stock LLVM and compare kernel times in node. --simd enables
# -Ctarget-feature=+simd128 for both.
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
FEAT=""; KEEP=0
while [[ "${1:-}" == --* ]]; do
  case "$1" in --simd) FEAT="-Ctarget-feature=+simd128" ;; --keep) KEEP=1 ;; esac
  shift
done
REPS=${1:-2000}
ONLY=${2:-}
cargo build -q --release --manifest-path tools/pliron-wasm-ld/Cargo.toml
LD="$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld$exe"
BE="${BACKEND:-$ROOT/target/debug/$lib}"
tag=scalar; [ -n "$FEAT" ] && tag=simd
P=$ROOT/target/wasm-bench/pliron-$tag
S=$ROOT/target/wasm-bench/stock-$tag
[ "$KEEP" = 1 ] || rm -rf "$P"
(cd tests/wasm/bench && \
  RUSTFLAGS="-Zcodegen-backend=$BE -Clinker=$LD $FEAT ${PLIRON_RUSTFLAGS:-}" CARGO_TARGET_DIR=$P \
  cargo build -q --release --target wasm32-unknown-unknown \
    -Zbuild-std=core,compiler_builtins -Zbuild-std-features=compiler-builtins-mem)
[ -f "$S/wasm32-unknown-unknown/release/wasmbench.wasm" ] || (cd tests/wasm/bench && \
  RUSTFLAGS="$FEAT" CARGO_TARGET_DIR=$S \
  cargo build -q --release --target wasm32-unknown-unknown \
    -Zbuild-std=core,compiler_builtins -Zbuild-std-features=compiler-builtins-mem)
ls -l "$P/wasm32-unknown-unknown/release/wasmbench.wasm" "$S/wasm32-unknown-unknown/release/wasmbench.wasm" | awk '{print $5, $NF}'
node tests/wasm/bench.mjs "$P/wasm32-unknown-unknown/release/wasmbench.wasm" \
  "$S/wasm32-unknown-unknown/release/wasmbench.wasm" "$REPS" "$ONLY"
