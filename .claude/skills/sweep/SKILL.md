---
name: sweep
description: Clean up local worktrees + branches whose PRs have merged. Invoke as `/sweep`. Scans `git worktree list` for non-primary worktrees, checks each branch against merged PRs on GitHub, and removes the matched ones. Pair with `/delegate` — after delegated PRs land, run `/sweep` to reclaim disk + branch space.
---

# Sweep skill

Removes local worktrees + branches whose associated PRs have merged. The cleanup-side counterpart to `/delegate`, which seeds new worktrees per issue.

## Procedure

1. **List worktrees.** `git worktree list --porcelain`. Parse blocks of `worktree <path>` / `HEAD <sha>` / `branch refs/heads/<name>` separated by blank lines. Skip the primary worktree (the current working directory) and skip any detached-HEAD worktrees (no `branch` line — nothing to match against).

2. **Match each worktree's branch to PR state.** For each non-primary worktree branch:
   - `gh pr list --head <branch> --state merged --json number,mergedAt,url --limit 1` — if non-empty, mark `merged`.
   - Otherwise `gh pr list --head <branch> --state all --json number,state,url --limit 1` — distinguish `open` / `closed` (closed-not-merged) / no-PR.

3. **Surface the plan.** Print a table of every non-primary worktree: path, branch, status (`merged: #NNN`, `open: #NNN`, `closed-not-merged: #NNN`, `no PR`). Then ask the user to confirm before removing anything.

4. **Remove confirmed entries.**
   - For each `merged` worktree: `git worktree remove <path>` (use `--force` only if the worktree has untracked-but-uncommitted files; the agent should have committed everything, but stragglers happen). Then `git branch -D <branch>` if the local branch still exists.
   - For `closed-not-merged`, `open`, and `no PR`: do not auto-remove. Surface them and let the user decide per-item — those represent abandoned work, in-flight PRs, and possibly in-progress agent runs respectively.

5. **Report what was swept** — paths removed, branches deleted, plus any worktrees left alone with the reason.

## Constraints and notes

- Never sweep the primary worktree, regardless of branch state.
- Never sweep worktrees whose branch has an open PR — those are in flight.
- `git worktree list --porcelain` output: blocks of `worktree <path>` / `HEAD <sha>` / `branch refs/heads/<name>` separated by blank lines. Detached-HEAD entries lack a `branch` line.
- A locked worktree refuses removal — `git worktree unlock <path>` first, then retry. If unsure why it's locked, surface the lock reason (`git worktree list` shows `locked` flag) instead of forcing.
- Remote tracking refs are pruned by GitHub when the PR merges with `--delete-branch`. If a stale `origin/<branch>` lingers, `git fetch --prune` clears it — optional final step.
