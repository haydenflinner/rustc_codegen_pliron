#!/usr/bin/env bash
# bench/run.sh [REPS] — build each bench kernel with stock rustc and with the
# pliron backend, run each REPS times (default 3), print best-of times in ms.
set -uo pipefail
cd "$(dirname "$0")/.."
REPS=${1:-3}
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) lib=rustc_codegen_pliron.dll; exe=.exe ;;
  Darwin)               lib=librustc_codegen_pliron.dylib; exe= ;;
  *)                    lib=librustc_codegen_pliron.so; exe= ;;
esac
BE="-Zcodegen-backend=$PWD/target/debug/$lib"
out=target/bench; mkdir -p "$out"
best() { local b=999999 t
  for _ in $(seq "$REPS"); do
    t=$("$1" 2>/dev/null | awk '{print $1; exit}')
    awk -v a="${t:-999999}" -v b="$b" 'BEGIN{exit !(a+0<b)}' && b=$t
  done; echo "$b"; }
printf '%-10s %10s %10s %8s %12s %12s\n' kernel llvm-ms pliron-ms ratio compile-llvm compile-plir
for src in bench/*.rs; do
  k=$(basename "$src" .rs)
  rm -f "$out/$k.llvm$exe" "$out/$k.plir$exe"
  llvm_t=$( { time rustc -O "$src" -o "$out/$k.llvm$exe" 2>/dev/null; } 2>&1 | awk '/real/{print $2}' )
  plir_t=$( { time rustc -O $BE "$src" -o "$out/$k.plir$exe" 2>/dev/null; } 2>&1 | awk '/real/{print $2}' )
  # Skip timing if a compile failed — a stale binary would report phantom numbers.
  L=-; P=-
  [ -x "$out/$k.llvm$exe" ] && L=$(best "$out/$k.llvm$exe")
  [ -x "$out/$k.plir$exe" ] && P=$(best "$out/$k.plir$exe")
  r=$(awk -v l="$L" -v p="$P" 'BEGIN{if(l+0>0&&p+0>0)printf "%.2f", p/l; else print "-"}')
  printf '%-10s %10s %10s %8s %12s %12s\n' "$k" "$L" "$P" "$r" "$llvm_t" "$plir_t"
done
