# Aether GitHub Workflow Contract

Repository: `iamacoffeepot/aether`.

Use `gh api` REST endpoints whenever REST can perform the operation. Avoid GraphQL-backed convenience commands. Use GraphQL only to read issue edit provenance, enumerate and resolve pull-request review threads, and mark a draft pull request ready for review.

## Durable workflow evidence

The contributor workflow is direct-drive. GitHub issue labels describe taxonomy only; they are not workflow state, routing state, approval, or progress.

Derive the current state from durable artifacts:

- an open issue without all required managed sections is unscoped;
- an open issue with complete managed sections and valid routing lines is planned;
- a planned issue is approved only by a current trusted hidden approval record defined below;
- an owned issue worktree or branch is implementation work in progress;
- a draft pull request is the reviewable implementation artifact;
- the current head's checks, native review blockers, review threads, and priced surface overflow determine whether it is landable;
- a merged pull request whose closing issue is closed is done.

Never infer one fact from another. A branch does not prove approval, a green check does not prove that no change request or thread is open, and a closed issue does not prove that a named pull request merged.

## Managed issue artifacts

Scope owns these exact H2 sections, in this order:

```text
## Problem statement
## Design notes
## Implementation plan
## Sub-issues
## Depends on
## Declared surface
## Side findings
```

`Sub-issues`, `Depends on`, and `Side findings` are optional. The other four sections are required for a planned issue. Reject duplicate managed headings.

The Implementation plan ends with exactly these three non-empty lines:

```text
**Size:** <s|m|l>
**Implementation model:** <haiku|sonnet|opus>
**Routing reason:** <one concise reason>
```

The Plan digest includes, in scope-owned order, the exact UTF-8 spans for Problem statement, Design notes, Implementation plan, optional Sub-issues, optional Depends on, and Declared surface. The digest deliberately excludes Side findings and every unmanaged section. The only layout byte excluded from a managed span is one empty-line separator immediately before a following H2: the content line ending remains, while an additional blank line and the exact LF/CRLF spelling remain approval-bearing. Use `approve/scripts/plan_digest.py`; do not reproduce its parser or canonicalization in another skill.

## Trusted approval records

An approval is one canonical single-line HTML comment in the issue body's unmanaged prefix before the first managed H2:

```text
<!-- aether-approval:v2 {"authority":"owner","base_sha":"<full commit>","effective_tier":"human","issue":123,"model":"opus","plan_sha256":"<64 lowercase hex>","policy_tier":"human","size":"l"} -->
```

Keep records in append order in the hidden evidence history immediately before `## Problem statement`. Any other hidden line already in that history is preserved byte-for-byte and read by nothing. The hidden prefix is outside every managed Plan span, so appending a record does not alter the digest it carries. Never place a record inside or after a managed section. Parse approval records only with `approve/scripts/approval_records.py`; it requires the exact one-line wrapper, compact sorted JSON, the eight keys shown, strict types and enums, and optional issue identity.

The payload's authority is descriptive; trust comes from the effective editor of the current body. Query the issue's latest `userContentEdits` editor through GraphQL; when GitHub reports no edit, use the issue author. Owner authority requires the effective editor to be the repository owner. Policy-auto authority requires the effective editor to be the owner or to have repository write permission. A later edit by anyone else makes every body record untrusted until a permitted editor revalidates the current body. A failed, truncated, or ambiguous provenance read is unknown authority, never a pass.

A current approval matches all of:

- the issue number;
- the freshly recomputed Plan digest, size, and implementation model;
- the captured base commit;
- the policy and effective tiers resolved for that same base;
- an authority permitted for the effective tier.

Any managed approval-bearing edit changes the digest. A different base commit requires a new approval. Changes to Side findings or unmanaged prose do not. Preserve old v2 lines byte-for-byte; non-matching records are durable history, not current authority. When several body records match, use the last trusted one in body order. Appending an exact matching record is idempotent and must not add another line. A trusted v2 body record is the only accepted approval: an issue comment, whatever its marker or author association, never carries approval authority.

## Native review blockers

No hidden record carries a review verdict, and landing requires none. Two native GitHub facts block a draft independently of its checks.

Read paginated pull-request reviews for native decisions only. For each reviewer, consider their newest non-dismissed native decision review (`APPROVED` or `CHANGES_REQUESTED`) across the pull request; a latest `CHANGES_REQUESTED` remains active across later commits until that reviewer submits a later `APPROVED` decision or GitHub reports the request dismissed. It blocks implementation success and landing, and no issue-body record or comment can clear it. Every unresolved review thread also blocks independently. A native `APPROVED` review may satisfy branch protection; nothing in this workflow requires one.

