#!/usr/bin/env bash
# Unified SessionStart hook — works for Claude (Muse), Muse, and Codex.
# Creates a detached worktree under .agents/worktrees/<key> so all agents
# share one worktree root (AGENTS.md: issue work in .agents/worktrees/issue-<N>).
# Muse (tbh), Claude session_id and Codex thread_id/session_id are all accepted.

set -u

input=$(cat)

# Project root — try Claude, Muse, and generic envs
project_dir="${CLAUDE_PROJECT_DIR:-${MUSE_PROJECT_DIR:-${TBH_PROJECT_DIR:-}}}"
if [[ -z "$project_dir" ]]; then
    script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
    project_dir=$(cd "$script_dir/.." && pwd)
fi
# Fallback for Codex/Muse: git common dir
if [[ ! -e "$project_dir/.git" && ! -e "$project_dir/.claude" && ! -e "$project_dir/.codex" ]]; then
    current_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
    git_common_dir=$(git -C "$current_root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)
    if [[ -n "$git_common_dir" ]]; then
        project_dir=$(dirname "$git_common_dir")
    else
        project_dir="$current_root"
    fi
fi

json_value() {
    local filter="$1"
    if command -v jq >/dev/null 2>&1; then
        printf '%s' "$input" | jq -r "$filter // empty" 2>/dev/null || true
    fi
}

# Try Claude/Muse/Codex fields first, then env
session_key=$(json_value '.session_id // .sessionId // .muse_session_id // .tbh_session_id // .thread_id // .threadId // .conversation_id // .conversationId // .id // .thread.id // .conversation.id // .session.id')
if [[ -z "$session_key" ]]; then
    session_key="${MUSE_SESSION_ID:-${TBH_SESSION_ID:-${CODEX_SESSION_ID:-${CODEX_THREAD_ID:-${CODEX_CONVERSATION_ID:-}}}}}"
fi
if [[ -z "$session_key" ]]; then
    session_key="${CLAUDE_SESSION_ID:-}"
fi
if [[ -z "$session_key" ]]; then
    session_key=$(json_value '.session_id // .muse_session_id // .tbh_session_id')
fi
if [[ -z "$session_key" ]]; then
    case "$(uname -s)" in
        Darwin)
            if command -v uuidgen >/dev/null 2>&1; then
                session_key=$(uuidgen 2>/dev/null | tr 'A-Z' 'a-z')
            fi
            ;;
        Linux)
            if [[ -r /proc/sys/kernel/random/uuid ]]; then
                session_key=$(tr 'A-Z' 'a-z' < /proc/sys/kernel/random/uuid 2>/dev/null)
            elif command -v uuidgen >/dev/null 2>&1; then
                session_key=$(uuidgen 2>/dev/null | tr 'A-Z' 'a-z')
            fi
            ;;
    esac
    if [[ -z "$session_key" ]]; then
        if command -v openssl >/dev/null 2>&1; then
            hex=$(openssl rand -hex 16 2>/dev/null)
            if [[ -n "$hex" ]]; then
                session_key=$(printf '%s' "$hex" | sed -E 's/(.{8})(.{4})(.{4})(.{4})(.{12})/\1-\2-\3-\4-\5/')
            fi
        fi
        if [[ -z "$session_key" ]]; then
            hex=$(od -An -tx1 -N16 /dev/urandom 2>/dev/null | tr -d ' \n' | tr 'A-Z' 'a-z')
            if [[ -n "$hex" ]]; then
                session_key=$(printf '%s' "$hex" | sed -E 's/(.{8})(.{4})(.{4})(.{4})(.{12})/\1-\2-\3-\4-\5/')
            fi
        fi
    fi
    if [[ -z "$session_key" ]]; then
        exit 0
    fi
fi

safe_key=$(printf '%s' "$session_key" | tr -cs 'A-Za-z0-9._-' '-' | sed 's/^-//; s/-$//' | cut -c 1-80)
if [[ -z "$safe_key" ]]; then
    exit 0
fi

# Unified location: .agents/worktrees for all agents (per AGENTS.md)
# Claude's old .claude/worktrees/<id> is deprecated; we still create a symlink
# for backwards compat if something expects it.
worktree_dir="$project_dir/.agents/worktrees/$safe_key"
legacy_dir="$project_dir/.claude/worktrees/$safe_key"

if command -v git >/dev/null 2>&1 && git -C "$project_dir" rev-parse --git-dir >/dev/null 2>&1; then
    if [[ ! -e "$worktree_dir" ]]; then
        mkdir -p "$(dirname "$worktree_dir")"
        git -C "$project_dir" worktree add --detach "$worktree_dir" origin/main >/dev/null 2>&1 \
            || git -C "$project_dir" worktree add --detach "$worktree_dir" HEAD >/dev/null 2>&1 || true
        # Lock for Claude sessions to prevent sweep reclamation
        git -C "$project_dir" worktree lock "$worktree_dir" --reason "active session $safe_key" >/dev/null 2>&1 || true
    fi
    # Back-compat symlink for old Claude path
    if [[ -e "$worktree_dir" && ! -e "$legacy_dir" ]]; then
        mkdir -p "$(dirname "$legacy_dir")"
        ln -sfn "$worktree_dir" "$legacy_dir" 2>/dev/null || true
    fi
fi

if [[ -e "$worktree_dir" ]] && command -v jq >/dev/null 2>&1; then
    context=$(cat <<EOF
This session has a prepared git worktree at:

    $worktree_dir

A hook cannot change the parent process cwd. To work isolated like other platforms, cd into that worktree for repo edits (e.g. cd "$worktree_dir"), or use the issue-specific worktree required by the Aether implement skill (.agents/worktrees/issue-<N>). Future sessions will auto-enter when the agent respects this context.
EOF
)
    jq -n --arg ctx "$context" '{hookSpecificOutput: {hookEventName: "SessionStart", additionalContext: $ctx}}'
fi

exit 0
