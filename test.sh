#!/usr/bin/env bash
# Usage: ./test.sh [--sysroot] [--mold]
# Builds the backend, then compiles and runs tests/{nostd,std,asm} with it.
# --sysroot additionally rebuilds core/alloc/std with the backend (-Zbuild-std).
# --mold additionally cross-links the std test to *-linux-musl with a pure-Rust
# linker — wild (PATH, cargo install wild-linker) or mold (PATH or a sibling
# `mold` checkout) — and runs it via docker/qemu.
# Links with the pure-Rust `wild` linker when it is on PATH (cargo install wild-linker).
set -euo pipefail
cd "$(dirname "$0")"
cargo build
case "$(uname -s)" in
  Darwin)              lib=librustc_codegen_pliron.dylib; exe= ;;
  MINGW*|MSYS*|CYGWIN*) lib=rustc_codegen_pliron.dll;     exe=.exe ;;
  *)                   lib=librustc_codegen_pliron.so;    exe= ;;
esac
BE="-Zcodegen-backend=$PWD/target/debug/$lib"
if [[ "$(uname -s)" == Linux ]] && command -v wild >/dev/null; then
  # gcc 11 has no --ld-path, so point its -B search dir at an `ld` that is wild.
  # ELF-only: wild must never be picked up for a native Mach-O link on macOS.
  mkdir -p target/wild-ld && ln -sf "$(command -v wild)" target/wild-ld/ld
  BE="$BE -Clinker-features=-lld -Clink-self-contained=-linker -Zunstable-options -Clink-arg=-B$PWD/target/wild-ld"
fi
out=target/tests; mkdir -p $out
# nostd links libc directly on unix; on Windows msvcrt ships with the toolchain.
lc=-lc; [[ -n "$exe" ]] && lc=
rustc $BE --edition 2024 -Cpanic=abort ${lc:+-Clink-arg=$lc} tests/nostd/main.rs -o $out/nostd$exe && $out/nostd$exe
tests="std unwind"; stockcfg=
# On Windows the unwind machinery only interoperates when std is also built by
# us: against the stock LLVM std, panics are real SEH throws our emulated-EH
# invoke path can't intercept. The unwind test still runs under --sysroot.
if [[ -n "$exe" ]]; then tests="std"; stockcfg="--cfg pliron_stock_seh"; fi
# asm test has x86-64 and aarch64 variants; other targets unsupported.
case "$(uname -m)" in x86_64|aarch64|arm64) tests="$tests asm" ;; esac
for t in $tests; do rustc $BE $stockcfg --edition 2024 tests/$t/main.rs -o $out/$t$exe && $out/$t$exe; done
# -O passes (full unroll of constant-trip loops): output must match stock LLVM rustc's
rustc $BE --edition 2021 -O tests/unroll/main.rs -o $out/unroll$exe && $out/unroll$exe | diff --strip-trailing-cr - tests/unroll/expected.out
# LICM must not speculate enum-payload derefs past the discriminant guard.
rustc $BE --edition 2021 -O tests/licm/main.rs -o $out/licm$exe && $out/licm$exe
# Post-alloc pair fusion must not pair a load with a load that uses its
# result as an address (pointer-chase + closure-env dependent loads).
rustc $BE --edition 2021 -O tests/pairfuse/main.rs -o $out/pairfuse$exe && $out/pairfuse$exe
# edgespec must not thread an edge past a dispatch block whose params it
# leaves stale — SplitWhitespace::next's cursor rebind was skipped and the
# iterator spun forever.
rustc $BE --edition 2021 -O tests/edgespec/main.rs -o $out/edgespec$exe && $out/edgespec$exe
# cparm must materialize carried-chain constants in each carried param's
# home block — one const in the join block leaves non-dominating uses on
# sibling arms (Vec::into_boxed_slice / try_call edges hit the verifier).
rustc $BE --edition 2021 -O -C codegen-units=1 tests/cparm/main.rs -o $out/cparm$exe && $out/cparm$exe
# celim's unpeeled retry must range the retry's own operands — a stale
# peeled base turned `depth != 0` into `depth-1 != 0` and broke
# BTreeMap's drop walk (build-script SIGSEGV compiling blake3).
rustc $BE --edition 2021 -O -C codegen-units=1 tests/celim/main.rs -o $out/celim$exe && $out/celim$exe
# punroll's peeled side-exit clones must not reach ambient uses of body
# values through un-cloned shared continuations (sequential loops with
# converging exits — Lint::parse_rust_version hit the verifier/EntryLivein).
rustc $BE --edition 2021 -O -C codegen-units=1 tests/punroll/main.rs -o $out/punroll$exe && $out/punroll$exe
# coldargs' i128 arg rebind must build the zero via iconcat — iconst has
# no i128 form (verifier unreachable; jiff overflow_add_duration hit it).
rustc $BE --edition 2021 -O -C codegen-units=1 tests/coldargs/main.rs -o $out/coldargs$exe && $out/coldargs$exe
# bcheck may only fold a bound check whose unsafe edge dies — a cold
# not-found exit is cold yet live; folding the scan loop's own bound test
# turned `s.contains('=')` into an out-of-bounds read (stage2 rustc
# parse_cfg emitted the wrong --cfg diagnostic).
rustc $BE --edition 2021 -O -C codegen-units=1 tests/bcheck/main.rs -o $out/bcheck$exe && $out/bcheck$exe
# slotmerge may only share slots with disjoint live ranges — escaped,
# overlapping, and edge-carried pointers must keep their own memory or
# live data is aliased (per-arm aggregate allocas are the frame-size win).
rustc $BE --edition 2021 -O -C codegen-units=1 tests/slotmerge/main.rs -o $out/slotmerge$exe && $out/slotmerge$exe
# frame-size regression: 20 match arms x 2 disjoint 136-B aggregates must
# color to ~2 regions (~272 B), not sum to ~5.4 KB — unmerged, this is the
# shape that made stage2 rustc_parse::parse_item_kind allocate ~18 KB and
# SIGBUS on deep parser recursion (tests/ui/expr/if/expr-stack-overflow).
if command -v objdump >/dev/null; then
  rustc $BE --crate-type rlib --edition 2021 -O -C codegen-units=1 tests/slotmerge/big.rs -o $out/big.rlib
  objdump -d $out/big.rlib > $out/big.dis
  frame=$(awk '/dispatch.*>:/{f=1} f&&/sub/{print; exit}' $out/big.dis |
      sed -n 's/.*sub[[:space:]]*sp, sp, #\(0x[0-9a-f]*\).*/\1/p; s/.*sub[ql]*[[:space:]]*\$\(0x[0-9a-f]*\),%rsp.*/\1/p')
  if [ -n "$frame" ] && [ "$((frame))" -ge 1024 ]; then
      echo "slotmerge: dispatch frame is $((frame))B (expected <1024)" >&2; exit 1
  fi
