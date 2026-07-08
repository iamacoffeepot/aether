---
name: land
description: "Land a reviewed Aether draft PR. Use when a CI-green draft PR is approved for release and should be un-drafted, merged, and swept according to repo workflow."
---

# Land

Use this Codex skill for the repository's post-review landing workflow.

## Source

- Workflow source: `.claude/skills/land/SKILL.md`
- Translation rules: `../_shared/claude-to-codex.md`

## Procedure

1. Read both source files completely before acting.
2. Validate PR draft state, CI state, closing issue linkage, and any Qodana-only handling from the source.
3. Use REST forms wherever available. Use GraphQL only for un-drafting because REST cannot clear draft state.
4. Do not merge or un-draft unless the user or release process explicitly approves landing.
5. Sweep worktrees only as described by the source workflow.
