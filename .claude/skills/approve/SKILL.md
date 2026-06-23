---
name: approve
description: Plan → Ready gate. Validates that an issue's scope artifacts are complete and any drafted ADR has merged, then advances Phase to Ready. Does NOT dispatch implementation — that's /implement's job. Idempotent on re-run. `--sweep` discovers and batch-approves every Plan-complete issue behind one confirmation.
---

# /approve — Plan → Ready gate

The primary human review point of the release flow. The user invokes `/approve <issue>` after reading the scope artifacts that `/scope` produced. The skill validates the gates and flips the issue to Ready; from there `/implement` (or the Phase C orchestrator) picks it up.

## Sweep approve

`/approve --sweep` is the batched discovery entry point: instead of taking issue numbers, it enumerates every Plan-complete issue, validates each against the same gates the single-issue path runs, and waits for one confirmation before flipping any to Ready. It mirrors `/implement --sweep` (the dispatch-side discovery mode) so a reviewer clears a whole scoped batch in one pass instead of typing each number.

1. **Enumerate over REST, in one call.** `phase:plan` is set only by `/scope` when it lands an issue at Plan, so the label alone is the eligibility signal — one REST query, off the contended GraphQL pool:

   ```bash
   gh api 'repos/iamacoffeepot/aether/issues?labels=phase:plan&state=open' --jq '.[].number'
   ```

   This is the REST issues endpoint (per `/scope` §REST-vs-GraphQL routing), not `gh issue list`, which is GraphQL-backed and drains the contended pool.

