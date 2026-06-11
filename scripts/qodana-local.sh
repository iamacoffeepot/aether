#!/usr/bin/env bash
# Run the CI-equivalent Qodana scan locally — fast.
#
# Why this exists: `qodana scan` (and the IDE) bind-mount the repo into
# the linter container. On colima that mount is virtiofs, and Qodana's
# `cargo metadata` pass does thousands of small random reads resolving
# the wasmtime/cranelift/wgpu trees — slow enough to time out
# (iamacoffeepot/aether#1099, and the CLAUDE.md "Qodana pre-flight"
# note). CI never hits this: it checks out onto the runner's native
# ext4 and caches `/data/cache` across runs.
#
# This script reproduces CI's advantages locally:
#   1. The project lives in a Docker *named volume* (the colima VM's
#      native ext4), not the virtiofs host mount — so the analyzer's
#      file IO is fast.
#   2. A persistent cache volume holds the bootstrapped toolchain +
#      cargo registry + analysis caches, so only the FIRST run pays the
#      ~minutes of bootstrap; later runs are warm.
#   3. Real git history rides into the project volume, so the scan runs
#      in the same *scoped* (PR) mode CI's qodana-action auto-enables on
#      a pull request: only findings newly introduced against the
#      merge-base with origin/main count toward `failThreshold`. CI's
#      action does this implicitly in a PR context (the "Using 'scoped'
#      script" log line); locally we pass `--diff-start <merge-base>`
#      explicitly. Pass `--full` to force the old whole-tree scan, and
#      the scan also falls back to whole-tree automatically when there is
#      no diff against origin/main (e.g. running on main itself).
#
# It runs the same linter image, profile, and fail-threshold as
# `.github/workflows/ci.yml`'s Qodana job, reading the same `qodana.yaml`
# (linter, profile, excludes, bootstrap, `failThreshold` — the single
# source of truth both this run and CI inherit). Matching CI's scope is
# the point: a plain whole-tree scan counts every pre-existing finding
# and over-reports against CI's PR-mode gate.
#
# Cost of syncing `.git`: the project volume now also carries the repo's
# git history (the scoped diff needs it). The sync is larger than the old
# source-only copy, but it lands on the VM's native ext4 so the scan IO
# stays fast.
#
# Timing: first run ~3.5min (bootstraps the toolchain + cold cargo
# metadata + analysis cache into the persistent cache volume); warm runs
# ~3.3min. Both well under the virtiofs timeout the naive bind-mount hits.
#
# Usage:
#   scripts/qodana-local.sh            # scoped scan vs origin/main merge-base
#   scripts/qodana-local.sh --full     # whole-tree scan (every finding)
#   scripts/qodana-local.sh --rebuild-cache   # drop the cache volume first
#
# Exit code is Qodana's: 0 = no findings over threshold, non-zero =
# findings (same gate as CI). SARIF + the HTML report land in
# ./.qodana-local/ (gitignored).

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

image=$(awk -F': *' '/^linter:/{print $2; exit}' qodana.yaml)
[[ -n "$image" ]] || { echo "qodana-local: could not read linter from qodana.yaml" >&2; exit 1; }

proj_vol=aether-qodana-project
cache_vol=aether-qodana-cache
results_vol=aether-qodana-results
out_dir="$repo_root/.qodana-local"

full_scan=0
for arg in "$@"; do
    case "$arg" in
        --full) full_scan=1 ;;
        --rebuild-cache) docker volume rm -f "$cache_vol" >/dev/null 2>&1 || true ;;
        *) echo "qodana-local: unknown arg '$arg'" >&2; exit 2 ;;
    esac
done

# colima/docker must be up. Don't auto-start — a cold colima boot is
# slow enough to look hung; tell the user to start it themselves.
if ! docker info >/dev/null 2>&1; then
    echo "qodana-local: docker is not reachable. Start colima first:" >&2
    echo "    colima start --cpu 6 --memory 12" >&2
    exit 1
fi

# Serialize concurrent runs. The project / cache / results volumes are
# fixed shared names, so a second run syncing into $proj_vol mid-scan
# would clobber the first (parallel implement-agent pre-flights, or a
# manual run alongside one). Take an exclusive lock for the rest of the
# script; others queue instead of corrupting the volume. An atomic
# `mkdir` lock — not `flock`, which isn't standard on the macOS host —
# released on any exit via the trap.
lock_dir="${TMPDIR:-/tmp}/aether-qodana.lock.d"
while ! mkdir "$lock_dir" 2>/dev/null; do
    echo "qodana-local: another scan holds the lock ($lock_dir); waiting ..."
    sleep 5
done
trap 'rmdir "$lock_dir" 2>/dev/null || true' EXIT

docker volume create "$proj_vol"   >/dev/null
docker volume create "$cache_vol"  >/dev/null
docker volume create "$results_vol" >/dev/null

