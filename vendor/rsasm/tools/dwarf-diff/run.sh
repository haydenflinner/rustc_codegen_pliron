#!/bin/bash
# Differential test for the DWARF sections rsasm writes itself: line tables
# from `.file`/`.loc` and call frame information from `.cfi_*`.
#
# Each snippet is assembled into an object by rsasm and by the target's
# reference, and the two have to agree on the line table, the call frame
# information and the compilation unit an assembler makes up for a line table
# (`.debug_info`, `.debug_abbrev`, `.debug_str`, `.debug_aranges`,
# `.debug_ranges` or `.debug_rnglists`): each section's type, flags,
# alignment and bytes, and every relocation against them, read through
# tools/mc-diff/relocs.awk the way a linker reads it.
#
#   tools/dwarf-diff/run.sh               # every target
#   tools/dwarf-diff/run.sh aarch64 sh    # just these
#   tools/dwarf-diff/run.sh -v aarch64    # and print llvm-dwarfdump's reading
#                                         # of both objects on a difference
#
# The reference is whichever assembler checks the target's encodings, since
# the two write different DWARF from the same source (see src/dwarf): GNU as
# 2.47 for x86, m68k, SuperH, RX, RL78, V850 and AVR, from tools/oracles/build.sh
# (found in RSASM_ORACLES, as for tools/xas-diff), and llvm-mc 22 for the
# rest. x86 is checked against the cross x86_64-elf-as rather than the host's
# as, which compresses debug sections by default and is a different release.
#
# Corpora hold snippets separated by `=== <name>` lines, or
# `=== <name> | <flag>` for one assembled with `-g` or `--gdwarf-<n>` (which
# llvm-mc spells `-g -dwarf-version=<n>`): common.txt and common-g.txt, whose
# snippets use only `nop` and register numbers and are run for every target,
# <key>.txt for each target's own, and <key>-compiler.txt with whole files
# from GCC (x86) and Clang (every target whose code it assembles), see
# compiler.sh. A snippet using `.cfi_*` is skipped for a target whose
# reference has no CFI. Both assemblers run in the same scratch directory,
# which a DWARF 5 table without `.file 0` names, and both are told to call
# themselves the reference in the compilation unit, through the
# DEBUG_PRODUCER variable llvm-mc reads and rsasm reads for this.
set -u
verbose=
[ "${1-}" = -v ] && { verbose=1; shift; }
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin="${RSASM_ORACLES:-$root/target/oracles}/bin"
awkscript="$root/tools/mc-diff/relocs.awk"

# key | rsasm arch, and options | reference: `xas <tool> <flags>` or
# `mc <triple> <flags>`
# | what differs: `cfi` where the reference has call frame information,
# `P` where it names the code section `P` (RX; rsasm writes `.text`),
# `norelocs` where its relocations are not compared (RL78, whose GNU as leaves
# every distance in the line table to the linker as a stack of relocation
# operations, where rsasm writes the numbers), and `nodiffs` where the
# relocations that restate a difference already in the bytes are not (AVR,
# whose GNU as adds an `R_AVR_DIFF*` to every distance into code, for linker
# relaxation, where rsasm writes the number alone), and `byoffset` where they are
# compared in offset order (MSP430, whose GNU as writes the
# `R_MSP430_SYM_DIFF` pairs of the line table's address advances after its
# other relocations, when it converts their frags; a pair stays together, and
# a linker reads the rest in any order)
TARGETS="
x86-64|x86-64|xas x86_64-elf-as|cfi
i386|i386|xas x86_64-elf-as --32|cfi
aarch64|aarch64|mc aarch64|cfi
arm|arm|mc armv7|cfi
thumb|thumb|mc thumbv7|cfi
riscv32|riscv32|mc riscv32 -mattr=+m,+a,+f,+d,+c|cfi
riscv64|riscv64|mc riscv64 -mattr=+m,+a,+f,+d,+c|cfi
powerpc|powerpc|mc powerpc|cfi
powerpc64|powerpc64|mc powerpc64|cfi
powerpc64le|powerpc64le|mc powerpc64le|cfi
mips|mips|mc mips|cfi
mipsel|mipsel|mc mipsel|cfi
mips64|mips64|mc mips64|cfi
sparc|sparc|mc sparc|cfi
sparcv9|sparcv9|mc sparcv9|cfi
m68k|m68k -d gas|xas m68k-elf-as|cfi
sh|sh|xas sh-elf-as|cfi
shl|shl|xas sh-elf-as -little|cfi
rx|rx|xas rx-elf-as|P
rl78|rl78|xas rl78-elf-as|norelocs
v850|v850|xas v850-elf-as|
avr|avr|xas avr-elf-as|cfi nodiffs
avr5|avr5|xas avr-elf-as -mmcu=avr5|cfi nodiffs
avr6|avr6|xas avr-elf-as -mmcu=avr6|cfi nodiffs
msp430|msp430|xas msp430-elf-as -mcpu=430|byoffset
msp430x|msp430x|xas msp430-elf-as -mcpu=430x|byoffset
"
SECTIONS=".debug_line .debug_line_str .eh_frame .debug_frame .debug_info .debug_abbrev
.debug_str .debug_aranges .debug_ranges .debug_rnglists"

