#!/bin/bash
# Regenerates the <key>-compiler.txt corpora of tools/dwarf-diff from
# compiler.c: what GCC and Clang write for a small program with -g, which is
# what line tables and frames are mostly made from in practice.
#
#   tools/dwarf-diff/compiler.sh            # every target
#   tools/dwarf-diff/compiler.sh avr5       # just these
#
# The committed corpora came from GCC 15.3 (x86-64 and i386, the targets
# whose reference is GNU as) and Clang 22.1 (every target; for AVR, the
# ATmega328P of an Arduino Uno, an avr5 core). Another version
# writes other code, which is fine: the harness compares rsasm with the
# reference on whatever the corpus holds.
#
# A few lines are rewritten or dropped first, where the object would need
# something rsasm does not do that has nothing to do with DWARF, and both
# assemblers then read the same edited file:
#
# - directives about the object's ABI or its ARM EHABI unwind tables
#   (`.attribute`, `.abiversion`, `.localentry`, `.ent`, `.fnstart`, ...),
#   and MIPS's `.set reorder`, since rsasm only assembles `noreorder` code;
# - the TOC set-up at a PowerPC ELFv2 global entry point;
# - `zext.b` (RISC-V) and `dsll`/`dsrl` by 32 or more (MIPS), as the
#   instruction each is an alias of;
# - MIPS O32's `$`-prefixed private labels, renamed `.L`;
# - `-fno-stack-protector`, since GNU as writes `%gs:20` in the short
#   `moffs` form rsasm lacks;
# - the `.cfi_*` directives for MSP430, whose GNU as has no CFI.
#
# Thumb is left out: Clang's Thumb-2 output uses encodings rsasm lacks.
set -u
here=$(cd "$(dirname "$0")" && pwd)
src="$here/compiler.c"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

command -v gcc > /dev/null || { echo "gcc not found" >&2; exit 1; }
command -v clang > /dev/null || { echo "clang not found" >&2; exit 1; }

# The directory in the file tables is made up, so the corpus is the same
# wherever it is regenerated.
common="-g -fno-ident -fno-stack-protector"

snippet() { # title, file
  echo "=== $1"
  cat "$2"
}

# key | GCC flags, or `-` for none | Clang flags
TARGETS="
x86-64|-m64|--target=x86_64-linux-gnu
i386|-m32 -fno-pic|--target=i386-linux-gnu -fno-pic
aarch64|-|--target=aarch64-linux-gnu
arm|-|--target=armv7a-linux-gnueabi -march=armv7-a -mfloat-abi=soft -fno-unwind-tables -fno-exceptions
riscv64|-|--target=riscv64-linux-gnu -march=rv64gc -fno-addrsig
riscv32|-|--target=riscv32-unknown-elf -march=rv32gc -fno-addrsig
powerpc|-|--target=powerpc-linux-gnu -fno-pic
powerpc64le|-|--target=powerpc64le-linux-gnu -mno-altivec -mno-vsx
mips|-|--target=mips-linux-gnu -fno-pic -mno-abicalls
mipsel|-|--target=mipsel-linux-gnu -fno-pic -mno-abicalls
mips64|-|--target=mips64-linux-gnuabi64 -march=mips64 -fno-pic -mno-abicalls
sparc|-|--target=sparc-linux-gnu -mcpu=v8
sparcv9|-|--target=sparcv9-linux-gnu
avr5|-|--target=avr -mmcu=atmega328p
msp430|-|--target=msp430-elf
"

clean() { # key; source on stdin
  grep -vE '^\s*\.(attribute|addrsig|addrsig_sym|cpu|fpu|eabi_attribute|abiversion|localentry|ent|end|frame|mask|fmask|nan|module|abicalls|ident|fnstart|fnend|cantunwind|pad|save|setfp)\b' |
    grep -vE '^\s*\.set\s+(reorder|(no)?(macro|mips16|micromips|at))\b' |
    grep -vE '\.TOC\.-\.Lfunc_gep[0-9]+@' |
    perl -pe 's/\bzext\.b(\s+)(\w+),\s*(\w+)/andi$1$2, $3, 255/' |
    perl -pe 's/\b(ds[lr][la])(\s+)(\$\w+),\s*(\$\w+),\s*(3[2-9]|[4-5]\d|6[0-3])\s*$/"${1}32$2$3, $4, ".($5-32)."\n"/e' |
    if [ "$1" = mips ] || [ "$1" = mipsel ]; then
      perl -pe 's/\$(?!(?:zero|at|v[01]|a[0-7]|t[0-9]|s[0-8]|k[01]|gp|sp|fp|ra|f\d+|\d+)\b)([A-Za-z_][\w.]*)/.L$1/g'
    elif [ "$1" = msp430 ]; then
      grep -vE '^\s*\.cfi_'
    else
      cat
    fi
}

wanted="${*:-}"
while IFS='|' read -r key gccflags clangflags; do
  [ -z "$key" ] && continue
  if [ -n "$wanted" ]; then
    case " $wanted " in *" $key "*) ;; *) continue ;; esac
  fi
  out="$here/$key-compiler.txt"
  : > "$out"
  for opt in -O0 -O2; do
    if [ "$gccflags" != - ]; then
      # shellcheck disable=SC2086
      (cd "$tmp" && gcc $gccflags $opt $common -fdebug-prefix-map="$here"=/src \
        -fdebug-prefix-map="$tmp"=/build -S -o gcc.s "$src") || exit 1
      clean "$key" < "$tmp/gcc.s" > "$tmp/gcc-clean.s"
      snippet "gcc $opt -g" "$tmp/gcc-clean.s" >> "$out"
    fi
    # shellcheck disable=SC2086
    (cd "$tmp" && clang $clangflags $opt $common -fdebug-compilation-dir=/build -S -o clang.s "$src") || exit 1
    clean "$key" < "$tmp/clang.s" > "$tmp/clang-clean.s"
    snippet "clang $opt -g" "$tmp/clang-clean.s" >> "$out"
  done
  echo "$out"
done <<< "$TARGETS"
