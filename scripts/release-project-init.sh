#!/usr/bin/env bash
# release-project-init.sh — bootstrap GitHub Projects for aether releases.
#
# Two modes:
#
#   release-project-init.sh --init-template [--owner <owner>]
#       One-time: create the release template project. Repurposes the
#       built-in Status field to carry the phase vocabulary (built-in
#       project workflows can only set Status, never a custom field)
#       and creates the remaining custom fields. The two workflow
#       toggles are UI-only (no API exists) — instructions printed.
#
#   release-project-init.sh <version> [--owner <owner>]
#       Per release: copy the template into "aether <version>". The
#       copy carries fields, views, and configured workflows (auto-add
#       workflows are excluded by GitHub, and unused — /sketch adds
#       items itself).
#
# The phase vocabulary lives in the Status field's options; tooling
# (release-state.json, the pipeline skills) keeps calling it "Phase" —
# only the UI header reads "Status" (the built-in field cannot be
# renamed or deleted).

set -euo pipefail

TEMPLATE_TITLE="aether release template"
OWNER="iamacoffeepot"
VERSION=""
INIT_TEMPLATE=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --init-template) INIT_TEMPLATE=1; shift;;
        --owner) OWNER="$2"; shift 2;;
        --*) echo "unknown arg: $1" >&2; exit 64;;
        *) VERSION="$1"; shift;;
    esac
done

if [[ "$INIT_TEMPLATE" -eq 0 && -z "$VERSION" ]]; then
    echo "usage: $0 <version> [--owner <owner>] | $0 --init-template [--owner <owner>]" >&2
    exit 64
fi

if [[ "$INIT_TEMPLATE" -eq 1 ]]; then
    echo "→ Creating template project: ${TEMPLATE_TITLE} (owner: ${OWNER})"
    PROJECT_JSON=$(gh project create --owner "$OWNER" --title "$TEMPLATE_TITLE" --format json)
    PROJECT_URL=$(echo "$PROJECT_JSON" | jq -r '.url')
    PROJECT_NUMBER=$(echo "$PROJECT_JSON" | jq -r '.number')
    echo "  ${PROJECT_URL}"

    echo "  ~ Status (repurposing options to the phase vocabulary)"
    STATUS_FIELD_ID=$(gh project field-list "$PROJECT_NUMBER" --owner "$OWNER" --format json \
        | jq -r '.fields[] | select(.name == "Status") | .id')
    gh api graphql -f query='
        mutation($fieldId: ID!) {
          updateProjectV2Field(input: {
            fieldId: $fieldId,
            singleSelectOptions: [
              {name: "Backlog",   color: GRAY,   description: "resting/default state"},
              {name: "Define",    color: BLUE,   description: "problem statement in progress"},
              {name: "Design",    color: BLUE,   description: "design rationale in progress"},
              {name: "Plan",      color: BLUE,   description: "impl plan written, awaiting /approve"},
              {name: "Ready",     color: GREEN,  description: "approved, ready for an agent"},
              {name: "Executing", color: YELLOW, description: "PR in flight"},
              {name: "Refine",    color: ORANGE, description: "CI loop / draft PR resting state"},
              {name: "Done",      color: PURPLE, description: "merged and closed"},
              {name: "Bounced",   color: RED,    description: "regressed; see the bounce-to:* label"},
              {name: "Stalled",   color: PINK,   description: "env/tooling halt"}
            ]
          }) { projectV2Field { ... on ProjectV2SingleSelectField { id } } }
        }' -f fieldId="$STATUS_FIELD_ID" >/dev/null

    cat <<EOF

✓ Template project ${PROJECT_NUMBER} created.

One-time manual steps (the workflow API is read/delete-only):
  1. Open ${PROJECT_URL}/settings/workflows
  2. "Item added to project" → enable, set Status: Backlog
  3. "Item closed"           → enable, set Status: Done
     (also disable "Pull request merged" if enabled — PRs aren't board items)
  4. Board view → group by Status

These workflows are carried into every copy made from this template.
EOF
    exit 0
fi

TITLE="aether ${VERSION}"
echo "→ Looking up template: ${TEMPLATE_TITLE} (owner: ${OWNER})"
TEMPLATE_NUMBER=$(gh project list --owner "$OWNER" --format json --limit 100 \
    | jq -r --arg t "$TEMPLATE_TITLE" '.projects[] | select(.title == $t) | .number' | head -1)
if [[ -z "$TEMPLATE_NUMBER" ]]; then
    echo "No project titled '${TEMPLATE_TITLE}' found. Run: $0 --init-template" >&2
    exit 1
fi

echo "→ Copying template ${TEMPLATE_NUMBER} → ${TITLE}"
PROJECT_JSON=$(gh project copy "$TEMPLATE_NUMBER" \
    --source-owner "$OWNER" --target-owner "$OWNER" --title "$TITLE" --format json)
PROJECT_URL=$(echo "$PROJECT_JSON" | jq -r '.url')
PROJECT_NUMBER=$(echo "$PROJECT_JSON" | jq -r '.number')

cat <<EOF

✓ Project ${PROJECT_NUMBER} created from template.
  ${PROJECT_URL}

Verify (one minute, in the UI):
  ${PROJECT_URL}/settings/workflows — both workflows should have copied:
    "Item added to project" → Status: Backlog
    "Item closed"           → Status: Done

Programmatic next:
  gh project field-list ${PROJECT_NUMBER} --owner ${OWNER} --format json  # field/option IDs
  (cache the field named "Status" under the key "Phase" in release-state.json)
EOF
