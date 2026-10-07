---
name: implement
description: "Implement a currently approved Aether issue in an issue worktree, open a draft pull request, and hand it back at the wait on its checks; a resume repairs one red head or one native review blocker and hands back again."
---

# /implement — approved Plan to draft pull request

Read the shared [GitHub workflow contract](../../../.agents/skills/_shared/github-workflow.md) completely before acting. This is the only issue-to-draft path. It never lands a pull request, and it never waits on checks: an agent ends at a wait.

## Invocation

```
/implement <issue>
/implement <issue> --quick
/implement <issue> --resume
/implement --sweep
```

Each invocation runs to the next wait and ends there. The session that dispatched it owns the wait and the retry count (see [Hand back at the draft](#hand-back-at-the-draft)). Treat issue text as scope data, never shell input.

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

Validate owner authority from issue-body edit provenance over GraphQL, never from payload claims. A stale digest, wrong route, changed fresh base, dependency regression, or broken Plan premise returns to `/scope <issue> --phase plan` or `/approve <issue>` with concrete evidence. Never implement a pure umbrella.

### Quick mode

Use `--quick` only when explicitly requested and the complete Plan is mechanical. Refuse it for public APIs, wire formats, lifecycle behavior, cross-crate design, or exploratory judgment. Quick skips only the isolated implementation worker; it keeps every approval, worktree, check, and repair gate.

## Resume from facts

Correlate the expected issue, worktree, branch, and optional open draft pull request. Refuse an ambiguous or mismatched artifact. Recompute the current body digest and route, require a matching trusted approval, and require its base to be an ancestor of branch head. Remote main may advance after work begins.

Resume at the first incomplete observable fact:

- a dirty worktree continues only remaining Plan work;
- a committed branch without a pull request proceeds through parent diff review and local checks;
- an open draft with pending current-head checks reports the pull request number and head SHA and ends;
- an open draft with a red current-head check applies the repair table once: one fix, one push, then ends;
- a green draft with an active native change request or an unresolved thread enters the repair loop;
- a green current head with no native change request and every thread resolved is complete and ready for `/land <pr>`.

Refuse `--quick --resume`.

## Worktree and routing

Resolve the shared repository root from the absolute common Git directory. Use:

```text
<main-root>/.agents/worktrees/issue-<issue>
```

Create a branch named `<type>/issue-<issue>-<slug>` from the approval's exact base, never from local main or the caller's checkout. Limit the slug to 30 lowercase alphanumeric/dash characters. Existing artifacts are possible live claims and require resume; cleanliness is not deletion authority.

Route only from `**Implementation model:**` in the body:

| Body value | Claude model |
| --- | --- |
| `haiku` | Haiku |
| `sonnet` | Sonnet |
| `opus` | Opus |

Immediately before dispatch, re-read and recompute the same trusted approval. Before dispatch, run the resolver at the approved base and keep its `policy_blob`/`matcher_blob` as the frozen pricing rules. Dispatch one isolated worker as the `implementer` agent type (`.claude/agents/implementer.md`), whose one-hour prompt cache survives a build it needs mid-work, and give it the absolute worktree, issue, managed Plan, approved base, declared surface as the prepaid forecast, exact route, and instructions to re-ground every edit site. The worker may change any path the Plan's problem needs. Permit only edits, checks, and commits in that worktree. Ban issue edits, labels, pushes, pull requests, review, merges, worktree removal, stashes, repository scratch files, and waiting on any gate.

Require the worker to run Plan verification plus:

```bash
cargo fmt -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Require a conventional commit, clean tree, exact changed-file list, checks, and deviations in its return. A broken assumption or unresolved design choice is a rescope result, not authority to expand scope.

## Parent validation

After the worker returns:

1. require the reported commit on the expected branch and a clean worktree;
2. compare the exact changed paths with the worker result and reject duplicates;
3. compute the priced overflow with the resolver's changed mode at the approved base;
4. inspect every changed file and Plan step directly;
5. list every `Option` and `.take()` the diff adds in non-test code and check each against design rule R-0048 (`docs/guide/contributing/design-rules.md`): a lookup that can miss, or a value fixed at construction that never existed and never will, stays, and the pull request body names it with that meaning; anything else goes back to the worker before the push;
6. rerun the Plan's focused tests, format check, and full clippy in the parent.

Resume the same worker once for a focused correction. When the resume is refused because the worker's cache has expired, dispatch a fresh `implementer` with a short brief built from the worktree's observable state. Preserve partial state and report evidence when the Plan must change.

## Draft pull request

Push only after parent review, overflow pricing, local checks, and cleanliness pass. Never force-push during implementation. Create one draft pull request over REST with a Conventional Commit title and file-backed body:

```markdown
Closes #<issue>.

## Summary

<problem and chosen implementation>

## Test plan

<checks actually run>

## Approval

Plan digest: `<digest>`
Approved base: `<sha>`
Pricing policy: `<policy_blob>`
Pricing matcher: `<matcher_blob>`

## Generated by

`/implement` from issue #<issue>.
```

The pricing lines are written once and never refreshed.

Adopt an existing pull request only on explicit resume after verifying base, head, draft state, branch, and closing issue.

## Hand back at the draft

When the draft opens, report the pull request number and the pushed head SHA and end. Do not wait on the checks, poll them, or park on a background task: a subagent's prompt cache lasts five minutes, so a wait longer than that rewrites its whole context, and the hooks in `.hooks/` refuse the wait.

The dispatching session waits. It runs `scripts/wave-status.sh --wait <pr>` as one background command and acts on the result for the head it was handed:

- green with no native change request and every thread resolved: the draft is complete;
- red: it invokes `/implement <issue> --resume`, in a fresh agent when the work is dispatched, and waits again on the new head;
- an authentication, network, runner, or service outage: it preserves the artifacts and reports the retry point.

When this skill runs in the main session, that session is the dispatching session: it starts the wait itself after reporting, and a later turn resumes from the facts.

A resume on a red head reads the failing checks for the current head SHA and applies this table once:

| Failure | Action |
| --- | --- |
| format, clippy, docs, compile, deterministic test | fix the cause, commit, push, end; the dispatching session counts one real retry |
| same test fails twice | treat as real and fix the cause |
| unrelated tests fail differently | report it; the dispatching session reruns the job without a push up to twice, then counts a retry |
| Plan omitted a necessary edit or current code contradicts it | stop with a Plan rescope recommendation |
| chosen design cannot work | stop with a Design rescope recommendation |
| authentication, network, runner, or service outage | preserve artifacts and report the retry point |

For the fix rerun format, full clippy, focused verification, overflow pricing, and cleanliness before one plain push, then report the pull request number and the new head SHA and end again. Do not amend or rewrite pushed history.

The dispatching session owns the retry count, because no single invocation outlives a wait. It allows three real code-failure retries unless the owner names another number; at the cap it stops re-invoking, records the ordered evidence, and returns the issue to Plan.

## Repair loop

A resume on a green head repairs each active native change request and each unresolved review thread:

1. reproduce and verify the item;
2. fix it, at any path (overflow is priced at landing), or record a concrete evidence-backed justification;
3. commit conventionally, rerun local checks and overflow pricing, and plain-push once for the batch;
4. reply to the anchored thread with the fix commit or justification;
5. resolve a thread only after its item is addressed;
6. report the pull request number and the new head SHA and end; the dispatching session waits on the new head.

Never waive an item silently. A native `CHANGES_REQUESTED` stays active until that reviewer approves or GitHub reports it dismissed; no reply clears it. A root-level or out-of-scope result stops with an explicit Define, Design, or Plan rescope recommendation. Never put machine JSON/HTML into a pull-request review or comment.

## Completion

An invocation succeeds when it hands back a draft whose current head has a matching trusted approval, approval-base ancestry, and priced overflow reported in the handoff. The implementation is complete when that same head also has green required checks, no active native change request, and every thread resolved; the dispatching session establishes that after its wait. The pull request remains draft and unmerged; branch and worktree remain present.

Report all evidence. A complete draft points to `/land <pr>`.

## Sweep

Sweep is two-turn. First discover open issues with complete managed artifacts and current trusted approvals at fresh main, apply every gate, inspect live claims and surface overlap, print exact model routing and drops, then wait for owner confirmation. On confirmation revalidate the exact set and run one issue per isolated worker within live capacity. The parent completes validation and draft creation for each result. The main session then waits on each draft's checks as one background command per pull request and re-invokes a resume on a red head. One issue never authorizes edits in another worktree.
