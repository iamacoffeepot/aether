# Release phase schema

Each Aether release is tracked entirely on GitHub issue labels — there is no
project board. Phase and all issue metadata ride `phase:*` / `type:*` / `size:*`
/ `model:*` labels. The executable Codex contracts are the matching `$scope`,
`$approve`, `$implement`, `$findings`, `$land`, `$bounce`, and `$release-init`
skills plus their shared
[GitHub workflow contract](https://github.com/iamacoffeepot/aether/blob/main/.agents/skills/_shared/github-workflow.md).
Read those sources for exact mutations; this page owns the lifecycle vocabulary.

## Phase — the `phase:*` label

The lifecycle vocabulary is the `phase:*` label set, the canonical phase state for an issue. Backlog and Done are **label-absence**: a fresh issue carries no `phase:*` label (Backlog), a merged/closed issue has its `phase:*` label deleted (Done), and every active phase in between carries its own label. The skills read lifecycle state off labels over REST (`labels=phase:ready`, …) — discovering, enumerating, and gating without any board query.

| Phase     | Label             | Meaning                                            | Advances by      |
|-----------|-------------------|----------------------------------------------------|------------------|
| Backlog   | *(no label)*      | Not yet picked up for this release                 | User             |
| Define    | `phase:define`    | Problem framing in progress                        | User + `$scope`  |
| Design    | `phase:design`    | Tradeoffs / options / ADR drafting                 | User + `$scope`  |
| Plan      | `phase:plan`      | Sequencing, dependencies, declared surface          | User + `$scope`  |
| Ready     | `phase:ready`     | Agent-ready; awaiting dispatch                     | Gate: `$approve` |
| Building  | `phase:building`  | Contracted state: PR head unproven, CI not green, or declared-surface gate red | Intended reconciler |
| QA        | `phase:qa`        | Contracted state: CI green; review/dogfood verdict owed | Intended reconciler |
| Findings  | `phase:findings`  | Contracted state: QA findings remain open          | Intended reconciler |
| Held      | `phase:held`      | Contracted state: CI and QA clear; land-eligible   | Intended reconciler |
| Done      | *(no label)*      | PR merged, issue closed                            | Auto             |
| Bounced   | `phase:bounced`   | Phase regression — see the `bounce-to:*` label     | User triage      |
| Stalled   | `phase:stalled`   | Env/tooling failure, blocks dispatch               | User triage      |

`Building` / `QA` / `Findings` / `Held` are computed post-Ready resting states
in the Codex lifecycle contract. The contract reserves their writes for a
reconciler, so skills do not assert those labels themselves. The checked-in
`main` tree does not currently contain `.github/workflows/reconciler.yml` (or
the hosted Review and Dogfood workflows that would supply its QA facts).
Consequently the phase meanings remain the intended contract, but the hosted
post-Ready transitions are unavailable until that machinery is checked in.

`Executing` and `Refine` are retired vocabulary. Current skills never write
them, and `release-project-init.sh` no longer creates them.

## Issue metadata — all labels

Phase and every other axis ride labels — durable, REST-cheap, and the signal the skills actually read:

| Metadata      | Lives as                                | Set by | Notes |
|---------------|-----------------------------------------|--------|-------|
| Type          | `type:*` label                          | `$sketch` at filing | Mirrors the conventional-commit prefix |
| Size          | `size:s\|m\|l` label                    | `$scope` at Plan | Dispatch context-cost prior; `size:xl` marks a fat issue needing breakdown |
| Model route   | `model:*` label                         | `$scope` at Plan | Routes the implementing agent's model |
| Agent-ready   | `phase:ready` label                     | `$approve` | "Ready" *is* the eligibility signal |
| Bounce target | `bounce-to:plan\|design\|define` label  | `$bounce` (or a self-bouncing skill) | Present only while `phase:bounced`; `$scope` reads it to resume, then clears it |

The ADR link lives in the issue's `## Design notes` section; per-issue auth budgets aren't persisted in v1 (a breach is noted in the self-bounce comment).

At Plan, `$scope` emits a fenced `## Declared surface` glob list. `$approve`
validates that it covers the planned targets and resolves the most restrictive
`auto|judge|human` tier from `approval-policy.yml` over every path
each declaration can permit, including higher-tier files inside a declared
subtree. An explicit `ADR flag:` or declared ADR edit makes the issue
ADR-bearing; the maturity-aware ADR hard gate (ADR-0146 §6) human-routes a new
or established (non-`Proposed`) ADR, while a change touching only still-`Proposed`
ADRs defers to the policy lookup (`docs/adr/**` is `judge`).
The `$approve` judge tier is currently shadow-only, so `judge` still requires
owner confirmation; `auto` does not. A verified owner-applied
`approval:pre-approved` label
makes a non-ADR issue's effective tier `auto` without bypassing any other gate;
an agent-applied label has no authority, and ADR work has no override. The same
declared surface later bounds the implementation diff. A validated pure umbrella instead carries the exact
`N/A — pure umbrella; no implementation PR` declaration and routes to human
approval; it never produces an implementation PR.

The declared surface is still the approved implementation boundary: `$approve`
validates it, and `$implement` requires the implementation handoff and diff
review to stay inside it. There is currently no hosted `Approval gate` status
because the reconciler workflow is absent. Branch protection requires only
`CI pass` and `Lint title`, and has no required-pull-request-review rule.

## Issue dependencies

Dependencies use GitHub's native issue relation, not a custom field. `$scope`
records them and `$approve` checks them; read those skills for exact mechanics.

## Contracted phase transitions

```
Backlog  → Define     body has a problem statement
Define   → Design     if multi-PR, umbrella issue exists; if architectural, ADR drafted
Design   → Plan       tradeoffs aired; ADR merged if applicable
Plan     → Ready      dependencies declared, one concept per issue (sets phase:ready)
Ready    → Building   $implement opens a PR; the intended reconciler observes it
Building → QA         CI green on the PR head and any declared-surface gate clear
QA       → Findings   review / dogfood verdict in, and findings are open
QA       → Held       review / dogfood verdict in, and nothing is open
Findings → Held       all threads resolved and rollups cleared
Held     → Done       PR merged (deletes the phase:* label)
Building/QA/Findings/Held → Building   any push (new head SHA), CI red, or declared-surface escape demotes/holds here
Building/QA/Findings/Held → Bounced    an upstream-phase issue surfaces (sets bounce-to:*)
Any      → Stalled    env/tooling failure (not the issue's fault)
```

The intended `Building → QA → Findings → Held` stretch is not a fixed walk: a
reconciler derives the target from observable facts, so a fresh push returns the
contracted state to Building until the new head is proven. No live skill emits
the superseded legacy edges (`Ready → Executing`, `Executing → Refine`, or
`Refine → Done`).

## Hosted lifecycle availability

The checked-in Actions directory is the authority for hosted automation. On
current `main`, `reconciler.yml`, `review.yml`, `dogfood.yml`, and
`quality-eval.yml` are absent. Related scripts and the `$implement`, `$findings`,
`$land`, `$review`, and `$dogfood` skills preserve the intended contracts, but
they do not make an absent Actions entry point runnable. Treat a skill step that
needs one of those workflows as unavailable, not as evidence that GitHub is
already enforcing the transition.

Current branch protection requires the `CI pass` and `Lint title` status checks
only. Automated review, dogfood, reconciliation, and declared-surface status are
not merge gates in the present repository configuration.

## Operations

- **Bootstrap lifecycle labels:** `$release-init` ensures the phase, bounce,
  size, and model-routing vocabulary exists.
- **File an issue:** `$sketch` creates a Backlog issue and its filing labels.
- **Advance phase:** use the skill that owns the target phase; the shared GitHub
  workflow contract defines the exact mutation and label-preservation rules.
