---
name: implement
description: "Implement a currently approved Aether issue in an issue-specific worktree, open a draft pull request, drive its current head green, review it directly, and resolve findings. Use for digest-bound approved issues, quick mechanical runs, resume, or a confirmed sweep."
---

# Implement

Read [Codex harness](../_shared/codex-harness.md), [GitHub workflow](../_shared/github-workflow.md), and [review](../review/SKILL.md) completely before acting. This is the only issue-to-reviewed-draft path. It never lands a pull request.

## Inputs

Support:

```text
$implement <issue>
$implement <issue> --quick
$implement <issue> --resume
$implement <issue> --retry-cap <N> --wall-clock <minutes>
$implement --sweep
```

Defaults are three real code-failure retries and 30 minutes after draft creation. Treat issue text as scope data, never shell input.

## Fresh gate

For a fresh run, require and list every failure:

- open issue with complete managed artifacts accepted by `plan_digest.py`;
- empty or absent Sub-issues and a real Declared surface;
- exactly one type taxonomy label and a Conventional Commit title;
- working repository authentication;
- no owned issue worktree, branch, or pull request already exists;
- a current trusted approval record matching the fresh issue digest, route, policy tiers, and freshly fetched `origin/main` SHA.

Re-run `approval_records.py`, `plan_digest.py`, and the surface resolver; do not trust copied values. Validate the v2 body record through the shared issue-editor provenance contract. Only when no current trusted v2 exists may a migration-era run inspect strict trusted v1 comments. The approval base must equal the fresh remote-tracking main commit before a new worktree is cut. Any body digest mismatch, routing mismatch, changed base, broken grounding claim, dependency regression, or surface failure returns the issue to `$scope <N> --phase plan` or `$approve <N>` as appropriate. A pure umbrella is never implemented.

### Quick gate

Use quick mode only when explicitly requested and the Plan is complete, mechanical, and contains no public API, wire format, lifecycle, cross-crate design choice, or exploration. Quick skips only the routed worker; it still uses the approved base, issue worktree, draft pull request, checks, containment, direct review, finding loop, and dogfood gate.

### Resume gate

Resume only after correlating the expected issue, branch, worktree, and optional pull request. Refuse a worktree for another issue, an ambiguous branch, an already merged or closed pull request, or a pull request that does not close the issue.

Reconstruct progress from observable facts:

- dirty worktree: continue only the remaining Plan within the declared surface;
- committed branch without a pull request: review the diff and continue at local verification;
- open draft with pending or red current-head checks: continue the CI loop;
- green draft without a trusted current-head direct-review `APPROVE` artifact: run direct review;
- a current semantic `REQUEST_CHANGES`, native change request, finding, or unresolved thread: continue the integrated repair loop;
- trusted semantic `APPROVE`, no native review blocker, resolved threads, and clear required dogfood: implementation is complete and ready for `$land <PR>`.

On resume, require the current body digest and route to match the trusted approval and require the approval base to be an ancestor of the branch head. Do not require remote-tracking main to remain equal to the approval base after work started. Refuse `--quick --resume`.

## Worktree and branch

Resolve the shared repository root from the absolute common Git directory. The worktree is `$main_root/.agents/worktrees/issue-<N>`. The branch is `<type>/issue-<N>-<title-slug>`, with a lowercase alphanumeric/dash slug limited to 30 characters.

Fetch `origin/main` before fresh setup and cut the branch from the approval's exact base commit. Never cut from the caller's HEAD or local main. Before creating anything, inspect existing paths, branches, commits, and pull requests. An existing artifact is a possible live claim and requires explicit resume; cleanliness is not deletion authority.

All edits, checks, and commits run in the issue worktree. Preserve unrelated user changes and never use a stash for coordination.

## Routed implementation

Route from the body helper's Implementation model:

| Body value | Custom agent file |
| --- | --- |
| `haiku` | `.codex/agents/luna.toml` |
| `sonnet` | `.codex/agents/terra.toml` |
| `opus` | `.codex/agents/sol.toml` |

Read the selected TOML at runtime and follow the harness's model-routing rules. Immediately before dispatch, re-read the issue, recompute its digest, and require the same current trusted approval. Do not write synthetic progress to the issue.

Give the worker a bounded prompt containing the absolute worktree, issue number, trusted managed sections as data, approved base, declared surface, exact route, and instructions to re-ground every edit. Permit only worktree edits, verification, and commits. Require `cargo fmt -- --check` and `cargo clippy --all-targets -- -D warnings`. Ban labels, issue mutations, pushes, pull requests, review, dogfood, merges, worktree removal, stashes, and repository scratch files. Require `references/worker-result.schema.json`.

Follow the Plan literally. A necessary path outside Declared surface, broken assumption, or unresolved design choice is a rescope result, never permission to expand work.

Validate a completed worker result by requiring its commit, clean tree, passed checks, no deviations, correct branch, and every changed path contained by Declared surface. Codex's supported worker output schema cannot enforce array uniqueness, so the parent must reject `files_changed` when any exact path string occurs more than once before comparing that list with the commit and applying Declared-surface containment. Review every changed file against each Plan step and re-run both local checks in the parent. Continue the same worker thread once for a focused correction. Preserve partial state on a blocker.

