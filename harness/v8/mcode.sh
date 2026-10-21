#!/usr/bin/env bash
# mcode.sh mod.wasm kernel func-substr — print the function's TurboFan
# machine code (tier it up by running the kernel).
set -eu
H=$(cd "$(dirname "$0")" && pwd)
idx=$(wasm-objdump -x "$1" | grep -E "^ - func\[[0-9]+\] sig=.*<[^>]*$3[^>]*>" | head -1 | sed -E 's/.*func\[([0-9]+)\].*/\1/')
node --print-wasm-code --print-wasm-code-function-index="$idx" "$H/mcode.mjs" "$1" "$2" 1 2>&1 \
  | awk '/compiler: TurboFan/{p=1} p'
