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
# dir rebuilt by it. Keys untouched for an hour are pruned — never a
# younger one: a concurrent run (another agent, the gate) may be
# building in it with a different backend.
BE_KEY=$(cksum < "${BACKEND:-$ROOT/target/debug/$lib}" | cut -d' ' -f1)
export CARGO_TARGET_DIR=$ROOT/target/wasm/be-$BE_KEY
export RUSTC_ICE=${RUSTC_ICE:-$ROOT/target/ices}
mkdir -p "$ROOT/target/wasm" "$CARGO_TARGET_DIR"
touch "$CARGO_TARGET_DIR"
find "$ROOT/target/wasm" -mindepth 1 -maxdepth 1 -type d -mmin +60 \
  ! -path "$CARGO_TARGET_DIR" -exec rm -rf {} + 2>/dev/null || true

# Six sysroot builds in parallel. They never shared artifacts anyway
# (different -Zbuild-std flags, features, targets), so each gets its own
# target dir — cargo locks per dir, so same-dir parallelism wouldn't help.
# The two wasi legs wait on the libc build; every leg logs separately.
logs=$CARGO_TARGET_DIR/logs; mkdir -p "$logs"
(
  cd tests/wasm && cargo build -q --release --target wasm32-unknown-unknown \
    -Zbuild-std=core,compiler_builtins -Zbuild-std-features=compiler-builtins-mem
) >"$logs/unk-core.log" 2>&1 &
p_core=$!
# std (alloc, HashMap, fmt, dlmalloc + memory.grow) on wasm32-unknown-unknown
(
  cd tests/wasm/std && CARGO_TARGET_DIR=$CARGO_TARGET_DIR/std-unk \
    cargo build -q --release --target wasm32-unknown-unknown -Zbuild-std=std,panic_abort
) >"$logs/unk-std.log" 2>&1 &
p_std=$!
# std on wasm32-wasip1: wasi-libc is replaced by tools/pliron-wasi-libc (pure Rust).
(
  cd tools/pliron-wasi-libc && CARGO_TARGET_DIR=$CARGO_TARGET_DIR/wasi-libc \
    cargo build -q --release --target wasm32-wasip1 -Zbuild-std=core,panic_abort
) >"$logs/wasi-libc.log" 2>&1 &
p_libc=$!
# panic=unwind on wasm32-unknown-unknown: same emulated EH via the literal
# `llvm.wasm.throw` symbol std's unwind::wasm uses.
(
  cd tests/wasm/unwind-unknown && RUSTFLAGS="$RUSTFLAGS -Cpanic=unwind" \
    CARGO_TARGET_DIR=$CARGO_TARGET_DIR/eh-unk \
    cargo build -q --release --target wasm32-unknown-unknown -Zbuild-std=std,panic_unwind
) >"$logs/eh-unk.log" 2>&1 &
p_ehunk=$!

fail=
# The wasi legs need libc.a / libunwind.a, so gate them on the libc build —
# `wait` on its pid lets the other three keep running meanwhile. The wasi
# test builds in the libc dir: same target and panic strategy, so it reuses
# the core/compiler_builtins the libc just built instead of redoing them.
WASI_LIBC=$CARGO_TARGET_DIR/wasi-libc/wasm32-wasip1/release
p_wasi=; p_eh=
if wait $p_libc; then
  ln -sf libpliron_wasi_libc.a "$WASI_LIBC/libc.a"
  # std's panic=unwind links ask for -lc and -lunwind; the Rust `unwind` rlib and
  # our libc provide the symbols, so an empty archive satisfies the -l flag.
  printf '!<arch>\n' > "$WASI_LIBC/libunwind.a"
  (
    cd tests/wasm/wasi && RUSTFLAGS="$RUSTFLAGS -Clink-self-contained=no -Lnative=$WASI_LIBC" \
      CARGO_TARGET_DIR=$CARGO_TARGET_DIR/wasi-libc \
      cargo build -q --release --target wasm32-wasip1 -Zbuild-std=std,panic_abort
  ) >"$logs/wasi.log" 2>&1 &
  p_wasi=$!
  # panic=unwind: emulated EH (__pliron_eh flag+exn) — catch_unwind, drop order,
  # payloads, resume_unwind.
  (
    cd tests/wasm/unwind && \
      RUSTFLAGS="$RUSTFLAGS -Cpanic=unwind -Clink-self-contained=no -Lnative=$WASI_LIBC" \
      CARGO_TARGET_DIR=$CARGO_TARGET_DIR/eh \
      cargo build -q --release --target wasm32-wasip1 -Zbuild-std=std,panic_unwind
  ) >"$logs/eh.log" 2>&1 &
  p_eh=$!
else
  fail="wasi-libc"
fi
wait $p_core  || fail="$fail unk-core"
wait $p_std   || fail="$fail unk-std"
wait $p_ehunk || fail="$fail eh-unk"
if [ -n "$p_wasi" ]; then wait $p_wasi || fail="$fail wasi"; fi
if [ -n "$p_eh" ];   then wait $p_eh   || fail="$fail eh";   fi
if [ -n "$fail" ]; then
  for n in $fail; do echo "=== $n ==="; cat "$logs/$n.log"; done
  exit 1
fi

node tests/wasm/run.mjs "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/wasmtest.wasm"
node tests/wasm/run_std.mjs "$CARGO_TARGET_DIR/std-unk/wasm32-unknown-unknown/release/wasmstdtest.wasm"
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
    "$CARGO_TARGET_DIR/wasi-libc/wasm32-wasip1/release/wasmwasitest.wasm" a1
else
  wasi_run=(node --no-warnings tests/wasm/run_wasi.mjs "$CARGO_TARGET_DIR/wasi-libc/wasm32-wasip1/release/wasmwasitest.wasm")
  if [ -n "$exe" ]; then "${wasi_run[@]}" || echo "(wasi fs leg failed on Windows host; ignored)"; else "${wasi_run[@]}"; fi
fi
if [ -n "$WT" ]; then
  "$WT" run --argv0=wasitest "--dir=$wtd_w" --env HELLO=world --env "WASI_DIR=$wtd_w" \
    "$CARGO_TARGET_DIR/eh/wasm32-wasip1/release/wasmunwindtest.wasm"
else
  node --no-warnings tests/wasm/run_wasi.mjs \
    "$CARGO_TARGET_DIR/eh/wasm32-wasip1/release/wasmunwindtest.wasm"
fi
node --no-warnings tests/wasm/run_eh_unk.mjs \
  "$CARGO_TARGET_DIR/eh-unk/wasm32-unknown-unknown/release/wasmunwindunk.wasm"
