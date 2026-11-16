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
#                    The set the 2026-11-16 comparison used:
#                    DSA_FEATURES="--no-default-features --features eval,squish,cad"
#   DSA_CARGO_ARGS   extra cargo args, e.g. --config profile.release.lto=false
#   DSA_JOBS         cargo -j                         (5)
#   DSA_WIPE=1       pliron: remove the wasm32 half of the target dir first (see below)
#   DSA_BACKEND_RAW=1  pliron: pass $PLIRON_BACKEND unkeyed (single-crate bisects, see below)
#   DSA_RUSTC_WRAPPER  rustc wrapper for cargo, e.g. sccache (default none: see below)
#   DSA_RETRIES      reruns after a signal-killed cargo/rustc (30; see earlyoom below)
#   DSA_PLIRON_FALLBACK  PLIRON_* settings for a second pass (see below; default none)
#   PLIRON_BACKEND   backend dylib   (<repo>/target/debug/...). The build loads a keyed
#                    private copy of it (below), so the backend can be rebuilt meanwhile.
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
#
# Effective profile, both sides: opt-level 3 (opt-level "z" for the
# polars/vortex/typst crates of the default feature set), codegen-units 1,
# strip=debuginfo (function names stay), panic=abort (target default), and
# NO LTO: the workspace says `lto = "thin"`, but literate-wasm is
# `crate-type = ["cdylib", "rlib"]` and cargo passes no `-C lto` to such a
# unit (`cargo -v`: `--crate-type cdylib --crate-type rlib -C opt-level=3
# -C codegen-units=1 -C strip=debuginfo`).
#
# Cargo does not track the backend dylib or PLIRON_* variables, so the flag
# is `-Zcodegen-backend=<repo>/target/backends/<key>/...`, where <key> names
# the dylib's contents and the code-affecting PLIRON_* variables
# (harness/backend_path.py). A rebuild with neither changed compiles nothing
# (no-op 1-2 s); with either changed cargo recompiles the wasm32 half, and
# the helper's `--gc` drops the units of the previous backend from $target
# (about 650 MB each for the eval set; host proc macros and build scripts
# carry no flag and are kept). DSA_WIPE=1 removes $target/wasm32-unknown-unknown
# first, as every build did before 2026-11-16: use it to get the side
# output of a knob again (PLIRON_CORPUS, PLIRON_WASM_STUBLOG, any of the
# reporting knobs the key leaves out), which only a rustc that runs writes.
#
# sccache: the same path makes a rustc wrapper safe on the pliron side (its
# key holds the flag string, not the dylib or PLIRON_*). It stays off by
# default so cargo_secs in BUILDINFO is a compile time; DSA_RUSTC_WRAPPER=sccache
# turns it on (BUILDINFO then says `wrapper=sccache`).
#
# Two-pass fallback. With DSA_PLIRON_FALLBACK set (e.g. "PLIRON_BUNROLL=0"),
# pass 1 runs `cargo build --keep-going` under the caller's environment and
# compiles everything that can be; if crates failed, pass 2 exports the
# fallback settings and builds the rest (the crates that panicked the backend
# and everything downstream of them); BUILDINFO lists them
# (`fallback_crates`). Both passes use one backend path, keyed on the
# caller's environment plus the fallback string, so pass 2 does not redo
# pass 1 and a later run without the fallback does not reuse its crates.
# dsa-snap (03:07) needed PLIRON_HALFUNROLL=0
# (regex-syntax), PLIRON_INDUCT2=0 (jiff), PLIRON_BUNROLL=0 (rapier2d) and
# PLIRON_TAILREC=0 (literate-eval) this way; from dsa-snap4 on the eval
# feature set compiles with no knob, so the default is no fallback.
# History and repros: $DSA_BENCH/findings/STATUS.md.
#
# Times, features eval,squish,cad, 14-core M-series shared with other builds
# (so +-50%), 2026-11-16; each package's BUILDINFO has its own numbers:
#   llvm    cold (host deps included)  306 s at -j 3;  no-op rebuild 1-2 s;
#           default features, cold     269 s at -j 5
#   pliron  wasm32 half rebuilt, host deps kept ("warm", what a rebuild with
#           a changed backend or knob does)
#                                      218-440 s at -j 3, 288 s at -j 4
#           cold adds the host proc macros/build scripts, about 1 min.
#   Peak rustc footprint under pliron is 4-7 GB for the big crates
#   (loro-internal 4.1 GB, literate-eval 5.5 GB, polars-core 6.8 GB):
#   more than two such builds at once trip the machine's earlyoom.
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

