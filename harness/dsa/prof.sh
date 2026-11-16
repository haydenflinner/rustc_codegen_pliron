#!/usr/bin/env bash
# harness/dsa/prof.sh WORKLOAD [LOOPS] [TOP] — per-function self time of one
# bench.mjs workload on each side (V8's sampling profiler, `node --cpu-prof`).
#
#   harness/dsa/prof.sh vm_fib_rec            # 30 calls per side, top 10
#   LLVM_PKG=... PLIRON_PKG=... harness/dsa/prof.sh game_tetris 50 15
#
# Each side runs alone in its own node process: bench.mjs --side=TAG
# --loop=N, i.e. set-up, then N calls of the workload. The first calls run
# in Liftoff code; with 30 calls of ~100 ms that is a few percent.
# Function names come from the wasm `name` section, which both builds keep
# (cargo's release default is strip=debuginfo, which leaves names; build.sh
# runs wasm-bindgen without --remove-name-section and wasm-bindgen demangles
# them). The profiles stay in $DSA_BENCH/prof/<workload>-<side>/ for
# chrome://inspect or speedscope.
set -euo pipefail
H=$(cd "$(dirname "$0")" && pwd)
BASE=${DSA_BENCH:-/tmp/dsa-bench}
w=${1:?usage: prof.sh WORKLOAD [LOOPS] [TOP]}
loops=${2:-30}
top=${3:-10}
for side in llvm pliron; do
  if [ "$side" = llvm ]; then dir=${LLVM_PKG:-$BASE/pkg-llvm-eval}; else dir=${PLIRON_PKG:-$BASE/pkg-pliron-eval}; fi
  D=$BASE/prof/$w-$side
  rm -rf "$D"; mkdir -p "$D"
  echo "== $side ($dir)"
  node --cpu-prof --cpu-prof-interval 200 --cpu-prof-dir="$D" "$H/bench.mjs" "--side=$side" "--$side=$dir" "--loop=$loops" "$w" | sed 's/^/  /'
  python3 -I "$H/selfprof.py" "$D"/*.cpuprofile "$top"
done
