#!/bin/bash
# Differential test of Mach-O objects against llvm-mc.
#
# Assembles the same source with `rsasm -a <triple>` and with
# `llvm-mc -triple=<triple> -filetype=obj`, and compares the two objects as
# canon.sh prints them: header, load commands, every section's header and
# bytes, the symbol table and each relocation.
#
#   tools/macho-diff/run.sh              # every machine with a corpus
#   tools/macho-diff/run.sh arm64        # just one
#
# Corpora live in tools/macho-diff/<arch>.txt, one statement per line, each
# assembled as a file of its own, and tools/macho-diff/<arch>-programs.txt,
# multi-line snippets separated by `=== <name>` lines. A line in the first can
# hold several statements separated by `;`, which becomes a new line before
# either assembler sees it (Darwin's arm64 assembly comments with `;`). The
# machine's llvm-mc corpora in tools/mc-diff run as well, in the same way.
#
# Matching objects are also compared byte for byte, and the count printed;
# set MACHO_DIFF_BYTES=1 to list the ones that only match canonically.
#
# A case both assemblers refuse counts as a match: Mach-O has no relocation
# for a good many things ELF can express (`adr` to another atom, a 32-bit
# absolute address on x86-64), and refusing those is part of what is checked.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

# arch | rsasm target | llvm triple | the machine's corpus in tools/mc-diff
#      [| the flags llvm-mc needs for that corpus]
#
# The arm64 flags are the ones tools/mc-diff passes for `aarch64`: the corpus
# it shares has SIMD, SVE and SME in it, and llvm-mc enables neither for a
# bare triple. rsasm assembles every extension its backend has, whatever the
# object format.
ARCHES="
x86-64|x86_64-apple-macos|x86_64-apple-macos|x86-64
arm64|arm64-apple-macos|arm64-apple-macos|aarch64|-mattr=+v9.5a,+sve2,+sve2p1,+sve2-aes,+sve2-sha3,+sve2-sm4,+sve2-bitperm,+sve-aes2,+sve-b16b16,+sve-bfscale,+sve-f16f32mm,+crypto,+dotprod,+i8mm,+fullfp16,+bf16,+lse,+rcpc,+rand,+memtag,+pauth,+fp16fml,+flagm,+sb,+ssbs,+predres,+tme,+ls64,+f64mm,+f32mm,+jsconv,+complxnum,+rcpc3,+cssc,+the,+d128,+lut,+faminmax,+fp8,+fp8fma,+fp8dot2,+fp8dot4,+sme,+sme2,+sme2p1
"

for tool in llvm-mc llvm-readobj llvm-objdump; do
  command -v "$tool" >/dev/null || { echo "$tool not found; skipping" >&2; exit 0; }
done
echo "oracle: $(llvm-mc --version | grep -m1 -oE 'LLVM version [0-9.]+')"
case "$(llvm-mc --version)" in
  *"LLVM version 22."*) ;;
  *) echo "warning: the corpora were verified against LLVM 22; expect version drift" >&2 ;;
esac
cargo build --quiet --manifest-path "$root/Cargo.toml" --all-features --bin rsasm || exit 1
rsasm="$root/target/debug/rsasm"

pass=0
fail=0
identical=0

compare() { # arch, rsasm target, triple, flags, name, source
  local arch=$1 target=$2 triple=$3 flags=$4 name=$5 src=$6 m r d
  d=$(mktemp -d)
  printf '%s\n' "$src" > "$d/in.s"
  if llvm-mc -triple="$triple" $flags -filetype=obj -o "$d/m.o" "$d/in.s" 2> "$d/merr"; then
    m=$("$here/canon.sh" "$d/m.o")
  else
    m="refused"
  fi
  if "$rsasm" -a "$target" -o "$d/r.o" "$d/in.s" 2> "$d/rerr"; then
    r=$("$here/canon.sh" "$d/r.o")
  else
    r="refused"
  fi
  if [ "$m" = "$r" ]; then
    pass=$((pass + 1))
    if [ "$m" != refused ] && cmp -s "$d/m.o" "$d/r.o"; then
      identical=$((identical + 1))
    elif [ "$m" != refused ] && [ -n "${MACHO_DIFF_BYTES:-}" ]; then
      echo "### [$arch] $name (equal, but not byte for byte)"
    fi
  else
    fail=$((fail + 1))
    echo "### [$arch] $name"
    printf '%s\n' "$src" | sed 's/^/    /'
    if [ "$m" = refused ] || [ "$r" = refused ]; then
      echo "  llvm-mc: $( [ "$m" = refused ] && head -2 "$d/merr" | tr '\n' ' ' || echo accepted)"
      echo "  rsasm:   $( [ "$r" = refused ] && head -2 "$d/rerr" | tr '\n' ' ' || echo accepted)"
    else
      diff <(printf '%s\n' "$m") <(printf '%s\n' "$r") | sed 's/^/  /'
    fi
  fi
  rm -rf "$d"
}

# Runs `compare` over each `=== name` snippet of a file.
snippets() { # file, arch, rsasm target, triple, flags
  local file=$1 snippet="" name="" line
  shift
  while IFS= read -r line; do
    case "$line" in
      "==="*)
        [ -n "$snippet" ] && compare "$@" "$name" "$snippet"
        snippet=""; name="${line#=== }" ;;
      *) snippet="$snippet$line
" ;;
    esac
  done < "$file"
  [ -n "$snippet" ] && compare "$@" "$name" "$snippet"
  return 0
}

# Runs `compare` over each line of a file.
lines() { # file, arch, rsasm target, triple, flags
  local file=$1 line
  shift
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    case "$line" in \#*) continue ;; esac
    compare "$@" "$line" "$(printf '%s\n' "$line" | tr ';' '\n')"
  done < "$file"
}

run_arch() { # arch, rsasm target, triple, mc-diff corpus, llvm-mc flags
  local arch=$1 mc="$root/tools/mc-diff/$4" before=$((pass + fail)) own
  set -- "$1" "$2" "$3" "${5-}"
  [ -f "$here/$arch.txt" ] && lines "$here/$arch.txt" "$@"
  [ -f "$here/$arch-programs.txt" ] && snippets "$here/$arch-programs.txt" "$@"
  own=$((pass + fail - before))
  # The ELF corpus for the same machine, which is written for llvm-mc too:
  # every instruction in it has to come out the same in a Mach-O object, and
  # every reference in it relocated as llvm-mc relocates it there.
  [ -f "$mc.txt" ] && lines "$mc.txt" "$@"
  [ -f "$mc-programs.txt" ] && snippets "$mc-programs.txt" "$@"
  echo "[$arch] $own cases, and $((pass + fail - before - own)) from tools/mc-diff"
}

wanted="${*:-}"
while IFS='|' read -r arch target triple mc flags; do
  [ -z "$arch" ] && continue
  if [ -n "$wanted" ]; then
    case " $wanted " in *" $arch "*) ;; *) continue ;; esac
  fi
  run_arch "$arch" "$target" "$triple" "$mc" "${flags:-}"
done <<< "$ARCHES"

echo "--- $pass matched, $fail differed ($identical of the objects byte for byte)"
[ "$fail" -eq 0 ]
