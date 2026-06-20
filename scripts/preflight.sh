#!/usr/bin/env bash
# Runs the CI-equivalent local pre-flight on the workspace.
#
# Mirrors the CI gates (.github/workflows/ci.yml) that are feasible
# to run locally: fmt + clippy + doc + nextest + wasm32 component
# cross-build + qodana. Qodana runs in the pinned `jetbrains/qodana-rust`
# container (read from qodana.yaml) diff-scoped to the origin/main
# merge-base, mirroring CI's PR-mode scan; scripts/qodana-report.sh still
# surfaces the CI run's findings and /land resolves them (see CLAUDE.md
# § "Qodana"). Requires Docker.
#
# On success, writes `.git/aether-preflight-passed` with the current
# HEAD sha + unix timestamp. The pre-push hook (.githooks/pre-push)
# uses this stamp to skip pre-flight when HEAD already passed.
#
# Usage:
#   scripts/preflight.sh                       # diff vs origin/main
#   scripts/preflight.sh --files A B ...       # explicit changed-file set
#                                              # (used by .githooks/pre-push)
#   scripts/preflight.sh --force               # ignore exception classes,
#                                              # run the full check set

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

# iamacoffeepot/aether#1156: pin the sha the checks actually run against.
# `stamp_pass` stamps *this* value and refuses to write if HEAD has moved
# since, so a rebase / amend / concurrent op / worktree churn mid-run can't
# leave a stamp attesting a sha the checks never validated.
HEAD_AT_START="$(git rev-parse HEAD)"

force=0
explicit=0
explicit_files=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --files)
            explicit=1
            shift
            while [[ $# -gt 0 && "$1" != --* ]]; do
                explicit_files+=("$1")
                shift
            done
            ;;
        --force)
            force=1
            shift
            ;;
        -h|--help)
            sed -n '2,19p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "preflight: unknown arg: $1" >&2
            exit 2
            ;;
    esac
done

stamp_pass() {
    local now
    now="$(git rev-parse HEAD)"
    if [[ "$now" != "$HEAD_AT_START" ]]; then
        echo "[preflight] HEAD moved during the run ($HEAD_AT_START -> $now) — re-run pre-flight." >&2
        exit 1
    fi
    mkdir -p "$(git rev-parse --git-dir)"
    echo "$HEAD_AT_START $(date -u +%s)" \
        > "$(git rev-parse --git-dir)/aether-preflight-passed"
}

if (( force )); then
    needs_rust=1
    needs_skip=0
    bucket_msg="--force; running full pre-flight"
else
    if (( explicit )); then
        changed=("${explicit_files[@]}")
    else
        if git rev-parse --verify origin/main >/dev/null 2>&1; then
            base=$(git merge-base HEAD origin/main 2>/dev/null \
                || git rev-parse origin/main)
        else
            base=$(git rev-parse HEAD~1 2>/dev/null || git rev-parse HEAD)
        fi
        changed=()
        while IFS= read -r f; do
            [[ -n "$f" ]] && changed+=("$f")
        done < <(git diff --name-only "$base..HEAD")
    fi

    if [[ ${#changed[@]} -eq 0 ]]; then
        echo "[preflight] no changed files; nothing to check."
        stamp_pass
        exit 0
    fi

    docs_pat='^(docs/|.*\.md$)'
    ci_pat='^(\.github/|\.claude/|\.githooks/|scripts/|qodana\.yaml$|qodana\.sarif\.json$|\.mcp\.json$|\.gitignore$|\.gitattributes$|rust-toolchain\.toml$|rustfmt\.toml$|clippy\.toml$)'
    rust_pat='(\.rs$|Cargo\.toml$|Cargo\.lock$|rust-toolchain\.toml$)'

    all_docs=1
    all_ci=1
    any_rust=0
    for f in "${changed[@]}"; do
        [[ "$f" =~ $docs_pat ]] || all_docs=0
        [[ "$f" =~ $ci_pat ]] || all_ci=0
        [[ "$f" =~ $rust_pat ]] && any_rust=1
    done

    needs_skip=0
    needs_rust=0
    bucket_msg=""
    if (( all_docs )); then
        needs_skip=1
        bucket_msg="docs-only change; skipping Rust pre-flight"
    elif (( all_ci )); then
        needs_skip=1
        bucket_msg="CI/repo-config-only change; CI will self-validate"
    elif (( any_rust )); then
        needs_rust=1
        bucket_msg="Rust source / Cargo manifest changed; running full pre-flight"
    else
        needs_skip=1
        bucket_msg="no compile-path change; skipping Rust pre-flight"
    fi
fi

echo "[preflight] $bucket_msg."

if (( needs_skip )); then
    stamp_pass
    exit 0
fi

run_step() {
    local label="$1"
    shift
    echo "  -> $label"
    "$@"
}

run_step "cargo fmt --all -- --check" \
    cargo fmt --all -- --check

run_step "cargo clippy --workspace --all-targets -- -D warnings" \
    cargo clippy --workspace --all-targets -- -D warnings

run_step "cargo doc --workspace --no-deps (rustdoc lints denied)" \
    env RUSTDOCFLAGS="-D rustdoc::redundant_explicit_links -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links" \
    cargo doc --workspace --no-deps

# Wasm32 component cross-build mirrors CI's pre-test step. `xtask dist`
# discovers component crates structurally (cdylib lib + example targets
# gated on the `aether-actor` dep, issue #439) and cross-builds each
# per-package. `--no-bins` keeps the preflight fast path wasm-only.
run_step "cargo xtask dist --no-bins (component wasm cross-build)" \
    cargo xtask dist --no-bins

# Slowest Rust step. The whole suite runs at full parallelism — the
# `aether-substrate-bundle` integration tests that fork real substrates carry
# a generous per-test slow-timeout in `.config/nextest.toml`, so concurrent
# fork contention stretches their wall-clock without tripping the steady-state
# cap. AETHER_REQUIRE_RUNTIME=1 mirrors CI so a missing wasm artifact fails
# loudly rather than skipping silently.
run_step "cargo nextest run (workspace, parallel)" \
    env AETHER_REQUIRE_RUNTIME=1 \
    cargo nextest run --workspace --all-features --profile ci

# Qodana mirrors CI's required scan locally, so a passing pre-flight covers
# the full `CI pass` set rather than skipping the one gate cargo can't run.
# It runs the pinned `jetbrains/qodana-rust` container named in qodana.yaml —
# the single config source, read with no `--fail-threshold` override so the
# gate stays identical to CI — which makes the local verdict match CI modulo
# the diff scope. `--diff-start` scopes the run to the origin/main merge-base,
# matching CI's PR mode: only findings the branch newly introduces count
# toward `failThreshold`, so pre-existing findings don't false-fail locally.
# Runs the container as root (`-u root`, matching ci.yml) so qodana.yaml's
# `bootstrap` can apt-get/rustup in-container — without it the bootstrap
# fails to write apt's lists as the host user (Qodana exit 100). Needs
# Docker. Fail loud if origin/main is absent rather than scanning the whole
# tree (which would re-flag the existing backlog).
qodana_base="$(git merge-base HEAD origin/main 2>/dev/null || git rev-parse origin/main 2>/dev/null || true)"
if [[ -z "$qodana_base" ]]; then
    echo "[preflight] origin/main not found — cannot diff-scope qodana." >&2
    echo "[preflight] run 'git fetch origin main' and retry." >&2
    exit 1
fi
run_step "qodana scan (diff-scoped to origin/main merge-base)" \
    qodana scan --diff-start "$qodana_base" -u root

stamp_pass
echo "[preflight] OK."
