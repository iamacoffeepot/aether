# ADR-0148: Native required-review merge gate

- **Status:** Accepted (demoted to defense-in-depth, 2026-07-17 — see note below)
- **Date:** 2026-07-12

> **Demotion note (2026-07-17, ADR-0149 migration step 3).** Landing authority
> moved to the Bloomery source port's compare-and-swap `land` (#3559): a resolved
> bloom reaches mainline by the source-port CAS advancing the mainline ref, not by
> a GitHub squash-merge under branch protection. This ADR's native required-review
> gate is therefore **demoted to defense-in-depth** around the GitHub-hosted
> mainline — it still blocks a stray direct PR merge and keeps the review history
> legible in the GitHub UI, but it is no longer the merge authority of record for
> bloom-driven change. The gate's mechanism (the `iamacritic` reviewer App, the
> identity constraints, the reconciler signals) is unchanged; only its standing
> relative to the source-port CAS is. Retiring the surrounding ADR-0146 `/land`
> machinery is #3562's scope, staged separately.

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
the writer App (`iamabuilder[bot]`), because PR creation must ride an App token for the event-driven reconciler
to hear the `pull_request` events (`GITHUB_TOKEN`-initiated events do not trigger workflows). The verdict
identity therefore cannot be builder. It also cannot be `GITHUB_TOKEN`: the repository toggle that would allow
Actions to approve PRs is the same toggle that allows it repo-wide, and `pull_request`-triggered workflows run
the *branch's* copy of the workflow files — a PR gone sideways could add a step that approves itself. The
approval channel must be reachable only by code on `main`.

## Decision

Merge gating moves to GitHub's native required-review mechanism, with a dedicated reviewer App as the verdict
identity.

- **Reviewer App.** `iamacritic` — permission-scoped to `pull_requests: write` (nothing else), installed on
  this repository only. Its private key lives in repo secrets (`CRITIC_APP_ID` / `CRITIC_APP_PRIVATE_KEY`)
  and is minted into a token only by workflows running `main`'s code (the same `create-github-app-token`
  pattern the writer App uses), so approval authority stays pinned to `main` whatever a PR branch does. Builder
  writes; critic reviews. The two-App split is the same separation of duties a human org has between author
  and reviewer.
- **Branch protection.** `main` gains `required_pull_request_reviews` with `required_approving_review_count: 1`
  and `dismiss_stale_reviews: true`. A new push dismisses a stale approval, so the refine loop naturally forces
  a re-review. Required conversation resolution stays as-is. The `Review gate` required status context is
  removed.
- **Verdict submission.** The review session's output becomes a single native review call: inline comments plus
  `event: REQUEST_CHANGES` when findings are actionable, `event: APPROVE` when clean. A standing
  `REQUEST_CHANGES` blocks the merge by itself; the fix loop pushes, resolves the threads, and critic's
  re-review replaces its own prior verdict.
- **Out-of-scope PRs.** The review session's scope predicate (Rust-touching) is unchanged. A PR outside it
  receives a session-less critic `APPROVE` — the same semantics as today's auto-green `Review gate` status,
  submitted through the same channel as every other verdict so the required-review count is satisfied for every
  PR class.
- **Over-cap PRs.** A PR exceeding the changed-file cap (`AETHER_REVIEW_MAX_FILES`) receives no verdict at all
  and rests blocked at review-required — fail-closed, as today — until the owner dispatches a full review
  (`workflow_dispatch`) or reviews it natively himself.
- **Owner waiver.** `review:skip` is retired. The owner overrides a verdict natively: approve the PR (he is
  never the author of a fleet PR), and dismiss critic's standing `REQUEST_CHANGES` if one exists. Dismissal
  requires a stated reason, so the waiver carries its own audit trail — the actor-verification shell code has
  no native-flow equivalent to keep.
- **Reconciler.** The review facts collapse to one primary signal: `reviewDecision` (`REVIEW_REQUIRED` =
  verdict owed, `CHANGES_REQUESTED` = findings, `APPROVED` = verdict in and clean), alongside the existing
  unresolved-thread count. The `Review gate` status read and the `review:unresolved` label read are deleted.
  Dogfood's `dogfood:unresolved` signal is untouched — it is a QA verdict, not a review, and stays a label.

## Consequences

- The review history becomes legible in the standard GitHub UI: builder authored, critic requested changes,
  critic (or the owner) approved. No labels or status contexts to cross-reference.
