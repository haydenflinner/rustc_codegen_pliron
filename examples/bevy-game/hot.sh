#!/bin/bash
# Hot-reload dev loop (see examples/pliron-hot). Builds the game with every
# pliron_bevy_game function behind a patch slot, runs a copy of it, and on each
# save of src/main.rs recompiles the crate to one object that the running game
# loads in place: no restart, ECS state kept.
#   ./hot.sh [game args...]        e.g. ./hot.sh --autoplay
#   ./hot.sh --patch OUT.o         just build one patch object
B="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$(dirname "$0")"
export RUSTUP_TOOLCHAIN=nightly-2026-10-06
export RUSTFLAGS="-Zcodegen-backend=$B/target/debug/librustc_codegen_pliron.so -Clinker-features=-lld -Clink-self-contained=-linker -Zunstable-options -Clink-arg=-B$B/target/wild-ld"
CR=(cargo rustc -q --target x86_64-unknown-linux-gnu --bin pliron_bevy_game --)
H=target/hot; mkdir -p "$H/patches"
if [[ "${1:-}" == --patch ]]; then
    exec "${CR[@]}" -Ccodegen-units=1 --emit=obj="$2"
fi
PLIRON_HOT=pliron_bevy_game "${CR[@]}" -Ccodegen-units=1 || exit 1
cp target/x86_64-unknown-linux-gnu/debug/pliron_bevy_game "$H/game"
rm -f "$H"/patches/*
"${CR[@]}" -Ccodegen-units=1 --emit=obj="$H/patches/base.ref" || exit 1
PLIRON_HOT_DIR="$H/patches" "$H/game" "$@" & GAME=$!
trap 'kill $GAME 2>/dev/null' EXIT
last=$(stat -c %Y src/main.rs); n=0
while kill -0 $GAME 2>/dev/null; do
    sleep 0.2
    m=$(stat -c %Y src/main.rs); [[ $m == "$last" ]] && continue
    last=$m; n=$((n + 1)); t0=$(date +%s%N)
    if "${CR[@]}" -Ccodegen-units=1 --emit=obj="$H/p.o"; then
        mv "$H/p.o" "$H/patches/$(printf %04d $n).o"
        echo "[hot.sh] patch $n built in $(( ($(date +%s%N) - t0) / 1000000 )) ms" >&2
    fi
done
