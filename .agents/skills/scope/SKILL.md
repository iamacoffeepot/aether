---
name: scope
description: "Walk an Aether issue from Backlog through Define, Design, and Plan. Use for release scoping that writes Problem statement, Design notes, Implementation plan, and phase/size/model labels."
---

# Scope

Use this Codex skill for the repository's issue scoping workflow.

## Source

- Workflow source: `.claude/skills/scope/SKILL.md`
- Translation rules: `../_shared/claude-to-codex.md`

## Procedure

1. Read both source files completely before acting.
2. Ground reads against fresh `origin/main` as the Claude source requires.
3. Write only scope-managed issue body sections: `## Problem statement`, `## Design notes`, `## Implementation plan`, `## Sub-issues`, `## Depends on`, and `## Side findings`.
4. Use REST `gh api` forms for issue body and label updates.
5. Stop at `phase:plan`; do not approve or implement from this skill.
6. If the source calls for sweep mode, spawn subagents only after the user confirms the sweep plan.
