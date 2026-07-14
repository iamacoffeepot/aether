#!/usr/bin/env bash
# Read-only scorecard over the fleet spend ledger (rung 1, #3166). Lists the
# trailing N UTC days' per-run usage records under s3://<bucket>/agent/usage/,
# reads each object, and prints the legibility rung 1 (and rung 2's weekly
# report, #3382) delivers on its own: spend per issue/ref, per task, and per
# model; cost-per-landed-PR; warm-vs-cold per task; the TTL-drift signals (the
# ephemeral_5m occurrence rate and the warm-resume cache-hit ratio); and
# verdict/refine quality signals (degrading to n/a until the write side
# populates them). Pure jq + aws s3 + shell, no build, no writes.
#
#   scripts/agent-scorecard.sh [--days N] [YYYY-MM-DD]
#     --days N     aggregate the trailing N UTC days ending on the end-day
#                  (default 1 — today's single-day behavior is unchanged)
#     YYYY-MM-DD   end day of the window; defaults to today (UTC)
#
# Env: AGENT_EVIDENCE_BUCKET (default aether-evidence). Reads on ambient creds
# (the RunsOn instance profile, or a developer's own AWS profile). `gh` is used
# only to mark which refs are merged PRs for the cost-per-landed-PR line; absent
# `gh`, that line falls back to cost-per-ref without the merged join.
set -uo pipefail

DAYS=1
END_DAY="$(date -u +%F)"
while [ $# -gt 0 ]; do
  case "$1" in
    --days)
      DAYS="$2"
      shift 2
      ;;
    *)
      END_DAY="$1"
      shift
      ;;
  esac
done
BUCKET="${AGENT_EVIDENCE_BUCKET:-aether-evidence}"

command -v aws >/dev/null || { echo "aws cli not found" >&2; exit 1; }
command -v jq  >/dev/null || { echo "jq not found" >&2; exit 1; }

# The N most recent UTC days ending on END_DAY, newest first.
days=()
for ((k = 0; k < DAYS; k++)); do
  days+=("$(date -u -d "${END_DAY} - ${k} days" +%F)")
