#!/usr/bin/env bash
# Usage: ./test.sh [--sysroot] [--mold] [--only a,b,c]
# Builds the backend, then compiles and runs tests/{nostd,std,asm} with it.
# --sysroot additionally rebuilds core/alloc/std with the backend (-Zbuild-std).
# --mold additionally cross-links the std test to *-linux-musl with a pure-Rust
# linker — wild (PATH, cargo install wild-linker) or mold (PATH or a sibling
# `mold` checkout) — and runs it via docker/qemu.
# Links with the pure-Rust `wild` linker when it is on PATH (cargo install wild-linker).
# --only runs only the comma-listed test names:
#   nostd std unwind asm unroll licm loadpre pairfuse edgespec cparm celim
#   punroll icmpfold coldargs bcheck unswitch slotmerge bittab polsw tailrec
#   sroa inlinecm retattr facts loopalign commute xpack loopvec proc_macro lto wasm pgo
#   asmcfi debuginfo
#   wasm_smoke — wasm emit smoke, no sysroot build (~1 s)
#   (sysroot/mold still need their flags)
set -euo pipefail
cd "$(dirname "$0")"

only=; sysroot=; mold=
for a in "$@"; do
  case "$a" in
    --only=*) only="${a#--only=}" ;;
    --sysroot) sysroot=1 ;;
    --mold) mold=1 ;;
    *) echo "test.sh: unknown flag '$a' (want --sysroot|--mold|--only=a,b,c)" >&2; exit 2 ;;
  esac
done
want() { [[ -z "$only" ]] || [[ ",$only," == *",$1,"* ]]; }

# TEST_NOBUILD=1: the caller (harness/gate.py) already built the backend.
[[ -n "${TEST_NOBUILD:-}" ]] || cargo build
export RUSTC_ICE=${RUSTC_ICE:-$PWD/target/ices}
case "$(uname -s)" in
  Darwin)              lib=librustc_codegen_pliron.dylib; exe= ;;
  MINGW*|MSYS*|CYGWIN*) lib=rustc_codegen_pliron.dll;     exe=.exe ;;
  *)                   lib=librustc_codegen_pliron.so;    exe= ;;
esac
BE="-Zcodegen-backend=$PWD/target/debug/$lib"
# Extra rustc flags for every stanza (experiments, e.g. -Zmir-move-elimination).
BE="$BE ${PLIRON_TEST_RUSTFLAGS:-}"
if [[ "$(uname -s)" == Linux ]] && command -v wild >/dev/null; then
  # gcc 11 has no --ld-path, so point its -B search dir at an `ld` that is wild.
  # ELF-only: wild must never be picked up for a native Mach-O link on macOS.
  mkdir -p target/wild-ld && ln -sf "$(command -v wild)" target/wild-ld/ld
  BE="$BE -Clinker-features=-lld -Clink-self-contained=-linker -Zunstable-options -Clink-arg=-B$PWD/target/wild-ld"
