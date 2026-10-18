#!/usr/bin/env bash
# wasm32 smoke test: core + compiler_builtins + tests/wasm all go through the
# pliron backend, are linked by tools/pliron-wasm-ld (no wasm-ld/LLVM), and run in node.
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$PWD
exe=
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) ROOT=$(cygpath -m "$PWD"); exe=.exe ;; esac
cargo build -q --release --manifest-path tools/pliron-wasm-ld/Cargo.toml
case "$(uname -s)" in
  Darwin)               lib=librustc_codegen_pliron.dylib ;;
  MINGW*|MSYS*|CYGWIN*) lib=rustc_codegen_pliron.dll ;;
  *)                    lib=librustc_codegen_pliron.so ;;
esac
# Every wpeep copy-forward round is checked by the Datalog audit
# (src/wpeep_audit.rs, ~7% build time); a rejected round fails the run.
export PLIRON_WASM_AUDIT=${PLIRON_WASM_AUDIT:-panic}
export RUSTFLAGS="-Zcodegen-backend=${BACKEND:-$ROOT/target/debug/$lib} -Clinker=$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld$exe"
# cargo doesn't track the backend dylib, so key the sysroot dir on its
# content: same backend -> core/std reuse (seconds), new backend -> fresh
# dir rebuilt by it. Older keys are pruned.
BE_KEY=$(cksum < "${BACKEND:-$ROOT/target/debug/$lib}" | cut -d' ' -f1)
export CARGO_TARGET_DIR=$ROOT/target/wasm/be-$BE_KEY
mkdir -p "$ROOT/target/wasm"
for d in "$ROOT"/target/wasm/*; do
  [[ "$d" == "$CARGO_TARGET_DIR" ]] || rm -rf "$d"
done
(cd tests/wasm && cargo build -q --release --target wasm32-unknown-unknown \
  -Zbuild-std=core,compiler_builtins -Zbuild-std-features=compiler-builtins-mem)
node tests/wasm/run.mjs "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/wasmtest.wasm"
# std (alloc, HashMap, fmt, dlmalloc + memory.grow) on wasm32-unknown-unknown
(cd tests/wasm/std && cargo build -q --release --target wasm32-unknown-unknown -Zbuild-std=std,panic_abort)
node tests/wasm/run_std.mjs "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/wasmstdtest.wasm"
# std on wasm32-wasip1: wasi-libc is replaced by tools/pliron-wasi-libc (pure Rust).
(cd tools/pliron-wasi-libc && CARGO_TARGET_DIR=$CARGO_TARGET_DIR/wasi-libc cargo build -q --release \
  --target wasm32-wasip1 -Zbuild-std=core,panic_abort)
WASI_LIBC=$CARGO_TARGET_DIR/wasi-libc/wasm32-wasip1/release
ln -sf libpliron_wasi_libc.a "$WASI_LIBC/libc.a"
# std's panic=unwind links ask for -lc and -lunwind; the Rust `unwind` rlib and
# our libc provide the symbols, so an empty archive satisfies the -l flag.
printf '!<arch>\n' > "$WASI_LIBC/libunwind.a"
(cd tests/wasm/wasi && RUSTFLAGS="$RUSTFLAGS -Clink-self-contained=no -Lnative=$WASI_LIBC" \
  cargo build -q --release --target wasm32-wasip1 -Zbuild-std=std,panic_abort)
# Prefer wasmtime: node's WASI on Windows lacks fd_readdir (errno 52) so the
# fs leg can't pass there; node remains the fallback runner via run_wasi.mjs.
WT=$(command -v wasmtime 2>/dev/null || true)
if [ -z "$WT" ]; then
  for cand in "$HOME"/bin/wasmtime/*/wasmtime.exe "$HOME"/bin/wasmtime/wasmtime; do
    [ -x "$cand" ] && WT=$cand && break
  done
fi
wtd=$(mktemp -d)
wtd_w=$(cygpath -m "$wtd" 2>/dev/null || echo "$wtd")
if [ -n "$WT" ]; then
  "$WT" run --argv0=wasitest "--dir=$wtd_w" --env HELLO=world --env "WASI_DIR=$wtd_w" \
    "$CARGO_TARGET_DIR/wasm32-wasip1/release/wasmwasitest.wasm" a1
else
  wasi_run=(node --no-warnings tests/wasm/run_wasi.mjs "$CARGO_TARGET_DIR/wasm32-wasip1/release/wasmwasitest.wasm")
  if [ -n "$exe" ]; then "${wasi_run[@]}" || echo "(wasi fs leg failed on Windows host; ignored)"; else "${wasi_run[@]}"; fi
fi
# panic=unwind: emulated EH (__pliron_eh flag+exn) — catch_unwind, drop order,
# payloads, resume_unwind.
(cd tests/wasm/unwind && \
  RUSTFLAGS="$RUSTFLAGS -Cpanic=unwind -Clink-self-contained=no -Lnative=$WASI_LIBC" \
  CARGO_TARGET_DIR=$CARGO_TARGET_DIR/eh \
  cargo build -q --release --target wasm32-wasip1 -Zbuild-std=std,panic_unwind)
if [ -n "$WT" ]; then
  "$WT" run --argv0=wasitest "--dir=$wtd_w" --env HELLO=world --env "WASI_DIR=$wtd_w" \
    "$CARGO_TARGET_DIR/eh/wasm32-wasip1/release/wasmunwindtest.wasm"
else
  node --no-warnings tests/wasm/run_wasi.mjs \
    "$CARGO_TARGET_DIR/eh/wasm32-wasip1/release/wasmunwindtest.wasm"
fi
# panic=unwind on wasm32-unknown-unknown: same emulated EH via the literal
# `llvm.wasm.throw` symbol std's unwind::wasm uses.
(cd tests/wasm/unwind-unknown && \
  RUSTFLAGS="$RUSTFLAGS -Cpanic=unwind" \
  CARGO_TARGET_DIR=$CARGO_TARGET_DIR/eh-unk \
  cargo build -q --release --target wasm32-unknown-unknown -Zbuild-std=std,panic_unwind)
node --no-warnings tests/wasm/run_eh_unk.mjs \
  "$CARGO_TARGET_DIR/eh-unk/wasm32-unknown-unknown/release/wasmunwindunk.wasm"
