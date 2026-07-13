---
name: resolve
description: Resolve a DIRTY PR's content conflict on the pipeline. Checks out the PR's branch, merges origin/main into it (merge-not-rebase), resolves every conflict hunk — semantic ones included, no "too complex to attempt" tier — commits the merge, non-force pushes, drives CI green through the same Refine loop /implement uses, then re-requests the review on the resolved head. A pure producer, like /implement: it neither opens a PR (the PR exists) nor merges (that is /land's job). Self-bounces only on a genuine incompatibility of intent — the sole route to a person.
---

# /resolve — the conflict-resolution skill

The pipeline's handler for a `dirty` PR. `/land` dispatches it on a content conflict (`mergeable_state: dirty`) instead of surfacing the conflict to a human: a conflict resolution is an ordinary agent-authored code change, and the pipeline already knows how to make such a change safe (CI green, a fresh review verdict on the resolved head, the reconciler's declared-surface containment). `/resolve` checks out the PR's branch, merges `origin/main` into it, resolves every conflict hunk, and drives the resolved head green and re-reviewed — the same shape as `/implement`, with conflict resolution in place of plan execution.

`/resolve` is a **pure producer**, like `/implement`. It does **not** open a PR — the PR already exists — and it does **not** merge — that is `/land`'s job on a later held→land pass. Its terminal state is a green, re-reviewed resolved head; from there the resolved PR re-enters the normal reconciler→`phase:held`→land path and lands the standard way (a tick, or native auto-merge). The only route to a person is a self-bounce (ask-and-park) on a genuine incompatibility of intent.

Its ref is a **PR number**, like `/land` — not an issue number.

## Invocation

```
/resolve <pr>                       resolve one dirty PR (defaults: retry-cap=3, wall=30min)
/resolve <pr> --retry-cap <N>       override the CI Refine-loop retry cap
/resolve <pr> --wall-clock <mins>   override wall-clock budget
```

## Preconditions

| Check | Refusal |
|-------|---------|
| PR exists and is not merged | "PR #N is merged (or missing) — nothing to resolve." |
| PR is a draft (the resolution rides the review gate before land) | "PR #N is not a draft — it may already be landing; resolve does not touch a non-draft PR." |
| `mergeable_state` is `dirty` (REST: `gh api repos/iamacoffeepot/aether/pulls/<n> --jq '.mergeable_state'`) | "PR #N is not `dirty` (state `<s>`) — the conflict is already resolved; nothing to do." A `dirty` classification is the whole trigger; a non-`dirty` state (`clean` / `behind` / `blocked` / `unstable`) means a prior resolve landed or `main` moved back under the branch. `unknown` is GitHub still computing — re-read once, then trust the local oracle (`git merge-tree`, per `/land` §Conflict prediction). |
| PR has a closing issue (the PR's closing-issue reference) | "PR #N has no closing issue. Link one (`Closes #M`) so the reconciler and declared-surface gate have a target." |
| `gh auth status` has `repo` scope | "Run `gh auth refresh` (repo scope is standard)." |

Read PR state over REST (`gh api repos/iamacoffeepot/aether/pulls/<n> --jq '.draft, .merged, .mergeable_state'`) per the §REST-vs-GraphQL routing table in `/scope`. The closing issue is where the reconciler reads phase and the declared-surface gate reads the PR's declared surface; resolve edits stay within that surface (see [What /resolve does NOT do](#what-resolve-does-not-do)).

## Resolution procedure

The resolution builds on the **merge-not-rebase** mechanic (#3261): `/land` merges `origin/main` into the branch rather than rebasing and force-pushing, so a resolve merge commit is an ordinary commit closed by an ordinary non-force push — nothing rebases it away.

1. **Check out the PR's branch.** Read the head ref over REST and check it out against the current `origin`:

   ```bash
   branch="$(gh api repos/iamacoffeepot/aether/pulls/<n> --jq '.head.ref')"
   git fetch origin
   git checkout "$branch"
   ```

2. **Merge `origin/main` into the branch.** This is the same merge-in `/land` runs for a `behind` branch, except here it conflicts:

   ```bash
   git merge origin/main
   ```

   The merge stops with conflict markers in the overlapping files.

3. **Resolve every conflict hunk.** Read *both* sides of each conflict — the branch's change and `origin/main`'s — and write the resolution that honors both intents. **Semantic conflicts are in scope**: there is no "too complex to attempt" tier and no mechanical additive-only classifier that bails on anything harder. Resolving a semantic conflict is the same class of work `/implement` does unsupervised every day, bounded by the same gates (CI green, a fresh review verdict, the declared-surface containment). Stay within the PR's declared surface — a resolution that would have to touch a file outside it is a signal to re-read the conflict, not to expand scope (the reconciler's declared-surface gate pins any overreach regardless).

   Resolution stays inside the branch's own contents — the merge folds `origin/main` in; it never rewrites `main`.

4. **Commit the merge.** Stage the resolved files and complete the merge commit — an ordinary merge commit, no `--amend`, no history rewrite:

   ```bash
   git add <resolved paths>
   git commit --no-edit
   ```

5. **Non-force push.** The merge commit sits on top of the branch's existing history, so the push is a plain fast-forward — never `--force`:

   ```bash
   git push origin "$branch"
   ```

   The push demotes the PR's closing issue to `phase:building` on the fresh head (the reconciler's push-demotes rule) and trips `dismiss_stale_reviews` — the prior approval drops to `REVIEW_REQUIRED` the moment the resolution lands, so the resolved head cannot ride the old verdict.