fi
out=target/tests; mkdir -p $out
llvmbin="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/host: //p')/bin"
llvmod="$llvmbin/llvm-objdump$exe"
# llvm-ar/nm/objcopy behave identically on every platform; the system
# tools (BSD ar/nm on macOS, none on Windows) don't.
llvmar="$llvmbin/llvm-ar$exe"; llvmnm="$llvmbin/llvm-nm$exe"; llvmoc="$llvmbin/llvm-objcopy$exe"
so=${lib##*.}
# On Windows the unwind machinery only interoperates when std is also built by
# us: against the stock LLVM std, panics are real SEH throws our emulated-EH
# invoke path can't intercept. The unwind test still runs under --sysroot.
stockcfg=; [[ -n "$exe" ]] && stockcfg="--cfg pliron_stock_seh"

t_nostd() {
  # nostd links libc directly on unix; on Windows msvcrt ships with the toolchain.
  local lc=-lc; [[ -n "$exe" ]] && lc=
  rustc $BE --edition 2024 -Cpanic=abort ${lc:+-Clink-arg=$lc} tests/nostd/main.rs -o $out/nostd$exe && $out/nostd$exe
}

t_std() { rustc $BE $stockcfg --edition 2024 tests/std/main.rs -o $out/std$exe && $out/std$exe; }
t_unwind() { rustc $BE $stockcfg --edition 2024 tests/unwind/main.rs -o $out/unwind$exe && $out/unwind$exe; }
t_asm() { rustc $BE --edition 2024 tests/asm/main.rs -o $out/asm$exe && $out/asm$exe; }

t_unroll() {
  # -O passes (full unroll of constant-trip loops): output must match stock LLVM rustc's
  rustc $BE --edition 2021 -O tests/unroll/main.rs -o $out/unroll$exe && $out/unroll$exe | diff --strip-trailing-cr - tests/unroll/expected.out
}

t_licm() {
  # LICM must not speculate enum-payload derefs past the discriminant guard.
  rustc $BE --edition 2021 -O tests/licm/main.rs -o $out/licm$exe && $out/licm$exe || return 1
  # `scan`'s `Option<&Pre>` is dereferenceable_or_null: its loads must not
  # be `can_move` (frozen) — that let them run above the null check.
  if PLIRON_CLIF=4scan rustc $BE --edition 2021 -O tests/licm/main.rs -o $out/licm2$exe 2>&1 >/dev/null \
      | awk '/4scan final/,0' | grep -q "can_move"; then
    echo "nullable Option<&T> param treated as dereferenceable (can_move load)"; return 1
  fi
}

t_loadpre() {
  # loadpre's edge insertion must feed the post-store/post-call value across
  # join edges — a compensating load on a missing arm reads end-of-pred
  # memory, so an aliasing store or writing call there must not go stale.
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/loadpre/main.rs -o $out/loadpre$exe && $out/loadpre$exe
}

t_pairfuse() {
  # Post-alloc pair fusion must not pair a load with a load that uses its
  # result as an address (pointer-chase + closure-env dependent loads).
  rustc $BE --edition 2021 -O tests/pairfuse/main.rs -o $out/pairfuse$exe && $out/pairfuse$exe
}

t_zext128() {
  # The i128 add/sub rules fused with a uextend operand must zero-extend
  # the narrow value — a raw W register can carry shl/eor/mul garbage in
  # bits 16..32 (rustlantis seed 24 miscompiled -65535 as -5439487).
  rustc $BE --edition 2021 -O tests/zext128/main.rs -o $out/zext128$exe && $out/zext128$exe
}

t_edgespec() {
  # edgespec must not thread an edge past a dispatch block whose params it
  # leaves stale — SplitWhitespace::next's cursor rebind was skipped and the
  # iterator spun forever. It must also see through enum tag+payload merges:
  # parse() exit blocks re-pack a Result whose tag is per-edge constant, so
  # the join's tag test and the packed word itself must both fold away.
  PLIRON_STATS=1 rustc $BE --edition 2021 -O tests/edgespec/main.rs -o $out/edgespec$exe 2> $out/edgespec.stats && $out/edgespec$exe
  grep -Eq 'edgespec [1-9]' $out/edgespec.stats || { echo "edgespec did not fire" >&2; exit 1; }
}

t_cparm() {
  # cparm must materialize carried-chain constants in each carried param's
  # home block — one const in the join block leaves non-dominating uses on
  # sibling arms (Vec::into_boxed_slice / try_call edges hit the verifier).
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/cparm/main.rs -o $out/cparm$exe && $out/cparm$exe
}

t_celim() {
  # celim's unpeeled retry must range the retry's own operands — a stale
  # peeled base turned `depth != 0` into `depth-1 != 0` and broke
  # BTreeMap's drop walk (build-script SIGSEGV compiling blake3).
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/celim/main.rs -o $out/celim$exe && $out/celim$exe
}

t_punroll() {
  # punroll's peeled side-exit clones must not reach ambient uses of body
  # values through un-cloned shared continuations (sequential loops with
  # converging exits — Lint::parse_rust_version hit the verifier/EntryLivein).
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/punroll/main.rs -o $out/punroll$exe && $out/punroll$exe
}

t_icmpfold() {
  # icmp_eqfold's eq/ne add-shift is i64-only: shifting the i8 whitespace
  # switch in strwork miscompiled through an untraced downstream path.
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/icmpfold/main.rs -o $out/icmpfold$exe && $out/icmpfold$exe
}

t_rangefold() {
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/rangefold/main.rs -o $out/rangefold$exe && $out/rangefold$exe
}

t_coldargs() {
  # coldargs' i128 arg rebind must build the zero via iconcat — iconst has
  # no i128 form (verifier unreachable; jiff overflow_add_duration hit it).
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/coldargs/main.rs -o $out/coldargs$exe && $out/coldargs$exe
}

t_bcheck() {
  # bcheck may only fold a bound check whose unsafe edge dies — a cold
  # not-found exit is cold yet live; folding the scan loop's own bound test
  # turned `s.contains('=')` into an out-of-bounds read (stage2 rustc
  # parse_cfg emitted the wrong --cfg diagnostic).
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/bcheck/main.rs -o $out/bcheck$exe && $out/bcheck$exe
}

t_unswitch() {
  # unswitch must drop a whole dead-fragment region, not just no-pred
  # blocks: the folded side of HashMap::get's capacity check leaves a
  # self-referencing dead cycle whose defs feed live clones — removing
  # only pred-less blocks detached v-defs still in use (hmget verifier hit).
  PLIRON_STATS=1 rustc $BE --edition 2021 -O -C codegen-units=1 tests/unswitch/main.rs -o $out/unswitch$exe 2> $out/unswitch.stats && $out/unswitch$exe
  grep -Eq 'unswitch [1-9]' $out/unswitch.stats || { echo "unswitch did not fire" >&2; exit 1; }
}

t_slotmerge() {
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
}

t_bittab() {
  # bittab must test bit 0 of the shifted mask — `sh != 0` accepts the whole
  # [lo, max] interval, so sparse sets take the interval's holes (rustc's
  # check_const_closure matched `kw::Fn` against {Move,Static,Use}).
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/bittab/main.rs -o $out/bittab$exe && $out/bittab$exe
}

t_polsw() {
  # switch-on-sparse-discriminant dispatch (rustc's param_default_policy shape)
  rustc $BE --edition 2024 -O tests/polsw/main.rs -o $out/polsw$exe && $out/polsw$exe
}

t_tailrec() {
  # tailrec must carry updated params AND the accumulator across the
  # back-edge (LLVM TRE for `op(expr, f(args))`) — a re-executed identity or
  # stale param miscomputes; non-commutative ops must be left alone.
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/tailrec/main.rs -o $out/tailrec$exe && $out/tailrec$exe
}

t_sroa() {
  # sroa pin: a non-copyable call arg with a known pointee extent pins only
  # that byte range into shared `pin_a` memory — the callee's writes and
  # pointers retained past the call must land where the caller's promoted
  # fields live, and multiple pinned args on one call retarget
  # independently (two Pins on one op must not trip the self-copy guard).
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/sroa/main.rs -o $out/sroa$exe && $out/sroa$exe
}

t_inlinecm() {
  # inlinecm: the cost model must accept the constant-mode dispatch the fixed
  # thresholds reject (and reject call-dense bodies they'd take); recursive
  # and oversized callees stay calls under both. Outputs must match.
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/inlinecm/main.rs -o $out/inlinecm$exe &&
      $out/inlinecm$exe > $out/inlinecm.out
  PLIRON_INLINE_MODEL=0 rustc $BE --edition 2021 -O -C codegen-units=1 tests/inlinecm/main.rs -o $out/inlinecm_l$exe &&
      $out/inlinecm_l$exe | diff --strip-trailing-cr - $out/inlinecm.out
}

t_facts() {
  # Bottom-up summaries: the facts PLIRON_FACTS prints for tests/facts must
  # match the expectations in its comments, and the program must still run.
  PLIRON_NOUNWIND=1 PLIRON_FACTS=fx_ rustc $BE --edition 2021 -O -C codegen-units=1 tests/facts/main.rs -o $out/facts$exe 2> $out/facts.txt && $out/facts$exe
  sed -E 's/_RNvCs[A-Za-z0-9]+_4main[0-9]+//' $out/facts.txt > $out/facts.got
  local bad=0
  while read -r want; do
    grep -qxF "$want" $out/facts.got || { echo "facts: missing '$want'" >&2; bad=1; }
  done <<'EOF'
nocapture fx_stash: [1]
nocapture fx_launder: []
nocapture fx_poke: [0, 1]
nocapture fx_copy: [0, 1]
nocapture fx_sum: [0]
nocapture fx_second: [0]
nowrite fx_stash: None
nowrite fx_sum: Some(true)
nowrite fx_depth: Some(true)
nnret fx_second: true
nnret fx_maybe: false
retarg fx_second: Some(1)
nounwind fx_depth: true
willreturn fx_sum: true
willreturn fx_depth: true
willreturn fx_vread: false
EOF
  return $bad
}

t_invokeblk() {
  # Every invoke ends its block after each IR pass (domcheck's mid-invoke
  # check); the program must still run and unwind correctly.
  PLIRON_CHECK_DOM=1 PLIRON_TRACE=pliron::domcheck=error rustc $BE --edition 2021 -O -C codegen-units=1 tests/invokeblk/main.rs -o $out/invokeblk$exe 2> $out/invokeblk.log || { cat $out/invokeblk.log >&2; return 1; }
  if grep -q "domcheck after" $out/invokeblk.log; then grep "domcheck after" $out/invokeblk.log | head -3 >&2; return 1; fi
  # The domcheck above is the fix's coverage; the run needs catch_unwind,
  # which can't intercept stock-std SEH throws on Windows (see `unwind`).
  [[ -n "$exe" ]] && return 0
  $out/invokeblk$exe 2>/dev/null > $out/invokeblk.out
  printf 'drop 3\n12\ndrop 1000\ncaught true\n' | diff - $out/invokeblk.out
}

t_xcgurot() {
  # xcgu copies carry their callees' bodies: `usize::rotate_left` (not the
  # intrinsic) must be copied too, or the link fails (undefined symbol).
  rustc $BE --edition 2021 -Copt-level=1 -C codegen-units=16 tests/xcgurot/main.rs -o $out/xcgurot$exe && $out/xcgurot$exe
}

t_xcgufnptr() {
  # A non-Rust-ABI callback compared by address keeps one address under
  # xcgu (no internal duplicate in other CGUs).
  rustc $BE --edition 2021 -O -C codegen-units=16 tests/xcgufnptr/main.rs -o $out/xcgufnptr$exe && $out/xcgufnptr$exe
}

t_selmul() {
  # Late select-chain folds, the madd carry, the stepped exit compare and
  # same-loop remat (all opt-in): output must match stock LLVM rustc's.
  PLIRON_SELCHAIN=1 PLIRON_MULCARRY=1 PLIRON_STEPCMP=1 PLIRON_REMATLOOP=0 rustc $BE --edition 2021 -O tests/selmul/main.rs -o $out/selmul$exe && $out/selmul$exe | diff --strip-trailing-cr - tests/selmul/expected.out
}

t_dseunwind() {
  # Stores through `&mut self` before a call that unwinds stay: the caller
  # that catches the panic reads them (dse unwind exits).
  rustc $BE --edition 2021 -O tests/dseunwind/main.rs -o $out/dseunwind$exe && $out/dseunwind$exe
}

t_unwindmem() {
  # Caller memory a `catch_unwind` caller reads after a panic: no pass may
  # hold it in registers or drop stores across a call that may unwind.
  rustc $BE --edition 2021 -O tests/unwindmem/main.rs -o $out/unwindmem$exe && $out/unwindmem$exe | diff --strip-trailing-cr - tests/unwindmem/expected.out
}

t_unwindstd() {
  # std's unwind-safety guards (SetLenOnDrop, sort holes, RefCell flags,
  # drop order, ...) under a panic at each of 12 points vs stock rustc.
  rustc $BE --edition 2021 -O tests/unwindstd/main.rs -o $out/unwindstd$exe && $out/unwindstd$exe | diff --strip-trailing-cr - tests/unwindstd/expected.out
}

t_mirshapes() {
  # Hand-written MIR in the shapes rustc's MoveElimination produces
  # (interior sret, escaping return place, no storage markers) and odd
  # StorageLive/Dead placement: output must match stock rustc's
  # (docs/history/2026-10-22-mir-move-elimination.md).
  for f in mirshapes markers; do
    rustc $BE --edition 2021 -O tests/mirshapes/$f.rs -o $out/$f$exe && $out/$f$exe | diff --strip-trailing-cr - tests/mirshapes/$f.expected.out || return 1
  done
}

t_slpsplat() {
  # slp xpack: two loads of one address splat only when nothing writes it
  # between them (rustlantis seeds 538/683/730).
  rustc $BE --edition 2021 -O tests/slpsplat/main.rs -o $out/slpsplat$exe && $out/slpsplat$exe
}

t_applestackargs() {
  # apple_aarch64 packs narrow stack args (u16 at +8, i8 at +0xa): an
  # extended one is stored/loaded at its own width, not the word (-O0,
  # where internal calls use the Apple convention).
  rustc $BE --edition 2021 tests/applestackargs/main.rs -o $out/applestackargs$exe && $out/applestackargs$exe | diff --strip-trailing-cr - tests/applestackargs/expected.out
}

t_fnalign() {
  # #[rustc_align(N)] reaches the object: plain fns and dyn shims.
  rustc $BE --edition 2021 tests/fnalign/main.rs -o $out/fnalign$exe && $out/fnalign$exe && \
  rustc $BE --edition 2021 -O tests/fnalign/main.rs -o $out/fnalign$exe && $out/fnalign$exe
}

t_rusttailcc() {
  # extern "tail" with byval (OnStack) aggregates: StructArgument exists
  # only on SysV x86-64; arm64 and Windows pass them as plain pointers.
  rustc $BE $stockcfg --edition 2021 --cfg aarch64 tests/rusttailcc/main.rs -o $out/rusttailcc$exe && $out/rusttailcc$exe >/dev/null 2>&1 && \
  rustc $BE $stockcfg --edition 2021 --cfg aarch64 -O tests/rusttailcc/main.rs -o $out/rusttailcc$exe && $out/rusttailcc$exe >/dev/null 2>&1
}

t_asmlabels() {
  # GAS numeric local labels with a leading zero (`01b`); aarch64 only.
  [ "$(uname -m)" = arm64 ] || return 0
  rustc $BE --edition 2021 tests/asmlabels/main.rs -o $out/asmlabels$exe
}

t_cvariadic() {
  # C-variadic defs: va_list layout per target; VaList forwarded to libc.
  if [[ -z "$exe" ]]; then
    rustc $BE --edition 2024 tests/cvariadic/main.rs -o $out/cvariadic$exe && $out/cvariadic$exe && \
    rustc $BE --edition 2024 -O tests/cvariadic/main.rs -o $out/cvariadic$exe && $out/cvariadic$exe
    return
  fi
  # Windows: `vsnprintf` isn't in the MSVC import libs here — msvcrt.lib
  # weak-aliases it to `_vsnprintf`, which lives only in the WinSDK's
  # legacy_stdio_definitions.lib (absent on this box), and ucrt.lib has
  # no import entry at all. The DLL itself exports it, so link CRT-free
  # against mkwinlibs' synthesized implibs instead.
  local libs=target/winlibs
  if [[ ! -f $libs/msvcrt.lib ]]; then
    uv run --no-project tools/mkwinlibs/mkwinlibs.py $libs \
      || python tools/mkwinlibs/mkwinlibs.py $libs \
      || py -3 tools/mkwinlibs/mkwinlibs.py $libs
  fi
  PLIRON_CRTFREE=1 rustc $BE --edition 2024 -Clinker=rust-lld -Lnative=$libs tests/cvariadic/main.rs -o $out/cvariadic$exe && $out/cvariadic$exe && \
  PLIRON_CRTFREE=1 rustc $BE --edition 2024 -O -Clinker=rust-lld -Lnative=$libs tests/cvariadic/main.rs -o $out/cvariadic$exe && $out/cvariadic$exe
}

t_narrowlsr() {
  # aarch64 add-with-lsr fold on i8/i16 operands (fuzz 1939/3441).
  rustc $BE --edition 2021 tests/narrowlsr/main.rs -o $out/narrowlsr$exe && $out/narrowlsr$exe && \
  rustc $BE --edition 2021 -O tests/narrowlsr/main.rs -o $out/narrowlsr$exe && $out/narrowlsr$exe
}

t_asmcfi() {
  # global_asm with .cfi_*: Mach-O __eh_frame + raw `_name` symbols; aarch64.
  [ "$(uname -m)" = arm64 ] || return 0
  rustc $BE --edition 2021 tests/asmcfi/main.rs -o $out/asmcfi$exe && $out/asmcfi$exe
}

t_debuginfo() {
  # -g: line tables and inlined frames, checked through std's own panic
  # backtrace (it reads the DWARF) at O0 and O2; variables at O0 (lldb);
  # the DWARF verifies; and -g changes no optimized code (identical text
  # with and without it).
  local src=tests/debuginfo/main.rs o bt l
  [[ -n "$exe" ]] && return 0
  for o in 0 2; do
    rustc $BE --edition 2021 -g -Copt-level=$o -Ccodegen-units=1 $src -o $out/debuginfo$o$exe
    bt=$(RUST_BACKTRACE=1 $out/debuginfo$o$exe 2>&1 || true)
    for f in leaf mid top main; do
      l=$(grep -n "// L:$f\$" $src | cut -d: -f1)
      echo "$bt" | grep -A1 -E "main::$f\$" | grep -q "main.rs:$l:" ||
        { echo "debuginfo O$o: no frame main::$f at main.rs:$l" >&2; echo "$bt" >&2; exit 1; }
    done
  done
  # Variables at O0, through a line breakpoint, when lldb is around.
  if command -v lldb >/dev/null; then
    l=$(grep -n "// L:top\$" $src | cut -d: -f1)
    lldb --batch -o "breakpoint set --file main.rs --line $l" -o run -o "frame variable p v" \
      -- $out/debuginfo0$exe > $out/debuginfo.lldb 2>&1 || true
    grep -q "p = (a = 5, b = -7)" $out/debuginfo.lldb && grep -q "length = 3" $out/debuginfo.lldb ||
      { echo "debuginfo: lldb shows no p/v at main.rs:$l" >&2; cat $out/debuginfo.lldb >&2; exit 1; }
  fi
  rustc $BE --edition 2021 -g -O -Ccodegen-units=1 --emit obj $src -o $out/debuginfo-g.o
  rustc $BE --edition 2021 -O -Ccodegen-units=1 --emit obj $src -o $out/debuginfo-nog.o
  # rustup ships no llvm-dwarfdump; use one from PATH or Homebrew's LLVM.
  local dd; dd=$(command -v llvm-dwarfdump || echo /opt/homebrew/opt/llvm/bin/llvm-dwarfdump)
  if [[ -x "$dd" ]]; then
    "$dd" --verify $out/debuginfo-g.o > $out/debuginfo.verify ||
      { cat $out/debuginfo.verify >&2; exit 1; }
  fi
  local sec=.text; [[ "$(uname -s)" == Darwin ]] && sec=__TEXT,__text
  "$llvmoc" --dump-section "$sec=$out/debuginfo-g.text" $out/debuginfo-g.o /dev/null
  "$llvmoc" --dump-section "$sec=$out/debuginfo-nog.text" $out/debuginfo-nog.o /dev/null
  cmp -s $out/debuginfo-g.text $out/debuginfo-nog.text || { echo "debuginfo: -g changed the code" >&2; exit 1; }
}

t_retattr() {
  # nnret/retarg must both fire (stats) and never lie (asserts): a wrongly
  # inferred nonnull-return or returned-arg folds these null/equality checks
  # the wrong way.
  PLIRON_STATS=1 rustc $BE --edition 2021 -O -C codegen-units=1 tests/retattr/main.rs -o $out/retattr$exe 2> $out/retattr.stats && $out/retattr$exe
  grep -Eq 'nnret .*: [1-9][0-9]* of' $out/retattr.stats || { echo "nnret inferred nothing" >&2; exit 1; }
  grep -Eq 'retarg .*: [1-9][0-9]* returned-arg bodies' $out/retattr.stats || { echo "retarg inferred nothing" >&2; exit 1; }
  grep -Eq 'retarg .*: [0-9]+ returned-arg bodies, [1-9][0-9]* call results folded' $out/retattr.stats || { echo "retarg folded nothing" >&2; exit 1; }
}

t_loopalign() {
  # Loop headers are 16-byte aligned (vcode marks back-edge targets in
  # emission order and pads before their labels — LLVM's `.p2align 4` loop
  # tops). Every backward-jump target must sit on a 16-byte boundary;
  # PLIRON_ALIGN=0 disables.
  if [[ -x "$llvmod" && "$(uname -m)" == x86_64 ]]; then
    rustc $BE --emit obj --edition 2021 -O -C codegen-units=1 tests/loopalign/main.rs -o $out/loopalign.o &&
        "$llvmod" -d --no-show-raw-insn $out/loopalign.o | sed -n '/spin.*>:/,/^$/p' > $out/loopalign.dis
    back=0; bad=0
    while read -r line; do
      if [[ "$line" =~ ^[[:space:]]*([0-9a-f]+):.*j[a-z]+[[:space:]]+0x([0-9a-f]+) ]]; then
        addr=$((16#${BASH_REMATCH[1]})); tgt=$((16#${BASH_REMATCH[2]}))
        if (( tgt < addr )); then
          back=$((back+1))
          (( tgt % 16 != 0 )) && bad=$((bad+1))
        fi
      fi
    done < $out/loopalign.dis
    (( back > 0 )) || { echo "loopalign: no back-edge found" >&2; exit 1; }
    (( bad == 0 )) || { echo "loopalign: $bad unaligned back-edge target(s)" >&2; exit 1; }
  fi
}

t_commute() {
  # commute must order the fewer-use operand first on commutative binops so
  # two-address isel clobbers the dead operand (a loop-invariant splat in
  # `a * b[i]` must become `mulpd splat, load`, not `movdqa` + `mulpd`).
  PLIRON_STATS=1 rustc $BE --edition 2021 -O -C codegen-units=1 tests/commute/main.rs -o $out/commute$exe 2> $out/commute.stats && $out/commute$exe
  grep -Eq 'commute [1-9][0-9]* .*saxpy' $out/commute.stats || { echo "commute did not fire on saxpy" >&2; exit 1; }
}

t_xpack() {
  # xpack pairs isomorphic scalar insts in one block into two-lane vector ops;
  # the quad_roots sqrts must pack into sqrtpd (x64 SSE2).
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/xpack/main.rs -o $out/xpack$exe && $out/xpack$exe &&
    echo "xpack ok" || exit 1
  if [[ -x "$llvmod" && "$(uname -m)" == x86_64 ]]; then
    rustc $BE --emit obj --edition 2021 -O -C codegen-units=1 tests/xpack/main.rs -o $out/xpack.o &&
        "$llvmod" -d --no-show-raw-insn $out/xpack.o > $out/xpack.dis &&
        grep -q sqrtpd $out/xpack.dis || { echo "xpack: no sqrtpd emitted" >&2; exit 1; }
  fi
}

t_loopvec() {
  # an inlined matmul's bounds-checked inner loop must version (bcheck) and
  # vectorize (loopvec) even though sibling loops crowd the pass budget and
  # the checks unwind through try_call panic paths.
  rustc $BE --edition 2021 -O -C codegen-units=1 tests/loopvec/main.rs -o $out/loopvec$exe && $out/loopvec$exe
  if [[ -x "$llvmod" ]]; then
    rustc $BE --emit obj --edition 2021 -O -C codegen-units=1 tests/loopvec/main.rs -o $out/loopvec.o
    # File, not pipe: `grep -q` closing early would SIGPIPE llvm-objdump and
    # pipefail would turn a match into a failure.
    "$llvmod" -d --no-show-raw-insn $out/loopvec.o > $out/loopvec.dis
    if [[ "$(uname -m)" == x86_64 ]]; then
        grep -qE 'mulpd|addpd' $out/loopvec.dis || { echo "loopvec: no packed f64 emitted" >&2; exit 1; }
        grep -qE 'pcmpeqb' $out/loopvec.dis || { echo "loopvec: no byte-mask count emitted" >&2; exit 1; }
    else
        grep -qE 'fmla\.2d|fmul\.2d|fadd\.2d' $out/loopvec.dis || { echo "loopvec: no f64x2 emitted" >&2; exit 1; }
        grep -qE 'cmeq\.16b' $out/loopvec.dis || { echo "loopvec: no byte-mask count emitted" >&2; exit 1; }
    fi
  fi
}

t_pgo() {
  # gen -> run -> use -> verify (docs/pgo.md): an instrumented build dumps
  # a .prof at exit, the pliron-prof CLI reads it, and a PGO_USE build
  # matches every function back and runs identically.
  (cd tools/pliron-prof && cargo build --release) &&
  (cd tools/pliron-prof-rt && cargo build --release) &&
  # thin .a: only the runtime object (bundled core/compiler_builtins
  # members' __rustc shims collide with the link's own std)
  local rt=$PWD/$out/libpliron_prof_rt.a
  # staticlib lands as `libX.a` on unix-ish, `X.lib` on windows-msvc —
  # same ar archive either way (llvm-ar reads both).
  local rt_full=$PWD/tools/pliron-prof-rt/target/release/libpliron_prof_rt.a
  [ -f "$rt_full" ] ||
    rt_full=$PWD/tools/pliron-prof-rt/target/release/pliron_prof_rt.lib
  local cli=$PWD/tools/pliron-prof/target/release/pliron-prof
  local prof=$out/pgo.prof
  (mkdir -p $out/pga && cd $out/pga &&
    "$llvmar" x $rt_full &&
    for o in pliron_prof_rt-*.o; do
      for s in $("$llvmnm" -g --defined-only "$o" | awk '{print $3}' |
        grep -E 'rust_begin_unwind|rust_eh_personality'); do
        "$llvmoc" --redefine-sym "$s=__pliron_rt_shim_$s" "$o"
      done
    done &&
    "$llvmar" rcs $rt pliron_prof_rt-*.o) &&
  rm -f $prof
  PLIRON_PGO=1 rustc $BE --edition 2021 -O -C codegen-units=1 tests/pgo/main.rs \
    -o $out/pgo_i$exe -Clink-arg="$rt" &&
    PLIRON_PROFILE_FILE=$prof $out/pgo_i$exe > $out/pgo_i.out &&
    $cli dump $prof > $out/pgo.dump &&
    grep -q 'functions' $out/pgo.dump &&
    # pgo::load reads the merged PLPROF01 map, not the raw PLRAW01 dump —
    # without this merge the use-side is rejected ("bad profile") and the
    # '0 functions matched' check below passes vacuously.
    $cli merge $prof -o $out/pgo.merged &&
    PLIRON_PGO_USE=$out/pgo.merged rustc $BE --edition 2021 -O -C codegen-units=1 tests/pgo/main.rs \
      -o $out/pgo_u$exe 2> $out/pgo_use.txt &&
    grep -Eq ': [1-9][0-9]* functions matched' $out/pgo_use.txt &&
    $out/pgo_u$exe | diff - $out/pgo_i.out
  # wasm leg: the instrumented module carries no runtime; the host pulls
  # the same .prof out of linear memory via __pliron_prof_hdrs.
  if command -v node > /dev/null 2>&1 && command -v cargo > /dev/null 2>&1; then
    # Keyed on the backend: cargo doesn't track the dylib, so a reused
    # dir would keep core from an older backend.
    local wdir=$PWD/$out/wasm-pgo-$(cksum < target/debug/$lib | cut -d' ' -f1)
    mkdir -p "$wdir" && touch "$wdir"
    # Prune only keys idle for an hour: a concurrent run may own a younger one.
    find "$PWD/$out" -mindepth 1 -maxdepth 1 -type d -name 'wasm-pgo*' -mmin +60 \
      ! -path "$wdir" -exec rm -rf {} + 2>/dev/null || true
    # RUSTFLAGS must be fully windows-spelled on msys: cargo's POSIX-path
    # translation mangles flag values containing ':' when any flag's path
    # is /c/-spelled. Build both off one native root.
    local wroot=$PWD
    case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) wroot=$(cygpath -m "$wroot") ;; esac
    cargo build -q --release --manifest-path tools/pliron-wasm-ld/Cargo.toml &&
    (cd tests/wasm &&
      RUSTFLAGS="-Zcodegen-backend=$wroot/target/debug/$lib -Clinker=$wroot/tools/pliron-wasm-ld/target/release/pliron-wasm-ld$exe" \
        CARGO_TARGET_DIR=$wdir PLIRON_PGO=1 \
        cargo build -q --release --target wasm32-unknown-unknown \
        -Zbuild-std=core,compiler_builtins \
        -Zbuild-std-features=compiler-builtins-mem) &&
    node tests/wasm/run.mjs \
      $wdir/wasm32-unknown-unknown/release/wasmtest.wasm &&
    node tests/wasm/pgo_extract.mjs \
      $wdir/wasm32-unknown-unknown/release/wasmtest.wasm $out/pgo-wasm.prof &&
    $cli dump $out/pgo-wasm.prof > $out/pgo-wasm.dump &&
    grep -q 'functions' $out/pgo-wasm.dump
  fi
}

t_proc_macro() {
  # proc macro built by us, loaded by stock rustc: exercises the C ABI (byval/sret) across the bridge
  rustc $BE --edition 2021 --crate-type proc-macro tests/proc_macro/pm.rs -o $out/libpm.$so &&
      rustc --edition 2021 tests/proc_macro/main.rs --extern pm=$out/libpm.$so -o $out/pm_user$exe && $out/pm_user$exe
}

t_lto() {
  # Fat LTO (-Clto): rustc leaves upstream Rust objects off the link line and
  # expects the backend's single LTO module to carry the whole crate graph
  # (src/lto.rs). main -> mid -> base: cross-crate generics, a #[used]
  # constructor nothing references, TLS, unwinding both ways across the
  # boundary, std. Built at O0 and -O, without and with -Clto, plus a -Clto
  # staticlib driven from C.
  local d=$out/lto o lto; rm -rf $d; mkdir -p $d
  local r="rustc $BE --edition 2024"
  local ext="--extern base=$d/libbase.rlib --extern mid=$d/libmid.rlib -L $d"
  for o in -Copt-level=0 -O; do
    $r $o --crate-type rlib --crate-name base tests/lto/base.rs -o $d/libbase.rlib &&
      $r $o --crate-type rlib --crate-name mid tests/lto/mid.rs --extern base=$d/libbase.rlib \
        -o $d/libmid.rlib || return 1
    for lto in -Clto=no -Clto; do
      $r $o $lto tests/lto/main.rs $ext -o $d/main$exe &&
        $d/main$exe | diff --strip-trailing-cr - tests/lto/expected.out ||
        { echo "lto: main $o $lto failed" >&2; return 1; }
    done
  done
  # rlib objects carry the LTO payload section; linked images never do.
  $llvmod -h $d/libbase.rlib | grep -q pliron_lto ||
    { echo "lto: no payload section in libbase.rlib" >&2; return 1; }
  if $llvmod -h $d/main$exe | grep -q pliron_lto; then
    echo "lto: payload section leaked into the executable" >&2; return 1
  fi
  printf '%s\n' '#[unsafe(no_mangle)]' 'pub extern "C" fn lto_entry(x: u32) -> u32 {' \
    '    let r = std::panic::catch_unwind(|| base::checked(x)).unwrap_or(1);' \
    '    r + mid::sums().0 as u32 + base::total(&[x as u16]) as u32' '}' > $d/slib.rs
  printf '%s\n' '#include <stdio.h>' 'unsigned lto_entry(unsigned);' \
    'int main(void) { printf("%u %u\n", lto_entry(2), lto_entry(9)); return 0; }' > $d/cmain.c
  $r -O -Clto --crate-type staticlib $d/slib.rs $ext -o $d/libslib.a &&
    cc $d/cmain.c $d/libslib.a -lpthread -ldl -lm -o $d/cmain &&
    [[ "$($d/cmain 2>/dev/null | tail -1)" == "278 266" ]] || { echo "lto: staticlib leg failed" >&2; return 1; }
}

t_wasm_smoke() {
  # Emit pipeline only: leaf crates -> pliron wasm objects -> pliron-wasm-ld
  # -> node. No -Zbuild-std, so ~seconds; the std-through-pliron legs are
  # `tests/wasm/run.sh` (the `wasm` stanza).
  command -v node >/dev/null \
    && rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown \
    || return 0
  tests/wasm/smoke.sh
}

t_sysroot() {
  (cd tests/sysroot && export RUSTFLAGS="$BE" CARGO_TARGET_DIR=../../target/sysroot &&
    T="$(rustc -vV | sed -n 's/host: //p')" &&
    cargo build -Zbuild-std=std,panic_unwind --target "$T" --bins &&
    ../../target/sysroot/$T/debug/sysroot-test$exe &&
    ../../target/sysroot/$T/debug/unwind-test$exe)
}

t_mold() {
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
}

want nostd && t_nostd
want std && t_std
# unwind can't intercept stock-std SEH throws on Windows; --sysroot covers it.
[[ -z "$exe" ]] && want unwind && t_unwind
# asm test has x86-64 and aarch64 variants; other targets unsupported.
case "$(uname -m)" in x86_64|aarch64|arm64) want asm && t_asm ;; esac
want unroll && t_unroll
want licm && t_licm
want loadpre && t_loadpre
want pairfuse && t_pairfuse
want zext128 && t_zext128
want edgespec && t_edgespec
want cparm && t_cparm
want celim && t_celim
want punroll && t_punroll
want icmpfold && t_icmpfold
want rangefold && t_rangefold
want coldargs && t_coldargs
want bcheck && t_bcheck
want unswitch && t_unswitch
want slotmerge && t_slotmerge
want bittab && t_bittab
want polsw && t_polsw
want tailrec && t_tailrec
want sroa && t_sroa
want inlinecm && t_inlinecm
want retattr && t_retattr
want invokeblk && t_invokeblk
want xcgurot && t_xcgurot
want xcgufnptr && t_xcgufnptr
want selmul && t_selmul
# Same stock-std SEH limitation as `unwind` above: every check here is a
# caught panic, which our emulated-EH catch can't see on Windows.
[[ -z "$exe" ]] && want dseunwind && t_dseunwind
[[ -z "$exe" ]] && want unwindmem && t_unwindmem
[[ -z "$exe" ]] && want unwindstd && t_unwindstd
want mirshapes && t_mirshapes
want slpsplat && t_slpsplat
want applestackargs && t_applestackargs
want fnalign && t_fnalign
# rusttailcc: byval args pass as plain pointers on Windows (x64
# StructArgument's copy-out calls memcpy with SysV-ordered regs); the
# panic-catch half is cfg'd out under pliron_stock_seh.
want rusttailcc && t_rusttailcc
want asmlabels && t_asmlabels
# cvariadic: on Windows the test links CRT-free against mkwinlibs
# implibs (this box's MSVC libs can't resolve vsnprintf — see
# t_cvariadic) rather than being skipped.
want cvariadic && t_cvariadic
want narrowlsr && t_narrowlsr
want asmcfi && t_asmcfi
want debuginfo && t_debuginfo
want facts && t_facts
want loopalign && t_loopalign
want commute && t_commute
want xpack && t_xpack
want loopvec && t_loopvec
want proc_macro && t_proc_macro
# Windows: link.exe has no relocatable link, so -Clto is a clean error there.
[[ -z "$exe" ]] && want lto && t_lto
want pgo && t_pgo
[[ -n "$sysroot" ]] && want sysroot && t_sysroot
[[ -n "$mold" ]] && want mold && t_mold
want wasm_smoke && t_wasm_smoke
if want wasm && command -v node >/dev/null \
    && rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown; then
  tests/wasm/run.sh
fi
