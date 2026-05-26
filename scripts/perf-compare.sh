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
# Build cost (iamacoffeepot/aether#1084). The base side used to recompile
# the whole dependency tree from scratch in its own worktree target dir.
# Two levers cut that:
#   - PERF_BASE_CACHE: if set to a file path, a cached base binary there
#     is reused as-is (the merge-base is fixed for a PR's life, so a
#     re-push skips the base build entirely); a freshly built base binary
#     is copied there for the workflow's actions/cache to persist.
#   - The cold base build shares the candidate's CARGO_TARGET_DIR, so it
#     reuses the already-compiled release deps and only the changed
#     workspace crates recompile.
#
# Env knobs (defaults tuned for the CI preset — warm, a fan-out + chain
# subset, ~1 min of measurement):
#   PERF_K               trials per side (default 12)
#   AETHER_PERF_WORKERS  pool sizes (default "max,2")
#   AETHER_PERF_FRAMES   frames per cell (default 200)
#   AETHER_PERF_TOPOS    "ci" | "full" (default "ci")
#   PERF_BASE_CACHE      file path for the cross-run base-binary cache
#                        (set by the workflow; unset locally = always build)
#   PERF_BASE_ENV        space-separated KEY=VALUE list applied as env to
#   PERF_CAND_ENV        only the base / only the candidate trial process
#                        (via the comparator's --base-env / --cand-env), so a
#                        run can pin a scheduler knob per side instead of
#                        relying on each binary's compiled default — e.g.
#                        PERF_BASE_ENV="AETHER_PEER_STEAL=1" measures the
#                        owner-only default candidate against a steal-on base
#                        on the same binary (iamacoffeepot/aether#1174). Unset
#                        = the plain code-vs-merge-base comparison.
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
base_cache="${PERF_BASE_CACHE:-}"

# Optional per-side scheduler-knob pins (iamacoffeepot/aether#1174). Each
# space-separated KEY=VALUE in PERF_BASE_ENV / PERF_CAND_ENV becomes a
# --base-env / --cand-env flag on the comparator, applied to only that side's
# trial process. The `${arr[@]+...}` guard keeps an empty array safe under
# `set -u`.
base_env_args=()
for kv in ${PERF_BASE_ENV:-}; do base_env_args+=(--base-env "$kv"); done
cand_env_args=()
for kv in ${PERF_CAND_ENV:-}; do cand_env_args+=(--cand-env "$kv"); done
pin_note=""
if [ -n "${PERF_BASE_ENV:-}${PERF_CAND_ENV:-}" ]; then
    pin_note=" · pinned base[${PERF_BASE_ENV:-default}] cand[${PERF_CAND_ENV:-default}]"
fi

# Transient working dir for the built binaries, plus the worktree (created
# only on a base-cache miss). Both cleaned up on exit.
work="$(mktemp -d "${TMPDIR:-/tmp}/aether-perf.XXXXXX")"
base_wt=""
cleanup() {
    if [ -n "$base_wt" ]; then
        git worktree remove --force "$base_wt" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

cand_trial="$work/aether-perf-trial-cand"
compare_bin="$work/aether-perf-compare"

# Resolve the baseline: the merge-base of the PR tip and main, so the
# comparison isolates THIS PR's changes (not whatever else merged since
# the branch forked).
git fetch --no-tags --quiet origin main
base_sha="$(git merge-base HEAD FETCH_HEAD)"
base_short="$(git rev-parse --short "$base_sha")"
echo "[perf-compare] baseline = merge-base $base_short; K=$K, workers=$AETHER_PERF_WORKERS, frames=$AETHER_PERF_FRAMES, topos=$AETHER_PERF_TOPOS"

# Build the candidate (PR) binaries into the shared target, then copy them
# aside. The base build below reuses this same target dir, and both crates
# emit `aether-perf-trial` at the same path — copying the candidate out
# first keeps the base build from clobbering it.
echo "[perf-compare] building candidate (PR) binaries…"
cargo build --release -p aether-substrate-bundle \
    --bin aether-perf-trial --bin aether-perf-compare --bin aether-perf-plot
cp "$ROOT/target/release/aether-perf-trial" "$cand_trial"
cp "$ROOT/target/release/aether-perf-compare" "$compare_bin"
# Copy the candidate `aether-perf-plot` aside too. The workflow's plot step
# renders from THIS binary instead of rebuilding, because the base build below
# clobbers the shared target's `aether-substrate` artifacts — a `cargo build
# --bin aether-perf-plot` afterward links the stale merge-base substrate and
# plots the wrong default (the distributions then diverge from the percentile
# table the comparison reports). Lands in `target/` (gitignored; survives this
# script's cleanup trap and persists across workflow steps).
cp "$ROOT/target/release/aether-perf-plot" "$ROOT/target/aether-perf-plot-cand"

# Resolve the base trial binary: prefer the cross-run cache, else build it
# from a throwaway worktree (sharing the candidate's target dir).
base_trial=""
if [ -n "$base_cache" ] && [ -x "$base_cache" ]; then
    echo "[perf-compare] base cache hit ($base_cache) — skipping base build"
    base_trial="$base_cache"
else
    base_wt="$(mktemp -d "${TMPDIR:-/tmp}/aether-perf-base.XXXXXX")"
    git worktree add --quiet --detach "$base_wt" "$base_sha"

    echo "[perf-compare] building base ($base_short) trial binary (shared target)…"
    if (cd "$base_wt" && CARGO_TARGET_DIR="$ROOT/target" \
            cargo build --release -p aether-substrate-bundle --bin aether-perf-trial); then
        built="$ROOT/target/release/aether-perf-trial"
        if [ -n "$base_cache" ]; then
            mkdir -p "$(dirname "$base_cache")"
            cp "$built" "$base_cache"
            base_trial="$base_cache"
        else
            base_trial="$work/aether-perf-trial-base"
            cp "$built" "$base_trial"
        fi
    else
        # The merge-base predates the perf-trial bin (a PR forked before
        # iamacoffeepot/aether#1081). Nothing to compare against — emit a
        # note, not a failure (this job is informational).
        cat > "$md_out" <<EOF
<!-- aether-perf-report -->
## dispatch perf

_Baseline ($base_short) predates the \`perf-trial\` harness — no comparison available for this PR._
EOF
        echo "[perf-compare] base build failed (baseline predates perf-trial); wrote note to $md_out"
        exit 0
    fi
fi

# Interleave K trials per side on this runner; render JSON + markdown.
echo "[perf-compare] running $K interleaved trials per side…"
"$compare_bin" \
    --base "$base_trial" \
    --cand "$cand_trial" \
    ${base_env_args[@]+"${base_env_args[@]}"} \
    ${cand_env_args[@]+"${cand_env_args[@]}"} \
    -k "$K" \
    --out "$json_out" \
    --title "PR vs merge-base $base_short" \
    --subtitle "baseline $base_short · $K trials/config, interleaved on one runner$pin_note" \
    > "$md_out"

echo "[perf-compare] wrote $json_out and $md_out"
