#!/usr/bin/env bash
# Build the dsa `literate-wasm` crate (wasm-bindgen cdylib: Loro CRDT page +
# the Mimas/"Nina" interpreter) to wasm32 with stock rustc+LLVM or with the
# pliron backend, then run wasm-bindgen, for harness/dsa/bench.mjs.
#
#   harness/dsa/build.sh llvm [NAME]     -> $DSA_BENCH/pkg-llvm[-NAME]
#   harness/dsa/build.sh pliron [NAME]   -> $DSA_BENCH/pkg-pliron[-NAME]
#
# Nothing is written into the dsa tree: CARGO_TARGET_DIR is
# $DSA_BENCH/target-<side>[-NAME], `--locked` keeps Cargo.lock as it is, and
# GIT_OPTIONAL_LOCKS=0 stops the crate's build.rs (`git status` for the
# source stamp) from refreshing the dsa index.
#
# Environment:
#   DSA_BENCH        output root                      (/tmp/dsa-bench)
#   DSA_HOST         the dsa `host` workspace         ($HOME/code/dsa/host)
#   DSA_FEATURES     cargo feature flags, both sides  ("" = the crate's default set)
#                    e.g. DSA_FEATURES="--no-default-features --features eval"
#   DSA_CARGO_ARGS   extra cargo args, e.g. --config profile.release.lto=false
#   DSA_JOBS         cargo -j                         (5)
#   DSA_KEEP=1       pliron: keep the wasm32 half of the target dir (see below)
#   DSA_RETRIES      reruns after a signal-killed cargo/rustc (30; see earlyoom below)
#   DSA_PLIRON_FALLBACK  PLIRON_* settings for pass 2 (see below)
#   PLIRON_BACKEND   backend dylib   (<repo>/target/dsa-snap6/librustc_codegen_pliron.dylib)
#   PLIRON_WASM_LD   linker          (<repo>/tools/pliron-wasm-ld/target/release/pliron-wasm-ld)
#   WASM_BINDGEN     wasm-bindgen CLI; must be 0.2.127 to match the crate
#   PLIRON_*         every other PLIRON_* variable reaches the pliron rustc
#                    processes unchanged (and is recorded in BUILDINFO).
#
# pliron side: std is rebuilt through the backend (-Zbuild-std; prebuilt std
# rlibs are LLVM objects pliron-wasm-ld cannot read). The flags go through
# CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS so host build scripts and
# proc macros stay on LLVM; cargo merges that variable with the workspace's
# `--cfg getrandom_backend="wasm_js"` (checked with `cargo -v`).
# The workspace release profile is `lto = "thin"`, `codegen-units = 1`,
# opt-level 3 (opt-level "z" for the polars/vortex/typst crates). The backend
# claims no ThinLTO, so rustc falls back to its fat-LTO path for the cdylib.
#
# Cargo does not track the backend dylib or PLIRON_* variables, so a pliron
# build first removes $target/wasm32-unknown-unknown (host proc macros and
# build scripts are kept). DSA_KEEP=1 skips that — only correct when neither
# the dylib nor any PLIRON_* variable changed.
#
# The snapshot backends cannot compile every crate of this tree with their
# defaults, so a pliron build is two passes. Pass 1 runs `cargo build
# --keep-going` under the caller's environment and compiles everything that
# can be. If crates failed, pass 2 exports DSA_PLIRON_FALLBACK (default
# below) and builds the rest: the crates that panicked the backend and
# everything downstream of them. BUILDINFO lists those crates
# (`fallback_crates`). DSA_PLIRON_FALLBACK="" turns pass 2 off, so the build
# stops at the backend panics; to apply a knob to every crate instead,
# export it yourself. The panics (logs naming the function are in
# $DSA_BENCH/findings/, see STATUS.md and REPRO.md there):
#   PLIRON_BUNROLL=0     `bunroll` broke `rapier2d::utils::
#                        smallest_abs_diff_between_angles::<WideF32x4>`:
#                        use of a non-dominating value.
#   PLIRON_TAILREC=0     literate-eval: pliron-clif-opt/src/tailrec.rs:40
#                        "attempt to subtract with overflow" (`accop`'s Smin
#                        identity `(1i64 << 63) - 1`; the snapshot dylib has
#                        overflow checks on).
# dsa-snap (03:07) also needed PLIRON_HALFUNROLL=0 (`punroll` broke
# `regex_syntax::unicode::class`) and PLIRON_INDUCT2=0 (`licm2` broke five
# jiff functions); dsa-snap2 (b67567f) fixes both.
#
# Times (14-core M-series, shared and loaded), 2026-11-16 — the BUILDINFO
# file of each package has its own numbers:
# TIMES-PLACEHOLDER
set -euo pipefail

