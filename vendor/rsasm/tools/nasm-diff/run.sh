#!/bin/bash
# Differential test against NASM, for the `nasm` dialect.
#
# Assembles whole programs with rsasm `-d nasm` and with NASM 2.16.03, built
# by tools/oracles/build.sh, and compares what they produce:
#
# - a flat binary (`bin`), byte for byte;
# - an ELF object (`elf32`, `elf64`), section by section: each section's type,
#   flags, alignment, size and bytes, every relocation (offset, type, symbol
#   and addend), and every global, weak and undefined symbol (value, size,
#   type, binding, visibility and section). Local symbols are not compared:
#   NASM writes every label into the symbol table, where rsasm, like GNU as,
#   keeps them to itself, and a linker never sees the difference.
# - a COFF object (`win32`, `win64`) as tools/coff-diff/canon.sh prints it:
#   every section's characteristics and bytes, every symbol, locals included,
#   with its auxiliary records, and every relocation.
#
#   tools/nasm-diff/run.sh                 # every corpus
#   tools/nasm-diff/run.sh bin elf64       # just these formats
#
# Corpora are tools/nasm-diff/<format>.txt, snippets separated by
# `=== <name>` lines, and <format>-lines-<bits>.txt, one instruction per
# line assembled after `bits <bits>`. A snippet may carry extra files for
# `%include` and `incbin`: a `--- file <name>` line starts one.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin="${RSASM_ORACLES:-$root/target/oracles}/bin"
nasm="$bin/nasm"

[ -x "$nasm" ] || { echo "no NASM in $bin; run tools/oracles/build.sh nasm" >&2; exit 0; }
"$nasm" -v | grep -q "version 2\.16\.03" || { echo "REF-MISSING: $nasm is not NASM 2.16.03" >&2; exit 1; }
command -v readelf > /dev/null || { echo "readelf not found" >&2; exit 0; }
cargo build --quiet --manifest-path "$root/Cargo.toml" --all-features --bin rsasm || exit 1
rsasm="$root/target/debug/rsasm"

pass=0
fail=0
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Writes a snippet's files into a fresh directory: `in.asm` and any
# `--- file <name>` parts.
unpack() { # dir; snippet on stdin
  awk -v d="$1" '
    BEGIN { f = d "/in.asm"; printf "" > f }
    /^--- file / { close(f); f = d "/" $3; printf "" > f; next }
    { print >> f }
  '
}

# What an ELF object holds, in a form two assemblers can agree on.
describe_elf() { # object
  readelf -SW "$1" | awk '
    /^  \[ *[0-9]+\]/ {
      sub(/^  \[ */, ""); sub(/\]/, "")
      idx = $1; name = $2; type = $3
      # Flags may be empty, which shifts the columns.
      if (NF == 11) { flags = $8; align = $11 } else { flags = ""; align = $10 }
      size = $6
      if (type == "NULL" || type == "SYMTAB" || type == "STRTAB" || type == "RELA" || type == "REL")
        next
      print "section", name, type, "flags=" flags, "align=" align, "size=0x" size
    }' | sort
  local s
  for s in $(readelf -SW "$1" | awk '/^  \[ *[0-9]+\]/ { sub(/^  \[ */, ""); sub(/\]/, ""); if ($3 == "PROGBITS") print $2 }'); do
    printf 'bytes %s ' "$s"
    llvm-objcopy -O binary --only-section="$s" "$1" "$work/sec.bin" 2>/dev/null
    xxd -p "$work/sec.bin" | tr -d '\n'
    echo
  done | sort
  readelf -rW "$1" | awk '
    /^Relocation section/ { sec = $3; gsub(/\x27/, "", sec); next }
    /^ *[0-9a-f]+ +[0-9a-f]+ +R_/ {
      target = ""
      for (i = 5; i <= NF; i++) target = target $i
      print "reloc", sec, $1 + 0 == $1 ? $1 : $1, $3, target
    }'
  readelf -SW "$1" | awk '/^  \[ *[0-9]+\]/ { sub(/^  \[ */, ""); sub(/\]/, ""); print $1, $2 }' > "$work/shnames"
  readelf -sW "$1" | awk -v names="$work/shnames" '
    BEGIN { while ((getline l < names) > 0) { split(l, p, " "); sec[p[1]] = p[2] } }
    $1 ~ /^[0-9]+:$/ && ($5 == "GLOBAL" || $5 == "WEAK" || $7 == "UND") && $8 != "" {
      ndx = ($7 in sec) ? sec[$7] : $7
      print "symbol", $8, "value=" $2, "size=" $3, $4, $5, $6, ndx
    }' | sort
}

reference() { # format dir
  if ! (cd "$2" && "$nasm" -f "$1" -o ref.out in.asm > ref.log 2>&1); then
    echo "REF-ERROR: $(grep -m2 -i error "$2/ref.log" | tr '\n' ' ')"
    return
  fi
  case "$1" in
    bin) xxd -p "$2/ref.out" | tr -d '\n'; echo ;;
    win*) "$here/../coff-diff/canon.sh" "$2/ref.out" ;;
    *) describe_elf "$2/ref.out" ;;
  esac
}

ours() { # format dir
  if ! (cd "$2" && "$rsasm" -d nasm -f "$1" -o ours.out in.asm > ours.log 2>&1); then
    echo "RSASM-ERROR: $(grep -m3 -A2 error "$2/ours.log" | tr '\n' ' ')"
    return
  fi
  case "$1" in
    bin) xxd -p "$2/ours.out" | tr -d '\n'; echo ;;
    win*) "$here/../coff-diff/canon.sh" "$2/ours.out" ;;
    *) describe_elf "$2/ours.out" ;;
  esac
}

compare() { # format name; snippet on stdin
  local d="$work/case" r m
  rm -rf "$d" && mkdir -p "$d"
  unpack "$d"
  r=$(reference "$1" "$d")
  m=$(ours "$1" "$d")
  if [ "$r" = "$m" ] && [ "${r#REF-}" = "$r" ]; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "### [$1] $2"
    sed 's/^/    |/' "$d/in.asm"
    diff <(printf '%s\n' "$r") <(printf '%s\n' "$m") | sed 's/^/  /'
  fi
}

run_format() { # format
  local f=$1 before=$((pass + fail)) progs="$here/$1.txt" lines
  if [ -f "$progs" ]; then
    local snippet="" name=""
    while IFS= read -r line; do
      case "$line" in
        "==="*)
          [ -n "$name" ] && compare "$f" "$name" <<< "$snippet"
          snippet=""; name="${line#=== }" ;;
        *) snippet="$snippet$line
" ;;
      esac
    done < "$progs"
    [ -n "$name" ] && compare "$f" "$name" <<< "$snippet"
  fi
  for lines in "$here/$1"-lines-*.txt; do
    [ -f "$lines" ] || continue
    local bits=${lines##*-lines-}
    bits=${bits%.txt}
    while IFS= read -r line; do
      [ -z "${line// /}" ] && continue
      case "$line" in \;*) continue ;; esac
      compare "$f" "bits $bits: $line" <<< "bits $bits
$line"
    done < "$lines"
  done
  local n=$((pass + fail - before))
  [ "$n" -gt 0 ] && echo "[$f] $n cases"
  return 0
}

for f in ${*:-bin elf32 elf64 win32 win64}; do
  run_format "$f"
done

echo "--- $pass matched, $fail differed"
[ "$fail" -eq 0 ]
