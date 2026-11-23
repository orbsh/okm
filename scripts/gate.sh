#!/bin/sh
# The workspace gate, with the one thing a test log cannot say about itself:
# whether it is COMPLETE.
#
# A run that stops after four test binaries still exits 0 — observed once on this
# workspace: 101 "passing" tests out of 795, no error, rc 0, log ending on a
# perfectly ordinary `test result: ok`. So a green log proves nothing on its own.
# What proves something is whether every test binary cargo knows about actually
# ran, so the number comes from cargo — the executables it just built — and not
# from a count a human has to keep up to date.
#
# Doc-tests add result lines without being binaries, so the check is "at least as
# many binaries ran as were built", never an equality on result lines.
set -u

log="${GATE_LOG:-$(mktemp)}"
start=$(date +%s)

# What cargo is going to run: `.profile.test` is true exactly for the targets that
# become test harnesses. Examples and plain binaries are built by `cargo test` too,
# and they have an executable — but nothing runs them, so counting executables
# alone over-counts by 10 on this workspace.
expected="${GATE_EXPECTED_RESULTS:-}"
if [ -z "$expected" ]; then
	if ! command -v jq >/dev/null; then
		echo "gate: jq counts the test binaries here (or set GATE_EXPECTED_RESULTS)" >&2
		exit 2
	fi
	expected=$(
		cargo test --workspace --all-features --no-run --message-format=json 2>>"$log" \
			| jq -r 'select(.reason == "compiler-artifact" and .profile.test and .executable != null) | .executable' \
			| sort -u \
			| wc -l \
			| tr -d ' '
	)
fi

cargo test --workspace --all-features >>"$log" 2>&1
rc=$?

ran=$(grep -c 'Running ' "$log" || true)
lines=$(grep -c 'test result:' "$log" || true)
passed=$(awk '/test result:/ {p += $4} END {print p + 0}' "$log")
failed=$(awk '/test result:/ {f += $6} END {print f + 0}' "$log")
elapsed=$(( $(date +%s) - start ))

echo "gate: $passed passed, $failed failed in ${elapsed}s — $ran of $expected test binaries, $lines result lines"
echo "gate: log at $log"

[ "$rc" -eq 0 ] || { echo "gate: cargo exited $rc"; exit 1; }
[ "$failed" -eq 0 ] || exit 1
if [ "$ran" -lt "$expected" ]; then
	{
		echo "gate: $ran test binaries ran, $expected were built"
		echo "gate: the run stopped early — this is NOT a pass, whatever the exit code said"
	} >&2
	exit 1
fi