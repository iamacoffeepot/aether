#!/usr/bin/env bash
# agent-job.sh — run a long command as a detached job and wait on it in
# slices short enough to keep a subagent's five-minute prompt cache warm.
#
#   agent-job.sh start <name> -- <command...>
#   agent-job.sh wait <name>
#   agent-job.sh wait <name> --until-done
#
# `start` detaches the command from the calling shell, so it survives the call
# and the end of the agent that started it. Its output goes to the job's log.
#
# `wait` blocks for at most one slice and prints one line, status first:
#
#   done job=<name> exit=<status> log=<path>
#   running job=<name> polls=<count> log=<path>
#   handoff job=<name> polls=<count> log=<path>
#   lost job=<name> log=<path>
#
# Each `running` answer is one poll: the caller re-reads its context from
# cache, which costs about a tenth of rebuilding it cold. After the poll limit
# the answer is `handoff`: the wait has cost as much as a rebuild would, so the
# subagent ends its turn with the job name and the session that dispatched it
# waits with `--until-done`, which blocks until the job ends and never counts.
# `lost` means the job's process is gone and it recorded no exit status.
#
# Job state lives under `.agents/jobs/<name>/` in the primary checkout, so a
# worker in an issue worktree and the session that dispatched it see the same
# job. The slice is the bound `.hooks/check-agent-wait.sh` holds every subagent
# call to; AGENT_JOB_SLICE_SECS shortens it and can never lengthen it.

set -u

slice_limit_secs=225
handoff_polls=10
probe_secs=2

usage() {
    {
        printf 'usage: agent-job.sh start <name> -- <command...>\n'
        printf '       agent-job.sh wait <name> [--until-done]\n'
    } >&2
    exit 2
}

jobs_root() {
    if [[ -n "${AGENT_JOB_ROOT:-}" ]]; then
        printf '%s\n' "$AGENT_JOB_ROOT"
        return
    fi
    local common
    common=$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || {
        printf 'agent-job: not inside a git checkout\n' >&2
        exit 2
    }
    printf '%s\n' "$(dirname "$common")/.agents/jobs"
}

job_dir() {
    local name="$1"
    [[ "$name" =~ ^[A-Za-z0-9._-]+$ ]] || {
        printf 'agent-job: bad job name: %s\n' "$name" >&2
        exit 2
    }
    printf '%s\n' "$(jobs_root)/$name"
}

job_is_alive() {
    local pid
    pid=$(cat "$1/pid" 2>/dev/null) || return 1
    [[ -n "$pid" ]] || return 1
    kill -0 "$pid" 2>/dev/null
}

start_job() {
    local name="${1:-}"
    [[ -n "$name" && "${2:-}" == "--" && $# -ge 3 ]] || usage
    shift 2

    local dir
    dir=$(job_dir "$name") || exit 2
    if [[ ! -e "$dir/exit" ]] && job_is_alive "$dir"; then
        printf 'running job=%s log=%s\n' "$name" "$dir/log"
        printf 'agent-job: job %s is already running; wait on it or pick another name\n' "$name" >&2
        exit 1
    fi

    rm -rf "$dir"
    mkdir -p "$dir" || exit 2
    printf '0\n' > "$dir/polls"

    # The exit status lands by rename, so a reader never sees a partial file.
    nohup bash -c '
        dir="$1"; shift
        "$@" > "$dir/log" 2>&1
        printf "%s\n" "$?" > "$dir/exit.partial"
        mv "$dir/exit.partial" "$dir/exit"
    ' agent-job "$dir" "$@" < /dev/null > /dev/null 2>&1 &
    printf '%s\n' "$!" > "$dir/pid"

    printf 'started job=%s log=%s\n' "$name" "$dir/log"
}

wait_job() {
    local name="${1:-}" mode="${2:-}"
    [[ -n "$name" ]] || usage
    [[ -z "$mode" || "$mode" == "--until-done" ]] || usage

    local dir
    dir=$(job_dir "$name") || exit 2
    [[ -d "$dir" ]] || {
        printf 'agent-job: no job named %s\n' "$name" >&2
        exit 2
    }

    local slice_secs="${AGENT_JOB_SLICE_SECS:-$slice_limit_secs}"
    [[ "$slice_secs" =~ ^[0-9]+$ ]] || slice_secs=$slice_limit_secs
    (( slice_secs > slice_limit_secs )) && slice_secs=$slice_limit_secs

    local polls
    polls=$(cat "$dir/polls" 2>/dev/null)
    [[ "$polls" =~ ^[0-9]+$ ]] || polls=0

    local deadline=$(( $(date +%s) + slice_secs ))
    while :; do
        if [[ -e "$dir/exit" ]]; then
            printf 'done job=%s exit=%s log=%s\n' "$name" "$(cat "$dir/exit")" "$dir/log"
            return 0
        fi
        if ! job_is_alive "$dir"; then
            # The job may have finished between the two checks above.
            [[ -e "$dir/exit" ]] && continue
            printf 'lost job=%s log=%s\n' "$name" "$dir/log"
            return 1
        fi
        if [[ "$mode" != "--until-done" ]]; then
            (( polls >= handoff_polls )) && break
            (( $(date +%s) >= deadline )) && break
        fi
        sleep "$probe_secs"
    done

    if (( polls < handoff_polls )); then
        polls=$(( polls + 1 ))
        printf '%s\n' "$polls" > "$dir/polls"
    fi
    if (( polls >= handoff_polls )); then
        printf 'handoff job=%s polls=%s log=%s\n' "$name" "$polls" "$dir/log"
    else
        printf 'running job=%s polls=%s log=%s\n' "$name" "$polls" "$dir/log"
    fi
}

verb="${1:-}"
[[ $# -gt 0 ]] && shift
case "$verb" in
    start) start_job "$@" ;;
    wait) wait_job "$@" ;;
    *) usage ;;
esac
