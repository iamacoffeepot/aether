---
name: release-init
description: "Bootstrap Aether release workflow labels. Use to ensure phase, bounce-to, size, and model labels exist for the issue lifecycle."
---

# Release Init

Use this Codex skill for release label bootstrap.

## Source

- Workflow source: `.claude/skills/release-init/SKILL.md`
- Translation rules: `../_shared/claude-to-codex.md`

## Procedure

1. Read both source files completely before acting.
2. Create or reconcile only the labels described by the source.
3. Use REST `gh api` label operations where available.
4. Do not alter issues, branches, or PRs beyond the label bootstrap.
