#!/usr/bin/env bash
# Disk report for the purerust workspace: every cargo/bootstrap build dir
# and scratch probe dir, largest first, plus free space.
#
#   harness/disk.sh            report only
#   harness/disk.sh --prune    also delete scratch build dirs next to this
#                              repo (target-*/ and target-*.err from
#                              experiments; never target/ itself)
#   harness/disk.sh --sweep    in target/debug/build, drop workspace-crate
#                              build units untouched for 2+ days (every
#                              env/flag/test-target variant leaves a unit
#                              behind; pliron-clif-opt had 267, 43 GB) and
#                              incremental sessions as old. Each unit holds
#                              its own fingerprint, so a deleted one that is
#                              still needed just rebuilds. Dependencies are
#                              left alone, so the next build stays warm.
set -euo pipefail
cd "$(dirname "$0")/.."
WS=$(cd .. && pwd)
echo "free: $(df -h . | awk 'NR==2 {print $4 " of " $2 " (" $5 " used)"}')"
{
  for d in "$WS"/*/target "$WS"/*/build "$WS"/rustc_codegen_pliron/target-* "$WS"/*/target/*/; do
    [[ -e "$d" ]] && du -sk "$d" 2>/dev/null
  done
} | sort -rn | head -25 | awk '{printf "%8.1f GB  %s\n", $1/1048576, $2}'
if [[ "${1:-}" == --sweep ]]; then
  for c in pliron-clif-opt pliron-clif-lower pliron-ir-opt pliron-cfg pliron-clif-check rustc_codegen_pliron; do
    d=target/debug/build/$c
    [[ -d $d ]] || continue
    find "$d" -mindepth 1 -maxdepth 1 -type d -mtime +2 -exec rm -rf {} +
  done
  find target/debug/incremental -mindepth 1 -maxdepth 1 -mtime +2 -exec rm -rf {} + 2>/dev/null || true
  echo "after sweep: $(du -sh target | cut -f1) in target/"
fi
if [[ "${1:-}" == --prune ]]; then
  for d in target-*; do
    [[ -e "$d" ]] || continue
    echo "rm -rf $d"; rm -rf "$d"
  done
fi
