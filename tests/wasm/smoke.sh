#!/usr/bin/env bash
# wasm emit smoke: two leaf crates -> pliron wasm objects (--emit obj) ->
# pliron-wasm-ld -> node. No -Zbuild-std, so no sysroot rebuild — covers the
# emit pipeline (waffle, wpeep + audit, serialization, reloc/link, run) in
# seconds. The std-through-pliron legs stay in tests/wasm/run.sh (gate:
# --only wasm / --full).
# r[verify verify.audit.wasm]
# r[verify verify.peepdiff.wasm]
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
# r[impl verify.audit.wasm]
export PLIRON_WASM_AUDIT=${PLIRON_WASM_AUDIT:-panic}
# The keyed private copy of the dylib (harness/backend_path.py), as every
# script that passes the flag uses. Nothing here is cached (plain rustc,
# --emit obj), so the dylib itself is as good where the helper cannot run.
DY=${BACKEND:-$ROOT/target/debug/$lib}
BE="-Zcodegen-backend=$(harness/backend_path.py --dylib "$DY" 2>/dev/null || echo "$DY")"
LD=$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld$exe
out=$ROOT/target/wasm-smoke; mkdir -p "$out"
# Every object before and after the byte peepholes, for peepdiff below.
# r[impl verify.peepdiff.wasm]
rm -rf "$out/moddump"
export PLIRON_WASM_MODDUMP=$out/moddump
# Two objects for peepdiff only (not linked): the shapes of the wpeep
# miscompiles of 2026-11-16. The second needs opt-level 1 for rustc to
# keep the `call_mut` shim out of line.
rustc $BE --edition 2024 -O --target wasm32-unknown-unknown --crate-type lib \
  --emit obj -o "$out/smoke_peep.o" tests/wasm/smoke/peep.rs
PLIRON_OPT_BISECT=0 rustc $BE --edition 2024 -C opt-level=1 --target wasm32-unknown-unknown --crate-type lib \
  --emit obj -o "$out/smoke_peep_raw.o" tests/wasm/smoke/peep_raw.rs
rustc $BE --edition 2024 -O --target wasm32-unknown-unknown --crate-type lib \
  --emit obj -o "$out/smoke_a.o" tests/wasm/smoke/a.rs
rustc $BE --edition 2024 -O --target wasm32-unknown-unknown --crate-type lib \
  --emit obj -o "$out/smoke_b.o" tests/wasm/smoke/b.rs
# One object with the optimizer off: raw lowering through the wasm stages.
PLIRON_OPT_BISECT=0 rustc $BE --edition 2024 -O --target wasm32-unknown-unknown --crate-type lib \
  --emit obj -o "$out/smoke_raw.o" tests/wasm/smoke/raw.rs
"$LD" "$out/smoke_a.o" "$out/smoke_b.o" "$out/smoke_raw.o" --export raw_divmod --export raw_digits \
  --export fib --export sum_sq --export hypot --export statics \
  --export indirect --export crosscall --export genes_hash --export cmp128 --export ops128 --export bb_keep --export fops64 --export fops32 \
  --export ctors \
  --no-entry -o "$out/smoke.wasm"
node tests/wasm/smoke/run.mjs "$out/smoke.wasm"
# Translation validation by execution: every body of the five objects
# before and after wpeep, on the same seeded inputs in V8 (~0.3 s).
node harness/wasm/peepdiff.mjs "$out/moddump" -q --trials 64
# r[verify wasm.ld.ctors]
# The same objects with `__wasm_call_ctors` exported: the embedder calls it,
# the exports are not wrapped.
"$LD" "$out/smoke_a.o" "$out/smoke_b.o" --export fib --export ctors \
  --export __wasm_call_ctors --no-entry -o "$out/smoke_ctors.wasm"
node tests/wasm/smoke/ctors_manual.mjs "$out/smoke_ctors.wasm"
