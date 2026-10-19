#!/usr/bin/env bash
# regalloc.sh CLIF_LOG [HEADER_LINE] — run the spill-solver slice on one
# function from a PLIRON_CLIF dump: extract facts, append the datalog
# solver tail, run mimas. HEADER_LINE is the 0-based line of the
# `==== clif <name> final ====` header (grep -n to find it; default =
# last function in the dump).
set -euo pipefail
cd "$(dirname "$0")/.."
MIM=${MIM:-~/repos/dsa/mimas/target/debug/mimas}
OUT=${OUT:-/tmp/regalloc_run.mim}
python3 tools/cliffacts.py "$1" ${2:-} > /tmp/cliffacts.mim
cat /tmp/cliffacts.mim mimas/regalloc-tail.mim > "$OUT"
exec "$MIM" run "$OUT"
