#!/usr/bin/env bash
# fuzz.sh [SEED] [COUNT] [--scalar] — generate random loop kernels, build them with the
# pliron backend and with stock LLVM (+simd128 unless --scalar), and require
# identical results (see tests/wasm/fuzz/gen.py).
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
SEED=${1:-1}
COUNT=${2:-40}
FEAT="-Ctarget-feature=+simd128"
[ "${3:-}" = "--scalar" ] && FEAT=""
cargo build -q --release --manifest-path tools/pliron-wasm-ld/Cargo.toml
LD="$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld$exe"
BE="${BACKEND:-$ROOT/target/debug/$lib}"
# The dialect->waffle path has no vector lowering; the CLIF path
# (full clif-opt pipeline -> waffle) does. Default to it; override with
# PLIRON_WASM_CLIF=0 to exercise the dialect path.
export PLIRON_WASM_CLIF="${PLIRON_WASM_CLIF:-1}"
W=$ROOT/target/wasm-fuzz
mkdir -p "$W/crate"
cat > "$W/crate/Cargo.toml" <<EOF
[package]
name = "wasmfuzz"
version = "0.1.0"
edition = "2024"
[lib]
crate-type = ["cdylib"]
path = "lib.rs"
[profile.release]
panic = "abort"
opt-level = 3
[workspace]
EOF
python3 tests/wasm/fuzz/gen.py "$SEED" "$COUNT" "$W/crate/lib.rs" 2>/dev/null || python tests/wasm/fuzz/gen.py "$SEED" "$COUNT" "$W/crate/lib.rs"
rm -rf "$W/p"
(cd "$W/crate" && \
  RUSTFLAGS="-Zcodegen-backend=$BE -Clinker=$LD $FEAT ${PLIRON_RUSTFLAGS:-}" CARGO_TARGET_DIR=$W/p \
  cargo build -q --release --target wasm32-unknown-unknown \
    -Zbuild-std=core,compiler_builtins -Zbuild-std-features=compiler-builtins-mem 2>&1 | grep -E "^error|^wvec: vectorized|^pliron-wasm" | head -400) || true
if [ ! -f "$W/s/wasm32-unknown-unknown/release/wasmfuzz.wasm" ] || [ "${KEEP_STOCK:-0}" != 1 ]; then
  rm -rf "$W/s"
  (cd "$W/crate" && RUSTFLAGS="$FEAT" CARGO_TARGET_DIR=$W/s \
    cargo build -q --release --target wasm32-unknown-unknown \
      -Zbuild-std=core,compiler_builtins -Zbuild-std-features=compiler-builtins-mem 2>&1 | grep -E "^error" -A8 | head -30) || true
fi
node tests/wasm/fuzz/fuzz.mjs "$W/p/wasm32-unknown-unknown/release/wasmfuzz.wasm" \
  "$W/s/wasm32-unknown-unknown/release/wasmfuzz.wasm"
