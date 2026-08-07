#!/usr/bin/env bash
# Unified PostToolUse hook — don't-dirty-main tripwire for both agents.
# Covers Claude's .claude/worktrees/<id> and Codex's .agents/worktrees/*:
# if the client is in a session worktree, check that worktree's main is clean;
# otherwise fall back to Codex's main_root check. Respects AETHER_CODEX_ALLOW_DIRTY_MAIN.

set -u

if [[ "${AETHER_CODEX_ALLOW_DIRTY_MAIN:-}" == "1" ]]; then
    exit 0
fi

input=$(cat)
session_id=$(printf '%s' "$input" | jq -r '.session_id // ""' 2>/dev/null || true)

project_dir="${CLAUDE_PROJECT_DIR:-}"
if [[ -z "$project_dir" ]]; then
    script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
    project_dir=$(cd "$script_dir/.." && pwd)
fi

# If we have a session worktree (Claude or unified), check its main
if [[ -n "$session_id" ]]; then
    # Unified location first, then legacy Claude location
    for wt in "$project_dir/.agents/worktrees/$session_id" "$project_dir/.claude/worktrees/$session_id"; do
        if [[ -e "$wt" ]]; then
            dirty=$(git -C "$project_dir" status --porcelain 2>/dev/null || true)
            if [[ -n "$dirty" ]]; then
                {
                    printf '[worktree boundary] the main worktree is now dirty:\n\n'
                    printf '%s\n' "$dirty" | sed 's/^/    /'
                    printf '\nA session works in its own worktree, not the main checkout.\n'
                    printf 'Revert it now, then redo the work in this session worktree:\n\n'
                    printf '    git -C %s checkout -- <path>\n' "$project_dir"
                    printf '    git -C %s clean -f <path>\n\n' "$project_dir"
                    printf 'The session worktree is %s; /tmp is fine for scratch.\n' "$wt"
                } >&2
                exit 2
            fi
            exit 0
        fi
    done
fi

# Fallback: Codex-style main_root check (works even without session_id)
current_root=$(git rev-parse --show-toplevel 2>/dev/null || true)
[[ -n "$current_root" ]] || exit 0
git_common_dir=$(git -C "$current_root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)
[[ -n "$git_common_dir" ]] || exit 0
main_root=$(dirname "$git_common_dir")
[[ -d "$main_root" ]] || exit 0
dirty=$(git -C "$main_root" status --porcelain 2>/dev/null || true)
[[ -z "$dirty" ]] && exit 0
{
    printf '[worktree boundary] the primary checkout is dirty:\n\n'
    printf '%s\n' "$dirty" | sed 's/^/    /'
    printf '\nAether issue work should happen in .agents/worktrees/issue-<N>, not the primary checkout.\n'
    printf 'Revert the primary checkout change or intentionally bypass this hook with AETHER_CODEX_ALLOW_DIRTY_MAIN=1.\n'
} >&2
exit 2
