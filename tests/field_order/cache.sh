#!/bin/bash
# Cache benchmark for field order (docs/pgo.md "Field order"): cache.rs
# plain, reordered from static heat (default policy: swept types keep
# rustc's order), reordered with that rule off, and with stock rustc.
# Usage: tests/field_order/cache.sh [elements...]   (default 20000 1000000)
set -eu
cd "$(dirname "$0")/../.."
case "$(uname)" in Darwin) lib=librustc_codegen_pliron.dylib ;; *) lib=librustc_codegen_pliron.so ;; esac
BE="-Zcodegen-backend=$PWD/target/release/$lib"
out=target/field_order_cache
mkdir -p "$out"
: > "$out/heat.txt"
: > "$out/order.log"
src=tests/field_order/cache.rs
rustc -O $BE $src -o "$out/plain" 2> /dev/null
PLIRON_FIELD_HEAT="$PWD/$out/heat.txt" rustc -O $BE $src -o "$out/heatbin" 2> /dev/null
PLIRON_FIELD_ORDER="$PWD/$out/heat.txt" PLIRON_FIELD_ORDER_LOG="$PWD/$out/order.log" \
    rustc -O $BE $src -o "$out/order" 2> /dev/null
# The same order file with the sweep rule off (PLIRON_FIELD_ORDER_SEQ=1):
# every listed struct is reordered, swept or not.
PLIRON_FIELD_ORDER="$PWD/$out/heat.txt" PLIRON_FIELD_ORDER_SEQ=1 \
    rustc -O $BE $src -o "$out/forced" 2> /dev/null
rustc -O $src -o "$out/llvm" 2> /dev/null
[ $# -gt 0 ] || set -- 20000 1000000
for n in "$@"; do
    reps=9
    [ "$n" -gt 100000 ] || reps=101
    for b in plain order forced llvm; do
        "$out/$b" "$n" $reps > "$out/$b.out"
        printf '%-6s %s\n' "$b" "$(tail -1 "$out/$b.out")"
    done
    cmp -s <(grep checksum "$out/plain.out") <(grep checksum "$out/order.out") || { echo "FAIL checksum"; exit 1; }
done
head -1 "$out/plain.out" "$out/order.out" "$out/forced.out"
cat "$out/order.log"
