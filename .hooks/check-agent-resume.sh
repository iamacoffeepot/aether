#!/usr/bin/env bash
# PreToolUse hook (matcher: SendMessage): a subagent is not resumed after its
# prompt cache has expired. A message to an idle agent makes it re-read its
# whole context; a fresh agent with a short brief costs less.
#
# The target's transcript is
#   <transcript_path without .jsonl>/subagents/agent-<to>.jsonl
# and its idle time is that file's modification age. The limit is 300 seconds
# for every agent type.

set -u

input=$(cat)

command -v jq >/dev/null 2>&1 || exit 0

field() {
    printf '%s' "$input" | jq -r "$1 // empty" 2>/dev/null || true
}

target=$(field '.tool_input.to')
session_transcript=$(field '.transcript_path')
[[ -n "$target" && -n "$session_transcript" ]] || exit 0

# A target that is not a plain agent id names no transcript file.
case "$target" in
    */*|.*) exit 0 ;;
esac

agents_dir="${session_transcript%.jsonl}/subagents"
agent_transcript="$agents_dir/agent-$target.jsonl"
[[ -f "$agent_transcript" ]] || exit 0

# GNU stat first, then BSD stat.
modified=$(stat -c %Y "$agent_transcript" 2>/dev/null || stat -f %m "$agent_transcript" 2>/dev/null || true)
case "$modified" in
    ''|*[!0-9]*) exit 0 ;;
esac
idle_secs=$(( $(date +%s) - modified ))

limit_secs=300

(( idle_secs > limit_secs )) || exit 0

printf '%s' "$input" | bash "$(dirname "${BASH_SOURCE[0]}")/record-agent-wait.sh" --refused expired-resume >/dev/null 2>&1 || true
{
    printf 'agent-resume: refused — agent %s has been idle %s seconds (limit %s), so its prompt cache has expired.\n' "$target" "$idle_secs" "$limit_secs"
    printf 'Do not resume it. Dispatch a fresh agent with a short brief built from the observable facts: the worktree, the branch, the pull request, and its checks.\n'
} >&2
exit 2
