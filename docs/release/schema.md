# Release project schema

Each aether release lives in its own GitHub Project (`aether 0.4`, `aether 0.5`, …), copied from the release template by `release-project-init.sh <version>` and driven by the `/release-*` skills.

## Phase (the board's one field — its grouping column)

The lifecycle vocabulary lives in the built-in **Status** field, whose options `release-project-init.sh --init-template` replaces with the phase set (GitHub's built-in workflows can only set Status, so the vocabulary has to live there). Everything else — the skills, `release-state.json`, this doc — calls it **Phase**: `release-state.json`'s `field_cache` key `"Phase"` points at the field named `Status`. Group the board view by Status to get the phase columns.

| Phase     | Meaning                                                    | Advances by  |
|-----------|------------------------------------------------------------|--------------|
| Backlog   | Not yet picked up for this release                         | User         |
| Define    | Problem framing in progress                                | User + Claude |
| Design    | Tradeoffs / options / ADR drafting                         | User + Claude |
| Plan      | Sequencing, dependencies, sub-issues                       | User + Claude |
| Ready     | Agent-ready; awaiting dispatch                             | Gate: `phase:ready` label |
| Executing | Agent has the issue; PR in flight                          | Auto         |
| Refine    | CI-feedback loop, agent-driven                             | Auto         |
| Done      | PR merged                                                  | Auto         |
| Bounced   | Phase regression — see the `bounce-to:*` label             | User triage  |
| Stalled   | Env/tooling failure, blocks dispatch                       | User triage  |

Phase is mirrored to a `phase:*` label on each issue (the board field isn't visible in `gh issue list`), so the skills read lifecycle state off labels over REST and never need a board query to enumerate or gate.

## Issue metadata — labels, not board fields

The board carries **only** Phase. Everything that used to be a custom single-select rides on issue labels instead — durable, REST-cheap, and the signal the skills actually read:

| Metadata      | Lives as                                | Set by | Notes |
|---------------|-----------------------------------------|--------|-------|
| Type          | `type:*` label                          | `/sketch` at filing | Mirrors the conventional-commit prefix |
| Size          | `size:s\|m\|l` label                    | `/scope` at Plan | Dispatch context-cost prior; `size:xl` marks a fat issue needing breakdown |
| Model route   | `model:*` label                         | `/scope` at Plan | Routes the implementing agent's model |
| Agent-ready   | `phase:ready` label                     | `/approve` | "Ready" *is* the eligibility signal — there is no separate field |
| Bounce target | `bounce-to:plan\|design\|define` label  | `/bounce` (or a self-bouncing skill) | Present only while `Phase=Bounced`; `/scope` reads it to resume, then clears it |

The ADR link lives in the issue's `## Design notes` section; per-issue auth budgets aren't persisted in v1 (a breach is noted in the self-bounce comment).

## Native fields used

- `Repository`: auto-populated when issues are added.
- Issue dependencies: GH's native feature, not a custom field. `gh issue edit <n> --add-dependency <m>` and the dependency graph view.

## Phase-transition rules (enforced by the `/release-*` skills)

```
Backlog  → Define     body has a problem statement
Define   → Design     if multi-PR, umbrella issue exists; if architectural, ADR drafted
Design   → Plan       tradeoffs aired; ADR merged if applicable
Plan     → Ready      dependencies declared, one concept per issue (sets phase:ready)
Ready    → Executing  /implement run (manually or by the fleet executor)
Executing → Refine    PR opened, CI running
Refine   → Done       CI green, merged
Executing/Refine → Bounced   agent surfaced an upstream-phase issue (sets bounce-to:*)
Any      → Stalled    env/tooling failure (not the issue's fault)
```

## Operations

- **Create release project:** `release-project-init.sh 0.4`
- **Add issue to project:** `gh project item-add <project> --owner iamacoffeepot --url <issue-url>`
- **Set Phase on an issue:** the `/release-*` skills write it via `updateProjectV2ItemFieldValue` (Phase field/option IDs cached in `release-state.json`), mirroring the `phase:*` label in the same step.

Field and option IDs aren't human-readable; the `/release-*` skills cache them at project-create time so callers use field names directly.
