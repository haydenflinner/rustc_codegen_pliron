#!/bin/bash
# Differential test against cross assemblers built by tools/oracles/build.sh.
#
# For targets neither llvm-mc nor the host's GNU as can assemble: m68k (in
# GNU and Motorola syntax), V850/RH850, RL78, RX, SuperH, AVR, MSP430, and the 8-bit
# Z80, 6502, 8080 and 8051. Assembles a corpus with rsasm and with the
# reference, and compares the code bytes. ARM and Thumb, which llvm-mc does
# assemble, are here too, as whole objects, for what GNU as decides and
# llvm-mc decides differently: literal pools, interworking and mapping
# symbols. So is PowerPC, for its AltiVec, VSX and POWER10 instructions:
# llvm-mc checks them too, and the two references accept different mnemonics
# and ranges, and only GNU as writes the absolute 34-bit relocation.
#
#   tools/xas-diff/run.sh              # every target with a corpus
#   tools/xas-diff/run.sh m68k rx      # just these
#
# Corpora: tools/xas-diff/<key>.txt, one statement per line, and optionally
# <key>-programs.txt with multi-line snippets separated by `=== <name>`.
# Motorola source is column-sensitive, so indent instructions in those corpora.
#
# Snippets in <key>-relocs.txt, in the programs format, are compared as whole
# objects instead: every allocated section's header and bytes, the global,
# weak and undefined symbols, and the relocations, as tools/mc-diff/canon.sh
# prints them; for ARM and Thumb also `e_flags` and every local symbol, with
# `canon.sh --full`, and for AVR `e_flags`, with `canon.sh --flags`. A snippet
# there named `refused: ...` matches when both assemblers reject it.
#
# AArch64 is here for its literal pools, which are GNU as's feature: where
# the pool goes, what it shares and how its runs are aligned is decided across
# instructions, and llvm-mc decides differently (it turns `ldr x0, =1` into
# `mov x0, #1`, and writes its entries in the order they were used). Its
# system instructions are here too: their operand names come from GNU's
# tables, and most of the newer ones llvm-mc does not know. The CPU string
# names every extension GNU as has a name for, since a system register is
# refused for the CPU rather than unknown; tools/tables/aarch64-sys.py, which
# generates the corpus, builds the same string from GNU as's own list.
#
# ARM is checked against GNU as for ARMv7-A, whose Thumb-2 no-ops and
# interworking rules are what `-march=armv7ve` gives; without a `-march` GNU
# as assumes an ARMv4T-era CPU. `armv7ve` is ARMv7-A with the security,
# virtualization and divide extensions, which together are the whole set this
# backend claims -- `smc`, `hvc`, `sdiv` and the banked `msr` need them, and
# `-mfpu=neon-vfpv4` adds the floating-point unit and NEON. Its
# snippets are assembled after `.syntax unified`, GNU as's default being the
# older divided syntax, and rsasm knowing only the unified one; the
# `-relocs` snippets say so themselves, since they are whole programs.
#
# A vendor syntax no reference assembler reads (CC-RL, CC-RH, CC-RX) is checked in
# pairs instead: <key>-pairs.txt holds snippets separated by `=== <name>`, each
# split by a `--- gnu` line into the vendor source, which rsasm assembles in
# the key's dialect, and the GNU-syntax source that means the same thing,
# which the reference assembles. The pairing itself is what is under test.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
# RSASM_ORACLES points somewhere else, so a worktree or a CI cache can share one
# build of the references instead of rebuilding binutils per checkout.
bin="${RSASM_ORACLES:-$root/target/oracles}/bin"

