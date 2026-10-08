#!/bin/bash
# Differential test of PE/COFF objects (`rsasm -f coff`) against llvm-mc, with
# GNU as for mingw as a second opinion on relocations.
#
#   tools/coff-diff/run.sh              # every target with a corpus
#   tools/coff-diff/run.sh aarch64      # just one
#
# llvm-mc is the reference for whole objects on all three machines: it is the
# one assembler here that writes COFF for x86-64, i386 and ARM64 alike, and
# rsasm follows it in everything the two references disagree on (see
# README.md). Each case is assembled by both and compared as canon.sh prints
# it: every section's characteristics and bytes, every symbol with its
# auxiliary records, and every relocation.
#
# GNU as (x86_64-w64-mingw32-as and i686-w64-mingw32-as, from
# tools/oracles/build.sh) disagrees with llvm-mc about nearly everything else
# in a COFF object — section alignment and padding, symbol order, the `.file`
# symbol, whether a relocation names a local label or its section — but not
# about relocations: which ones, where, of what type, against what, and what
# the field holds. So for x86 each case is also compared against GNU as in
# canon.sh's `--relocs` form. Cases in a `*-llvm.txt` corpus are llvm-mc only,
# for syntax GNU as reads differently or not at all (`.def` inside a line,
# AArch64).
#
# Corpora, in tools/coff-diff/:
#   <target>.txt           one case per line
#   <target>-programs.txt  multi-line cases separated by `=== name` lines
#   <target>-llvm.txt      `=== name` cases compared against llvm-mc alone
#   <target>-padding.txt   the same, for alignment padding in code
#   <target>-compiler.txt  the same, for Clang's output (see compiler.sh)
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
oracles="${RSASM_ORACLES:-$root/target/oracles}"

# target | rsasm arch | llvm triple | GNU as
TARGETS="
x86-64|x86-64|x86_64-windows-msvc|x86_64-w64-mingw32-as
i386|i386|i686-windows-msvc|i686-w64-mingw32-as
aarch64|aarch64|aarch64-windows-msvc|
"

command -v llvm-mc >/dev/null || { echo "llvm-mc not found; skipping" >&2; exit 0; }
command -v llvm-readobj >/dev/null || { echo "llvm-readobj not found; skipping" >&2; exit 0; }
echo "oracle: $(llvm-mc --version | grep -m1 -oE 'LLVM version [0-9.]+')"
case "$(llvm-mc --version)" in
  *"LLVM version 22."*) ;;
  *) echo "warning: the corpora were verified against LLVM 22; expect version drift" >&2 ;;
esac
cargo build --quiet --manifest-path "$root/Cargo.toml" --all-features --bin rsasm || exit 1
rsasm="$root/target/debug/rsasm"

pass=0
fail=0

# Assembles `$d/in.s` with one reference into `$d/$2`, printing its canonical
# form, or why it could not.
reference() { # command..., canon flag, object
  local canon_flag=$1 obj=$2
  shift 2
  if "$@" -o "$d/$obj" "$d/in.s" 2> "$d/err"; then
    "$here/canon.sh" $canon_flag "$d/$obj"
  else
    echo "REF-ERROR: $(head -3 "$d/err" | tr '\n' ' ')"
  fi
}

compare() { # target, rsasm arch, triple, gas, gas-too, name, source
  local target=$1 rs=$2 triple=$3 gas=$4 gas_too=$5 name=$6 src=$7 m r
  d=$(mktemp -d)
  printf '%s\n' "$src" > "$d/in.s"
  m=$(reference "" m.obj llvm-mc -triple="$triple" -filetype=obj)
  if "$rsasm" -f coff -a "$rs" -o "$d/r.obj" "$d/in.s" 2> "$d/rerr"; then
    r=$("$here/canon.sh" "$d/r.obj")
  else
    r="RSASM-ERROR: $(head -3 "$d/rerr" | tr '\n' ' ')"
  fi
  if [ "$m" = "$r" ]; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    echo "### [$target] $name"
    printf '%s\n' "$src" | sed 's/^/    /'
    diff <(printf '%s\n' "$m") <(printf '%s\n' "$r") | sed 's/^/  /'
  fi

  if [ "$gas_too" = 1 ] && [ -n "$gas" ]; then
    if [ ! -x "$oracles/bin/$gas" ]; then
      echo "REF-MISSING: $oracles/bin/$gas (run tools/oracles/build.sh)"
      rm -rf "$d"
      return
    fi
    m=$(reference --relocs g.obj "$oracles/bin/$gas")
    if [ -f "$d/r.obj" ]; then
      r=$("$here/canon.sh" --relocs "$d/r.obj")
    fi
    if [ "$m" = "$r" ]; then
      pass=$((pass + 1))
    else
      fail=$((fail + 1))
      echo "### [$target] $name (relocations, against GNU as)"
      printf '%s\n' "$src" | sed 's/^/    /'
      diff <(printf '%s\n' "$m") <(printf '%s\n' "$r") | sed 's/^/  /'
    fi
  fi
  rm -rf "$d"
}

snippets() { # file, gas-too, target, rsasm arch, triple, gas
  local file=$1 gas_too=$2 snippet="" name="" line
  shift 2
  while IFS= read -r line; do
    case "$line" in
      "==="*)
        [ -n "$snippet" ] && compare "$1" "$2" "$3" "$4" "$gas_too" "$name" "$snippet"
        snippet=""; name="${line#=== }" ;;
      *) snippet="$snippet$line
" ;;
    esac
  done < "$file"
  [ -n "$snippet" ] && compare "$1" "$2" "$3" "$4" "$gas_too" "$name" "$snippet"
  return 0
}

run_target() { # target, rsasm arch, triple, gas
  local target=$1 before=$((pass + fail)) line
  if [ -f "$here/$target.txt" ]; then
    while IFS= read -r line; do
      [ -z "$line" ] && continue
      case "$line" in \#*) continue ;; esac
      compare "$1" "$2" "$3" "$4" 1 "$line" "$line"
    done < "$here/$target.txt"
  fi
  [ -f "$here/$target-programs.txt" ] && snippets "$here/$target-programs.txt" 1 "$@"
  [ -f "$here/$target-llvm.txt" ] && snippets "$here/$target-llvm.txt" 0 "$@"
  [ -f "$here/$target-padding.txt" ] && snippets "$here/$target-padding.txt" 0 "$@"
  [ -f "$here/$target-compiler.txt" ] && snippets "$here/$target-compiler.txt" 0 "$@"
  echo "[$target] $((pass + fail - before)) comparisons"
}

wanted="${*:-}"
while IFS='|' read -r target rs triple gas; do
  [ -z "$target" ] && continue
  if [ -n "$wanted" ]; then
    case " $wanted " in *" $target "*) ;; *) continue ;; esac
  fi
  run_target "$target" "$rs" "$triple" "$gas"
done <<< "$TARGETS"

echo "--- $pass matched, $fail differed"
[ "$fail" -eq 0 ]