If the merge cannot be resolved because the two sides encode an incompatible product decision the box cannot settle, abort the merge (`git merge --abort`, leaving the branch clean and unchanged) and **self-bounce** — see [Self-bounce mechanics](#self-bounce-mechanics). That is the only case that reaches a person.

## Refine loop (spin the resolved head green)

After the resolution push, drive CI green on the new sha through the **same Refine loop `/implement` uses** — see `/implement` §Refine loop (the spin-until-green part). In brief:

1. Wait for CI via `scripts/wave-status.sh --wait <pr>` (REST poll, fast-fails on a deterministic red).
2. CI green — or green except a sole `Qodana scan` red held for `/land` — → go to [Terminal state](#terminal-state).
3. CI failed → pull logs, classify (format/clippy/build/test per `/implement`'s table), fix in the branch, push to the same branch, and loop. A resolution can shake out a build or test failure the merge introduced; those are fixed in the loop exactly as `/implement` fixes its own.
4. Retry cap / wall-clock hit → self-bounce (ask-and-park in the headless wrapper), same budget as `/implement` (retry-cap 3, wall 30 min; overridable). A CI failure the box cannot drive green is *not* the incompatible-intent case — it self-bounces with the attempt history, like `/implement`.

The Refine loop writes no phase label — the reconciler computes `building` / `qa` from CI state on every push, exactly as it does for an `/implement` PR.

## Review re-request

Post-#3246 the review runs **only on an explicit request** — it does not chain off a CI completion. The resolution push already dismissed the stale approval (`dismiss_stale_reviews`), so once the Refine loop reaches CI green, re-request the review on the resolved head — the same `@barista review` channel `/findings` uses for a fix push:

```bash
gh api -X POST repos/iamacoffeepot/aether/issues/<n>/comments -f body='@barista review'
```

(`@barista full review` is the same request with the changed-`.rs` size-cap bypass.) The comment trigger is restricted to the owner and the fleet App — the box posts as the App, so the comment is admitted. barista submits one fresh `APPROVE` / `REQUEST_CHANGES` verdict against the resolved head; with no open thread and a non-actionable rollup the reconciler recomputes the PR back through `building` → `qa` → `held`. Without this comment the resolved head sits at `REVIEW_REQUIRED` indefinitely — the re-request is what earns the fresh verdict the resolved head must have before it can land.

## Terminal state

CI green (or green except a sole `Qodana scan` red held for `/land`) with the review re-requested:

1. **No phase-label write, no PR open, no merge.** The resolved head is a green, re-reviewed producer artifact; the reconciler reads the observable facts (CI green, fresh verdict pending/in, threads resolved) and computes the resting state — `phase:held` when the fresh verdict is non-actionable, or `phase:findings` when the re-review posts actionable findings (resolve them with `/findings <pr>`, not here). A sole `Qodana scan` red is normal and left for `/land`'s Qodana sweep.
2. **Hand back to the held→land path.** From `phase:held`, the standard land path takes over — a tick, or native auto-merge — with no further resolve action. `/resolve` neither un-drafts nor merges; landing is `/land`'s call.
3. Print to user:

   ```
   ✓ PR #<n> conflict resolved and CI-green.
   Resolved head: <pr-url>
   Branch: <branch>
   Review re-requested (@barista review) on the resolved head.
   Next: the reconciler computes held once the fresh verdict is in; the held→land path lands it.
   ```

## Self-bounce mechanics

`/resolve`'s **only** route to a person is a genuine incompatibility of intent — the two sides encode an incompatible product decision the box cannot settle by honoring both (not merely a hard-to-read hunk, and not a CI failure). Uses the same machinery as `/bounce` (see that skill's "Self-bounce by other skills" section): abort the in-progress merge so the branch is left clean and unchanged (`git merge --abort`), then surface the incompatibility with the specific conflicting files and the two intents in tension, and stop. A CI failure the Refine loop cannot drive green is the *other* self-bounce case — the budget breach — and it carries the attempt history, exactly like `/implement`'s retry-cap / wall-clock bounce.

The branch is left at its pre-merge state (the resolution was never pushed), so a human — or a re-dispatched resolve after the decision is made — starts from a clean branch.

## What /resolve does NOT do

- **Open a PR.** The PR already exists — resolve rebuilds its head, it does not create one. (This is the split from `/implement`, which opens the PR.)
- **Merge or un-draft the PR.** Landing is `/land`'s job on the next held→land pass; resolve's terminal state is a green, re-reviewed resolved head.
- **Force-push.** The resolution is an ordinary merge commit on top of the branch's history, pushed fast-forward. A force-push would rewrite the reviewed branch — banned.
- **Resolve outside the PR's declared surface.** The merge folds `origin/main` in, leaving the PR's changed-file set as the branch's own changes; a resolution forced to touch a new file is pinned by the reconciler's declared-surface gate, exactly as any overreach is.
- **Rebase.** Resolve builds on the merge-not-rebase convention (#3261); it merges `origin/main` into the branch and never rebases the branch onto `main`.
- **Escalate an ordinary conflict to a human.** There is no "too complex to attempt" tier — a semantic conflict is resolved, not surfaced. The sole human route is an incompatible-intent self-bounce.
- **Edit the issue body or write phase labels** (beyond the terminal reconciler-computed states, which resolve does not write). `/scope` owns the body; the reconciler owns the phase.
