#!/usr/bin/env bash
# Stage rustc.wasm, pliron-wasm-ld.wasm and the wasm32-unknown-unknown sysroot for the browser page.
# Needs: the wasm32-wasip1-hosted stage2 from selfhost (RUST=~/work/rust-sh) and `./test.sh` run once.
set -euo pipefail
cd "$(dirname "$0")"
ROOT=$(cd ../.. && pwd)
RUST=${RUST:-$HOME/work/rust-sh}
S=$RUST/build/wasm32-wasip1/stage2
LIB=$RUST/build/x86_64-unknown-linux-gnu/stage2/lib/rustlib/wasm32-unknown-unknown/lib
WASI_LIBC=$ROOT/target/wasm/wasi-libc/wasm32-wasip1/release
(cd $ROOT/tools/pliron-wasm-ld && \
  RUSTFLAGS="-Zcodegen-backend=$ROOT/target/debug/librustc_codegen_pliron.so -Clinker=$ROOT/tools/pliron-wasm-ld/target/release/pliron-wasm-ld -Clink-self-contained=no -Lnative=$WASI_LIBC" \
  CARGO_TARGET_DIR=$ROOT/target/wasm/ld cargo build -q --release --target wasm32-wasip1 -Zbuild-std=std,panic_abort)
cp $ROOT/target/wasm/ld/wasm32-wasip1/release/pliron-wasm-ld.wasm ld.wasm
wasm-tools strip --all $S/bin/rustc.wasm -o rustc.wasm
rm -rf sysroot && mkdir -p sysroot/lib/rustlib/wasm32-unknown-unknown/lib sysroot/lib/rustlib/wasm32-wasip1/lib sysroot/wasi-libc
cp $LIB/*.rlib $LIB/*.rmeta sysroot/lib/rustlib/wasm32-unknown-unknown/lib/
# std for wasm32-wasip1 programs; libtest/getopts/proc_macro are never linked into a bin.
WLIB=${LIB%/wasm32-unknown-unknown/lib}/wasm32-wasip1/lib
for f in $WLIB/*.rlib $WLIB/*.rmeta; do
  case $(basename $f) in libtest-*|libgetopts-*|libproc_macro-*) ;; *) cp $f sysroot/lib/rustlib/wasm32-wasip1/lib/ ;; esac
done
cp $WASI_LIBC/libpliron_wasi_libc.a sysroot/wasi-libc/libc.a
# One gzip bundle per target (fetched on first use) + manifest of [path, offset, len]; served as-is and
# decompressed in the worker with DecompressionStream, so any static server works.
python3 - <<'PY'
import gzip, json, os, hashlib
out = {"targets": {}}
for t in sorted(os.listdir("sysroot/lib/rustlib")):
    files, blob = [], bytearray()
    roots = [f"lib/rustlib/{t}/lib"] + (["wasi-libc"] if t == "wasm32-wasip1" else [])
    for r in roots:
        for f in sorted(os.listdir(f"sysroot/{r}")):
            d = open(f"sysroot/{r}/{f}", "rb").read()
            files.append([f"{r}/{f}", len(blob), len(d)]); blob += d
    open(f"sysroot-{t}.bin.gz", "wb").write(gzip.compress(bytes(blob), 6))
    out["targets"][t] = files
for f in ["rustc.wasm", "ld.wasm"]:
    open(f + ".gz", "wb").write(gzip.compress(open(f, "rb").read(), 6))
h = hashlib.sha256()
for f in sorted(x for x in os.listdir(".") if x.endswith(".gz")):
    h.update(open(f, "rb").read())
out["version"] = h.hexdigest()[:16]
json.dump(out, open("manifest.json", "w"))
PY
npm i --silent
echo "serve with: python3 -m http.server 8787   (then open http://localhost:8787/)"
