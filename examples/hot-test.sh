#!/bin/bash
# Tests on save for a crate's own `#[test]`s (see docs/hot-reload.md):
# after an edit, rerun only the tests that entered a function whose code
# changed.
#
#   examples/hot-test.sh [crate dir]          watch src/, rerun on every save
#   HOT_TEST_ONCE=1 examples/hot-test.sh DIR  one pass, then exit (state is
#                                             kept in target/hot-test)
#
# The crate is built with PLIRON_HOT + PLIRON_HOT_REACH=1: every function
# sits behind a thunk that marks it entered. Each test runs in its own
# process and leaves the list of functions it entered
# (PLIRON_HOT_REACH_OUT). After a save the crate is rebuilt, `hotdiff`
# compares the old and new object by per-function fingerprint, and a test
# reruns when its list meets the changed set, when it failed last time, or
# when it is new. Changes in dependencies are not seen: restart the script.
#
# The crate needs `pliron_hot` (path = ".../examples/pliron-hot") as a
# dev-dependency and one line: `#[cfg(test)] use pliron_hot as _;`.
set -euo pipefail
B="$(cd "$(dirname "$0")/.." && pwd)"
export RUSTUP_TOOLCHAIN=nightly-2026-10-22
DIR="${1:-.}"; cd "$DIR"
case "$(uname)" in Darwin) DYLIB=dylib; U=_pliron_hot_reach_init;; *) DYLIB=so; U=pliron_hot_reach_init;; esac
BACKEND="${PLIRON_BACKEND:-$B/target/debug/librustc_codegen_pliron.$DYLIB}"
NAME=$(cargo metadata --no-deps --format-version 1 -q --offline \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["targets"][0]["name"])')
CRATE="${NAME//-/_}"
export CARGO_INCREMENTAL=0
export RUSTFLAGS="-Zcodegen-backend=$BACKEND -Ccodegen-units=1 -Csave-temps -Clink-arg=-Wl,-u,$U"
# An explicit --target keeps RUSTFLAGS (the forced `-u` symbol above all)
# off build scripts and proc macros.
HOST=$(rustc -vV | sed -n 's/^host: //p')
S=target/hot-test; mkdir -p "$S/reach" "$S/status"
HOTDIFF="$S/hotdiff"
if [[ ! -x "$HOTDIFF" ]]; then
    RUSTFLAGS= CARGO_TARGET_DIR="$S/tool" cargo build -q --offline --release \
        --manifest-path "$B/examples/pliron-hot/Cargo.toml" --bin hotdiff
    cp "$S/tool/release/hotdiff" "$HOTDIFF"
fi

pass() {
    local exe obj changed n=0 ran=0 failed=0
    exe=$(PLIRON_HOT="$CRATE" PLIRON_HOT_REACH=1 cargo test -q --offline --no-run --target "$HOST" --message-format=json 2>"$S/build.log" \
        | python3 -c '
import json, sys
exe = ""
for l in sys.stdin:
    try: m = json.loads(l)
    except ValueError: continue
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["target"]["name"] == sys.argv[1]:
        exe = m["executable"]
print(exe)' "$NAME") || true
    if [[ -z "$exe" ]]; then echo "[hot-test] build failed:"; tail -20 "$S/build.log"; return 1; fi
    # The crate's object: the newest `<crate>.<hash>-cgu.N.rcgu.o` next to
    # the binary (the other `.rcgu.o` there is the allocator shim).
    obj=$(python3 -c '
import glob, os, sys
fs = [f for f in glob.glob(glob.escape(sys.argv[1]) + ".*.rcgu.o") if "-cgu." in f]
if fs:
    print(max(fs, key=os.path.getmtime))' "$exe")
    if [[ -z "$obj" ]]; then echo "[hot-test] no object next to $exe"; return 1; fi
    if [[ -f "$S/prev.o" ]]; then
        "$HOTDIFF" "$S/prev.o" "$obj" | sort > "$S/changed.txt"
    else
        : > "$S/changed.txt"; rm -f "$S"/reach/* "$S"/status/*
    fi
    changed=$(wc -l < "$S/changed.txt" | tr -d ' ')
    while IFS= read -r t; do
        n=$((n + 1))
        if [[ -f "$S/reach/$t" && "$(cat "$S/status/$t" 2>/dev/null)" == ok ]] \
            && [[ -z "$(comm -12 "$S/changed.txt" "$S/reach/$t" | head -1)" ]]; then
            continue
        fi
        ran=$((ran + 1))
        if PLIRON_HOT_REACH_OUT="$S/reach/$t.new" "$exe" --exact "$t" > "$S/out.txt" 2>&1; then
            echo ok > "$S/status/$t"
        else
            echo failed > "$S/status/$t"; failed=$((failed + 1))
            echo "[hot-test] FAILED $t"; sed 's/^/    /' "$S/out.txt" | tail -15
        fi
        if [[ -f "$S/reach/$t.new" ]]; then sort "$S/reach/$t.new" > "$S/reach/$t"; rm -f "$S/reach/$t.new"; fi
        echo "[hot-test] ran $t"
    done < <("$exe" --list --format terse 2>/dev/null | sed -n 's/: test$//p')
    cp "$obj" "$S/prev.o"
    echo "[hot-test] ran $ran of $n tests ($changed changed functions), $failed failed"
}

pass || true
[[ -n "${HOT_TEST_ONCE:-}" ]] && exit 0
stamp() { find src -name '*.rs' -exec stat -f %m {} + 2>/dev/null || find src -name '*.rs' -exec stat -c %Y {} +; }
last=$(stamp | sort -n | tail -1)
echo "[hot-test] watching src/"
while true; do
    sleep 0.3
    m=$(stamp | sort -n | tail -1); [[ $m == "$last" ]] && continue
    last=$m; pass || true
done
