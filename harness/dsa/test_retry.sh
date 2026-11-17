#!/usr/bin/env bash
# test_retry.sh — build.sh reruns cargo when earlyoom kills a rustc.
#
# A fake cargo stands in for the real one (DSA_CARGO): its first run
# prints what cargo prints when a rustc dies of SIGTERM, followed by a
# long command line as cargo does (so the signal line is far from the end
# of the log), and exits 101; its second run succeeds. build.sh must
# retry once and finish. A second case fails with a plain compile error
# and must not be retried. The old test, `tail | grep -q` under
# `set -o pipefail`, is checked to miss the first log: that is the bug.
set -euo pipefail
H=$(cd "$(dirname "$0")" && pwd)
T=$(mktemp -d "${TMPDIR:-/tmp}/dsa-retry.XXXXXX")
trap 'rm -rf "$T"' EXIT
fail() { echo "test_retry: FAIL: $*" >&2; exit 1; }

# A cargo whose behaviour per call is read from $T/plan (one word per call).
cat > "$T/cargo" <<'EOF'
#!/usr/bin/env bash
n=$(( $(cat "$FAKE_DIR/calls" 2>/dev/null || echo 0) + 1 )); echo $n > "$FAKE_DIR/calls"
what=$(sed -n "${n}p" "$FAKE_DIR/plan")
case $what in
  sigterm)
    echo "   Compiling polars-core v0.55.1"
    echo "error: could not compile \`polars-core\` (lib)"
    echo
    echo "Caused by:"
    printf '  process didn'"'"'t exit successfully: `rustc --crate-name polars_core'
    head -c 200000 /dev/zero | tr '\0' 'x'
    echo '` (signal: 15, SIGTERM: termination signal)'
    # The rest of cargo's output after the signal line: tens of KB.
    for i in $(seq 1 2000); do echo "warning: build failed, waiting for other jobs to finish... $i"; done
    exit 101 ;;
  error)
    echo "error[E0308]: mismatched types"
    echo "error: could not compile \`x\` (lib) due to 1 previous error"
    exit 101 ;;
  ok)
    d=$CARGO_TARGET_DIR/wasm32-unknown-unknown/release; mkdir -p "$d"
    printf '\0asm\1\0\0\0' > "$d/literate_wasm.wasm"
    exit 0 ;;
esac
EOF
cat > "$T/wasm-bindgen" <<'EOF'
#!/usr/bin/env bash
while [ $# -gt 0 ]; do [ "$1" = --out-dir ] && { mkdir -p "$2"; out=$2; }; shift; done
printf '\0asm\1\0\0\0' > "$out/literate_wasm_bg.wasm"
EOF
chmod +x "$T/cargo" "$T/wasm-bindgen"
mkdir -p "$T/host"

run() { # run PLAN... ; runs build.sh llvm with the fake tools, sets rc
  printf '%s\n' "$@" > "$T/plan"; rm -f "$T/calls"
  set +e
  FAKE_DIR=$T DSA_CARGO=$T/cargo WASM_BINDGEN=$T/wasm-bindgen DSA_BENCH=$T/bench \
    DSA_HOST=$T/host DSA_RETRY_SLEEP=0 bash "$H/build.sh" llvm t > "$T/out" 2>&1
  rc=$?
  set -e
}

# 1. Killed rustc, then success: one retry, exit 0.
run sigterm ok
[ "$rc" = 0 ] || { cat "$T/out"; fail "build.sh exited $rc after a SIGTERM'd rustc"; }
[ "$(cat "$T/calls")" = 2 ] || fail "cargo ran $(cat "$T/calls") times, want 2"
grep -q "attempt 1 lost a process to a signal" "$T/bench/logs/llvm-t.log" ||
  fail "no retry message in the log"

# The old test misses this log under pipefail (the bug this fixes).
log=$T/bench/logs/llvm-t.log
( set -o pipefail; tail -n +1 "$log" | grep -q 'signal: 15' ) &&
  echo "test_retry: note: the old tail|grep test happened to see the signal this time" ||
  echo "test_retry: the old tail|grep test misses the signal line (status 141 under pipefail)"

# 2. A compile error is not retried.
run error ok
[ "$rc" != 0 ] || fail "build.sh succeeded after a compile error"
[ "$(cat "$T/calls")" = 1 ] || fail "a compile error was retried ($(cat "$T/calls") calls)"

# 3. lost_to_signal on its own: cargo killed itself (rc >= 128) counts.
. "$H/retry.sh"
: > "$T/empty.log"
lost_to_signal "$T/empty.log" 0 137 || fail "rc 137 not seen as a signal"
lost_to_signal "$T/empty.log" 0 101 && fail "rc 101 with no signal line seen as a signal"

echo "test_retry: OK"
