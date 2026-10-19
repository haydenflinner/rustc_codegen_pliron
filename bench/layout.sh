#!/usr/bin/env bash
# bench/layout.sh — re-run one kernel under several shuffled link layouts to
# separate a real codegen delta from placement/alignment luck.
#
# A pass can leave a hot function byte-identical yet shift its address; the
# low address bits index the I-cache and branch predictor, so the same bytes
# can measure several % faster or slower depending on where they land. Run a
# suspicious kernel under N layouts and compare medians, not single numbers.
#
#   bench/layout.sh sieve            # 3 layouts (1 as-built + 2 shuffled)
#   bench/layout.sh sieve 5          # more samples
#   PLIRON_CICM=1 bench/layout.sh sieve   # ambient PLIRON_* config applies
#   bench/layout.sh "sieve|hmget" 2 --verbose   # extra args go to criterion
#
# Mechanism: `-Wl,-order_file` (Darwin) / `--symbol-ordering-file` (lld)
# reorders __text by symbol list at link time — identical codegen, different
# placement. ~4 s per extra layout plus one criterion run.
#
# Each layout needs a distinct order-file PATH: cargo fingerprints the
# link-arg string, not the file contents, so reusing one path reuses the
# cached binary. Order files land in target/layout/ for reproduction.
set -euo pipefail
cd "$(dirname "$0")/.."

KERNEL=${1:?usage: bench/layout.sh KERNEL [N] [criterion args...]}
N=${2:-3}
shift; [ $# -gt 0 ] && shift || true

case "$(uname -s)" in
  Darwin) lib=librustc_codegen_pliron.dylib ; ordflag() { echo "-Wl,-order_file,$1"; } ;;
  *)      lib=librustc_codegen_pliron.so
          ordflag() { echo "-Wl,--symbol-ordering-file=$1"; } ;;
esac

if command -v uv >/dev/null; then
  PY=(uv run --no-project python)
else
  PY=(python3)
  command -v "${PY[0]}" >/dev/null && "${PY[@]}" -c pass >/dev/null 2>&1 || PY=(python)
fi

bench_bin() {
  cargo rustc -p pliron-bench --bench kernels --profile bench \
      --message-format json-render-diagnostics -- "$@" \
    | "${PY[@]}" -c '
import json, sys
exe = ""
for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    if msg.get("reason") == "compiler-artifact" and msg.get("executable"):
        exe = msg["executable"]
if not exe:
    sys.exit("bench_bin: no executable in cargo output")
print(exe)'
}

# criterion "time: [lo mid hi]" -> mid in ns
mid_ns() {
  "${PY[@]}" -c '
import re, sys
for line in sys.stdin:
    m = re.match(r"^(\S+)\s+time:\s+\[[\d.]+ [a-zµ]+ ([\d.]+) ([a-zµ]+)", line)
    if m:
        unit = {"ps":1e-3,"ns":1,"µs":1e3,"us":1e3,"ms":1e6,"s":1e9}[m.group(3)]
        print(float(m.group(2)) * unit)'
}

cargo build >/dev/null   # the backend dylib
touch bench/benches/kernels.rs   # env isn't fingerprinted — force rebuild
bin=$(bench_bin -Z "codegen-backend=$PWD/target/debug/$lib")

orddir=target/layout
mkdir -p "$orddir"

declare -a meds
i=0
while [ $i -lt "$N" ]; do
  if [ $i -eq 0 ]; then
    run_bin=$bin; tag="L0 as-built"
  else
    ord="$orddir/ord_$$_$i.txt"
    # defined symbols only ("addr type name"), deduped, shuffled
    nm "$bin" | awk 'NF==3 {print $3}' | sort -u \
      | "${PY[@]}" -c 'import sys, random; l=sys.stdin.read().splitlines(); random.seed('"$i"'); random.shuffle(l); print("\n".join(l))' \
      > "$ord"
    run_bin=$(bench_bin -Z "codegen-backend=$PWD/target/debug/$lib" \
                        -C "link-arg=$(ordflag "$PWD/$ord")")
    tag="L$i $ord"
  fi
  echo "=== $tag" >&2
  med=$("$run_bin" --bench "^$KERNEL\$" "$@" | tee /dev/stderr | mid_ns)
  meds+=("${med:-nan}")
  i=$((i + 1))
done

{
  echo "== layout medians (ns) =="
  printf '%s\n' "${meds[@]}" | "${PY[@]}" -c '
import sys, statistics
v = [float(x) for x in sys.stdin.read().split()]
v.sort()
print("runs   :", " ".join(f"{x:.0f}" for x in v))
print(f"median : {statistics.median(v):.0f}  spread: {(v[-1]-v[0])/v[0]*100:.1f}%")'
}
