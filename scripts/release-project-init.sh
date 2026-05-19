#!/usr/bin/env bash
# release-project-init.sh — create a GitHub Project v2 for an aether release.
#
# Usage:
#   release-project-init.sh <version> [--owner <owner>]
#
# Example:
#   release-project-init.sh 0.4
#   release-project-init.sh 0.4-sandbox --owner iamacoffeepot
#
# Idempotent? No. Creates a fresh project each call.

set -euo pipefail

VERSION="${1:?usage: $0 <version> [--owner <owner>]}"
shift || true

OWNER="iamacoffeepot"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --owner) OWNER="$2"; shift 2;;
        *) echo "unknown arg: $1" >&2; exit 64;;
    esac
done

TITLE="aether ${VERSION}"

echo "→ Creating project: ${TITLE} (owner: ${OWNER})"
PROJECT_JSON=$(gh project create --owner "$OWNER" --title "$TITLE" --format json)
PROJECT_URL=$(echo "$PROJECT_JSON" | jq -r '.url')
PROJECT_NUMBER=$(echo "$PROJECT_JSON" | jq -r '.number')
echo "  ${PROJECT_URL}"

create_select() {
    local name="$1" options="$2"
    echo "  + ${name} (single-select)"
    gh project field-create "$PROJECT_NUMBER" --owner "$OWNER" \
        --name "$name" --data-type SINGLE_SELECT \
        --single-select-options "$options" >/dev/null
}

create_text() {
    local name="$1"
    echo "  + ${name} (text)"
    gh project field-create "$PROJECT_NUMBER" --owner "$OWNER" \
        --name "$name" --data-type TEXT >/dev/null
}

create_select "Phase"      "Backlog,Define,Design,Plan,Ready,Executing,Refine,Done,Bounced,Stalled"
create_select "Type"       "feat,fix,chore,docs,refactor,ci,test"
create_select "Size"       "S,M,L"
create_select "AgentReady" "No,Yes"
create_select "BounceTo"   "Plan,Design,Define"
create_text   "ADR"
create_text   "AuthBudget"

cat <<EOF

✓ Project ${PROJECT_NUMBER} created.

Next steps (manual, in the UI):
  1. Open ${PROJECT_URL}
  2. Board view → group by Phase (not the default Status)
  3. Optionally hide the default Status field

Programmatic next:
  gh project item-add ${PROJECT_NUMBER} --owner ${OWNER} --url <issue-url>
  gh project field-list ${PROJECT_NUMBER} --owner ${OWNER} --format json  # for field/option IDs
EOF
