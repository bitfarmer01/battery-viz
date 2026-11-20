#!/usr/bin/env sh
# Merge gate: fmt check, clippy, and the full test suite.
# Usage: scripts/verify.sh   (exit 0 = green; output ends with a paste-ready summary)
set -u
cd "$(dirname "$0")/.."

fail=0
# NOTE: fmt and clippy are reported but do not fail the run until tree-wide
# adoption (a later PR reformats); test failure is the hard gate.
echo "=== cargo fmt --check ==="
if cargo fmt --check; then
    FMT=ok
else
    FMT=FAILED
fi

echo
echo "=== cargo clippy --all-targets ==="
CLIPPY_OUT=$(cargo clippy --all-targets 2>&1)
CLIPPY_EXIT=$?
echo "$CLIPPY_OUT" | grep -E '^(warning|error)' | sort | uniq -c | sort -rn || true
[ "$CLIPPY_EXIT" -eq 0 ] && CLIPPY=ok || { CLIPPY=FAILED; fail=1; }
# (clippy hard-fails only on errors, not on pre-existing warnings)

echo
echo "=== cargo test --all-targets ==="
TEST_OUT=$(cargo test --all-targets 2>&1)
TEST_EXIT=$?
echo "$TEST_OUT" | grep -E '^test result' || true
[ "$TEST_EXIT" -eq 0 ] && TEST=ok || { TEST=FAILED; fail=1; }

TESTS=$(echo "$TEST_OUT" | grep -oE '[0-9]+ passed' | head -1)

echo
echo "=== verify summary (paste into PR) ==="
echo "- cargo fmt --check: $FMT"
echo "- cargo clippy --all-targets: $CLIPPY"
echo "- cargo test --all-targets: $TEST (${TESTS:-unknown})"
echo "- head: $(git rev-parse HEAD)"
exit $fail
