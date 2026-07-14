#!/usr/bin/env bash
# Batched PR wave-status sweep over REST. Prints one aligned line per open PR
# authored by iamacoffeepot, enriched with CI verdict (the `CI pass` aggregator
# conclusion), draft state, review state, and head branch — all in a single
# script invocation so a multi-PR status check costs one Bash tool call, not
# one command per PR per fact.
#
# Usage:
#
#   scripts/wave-status.sh                   print all open PRs
#   scripts/wave-status.sh 42 99             restrict to PR #42 and #99
#   scripts/wave-status.sh --wait <pr>       loop (every 20s) until the `CI pass`
#                                            aggregator for <pr>'s CURRENT head
#                                            SHA completes; exit 0 on success,
#                                            1 on failure. The head is re-read
#                                            every tick so the wait follows a
#                                            fix push instead of settling on a
#                                            superseded head's verdict.
#                                            Fast-fails (exits 1 early) the moment
#                                            a deterministic check (fmt / clippy /
#                                            docs / marker build / hook tests)
#                                            concludes failure, without waiting
#                                            for the slow jobs to finish.
#   scripts/wave-status.sh --wait-verdict <pr>
#                                            loop (every 20s) until critic holds a
#                                            standing verdict on <pr>'s CURRENT
#                                            head SHA (issue #3405 — the implement
#                                            verdict loop's wait); exit 0 on
#                                            APPROVED, 1 on CHANGES_REQUESTED. A
#                                            verdict on a stale SHA keeps polling
#                                            (a fix push dismissed it), and the
#                                            head is re-read every tick so the
#                                            wait follows a push.
#
# Output (snapshot mode):
#
#   #42  ready  ci:success      review:approved   feat/issue-42-some-slug
#   #99  draft  ci:pending      review:none       fix/issue-99-other-slug
#
# Columns (space-padded for alignment):
#   #N    — PR number
#   state — "draft" or "ready"
#   ci    — "ci:success" | "ci:failure" | "ci:pending" (aggregator not settled yet)
#            "ci:none" when no check-runs exist for the head sha
#   review — "review:approved" | "review:changes-requested" | "review:pending"
#             | "review:none"
#   branch — head branch name
#
# REST only — no `gh pr list` / `gh pr checks` (GraphQL-backed). Every call
# here goes through the REST endpoints (`pulls`, `pulls/<n>/reviews`,
# `commits/<sha>/check-runs`) to stay off the contended per-user GraphQL pool.
#
# The CI verdict is the `CI pass` aggregator's conclusion — the required merge
# gate. The loop in `--wait` mode exits only when `CI pass` is completed AND no
# check-run is still pending, so a subset-registered matrix (only `Detect
# changes` up) can't produce a false green.

set -euo pipefail

OWNER="iamacoffeepot"
REPO="iamacoffeepot/aether"
WAIT_MODE=0
WAIT_PR=""
VERDICT_MODE=0
VERDICT_PR=""

# Collect optional PR-number filters and parse --wait / --wait-verdict.
FILTER_PRS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --wait)
            WAIT_MODE=1
            WAIT_PR="$2"
            shift 2
            ;;
        --wait-verdict)
            VERDICT_MODE=1
            VERDICT_PR="$2"
            shift 2
            ;;
        --*)
            echo "unknown flag: $1" >&2
            echo "usage: wave-status.sh [<pr> ...] | --wait <pr> | --wait-verdict <pr>" >&2
            exit 2
            ;;
        *)
            FILTER_PRS+=("$1")
            shift
            ;;
    esac
done

# Fetch a PR's `CI pass` aggregator verdict from check-runs.
# Returns: "success" | "failure" | "cancelled" | "neutral" | "pending" | "none"
ci_verdict() {
    local sha="$1"
    local runs
    runs=$(gh api "repos/$REPO/commits/$sha/check-runs" --paginate --jq '.check_runs' 2>/dev/null) || { echo "none"; return 0; }
    local agg_conclusion pending
    agg_conclusion=$(echo "$runs" | jq -r '[.[] | select(.name == "CI pass" and .status == "completed")] | first | .conclusion // empty')
    pending=$(echo "$runs" | jq '[.[] | select(.status != "completed")] | length')
    if [[ -z "$agg_conclusion" ]]; then
        echo "pending"
        return 0
    fi
    if [[ "$pending" != "0" ]]; then
        echo "pending"
        return 0
    fi
    echo "$agg_conclusion"
}

