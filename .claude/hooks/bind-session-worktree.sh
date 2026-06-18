#!/usr/bin/env bash
# SessionStart hook — give each Claude session its own git worktree.
#
# On every session start this hook ensures the session's worktree exists at
# .claude/worktrees/<session-id> (idempotent: `git worktree add` when absent,
# no-op when already present), locks it against reclamation, and injects an
# instruction to switch into it.
#
# Reads the SessionStart hook payload JSON from stdin (Claude Code hook
# protocol) and emits added context via the hookSpecificOutput.additionalContext
# form. It never blocks — it primes the conversation, it does not gate it. The
# hook cannot change the running session's cwd (fixed at launch), so the session
# stays repo-rooted until it calls EnterWorktree; the don't-dirty-main boundary
# is enforced by separate PreToolUse/PostToolUse hooks.

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

# Ensure the per-session worktree exists, then lock it. The lock makes
# `git worktree remove` refuse to reclaim this worktree while the session is
# live — whether the removal comes from a /sweep run, an ad-hoc cleanup, or
# another session's sweep — so a clean-but-undiverged session worktree is never
# mistaken for abandoned cruft. The lock is released by the SessionEnd hook on a
# clean exit; a crash leaves it locked, which /sweep resolves by probing for a
# live cwd before unlocking. Both the add and the lock are idempotent and never
# fatal — a failure here must not stop the session from starting, and re-locking
# an already-locked worktree is a harmless no-op (the `|| true` absorbs the
# "already locked" status).
if command -v git >/dev/null 2>&1 \
    && git -C "$project_dir" rev-parse --git-dir >/dev/null 2>&1; then
    if [[ ! -e "$worktree_dir" ]]; then
        git -C "$project_dir" worktree add "$worktree_dir" >/dev/null 2>&1 || true
    fi
    git -C "$project_dir" worktree lock "$worktree_dir" \
        --reason "active claude session $session_id" >/dev/null 2>&1 || true
fi

# Emit the SessionStart added-context JSON for the text in $1, built safely with
# jq so the body is escaped correctly.
emit_context() {
    jq -n --arg ctx "$1" \
        '{hookSpecificOutput: {hookEventName: "SessionStart", additionalContext: $ctx}}'
}

emit_context "$(printf 'This session has its own git worktree at:\n\n    %s\n\nFirst action: switch into it by calling the EnterWorktree tool with that path (it already exists — this hook created it), so your edits and commands land in the worktree, never the main checkout.' "$worktree_dir")"
exit 0
