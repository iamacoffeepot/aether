#!/usr/bin/env bash
# Local attestation producer: runs the CI-equivalent checks under `witness`,
# emitting one signed in-toto attestation per step, bound to the current commit.
#
# The verifier (scripts/attest-verify.sh, run by the attest-verify workflow)
# resolves each attestation's signing key against the PR author's
# `github.com/<author>.keys` and confirms the author is a write-collaborator, so
# the cheap signed proof can stand in for re-running the expensive checks on the
# runner.
#
# Signing reuses the author's existing SSH key. `witness`'s file signer reads
# PEM; the OpenSSH key is repacked to PKCS8 with `sshpk-conv` (the published
# `sshpk` tool) into tmpfs, used to sign, and shredded — the same public key is
# already on the author's GitHub account, so nothing new is registered.
#
# Each step runs in a fresh clone of HEAD so witness's git attestor has a real
# repository to read: it binds the step to the commit (`commithash`) and records
# the tree status, so the verifier can prove each check ran on a clean checkout
# of the PR head — not just a self-declared sha. Running in a clone also gives
# qodana the history it needs to diff-scope.
#
# On success the same `.git/aether-preflight-passed` stamp `scripts/preflight.sh`
# writes is stamped here too: attest runs a superset of preflight's checks on the
# committed HEAD, so the stamp is earned, and the pre-push hooks
# (`.githooks/pre-push`, `.claude/hooks/check-pre-push.sh`) then skip a redundant
# pre-flight — including this script's own `--publish` push of the attestation
# ref.
#
# PII: only the git attestor is added (`-a git`) — it records commit metadata
# (author / committer / remote), already public in the PR, and no machine, env,
# or username data; the environment attestor stays off. Each step's stdout/stderr
# is redirected to a local log so the command-run attestor records no `$HOME`
# paths, and an external CARGO_TARGET_DIR keeps build output out of the clone the
# git attestor walks.
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

# Canonical check set (CANONICAL_STEPS / canonical_cmd), shared with the verifier.
# shellcheck source=scripts/checks.sh
source "$ROOT/scripts/checks.sh"

# shellcheck disable=SC2153
SIGN_KEY="${AETHER_ATTEST_KEY:-$HOME/.ssh/id_ed25519}"

publish=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --publish) publish=1; shift ;;
        -h|--help) sed -n '2,36p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "attest: unknown arg: $1" >&2; exit 2 ;;
    esac
done

die() { echo "[attest] $*" >&2; exit 1; }

command -v witness    >/dev/null 2>&1 || die "witness not on PATH (go install github.com/in-toto/witness@latest)"
command -v sshpk-conv >/dev/null 2>&1 || die "sshpk-conv not on PATH (npm i -g sshpk)"
[[ -f "$SIGN_KEY" ]] || die "signing key not found: $SIGN_KEY"

# The attestation must reflect a committed tree, so refuse a dirty worktree up
# front; the git attestor and verifier enforce the clean-tree claim per step too.
[[ -z "$(git status --porcelain)" ]] || die "worktree is dirty; commit or stash before attesting"
HEAD_SHA="$(git rev-parse HEAD)"
# Computed here in the real checkout (which has origin/main) for qodana's
# diff-scope; the merge-base is an ancestor of HEAD, so it is present in the
# clone below.
QODANA_BASE="$(git merge-base HEAD origin/main)"

# Keep build artifacts out of the clone the git attestor walks, so its tree
# status stays clean and the material walk stays source-only. Persisted across
# runs so the producer stays incrementally warm.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/aether-attest-target}"
mkdir -p "$CARGO_TARGET_DIR"

# A fresh clone of HEAD with a real `.git`, under a Docker-shared path ($HOME,
# for qodana's container). Every step runs here: witness's git attestor reads
# this repo to bind each step to the commit and record a clean tree, and qodana
# gets the history it needs to diff-scope. Removed on exit.
RUNDIR="$(mktemp -d "$HOME/.cache/aether-attest-run.XXXXXX")"
RUN="$RUNDIR/scan"
git clone --quiet "$ROOT" "$RUN"
git -C "$RUN" -c advice.detachedHead=false checkout --quiet "$HEAD_SHA"

