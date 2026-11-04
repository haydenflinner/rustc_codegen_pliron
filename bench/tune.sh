#!/usr/bin/env bash
# bench/tune.sh — the PLIRON_* knob-tuning driver. For each config in
# CONFIGS, rebuilds the bench crate (the knobs gate codegen of the
# kernels, not the backend dylib, so each row is one cargo-rustc plus one
# benchfmt run), records the median ns/op per kernel, then asks mimas to
# rank the results through a `datalog {}` block: rows -> `res` input
# relation -> `best` rule.
set -euo pipefail
cd "$(dirname "$0")/.."

KERNELS=("fib" "matmul")
CONFIGS=(
  ""
  "PLIRON_INSTCOMBINE=egg"
  "PLIRON_OPT_BISECT=3"
  "PLIRON_PIPELINE=jumpthread,clifpeep,loadfwd"
)
RESULTS=bench/tune/results.tsv

BENCHTIME=${BENCHTIME:-2s}
COUNT=${COUNT:-3}

case "$(uname -s)" in
  Darwin) lib=librustc_codegen_pliron.dylib ;;
  *)      lib=librustc_codegen_pliron.so ;;
esac
PY=python3

bench_bin() {
  env $1 cargo rustc -p pliron-bench --bench kernels --profile bench \
      --message-format json-render-diagnostics -- ${2:-} -Z "codegen-backend=$PWD/target/debug/$lib" \
    | "$PY" -c '
import json, sys
exe = ""
for line in sys.stdin:
    try: msg = json.loads(line)
    except ValueError: continue
    if msg.get("reason") == "compiler-artifact" and msg.get("executable"):
        exe = msg["executable"]
if not exe: sys.exit("bench_bin: no executable")
print(exe)'
}

# median of each benchmark's ns/op across -count epochs
med_ns() {
  "$PY" -c '
import json, statistics, sys
rows = {}
for line in sys.stdin:
    f = line.split()
    if len(f) >= 4 and f[0].startswith("Benchmark"):
        try:
            rows.setdefault(f[0][len("Benchmark"):].rsplit("-", 1)[0].lower(), []).append(float(f[2]))
        except ValueError:
            pass
for k, v in rows.items():
    print(json.dumps({"k": k, "ns": statistics.median(v)}))'
}

cargo build
: > "$RESULTS"

cid=0
for cfg in "${CONFIGS[@]}"; do
  echo "=== config $cid: ${cfg:-base}"
  touch bench/benches/kernels.rs   # env isn't fingerprinted — force rebuild
  bin=$(bench_bin "$cfg")
  for k in "${KERNELS[@]}"; do
    "$bin" -bench "$k" -benchtime "$BENCHTIME" -count "$COUNT" -cpu 1 | med_ns | \
      while read -r row; do
        echo "$row" | "$PY" -c "import json,sys; r=json.load(sys.stdin); print(f\"{r['k']}\t$cid\t{r['ns']}\")" >> "$RESULTS"
      done
  done
  cid=$((cid + 1))
done
cat "$RESULTS"

# results -> rank.mim: rows feed `res` via `from`, `beaten` is host-computed
# (anti-join) so `best` can negate it stratified
{
  echo 'let rows = ['
  while IFS=$'\t' read -r k c ns; do echo "  (\"$k\", $c, $ns),"; done < "$RESULTS"
  echo '];'
  cat <<'MIM'
fn beaten_row(a: (str, int, float), rows: [(str, int, float)]) -> bool {
    for b in rows {
        if b.0 == a.0 && b.2 < a.2 { return true; }
    }
    false
}
let beaten = for a in rows collect (a.0, a.2, beaten_row(a, rows));
let (res_, beaten_, best_) = datalog {
    rel res(kernel: str, cid: int, ns: float) from rows
    rel beaten(kernel: str, ns: float, f: bool) from beaten
    rel best(kernel: str, cid: int, ns: float)
    best(k, c, ns) if res(k, c, ns), !beaten(k, ns, true)
};
print("== best per kernel ==");
for b in best_ { print(f"{b.0} config {b.1} {b.2} ns"); }
MIM
} > bench/tune/rank.mim
~/repos/dsa/mimas/target/debug/mimas run bench/tune/rank.mim