side=${1:?usage: build.sh llvm|pliron [NAME]}
name=${2:-}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DSA_BENCH=${DSA_BENCH:-/tmp/dsa-bench}
DSA_HOST=${DSA_HOST:-$HOME/code/dsa/host}
DSA_FEATURES=${DSA_FEATURES-}
DSA_CARGO_ARGS=${DSA_CARGO_ARGS-}
DSA_JOBS=${DSA_JOBS:-5}
DSA_PLIRON_FALLBACK=${DSA_PLIRON_FALLBACK-}
TOOLCHAIN=nightly-2026-10-22
WASM_BINDGEN=${WASM_BINDGEN:-$HOME/Library/Caches/.wasm-pack/wasm-bindgen-cargo-install-0.2.127/wasm-bindgen}

tag=$side${name:+-$name}
target=$DSA_BENCH/target-$tag
pkg=$DSA_BENCH/pkg-$tag
mkdir -p "$DSA_BENCH/logs" "$DSA_BENCH/ices"
log=$DSA_BENCH/logs/$tag.log
rm -f "$DSA_BENCH/logs/$tag.exit"
trap 'echo $? > "$DSA_BENCH/logs/$tag.exit"' EXIT

# The global ~/.cargo/config.toml sets rustc-wrapper = "sccache", which
# returns stale objects for this backend: an empty RUSTC_WRAPPER turns it off.
export RUSTC_WRAPPER= GIT_OPTIONAL_LOCKS=0 RUSTC_ICE=$DSA_BENCH/ices
export CARGO_TARGET_DIR=$target

args=(build --release --locked --target wasm32-unknown-unknown -p literate-wasm -j "$DSA_JOBS")
# shellcheck disable=SC2206
args+=($DSA_FEATURES $DSA_CARGO_ARGS)

case $side in
  llvm)
    # Stock rustc, prebuilt std, rustc's own wasm-ld (rust-lld).
    ;;
  pliron)
    PLIRON_BACKEND=${PLIRON_BACKEND:-$ROOT/target/dsa-snap6/librustc_codegen_pliron.dylib}
    PLIRON_WASM_LD=${PLIRON_WASM_LD:-$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld}
    [ -f "$PLIRON_BACKEND" ] || { echo "no backend dylib at $PLIRON_BACKEND" >&2; exit 2; }
    [ -x "$PLIRON_WASM_LD" ] || { echo "no linker at $PLIRON_WASM_LD" >&2; exit 2; }
    export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="-Zcodegen-backend=$PLIRON_BACKEND -Clinker=$PLIRON_WASM_LD"
    # std at one codegen unit, as bench/wasm/run.sh builds it (the dsa
    # release profile already says codegen-units = 1 for everything).
    args+=(-Zbuild-std=std,panic_abort --config 'profile.release.package."*".codegen-units=1')
    if [ "${DSA_KEEP:-0}" != 1 ]; then rm -rf "$target/wasm32-unknown-unknown"; fi
    ;;
  *) echo "usage: build.sh llvm|pliron [NAME]" >&2; exit 2 ;;
esac

