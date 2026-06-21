#!/usr/bin/env bash
# Local attestation producer: runs the CI-equivalent checks under `witness`,
# emitting one signed in-toto attestation per step, bound to the current commit.
#
# The verifier (a GitHub Action, separate change) resolves each attestation's
# signing key against the PR author's `github.com/<author>.keys` and confirms
# the author is a write-collaborator, so the cheap signed proof can stand in for
# re-running the expensive checks on the runner.
#
# Signing reuses the author's existing SSH key. `witness`'s file signer reads
# PEM; the OpenSSH key is repacked to PKCS8 with `sshpk-conv` (the published
# `sshpk` tool) into tmpfs, used to sign, and shredded — the same public key is
# already on the author's GitHub account, so nothing new is registered.
#
# PII: the witness environment attestor (username / hostname / env vars) is
# dropped (`-a ''`); each step's stdout/stderr is redirected to a local log so
# the command-run attestor records no `$HOME` paths; materials are repo-relative
# and the only product is the commit-binding subject. Build output is kept out
# of the walked tree via an external CARGO_TARGET_DIR, so materials stay
# source-only.
#
# Prerequisites on PATH:
#   witness     — go install github.com/in-toto/witness@latest
#   sshpk-conv  — npm i -g sshpk
#
# Usage:
#   scripts/attest.sh                 # attest the current (clean) HEAD locally
#   scripts/attest.sh --publish       # also push the proofs to the side ref
#   AETHER_ATTEST_KEY=~/.ssh/id_x scripts/attest.sh

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

# shellcheck disable=SC2153
SIGN_KEY="${AETHER_ATTEST_KEY:-$HOME/.ssh/id_ed25519}"

publish=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --publish) publish=1; shift ;;
        -h|--help) sed -n '2,28p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "attest: unknown arg: $1" >&2; exit 2 ;;
    esac
done

die() { echo "[attest] $*" >&2; exit 1; }

command -v witness    >/dev/null 2>&1 || die "witness not on PATH (go install github.com/in-toto/witness@latest)"
command -v sshpk-conv >/dev/null 2>&1 || die "sshpk-conv not on PATH (npm i -g sshpk)"
[[ -f "$SIGN_KEY" ]] || die "signing key not found: $SIGN_KEY"

# The attestation must reflect a committed tree: each step binds to HEAD via a
# product subject derived from the commit sha (below), and a dirty worktree
# would let the recorded materials diverge from that commit.
[[ -z "$(git status --porcelain)" ]] || die "worktree is dirty; commit or stash before attesting"
HEAD_SHA="$(git rev-parse HEAD)"

# Keep build artifacts out of the witnessed working tree so the material
# attestor walks source only (fast, no target/ pollution). Persisted across
# runs so the producer stays incrementally warm.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/aether-attest-target}"
mkdir -p "$CARGO_TARGET_DIR"

# Ephemeral PKCS8 signing key in a private temp dir; shredded on exit.
KEYDIR="$(mktemp -d)"
LOGDIR="$(mktemp -d)"
# The commit-binding subject: a tree-local file each step's witnessed command
# fills with the head sha, recorded by the product attestor. Its digest is
# sha256(head_sha), which the verifier reconstructs — so binding needs no git
# metadata and works in a linked worktree. Removed after each step and on exit.
SUBJECT="$ROOT/.aether-attest-subject"
cleanup() {
    [[ -f "$KEYDIR/key.pem" ]] && { command -v shred >/dev/null 2>&1 && shred -u "$KEYDIR/key.pem" 2>/dev/null || rm -f "$KEYDIR/key.pem"; }
    rm -f "$SUBJECT"
    rm -rf "$KEYDIR" "$LOGDIR"
}
trap cleanup EXIT
( umask 077; sshpk-conv -p -t pkcs8 -f "$SIGN_KEY" > "$KEYDIR/key.pem" 2>/dev/null ) \
    || die "key conversion failed (encrypted key? only unencrypted ed25519 supported)"