2. **Gate-check each candidate.** Run the full [gate checks](#gate-checks) per issue — `Phase == Plan`, the three §-sections present and non-empty, every referenced ADR PR merged, exactly one `model:*` label, not blocked by a `blocked`/`wontfix`/`duplicate` label, freshness gate (targeted paths exist on `origin/main` and none churned since scope). Drop any issue that fails and record the reason; the sweep never silently skips — every dropped issue is listed in the plan with its drop reason. `--skip-adr` is **not** honored in sweep mode: a batch is the wrong place for a per-issue emergency override, so an unmerged-ADR issue is dropped and listed, to be approved singly with `/approve <n> --skip-adr` if the override is intended.

3. **Print the approve plan and wait for confirmation.** A batch label write is cheap to do but annoying to unwind, so one confirmation prompt covers the set. Print the issues that will be approved (with their `size:*` / `model:*` for context), any umbrella issues flagged distinctly (an umbrella with `## Sub-issues` is approvable — approving means "the plan is approved, children split correctly" — but it is not itself `/implement`-able; see [Multi-PR umbrella issues](#multi-pr-umbrella-issues)), and the dropped-with-reason list, then stop and wait:

   ```
   Sweep: 6 Plan issues, 2 dropped, 4 to approve.

   Approve → Ready:
     #1756  back every actor inbox with the settling-inbox primitive   size:m  model:sonnet
     #1757  single-ownership dispatched envelope via take_inbound       size:m  model:opus
     #1758  migrate capture replies to the retained inbound guard       size:l  model:opus
     #1754  close mail lineage … (umbrella — plan approved, not dispatched)

   Dropped:
     #1719  Phase=Design, not Plan
     #1740  ADR PR #1738 not merged (approve singly with /approve 1740 --skip-adr to override)
     #1762  Targets removed on main: crates/aether-capabilities/src/audio/mod.rs

   Confirm approve? (no label write happens until your go-ahead)
   ```

4. **On confirmation, approve the batch.** Apply [Actions on pass](#actions-on-pass) over the passing set — reconcile each passing issue's label to `phase:ready` (a REST `PUT …/labels` per issue, see [Phase label reconcile](#phase-label-reconcile)). The sweep never auto-confirms.

`--sweep` takes no issue argument — it discovers them. It does not combine with `--note` or `--skip-adr`, both single-issue concerns.

## Invocation

```
/approve <issue>                    standard (single issue)
/approve <issue> [<issue> …]        batch — validate each, swap all to phase:ready over REST
/approve --sweep                    discover every Plan-complete issue, validate each, confirm, approve all
/approve <issue> --note "<text>"    posts the text as a comment on the issue
/approve <issue> --skip-adr         bypass the ADR-merged check (emergency override)
```

## Gate checks

Run all of these. **Refuse** if any fail; list every failure in the refusal output, don't stop at the first.

| Gate | Check | Refusal message |
|------|-------|-----------------|
| Phase | issue carries the `phase:plan` label | "Issue is at <current>, not Plan. Use `/scope` or `/bounce` first." |
| Problem statement | body has `## Problem statement` and the section is non-empty | "Missing or empty §Problem statement." |
| Design notes | body has `## Design notes` and is non-empty | "Missing or empty §Design notes." |
| Implementation plan | body has `## Implementation plan` and is non-empty | "Missing or empty §Implementation plan." |
| ADR merged | if §Design notes references an ADR PR, that PR's `mergedAt` is non-null | "ADR PR #M is not merged. Merge it or pass `--skip-adr` to override." |
| Model label | exactly one `model:*` label present (REST: `gh api repos/iamacoffeepot/aether/issues/<n>/labels`) | "Missing model:* label (or more than one). `/scope` stamps model routing at Plan — re-run its Plan step or add the label by hand." |
| Not blocked | no `blocked` / `wontfix` / `duplicate` label present | "Issue carries label '<label>' which blocks approval." |
| Freshness | targeted paths exist on `origin/main` and none have churned since scope | "Targets removed on main: <paths>" (hard refuse) / "Targets churned since scope — re-ground before approving: <paths>" (soft surface) |

If **all** gates pass, proceed.

## Freshness gate

Runs after the structural gates pass, against a freshly-fetched `origin/main`. Two tiers; both operate on paths extracted from §Implementation plan "files touched" segments and §Design notes §Affected surfaces.

**Tier A — target existence (hard gate).** For each extracted path, test `git cat-file -e origin/main:<path>`. If any path is absent on `origin/main`, the plan targets removed code — refuse with the missing paths listed. `git fetch origin main` first so the check uses the current remote state, not a stale local cache.

```bash
git fetch origin main
git cat-file -e origin/main:<path>   # exit 0 = exists, exit 128 = gone
```

A Tier A failure is a hard refusal (single) or drop-with-reason (sweep): the issue's premise is provably dead — there is nothing to implement.

**Tier B — drift since scope (soft surface).** The scoped-at reference is the timestamp of the most-recent `phase:plan` labeled event on the issue timeline:

```bash
gh api repos/iamacoffeepot/aether/issues/<n>/timeline \
  --jq '[.[] | select(.event=="labeled" and .label.name=="phase:plan")] | last | .created_at'
```

For each referenced path and each ADR file named in §Design notes, check for commits on `origin/main` since that timestamp:

```bash
git log origin/main --since=<scoped-at> -- <path>
```

A non-empty result means `main` has churned the target since the issue was scoped. Tier B does not auto-refuse: surface "Targets churned since scope — re-ground before approving: <paths>" so the human reviewer decides. In sweep mode, treat a Tier B hit as a drop-with-reason and list it in the plan (the reviewer resolves it singly with `/approve <n>` after grounding).

**Symbol-tier follow-on (pending #2204).** Symbol-level checking — confirming named target symbols still exist on `origin/main`, not just the files — extends the same Tier A machinery once #2204's stable-anchor + discovery-command plan convention lands. Until then the freshness gate runs on paths only.

## Actions on pass

1. Reconcile each approved issue's label to `phase:ready` (a REST `PUT …/labels` per issue, see [Phase label reconcile](#phase-label-reconcile)) — the `phase:ready` label is the canonical phase state and the agent-eligibility signal `/implement` reads. A single-issue `/approve` is the N=1 case — one label swap. When a batch mixes passing and failing issues, swap the label only for the ones that cleared every gate and list the rest in the refusal.
2. No comment on a plain approve — the `phase:ready` label and the timeline's label event already record it. If `--note` was passed, post the note as prose markdown:

   ```markdown
   **Approved** — <note text>
   ```

3. Print a summary to the user:

   ```
   ✓ #N approved.
   Phase: Plan → Ready
   Next: /implement <N>   (or wait for the orchestrator)
   ```

## Idempotency

If `/approve` is re-run on an issue that already carries `phase:ready`:

- Re-validate the gates (catches drift if anyone hand-edited the body).
- If gates still pass: no-op, print *"Already approved — Phase=Ready."* No new comment.
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

The `phase:*` label is the canonical phase state — it is the only phase store the pipeline keeps, legible on the issue itself and discoverable over the REST issues endpoint. The swap rides REST: `gh issue edit --add-label/--remove-label` is GraphQL-backed, while the `gh api …/labels` endpoints are REST, so the phase write stays off the contended pool.

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

The single `PUT …/labels` replaces the label set with the non-`phase:*` labels plus `phase:ready`, so the issue never carries two phase labels and never carries zero — a tighter guarantee than a remove-then-add pair, which has a window between its two calls. The only write this skill makes is `phase:ready`; run the swap once per approved issue. On idempotent re-run (already Ready) the swap re-asserts the same set — a harmless no-op that also self-heals a hand-stripped label.

## Failure modes

- **GitHub API rate limit**: retry with backoff. If still failing, abort and tell the user the rate-limit reset time.
- **Hand-edits during validation**: if the issue body changes between the gate read and the label swap, re-read and re-validate before committing the phase-label transition. Don't write a partial transition.

## What `/approve` does NOT do

- Dispatch implementation. Run `/implement <issue>` (or wait for the Phase C orchestrator) after approval.
- Edit the issue body. Even if a gate fails because a section is missing, /approve doesn't write the missing section — that's `/scope`'s job.
- Auto-resolve side findings.
- Close umbrella issues when children complete. Future work.
- Notify anyone. The printed summary (and the `phase:ready` label) is the surface; comments appear only for `--note` / `--skip-adr`.
