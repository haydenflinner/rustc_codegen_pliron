# harness/dsa/retry.sh — sourced by build.sh and tests/dsa/retry.sh.
#
# lost_to_signal LOG MARK RC: did the cargo run that appended to LOG after
# line MARK, and exited RC, lose a process to a signal? True when cargo
# itself died of one (RC >= 128) or when a rustc did: cargo then exits 101
# and reports `(signal: 15, SIGTERM: termination signal)` (or signal 9),
# which is what the machine's earlyoom sends under memory pressure.
#
# One process reads the log. The old test, `tail -n +N LOG | grep -q`,
# failed under `set -o pipefail` whenever the match came early: grep -q
# exits at the first match, tail dies of SIGPIPE writing the rest (cargo
# prints the failing rustc's whole command line, tens of KB, after the
# signal line), the pipeline's status is 141, and build.sh gave up
# instead of retrying ("cargo failed (exit 101) after 232 s, 1 attempt(s)").
lost_to_signal() {
  local log=$1 mark=$2 rc=$3
  [ "$rc" -ge 128 ] && return 0
  awk -v m="$mark" 'NR > m && /signal: (9|15)[,)]/ { f = 1 } END { exit !f }' "$log"
}
