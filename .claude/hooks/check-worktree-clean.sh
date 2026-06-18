#!/usr/bin/env bash
# PostToolUse hook — the don't-dirty-main tripwire for Bash commands.
#
# A Bash command can dirty the main checkout in too many open-ended ways
# (`> file`, `sed -i`, `git checkout`, applying a patch) to gate statically
# before it runs, so this is the detect half of the don't-dirty-main rule:
# after the command runs it checks `git status --porcelain` on the main
# worktree. A non-empty result means the command left a tracked file changed
# (or dropped non-ignored scratch) in the main checkout, so the hook returns
# exit 2 naming the dirtied paths and the revert corrective. It cannot un-run
# the command, but it reliably detects the violation and drives the fix —
# false positives are near-zero because a session works in its own worktree and
# gitignored scratch (.claude/worktrees) never shows here.
#
# Reads the PostToolUse tool-call JSON from stdin (Claude Code hook protocol).
# Exits 0 when main is clean, 2 (with a stderr reason) when it is dirty. Fails
# open (exit 0) when the session has no worktree, so an unbound session is never
# flagged and enforcement degrades to a no-op.

set -u

input=$(cat)
session_id=$(printf '%s' "$input" | jq -r '.session_id // ""')

# Project root: CLAUDE_PROJECT_DIR is exported for hooks; fall back to the hook
# script's own location (two levels up from .claude/hooks/) when it is unset.
project_dir="${CLAUDE_PROJECT_DIR:-}"
if [[ -z "$project_dir" ]]; then
    script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
    project_dir=$(cd "$script_dir/../.." && pwd)
fi

# Without a session id there is nothing to key the worktree on — fail open.
[[ -z "$session_id" ]] && exit 0

worktree_dir="$project_dir/.claude/worktrees/$session_id"
[[ -e "$worktree_dir" ]] || exit 0

command -v git >/dev/null 2>&1 || exit 0
git -C "$project_dir" rev-parse --git-dir >/dev/null 2>&1 || exit 0

dirty=$(git -C "$project_dir" status --porcelain 2>/dev/null)
[[ -z "$dirty" ]] && exit 0

{
    printf '[worktree boundary] the main worktree is now dirty:\n\n'
    printf '%s\n' "$dirty" | sed 's/^/    /'
    printf '\n'
    printf 'A session works in its own worktree, not the main checkout. The last Bash\n'
    printf 'command changed a tracked file in the main checkout (%s).\n\n' "$project_dir"
    printf 'Revert it now, then redo the work in this session'\''s worktree:\n\n'
    printf '    git -C %s checkout -- <path>      # tracked changes\n' "$project_dir"
    printf '    git -C %s clean -f <path>         # untracked scratch\n\n' "$project_dir"
    printf 'The session worktree is .claude/worktrees/%s; /tmp is fine for scratch.\n' "$session_id"
} >&2

exit 2
