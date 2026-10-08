#!/usr/bin/env bash
# Usage: ./test.sh [--sysroot] [--mold]
# Builds the backend, then compiles and runs tests/{nostd,std,asm} with it.
# --sysroot additionally rebuilds core/alloc/std with the backend (-Zbuild-std).
# --mold additionally cross-links the std test to *-linux-musl with the pure-Rust
# `mold` linker (PATH or a sibling `mold` checkout) and runs it via docker/qemu.
# Links with the pure-Rust `wild` linker when it is on PATH (cargo install wild-linker).
set -euo pipefail
cd "$(dirname "$0")"
cargo build
case "$(uname -s)" in
  Darwin) so=dylib ;;
  *)      so=so ;;
esac
BE="-Zcodegen-backend=$PWD/target/debug/librustc_codegen_pliron.$so"
if command -v wild >/dev/null; then
  # gcc 11 has no --ld-path, so point its -B search dir at an `ld` that is wild.
  mkdir -p target/wild-ld && ln -sf "$(command -v wild)" target/wild-ld/ld
  BE="$BE -Clinker-features=-lld -Clink-self-contained=-linker -Zunstable-options -Clink-arg=-B$PWD/target/wild-ld"
fi
out=target/tests; mkdir -p $out
rustc $BE --edition 2024 -Cpanic=abort -Clink-arg=-lc tests/nostd/main.rs -o $out/nostd && $out/nostd
tests="std unwind"
# asm test has x86-64 and aarch64 variants; other targets unsupported.
case "$(uname -m)" in x86_64|aarch64|arm64) tests="$tests asm" ;; esac
for t in $tests; do rustc $BE --edition 2024 tests/$t/main.rs -o $out/$t && $out/$t; done
# proc macro built by us, loaded by stock rustc: exercises the C ABI (byval/sret) across the bridge
rustc $BE --edition 2021 --crate-type proc-macro tests/proc_macro/pm.rs -o $out/libpm.$so &&
    rustc --edition 2021 tests/proc_macro/main.rs --extern pm=$out/libpm.$so -o $out/pm_user && $out/pm_user
for a in "$@"; do
  if [[ "$a" == --sysroot ]]; then
    (cd tests/sysroot && export RUSTFLAGS="$BE" CARGO_TARGET_DIR=../../target/sysroot &&
      T="$(rustc -vV | sed -n 's/host: //p')" &&
      cargo build -Zbuild-std=std,panic_unwind --target "$T" --bins &&
      ../../target/sysroot/$T/debug/sysroot-test && ../../target/sysroot/$T/debug/unwind-test)
  elif [[ "$a" == --mold ]]; then
    # mold is ELF-only (Mach-O support lives in the commercial sold fork), so
    # the pure-Rust link path targets *-linux-musl: static, self-contained CRT.
    case "$(uname -m)" in
      arm64|aarch64) mtarget=aarch64-unknown-linux-musl ;;
      x86_64)        mtarget=x86_64-unknown-linux-musl ;;
      *) echo "mold test: unsupported host arch" >&2; exit 1 ;;
    esac
    MOLD="$(command -v mold || echo "$PWD/../mold/target/release/mold")"
    rustup target list --installed | grep -qx "$mtarget" ||
      rustup target add "$mtarget"
    rustc $BE --edition 2024 -O --target "$mtarget" \
      -Clinker="$MOLD" -Clinker-flavor=ld \
      tests/std/main.rs -o "$out/std_mold"
    if [[ "$(uname -s)" == Linux ]]; then "$out/std_mold";
    elif command -v docker >/dev/null; then
      docker run --rm -v "$PWD/$out:/t" -w /t alpine ./std_mold
    elif command -v "qemu-${mtarget%%-*}" >/dev/null; then
      "qemu-${mtarget%%-*}" "$out/std_mold"
    else
      echo "mold: linked $out/std_mold (no docker/qemu to run it)"
    fi
  fi
done
if command -v node >/dev/null; then tests/wasm/run.sh; fi
