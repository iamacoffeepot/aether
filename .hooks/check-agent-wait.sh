#!/usr/bin/env bash
# PreToolUse hook (matcher: Bash): an agent ends at a wait.
#
# A subagent's prompt cache lasts five minutes (the main session's lasts an
# hour), so a subagent that waits longer rewrites its whole context on its next
# call. This hook refuses the waits in a subagent and leaves the main session
# alone. The harness puts `agent_id` on a subagent's hook input only.
#
# Refused in any subagent:
#   - a gate wait: `wave-status.sh --wait`, `gh run watch`, `gh pr checks --watch`;
#   - an `until` / `while` / `for` loop whose body sleeps;
#   - a single `sleep` of 240 seconds or more.
# Refused unless the agent type is `implementer`, whose cache lasts an hour:
#   - a foreground call with a `timeout` above 270000 milliseconds.

set -u

input=$(cat)

command -v jq >/dev/null 2>&1 || exit 0

field() {
    printf '%s' "$input" | jq -r "$1 // empty" 2>/dev/null || true
}

agent_id=$(field '.agent_id')
[[ -n "$agent_id" ]] || exit 0

agent_type=$(field '.agent_type')
command=$(field '.tool_input.command')
in_background=$(field '.tool_input.run_in_background')

sleep_limit_secs=240
timeout_limit_millis=270000

refuse() {
    local rule="$1" what="$2"
    printf '%s' "$input" | bash "$(dirname "${BASH_SOURCE[0]}")/record-agent-wait.sh" --refused "$rule" >/dev/null 2>&1 || true
    {
        printf 'agent-wait: refused — %s.\n' "$what"
        printf 'An agent ends at a wait: a subagent prompt cache lasts five minutes, so waiting here rewrites your whole context.\n'
        printf 'End your turn and hand the handle (pull request number, log path, or job name) back in your report; the session that dispatched you waits.\n'
    } >&2
    exit 2
}

# Tests the command word of one shell segment, after leading assignments,
# wrappers, their flags, and an opening quote (`bash -c "…"`).
segment_is_gate_wait() {
    local segment="$1" word
    local wait_flag_re='[[:space:]]--wait([[:space:]=]|$)'
    local run_watch_re='[[:space:]]run[[:space:]]+watch([[:space:]]|$)'
    local checks_re='[[:space:]]pr[[:space:]]+checks([[:space:]]|$)'
    local watch_flag_re='[[:space:]]--watch([[:space:]=]|$)'
    while :; do
        segment="${segment#"${segment%%[![:space:]\"\']*}"}"
        word="${segment%%[[:space:]]*}"
        case "$word" in
            *=*|-*|nohup|exec|time|command|bash|sh|zsh) segment="${segment#"$word"}" ;;
            timeout)
                segment="${segment#"$word"}"
                segment="${segment#"${segment%%[![:space:]]*}"}"
                segment="${segment#"${segment%%[[:space:]]*}"}"
                ;;
            *) break ;;
        esac
        [[ -n "$segment" ]] || return 1
    done

    case "$word" in
        wave-status.sh|*/wave-status.sh)
            [[ "$segment" =~ $wait_flag_re ]] && return 0
            ;;
        gh)
            [[ "$segment" =~ $run_watch_re ]] && return 0
            [[ "$segment" =~ $checks_re && "$segment" =~ $watch_flag_re ]] && return 0
            ;;
    esac
    return 1
}

waits_on_gate=0
while IFS= read -r segment; do
    if segment_is_gate_wait "$segment"; then
        waits_on_gate=1
        break
    fi
done < <(printf '%s\n' "$command" | tr ';&|()' '\n\n\n\n\n')

sleeping_loop_re='(^|[^[:alnum:]_.-])(until|while|for)[[:space:]].*[^[:alnum:]_.-]sleep[[:space:]].*[^[:alnum:]_.-]done([^[:alnum:]_.-]|$)'
loops_on_sleep=0
[[ "$command" =~ $sleeping_loop_re ]] && loops_on_sleep=1

longest_sleep_secs=0
while IFS= read -r amount; do
    [[ -n "$amount" ]] || continue
    number="${amount%%[!0-9]*}"
    unit="${amount##*[0-9.]}"
    [[ -n "$number" ]] || continue
    # Force base 10 so a leading zero is not read as octal.
    secs=$((10#$number))
    case "$unit" in
        m) secs=$((secs * 60)) ;;
        h) secs=$((secs * 3600)) ;;
        d) secs=$((secs * 86400)) ;;
    esac
    (( secs > longest_sleep_secs )) && longest_sleep_secs=$secs
done < <(printf '%s\n' "$command" \
    | grep -oE '(^|[^[:alnum:]_.-])sleep[[:space:]]+[0-9]+(\.[0-9]+)?[smhd]?' \
    | sed -E 's/^.*sleep[[:space:]]+//')
sleeps_long=0
(( longest_sleep_secs >= sleep_limit_secs )) && sleeps_long=1

over_timeout=$(printf '%s' "$input" | jq -r --argjson limit "$timeout_limit_millis" '((.tool_input.timeout // 0) > $limit)' 2>/dev/null || true)
blocks_long=0
if [[ "$over_timeout" == "true" && "$in_background" != "true" && "$agent_type" != "implementer" ]]; then
    blocks_long=1
fi

if (( waits_on_gate )); then
    refuse gate-wait "this command waits on a gate (a checks wait or watch)"
fi
if (( loops_on_sleep )); then
    refuse sleep-loop "this command polls in a loop that sleeps"
fi
if (( sleeps_long )); then
    refuse long-sleep "this command sleeps ${longest_sleep_secs} seconds (limit: under ${sleep_limit_secs})"
fi
if (( blocks_long )); then
    refuse long-timeout "a foreground timeout above ${timeout_limit_millis} milliseconds outlives your cache; only the implementer agent type may block that long"
fi

exit 0
