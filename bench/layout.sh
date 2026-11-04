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
#   bench/layout.sh "sieve|hmget" 2  # |-separated names; extra args go to
#                                    # the benchfmt binary
#
# Mechanism: `-Wl,-order_file` (Darwin) / `--symbol-ordering-file` (lld)
# reorders __text by symbol list at link time — identical codegen, different
# placement. ~4 s per extra layout plus one benchfmt run.
#
# Each layout needs a distinct order-file PATH: cargo fingerprints the
# link-arg string, not the file contents, so reusing one path reuses the
# cached binary. Order files land in target/layout/ for reproduction.
set -euo pipefail
cd "$(dirname "$0")/.."

KERNEL=${1:?usage: bench/layout.sh KERNEL [N] [bench args...]}
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

# benchfmt "BenchmarkName iters ns ns/op" epochs -> "name median_ns" per kernel
med_ns() {
  "${PY[@]}" -c '
import statistics, sys
rows = {}
for line in sys.stdin:
    f = line.split()
    if len(f) >= 4 and f[0].startswith("Benchmark"):
        try:
            rows.setdefault(f[0][len("Benchmark"):].rsplit("-", 1)[0].lower(), []).append(float(f[2]))
        except ValueError:
            pass
for k, v in rows.items():
    print(k, int(statistics.median(v)))'
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
  for k in $KERNEL; do
    while read -r row; do meds+=("$row"); done \
      < <("$run_bin" -bench "$k" -benchtime "${BENCHTIME:-1s}" -count "${COUNT:-5}" -cpu 1 "$@" \
          | tee /dev/stderr | med_ns)
  done
  i=$((i + 1))
done

{
  echo "== layout medians (ns) =="
  printf '%s\n' "${meds[@]}" | "${PY[@]}" -c '
import sys, statistics, collections
g = collections.defaultdict(list)
for line in sys.stdin:
    f = line.split()
    if len(f) >= 2:
        g[f[0]].append(float(f[1]))
for k, v in g.items():
    v.sort()
    print(f"{k}: " + " ".join(f"{x:.0f}" for x in v))
    print(f"{k}: median {statistics.median(v):.0f}  spread {(v[-1]-v[0])/v[0]*100:.1f}%")'
}
