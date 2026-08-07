#!/usr/bin/env bash
# SessionStart hook: best-effort Codex session worktree preparation.
#
# A hook subprocess cannot change the parent Codex process cwd. When Codex
# provides a stable thread/session id, this script creates a detached worktree
# under .agents/worktrees/codex-<id> and emits model-visible guidance in the
# same additionalContext shape used by the Claude hook. If Codex ignores that
# output, the script is still harmless: it either created a usable worktree or
# stayed silent when no stable id was present.

set -u

input=$(cat)

current_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
git_common_dir=$(git -C "$current_root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)
if [[ -n "$git_common_dir" ]]; then
    main_root=$(dirname "$git_common_dir")
else
    main_root="$current_root"
fi

json_value() {
    local filter="$1"
    if command -v jq >/dev/null 2>&1; then
        printf '%s' "$input" | jq -r "$filter // empty" 2>/dev/null || true
    fi
}

session_key=$(
    json_value '.session_id // .sessionId // .thread_id // .threadId // .conversation_id // .conversationId // .id // .thread.id // .conversation.id'
)

if [[ -z "$session_key" ]]; then
    session_key="${CODEX_SESSION_ID:-${CODEX_THREAD_ID:-${CODEX_CONVERSATION_ID:-}}}"
fi

if [[ -z "$session_key" ]]; then
    exit 0
fi

safe_key=$(printf '%s' "$session_key" | tr -cs 'A-Za-z0-9._-' '-' | sed 's/^-//; s/-$//' | cut -c 1-80)
if [[ -z "$safe_key" ]]; then
    exit 0
fi

worktree_dir="$main_root/.agents/worktrees/codex-$safe_key"

if command -v git >/dev/null 2>&1 \
    && git -C "$main_root" rev-parse --git-dir >/dev/null 2>&1; then
    if [[ ! -e "$worktree_dir" ]]; then
        mkdir -p "$(dirname "$worktree_dir")"
        git -C "$main_root" worktree add --detach "$worktree_dir" origin/main >/dev/null 2>&1 \
            || git -C "$main_root" worktree add --detach "$worktree_dir" HEAD >/dev/null 2>&1 \
            || true
    fi
fi

if [[ -e "$worktree_dir" ]] && command -v jq >/dev/null 2>&1; then
    context=$(cat <<EOF
This Codex session has a prepared git worktree at:

    $worktree_dir

A hook cannot change the parent Codex process cwd. Use that worktree for repo edits, or use the issue-specific worktree required by the Aether implement skill for planned issue work.
EOF
)
    jq -n --arg ctx "$context" \
        '{hookSpecificOutput: {hookEventName: "SessionStart", additionalContext: $ctx}}'
fi

exit 0
