#!/bin/bash
# Build the game with rustc_codegen_pliron + wild (run ../../test.sh first so the
# backend and target/wild-ld exist). Cargo doesn't track the backend .so, so
# delete the target dir after backend changes.
#   ./build.sh                                  target crates via pliron; proc macros/build scripts on stock rustc
#   ./build.sh -Zbuild-std=std,panic_unwind     ...and std itself via pliron
#   ./build.sh --host                           every crate incl. proc macros/build scripts via pliron (target-host/)
B="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$(dirname "$0")"
export RUSTUP_TOOLCHAIN=nightly-2026-10-06
export RUSTFLAGS="-Zcodegen-backend=$B/target/debug/librustc_codegen_pliron.so -Clinker-features=-lld -Clink-self-contained=-linker -Zunstable-options -Clink-arg=-B$B/target/wild-ld"
if [[ "${1:-}" == --host ]]; then
    shift
    CARGO_TARGET_DIR=target-host exec cargo build "$@"
fi
exec cargo build --target x86_64-unknown-linux-gnu "$@"
