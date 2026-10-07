#!/bin/bash
# Hot-patch smoke test: run a PLIRON_HOT base binary, recompile `value()` to read
# a static, drop the object into PLIRON_HOT_DIR and check the running process
# picks it up with the static's state intact.
set -euo pipefail
B="$(cd "$(dirname "$0")/../.." && pwd)"
export RUSTUP_TOOLCHAIN=nightly-2026-10-06
FL="-Zcodegen-backend=$B/target/debug/librustc_codegen_pliron.so -Ccodegen-units=1"
W="$B/target/hot-test"; rm -rf "$W"; mkdir -p "$W/patches"
RUSTFLAGS="$FL -Clinker=$B/selfhost/cc-wild" CARGO_TARGET_DIR="$W/agent" \
    cargo build -q --manifest-path "$B/examples/pliron-hot/Cargo.toml"
D="$W/agent/debug"
cp "$B/tests/hot/demo.rs" "$W/demo.rs"
LIB=$(find "$D/build/pliron_hot" -name 'libpliron_hot-*.rlib' | head -1)
LDEPS=(); for d in "$D"/build/*/*/out; do LDEPS+=(-L "dependency=$d"); done
RC=(rustc $FL --edition 2024 -Clinker="$B/selfhost/cc-wild" --extern pliron_hot="$LIB" --extern pliron_hot="${LIB%.rlib}.rmeta" "${LDEPS[@]}" "$W/demo.rs")
PLIRON_HOT=demo "${RC[@]}" -o "$W/demo"
"${RC[@]}" --emit=obj -o "$W/patches/base.ref"
PLIRON_HOT_DIR="$W/patches" "$W/demo" > "$W/out.txt" 2> "$W/err.txt" & PID=$!
sleep 0.3
sed -i 's/^    1$/    1000 + TICKS.load(Ordering::Relaxed)/' "$W/demo.rs"
t0=$(date +%s.%N)
"${RC[@]}" --emit=obj -o "$W/p1.o"
mv "$W/p1.o" "$W/patches/p1.o"
wait $PID
echo "patch build: $(echo "$(date +%s.%N) - $t0" | bc | cut -c1-5)s"
cat "$W/err.txt"
last=$(tail -1 "$W/out.txt"); echo "$last"
read -r _ t _ v <<< "$last"
[[ $t -gt 0 && $v -eq $((1000 + t + 1)) ]] && echo "hot patch OK"
