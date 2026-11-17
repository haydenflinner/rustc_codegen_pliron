#!/usr/bin/env bash
# mcode.sh mod.wasm kernel func-substr [iters] — print the function's
# TurboFan machine code (tier it up by running the kernel ITERS times per
# call, default 200: a function that has not tiered up has no TurboFan code).
# The function is chosen by name substring through the name section; the
# first match wins, so make the substring unique. node 26's
# `--print-wasm-code-function-index` does not filter (it printed the first
# TurboFan function of the run, `hmget::setup` for k_hmget), so every
# function's code is printed and the one whose `index:` matches is kept.
set -eu
H=$(cd "$(dirname "$0")" && pwd)
idx=$(wasm-objdump -x "$1" | grep -E "^ - func\[[0-9]+\] sig=.*<[^>]*$3[^>]*>" | head -1 | sed -E 's/.*func\[([0-9]+)\].*/\1/')
[ -n "$idx" ] || { echo "mcode.sh: no function matching '$3' in $1" >&2; exit 1; }
node --print-wasm-code "$H/mcode.mjs" "$1" "$2" "${4:-200}" 2>&1 | python3 -I -c '
import sys
want = "index: " + sys.argv[1]
cur = []; found = 0
for line in sys.stdin:
    if line.startswith("--- WebAssembly code ---"):
        cur = []; continue
    if line.startswith("--- End code ---"):
        if any(l.strip() == want for l in cur) and any("compiler: TurboFan" in l for l in cur):
            sys.stdout.write("".join(cur) + "\n"); found += 1
        cur = []; continue
    cur.append(line)
if not found:
    sys.exit("mcode.sh: function %s has no TurboFan code in this run (raise ITERS)" % sys.argv[1])
' "$idx"
