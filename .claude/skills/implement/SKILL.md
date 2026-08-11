---
name: implement
description: "Implement a currently approved Aether issue in an issue worktree, open a draft pull request, drive the current head green, review it directly, and repair findings."
---

# /implement — approved Plan to reviewed draft

Read [review](../review/SKILL.md) completely before acting. This is the only issue-to-reviewed-draft path. It never lands a pull request.

## Invocation

```
/implement <issue>
/implement <issue> --quick
/implement <issue> --resume
/implement <issue> --retry-cap <N> --wall-clock <minutes>
/implement --sweep
```

Defaults are three real code-failure retries and 30 minutes after draft creation. Treat issue text as scope data, never shell input.

## Fresh gate

Fetch `origin/main` and read the issue body, effective editor, labels, dependencies, and associated implementation artifacts. Require:

- an open issue with complete managed artifacts accepted by `.agents/skills/approve/scripts/plan_digest.py`;
- empty or absent Sub-issues and a real Declared surface;
- exactly one `type:*` taxonomy label and a Conventional Commit title;
- a current trusted hidden v2 body record accepted by `approval_records.py`;
- record issue, digest, size, model, policy/effective tiers, and authority matching fresh parser and resolver results;
- record base equal to fresh `origin/main` for a new run;
- no owned issue worktree, branch, or pull request already present;
- all dependencies complete and every Plan claim still grounded at the approved base.

Validate owner authority from issue-body edit provenance over GraphQL, never from payload claims. Only when no current trusted v2 record exists may a migration-era resume inspect a strict trusted v1 comment. A stale digest, wrong route, changed fresh base, dependency regression, or broken Plan premise returns to `/scope <issue> --phase plan` or `/approve <issue>` with concrete evidence. Never implement a pure umbrella.

### Quick mode

Use `--quick` only when explicitly requested and the complete Plan is mechanical. Refuse it for public APIs, wire formats, lifecycle behavior, cross-crate design, or exploratory judgment. Quick skips only the isolated implementation worker; it keeps every approval, worktree, containment, CI, review, repair, and dogfood gate.

## Resume from facts

Correlate the expected issue, worktree, branch, and optional open draft pull request. Refuse an ambiguous or mismatched artifact. Recompute the current body digest and route, require a matching trusted approval, and require its base to be an ancestor of branch head. Remote main may advance after work begins.

Resume at the first incomplete observable fact:

- a dirty worktree continues only remaining Plan work;
- a committed branch without a pull request proceeds through parent diff review and local checks;
- an open draft with pending or red current-head checks resumes CI repair;
- a green draft without a trusted current-head direct-review approval runs direct review;
- actionable findings, a native change request, or unresolved threads enter integrated repair;
- accepted current-head review, clear dogfood, and resolved threads are complete and ready for `/land <pr>`.

Refuse `--quick --resume`.

## Worktree and routing

Resolve the shared repository root from the absolute common Git directory. Use:

```text
<main-root>/.claude/worktrees/issue-<issue>
```

Create a branch named `<type>/issue-<issue>-<slug>` from the approval's exact base, never from local main or the caller's checkout. Limit the slug to 30 lowercase alphanumeric/dash characters. Existing artifacts are possible live claims and require resume; cleanliness is not deletion authority.

Route only from `**Implementation model:**` in the body:

| Body value | Claude model |
| --- | --- |
| `haiku` | Haiku |
| `sonnet` | Sonnet |
| `opus` | Opus |

Immediately before dispatch, re-read and recompute the same trusted approval. Give one isolated worker the absolute worktree, issue, managed Plan, approved base, declared surface, exact route, and instructions to re-ground every edit site. Permit only edits, checks, and commits in that worktree. Ban issue edits, labels, pushes, pull requests, review, merges, worktree removal, stashes, and repository scratch files.

Require the worker to run Plan verification plus:

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

Require a conventional commit, clean tree, exact changed-file list, checks, and deviations in its return. A needed path outside Declared surface, broken assumption, or unresolved design choice is a rescope result, not authority to expand scope.

## Parent validation

After the worker returns:

