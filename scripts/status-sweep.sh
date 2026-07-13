#!/usr/bin/env bash
# Board-wide pending-ask sweep over REST. Prints one aligned line per open issue
# carrying agent:awaiting-answer — every ask the pipeline has parked on the owner
# — in a single script invocation, so a "what is waiting on me" check costs one
# Bash tool call, not one command per issue per fact.
#
# This is the local query surface that replaces the retired central verdict
# ticket (#3316): the approve sweep and the single-issue flows now park each ask
# on the issue it concerns, and this read-only sweep aggregates them on demand
# rather than maintaining one shared ticket. Nothing here mutates GitHub.
#
# Usage:
#
#   scripts/status-sweep.sh                  print every open parked ask
#   scripts/status-sweep.sh 3316 3200        restrict to issue #3316 and #3200
#
# Output (space-padded for alignment):
#
#   #3316  task=approve   ref=3316  age=2d   **Parked on #3316 — need a decision.**
#   #3200  task=scope     ref=3200  age=5d   **Parked on #3200 — need a decision.**
#
# Columns:
#   #N     — issue number
#   task   — the parked task from the awaiting-answer marker (approve / scope / …)
#   ref    — the marker's ref (the issue/PR the task acts on)
#   age    — days since the park comment was posted (? when undeterminable)
#   ask    — the first line of the parked question (the bold "Parked on …" header)
#
# REST only — no `gh issue list` (GraphQL-backed). Every call goes through the
# REST `issues` endpoints to stay off the contended per-user GraphQL pool.

set -euo pipefail

REPO="iamacoffeepot/aether"

# Optional positional issue-number filters.
FILTER=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --*)
            echo "unknown flag: $1" >&2
            echo "usage: status-sweep.sh [<issue> ...]" >&2
            exit 2
            ;;
        *)
            FILTER+=("$1")
            shift
            ;;
    esac
done

# Portable ISO-8601 → epoch seconds: GNU date first, then BSD/macOS date.
# Echoes nothing on failure so the caller renders age=?.
iso_to_epoch() {
    local ts="$1"
    date -d "$ts" +%s 2>/dev/null && return 0
    date -j -f "%Y-%m-%dT%H:%M:%SZ" "$ts" +%s 2>/dev/null && return 0
    return 0
}

# Print one status line for a parked issue given its number.
print_ask_line() {
    local num="$1"
    local body marker task ref
    body="$(gh api "repos/$REPO/issues/$num" --jq '.body // ""' 2>/dev/null || echo "")"

    # The marker lives in the park comment (single-issue and per-issue-ask parks
    # post it there); read the body first for parity with the chat route step,
    # then fall back to comments. Capture the comment carrying it so its
    # created_at dates the park.
    local created=""
    marker="$(grep -m1 -oE '<!-- aether-agent:awaiting-answer[^>]*-->' <<<"$body" || true)"
    local ask=""
    if [[ -n "$marker" ]]; then
        ask="$(awk '/<!-- aether-agent:awaiting-answer/{f=1; next} f && NF {print; exit}' <<<"$body" || true)"
        created="$(gh api "repos/$REPO/issues/$num" --jq '.created_at' 2>/dev/null || echo "")"
    else
        local comments
        comments="$(gh api "repos/$REPO/issues/$num/comments" --paginate \
            --jq '[.[] | select(.body | test("<!-- aether-agent:awaiting-answer")) ] | last // {}' 2>/dev/null || echo '{}')"
        local cbody
        cbody="$(jq -r '.body // ""' <<<"$comments")"
        marker="$(grep -m1 -oE '<!-- aether-agent:awaiting-answer[^>]*-->' <<<"$cbody" || true)"
        ask="$(awk '/<!-- aether-agent:awaiting-answer/{f=1; next} f && NF {print; exit}' <<<"$cbody" || true)"
        created="$(jq -r '.created_at // ""' <<<"$comments")"
    fi

    task="$(grep -oE 'task=[a-z-]+' <<<"$marker" | head -n1 | cut -d= -f2 || true)"
    ref="$(grep -oE 'ref=[a-z0-9]+' <<<"$marker" | head -n1 | cut -d= -f2 || true)"
    [ -n "$task" ] || task="?"
    [ -n "$ref" ] || ref="$num"

    local age="?"
    if [[ -n "$created" ]]; then
        local parked now
        parked="$(iso_to_epoch "$created")"
        now="$(date +%s)"
        if [[ -n "$parked" ]]; then
            age="$(( (now - parked) / 86400 ))d"
        fi
    fi

    [ -n "$ask" ] || ask="(no question text found)"
    printf '%-7s  %-14s  %-10s  %-6s  %s\n' \
        "#$num" "task=$task" "ref=$ref" "age=$age" "$ask"
}

# Enumerate open issues carrying agent:awaiting-answer (PRs filtered out).
nums="$(gh api "repos/$REPO/issues?labels=agent:awaiting-answer&state=open&per_page=100" --paginate \
    --jq '.[] | select(has("pull_request") | not) | .number' 2>/dev/null || true)"

if [[ ${#FILTER[@]} -gt 0 ]]; then
    filtered=""
    for n in $nums; do
        for f in "${FILTER[@]}"; do
            [ "$n" = "$f" ] && filtered="${filtered}${n}"$'\n'
        done
    done
    nums="$filtered"
fi

if [[ -z "${nums//[[:space:]]/}" ]]; then
    echo "No open issues carrying agent:awaiting-answer — nothing is parked on you."
    exit 0
fi

for n in $nums; do
    print_ask_line "$n"
done
