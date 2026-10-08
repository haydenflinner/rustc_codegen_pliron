#!/bin/bash
# Differential test of flat binaries against a reference assembler and linker.
#
# `rsasm -f bin --base <addr>` has to do the linker's job for every fixup in
# the file: a page-relative `adrp`, a PowerPC `@ha`, a MIPS `%hi`, a RISC-V
# `%pcrel_lo`. The other harnesses only compare relocatable output, where the
# linker does that part, so they cannot see a flat binary that gets it wrong.
# This one assembles each snippet with the reference assembler, links the
# object at the same address with a linker script that lays the sections out
# end to end in the order the object lists them — which is what rsasm's flat
# layout does — and compares the whole image with rsasm's.
#
#   tools/flat-diff/run.sh              # every target with a corpus
#   tools/flat-diff/run.sh aarch64 rx   # just these
#   FLAT_DIFF_SHOW=1 tools/flat-diff/run.sh ...  # also print each matching
#                                                # case as a tests/flat.rs row
#
# Corpora: tools/flat-diff/<corpus>.txt, snippets separated by `=== <name>`
# lines; the table below names the corpora each target reads, so variants
# that differ only in byte order share one. `=== <name> @ <addr>` links that
# snippet at <addr> instead of the target's default base, for the carries
# that only show at particular addresses. A snippet named `refused: ...` is
# one the linker rejects, and it matches when rsasm rejects it too.
#
# The linker is GNU ld 2.47, from the oracles directory, for every target;
# the host's GNU ld stands in for x86 when there is none there. A target whose
# assembler or linker is missing is skipped. lld is not a substitute: it
# rewrites `adrp` pairs and adds Thumb interworking stubs by default, has no
# SPARC `WDISP22`, and does not check a MIPS jump's 256 MB region.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin="${RSASM_ORACLES:-$root/target/oracles}/bin"

# key | corpora | rsasm arch | assembler | assembler flags | linkers | linker flags | base
#
# The assembler is `mc:<triple>` for llvm-mc, or a GNU as program name looked
# up in the oracles directory (or PATH). The linkers are
# a comma-separated list of GNU ld program names found the same way; the
# first one present is used.
#
# RISC-V is linked with --no-relax: the linker would otherwise shorten `call`
# sequences, which is an optimization rsasm does not attempt. So is MSP430,
# whose GNU ld relaxes by default and turns a `br` it can reach into a `jmp`. PowerPC64 is
# linked with --no-toc-optimize for the same reason. A linker flag written as
# a linker-script command, `OUTPUT_ARCH(sparc)`, goes into the script: that
# is the only way to have the V9 GNU ld write a 32-bit SPARC image.
#
# There is no MIPS64 row: the MIPS GNU ld among the oracles emulates only o32,
# so it cannot link an n64 object.
#
# AVR has a row per core, each with its own corpus: GNU ld refuses to link an
# object for one AVR core into an image for another it is not compatible with,
# so each is linked with its own emulation, and never with --relax. avr6 is
# linked with --no-stubs: GNU ld would otherwise route `gs()` through
# trampolines it adds, which only a linker can build.
TARGETS="
x86-64|x86-64|x86-64|x86_64-elf-as|--64|x86_64-elf-ld|-m elf_x86_64|0x401000
i386|i386|i386|x86_64-elf-as|--32|x86_64-elf-ld|-m elf_i386|0x8048000
aarch64|aarch64|aarch64|mc:aarch64||aarch64-elf-ld||0x400000
aarch64-gas|aarch64-gas|aarch64|aarch64-elf-as||aarch64-elf-ld||0x400000
arm|arm|arm|mc:armv7||arm-none-eabi-ld||0x8000
thumb|thumb|thumb|mc:thumbv7||arm-none-eabi-ld||0x8000
arm-gas|arm-gas|arm|arm-none-eabi-as|-march=armv7-a|arm-none-eabi-ld||0x8000
thumb-gas|thumb-gas|thumb|arm-none-eabi-as|-march=armv7-a -mthumb|arm-none-eabi-ld||0x8000
riscv32|riscv|riscv32|mc:riscv32|-mattr=+m,+a,+f,+d,+c|riscv64-elf-ld|-m elf32lriscv --no-relax|0x10000
riscv64|riscv|riscv64|mc:riscv64|-mattr=+m,+a,+f,+d,+c|riscv64-elf-ld|-m elf64lriscv --no-relax|0x10000
powerpc|powerpc|powerpc|mc:powerpc||powerpc64-linux-gnu-ld|-m elf32ppc|0x10000000
powerpc64|powerpc64|powerpc64|mc:powerpc64||powerpc64-linux-gnu-ld|-m elf64ppc --no-toc-optimize|0x10000000
powerpc64le|powerpc64|powerpc64le|mc:powerpc64le||powerpc64-linux-gnu-ld|-m elf64lppc --no-toc-optimize|0x10000000
mips|mips|mips|mc:mips||mips64-elf-ld||0x400000
mipsel|mips|mipsel|mc:mipsel||mips64-elf-ld|-EL|0x400000
sparc|sparc|sparc|mc:sparc||sparc64-elf-ld|-b elf32-sparc OUTPUT_FORMAT(elf32-sparc) OUTPUT_ARCH(sparc)|0x100000
sparcv9|sparc|sparcv9|mc:sparcv9||sparc64-elf-ld||0x100000
m68k|m68k|m68k|m68k-elf-as||m68k-elf-ld||0x10000
sh|sh|sh|sh-elf-as||sh-elf-ld||0x10000
shl|sh|shl|sh-elf-as|-little|sh-elf-ld|-EL|0x10000
rx|rx|rx|rx-elf-as||rx-elf-ld||0x10000
rl78|rl78|rl78|rl78-elf-as||rl78-elf-ld||0x2000
msp430|msp430|msp430|msp430-elf-as|-mcpu=430 -mP|msp430-elf-ld|--no-relax|0x1000
msp430x|msp430,msp430x|msp430x|msp430-elf-as|-mcpu=430x -mP|msp430-elf-ld|--no-relax|0x4000
v850|v850|v850|v850-elf-as||v850-elf-ld||0x100000
rh850|v850,rh850|rh850|v850-elf-as|-mv850e3v5|v850-elf-ld||0x100000
avr|avr|avr|avr-elf-as||avr-elf-ld||0x0
avr5|avr5|avr5|avr-elf-as|-mmcu=avr5|avr-elf-ld|-m avr5|0x0
avr51|avr51|avr51|avr-elf-as|-mmcu=avr51|avr-elf-ld|-m avr51|0x0
avr6|avr6|avr6|avr-elf-as|-mmcu=avr6|avr-elf-ld|-m avr6 --no-stubs|0x0
avrtiny|avrtiny|avrtiny|avr-elf-as|-mmcu=avrtiny|avr-elf-ld|-m avrtiny|0x0
"

