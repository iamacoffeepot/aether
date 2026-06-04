#!/usr/bin/env bash
# Repeat-run ("soak") the flake-marked concurrent tests to surface timing
# flakes before a release. Concurrent programming is hard and timing flakes
# pass on a lucky run, so a single green CI pass does not clear a concurrent
# test (iamacoffeepot/aether#1060).
#
# Marking is by name, because nextest selects on test name/path, not on a
# Rust attribute: a flake-prone test lives in a `mod heavy` submodule, whose
# `::heavy::` path segment shows in the test name (iamacoffeepot/aether#1341).
# That same marker drives the `serial-heavy` test-group in
# `.config/nextest.toml` (which serializes the set during normal CI runs), so
# one convention covers both serialize-in-CI and soak-before-release.
# nextest's `--stress-count` then runs each selected test N times, each in a
# fresh process — the process isolation that matters for timing flakes.
#
# Usage:
#   scripts/flake-soak.sh [count]     # default 200
#   AETHER_FLAKE_FILTER='test(/::heavy::/)' scripts/flake-soak.sh 1000
#
# Exits non-zero if any iteration of any marked test fails.
set -euo pipefail

count="${1:-200}"
filter="${AETHER_FLAKE_FILTER:-test(/::heavy::/)}"

cd "$(git rev-parse --show-toplevel)"

echo "[flake-soak] soaking '${filter}' ×${count} (fresh process per run)…"
exec cargo nextest run \
    --workspace --all-features \
    --profile flake-soak \
    --stress-count "${count}" \
    -E "${filter}"
