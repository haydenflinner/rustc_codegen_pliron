#!/bin/bash
# Prints an ELF object in a form two assemblers can agree on.
#
#   tools/mc-diff/canon.sh [--full] o.o
#
# Three parts, each in the object's own order:
#
#   section .text SHT_PROGBITS flags=0x6 size=0x10 align=4
#     0000000013050000...
#   symbol foo Global Function STV_DEFAULT .text+0x4
#   .rela.text 0x4 R_RISCV_CALL_PLT foo+0x0
#
# A section is listed if it is allocated and not empty, with its bytes unless
# it is SHT_NOBITS, and so is AVR's `.avr.prop`, which is not allocated but is
# what the linker relaxes the code by. What a reference writes of its own accord is left out:
# the ABI and attribute sections (`.reginfo`, `.MIPS.abiflags`,
# `.riscv.attributes`, `.ARM.attributes`, `.note.*`, ...), and `.text`,
# `.data` and `.bss` while they are empty, which GNU as always creates.
#
# A symbol is listed if it is global, weak or undefined: which local labels
# reach the symbol table, and under what names, differs between assemblers
# without meaning anything to a linker. Relocations are read through
# relocs.awk, which names a local target by its section and offset for the
# same reason.
#
# With `--full`, for a comparison against the one reference a target follows
# for its whole objects, `e_flags` is
# printed first, and the symbol list takes in every named local symbol but
# section and file symbols, sorted: ARM's mapping symbols and Thumb function
# bits are local, and are what that comparison is for. `--flags` prints
# `e_flags` alone, for a target whose header says something (AVR's core and
# relaxation flag) and whose local symbols do not.
#
# `--zero-relocated NAME=WIDTH` blanks the `WIDTH` bytes at each relocation in
# section `NAME` before printing it. A field a relocation covers belongs to the
# linker, and the references disagree about what they leave in it: GNU as for
# AVR writes uninitialized memory into the addresses in `.avr.prop`, so the
# same build differs from run to run and host to host.
#
# Plain POSIX awk: no strtonum, so hex is converted by hand.
set -u
here=$(cd "$(dirname "$0")" && pwd)
full=0
flags=0
zero_name=
zero_width=0
[ "$1" = --full ] && { full=1; flags=1; shift; }
[ "$1" = --flags ] && { flags=1; shift; }
if [ "$1" = --zero-relocated ]; then
  zero_name=${2%%=*}
  zero_width=${2#*=}
  shift 2
fi
obj=$1
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

llvm-readobj --sections "$obj" | ${AWK:-awk} '
  function hex(h,    i, v) {
    sub(/^0x/, "", h); h = toupper(h); v = 0
    for (i = 1; i <= length(h); i++) v = v * 16 + index("0123456789ABCDEF", substr(h, i, 1)) - 1
    return v
  }
  function flush() {
    # `.avr.prop` is not loaded, but is what a linker relaxing AVR code reads.
    if (name == "" || (!alloc && name != ".avr.prop") || size == 0) return
    if (name ~ /^\.(reginfo|pdr|comment|gnu\.attributes|riscv\.attributes|note)/ || name ~ /^\.(MIPS|ARM)\./) return
    printf "%s %s %s flags=%s size=0x%x align=%d\n", idx, name, type, flags, size, align
  }
  $1 == "Section" && $2 == "{" { name = ""; alloc = 0 }
  $1 == "Index:" { idx = $2 }
  $1 == "Name:" { name = $2 }
  $1 == "Type:" { type = $2 }
  $1 == "Flags" { f = $3; gsub(/[()]/, "", f); flags = f; alloc = int(hex(f) / 2) % 2 }
  $1 == "Size:" { size = $2 + 0 }
  $1 == "AddressAlignment:" { align = $2 }
  $1 == "}" { flush(); name = "" }
' > "$tmp/sections"

# By name: GNU as creates `.text`, `.data` and `.bss` before reading the
# source, and the others as the source names them, so the order of the
# section headers says nothing about the program.
sort -k2,2 "$tmp/sections" | while read -r idx name type flags size align; do
  echo "section $name $type $flags $size $align"
  [ "$type" = SHT_NOBITS ] && continue
  llvm-objcopy --dump-section "$name=$tmp/bytes" "$obj" "$tmp/copy" 2> /dev/null
  hex=$(xxd -p "$tmp/bytes" | tr -d '\n')
  if [ "$name" = "$zero_name" ]; then
    hex=$(llvm-readobj --relocations "$obj" |
      ${AWK:-awk} -v hex="$hex" -v want="$name" -v width="$zero_width" '
        function hexval(h,   i, c, n, d) {
          n = 0
          sub(/^0[xX]/, "", h)
          for (i = 1; i <= length(h); i++) {
            c = tolower(substr(h, i, 1))
            d = index("0123456789abcdef", c) - 1
            if (d >= 0) n = n * 16 + d
          }
          return n
        }
        $1 == "Section" { insect = ($3 == want || $3 == ".rel" want || $3 == ".rela" want) }
        insect && $1 ~ /^0x/ {
          off = hexval($1) * 2
          blank = ""
          for (i = 0; i < width * 2; i++) blank = blank "0"
          hex = substr(hex, 1, off) blank substr(hex, off + width * 2 + 1)
        }
        END { print hex }
      ')
  fi
  printf '  %s\n' "$hex"
done

[ "$flags" = 1 ] && llvm-readobj --file-headers "$obj" |
  ${AWK:-awk} '$1 == "Flags" { f = $3; gsub(/[()]/, "", f); print "flags " f; exit }'

llvm-readobj --symbols "$obj" > "$tmp/syms"
${AWK:-awk} -v full="$full" '
  function field(line,    s) { s = line; sub(/^[^:]*: ?/, "", s); sub(/ ?\([0-9a-fA-Fx]+\)$/, "", s); return s }
  $1 == "Symbol" && $2 == "{" { n++; other = "STV_DEFAULT"; inother = 0 }
  $1 == "Name:" { name = field($0) }
  $1 == "Value:" { value = $2 }
  $1 == "Binding:" { bind = $2 }
  $1 == "Type:" { type = $2 }
  $1 == "Other" && $2 == "[" { inother = 1 }
  inother && $1 ~ /^STV_/ { other = $1 }
  $1 == "]" { inother = 0 }
  $1 == "Section:" {
    sect = field($0)
    # GNU as for RL78 declares `__rl78_abs__` in every object, used or not.
    if (n > 1 && (bind != "Local" || sect == "Undefined") && name != "__rl78_abs__")
      printf "symbol %s %s %s %s %s+%s\n", name, bind, type, other, sect, value
    else if (full && n > 1 && name != "" && type != "Section" && type != "File")
      printf "symbol %s %s %s %s %s+%s\n", name, bind, type, other, sect, value
  }
' "$tmp/syms" | { if [ "$full" = 1 ]; then LC_ALL=C sort; else cat; fi; }

# By section and offset, keeping the order of entries at one offset (a
# RISC-V ADD/SUB pair): GNU as writes the fixups of instructions it relaxed
# after the others.
llvm-readobj --relocs --expand-relocs "$obj" |
  ${AWK:-awk} -f "$here/relocs.awk" "$tmp/syms" - |
  ${AWK:-awk} '{ o = substr($2, 3); printf "%s\t%16s\t%s\n", $1, o, $0 }' |
  sort -s -t "$(printf '\t')" -k1,1 -k2,2 | cut -f3-
