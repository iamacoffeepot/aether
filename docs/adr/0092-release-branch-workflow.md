# ADR-0092: Release-branch workflow

- **Status:** Proposed
- **Date:** 2026-06-03

## Context

Releases are scoped on GitHub Project boards (ADR-era release tooling: `/release-init`, `/scope`, `/approve`, `/implement`, `/bounce`). The active release is named by `.claude/release-state.json` — a **local, gitignored** file holding `active_project`, `release_version`, and a field/option-ID cache for the board. Every release-flow skill reads it to know which board to drive.

This has two friction points that compound as soon as more than one release is in play:

1. **One active release at a time.** `release-state.json` names exactly one project. With 0.4 finishing and 0.5 (Plato) opening, the file went stale (still pointed at the finished 0.4 board), and there is no clean way to "hop" between release contexts — you hand-edit the file and rebuild its cache. Creating a 0.6 board makes it worse: `/release-init` repoints `active_project` to whatever it just created, stealing "active" from the in-progress release.
2. **Everything lands on `main`.** Feature PRs target the released trunk directly. `main` is protected (required checks `CI pass` + `Lint PR title`, linear history, squash-only, no force) and the repo has native auto-merge on, so a green PR merges into the released trunk immediately. There is no staging boundary between "implemented" and "released" — the release *is* whatever has accumulated on `main`.

We want a release to be an isolated, reviewable unit with its own context that we can switch into and out of by switching branches, and a deliberate boundary at which a release becomes part of the trunk.

Verified current state (2026-06-03): `main` required checks are `CI pass` + `Lint PR title`; `linear_history=true`, `allow_merge_commit=false`, `allow_squash_merge=true`, `allow_rebase_merge=false`, `allow_auto_merge=true`, `delete_branch_on_merge=true`. CI (`ci.yml`), PR-title lint (`pr-title.yml`), and perf-compare (`perf-compare.yml`) all trigger on bare `pull_request:` with **no base-branch filter**, so they fire on PRs to any base.

## Decision

Adopt a **long-lived release branch** per release.

1. **Branch.** Each release gets `release/<version>` (e.g. `release/0.5`), cut from `main`. It is protected with the same rules as `main` (required checks `CI pass` + `Lint PR title`, linear history, no force, no deletion).
2. **PRs target the release branch.** `/implement` (and ad-hoc fixes, ADRs, and `/scope`'s ADR drafts) branch from and target `release/<version>`, not `main`. The release branch is the integration point for the release's work.
3. **`release-state.json` is tracked, per release branch.** It moves out of `.gitignore` and is committed on the release branch. `git checkout release/0.5` *is* switching release context — the active board, version, and field cache come with the branch. No hand-editing, no hopping. A different release branch carries its own. (`release-units.json` follows the same rule.)
4. **Per-PR review is preserved.** PRs into the release branch are opened as **drafts**, held for the user's read, then un-drafted to let CI-green auto-merge land them (per ADR-era review discipline — native auto-merge means a non-draft green PR merges itself, so draft is the review gate). The release branch is *not* released, but we keep the per-PR read rather than deferring all review to the end.
5. **Release → main by rebase.** At release end, `release/<version>` lands on `main` by **rebase merge** (replaying each PR-commit onto `main`), keeping `main` linear and preserving per-PR history. This requires enabling `allow_rebase_merge` on the repo. The release branch is tagged/retained as the granular record.

## Consequences

**Skills:**
- `/release-init <ver>` — additionally cut + push `release/<ver>` from `main`, set its branch protection, and commit the (now-tracked) `release-state.json` on it. Stops repointing a shared local file; the branch *is* the pointer.
- `/implement` — branch and worktree off the active release branch; `gh pr create --base release/<ver>`. The branch name derives from `release-state.json`'s `release_version`. Draft-PR done-condition is unchanged (still the review gate).
- `/adr`, `/scope` (ADR-draft sub-step) — base ADR PRs on the release branch.
- `/sweep` — "merged into `main`" detection becomes "merged into the active release branch."
- `/approve`'s ADR-merged gate is base-agnostic (`mergedAt`), so it is unaffected.

**Repo config:**
- `.gitignore` — un-ignore `.claude/release-state.json` and `.claude/release-units.json`.
- Enable `allow_rebase_merge` on the repo (for the release → main landing).
- Per-release: create `release/<ver>`, replicate protection, commit `release-state.json`.

**CI:** No workflow edits are required for gating — `ci.yml` / `pr-title.yml` / `perf-compare.yml` already trigger on all PRs. (Optional: add `release/**` to `ci.yml`'s `push:` filter if a post-merge-to-release job is ever needed; none is today.)

**Docs/memory:** CLAUDE.md Branches/Merging sections document the release-branch model; the "cut branches from fresh main" rule becomes "cut from the active release branch"; the release-execution-model memory is updated.

**Neutral/negative:**
- A second protected branch to maintain; protection must be set per release branch at init.
- `main` no longer reflects in-progress work — it is the released trunk only. Reading "what's the engine doing now" means looking at the release branch.
- `release-state.json` on `main` is whatever the last merged release left; a new release branch overwrites it at init. Acceptable — the branch is the source of truth.

## Alternatives considered

- **Stay on `main`, fix only the stale pointer.** Rejected — doesn't solve the one-active-release constraint or give a staging boundary; the hopping pain returns the moment two releases overlap.
- **Squash the whole release into one commit on `main`.** Rejected — collapses per-PR history on the trunk; chose rebase to keep granular linear history.
- **Merge-commit release → main** (relax linear history). Rejected — keeps `main`'s linear-history invariant intact by rebasing instead.
- **Auto-merge PRs into the release branch without per-PR review** (review only at release → main). Rejected — the user keeps the per-PR read; the release → main rebase is a rubber-stamp, not the primary review gate.
