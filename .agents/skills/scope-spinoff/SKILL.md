---
name: scope-spinoff
description: "Spin selected Aether scope side findings into child Backlog issues. Use when an issue has a Side findings section and the user chooses entries to file separately."
---

# Scope Spinoff

Use this Codex skill for side-finding triage.

## Source

- Workflow source: `.claude/skills/scope-spinoff/SKILL.md`
- Translation rules: `../_shared/claude-to-codex.md`

## Procedure

1. Read both source files completely before acting.
2. Read the parent issue's `## Side findings` section.
3. Ask for selected indices unless the invocation supplies them or requests `--all`.
4. File each selected finding through sketch mechanics.
5. Update the parent body only as the source directs, preserving unselected findings and user prose.
