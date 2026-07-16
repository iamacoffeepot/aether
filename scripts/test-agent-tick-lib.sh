#!/usr/bin/env bash
# test-agent-tick-lib.sh — unit tests for the pure predicates in
# scripts/agent-tick-lib.sh. Exercises the #3484 umbrella discriminator
# (sub_issue_numbers + body_depends_on): a non-empty ## Sub-issues section is
# an implementable slice only when every listed child back-references its parent
# via ## Depends on. Exits non-zero on any failed assertion (the
# test-surface-match.py convention). Wired into CI's Approval-machinery job.
set -euo pipefail

# The lib keys some functions on $REPO / $OWNER; set placeholders so a bare
# source under `set -u` cannot trip on them (the predicates under test read
# only stdin and $1, but sourcing evaluates the whole file).
REPO="${REPO:-owner/repo}" OWNER="${OWNER:-owner}"

# shellcheck source=scripts/agent-tick-lib.sh
source "$(dirname "$0")/agent-tick-lib.sh"

fail=0
check() {
  # check <label> <expected> <actual>
  if [ "$2" = "$3" ]; then
    printf '  ok   %s\n' "$1"
  else
    printf '  FAIL %s — expected [%s], got [%s]\n' "$1" "$2" "$3"
    fail=1
  fi
}

# body_depends_on returns 0/1 as a predicate; capture that as a "true"/"false".
depends_on() {
  # depends_on <parent> <body> → prints true/false
  if body_depends_on "$1" <<<"$2"; then printf 'true'; else printf 'false'; fi
}

echo "body_depends_on:"
check "canonical bullet form" true \
  "$(depends_on 100 "$(printf '## Depends on\n- #100 — needs parent\n')")"
check "non-matching bullet" false \
  "$(depends_on 100 "$(printf '## Depends on\n- #200 — other\n')")"
check "no Depends on section" false \
  "$(depends_on 100 "$(printf '## Summary\nsome text\n')")"
check "inline header form" true \
  "$(depends_on 100 "$(printf '## Depends on: #100\n')")"
check "whole-token, no prefix match" false \
  "$(depends_on 10 "$(printf '## Depends on\n- #100 — needs parent\n')")"
check "whole-token, no suffix match" false \
  "$(depends_on 100 "$(printf '## Depends on\n- #1000 — other\n')")"
check "ref outside Depends on section ignored" false \
  "$(depends_on 100 "$(printf '## Summary\ncloses #100 eventually\n## Depends on\n- #200 — other\n')")"

echo "sub_issue_numbers:"
check "two bullet children" "3461 3462" \
  "$(printf '## Sub-issues\n- #3461 host crate\n- #3462 store cap\n\n## Next\n- #9999 unrelated\n' \
    | sub_issue_numbers | tr '\n' ' ' | sed 's/ $//')"
check "empty Sub-issues section" "" \
  "$(printf '## Sub-issues\n\n## Next\n- #9999 unrelated\n' | sub_issue_numbers | tr -d '\n')"
check "no Sub-issues section" "" \
  "$(printf '## Summary\n- #42 not a sub-issue\n' | sub_issue_numbers | tr -d '\n')"
check "dedup + sort" "10 20" \
  "$(printf '## Sub-issues\n- #20 b\n- #10 a\n- #20 dup\n' | sub_issue_numbers | tr '\n' ' ' | sed 's/ $//')"

if [ "$fail" -ne 0 ]; then
  echo "FAILED"
  exit 1
fi
echo "PASSED"
