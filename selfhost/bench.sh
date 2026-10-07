#!/usr/bin/env bash
# Usage: bench.sh [label] -- best-of-3 stage2 rustc on regex-syntax (metadata, link) + instructions (link)
RC=${RC:-$HOME/work/rust-sh/build/x86_64-unknown-linux-gnu/stage2/bin/rustc}
SRC=$(ls -d ~/.cargo/registry/src/*/regex-syntax-0.8.10)/src/lib.rs
O=$(mktemp -d)
best() { local b=999; for i in 1 2 3; do s=$(date +%s.%N); "$RC" --edition 2021 --crate-type lib "$@" "$SRC" --out-dir $O -Clinker=$HOME/work/rustc_codegen_pliron/selfhost/cc-wild >/dev/null 2>&1 || { echo FAIL >&2; }; e=$(date +%s.%N); b=$(echo "$e - $s" | bc | awk -v b=$b '{print ($1<b)?$1:b}'); done; printf %.2f $b; }
m=$(best --emit=metadata); l=$(best --emit=link)
ins() { perf stat -x, -e instructions:u "$RC" --edition 2021 --crate-type lib "$@" "$SRC" --out-dir $O -Clinker=$HOME/work/rustc_codegen_pliron/selfhost/cc-wild 2>&1 >/dev/null | awk -F, '/instructions/{printf "%.2fB", $1/1e9}'; }
im=$(ins --emit=metadata); il=$(ins --emit=link)
echo "${1:-run}: metadata ${m}s link ${l}s instructions:u metadata ${im} link ${il}"
rm -rf $O
