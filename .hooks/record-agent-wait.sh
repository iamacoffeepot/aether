#!/usr/bin/env bash
# Appends one line to the agent-wait ledger, so the cost of subagent waits is
# measurable without re-reading transcripts.
#
# As a PostToolUse Bash hook (no arguments) it records a subagent call that ran
# five minutes or longer: a subagent's prompt cache lasts five minutes, so such
# a call rewrote the agent's whole context.
#
# The check-agent-*.sh hooks call it as `record-agent-wait.sh --refused <rule>`
# with their own hook input on stdin to record a refusal.
#
# The ledger is advisory. This script always exits 0, so a failed write never
# changes a verdict.

set -u

input=$(cat)
refused=""
if [[ "${1:-}" == "--refused" ]]; then
    refused="${2:-unknown}"
fi

command -v jq >/dev/null 2>&1 || exit 0

threshold_millis=300000

if [[ -z "$refused" ]]; then
    agent_id=$(printf '%s' "$input" | jq -r '.agent_id // empty' 2>/dev/null || true)
    [[ -n "$agent_id" ]] || exit 0

    ran_long=$(printf '%s' "$input" | jq -r --argjson limit "$threshold_millis" '((.duration_ms // 0) >= $limit)' 2>/dev/null || true)
    [[ "$ran_long" == "true" ]] || exit 0
fi

root="${CLAUDE_PROJECT_DIR:-}"
if [[ -z "$root" ]]; then
    root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." 2>/dev/null && pwd) || exit 0
fi
ledger_dir="$root/.agents/ledger"
mkdir -p "$ledger_dir" 2>/dev/null || exit 0

line=$(printf '%s' "$input" | jq -c --arg time "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg refused "$refused" '
    {
        time: $time,
        session_id: (.session_id // null),
        agent_id: (.agent_id // null),
        agent_type: (.agent_type // null),
        refused: (if $refused == "" then null else $refused end),
        duration_millis: (.duration_ms // null),
        target: (if .tool_name == "SendMessage" then (.tool_input.to // null) else null end),
        command: ((.tool_input.command // null) | if type == "string" then .[0:160] else null end)
    } | with_entries(select(.value != null))' 2>/dev/null || true)
[[ -n "$line" ]] || exit 0

printf '%s\n' "$line" >> "$ledger_dir/agent-waits.jsonl" 2>/dev/null || true
exit 0
