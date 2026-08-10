---
name: land
description: "Land an approved Aether draft pull request after independently revalidating its current issue digest, ancestry, declared surface, checks, review, threads, and required dogfood, then reconcile the closing issue and clean its worktree."
---

# Land

Read [Codex harness](../_shared/codex-harness.md) and [GitHub workflow](../_shared/github-workflow.md) completely before acting. Landing is serial and requires explicit user authorization for a named pull request or a confirmed sweep.

Support:

```text
$land <PR>
$land <PR> --no-sweep
$land --sweep
```

The invocation authorizes clearing draft state, an ordinary squash merge, closing-issue reconciliation, and safe cleanup for the named eligible pull request. It does not authorize changing implementation, waiving a gate, resolving a conflict, or force-pushing. Ask separately before the exact force-push procedure below.

## Read and correlate

Fetch `origin/main` without switching the caller's worktree. Read the pull request over REST and require:

- open, draft, and targeting `main`;
- head branch in this repository, not a fork;
- exactly one open closing issue unambiguously named in the body;
- expected issue worktree/branch ownership or a documented already-cleaned local state;
- Conventional Commit title.

Read the closing issue body and comments, current pull-request head, commits, changed files, check suites/runs, reviews, and review threads. Read dogfood evidence when the issue requires it. A failed or truncated read is a hard unknown, never a pass.

## Independent landing gates

All evidence must describe the same current head SHA.

### Approval identity

Run `plan_digest.py` on the current issue body and resolve its current routing and policy evidence. Find a trusted immutable approval comment whose issue, Plan digest, size/model, and policy/effective tiers match. Require its approved base to exist and be an ancestor of the pull-request head.

The remote-tracking main commit may have advanced since approval; that alone does not stale an in-flight pull request. A changed issue digest, changed route, missing trusted record, or approval base outside head ancestry is ineligible and must return to scope or approval.

### Surface containment

Parse Declared surface with the same strict rules and canonical matcher used during approval. Enumerate every changed path from the pull request's actual base comparison and require all paths inside the surface. Require the pull request to contain the scoped concept and no unrelated change. Re-run after any rebase and immediately before merge.

### Checks

Require every repository-required check for the current head to be completed successfully. Treat missing, skipped-required, pending, cancelled, stale-head, or unreadable checks as not ready. Do not substitute local tests for branch protection.

### Review and threads

Require a current-head approving review produced by the direct review contract, no later current-head `CHANGES_REQUESTED`, and no unresolved review thread. Read reviews over REST and threads through GraphQL. A dismissal or approval on an earlier head is not current acceptance.

### Dogfood

When Dogfood brief is `N/A`, record the explicit exemption. Otherwise require a durable dogfood rollup for the current head with the specified medium/surface, cleanup complete, and no actionable finding. A stale-head or ambiguous result is not sufficient.

## Predict merge state

Use the pull request's REST mergeability fields as hints and compute locally from fetched commits:

1. verify the head commit object and approval-base ancestry;
2. determine whether head already contains current `origin/main`;
3. run a merge-tree prediction between current main and head;
4. classify as clean/direct, behind but clean, or content-conflicted.

A content conflict stops landing. Do not rewrite implementation or choose a resolution in this skill.

If the branch is behind but the platform can merge it cleanly, prefer direct squash merge after all gates. Rebase only when branch protection or the merge API requires an up-to-date branch.

## Explicit rebase and force-push

Because a rebase rewrites reviewed commits, show the exact branch, old head, current main, predicted result, and `--force-with-lease` command, then obtain a fresh explicit user approval. After approval:

1. require a clean issue worktree and unchanged remote head;
2. fetch and rebase onto current `origin/main`;
3. abort the rebase and stop on any conflict;
4. run `cargo fmt -- --check` and `cargo clippy --all-targets -- -D warnings`;
5. push with `--force-with-lease`, never plain force;
6. wait for the new head's CI;
7. require a new direct review verdict, new dogfood evidence when applicable, resolved threads, fresh containment, and every landing gate again.

Do not run full local tests or distributions unless the user explicitly asks; CI is the full build engine.

## Clear draft and merge

Immediately before mutation, re-read the pull request, issue body/comments, head, checks, reviews, threads, dogfood, merge prediction, and every gate. Abort on any change.

1. Read the pull request node id.
2. Use GraphQL `markPullRequestReadyForReview` and verify draft is false.
3. Poll REST briefly for configured native merge behavior.
4. If it does not merge, the explicit land request authorizes one REST squash merge using the pull-request title as `commit_title`.
5. Re-read after an uncertain response before retrying.
6. Continue only after REST confirms `merged: true`; capture merged time and merge SHA.

Use a bounded monitor and update the user at least once a minute. If merge remains incomplete, report it ready but unmerged and perform no cleanup.

## Reconcile issue and cleanup

After confirmed merge, re-read the closing issue and require it closed by the pull request. If it remains open, report the linkage inconsistency; do not represent it as done or mutate unrelated metadata.

Unless `--no-sweep` was passed, inspect the exact issue worktree. Remove it and delete its local branch only when the pull request is confirmed merged and the worktree is clean. Never force-remove a dirty or locked worktree. Report retained artifacts for `$sweep worktrees`.

Report pull-request URL, merge SHA, approved digest/base, containment, checks, review, threads, dogfood, direct-versus-native merge, any rebase, issue closure, and cleanup.

## Sweep mode

Sweep is two-turn and serial.

1. Enumerate open draft pull requests over REST.
2. Correlate each with one closing issue and apply every gate.
3. Predict clean/direct, behind/direct, behind/rebase requiring separate approval, or conflicted.
4. Show the ordered sequence and all proposed mutations: clear draft, optional separately authorized rebase, squash merge, issue verification, worktree removal, and branch deletion.
5. End for confirmation.

After confirmation, revalidate and land one at a time. Fetch and recompute every remaining candidate after each merge because main changed. A newly ineligible or conflicted pull request halts the sequence; report what landed and what remains. Never land in parallel.
