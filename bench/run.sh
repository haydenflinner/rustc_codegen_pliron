#!/usr/bin/env bash
# bench/run.sh — build the benchfmt bench target with stock rustc (LLVM) and
# with the pliron backend, run COUNT steady-state -benchtime epochs of each
# kernel under both, then compare with benchstat (median ± spread, Mann-Whitney
# U p-values, geomean). Raw benchfmt output lands in target/bench/{llvm,pliron}.txt
# and per-kernel medians append to bench/history.tsv so parity can be tracked
# over time (commit it when it moves a number).
#
# Extra args are forwarded to both binaries, e.g.
#   bench/run.sh tokenize        # run a subset by name substring
#   BENCHTIME=1s COUNT=4 bench/run.sh
#   bench/run.sh -cpuprofile /tmp/cpu.pb.gz
# r[impl verify.perf.runtime]
# r[verify verify.perf.runtime]
set -euo pipefail
cd "$(dirname "$0")/.."

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) lib=rustc_codegen_pliron.dll ;;
  Darwin)               lib=librustc_codegen_pliron.dylib ;;
  *)                    lib=librustc_codegen_pliron.so ;;
esac

BENCHTIME=${BENCHTIME:-2s}
COUNT=${COUNT:-6}
OUT=target/bench
mkdir -p "$OUT"

cargo build  # the backend dylib

# uv-managed python if present; else python3/python (skip the Windows alias stub)
if command -v uv >/dev/null; then
  PY=(uv run --no-project python)
else
  PY=(python3)
  command -v "${PY[0]}" >/dev/null && "${PY[@]}" -c pass >/dev/null 2>&1 || PY=(python)
fi

# Compile the bench target and print the resulting binary path. Args after the
# cargo options go to the bench crate's rustc invocation only, so
# -Zcodegen-backend covers the kernels without rebuilding benchfmt.
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

benchstat_bin() {
  if command -v benchstat >/dev/null; then
    echo benchstat
  elif [ -x "$HOME/go/bin/benchstat" ]; then
    echo "$HOME/go/bin/benchstat"
  fi
}

llvm_bin=$(bench_bin)
"$llvm_bin" -benchtime "$BENCHTIME" -count "$COUNT" -cpu 1 -benchmem "$@" \
  | tee "$OUT/llvm.txt"

# Cargo's fingerprint does not cover the backend dylib, so without this the
# flag-identical second build would reuse the stale artifact. `cargo rustc`
# args are fingerprinted though, so only the bench crate itself is removed.
cargo clean -p pliron-bench --profile bench
plir_bin=$(bench_bin -Z "codegen-backend=$PWD/target/debug/$lib")
"$plir_bin" -benchtime "$BENCHTIME" -count "$COUNT" -cpu 1 -benchmem "$@" \
  | tee "$OUT/pliron.txt"

if bs=$(benchstat_bin); then
  "$bs" "$OUT/llvm.txt" "$OUT/pliron.txt"
else
  echo "note: install benchstat for the comparison table:" >&2
  echo "  go install golang.org/x/perf/cmd/benchstat@latest" >&2
fi

# Append per-kernel epoch medians to the history log.
HIST=bench/history.tsv
[ -f "$HIST" ] || printf '# date\tcommit\thost\timpl\tkernel\tns_per_op\tb_per_op\tallocs_op\n' > "$HIST"
TS=$(date +%Y-%m-%dT%H:%M:%S)
SHA=$(git rev-parse --short HEAD 2>/dev/null || echo '?')
HOST=$(uname -sm | tr ' ' '-')
for impl in llvm pliron; do
  "${PY[@]}" - "$OUT/$impl.txt" "$TS" "$SHA" "$HOST" "$impl" >> "$HIST" <<'EOF'
import statistics, sys

path, ts, sha, host, impl = sys.argv[1:6]
rows = {}
for line in open(path):
    f = line.split()
    if len(f) < 4 or not f[0].startswith("Benchmark"):
        continue
    name = f[0][len("Benchmark"):].rsplit("-", 1)[0].lower()
    try:
        ns = float(f[2])
    except ValueError:
        continue
    b_op = a_op = ""
    for i, tok in enumerate(f):
        if tok == "B/op":
            b_op = f[i - 1]
        elif tok == "allocs/op":
            a_op = f[i - 1]
    rows.setdefault(name, []).append((ns, b_op, a_op))
for name, epochs in rows.items():
    ns = statistics.median(e[0] for e in epochs)
    b_op = next((e[1] for e in epochs if e[1]), "")
    a_op = next((e[2] for e in epochs if e[2]), "")
    print(f"{ts}\t{sha}\t{host}\t{impl}\t{name}\t{ns:.0f}\t{b_op}\t{a_op}")
EOF
done
echo "appended medians to $HIST" >&2