# The global ~/.cargo/config.toml sets rustc-wrapper = "sccache". An empty
# RUSTC_WRAPPER turns it off so the build time is a compile time, on both
# sides; DSA_RUSTC_WRAPPER names one to use (safe with the keyed path below).
export RUSTC_WRAPPER=${DSA_RUSTC_WRAPPER-} GIT_OPTIONAL_LOCKS=0 RUSTC_ICE=$DSA_BENCH/ices
export CARGO_TARGET_DIR=$target

args=(build --release --locked --target wasm32-unknown-unknown -p literate-wasm -j "$DSA_JOBS")
# shellcheck disable=SC2206
args+=($DSA_FEATURES $DSA_CARGO_ARGS)

case $side in
  llvm)
    # Stock rustc, prebuilt std, rustc's own wasm-ld (rust-lld).
    ;;
  pliron)
    PLIRON_BACKEND=${PLIRON_BACKEND:-$ROOT/target/debug/librustc_codegen_pliron.dylib}
    PLIRON_WASM_LD=${PLIRON_WASM_LD:-$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld}
    [ -f "$PLIRON_BACKEND" ] || { echo "no backend dylib at $PLIRON_BACKEND" >&2; exit 2; }
    [ -x "$PLIRON_WASM_LD" ] || { echo "no linker at $PLIRON_WASM_LD" >&2; exit 2; }
    if [ "${DSA_WIPE:-0}" = 1 ]; then rm -rf "$target/wasm32-unknown-unknown"; fi
    # The keyed copy: its path changes when the dylib or a code-affecting
    # PLIRON_* variable does, and only then. The fallback string is part of
    # the key because pass 2 compiles under this same path.
    # DSA_BACKEND_RAW=1 passes $PLIRON_BACKEND itself, for bisecting one
    # crate: cargo then does not notice a changed knob or dylib, so a
    # knob reaches only the crates whose build dirs the caller removed
    # (`rm -rf $target/wasm32-unknown-unknown/release/build/<crate>`) and
    # their dependents; everything else stays as built.
    if [ "${DSA_BACKEND_RAW:-0}" = 1 ]; then
      backend_keyed=$PLIRON_BACKEND
    else
      backend_keyed=$("$ROOT/harness/backend_path.py" --dylib "$PLIRON_BACKEND" \
        --salt "${DSA_PLIRON_FALLBACK:+fallback:$DSA_PLIRON_FALLBACK}" --gc "$target")
    fi
    export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="-Zcodegen-backend=$backend_keyed -Clinker=$PLIRON_WASM_LD"
    # std at one codegen unit, as bench/wasm/run.sh builds it (the dsa
    # release profile already says codegen-units = 1 for everything).
    args+=(-Zbuild-std=std,panic_abort --config 'profile.release.package."*".codegen-units=1')
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
    # Still in use: the helper prunes keyed copies by last use.
    [ -z "${backend_keyed:-}" ] || touch "$(dirname "$backend_keyed")"
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
  echo "wrapper=${RUSTC_WRAPPER:-<none>}"
  if [ "$side" = pliron ]; then
    echo "backend=$PLIRON_BACKEND"
    echo "backend_keyed=$backend_keyed"
    echo "backend_sha256=$(shasum -a 256 "$backend_keyed" | cut -d' ' -f1)"
    echo "linker=$PLIRON_WASM_LD"
    echo "env=${pass1_env:-<backend defaults>}"
    echo "fallback_env=${fallback_crates:+$DSA_PLIRON_FALLBACK}"
    echo "fallback_crates=$fallback_crates"
  fi
} > "$pkg/BUILDINFO"
cat "$pkg/BUILDINFO"