# Fetch a PR's review state — the highest-signal state across all reviews.
# An APPROVED review only counts when its commit_id matches the current head
# sha (a push doesn't auto-dismiss a stale approval on GitHub's side, so an
# approval on a superseded head falls through to "pending" instead of
# misreporting "approved"). CHANGES_REQUESTED is not head-gated: GitHub
# doesn't auto-dismiss it on a push either, and it should keep blocking the
# merge until a fresh review clears it.
# Returns: "approved" | "changes-requested" | "pending" | "none"
review_state() {
    local pr_num="$1"
    local sha="$2"
    local reviews
    reviews=$(gh api "repos/$REPO/pulls/$pr_num/reviews" --jq '[.[] | {state, commit_id}]' 2>/dev/null) || { echo "none"; return 0; }
    local count
    count=$(echo "$reviews" | jq 'length')
    if [[ "$count" == "0" ]]; then
        echo "none"
        return 0
    fi
    # CHANGES_REQUESTED beats APPROVED beats PENDING (commented/dismissed).
    local cr approved
    cr=$(echo "$reviews" | jq '[.[] | select(.state == "CHANGES_REQUESTED")] | length')
    approved=$(echo "$reviews" | jq --arg sha "$sha" '[.[] | select(.state == "APPROVED" and .commit_id == $sha)] | length')
    if [[ "$cr" != "0" ]]; then
        echo "changes-requested"
    elif [[ "$approved" != "0" ]]; then
        echo "approved"
    else
        echo "pending"
    fi
}

# Print one status line for a PR given its JSON object from the pulls list.
print_pr_line() {
    local pr_json="$1"
    local num sha branch draft_flag
    num=$(echo "$pr_json" | jq -r '.number')
    sha=$(echo "$pr_json" | jq -r '.head.sha')
    branch=$(echo "$pr_json" | jq -r '.head.ref')
    draft_flag=$(echo "$pr_json" | jq -r '.draft')

    local state_col ci_col review_col
    state_col=$([ "$draft_flag" = "true" ] && echo "draft" || echo "ready")
    ci_col="ci:$(ci_verdict "$sha")"
    review_col="review:$(review_state "$num" "$sha")"

    printf '%-6s  %-7s  %-18s  %-28s  %s\n' \
        "#$num" "$state_col" "$ci_col" "$review_col" "$branch"
}

# Guarded per-tick head-sha re-read shared by the --wait and --wait-verdict
# loops — the wait follows a fix push instead of pinning the startup head.
# `echo "$prev"` on failure keeps the previous sha for this tick only; a bare
# `sha=$(cmd 2>/dev/null) || true` would wipe $sha to empty on a transient
# failure instead, since the assignment itself always succeeds.
pr_head_sha() {
    local pr="$1" prev="$2"
    local sha
    sha=$(gh api "repos/$REPO/pulls/$pr" --jq '.head.sha' 2>/dev/null) || { echo "$prev"; return 0; }
    echo "$sha"
}

