#!/usr/bin/env bash
# harness/dsa/hot.sh SIDE PKGDIR FUNC WORKLOAD [LOOPS] [TOP] — where one
# function's time goes inside V8's optimized code, per machine instruction.
#
#   harness/dsa/hot.sh pliron /tmp/dsa-bench/pkg-pliron 'vm::step_one' vm_loop
#   harness/dsa/hot.sh llvm   /tmp/dsa-bench/pkg-llvm   'vm::step_one' vm_loop 10 8
#
# Runs bench.mjs --side=SIDE --loop=LOOPS WORKLOAD under `node --prof
# --print-wasm-code` and hands the tick log and the code listing to
# hotfn.py: ticks per mnemonic, the share on frame accesses, and the
# hottest instruction runs with their listing (calls into V8's runtime
# show up as `;; external reference (wasm::memory_copy)` comments).
# FUNC is matched against the END of the demangled name, crate hashes
# removed (`vm::step_one`, `memchr::memchr_aligned`).
# The function-level profile (prof.sh) says which function; this says
# what in it. It is how the interpreter's per-step `memory.copy` was found.
# The listing is every function V8 compiled (hundreds of MB for a big
# module) and stays in $DSA_BENCH/hot/ for a second hotfn.py run.
set -euo pipefail
H=$(cd "$(dirname "$0")" && pwd)
BASE=${DSA_BENCH:-/tmp/dsa-bench}
side=${1:?usage: hot.sh SIDE PKGDIR FUNC WORKLOAD [LOOPS] [TOP]}; pkg=$2; fn=$3; w=$4
D=$BASE/hot/$w-$side; rm -rf "$D"; mkdir -p "$D"; cd "$D"
node --prof --prof-sampling-interval=100 --logfile=v8.log --no-logfile-per-isolate --print-wasm-code \
  "$H/bench.mjs" "--side=$side" "--$side=$pkg" "--loop=${5:-10}" "$w" > code.txt 2>&1
python3 -I "$H/hotfn.py" "$D" "$fn" "${6:-6}"
echo "artifacts: $D"
