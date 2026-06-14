#!/usr/bin/env bash
# release-project-init.sh — bootstrap the label vocabulary for aether releases.
#
#   release-project-init.sh <version> [--owner <owner>]
#       Ensure the phase / bounce-to / size / model labels exist on the repo,
#       then print the minimal release-state.json the /release-init skill
#       writes. Idempotent — a re-run only fills gaps.
#
# Issue phase is carried entirely by phase:* labels: Backlog and Done are
# label-absence, each active phase has its own label. size:* and model:* carry
# the routing metadata /scope stamps at Plan. There is no project board — every
# pipeline write rides REST, so the contended GraphQL pool stays free.

set -euo pipefail

OWNER="iamacoffeepot"
REPO="aether"
VERSION=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --owner) OWNER="$2"; shift 2;;
        --*) echo "unknown arg: $1" >&2; exit 64;;
        *) VERSION="$1"; shift;;
    esac
done

if [[ -z "$VERSION" ]]; then
    echo "usage: $0 <version> [--owner <owner>]" >&2
    exit 64
fi

ensure_label() {
    # ensure_label <name> <color> <description>
    # gh label create rides the REST labels API; --force updates if it exists.
    gh label create "$1" --repo "$OWNER/$REPO" --color "$2" --description "$3" --force >/dev/null
}

echo "→ Ensuring pipeline labels on ${OWNER}/${REPO}"

# Phase vocabulary — Backlog and Done are label-absence, so they carry no label.
ensure_label "phase:define"    1d76db "problem statement in progress"
ensure_label "phase:design"    1d76db "design rationale in progress"
ensure_label "phase:plan"      1d76db "impl plan written, awaiting /approve"
ensure_label "phase:ready"     0e8a16 "approved, ready for an agent"
ensure_label "phase:executing" fbca04 "PR in flight"
ensure_label "phase:refine"    d93f0b "CI loop / draft PR resting state"
ensure_label "phase:bounced"   b60205 "regressed; see the bounce-to:* label"
ensure_label "phase:stalled"   e99695 "env/tooling halt"

# Resume targets stamped by /bounce.
ensure_label "bounce-to:define" c5def5 "/scope resumes from Define"
ensure_label "bounce-to:design" c5def5 "/scope resumes from Design"
ensure_label "bounce-to:plan"   c5def5 "/scope resumes from Plan"

# Size (weight) — XL marks a fat issue for /sweep fat (ADR-0110).
ensure_label "size:s"  bfdadc "single file, single concept"
ensure_label "size:m"  bfdadc "single crate, multiple files"
ensure_label "size:l"  bfdadc "cross-crate or architectural"
ensure_label "size:xl" 5319e7 "fat — needs /sweep fat breakdown"

# Model routing stamped by /scope at Plan.
ensure_label "model:haiku"  fef2c0 "trivial text-only work"
ensure_label "model:sonnet" fef2c0 "mechanical, fully-specified work"
ensure_label "model:opus"   fef2c0 "judgment / cross-crate / design-adjacent"
ensure_label "model:fable"  fef2c0 "top tier, pinned by a human"

cat <<EOF

✓ Labels ensured for aether ${VERSION}.

Write .claude/release-state.json (the /release-init skill does this):
  {
    "release_version": "${VERSION}",
    "owner": "${OWNER}"
  }
EOF
