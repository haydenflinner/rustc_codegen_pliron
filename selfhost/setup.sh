#!/bin/bash
# Self-host: make a local (never pushed) rust worktree at the commit of the
# pinned nightly, drop this backend in as compiler/rustc_codegen_pliron, and
# build with codegen-backends = ["pliron"]. Stage1 rustc is built by stage0
# (LLVM); stage1 std and the stage2 compiler are built through pliron.
#   selfhost/setup.sh [RUST_CHECKOUT] [WORKTREE]
set -euo pipefail
B="$(cd "$(dirname "$0")/.." && pwd)"
RUST="${1:-$HOME/work/rust}"
WT="${2:-$HOME/work/rust-sh}"
COMMIT=$(rustc +nightly-2026-10-06 -vV | sed -n 's/^commit-hash: //p')
if [[ ! -d "$WT" ]]; then
    git -C "$RUST" cat-file -e "$COMMIT" 2>/dev/null || git -C "$RUST" fetch --depth=1 origin "$COMMIT"
    git -C "$RUST" worktree add --detach "$WT" "$COMMIT"
    git -C "$WT" apply "$B/selfhost/bootstrap.patch"
    cp "$B/selfhost/bootstrap.toml" "$WT/bootstrap.toml"
fi
# -c, no -t: changed files get a fresh mtime, so cargo rebuilds the backend even
# when the edit is older than the last build artifact.
rsync -rlpc --delete --exclude target --exclude rust-toolchain.toml --exclude examples --exclude .git \
    "$B/" "$WT/compiler/rustc_codegen_pliron/"
echo "now: cd $WT && ./x build --stage 1 library && ./x build --stage 2 compiler"
