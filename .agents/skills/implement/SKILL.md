---
name: implement
description: "Implement an approved Aether issue in a separate worktree, open a draft PR, and drive CI to green. Use only for phase:ready issues unless the user explicitly requests quick mode."
---

# Implement

Use this Codex skill for the repository's issue-to-draft-PR workflow.

## Source

- Workflow source: `.claude/skills/implement/SKILL.md`
- Translation rules: `../_shared/claude-to-codex.md`

## Procedure

1. Read both source files completely before acting.
2. Verify preconditions: `phase:ready`, exactly one `model:*`, no umbrella sub-issues, an implementation plan, and working `gh` auth with `repo` scope.
3. Create a dedicated Codex worktree under `.agents/worktrees/issue-<N>` from `origin/main`; do not edit the primary `main` checkout.
4. Move the issue to `phase:executing` before implementation work starts.
5. Follow the issue's `## Implementation plan` literally. Deviations are bounces, not freelancing.
6. Commit the work, run `cargo fmt`, assert a clean worktree, push, and open a draft PR over REST.
7. Use `scripts/wave-status.sh --wait <pr>` for CI monitoring. Leave successful work at `phase:refine` with the PR still draft.
