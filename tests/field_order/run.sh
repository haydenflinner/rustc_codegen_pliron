#!/bin/bash
# Field-order check (docs/pgo.md "Field order"): build main.rs plain, with
# PLIRON_FIELD_HEAT, and with order files that force permutations; sizes,
# alignments and values must match the plain run (only `offset_of` lines
# may differ). Usage: tests/field_order/run.sh [release|debug] [-O|-Copt-level=0]
set -eu
cd "$(dirname "$0")/../.."
prof=${1:-release}
opt=${2:--O}
case "$(uname)" in Darwin) lib=librustc_codegen_pliron.dylib ;; *) lib=librustc_codegen_pliron.so ;; esac
BE="-Zcodegen-backend=$PWD/target/$prof/$lib"
out=target/field_order
rm -rf "$out"
mkdir -p "$out"
build() { # name, env...
    local name=$1
    shift
    env "$@" rustc $opt $BE tests/field_order/main.rs -o "$out/$name"
    "$out/$name" > "$out/$name.out"
}
build plain PLIRON_NOP=1
build heat "PLIRON_FIELD_HEAT=$PWD/$out/heat.txt"
grep -c . "$out/heat.txt" > /dev/null
# Order files from the heat file's (type, field) pairs: weights rising and
# falling in declaration-independent (sorted) order, and the heat itself.
sort -u -k1,2 "$out/heat.txt" | awk -F'\t' '{print $1 "\t" $2 "\t" NR}' > "$out/up.txt"
sort -u -k1,2 "$out/heat.txt" | awk -F'\t' '{print $1 "\t" $2 "\t" 100000-NR}' > "$out/down.txt"
fail=0
for o in heat up down; do
    build "order-$o" "PLIRON_FIELD_ORDER=$PWD/$out/$o.txt" "PLIRON_FIELD_ORDER_LOG=$PWD/$out/$o.log"
    n=$(grep -v 'skip-seq$' "$out/$o.log" 2>/dev/null | sort -u | wc -l | tr -d ' ')
    if diff <(grep -v '^offset_of' "$out/plain.out") <(grep -v '^offset_of' "$out/order-$o.out") > "$out/$o.diff"; then
        d=$(diff <(grep '^offset_of' "$out/plain.out") <(grep '^offset_of' "$out/order-$o.out") | grep -c '^>' || true)
        echo "ok   order-$o: $n layouts permuted, $d offset_of lines differ"
    else
        echo "FAIL order-$o: output differs ($out/$o.diff)"
        fail=1
    fi
done
diff "$out/plain.out" "$out/heat.out" > /dev/null || { echo "FAIL heat run output differs"; fail=1; }
exit $fail
