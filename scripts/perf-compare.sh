#!/usr/bin/env bash
# Noise-aware dispatch perf comparison for a PR (iamacoffeepot/aether#1077,
# ADR-0085). Builds the `aether-perf-trial` sweep binary from both the PR
# tip and its merge-base with main, then interleaves K trials of each on
# THIS runner (same machine — the pairing that cancels run-to-run drift)
# and renders the comparison.
#
# Outputs (in repo root):
#   perf-report.json   machine-readable ComparisonReport
#   perf-report.md     sticky PR-comment markdown body
#
# Informational only — exits 0 even with regressions present (a non-zero
# exit means an operational failure: a build broke, a trial crashed).
#
# Env knobs (defaults tuned for the CI preset — warm, a fan-out + chain
# subset, ~1 min of measurement once iamacoffeepot/aether#1079 landed):
#   PERF_K               trials per side (default 12)
#   AETHER_PERF_WORKERS  pool sizes (default "max,2")
#   AETHER_PERF_FRAMES   frames per cell (default 200)
#   AETHER_PERF_TOPOS    "ci" | "full" (default "ci")
#
# Usage: scripts/perf-compare.sh

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

K="${PERF_K:-12}"
export AETHER_PERF_WORKERS="${AETHER_PERF_WORKERS:-max,2}"
export AETHER_PERF_FRAMES="${AETHER_PERF_FRAMES:-200}"
export AETHER_PERF_TOPOS="${AETHER_PERF_TOPOS:-ci}"

json_out="$ROOT/perf-report.json"
md_out="$ROOT/perf-report.md"

# Resolve the baseline: the merge-base of the PR tip and main, so the
# comparison isolates THIS PR's changes (not whatever else merged since
# the branch forked).
git fetch --no-tags --quiet origin main
base_sha="$(git merge-base HEAD FETCH_HEAD)"
base_short="$(git rev-parse --short "$base_sha")"
echo "[perf-compare] baseline = merge-base $base_short; K=$K, workers=$AETHER_PERF_WORKERS, frames=$AETHER_PERF_FRAMES, topos=$AETHER_PERF_TOPOS"

# Base checkout in a throwaway worktree, cleaned up on exit.
base_wt="$(mktemp -d "${TMPDIR:-/tmp}/aether-perf-base.XXXXXX")"
cleanup() {
    git worktree remove --force "$base_wt" 2>/dev/null || true
}
trap cleanup EXIT
git worktree add --quiet --detach "$base_wt" "$base_sha"

# Build the candidate (PR) trial + compare binaries, and the base trial.
echo "[perf-compare] building candidate (PR) binaries…"
cargo build --release -p aether-substrate-bundle \
    --bin aether-perf-trial --bin aether-perf-compare

cand_trial="$ROOT/target/release/aether-perf-trial"
compare_bin="$ROOT/target/release/aether-perf-compare"
base_trial="$base_wt/target/release/aether-perf-trial"

echo "[perf-compare] building base ($base_short) trial binary…"
if ! (cd "$base_wt" && cargo build --release -p aether-substrate-bundle --bin aether-perf-trial); then
    # The merge-base predates the perf-trial bin (a PR forked before
    # iamacoffeepot/aether#1081). Nothing to compare against — emit a note,
    # not a failure (this job is informational).
    cat > "$md_out" <<EOF
<!-- aether-perf-report -->
## dispatch perf

_Baseline ($base_short) predates the \`perf-trial\` harness — no comparison available for this PR._
EOF
    echo "[perf-compare] base build failed (baseline predates perf-trial); wrote note to $md_out"
    exit 0
fi

# Interleave K trials per side on this runner; render JSON + markdown.
echo "[perf-compare] running $K interleaved trials per side…"
"$compare_bin" \
    --base "$base_trial" \
    --cand "$cand_trial" \
    -k "$K" \
    --out "$json_out" \
    --title "PR vs merge-base $base_short" \
    --subtitle "baseline $base_short · $K trials/config, interleaved on one runner" \
    > "$md_out"

echo "[perf-compare] wrote $json_out and $md_out"