OUTDIR="$(git rev-parse --git-dir)/aether-attestations/$HEAD_SHA"
rm -rf "$OUTDIR"; mkdir -p "$OUTDIR"

# Canonical check set. MUST mirror scripts/preflight.sh and .github/workflows/
# ci.yml — the attestation is only meaningful if it runs the same gates CI does.
# (Unifying these onto one shared definition is a follow-up.)
attest_step() {
    local name="$1"; shift
    echo "[attest] -> $name"
    # One witnessed shell does both binding and PII-scrubbing before exec'ing the
    # real command:
    #   * writes the head sha to the tree-local subject file, scoped as the only
    #     product (--attestor-product-include-glob) — a commit-bound subject that
    #     needs no git metadata, so this works in a linked worktree.
    #   * redirects the step's stdout/stderr to a tmpfs log. The log path rides in
    #     the environment, never as a `cmd` arg, so the command-run attestor's
    #     verbatim `cmd` carries no machine-identifying path while the real
    #     command stays visible.
    # `-a ''` drops the environment attestor (PII) and the git attestor (unused).
    if ! ATTEST_SHA="$HEAD_SHA" ATTEST_SUBJECT="$SUBJECT" ATTEST_STEP_LOG="$LOGDIR/$name.log" \
            witness run \
            --step "$name" \
            -a '' \
            --attestor-product-include-glob "$(basename "$SUBJECT")" \
            --signer-file-key-path "$KEYDIR/key.pem" \
            -o "$OUTDIR/$name.json" \
            -- bash -c 'printf %s "$ATTEST_SHA" > "$ATTEST_SUBJECT"; exec >"$ATTEST_STEP_LOG" 2>&1; exec "$@"' attest "$@"; then
        rm -f "$SUBJECT"
        echo "[attest] step '$name' FAILED:" >&2
        tail -40 "$LOGDIR/$name.log" >&2 || true
        die "attestation aborted at step '$name'"
    fi
    rm -f "$SUBJECT"
}

attest_step fmt    cargo fmt --all -- --check
attest_step clippy cargo clippy --workspace --all-targets -- -D warnings
attest_step doc    env RUSTDOCFLAGS="-D rustdoc::redundant_explicit_links -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links" cargo doc --workspace --no-deps
attest_step dist   cargo xtask dist --no-bins
attest_step test   env AETHER_REQUIRE_RUNTIME=1 cargo nextest run --workspace --all-features --profile ci
attest_step qodana qodana scan --diff-start "$(git merge-base HEAD origin/main)" -u root

echo "[attest] OK — $(ls "$OUTDIR"/*.json | wc -l | tr -d ' ') signed attestations for $HEAD_SHA"
echo "[attest] $OUTDIR"

# Publish onto a side ref keyed by the commit sha — refs/attestations/<sha>.
# The attestations become git objects pushed under their own ref namespace; the
# working tree, index, and every branch are untouched (a private index builds
# the tree, never $GIT_INDEX_FILE's default). The verifier fetches this ref by
# the PR's head sha. Re-publishing the same sha is idempotent (force is safe on
# a content-keyed ref).
if (( publish )); then
    ref="refs/attestations/$HEAD_SHA"
    index="$(mktemp)"
    GIT_INDEX_FILE="$index" git read-tree --empty
    for f in "$OUTDIR"/*.json; do
        blob="$(git hash-object -w "$f")"
        GIT_INDEX_FILE="$index" git update-index --add --cacheinfo "100644,$blob,$(basename "$f")"
    done
    tree="$(GIT_INDEX_FILE="$index" git write-tree)"
    rm -f "$index"
    commit="$(git commit-tree "$tree" -m "attestations for $HEAD_SHA")"
    git push --force origin "$commit:$ref"
    echo "[attest] published $ref"
fi
