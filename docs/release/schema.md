# Release phase schema

Each aether release is tracked entirely on GitHub issue labels — there is no project board. Phase and all issue metadata ride `phase:*` / `type:*` / `size:*` / `model:*` labels, set by the `/release-*` skills, so every operation with a REST form uses REST and the contended GraphQL pool stays free. GraphQL is reserved for facts and mutations GitHub does not expose over REST, including closing-issue references, review-thread reads and resolution, and PR un-draft. `release-project-init.sh <version>` ensures the label vocabulary exists.

## Phase — the `phase:*` label

The lifecycle vocabulary is the `phase:*` label set, the canonical phase state for an issue. Backlog and Done are **label-absence**: a fresh issue carries no `phase:*` label (Backlog), a merged/closed issue has its `phase:*` label deleted (Done), and every active phase in between carries its own label. The skills read lifecycle state off labels over REST (`labels=phase:ready`, …) — discovering, enumerating, and gating without any board query.

| Phase     | Label             | Meaning                                            | Advances by      |
|-----------|-------------------|----------------------------------------------------|------------------|
| Backlog   | *(no label)*      | Not yet picked up for this release                 | User             |
| Define    | `phase:define`    | Problem framing in progress                        | User + `/scope`  |
| Design    | `phase:design`    | Tradeoffs / options / ADR drafting                 | User + `/scope`  |
| Plan      | `phase:plan`      | Sequencing, dependencies, declared surface          | User + `/scope`  |
| Ready     | `phase:ready`     | Agent-ready; awaiting dispatch                     | Gate: `/approve` |
| Building  | `phase:building`  | PR open; head unproven, CI not green, or declared-surface gate red | Reconciler |
| QA        | `phase:qa`        | CI green; review/dogfood verdict owed              | Reconciler       |
| Findings  | `phase:findings`  | QA findings open — threads or rollups unresolved   | Reconciler       |
| Held      | `phase:held`      | CI green, QA complete, all threads resolved; land-eligible | Reconciler |
| Done      | *(no label)*      | PR merged, issue closed                            | Auto             |
| Bounced   | `phase:bounced`   | Phase regression — see the `bounce-to:*` label     | User triage      |
| Stalled   | `phase:stalled`   | Env/tooling failure, blocks dispatch               | User triage      |