# --wait mode: loop until the aggregator settles for the given PR, exit 0 on
# success, 1 on failure. Uses the same REST check-runs endpoint as the snapshot
# path so the behaviour is identical to the CI-loop in /implement.
if [[ $WAIT_MODE -eq 1 ]]; then
    if [[ -z "$WAIT_PR" ]]; then
        echo "--wait requires a PR number" >&2
        exit 2
    fi
    sha=$(gh api "repos/$REPO/pulls/$WAIT_PR" --jq '.head.sha')
    echo "[wave-status] waiting for CI pass on PR #$WAIT_PR (sha ${sha:0:8}…)"
    # Deterministic checks: never flaky. A failure in this set already dooms
    # `CI pass`, so the caller fixes and re-pushes now rather than waiting out
    # the slow jobs — the new push supersedes the run.
    fast_fail_names='["Format","Clippy","Docs","Marker-only host build"]'
    while :; do
        sha=$(pr_head_sha "$WAIT_PR" "$sha")
        runs=$(gh api "repos/$REPO/commits/$sha/check-runs" --paginate --jq '.check_runs' 2>/dev/null) || { sleep 20; continue; }
        fast_red=$(echo "$runs" | jq -r --argjson ff "$fast_fail_names" \
            '.[] | select(.status == "completed")
                 | select(.conclusion == "failure" or .conclusion == "timed_out" or .conclusion == "action_required" or .conclusion == "startup_failure")
                 | select(.name as $n | $ff | index($n))
                 | .name + ": " + .conclusion')
        if [[ -n "$fast_red" ]]; then
            echo "[wave-status] deterministic check failed — fast-fail (other jobs may still be running):"
            echo "$fast_red"
            exit 1
        fi
        agg_done=$(echo "$runs" | jq '[.[] | select(.name == "CI pass" and .status == "completed")] | length')
        pending=$(echo "$runs" | jq '[.[] | select(.status != "completed")] | length')
        if [[ "$agg_done" = "1" && "$pending" = "0" ]]; then
            verdict=$(echo "$runs" | jq -r '.[] | select(.name == "CI pass") | .conclusion')
            echo "[wave-status] CI pass: $verdict"
            echo "$runs" | jq -r '.[] | select(.conclusion != "success" and .conclusion != null) | .name + ": " + .conclusion'
            [[ "$verdict" == "success" ]] && exit 0 || exit 1
        fi
        sleep 20
    done
fi

# --wait-verdict mode (issue #3405) — see the usage comment above for the
# wait/exit semantics; re-reads sha every tick so a stale-SHA verdict (the
# snapshot review column's known staleness gap) never short-circuits the wait.
if [[ $VERDICT_MODE -eq 1 ]]; then
    if [[ -z "$VERDICT_PR" ]]; then
        echo "--wait-verdict requires a PR number" >&2
        exit 2
    fi
    CRITIC_LOGIN="iamacritic[bot]"
    # One unguarded fetch first (mirroring --wait): a bad PR number or auth
    # failure surfaces gh's real error and exits now; the guarded in-loop
    # re-reads absorb only transient hiccups.
    sha=$(gh api "repos/$REPO/pulls/$VERDICT_PR" --jq '.head.sha')
    echo "[wave-status] waiting for a $CRITIC_LOGIN verdict on PR #$VERDICT_PR's current head…"
    while :; do
        sha=$(pr_head_sha "$VERDICT_PR" "$sha")
        # Stream matching reviews per page (chronological), keep the newest
        # verdict on the current head; tail consumes fully, no SIGPIPE.
        verdict=$(gh api "repos/$REPO/pulls/$VERDICT_PR/reviews" --paginate \
            --jq ".[] | select(.user.login == \"$CRITIC_LOGIN\" and .commit_id == \"$sha\"
                       and (.state == \"APPROVED\" or .state == \"CHANGES_REQUESTED\")) | .state" \
            2>/dev/null | tail -1) || verdict=""
        if [[ "$verdict" == "APPROVED" ]]; then
            echo "[wave-status] verdict on ${sha:0:8}…: APPROVED"
            exit 0
        elif [[ "$verdict" == "CHANGES_REQUESTED" ]]; then
            echo "[wave-status] verdict on ${sha:0:8}…: CHANGES_REQUESTED"
            exit 1
        fi
        sleep 20
    done
fi

# Snapshot mode: list open PRs authored by the owner, apply optional filter.
prs_json=$(gh api "repos/$REPO/pulls?state=open&per_page=100" --paginate \
    --jq "[.[] | select(.user.login == \"$OWNER\")]" 2>/dev/null)

total=$(echo "$prs_json" | jq 'length')
if [[ "$total" == "0" ]]; then
    echo "No open PRs authored by $OWNER."
    exit 0
fi

# If positional PR numbers were given, filter to those.
if [[ ${#FILTER_PRS[@]} -gt 0 ]]; then
    filter_json=$(printf '%s\n' "${FILTER_PRS[@]}" | jq -R '.' | jq -sc '.')
    prs_json=$(echo "$prs_json" | jq --argjson f "$filter_json" '[.[] | select([.number | tostring] | inside($f | map(tostring)))]')
fi

# Iterate and print.
while IFS= read -r pr_json; do
    print_pr_line "$pr_json"
done < <(echo "$prs_json" | jq -c '.[]')
