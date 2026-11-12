#!/bin/bash
# Hot-patch smoke test: run a PLIRON_HOT base binary, recompile `value()` to read
# a static, drop the object into PLIRON_HOT_DIR and check the running process
# picks it up with the static's state intact.
# r[verify native.link.wild]
# r[verify native.hot.slot]
# r[verify native.hot.loader]
# r[verify native.hot.limits]
set -euo pipefail
B="$(cd "$(dirname "$0")/../.." && pwd)"
export RUSTUP_TOOLCHAIN=nightly-2026-10-22
case "$(uname)" in
Darwin) DYLIB=dylib; LINKER=(); SEDI=(-i '') ;;
*)      DYLIB=so;    LINKER=(-Clinker="$B/selfhost/cc-wild"); SEDI=(-i) ;;
esac
FL="-Zcodegen-backend=$B/target/debug/librustc_codegen_pliron.$DYLIB -Ccodegen-units=1"
W="$B/target/hot-test"; rm -rf "$W"; mkdir -p "$W/patches"
RUSTFLAGS="$FL ${LINKER[*]:-}" CARGO_TARGET_DIR="$W/agent" \
    cargo build -q --manifest-path "$B/examples/pliron-hot/Cargo.toml"
D="$W/agent/debug"
cp "$B/tests/hot/demo.rs" "$W/demo.rs"
LIB=$(find "$D/build/pliron_hot" -name 'libpliron_hot-*.rlib' | head -1)
LDEPS=(); for d in "$D"/build/*/*/out; do LDEPS+=(-L "dependency=$d"); done
RC=(rustc $FL --edition 2024 "${LINKER[@]}" --extern pliron_hot="$LIB" --extern pliron_hot="${LIB%.rlib}.rmeta" "${LDEPS[@]}" "$W/demo.rs")
PLIRON_HOT=demo "${RC[@]}" -o "$W/demo"
"${RC[@]}" --emit=obj -o "$W/patches/base.ref"
PLIRON_HOT_DIR="$W/patches" "$W/demo" > "$W/out.txt" 2> "$W/err.txt" & PID=$!
sleep 0.3
sed "${SEDI[@]}" 's/^    1$/    1000 + TICKS.load(Ordering::Relaxed)/' "$W/demo.rs"
t0=$(date +%s.%N)
"${RC[@]}" --emit=obj -o "$W/p1.o"
mv "$W/p1.o" "$W/patches/p1.o"
wait $PID
echo "patch build: $(echo "$(date +%s.%N) - $t0" | bc | cut -c1-5)s"
cat "$W/err.txt"
last=$(tail -1 "$W/out.txt"); echo "$last"
read -r _ t _ v <<< "$last"
[[ $t -gt 0 && $v -eq $((1000 + t + 1)) ]] && echo "hot patch OK"

# Layout guard (`PLIRON_HOT_LAYOUT=1`): a patch that changes a struct the
# live process holds values of must be refused, not applied -- the old
# `STATE` bytes would be read through the new field offsets.
cp "$B/tests/hot/layout.rs" "$W/layout.rs"
RL=("${RC[@]/demo.rs/layout.rs}")
export PLIRON_HOT_LAYOUT=1
rm -f "$W"/patches/*
PLIRON_HOT=layout "${RL[@]}" -o "$W/layout"
"${RL[@]}" --emit=obj -o "$W/patches/base.ref"
PLIRON_HOT_DIR="$W/patches" "$W/layout" > "$W/lout.txt" 2> "$W/lerr.txt" & PID=$!
sleep 0.3
# 1: a body-only edit still applies. 2: a field inserted before `n` does not.
sed "${SEDI[@]}" 's/^    STATE.lock().unwrap().n$/    STATE.lock().unwrap().n + 100/' "$W/layout.rs"
"${RL[@]}" --emit=obj -o "$W/q1.o"; mv "$W/q1.o" "$W/patches/q1.o"
sleep 0.3
sed "${SEDI[@]}" 's/^struct State { n: u64 }$/struct State { pad: [u64; 4], n: u64 }/; s/State { n: 7 }/State { pad: [9; 4], n: 7 }/' "$W/layout.rs"
"${RL[@]}" --emit=obj -o "$W/q2.o"; mv "$W/q2.o" "$W/patches/q2.o"
wait $PID || true
cat "$W/lerr.txt"
grep -q "q1.o: [0-9]* functions patched" "$W/lerr.txt"
grep -q 'q2.o: not applied: the layout of `State` (8 -> 40 bytes' "$W/lerr.txt"
# After q1 the value is 107 and stays 107: q2's code never ran.
[[ "$(tail -1 "$W/lout.txt")" == "value 107" ]] && ! grep -qv -e "value 7" -e "value 107" "$W/lout.txt" && echo "hot layout guard OK"
