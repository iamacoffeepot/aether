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
- the current head's checks, trusted hidden direct-review verdict, native review blockers, review threads, declared-surface diff, and dogfood evidence determine whether it is landable;
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

The Plan digest includes, in scope-owned order, the exact UTF-8 spans for Problem statement, Design notes, Implementation plan, optional Sub-issues, optional Depends on, Declared surface, and Dogfood brief. It deliberately excludes Side findings and every unmanaged section. The only layout byte excluded from a managed span is one empty-line separator immediately before a following H2: the content line ending remains, while an additional blank line and the exact LF/CRLF spelling remain approval-bearing. Use `approve/scripts/plan_digest.py`; do not reproduce its parser or canonicalization in another skill.

## Trusted approval records

An approval is one canonical single-line HTML comment in the issue body's unmanaged prefix before the first managed H2:

```text
<!-- aether-approval:v2 {"authority":"owner","base_sha":"<full commit>","effective_tier":"human","issue":123,"model":"opus","plan_sha256":"<64 lowercase hex>","policy_tier":"human","size":"l"} -->
```

Keep records in append order in the hidden evidence history immediately before `## Problem statement`. Direct-review records defined below share that append-only history. The hidden prefix is outside every managed Plan span, so appending a record does not alter the digest it carries. Never place a record inside or after a managed section. Parse approval records only with `approve/scripts/approval_records.py`; it requires the exact one-line wrapper, compact sorted JSON, the eight keys shown, strict types and enums, and optional issue identity.

The payload's authority is descriptive; trust comes from the effective editor of the current body. Query the issue's latest `userContentEdits` editor through GraphQL; when GitHub reports no edit, use the issue author. Owner authority requires the effective editor to be the repository owner. Policy-auto authority requires the effective editor to be the owner or to have repository write permission. A later edit by anyone else makes every body record untrusted until a permitted editor revalidates the current body. A failed, truncated, or ambiguous provenance read is unknown authority, never a pass.

A current approval matches all of:

- the issue number;
- the freshly recomputed Plan digest, size, and implementation model;
- the captured base commit;
- the policy and effective tiers resolved for that same base;
- an authority permitted for the effective tier.

Any managed approval-bearing edit changes the digest. A different base commit requires a new approval. Changes to Side findings or unmanaged prose do not. Preserve old v2 lines byte-for-byte; non-matching records are durable history, not current authority. When several body records match, use the last trusted one in body order. Appending an exact matching record is idempotent and must not add another line.

During migration only, when no current trusted v2 body record exists, consumers may fall back to a legacy v1 issue comment with exactly these two lines:

```text
<!-- aether-approval:v1 -->
{"authority":"owner","base_sha":"<full commit>","effective_tier":"human","issue":123,"model":"opus","plan_sha256":"<64 lowercase hex>","policy_tier":"human","size":"l"}
```

The v1 comment must satisfy the same strict payload and current-identity checks. Trust still comes from its GitHub `author_association`: owner authority requires `OWNER`, while policy-auto accepts `OWNER`, `MEMBER`, or `COLLABORATOR`. Approve never writes v1. Once an equivalent v2 line has been inserted and verified, the redundant visible v1 comment may be deleted; otherwise old comments remain read-only history.

## Trusted direct-review verdicts

GitHub forbids a pull-request author from submitting a native `APPROVE` review on their own pull request. Direct review therefore records its semantic verdict in the closing issue's hidden evidence history, never as a claimed native approval or a machine-formatted pull-request review or comment. The canonical record is one physical line with this exact wrapper and compact sorted JSON:

```text
<!-- aether-direct-review:v2 {"head_sha":"<40 lowercase hex>","issue":123,"plan_sha256":"<64 lowercase hex>","pull_request":456,"verdict":"APPROVE"} -->
```

The JSON object has exactly the five keys shown in that order and no whitespace outside JSON strings. `issue` and `pull_request` are positive integers; `verdict` is `APPROVE` or `REQUEST_CHANGES`. Append the record immediately before `## Problem statement`, after every earlier hidden evidence line, and preserve all older approval and direct-review records byte-for-byte. Never place it inside a managed Plan span.

A trusted direct-review artifact must satisfy every condition below:

- it was read from the body of the current pull request's unique closing issue;
- its wrapper and compact JSON strictly parse as the canonical single-line artifact above;
- the effective body editor is the repository owner, member, or collaborator; resolve the latest `userContentEdits` editor through GraphQL (or the issue author when GitHub reports no edit), then verify that login's repository ownership or collaborator relationship instead of borrowing the original author's association for an edited body;
- its payload `issue` is the closing issue number;
- its payload `head_sha` and the freshly re-read pull-request head are the same full commit;
- its payload `pull_request` is the current pull-request number; and
- its payload `plan_sha256` matches a fresh digest of the current closing issue.

Do not trust a login name, payload field, issue comment, pull-request review or comment, native `APPROVED` review, legacy pull-request machine record, or record for another head as a substitute. A head or managed-Plan change makes prior artifacts stale automatically. Among valid trusted v2 records for the exact current issue, pull request, head, and digest, the last one in body order is the semantic verdict. `REQUEST_CHANGES` enters or durably records repair, while `APPROVE` satisfies only the direct-review gate.

Appending is idempotent and concurrent-edit safe. Immediately before mutation, re-read the pull request head, closing issue number/title/body, effective editor, and Plan digest. If the last valid current-fact record already has the desired verdict, do not append another. Otherwise build the complete candidate body in a temporary file with `apply_patch`, changing only the insertion immediately before `## Problem statement`. Re-read the body immediately before `PATCH` and require it to equal the source snapshot byte-for-byte; if it changed, rebuild from the fresh body instead of overwriting either edit. Send the file with `-F body=@<path>`, then re-read the body and effective editor and require the appended line and all trust checks to pass. Never edit or delete older artifacts.

Native review state remains an independent blocker. Read paginated pull-request reviews for native decisions only. For each reviewer, consider their newest non-dismissed native decision review (`APPROVED` or `CHANGES_REQUESTED`) across the pull request; a latest `CHANGES_REQUESTED` remains active across later commits until that reviewer submits a later `APPROVED` decision or GitHub reports the request dismissed. It blocks implementation success and landing even when the hidden semantic artifact says `APPROVE`. A hidden issue-body record cannot clear it. Every unresolved review thread also blocks independently. Native `APPROVED` reviews may satisfy branch protection, but they neither create nor replace the trusted direct-review artifact.

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
- Hidden body comments hold approval and direct-review machinery. Visible issue and pull-request comments or reviews are only for concise human-directed evidence; do not post machine JSON/HTML or synthetic progress state.

## Pull-request facts

Before implementation, review, or landing, correlate the closing issue, base branch, head branch, and owned issue worktree. Reject ambiguous or duplicate associations. Always evaluate checks, reviews, and threads for the current head SHA.

Declared-surface containment is a hard gate. Parse the issue's validated surface, enumerate `git diff --name-only <base>...<head>`, and reject every changed path not matched by the canonical surface matcher. Re-run containment after every corrective push and immediately before landing.

Review acceptance requires the newest trusted hidden direct-review artifact for the current issue, pull request, head, and digest to say `APPROVE`, no active per-reviewer native `CHANGES_REQUESTED` decision, and no unresolved review thread. These three gates are evaluated separately. Dogfood is required only when the issue's Dogfood brief is not an `N/A` artifact; its result must identify the current head and be clear of actionable findings.

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
