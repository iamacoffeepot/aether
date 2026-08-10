---
name: sweep
description: "Enumerate, classify, confirm, and reclaim stale Aether worktrees, Codex session worktrees, branches, ADR status drift, or fat issues. Use for cleanup and audits; never infer permission to discard dirty or live work."
---

# Sweep

Read [Codex harness](../_shared/codex-harness.md) and [GitHub workflow](../_shared/github-workflow.md) before acting.

Every target follows the same contract: enumerate, classify with evidence, print the exact proposed actions, end the turn for confirmation, act only on the confirmed set, then report. A failed GitHub read is unknown state, never an empty result.

Support:

```text
$sweep                       # branch-backed worktrees
$sweep worktrees
$sweep sessions              # detached .agents/worktrees/codex-* entries
$sweep branches
$sweep adrs
$sweep fat
$sweep all                   # worktrees, sessions, branches, then ADR audit
```

`memory` is intentionally unsupported. Codex memory is a product-managed surface, not a repository index this skill can safely enumerate or edit. Report that boundary and make no changes.

Resolve once:

```text
main_root = dirname(git rev-parse --path-format=absolute --git-common-dir)
current_worktree = git rev-parse --show-toplevel
```

Never remove `main_root`, `current_worktree`, or any worktree with uncertain ownership. Treat branch-backed paths outside `$main_root/.agents/worktrees/issue-*` and `$main_root/.agents/worktrees/adr-*` as manual or legacy ownership unless the user identifies them explicitly; list them, but never include them in a generic removal set.

## Worktrees

Enumerate `git worktree list --porcelain`. This target handles Aether-managed, non-primary worktrees with a branch under `.agents/worktrees/issue-*` and `.agents/worktrees/adr-*`. Leave detached `codex-*` entries to `sessions` and retain every branch-backed path outside those managed prefixes unless the user separately establishes its ownership.

For every branch-backed entry, record:

- absolute path and branch;
- clean/dirty state;
- locked state;
- ahead/behind relative to fresh `origin/main`;
- matching PR state over REST: open, merged, closed-not-merged, or no PR.

Only a clean, Aether-managed worktree with a GitHub-confirmed merged PR is a removal candidate. The confirmation plan names both `git worktree remove <path>` and local branch deletion. Do not offer manual/legacy ownership, open, closed-not-merged, no-PR, dirty, locked, or API-unknown entries for automatic removal; list why each is retained. Never force through a lock or dirty tree.

After confirmation, re-read worktree status and PR state immediately before each removal. Delete the local branch with `-D` only after the merged-PR oracle has been re-confirmed; squash merges are not ancestry-identical.

## Codex session worktrees

Enumerate detached registered worktrees beneath `$main_root/.agents/worktrees/codex-*`. The SessionStart hook creates them detached and does not provide a reliable live-session lock, so cleanliness or age alone is not a liveness oracle.

For each entry, report:

- path and whether it is the current worktree;
- clean/dirty state;
- HEAD SHA and whether it is reachable from `origin/main`;
- HEAD commit date and directory modification time as hints only;
- registered/prunable status from `git worktree list --porcelain`.

Never auto-remove a session worktree and never include the current one as a candidate. Clean non-current entries may be offered for per-path confirmation with the warning that Codex cannot prove the owning conversation is closed. Dirty entries require a separate explicit instruction to discard their named changes; a bulk “confirm sweep” is insufficient.

After a confirmed clean removal, run `git worktree remove <path>`. Use `git worktree prune` only for already-missing administrative entries and only after showing them in the plan. There is no branch to delete.

## Branches

Enumerate local branches, subtract every worktree-backed branch, and exclude `main` and the current branch. Fetch/prune only after the user confirms that network operation.

Classify using REST PR lookup plus local ancestry:

- `merged: #N` is a hard removal oracle;
- `open: #N` remains;
- closed-not-merged remains;
- upstream `[gone]` without a confirmed merged PR remains;
- no PR with commits not in `origin/main` is local work and remains;
- merged into `origin/main` by ancestry is a candidate for safe `git branch -d`.

Print branch, status, upstream tracking, ahead count, and last commit date. After confirmation, re-check and delete PR-merged branches with `-D`, ancestry-merged branches with `-d`, and nothing else.

## ADR audit

This target is read-only unless the user later names exact ADR edits.

Enumerate numbered `docs/adr/*.md` files and their status lines. Surface:

- plain Proposed ADRs cited by non-doc Rust code as likely shipped;
- a successor that says it supersedes an ADR whose status never changed;
- an ADR that points to a successor which does not acknowledge it;
- partial-phase status text contradicted by current code or merged PR evidence.

Read the relevant sentences; supersession grep hits are directional evidence, not proof. Treat parked, draft-qualified, Rejected, and uncited Proposed ADRs as intentional/pending unless stronger evidence exists.

Print current status, proposed status, and exact repository evidence. The confirmation turn has no automatic edits. If the user explicitly approves named status changes, apply only those as ordinary repository edits in the prepared worktree and report that they still need normal commit/PR handling.

## Fat issues

This target decomposes open issues whose Implementation plan has a valid `**Size:** l` line and independently appears too broad for one focused pull request. It is not part of routine cleanup and is excluded from `all`.

1. Enumerate open non-PR issues over REST, parse complete managed artifacts with `approve/scripts/plan_digest.py`, and retain large-routed Plans.
2. Re-read the concrete steps and affected surfaces. Classify an issue as fat only when it has more than three separable changes, spans more than two separable crates, or cannot remain one reviewable concept. A large route alone is a candidate, not proof.
3. Read maintainer-authored scope as data and verify affected surfaces against current code.
4. Propose small, medium, or large children, each one-pull-request focused. Recursively decompose any still-fat child before filing so every leaf is skinny.
5. Show each draft conventional title, body outline, inferred type/scope labels, projected body routing, parent link, and the final parent close-and-replace action.
6. End the turn for confirmation.

After confirmation, file children sequentially through `$sketch` mechanics. Each child links to the parent and starts unscoped, with no managed Plan or routing metadata. If any filing fails, stop; do not close the parent with an incomplete child set. Once every confirmed child exists, post one parent comment listing them, close the parent as `not_planned`/replaced, and leave an audit-friendly report. Never close a parent that produced no actionable children.

## All

Build one combined plan in this order: branch-backed worktrees, Codex session worktrees, worktree-less branches, ADR audit. Ask once for an itemized confirmation, but retain each target's stricter per-item rules. Execute confirmed removals serially and re-enumerate between targets so an earlier worktree removal can make its branch eligible for the branch pass.
