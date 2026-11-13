#!/bin/bash
# Tests-on-save smoke test (`examples/hot-test.sh`): three tests over two
# functions; after an edit to `b` only the two tests that entered `b` rerun.
# r[verify native.hot.slot]
set -euo pipefail
B="$(cd "$(dirname "$0")/../.." && pwd)"
case "$(uname)" in Darwin) SEDI=(-i '') ;; *) SEDI=(-i) ;; esac
W="$B/target/hot-test/reach"; rm -rf "$W"; mkdir -p "$W/src"
cat > "$W/Cargo.toml" <<TOML
[package]
name = "reachdemo"
version = "0.1.0"
edition = "2024"
[workspace]
[dev-dependencies]
pliron_hot = { path = "$B/examples/pliron-hot" }
TOML
cat > "$W/src/lib.rs" <<'RS'
#[cfg(test)]
use pliron_hot as _;

#[inline(never)]
pub fn a() -> u32 {
    1
}

#[inline(never)]
pub fn b() -> u32 {
    2
}

#[cfg(test)]
mod t {
    #[test]
    fn ta() {
        assert_eq!(super::a(), 1);
    }
    #[test]
    fn tb() {
        assert_eq!(super::b(), 2);
    }
    #[test]
    fn tab() {
        assert_eq!(super::a() + super::b(), 3);
    }
}
RS
HOT_TEST_ONCE=1 "$B/examples/hot-test.sh" "$W" | tee "$W/pass1.txt"
grep -q "ran 3 of 3 tests" "$W/pass1.txt"
sed "${SEDI[@]}" 's/^    2$/    std::hint::black_box(1) + 1/' "$W/src/lib.rs"
HOT_TEST_ONCE=1 "$B/examples/hot-test.sh" "$W" | tee "$W/pass2.txt"
grep -q "ran 2 of 3 tests" "$W/pass2.txt"
grep -q "ran t::tb" "$W/pass2.txt" && grep -q "ran t::tab" "$W/pass2.txt" && ! grep -q "ran t::ta$" "$W/pass2.txt"
# Nothing changed: nothing reruns.
HOT_TEST_ONCE=1 "$B/examples/hot-test.sh" "$W" | tee "$W/pass3.txt"
grep -q "ran 0 of 3 tests" "$W/pass3.txt" && echo "hot tests-on-save OK"