echo "build.sh: $tag -> $pkg (log $log)"
# This machine runs `earlyoom --prefer rustc|cargo|...`: under memory
# pressure (anybody's) it SIGTERMs rustc and cargo processes, ours included,
# whatever their size. A cargo run that died of a signal, or lost a rustc to
# one, is rerun without wiping anything (cargo resumes), up to DSA_RETRIES
# times. cargo_secs in BUILDINFO is the sum over attempts.
: > "$log"
attempts=0
run_cargo() { # run_cargo EXTRA_CARGO_ARGS... ; sets rc
  local n=0 mark
  while :; do
    n=$((n + 1)); attempts=$((attempts + 1))
    mark=$(wc -l < "$log")
    set +e
    (cd "$DSA_HOST" && cargo "+$TOOLCHAIN" "${args[@]}" "$@") >> "$log" 2>&1
    rc=$?
    set -e
    [ $rc -eq 0 ] && return 0
    if [ "$n" -lt "${DSA_RETRIES:-30}" ] &&
       { [ $rc -ge 128 ] || tail -n "+$((mark + 1))" "$log" | grep -q 'signal: 15'; }; then
      echo "build.sh: attempt $n lost a process to a signal (exit $rc); retrying" | tee -a "$log"
      sleep 20
      continue
    fi
    return 0
  done
}
t0=$(date +%s)
fallback_crates=
pass1_env=$(env | grep '^PLIRON_' | grep -v '^PLIRON_BACKEND=\|^PLIRON_WASM_LD=' | sort | tr '\n' ' ' || true)
if [ "$side" = pliron ] && [ -n "$DSA_PLIRON_FALLBACK" ]; then
  # Pass 1: the caller's PLIRON_* environment, every crate that compiles.
  run_cargo --keep-going
  if [ $rc -ne 0 ]; then
    grep -o 'could not compile `[^`]*`' "$log" | sort -u | sed 's/^/build.sh: pass 1: /'
    echo "build.sh: pass 2 with $DSA_PLIRON_FALLBACK" | tee -a "$log"
    mark=$(wc -l < "$log")
    # shellcheck disable=SC2086
    export $DSA_PLIRON_FALLBACK
    run_cargo
    fallback_crates=$(tail -n "+$((mark + 1))" "$log" | sed -n 's/^ *Compiling \([^ ]*\) v.*/\1/p' | sort -u | tr '\n' ' ')
  fi
else
  run_cargo
fi
t1=$(date +%s)
if [ $rc -ne 0 ]; then
  echo "build.sh: cargo failed (exit $rc) after $((t1 - t0)) s, $attempts attempt(s); see $log" >&2
  grep -n "^error" "$log" | cut -c1-300 | head -20 >&2 || true
  exit $rc
fi

raw=$target/wasm32-unknown-unknown/release/literate_wasm.wasm
rm -rf "$pkg.tmp"
# Same options on both sides. No wasm-opt and no name-section strip (the
# project's own `wasm:fast` strips names; profiles need them).
"$WASM_BINDGEN" --target web --out-dir "$pkg.tmp" --out-name literate_wasm "$raw" >> "$log" 2>&1
rm -rf "$pkg"
mv "$pkg.tmp" "$pkg"
t2=$(date +%s)

{
  echo "side=$side"
  echo "name=$name"
  echo "date=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "features=${DSA_FEATURES:-<default>}"
  echo "cargo_args=${DSA_CARGO_ARGS:-<none>}"
  echo "cargo_secs=$((t1 - t0))"
  echo "cargo_attempts=$attempts"
  echo "jobs=$DSA_JOBS"
  echo "bindgen_secs=$((t2 - t1))"
  echo "raw_wasm_bytes=$(stat -f %z "$raw")"
  echo "pkg_wasm_bytes=$(stat -f %z "$pkg/literate_wasm_bg.wasm")"
  echo "dsa_head=$(git -C "$DSA_HOST" rev-parse --short=12 HEAD 2>/dev/null || echo ?)"
  if [ "$side" = pliron ]; then
    echo "backend=$PLIRON_BACKEND"
    echo "backend_sha256=$(shasum -a 256 "$PLIRON_BACKEND" | cut -d' ' -f1)"
    echo "linker=$PLIRON_WASM_LD"
    echo "env=${pass1_env:-<backend defaults>}"
    echo "fallback_env=${fallback_crates:+$DSA_PLIRON_FALLBACK}"
    echo "fallback_crates=$fallback_crates"
  fi
} > "$pkg/BUILDINFO"
cat "$pkg/BUILDINFO"