command -v llvm-mc > /dev/null || { echo "llvm-mc not found; skipping" >&2; exit 0; }
command -v llvm-readobj > /dev/null || { echo "llvm-readobj not found; skipping" >&2; exit 0; }
[ -d "$bin" ] || { echo "no oracles in $bin; run tools/oracles/build.sh" >&2; exit 0; }
cargo build --quiet --manifest-path "$root/Cargo.toml" --all-features --bin rsasm || exit 1
rsasm="$root/target/debug/rsasm"

pass=0
fail=0

# What the two objects have to agree on.
canon() { # object
  local o=$1 s
  for s in $SECTIONS; do
    llvm-readobj --sections "$o" | ${AWK:-awk} -v want="$s" '
      $1 == "Name:" { name = $2 }
      $1 == "Type:" { type = $2 }
      $1 == "Flags" { flags = $3; inflags = 1; list = ""; next }
      inflags && $1 == "]" { inflags = 0 }
      inflags { list = list " " $1 }
      $1 == "AddressAlignment:" && name == want { print want, type, flags list, "align=" $2 }'
    # `-O binary` leaves out sections that are not loaded; this does not.
    if llvm-objcopy --dump-section="$s=$o.bin" "$o" "$o.tmp" 2> /dev/null && [ -s "$o.bin" ]; then
      xxd -p "$o.bin" | tr -d '\n'
      echo
    fi
    rm -f "$o.bin" "$o.tmp"
  done
  llvm-readobj --symbols "$o" > "$o.syms"
  # Grouped by section, since the order the sections come in says nothing.
  llvm-readobj --relocs --expand-relocs "$o" | ${AWK:-awk} -f "$awkscript" "$o.syms" - |
    grep -E "^\\.rela?($(echo $SECTIONS | sed 's/\./\\./g; s/ /|/g')) " | sort -s -k1,1
}

# Relocation lines sorted by section and then offset, keeping the order of
# entries at one offset; everything else as it was.
by_offset() {
  ${AWK:-awk} '{ if ($1 ~ /^\.rel/) { o = substr($2, 3); printf "1\t%s\t%16s\t%s\n", $1, o, $0 }
                 else printf "0\t\t\t%s\n", $0 }' |
    sort -s -t "$(printf '\t')" -k1,1 -k2,2 -k3,3 | cut -f4-
}

