#!/usr/bin/env bash
# Commit a staged dogfood run directory to the orphan `dogfood/evidence`
# branch at `evidence/<issue>/<run-id>/` and push it (issue 2967).
#
# The branch is orphan so it never bloats a checkout of main and can be
# history-squashed on a /sweep cadence once issues close. Every git
# operation runs inside a throwaway worktree checked out into a mktemp
# dir, so the invoking checkout is never touched; the worktree is removed
# on exit via a trap. A concurrent push (another run racing to the same
# branch) is handled by up to three fetch-and-reapply retries.
#
# Usage:
#   scripts/dogfood-evidence.sh <staged-run-dir> <issue> <run-id>
#
# <staged-run-dir> is the local directory holding this run's evidence
# (rollup.json, judged-frame.png, solution/…); <issue> and <run-id> form the
# `evidence/<issue>/<run-id>/` path on the branch. Bash + git/coreutils
# only, no external deps.

set -euo pipefail

staged_dir="${1:?usage: dogfood-evidence.sh <staged-run-dir> <issue> <run-id>}"
issue="${2:?usage: dogfood-evidence.sh <staged-run-dir> <issue> <run-id>}"
run_id="${3:?usage: dogfood-evidence.sh <staged-run-dir> <issue> <run-id>}"

branch=dogfood/evidence
remote=origin
max_retries=3

if [[ ! -d "$staged_dir" ]]; then
  echo "dogfood-evidence: staged run dir not found: $staged_dir" >&2
  exit 1
fi

repo_root="$(git rev-parse --show-toplevel)"

# A detached temp worktree keeps every branch operation off the invoking
# checkout. The trap removes it (and prunes the registration) whatever the
# exit path.
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/dogfood-evidence.XXXXXX")"
cleanup() {
  git -C "$repo_root" worktree remove --force "$work_dir" 2>/dev/null || true
  rm -rf "$work_dir" 2>/dev/null || true
  git -C "$repo_root" worktree prune 2>/dev/null || true
}
trap cleanup EXIT

git -C "$repo_root" worktree add --detach "$work_dir" >/dev/null

# Check out the branch if the remote already has it, otherwise bootstrap a
# fresh orphan with a clean tree.
git -C "$work_dir" fetch "$remote" "$branch" 2>/dev/null || true
if git -C "$work_dir" rev-parse --verify --quiet "refs/remotes/$remote/$branch" >/dev/null; then
  git -C "$work_dir" switch -c "$branch" "$remote/$branch" >/dev/null
else
  git -C "$work_dir" switch --orphan "$branch" >/dev/null
  git -C "$work_dir" rm -rf . >/dev/null 2>&1 || true
fi

dest="$work_dir/evidence/$issue/$run_id"
mkdir -p "$dest"
cp -R "$staged_dir/." "$dest/"

git -C "$work_dir" add "evidence/$issue/$run_id"
if git -C "$work_dir" diff --cached --quiet; then
  echo "dogfood-evidence: nothing to commit for evidence/$issue/$run_id (already present and identical)"
  exit 0
fi

# Commit with an explicit bot identity: the CI runner has no git
# user.name/email configured, and commit dies on "empty ident name"
# without one. -c scoped so nothing leaks into the invoking checkout's
# config when the script runs on a dev box.
git -C "$work_dir" \
  -c user.name="github-actions[bot]" \
  -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
  commit -q -m "evidence: dogfood run $issue/$run_id"

# Push with fetch-and-reapply retries. A rejected push means another run
# advanced the branch between our fetch and our push; rebase our single
# evidence commit onto the fresh tip and try again. Paths are unique per
# run (issue/run-id), so the rebase never conflicts.
attempt=0
while true; do
  if git -C "$work_dir" push "$remote" "$branch:$branch"; then
    echo "dogfood-evidence: pushed evidence/$issue/$run_id to $branch"
    break
  fi
  attempt=$((attempt + 1))
  if [[ "$attempt" -ge "$max_retries" ]]; then
    echo "dogfood-evidence: push rejected after $max_retries attempts" >&2
    exit 1
  fi
  echo "dogfood-evidence: push rejected, refetching and reapplying (attempt $attempt/$max_retries)" >&2
  git -C "$work_dir" fetch "$remote" "$branch"
  # Rebase recommits, so it needs the same explicit identity as the commit.
  git -C "$work_dir" \
    -c user.name="github-actions[bot]" \
    -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
    rebase "$remote/$branch"
done
