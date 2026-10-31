#!/usr/bin/env bash
# Record a CLIF corpus: every function's pre-pipeline CLIF + facts, as the
# backend sees it at -O (PLIRON_CORPUS). Record once, then replay it with
# `clif-check report|corpus|bench` in seconds without rustc — see
# harness/live/README.md.
#
#   harness/corpus.sh smoke              tests/*/main.rs        -> target/corpus/smoke
#   harness/corpus.sh std                std+alloc+core (-Zbuild-std, release) -> target/corpus/std
#   harness/corpus.sh cargo DIR [ARGS]   any cargo project (release) -> target/corpus/<basename DIR>
#
# OUT=<dir> overrides the destination. The backend is target/debug's dylib
# (build it first). Re-recording replaces the set.
# r[impl verify.corpus.record]
# r[verify verify.corpus.record]
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
# The scratch projects below live outside the repo: pin its toolchain.
export RUSTUP_TOOLCHAIN=$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)
case "$(uname -s)" in
  Darwin) lib=librustc_codegen_pliron.dylib ;;
  *)      lib=librustc_codegen_pliron.so ;;
esac
BE="-Zcodegen-backend=$ROOT/target/debug/$lib"
HOST=$(rustc -vV | sed -n 's/host: //p')
export RUSTC_ICE=${RUSTC_ICE:-$ROOT/target/ices}
set_=${1:?usage: corpus.sh smoke|std|cargo DIR}
shift
out=${OUT:-$ROOT/target/corpus/$set_}
[[ $set_ == cargo ]] && out=${OUT:-$ROOT/target/corpus/$(basename "${1:?cargo DIR}")}
rm -rf "$out"; mkdir -p "$out"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

case "$set_" in
  smoke)
    for src in tests/*/main.rs; do
      t=$(basename "$(dirname "$src")")
      # Each test's own subdir: the dump's per-process counter restarts at 0.
      PLIRON_CORPUS="$out/$t" rustc $BE --edition 2021 -O -C codegen-units=1 \
        "$src" -o "$tmp/$t" >/dev/null 2>&1 || echo "corpus: $t did not build (skipped)" >&2
    done ;;
  std)
    mkdir -p "$tmp/p/src"
    printf '[package]\nname="corpusstd"\nversion="0.0.0"\nedition="2021"\n[workspace]\n' > "$tmp/p/Cargo.toml"
    echo 'fn main() { println!("{:?}", std::collections::HashMap::<u8, String>::new()); }' > "$tmp/p/src/main.rs"
    (cd "$tmp/p" && RUSTFLAGS="$BE" PLIRON_CORPUS="$out" CARGO_TARGET_DIR="$tmp/t" \
      cargo build -q --release --target "$HOST" -Zbuild-std=std,panic_abort) ;;
  cargo)
    dir=$1; shift
    (cd "$dir" && RUSTFLAGS="$BE" PLIRON_CORPUS="$out" CARGO_TARGET_DIR="$tmp/t" \
      cargo build -q --release "$@") ;;
  *) echo "corpus.sh: unknown set '$set_'" >&2; exit 2 ;;
esac
n=$(find "$out" -name '*.clif' | wc -l | tr -d ' ')
echo "corpus: $n functions in $out"
[[ $n -gt 0 ]] || { echo "corpus: nothing recorded" >&2; exit 1; }
