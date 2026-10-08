#!/usr/bin/env bash
# Regression tests for scripts/agent-job.sh: the answers a waiting agent acts
# on. Each case runs real jobs against a throwaway job root.
set -u

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/agent-job-test.XXXXXX")
trap 'rm -rf "$ROOT"' EXIT

pass=0
fail=0
job() { AGENT_JOB_ROOT="$ROOT" AGENT_JOB_SLICE_SECS="${SLICE:-1}" bash "$HERE/agent-job.sh" "$@" 2>&1; }
# expect <desc> <want-prefix> <output>
expect() {
    case "$3" in
        "$2"*) pass=$((pass + 1)); printf 'PASS  %s\n' "$1" ;;
        *) fail=$((fail + 1)); printf 'FAIL  %s\n      want prefix: %s\n      got: %s\n' "$1" "$2" "$3" ;;
    esac
}

job start quick -- bash -c 'echo hello; exit 3' >/dev/null
expect "a finished job answers done with its exit status" "done job=quick exit=3 log=" "$(SLICE=20 job wait quick)"
expect "the job's output is in its log" "hello" "$(cat "$ROOT/quick/log")"

job start slow -- sleep 60 >/dev/null
expect "a job that outlives the slice answers running" "running job=slow polls=1 " "$(job wait slow)"
expect "a second start of a running name is refused" "running job=slow " "$(job start slow -- sleep 60)"
for _ in 2 3 4 5 6 7 8 9; do job wait slow >/dev/null; done
expect "the tenth running answer is the handoff" "handoff job=slow polls=10 " "$(job wait slow)"
started=$(date +%s)
expect "a wait after the handoff answers at once" "handoff job=slow polls=10 " "$(SLICE=200 job wait slow)"
elapsed=$(( $(date +%s) - started ))
if (( elapsed < 5 )); then pass=$((pass + 1)); echo "PASS  the answer after a handoff does not block"; else fail=$((fail + 1)); echo "FAIL  the answer after a handoff blocked ${elapsed}s"; fi

# The wrapper dies first, so it records no exit status; its command is found
# beforehand, because it is orphaned once the wrapper is gone.
wrapper=$(cat "$ROOT/slow/pid")
orphan=$(pgrep -P "$wrapper")
kill -9 "$wrapper" 2>/dev/null
kill $orphan 2>/dev/null
sleep 1
expect "a killed job with no exit status answers lost" "lost job=slow " "$(job wait slow)"

job start brief -- sleep 3 >/dev/null
expect "until-done outlasts the slice and answers done" "done job=brief exit=0 " "$(job wait brief --until-done)"

expect "a slice above the limit is clamped, not obeyed" "done job=brief " "$(SLICE=99999 job wait brief)"
expect "a name with a path separator is refused" "agent-job: bad job name" "$(job wait ../etc)"
expect "an unknown job is an error" "agent-job: no job named" "$(job wait nobody)"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
(( fail == 0 ))
