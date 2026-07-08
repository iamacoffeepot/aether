---
name: bounce
description: "Regress an Aether issue to an earlier phase with a required reason. Use when scope, approval, implementation, or review discovers that Define, Design, or Plan must be redone."
---

# Bounce

Use this Codex skill for explicit phase regression.

## Source

- Workflow source: `.claude/skills/bounce/SKILL.md`
- Translation rules: `../_shared/claude-to-codex.md`

## Procedure

1. Read both source files completely before acting.
2. Require a concrete reason.
3. Validate the target phase is a regression to Define, Design, or Plan.
4. Reconcile labels with REST: `phase:bounced` plus the matching `bounce-to:*` label.
5. Post the required reason as an issue comment.
