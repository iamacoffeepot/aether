#!/usr/bin/env bash
# PreToolUse hook — enforce the role boundary and the edit half of the
# don't-dirty-main worktree boundary (ADR-0110 § "Hook-enforced guardrails").
#
# Two gates run before a tool executes, both reading the session-keyed role
# marker the binding hook (#1818) writes at .claude/roles/<session-id>:
#
#   (a) Role gate (all tools) — the active role's deny table matches the git/gh
#       effect verb that stands in for a denied action (merge / push /
#       issue-creation — caught before they run) against the Bash command, and
#       asks to confirm. Skill-form denials are not Bash commands and have no
#       enforceable Bash stand-in, so they are not carried here (ADR-0111 makes
#       an out-of-role skill advisory at most, and its effects hit this gate).
#         dreamer / scoper -> merge, code-push
#         orchestrator     -> issue-creation
#         everything       -> nothing
#   (b) Edit-path gate (Edit/Write/MultiEdit/NotebookEdit) — resolves the
#       target file_path to absolute and asks to confirm a write that would
#       dirty the main worktree. /tmp scratch, the session's own worktree
#       (.claude/worktrees/<session-id>), and any aether-gitignored path are
#       allowed silently — a gitignored path never shows in `git status`,
#       matching the PostToolUse tripwire check-worktree-clean.sh.
#
# Reads the PreToolUse tool-call JSON from stdin (Claude Code hook protocol).
# A crossing is surfaced as an ask-to-confirm prompt (ADR-0111): the hook prints
# a `permissionDecision: "ask"` decision on stdout and exits 0, so the tool runs
# only if the operator confirms. Fails open (plain exit 0, no prompt) when the
# role marker is absent, so an unbound session is never gated.
#
# Stays bash 3.2 compatible (the macOS system bash): no associative arrays,
# the deny table is expressed as case-statement data functions.

set -u

input=$(cat)
session_id=$(printf '%s' "$input" | jq -r '.session_id // ""')
tool_name=$(printf '%s' "$input" | jq -r '.tool_name // ""')
command=$(printf '%s' "$input" | jq -r '.tool_input.command // ""')
file_path=$(printf '%s' "$input" | jq -r '.tool_input.file_path // ""')

# Project root: CLAUDE_PROJECT_DIR is exported for hooks; fall back to the hook
# script's own location (two levels up from .claude/hooks/) when it is unset.
project_dir="${CLAUDE_PROJECT_DIR:-}"
if [[ -z "$project_dir" ]]; then
    script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
    project_dir=$(cd "$script_dir/../.." && pwd)
fi

# Without a session id there is nothing to key the binding on — fail open.
[[ -z "$session_id" ]] && exit 0

marker_file="$project_dir/.claude/roles/$session_id"
[[ -r "$marker_file" ]] || exit 0
role=$(tr -d '[:space:]' < "$marker_file")
[[ -n "$role" ]] || exit 0

# Command position: start of line or right after a separator (`;`, `&`, `|`, and
# thus `&&` / `||`), mirroring check-pre-push.sh so a heredoc/quoted body that
# merely mentions a verb does not false-positive.
pos='(^|[;&|])[[:space:]]*'