command -v llvm-objcopy > /dev/null || { echo "llvm-objcopy not found; skipping" >&2; exit 0; }
command -v llvm-readelf > /dev/null || { echo "llvm-readelf not found; skipping" >&2; exit 0; }

cargo build --quiet --manifest-path "$root/Cargo.toml" --all-features --bin rsasm || exit 1
rsasm="$root/target/debug/rsasm"

pass=0
fail=0
skip=0

tool() { # name -> path, or nothing
  if [ -x "$bin/$1" ]; then echo "$bin/$1"; else command -v "$1"; fi
}

# Assembles and links $d/in.s at $base into $d/ref.bin. Prints why and
# returns 1 if it cannot.
reference() { # as asflags linkers ldflags base
  local as=$1 asflags=$2 linkers=$3 base=$5 asm l sections ldflags=() script=()
  for l in $4; do
    case "$l" in OUTPUT_*) script+=("$l") ;; *) ldflags+=("$l") ;; esac
  done
  case "$as" in
    mc:*)
      llvm-mc -triple="${as#mc:}" $asflags -filetype=obj -o "$d/ref.o" "$d/in.s" > "$d/log" 2>&1 ||
        { echo "REF-ERROR: $(head -3 "$d/log" | tr '\n' ' ')"; return 1; } ;;
    *)
      asm=$(tool "$as") || { echo "REF-MISSING: $as"; return 1; }
      "$asm" $asflags -o "$d/ref.o" "$d/in.s" > "$d/log" 2>&1 ||
        { echo "REF-ERROR: $(head -3 "$d/log" | tr '\n' ' ')"; return 1; } ;;
  esac
  local link=""
  for l in ${linkers//,/ }; do
    link=$(tool "$l") && break
  done
  [ -n "$link" ] || { echo "REF-MISSING: $linkers"; return 1; }
  # The allocated sections with contents, in the object's order, are the
  # image. Everything else is placed after them, where it cannot move the
  # image; left to itself the linker would put MIPS's `.reginfo`, or the
  # `.plt` the RL78 GNU ld makes whether or not anything uses it, straight
  # after `.text`.
  sections=$(llvm-readelf -S --wide "$d/ref.o" | sed -n 's/^ *\[ *[0-9]*\] //p' |
    awk '$2 == "PROGBITS" && $7 ~ /A/ { print $1 }')
  {
    [ ${#script[@]} -gt 0 ] && echo "${script[*]}"
    echo "SECTIONS {"
    echo "  . = $base;"
    for s in $sections; do echo "  $s : { *($s) }"; done
    echo "  .rest : { *(*) }"
    echo "}"
  } > "$d/link.ld"
  "$link" "${ldflags[@]}" -e "$base" -T "$d/link.ld" -o "$d/ref.elf" "$d/ref.o" > "$d/log" 2>&1 ||
    { echo "LINK-ERROR: $(grep -m3 -iE 'error|undefined|relocation' "$d/log" | tr '\n' ' ')"; return 1; }
  local only=()
  for s in $sections; do only+=(--only-section="$s"); done
  llvm-objcopy -O binary "${only[@]}" "$d/ref.elf" "$d/ref.bin" 2> "$d/log" ||
    { echo "REF-ERROR: objcopy: $(head -1 "$d/log")"; return 1; }
}

# Prints a case as a row of the tables in tests/flat.rs: the source, and the
# reference image as its length plus the runs of 16-byte lines that are not
# all zero, so that a `.space 0x8000` costs nothing. No image means the
# reference refused the snippet.
rust_case() { # key name base source [image]
  echo "    // [$1]"
  echo "    Case {"
  echo "        name: \"$2\","
  echo "        base: $3,"
  printf '        src: r#"%s"#,\n' "$4"
  if [ $# -lt 5 ]; then
    echo "        image: None,"
  else
    echo "        image: Some(($(wc -c < "$5"), &["
    xxd -p -c16 "$5" | awk '
      function flush() { if (run != "") printf "            (%#x, \"%s\"),\n", start, run }
      {
        off = (NR - 1) * 16
        bytes = $0; gsub(/../, "& ", bytes); sub(/ $/, "", bytes)
        if (bytes ~ /[1-9a-f]/) {
          if (run != "" && off == following) run = run " " bytes
          else { flush(); start = off; run = bytes }
          following = off + 16
        }
      }
      END { flush() }'
    echo "        ])),"
  fi
  echo "    },"
}

# Hex of a file, 16 bytes to a line, with the image address of each line.
dump() { # file base
  xxd -g1 -o "$(($2))" "$1" | cut -c1-58
}

compare() { # key arch as asflags linkers ldflags base name source
  local key=$1 arch=$2 base=$7 name=$8 src=$9 m r=""
  case "$name" in *" @ "*) base=${name##* @ }; name=${name% @ *} ;; esac
  d=$(mktemp -d)
  printf '%s\n' "$src" > "$d/in.s"
  m=$(reference "$3" "$4" "$5" "$6" "$base")
  case "$m" in
    REF-MISSING:*) skip=$((skip + 1)); rm -rf "$d"; return ;;
  esac
  if ! "$rsasm" -a "$arch" -f bin --base "$base" -o "$d/rs.bin" "$d/in.s" > "$d/rslog" 2>&1; then
    r="RSASM-ERROR: $(tr '\n' ' ' < "$d/rslog")"
  fi
  if [ -z "$m" ] && [ -z "$r" ] && cmp -s "$d/ref.bin" "$d/rs.bin"; then
    pass=$((pass + 1))
    [ -n "${FLAT_DIFF_SHOW:-}" ] && rust_case "$key" "$name" "$base" "$src" "$d/ref.bin"
  elif [ "${name#refused: }" != "$name" ] && [ "${m#LINK-ERROR}" != "$m" ] && [ -n "$r" ]; then
    pass=$((pass + 1))
    [ -n "${FLAT_DIFF_SHOW:-}" ] && rust_case "$key" "$name" "$base" "$src"
  else
    fail=$((fail + 1))
    echo "### [$key] $name @ $base"
    printf '%s\n' "$src" | sed 's/^/    |/'
    if [ -n "$m" ] || [ -n "$r" ]; then
      echo "  reference: ${m:-ok}"
      echo "  rsasm:     ${r:-ok}"
    else
      # Only the lines that differ, so a mismatch in a large image stays legible.
      diff <(dump "$d/ref.bin" "$base") <(dump "$d/rs.bin" "$base") |
        sed -n 's/^< /  reference: /p; s/^> /  rsasm:     /p'
    fi
  fi
  rm -rf "$d"
}

run_target() { # corpora key arch as asflags linkers ldflags base
  local corpora=$1 before=$((pass + fail)) snippet name progs
  shift
  for progs in ${corpora//,/ }; do
    [ -f "$here/$progs.txt" ] || continue
    snippet="" name=""
    while IFS= read -r line; do
      case "$line" in
        "==="*)
          [ -n "$name" ] && compare "$@" "$name" "$snippet"
          snippet=""; name="${line#=== }" ;;
        *) snippet="$snippet$line
" ;;
      esac
    done < "$here/$progs.txt"
    [ -n "$name" ] && compare "$@" "$name" "$snippet"
  done
  local n=$((pass + fail - before))
  [ "$n" -gt 0 ] && echo "[$1] $n cases"
  return 0
}

wanted="${*:-}"
while IFS='|' read -r key corpus arch as asflags linkers ldflags base; do
  [ -z "$key" ] && continue
  if [ -n "$wanted" ]; then
    case " $wanted " in *" $key "*) ;; *) continue ;; esac
  fi
  run_target "$corpus" "$key" "$arch" "$as" "$asflags" "$linkers" "$ldflags" "$base"
done <<< "$TARGETS"

[ "$skip" -gt 0 ] && echo "($skip cases skipped: no reference assembler or linker)"
echo "--- $pass matched, $fail differed"
[ "$fail" -eq 0 ]
