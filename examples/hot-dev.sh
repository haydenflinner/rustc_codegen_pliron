#!/bin/bash
# Hot-reload dev loop for any binary crate (see docs/hot-reload.md).
# Builds the crate with PLIRON_HOT so every function sits behind a patch
# slot, runs it with PLIRON_HOT_DIR set, and on each change under src/
# recompiles the crate to one patch object that the running process loads
# in place: no restart, and Rust statics keep their live state.
#
#   examples/hot-dev.sh [crate dir] [-- app args...]
#
# The crate must depend on `pliron_hot` (path = ".../examples/pliron-hot")
# and call `pliron_hot::start()` early in main().
set -euo pipefail
B="$(cd "$(dirname "$0")/.." && pwd)"
export RUSTUP_TOOLCHAIN=nightly-2026-10-22
DIR="${1:-.}"; [[ "$DIR" == --* ]] && DIR=.
cd "$DIR"; shift 2>/dev/null || true
ARGS=(); [[ "${1:-}" == -- ]] && { shift; ARGS=("$@"); }
case "$(uname)" in Darwin) DYLIB=dylib;; *) DYLIB=so;; esac
NAME=$(cargo metadata --no-deps --format-version 1 -q \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["targets"][0]["name"])')
export RUSTFLAGS="-Zcodegen-backend=$B/target/debug/librustc_codegen_pliron.$DYLIB -Ccodegen-units=1"
# Type-layout table in base.ref and every patch: the loader refuses a
# patch that changes a struct/enum the live process holds values of.
export PLIRON_HOT_LAYOUT=1
W=target/hot; mkdir -p "$W/patches"
CR=(cargo rustc -q --bin "$NAME" --)
echo "[hot-dev] building $NAME with PLIRON_HOT"
PLIRON_HOT="${NAME//-/_}" "${CR[@]}" -Ccodegen-units=1
cp "target/debug/$NAME" "$W/app"
rm -f "$W"/patches/*.o
"${CR[@]}" -Ccodegen-units=1 --emit=obj="$W/patches/base.ref"
PLIRON_HOT_DIR="$W/patches" "$W/app" "${ARGS[@]}" & APP=$!
trap 'kill $APP 2>/dev/null' EXIT
echo "[hot-dev] running; edit files under src/ to hot-patch"
stamp() { find src -name '*.rs' -exec stat -f %m {} + 2>/dev/null || find src -name '*.rs' -exec stat -c %Y {} +; }
last=$(stamp | sort -n | tail -1); n=0
while kill -0 $APP 2>/dev/null; do
    sleep 0.3
    m=$(stamp | sort -n | tail -1); [[ $m == "$last" ]] && continue
    last=$m; n=$((n + 1)); t0=$(date +%s%N)
    if "${CR[@]}" -Ccodegen-units=1 --emit=obj="$W/p.o"; then
        mv "$W/p.o" "$W/patches/$(printf %04d $n).o"
        echo "[hot-dev] patch $n built in $(( ($(date +%s%N) - t0) / 1000000 )) ms" >&2
    fi
done