# Deny table (data). action_regex maps an action to the command-position
# pattern that stands in for it: merge / push / issue-creation appear as their
# git/gh verb (they must be caught before they run). issue-creation requires
# POST so a plain `gh api .../issues` read is not mistaken for a create.
action_regex() {
    case "$1" in
        merge) printf '%s' "${pos}(gh[[:space:]]+pr[[:space:]]+merge|git[[:space:]]+merge[[:space:]]|gh[[:space:]]+api[[:space:]][^|;&]*merge)" ;;
        push) printf '%s' "${pos}git[[:space:]]+push" ;;
        # issue-creation only: `issues` must be the final path segment (preceded
        # by `/` or space, followed by space), so POST to .../issues/<n>/comments
        # or .../issues/<n>/labels (commenting, labelling) is not mistaken for it.
        issue_create) printf '%s' "${pos}(gh[[:space:]]+issue[[:space:]]+create|gh[[:space:]]+api[[:space:]][^|;&]*POST[^|;&]*[/[:space:]]issues[[:space:]]|gh[[:space:]]+api[[:space:]][^|;&]*[/[:space:]]issues[[:space:]][^|;&]*POST)" ;;
    esac
}
action_label() {
    case "$1" in
        merge) printf 'merge a pull request (gh pr merge / git merge / gh api .../merge)' ;;
        push) printf 'push code (git push)' ;;
        issue_create) printf 'create issues (gh issue create / gh api POST .../issues)' ;;
    esac
}
# role -> denied actions. everything (and any unknown role) denies nothing.
role_actions() {
    case "$1" in
        dreamer | scoper) printf 'merge push' ;;
        orchestrator) printf 'issue_create' ;;
        *) printf '' ;;
    esac
}
role_remedy() {
    case "$1" in
        dreamer) printf 'A dreamer files and scopes issues. Hand the work to an orchestrator session, or re-declare this session'\''s role.' ;;
        scoper) printf 'A scoper walks an issue Define -> Design -> Plan and stops. Hand the work to an orchestrator session, or re-declare this session'\''s role.' ;;
        orchestrator) printf 'An orchestrator turns scoped issues into merged PRs; a design gap bounces back to a dreamer/scoper rather than being scoped in place. Re-declare this session'\''s role to ideate or scope.' ;;
        *) printf 'Re-declare this session'\''s role to proceed.' ;;
    esac
}

# Surface a boundary crossing as a PreToolUse ask-to-confirm decision (ADR-0111):
# print the hook JSON carrying the reason on stdout and exit 0, so the tool runs
# only if the operator confirms. Supersedes the old `exit 2` hard deny.
emit_ask() {
    jq -nc --arg r "$1" \
        '{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:"ask",permissionDecisionReason:$r}}'
    exit 0
}

# Role gate — match the command against each action the active role denies.
if [[ -n "$command" ]]; then
    for action in $(role_actions "$role"); do
        if printf '%s' "$command" | grep -qE "$(action_regex "$action")"; then
            reason=$(
                printf '[role boundary] the %s role does not normally %s.\n\n' "$role" "$(action_label "$action")"
                printf 'ADR-0110 binds this session to the %s role. Confirm only if you mean to\n' "$role"
                printf 'cross that boundary.\n\n'
                printf '%s' "$(role_remedy "$role")"
            )
            emit_ask "$reason"
        fi
    done
fi

# Edit-path gate — only the edit tools, which declare their target up front.
case "$tool_name" in
    Edit | Write | MultiEdit | NotebookEdit) ;;
    *) exit 0 ;;
esac
[[ -n "$file_path" ]] || exit 0

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
session_worktree=$(normalize_path "$project_dir/.claude/worktrees/$session_id")
main_root=$(normalize_path "$project_dir")

# Allow temp scratch (/tmp and the macOS temp roots).
case "$target" in
    /tmp/* | /tmp | /private/tmp/* | /var/tmp/* | /var/folders/*) exit 0 ;;
esac
# Allow the session's own worktree. A session reaches the worktrees of agents it
# spawns through dispatch, not by editing them — those agents run outside this
# guardrail (no role marker of their own, so the hook fails open for them) and
# work in their own worktrees, so the bound session itself stays own-worktree.
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
            printf 'ADR-0110 holds every role to the don'\''t-dirty-main rule. Confirm only if\n'
            printf 'you mean to dirty the main checkout; otherwise redo it under:\n\n'
            printf '    %s' "$session_worktree"
        )
        emit_ask "$reason"
        ;;
esac

# Outside the main worktree entirely (home, another mount) — cannot dirty main.
exit 0
