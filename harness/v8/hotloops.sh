#!/bin/sh
# hotloops.sh mod.wasm kernel fn-substr iters reps [top]
# Ticks of the function's TurboFan code per innermost machine-code loop
# (size, share, the wasm offsets inside) and the hottest single
# instructions. hotw.sh attributes ticks to the nearest source position,
# which smears a loop without trapping ops over whatever precedes it.
H=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$H/../.." && pwd); OUT=$ROOT/target/v8prof; mkdir -p "$OUT"
wasm=$1; idx=$(wasm-objdump -x "$wasm" | grep -E "^ - func\[[0-9]+\] sig=.*<[^>]*$3[^>]*>" | head -1 | sed -E 's/.*func\[([0-9]+)\].*/\1/')
T=$(mktemp -d "$OUT/hl.XXXX"); cd "$T"
node --prof --prof-sampling-interval=50 --logfile=v8.log --no-logfile-per-isolate --print-wasm-code --print-wasm-code-function-index="$idx" "$H/prof.mjs" "$wasm" "$2" "$4" "$5" > code.txt 2>&1
wasm-objdump -d "$wasm" > od.txt
python3 -I "$H/hotloops.py" v8.log code.txt od.txt "$idx" "${6:-14}"
echo "artifacts: $T idx=$idx"
