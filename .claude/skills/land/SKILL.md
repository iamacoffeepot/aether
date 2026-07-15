---
name: land
description: Land a CI-green draft PR — un-draft, enable native auto-merge to squash it, sweep the worktree (the reconciler clears the closing issue's phase:* label at Done on the merge-close event). `--sweep` discovers this shard's held green draft PRs and lands them in sequence, predicting and recomputing conflict state after every merge, merging `origin/main` into behind branches when branch protection requires up-to-date (strict mode), and dispatching a `resolve` task for a dirty content conflict rather than touching the branch contents itself.
---

# /land — PR landing skill

The post-review landing action: take a draft PR that the user has approved, un-draft it, enable native auto-merge so GitHub squashes it onto `main` the instant its required gates go green, and sweep the merged worktree. Clearing the closing issue's `phase:*` label at Done (Done is label-absence) is the reconciler's, done reactively on the PR's merge-close event — `/land` no longer writes phase (issue #3446). Deliberately separate from `/implement`, which holds at draft and never merges.

Two entry shapes, one skill:

- **Single mode** — `/land <pr>` — land one named PR through the full linear sequence.
- **Sweep mode** — `/land --sweep` — discover this shard's held green draft PRs, predict conflict state for each, print a land plan, confirm, then land in sequence with a recompute after every merge.

## Invocation

```
/land <pr>                  land one draft PR through the full sequence
/land --sweep               discover held green draft PRs, plan, confirm, land in sequence
/land <pr> --no-sweep       single mode only; suppress the post-land worktree sweep
```

## Preconditions

| Check | Refusal |
|-------|---------|
| PR exists and is a draft | "PR #N is not a draft — it may have already been un-drafted or merged." |
| CI green — no required check red, none pending | "PR #N has a required check red (or checks pending). Wait, or use `/implement <issue>` to fix a red." |
| PR has a closing issue (the PR's closing-issue reference) | "PR #N has no closing issue. Link one (`Closes #M`) or delete the phase label manually." |
| Closing issue at `phase:held` (REST: `gh api 'repos/iamacoffeepot/aether/issues?labels=phase:held&state=open' --jq '.[].number'`) | "PR #N's closing issue is not at `phase:held`. The reconciler holds it at `building` / `qa` / `findings` until CI is green, the QA verdict is in, and every review thread is resolved. Resolve open findings with `/findings <pr>`; a red needs `/implement <issue>`." |

Read PR draft state and `mergeable_state` over REST (`gh api repos/iamacoffeepot/aether/pulls/<n> --jq '.draft, .mergeable_state'`); read CI state from the REST check-runs endpoint (`gh api repos/iamacoffeepot/aether/commits/<sha>/check-runs`). Both are REST forms per the §REST-vs-GraphQL routing table in `/scope`. `phase:held` is the single eligibility signal: the reconciler reaches it only when CI is green, the review/dogfood rollup is non-actionable, and no review thread is open, so it subsumes the underlying review verdict and `dogfood:unresolved` state — `/findings` resolves the threads, the reconciler computes `findings` → `held`, and `/land` trusts `held`. The review verdict is enforced natively: branch protection's required review blocks the merge while critic's standing `REQUEST_CHANGES` verdict stands, and the owner overrides by approving the PR or dismissing the verdict natively. That native required review is the hard enforcement; the `phase:held` precondition is the pipeline-level gate on top of it.

## Sweep land

`/land --sweep` is the batched orchestrator entry point: it discovers the shard's held green draft PRs, validates each against the same gates single mode runs, prints a land plan with per-PR conflict prediction, and waits for one confirmation before landing anything.

1. **Enumerate held green draft PRs over REST.** The reconciler advances a PR's closing issue to `phase:held` once CI is green, the QA verdict is in, and every thread is resolved, so `phase:held` on an open issue is the eligibility signal, queried over REST and off the contended GraphQL pool:

   ```bash
   gh api 'repos/iamacoffeepot/aether/issues?labels=phase:held&state=open' --jq '.[].number'
   ```

   For each closing issue found, look up its open draft PR over REST:

   ```bash
   gh api 'repos/iamacoffeepot/aether/pulls?state=open' \
     --jq '[.[] | select(.draft == true)] | .[].number'
   ```

   Cross-reference to find draft PRs whose closing issue is in the `phase:held` set. Drop any PR whose closing issue is not in the set; list it in the dropped section with reason "no phase:held closing issue".

2. **Gate-check each candidate.** Run the full [Preconditions](#preconditions) per PR. Drop any that fail and record the reason. The sweep never silently skips — every dropped PR is listed in the plan with its drop reason.

3. **Predict conflict state.** For each passing candidate, predict its merge state via the local oracle (see [Conflict prediction and routing](#conflict-prediction-and-routing)). Attach the prediction — `clean`, `behind`, or `dirty` — to each entry in the plan.

4. **Print the land plan and wait for confirmation.** Landing serializes on `main` and each merge advances HEAD, so the plan is a preview that the recompute loop will keep fresh as it executes. Print the ordered PR list, per-PR predicted state, and the dropped-with-reason list, then stop and wait:

   ```
   Sweep: 5 held green draft PRs, 1 dropped, 4 to land.

   Land sequence (in order):
     #1801  feat(aether-data): kind-id newtype helpers     clean
     #1803  fix(aether-codec): frame decoder edge case     clean
     #1805  feat(substrate-bundle): boot manifest          behind  → will merge direct (strict off)
     #1807  chore(workflow): /land skill                   clean

   Dropped:
     #1799  PR not CI-green (fmt check failing)

   Confirm land sequence? (no merge happens until your go-ahead)
   ```

5. **On confirmation, land in sequence.** Land each PR through the [Landing sequence](#landing-sequence) in the printed order. After every merge, **recompute the remaining predictions** — the HEAD of `main` has advanced and a previously-clean branch may now be `behind`. A recomputed `dirty` dispatches a `resolve` task for that PR and continues to the next PR rather than halting the sequence (see [Dirty conflict handling](#conflict-prediction-and-routing)).

The sweep never auto-confirms; a `dirty` conflict is handed to a dispatched `resolve` task rather than resolved inline. Landing is serial by construction — each merge advances `main` and the recompute loop updates conflict state after it — so sweep concurrency is 1 and no cap applies.

## Landing sequence

Single-mode steps, executed once per PR (sweep mode iterates this per PR in order):

1. **Gate-check.** Verify draft state, CI green, and closing-issue presence per [Preconditions](#preconditions). Abort on any failure.

2. **Predict conflict state.** Run [Conflict prediction and routing](#conflict-prediction-and-routing) for this PR's branch. If `dirty`, dispatch a `resolve` task and abort this land — never un-draft a dirty branch (see [Dirty conflict handling](#conflict-prediction-and-routing)).

3. **Handle a `behind` branch.** Before acting on a `behind` classification, read `required_status_checks.strict` from branch protection once per `/land` invocation (cache the result for `--sweep`; it is stable across the run):

   ```bash
   gh api repos/iamacoffeepot/aether/branches/main/protection \
     --jq '.required_status_checks.strict'
   ```

   On a read failure or absent field, default to `true` (conservative: treat as strict-on and merge-in).

   - **strict=false and `merge-tree`-clean** — GitHub does not require the branch to be up-to-date before merging, so behind+clean is already mergeable. Skip the merge-in; proceed directly to step 4 (un-draft). Note "behind → merged direct (strict off)" in the summary.
   - **strict=true (or read failure)** — the branch must be up-to-date before merging. Proceed with the merge-in sequence below.

   **Merge-in sequence (strict=true or read failure):**

   The merge runs inside the branch's own worktree (`<m>` is the closing issue; step 6 sweeps exactly this path). `git merge origin/main` merges into the worktree's current HEAD.

   ```bash
   wt=.claude/worktrees/issue-<m>
   git -C "$wt" fetch origin
   git -C "$wt" merge origin/main
   ```
   If the merge produces conflicts, the branch becomes `dirty` — dispatch a `resolve` task and abort this land (see [Dirty conflict handling](#conflict-prediction-and-routing)).

   Push the merged branch:
   ```bash
   git -C "$wt" push origin <branch>
   ```
   The push triggers a fresh CI run for the new sha, which is the gate. Then re-predict. In `--sweep` mode the recompute loop iterates this same merge-in action after every sibling merge, so a branch that becomes `behind` after a sibling lands is merged by the same path — no separate sweep handling is needed.

4. **Un-draft via GraphQL.** The REST `pulls` PATCH cannot clear `draft`, so this is a GraphQL-only op (per `/scope` §REST-vs-GraphQL routing). It is one of the pipeline's few GraphQL-only ops — it joins `/findings`'s `reviewThreads` query and `resolveReviewThread` mutation (per `/scope` §GitHub API budget → GraphQL-only list); every other operation, phase state included, runs on REST now that the project board is retired:
   ```bash
   gh api graphql -f query='
   mutation {
     markPullRequestReadyForReview(input: { pullRequestId: "<pr-node-id>" }) {
       pullRequest { isDraft }
     }
   }'
   ```
   Verify `isDraft` is `false` in the response before proceeding.

5. **Enable native auto-merge to squash-merge.** Enable per-PR auto-merge so GitHub fires the squash the instant every required check passes — including the `dismiss_stale_reviews` re-review the un-draft (step 4) triggers — rather than waiting for a tick to notice the PR is mergeable:
   ```bash
   gh pr merge <n> --auto --squash
   ```
   This is the GraphQL `enablePullRequestAutoMerge` mutation under the hood (per `/scope` §REST-vs-GraphQL routing's GraphQL-only list) — there is no REST equivalent, so this is one of the pipeline's rare `gh` convenience-subcommand calls rather than a `gh api` form. If auto-merge cannot be enabled (all required gates are already green with nothing left for auto-merge to wait on, or a repo-setting race), fall back to the immediate REST squash merge:
   ```bash
   gh api -X PUT repos/iamacoffeepot/aether/pulls/<n>/merge \
     -f merge_method=squash \
     -f commit_title="<pr-title>"
   ```
   Either way, poll the PR state (`gh api repos/iamacoffeepot/aether/pulls/<n> --jq '.merged'`) until `true` before proceeding — the merge must be confirmed before the sweep tail runs, and the reconciler's Done-cleanup keys on the same merge-close, so an un-landed PR must never be treated as landed.

   `/land` writes no phase label here. `Done` is label-absence, and the reconciler — the single writer of the post-green phase stretch — owns clearing the closing issue's `phase:*` label, firing on the PR's merge-close (`closed`) event rather than a `/land` follow-up delete. The old follow-up delete raced the merge-close: the squash-merge's `Closes #N` closed the issue several seconds before the delete landed, so a board monitor read a closed issue still carrying `phase:held` (issue #3446). `/land` polls `.merged` to `true` in step 5 (it must never proceed on an un-landed PR) and stops there; the reconciler's `merged`-gated Done branch clears the label atomically with — and reactively to — the close.

6. **Sweep the merged worktree.** Run the worktree removal for this PR's branch, equivalent to `/sweep worktrees` §Target: worktrees step 4 for the merged entry:
   ```bash
   git worktree remove "$(git rev-parse --show-toplevel)/.claude/worktrees/issue-<m>"
   git branch -D <branch>
   ```
   If the worktree has uncommitted files (rare — the implement agent should have committed everything), use `--force`. Skip this step when `--no-sweep` was passed.

7. **Print summary.**
   ```
   ✓ #<n> landed.
   Merged: <pr-url>
   Issue #<m>: Phase → Done
   Worktree: .claude/worktrees/issue-<m> swept
   ```

## Conflict prediction and routing

The local oracle for merge state is `git merge-tree --write-tree`. GitHub's `mergeable_state` field is the cross-check, not the primary signal — GitHub computes it asynchronously and can return `unknown` transiently.

```bash
git fetch origin
git merge-tree --write-tree origin/main origin/<branch>
```

Classify the result into one of three states:

| State | Condition | Action |
|-------|-----------|--------|
| **clean** | `merge-tree` exits 0 with a valid tree hash and no conflict markers | Proceed with the landing sequence. |
| **behind** | branch's `merge-base` with `origin/main` is not `origin/main` itself (the branch needs a merge-in), but `merge-tree` would produce a clean tree | strict=true → merge `origin/main` into the branch, push, re-predict; strict=false → behind+clean is mergeable, merge direct (no merge-in). |
| **dirty** | `merge-tree` exits non-zero or produces output containing conflict markers (`<<<<<<<`) | Dispatch a `resolve` task (see the **Dirty conflict handling** note below); `/land` never edits the branch contents itself. |

Cross-check: after the local oracle classifies a branch, compare against `mergeable_state` from `gh api repos/iamacoffeepot/aether/pulls/<n> --jq '.mergeable_state'`:

- `clean` / `has_hooks` → agrees with the oracle's `clean` classification.
- `behind` → agrees with the oracle's `behind` classification.
- `dirty` → agrees with the oracle's `dirty` classification.
- `unknown` → transient; trust the local oracle and note the `unknown` in the plan.
- `clean` paired with the oracle's `behind` → agreement when strict=false; GitHub reports a behind+clean branch as `clean` when up-to-date is not required. Do not route this as a disagreement.
- A disagreement between the oracle and `mergeable_state` (e.g. oracle says `clean`, GitHub says `dirty`) → the local oracle can be wrong when the remote diverges from a local fetch, so a fresh `git fetch origin` and re-run resolves most cases. A disagreement that persists after a re-fetch is treated as `dirty` — dispatch a `resolve` task (see the **Dirty conflict handling** note below), the same as any confirmed `dirty`.

**Dirty conflict handling.** A content conflict is not a human trigger — it is an ordinary agent-authored code change the pipeline already knows how to make safe (CI green, a fresh review verdict on the resolved head, the reconciler's declared-surface containment). So when a branch is `dirty`, `/land` does not surface-and-stop; it dispatches a `resolve` task — a headless Claude box that checks out the branch, merges `origin/main` into it (the merge-not-rebase mechanic), resolves every conflict hunk (semantic ones included — there is no "too complex to attempt" tier), and drives the resolved head green and re-reviewed:

```bash
gh workflow run agent-work.yml -f task=resolve -f ref=<n>
```

Then abort this land — the resolve box owns the branch now. `/resolve` is a pure producer, like `/implement`: it neither opens a PR (the PR exists) nor merges (that is `/land`'s job on a later pass). Its resolution push trips `dismiss_stale_reviews` (the prior approval drops to `REVIEW_REQUIRED`) and, once CI is green, the resolve box re-requests the review, so the resolved head earns a fresh verdict; the reconciler then recomputes it back to `phase:held` and the standard held→land path (a tick, or native auto-merge) lands it. A resolve box self-bounces (ask-and-park) only on a genuine incompatibility of intent — two sides encoding an incompatible product decision — which is the sole route to a person.

In `--sweep` mode, a `dirty` PR no longer halts the remaining sequence: dispatch its `resolve` task, note the dispatch in the plan, and continue landing the rest of the sequence. Landing the siblings first can only change the conflict shape, which the resolve box re-reads when it runs; the resolved PR rejoins a future sweep once its resolved head is held and green.

**Recompute after every merge (`--sweep` only).** After each successful merge, `origin/main` has advanced. Recompute the conflict prediction for every remaining PR in the sequence using the same local oracle before proceeding to the next land. A branch that was `clean` against the prior `main` can be `behind` (or even `dirty`, in the degenerate case) after a sibling lands. When strict=false, a recomputed `behind` branch that is `merge-tree`-clean stays mergeable and the sweep merges it directly — no merge-in, no CI re-run.

## Phase label reconcile

`Done` carries no `phase:*` label — it is label-absence, the canonical resting state for a closed issue. `/land` does **not** write that label itself: the reconciler owns clearing the closing issue's `phase:*` label at Done, firing on the PR's merge-close (`closed`) event with a `merged`-strict gate and a `*/15` schedule backstop for a missed event (`.github/workflows/reconciler.yml`). This keeps the reconciler the single writer of the post-green phase stretch and removes the land-side follow-up delete that raced the merge-close (issue #3446). `/land`'s only phase-relevant act is the step-5 `.merged` poll, which confirms the merge before the sweep tail — it never edits a phase label.

## What /land does NOT do

- Resolve content conflicts inline. `/land` never edits a branch's contents itself; a `dirty` branch is handed to a dispatched `resolve` task (a headless box that merges `origin/main` in and resolves the hunks), which re-enters the held→land path once its resolved head is green and re-reviewed.
- Un-draft a PR with a required check red. The gate enforces green before un-draft.
- Resolve findings or strip `dogfood:unresolved` itself. `/findings` resolves the review threads and pushes the fixes; the reconciler reads that and computes `findings` → `held`; `/land` trusts `held`. A fresh critic verdict on the next review re-request supersedes the standing `REQUEST_CHANGES`, and the dogfood runner's poster clears `dogfood:unresolved` on a re-run that finds nothing actionable. `/land` only refuses while the closing issue is short of `phase:held`.
- Approve the PR or dismiss critic's review verdict. The review waiver is the owner's signoff alone — the owner overrides a standing `REQUEST_CHANGES` by approving the PR or dismissing the verdict natively (ADR-0148 §Owner waiver); an agent never does either, whoever's token it holds. Same class as the label-strip rule above.
- Land PRs in parallel. Protected `main` enforces linear history; parallel landing races to discover the serialization. The sequence lands one at a time with recompute.
- Write the closing issue's `phase:*` label at Done. The reconciler owns Done-cleanup on the merge-close event (issue #3446); `/land` only confirms the merge via the step-5 `.merged` poll.
- Remove a worktree whose PR has not merged. The sweep tail runs only after a confirmed merge.
- Edit the issue body. `/scope` owns the body; `/land` touches no issue label.
- Dispatch implementation. `/implement` handles that; `/land` acts on PRs that implement has already produced and the user has reviewed.