# Sync the working tree — including `.git` — into the project volume.
# Real git history is what lets the scan run scoped: `qodana scan
# --diff-start <merge-base>` walks the history to compute the changed
# region. Still exclude the heavy non-source trees (target/ and the
# artifact dirs); keep every cargo workspace member (crates/*,
# demos/sokoban) and the spikes/ paths the root Cargo.toml `exclude`
# references, or `cargo metadata` breaks.
echo "qodana-local: syncing working tree (+ .git) → $proj_vol ..."
# COPYFILE_DISABLE=1 stops macOS `tar` (bsdtar) from emitting AppleDouble
# `._*` sidecar entries for files carrying extended attributes. Without
# it those extract as real `._foo.rs` files in the Linux container, and
# Qodana's "Detached file" inspection fires on every one (~one per source
# file) — pure noise CI never sees. The `._*` exclude is belt-and-braces.
COPYFILE_DISABLE=1 tar -C "$repo_root" \
    --exclude='._*' \
    --exclude=./target --exclude=./.claude \
    --exclude=./wishes --exclude=./research \
    --exclude='./*.png' --exclude=./.qodana-local \
    -cf - . \
    | docker run --rm -i -v "$proj_vol":/data/project alpine \
        sh -c 'cd /data/project && rm -rf ./* ./.[!.]* 2>/dev/null; tar -xf -'

# In a `.claude/worktrees/` worktree, `.git` is a gitdir-*pointer* file,
# not the object store — the sync above copied that pointer, so the
# container has no usable repo to diff. Reconstruct a standalone `.git`:
# the common dir (objects + refs, shared across worktrees) is the bulk,
# overlaid with this worktree's HEAD + index so the right commit reads as
# HEAD. A normal checkout has `.git` as a real directory, so this whole
# block is skipped and the tar above already carried real history.
if [[ -f "$repo_root/.git" ]]; then
    common_dir=$(cd "$(git rev-parse --git-common-dir)" && pwd)
    git_dir=$(git rev-parse --absolute-git-dir)
    echo "qodana-local: worktree — overlaying real git history ($common_dir) ..."
    COPYFILE_DISABLE=1 tar -C "$common_dir" \
        --exclude='._*' --exclude=./worktrees -cf - . \
        | docker run --rm -i -v "$proj_vol":/data/project alpine \
            sh -c 'cd /data/project && rm -rf .git && mkdir .git && tar -C .git -xf -'
    COPYFILE_DISABLE=1 tar -C "$git_dir" -cf - ./HEAD ./index 2>/dev/null \
        | docker run --rm -i -v "$proj_vol":/data/project alpine \
            sh -c 'cd /data/project/.git && tar -xf -' || true
fi

# Decide scoped vs full scan. CI's qodana-action auto-enables scoped (PR)
# mode in a pull-request context, diffing against the base branch so only
# newly-introduced findings count toward `failThreshold`. Mirror that:
# diff against the merge-base with origin/main and hand it to the scan as
# `--diff-start`. The merge-base resolves here on the host against the
# shared repo refs, so this works from a `.claude/worktrees/` worktree
# too; the `.git` synced above carries the history the container walks.
scan_args=()
if [[ "$full_scan" -eq 1 ]]; then
    echo "qodana-local: --full → whole-tree scan (every finding counts)."
else
    base=$(git merge-base HEAD origin/main 2>/dev/null || true)
    head_sha=$(git rev-parse HEAD 2>/dev/null || true)
    if [[ -n "$base" && "$base" != "$head_sha" ]]; then
        echo "qodana-local: scoped scan → --diff-start $base (vs origin/main)."
        scan_args+=(--diff-start "$base")
    else
        echo "qodana-local: no diff against origin/main (on main, or merge-base == HEAD) → whole-tree scan."
    fi
fi

rm -rf "$out_dir"; mkdir -p "$out_dir"

echo "qodana-local: scanning ($image) ..."
set +e
docker run --rm -u root \
    -v "$proj_vol":/data/project \
    -v "$cache_vol":/data/cache \
    -v "$results_vol":/data/results \
    ${QODANA_TOKEN:+-e QODANA_TOKEN="$QODANA_TOKEN"} \
    "$image" ${scan_args[@]+"${scan_args[@]}"}
scan_status=$?
set -e

# Copy SARIF + report out of the results volume to the gitignored dir.
docker run --rm -v "$results_vol":/data/results -v "$out_dir":/out alpine \
    sh -c 'cp -a /data/results/. /out/ 2>/dev/null || true'

sarif="$out_dir/qodana.sarif.json"
if [[ -f "$sarif" ]]; then
    echo
    echo "qodana-local: findings by severity —"
    # Summarise the SARIF result set without pulling in jq deps beyond
    # what's already on the host.
    if command -v jq >/dev/null 2>&1; then
        jq -r '
          [.runs[].results[]?
           | .level // (.properties.qodanaSeverity // "unknown")] as $lv
          | ($lv | group_by(.) | map({k:.[0], n:length}))[]
          | "  \(.k): \(.n)"' "$sarif" 2>/dev/null || echo "  (could not parse SARIF)"
        total=$(jq '[.runs[].results[]?] | length' "$sarif" 2>/dev/null || echo "?")
        echo "  total: $total"
    fi
    echo "qodana-local: full report → $out_dir (open report/index.html)"
fi

echo "qodana-local: scan exit status $scan_status"
exit "$scan_status"
