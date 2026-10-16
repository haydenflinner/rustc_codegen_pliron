#!/usr/bin/env bash
# real.sh [--simd] [--native-only|--wasm-only] — compare pliron and stock LLVM on the
# std-using programs in tests/wasm/real: run time, compile time and wasm size.
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
FEAT=""; MODE=both
for a in "$@"; do case "$a" in --simd) FEAT="-Ctarget-feature=+simd128" ;; --native-only) MODE=native ;; --wasm-only) MODE=wasm ;; esac; done
BE="${BACKEND:-$ROOT/target/debug/$lib}"
# The dialect->waffle path has no vector lowering; the CLIF path
# (full clif-opt pipeline -> waffle) does. Default to it; override with
# PLIRON_WASM_CLIF=0 to exercise the dialect path.
export PLIRON_WASM_CLIF="${PLIRON_WASM_CLIF:-1}"
W=$ROOT/target/wasm-real
mkdir -p "$W/crate"
cp tests/wasm/real/real.rs tests/wasm/real/lib.rs tests/wasm/real/main.rs "$W/crate/"
secs() { python -c "import time,sys;print(f'{time.time()-float(sys.argv[1]):.1f}')" "$1"; }
now() { python -c "import time;print(time.time())"; }
if [ "$MODE" != native ]; then
  cargo build -q --release --manifest-path tools/pliron-wasm-ld/Cargo.toml
  LD="$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld$exe"
  cat > "$W/crate/Cargo.toml" <<EOT
[package]
name = "wasmreal"
version = "0.1.0"
edition = "2024"
[lib]
crate-type = ["cdylib"]
path = "lib.rs"
[profile.release]
panic = "abort"
opt-level = 3
[workspace]
EOT
  rm -rf "$W/p" "$W/s"
  t0=$(now)
  (cd "$W/crate" && RUSTFLAGS="-Zcodegen-backend=$BE -Clinker=$LD $FEAT ${PLIRON_RUSTFLAGS:-}" CARGO_TARGET_DIR=$W/p \
    cargo build -q --release --target wasm32-unknown-unknown -Zbuild-std=std,panic_abort 2>&1 | grep -E "^error" -A6 | head -20) || true
  echo "wasm build (std + program) pliron: $(secs "$t0") s"
  t0=$(now)
  (cd "$W/crate" && RUSTFLAGS="$FEAT" CARGO_TARGET_DIR=$W/s \
    cargo build -q --release --target wasm32-unknown-unknown -Zbuild-std=std,panic_abort 2>&1 | grep -E "^error" -A6 | head -20) || true
  echo "wasm build (std + program) llvm:   $(secs "$t0") s"
  ls -l "$W/p/wasm32-unknown-unknown/release/wasmreal.wasm" "$W/s/wasm32-unknown-unknown/release/wasmreal.wasm" | awk '{print "size", $5, $NF}'
  node tests/wasm/real.mjs "$W/p/wasm32-unknown-unknown/release/wasmreal.wasm" "$W/s/wasm32-unknown-unknown/release/wasmreal.wasm"
fi
if [ "$MODE" != wasm ]; then
  echo "--- native ($(rustc -vV | sed -n 's/^host: //p'))"
  cd "$W/crate"
  t0=$(now); rustc -O --edition 2024 main.rs -o real_llvm$exe 2>/dev/null; echo "native compile llvm:   $(secs "$t0") s"
  t0=$(now); rustc -O --edition 2024 -Zcodegen-backend="$BE" main.rs -o real_plir$exe 2>/dev/null; echo "native compile pliron: $(secs "$t0") s"
  ls -l real_llvm$exe real_plir$exe | awk '{print "size", $5, $NF}'
  echo "program        llvm-ms  pliron-ms  ratio"
  ./real_llvm$exe > llvm.txt; ./real_plir$exe > plir.txt
  paste -d' ' llvm.txt plir.txt | awk '{ r=$5/$2; printf "%-12s %9.2f %9.2f %7.2f %s\n", $1, $2, $5, r, ($3==$6)?"":"MISMATCH" }'
fi