# key | rsasm arch | rsasm dialect | reference command | how to get the code
#     [| the key whose one-line corpus this one shares]
#
# The extraction is `elf:<section>` for an ELF object — RX keeps code in `P`,
# the Renesas name, not `.text` — or `bin` for a flat binary. The 8-bit
# references need a second tool to make one: `linked:<ld>` links the object at
# address 0 and takes `.text`, which resolves the absolute addresses an
# unlinked object leaves as relocations; `ld65` lays a ca65 object out with
# ld65; `p2bin` converts AS's code file; `sdld` links an sdas object into
# Intel HEX and takes the image from that. rsasm assembles a flat binary for
# all of them, so a leading `org` is the image's load address on both sides.
#
# The 8051 has two references, which read different source. AS reads the
# Intel syntax the 8-bit dialect does, but predefines no register names, so
# every i8051 snippet is assembled after AS's own `stddef51.inc`, the header
# rsasm's names were taken from. sdas8051 has asxxxx's directives, `0x`
# numbers and `.` for the location counter, and needs an absolute area before
# an `.org`, so its snippets are in a corpus of their own written in the
# spelling the two share. A snippet is not compared with the prelude: rsasm
# sees only the snippet.
#
# `p2hex` compares Intel HEX text instead of an image: AS's p2hex against
# `rsasm -f ihex`. p2hex leaves reserved space and `ORG` gaps out of its
# records where rsasm writes the zeros `-f bin` has, so those programs are
# written without gaps.
#
# MSP430 is checked for each ISA GNU as's `-mcpu` selects. The polymorphic
# branches (`jump`, `beq`, ...) need `-mP` there, which also leaves more to
# the linker in data sections, so they have corpora of their own.
#
# vasm is only a secondary reference, run with `-no-opt -devpac`. By default it
# is an optimizing assembler that rewrites instructions (`move.l #1,d0` becomes
# `moveq #1,d0`) and deletes branches, which is not what rsasm or GNU as do;
# with -no-opt it also stops choosing absolute-short addresses, which GNU as
# does. So GNU as `--mri` is the reference for Motorola encodings, and vasm
# corpora should hold only cases with no size choice in them.
#
# The m68k keys after the first three are one per CPU model, named as
# `-mcpu` names it: their corpora are generated by tools/fuzz/m68k.py from
# GNU's opcode table, so that between them every form in it is assembled once.
TARGETS="
m68k|m68k|gas|m68k-elf-as|elf:.text
m68k-mot|m68k|motorola|m68k-elf-as --mri|elf:.text
m68k-vasm|m68k|motorola|vasmm68k_mot -quiet -no-opt -devpac -Fbin|bin
m68k-vasm-020|m68k|motorola|vasmm68k_mot -quiet -no-opt -devpac -m68020 -m68881 -m68851 -Fbin|bin
m68k-000|68000|gas|m68k-elf-as -m68000|elf:.text
m68k-020|m68k|gas|m68k-elf-as -m68020|elf:.text
m68k-020-mot|m68k|motorola|m68k-elf-as --mri -m68020|elf:.text
m68k-030|68030|gas|m68k-elf-as -m68030|elf:.text
m68k-030-mot|68030|motorola|m68k-elf-as --mri -m68030|elf:.text
m68k-040|68040|gas|m68k-elf-as -m68040|elf:.text
m68k-040-mot|68040|motorola|m68k-elf-as --mri -m68040|elf:.text
m68k-060|68060|gas|m68k-elf-as -m68060|elf:.text
m68k-060-mot|68060|motorola|m68k-elf-as --mri -m68060|elf:.text
m68k-cpu32|cpu32|gas|m68k-elf-as -mcpu32|elf:.text
m68k-cpu32-mot|cpu32|motorola|m68k-elf-as --mri -mcpu32|elf:.text
m68k-fido|fidoa|gas|m68k-elf-as -mcpu=fidoa|elf:.text
m68k-fido-mot|fidoa|motorola|m68k-elf-as --mri -mcpu=fidoa|elf:.text
m68k-5475|5475|gas|m68k-elf-as -mcpu=5475|elf:.text
m68k-5475-mot|5475|motorola|m68k-elf-as --mri -mcpu=5475|elf:.text
m68k-54455|54455|gas|m68k-elf-as -mcpu=54455|elf:.text
m68k-54455-mot|54455|motorola|m68k-elf-as --mri -mcpu=54455|elf:.text
m68k-5208|5208|gas|m68k-elf-as -mcpu=5208|elf:.text
m68k-5208-mot|5208|motorola|m68k-elf-as --mri -mcpu=5208|elf:.text
v850|v850|gas|v850-elf-as|elf:.text
rh850|rh850|gas|v850-elf-as -mv850e3v5|elf:.text
rl78|rl78|gas|rl78-elf-as|elf:.text
rx|rx|gas|rx-elf-as|elf:P
sh|sh|gas|sh-elf-as|elf:.text
shl|shl|gas|sh-elf-as -little|elf:.text
rl78-ccrl|rl78|ccrl|rl78-elf-as|elf:.text
rh850-ccrh|rh850|ccrh|v850-elf-as -mv850e3v5|elf:.text
rx-ccrx|rx|ccrx|rx-elf-as|elf:P
6502|6502|8bit|ca65|ld65
6502-vasm|6502|8bit|vasm6502_oldstyle -quiet -Fbin|bin
z80|z80|8bit|z80-elf-as|linked:z80-elf-ld
z80-gas|z80|gas|z80-elf-as|linked:z80-elf-ld|z80
z80-vasm|z80|8bit|vasmz80_oldstyle -quiet -Fbin|bin|z80
i8080|i8080|8bit|asl -cpu 8080|p2bin
i8051|8051|8bit|asl -cpu 8051 -i $bin/../share/asl|p2bin
i8051-sdas|8051|8bit|sdas8051 -o|sdld
i8051-hex|8051|8bit|asl -cpu 8051 -i $bin/../share/asl|p2hex
aarch64|aarch64|gas|aarch64-elf-as -march=armv9.5-a+crc+crypto+fp+lse+lsfe+lse128+lsui+simd+pan+lor+ras+rdma+fp16+fp16fml+fprcvt+profile+sve+tme+fcma+jscvt+rcpc+rcpc2+dotprod+sha2+frintts+sb+predres+predres2+poe2+tev+aes+sm4+sha3+rng+ssbs+lscp+memtag+occmo+cmpbr+sve2+sve2-sm4+sve2-aes+sve2-sha3+sve2-bitperm+sme+sme-f64f64+sme-i16i64+sme2+bf16+i8mm+f32mm+f64mm+ls64+flagm+flagm2+pauth+xs+wfxt+mops+hbc+cssc+chk+gcs+the+rasv2+ite+d128+sve-b16b16+sve-bfscale+sme2p1+sve2p1+sve-f16f32mm+f8f32mm+f8f16mm+sve-aes+sve-aes2+ssve-aes+sve-bitperm+ssve-bitperm+rcpc3+cpa+faminmax+fp8+lut+brbe+sme-lutv2+fp8fma+fp8dot4+fp8dot2+ssve-fp8fma+ssve-fp8dot4+ssve-fp8dot2+sme-f8f32+sme-f8f16+sme-f16f16+sme-b16b16+pops+sve2p2+sme2p2+gcie+ssve-fexpa+sme-tmop+sme-mop4+mops-go+sve2p3+sme2p3+f16f32dot+f16f32mm+f16mm+sve-b16mm+mtetc+tlbid+sme-fa64|elf:.text
arm|arm|gas|arm-none-eabi-as -march=armv7ve -mfpu=neon-vfpv4|elf:.text
msp430|msp430|gas|msp430-elf-as -mcpu=430|elf:.text
msp430x|msp430x|gas|msp430-elf-as -mcpu=430x|elf:.text
msp430xv2|msp430xv2|gas|msp430-elf-as -mcpu=430xv2|elf:.text|msp430
msp430-poly|msp430|gas|msp430-elf-as -mcpu=430 -mP|elf:.text
msp430x-poly|msp430x|gas|msp430-elf-as -mcpu=430x -mP|elf:.text|msp430-poly
thumb|thumb|gas|arm-none-eabi-as -march=armv7ve -mfpu=neon-vfpv4 -mthumb|elf:.text
powerpc64|powerpc64|gas|powerpc64-linux-gnu-as -a64 -mbig -mfuture|elf:.text
powerpc64le|powerpc64le|gas|powerpc64-linux-gnu-as -a64 -mlittle -mfuture|elf:.text|powerpc64
powerpc|powerpc|gas|powerpc64-linux-gnu-as -a32 -mbig -mfuture|elf:.text
avr|avr|gas|avr-elf-as|elf:.text
avr51|avr51|gas|avr-elf-as -mmcu=avr51|elf:.text
avrxmega|atxmega128a1u|gas|avr-elf-as -mmcu=atxmega128a1u|elf:.text
avrtiny|avrtiny|gas|avr-elf-as -mmcu=avrtiny|elf:.text
"

