#!/bin/bash
# Differential test for source that switches targets with `.arch`.
#
# No reference assembler reads such a file, so each snippet is split at its
# `.arch <name>` lines, every part is assembled by the reference for its
# target (with the `.arch` line removed), and the parts' code is concatenated.
# rsasm assembles the whole snippet, starting as the first part's target, and
# its code has to equal that concatenation. What this checks is that each part
# is read the way its own target's assembler reads it — comment characters and
# number spellings included — and encoded in that target's byte order.
#
#   tools/multiarch-diff/run.sh        # report the snippets that differ
#   tools/multiarch-diff/run.sh -v     # and print every match, for tests
#
# The corpus, programs.txt, holds snippets separated by `=== <name>` lines.
# A snippet may have a `--- split` line: rsasm assembles what is above it,
# where the switches can be anywhere (in a macro, a conditional, a `.rept`),
# and the references assemble what is below it, the same program with every
# `.arch` at the top level.
#
# Every part is assembled on its own, so it must not refer to a label in
# another, and alignment in a part only means the same thing in both when the
# parts before it end on that boundary.
#
# References: GNU as 2.47 (x86_64-elf-as) for x86, llvm-mc 22 for the targets LLVM
# supports, and the cross GNU as builds of tools/oracles/build.sh (found in
# RSASM_ORACLES, as for tools/xas-diff) for the rest. A part whose reference
# is missing fails the snippet rather than skipping it.
set -u
verbose=
[ "${1-}" = -v ] && verbose=1
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin="${RSASM_ORACLES:-$root/target/oracles}/bin"

# `.arch` name | reference: `gas <flags>` for the host's as, `mc <triple>
# <flags>` for llvm-mc, or `xas <tool> <flags>` for a cross assembler | code
# section
REFS="
x86-64|xas x86_64-elf-as --64|.text
i386|xas x86_64-elf-as --32|.text
aarch64|mc aarch64|.text
arm|mc armv7|.text
riscv32|mc riscv32 -mattr=+m,+a,+f,+d,+c|.text
riscv64|mc riscv64 -mattr=+m,+a,+f,+d,+c|.text
powerpc|mc powerpc|.text
powerpc64le|mc powerpc64le|.text
mips|mc mips|.text
mipsel|mc mipsel|.text
sparc|mc sparc|.text
m68k|xas m68k-elf-as|.text
68000|xas m68k-elf-as -m68000|.text
68040|xas m68k-elf-as -m68040|.text
cpu32|xas m68k-elf-as -mcpu32|.text
5475|xas m68k-elf-as -mcpu=5475|.text
sh|xas sh-elf-as|.text
shl|xas sh-elf-as -little|.text
rx|xas rx-elf-as|P
rl78|xas rl78-elf-as|.text
v850|xas v850-elf-as|.text
rh850|xas v850-elf-as -mv850e3v5|.text
avr|xas avr-elf-as|.text
avr5|xas avr-elf-as -mmcu=avr5|.text
msp430|xas msp430-elf-as -mcpu=430|.text
msp430x|xas msp430-elf-as -mcpu=430x|.text
"

command -v llvm-objcopy > /dev/null || { echo "llvm-objcopy not found" >&2; exit 0; }
cargo build --quiet --manifest-path "$root/Cargo.toml" --all-features --example hexdump || exit 1
hexdump="$root/target/debug/examples/hexdump"

pass=0
fail=0

