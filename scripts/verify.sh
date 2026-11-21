#!/usr/bin/env bash
# Local merge gate (M3): fmt check, clippy, tests — all three HARD.
# Any failure fails the script; the summary is paste-ready.
#
# Known pre-existing warnings are allowlisted in source with
# `#[allow(...)] // Allowlist (M3 lint gate): …` comments — there is no
# warning allowlist here. `cargo fmt --check` covers the whole tree: the
# one-time tree-wide `cargo fmt` landed in this same PR, so drift from
# that point on fails the gate.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== cargo fmt --check (hard) =="
cargo fmt --check
echo
echo "== cargo clippy --all-targets -- -D warnings (hard) =="
cargo clippy --all-targets -- -D warnings
echo
echo "== cargo test --all-targets =="
cargo test --all-targets
echo
echo "== summary =="
HEAD_SHA=$(git rev-parse HEAD)
TESTS=$(cargo test --all-targets 2>&1 | grep -E 'test result' | awk '{ passed+=$4; failed+=$6 } END { print passed+0 " passed, " failed+0 " failed" }')
echo "Head: $HEAD_SHA"
echo "Tests: $TESTS"