[ -d "$bin" ] || { echo "no oracles in $bin; run tools/oracles/build.sh" >&2; exit 0; }
command -v llvm-objcopy > /dev/null || { echo "llvm-objcopy not found" >&2; exit 0; }
command -v llvm-readobj > /dev/null || { echo "llvm-readobj not found" >&2; exit 0; }
cargo build --quiet --manifest-path "$root/Cargo.toml" --all-features --example hexdump --bin rsasm ||
  exit 1
hexdump="$root/target/debug/examples/hexdump"
rsasm="$root/target/debug/rsasm"

pass=0
fail=0

# Source a reference needs ahead of every snippet that rsasm does not; see
# the 8051 note above.
prelude() { # key
  case "$1" in
    arm | thumb) printf '\t.syntax unified\n' ;;
    i8051 | i8051-hex) printf '\tinclude "stddef51.inc"\n' ;;
    i8051-sdas) printf '\t.area CSEG (ABS)\n' ;;
  esac
}

reference() { # command, extraction; source on stdin
  local cmd=$1 extract=$2 d tool
  d=$(mktemp -d)
  cat > "$d/in.s"
  tool=${cmd%% *}
  if [ ! -x "$bin/$tool" ]; then
    echo "REF-MISSING: $tool"; rm -rf "$d"; return
  fi
  case "$extract" in
    ld65)
      # One memory area from address 0, holding every segment ca65 names.
      printf '%s\n' 'MEMORY { M: start = 0, size = $10000, file = %O; }' \
        'SEGMENTS { CODE: load = M, type = rw; RODATA: load = M, type = rw, optional = yes;' \
        '  DATA: load = M, type = rw, optional = yes; ZEROPAGE: load = M, type = rw, optional = yes; }' \
        > "$d/flat.cfg"
      if ! (cd "$d" && "$bin/$tool" ${cmd#"$tool"} -o in.o in.s > log 2>&1 &&
        "$bin/ld65" -C flat.cfg -o out.bin in.o >> log 2>&1); then
        echo "REF-ERROR: $(grep -m2 -iE 'error' "$d/log" | tr '\n' ' ')"; rm -rf "$d"; return
      fi ;;
    p2bin)
      if ! (cd "$d" && "$bin/$tool" ${cmd#"$tool"} -q -o in.p in.s > log 2>&1 &&
        "$bin/p2bin" -q -l 0 in.p out.bin >> log 2>&1) || grep -q 'error' "$d/log"; then
        echo "REF-ERROR: $(grep -m2 -iE 'error' "$d/log" | tr '\n' ' ')"; rm -rf "$d"; return
      fi ;;
    p2hex)
      if ! (cd "$d" && "$bin/$tool" ${cmd#"$tool"} -q -o in.p in.s > log 2>&1 &&
        "$bin/p2hex" -q in.p out.hex >> log 2>&1) || grep -q 'error' "$d/log"; then
        echo "REF-ERROR: $(grep -m2 -iE 'error' "$d/log" | tr '\n' ' ')"; rm -rf "$d"; return
      fi
      tr -d '\r' < "$d/out.hex" | tr '\n' ' ' | sed 's/ $//'
      echo
      rm -rf "$d"
      return ;;
    sdld)
      # sdas8051 truncates an operand that does not fit without a word, and
      # leaves a branch that does not reach to sdld, which warns and may still
      # write the file; either message is a refusal.
      if ! (cd "$d" && "$bin/$tool" ${cmd#"$tool"} in.s > log 2>&1 &&
        "$bin/sdld" -i out.ihx in.rel >> log 2>&1) || grep -qE 'Error|Warning' "$d/log"; then
        echo "REF-ERROR: $(grep -m2 -E 'Error|Warning' "$d/log" | tr '\n' ' ')"; rm -rf "$d"; return
      fi
      llvm-objcopy -I ihex -O binary "$d/out.ihx" "$d/out.bin" 2>/dev/null ;;
    linked:*)
      if ! (cd "$d" && "$bin/$tool" ${cmd#"$tool"} -o in.o in.s > log 2>&1 &&
        "$bin/${extract#linked:}" -e 0 -Ttext=0 -o out.elf in.o >> log 2>&1); then
        echo "REF-ERROR: $(head -3 "$d/log" | tr '\n' ' ')"; rm -rf "$d"; return
      fi
      llvm-objcopy -O binary --only-section=.text "$d/out.elf" "$d/out.bin" 2>/dev/null ;;
    bin)
      if ! (cd "$d" && "$bin/$tool" ${cmd#"$tool"} -o out.bin in.s > log 2>&1); then
        echo "REF-ERROR: $(grep -m2 -iE 'error|fatal' "$d/log" | tr '\n' ' ')"; rm -rf "$d"; return
      fi ;;
    elf:*)
      if ! (cd "$d" && "$bin/$tool" ${cmd#"$tool"} -o out.o in.s > log 2>&1); then
        echo "REF-ERROR: $(head -3 "$d/log" | tr '\n' ' ')"; rm -rf "$d"; return
      fi
      llvm-objcopy -O binary --only-section="${extract#elf:}" "$d/out.o" "$d/out.bin" 2>/dev/null ;;
  esac
  [ -f "$d/out.bin" ] && xxd -p "$d/out.bin" | tr -d '\n' | sed 's/../& /g;s/ $//'
  echo
  rm -rf "$d"
}

rsasm_ihex() { # arch dialect source: rsasm's Intel HEX, one line
  local d
  d=$(mktemp -d)
  printf '%s\n' "$3" > "$d/in.s"
  if "$rsasm" -a "$1" -d "$2" -f ihex -o "$d/out.hex" "$d/in.s" > "$d/log" 2>&1; then
    tr '\n' ' ' < "$d/out.hex" | sed 's/ $//'
    echo
  else
    echo "RSASM-ERROR: $(head -3 "$d/log" | tr '\n' ' ')"
  fi
  rm -rf "$d"
}

compare() { # key arch dialect cmd extract name source [reference-source]
  local r m gnu="${8-$7}" format=bin
  m=$({ prelude "$1"; printf '%s\n' "$gnu"; } | reference "$4" "$5")
  # Everything but an unlinked ELF object is compared as a flat image.
  [ "${5%%:*}" = elf ] && format=elf
  if [ "$5" = p2hex ]; then
    r=$(rsasm_ihex "$2" "$3" "$7")
  else
    r=$(printf '%s\n' "$7" | "$hexdump" "$2" "$3" "$format" 2>&1)
  fi
  # A reference that fails is never a match: a pair whose GNU half does not
  # assemble proves nothing about the vendor half.
  if [ "$m" = "$r" ] && [ "${m#REF-}" = "$m" ]; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "### [$1] $6"
    printf '%s\n' "$7" | sed 's/^/    |/'
    if [ $# -ge 8 ]; then
      echo "  --- gnu"
      printf '%s\n' "$gnu" | sed 's/^/    |/'
    fi
    echo "  reference: $m"
    echo "  rsasm:     $r"
  fi
}

compare_object() { # key arch dialect cmd name source
  local d m r tool=${4%% *} flags="" full=""
  # GNU as for RX renames `.text`, `.data` and `.bss` to Renesas's `P`, `D_1`
  # and `B_1`, which rsasm does not; asked to keep the usual names, it does.
  case "$1" in rx*) flags=-muse-conventional-section-names ;; esac
  # ARM and AArch64 objects are compared whole, local symbols — the mapping
  # symbols among them — and `e_flags` included.
  # AVR objects with their `e_flags`, which name the core and say the object
  # is prepared for linker relaxation; their local symbols are the labels
  # relocations name, which canon.sh already reads by section and offset.
  # GNU as for AVR writes uninitialized bytes into the record addresses in
  # `.avr.prop`, which differ from run to run; the linker takes those fields
  # from the relocations, so they are blanked before the comparison.
  case "$1" in
    arm | thumb | aarch64) full=--full ;;
    avr*) full="--flags --zero-relocated .avr.prop=4" ;;
  esac
  d=$(mktemp -d)
  printf '%s\n' "$6" > "$d/in.s"
  if [ ! -x "$bin/$tool" ]; then
    m="REF-MISSING: $tool"
  elif (cd "$d" && "$bin/$tool" ${4#"$tool"} $flags -o ref.o in.s > log 2>&1); then
    m=$("$root/tools/mc-diff/canon.sh" $full "$d/ref.o")
  else
    m="REF-ERROR: $(head -3 "$d/log" | tr '\n' ' ')"
  fi
  if "$rsasm" -a "$2" -d "$3" -o "$d/rs.o" "$d/in.s" > "$d/log" 2>&1; then
    r=$("$root/tools/mc-diff/canon.sh" $full "$d/rs.o")
  else
    r="RSASM-ERROR: $(head -3 "$d/log" | tr '\n' ' ')"
  fi
  rm -rf "$d"
  if [ "${5#refused: }" != "$5" ]; then
    # Both have to refuse it; matching output would mean neither did.
    if [ "${m#REF-ERROR}" != "$m" ] && [ "${r#RSASM-ERROR}" != "$r" ]; then
      pass=$((pass + 1))
      return
    fi
  elif [ "$m" = "$r" ] && [ "${m#REF-}" = "$m" ]; then
    pass=$((pass + 1))
    return
  fi
  fail=$((fail + 1))
  echo "### [$1] $5 (object)"
  printf '%s\n' "$6" | sed 's/^/    |/'
  if [ "${m#REF-}" != "$m" ] || [ "${r#RSASM-}" != "$r" ] || [ "$m" = "$r" ]; then
    echo "  reference: ${m:0:300}"
    echo "  rsasm:     ${r:0:300}"
  else
    diff <(printf '%s\n' "$m") <(printf '%s\n' "$r") | sed -n 's/^< /  reference: /p; s/^> /  rsasm:     /p'
  fi
}

run_target() { # key arch dialect cmd extract corpus
  local lines="$here/$6.txt" progs="$here/$1-programs.txt" before=$((pass + fail))
  if [ -f "$lines" ]; then
    while IFS= read -r line; do
      [ -z "${line// /}" ] && continue
      case "$line" in \#*) continue ;; esac
      compare "$1" "$2" "$3" "$4" "$5" "$line" "$line"
    done < "$lines"
  fi
  if [ -f "$progs" ]; then
    local snippet="" name=""
    while IFS= read -r line; do
      case "$line" in
        "==="*)
          [ -n "$snippet" ] && compare "$1" "$2" "$3" "$4" "$5" "$name" "$snippet"
          snippet=""; name="${line#=== }" ;;
        *) snippet="$snippet$line
" ;;
      esac
    done < "$progs"
    [ -n "$snippet" ] && compare "$1" "$2" "$3" "$4" "$5" "$name" "$snippet"
  fi
  local objs="$here/$1-relocs.txt"
  if [ -f "$objs" ]; then
    local snippet="" name=""
    while IFS= read -r line; do
      case "$line" in
        "==="*)
          [ -n "$snippet" ] && compare_object "$1" "$2" "$3" "$4" "$name" "$snippet"
          snippet=""; name="${line#=== }" ;;
        *) snippet="$snippet$line
