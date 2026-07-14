---
name: findings
description: Resolve the QA findings on a PR at phase:findings — fix or justify each review/dogfood rollup finding, reply on its thread with the fix commit, then resolve the thread via the GraphQL resolveReviewThread mutation (no REST verb exists, which is why replies alone never cleared the state). Writes no phase:* label — the reconciler computes findings→held once every thread is resolved and the re-run rollup is non-actionable.
---

# /findings — resolve QA findings on a held PR

The action that clears a PR out of `phase:findings`. When a PR's CI is green but the automated review or dogfood rollup posted actionable findings — inline review-comment threads, or findings folded into the review body — the reconciler parks the closing issue at `phase:findings`. `/findings <pr>` works that PR: it fixes or justifies every finding, replies on each thread with the fix commit, and resolves the thread. The reconciler (`.github/workflows/reconciler.yml`) then recomputes `findings` → `held` on its own once nothing is open — this skill never writes a phase label.

The step agents kept skipping is the last one. Replying to a finding leaves the thread open, and an open thread keeps the reconciler at `phase:findings`; the thread only closes through the GraphQL `resolveReviewThread` mutation, which has no REST verb. This skill makes reply-then-resolve a single mandate so the state actually advances.

## Invocation

```
/findings <pr>              resolve every open finding on the PR, then let the reconciler advance the phase
```

## Precondition

The closing issue is at `phase:findings` — the reconciler's state for "CI green, but the review/dogfood rollup posted actionable findings and/or review threads are open." Read it over REST off the contended pool:

```bash
gh api 'repos/iamacoffeepot/aether/issues?labels=phase:findings&state=open' --jq '.[].number'
```

If the PR's closing issue is not at `phase:findings`, there is nothing for this skill to do — a green PR with no open findings is already at `phase:held` (the reconciler advances it there), and `/land` takes it from there. Refuse with *"PR #N is not at `phase:findings` — no open findings to resolve."*

## Mandate

Work every finding through the same three steps, in this order. The order is load-bearing: fix first so the reply can name the fix commit, reply second so the thread carries the resolution, resolve last so the reconciler sees a closed thread. Skipping the resolve is the failure this skill exists to prevent.

1. **Fix or justify.** First get onto the PR's head branch — read the head ref over REST and check it out against the current `origin`, exactly as `/resolve` §Resolution procedure step 1 does, so a box that starts anywhere other than the branch (a headless run lands on the default branch) can commit and push a fix:

   ```bash
   branch="$(gh api repos/iamacoffeepot/aether/pulls/<pr> --jq '.head.ref')"
   git fetch origin
   git checkout "$branch"
   ```

   Then implement a fix in a follow-up commit on that branch, or decline the finding with a written reason. Every finding is resolved exactly one of these two ways — no finding is silently skippable. A fix is a normal commit-and-push to the PR branch; a decline is a reason the reviewer (or the next reader) can weigh, carried in the reply.

2. **Reply with the fix commit.** How the reply is posted depends on where the finding lives:
   - **Anchored findings** are inline review-comment threads — the COMMENT review posts them as inline comments (`scripts/post-review-rollup.mjs` sets `review.comments` to the inline set). Reply on the thread over REST with `in_reply_to` set to the thread's first comment `databaseId`, naming the fix commit sha (or the decline reason).
   - **Folded findings** live in the review body rather than a thread — the poster folds inline findings into the review summary when the anchor hunk is outdated (a 422 on the inline post). These are not review threads and have no node to resolve; reply under the review or summary comment, quoting the finding and naming the fix commit (or the decline reason).

3. **Resolve the thread.** Anchored findings only. Resolve the thread via the GraphQL `resolveReviewThread` mutation — the step with no REST equivalent, and the one agents omitted. A folded finding has no thread node, so it has nothing to resolve here; its reply in step 2 is the whole of its resolution, and the reconciler reads its closure from the re-run rollup rather than a thread state.

## Concrete invocations

**Enumerate the open threads.** One GraphQL query gets both the thread node `id` (for the resolve in step 3) and the first comment's `databaseId` (for the REST reply in step 2), so the whole loop reads from one call:

```bash
gh api graphql -f query='
query($owner:String!,$repo:String!,$pr:Int!){
  repository(owner:$owner,name:$repo){
    pullRequest(number:$pr){
      reviewThreads(first:100){
        nodes{ id isResolved isOutdated
          comments(first:1){ nodes{ databaseId path body } } } } } } }' \
  -F owner=iamacoffeepot -F repo=aether -F pr=<pr> \
  --jq '.data.repository.pullRequest.reviewThreads.nodes[] | select(.isResolved==false)'
```

**Reply on the thread** (REST — `in_reply_to` is the first comment's `databaseId` from the query above):

```bash
gh api -X POST repos/iamacoffeepot/aether/pulls/<pr>/comments \
  -f body="Fixed in <sha>." -F in_reply_to=<comment-databaseId>
```

**Resolve the thread** (GraphQL-only — the step agents omitted; `threadId` is the thread node `id` from the query above):

```bash
gh api graphql -f query='
mutation($threadId:ID!){
  resolveReviewThread(input:{threadId:$threadId}){ thread{ isResolved } } }' \
  -F threadId=<thread-node-id> --jq '.data.resolveReviewThread.thread.isResolved'
```

Verify the mutation returns `true` before counting the thread done.

Both the `reviewThreads` query and the `resolveReviewThread` mutation are GraphQL-only — no REST endpoint exists for either. They join the PR un-draft (`markPullRequestReadyForReview`) as the pipeline's GraphQL-only operations; see `/scope` §GitHub API budget → GraphQL-only list rather than restating the whole budget here. Every other operation this skill issues — the reply, the fix push — rides REST.

## How the phase advances

A fix push demotes `findings` → `building` on the fresh head (the reconciler's push-demotes rule) and dismisses barista's stale verdict (`dismiss_stale_reviews`), and CI runs against the new sha. **Wait for CI green** on the new sha before re-requesting — poll `scripts/wave-status.sh --wait <pr>` (the same REST poll `/resolve` §Refine loop uses; it loops until the `CI pass` aggregator settles and fast-fails on a deterministic red). Once it is green again, **re-request the review**: post an `@barista review` comment on the PR. The review runs only on request, so this comment is what produces the fresh full verdict that supersedes the standing `REQUEST_CHANGES` — without it the PR sits at `REVIEW_REQUIRED` indefinitely (`@barista full review` is the same request with the changed-`.rs` size-cap bypass). With no open thread and a non-actionable re-requested rollup the reconciler recomputes the PR to `phase:held`. This skill makes the observable facts true — the fixes are pushed, the threads are resolved, the re-review is requested — and the reconciler computes the label from them. Threads resolve and unresolve raise no Actions event, so the `*/15` reconciler backstop is what picks up the final resolve and advances the state; a `workflow_dispatch` re-reconcile with the PR number is the way to advance it immediately rather than waiting for the backstop.

## What /findings does NOT do

- Write a `phase:*` label. The reconciler is the single writer of `phase:building` / `phase:qa` / `phase:findings` / `phase:held`; this skill changes reality (fixes pushed, threads resolved) and lets the reconciler compute the state. Two writers of the `building` → `held` stretch is exactly the asymmetry the reconciler's single-writer contract removes.
- Un-draft, merge, or land the PR. `/land` acts on a PR at `phase:held`; `/findings` only clears the findings that stand between the PR and that state.
- Strip `dogfood:unresolved` or dismiss barista's review verdict. That label is the dogfood runner's, cleared by a re-run that finds nothing actionable (or by the owner's deliberate strip for a declined finding, per `/land`); the review verdict is barista's native review, superseded by a fresh verdict on the next `@barista review` re-request or an owner dismissal. This skill resolves threads and pushes fixes — it changes neither the label nor the verdict directly.
- Edit the issue body. `/scope` owns the body.
- Waive a finding on the owner's behalf. A decline carries a written reason on the thread for the reviewer to weigh; it is not an owner review waiver (approving the PR or dismissing barista's verdict natively), which is the owner's signoff alone.
