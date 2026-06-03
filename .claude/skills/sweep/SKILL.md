---
name: sweep
description: Reclaim stale local state. `/sweep` (or `/sweep worktrees`) removes worktrees + branches whose PRs merged; `/sweep branches` prunes worktree-less local branches whose PRs merged; `/sweep memory` compresses + de-indexes the project's stale memory index; `/sweep all` runs every target. Each enumerates candidates, classifies by a staleness signal, prints a plan, and confirms before acting. Pair the git targets with `/implement` — after implemented PRs land, run `/sweep` to reclaim disk + branch space.
---

# Sweep skill

Reclaims accumulated local cruft. Every target runs the same five beats — **enumerate candidates → classify each by a staleness signal → surface a plan → confirm → act → report** — and differs only in the *signal* (what marks an entry stale) and the *action* (how it's removed).

## Targets

Parse the invocation argument to pick the target. Bare `/sweep` defaults to `worktrees` (unchanged behaviour).

| Arg | Sweeps | Staleness signal | Auto-removes |
|-----|--------|------------------|--------------|
| `worktrees` (default) | non-primary git worktrees + their branches | branch's PR merged on GitHub | merged only |
| `branches` | local branches **without** a worktree | PR merged / `[gone]` upstream / merged into main | PR-merged only |
| `memory` | the project's `MEMORY.md` index | over-limit / over-long lines / superseded / orphaned / stale refs | nothing (compress + de-index only) |
| `all` | run `worktrees`, then `branches`, then `memory` in sequence | — | per-target |

The git targets auto-remove only the entries with a hard merge-oracle (a merged PR) after one confirm; everything fuzzier is surfaced and left for the user. The `memory` target has no oracle, so it **never deletes files** — it tightens index hooks and drops index lines for superseded notes (the topic files stay on disk as archive).

## Target: worktrees (default)

1. **List worktrees.** `git worktree list --porcelain`. Parse blocks of `worktree <path>` / `HEAD <sha>` / `branch refs/heads/<name>` separated by blank lines. Skip the primary worktree (the current working directory) and skip any detached-HEAD worktrees (no `branch` line — nothing to match against).

2. **Match each worktree's branch to PR state.** For each non-primary worktree branch:
   - `gh pr list --head <branch> --state merged --json number,mergedAt,url --limit 1` — if non-empty, mark `merged`.
   - Otherwise `gh pr list --head <branch> --state all --json number,state,url --limit 1` — distinguish `open` / `closed` (closed-not-merged) / no-PR.

3. **Surface the plan.** Print a table of every non-primary worktree: path, branch, status (`merged: #NNN`, `open: #NNN`, `closed-not-merged: #NNN`, `no PR`). Then ask the user to confirm before removing anything.

4. **Remove confirmed entries.**
   - For each `merged` worktree: `git worktree remove <path>` (use `--force` only if the worktree has untracked-but-uncommitted files; the agent should have committed everything, but stragglers happen). Then `git branch -D <branch>` if the local branch still exists.
   - For `closed-not-merged`, `open`, and `no PR`: do not auto-remove. Surface them and let the user decide per-item — those represent abandoned work, in-flight PRs, and possibly in-progress agent runs respectively.

5. **Report what was swept** — paths removed, branches deleted, plus any worktrees left alone with the reason.

## Target: branches

Worktree-less local branches — the ones you checked out and worked on directly, with no dedicated worktree. (Branches that *do* back a worktree are the `worktrees` target's job; skip them here so the two don't double-handle the same branch.)

1. **List local branches with upstream-tracking state.** `git for-each-ref --format='%(refname:short) %(upstream:track)' refs/heads`. Build the worktree-branch set from `git worktree list --porcelain` and subtract it. Always exclude the current branch and the default branch (`main`).

2. **Classify each remaining branch.**
   - `gh pr list --head <branch> --state merged --json number,url --limit 1` non-empty → `merged: #NNN`. This is the hard signal (a squash-merge leaves the branch *not* an ancestor of `main`, so PR state is the only reliable merged signal).
   - `[gone]` in the upstream-track column → the remote branch was deleted (the usual after a `--delete-branch` merge). Treat as a *merged candidate* but confirm against the PR check before removing.
   - `git branch --merged main` lists it → `merged-into-main` (true-merge ancestry).
   - Open PR (`gh pr list --head <branch> --state open`) → `open: #NNN` — leave, in flight.
   - No PR and unpushed commits (`%(upstream:track)` shows `ahead` or no upstream + commits not in `main`) → `local-wip` — potential lost work; surface, never auto-remove.

3. **Surface the plan.** Table: branch, status, last-commit date. Confirm before removing.

4. **Remove confirmed entries.**
   - `merged: #NNN` → `git branch -D <branch>` (PR-merge confirmed by GitHub, so force-delete is safe even though squash leaves it non-ancestor).
   - `merged-into-main` → `git branch -d <branch>` (let git's ancestry check be the backstop).
   - `[gone]`-only without a confirmed merged PR, `open`, `local-wip` → do not auto-remove; surface for a per-item decision.

5. **Report** — branches deleted, branches left alone with the reason. Optionally `git fetch --prune` to clear stale `origin/<branch>` tracking refs.

## Target: memory

Curates the current project's auto-memory index (`MEMORY.md`) without losing knowledge. Compress + de-index only — **never delete topic files**.

1. **Locate the memory dir.** `~/.claude/projects/<slug>/memory/`, where `<slug>` is the project's absolute path with every `/` replaced by `-` (e.g. `/Users/x/workspace/aether` → `-Users-x-workspace-aether`). Compute it: `slug=$(echo "$PWD" | sed 's#/#-#g')`.

2. **Measure + enumerate.**
   - Index size: `wc -c MEMORY.md` against the ~24.4 KB (≈24985-byte) limit. Note the margin.
   - Over-long index lines: `awk 'length>200'` — each should be a tight one-liner.
   - Topic files vs index: `ls *.md` minus the entries linked from `MEMORY.md` → **orphaned files** (on disk, never recalled).

3. **Classify each index entry.**
   - **Oversized** (line >~200 chars) → compress the hook (the text after the em-dash), keeping load-bearing identifiers (ADR / PR / issue numbers, crate / mailbox / API names — that's the searchable signal). This is the primary lever; it's lossless.
   - **Superseded** — the body or hook says `superseded` / `historical` / `retired`, or another note explicitly supersedes it (e.g. "Supersedes the N notes below") → **de-index candidate**.
   - **Stale reference** — the entry names a file / crate / symbol / flag / mailbox. Grep the repo to confirm it still exists; if it's been renamed or removed, the entry is stale → surface for the user (a rename-fix or a removal, their call — do not auto-act).
   - **Duplicate** — two entries cover the same fact → consolidation candidate (surface).

4. **Surface the plan.** Per-entry: proposed action (`compress` / `de-index` / `flag-stale` / `leave`) and the resulting projected index size + margin. Before listing a de-index, grep the other topic files for inbound `[[name]]` links to it — dangling links are allowed, but note which retained notes will point at a de-indexed (archived) file. Confirm.

5. **Act.**
   - Compress oversized hooks in place (Edit).
   - For confirmed-superseded entries: remove the index line from `MEMORY.md` but **leave the topic file on disk** — it becomes archive, and any inbound `[[links]]` still resolve to a real file. Re-checking the limit after compression often clears it without any de-indexing.
   - Stale-reference + duplicate entries: surface only; let the user choose fix-vs-remove.

6. **Report** — new index size + margin, count of lines compressed, entries de-indexed (with their archived filenames), and stale refs flagged for follow-up.

## Constraints and notes

- **Confirm before acting**, on every target. Auto-removal is limited to the hard-oracle cases (PR-merged worktrees / branches); memory never auto-deletes anything off disk.
- **worktrees**: never sweep the primary worktree or a worktree whose branch has an open PR. `git worktree list --porcelain` blocks are `worktree <path>` / `HEAD <sha>` / `branch refs/heads/<name>` separated by blank lines; detached-HEAD entries lack a `branch` line. A locked worktree refuses removal — `git worktree unlock <path>` first, or surface the lock reason rather than forcing.
- **branches**: never delete the current or default branch, a branch with an open PR, or a worktree-backed branch (handled by `worktrees`). A branch with unpushed commits and no merged PR is potential lost work — surface, never auto-remove.
- **memory**: never delete topic files (de-index keeps them as archive). Don't touch `user`-type memories (identity facts, rarely stale) without an explicit ask. When compressing, preserve the searchable identifiers. Verify a named file/symbol still exists in the repo before calling an entry stale — a memory reflects what was true when written, not necessarily now.
- Remote tracking refs are pruned by GitHub when a PR merges with `--delete-branch`. If a stale `origin/<branch>` lingers, `git fetch --prune` clears it — optional final step for the git targets.
