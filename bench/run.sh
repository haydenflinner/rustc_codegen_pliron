#!/usr/bin/env bash
# bench/run.sh — build the criterion bench target with stock rustc (LLVM) and
# with the pliron backend, then compare the two. The LLVM numbers are saved as
# the 'llvm' baseline and the pliron binary is run against it, so criterion
# prints per-kernel change and significance instead of a best-of-N table.
#
# Extra args are forwarded to both criterion binaries, e.g.
#   bench/run.sh tokenize        # run a subset by name filter
#   bench/run.sh --verbose
# r[impl verify.perf.runtime]
# r[verify verify.perf.runtime]
set -euo pipefail
cd "$(dirname "$0")/.."

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) lib=rustc_codegen_pliron.dll ;;
  Darwin)               lib=librustc_codegen_pliron.dylib ;;
  *)                    lib=librustc_codegen_pliron.so ;;
esac

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
# -Zcodegen-backend covers the kernels without rebuilding criterion.
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

llvm_bin=$(bench_bin)
"$llvm_bin" --bench --save-baseline llvm "$@"

# Cargo's fingerprint does not cover the backend dylib, so without this the
# flag-identical second build would reuse the stale artifact. `cargo rustc`
# args are fingerprinted though, so only the bench crate itself is removed.
cargo clean -p pliron-bench --profile bench
plir_bin=$(bench_bin -Z "codegen-backend=$PWD/target/debug/$lib")
"$plir_bin" --bench --baseline llvm "$@"
