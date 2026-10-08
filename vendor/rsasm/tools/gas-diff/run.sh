#!/bin/bash
# Differential test against GNU as.
#
# Assembles the same source with rsasm and with GNU as, and compares the
# resulting .text bytes. The hermetic expectations in tests/x86_encoding.rs
# were produced by running this.
#
# The reference is the pinned GNU as 2.47 that tools/oracles/build.sh builds
# (x86_64-elf-as, found through RSASM_ORACLES as for the other harnesses).
# Without it the host's `as` is used, with a warning: GNU as keeps changing
# its x86 output — 2.42 has no `salc` and marks fewer GOT loads relaxable
# than 2.46 — so only the pinned build is a stable reference.
#
#   tools/gas-diff/run.sh                 # every corpus
#   tools/gas-diff/run.sh <file>...       # just these
#
# A corpus's name says how it is assembled:
#
#   instructions.txt, programs.txt,  64-bit mode (`as --64`)
#   x86-64-*.txt
#   i386*.txt                        32-bit mode (`as --32`)
#   i8086*.txt                       16-bit mode: `as --32` with `.code16`
#   *-intel*.txt                     Intel syntax, after `.intel_syntax noprefix`
#
# A file whose name contains `programs` holds multi-line snippets separated
# by `=== <name>` lines. One containing `relocs` holds snippets in the same
# format that are compared as whole objects: every allocated section's header
# and bytes, the global and undefined symbols, and the relocations, as
# tools/mc-diff/canon.sh prints them. That part needs llvm-readobj and
# llvm-objcopy, and is skipped without. Any other corpus holds one
# instruction per line.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

bin="${RSASM_ORACLES:-$root/target/oracles}/bin"
if [ -x "$bin/x86_64-elf-as" ]; then
  gas="$bin/x86_64-elf-as"
elif command -v as >/dev/null; then
  gas=as
  echo "warning: no pinned x86_64-elf-as in $bin; using the host's $(as --version | head -1)" >&2
else
  echo "GNU as not found; skipping" >&2
  exit 0
fi
cargo build --quiet --manifest-path "$root/Cargo.toml" --example hexdump --bin rsasm || exit 1
hexdump="$root/target/debug/examples/hexdump"
rsasm="$root/target/debug/rsasm"

# Set per corpus by `configure`.
gasflags=--64
arch=x86-64
header=

configure() { # corpus file
  local base
  base=$(basename "$1")
  header=
  case "$base" in
    i386*) gasflags=--32 arch=i386 ;;
    i8086*) gasflags=--32 arch=i386 header=".code16
" ;;
    *) gasflags=--64 arch=x86-64 ;;
  esac
  case "$base" in
    *-intel*) header="$header.intel_syntax noprefix
" ;;
  esac
}

gas() {
  local d
  d=$(mktemp -d)
  cat > "$d/in.s"
  if ! "$gas" $gasflags -o "$d/out.o" "$d/in.s" 2> "$d/err"; then
    echo "GAS-ERROR: $(grep -v 'Assembler messages' "$d/err" | head -3 | tr '\n' ' ')"
    rm -rf "$d"; return
  fi
  objcopy -O binary --only-section=.text "$d/out.o" "$d/out.bin" 2>/dev/null
  xxd -p "$d/out.bin" | tr -d '\n' | sed 's/../& /g;s/ $//'
  echo
  rm -rf "$d"
}

pass=0; fail=0
compare() { # name, source
  local name=$1 src=$header$2 g r
  g=$(printf '%s\n' "$src" | gas)
  r=$(printf '%s\n' "$src" | "$hexdump" "$arch" 2>&1)
  if [ "$g" = "$r" ]; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "### $name"
    printf '%s\n' "$src" | sed 's/^/    /'
    echo "  gas:   $g"
    echo "  rsasm: $r"
  fi
}

compare_object() { # name, source
  local name=$1 src=$header$2 g r d
  d=$(mktemp -d)
  printf '%s\n' "$src" > "$d/in.s"
  if "$gas" $gasflags -o "$d/g.o" "$d/in.s" 2> "$d/err"; then
    g=$("$root/tools/mc-diff/canon.sh" "$d/g.o")
  else
    g="GAS-ERROR: $(grep -v 'Assembler messages' "$d/err" | head -3 | tr '\n' ' ')"
  fi
  if "$rsasm" -a "$arch" -o "$d/r.o" "$d/in.s" 2> "$d/err"; then
    r=$("$root/tools/mc-diff/canon.sh" "$d/r.o")
  else
    r="RSASM-ERROR: $(head -3 "$d/err" | tr '\n' ' ')"
  fi
  rm -rf "$d"
  if [ "$g" = "$r" ]; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "### [$arch] $name (object)"
    printf '%s\n' "$src" | sed 's/^/    /'
    echo "  gas:"
    printf '%s\n' "$g" | sed 's/^/    /'
    echo "  rsasm:"
    printf '%s\n' "$r" | sed 's/^/    /'
  fi
}

run_lines() {
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    case "$line" in \#*) continue ;; esac
    compare "$line" "$line"
  done < "$1"
}

run_snippets() { # file, compare function
  local snippet="" name=""
  while IFS= read -r line; do
    case "$line" in
      "==="*)
        [ -n "$snippet" ] && "$2" "$name" "$snippet"
        snippet=""; name="${line#=== }" ;;
      *) snippet="$snippet$line
" ;;
    esac
  done < "$1"
  [ -n "$snippet" ] && "$2" "$name" "$snippet"
  return 0
}

run_corpus() {
  configure "$1"
  case "$(basename "$1")" in
    *relocs*)
      if ! command -v llvm-readobj > /dev/null || ! command -v llvm-objcopy > /dev/null; then
        echo "llvm-readobj or llvm-objcopy not found; skipping $(basename "$1")" >&2
        return 0
      fi
      run_snippets "$1" compare_object ;;
    *programs*) run_snippets "$1" compare ;;
    *) run_lines "$1" ;;
  esac
}

if [ $# -gt 0 ]; then
  for f in "$@"; do run_corpus "$f"; done
else
  for f in "$here"/*.txt; do run_corpus "$f"; done
fi

echo "--- $pass matched, $fail differed"
[ "$fail" -eq 0 ]