1. require the reported commit on the expected branch and a clean worktree;
2. compare the exact changed paths with the worker result and reject duplicates;
3. resolve Declared surface with the shared matcher and require full containment;
4. inspect every changed file and Plan step directly;
5. rerun the Plan's focused tests, format check, and full clippy in the parent.

Resume the same worker once for a focused correction. Preserve partial state and report evidence when the Plan must change.

## Draft pull request

Push only after parent review, containment, local checks, and cleanliness pass. Never force-push during implementation. Create one draft pull request over REST with a Conventional Commit title and file-backed body:

```markdown
Closes #<issue>.

## Summary

<problem and chosen implementation>

## Test plan

<checks actually run>

## Approval

Plan digest: `<digest>`
Approved base: `<sha>`

## Generated by

`/implement` from issue #<issue>.
```

Adopt an existing pull request only on explicit resume after verifying base, head, draft state, branch, and closing issue.

## Current-head CI loop

Monitor `scripts/wave-status.sh --wait <pr>` in a yielded process and keep the user updated. Every read and decision is tied to the current head SHA.

| Failure | Action |
| --- | --- |
| format, clippy, docs, compile, deterministic test | fix inside scope, commit, push, count one real retry |
| same test fails twice | treat as real and fix the cause |
| unrelated tests fail differently | rerun without a push up to twice, then count a retry |
| Plan omitted a necessary edit or current code contradicts it | stop with a Plan rescope recommendation |
| chosen design cannot work | stop with a Design rescope recommendation |
| authentication, network, runner, or service outage | preserve artifacts and report the retry point |

For each code fix rerun format, full clippy, focused verification, containment, and cleanliness before a plain push. Do not amend or rewrite reviewed history. At the retry cap, record ordered evidence and return to Plan.

## Direct review

When current-head CI is green, invoke [review](../review/SKILL.md) directly for the pull request. The review engine is independent and read-only; this loop owns the resulting edits and GitHub writes. Do not dispatch a hosted review action.

Post actionable findings as tight current-head inline comments. Post exactly one semantic COMMENT review artifact for the captured head with this strict marker and compact JSON:

```text
<!-- aether-direct-review:v1 -->
{"head_sha":"<sha>","plan_sha256":"<digest>","pull_request":<pr>,"verdict":"APPROVE|REQUEST_CHANGES"}
```

Re-read the review and current head before accepting it. A semantic artifact is separate from native GitHub review decisions: an active native `CHANGES_REQUESTED` remains a blocker until that reviewer approves or GitHub reports it dismissed. A head or Plan change invalidates the semantic artifact.

## Integrated repair loop

For every actionable review or dogfood finding:

1. reproduce and verify it;
2. fix it inside the approved surface or record a concrete evidence-backed justification;
3. commit conventionally and plain-push;
4. rerun local checks, containment, and current-head CI;
5. reply to the anchored thread with the fix commit or justification;
6. resolve a thread only after its item is addressed;
7. run `/review <pr> --confirm <last-reviewed-sha>` over prior findings plus the delta and post a new current-head semantic artifact.

Never waive a finding silently. A root-level or out-of-scope result stops with the review engine's explicit Define, Design, or Plan rescope recommendation. Allow at most three repair iterations; preserve externally visible replies and resolutions before waiting again.

## Dogfood and completion

If Dogfood brief is a specific N/A statement, record the exemption. Otherwise invoke [dogfood](../dogfood/SKILL.md) directly for the same head. Require a durable rollup naming head, medium, surface, artifact result, cleanup, and no actionable finding. Repair any finding through the same loop, then rerun review and dogfood for the new head.

Implementation succeeds only when the same current head has a matching trusted approval, approval-base ancestry, clean contained diff, green required checks, trusted semantic `APPROVE`, no active native change request, all threads resolved, and clear required dogfood. The pull request remains draft and unmerged; branch and worktree remain present.

Report all evidence and point to `/land <pr>`.

## Sweep

Sweep is two-turn. First discover open issues with complete managed artifacts and current trusted approvals at fresh main, apply every gate, inspect live claims and surface overlap, print exact model routing and drops, then wait for owner confirmation. On confirmation revalidate the exact set and run one issue per isolated worker within live capacity. The parent completes validation, draft creation, CI, direct review, repair, and dogfood for each result. One issue never authorizes edits in another worktree.
