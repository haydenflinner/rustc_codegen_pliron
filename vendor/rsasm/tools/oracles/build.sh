#!/bin/bash
# Build the reference assemblers the differential tests compare against, for
# targets that neither the host's GNU as nor llvm-mc can assemble.
#
#   tools/oracles/build.sh            # everything
#   tools/oracles/build.sh m68k-elf   # just one
#
# Installs into target/oracles/ (gitignored). Nothing outside the repo is
# touched, and nothing needs root.
#
# Versions are pinned on purpose. An assembler's output is part of what the
# corpora record, and it changes between releases: llvm-mc 18 and 22 disagree
# on nine of our x86/RISC-V/SPARC cases, and none of them were rsasm bugs.
# Bump a version only after reading every difference the new one produces.
set -euo pipefail

BINUTILS_VERSION=2.47
BINUTILS_URL="https://ftp.gnu.org/gnu/binutils/binutils-$BINUTILS_VERSION.tar.xz"
BINUTILS_SHA256=154ab23b60070e8f27013c22977f1129425d67d1e8acd6e13010e617811e4cff

# vasm publishes no versioned download — this URL always serves the latest
# release — and has no working HTTPS. The checksum is what pins it: if the
# tarball ever changes, the build stops rather than quietly assembling the
# corpora against a different reference. This is vasm 2.0f, m68k backend 2.8c.
VASM_URL="http://sun.hasenbraten.de/vasm/release/vasm.tar.gz"
VASM_SHA256=c84b2de1cbb87831795fe64a85c5d9a7002a766e3a7c30b0a2d7d5e99d878f49

# cc65's ca65, the most widely used 6502 assembler today, with ld65 to lay the
# object out as a flat binary. 2.19 is the latest tagged release.
CC65_VERSION=2.19
CC65_URL="https://github.com/cc65/cc65/archive/refs/tags/V$CC65_VERSION.tar.gz"
CC65_SHA256=157b8051aed7f534e5093471e734e7a95e509c577324099c3c81324ed9d0de77

# The Macro Assembler AS (asl), for the 8080 in Intel's own syntax, which
# neither GNU as nor vasm (whose `RST` takes a Zilog address) follows. Builds
# are numbered and each archive is kept, so this URL is stable; plain HTTP
# only, like vasm, so the checksum is the pin.
ASL_BUILD=142-bld311
ASL_URL="http://john.ccac.rwth-aachen.de:8000/ftp/as/source/c_version/asl-current-$ASL_BUILD.tar.gz"
ASL_SHA256=b3213b8f6b9dace8eec06e1bdffdfa5a937fa1a6e588edf0205918220e67d6f8

# SDCC, for sdas8051 and sdld: the asxxxx-lineage assembler and linker that
# SDCC ships, and the second reference for the 8051. Only the `sdas` directory
# is used, and it needs no configure — the handful of macros configure would
# have written are passed on the command line — so nothing of the compiler
# itself is built. 4.4.0 is a release tarball with a stable URL.
SDCC_VERSION=4.4.0
SDCC_URL="https://downloads.sourceforge.net/project/sdcc/sdcc/$SDCC_VERSION/sdcc-src-$SDCC_VERSION.tar.bz2"
SDCC_SHA256=ae8c12165eb17680dff44b328d8879996306b7241efa3a83b2e3b2d2f7906a75

# NASM, the reference for the `nasm` dialect. 2.16.03 is the last 2.16.x
# release, the version most NASM source in circulation was written against.
# The SHA-256 was taken from the tarball on nasm.us, and its SHA-512 checked
# against the one Gentoo's dev-lang/nasm Manifest records.
NASM_VERSION=2.16.03
NASM_URL="https://www.nasm.us/pub/nasm/releasebuilds/$NASM_VERSION/nasm-$NASM_VERSION.tar.xz"
NASM_SHA256=1412a1c760bbd05db026b6c0d1657affd6631cd0a63cddb6f73cc6d4aa616148

