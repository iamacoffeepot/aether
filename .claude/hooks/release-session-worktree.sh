#!/usr/bin/env bash
# SessionEnd hook — release the git lock this session's worktree was given at
# bind time, so a later /sweep can reclaim the worktree once the session is gone.
#
# The bind hook (bind-session-worktree.sh) locks .claude/worktrees/<session-id>
# so a `git worktree remove` refuses to reclaim a live session's worktree. On a
# clean exit this hook unlocks it; a crash that skips SessionEnd leaves the
# worktree locked, which /sweep resolves by probing for a live cwd before
# unlocking.
#
# Reads the SessionEnd hook payload JSON from stdin (Claude Code hook protocol).
# Silent and never fatal — unlocking is best-effort cleanup, not a gate.

set -u

input=$(cat)

# Project root: CLAUDE_PROJECT_DIR is exported for hooks; fall back to the hook
# script's own location (two levels up from .claude/hooks/) when it is unset.
project_dir="${CLAUDE_PROJECT_DIR:-}"
if [[ -z "$project_dir" ]]; then
    script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
    project_dir=$(cd "$script_dir/../.." && pwd)
fi

session_id=$(printf '%s' "$input" | jq -r '.session_id // ""')

# Without a session id there is nothing to key the worktree on — stay silent.
if [[ -z "$session_id" ]]; then
    exit 0
fi

worktree_dir="$project_dir/.claude/worktrees/$session_id"

# Release the lock so /sweep can reclaim the worktree. Idempotent and never
# fatal: unlocking an already-unlocked worktree (or one git no longer tracks)
# is a harmless no-op absorbed by the `|| true`.
if command -v git >/dev/null 2>&1 \
    && git -C "$project_dir" rev-parse --git-dir >/dev/null 2>&1; then
    git -C "$project_dir" worktree unlock "$worktree_dir" >/dev/null 2>&1 || true
fi

exit 0
