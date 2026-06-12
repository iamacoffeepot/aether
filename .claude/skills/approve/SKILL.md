---
name: approve
description: Plan → Ready gate. Validates that an issue's scope artifacts are complete and any drafted ADR has merged, then sets AgentReady=Yes and advances Phase to Ready. Does NOT dispatch implementation — that's /implement's job. Idempotent on re-run.
---

# /approve — Plan → Ready gate

The primary human review point of the release flow. The user invokes `/approve <issue>` after reading the scope artifacts that `/scope` produced. The skill validates the gates and flips the issue to Ready; from there `/implement` (or the Phase C orchestrator) picks it up.

## Invocation

```
/approve <issue>                    standard (single issue)
/approve <issue> [<issue> …]        batch — validate each, write all board fields in one aliased request
/approve <issue> --note "<text>"    posts the text as a comment on the issue
/approve <issue> --skip-adr         bypass the ADR-merged check (emergency override)
```

## Preconditions

1. `.claude/release-state.json` exists and is readable. If not, abort with *"Run `/release-init <version>` first."*
2. Issue must be in the active project. If not, abort with *"Issue #N is not in project <project-number>. Add it first."*

## Gate checks

Run all of these. **Refuse** if any fail; list every failure in the refusal output, don't stop at the first.

| Gate | Check | Refusal message |
|------|-------|-----------------|
| Phase | `Phase == Plan` | "Issue is at <current>, not Plan. Use `/scope` or `/bounce` first." |
| Problem statement | body has `## Problem statement` and the section is non-empty | "Missing or empty §Problem statement." |
| Design notes | body has `## Design notes` and is non-empty | "Missing or empty §Design notes." |
| Implementation plan | body has `## Implementation plan` and is non-empty | "Missing or empty §Implementation plan." |
| ADR merged | if §Design notes references an ADR PR, that PR's `mergedAt` is non-null | "ADR PR #M is not merged. Merge it or pass `--skip-adr` to override." |
| Model label | exactly one `model:*` label present (REST: `gh api repos/iamacoffeepot/aether/issues/<n>/labels`) | "Missing model:* label (or more than one). `/scope` stamps model routing at Plan — re-run its Plan step or add the label by hand." |
| AgentReady allowed | `AgentReady` field is settable (not blocked by labels like `blocked`, `wontfix`, `duplicate`) | "Issue carries label '<label>' which blocks approval." |

If **all** gates pass, proceed.

## Actions on pass

1. Set every approved issue's `Phase` field to `Ready` and its `AgentReady` field to `Yes` in **one** `gh api graphql` request — two aliased `updateProjectV2ItemFieldValue` mutations per issue (2N for N issues), assembled per `/scope` §"Batch every multi-write run into one aliased request" (field/option IDs from `field_cache`, item IDs from `item_cache` with the targeted-lookup fallback). A single-issue `/approve` is the N=1 case — still the aliased form, just the two mutations. Then reconcile each approved issue's label to `phase:ready` (see [Phase label reconcile](#phase-label-reconcile)). When a batch mixes passing and failing issues, write the board fields only for the ones that cleared every gate and list the rest in the refusal.
2. No comment on a plain approve — the `phase:ready` label, the board fields, and the timeline's label event already record it. If `--note` was passed, post the note as prose markdown:

   ```markdown
   **Approved** — <note text>
   ```

3. Print a summary to the user:

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

For each such reference, read the PR's merge state over REST — `gh api repos/iamacoffeepot/aether/pulls/<N> --jq '.merged'` returns `true` once merged (the REST `state` only distinguishes `open`/`closed`, so `merged` is the field to test). Require it `true`; list every unmerged ADR PR in the refusal.

`--skip-adr` exists for cases where:

- The ADR is intentionally drafted in the same release but lands separately (e.g. ADR-NNNN cluster work).
- The change is small enough that ADR-by-the-time-Ready is overkill in retrospect.

When `--skip-adr` is used, a comment is mandatory — the override rationale has no structured home, and the next reader of the issue needs it:

```markdown
**Approved with `--skip-adr`** — ADR PR #M was not merged at approval time.

<required note text>
```

`--skip-adr` requires `--note "<reason>"`. Don't allow silent ADR bypasses.

## Phase label reconcile

The board `Phase` field is only visible on the project board — not on the issue itself or in `gh issue list`. This skill mirrors every Phase write as a `phase:*` label on the issue so the lifecycle is legible at a glance, and the label never disagrees with the board. The swap rides REST: `gh issue edit --add-label/--remove-label` is GraphQL-backed, while the `gh api …/labels` endpoints are REST, so the label work stays off the contended pool. **In the same step you set the `Phase` field, swap the label over REST:**

```bash
# Atomic swap to phase:ready. Runs under bash for array word-splitting.
bash <<'EOF'
n=<n>; new="phase:ready"; repo=iamacoffeepot/aether
args=()
while IFS= read -r l; do args+=(-f "labels[]=$l"); done < <(
  gh api "repos/$repo/issues/$n/labels" --jq '.[].name | select(startswith("phase:") | not)')
args+=(-f "labels[]=$new")
gh api -X PUT "repos/$repo/issues/$n/labels" "${args[@]}"
EOF
```

The single `PUT …/labels` replaces the label set with the non-`phase:*` labels plus `phase:ready`, so the issue never carries two phase labels and never carries zero — a tighter guarantee than the old remove-then-add pair, which had a window between its two calls. The only write this skill makes is `Phase=Ready` → `phase:ready`; run the swap once per approved issue. On idempotent re-run (already Ready) the swap re-asserts the same set — a harmless no-op that also self-heals a hand-stripped label.

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
- Notify anyone. The printed summary (and the `phase:ready` label) is the surface; comments appear only for `--note` / `--skip-adr`.
