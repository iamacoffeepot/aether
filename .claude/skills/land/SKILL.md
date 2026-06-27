---
name: land
description: Land a CI-green draft PR — un-draft, squash-merge, delete the closing issue's phase:* label (Done), sweep the worktree. `--sweep` discovers this shard's held green draft PRs and lands them in sequence, predicting and recomputing conflict state after every merge, auto-rebasing behind branches when branch protection requires up-to-date (strict mode), and routing dirty content conflicts to the user rather than touching their contents.
---

# /land — PR landing skill

The post-review landing action: take a draft PR that the user has approved, un-draft it, let native auto-merge squash it onto `main`, delete the closing issue's `phase:*` label (Done is label-absence), and sweep the merged worktree. Deliberately separate from `/implement`, which holds at draft and never merges.

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
| CI green — or green except `Qodana scan` as the sole failing required check, none pending (the [Qodana sweep](#qodana-sweep) resolves it before un-draft) | "PR #N has a non-Qodana required check red (or checks pending). Wait, or use `/implement <issue>` to fix a non-Qodana red." |
| PR has a closing issue (the PR's closing-issue reference) | "PR #N has no closing issue. Link one (`Closes #M`) or delete the phase label manually." |

Read PR draft state and `mergeable_state` over REST (`gh api repos/iamacoffeepot/aether/pulls/<n> --jq '.draft, .mergeable_state'`); read CI state from the REST check-runs endpoint (`gh api repos/iamacoffeepot/aether/commits/<sha>/check-runs`). Both are REST forms per the §REST-vs-GraphQL routing table in `/scope`.

## Sweep land

`/land --sweep` is the batched orchestrator entry point: it discovers the shard's held green draft PRs, validates each against the same gates single mode runs, prints a land plan with per-PR conflict prediction, and waits for one confirmation before landing anything.

1. **Enumerate held green draft PRs over REST.** `/implement` leaves every implemented PR as a draft held at `phase:refine`, so `phase:refine` on an open issue is the eligibility signal, queried over REST and off the contended GraphQL pool:

   ```bash
   gh api 'repos/iamacoffeepot/aether/issues?labels=phase:refine&state=open' --jq '.[].number'
   ```

   For each closing issue found, look up its open draft PR over REST:

   ```bash
   gh api 'repos/iamacoffeepot/aether/pulls?state=open' \
     --jq '[.[] | select(.draft == true)] | .[].number'
   ```

   Cross-reference to find draft PRs whose closing issue is in the `phase:refine` set. Drop any PR whose closing issue is not in the set; list it in the dropped section with reason "no phase:refine closing issue".

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

5. **On confirmation, land in sequence.** Land each PR through the [Landing sequence](#landing-sequence) in the printed order. After every merge, **recompute the remaining predictions** — the HEAD of `main` has advanced and a previously-clean branch may now be `behind`. A recomputed `dirty` halts the sequence and surfaces the conflict to the user before proceeding to the next PR.

The sweep never auto-confirms and never auto-resolves a `dirty` conflict. Landing is serial by construction — each merge advances `main` and the recompute loop updates conflict state after it — so sweep concurrency is 1 and no cap applies.

## Landing sequence

Single-mode steps, executed once per PR (sweep mode iterates this per PR in order):

1. **Gate-check.** Verify draft state, CI green, and closing-issue presence per [Preconditions](#preconditions). Abort on any failure.

2. **Predict conflict state.** Run [Conflict prediction and routing](#conflict-prediction-and-routing) for this PR's branch. If `dirty`, surface and abort — do not un-draft a dirty branch.

3. **Handle a `behind` branch.** Before acting on a `behind` classification, read `required_status_checks.strict` from branch protection once per `/land` invocation (cache the result for `--sweep`; it is stable across the run):

   ```bash
   gh api repos/iamacoffeepot/aether/branches/main/protection \
     --jq '.required_status_checks.strict'
   ```

   On a read failure or absent field, default to `true` (conservative: treat as strict-on and rebase).

   - **strict=false and `merge-tree`-clean** — GitHub does not require the branch to be up-to-date before merging, so behind+clean is already mergeable. Skip the rebase, re-attest, and force-push; proceed directly to step 4 (Qodana sweep) / step 5 (un-draft). Note "behind → merged direct (strict off)" in the summary.
   - **strict=true (or read failure)** — the branch must be up-to-date before merging. Proceed with the full rebase sequence below.

   **Full rebase sequence (strict=true or read failure):**

   The rebase runs inside the branch's own worktree (`<m>` is the closing issue; step 8 sweeps exactly this path). `git rebase origin/main` with no branch argument rebases the worktree's current HEAD in place — git refuses the `<branch>` argument when that branch is checked out in another worktree, so the argument is dropped.

   Capture the pre-rebase head first — it is the attestation key for detection:
   ```bash
   wt=.claude/worktrees/issue-<m>
   old=$(git rev-parse origin/<branch>)
   git -C "$wt" fetch origin
   git -C "$wt" rebase origin/main
   ```
   If the rebase produces conflicts, the branch becomes `dirty` — surface and abort.

   **Re-attest on the attested path.** Before the force-push, detect whether this PR is on the attested path using the same signal `ci.yml` keys on — the presence of a `refs/attestations/<sha>` ref:
   ```bash
   git ls-remote --exit-code origin "refs/attestations/$old"
   ```
   When the ref is present (exit 0), the PR was on the attested fast-path; run `scripts/attest.sh --publish` against the post-rebase worktree HEAD to publish a fresh `refs/attestations/<new-sha>` before CI sees the new push:
   ```bash
   (cd "$wt" && CARGO_TARGET_DIR=/mnt/dev/tmp/aether-attest-target \
     TMPDIR=/mnt/dev/tmp \
     scripts/attest.sh --publish)
   ```
   This must complete before the force-push so the attestation ref lands before CI's `synchronize` event triggers the `changes` job's ref-keyed opt-in check. When the ref is absent (exit non-zero), the PR was not on the attested path — skip re-attest; the force-push triggers normal heavy CI for the new sha, which is the correct behavior.

   If attest tooling is unavailable (`witness` / `sshpk-conv` / Docker / signing key — see `/implement`'s "Attested path (`--attest`)" precondition table), degrade gracefully: surface that the attested fast-path was dropped for this rebase and proceed with the force-push. The merge is never blocked — `scripts/attest-verify.sh` exits 0 (pass-through) when no attestation ref is present for the head sha, so the new sha runs full heavy CI and still merges. The bare cost is the lost fast-path.

   Then force-push:
   ```bash
   git -C "$wt" push --force-with-lease origin <branch>
   ```
   Then re-predict. In `--sweep` mode the recompute loop iterates this same rebase action after every sibling merge, so a branch that becomes `behind` after a sibling lands is re-attested by the same path — no separate sweep handling is needed.

4. **Qodana sweep (only when `Qodana scan` is the sole red).** When gate-check found `Qodana scan` as the one failing required check, resolve it before un-drafting — run the [Qodana sweep](#qodana-sweep): fetch the findings from the `qodana-report` artifact, triage and fix them in the worktree, re-push, and wait for `CI pass` green. Only then proceed. Skip this step when the PR is already fully green; bail to the user (do not un-draft) when the sweep surfaces an artifact-missing / outside-the-diff / uncertain case.

5. **Un-draft via GraphQL.** The REST `pulls` PATCH cannot clear `draft`, so this is a GraphQL-only op (per `/scope` §REST-vs-GraphQL routing). This is the **sole remaining GraphQL-only op in the whole pipeline** — every other operation, phase state included, runs on REST now that the project board is retired:
   ```bash
   gh api graphql -f query='
   mutation {
     markPullRequestReadyForReview(input: { pullRequestId: "<pr-node-id>" }) {
       pullRequest { isDraft }
     }
   }'
   ```
   Verify `isDraft` is `false` in the response before proceeding.

6. **Squash-merge.** With auto-merge enabled on this repo, un-drafting a green PR typically lets GitHub's native auto-merge land it. When that is not relied on (e.g. auto-merge disabled, or a race window), issue the REST squash merge directly:
   ```bash
   gh api -X PUT repos/iamacoffeepot/aether/pulls/<n>/merge \
     -f merge_method=squash \
     -f commit_title="<pr-title>"
   ```
   Poll the PR state (`gh api repos/iamacoffeepot/aether/pulls/<n> --jq '.merged'`) until `true` before proceeding to avoid marking an un-landed issue Done (deleting its phase label).

7. **Move the closing issue to Done — delete its `phase:*` label.** `Done` is label-absence (per the phase-label-reconcile rules in `/scope` and `/implement`), so the canonical phase write here is a REST label delete, not a swap:
   ```bash
   gh api "repos/iamacoffeepot/aether/issues/<m>/labels" \
     --jq '.[].name | select(startswith("phase:"))' \
     | while read -r l; do
         gh api -X DELETE "repos/iamacoffeepot/aether/issues/<m>/labels/$l"
       done
   ```

8. **Sweep the merged worktree.** Run the worktree removal for this PR's branch, equivalent to `/sweep worktrees` §Target: worktrees step 4 for the merged entry:
   ```bash
   git worktree remove "$(git rev-parse --show-toplevel)/.claude/worktrees/issue-<m>"
   git branch -D <branch>
   ```
   If the worktree has uncommitted files (rare — the implement agent should have committed everything), use `--force`. Skip this step when `--no-sweep` was passed.

9. **Print summary.**
   ```
   ✓ #<n> landed.
   Merged: <pr-url>
   Issue #<m>: Phase → Done
   Worktree: .claude/worktrees/issue-<m> swept
   ```

## Qodana sweep

Qodana is a required CI gate (`Qodana scan`, in `ci-pass`), not a local pre-flight step. `/implement` holds a draft whose only red is `Qodana scan` at `phase:refine`; `/land` resolves it here, before un-drafting. Invoked from [Landing sequence](#landing-sequence) step 4 when `Qodana scan` is the sole red.

1. **Confirm Qodana-only.** From the REST check-runs set, the failing required checks minus `CI pass` must be exactly `{Qodana scan}`. Any other red is a real failure — refuse and route to `/implement`.
2. **Fetch + parse the findings.** `scripts/qodana-report.sh <pr>` downloads the PR's `qodana-report` CI artifact, parses the SARIF, filters to findings on the PR's own changes, and prints the actionable list (`file:line  [severity] ruleId — message`), exiting non-zero when PR-diff findings exist. `--all` prints the whole-tree set.
3. **Triage + fix in the worktree.** Resolve each finding. Surface a suspected **false positive** to the user rather than editing a `qodana.yaml` exclude or committing a `--baseline` — those weaken the gate and need explicit sign-off (fix what's fixable; baseline only verified FPs).
4. **Local sanity** before re-push: `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, and the affected `cargo nextest`.
5. **Commit + push**, then `scripts/wave-status.sh --wait <pr>` until green, and continue the landing sequence from un-draft.

**Bail out — surface to the user, do not fix —** when `qodana-report.sh` reports no artifact or the Qodana job crashed (EAP infra; the old `Stalled` case); when findings land outside the PR's own changes; or when the find set is too large / uncertain for a confident pass (a large clean set may go to a focused `Agent`; inline is the default). Never auto-suppress a finding to make the gate pass.

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
| **behind** | branch's `merge-base` with `origin/main` is not `origin/main` itself (the branch needs rebase), but `merge-tree` would produce a clean tree | strict=true → auto-rebase onto fresh `main`, re-push, re-predict; strict=false → behind+clean is mergeable, merge direct (no rebase). |
| **dirty** | `merge-tree` exits non-zero or produces output containing conflict markers (`<<<<<<<`) | Surface to the user. Never touch the branch contents. |

Cross-check: after the local oracle classifies a branch, compare against `mergeable_state` from `gh api repos/iamacoffeepot/aether/pulls/<n> --jq '.mergeable_state'`:

- `clean` / `has_hooks` → agrees with the oracle's `clean` classification.
- `behind` → agrees with the oracle's `behind` classification.
- `dirty` → agrees with the oracle's `dirty` classification.
- `unknown` → transient; trust the local oracle and note the `unknown` in the plan.
- `clean` paired with the oracle's `behind` → agreement when strict=false; GitHub reports a behind+clean branch as `clean` when up-to-date is not required. Do not route this as a disagreement.
- A disagreement between the oracle and `mergeable_state` (e.g. oracle says `clean`, GitHub says `dirty`) → treat as `dirty` and surface the disagreement before proceeding. The local oracle can be wrong when the remote diverges from a local fetch; a fresh `git fetch origin` and re-run resolves most cases.

**Dirty conflict handling.** When a branch is `dirty`, `/land` surfaces the specific conflicting files from `merge-tree`'s output and stops:

```
✗ PR #<n> has a content conflict — landing aborted.
Conflicting files:
  crates/aether-data/src/id.rs
  crates/aether-kinds/src/lib.rs

Branch contents untouched. Options:
  1. Resolve manually: rebase <branch> onto main, fix the conflicts, re-push, then re-run /land <n>.
  2. Delegate: /implement <issue> --resume to have an agent resolve the rebase.
```

In `--sweep` mode, a `dirty` PR halts the remaining sequence — a conflict requires a human (or a delegated agent) decision, and landing subsequent PRs can change the conflict shape. Print the halt reason, list the remaining PRs that were not landed, and wait for the user to resolve before re-running.

**Recompute after every merge (`--sweep` only).** After each successful merge, `origin/main` has advanced. Recompute the conflict prediction for every remaining PR in the sequence using the same local oracle before proceeding to the next land. A branch that was `clean` against the prior `main` can be `behind` (or even `dirty`, in the degenerate case) after a sibling lands. When strict=false, a recomputed `behind` branch that is `merge-tree`-clean stays mergeable and the sweep merges it directly — no rebase, no force-push, no CI re-run.

## Phase label reconcile

`Done` carries no `phase:*` label — it is label-absence, the canonical resting state for a closed issue. The landing sequence deletes the current `phase:*` label instead of swapping it, per the rule in `/scope` §Phase label reconcile:

```bash
gh api "repos/iamacoffeepot/aether/issues/<m>/labels" \
  --jq '.[].name | select(startswith("phase:"))' \
  | while read -r l; do
      gh api -X DELETE "repos/iamacoffeepot/aether/issues/<m>/labels/$l"
    done
```

This is the same form `/scope` documents for the `Backlog` and `Done` phases — a REST `DELETE …/labels` per phase label, off the contended pool.

## What /land does NOT do

- Auto-resolve content conflicts. A `dirty` branch always surfaces to the user (or an optional delegated agent).
- Un-draft a PR with a non-Qodana required check red. The gate enforces green before un-draft, except a sole `Qodana scan` red, which the [Qodana sweep](#qodana-sweep) resolves first.
- Auto-suppress Qodana findings — edit a `qodana.yaml` exclude or commit a `--baseline` — without surfacing to the user. The Qodana sweep fixes findings or surfaces them; it never silences them.
- Land PRs in parallel. Protected `main` enforces linear history; parallel landing races to discover the serialization. The sequence lands one at a time with recompute.
- Delete the `phase:*` label before verifying the merge completed (`merged` field confirmed `true`).
- Remove a worktree whose PR has not merged. The sweep tail runs only after a confirmed merge.
- Edit the issue body. `/scope` owns the body; `/land` only touches the `phase:*` label.
- Dispatch implementation. `/implement` handles that; `/land` acts on PRs that implement has already produced and the user has reviewed.
