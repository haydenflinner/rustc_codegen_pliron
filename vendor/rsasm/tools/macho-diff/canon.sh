#!/bin/bash
# Prints a Mach-O object in a form two assemblers can agree on.
#
#   tools/macho-diff/canon.sh o.o
#
# Everything llvm-readobj and llvm-objdump read out of the object:
#
#   header X86-64 CPU_SUBTYPE_X86_64_ALL Relocatable cmds=3 size=416 flags=0x2000
#   segment vmsize=0x29 fileoff=448 filesize=41 nsects=3
#   version LC_BUILD_VERSION macos 14.0 n/a
#   dice 0x00000004 4 JUMP_TABLE32
#   section __TEXT,__text addr=0x0 size=0x1a offset=448 align=0 reloff=0x1f0 nreloc=3 type=0x0 attrs=0x800004 r1=0x0 r2=0x0
#     554889e5...
#   dysymtab local=2@0 extdef=1@2 undef=1@3
#   symbol _main Section __text value=0x0 ref=0x0 desc=0x0 Extern
#   reloc __text 0x11 pcrel=1 len=2 X86_64_RELOC_BRANCH _local
#
# Sections and relocations keep the object's order, which is part of what
# is being checked: a Mach-O linker reads a pair of relocations by position,
# and a section's index is what a symbol's `n_sect` counts. So do symbols
# within each of the three runs `LC_DYSYMTAB` describes, except the local
# run, which is sorted: which order a writer creates its local symbols in
# means nothing to a linker, and the other two runs have to be sorted by name
# anyway. A relocation's target is written as the symbol, section or addend
# the entry holds, not as an index.
#
# The symbol and string table offsets are left out, being the only fields
# that depend on how long the string table is, and how a writer shares the
# tails of names in it is not something a linker can see.
#
# Plain POSIX awk.
set -u
obj=$1
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

llvm-readobj --file-headers --macho-segment --macho-version-min "$obj" | ${AWK:-awk} '
  function paren(s) { sub(/.*\(/, "", s); sub(/\).*/, "", s); return s }
  /^MachHeader/ { inh = 1 }
  inh && $1 == "CpuType:" { cpu = $2 }
  inh && $1 == "CpuSubType:" { sub_ = $2 }
  inh && $1 == "FileType:" { ft = $2 }
  inh && $1 == "NumOfLoadCommands:" { ncmds = $2 }
  inh && $1 == "SizeOfLoadCommands:" { sizecmds = $2 }
  inh && $1 == "Flags" { flags = paren($0) }
  inh && $1 == "}" { printf "header %s %s %s cmds=%s size=%s flags=%s\n", cpu, sub_, ft, ncmds, sizecmds, flags; inh = 0 }
  /^Segment/ { inseg = 1 }
  inseg && $1 == "vmsize:" { vms = $2 }
  inseg && $1 == "fileoff:" { fo = $2 }
  inseg && $1 == "filesize:" { fs = $2 }
  inseg && $1 == "nsects:" { ns = $2 }
  inseg && $1 == "}" { printf "segment vmsize=%s fileoff=%s filesize=%s nsects=%s\n", vms, fo, fs, ns; inseg = 0 }
  /^MinVersion/ { inv = 1; cmd = ""; plat = ""; ver = ""; sdk = "" }
  inv && $1 == "Cmd:" { cmd = $2 }
  inv && $1 == "Platform:" { plat = $2 }
  inv && $1 == "Version:" { ver = $2 }
  inv && $1 == "SDK:" { sdk = $2 }
  inv && $1 == "}" { printf "version %s %s %s %s\n", cmd, plat, ver, sdk; inv = 0 }
'

# The data-in-code entries, as `llvm-objdump` lists them.
llvm-objdump --macho --data-in-code "$obj" | ${AWK:-awk} '
  $1 ~ /^0x/ { printf "dice %s %s %s\n", $1, $2, $3 }
'