" ;;
      esac
    done < "$objs"
    [ -n "$snippet" ] && compare_object "$1" "$2" "$3" "$4" "$name" "$snippet"
  fi
  local pairs="$here/$1-pairs.txt"
  if [ -f "$pairs" ]; then
    local vendor="" gnu="" side="" name=""
    flush_pair() {
      if [ -n "$name" ]; then
        if [ "$side" = gnu ]; then
          compare "$1" "$2" "$3" "$4" "$5" "$name" "$vendor" "$gnu"
        else
          fail=$((fail + 1))
          echo "### [$1] $name: no \`--- gnu\` half"
        fi
      fi
    }
    while IFS= read -r line; do
      case "$line" in
        "==="*)
          flush_pair "$@"
          vendor=""; gnu=""; side=""; name="${line#=== }" ;;
        "--- gnu") side=gnu ;;
        *)
          if [ "$side" = gnu ]; then
            gnu="$gnu$line
"
          else
            vendor="$vendor$line
"
          fi ;;
      esac
    done < "$pairs"
    flush_pair "$@"
  fi
  local n=$((pass + fail - before))
  [ "$n" -gt 0 ] && echo "[$1] $n cases"
  return 0
}

wanted="${*:-}"
while IFS='|' read -r key arch dialect cmd extract corpus; do
  [ -z "$key" ] && continue
  if [ -n "$wanted" ]; then
    case " $wanted " in *" $key "*) ;; *) continue ;; esac
  fi
  corpus=${corpus:-$key}
  [ -f "$here/$corpus.txt" ] || [ -f "$here/$key-programs.txt" ] ||
    [ -f "$here/$key-pairs.txt" ] || [ -f "$here/$key-relocs.txt" ] || continue
  run_target "$key" "$arch" "$dialect" "$cmd" "$extract" "$corpus"
done <<< "$TARGETS"

echo "--- $pass matched, $fail differed"
[ "$fail" -eq 0 ]
