#!/usr/bin/env bash
# SubagentStop hook: a subagent does not end while a shell task it started is
# still running. Ending there parks the agent on the task; the wake-up arrives
# after its five-minute prompt cache has expired and rewrites its whole context.
#
# The harness lists the whole session's background tasks at this event, the
# main session's included, so a task belongs to the stopping agent only when
# its id appears in the agent's own transcript.

set -u

input=$(cat)

command -v jq >/dev/null 2>&1 || exit 0

transcript=$(printf '%s' "$input" | jq -r '.agent_transcript_path // empty' 2>/dev/null || true)
[[ -n "$transcript" && -r "$transcript" ]] || exit 0

owned=()
while IFS= read -r task_id; do
    [[ -n "$task_id" ]] || continue
    if grep -qwF -- "$task_id" "$transcript" 2>/dev/null; then
        owned+=("$task_id")
    fi
done < <(printf '%s' "$input" \
    | jq -r '.background_tasks[]? | select(.type == "shell" and .status == "running") | .id // empty' 2>/dev/null)

(( ${#owned[@]} )) || exit 0

printf '%s' "$input" | bash "$(dirname "${BASH_SOURCE[0]}")/record-agent-wait.sh" --refused running-task >/dev/null 2>&1 || true
{
    printf 'agent-stop: refused — a background shell task you started is still running: %s.\n' "${owned[*]}"
    printf 'Stop the task or let it finish before you end. Do not park on a long job: hand its handle (log path, job name, pull request number) back in your report and the session that dispatched you waits on it.\n'
} >&2
exit 2