# GNU as and ld, one build per target: gas is single-target by construction.
# The linkers are what the link tests use to check that relocations mean what
# rsasm intends, which comparing bytes against another assembler cannot show.
#   m68k-elf              Motorola 68000 family
#   v850-elf              V850 and RH850 (v850e3v5)
#   rl78-elf              Renesas RL78
#   rx-elf                Renesas RX
#   sh-elf                SuperH
#   avr-elf               Microchip AVR
#   msp430-elf            TI MSP430
#   z80-elf               Zilog Z80
# Linkers (and a second assembler) for the targets llvm-mc checks:
#   arm-none-eabi         ARM and Thumb
#   aarch64-elf           AArch64
#   riscv64-elf           RISC-V, 32- and 64-bit
#   powerpc64-linux-gnu   PowerPC, 32/64-bit, both byte orders
#   mips64-elf            MIPS, 32/64-bit, both byte orders
#   sparc64-elf           SPARC V8 and V9
#   x86_64-elf            x86-64, i386
# And the PE/COFF pair, GNU as's second opinion on the objects `-f win64` and
# `-f win32` write; llvm-mc is the first (see tools/coff-diff):
#   x86_64-w64-mingw32    x86-64 COFF
#   i686-w64-mingw32      i386 COFF
BINUTILS_TARGETS="m68k-elf v850-elf rl78-elf rx-elf sh-elf avr-elf msp430-elf z80-elf arm-none-eabi aarch64-elf riscv64-elf powerpc64-linux-gnu mips64-elf sparc64-elf x86_64-elf x86_64-w64-mingw32 i686-w64-mingw32"

root=$(cd "$(dirname "$0")/../.." && pwd)
# RSASM_ORACLES overrides the install directory, so several checkouts or a CI
# cache can share a single build.
out="${RSASM_ORACLES:-$root/target/oracles}"
src="$out/src"
mkdir -p "$src" "$out/bin"
jobs=$(nproc 2>/dev/null || echo 4)

wanted="${*:-$BINUTILS_TARGETS vasm vasm-6502 vasm-z80 cc65 asl sdas nasm}"

fetch() { # url dest sha256
  [ -s "$2" ] || { echo "fetching $1"; curl -fsSL --retry 3 -o "$2.part" "$1" && mv "$2.part" "$2"; }
  local got
  got=$(sha256sum "$2" | cut -d' ' -f1)
  if [ "$got" != "$3" ]; then
    echo "checksum mismatch for $2" >&2
    echo "  expected $3" >&2
    echo "  got      $got" >&2
    echo "The reference changed upstream. Rebuild it deliberately, re-run the" >&2
    echo "corpora, and read every difference before updating the checksum." >&2
    rm -f "$2"
    exit 1
  fi
}

build_binutils() { # target
  local t=$1
  if [ -x "$out/bin/$t-as" ] && [ -x "$out/bin/$t-ld" ]; then echo "$t already built"; return; fi
  fetch "$BINUTILS_URL" "$src/binutils-$BINUTILS_VERSION.tar.xz" "$BINUTILS_SHA256"
  [ -d "$src/binutils-$BINUTILS_VERSION" ] || tar -xJf "$src/binutils-$BINUTILS_VERSION.tar.xz" -C "$src"
  local b="$out/build/binutils-$t"
  rm -rf "$b" && mkdir -p "$b"
  echo "building binutils $BINUTILS_VERSION for $t"
  (
    cd "$b"
    "$src/binutils-$BINUTILS_VERSION/configure" \
      --target="$t" --prefix="$out" \
      --disable-nls --disable-werror --disable-gdb --disable-gdbserver \
      --disable-sim --disable-readline --disable-libdecnumber \
      --disable-gprof --disable-gprofng --enable-ld --disable-gold \
      --without-zstd --without-debuginfod > configure.log 2>&1
    make -j"$jobs" all-gas all-binutils all-ld > make.log 2>&1
    make install-gas install-binutils install-ld > install.log 2>&1
  ) || { echo "build of $t failed; see $b/*.log" >&2; return 1; }
  echo "built $t-as and $t-ld"
}

build_vasm() { # cpu syntax
  local exe="vasm$1_$2"
  if [ -x "$out/bin/$exe" ]; then echo "$exe already built"; return; fi
  fetch "$VASM_URL" "$src/vasm.tar.gz" "$VASM_SHA256"
  # One tree per backend: vasm builds a single CPU and syntax per binary, and
  # its objects do not say which, so builds sharing a tree would mix them.
  local b="$out/build/vasm-$1-$2"
  rm -rf "$b" && mkdir -p "$b" && tar -xzf "$src/vasm.tar.gz" -C "$b"
  echo "building vasm ($1, $2 syntax)"
  (cd "$b/vasm" && make -j"$jobs" CPU="$1" SYNTAX="$2" > make.log 2>&1) \
    || { echo "vasm build failed; see $b/vasm/make.log" >&2; return 1; }
  cp "$b/vasm/$exe" "$out/bin/"
  echo "built $exe"
}

