#!/usr/bin/env bash
# PostToolUse hook: fail if the primary checkout is left dirty.

set -u

if [[ "${AETHER_CODEX_ALLOW_DIRTY_MAIN:-}" == "1" ]]; then
    exit 0
fi

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
    printf '\n'
    printf 'Aether Codex issue work should happen in .agents/worktrees/issue-<N>, not the primary checkout.\n'
    printf 'Revert the primary checkout change or intentionally bypass this hook with AETHER_CODEX_ALLOW_DIRTY_MAIN=1.\n'
} >&2

exit 2
