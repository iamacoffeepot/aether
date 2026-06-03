---
name: approve
description: Plan → Ready gate. Validates that an issue's scope artifacts are complete and any drafted ADR has merged, then sets AgentReady=Yes and advances Phase to Ready. Does NOT dispatch implementation — that's /implement's job. Idempotent on re-run.
---

# /approve — Plan → Ready gate

The primary human review point of the release flow. The user invokes `/approve <issue>` after reading the scope artifacts that `/scope` produced. The skill validates the gates and flips the issue to Ready; from there `/implement` (or the Phase C orchestrator) picks it up.

## Invocation

```
/approve <issue>                    standard
/approve <issue> --note "<text>"    adds the text to the audit comment
/approve <issue> --skip-adr         bypass the ADR-merged check (emergency override)
```

## Preconditions

1. `.claude/release-state.json` exists and is readable. If not, abort with *"Run `/release-init <version>` first."*
2. Issue must be in the active project. If not, abort with *"Issue #N is not in project <project-number>. Add it first."*

## Gate checks

Run all of these. **Refuse** if any fail; list every failure in the refusal comment, don't stop at the first.

| Gate | Check | Refusal message |
|------|-------|-----------------|
| Phase | `Phase == Plan` | "Issue is at <current>, not Plan. Use `/scope` or `/bounce` first." |
| Problem statement | body has `## Problem statement` and the section is non-empty | "Missing or empty §Problem statement." |
| Design notes | body has `## Design notes` and is non-empty | "Missing or empty §Design notes." |
| Implementation plan | body has `## Implementation plan` and is non-empty | "Missing or empty §Implementation plan." |
| ADR merged | if §Design notes references an ADR PR, that PR's `mergedAt` is non-null | "ADR PR #M is not merged. Merge it or pass `--skip-adr` to override." |
| AgentReady allowed | `AgentReady` field is settable (not blocked by labels like `blocked`, `wontfix`, `duplicate`) | "Issue carries label '<label>' which blocks approval." |

If **all** gates pass, proceed.

## Actions on pass

1. Set the project item's `AgentReady` field to `Yes`.
2. Set the project item's `Phase` field to `Ready`, and reconcile the issue label to `phase:ready` (see [Phase label reconcile](#phase-label-reconcile)).
3. Post an audit comment:

   ```
   [approve] Plan approved by <user>. Phase → Ready, AgentReady=Yes.
   <--note text if passed>
   ```

4. Print a summary to the user:

   ```
   ✓ #N approved.
   Phase: Plan → Ready
   AgentReady: No → Yes
   Next: /implement <N>   (or wait for the orchestrator)
   ```

## Idempotency

If `/approve` is re-run on an issue that already has `Phase=Ready` and `AgentReady=Yes`:

- Re-validate the gates (catches drift if anyone hand-edited the body).
- If gates still pass: no-op, print *"Already approved — Phase=Ready, AgentReady=Yes."* No new comment.
- If gates now fail: refuse and list failures. Don't auto-bounce — let the user decide whether to fix the body or `/bounce` the issue.

## Side findings

`/approve` is intentionally **not the place to triage side findings**. The §Side findings section is informational at this gate. Side findings get triaged via `/scope-spinoff <issue>` before or after approval — the user's call when. Approving an issue with un-triaged side findings is fine and common; the findings stay in the body for the next reviewer (or a future maintenance pass).

## Multi-PR umbrella issues

If §Sub-issues lists children, the umbrella's `/approve` means "the overall plan is approved, children are split correctly". Each child still goes through its own `/scope` → `/approve` flow. The umbrella itself may not be `/implement`-able (no code to write at this level); leaving it at `Phase=Ready` is correct — it advances to `Done` only when every child is `Done`.

A future `/release-promote-umbrella <parent>` skill can auto-close the umbrella when all children are Done. Out of scope for v1.

## ADR gate, in detail

Parse §Design notes for a URL or reference matching one of:

- `https://github.com/<owner>/<repo>/pull/<N>`
- `Closes <owner>/<repo>#<N>` (the cross-repo close form per the user's memory)
- A bare `#<N>` paired with an "ADR" mention nearby

For each such reference, run `gh pr view <N> --json mergedAt,state` and require `state == MERGED`. List every unmerged ADR PR in the refusal.

`--skip-adr` exists for cases where:

- The ADR is intentionally drafted in the same release but lands separately (e.g. ADR-NNNN cluster work).
- The change is small enough that ADR-by-the-time-Ready is overkill in retrospect.

When `--skip-adr` is used, the audit comment is verbose:

```
[approve] Plan approved by <user> with --skip-adr override.
   Unmerged ADRs at approval time: #M
   Reason: <required note text>
```

`--skip-adr` requires `--note "<reason>"`. Don't allow silent ADR bypasses.

## Phase label reconcile

The board `Phase` field is only visible on the project board — not on the issue itself or in `gh issue list`. This skill mirrors every Phase write as a `phase:*` label on the issue so the lifecycle is legible at a glance, and the label never disagrees with the board. **In the same step you set the `Phase` field, reconcile the label:**

```bash
gh issue edit <n> \
  --remove-label "phase:define,phase:design,phase:plan,phase:ready,phase:executing,phase:refine,phase:bounced,phase:stalled" \
  --add-label "phase:<new>"
```

`--remove-label` ignores labels the issue doesn't carry, so this single line is safe on any transition — it strips whatever phase label was present and applies the new one (lowercased: `Phase=Ready` → `phase:ready`). The only write this skill makes is `Phase=Ready` → `phase:ready`. On idempotent re-run (already Ready) the reconcile is a harmless no-op; run it anyway so a hand-stripped label self-heals.

## Failure modes

- **Issue not in project**: instruct user to add it via `gh project item-add`.
- **`release-state.json` stale (field IDs invalid)**: re-run `/release-init <version> --reuse <num>` to rebuild the cache. Same advice as `/scope`.
- **GitHub API rate limit**: retry with backoff. If still failing, abort and tell the user the rate-limit reset time.
- **Hand-edits during validation**: if the issue body changes between the gate read and the field update, re-read and re-validate before committing the Phase transition. Don't write a partial transition.

## What `/approve` does NOT do

- Dispatch implementation. Run `/implement <issue>` (or wait for the Phase C orchestrator) after approval.
- Edit the issue body. Even if a gate fails because a section is missing, /approve doesn't write the missing section — that's `/scope`'s job.
- Auto-resolve side findings.
- Close umbrella issues when children complete. Future work.
- Notify anyone. The audit comment is the notification surface.