# Ephemeral PKCS8 signing key in a private temp dir; shredded on exit.
KEYDIR="$(mktemp -d)"
LOGDIR="$(mktemp -d)"
cleanup() {
    [[ -f "$KEYDIR/key.pem" ]] && { command -v shred >/dev/null 2>&1 && shred -u "$KEYDIR/key.pem" 2>/dev/null || rm -f "$KEYDIR/key.pem"; }
    rm -rf "$KEYDIR" "$LOGDIR" "$RUNDIR"
}
trap cleanup EXIT
( umask 077; sshpk-conv -p -t pkcs8 -f "$SIGN_KEY" > "$KEYDIR/key.pem" 2>/dev/null ) \
    || die "key conversion failed (encrypted key? only unencrypted ed25519 supported)"

OUTDIR="$(git rev-parse --git-dir)/aether-attestations/$HEAD_SHA"
rm -rf "$OUTDIR"; mkdir -p "$OUTDIR"

# attest_step runs one canonical check (scripts/checks.sh) under witness.
attest_step() {
    local name="$1"; shift
    echo "[attest] -> $name"
    # Reset the clone to a pristine HEAD checkout so the git attestor records a
    # clean tree (build output is in the external CARGO_TARGET_DIR; this only
    # clears stray untracked files such as qodana's .qodana/).
    git -C "$RUN" reset -q --hard HEAD
    git -C "$RUN" clean -qfd
    # Run inside the clone (CWD = the real-`.git` checkout) so `-a git` binds the
    # step to the commit and records the tree status. `-a git` adds only the git
    # attestor — no environment attestor, so no machine/env PII. stdout/stderr go
    # to a tmpfs log whose path rides in the environment, never a `cmd` arg, so
    # the command-run attestor records no machine path while the command stays
    # visible.
    if ! ( cd "$RUN" && ATTEST_STEP_LOG="$LOGDIR/$name.log" \
            witness run --step "$name" -a git \
            --signer-file-key-path "$KEYDIR/key.pem" \
            -o "$OUTDIR/$name.json" \
            -- bash -c 'exec >"$ATTEST_STEP_LOG" 2>&1; exec "$@"' attest "$@" ); then
        echo "[attest] step '$name' FAILED:" >&2
        tail -40 "$LOGDIR/$name.log" >&2 || true
        die "attestation aborted at step '$name'"
    fi
}

# Run each canonical step in order, adding the per-run wrappers each needs.
# qodana runs last: its .qodana/ output is untracked (not gitignored), so a step
# after it would see a dirty tree; the clone gives it the full history it needs
# to diff-scope to the origin/main merge-base.
for step in $CANONICAL_STEPS; do
    read -ra cmd <<< "$(canonical_cmd "$step")"
    case "$step" in
        doc)    attest_step "$step" env RUSTDOCFLAGS="-D rustdoc::redundant_explicit_links -D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links" "${cmd[@]}" ;;
        test)   attest_step "$step" env AETHER_REQUIRE_RUNTIME=1 "${cmd[@]}" ;;
        qodana) attest_step "$step" "${cmd[@]}" --diff-start "$QODANA_BASE" -u root ;;
        *)      attest_step "$step" "${cmd[@]}" ;;
    esac
done

echo "[attest] OK — $(ls "$OUTDIR"/*.json | wc -l | tr -d ' ') signed attestations for $HEAD_SHA"
echo "[attest] $OUTDIR"

# All canonical checks passed on this committed HEAD, so stamp the pre-flight
# marker the pre-push hooks read — the same file scripts/preflight.sh writes.
# attest ran a superset of preflight's checks on HEAD_SHA, so the stamp is
# earned; writing it before the publish push below also lets this script's own
# `git push` of the attestation ref pass the hook's stamp short-circuit instead
# of re-triggering pre-flight. Refuse to stamp if HEAD moved since the run
# started, mirroring preflight's stamp_pass guard.
now="$(git rev-parse HEAD)"
[[ "$now" == "$HEAD_SHA" ]] || die "HEAD moved during the run ($HEAD_SHA -> $now) — re-run attest."
echo "$HEAD_SHA $(date -u +%s)" > "$(git rev-parse --git-dir)/aether-preflight-passed"
echo "[attest] stamped pre-flight marker for $HEAD_SHA"

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
