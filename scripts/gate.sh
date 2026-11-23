#!/bin/sh
# The workspace gate, with the one thing a test log cannot say about itself:
# whether it is COMPLETE.
#
# A run that stops after four test binaries still exits 0 — observed once on this
# workspace: 101 "passing" tests out of 795, no error, rc 0, log ending on a
# perfectly ordinary `test result: ok`. So a green log proves nothing on its own;
# what proves something is the number of result lines, which is the number of test
# targets cargo ran. GATE_EXPECTED_RESULTS is that number, and it goes up when a
# test target is added.
set -u

log="${GATE_LOG:-$(mktemp)}"
expected="${GATE_EXPECTED_RESULTS:-138}"
start=$(date +%s)

cargo test --workspace --all-features >"$log" 2>&1
rc=$?

lines=$(grep -c 'test result:' "$log" || true)
passed=$(awk '/test result:/ {p += $4} END {print p + 0}' "$log")
failed=$(awk '/test result:/ {f += $6} END {print f + 0}' "$log")
elapsed=$(( $(date +%s) - start ))

echo "gate: $passed passed, $failed failed in ${elapsed}s — $lines result lines (expected >= $expected)"
echo "gate: log at $log"

[ "$rc" -eq 0 ] || { echo "gate: cargo exited $rc"; exit 1; }
[ "$failed" -eq 0 ] || exit 1
if [ "$lines" -lt "$expected" ]; then
	{
		echo "gate: only $lines result lines, expected at least $expected"
		echo "gate: the run stopped early — this is NOT a pass, whatever the exit code said"
	} >&2
	exit 1
fi