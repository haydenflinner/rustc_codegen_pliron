#!/bin/sh
# Cheap stand-in for `tracey query validate`: every `r[impl id]` /
# `r[verify id]` in the traced sources must name a requirement defined in
# spec/chapters/*.typ. Prints unknown ids and per-id annotation counts.
set -eu
cd "$(dirname "$0")/.."
defs=$(mktemp); refs=$(mktemp)
trap 'rm -f "$defs" "$refs"' EXIT
grep -ho '#r("[a-z0-9.-]*"' spec/chapters/*.typ | sed 's/#r("//; s/"$//' | sort -u >"$defs"
grep -rhoE 'r\[(impl|verify) [a-z0-9.-]+(\+[0-9]+)?\]' \
  src pliron-cfg/src pliron-ir-opt pliron-clif-lower/src pliron-clif-opt pliron-clif-check/src tools tests \
  --include='*.rs' 2>/dev/null | sed -E 's/r\[(impl|verify) //; s/(\+[0-9]+)?\]$//' | sort >"$refs" || true
bad=$(sort -u "$refs" | comm -23 - "$defs")
echo "$(wc -l <"$defs" | tr -d ' ') requirements, $(wc -l <"$refs" | tr -d ' ') annotations, $(sort -u "$refs" | wc -l | tr -d ' ') ids covered"
if [ -n "$bad" ]; then echo "unknown ids:"; echo "$bad"; exit 1; fi