- The verdict gates itself. There is no window where the label and the status mirror disagree, and no mirror
  job to keep honest.
- Every PR class now requires an approval, including the owner's own PRs — he cannot self-approve, so his PRs
  wait on critic like everyone else's. This is dogfooding, not an accident; if it chafes, an admin-bypass
  allowance in branch protection is the native escape hatch, decided separately.
- Session-authored PRs (opened in a supervised Claude session on the owner's token) author as the owner, so
  critic or builder can approve them; fleet PRs author as builder, so critic or the owner can. Critic is the
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
- **Builder as the reviewer** — rejected: GitHub 422s a verdict review on your own PR, and builder must remain
  the PR author for event-wake semantics. Commit authorship (the owner's public identity, ADR-0146 arc) is
  metadata; the self-approval rule keys on the PR's creating actor.
- **An owner PAT for the reviewer** — rejected: fleet actions become indistinguishable from the owner's,
  dissolving the supervised-vs-unsupervised boundary, and the owner then cannot approve fleet PRs he
  effectively authored.
- **A machine user account** — rejected: a second full account with PAT rotation and a seat, for a result the
  App gives scoped and rotation-free.
- **Keeping the mirror chain** — rejected: it is a bespoke re-implementation of review gating with more moving
  parts, no self-gating verdict, and an illegible history; the monster this ADR exists to retire.

## Amendment (2026-07-14): review bounce as a third terminal outcome (issue #3391)

The Decision above pins the *verdict* model to exactly two native events — `APPROVE` /
`REQUEST_CHANGES` — and states there is "no third outcome". That remains true of the **verdict**, but
the review *lifecycle* now grows a third terminal *outcome*: **bounce**. When the deep pass finds a
fundamental problem (a design flaw or a major security concern), or the confirm pass (the
one-deep-then-confirm lifecycle, issue #3390) judges the delta so large or divergent that only a
complete re-review would do, the right terminal is not another round of PR-level `REQUEST_CHANGES`
ping-pong — it is to send the work back to re-scoping. Standing owner directives: the deep pass runs at
most once per PR lifecycle; confirm outcomes are exactly {approve, escalate}; "needs a full re-review"
is itself the bounce signal, never a second deep pass.

Bounce is expressed **out-of-band of the native verdict**, never as a third native review event —
GitHub's review API exposes only `APPROVE` / `REQUEST_CHANGES` / `COMMENT`, and this ADR's merge gate
keys on the native `reviewDecision`, which cannot carry it. The core decision is therefore
**unchanged**: native required review still gates the merge on `APPROVE` / `REQUEST_CHANGES`, critic is
still the verdict identity, and a bounced PR still carries critic's native `REQUEST_CHANGES` so it
stays merge-blocked. Bounce lives *beside* the verdict, not in place of it. The mechanism:

- **Signal.** On a bounce, `scripts/post-review-rollup.mjs` takes a bounce path beside its two-way
  `verdictEvent` branch: it posts the reviewer's bounce reason and stamps a `review:bounce` label (plus
  `review:bounce-to:<phase>` carrying the resume phase — typically `design` for a fundamental design
  flaw, `plan` for a scope-level miss) on the PR. A label is the carrier because it survives on the PR
  for the reconciler to read and is not a native verdict, so it does not perturb the merge gate.
- **Reconciler edge.** `.github/workflows/reconciler.yml` gains a new first-match edge: a PR carrying
  `review:bounce` regresses its **linked issue** (via `closingIssuesReferences`) to `phase:bounced` +
  `bounce-to:<phase>` — the same atomic-`PUT` label reconcile `/bounce` performs — and records the
  bounce reason as an issue comment. This is a genuinely new source→target: the reconciler previously
  mapped only the native `reviewDecision` to `phase:{building,qa,findings,held}`, never to a phase
  regression.
- **PR disposition.** A bounced PR no longer merges; the work restarts from the regressed issue and a
  fresh `/implement` supersedes it. The bounced PR is **closed** (with a comment linking the regressed
  issue) so it does not sit stale in the review pool — the fresh implement PR carries the issue's
  closing keyword.

This is an amendment, not a supersession: the required-review merge gate — the whole subject of the
original Decision — stands intact. Bounce adds a review→issue-phase-regression edge that the two-outcome
verdict model had no room for, carried by a label rather than a native event precisely so the native
gate is untouched.