build_cc65() {
  if [ -x "$out/bin/ca65" ] && [ -x "$out/bin/ld65" ]; then echo "ca65 already built"; return; fi
  fetch "$CC65_URL" "$src/cc65-$CC65_VERSION.tar.gz" "$CC65_SHA256"
  local b="$out/build/cc65"
  rm -rf "$b" && mkdir -p "$b" && tar -xzf "$src/cc65-$CC65_VERSION.tar.gz" -C "$b"
  echo "building cc65 $CC65_VERSION (ca65, ld65)"
  (cd "$b/cc65-$CC65_VERSION" && make -j"$jobs" -C src ca65 ld65 > make.log 2>&1) \
    || { echo "cc65 build failed; see $b/cc65-$CC65_VERSION/make.log" >&2; return 1; }
  cp "$b/cc65-$CC65_VERSION/bin/ca65" "$b/cc65-$CC65_VERSION/bin/ld65" "$out/bin/"
  echo "built ca65 and ld65"
}

build_asl() {
  if [ -x "$out/bin/asl" ] && [ -x "$out/bin/p2bin" ] && [ -x "$out/bin/p2hex" ] &&
    [ -f "$out/share/asl/stddef51.inc" ]; then echo "asl already built"; return; fi
  fetch "$ASL_URL" "$src/asl-$ASL_BUILD.tar.gz" "$ASL_SHA256"
  local b="$out/build/asl"
  rm -rf "$b" && mkdir -p "$b" && tar -xzf "$src/asl-$ASL_BUILD.tar.gz" -C "$b"
  echo "building asl $ASL_BUILD (asl, p2bin, p2hex)"
  # AS has no configure script; its Makefile.def names the compiler.
  sed 's/^CFLAGS = .*/CFLAGS = -O2/' "$b/asl-current/Makefile.def.tmpl" > "$b/asl-current/Makefile.def"
  (cd "$b/asl-current" && make -j"$jobs" asl p2bin p2hex > make.log 2>&1) \
    || { echo "asl build failed; see $b/asl-current/make.log" >&2; return 1; }
  cp "$b/asl-current/asl" "$b/asl-current/p2bin" "$b/asl-current/p2hex" "$out/bin/"
  # AS predefines no register names: the MCS-51 SFR and bit definitions live
  # in include/stddef51.inc, which the 8051 corpora include.
  mkdir -p "$out/share/asl"
  cp -r "$b/asl-current/include/." "$out/share/asl/"
  echo "built asl, p2bin and p2hex"
}