llvm-readobj --sections --section-data "$obj" | ${AWK:-awk} '
  function paren(s) { sub(/.*\(/, "", s); sub(/\).*/, "", s); return s }
  $1 == "Section" && $2 == "{" { data = ""; indata = 0 }
  $1 == "Name:" { name = $2 }
  $1 == "Segment:" { seg = $2 }
  $1 == "Address:" { addr = tolower($2) }
  $1 == "Size:" { size = tolower($2) }
  $1 == "Offset:" { off = $2 }
  $1 == "Alignment:" { align = $2 }
  $1 == "RelocationOffset:" { reloff = tolower($2) }
  $1 == "RelocationCount:" { nreloc = $2 }
  $1 == "Type:" { type = paren($0) }
  $1 == "Attributes" { attrs = paren($0) }
  $1 == "Reserved1:" { r1 = tolower($2) }
  $1 == "Reserved2:" { r2 = tolower($2) }
  $1 == "SectionData" { indata = 1; next }
  indata && $1 == ")" { indata = 0; next }
  indata {
    line = $0
    sub(/^ *[0-9A-Fa-f]+: /, "", line)
    sub(/ *\|.*$/, "", line)
    gsub(/ /, "", line)
    data = data tolower(line)
    next
  }
  $1 == "}" && name != "" {
    printf "section %s,%s addr=%s size=%s offset=%s align=%s reloff=%s nreloc=%s type=%s attrs=%s r1=%s r2=%s\n", seg, name, addr, size, off, align, reloff, nreloc, type, attrs, r1, r2
    if (data != "") printf "  %s\n", data
    name = ""
  }
'

llvm-readobj --symbols --macho-dysymtab "$obj" > "$tmp/syms"
${AWK:-awk} '
  function paren(s) { sub(/.*\(/, "", s); sub(/\).*/, "", s); return s }
  function field(line,    s) { s = line; sub(/^[^:]*: ?/, "", s); sub(/ ?\([0-9a-fA-Fx]+\)$/, "", s); return s }
  $1 == "Symbol" && $2 == "{" { n++; ext = ""; pext = ""; flags = "" }
  $1 == "Name:" { name[n] = field($0) }
  $1 == "PrivateExtern" { pext = " PrivateExtern" }
  $1 == "Extern" { ext = " Extern" }
  $1 == "Type:" { type = $2 }
  $1 == "Section:" { sect = field($0); if (sect == "") sect = "-" }
  $1 == "RefType:" { ref = paren($0) }
  $1 == "Flags" { flags = paren($0) }
  $1 == "Value:" { value = tolower($2) }
  $1 == "}" && n > 0 && name[n] != "" {
    line[n] = sprintf("symbol %s %s %s value=%s ref=%s desc=%s%s%s", name[n], type, sect, value, ref, flags, pext, ext)
    name[n] = ""
  }
  $1 == "ilocalsym:" { il = $2 }
  $1 == "nlocalsym:" { nl = $2 }
  $1 == "iextdefsym:" { ie = $2 }
  $1 == "nextdefsym:" { ne = $2 }
  $1 == "iundefsym:" { iu = $2 }
  $1 == "nundefsym:" { nu = $2 }
  END {
    printf "dysymtab local=%s@%s extdef=%s@%s undef=%s@%s\n", nl, il, ne, ie, nu, iu
    # Locals sorted, the rest in table order.
    for (i = 1; i <= nl; i++) print line[i] > "/dev/stderr"
    for (i = nl + 1; i <= n; i++) print line[i]
  }
' "$tmp/syms" 2> "$tmp/locals" > "$tmp/rest"
head -1 "$tmp/rest"
LC_ALL=C sort "$tmp/locals"
tail -n +2 "$tmp/rest"

llvm-readobj --relocations --expand-relocs "$obj" | ${AWK:-awk} '
  function paren(s) { sub(/.*\(/, "", s); sub(/\).*/, "", s); return s }
  $1 == "Section" && $NF == "{" { sect = $2 }
  $1 == "Relocation" { off = ""; target = "" }
  $1 == "Offset:" { off = tolower($2) }
  $1 == "PCRel:" { pcrel = $2 }
  $1 == "Length:" { len = $2 }
  $1 == "Type:" { type = $2 }
  $1 == "Symbol:" { target = $2 }
  $1 == "Section:" && off != "" { target = ($2 == "-" ? "addend=" paren($0) : "section=" $2) }
  $1 == "}" && off != "" {
    printf "reloc %s %s pcrel=%s len=%s %s %s\n", sect, off, pcrel, len, type, target
    off = ""
  }
'
