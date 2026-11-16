#!/usr/bin/env bash
# hotw.sh mod.wasm kernel func-substr iters reps [top]
# Like hot.sh, but attributes ticks to wasm instructions through the
# TurboFan source-position table (function-relative byte offsets).
set -eu
H=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$H/../.." && pwd); OUT=$ROOT/target/v8prof; mkdir -p "$OUT"
# Absolutize: we `cd` into the temp dir below, so a relative path would break.
wasm=$1
case $wasm in /*) ;; *) wasm=$PWD/$wasm ;; esac
wasm=$(cd "$(dirname "$wasm")" && pwd)/$(basename "$wasm")
idx=$(wasm-objdump -x "$wasm" | grep -E "^ - func\[[0-9]+\] sig=.*<[^>]*$3[^>]*>" | head -1 | sed -E 's/.*func\[([0-9]+)\].*/\1/')
D=$(mktemp -d "$OUT/hotw.XXXX"); cd "$D"
node --prof --prof-sampling-interval=50 --logfile=v8.log --no-logfile-per-isolate \
  --print-wasm-code --print-wasm-code-function-index="$idx" "$H/prof.mjs" "$wasm" "$2" "$4" "$5" > code.txt 2>&1
wasm-objdump -d "$wasm" > od.txt
python3 -I "$H/hotwasm.py" v8.log code.txt od.txt "$idx" "${6:-40}"
echo "artifacts: $D"
