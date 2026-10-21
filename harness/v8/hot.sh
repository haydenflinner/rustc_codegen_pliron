#!/usr/bin/env bash
# hot.sh mod.wasm kernel func-substr iters reps [min_pct]
# Per-instruction TurboFan heat for one function: V8 --prof ticks mapped
# onto the --print-wasm-code listing of the same run.
set -eu
H=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$H/../.." && pwd); OUT=$ROOT/target/v8prof; mkdir -p "$OUT"
idx=$(wasm-objdump -x "$1" | grep -E "^ - func\[[0-9]+\] sig=.*<[^>]*$3[^>]*>" | head -1 | sed -E 's/.*func\[([0-9]+)\].*/\1/')
D=$(mktemp -d "$OUT/hot.XXXX"); cd "$D"
node --prof --prof-sampling-interval=50 --logfile=v8.log --no-logfile-per-isolate \
  --print-wasm-code --print-wasm-code-function-index="$idx" "$H/prof.mjs" "$1" "$2" "$4" "$5" > code.txt 2>&1
python3 -I "$H/hotasm.py" v8.log code.txt "${6:-0.5}"
echo "artifacts: $D"
