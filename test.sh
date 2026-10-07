#!/usr/bin/env bash
# Usage: ./test.sh [--sysroot]
# Builds the backend, then compiles and runs tests/{nostd,std,asm} with it.
# --sysroot additionally rebuilds core/alloc/std with the backend (-Zbuild-std).
# Links with the pure-Rust `wild` linker when it is on PATH (cargo install wild-linker).
set -euo pipefail
cd "$(dirname "$0")"
cargo build
BE="-Zcodegen-backend=$PWD/target/debug/librustc_codegen_pliron.so"
if command -v wild >/dev/null; then
  # gcc 11 has no --ld-path, so point its -B search dir at an `ld` that is wild.
  mkdir -p target/wild-ld && ln -sf "$(command -v wild)" target/wild-ld/ld
  BE="$BE -Clinker-features=-lld -Clink-self-contained=-linker -Zunstable-options -Clink-arg=-B$PWD/target/wild-ld"
fi
out=target/tests; mkdir -p $out
rustc $BE --edition 2024 -Cpanic=abort -Clink-arg=-lc tests/nostd/main.rs -o $out/nostd && $out/nostd
for t in std asm unwind; do rustc $BE --edition 2024 tests/$t/main.rs -o $out/$t && $out/$t; done
# proc macro built by us, loaded by stock rustc: exercises the C ABI (byval/sret) across the bridge
rustc $BE --edition 2021 --crate-type proc-macro tests/proc_macro/pm.rs -o $out/libpm.so &&
    rustc --edition 2021 tests/proc_macro/main.rs --extern pm=$out/libpm.so -o $out/pm_user && $out/pm_user
if [[ "${1:-}" == --sysroot ]]; then
  (cd tests/sysroot && export RUSTFLAGS="$BE" CARGO_TARGET_DIR=../../target/sysroot &&
    T="$(rustc -vV | sed -n 's/host: //p')" &&
    cargo build -Zbuild-std=std,panic_unwind --target "$T" --bins &&
    ../../target/sysroot/$T/debug/sysroot-test && ../../target/sysroot/$T/debug/unwind-test)
fi
if command -v node >/dev/null; then tests/wasm/run.sh; fi