build_sdas() {
  if [ -x "$out/bin/sdas8051" ] && [ -x "$out/bin/sdld" ]; then echo "sdas8051 already built"; return; fi
  fetch "$SDCC_URL" "$src/sdcc-src-$SDCC_VERSION.tar.bz2" "$SDCC_SHA256"
  local b="$out/build/sdas"
  rm -rf "$b" && mkdir -p "$b" && tar -xjf "$src/sdcc-src-$SDCC_VERSION.tar.bz2" -C "$b"
  local t="$b/sdcc-$SDCC_VERSION"
  echo "building sdcc $SDCC_VERSION (sdas8051, sdld)"
  # sdas includes ../../sdccconf.h, which configure would write; the only
  # thing it takes from it is the path separator. The linker's
  # asxxxx_config.h.in is a template of `#undef`s that only Windows fills in,
  # so the *nix widths go in directly.
  printf "#define DIR_SEPARATOR_CHAR '/'\n" > "$t/sdccconf.h"
  sed -e 's/^#undef TYPE_BYTE/#define TYPE_BYTE char/' \
      -e 's/^#undef TYPE_WORD/#define TYPE_WORD short/' \
      -e 's/^#undef TYPE_DWORD/#define TYPE_DWORD int/' \
      -e 's/^#undef TYPE_UBYTE/#define TYPE_UBYTE unsigned char/' \
      -e 's/^#undef TYPE_UWORD/#define TYPE_UWORD unsigned short/' \
      -e 's/^#undef TYPE_UDWORD/#define TYPE_UDWORD unsigned int/' \
      "$t/sdas/linksrc/asxxxx_config.h.in" > "$t/sdas/linksrc/asxxxx_config.h"
  # `-std=gnu17`: the sources declare `elf()` unprototyped and define it with
  # an argument, which C23 makes an error.
  local defs="-O2 -w -std=gnu17 -DVERSIONHI=4 -DVERSIONLO=4 -DVERSIONP=0"
  (
    cd "$t"
    # shellcheck disable=SC2086
    ${CC:-cc} $defs -DVERSION=\"$SDCC_VERSION\" -DSDCDB -DNOICE -DINDEXLIB \
      -Isdas/as8051 -Isdas/asxxsrc -Isupport/util -o "$out/bin/sdas8051" \
      sdas/as8051/i51pst.c sdas/as8051/i51mch.c sdas/as8051/i51adr.c \
      sdas/asxxsrc/asdbg.c sdas/asxxsrc/asdata.c sdas/asxxsrc/asexpr.c \
      sdas/asxxsrc/aslex.c sdas/asxxsrc/aslist.c sdas/asxxsrc/asmain.c \
      sdas/asxxsrc/asout.c sdas/asxxsrc/assubr.c sdas/asxxsrc/assym.c \
      sdas/asxxsrc/asmcro.c sdas/asxxsrc/sdas.c sdas/asxxsrc/strcmpi.c \
      support/util/dbuf.c support/util/dbuf_string.c -lm > as.log 2>&1
    # shellcheck disable=SC2086
    ${CC:-cc} $defs -DVERSION=\"$SDCC_VERSION\" -DINDEXLIB \
      -Isdas/linksrc -Isdas/asxxsrc -Isupport/util -o "$out/bin/sdld" \
      sdas/linksrc/lk_readnl.c sdas/linksrc/lkaomf51.c sdas/linksrc/lkar.c \
      sdas/linksrc/lkarea.c sdas/linksrc/lkdata.c sdas/linksrc/lkelf.c \
      sdas/linksrc/lkeval.c sdas/linksrc/lkhead.c sdas/linksrc/lklex.c \
      sdas/linksrc/lklib.c sdas/linksrc/lklibr.c sdas/linksrc/lklist.c \
      sdas/linksrc/lkmain.c sdas/linksrc/lkmem.c sdas/linksrc/lknoice.c \
      sdas/linksrc/lkout.c sdas/linksrc/lkrel.c sdas/linksrc/lkrloc.c \
      sdas/linksrc/lkrloc3.c sdas/linksrc/lksdcclib.c sdas/linksrc/lksym.c \
      sdas/linksrc/sdld.c sdas/linksrc/lksdcdb.c sdas/linksrc/lkbank.c \
      sdas/asxxsrc/strcmpi.c -lm > ld.log 2>&1
  ) || { echo "sdas build failed; see $t/as.log and $t/ld.log" >&2; return 1; }
  echo "built sdas8051 and sdld"
}

build_nasm() {
  if [ -x "$out/bin/nasm" ]; then echo "nasm already built"; return; fi
  fetch "$NASM_URL" "$src/nasm-$NASM_VERSION.tar.xz" "$NASM_SHA256"
  rm -rf "$src/nasm-$NASM_VERSION" && tar -xJf "$src/nasm-$NASM_VERSION.tar.xz" -C "$src"
  echo "building nasm $NASM_VERSION"
  (
    cd "$src/nasm-$NASM_VERSION"
    ./configure > configure.log 2>&1
    make -j"$jobs" nasm > make.log 2>&1
  ) || { echo "nasm build failed; see $src/nasm-$NASM_VERSION/*.log" >&2; return 1; }
  cp "$src/nasm-$NASM_VERSION/nasm" "$out/bin/"
  echo "built nasm"
}

for w in $wanted; do
  case "$w" in
    vasm) build_vasm m68k mot ;;
    vasm-6502) build_vasm 6502 oldstyle ;;
    vasm-z80) build_vasm z80 oldstyle ;;
    cc65) build_cc65 ;;
    asl) build_asl ;;
    sdas) build_sdas ;;
    nasm) build_nasm ;;
    *) build_binutils "$w" ;;
  esac
done

echo "oracles in $out/bin:"
ls "$out/bin"