done
range_newest="${days[0]}"
range_oldest="${days[$((${#days[@]} - 1))]}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# One object per run across the whole window — concatenate into a single
# JSONL stream for jq. The existing jq -s aggregation below is window-agnostic
# (it just aggregates whatever rows land in $ledger), so it is unchanged by
# the day count; only the fetch loop and the header are range-aware.
ledger="$tmp/ledger.jsonl"
: > "$ledger"
found=0
for d in "${days[@]}"; do
  daydir="$tmp/${d}"
  mkdir -p "$daydir"
  aws s3 cp "s3://${BUCKET}/agent/usage/${d}/" "$daydir/" --recursive --exclude '*' --include '*.json' >/dev/null 2>&1 \
    || continue
  for f in "$daydir"/*.json; do
    [ -e "$f" ] || continue
    cat "$f" >> "$ledger"
    echo >> "$ledger"
    found=$((found + 1))
  done
done
if [ "$found" -eq 0 ]; then
  if [ "$DAYS" -eq 1 ]; then
    echo "no ledger objects under s3://${BUCKET}/agent/usage/${END_DAY}/ (empty day, or read denied)"
  else
    echo "no ledger objects under s3://${BUCKET}/agent/usage/ for ${range_oldest}..${range_newest} (empty window, or read denied)"
  fi
  exit 0
fi

if [ "$DAYS" -eq 1 ]; then
  echo "Spend scorecard — ${END_DAY} (${found} runs, s3://${BUCKET}/agent/usage/${END_DAY}/)"
else
  echo "Spend scorecard — ${range_oldest}..${range_newest} (${DAYS} days, ${found} runs, s3://${BUCKET}/agent/usage/)"
fi
echo

jq -s -r '
  def usd: if . == null then 0 else . end;
  def money($x): "$" + ($x * 100 | round / 100 | tostring);

  (map(.cost_usd | usd) | add) as $total
  | "Total spend: \(money($total))   mean/run: \(money($total / (length)))"
  , ""
  , "By task:"
  , ( group_by(.task // "unknown")
      | map({task: (.[0].task // "unknown"), cost: (map(.cost_usd|usd)|add), runs: length})
      | sort_by(-.cost)[]
      | "  \(.task)  \(money(.cost))  (\(.runs) runs)" )
  , ""
  , "By model:"
  , ( group_by(.model // "unknown")
      | map({model: (.[0].model // "unknown"), cost: (map(.cost_usd|usd)|add), runs: length})
      | sort_by(-.cost)[]
      | "  \(.model)  \(money(.cost))  (\(.runs) runs)" )
  , ""
  , "By ref (issue/PR):"
  , ( group_by(.ref // "none")
      | map({ref: (.[0].ref // "none"), cost: (map(.cost_usd|usd)|add), runs: length})
      | sort_by(-.cost)[]
      | "  #\(.ref)  \(money(.cost))  (\(.runs) runs)" )
  , ""
  , "Warm-vs-cold per task (pool boxes):"
  , ( map(select(.pool != null))
      | if length == 0 then "  (no pool-metered runs)"
        else ( group_by(.task // "unknown")
               | map({ task: (.[0].task // "unknown"),
                       warm: (map(select(.pool.warm == true)) | length),
                       cold: (map(select(.pool.warm != true)) | length),
                       cost: (map(.cost_usd|usd)|add) })[]
               | "  \(.task)  warm \(.warm) / cold \(.cold)  \(money(.cost))" )
        end )
  , ""
  , "TTL-drift signals:"
  , ( (map(select((.cache_write_5m // 0) > 0)) | length) as $drift
      | "  ephemeral_5m runs: \($drift) / \(length)  (the 5m bucket lighting up = provider TTL dropped to the 5m class)" )
  , ( map(select(.pool.warm == true and (.first_call_cache_read // 0) > 0))
      | if length == 0 then "  warm-resume hit ratio: (no warm runs with a first-call read)"
        else ( map((.first_call_cache_read // 0)
                   / (((.first_call_cache_read // 0) + (.first_call_cache_write // 0) + (.first_call_input // 0)) | if . == 0 then 1 else . end))
               | (add / length) ) as $ratio
             | "  warm-resume hit ratio: \(($ratio * 1000 | round / 10))%  (first-call cache_read share, avg over warm runs)"
        end )
  , ""
  , "Quality signals:"
  , ( if (map(select(.verdict != null)) | length) == 0 and (map(select(.refine_iterations != null)) | length) == 0
      then "  n/a — not yet in ledger record (verdict/refine sourcing is a write-side follow-up)"
      else ( group_by(.task // "unknown")
             | map({
                 task: (.[0].task // "unknown"),
                 verdicts: (map(select(.verdict != null) | .verdict) | group_by(.) | map({key: .[0], count: length})),
                 mean_refine: ( (map(select(.refine_iterations != null) | .refine_iterations)) as $r
                   | if ($r | length) == 0 then null else (($r | add) / ($r | length)) end )
               })[]
             | "  \(.task)  verdicts \(if (.verdicts | length) == 0 then "n/a" else (.verdicts | map("\(.key)=\(.count)") | join(", ")) end)  mean-refine \(.mean_refine // "n/a")" )
      end )
' "$ledger"

# cost-per-landed-PR: total spend attributed to refs whose PR merged. Needs a
# merged-status lookup, so it is a `gh` best-effort join rather than a pure jq
# aggregate; without `gh` (or its auth) it degrades to a plain cost-per-ref note.
echo
if command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
  repo="${GH_REPO:-iamacoffeepot/aether}"
  refs="$(jq -r '.ref // empty' "$ledger" | sort -u)"
  landed_cost=0
  landed_n=0
  for ref in $refs; do
    [[ "$ref" =~ ^[0-9]+$ ]] || continue
    merged="$(gh api "repos/${repo}/pulls/${ref}" --jq '.merged' 2>/dev/null || echo false)"
    if [ "$merged" = "true" ]; then
      c="$(jq -s "[.[] | select(.ref == \"$ref\") | .cost_usd // 0] | add" "$ledger")"
      landed_cost="$(jq -n "$landed_cost + $c")"
      landed_n=$((landed_n + 1))
    fi
  done
  if [ "$landed_n" -gt 0 ]; then
    printf 'Cost-per-landed-PR: $%.2f over %d landed (total $%.2f)\n' \
      "$(jq -n "$landed_cost / $landed_n")" "$landed_n" "$(jq -n "$landed_cost")"
  else
    echo "Cost-per-landed-PR: no merged PRs among today's refs yet"
  fi
else
  echo "Cost-per-landed-PR: (gh unavailable — showing cost-per-ref above; the merged join needs gh)"
fi