part() { # .arch name; source on stdin. Prints the code as hex, or REF-*.
  local name=$1 ref cmd section kind d status
  ref=$(printf '%s\n' "$REFS" | grep -m1 "^$name|")
  if [ -z "$ref" ]; then
    echo "REF-UNKNOWN: $name"; return
  fi
  IFS='|' read -r _ cmd section <<< "$ref"
  # shellcheck disable=SC2086
  set -- $cmd
  kind=$1; shift
  d=$(mktemp -d)
  cat > "$d/in.s"
  case "$kind" in
    gas) command -v as > /dev/null || { echo "REF-MISSING: as"; rm -rf "$d"; return; }
      as "$@" -o "$d/out.o" "$d/in.s" > "$d/log" 2>&1 ;;
    mc) command -v llvm-mc > /dev/null || { echo "REF-MISSING: llvm-mc"; rm -rf "$d"; return; }
      local triple=$1; shift
      llvm-mc -triple="$triple" "$@" -filetype=obj -o "$d/out.o" "$d/in.s" > "$d/log" 2>&1 ;;
    xas) [ -x "$bin/$1" ] || { echo "REF-MISSING: $1"; rm -rf "$d"; return; }
      local tool=$1; shift
      "$bin/$tool" "$@" -o "$d/out.o" "$d/in.s" > "$d/log" 2>&1 ;;
  esac
  status=$?
  if [ $status -ne 0 ]; then
    echo "REF-ERROR ($name): $(head -3 "$d/log" | tr '\n' ' ')"; rm -rf "$d"; return
  fi
  llvm-objcopy -O binary --only-section="$section" "$d/out.o" "$d/out.bin" 2> /dev/null
  [ -f "$d/out.bin" ] && xxd -p "$d/out.bin" | tr -d '\n'
  rm -rf "$d"
}

reference() { # split source on stdin; prints the concatenated code as hex
  local line name="" chunk="" out="" err="" hex
  flush() {
    [ -z "$name" ] && return
    hex=$(printf '%s' "$chunk" | part "$name")
    case "$hex" in REF-*) err="$err $hex"; return ;; esac
    out="$out$hex"
  }
  while IFS= read -r line; do
    # The rest of a switching line is a comment by the old target's rules,
    # and read by them.
    if [[ "$line" =~ ^[[:space:]]*\.arch[[:space:]]+([^[:space:]]+) ]]; then
      flush
      name=${BASH_REMATCH[1]}
      chunk=""
    elif [ -z "$name" ]; then
      [ -n "${line// /}" ] && { echo "REF-ERROR: text before the first \`.arch\`"; return; }
    else
      chunk="$chunk$line
"
    fi
  done
  flush
  if [ -n "$err" ]; then
    echo "$err"
  else
    printf '%s\n' "$out" | sed 's/../& /g;s/ $//'
  fi
}

compare() { # name, rsasm source, split source
  local name=$1 src=$2 split=$3 first m r
  first=$(printf '%s\n' "$split" | sed -nE 's/^[[:space:]]*\.arch[[:space:]]+([^[:space:]]+).*/\1/p' | head -1)
  m=$(printf '%s\n' "$split" | reference)
  r=$(printf '%s\n' "$src" | "$hexdump" "$first" gas elf 2>&1)
  if [ "$m" = "$r" ] && [ "${m#*REF-}" = "$m" ]; then
    pass=$((pass + 1))
    [ -n "$verbose" ] && printf '%s\n  %s\n' "=== $name" "$m"
  else
    fail=$((fail + 1))
    echo "### $name"
    printf '%s\n' "$src" | sed 's/^/    |/'
    if [ "$src" != "$split" ]; then
      echo "  --- split"
      printf '%s\n' "$split" | sed 's/^/    |/'
    fi
    echo "  references: $m"
    echo "  rsasm:      $r"
  fi
}

snippet="" split="" side="" name=""
flush_snippet() {
  [ -z "$name" ] && return
  if [ "$side" = split ]; then
    compare "$name" "$snippet" "$split"
  else
    compare "$name" "$snippet" "$snippet"
  fi
}
while IFS= read -r line; do
  case "$line" in
    "==="*)
      flush_snippet
      snippet=""; split=""; side=""; name="${line#=== }" ;;
    "--- split") side=split ;;
    *)
      if [ "$side" = split ]; then
        split="$split$line
"
      else
        snippet="$snippet$line
"
      fi ;;
  esac
done < "$here/programs.txt"
flush_snippet

echo "--- $pass matched, $fail differed"
[ "$fail" -eq 0 ]