fi
# bittab must test bit 0 of the shifted mask — `sh != 0` accepts the whole
# [lo, max] interval, so sparse sets take the interval's holes (rustc's
# check_const_closure matched `kw::Fn` against {Move,Static,Use}).
rustc $BE --edition 2021 -O -C codegen-units=1 tests/bittab/main.rs -o $out/bittab$exe && $out/bittab$exe
# switch-on-sparse-discriminant dispatch (rustc's param_default_policy shape)
rustc $BE --edition 2024 -O tests/polsw/main.rs -o $out/polsw$exe && $out/polsw$exe
# tailrec must carry updated params AND the accumulator across the
# back-edge (LLVM TRE for `op(expr, f(args))`) — a re-executed identity or
# stale param miscomputes; non-commutative ops must be left alone.
rustc $BE --edition 2021 -O -C codegen-units=1 tests/tailrec/main.rs -o $out/tailrec$exe && $out/tailrec$exe
# sroa pin: a non-copyable call arg with a known pointee extent pins only
# that byte range into shared `pin_a` memory — the callee's writes and
# pointers retained past the call must land where the caller's promoted
# fields live, and multiple pinned args on one call retarget
# independently (two Pins on one op must not trip the self-copy guard).
rustc $BE --edition 2021 -O -C codegen-units=1 tests/sroa/main.rs -o $out/sroa$exe && $out/sroa$exe
# proc macro built by us, loaded by stock rustc: exercises the C ABI (byval/sret) across the bridge
so=${lib##*.}
rustc $BE --edition 2021 --crate-type proc-macro tests/proc_macro/pm.rs -o $out/libpm.$so &&
    rustc --edition 2021 tests/proc_macro/main.rs --extern pm=$out/libpm.$so -o $out/pm_user$exe && $out/pm_user$exe
for a in "$@"; do
  if [[ "$a" == --sysroot ]]; then
    (cd tests/sysroot && export RUSTFLAGS="$BE" CARGO_TARGET_DIR=../../target/sysroot &&
      T="$(rustc -vV | sed -n 's/host: //p')" &&
      cargo build -Zbuild-std=std,panic_unwind --target "$T" --bins &&
      ../../target/sysroot/$T/debug/sysroot-test$exe &&
      ../../target/sysroot/$T/debug/unwind-test$exe)
  elif [[ "$a" == --mold ]]; then
    # Both pure-Rust linkers are ELF-only (Mach-O lives in the commercial sold
    # fork), so this path targets *-linux-musl: static, self-contained CRT.
    # wild speaks the lld-compatible dialect; mold speaks GNU ld.
    case "$(uname -m)" in
      arm64|aarch64) mtarget=aarch64-unknown-linux-musl ;;
      x86_64)        mtarget=x86_64-unknown-linux-musl ;;
      *) echo "mold test: unsupported host arch" >&2; exit 1 ;;
    esac
    if command -v wild >/dev/null; then
      LD="$(command -v wild)"; flavor=ld.lld
    else
      LD="$(command -v mold || echo "$PWD/../mold/target/release/mold")"; flavor=ld
    fi
    rustup target list --installed | grep -qx "$mtarget" ||
      rustup target add "$mtarget"
    rustc $BE --edition 2024 -O --target "$mtarget" \
      -Clinker="$LD" -Clinker-flavor=$flavor \
      tests/std/main.rs -o "$out/std_rustld"
    if [[ "$(uname -s)" == Linux ]]; then "$out/std_rustld";
    elif command -v docker >/dev/null; then
      docker run --rm -v "$PWD/$out:/t" -w /t alpine ./std_rustld
    elif command -v "qemu-${mtarget%%-*}" >/dev/null; then
      "qemu-${mtarget%%-*}" "$out/std_rustld"
    else
      echo "rust-ld: linked $out/std_rustld (no docker/qemu to run it)"
    fi
  fi
done
if command -v node >/dev/null \
    && rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown; then
  tests/wasm/run.sh
fi
