#!/usr/bin/env bash
# cpuprof.sh mod.wasm kernel iters reps [top] — per-function self time.
set -eu
H=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$H/../.." && pwd); OUT=$ROOT/target/v8prof; mkdir -p "$OUT"
wasm=$1
case $wasm in /*) ;; *) wasm=$PWD/$wasm ;; esac
wasm=$(cd "$(dirname "$wasm")" && pwd)/$(basename "$wasm")
D=$(mktemp -d "$OUT/cpu.XXXX")
node --cpu-prof --cpu-prof-interval 100 --cpu-prof-dir="$D" "$H/prof.mjs" "$wasm" "$2" "$3" "$4"
python3 -I "$H/selfprof.py" "$D"/*.cpuprofile "${5:-20}"
