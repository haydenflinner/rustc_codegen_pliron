#!/bin/bash
# Prints a PE/COFF object in a form two assemblers can agree on.
#
#   tools/coff-diff/canon.sh [--relocs] o.obj
#
# Four kinds of line, each set sorted so that the order the tables happen to
# be in says nothing:
#
#   machine IMAGE_FILE_MACHINE_AMD64
#   section .text 0x60300020 11 b8010000e800000000c3
#   symbol main External 0x0 .text+0x0
#   reloc .text 0x6 IMAGE_REL_AMD64_REL32 .text+0x0
#
# A relocation's target is written by what the linker will make of it rather
# than by how the symbol table spells it: a symbol defined in the object is
# its section and value, so naming the symbol and naming its section with the
# offset in the field read the same. The addend is not printed — COFF keeps
# it in the section's bytes, which are compared above.
#
# A symbol is printed with its storage class, its section and value, and
# whatever auxiliary record follows it: a section definition's length,
# relocation count, checksum and COMDAT selection, or a weak external's
# fallback. Symbol *order* is not compared, since nothing but a COMDAT
# depends on it, and a COMDAT's selection is printed on the section.
#
# With `--relocs`, only the machine and the relocations are printed, each with
# the addend its field holds: that is the part of a COFF object GNU as and
# llvm-mc agree on, so it is what the secondary reference is checked against.
#
# Plain POSIX awk: no strtonum, so hex is converted by hand.
set -u
mode=full
[ "$1" = --relocs ] && { mode=relocs; shift; }
obj=$1
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

readobj() { llvm-readobj "$@" "$obj"; }

readobj --file-headers | ${AWK:-awk} '$1 == "Machine:" { print "machine", $2; exit }'

readobj --symbols > "$tmp/syms"

# The symbol table, as both a printable list and a lookup for relocations.
${AWK:-awk} '
  function field(line,    s) { s = line; sub(/^[^:]*: ?/, "", s); sub(/ \([0-9a-fA-FxX-]+\)$/, "", s); return s }
  $1 == "Symbol" && $2 == "{" { n++; aux = ""; name = ""; value = 0; sect = ""; class = ""; ctype = "" }
  $1 == "Name:" && name == "" { name = field($0) }
  $1 == "Value:" { value = $2 }
  $1 == "Section:" { sect = field($0) }
  $1 == "StorageClass:" { class = $2 }
  $1 == "ComplexType:" { ctype = $2 }
  $1 == "Length:" { aux = aux " len=" $2 }
  $1 == "RelocationCount:" { aux = aux " relocs=" $2 }
  $1 == "Checksum:" { aux = aux " sum=" $2 }
  $1 == "Selection:" { aux = aux " select=" $2 }
  $1 == "AssocSection:" { aux = aux " assoc=" $2 }
  $1 == "Linked:" { aux = aux " linked=" field($0) }
  $1 == "FileName:" { aux = aux " file=" field($0) }
  $1 == "}" && name != "" {
    print "symbol", name, class, ctype, sect "+" value aux
    name = ""
  }
' "$tmp/syms" | LC_ALL=C sort > "$tmp/symlist"

# Section headers and bytes. A section is printed with the characteristics
# word whole: the alignment lives in it, and so do the COMDAT and
# discardable bits.
readobj --sections --section-data | ${AWK:-awk} '
  function flush() {
    if (name != "") printf "section %s %s %s %s\n", name, flags, size, data
  }
  $1 == "Section" && $2 == "{" { flush(); name = ""; data = ""; size = 0 }
  $1 == "Name:" && name == "" { name = $2 }
  $1 == "RawDataSize:" { size = $2 }
  $1 == "Characteristics" { f = $3; gsub(/[()]/, "", f); flags = f }
  /^ *[0-9A-F][0-9A-F][0-9A-F][0-9A-F]: / {
    line = $0
    sub(/^ *[0-9A-F]*: /, "", line)
    sub(/ *\|.*$/, "", line)
    gsub(/ /, "", line)
    data = data tolower(line)
  }
  END { flush() }
' | LC_ALL=C sort > "$tmp/sections"

# Relocations, with each target resolved through the symbol table.
readobj --relocs --expand-relocs | ${AWK:-awk} '
  function field(line,    s) { s = line; sub(/^[^:]*: ?/, "", s); sub(/ \([0-9a-fA-FxX-]+\)$/, "", s); return s }
  # ---- the first file: the symbol table ----
  FNR == NR {
    if ($1 == "Symbol" && $2 == "{") { idx = nsyms++; name[idx] = ""; aux = 0 }
    else if ($1 == "Name:" && name[idx] == "") name[idx] = field($0)
    else if ($1 == "Value:") value[idx] = $2
    else if ($1 == "Section:") sect[idx] = field($0)
    else if ($1 == "AuxSymbolCount:") { for (i = 0; i < $2; i++) nsyms++ }
    next
  }
  # ---- the second file: the relocations ----
  $1 == "Section" && $NF == "{" { relsec = $3 }
  $1 == "Offset:" { offset = $2 }
  $1 == "Type:" { type = $2 }
  $1 == "SymbolIndex:" {
    s = $2 + 0
    if (sect[s] == "IMAGE_SYM_UNDEFINED" || sect[s] == "IMAGE_SYM_ABSOLUTE")
      target = name[s]
    else
      target = sect[s] "+" value[s]
    printf "reloc %s %s %s %s\n", relsec, offset, type, target
  }
' "$tmp/syms" - | LC_ALL=C sort > "$tmp/relocs"

# In `--relocs` mode each relocation is printed with its addend folded into
# its target: COFF keeps the addend in the relocated field, and the two
# references split an address differently between the two — llvm-mc names a
# local label and leaves the field alone, GNU as names the label's section and
# puts the label's offset in the field — so only their sum means anything.
# Sections are not compared at all in this mode, since GNU as pads them to an
# alignment of its own.
if [ "$mode" = relocs ]; then
  ${AWK:-awk} '
    function hexval(h,    i, v) {
      h = toupper(h); v = 0
      for (i = 1; i <= length(h); i++) v = v * 16 + index("0123456789ABCDEF", substr(h, i, 1)) - 1
      return v
    }
    # The n-byte little-endian field at `off`, as a signed number.
    function field(name, off, n,    d, i, v) {
      d = data[name]; v = 0
      for (i = n - 1; i >= 0; i--) v = v * 256 + hexval(substr(d, (off + i) * 2 + 1, 2))
      if (v >= 2 ^ (n * 8 - 1)) v -= 2 ^ (n * 8)
      return v
    }
    FNR == NR { if ($1 == "section") data[$2] = $5; next }
    {
      n = 4
      if ($4 ~ /ADDR64/) n = 8
      if ($4 ~ /SECTION$/ || $4 ~ /DIR16/) n = 2
      o = $3; sub(/^0x/, "", o)
      a = field($2, hexval(o), n)
      target = $5
      if ($4 ~ /SECTION$/) {
        # Only the section counts; there is no offset in a section index.
        sub(/\+.*/, "", target)
      } else if (target ~ /\+-?[0-9]+$/) {
        base = target; sub(/\+-?[0-9]+$/, "", base)
        v = target; sub(/^.*\+/, "", v)
        target = base "+" (v + a)
      } else {
        target = target "+" a
      }
      printf "reloc %s %s %s %s\n", $2, $3, $4, target
    }
  ' "$tmp/sections" "$tmp/relocs"
  exit 0
fi
cat "$tmp/sections" "$tmp/symlist" "$tmp/relocs"
