# Aether GitHub Workflow Contract

Repository: `iamacoffeepot/aether`.

Use `gh api` REST endpoints whenever REST can perform the operation. Avoid GraphQL-backed convenience commands. Use GraphQL only to enumerate and resolve pull-request review threads and to mark a draft pull request ready for review.

## Durable workflow evidence

The contributor workflow is direct-drive. GitHub issue labels describe taxonomy only; they are not workflow state, routing state, approval, or progress.

Derive the current state from durable artifacts:

- an open issue without all required managed sections is unscoped;
- an open issue with complete managed sections and valid routing lines is planned;
- a planned issue is approved only by a current trusted approval comment defined below;
- an owned issue worktree or branch is implementation work in progress;
- a draft pull request is the reviewable implementation artifact;
- the current head's checks, reviews, review threads, declared-surface diff, and dogfood evidence determine whether it is landable;
- a merged pull request whose closing issue is closed is done.

Never infer one fact from another. A branch does not prove approval, a green check does not prove review acceptance, and a closed issue does not prove that a named pull request merged.

## Managed issue artifacts

Scope owns these exact H2 sections, in this order:

```text
## Problem statement
## Design notes
## Implementation plan
## Sub-issues
## Depends on
## Declared surface
## Dogfood brief
## Side findings
```

`Sub-issues`, `Depends on`, and `Side findings` are optional. The other five sections are required for a planned issue. Reject duplicate managed headings.

The Implementation plan ends with exactly these three non-empty lines:

```text
**Size:** <s|m|l>
**Implementation model:** <haiku|sonnet|opus>
**Routing reason:** <one concise reason>
```

The Plan digest includes, in scope-owned order, the exact UTF-8 spans for Problem statement, Design notes, Implementation plan, optional Sub-issues, optional Depends on, Declared surface, and Dogfood brief. It deliberately excludes Side findings and every unmanaged section. Use `approve/scripts/plan_digest.py`; do not reproduce its parser or canonicalization in another skill.

## Trusted approval comments

An approval is one immutable issue comment with exactly two lines: the marker and one compact JSON object.

```text
<!-- aether-approval:v1 -->
{"authority":"owner","base_sha":"<full commit>","effective_tier":"human","issue":123,"model":"opus","plan_sha256":"<64 lowercase hex>","policy_tier":"human","size":"l"}
```

The JSON object has exactly the eight keys shown. Validate types and enum values strictly. `authority` is either `owner` or `policy-auto`. The payload's authority is descriptive; trust comes from the comment author and GitHub's `author_association`. Owner authority requires `OWNER`. Policy-auto authority requires `OWNER`, `MEMBER`, or `COLLABORATOR`. Ignore comments from any other association even when their payload claims authority.

A current approval matches all of:

- the issue number;
- the freshly recomputed Plan digest, size, and implementation model;
- the captured base commit;
- the policy and effective tiers resolved for that same base;
- an authority permitted for the effective tier.

Any managed approval-bearing edit changes the digest. A different base commit requires a new approval. Changes to Side findings or unmanaged prose do not. Never edit or delete old approval comments; non-matching records are durable history, not current authority. When several comments match, use the newest trusted one. Posting an exact matching record is idempotent and must not create another comment.

## REST reads

Prefer one shaped read over several convenience calls:

```text
gh api repos/iamacoffeepot/aether/issues/<N> \
  --jq '{number,title,body,state,state_reason,user:.user.login,author_association,labels:[.labels[].name]}'
```

Use paginated REST endpoints for comments, issue timelines, pull requests, reviews, commits, check suites, and check runs. Verify comment trust with `author_association`. A failed or truncated read is unknown state, never an empty set.

## Bodies and comments

- Put outbound markdown and JSON in a temporary file using `apply_patch`; never interpolate issue or review text into a shell command.
- Create or edit with file inputs such as `-F body=@/tmp/aether-issue-<N>.md`.
- Preserve every unmanaged body byte when replacing managed sections.
- Immediately before a full-body `PATCH`, re-read issue number, title, and body. Abort on a concurrent managed-section edit; merge only non-overlapping user prose.
- Comments hold immutable approvals and concise human-directed evidence. Do not post synthetic progress state.

## Pull-request facts

Before implementation, review, or landing, correlate the closing issue, base branch, head branch, and owned issue worktree. Reject ambiguous or duplicate associations. Always evaluate checks, reviews, and threads for the current head SHA.

Declared-surface containment is a hard gate. Parse the issue's validated surface, enumerate `git diff --name-only <base>...<head>`, and reject every changed path not matched by the canonical surface matcher. Re-run containment after every corrective push and immediately before landing.

Review acceptance requires no current `CHANGES_REQUESTED`, the required approving verdict for the current head, and no unresolved review thread. Dogfood is required only when the issue's Dogfood brief is not an `N/A` artifact; its result must identify the current head and be clear of actionable findings.

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
- When implementation discovers a broken Plan assumption, hand the issue back with `$scope <issue> --phase plan` and evidence. Use `design` for a failed design choice and `define` for unclear intent.
- Never expand the declared surface to make an implementation fit. Scope must revise the artifact and approval must be recomputed.
- Do not merge, delete a worktree, or delete a branch until REST proves the named pull request merged and the worktree is clean.
