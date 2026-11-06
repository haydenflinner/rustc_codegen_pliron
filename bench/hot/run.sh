#!/bin/bash
# PLIRON_HOT scaling run: a small app bin over a large generated leaf
# crate. Builds the leaf thunked, runs the app, then for each --edit
# round rewrites leaf::probe, rebuilds the leaf to one patch object and
# drops it into PLIRON_HOT_DIR while the app runs.
#
#   bench/hot/run.sh [--fns N] [--rounds K] [--free]
#
# Reports per round: patch build time, apply time ([hot] log), and the
# app's RSS. Pass --keep to keep target/hot-leaf, --free to enable
# PLIRON_HOT_FREE map reclamation.
set -euo pipefail
B="$(cd "$(dirname "$0")/../.." && pwd)"
export RUSTUP_TOOLCHAIN=nightly-2026-10-22
N=4000; ROUNDS=3; KEEP=0; FREE=0
while [[ $# -gt 0 ]]; do case "$1" in
    --fns) N=$2; shift 2;; --rounds) ROUNDS=$2; shift 2;;
    --keep) KEEP=1; shift;; --free) FREE=1; shift;;
    *) echo "unknown arg $1"; exit 1;;
esac; done
case "$(uname)" in Darwin) DYLIB=dylib;; *) DYLIB=so;; esac
W="$B/target/hot-leaf"; WS="$B/bench/hot/ws"
rm -rf "$W"; mkdir -p "$W/patches"
pkill -f "hot-leaf/build/debug/hot_app" 2>/dev/null || true
python3 "$B/bench/hot/gen_leaf.py" "$WS/leaf/src/lib.rs" "$N"
RUSTFLAGS="-Zcodegen-backend=$B/target/debug/librustc_codegen_pliron.$DYLIB -Ccodegen-units=1"
t0=$(date +%s.%N)
(cd "$WS/app" && PLIRON_HOT=leaf RUSTFLAGS="$RUSTFLAGS" cargo build -q --target-dir "$W/build")
echo "base build: $(echo "$(date +%s.%N) - $t0" | bc | cut -c1-6)s"
APPBIN="$W/build/debug/hot_app"
(cd "$WS/app" && RUSTFLAGS="$RUSTFLAGS" cargo rustc -q --target-dir "$W/build" -p leaf --lib -- --emit=obj="$W/patches/base.ref")
HOT_FREE=""
[[ $FREE == 1 ]] && HOT_FREE=1
PLIRON_HOT_DIR="$W/patches" ${HOT_FREE:+PLIRON_HOT_FREE=1} "$APPBIN" > "$W/out.txt" 2> "$W/err.txt" & APP=$!
trap 'kill $APP 2>/dev/null' EXIT
sleep 0.5
kill -0 "$APP" 2>/dev/null || { echo "app failed to start"; cat "$W/err.txt"; exit 1; }
rss() { ps -o rss= -p "$APP" 2>/dev/null | tr -d ' '; }
for r in $(seq 1 "$ROUNDS"); do
    python3 "$B/bench/hot/edit_probe.py" "$WS/leaf/src/lib.rs" \
        "$((1000 * r)) + COUNTER.load(Ordering::Relaxed)"
    t0=$(date +%s.%N)
    (cd "$WS/app" && RUSTFLAGS="$RUSTFLAGS" cargo rustc -q --target-dir "$W/build" -p leaf --lib -- --emit=obj="$W/p.o")
    bt=$(echo "$(date +%s.%N) - $t0" | bc | cut -c1-6)
    tag=$(printf %04d "$r").o
    mv "$W/p.o" "$W/patches/$tag"
    for _ in $(seq 1 200); do grep -q "$tag.*functions patched" "$W/err.txt" && break; sleep 0.05; done
    sleep 0.2
    line=$(grep "$tag.*functions patched" "$W/err.txt" | tail -1)
    echo "round $r: patch build ${bt}s, $line, app rss $(rss)KB"
    tail -1 "$W/out.txt"
done
kill $APP 2>/dev/null || true
[[ $KEEP == 0 ]] && rm -rf "$W"
