#!/usr/bin/env bash
# wasm emit smoke: two leaf crates -> pliron wasm objects (--emit obj) ->
# pliron-wasm-ld -> node. No -Zbuild-std, so no sysroot rebuild — covers the
# emit pipeline (waffle, wpeep + audit, serialization, reloc/link, run) in
# seconds. The std-through-pliron legs stay in tests/wasm/run.sh (gate:
# --only wasm / --full).
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
# Keep the wpeep Datalog audit on the smoke bodies too.
export PLIRON_WASM_AUDIT=${PLIRON_WASM_AUDIT:-panic}
BE="-Zcodegen-backend=${BACKEND:-$ROOT/target/debug/$lib}"
LD=$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld$exe
out=$ROOT/target/wasm-smoke; mkdir -p "$out"
rustc $BE --edition 2024 -O --target wasm32-unknown-unknown --crate-type lib \
  --emit obj -o "$out/smoke_a.o" tests/wasm/smoke/a.rs
rustc $BE --edition 2024 -O --target wasm32-unknown-unknown --crate-type lib \
  --emit obj -o "$out/smoke_b.o" tests/wasm/smoke/b.rs
"$LD" "$out/smoke_a.o" "$out/smoke_b.o" \
  --export fib --export sum_sq --export hypot --export statics \
  --export indirect --export crosscall --no-entry -o "$out/smoke.wasm"
node tests/wasm/smoke/run.mjs "$out/smoke.wasm"
