#!/usr/bin/env bash
# PreToolUse hook — the edit half of the don't-dirty-main worktree boundary.
#
# A session works in its own worktree (.claude/worktrees/<session-id>, created
# by the SessionStart bind hook). This gate runs before the edit tools
# (Edit/Write/MultiEdit/NotebookEdit), which declare their target up front: it
# resolves the target file_path to absolute and asks to confirm a write that
# would dirty the main checkout. /tmp scratch, the session's own worktree, and
# any aether-gitignored path are allowed silently — a gitignored path never
# shows in `git status`, matching the PostToolUse tripwire check-worktree-clean.sh.
#
# Reads the PreToolUse tool-call JSON from stdin (Claude Code hook protocol).
# A crossing is surfaced as an ask-to-confirm prompt: the hook prints a
# `permissionDecision: "ask"` decision on stdout and exits 0, so the tool runs
# only if the operator confirms. Fails open (plain exit 0, no prompt) when the
# session has no worktree, so an unbound session is never gated.

set -u

input=$(cat)
session_id=$(printf '%s' "$input" | jq -r '.session_id // ""')
tool_name=$(printf '%s' "$input" | jq -r '.tool_name // ""')
file_path=$(printf '%s' "$input" | jq -r '.tool_input.file_path // ""')

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
# No worktree for this session — fail open, so an unbound session is never gated.
[[ -e "$worktree_dir" ]] || exit 0

# Only the edit tools, which declare their target up front.
case "$tool_name" in
    Edit | Write | MultiEdit | NotebookEdit) ;;
    *) exit 0 ;;
esac
[[ -n "$file_path" ]] || exit 0

# Surface a boundary crossing as a PreToolUse ask-to-confirm decision: print the
# hook JSON carrying the reason on stdout and exit 0, so the tool runs only if
# the operator confirms.
emit_ask() {
    jq -nc --arg r "$1" \
        '{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:"ask",permissionDecisionReason:$r}}'
    exit 0
}

# Resolve the target to an absolute, lexically-normalized path (the file may
# not exist yet for a Write, so canonicalize . / .. segments without touching
# the filesystem). A relative path resolves against the main project root.
normalize_path() {
    local path="$1"
    [[ "$path" == /* ]] || path="$project_dir/$path"
    local parts=()
    local seg
    local IFS='/'
    for seg in $path; do
        case "$seg" in
            '' | '.') ;;
            '..') ((${#parts[@]})) && unset 'parts[${#parts[@]}-1]' ;;
            *) parts+=("$seg") ;;
        esac
    done
    ((${#parts[@]})) && printf '/%s' "${parts[@]}" || printf '/'
}

target=$(normalize_path "$file_path")
session_worktree=$(normalize_path "$worktree_dir")
main_root=$(normalize_path "$project_dir")

# Allow temp scratch (/tmp and the macOS temp roots).
case "$target" in
    /tmp/* | /tmp | /private/tmp/* | /var/tmp/* | /var/folders/*) exit 0 ;;
esac
# Allow the session's own worktree. A session reaches the worktrees of agents it
# spawns through dispatch, not by editing them — those agents run outside this
# guardrail (no worktree of their own keyed to this session id, so the hook
# fails open for them) and work in their own worktrees, so the bound session
# itself stays own-worktree.
case "$target" in
    "$session_worktree"/* | "$session_worktree") exit 0 ;;
esac
# Under the main worktree: ask before a write that would actually dirty aether's
# tracked state. A path aether gitignores never shows in `git status` (matching
# the PostToolUse tripwire), so allow it silently — no prompt.
case "$target" in
    "$main_root"/* | "$main_root")
        if command -v git >/dev/null 2>&1 \
            && git -C "$main_root" rev-parse --git-dir >/dev/null 2>&1 \
            && git -C "$main_root" check-ignore -q "$target" 2>/dev/null; then
            exit 0
        fi
        reason=$(
            printf '[worktree boundary] this edit lands in the main worktree:\n\n'
            printf '    %s\n\n' "$target"
            printf 'This session has its own worktree. Confirm only if you mean to dirty the\n'
            printf 'main checkout; otherwise redo it under:\n\n'
            printf '    %s' "$session_worktree"
        )
        emit_ask "$reason"
        ;;
esac

# Outside the main worktree entirely (home, another mount) — cannot dirty main.
exit 0
