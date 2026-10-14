#!/usr/bin/env bash
# pgo_loop.sh <phase> — self-hosted rustc PGO measurement loop.
#
# Phases (run in order):
#   base      — bench.sh the current stage2 rustc (baseline)
#   instr     — rebuild stage2 crates instrumented (PLIRON_PGO=1);
#               every rustc it spawns then leaves pliron-<pid>.prof
#               in its cwd (scattered under the tree — find collects)
#   workload  — run the instrumented stage2 rustc on a representative
#               compile workload to collect counts, merge all .prof
#   pgo       — rebuild stage2 with PLIRON_PGO_USE=<merged>
#   bench     — bench.sh the PGO'd stage2 rustc
set -e
cd "$HOME/work/rust-sh"
B=build/x86_64-unknown-linux-gnu
RC=$B/stage2/bin/rustc
MERGED=$PWD/pgo-merged.prof
PROF=$HOME/repos/rustc-codegen-pliron/tools/pliron-prof/target/release/pliron-prof

case "${1:?phase}" in
base)
    "$HOME/repos/rustc-codegen-pliron/selfhost/bench.sh" base
    ;;
instr)
    rm -rf "$B/stage2-rustc" pgo-profs
    # PLIRON_TIMING records per-CGU per-step wall time on stderr —
    # answers "which pass is slow on the giant crates" for free.
    PLIRON_PGO=1 PLIRON_TIMING=1 ./x build --stage 2 compiler
    mkdir -p pgo-profs && find . -name 'pliron-*.prof' -exec mv {} pgo-profs/ \; \
        && ls pgo-profs | wc -l
    ;;
workload)
    SRC=$(ls -d ~/.cargo/registry/src/*/regex-syntax-0.8.10)/src/lib.rs
    for i in 1 2 3; do
        "$RC" --edition 2021 --crate-type lib "$SRC" -o /tmp/pgo_wl_$i.rlib \
            -Clinker="$HOME/repos/rustc-codegen-pliron/selfhost/cc-wild"
    done
    find . -name 'pliron-*.prof' -exec mv {} pgo-profs/ \; 2>/dev/null
    "$PROF" merge pgo-profs/pliron-*.prof -o "$MERGED" && ls -la "$MERGED"
    ;;
pgo)
    rm -rf "$B/stage2-rustc"
    PLIRON_PGO_USE="$MERGED" ./x build --stage 2 compiler
    ;;
bench)
    "$HOME/repos/rustc-codegen-pliron/selfhost/bench.sh" pgo
    ;;
esac