compare() { # key, rsasm arch, reference, quirks, name, source, flag
  local key=$1 rs=$2 ref=$3 quirks=$4 name=$5 src=$6 flag=$7 d kind tool m r producer mcflags
  local -a xasflags=()
  d=$(mktemp -d)
  printf '%s\n' "$src" > "$d/in.s"
  # shellcheck disable=SC2086
  set -- $ref
  kind=$1; shift
  case "$flag" in
    "") mcflags= ;;
    -g) mcflags=-g; xasflags=(-g) ;;
    --gdwarf-*) mcflags="-g -dwarf-version=${flag#--gdwarf-}"; xasflags=("$flag") ;;
    *) echo "unknown flag in [$key] $name: $flag" >&2; exit 1 ;;
  esac
  case "$kind" in
    mc) tool=$1; shift; producer="llvm-mc"
      # shellcheck disable=SC2086
      (cd "$d" && DEBUG_PRODUCER=$producer llvm-mc -triple="$tool" "$@" $mcflags -filetype=obj \
        -o ref.o in.s) > "$d/ref.log" 2>&1 ;;
    xas) tool=$1; shift; producer="GNU AS 2.47"
      if [ ! -x "$bin/$tool" ]; then echo "REF-MISSING: $tool" > "$d/ref.log"; false
      else (cd "$d" && "$bin/$tool" --nocompress-debug-sections "${xasflags[@]}" "$@" -o ref.o in.s) \
        > "$d/ref.log" 2>&1; fi ;;
  esac
  if [ $? -eq 0 ] && [ -f "$d/ref.o" ]; then
    m=$(canon "$d/ref.o")
    case " $quirks " in *" P "*) m=$(printf '%s\n' "$m" | sed 's/ P+0x/ .text+0x/') ;; esac
  else
    m="REF-ERROR: $(grep -m3 -iE 'error|missing' "$d/ref.log" | tr '\n' ' ')"
  fi
  # shellcheck disable=SC2086
  if (cd "$d" && DEBUG_PRODUCER=$producer "$rsasm" -a $rs $flag -o rs.o in.s) > "$d/rs.log" 2>&1; then
    r=$(canon "$d/rs.o")
  else
    r="RSASM-ERROR: $(grep -m3 -i error "$d/rs.log" | tr '\n' ' ')"
  fi
  case " $quirks " in
    *" norelocs "*)
      m=$(printf '%s\n' "$m" | grep -v '^\.rel')
      r=$(printf '%s\n' "$r" | grep -v '^\.rel') ;;
    *" nodiffs "*)
      m=$(printf '%s\n' "$m" | grep -v ' R_AVR_DIFF')
      r=$(printf '%s\n' "$r" | grep -v ' R_AVR_DIFF') ;;
    *" byoffset "*)
      m=$(printf '%s\n' "$m" | by_offset)
      r=$(printf '%s\n' "$r" | by_offset) ;;
  esac
  if [ "$m" = "$r" ] && [ "${m#REF-}" = "$m" ]; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "### [$key] $name"
    [ -n "$flag" ] && echo "    (assembled with $flag)"
    printf '%s\n' "$src" | sed 's/^/    |/'
    diff <(printf '%s\n' "$m") <(printf '%s\n' "$r") | sed 's/^</  ref:  /; s/^>/  rsasm:/'
    if [ -n "$verbose" ] && [ -f "$d/ref.o" ] && [ -f "$d/rs.o" ]; then
      diff <(llvm-dwarfdump --all "$d/ref.o" | tail -n +2) \
        <(llvm-dwarfdump --all "$d/rs.o" | tail -n +2) |
        sed 's/^/    /'
    fi
  fi
  rm -rf "$d"
}

run_file() { # file, key, rsasm arch, reference, quirks
  local file=$1 key=$2 rs=$3 ref=$4 quirks=$5 snippet="" name="" flag="" line
  [ -f "$file" ] || return 0
  flush() {
    [ -z "$name" ] && return
    case "$snippet $quirks " in
      *.cfi_*) case " $quirks " in *" cfi "*) ;; *) return ;; esac ;;
    esac
    compare "$key" "$rs" "$ref" "$quirks" "$name" "$snippet" "$flag"
  }
  while IFS= read -r line; do
    case "$line" in
      "==="*" | "*) flush; snippet=""; name="${line#=== }"; flag="${name##* | }"; name="${name% | *}" ;;
      "==="*) flush; snippet=""; name="${line#=== }"; flag="" ;;
      *) snippet="$snippet$line
" ;;
    esac
  done < "$file"
  flush
}

wanted="${*:-}"
while IFS='|' read -r key rs ref quirks; do
  [ -z "$key" ] && continue
  if [ -n "$wanted" ]; then
    case " $wanted " in *" $key "*) ;; *) continue ;; esac
  fi
  before=$((pass + fail))
  for f in "$here"/common*.txt "$here/$key.txt" "$here/$key"-*.txt; do
    run_file "$f" "$key" "$rs" "$ref" "$quirks"
  done
  echo "[$key] $((pass + fail - before)) cases"
done <<< "$TARGETS"

echo "--- $pass matched, $fail differed"
[ "$fail" -eq 0 ]