`Building` / `QA` / `Findings` / `Held` are the computed post-green resting states, written only by the reconciler workflow (`.github/workflows/reconciler.yml`) — see [The reconciler](#the-reconciler) below. `Executing` and `Refine` are **superseded** by this vocabulary: `phase:executing` maps to `phase:building` (straight rename) and `phase:refine` decomposes into `qa` / `findings` / `held`. The live skills no longer write either, and `release-project-init.sh` no longer creates them; the reconciler still migrates a straggler `executing` / `refine` label — one left on an in-flight PR from before the vocabulary flip — forward on its next recompute, so no PR stalls on the retired name.

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

At Plan, `/scope` emits a fenced `## Declared surface` glob list. `/approve`
validates that it covers the planned targets and resolves the most restrictive
`auto|judge|human` tier from `.github/approval-policy.yml` over every path
each declaration can permit, including higher-tier files inside a declared
subtree. An explicit `ADR flag:` or declared ADR edit is always human-routed.
The hosted judge is currently shadow-only, so `judge` still requires owner
confirmation; `auto` does not. The hosted tick dispatches an approval run only
for an exact `auto` result, and the headless gate resolves the tier again
before writing Ready. A verified owner-applied `approval:pre-approved` label
makes a non-ADR issue's effective tier `auto` without bypassing any other gate;
an agent-applied label has no authority, and ADR work has no override. The same
declared surface later bounds the implementation diff. A validated pure umbrella instead carries the exact
`N/A — pure umbrella; no implementation PR` declaration and routes to human
approval; it never produces an implementation PR.

`Approval gate` is a required status, so the reconciler posts it on every PR
path. A PR with no closing issue, an issue outside the reconciler phase domain,
or an issue with no fenced `## Declared surface` receives a passing no-op status
rather than silence. When the section exists, the reconciler treats its globs as
the implementation boundary and compares every changed PR path. An escaping
diff stays in Building until it is trimmed, the repository owner edits the
declaration, or the owner applies `approval:surface-ok`.

## Issue dependencies

GitHub's native feature, not a custom field: `gh issue edit <n> --add-dependency <m>` and the dependency graph view.

## Phase-transition rules (enforced by the `/release-*` skills)

```
Backlog  → Define     body has a problem statement
Define   → Design     if multi-PR, umbrella issue exists; if architectural, ADR drafted
Design   → Plan       tradeoffs aired; ADR merged if applicable
Plan     → Ready      dependencies declared, one concept per issue (sets phase:ready)
Ready    → Building   /implement opens a PR; the reconciler sees it on first firing
Building → QA         CI green on the PR head and any declared-surface gate clear
QA       → Findings   review / dogfood verdict in, and findings are open
QA       → Held       review / dogfood verdict in, and nothing is open
Findings → Held       all threads resolved and rollups cleared
Held     → Done       PR merged (deletes the phase:* label)
Building/QA/Findings/Held → Building   any push (new head SHA), CI red, or declared-surface escape demotes/holds here
Building/QA/Findings/Held → Bounced    an upstream-phase issue surfaces (sets bounce-to:*)
Any      → Stalled    env/tooling failure (not the issue's fault)
```

The `Building → QA → Findings → Held` stretch is not a fixed walk — the reconciler recomputes the whole target from observable facts on every firing, so a PR jumps straight to `Held` when everything is already in, and any push demotes a `Held` / `Findings` / `QA` PR back to `Building` on the fresh head. The superseded legacy edges (`Ready → Executing`, `Executing → Refine`, `Refine → Done`) no longer fire from any live skill; the reconciler still migrates a straggler `executing` / `refine` label — left on an in-flight PR from before the flip — forward on its next recompute.

## The reconciler

The post-green stretch (`building` → `qa` → `findings` → `held`) is computed, not asserted. One hosted workflow, `.github/workflows/reconciler.yml` (name `Reconciler`, no Claude, no cargo), is the **sole writer** of `phase:building` / `phase:qa` / `phase:findings` / `phase:held`. It never writes `define` / `design` / `plan` / `ready` / `bounced` / `stalled` — those stay owned by their skills. On every relevant event it resolves the PR, finds the issue it closes (GraphQL `closingIssuesReferences`), gates on that issue's phase being in the reconciler domain (`ready` / `executing` / `building` / `refine` / `qa` / `findings` / `held`), reads the ground-truth facts, computes one target phase by a first-match table, and writes it with the atomic non-phase-preserving label `PUT`.

**Facts** (per-head unless noted): CI check-runs conclusion on the PR's current head SHA; the native `reviewDecision` (`APPROVED` / `CHANGES_REQUESTED` / `REVIEW_REQUIRED`) aggregated from barista's required native review; an optional fenced `## Declared surface` block compared with the PR's changed paths; the `dogfood:unresolved` PR label; whether the closing issue's `## Dogfood brief` is present and not `N/A`; the issue's dogfood marker `verdict=`; and the GraphQL `reviewThreads(isResolved: false)` count.

**Computation** (first match wins): a declared-surface escape, just-fired `synchronize`, or CI-not-green head → `building`; a review verdict owed for this head or a dogfood trial owed with no verdict in → `qa`; findings open (unresolved labels or threads) → `findings`; otherwise → `held` (land-eligible). Every firing recomputes from scratch, so a missed event self-heals on the next one. Every exit path posts `Approval gate`; the enforced path writes it before advisory label/comment mirrors and verifies that only a repository-owner-applied `approval:surface-ok` waiver counts.

**Triggers**: `pull_request` (`opened` / `reopened` / `synchronize` / `ready_for_review` / `labeled` / `unlabeled` — the payload carries the PR; `synchronize` is the push-demotes path; label events wake it when a QA post job flips `dogfood:unresolved`, and a `pull_request_review` event wakes it on a native review verdict); `workflow_run` on **CI only** (CI runs are `pull_request`-triggered, so their head branch resolves the PR; Review and Dogfood runs anchor to `main` and cannot be resolved this way); `workflow_dispatch` with a `pr` input (the manual re-reconcile, and the authoritative QA-completion hand-off — the Review and Dogfood post jobs each poke it with the exact PR number they know); and a `*/15` `schedule` backstop that recomputes every open PR in the domain (the supported answer to thread resolves / unresolves having no Actions event, and the self-heal for any missed poke).

## Operations

- **Bootstrap a release:** `release-project-init.sh <version>` — ensures the `phase:*` / `bounce-to:*` / `size:*` / `model:*` labels exist (REST `gh label create`).
- **File an issue:** `/sketch` — REST `POST …/issues` with the `type:*` (and `crate:*`) labels; a fresh issue is Backlog by label-absence.
- **Advance phase:** the `/release-*` skills swap the `phase:*` label atomically over REST (`PUT …/issues/<n>/labels`, replacing the prior `phase:*` with the new one); Backlog and Done delete the label rather than swap it.

Every operation with a REST form rides REST — there is no board to write. The few GraphQL-only review-thread, closing-reference, and un-draft operations are called directly rather than through convenience commands.
