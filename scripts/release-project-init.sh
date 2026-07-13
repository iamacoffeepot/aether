#!/usr/bin/env bash
# release-project-init.sh — bootstrap the label vocabulary for aether releases.
#
#   release-project-init.sh <version> [--owner <owner>]
#       Ensure the phase / bounce-to / approval / size / model labels exist on
#       the repo.
#       Idempotent — a re-run only fills gaps.
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
ensure_label "phase:building"  fbca04 "PR open; CI not yet green (reconciler-computed)"
ensure_label "phase:qa"        d4c5f9 "CI green; review/dogfood verdict owed (reconciler-computed)"
ensure_label "phase:findings"  d93f0b "QA findings open — threads or rollups unresolved (reconciler-computed)"
ensure_label "phase:held"      0e8a16 "CI green, QA complete, all threads resolved; land-eligible (reconciler-computed)"
ensure_label "phase:bounced"   b60205 "regressed; see the bounce-to:* label"
ensure_label "phase:stalled"   e99695 "env/tooling halt"

# Resume targets stamped by /bounce.
ensure_label "bounce-to:define" c5def5 "/scope resumes from Define"
ensure_label "bounce-to:design" c5def5 "/scope resumes from Design"
ensure_label "bounce-to:plan"   c5def5 "/scope resumes from Plan"

# Declared-surface enforcement, carried on the PR (not the issue). The
# reconciler sets approval:surface-exceeded when a PR's diff escapes its issue's
# ## Declared surface and mirrors it into the required `Approval gate` commit
# status; approval:surface-ok is the owner's waiver, honoured only when the
# owner applied it (timeline actor check).
ensure_label "approval:surface-exceeded" d93f0b "PR diff escapes the issue's declared surface — re-approval owed"
ensure_label "approval:surface-ok"       0e8a16 "owner waiver: declared-surface overreach accepted"

# The cloud fleet's control surface (ADR-0146). dont-touch is the per-issue kill
# switch — the dispatcher, the judge, and the executor all refuse a benched issue.
# These belong in the bootstrap: a label that does not exist cannot be applied, so
# without them the kill switch is unusable on a fresh repo.
ensure_label "agent:dont-touch"      000000 "fleet-blind: the dispatcher, judge, and executor skip this issue. Only the owner removes it."
ensure_label "agent:awaiting-answer" fbca04 "an agent parked a question here; an owner reply resumes its session"

# The approve sweep's verdict digest (#3190) — a machine-filed issue listing the
# non-auto phase:plan batch; the owner's single reply approves the batch and
# closes it. A sibling of `alert` (machine-filed, never a work item) with the
# opposite ask: an alert says "go look at a failure", a digest says "reply to
# finish a workflow". Open = awaiting the verdict; closed = done.
ensure_label "agent:digest" 1d76db "approve-sweep verdict digest — one owner reply approves the listed batch"

# Machine-filed alerts (the context-budget canary, the nightly fuzzer). They carry
# no type:* label because they never went through /sketch, so without this tag the
# dispatcher reads them as Backlog and scopes them. The alert-filing workflows
# stamp it themselves; it is bootstrapped here because a label that does not exist
# cannot be applied.
ensure_label "alert" b60205 "machine-filed alert, not a work item — the fleet never scopes these"

# The owner's per-issue approval override. Resolves an issue's approval tier to
# `auto` whatever the policy says — but it can never pass the ADR hard gate, and
# it counts only when the OWNER applied it (the tick verifies the timeline actor).
ensure_label "approval:pre-approved" 0e8a16 "owner: approve this one regardless of tier (never an ADR)"

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

echo ""
echo "✓ Labels ensured for aether ${VERSION}."