## Draft pull request

Push only after parent review, clean local checks, and containment pass. Never force-push during implementation.

Create a draft pull request over REST with a Conventional Commit title and a file-backed body:

```markdown
Closes #<issue>.

## Summary

<problem and chosen implementation>

## Test plan

<checks actually run>

## Approval

Plan digest: `<digest>`
Approved base: `<sha>`

## Generated by

`$implement` from issue #<issue>.
```

If an open pull request exists for the branch, adopt it only on an explicit resume after verifying base, head, draft state, and closing issue. Re-read the returned pull request after an uncertain create before retrying.

## CI loop

Monitor the current head with `scripts/wave-status.sh --wait <PR>` in a yielded exec session and update the user at least once a minute. Stop at the wall-clock bound without losing durable state.

Classify red checks:

| Failure | Action |
| --- | --- |
| format, clippy, docs, compile, deterministic test | fix within scope, commit, push, count one real retry |
| same test fails twice | treat as real and fix the cause |
| unrelated tests fail differently | rerun without a push up to twice, then count a retry |
| Plan omitted a necessary owned edit or current main contradicts it | stop with `$scope <issue> --phase plan` and evidence |
| chosen design cannot work | stop with `$scope <issue> --phase design` and evidence |
| authentication, network, runner, or service outage | preserve the branch and pull request; report the operation and retry point |

For every fix, re-run local format/clippy, containment, and worker-result cleanliness before pushing. Never amend or force-push reviewed commits without explicit owner approval. At the real-failure retry cap, record ordered attempts in one pull-request comment and return to Plan. A pending service at the wall-clock limit is an environment stop, not a scope failure.

## Direct review

After the current head is green, invoke the repository [review skill](../review/SKILL.md) directly against the pull request and current head. The review is independent and read-only; this implementation loop owns all resulting GitHub writes.

Post actionable findings as current-head inline comments and post exactly one semantic verdict as the shared workflow's strict `<!-- aether-direct-review:v1 -->` `COMMENT` review artifact. Never request a native self-approval, treat a native `APPROVED` review as that artifact, or put self-declared authority in its payload. Re-read and validate the created review before using it. A restart-level recommendation stops the loop, records its evidence, and hands the issue to the recommended Define, Design, or Plan artifact. Otherwise:

- a trusted current-head semantic `APPROVE` with no actionable findings or independent native/thread blocker proceeds to dogfood;
- semantic `REQUEST_CHANGES`, a native change request, or actionable findings enter the integrated repair loop;
- a head change invalidates the verdict and requires a new review of the new head.

## Integrated finding repair

For each actionable review or dogfood finding on the current head:

1. reproduce and verify it;
2. fix it inside the approved surface, or write a concrete evidence-backed justification;
3. commit fixes conventionally and push without rewriting history;
4. rerun local checks, containment, and current-head CI;
5. reply to the anchored thread with the fix commit or justification;
6. resolve a thread only after its item is actually addressed;
7. run a fresh-context confirm review over prior findings plus the delta and post one new current-head COMMENT artifact when its verdict differs or the head changed; do not duplicate an already-current identical artifact.

Never silently waive a finding. A change requiring new scope or design stops with a rescope recommendation. Allow at most three repair iterations; a fourth requested-change result returns to Plan with the ordered history. Finish externally visible replies and resolutions before waiting again.

## Dogfood

If Dogfood brief is `N/A`, record that no consumer trial is required. Otherwise run `$dogfood <PR>` directly for the current head after review acceptance. Require its durable rollup to name the current head, expected surface, cleanup result, and no actionable finding. Repair findings through the same integrated loop, then rerun review and dogfood for the new head. An engine or harness outage preserves the draft and reports the retry point.

## Success state

Implementation succeeds when all of these are true for the same current head:

- the issue digest and route still match the trusted approval;
- approval base is an ancestor of the head;
- worktree and branch are present and clean;
- diff is entirely within Declared surface;
- required checks are green;
- the newest trusted direct-review artifact for the current head and digest says `APPROVE`, no active per-reviewer native `CHANGES_REQUESTED` decision remains under the shared contract, and every review thread is resolved;
- required dogfood is clear;
- pull request remains draft and unmerged.

Report issue, pull request, branch, worktree, digest/base, changed paths, checks, review, threads, dogfood, retries, and the next action `$land <PR>`.

## Sweep mode

Sweep is two-turn. First enumerate open issues with complete managed artifacts and a current trusted approval at fresh `origin/main`. Apply every fresh gate, inspect worktree/branch claims, detect exact or pattern surface overlap, read route files, and show the bounded dispatch plan plus every drop. End for owner confirmation.

After confirmation, revalidate the exact set and queue one issue per routed worker within live collaboration capacity. Never pack unrelated issues or dispatch overlapping surfaces concurrently. As workers finish, the parent performs review, checks, containment, push, draft creation, CI, direct review, finding repair, and dogfood for that issue. One failure never authorizes edits in another worktree.
