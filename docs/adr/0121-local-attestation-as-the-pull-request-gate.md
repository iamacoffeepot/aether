# ADR-0121: Local Attestation as the Pull-Request Gate

- **Status:** Proposed
- **Date:** 2026-06-21

## Context

The expensive correctness checks — clippy, the full nextest suite, the wasm
cross-build, qodana — run twice for every change. A contributor runs them
locally through `scripts/preflight.sh` before pushing, and CI re-runs the same
set on the runner to gate the merge. The runner re-execution is the long pole
on every pull request, and for a change a collaborator has already validated
locally it re-derives a result we have high confidence in.

The checks are deterministic against a committed tree. If a trusted party can
prove they ran a specific check against a specific commit and it passed, the
runner re-run buys little for that party's own changes. The missing piece was a
way to carry that proof to the gate that the gate can trust without re-running
the work.

Three pieces, already built and merged, supply the proof:

- **Producer** (`scripts/attest.sh`, issue #2114 / PRs #2115, #2127).
  Runs each canonical check under `witness`, emitting one signed in-toto
  attestation per step, bound to the current commit through a product subject
  whose digest is `sha256(head_sha)`. It signs by repacking the author's
  existing SSH key to PKCS8 in tmpfs — the same public key already on the
  author's GitHub account, so nothing new is registered. Attestations publish to
  a side ref `refs/attestations/<sha>`, never into the tree or any branch.
- **Verifier** (`scripts/attest-verify.sh`, PR #2117). Resolves each
  attestation's signing key against the author's `github.com/<author>.keys`,
  confirms the author is a write-collaborator, checks every signature and the
  commit binding, and matches each step's recorded command against the canonical
  check string (so an attestation that ran `true` under the name "clippy" is
  rejected).
- **Verify workflow** (`.github/workflows/attest-verify.yml`, PRs #2117, #2124,
  #2126). Runs the verifier on `pull_request_target`, so the gate logic comes
  from the trusted base branch and never executes pull-request-head code.

What remained unspecified is how the merge gate consumes the proof: which check
becomes required, what happens to the heavy CI jobs, and how a contributor who
*cannot* attest is still gated. The trust model forces that last question — only
a write-collaborator can produce a verifiable attestation, because the proof is
"a collaborator vouches for their own run with a key GitHub already binds to
their identity." An outside contributor has no such key relationship, so the gate
cannot extend them the attested fast path.

## Decision

**The pull-request gate forks on whether the author is a write-collaborator.**
The classification is the same `repos/<repo>/collaborators/<author>/permission`
check the verifier already performs, run once up front and exposed as a job
output the downstream jobs branch on.

- **Collaborator pull request — attestation path.** `verify` is the gate. The
  heavy jobs (clippy, test, qodana, doc, dist) are skipped on the pull request.
  A collaborator who forgets to publish attestations gets a failing `verify`,
  not a silent fallback to the runner — the fast path stays honest because the
  only way to satisfy the gate is to produce the proof.
- **Non-collaborator pull request — real-CI path.** The heavy jobs run on the
  runner and gate the merge exactly as they do today. `verify` is not
  applicable and does not block, because an outside contributor has no key
  relationship to sign with.
- **Push to main — canary.** The heavy jobs always run on push to `main`, after
  the merge. This is the independent backstop for the attestation path: the
  attested merge trusted the collaborator's local run, and the canary re-derives
  that result on the runner against the merged tree. A red canary means a bad or
  mistaken attestation landed, surfaced through the normal failed-run
  notification on `main`.

Nothing merges ungated. Trust — skipping the runner re-run — is extended only to
write-collaborators, and even for them it is provisional until the post-merge
canary confirms it independently.

GitHub branch protection requires *all* listed checks, so the collaborator/
non-collaborator fork (one branch passes, the other is skipped) cannot be
expressed as two separate required checks. A single required aggregator
(`CI pass`, kept) resolves the fork: it `needs` both the heavy jobs and
`verify`, and passes when the path that applies to this author class passed —
`verify` green for a collaborator, the heavy jobs green for a non-collaborator.
The required set on `main` becomes `Lint PR title` + `CI pass`, unchanged in
shape; what changes is which jobs feed the aggregator under which author class.

There is **no nightly re-validation and no separate alerting** beyond the
push-to-main canary and its default failed-run notification. The canary is the
only backstop.

## Consequences

- A collaborator's pull request no longer waits on a runner re-run of checks
  they already passed locally; the gate is the seconds-long `verify` plus the
  one-time producer run they invoke before pushing. This is the saving the whole
  arc was built for.
- Outside contributions are unaffected and fully gated by real CI, with no new
  expectation placed on a contributor who cannot attest.
- A bad attestation (a check that passed locally but should not have, or local
  state that diverges from the committed tree) can merge and is caught only
  after landing, by the canary. The window is "from merge to canary completion."
  This is the deliberate residual risk of trusting a collaborator's local run;
  the canary bounds it rather than eliminating it.
- The canonical check list now lives in three places that must agree —
  `scripts/preflight.sh`, `scripts/attest.sh`, and `.github/workflows/ci.yml` —
  because the verifier's command-match only means something if all three name
  the same gates. Unifying them onto one shared definition is follow-on work
  this decision makes load-bearing rather than merely tidy.
- Branch protection itself does not change shape (`Lint PR title` + `CI pass`).
  The behavioral change is entirely in how `ci.yml` gates and feeds the
  aggregator, which keeps the protection-rule edit small and reversible.

## Alternatives considered

- **Fork on whether an attestation is present**, rather than on collaborator
  status. Rejected: a collaborator who forgot to attest would silently fall back
  to the runner, blurring the gate's expectation per author class, and the
  determinant (a side ref exists) is less stable than membership, which is
  decided once up front.
- **Keep the heavy jobs required on collaborator pull requests too**, running
  both attestation and real CI for a confidence-building period. Rejected as the
  steady state because it forgoes the entire saving; it remains available as a
  temporary rollback by re-enabling the jobs' pull-request trigger if the canary
  ever shows the attested path is untrustworthy.
- **Nightly re-validation of `main`.** Rejected as redundant: the push-to-main
  canary already re-derives every merge's checks on the runner, so a scheduled
  re-run would only re-test already-canaried commits.
- **Maintainer re-attests outside pull requests.** Rejected: it puts a manual
  local run in the path of every external contribution, where real CI already
  gates them automatically at no maintainer cost.
```
