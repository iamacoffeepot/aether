---
name: land
description: "Land an approved Aether draft pull request after independently revalidating approval, ancestry, surface overflow pricing, checks, direct review, and threads; then reconcile the issue and clean local artifacts."
---

# /land — independently gate and merge a draft

Landing is serial and requires explicit user authorization for a named pull request or a confirmed sweep. It never waives a gate, rewrites reviewed history, or chooses a content-conflict resolution.

Read the shared [GitHub workflow contract](../../../.agents/skills/_shared/github-workflow.md) completely before acting.

## Invocation

```
/land <pr>
/land <pr> --no-sweep
/land --sweep
```

The named invocation authorizes clearing draft state, one ordinary squash merge, closing-issue verification, and safe cleanup if every gate passes. It does not authorize force-push.

## Correlate the artifacts

Fetch `origin/main` without switching the caller's checkout. Read the pull request over REST and require it open, draft, targeting `main`, headed by a branch in this repository, conventionally titled, and closing exactly one open issue. Correlate that issue with the expected `.agents/worktrees/issue-<issue>` and branch, or with a documented already-cleaned local state.

Read the issue body and effective editor, pull-request head and commits, changed files, check suites/runs, reviews, and review threads. Failed, truncated, stale-head, or ambiguous reads are unknown and therefore ineligible.

## Independent gates

All evidence must describe one current head SHA.

### Approval identity

Run the shared `plan_digest.py`, `approval_records.py`, surface matcher, and tier resolver against the current issue body. Validate body-editor provenance and select a trusted hidden v2 record whose issue, digest, size, model, tiers, and authority match current facts. Only when no current trusted v2 exists may a migration-era pull request inspect a strict trusted v1 comment.

Require the approved base commit to exist and be an ancestor of pull-request head. Current main may have advanced since approval; that alone does not stale in-flight work. A changed digest, route, missing trusted record, or base outside head ancestry is ineligible.

### Surface overflow

Price every changed path outside the approved surface:

1. Recompute the overflow at the approval base with `git diff --name-only --no-renames origin/main...<head>` and the resolver's changed mode.
2. Verify the resolver's reported blobs against the frozen `Pricing policy:` and `Pricing matcher:` lines in the draft's `## Approval` section. When the lines are missing, state the derived blobs; when they differ, everything prices `human`. Apply the ADR override and the error → `human` rule.
3. Settle each path by tier:
   - **auto** — listed; settled by direct review.
   - **judge** — read the path's current-head diff against the Plan and record `ACCEPT` or `REJECT: <reason>`. Every one must be `ACCEPT`; a `REJECT` makes the draft ineligible unless the owner accepts it.
   - **human** — print the paths and stop for the owner's explicit confirmation naming this pull request. In a sweep, the first-turn plan lists the overflow so that confirmation covers it.
4. Write the "Surface overflow" pull-request comment naming the head SHA and frozen blobs, with one `<path> — <tier> — <settlement>` line per overflow path, or "None." when there is no overflow. Edit the existing comment in place when present. The settlement is `listed` for auto, `ACCEPT` or `REJECT: <reason>` for judge, and `awaiting owner` or `confirmed by owner` for human.

Confirm the diff implements the scoped concept and contains no unrelated change. Re-price overflow after any main merge and immediately before merge.

### Checks

Require every repository-required check for the current head to be completed successfully. Missing, skipped-required, pending, cancelled, stale, or unreadable checks do not pass. Local checks are supporting evidence, never a substitute.

### Review and threads

Read the closing issue body and effective editor and validate its canonical hidden direct-review records. Require the last trusted record matching the issue number, current pull request, head SHA, and freshly computed Plan digest to say `APPROVE`. Reject an earlier-head or earlier-digest record, untrusted effective editor, legacy pull-request machine marker, or ordinary human handoff as a substitute.

Read paginated pull-request reviews only to evaluate native decisions separately. For each reviewer, take the newest non-dismissed native decision and require none to be `CHANGES_REQUESTED`. A hidden semantic record cannot clear a native request. Enumerate GraphQL review threads and require every thread resolved.

## Predict merge state

Fetch exact commits and use `git merge-tree --write-tree origin/main <head>` as the local oracle, cross-checking REST mergeability. Classify:

- **clean/direct** — head contains current main or the platform permits the clean merge;
- **behind but clean** — merge-tree succeeds, but branch protection requires current main in head;
- **content-conflicted** — the oracle or repeated fresh platform result reports content conflict.

On content conflict, hand the named draft directly to `/resolve <pr>` and stop. Do not dispatch a hosted job and do not edit the branch from land.

For behind-but-clean with a strict up-to-date requirement, require a clean owned worktree and unchanged remote head, then merge `origin/main` into the branch without rebasing, run format and full clippy, commit the merge, and plain-push. Wait for current-head CI, directly inspect and repair the new head, then append and re-read its hidden direct-review record through the shared contract's file-backed, byte-for-byte concurrency guard and provenance check. Re-price overflow and apply every landing gate again. If that merge produces content conflicts, abort it and hand the pull request to `/resolve <pr>`.

Never rebase or force-push from this skill.

## Final confirmation and merge

Immediately before mutation, re-read the pull request, issue body and editor provenance, head, approval, ancestry, diff, checks, hidden direct-review verdict, native reviews, threads, overflow, and merge prediction. Abort on any change.

1. Read the pull-request node id.
2. Clear draft state through GraphQL `markPullRequestReadyForReview`; verify draft is false.
3. Poll REST briefly for configured native merge behavior.
4. If it does not merge, perform one REST squash merge with the pull-request title as `commit_title`.
5. Re-read after any uncertain response; never blindly retry.
6. Continue only when REST reports `merged: true`; capture merge SHA and timestamp.

If merge does not complete, report the pull request ready but unmerged and do no cleanup.

## Reconcile and clean

After confirmed merge, require the closing issue to be closed by that pull request. If not, report the linkage inconsistency rather than representing the issue as done.

Unless `--no-sweep` was passed, inspect the exact worktree. Remove it and delete its local branch only when the pull request is confirmed merged and the worktree is clean. Never force-remove dirty or locked work. Verify any remote branch deletion separately and never treat a missing remote branch as an error requiring recreation.

Report pull-request URL, merge SHA, digest/base, overflow, checks, semantic and native review, threads, conflict prediction, issue closure, and cleanup.

## Sweep

Sweep is two-turn and serial. First enumerate open draft pull requests, correlate each to one closing issue, apply every gate, predict clean/direct, behind-but-clean, or conflicted, and print the exact ordered mutations, each candidate's priced overflow, and drops. Wait for confirmation.

On confirmation, revalidate and land one at a time. Fetch and recompute every remaining candidate after each merge because main advanced. A newly ineligible pull request stops the sequence. A conflicted candidate is handed directly to `/resolve <pr>` and retained for a later land pass; continue only when the user-approved sweep plan explicitly said conflicted candidates would be handed off.
