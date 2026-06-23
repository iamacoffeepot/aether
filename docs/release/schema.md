# Release phase schema

Each aether release is tracked entirely on GitHub issue labels — there is no project board. Phase and all issue metadata ride `phase:*` / `type:*` / `size:*` / `model:*` labels, set by the `/release-*` skills, so every pipeline write goes over REST and the contended GraphQL pool stays free (ProjectsV2 is GraphQL-only; the one GraphQL op the pipeline still issues is the PR un-draft at land time). `release-project-init.sh <version>` ensures the label vocabulary exists.

## Phase — the `phase:*` label

The lifecycle vocabulary is the `phase:*` label set, the canonical phase state for an issue. Backlog and Done are **label-absence**: a fresh issue carries no `phase:*` label (Backlog), a merged/closed issue has its `phase:*` label deleted (Done), and every active phase in between carries its own label. The skills read lifecycle state off labels over REST (`labels=phase:ready`, …) — discovering, enumerating, and gating without any board query.

| Phase     | Label             | Meaning                                            | Advances by      |
|-----------|-------------------|----------------------------------------------------|------------------|
| Backlog   | *(no label)*      | Not yet picked up for this release                 | User             |
| Define    | `phase:define`    | Problem framing in progress                        | User + Claude    |
| Design    | `phase:design`    | Tradeoffs / options / ADR drafting                 | User + Claude    |
| Plan      | `phase:plan`      | Sequencing, dependencies, sub-issues               | User + Claude    |
| Ready     | `phase:ready`     | Agent-ready; awaiting dispatch                     | Gate: `/approve` |
| Executing | `phase:executing` | Agent has the issue; PR in flight                  | Auto             |
| Refine    | `phase:refine`    | CI-feedback loop, agent-driven                     | Auto             |
| Done      | *(no label)*      | PR merged, issue closed                            | Auto             |
| Bounced   | `phase:bounced`   | Phase regression — see the `bounce-to:*` label     | User triage      |
| Stalled   | `phase:stalled`   | Env/tooling failure, blocks dispatch               | User triage      |

## Issue metadata — all labels

Phase and every other axis ride labels — durable, REST-cheap, and the signal the skills actually read:

| Metadata      | Lives as                                | Set by | Notes |
|---------------|-----------------------------------------|--------|-------|
| Type          | `type:*` label                          | `/sketch` at filing | Mirrors the conventional-commit prefix |
| Size          | `size:s\|m\|l` label                    | `/scope` at Plan | Dispatch context-cost prior; `size:xl` marks a fat issue needing breakdown |
| Model route   | `model:*` label                         | `/scope` at Plan | Routes the implementing agent's model |
| Agent-ready   | `phase:ready` label                     | `/approve` | "Ready" *is* the eligibility signal |
| Bounce target | `bounce-to:plan\|design\|define` label  | `/bounce` (or a self-bouncing skill) | Present only while `phase:bounced`; `/scope` reads it to resume, then clears it |

The ADR link lives in the issue's `## Design notes` section; per-issue auth budgets aren't persisted in v1 (a breach is noted in the self-bounce comment).

## Issue dependencies

GitHub's native feature, not a custom field: `gh issue edit <n> --add-dependency <m>` and the dependency graph view.

## Phase-transition rules (enforced by the `/release-*` skills)

```
Backlog  → Define     body has a problem statement
Define   → Design     if multi-PR, umbrella issue exists; if architectural, ADR drafted
Design   → Plan       tradeoffs aired; ADR merged if applicable
Plan     → Ready      dependencies declared, one concept per issue (sets phase:ready)
Ready    → Executing  /implement run (manually or by the fleet executor)
Executing → Refine    PR opened, CI running
Refine   → Done       CI green, merged (deletes the phase:* label)
Executing/Refine → Bounced   agent surfaced an upstream-phase issue (sets bounce-to:*)
Any      → Stalled    env/tooling failure (not the issue's fault)
```

## Operations

- **Bootstrap a release:** `release-project-init.sh <version>` — ensures the `phase:*` / `bounce-to:*` / `size:*` / `model:*` labels exist (REST `gh label create`).
- **File an issue:** `/sketch` — REST `POST …/issues` with the `type:*` (and `crate:*`) labels; a fresh issue is Backlog by label-absence.
- **Advance phase:** the `/release-*` skills swap the `phase:*` label atomically over REST (`PUT …/issues/<n>/labels`, replacing the prior `phase:*` with the new one); Backlog and Done delete the label rather than swap it.

Every operation rides REST — there is no board to write.