## REST reads

Prefer one shaped read over several convenience calls:

```text
gh api repos/iamacoffeepot/aether/issues/<N> \
  --jq '{number,title,body,state,state_reason,user:.user.login,author_association,labels:[.labels[].name]}'
```

Use paginated REST endpoints for comments, issue timelines, pull requests, reviews, commits, check suites, and check runs. Verify comment trust with `author_association`. A failed or truncated read is unknown state, never an empty set.

## Bodies and comments

- Put outbound markdown and JSON in a temporary file using the harness's file-edit tool; never interpolate issue or review text into a shell command.
- Create or edit with file inputs such as `-F body=@/tmp/aether-issue-<N>.md`.
- Preserve every unmanaged body byte when replacing managed sections.
- Immediately before a full-body `PATCH`, re-read issue number, title, and body. Abort on a concurrent managed-section edit; merge only non-overlapping user prose.
- Hidden body comments hold approval machinery. Visible issue and pull-request comments or reviews are only for concise human-directed evidence; do not post machine JSON/HTML or synthetic progress state.

## Pull-request facts

Before implementation, review, or landing, correlate the closing issue, base branch, head branch, and owned issue worktree. Reject ambiguous or duplicate associations. Always evaluate checks, reviews, and threads for the current head SHA.

Surface overflow is priced, never forbidden. Enumerate changed paths with `git diff --name-only --no-renames origin/main...<head>` and price the paths outside the approved surface with `resolve_approval_tier.py --ref <approval base_sha> --surface-file … --changed-file …`. An overflow path under `docs/adr/` that is a new ADR, or that edits an ADR not `Status: Proposed` at the base, prices `human`. An unsafe path spelling or any resolver error prices the whole overflow `human`. The pricing rules are frozen when work starts: `/implement` writes the resolver's `policy_blob` and `matcher_blob` once into the draft's `## Approval` section as `Pricing policy:` and `Pricing matcher:` lines, and `/land` requires the resolver's reported blobs at the approval base to equal them. Missing lines mean `/land` derives the blobs from the base and states them; a mismatch means everything prices `human`. Auto-tier overflow lands with no further verdict; judge-tier overflow needs the landing agent's own `ACCEPT` for every path; human-tier overflow needs the owner's explicit confirmation naming the pull request, in the landing session. Every verdict and confirmation is bound to the head: re-price after every push and immediately before merge. `/land` records the result as one plain-prose "Surface overflow" pull-request comment, edited in place when it already exists, as evidence rather than authority.

Review acceptance requires no active per-reviewer native `CHANGES_REQUESTED` decision and no unresolved review thread. The two gates are evaluated separately.

## Common mutations

```text
Create issue:  POST repos/iamacoffeepot/aether/issues
Edit issue:    PATCH repos/iamacoffeepot/aether/issues/<N>
Comment:       POST repos/iamacoffeepot/aether/issues/<N>/comments
Create draft:  POST repos/iamacoffeepot/aether/pulls  (draft=true)
Read PR:       GET repos/iamacoffeepot/aether/pulls/<PR>
PRs by head:   GET repos/iamacoffeepot/aether/pulls?head=iamacoffeepot:<branch>&state=<state>
Check runs:    GET repos/iamacoffeepot/aether/commits/<sha>/check-runs
Reviews:       GET repos/iamacoffeepot/aether/pulls/<PR>/reviews
Merge:         PUT repos/iamacoffeepot/aether/pulls/<PR>/merge  (merge_method=squash)
```

Review-thread enumeration and resolution use the GraphQL `reviewThreads` query and `resolveReviewThread` mutation. Clearing draft state uses `markPullRequestReadyForReview`.

## Failure discipline

- Re-read after an uncertain mutation before retrying, so a timeout cannot duplicate an issue, comment, pull request, review, or merge.
- Preserve owned worktrees and branches on authentication, network, runner, or service failure. Report the concrete failing operation; do not encode the outage in issue metadata.
- When implementation discovers a broken Plan assumption, hand the issue back with `/scope <issue> --phase plan` (`$scope` in Codex) and evidence. Use `design` for a failed design choice and `define` for unclear intent.
- Never edit Declared surface from implementation, resolution, or landing; overflow is priced instead.
- Do not merge, delete a worktree, or delete a branch until REST proves the named pull request merged and the worktree is clean.
