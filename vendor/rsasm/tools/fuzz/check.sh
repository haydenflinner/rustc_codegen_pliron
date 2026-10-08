#!/bin/bash
# Ad-hoc three-way check: rsasm, GNU as and llvm-mc on one instruction each,
# read from stdin (one per line).
#
#   tools/fuzz/check.sh [mode] [syntax] < cases
#
# mode is 64 (default), 32 or 16; syntax is att (default) or intel.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin="${RSASM_ORACLES:-$root/target/oracles}/bin"
gas="$bin/x86_64-elf-as"
mode=${1:-64}
syntax=${2:-att}
case $mode in
  64) gasflags=--64 arch=x86-64 triple=x86_64 hdr= ;;
  32) gasflags=--32 arch=i386 triple=i386 hdr= ;;
  16) gasflags=--32 arch=i386 triple=i386 hdr=".code16" ;;
esac
if [ "$syntax" = intel ]; then hdr="$hdr
.intel_syntax noprefix"; fi
d=$(mktemp -d)
trap 'rm -rf "$d"' EXIT
while IFS= read -r line; do
  [ -n "$line" ] || continue
  printf '%s\n%s\n' "$hdr" "$line" > "$d/in.s"
  if "$gas" $gasflags -o "$d/o.o" "$d/in.s" 2> "$d/err"; then
    g=$(objcopy -O binary --only-section=.text "$d/o.o" "$d/o.bin" 2>/dev/null;
        xxd -p "$d/o.bin" | tr -d '\n' | sed 's/../& /g;s/ $//')
  else
    g="ERR: $(grep -v 'Assembler messages' "$d/err" | head -1)"
  fi
  mcargs=(-triple="$triple" -show-encoding -filetype=obj)
  [ "$syntax" = intel ] && mcargs+=(-x86-asm-syntax=intel)
  if llvm-mc "${mcargs[@]}" -o "$d/m.o" "$d/in.s" > /dev/null 2> "$d/merr"; then
    m=$(llvm-objcopy -O binary --only-section=.text "$d/m.o" "$d/m.bin" 2>/dev/null;
        xxd -p "$d/m.bin" | tr -d '\n' | sed 's/../& /g;s/ $//')
  else
    m="ERR: $(grep error "$d/merr" | head -1)"
  fi
  r=$("$root/target/release/rsasm" -a "$arch" -f bin --hex "$d/in.s" 2>&1 | head -3 | tr '\n' ' ')
  r=${r%% }
  if [ "$g" = "$m" ] && [ "$g" = "$r" ]; then
    printf 'ok   %-44s %s\n' "$line" "$r"
  elif [ "${g#ERR}" != "$g" ] && [ "${m#ERR}" != "$m" ] && [ "${r#error}" != "$r" ]; then
    printf 'rej  %-44s %s\n' "$line" "${r%%  -->*}"
  else
    printf 'DIFF %-44s\n       gas: %s\n        mc: %s\n     rsasm: %s\n' "$line" "$g" "$m" "$r"
  fi
done
