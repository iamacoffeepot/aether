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
#   #3316  task=approve   age=2d   **Parked on #3316 — need a decision.**
#   #3200  task=scope     age=5d   **Parked on #3200 — need a decision.**
#
# Columns:
#   #N     — issue number (the work ref is always this same number, #3336)
#   task   — the parked task from the agent:park:<task> label (approve / scope / …)
#   age    — days since the agent:awaiting-answer label was applied (? when undeterminable)
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
    local labels task ask created

    # task: the agent:park:<task> label (#3336 moved it off the free-text
    # marker). Transition fallback below reads task= off the old marker for a
    # park written before #3336 landed.
    labels="$(gh api "repos/$REPO/issues/$num/labels" --jq '.[].name' 2>/dev/null || echo "")"
    task="$(grep -m1 -oE '^agent:park:[a-z-]+$' <<<"$labels" | cut -d: -f3 || true)"

    # Park age dates from the most recent agent:awaiting-answer labeled event —
    # the same authoritative park timestamp agent-tick's nudge loop reads.
    created="$(gh api "repos/$REPO/issues/$num/timeline?per_page=100" --paginate \
        --jq '[.[] | select(.event=="labeled" and .label.name=="agent:awaiting-answer") | .created_at] | last // empty' 2>/dev/null || echo "")"

    # Question text: the first non-empty line of the latest park comment. The
    # comment is now pure prose opening with the bold "**Parked on …**" header
    # (#3336 dropped the HTML marker) — select the latest comment carrying it.
    ask="$(gh api "repos/$REPO/issues/$num/comments" --paginate \
        --jq '[.[] | select(.body | test("\\*\\*Parked on"))] | last | .body // ""' 2>/dev/null \
        | awk 'NF {print; exit}' || true)"

    # Transition fallback for a pre-#3336 park still carrying the marker: recover
    # the task and question from the marker in the body or the latest park comment.
    if [[ -z "$task" || -z "$ask" ]]; then
        local body marker=""
        body="$(gh api "repos/$REPO/issues/$num" --jq '.body // ""' 2>/dev/null || echo "")"
        local mbody="$body"
        marker="$(grep -m1 -oE '<!-- aether-agent:awaiting-answer[^>]*-->' <<<"$body" || true)"
        if [[ -z "$marker" ]]; then
            local comments
            comments="$(gh api "repos/$REPO/issues/$num/comments" --paginate \
                --jq '[.[] | select(.body | test("<!-- aether-agent:awaiting-answer")) ] | last // {}' 2>/dev/null || echo '{}')"
            mbody="$(jq -r '.body // ""' <<<"$comments")"
            marker="$(grep -m1 -oE '<!-- aether-agent:awaiting-answer[^>]*-->' <<<"$mbody" || true)"
        fi
        [[ -n "$task" ]] || task="$(grep -oE 'task=[a-z-]+' <<<"$marker" | head -n1 | cut -d= -f2 || true)"
        [[ -n "$ask" ]] || ask="$(awk '/<!-- aether-agent:awaiting-answer/{f=1; next} f && NF {print; exit}' <<<"$mbody" || true)"
    fi

    [ -n "$task" ] || task="?"

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
    printf '%-7s  %-14s  %-6s  %s\n' \
        "#$num" "task=$task" "age=$age" "$ask"
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
