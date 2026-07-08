---
name: adr
description: "Scaffold a new Aether Architecture Decision Record. Use when a load-bearing architectural decision needs a numbered ADR draft from docs/adr/TEMPLATE.md."
---

# ADR

Use this Codex skill for ADR scaffolding.

## Source

- Workflow source: `.claude/skills/adr/SKILL.md`
- Translation rules: `../_shared/claude-to-codex.md`

## Procedure

1. Read both source files completely before acting.
2. Require a title from the user.
3. Confirm the target worktree is clean and on up-to-date `main` before creating the ADR branch.
4. Pick the next `docs/adr/NNNN-*.md` number, copy `docs/adr/TEMPLATE.md`, and fill only the allowed placeholders.
5. Do not commit, push, or open a PR until the ADR content is written.
