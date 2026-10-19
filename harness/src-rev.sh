#!/bin/sh
# Print this checkout's source revision: `git describe --always --dirty`,
# plus a short hash of the uncommitted diff when dirty, so two different
# dirty trees never print the same id. Stamped into the backend dylib by
# build.rs (`PLIRON_VERSION=1` prints it) and into self-host copies by
# selfhost/setup.sh (.pliron-src-rev).
cd "$(dirname "$0")/.." || exit 1
rev=$(git describe --always --dirty --abbrev=10 2>/dev/null) || { echo unknown; exit 0; }
case "$rev" in
  *-dirty) echo "$rev-$(git diff HEAD | cksum | cut -d' ' -f1)" ;;
  *) echo "$rev" ;;
esac
