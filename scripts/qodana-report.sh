#!/usr/bin/env bash
# Fetch and triage a PR's Qodana findings from CI's `qodana-report`
# artifact — the local-scan replacement (issue 1921). Qodana is a
# required CI gate (`ci-pass`), not a local pre-flight step; when a PR's
# `Qodana scan` check is red, `/land` runs this to pull the findings,
# filter them to the PR's own changes, and print an actionable list to
# resolve in the worktree.
#
# Usage:
#   scripts/qodana-report.sh <pr>          # findings on the PR's changed files
#   scripts/qodana-report.sh <pr> --all    # every finding in the artifact
#
# Read-only: downloads the artifact to a temp dir, parses the SARIF, and
# prints a per-file grouped summary with a total. Exits non-zero when
# findings land on the PR's own changes (the set `/land` must resolve);
# exits 3 when the artifact is missing (the Qodana job likely crashed —
# surface to the user rather than treating it as "no findings").

set -euo pipefail

repo=iamacoffeepot/aether
pr="${1:?usage: qodana-report.sh <pr> [--all]}"
all=0
[[ "${2:-}" == "--all" ]] && all=1

for tool in gh jq unzip; do
    command -v "$tool" >/dev/null || { echo "qodana-report: '$tool' not found" >&2; exit 2; }
done

# The PR's head branch + sha (REST; `gh pr view` is GraphQL-backed).
head_ref=$(gh api "repos/$repo/pulls/$pr" --jq '.head.ref')
head_sha=$(gh api "repos/$repo/pulls/$pr" --jq '.head.sha')

# The CI workflow run for that head sha — it carries the `qodana-report`
# artifact the Qodana job uploads.
run_id=$(gh api "repos/$repo/actions/runs?head_sha=$head_sha&event=pull_request" \
    --jq '[.workflow_runs[] | select(.name == "CI")] | .[0].id // empty')
if [[ -z "$run_id" ]]; then
    echo "qodana-report: no CI workflow run found for $head_ref ($head_sha)" >&2
    exit 3
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if ! gh run download "$run_id" -n qodana-report -D "$tmp" 2>/dev/null; then
    echo "qodana-report: no 'qodana-report' artifact on CI run $run_id —" >&2
    echo "  the Qodana job likely crashed (EAP infra). Surface to the user; do not assume zero findings." >&2
    exit 3
fi

# The artifact may carry a nested qodana-report.zip → unpacked/qodana.sarif.json.
nested=$(find "$tmp" -name 'qodana-report.zip' | head -1)
if [[ -n "$nested" ]]; then
    # unzip exits 1 on benign "stripped absolute path spec" warnings even
    # though it extracted every file successfully. Tolerate it: the
    # qodana.sarif.json presence check below is the real success gate — if
    # the extraction actually produced nothing, we fall through to exit 3.
    unzip -q -o "$nested" -d "$tmp/unpacked" || true
fi
sarif=$(find "$tmp" -name 'qodana.sarif.json' | head -1)
if [[ -z "$sarif" ]]; then
    echo "qodana-report: no qodana.sarif.json inside the artifact" >&2
    exit 3
fi

# The PR's changed-file set, for the default filter.
git fetch origin --quiet 2>/dev/null || true
mapfile -t changed < <(git diff --name-only "origin/main...$head_sha" 2>/dev/null \
    || git diff --name-only origin/main)
is_changed() {
    local f="$1"
    for c in "${changed[@]}"; do [[ "$c" == "$f" ]] && return 0; done
    return 1
}

# Parse each SARIF result into `uri<TAB>line<TAB>severity<TAB>ruleId<TAB>message`.
mapfile -t rows < <(jq -r '
  .runs[].results[]
  | ((.locations[0].physicalLocation) // {}) as $pl
  | [ ($pl.artifactLocation.uri // "?"),
      ($pl.region.startLine // 0 | tostring),
      (.properties.qodanaSeverity // .level // "?"),
      (.ruleId // "?"),
      ((.message.text // "") | gsub("[\t\n]"; " ")) ]
  | @tsv
' "$sarif")

declare -A by_file
total=0
on_diff=0
for row in "${rows[@]}"; do
    IFS=$'\t' read -r uri line sev rule msg <<<"$row"
    if (( ! all )) && ! is_changed "$uri"; then
        continue
    fi
    diff_marker=""
    if is_changed "$uri"; then diff_marker=" *"; on_diff=$((on_diff + 1)); fi
    by_file["$uri"]+="  $uri:$line  [$sev] $rule — $msg$diff_marker"$'\n'
    total=$((total + 1))
done

scope=$([[ $all -eq 1 ]] && echo "whole-tree" || echo "PR-diff")
echo "Qodana findings for PR #$pr ($head_ref) — $scope scope, from CI run $run_id"
echo
if (( total == 0 )); then
    echo "  none."
else
    for f in $(printf '%s\n' "${!by_file[@]}" | sort); do
        echo "$f:"
        printf '%s' "${by_file[$f]}"
    done
    echo
fi
echo "total: $total  |  on the PR's changed files: $on_diff  (* = on the PR diff)"

# Non-zero when findings land on the PR's own changes — the set /land resolves.
(( on_diff == 0 ))
