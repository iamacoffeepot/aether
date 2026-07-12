# ADR-0148: Native required-review merge gate

- **Status:** Proposed
- **Date:** 2026-07-12

## Context

The pipeline's review stage (ADR-0146) currently gates merges through a hand-built mirror chain. The review
workflow runs a Claude session against a Rust-touching PR and posts its findings as a `COMMENT` review —
deliberately advisory, with no blocking force of its own. An advisory `review:unresolved` label carries the
verdict, a separate `gate` job mirrors that label into a required `Review gate` commit status on the head SHA,
and branch protection requires that status. The owner's waiver is another label, `review:skip`, whose
`labeled`-event actor the gate re-verifies so an agent-held token cannot forge it. The reconciler reads three
distinct signals to compute `phase:findings` / `phase:held`: the `Review gate` status, the `review:unresolved`
label, and unresolved review threads over GraphQL.

This chain re-implements, in labels and status posts, exactly what GitHub's pull-request review machinery
already does natively: a reviewer submits a verdict, a requested change blocks the merge, an approval releases
it, and a new push invalidates a stale approval. The hand-built version has three moving parts to keep honest
(label writes, the status mirror, the actor check on the waiver), the review session's verdict does not gate
anything by itself, and the review history is illegible in the standard GitHub UI — it lives in labels and
status contexts instead of the review timeline.

Replacing the mirror with native reviews runs into one rigid constraint: **identity**. GitHub hard-rejects a
review verdict (`APPROVE` / `REQUEST_CHANGES`) whose author is the PR author — and every fleet PR is opened by
the writer App (`iamakettle[bot]`), because PR creation must ride an App token for the event-driven reconciler
to hear the `pull_request` events (`GITHUB_TOKEN`-initiated events do not trigger workflows). The verdict
identity therefore cannot be kettle. It also cannot be `GITHUB_TOKEN`: the repository toggle that would allow
Actions to approve PRs is the same toggle that allows it repo-wide, and `pull_request`-triggered workflows run
the *branch's* copy of the workflow files — a PR gone sideways could add a step that approves itself. The
approval channel must be reachable only by code on `main`.

## Decision

Merge gating moves to GitHub's native required-review mechanism, with a dedicated reviewer App as the verdict
identity.

- **Reviewer App.** `iamabarista` — permission-scoped to `pull_requests: write` (nothing else), installed on
  this repository only. Its private key lives in repo secrets (`BARISTA_APP_ID` / `BARISTA_APP_PRIVATE_KEY`)
  and is minted into a token only by workflows running `main`'s code (the same `create-github-app-token`
  pattern the writer App uses), so approval authority stays pinned to `main` whatever a PR branch does. Kettle
  writes; barista reviews. The two-App split is the same separation of duties a human org has between author
  and reviewer.
- **Branch protection.** `main` gains `required_pull_request_reviews` with `required_approving_review_count: 1`
  and `dismiss_stale_reviews: true`. A new push dismisses a stale approval, so the refine loop naturally forces
  a re-review. Required conversation resolution stays as-is. The `Review gate` required status context is
  removed.
- **Verdict submission.** The review session's output becomes a single native review call: inline comments plus
  `event: REQUEST_CHANGES` when findings are actionable, `event: APPROVE` when clean. A standing
  `REQUEST_CHANGES` blocks the merge by itself; the fix loop pushes, resolves the threads, and barista's
  re-review replaces its own prior verdict.
- **Out-of-scope PRs.** The review session's scope predicate (Rust-touching) is unchanged. A PR outside it
  receives a session-less barista `APPROVE` — the same semantics as today's auto-green `Review gate` status,
  submitted through the same channel as every other verdict so the required-review count is satisfied for every
  PR class.
- **Over-cap PRs.** A PR exceeding the changed-file cap (`AETHER_REVIEW_MAX_FILES`) receives no verdict at all
  and rests blocked at review-required — fail-closed, as today — until the owner dispatches a full review
  (`workflow_dispatch`) or reviews it natively himself.
- **Owner waiver.** `review:skip` is retired. The owner overrides a verdict natively: approve the PR (he is
  never the author of a fleet PR), and dismiss barista's standing `REQUEST_CHANGES` if one exists. Dismissal
  requires a stated reason, so the waiver carries its own audit trail — the actor-verification shell code has
  no native-flow equivalent to keep.
- **Reconciler.** The review facts collapse to one primary signal: `reviewDecision` (`REVIEW_REQUIRED` =
  verdict owed, `CHANGES_REQUESTED` = findings, `APPROVED` = verdict in and clean), alongside the existing
  unresolved-thread count. The `Review gate` status read and the `review:unresolved` label read are deleted.
  Dogfood's `dogfood:unresolved` signal is untouched — it is a QA verdict, not a review, and stays a label.

## Consequences

- The review history becomes legible in the standard GitHub UI: kettle authored, barista requested changes,
  barista (or the owner) approved. No labels or status contexts to cross-reference.
- The verdict gates itself. There is no window where the label and the status mirror disagree, and no mirror
  job to keep honest.
- Every PR class now requires an approval, including the owner's own PRs — he cannot self-approve, so his PRs
  wait on barista like everyone else's. This is dogfooding, not an accident; if it chafes, an admin-bypass
  allowance in branch protection is the native escape hatch, decided separately.
- Session-authored PRs (opened in a supervised Claude session on the owner's token) author as the owner, so
  barista or kettle can approve them; fleet PRs author as kettle, so barista or the owner can. Barista is the
  one identity that can approve every class, which is what earns it its existence.
- A dismissed-stale approval means each refine push costs a re-review. The review workflow already re-runs per
  push today, so the session cost is unchanged; only the approval's lifetime shortens to match reality.
- Native auto-merge waits on required reviews, so the un-draft → auto-merge landing flow is unchanged.
- Follow-on work, each its own PR: the review workflow rework (verdict submission, out-of-scope approve arm,
  retiring the `gate` job and label writes), the reconciler's `reviewDecision` keying, the branch-protection
  change (owner-signed), and the skill-text updates (`/land`, `/findings`, `/implement` and their headless
  variants) plus the pipeline guide page.

## Alternatives considered

- **`GITHUB_TOKEN` as the verdict identity** — rejected: the create-and-approve toggle is repo-wide, and
  branch-copy workflows run on `pull_request` triggers, so the PR under review could reach the approval
  channel. The gate exists precisely for the agent-gone-sideways case; an approval path the PR can reach is
  decorative.
- **Kettle as the reviewer** — rejected: GitHub 422s a verdict review on your own PR, and kettle must remain
  the PR author for event-wake semantics. Commit authorship (the owner's public identity, ADR-0146 arc) is
  metadata; the self-approval rule keys on the PR's creating actor.
- **An owner PAT for the reviewer** — rejected: fleet actions become indistinguishable from the owner's,
  dissolving the supervised-vs-unsupervised boundary, and the owner then cannot approve fleet PRs he
  effectively authored.
- **A machine user account** — rejected: a second full account with PAT rotation and a seat, for a result the
  App gives scoped and rotation-free.
- **Keeping the mirror chain** — rejected: it is a bespoke re-implementation of review gating with more moving
  parts, no self-gating verdict, and an illegible history; the monster this ADR exists to retire.
