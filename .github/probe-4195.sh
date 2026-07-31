#!/usr/bin/env bash
# TEMPORARY diagnostic probe for issue 4195. Not a change to the repo —
# this file exists only on the probe branch and is deleted with its
# pull request. Do not merge.
#
# Reproduces the load shape the two captured failing CI logs share, which
# every prior local attempt missed: a long serialized wasm-compile stall
# followed by trampoline activation, both route claims and the drop inside
# a ~15 ms burst, on a small CPU budget with other compile-heavy work
# co-scheduled. Prior attempts used N spinning CPU hogs — continuous
# contention rather than stall-then-burst.
#
# The shape is produced by the workload itself: each copy of
# `dropped_component_routes_are_purged` compiles the fixture bundle wasm
# twice, serialized on the component cap's thread, and then runs its whole
# lifecycle burst. Several copies pinned to a 4-CPU set therefore contend
# exactly where CI contends — one copy's burst lands inside another's
# compile stall.
set -uo pipefail

BIN="${1:?usage: probe-4195.sh <test-binary>}"
TEST_NAME="tests::dropped_component_routes_are_purged"
ROUNDS="${ROUNDS:-40}"
CONCURRENCY="${CONCURRENCY:-4}"
CPUS="${CPUS:-0-3}"
BUDGET_SECONDS="${BUDGET_SECONDS:-3000}"
LOGDIR="${LOGDIR:-probe-logs}"

echo "nproc=$(nproc) pinned=${CPUS} concurrency=${CONCURRENCY} rounds=${ROUNDS} budget=${BUDGET_SECONDS}s"

mkdir -p "$LOGDIR"
runs=0
fails=0
round=0

while [ "$round" -lt "$ROUNDS" ] && [ "$SECONDS" -lt "$BUDGET_SECONDS" ]; do
    round=$((round + 1))
    pids=()
    for copy in $(seq 1 "$CONCURRENCY"); do
        taskset -c "$CPUS" "$BIN" --exact "$TEST_NAME" --nocapture \
            >"${LOGDIR}/round-${round}-copy-${copy}.log" 2>&1 &
        pids+=("$!")
    done

    copy=0
    for pid in "${pids[@]}"; do
        copy=$((copy + 1))
        runs=$((runs + 1))
        if ! wait "$pid"; then
            fails=$((fails + 1))
            echo "FAIL round=${round} copy=${copy} log=${LOGDIR}/round-${round}-copy-${copy}.log"
        fi
    done
    echo "round ${round}: executions=${runs} failures=${fails} elapsed=${SECONDS}s"
done

# The prediction under test (issue 4195, fourth investigation pass): a
# failing execution carries exactly two `TargetNotFound` monitor refusals
# for the trampoline mailbox, and a `watchers=0` vacate. Zero refusals
# falsifies the surviving mechanism and is the more valuable outcome.
refusals=$(grep -h -o "route holder is not monitorable" "$LOGDIR"/*.log | wc -l)
target_not_found=$(grep -h -o "error=TargetNotFound" "$LOGDIR"/*.log | wc -l)
vacate_empty=$(grep -h -o "watchers=0" "$LOGDIR"/*.log | wc -l)
vacate_any=$(grep -h -o "watchers=[0-9]*" "$LOGDIR"/*.log | sort | uniq -c | tr '\n' ' ')

echo "=== probe 4195 summary ==="
echo "executions:              ${runs}"
echo "failures:                ${fails}"
echo "monitor refusals logged: ${refusals}"
echo "TargetNotFound:          ${target_not_found}"
echo "vacate watchers=0:       ${vacate_empty}"
echo "vacate watcher counts:   ${vacate_any}"

if [ "$fails" -gt 0 ]; then
    first_failing=$(grep -l "panicked" "$LOGDIR"/*.log | head -1)
    if [ -n "$first_failing" ]; then
        echo "--- first failing log: ${first_failing} ---"
        head -c 40000 "$first_failing"
    fi
    exit 1
fi
