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
# This script reproduces both of CI's advantages locally:
#   1. The project lives in a Docker *named volume* (the colima VM's
#      native ext4), not the virtiofs host mount — so the analyzer's
#      file IO is fast.
#   2. A persistent cache volume holds the bootstrapped toolchain +
#      cargo registry + analysis caches, so only the FIRST run pays the
#      ~minutes of bootstrap; later runs are warm.
#
# It runs the same linter image, profile, and `--fail-threshold 0` as
# `.github/workflows/ci.yml`'s Qodana job, reading the same
# `qodana.yaml` (linter, profile, excludes, bootstrap). Findings match
# CI: validated against main run 26928217340, local reproduced 19 of 22
# findings exactly (RsUnnecessaryQualifications, DuplicatedCode,
# RsUnusedImport, CargoUnusedDependency all matched).
#
# Caveat — one inspection can't run offline: `NewCrateVersionAvailable`
# queries crates.io for newer dep versions and needs the Qodana Cloud /
# network integration gated behind QODANA_TOKEN, so a tokenless local
# run skips it (the 3-finding gap above). Export QODANA_TOKEN to close
# it; otherwise that one check stays CI-only.
#
# Timing: first run ~3.5min (bootstraps the toolchain + cold cargo
# metadata + analysis cache into the persistent cache volume); warm runs
# ~3.3min. Both well under the virtiofs timeout the naive bind-mount hits.
#
# Usage:
#   scripts/qodana-local.sh            # scan the working tree
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

for arg in "$@"; do
    case "$arg" in
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

docker volume create "$proj_vol"   >/dev/null
docker volume create "$cache_vol"  >/dev/null
docker volume create "$results_vol" >/dev/null

# Sync the working tree into the project volume. Exclude the heavy
# non-source trees (target/.git and the artifact dirs) so the copy is
# ~12MB / sub-second; keep every cargo workspace member (crates/*,
# demos/sokoban) and the spikes/ paths the root Cargo.toml `exclude`
# references, or `cargo metadata` breaks.
echo "qodana-local: syncing working tree → $proj_vol ..."
# COPYFILE_DISABLE=1 stops macOS `tar` (bsdtar) from emitting AppleDouble
# `._*` sidecar entries for files carrying extended attributes. Without
# it those extract as real `._foo.rs` files in the Linux container, and
# Qodana's "Detached file" inspection fires on every one (~one per source
# file) — pure noise CI never sees. The `._*` exclude is belt-and-braces.
COPYFILE_DISABLE=1 tar -C "$repo_root" \
    --exclude='._*' \
    --exclude=./target --exclude=./.git --exclude=./.claude \
    --exclude=./wishes --exclude=./research --exclude=./traces \
    --exclude=./audits --exclude='./*.png' --exclude=./.qodana-local \
    -cf - . \
    | docker run --rm -i -v "$proj_vol":/data/project alpine \
        sh -c 'cd /data/project && rm -rf ./* ./.[!.]* 2>/dev/null; tar -xf -'

# Qodana wants a git repo at the project root (rev-parse --show-toplevel)
# for VCS metadata; we dropped .git in the sync, so seed a throwaway one
# to silence the "not a git repository" log noise. Cosmetic only — the
# scan runs fine without it (full-scan, no changed-files scoping) — so
# this is best-effort: `|| true` guarantees a git hiccup never sinks the
# pre-flight. `safe.directory '*'` defuses git's dubious-ownership guard
# on the root-owned volume.
docker run --rm -v "$proj_vol":/data/project -e HOME=/root alpine sh -c '
    apk add --no-cache git >/dev/null 2>&1 || exit 0
    git config --global --add safe.directory "*"
    git config --global user.email local@qodana
    git config --global user.name local
    cd /data/project || exit 0
    if [ ! -d .git ]; then
        git init -q && git add -A && git commit -qm snapshot
    fi' || true

rm -rf "$out_dir"; mkdir -p "$out_dir"

echo "qodana-local: scanning ($image) ..."
set +e
docker run --rm -u root \
    -v "$proj_vol":/data/project \
    -v "$cache_vol":/data/cache \
    -v "$results_vol":/data/results \
    ${QODANA_TOKEN:+-e QODANA_TOKEN="$QODANA_TOKEN"} \
    "$image" \
    --fail-threshold 0
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
